//! Independent processes exercise the same public protection API as repository work.
use rdedup_lib::backends::local::Local;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
enum Holder {
    Shared,
    Exclusive,
    Worker,
    Stream,
}

impl Holder {
    fn argument(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Exclusive => "exclusive",
            Self::Worker => "worker",
            Self::Stream => "stream",
        }
    }
}

struct HolderProcess {
    child: Child,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Drop for HolderProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl HolderProcess {
    fn start(repository: &Path, holder: Holder) -> Self {
        let mut process = Self {
            child: Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "lock_holder_process",
                    "--nocapture",
                ])
                .env("RDEDUP_LOCK_TEST_REPOSITORY", repository)
                .env("RDEDUP_LOCK_TEST_HOLDER", holder.argument())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
            reader: None,
        };
        let output = process.child.stdout.take().unwrap();
        let (ready, received) = mpsc::channel();
        process.reader = Some(std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if line.unwrap().ends_with("LOCK_READY") {
                    let _ = ready.send(());
                }
            }
        }));
        received.recv_timeout(Duration::from_secs(15)).unwrap();
        process
    }
    fn release(mut self) {
        self.child
            .stdin
            .take()
            .unwrap()
            .write_all(b"release\n")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(Instant::now() < deadline, "holder did not release");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn terminate(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

fn retain_until_release(protection: impl Sized) {
    println!("LOCK_READY");
    std::io::stdout().flush().unwrap();
    let mut command = String::new();
    std::io::stdin().read_line(&mut command).unwrap();
    assert_eq!(command.trim(), "release");
    drop(protection);
}

#[test]
#[ignore = "child process entry point; requires its parent's repository and control pipe"]
fn lock_holder_process() {
    let repository = std::env::var_os("RDEDUP_LOCK_TEST_REPOSITORY").unwrap();
    let storage = Local::new(repository.into());
    match std::env::var("RDEDUP_LOCK_TEST_HOLDER").unwrap().as_str() {
        "shared" => retain_until_release(storage.shared_operation().unwrap()),
        "exclusive" => {
            retain_until_release(storage.exclusive_operation().unwrap())
        }
        "worker" => {
            let operation = storage.shared_operation().unwrap();
            let worker = operation.workers().new_thread().unwrap();
            drop(operation);
            retain_until_release(worker);
        }
        "stream" => {
            let operation = storage.shared_operation().unwrap();
            let stream = operation.read_object(Path::new("payload")).unwrap();
            drop(operation);
            retain_until_release(stream);
        }
        mode => panic!("unknown holder: {mode}"),
    }
}

#[test]
fn independent_shared_holders_exclude_gc_until_the_last_holder_exits() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Local::new(directory.path().to_owned());
    drop(storage.try_exclusive_operation().unwrap());
    let first = HolderProcess::start(directory.path(), Holder::Shared);
    let second = HolderProcess::start(directory.path(), Holder::Shared);
    drop(storage.try_shared_operation().unwrap());
    assert!(matches!(
        storage.try_exclusive_operation(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    first.release();
    assert!(matches!(
        storage.try_exclusive_operation(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    second.release();
    drop(storage.try_exclusive_operation().unwrap());
}

#[test]
fn an_independent_exclusive_holder_blocks_reads_and_writes_until_terminated() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Local::new(directory.path().join("."));
    drop(storage.try_shared_operation().unwrap());
    let holder = HolderProcess::start(directory.path(), Holder::Exclusive);
    assert!(matches!(
        storage.try_shared_operation(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    assert!(matches!(
        storage.try_exclusive_operation(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    holder.terminate();
    drop(storage.try_shared_operation().unwrap());
    drop(storage.try_exclusive_operation().unwrap());
}

#[test]
fn independent_workers_and_streams_keep_the_original_protection_alive() {
    for kind in [Holder::Worker, Holder::Stream] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("payload"), b"retained stream")
            .unwrap();
        let storage = Local::new(directory.path().to_owned());
        let holder = HolderProcess::start(directory.path(), kind);
        assert!(matches!(
            storage.try_exclusive_operation(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        holder.release();
        drop(storage.try_exclusive_operation().unwrap());
    }
}

#[cfg(unix)]
#[test]
fn a_symbolic_repository_alias_uses_the_same_cross_process_lock() {
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    let alias = directory.path().join("alias");
    std::os::unix::fs::symlink(&repository, &alias).unwrap();
    let holder = HolderProcess::start(&repository, Holder::Exclusive);
    let storage = Local::new(alias);
    assert!(matches!(
        storage.try_shared_operation(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    holder.release();
    drop(storage.try_exclusive_operation().unwrap());
}

#[test]
fn inaccessible_lock_storage_is_not_contention() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join(".lock")).unwrap();
    let storage = Local::new(directory.path().to_path_buf());
    assert!(matches!(
        storage.try_shared_operation(),
        Err(std::fs::TryLockError::Error(_))
    ));
    assert!(matches!(
        storage.try_exclusive_operation(),
        Err(std::fs::TryLockError::Error(_))
    ));
}
