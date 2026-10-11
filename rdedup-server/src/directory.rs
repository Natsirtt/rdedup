//! Incremental listings require an explicit completion record before acceptance.
use crate::lease::LeaseEntries;
use crate::response::Error;
use crate::server::Repository;
use crate::transfer::{BodyPiece, ResponseStream};
use axum::body::{Body, Bytes};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rdedup_lib::backends::local::EntryKind;
use rdedup_lib::backends::{DirectoryPath, ObjectPath};
use rdedup_protocol::object::{
    DirectoryQuery, ListScope, ListingRecord, RepositoryPath,
};
use rdedup_protocol::problem::{Problem, ProblemKind};
use rdedup_protocol::route;
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc;

fn directory(
    query: Result<Query<DirectoryQuery>, QueryRejection>,
) -> Result<DirectoryQuery, Error> {
    query
        .map(|Query(query)| query)
        .map_err(|_| Error::Problem(ProblemKind::InvalidRequest))
}

fn directory_path(query: &DirectoryQuery) -> Result<DirectoryPath, Error> {
    let path = query
        .path
        .as_ref()
        .map(RepositoryPath::to_path_buf)
        .unwrap_or_default();
    DirectoryPath::new(path)
        .map_err(|_| Error::Problem(ProblemKind::InvalidRequest))
}

pub(crate) async fn list(
    State(repository): State<Arc<Repository>>,
    query: Result<Query<DirectoryQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, Error> {
    let query = directory(query)?;
    let path = directory_path(&query)?;
    let principal = repository.read_principal(&headers)?;
    let lease = repository.lease_id(&headers)?;
    let permit = repository.leases.shared(principal, lease).await?;
    let entries = tokio::task::spawn_blocking(move || match query.scope {
        ListScope::Immediate => permit.read_directory(path.as_path()),
        ListScope::Recursive => permit.walk_directory(path.as_path()),
    })
    .await??;
    let (sender, receiver) = mpsc::channel(2);
    tokio::spawn(produce(entries, sender));
    Ok((
        [(header::CONTENT_TYPE, route::LISTING_CONTENT_TYPE)],
        Body::from_stream(ResponseStream::new(receiver)),
    )
        .into_response())
}

pub(crate) async fn remove(
    State(repository): State<Arc<Repository>>,
    query: Result<Query<DirectoryQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<StatusCode, Error> {
    let query = directory(query)?;
    let path = directory_path(&query)?;
    if path.as_path().as_os_str().is_empty() {
        return Err(Error::Problem(ProblemKind::InvalidRequest));
    }
    let principal = repository.principal(&headers)?;
    let lease = repository.lease_id(&headers)?;
    let permit = repository.leases.exclusive(principal, lease).await?;
    tokio::task::spawn_blocking(move || {
        permit.remove_directory(path.as_path().to_owned())
    })
    .await??;
    Ok(StatusCode::NO_CONTENT)
}

enum ListingStep {
    More {
        entries: Box<LeaseEntries>,
        bytes: Bytes,
    },
    Finished(Bytes),
}

impl ListingStep {
    fn read(mut entries: Box<LeaseEntries>) -> Result<Self, Error> {
        let mut bytes = Vec::new();
        for _ in 0..16 {
            let entry = match entries.next() {
                Some(Ok(entry)) => entry,
                Some(Err(error)) => {
                    append(
                        &mut bytes,
                        ListingRecord::Failed {
                            problem: Problem {
                                kind: Error::from(error).kind(),
                                detail: "directory scan failed".to_owned(),
                            },
                        },
                    )?;
                    return Ok(Self::Finished(bytes.into()));
                }
                None => {
                    append(&mut bytes, ListingRecord::Complete)?;
                    return Ok(Self::Finished(bytes.into()));
                }
            };
            let admitted = match entry.kind() {
                EntryKind::File => {
                    ObjectPath::new(entry.as_path().to_owned()).is_ok()
                }
                EntryKind::Directory => {
                    DirectoryPath::new(entry.as_path().to_owned()).is_ok()
                }
            };
            if admitted {
                let path = RepositoryPath::from_path(entry.as_path())
                    .map_err(|_| Error::Problem(ProblemKind::StorageFailure))?;
                append(&mut bytes, ListingRecord::Entry { path })?;
            }
        }
        Ok(Self::More {
            entries,
            bytes: bytes.into(),
        })
    }
}

fn append(bytes: &mut Vec<u8>, record: ListingRecord) -> Result<(), Error> {
    serde_json::to_writer(&mut *bytes, &record)
        .map_err(|_| Error::Problem(ProblemKind::StorageFailure))?;
    bytes.push(b'\n');
    Ok(())
}

async fn produce(entries: LeaseEntries, sender: mpsc::Sender<BodyPiece>) {
    let mut validity = entries.lease_check();
    let mut entries = Box::new(entries);
    loop {
        let step = match tokio::task::spawn_blocking(move || {
            ListingStep::read(entries)
        })
        .await
        {
            Ok(Ok(step)) => step,
            Ok(Err(error)) => {
                let _ =
                    sender.try_send(BodyPiece::Failed(io::Error::other(error)));
                return;
            }
            Err(error) => {
                let _ =
                    sender.try_send(BodyPiece::Failed(io::Error::other(error)));
                return;
            }
        };
        match step {
            ListingStep::More {
                entries: remaining,
                bytes,
            } => {
                entries = remaining;
                if bytes.is_empty() {
                    continue;
                }
                tokio::select! {
                    _ = validity.wait_until_lost() => return,
                    sent = sender.send(BodyPiece::Bytes(bytes)) => {
                        if sent.is_err() { return; }
                    }
                }
            }
            ListingStep::Finished(bytes) => {
                tokio::select! {
                    _ = validity.wait_until_lost() => {},
                    _ = sender.send(BodyPiece::Bytes(bytes)) => {},
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::{LeasePolicy, LeaseService, Principal};
    use rdedup_lib::backends::local::Local;
    use rdedup_protocol::lease::{Acquisition, LeaseMode, LeaseRequestId};
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn stalled_listing_terminates_without_completion_and_releases_its_guard(
    ) {
        let directory = tempfile::TempDir::new().unwrap();
        let path =
            std::path::Path::new("0000000000000000-0000000000000000/name");
        std::fs::create_dir_all(directory.path().join(path)).unwrap();
        for index in 0..64 {
            std::fs::write(
                directory
                    .path()
                    .join(path)
                    .join(format!("name-{index}.yml")),
                b"",
            )
            .unwrap();
        }
        let service = LeaseService::start(
            directory.path().to_owned(),
            LeasePolicy::new(
                Duration::from_secs(60),
                Duration::from_secs(120),
                Instant::now(),
            )
            .unwrap(),
        );
        let owner = Principal::Credential("listing-test".parse().unwrap());
        let request = LeaseRequestId::generate();
        service
            .acquire(owner.clone(), request, LeaseMode::Shared)
            .await
            .unwrap();
        let lease = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match service.status(owner.clone(), request).await.unwrap() {
                    Acquisition::Granted { lease } => break lease.id,
                    Acquisition::Queued => tokio::task::yield_now().await,
                    Acquisition::Closed => {
                        panic!("request closed before admission")
                    }
                }
            }
        })
        .await
        .unwrap();
        let permit = service.shared(owner.clone(), lease).await.unwrap();
        let entries = permit.read_directory(path).unwrap();
        drop(permit);
        let (sender, mut receiver) = mpsc::channel(1);
        let producer = tokio::spawn(produce(entries, sender));
        let first =
            tokio::time::timeout(Duration::from_secs(5), receiver.recv())
                .await
                .unwrap()
                .unwrap();
        let BodyPiece::Bytes(bytes) = &first else {
            panic!("scan failed before producing entries")
        };
        assert!(std::str::from_utf8(bytes).unwrap().lines().any(
            |line| matches!(
                serde_json::from_str::<ListingRecord>(line).unwrap(),
                ListingRecord::Entry { .. }
            )
        ));

        let mut pieces = vec![first];
        service.release(owner, lease).await.unwrap();
        // The consumer deliberately stops pulling frames. The producer must
        // terminate under backpressure before the consumer resumes.
        tokio::time::timeout(Duration::from_secs(5), producer)
            .await
            .unwrap()
            .unwrap();
        while let Ok(piece) = receiver.try_recv() {
            pieces.push(piece);
        }
        for piece in pieces {
            if let BodyPiece::Bytes(bytes) = piece {
                for line in std::str::from_utf8(&bytes).unwrap().lines() {
                    assert!(!matches!(
                        serde_json::from_str::<ListingRecord>(line).unwrap(),
                        ListingRecord::Complete
                    ));
                }
            }
        }
        assert!(Local::new(directory.path().to_owned())
            .try_exclusive_operation()
            .is_ok());
    }
}
