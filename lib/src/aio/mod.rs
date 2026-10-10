//! Asynchronous IO operations & backends
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::{io, thread};

use dangerous_option::DangerousOption as AutoOption;
use serde::{Deserialize, Serialize};
use sgdata::SGData;
use slog::{o, trace};
use slog::{Level, Logger};
use slog_perf::TimeReporter;
use url::Url;

pub(crate) mod local;
pub(crate) use self::local::Local;

#[cfg(feature = "backend-http")]
pub(crate) mod http;
#[cfg(feature = "backend-http")]
pub(crate) use self::http::HttpReadOnly;

#[cfg(feature = "backend-b2")]
pub(crate) mod b2;
#[cfg(feature = "backend-b2")]
pub(crate) use self::b2::B2;

pub(crate) mod backend;
pub(crate) mod promotion;
use promotion::ChunkPromotion;
pub(crate) mod local_cache;

use self::backend::*;

// {{{ Misc
struct WriteArgs {
    path: PathBuf,
    data: SGData,
    idempotent: bool,
    complete_tx: mpsc::Sender<io::Result<()>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Metadata {
    pub len: u64,
    pub is_file: bool,
    pub(crate) created: chrono::DateTime<chrono::Utc>,
}

/// A result of async io operation
///
/// It behaves a bit like a future/promise. It is happening
/// in the background, and only calling `wait` will make sure
/// the operations completed and return result.
#[must_use]
pub struct AsyncIOResult<T> {
    rx: mpsc::Receiver<io::Result<T>>,
}

impl<T> AsyncIOResult<T> {
    /// Block until result arrives
    pub fn wait(self) -> io::Result<T> {
        self.rx.recv().map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "I/O worker ended without a result",
            )
        })?
    }
}

#[derive(Clone, Debug)]
pub struct WriteStats {
    pub new_chunks: usize,
    pub new_bytes: u64,
}
// }}}

// {{{ Message
/// Message sent to a worker pool
///
/// Each type of job
enum Message {
    PromoteChunk(ChunkPromotion, mpsc::Sender<io::Result<()>>),
    Write(WriteArgs),
    Read(PathBuf, mpsc::Sender<io::Result<SGData>>),
    ReadMetadata(PathBuf, mpsc::Sender<io::Result<Metadata>>),
    List(PathBuf, mpsc::Sender<io::Result<Vec<PathBuf>>>),
    ListRecursively(PathBuf, mpsc::Sender<io::Result<Vec<PathBuf>>>),
    Remove(PathBuf, mpsc::Sender<io::Result<()>>),
    RemoveDirAll(PathBuf, mpsc::Sender<io::Result<()>>),
    Rename(PathBuf, PathBuf, mpsc::Sender<io::Result<()>>),
}
// }}}

// {{{ AsyncIO
/// A handle to a async-io worker pool
///
/// This object abstracts away asynchronous operations on the backend.
#[derive(Clone)]
pub struct AsyncIO {
    /// Data shared between threads of the pool.
    shared: Arc<AsyncIOShared>,
    /// tx endpoind of mpmc queue used to send jobs
    /// to the pool.
    tx: AutoOption<crossbeam_channel::Sender<Message>>,
}

impl AsyncIO {
    pub(crate) fn promote_chunk(
        &self,
        promotion: ChunkPromotion,
    ) -> AsyncIOResult<()> {
        let (sender, receiver) = mpsc::channel();
        // Disconnection drops the reply sender; wait reports BrokenPipe.
        let _ = self.tx.send(Message::PromoteChunk(promotion, sender));
        AsyncIOResult { rx: receiver }
    }

    pub(crate) fn new(
        operation: BackendOperation<Shared>,
        log: Logger,
    ) -> io::Result<Self> {
        let thread_num = 4 * num_cpus::get();
        let (tx, rx) = crossbeam_channel::bounded(thread_num);

        let shared = AsyncIOThreadShared::new();

        // Prepare every worker before spawning; a construction failure drops
        // the entire operation synchronously without detached worker threads.
        let workers = (0..thread_num)
            .map(|_| {
                operation
                    .new_thread()
                    .map(|worker| worker.into_protected_thread())
            })
            .collect::<io::Result<Vec<_>>>()?;
        let mut join = Vec::with_capacity(thread_num);
        for backend in workers {
            let receiver = rx.clone();
            let worker_shared = shared.clone();
            let worker_log = log.clone();
            join.push(thread::spawn(move || {
                let mut worker = AsyncIOThread::new(
                    worker_shared,
                    receiver,
                    backend,
                    worker_log,
                );
                worker.run();
            }));
        }
        drop(rx);

        let shared = AsyncIOShared {
            join,
            log: log.clone(),
            stats: shared,
        };

        Ok(AsyncIO {
            shared: Arc::new(shared),
            tx: AutoOption::new(tx),
        })
    }

    pub fn stats(&self) -> AsyncIOThreadShared {
        self.shared.stats.clone()
    }

    pub fn list(&self, path: PathBuf) -> AsyncIOResult<Vec<PathBuf>> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Message::List(path, tx))
            .expect("aio tx closed: list");
        AsyncIOResult { rx }
    }

    // TODO: No need for it anymore?
    #[allow(dead_code)]
    pub fn list_recursively(
        &self,
        path: PathBuf,
    ) -> Box<dyn Iterator<Item = io::Result<PathBuf>>> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Message::ListRecursively(path, tx))
            .expect("aio tx closed: list_recursively");

        let iter = rx.into_iter().flat_map(|batch| match batch {
            Ok(batch) => Box::new(batch.into_iter().map(Ok))
                as Box<dyn Iterator<Item = io::Result<PathBuf>>>,
            Err(e) => Box::new(Some(Err(e)).into_iter())
                as Box<dyn Iterator<Item = io::Result<PathBuf>>>,
        });
        Box::new(iter)
    }

    // TODO: No need for it anymore
    #[allow(dead_code)]
    pub fn write_idempotent(
        &self,
        path: PathBuf,
        sg: SGData,
    ) -> AsyncIOResult<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Message::Write(WriteArgs {
                path,
                data: sg,
                idempotent: true,
                complete_tx: tx,
            }))
            .expect("aio tx closed: write_idempotent");
        AsyncIOResult { rx }
    }

    pub fn read(&self, path: PathBuf) -> AsyncIOResult<SGData> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Message::Read(path, tx))
            .expect("aio tx closed: read");
        AsyncIOResult { rx }
    }

    pub(crate) fn read_metadata(
        &self,
        path: PathBuf,
    ) -> AsyncIOResult<Metadata> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Message::ReadMetadata(path, tx))
            .expect("aio tx closed: read_metadata");
        AsyncIOResult { rx }
    }
}

/// Destructive I/O capability retaining exclusive repository protection.
pub(crate) struct ExclusiveAsyncIO {
    shared: AsyncIO,
}

impl ExclusiveAsyncIO {
    pub(crate) fn new(
        operation: BackendOperation<Exclusive>,
        log: Logger,
    ) -> io::Result<Self> {
        Ok(Self {
            shared: AsyncIO::new(operation.shared(), log)?,
        })
    }

    pub(crate) fn shared(&self) -> &AsyncIO {
        &self.shared
    }

    pub fn replace(&self, path: PathBuf, sg: SGData) -> AsyncIOResult<()> {
        let (tx, rx) = mpsc::channel();
        self.shared
            .tx
            .send(Message::Write(WriteArgs {
                path,
                data: sg,
                idempotent: false,
                complete_tx: tx,
            }))
            .expect("aio tx closed: replace");
        AsyncIOResult { rx }
    }

    pub fn remove(&self, path: PathBuf) -> AsyncIOResult<()> {
        let (tx, rx) = mpsc::channel();
        self.shared
            .tx
            .send(Message::Remove(path, tx))
            .expect("aio tx closed: remove");
        AsyncIOResult { rx }
    }

    pub fn remove_dir_all(&self, path: PathBuf) -> AsyncIOResult<()> {
        let (tx, rx) = mpsc::channel();
        self.shared
            .tx
            .send(Message::RemoveDirAll(path, tx))
            .expect("aio tx closed: remove_dir_all");
        AsyncIOResult { rx }
    }

    pub fn rename(&self, src: PathBuf, dst: PathBuf) -> AsyncIOResult<()> {
        let (tx, rx) = mpsc::channel();
        self.shared
            .tx
            .send(Message::Rename(src, dst, tx))
            .expect("aio tx closed: rename");
        AsyncIOResult { rx }
    }
}

impl Drop for AsyncIO {
    fn drop(&mut self) {
        // It is important that the tx is dropped before `shared` is.
        // Otherwise join on worker threads will hang, as they are never
        // going to receive termination.
        AutoOption::take_unchecked(&mut self.tx);
    }
}
// }}}

// {{{ AsyncIOShared & internals
/// Arc-ed shared `AsyncIO` data
///
/// Bunch of stuff shared between each thread of the worker pool
pub struct AsyncIOShared {
    join: Vec<thread::JoinHandle<()>>,
    log: slog::Logger,
    stats: AsyncIOThreadShared,
}

impl Drop for AsyncIOShared {
    fn drop(&mut self) {
        trace!(self.log, "Waiting for all threads to finish");
        for join in self.join.drain(..) {
            join.join().expect("AsyncIO worker thread panicked")
        }
    }
}

struct AsyncIOSharedInner {
    /// Keeps tracks of `write` stats.
    write_stats: WriteStats,
    /// PathBufs being currently processed by the pool.
    /// Used to synchronize operations between each other.
    in_progress: HashSet<PathBuf>,
}

impl Drop for AsyncIOSharedInner {
    fn drop(&mut self) {
        debug_assert!(self.in_progress.is_empty());
    }
}

#[derive(Clone)]
pub struct AsyncIOThreadShared {
    inner: Arc<Mutex<AsyncIOSharedInner>>,
}

impl AsyncIOThreadShared {
    pub fn new() -> Self {
        let inner = AsyncIOSharedInner {
            write_stats: WriteStats {
                new_bytes: 0,
                new_chunks: 0,
            },
            in_progress: Default::default(),
        };

        AsyncIOThreadShared {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    pub fn get_stats(&self) -> WriteStats {
        let sh = self.inner.lock().unwrap();
        sh.write_stats.clone()
    }
}
// }}}

// {{{ AsyncIOThread
/// A single thread in the worker pool.
struct AsyncIOThread {
    shared: AsyncIOThreadShared,
    rx: crossbeam_channel::Receiver<Message>,
    log: Logger,
    time_reporter: TimeReporter,
    backend: RefCell<ProtectedThread>,
}

/// Guard that removes entry from the pending paths on drop
struct PendingGuard<'a, 'b>(&'a AsyncIOThread, &'b Path);

impl Drop for PendingGuard<'_, '_> {
    fn drop(&mut self) {
        let mut sh = self.0.shared.inner.lock().unwrap();
        sh.in_progress.remove(self.1);
    }
}

impl AsyncIOThread {
    fn new(
        shared: AsyncIOThreadShared,
        rx: crossbeam_channel::Receiver<Message>,
        backend: ProtectedThread,
        log: Logger,
    ) -> Self {
        let t = TimeReporter::new_with_level(
            "chunk-writer",
            log.clone(),
            Level::Debug,
        );
        AsyncIOThread {
            log: log.new(o!("module" => "asyncio")),
            shared,
            rx,
            time_reporter: t,
            backend: RefCell::new(backend),
        }
    }

    pub fn run(&mut self) {
        loop {
            self.time_reporter.start("rx");

            if let Ok(msg) = self.rx.recv() {
                match msg {
                    Message::PromoteChunk(promotion, sender) => {
                        let result = self
                            .backend
                            .borrow_mut()
                            .thread
                            .promote_chunk(promotion);
                        let _ = sender.send(result);
                    }

                    Message::Write(WriteArgs {
                        path,
                        data,
                        idempotent,
                        complete_tx,
                    }) => self.write(path, data, idempotent, complete_tx),
                    Message::Read(path, tx) => self.read(path, tx),
                    Message::ReadMetadata(path, tx) => {
                        self.read_metadata(path, tx)
                    }
                    Message::List(path, tx) => self.list(path, tx),
                    Message::ListRecursively(path, tx) => {
                        self.list_recursively(path, tx)
                    }
                    Message::Remove(path, tx) => self.remove(path, tx),
                    Message::RemoveDirAll(path, tx) => {
                        self.remove_dir_all(path, tx)
                    }
                    Message::Rename(src_path, dst_path, tx) => {
                        self.rename(src_path, dst_path, tx)
                    }
                }
            } else {
                break;
            }
        }
    }

    fn write_inner(
        &mut self,
        path: PathBuf,
        sg: SGData,
        idempotent: bool,
    ) -> io::Result<()> {
        // check `in_progress` and add atomically
        // if not already there
        loop {
            let mut sh = self.shared.inner.lock().unwrap();

            if sh.in_progress.contains(&path) {
                // A completed write must be visible before an idempotent
                // caller can decide that the existing value is equivalent.
                drop(sh);
                thread::sleep(std::time::Duration::from_millis(100));
            } else {
                sh.in_progress.insert(path.clone());
                break;
            }
        }

        let len = sg.len();
        let res = self.backend.borrow_mut().thread.write(
            path.clone(),
            sg,
            idempotent,
        );
        {
            let mut sh = self.shared.inner.lock().unwrap();
            sh.in_progress.remove(&path);
            sh.write_stats.new_bytes += len as u64;
            sh.write_stats.new_chunks += 1;
        }

        res
    }

    fn write(
        &mut self,
        path: PathBuf,
        sg: SGData,
        idempotent: bool,
        tx: mpsc::Sender<io::Result<()>>,
    ) {
        trace!(self.log, "write"; "path" => %path.display());

        self.time_reporter.start("read");
        let res = self.write_inner(path, sg, idempotent);

        // A cancelled consumer can drop its receipt while storage finishes.
        let _ = tx.send(res);
    }

    fn pending_wait_and_insert<'a, 'path>(
        &'a self,
        path: &'path Path,
    ) -> PendingGuard<'a, 'path> {
        loop {
            let mut sh = self.shared.inner.lock().unwrap();

            if sh.in_progress.contains(path) {
                // a bit lame, but will do, since this should not really
                // happen in practice anyway
                drop(sh);
                thread::sleep(std::time::Duration::from_millis(1000));
            } else {
                sh.in_progress.insert(path.to_path_buf());
                break;
            }
        }
        PendingGuard(self, path)
    }

    fn read(&mut self, path: PathBuf, tx: mpsc::Sender<io::Result<SGData>>) {
        trace!(self.log, "read"; "path" => %path.display());

        self.time_reporter.start("read");
        let res = {
            let _guard = self.pending_wait_and_insert(&path);
            self.backend.borrow_mut().thread.read(path.clone())
        };
        self.time_reporter.start("read send response");
        let _ = tx.send(res);
    }

    fn read_metadata(
        &mut self,
        path: PathBuf,
        tx: mpsc::Sender<io::Result<Metadata>>,
    ) {
        trace!(self.log, "read-metadata"; "path" => %path.display());

        self.time_reporter.start("read-metadata");
        let res = {
            let _guard = self.pending_wait_and_insert(&path);
            self.backend.borrow_mut().thread.read_metadata(path.clone())
        };

        self.time_reporter.start("read send response");
        let _ = tx.send(res);
    }

    fn list(
        &mut self,
        path: PathBuf,
        tx: mpsc::Sender<io::Result<Vec<PathBuf>>>,
    ) {
        trace!(self.log, "list"; "path" => %path.display());

        self.time_reporter.start("list");
        let res = self.backend.borrow_mut().thread.list(path);
        self.time_reporter.start("list send response");
        let _ = tx.send(res);
    }

    fn list_recursively(
        &mut self,
        path: PathBuf,
        tx: mpsc::Sender<io::Result<Vec<PathBuf>>>,
    ) {
        trace!(self.log, "list"; "path" => %path.display());
        self.time_reporter.start("list");

        self.backend.borrow_mut().thread.list_recursively(path, tx)
    }

    fn remove(&mut self, path: PathBuf, tx: mpsc::Sender<io::Result<()>>) {
        trace!(self.log, "remove"; "path" => %path.display());

        self.time_reporter.start("remove");
        let res = {
            let _guard = self.pending_wait_and_insert(&path);
            self.backend.borrow_mut().thread.remove(path.clone())
        };
        self.time_reporter.start("remove send response");
        let _ = tx.send(res);
    }

    fn remove_dir_all(
        &mut self,
        path: PathBuf,
        tx: mpsc::Sender<io::Result<()>>,
    ) {
        trace!(self.log, "remove-dir-all"; "path" => %path.display());

        self.time_reporter.start("remove-dir-all");
        let res = self.backend.borrow_mut().thread.remove_dir_all(path);

        self.time_reporter.start("remove send response");
        let _ = tx.send(res);
    }

    fn rename(
        &mut self,
        src_path: PathBuf,
        dst_path: PathBuf,
        tx: mpsc::Sender<io::Result<()>>,
    ) {
        trace!(
            self.log,
            "rename";
            "src-path" => %src_path.display(),
            "dst-path" => %dst_path.display()
        );

        self.time_reporter.start("rename");
        let res = {
            let _guard = self.pending_wait_and_insert(&src_path);
            let _guard = self.pending_wait_and_insert(&dst_path);
            self.backend
                .borrow_mut()
                .thread
                .rename(src_path.clone(), dst_path.clone())
        };
        self.time_reporter.start("remove send response");
        let _ = tx.send(res);
    }
}
// }}}

/// Converts the given `url` into a backend instance.
///
/// # Panics
///
/// Panics if the `url` specifies a supported backend schema that is not enabled
/// through future flags
//
// ```norust
// let s = "file:/foo/bar";
// let s = "b2:myid#bucket";
// ```
pub(crate) fn backend_from_url(
    url: &Url,
) -> io::Result<Box<dyn Backend + Send + Sync>> {
    if url.scheme() == "file" {
        return Ok(Box::new(Local::new(url.to_file_path().unwrap())));
    } else if url.scheme() == "http" || url.scheme() == "https" {
        #[cfg(feature = "backend-http")]
        {
            return Ok(Box::new(HttpReadOnly::new(url.clone())));
        }

        #[cfg(not(feature = "backend-http"))]
        {
            panic!("HTTP backend feature is not enabled");
        }
    } else if url.scheme() == "b2" {
        #[cfg(feature = "backend-b2")]
        {
            let id = url.path();
            let bucket = url.fragment().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "bucket in the url missing",
                )
            })?;
            let key = std::env::var_os("RDEDUP_B2_KEY")
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "RDEDUP_B2_KEY environment variable not found",
                    )
                })?
                .into_string()
                .map_err(|os_string| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "b2 key is not utf8 string: {}",
                            os_string.to_string_lossy()
                        ),
                    )
                })?;
            return Ok(Box::new(B2::new(id, bucket, &key)));
        }

        #[cfg(not(feature = "backend-b2"))]
        {
            panic!("Backblaze B2 backend feature not enabled");
        }
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("Unsupported scheme: {}", url.scheme()),
    ))
}

// vim: foldmethod=marker foldmarker={{{,}}}
