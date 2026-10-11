//! Bounded HTTP object transfer under the originating lease's protection.
use crate::lease::{LeaseReader, LeaseWriter};
use crate::response::Error;
use crate::server::Repository;
use crate::transfer::{BodyPiece, ResponseStream};
use axum::body::{Body, Bytes};
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use rdedup_lib::backends::local::Publication;
use rdedup_lib::backends::{Metadata, ObjectPath};
use rdedup_protocol::object::RepositoryPath;
use rdedup_protocol::problem::ProblemKind;
use rdedup_protocol::route;
use std::io::{self, Read, Write};
use std::sync::Arc;
use tokio::sync::mpsc;

const TRANSFER_BUFFER_SIZE: usize = 128 * 1024;

fn object_path(
    path: Result<Path<RepositoryPath>, PathRejection>,
) -> Result<ObjectPath, Error> {
    let Path(path) =
        path.map_err(|_| Error::Problem(ProblemKind::InvalidRequest))?;
    ObjectPath::new(path.to_path_buf())
        .map_err(|_| Error::Problem(ProblemKind::InvalidRequest))
}

pub(crate) async fn get(
    State(repository): State<Arc<Repository>>,
    path: Result<Path<RepositoryPath>, PathRejection>,
    headers: HeaderMap,
) -> Result<Response, Error> {
    let path = object_path(path)?;
    let principal = repository.read_principal(&headers)?;
    let lease = repository.lease_id(&headers)?;
    let permit = repository.leases.shared(principal, lease).await?;
    let opened = tokio::task::spawn_blocking(move || {
        let metadata = permit.metadata(path.as_path().to_owned())?;
        let reader = permit.read_object(path.as_path())?;
        Ok::<_, crate::lease::ObjectError>(OpenedObject { reader, metadata })
    })
    .await??;
    let response_headers = metadata_headers(&opened.metadata)?;
    let (sender, receiver) = mpsc::channel(2);
    tokio::spawn(download(opened.reader, sender));
    Ok((
        response_headers,
        Body::from_stream(ResponseStream::new(receiver)),
    )
        .into_response())
}

pub(crate) async fn head(
    State(repository): State<Arc<Repository>>,
    path: Result<Path<RepositoryPath>, PathRejection>,
    headers: HeaderMap,
) -> Result<Response, Error> {
    let path = object_path(path)?;
    let principal = repository.read_principal(&headers)?;
    let lease = repository.lease_id(&headers)?;
    let permit = repository.leases.shared(principal, lease).await?;
    let metadata = tokio::task::spawn_blocking(move || {
        permit.metadata(path.as_path().to_owned())
    })
    .await??;
    Ok((metadata_headers(&metadata)?, Body::empty()).into_response())
}

pub(crate) async fn put(
    State(repository): State<Arc<Repository>>,
    path: Result<Path<RepositoryPath>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, Error> {
    let path = object_path(path)?;
    let principal = repository.principal(&headers)?;
    let lease = repository.lease_id(&headers)?;
    let mut conditions = headers.get_all(header::IF_NONE_MATCH).iter();
    let condition = conditions.next();
    if conditions.next().is_some()
        || condition.is_some_and(|value| value != "*")
    {
        return Err(Error::Problem(ProblemKind::InvalidRequest));
    }
    let writer = if condition.is_some() {
        let permit = repository.leases.writable(principal, lease).await?;
        tokio::task::spawn_blocking(move || {
            permit.create_object(path.as_path())
        })
        .await??
    } else {
        let permit = repository.leases.exclusive(principal, lease).await?;
        tokio::task::spawn_blocking(move || {
            permit.replace_object(path.as_path())
        })
        .await??
    };
    match upload(writer, body).await? {
        Publication::Published => Ok(StatusCode::NO_CONTENT),
        Publication::AlreadyExists => {
            Err(Error::Problem(ProblemKind::AlreadyExists))
        }
    }
}

pub(crate) async fn remove(
    State(repository): State<Arc<Repository>>,
    path: Result<Path<RepositoryPath>, PathRejection>,
    headers: HeaderMap,
) -> Result<StatusCode, Error> {
    let path = object_path(path)?;
    let principal = repository.principal(&headers)?;
    let lease = repository.lease_id(&headers)?;
    let permit = repository.leases.exclusive(principal, lease).await?;
    tokio::task::spawn_blocking(move || {
        permit.remove(path.as_path().to_owned())
    })
    .await??;
    Ok(StatusCode::NO_CONTENT)
}

fn metadata_headers(metadata: &Metadata) -> Result<HeaderMap, Error> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(metadata.len));
    let encoded = serde_json::to_string(metadata)
        .map_err(|_| Error::Problem(ProblemKind::StorageFailure))?;
    let value = HeaderValue::from_str(&encoded)
        .map_err(|_| Error::Problem(ProblemKind::StorageFailure))?;
    headers.insert(route::METADATA_HEADER, value);
    Ok(headers)
}

struct OpenedObject {
    reader: LeaseReader,
    metadata: Metadata,
}

struct ReadStep {
    reader: LeaseReader,
    bytes: io::Result<Bytes>,
}

impl ReadStep {
    fn read(mut reader: LeaseReader) -> Self {
        let mut bytes = vec![0; TRANSFER_BUFFER_SIZE];
        let result = reader.read(&mut bytes).map(|length| {
            bytes.truncate(length);
            Bytes::from(bytes)
        });
        Self {
            reader,
            bytes: result,
        }
    }
}

async fn download(mut reader: LeaseReader, sender: mpsc::Sender<BodyPiece>) {
    let mut validity = reader.lease_check();
    loop {
        let step =
            match tokio::task::spawn_blocking(move || ReadStep::read(reader))
                .await
            {
                Ok(step) => step,
                Err(error) => {
                    let _ = sender
                        .try_send(BodyPiece::Failed(io::Error::other(error)));
                    return;
                }
            };
        reader = step.reader;
        let piece = match step.bytes {
            Ok(bytes) if bytes.is_empty() => return,
            Ok(bytes) => BodyPiece::Bytes(bytes),
            Err(error) => {
                let _ = sender.try_send(BodyPiece::Failed(error));
                return;
            }
        };
        tokio::select! {
            error = validity.wait_until_lost() => {
                let _ = sender.try_send(BodyPiece::Failed(io::Error::other(error)));
                return;
            }
            sent = sender.send(piece) => {
                if sent.is_err() { return; }
            }
        }
    }
}

struct WriteStep {
    writer: LeaseWriter,
    result: io::Result<()>,
}

impl WriteStep {
    fn write(mut writer: LeaseWriter, bytes: Bytes) -> Self {
        let result = writer.write_all(&bytes);
        Self { writer, result }
    }
}

async fn upload(
    mut writer: LeaseWriter,
    body: Body,
) -> Result<Publication, Error> {
    let mut validity = writer.lease_check();
    let mut body = body.into_data_stream();
    loop {
        let piece = tokio::select! {
            error = validity.wait_until_lost() => return Err(error.into()),
            piece = body.next() => piece,
        };
        let Some(piece) = piece else {
            break;
        };
        let mut bytes =
            piece.map_err(|_| Error::Problem(ProblemKind::InvalidRequest))?;
        while !bytes.is_empty() {
            let chunk = bytes.split_to(bytes.len().min(TRANSFER_BUFFER_SIZE));
            let step = tokio::task::spawn_blocking(move || {
                WriteStep::write(writer, chunk)
            })
            .await?;
            writer = step.writer;
            validity.ensure_active()?;
            step.result?;
        }
    }
    Ok(tokio::task::spawn_blocking(move || writer.commit()).await??)
}
