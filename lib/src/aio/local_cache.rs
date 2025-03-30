use std::path::PathBuf;
use std::sync::mpsc::Sender;
use sgdata::SGData;
use crate::aio::{Local, Metadata};
use crate::backends::{Backend, BackendThread, Lock};

//#[derive(Debug)]
pub struct LocalCache {
    local: Box<Local>,
    remote: Box<dyn Backend>,
}

struct CombinedLocks {
    locks: Vec<Box<dyn Lock>>
}

impl CombinedLocks {
    fn new(locks: Vec<Box<dyn Lock>>) -> Self {
        CombinedLocks { locks }
    }
}

impl Lock for CombinedLocks {}

pub struct LocalCacheThread {
    local: Box<dyn BackendThread>,
    remote: Box<dyn BackendThread>,
}

impl LocalCache {
    pub fn new(path: PathBuf, remote: Box<dyn Backend>) -> Self {
       LocalCache { local: Box::new(Local::new(path)), remote }
    }
}

impl Backend for LocalCache {
    fn lock_exclusive(&self) -> std::io::Result<Box<dyn Lock>> {
        let remote_lock = self.remote.lock_exclusive()?;
        let local_lock = self.local.lock_exclusive()?;
        Ok(Box::new(CombinedLocks::new(vec![remote_lock, local_lock])))
    }

    fn lock_shared(&self) -> std::io::Result<Box<dyn Lock>> {
        let remote_lock = self.remote.lock_shared()?;
        // We lock the local cache with an exclusive lock because any read cache miss will trigger
        // a write operation to the local cache to cache the remotely-read chunk. Currently, the
        // lock cannot be acquired and released granularly only when needed. This makes a cache-repo
        // unable to be used even in read-only mode concurrently as of the current implementation.
        let local_lock = self.local.lock_exclusive()?;
        Ok(Box::new(CombinedLocks::new(vec![remote_lock, local_lock])))
    }

    fn new_thread(&self) -> std::io::Result<Box<dyn BackendThread>> {
        let local_thread = self.local.new_thread()?;
        let remote_thread = self.remote.new_thread()?;
        Ok(
            Box::new(LocalCacheThread {
                local: local_thread,
                remote: remote_thread
            })
        )
    }
}

impl BackendThread for LocalCacheThread {
    // We generally delegate the write operation to the remote store first; and then duplicate them locally if it succeeded only.
    // Read operations try to hit the local cache first, and delegate to the remote on error (assuming there was no cached value).

    fn remove_dir_all(&mut self, path: PathBuf) -> std::io::Result<()> {
        let result = self.remote.remove_dir_all(path.clone());
        match result {
            Ok(()) => self.local.remove_dir_all(path),
            Err(e) => Err(e)
        }
    }

    fn rename(&mut self, src_path: PathBuf, dst_path: PathBuf) -> std::io::Result<()> {
        let result = self.remote.rename(src_path.clone(), dst_path.clone());
        match result {
            Ok(()) => self.local.rename(src_path, dst_path),
            Err(e) => Err(e)
        }
    }

    fn write(&mut self, path: PathBuf, sg: SGData, idempotent: bool) -> std::io::Result<()> {
        let result = self.remote.write(path.clone(), sg.clone(), idempotent);
        match result {
            Ok(()) => self.local.write(path, sg, idempotent),
            Err(e) => Err(e)
        }
    }

    fn read(&mut self, path: PathBuf) -> std::io::Result<SGData> {
        let result = self.local.read(path.clone());
        match result {
            Ok(data) => {
                Ok(data)
            },
            Err(_) => { // TODO: check if different errors can occur, and only fetch from remote when it's an expected "no such file" or similar error?
                match self.remote.read(path.clone()) {
                    Ok(data) => {
                        let cache_result = self.local.write(path, data.clone(), false);
                        if cache_result.is_err() {
                            // Return an Err anyway? Or how can we both soft-report and error without critically failing since
                            // technically we do have the data. Anything better than eprintln?
                            eprintln!("Successfully read data from remote; but failed to cache it!");
                        }
                        Ok(data)
                    },
                    Err(e) => Err(e)
                }
            }
        }
    }

    fn remove(&mut self, path: PathBuf) -> std::io::Result<()> {
        let result = self.remote.remove(path.clone());
        match result {
            Ok(()) => self.local.remove(path),
            Err(e) => Err(e)
        }
    }

    fn read_metadata(&mut self, path: PathBuf) -> std::io::Result<Metadata> {
        self.remote.read_metadata(path.clone())
    }

    // Simply rely on the remote as the ground truth for listing
    fn list(&mut self, path: PathBuf) -> std::io::Result<Vec<PathBuf>> {
        self.remote.list(path)
    }

    fn list_recursively(&mut self, path: PathBuf, tx: Sender<std::io::Result<Vec<PathBuf>>>) {
        self.remote.list_recursively(path, tx)
    }
}
