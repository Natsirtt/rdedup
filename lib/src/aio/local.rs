// {{{ use and mod
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::{fs, io, mem};

use rand::distr::Alphanumeric;
use rand::Rng;
use sgdata::SGData;
use walkdir::WalkDir;

use super::backend::{
    Backend, BackendOperation, BackendThread, Exclusive, OperationState, Shared,
};
use super::Metadata;
use crate::config;
use crate::INGRESS_BUFFER_SIZE;
// }}}

pub(crate) fn lock_file_path(path: &Path) -> PathBuf {
    path.join(config::LOCK_FILE)
}

#[derive(Debug)]
pub struct Local {
    path: PathBuf,
}

#[derive(Debug)]
pub struct LocalThread {
    path: PathBuf,
    rand_ext: String,
}

struct LocalOperation {
    path: PathBuf,
    _lock: fs::File,
}

impl Backend for Local {
    fn begin_exclusive(&self) -> io::Result<BackendOperation<Exclusive>> {
        let file = self.open_lock_file()?;
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(BackendOperation::new(LocalOperation {
            path: self.path.clone(),
            _lock: file,
        }))
    }

    fn begin_shared(&self) -> io::Result<BackendOperation<Shared>> {
        let file = self.open_lock_file()?;
        fs2::FileExt::lock_shared(&file)?;
        Ok(BackendOperation::new(LocalOperation {
            path: self.path.clone(),
            _lock: file,
        }))
    }
}

impl OperationState for LocalOperation {
    fn new_thread(&self) -> io::Result<Box<dyn BackendThread>> {
        Ok(Box::new(LocalThread {
            path: self.path.clone(),
            rand_ext: rand::rng()
                .sample_iter(&Alphanumeric)
                .take(20)
                .map(char::from)
                .collect(),
        }))
    }
}

impl Local {
    fn open_lock_file(&self) -> io::Result<fs::File> {
        fs::create_dir_all(&self.path)?;
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_file_path(&self.path))
    }

    pub fn new(path: PathBuf) -> Self {
        Local { path }
    }
}

impl BackendThread for LocalThread {
    fn promote_chunk(
        &mut self,
        promotion: super::promotion::ChunkPromotion,
    ) -> io::Result<()> {
        let source = self.path.join(promotion.source_path());
        let destination = self.path.join(promotion.destination_path());
        // Validated chunk paths have a generation and chunk directory, so the
        // destination has a parent regardless of repository nesting depth.
        let parent = destination.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "chunk destination has no parent",
            )
        })?;
        fs::create_dir_all(parent)?;
        match fs::hard_link(&source, &destination) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if fs::metadata(&destination)?.is_file() {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "chunk destination is not a file",
                    ))
                }
            }
            Err(error) => Err(error),
        }
    }

    fn remove_dir_all(&mut self, path: PathBuf) -> io::Result<()> {
        let path = self.path.join(path);
        fs::remove_dir_all(&path)
    }

    fn rename(
        &mut self,
        src_path: PathBuf,
        dst_path: PathBuf,
    ) -> io::Result<()> {
        let src_path = self.path.join(src_path);
        let dst_path = self.path.join(dst_path);

        match fs::rename(&src_path, &dst_path) {
            Ok(_) => Ok(()),
            Err(_e) => {
                fs::create_dir_all(dst_path.parent().unwrap())?;
                fs::rename(&src_path, &dst_path)
            }
        }
    }

    fn write(
        &mut self,
        path: PathBuf,
        sg: SGData,
        idempotent: bool,
    ) -> io::Result<()> {
        let path = self.path.join(path);
        // check if exists on disk
        // remove from `in_progress` if it does
        if idempotent && path.exists() {
            return Ok(());
        }

        let tmp_path = path.with_extension(format!("{}.tmp", self.rand_ext));
        let mut chunk_file = match fs::File::create(&tmp_path) {
            Ok(file) => Ok(file),
            Err(_) => {
                fs::create_dir_all(path.parent().unwrap())?;
                fs::File::create(&tmp_path)
            }
        }?;

        for data_part in sg.as_parts() {
            chunk_file.write_all(data_part)?;
        }

        chunk_file.sync_data()?;
        drop(chunk_file);

        if idempotent {
            match fs::hard_link(&tmp_path, &path) {
                Ok(()) => {
                    let _ = fs::remove_file(&tmp_path);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let _ = fs::remove_file(&tmp_path);
                }
                Err(error) => {
                    let _ = fs::remove_file(&tmp_path);
                    return Err(error);
                }
            }
        } else {
            fs::rename(&tmp_path, &path)?;
        }

        Ok(())
    }

    fn read(&mut self, path: PathBuf) -> io::Result<SGData> {
        let path = self.path.join(path);

        let mut file = fs::File::open(&path)?;

        let mut bufs = Vec::with_capacity(16 * 1024 / INGRESS_BUFFER_SIZE);
        loop {
            let mut buf: Vec<u8> = vec![0u8; INGRESS_BUFFER_SIZE];
            let len = file.read(&mut buf[..])?;

            if len == 0 {
                return Ok(SGData::from_many(bufs));
            }
            buf.truncate(len);
            bufs.push(buf);
        }
    }

    fn remove(&mut self, path: PathBuf) -> io::Result<()> {
        let path = self.path.join(path);
        fs::remove_file(&path)
    }

    fn read_metadata(&mut self, path: PathBuf) -> io::Result<Metadata> {
        let path = self.path.join(path);
        let md = fs::metadata(&path)?;
        let created = if let Ok(created) = md.created().map(Into::into) {
            created
        } else if let Ok(modified) = md.modified().map(Into::into) {
            modified
        } else {
            return Err(io::Error::other(format!(
                "filesystem metadata does not contain `created` or `modified` for {}",
                path.display()
            )));
        };
        Ok(Metadata {
            len: md.len(),
            is_file: md.is_file(),
            created,
        })
    }

    fn list(&mut self, path: PathBuf) -> io::Result<Vec<PathBuf>> {
        let path = self.path.join(path);
        let mut v = Vec::with_capacity(128);

        let dir = fs::read_dir(path);

        match dir {
            Ok(dir) => {
                for entry in dir {
                    let entry = entry?;
                    v.push(entry.path());
                }
                Ok(v)
            }
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e),
        }
    }

    fn list_recursively(
        &mut self,
        path: PathBuf,
        tx: mpsc::Sender<io::Result<Vec<PathBuf>>>,
    ) {
        let path = self.path.join(path);

        if !path.exists() {
            return;
        }

        let mut v = Vec::with_capacity(128);

        for path in WalkDir::new(path) {
            match path {
                Ok(path) => {
                    if !path.file_type().is_file() {
                        continue;
                    }
                    v.push(path.path().into());
                    if v.len() > 100 {
                        tx.send(Ok(mem::take(&mut v))).expect("send failed")
                    }
                }
                Err(e) => tx.send(Err(e.into())).expect("send failed"),
            }
        }
        if !v.is_empty() {
            tx.send(Ok(v)).expect("send failed")
        }
    }
}

// vim: foldmethod=marker foldmarker={{{,}}}

#[cfg(test)]
mod tests {
    use super::{lock_file_path, Local};
    use crate::backends::Backend;
    use fs2::FileExt;
    use std::fs::{self, File};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct RepositoryDirectory(PathBuf);

    impl RepositoryDirectory {
        fn new() -> Self {
            // This counter distinguishes temporary resources, not test inputs.
            static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "rdedup-operation-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn observer(&self) -> File {
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(lock_file_path(&self.0))
                .unwrap()
        }
    }

    impl Drop for RepositoryDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn concurrent_shared_promotions_preserve_source_and_destination() {
        let directory = RepositoryDirectory::new();
        let backend = Local::new(directory.0.clone());
        let source = PathBuf::from(format!(
            "0000000000000000-0000000000000000/chunk/{}",
            "ab".repeat(32)
        ));
        let destination = PathBuf::from(format!(
            "0000000000000001-0000000000000000/chunk/{}",
            "ab".repeat(32)
        ));
        let promotion = crate::backends::ChunkPromotion::new(
            source.clone(),
            destination.clone(),
        )
        .unwrap();
        let operation = backend.begin_shared().unwrap();
        let mut first = operation.new_thread().unwrap();
        first
            .create(
                source.clone(),
                sgdata::SGData::from_single(b"stored chunk".to_vec()),
            )
            .unwrap();
        let mut second = operation.new_thread().unwrap();
        let other_promotion = promotion.clone();
        std::thread::scope(|scope| {
            let first_write =
                scope.spawn(move || first.promote_chunk(promotion));
            let second_write =
                scope.spawn(move || second.promote_chunk(other_promotion));
            first_write.join().unwrap().unwrap();
            second_write.join().unwrap().unwrap();
        });
        assert_eq!(
            fs::read(directory.0.join(&source)).unwrap(),
            b"stored chunk"
        );
        assert_eq!(
            fs::read(directory.0.join(&destination)).unwrap(),
            b"stored chunk"
        );
        drop(operation);
        let exclusive = backend.begin_exclusive().unwrap();
        exclusive.new_thread().unwrap().remove(source).unwrap();
        assert_eq!(
            fs::read(directory.0.join(destination)).unwrap(),
            b"stored chunk"
        );
    }

    #[test]
    fn shared_worker_keeps_exclusive_access_blocked_after_operation_drop() {
        let directory = RepositoryDirectory::new();
        let backend = Local::new(directory.0.clone());
        let operation = backend.begin_shared().unwrap();
        let mut worker = operation.new_thread().unwrap();
        let observer = directory.observer();
        drop(operation);
        drop(backend);

        assert!(observer.try_lock_exclusive().is_err());
        worker.list(PathBuf::new()).unwrap();
        drop(worker);
        observer.try_lock_exclusive().unwrap();
    }

    #[test]
    fn exclusive_worker_keeps_readers_blocked_after_operation_drop() {
        let directory = RepositoryDirectory::new();
        let backend = Local::new(directory.0.clone());
        let operation = backend.begin_exclusive().unwrap();
        let worker = operation.new_thread().unwrap();
        let observer = directory.observer();
        drop(operation);

        assert!(FileExt::try_lock_shared(&observer).is_err());
        drop(worker);
        FileExt::try_lock_shared(&observer).unwrap();
    }

    #[test]
    fn dropping_one_shared_operation_does_not_release_another() {
        let directory = RepositoryDirectory::new();
        let backend = Local::new(directory.0.clone());
        let first = backend.begin_shared().unwrap();
        let second = backend.begin_shared().unwrap();
        let observer = directory.observer();

        drop(first);
        assert!(observer.try_lock_exclusive().is_err());
        drop(second);
        observer.try_lock_exclusive().unwrap();
    }

    #[test]
    fn restricting_an_exclusive_operation_retains_exclusive_protection() {
        let directory = RepositoryDirectory::new();
        let backend = Local::new(directory.0.clone());
        let exclusive = backend.begin_exclusive().unwrap();
        let shared_capability = exclusive.shared();
        let observer = directory.observer();
        drop(exclusive);

        assert!(FileExt::try_lock_shared(&observer).is_err());
        drop(shared_capability);
        FileExt::try_lock_shared(&observer).unwrap();
    }
}
