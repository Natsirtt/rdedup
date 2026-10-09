use rdedup_lib::backends::local::Local;
use rdedup_protocol::lease::{Acquisition, LeaseId, LeaseMode, LeaseRequestId};
use rdedup_protocol::problem::ProblemKind;
use rdedup_server::lease::{Error, LeasePolicy, LeaseService, Principal};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn owner() -> Principal {
    Principal::Credential("builder".parse().unwrap())
}

fn service(directory: &TempDir) -> LeaseService {
    let policy = LeasePolicy::new(
        Duration::from_secs(30),
        Duration::from_secs(60),
        Instant::now(),
    )
    .unwrap();
    LeaseService::start(directory.path().to_owned(), policy)
}

async fn granted(service: &LeaseService, request: LeaseRequestId) -> LeaseId {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match service.status(owner(), request).await.unwrap() {
                Acquisition::Granted { lease } => return lease.id,
                Acquisition::Queued => {
                    tokio::time::sleep(Duration::from_millis(5)).await
                }
                Acquisition::Closed => {
                    panic!("request closed instead of being granted")
                }
            }
        }
    })
    .await
    .expect("lease acquisition timed out")
}

#[tokio::test]
async fn released_upload_blocks_exclusive_until_its_staging_file_is_closed() {
    let directory = TempDir::new().unwrap();
    let service = service(&directory);
    let shared_request = LeaseRequestId::generate();
    service
        .acquire(owner(), shared_request, LeaseMode::Shared)
        .await
        .unwrap();
    let shared = granted(&service, shared_request).await;
    let permit = service.writable(owner(), shared).await.unwrap();
    let mut upload = permit.create_object(Path::new("pending-object")).unwrap();
    upload.write_all(b"incomplete").unwrap();
    drop(permit);
    service.release(owner(), shared).await.unwrap();

    let exclusive_request = LeaseRequestId::generate();
    service
        .acquire(owner(), exclusive_request, LeaseMode::Exclusive)
        .await
        .unwrap();
    assert_eq!(
        service.status(owner(), exclusive_request).await.unwrap(),
        Acquisition::Queued
    );
    assert!(Local::new(directory.path().to_owned())
        .try_exclusive_operation()
        .is_err());
    assert!(upload.commit().is_err());
    assert!(!directory.path().join("pending-object").exists());
    let exclusive = granted(&service, exclusive_request).await;
    assert!(service.exclusive(owner(), exclusive).await.is_ok());
    service.release(owner(), exclusive).await.unwrap();
}

#[tokio::test]
async fn shared_leases_cannot_obtain_destructive_permission_or_cross_owners() {
    let directory = TempDir::new().unwrap();
    let service = service(&directory);
    let request = LeaseRequestId::generate();
    service
        .acquire(owner(), request, LeaseMode::Shared)
        .await
        .unwrap();
    let lease = granted(&service, request).await;
    assert!(matches!(
        service.exclusive(owner(), lease).await,
        Err(Error::Rejected(ProblemKind::LeaseMode))
    ));
    let other = Principal::Credential("another-builder".parse().unwrap());
    assert!(matches!(
        service.shared(other.clone(), lease).await,
        Err(Error::Rejected(ProblemKind::Forbidden))
    ));
    assert!(matches!(
        service.acquire(other, request, LeaseMode::Shared).await,
        Err(Error::Rejected(ProblemKind::Forbidden))
    ));
    assert!(matches!(
        service
            .acquire(
                Principal::Anonymous,
                LeaseRequestId::generate(),
                LeaseMode::Exclusive
            )
            .await,
        Err(Error::Rejected(ProblemKind::Unauthorized))
    ));
}

#[tokio::test]
async fn an_external_local_operation_blocks_remote_exclusive_admission() {
    let directory = TempDir::new().unwrap();
    let local = Local::new(directory.path().to_owned())
        .shared_operation()
        .unwrap();
    let service = service(&directory);
    let request = LeaseRequestId::generate();
    service
        .acquire(owner(), request, LeaseMode::Exclusive)
        .await
        .unwrap();
    // Polling remains responsive while the OS lock is unavailable.
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            service.status(owner(), request).await.unwrap(),
            Acquisition::Queued
        );
    }
    drop(local);
    let lease = granted(&service, request).await;
    service.release(owner(), lease).await.unwrap();
}

#[tokio::test]
async fn filesystem_lock_errors_are_reported_instead_of_granting_unprotected_access(
) {
    let directory = TempDir::new().unwrap();
    let file = directory.path().join("not-a-directory");
    std::fs::write(&file, b"file").unwrap();
    let policy = LeasePolicy::new(
        Duration::from_secs(30),
        Duration::from_secs(60),
        Instant::now(),
    )
    .unwrap();
    let service = LeaseService::start(file, policy);
    let request = LeaseRequestId::generate();
    service
        .acquire(owner(), request, LeaseMode::Shared)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match service.status(owner(), request).await {
                Err(Error::Storage(_)) => break,
                Ok(Acquisition::Queued) => {
                    tokio::time::sleep(Duration::from_millis(5)).await
                }
                _ => panic!("storage failure was not propagated"),
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn expiration_prevents_publication_and_keeps_the_guard_until_cleanup() {
    let directory = TempDir::new().unwrap();
    let policy = LeasePolicy::new(
        Duration::from_millis(200),
        Duration::from_secs(1),
        Instant::now(),
    )
    .unwrap();
    let service = LeaseService::start(directory.path().to_owned(), policy);
    let request = LeaseRequestId::generate();
    service
        .acquire(owner(), request, LeaseMode::Shared)
        .await
        .unwrap();
    let lease = granted(&service, request).await;
    let permit = service.writable(owner(), lease).await.unwrap();
    let mut upload = permit.create_object(Path::new("expired-object")).unwrap();
    upload.write_all(b"unpublished").unwrap();
    drop(permit);
    tokio::time::timeout(Duration::from_secs(5), async {
        while upload.flush().is_ok() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(Local::new(directory.path().to_owned())
        .try_exclusive_operation()
        .is_err());
    assert!(upload.commit().is_err());
    assert!(!directory.path().join("expired-object").exists());
    assert_eq!(
        service.status(owner(), request).await.unwrap(),
        Acquisition::Closed
    );
    assert!(Local::new(directory.path().to_owned())
        .try_exclusive_operation()
        .is_ok());
}

#[tokio::test]
async fn stopping_the_coordinator_invalidates_reads_without_releasing_the_file_lock(
) {
    use std::io::Read;
    let directory = TempDir::new().unwrap();
    std::fs::write(directory.path().join("object"), b"content").unwrap();
    let service = service(&directory);
    let request = LeaseRequestId::generate();
    service
        .acquire(owner(), request, LeaseMode::Shared)
        .await
        .unwrap();
    let lease = granted(&service, request).await;
    let permit = service.shared(owner(), lease).await.unwrap();
    let mut reader = permit.read_object(Path::new("object")).unwrap();
    drop(permit);
    drop(service);
    tokio::time::timeout(Duration::from_secs(5), async {
        while reader.read(&mut []).is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let local = Local::new(directory.path().to_owned());
    assert!(local.try_exclusive_operation().is_err());
    drop(reader);
    assert!(local.try_exclusive_operation().is_ok());
}

#[tokio::test]
async fn anonymous_shared_leases_only_produce_read_permits() {
    let directory = TempDir::new().unwrap();
    let service = service(&directory);
    let request = LeaseRequestId::generate();
    service
        .acquire(Principal::Anonymous, request, LeaseMode::Shared)
        .await
        .unwrap();
    let lease = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match service.status(Principal::Anonymous, request).await.unwrap() {
                Acquisition::Granted { lease } => break lease.id,
                Acquisition::Queued => {
                    tokio::time::sleep(Duration::from_millis(5)).await
                }
                Acquisition::Closed => panic!("unexpected closed request"),
            }
        }
    })
    .await
    .unwrap();
    assert!(service.shared(Principal::Anonymous, lease).await.is_ok());
    assert!(matches!(
        service.writable(Principal::Anonymous, lease).await,
        Err(Error::Rejected(ProblemKind::Unauthorized))
    ));
}

#[tokio::test]
async fn abandoned_requests_and_terminal_retention_do_not_consume_capacity_forever(
) {
    use rdedup_server::lease::{RequestCapacity, RequestPolicy};
    use std::num::NonZeroUsize;
    let directory = TempDir::new().unwrap();
    let local = Local::new(directory.path().to_owned())
        .exclusive_operation()
        .unwrap();
    let now = Instant::now();
    let policy =
        LeasePolicy::new(Duration::from_secs(30), Duration::from_secs(60), now)
            .unwrap();
    let requests = RequestPolicy::new(
        RequestCapacity::new(NonZeroUsize::new(1).unwrap()),
        Duration::from_millis(100),
        Duration::from_millis(200),
        now,
    )
    .unwrap();
    let service = LeaseService::with_request_policy(
        directory.path().to_owned(),
        policy,
        requests,
    );
    let first = LeaseRequestId::generate();
    let second = LeaseRequestId::generate();
    service
        .acquire(owner(), first, LeaseMode::Shared)
        .await
        .unwrap();
    assert!(matches!(
        service.acquire(owner(), second, LeaseMode::Shared).await,
        Err(Error::Rejected(ProblemKind::Unavailable))
    ));
    // The first request is abandoned while the external operation holds the lock.
    // Retry a different request; this must not refresh the abandoned request.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match service.acquire(owner(), second, LeaseMode::Shared).await {
                Ok(Acquisition::Queued) => break,
                Err(Error::Rejected(ProblemKind::Unavailable)) => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                _ => panic!("unexpected capacity response"),
            }
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        service.status(owner(), first).await,
        Err(Error::Rejected(ProblemKind::NotFound))
    ));
    drop(local);
    let lease = granted(&service, second).await;
    service.release(owner(), lease).await.unwrap();
}
