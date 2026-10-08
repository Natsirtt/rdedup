use crate::backends::local::Local;
use crate::backends::{Backend, BackendThread, Lock, Metadata};
use crate::{settings, BackendSelectFn, Repo};
use sgdata::SGData;
use std::io::{self, Cursor};
use std::path::PathBuf;
use std::sync::Arc;
use url::Url;

struct HeldLock {
    _lock: Box<dyn Lock>,
}

impl Lock for HeldLock {}

struct FailingWriteBackend {
    local: Arc<Local>,
}

impl Backend for FailingWriteBackend {
    fn lock_exclusive(&self) -> io::Result<Box<dyn Lock>> {
        Ok(Box::new(HeldLock {
            _lock: self.local.lock_exclusive()?,
        }))
    }

    fn lock_shared(&self) -> io::Result<Box<dyn Lock>> {
        Ok(Box::new(HeldLock {
            _lock: self.local.lock_shared()?,
        }))
    }

    fn new_thread(&self) -> io::Result<Box<dyn BackendThread>> {
        Ok(Box::new(FailingWriteThread {
            local: self.local.new_thread()?,
        }))
    }
}

struct FailingWriteThread {
    local: Box<dyn BackendThread>,
}

impl BackendThread for FailingWriteThread {
    fn remove_dir_all(&mut self, path: PathBuf) -> io::Result<()> {
        self.local.remove_dir_all(path)
    }

    fn rename(
        &mut self,
        src_path: PathBuf,
        dst_path: PathBuf,
    ) -> io::Result<()> {
        self.local.rename(src_path, dst_path)
    }

    fn write(
        &mut self,
        path: PathBuf,
        data: SGData,
        idempotent: bool,
    ) -> io::Result<()> {
        if idempotent {
            Err(io::Error::other("injected chunk write failure"))
        } else {
            self.local.write(path, data, idempotent)
        }
    }

    fn read(&mut self, path: PathBuf) -> io::Result<SGData> {
        self.local.read(path)
    }

    fn remove(&mut self, path: PathBuf) -> io::Result<()> {
        self.local.remove(path)
    }

    fn read_metadata(&mut self, path: PathBuf) -> io::Result<Metadata> {
        self.local.read_metadata(path)
    }

    fn list(&mut self, path: PathBuf) -> io::Result<Vec<PathBuf>> {
        self.local.list(path)
    }

    fn list_recursively(
        &mut self,
        path: PathBuf,
        tx: std::sync::mpsc::Sender<io::Result<Vec<PathBuf>>>,
    ) {
        self.local.list_recursively(path, tx)
    }
}

struct RepositoryDirectory(PathBuf);

impl Drop for RepositoryDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn chunk_write_failure_returns_to_caller_without_publishing_name() {
    let repository_path = std::env::temp_dir().join(format!(
        "rdedup-chunk-failure-test-{}",
        uuid::Uuid::new_v4()
    ));
    let _repository_directory = RepositoryDirectory(repository_path.clone());
    let mut repository_settings = settings::Repo::new();
    repository_settings
        .set_compression(settings::Compression::None)
        .unwrap();
    drop(
        Repo::init_from_url(
            Arc::new(Url::from_file_path(&repository_path).unwrap()),
            &|| Ok(String::new()),
            repository_settings,
            None,
        )
        .unwrap(),
    );

    let local = Arc::new(Local::new(repository_path.clone()));
    let backend: Arc<BackendSelectFn> = Arc::new(move || {
        Ok(Box::new(FailingWriteBackend {
            local: local.clone(),
        }) as Box<dyn Backend + Send + Sync>)
    });
    let repository = Repo::open(backend, None).unwrap();
    let encryption = repository.unlock_encrypt(&|| Ok(String::new())).unwrap();

    let error = repository
        .write(
            "must-not-publish",
            Cursor::new(b"artifact bytes"),
            &encryption,
        )
        .unwrap_err();
    assert!(error.to_string().contains("failed to write chunk"));

    let local_repository = Repo::open_from_url(
        Arc::new(Url::from_file_path(repository_path).unwrap()),
        None,
    )
    .unwrap();
    assert!(local_repository.list_names().unwrap().is_empty());
}
