//! Protected backend operations and their worker capabilities.
//!
//! A backend acquires protection before constructing an operation. Each worker
//! retains that exact operation, so dropping the initiating handle cannot release
//! protection while storage work remains in flight.
use std::io;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use super::promotion::ChunkPromotion;
use sgdata::SGData;

/// Protection permitting concurrent readers and additive writers.
pub struct Shared;

/// Protection excluding every other repository operation.
pub struct Exclusive;

/// A repository's source of protected operations.
pub trait Backend: Send + Sync {
    /// Acquire protection for additive writes and reads.
    ///
    /// # Errors
    /// Returns the acquisition failure without constructing an operation.
    fn begin_shared(&self) -> io::Result<BackendOperation<Shared>>;

    /// Acquire protection for removal, GC, and metadata replacement.
    ///
    /// # Errors
    /// Returns the acquisition failure without constructing an operation.
    fn begin_exclusive(&self) -> io::Result<BackendOperation<Exclusive>>;
}

/// Backend-owned protection and the factory for workers using it.
///
/// Implementations acquire protection before construction and release it on drop.
/// The operation wrapper shares this resource with its workers. This is shared
/// ownership of one protection resource, not a registry of interchangeable locks.
pub trait OperationState: Send + Sync {
    /// Construct storage access bound to this operation's protection.
    ///
    /// # Errors
    /// Returns any failure to prepare the worker.
    fn new_thread(&self) -> io::Result<Box<dyn BackendThread>>;
}

/// An acquired repository operation with a statically known capability.
pub struct BackendOperation<Access> {
    state: Arc<dyn OperationState>,
    access: PhantomData<Access>,
}

impl<Access> Clone for BackendOperation<Access> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            access: PhantomData,
        }
    }
}

impl<Access> BackendOperation<Access> {
    /// Own already-acquired protection supplied by a backend implementation.
    pub fn new(state: impl OperationState + 'static) -> Self {
        Self {
            state: Arc::new(state),
            access: PhantomData,
        }
    }

    /// Create a worker retaining this operation until the worker is dropped.
    ///
    /// # Errors
    /// Returns the backend's worker construction error.
    pub fn new_thread(&self) -> io::Result<BackendWorker<Access>> {
        Ok(BackendWorker {
            thread: self.state.new_thread()?,
            operation: self.clone(),
        })
    }
}

impl BackendOperation<Exclusive> {
    /// Restrict a handle's capabilities without releasing exclusive protection.
    pub fn shared(&self) -> BackendOperation<Shared> {
        BackendOperation {
            state: Arc::clone(&self.state),
            access: PhantomData,
        }
    }
}

/// Storage access retaining its originating operation's protection.
///
/// Shared workers cannot invoke destructive operations:
///
/// ```compile_fail
/// use rdedup_lib::backends::{BackendWorker, Shared};
/// fn remove(mut worker: BackendWorker<Shared>) {
///     worker.remove("name.yml".into()).unwrap();
/// }
/// ```
pub struct BackendWorker<Access> {
    // Drop storage handles before releasing their protection.
    thread: Box<dyn BackendThread>,
    operation: BackendOperation<Access>,
}

impl<Access> BackendWorker<Access> {
    /// Atomically publish an object without replacing an existing object.
    ///
    /// An existing destination is left intact. The caller compares logical
    /// identity when existence alone does not establish an equivalent retry.
    ///
    /// # Errors
    /// Returns a storage or protection failure.
    pub fn create(
        &mut self,
        path: PathBuf,
        contents: SGData,
    ) -> io::Result<()> {
        self.thread.write(path, contents, true)
    }

    /// Make an existing chunk available in a newer generation without removal.
    ///
    /// # Errors
    /// Returns a storage or protection failure.
    pub fn promote_chunk(
        &mut self,
        promotion: ChunkPromotion,
    ) -> io::Result<()> {
        self.thread.promote_chunk(promotion)
    }

    /// Read one stored object under this operation's protection.
    ///
    /// # Errors
    /// Returns a storage or protection failure.
    pub fn read(&mut self, path: PathBuf) -> io::Result<SGData> {
        self.thread.read(path)
    }

    /// Read metadata without transferring an object's contents.
    ///
    /// # Errors
    /// Returns a storage or protection failure.
    pub fn read_metadata(
        &mut self,
        path: PathBuf,
    ) -> io::Result<super::Metadata> {
        self.thread.read_metadata(path)
    }

    /// List the immediate entries in a repository directory.
    ///
    /// # Errors
    /// Returns a storage or protection failure.
    pub fn list(&mut self, path: PathBuf) -> io::Result<Vec<PathBuf>> {
        self.thread.list(path)
    }

    /// Send recursive listing results while retaining operation protection.
    pub fn list_recursively(
        &mut self,
        path: PathBuf,
        sender: mpsc::Sender<io::Result<Vec<PathBuf>>>,
    ) {
        self.thread.list_recursively(path, sender);
    }

    pub(super) fn into_protected_thread(self) -> ProtectedThread {
        ProtectedThread {
            thread: self.thread,
            _protection: self.operation.state,
        }
    }
}

impl BackendWorker<Exclusive> {
    /// Replace an object's contents while excluding other repository operations.
    ///
    /// # Errors
    /// Returns a storage or protection failure.
    pub fn replace(
        &mut self,
        path: PathBuf,
        contents: SGData,
    ) -> io::Result<()> {
        self.thread.write(path, contents, false)
    }

    /// Remove one object with exclusive protection.
    ///
    /// # Errors
    /// Returns a storage or protection failure.
    pub fn remove(&mut self, path: PathBuf) -> io::Result<()> {
        self.thread.remove(path)
    }

    /// Remove a directory tree with exclusive protection.
    ///
    /// # Errors
    /// Returns a storage or protection failure.
    pub fn remove_dir_all(&mut self, path: PathBuf) -> io::Result<()> {
        self.thread.remove_dir_all(path)
    }

    /// Rename an arbitrary object with exclusive protection.
    ///
    /// # Errors
    /// Returns a storage or protection failure.
    pub fn rename(
        &mut self,
        source: PathBuf,
        destination: PathBuf,
    ) -> io::Result<()> {
        self.thread.rename(source, destination)
    }
}

/// Type-erased worker used only by the internal I/O command dispatcher.
pub(super) struct ProtectedThread {
    pub(super) thread: Box<dyn BackendThread>,
    _protection: Arc<dyn OperationState>,
}

/// Storage primitives implemented by a backend, behind protected workers.
///
/// Implementations must not expose directly constructible unprotected workers.
pub trait BackendThread: Send {
    /// Publish the destination of a validated promotion without removing its source.
    ///
    /// # Errors
    /// Returns a storage or protection failure.
    fn promote_chunk(&mut self, promotion: ChunkPromotion) -> io::Result<()>;

    fn remove_dir_all(&mut self, path: PathBuf) -> io::Result<()>;

    fn rename(
        &mut self,
        src_path: PathBuf,
        dst_path: PathBuf,
    ) -> io::Result<()>;

    fn write(
        &mut self,
        path: PathBuf,
        sg: SGData,
        idempotent: bool,
    ) -> io::Result<()>;

    fn read(&mut self, path: PathBuf) -> io::Result<SGData>;

    fn remove(&mut self, path: PathBuf) -> io::Result<()>;

    fn read_metadata(&mut self, path: PathBuf) -> io::Result<super::Metadata>;
    fn list(&mut self, path: PathBuf) -> io::Result<Vec<PathBuf>>;

    fn list_recursively(
        &mut self,
        path: PathBuf,
        tx: mpsc::Sender<io::Result<Vec<PathBuf>>>,
    );
}
