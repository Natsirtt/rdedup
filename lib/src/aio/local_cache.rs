use super::backend::ProtectedThread;
use crate::aio::{Local, Metadata};
use crate::backends::{
    Backend, BackendOperation, BackendThread, Exclusive, OperationState, Shared,
};
use sgdata::SGData;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

pub struct LocalCache {
    local: Box<Local>,
    remote: Box<dyn Backend>,
}

struct CacheOperation {
    local: BackendOperation<Shared>,
    remote: BackendOperation<Shared>,
}

impl OperationState for CacheOperation {
    fn new_thread(&self) -> std::io::Result<Box<dyn BackendThread>> {
        Ok(Box::new(LocalCacheThread {
            local: self.local.new_thread()?.into_protected_thread(),
            remote: self.remote.new_thread()?.into_protected_thread(),
        }))
    }
}

pub struct LocalCacheThread {
    local: ProtectedThread,
    remote: ProtectedThread,
}

impl LocalCache {
    pub fn new(path: PathBuf, remote: Box<dyn Backend>) -> Self {
        LocalCache {
            local: Box::new(Local::new(path)),
            remote,
        }
    }
}

impl Backend for LocalCache {
    fn begin_exclusive(&self) -> std::io::Result<BackendOperation<Exclusive>> {
        let remote = self.remote.begin_exclusive()?;
        let local = self.local.begin_exclusive()?;
        Ok(BackendOperation::new(CacheOperation {
            local: local.shared(),
            remote: remote.shared(),
        }))
    }

    fn begin_shared(&self) -> std::io::Result<BackendOperation<Shared>> {
        let remote = self.remote.begin_shared()?;
        // Cache fills create missing entries atomically. Shared protection also
        // excludes destructive cache maintenance without serializing readers.
        let local = self.local.begin_shared()?;
        Ok(BackendOperation::new(CacheOperation { local, remote }))
    }
}

impl BackendThread for LocalCacheThread {
    fn promote_chunk(
        &mut self,
        promotion: super::promotion::ChunkPromotion,
    ) -> std::io::Result<()> {
        self.remote.thread.promote_chunk(promotion.clone())?;
        // Cache entries are optional; only promote a locally present source.
        match self
            .local
            .thread
            .read_metadata(promotion.source_path().to_path_buf())
        {
            Ok(_) => self.local.thread.promote_chunk(promotion),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    // Writes reach the authoritative store before updating the optional cache.
    // Reads fall back to the remote only when the cached entry is absent.

    fn remove_dir_all(&mut self, path: PathBuf) -> std::io::Result<()> {
        let result = self.remote.thread.remove_dir_all(path.clone());
        match result {
            Ok(()) => self.local.thread.remove_dir_all(path),
            Err(e) => Err(e),
        }
    }

    fn rename(
        &mut self,
        src_path: PathBuf,
        dst_path: PathBuf,
    ) -> std::io::Result<()> {
        let result = self
            .remote
            .thread
            .rename(src_path.clone(), dst_path.clone());
        match result {
            Ok(()) => self.local.thread.rename(src_path, dst_path),
            Err(e) => Err(e),
        }
    }

    fn write(
        &mut self,
        path: PathBuf,
        sg: SGData,
        idempotent: bool,
    ) -> std::io::Result<()> {
        let result =
            self.remote
                .thread
                .write(path.clone(), sg.clone(), idempotent);
        match result {
            Ok(()) => self.local.thread.write(path, sg, idempotent),
            Err(e) => Err(e),
        }
    }

    fn read(&mut self, path: PathBuf) -> std::io::Result<SGData> {
        let result = self.local.thread.read(path.clone());
        match result {
            Ok(data) => Ok(data),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match self.remote.thread.read(path.clone()) {
                    Ok(data) => {
                        let cache_result =
                            self.local.thread.write(path, data.clone(), true);
                        if cache_result.is_err() {
                            // The authoritative read succeeded; failure to fill
                            // this optional cache does not invalidate its bytes.
                            eprintln!("Successfully read data from remote; but failed to cache it!");
                        }
                        Ok(data)
                    }
                    Err(e) => Err(e),
                }
            }
            Err(error) => Err(error),
        }
    }

    fn remove(&mut self, path: PathBuf) -> std::io::Result<()> {
        let result = self.remote.thread.remove(path.clone());
        match result {
            Ok(()) => self.local.thread.remove(path),
            Err(e) => Err(e),
        }
    }

    fn read_metadata(&mut self, path: PathBuf) -> std::io::Result<Metadata> {
        self.remote.thread.read_metadata(path.clone())
    }

    // Simply rely on the remote as the ground truth for listing
    fn list(&mut self, path: PathBuf) -> std::io::Result<Vec<PathBuf>> {
        self.remote.thread.list(path)
    }

    fn list_recursively(
        &mut self,
        path: PathBuf,
        tx: Sender<std::io::Result<Vec<PathBuf>>>,
    ) {
        self.remote.thread.list_recursively(path, tx)
    }
}
