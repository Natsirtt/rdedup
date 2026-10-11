//! Destructive relocation and additive chunk promotion use different capabilities.
use crate::response::Error;
use crate::server::Repository;
use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use rdedup_lib::backends::{ChunkPromotion, DirectoryPath, ObjectPath};
use rdedup_protocol::object::Rename;
use rdedup_protocol::problem::ProblemKind;
use std::sync::Arc;

pub(crate) async fn promote(
    State(repository): State<Arc<Repository>>,
    headers: HeaderMap,
    payload: Result<Json<ChunkPromotion>, JsonRejection>,
) -> Result<StatusCode, Error> {
    let Json(promotion) =
        payload.map_err(|_| Error::Problem(ProblemKind::InvalidRequest))?;
    let principal = repository.principal(&headers)?;
    let lease = repository.lease_id(&headers)?;
    let permit = repository.leases.writable(principal, lease).await?;
    tokio::task::spawn_blocking(move || permit.promote(promotion)).await??;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn rename(
    State(repository): State<Arc<Repository>>,
    headers: HeaderMap,
    payload: Result<Json<Rename>, JsonRejection>,
) -> Result<StatusCode, Error> {
    let Json(rename) =
        payload.map_err(|_| Error::Problem(ProblemKind::InvalidRequest))?;
    let source = rename.source.to_path_buf();
    let destination = rename.destination.to_path_buf();
    for path in [&source, &destination] {
        if ObjectPath::new(path.clone()).is_err()
            && DirectoryPath::new(path.clone()).is_err()
        {
            return Err(Error::Problem(ProblemKind::InvalidRequest));
        }
    }
    let principal = repository.principal(&headers)?;
    let lease = repository.lease_id(&headers)?;
    let permit = repository
        .leases
        .exclusive(principal.clone(), lease)
        .await?;
    let reader = repository.leases.shared(principal, lease).await?;
    tokio::task::spawn_blocking(move || {
        let metadata = reader.metadata(source.clone())?;
        // Names may contain directories, so layout syntax alone cannot decide
        // whether a location denotes a directory or a published name file.
        if metadata.is_file {
            let source = ObjectPath::new(source)
                .map_err(|_| Error::Problem(ProblemKind::InvalidRequest))?;
            let destination = ObjectPath::new(destination)
                .map_err(|_| Error::Problem(ProblemKind::InvalidRequest))?;
            permit.rename(
                source.as_path().to_owned(),
                destination.as_path().to_owned(),
            )?;
        } else {
            let source = DirectoryPath::new(source)
                .map_err(|_| Error::Problem(ProblemKind::InvalidRequest))?;
            let destination = DirectoryPath::new(destination)
                .map_err(|_| Error::Problem(ProblemKind::InvalidRequest))?;
            permit.rename(
                source.as_path().to_owned(),
                destination.as_path().to_owned(),
            )?;
        }
        Ok::<_, Error>(())
    })
    .await??;
    Ok(StatusCode::NO_CONTENT)
}
