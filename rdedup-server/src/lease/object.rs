//! Object streams check their originating lease between bounded I/O steps.
use super::{ExclusivePermit, LeaseCheck, SharedPermit, Writable};
use rdedup_lib::backends::local::{
    PendingObject, Publication, StoredObject, StreamError,
};
use rdedup_lib::backends::{ChunkPromotion, Metadata};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Object failures retain the distinction between lease loss and storage failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Protection expired or the coordinator stopped.
    #[error(transparent)]
    Lease(#[from] super::Error),
    /// Preparing or publishing a stream failed.
    #[error(transparent)]
    Stream(#[from] StreamError),
    /// A filesystem object operation failed.
    #[error("repository object I/O failed")]
    Storage(#[from] io::Error),
}

/// A stored object whose reads cannot continue under an expired lease.
pub struct LeaseReader {
    object: StoredObject,
    validity: LeaseCheck,
}

impl Read for LeaseReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.validity.ensure_active().map_err(io::Error::other)?;
        self.object.read(buffer)
    }
}

/// An unpublished object; losing its lease prevents further writes or commit.
pub struct LeaseWriter {
    object: PendingObject,
    validity: LeaseCheck,
}

impl Write for LeaseWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.validity.ensure_active().map_err(io::Error::other)?;
        self.object.write(buffer)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.validity.ensure_active().map_err(io::Error::other)?;
        self.object.flush()
    }
}

impl LeaseWriter {
    /// Publish a fully received object under its originating protection.
    ///
    /// # Errors
    /// Returns lease loss or a synchronization/publication failure.
    pub fn commit(self) -> Result<Publication, Error> {
        self.validity.ensure_active()?;
        let prepared = self.object.synchronize()?;
        self.validity.ensure_active()?;
        Ok(prepared.publish()?)
    }
}

impl<Access> SharedPermit<Access> {
    /// Open a streaming read retaining this lease's local protection.
    ///
    /// # Errors
    /// Returns lease loss or failure to open the object.
    pub fn read_object(&self, path: &Path) -> Result<LeaseReader, Error> {
        self.validity.ensure_active()?;
        Ok(LeaseReader {
            object: self.operation.read_object(path)?,
            validity: self.validity.clone(),
        })
    }
    /// Inspect metadata without transferring object bytes.
    ///
    /// # Errors
    /// Returns lease loss or an object lookup failure.
    pub fn metadata(&self, path: PathBuf) -> Result<Metadata, Error> {
        self.validity.ensure_active()?;
        Ok(self.operation.workers().new_thread()?.read_metadata(path)?)
    }
    /// Enumerate immediate directory entries under this operation's protection.
    ///
    /// # Errors
    /// Returns lease loss or a directory lookup failure.
    pub fn list(&self, path: PathBuf) -> Result<Vec<PathBuf>, Error> {
        self.validity.ensure_active()?;
        Ok(self.operation.workers().new_thread()?.list(path)?)
    }
}

impl SharedPermit<Writable> {
    /// Stage an additive object; cancellation removes its temporary entry.
    ///
    /// # Errors
    /// Returns lease loss or failure to prepare temporary storage.
    pub fn create_object(&self, path: &Path) -> Result<LeaseWriter, Error> {
        self.validity.ensure_active()?;
        Ok(LeaseWriter {
            object: self.operation.create_object(path)?,
            validity: self.validity.clone(),
        })
    }
    /// Make a validated chunk available in a newer generation without removal.
    ///
    /// # Errors
    /// Returns lease loss or a promotion failure.
    pub fn promote(&self, promotion: ChunkPromotion) -> Result<(), Error> {
        self.validity.ensure_active()?;
        Ok(self
            .operation
            .workers()
            .new_thread()?
            .promote_chunk(promotion)?)
    }
}

impl ExclusivePermit {
    /// Stage replacement contents while retaining exclusive protection.
    ///
    /// # Errors
    /// Returns lease loss or failure to prepare temporary storage.
    pub fn replace_object(&self, path: &Path) -> Result<LeaseWriter, Error> {
        self.validity.ensure_active()?;
        Ok(LeaseWriter {
            object: self.operation.replace_object(path)?,
            validity: self.validity.clone(),
        })
    }
    /// Remove one object while excluding other repository operations.
    ///
    /// # Errors
    /// Returns lease loss or a removal failure.
    pub fn remove(&self, path: PathBuf) -> Result<(), Error> {
        self.validity.ensure_active()?;
        Ok(self.operation.workers().new_thread()?.remove(path)?)
    }
    /// Remove a directory tree while excluding other repository operations.
    ///
    /// # Errors
    /// Returns lease loss or a removal failure.
    pub fn remove_directory(&self, path: PathBuf) -> Result<(), Error> {
        self.validity.ensure_active()?;
        Ok(self
            .operation
            .workers()
            .new_thread()?
            .remove_dir_all(path)?)
    }
    /// Relocate an arbitrary admitted repository entry with exclusive protection.
    ///
    /// # Errors
    /// Returns lease loss or a rename failure.
    pub fn rename(
        &self,
        source: PathBuf,
        destination: PathBuf,
    ) -> Result<(), Error> {
        self.validity.ensure_active()?;
        Ok(self
            .operation
            .workers()
            .new_thread()?
            .rename(source, destination)?)
    }
}
