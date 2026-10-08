use crate::config::{ConfigError, ServerConfig};
use crate::lease::{
    unix_milliseconds, AcquireResult, LeaseError, LeaseGrant, LeaseManager,
    LeaseMode, RequestStatus,
};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use rdedup_lib::backends::local::Local;
use rdedup_lib::backends::{Backend, BackendThread};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io;
use std::path::{Path as FilePath, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use uuid::Uuid;

#[derive(Clone)]
struct ServerState {
    config: ServerConfig,
    leases: Arc<LeaseManager>,
    token_hashes: Arc<HashMap<[u8; 32], String>>,
}

#[derive(Serialize)]
struct LeaseResponse {
    lease_id: Uuid,
    mode: &'static str,
    expires_at_unix_ms: u128,
    renewal_deadline_unix_ms: Option<u128>,
}

#[derive(Serialize)]
struct PendingResponse {
    request_id: Uuid,
    status: &'static str,
}

#[derive(Serialize)]
struct RequestStatusResponse {
    status: &'static str,
    lease: Option<LeaseResponse>,
}

#[derive(Deserialize)]
struct AcquireRequest {
    mode: String,
}

#[derive(Deserialize)]
struct RenameRequest {
    source: String,
    destination: String,
}

#[derive(Deserialize)]
struct DirectoryQuery {
    recursive: Option<bool>,
}

#[derive(Serialize)]
struct DirectoryEntry {
    name: String,
    kind: &'static str,
}

impl From<LeaseGrant> for LeaseResponse {
    fn from(grant: LeaseGrant) -> Self {
        LeaseResponse {
            lease_id: grant.id,
            mode: match grant.mode {
                LeaseMode::Shared => "shared",
                LeaseMode::Exclusive => "exclusive",
            },
            expires_at_unix_ms: unix_milliseconds(grant.expires_at),
            renewal_deadline_unix_ms: grant
                .renewal_deadline
                .map(unix_milliseconds),
        }
    }
}

fn make_state(config: ServerConfig) -> ServerState {
    let token_hashes = config
        .tokens
        .iter()
        .map(|(name, token)| {
            let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
            (hash, name.clone())
        })
        .collect();
    let leases = LeaseManager::new(
        config.lease_ttl,
        config.lease_renewal_grace,
        config.lease_request_ttl,
    );
    ServerState {
        config,
        leases: Arc::new(leases),
        token_hashes: Arc::new(token_hashes),
    }
}

pub fn router(config: ServerConfig) -> Router {
    let state = make_state(config);
    Router::new()
        .route(
            "/api/v1/objects/{*path}",
            get(read_object).put(write_object).delete(remove_object),
        )
        .route("/api/v1/metadata/{*path}", get(read_metadata))
        .route("/api/v1/directories", get(list_directory))
        .route(
            "/api/v1/directories/{*path}",
            get(list_directory).delete(remove_directory),
        )
        .route("/api/v1/renames", post(rename_object))
        .route("/api/v1/leases", post(acquire_lease))
        .route(
            "/api/v1/leases/{lease_id}",
            patch(renew_lease).delete(release_lease),
        )
        .route(
            "/api/v1/lease-requests/{request_id}",
            get(lease_request_status).delete(cancel_lease_request),
        )
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(state)
}

pub async fn run(config_path: Option<PathBuf>) -> io::Result<()> {
    let config = ServerConfig::load(config_path).map_err(config_error)?;
    if config.tokens.is_empty() {
        eprintln!("warning: no client tokens are configured; writes and exclusive operations are unavailable");
    }
    warn_if_large_chunks(&config.repository_path)?;

    let listener = TcpListener::bind(config.bind_address).await?;
    eprintln!("rdedup server listening on {}", config.bind_address);
    axum::serve(listener, router(config))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(io::Error::other)
}

fn config_error(error: ConfigError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}

fn warn_if_large_chunks(repository_path: &FilePath) -> io::Result<()> {
    let configuration = std::fs::read(repository_path.join("config.yml"))?;
    let document: serde_yaml::Value = serde_yaml::from_slice(&configuration)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let bits = document
        .get("chunking")
        .and_then(|chunking| chunking.get("chunk_bits"))
        .and_then(serde_yaml::Value::as_u64)
        .unwrap_or(17);
    if bits >= 24 {
        eprintln!("warning: repository chunk_bits is {bits}; HTTP requests buffer one full chunk and may use substantial memory");
    }
    Ok(())
}

fn authorized(headers: &HeaderMap, state: &ServerState) -> bool {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    let candidate = Sha256::digest(token.as_bytes());
    state
        .token_hashes
        .keys()
        .any(|configured| bool::from(candidate.as_slice().ct_eq(configured)))
}

fn require_read(
    headers: &HeaderMap,
    state: &ServerState,
) -> Result<(), ApiFailure> {
    if state.config.require_read_auth && !authorized(headers, state) {
        Err(ApiFailure::new(
            StatusCode::UNAUTHORIZED,
            "authentication is required",
        ))
    } else {
        Ok(())
    }
}

fn require_write(
    headers: &HeaderMap,
    state: &ServerState,
) -> Result<(), ApiFailure> {
    if authorized(headers, state) {
        Ok(())
    } else {
        Err(ApiFailure::new(
            StatusCode::UNAUTHORIZED,
            "authentication is required",
        ))
    }
}

fn require_lease_auth(
    headers: &HeaderMap,
    state: &ServerState,
    mode: LeaseMode,
) -> Result<(), ApiFailure> {
    match mode {
        LeaseMode::Shared => require_read(headers, state),
        LeaseMode::Exclusive => require_write(headers, state),
    }
}

fn lease_id(headers: &HeaderMap) -> Result<Uuid, ApiFailure> {
    headers
        .get("x-rdedup-lease")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or_else(|| {
            ApiFailure::new(
                StatusCode::BAD_REQUEST,
                "missing or invalid lease id",
            )
        })
}

fn validate_lease(
    headers: &HeaderMap,
    state: &ServerState,
    required_mode: LeaseMode,
) -> Result<(), ApiFailure> {
    let id = lease_id(headers)?;
    state
        .leases
        .validate(id, required_mode)
        .map_err(lease_error)
}

fn lease_error(error: LeaseError) -> ApiFailure {
    match error {
        LeaseError::Gone => ApiFailure::with_type(
            StatusCode::GONE,
            "urn:rdedup:problem:lease-expired",
            error.to_string(),
        ),
        LeaseError::RenewalClosed => ApiFailure::with_type(
            StatusCode::CONFLICT,
            "urn:rdedup:problem:lease-renewal-closed",
            error.to_string(),
        ),
        LeaseError::RequestNotFound => {
            ApiFailure::new(StatusCode::NOT_FOUND, error.to_string())
        }
    }
}

fn safe_path(path: &str, allow_root: bool) -> Result<PathBuf, ApiFailure> {
    if path.is_empty() && allow_root {
        return Ok(PathBuf::new());
    }
    if path.is_empty()
        || path.contains('\\')
        || path.contains('\0')
        || path.split('/').any(|segment| {
            segment.is_empty()
                || segment == "."
                || segment == ".."
                || segment.contains(':')
        })
    {
        return Err(ApiFailure::new(
            StatusCode::BAD_REQUEST,
            "invalid repository path",
        ));
    }
    Ok(path.split('/').collect())
}

fn backend_thread(state: &ServerState) -> io::Result<Box<dyn BackendThread>> {
    Local::new(state.config.repository_path.clone()).new_thread()
}

enum ProtectedOperationError {
    Lease(LeaseError),
    Io(io::Error),
}

fn protected_operation<T>(
    state: &ServerState,
    lease_id: Uuid,
    required_mode: LeaseMode,
    operation: impl FnOnce() -> io::Result<T>,
) -> Result<T, ProtectedOperationError> {
    state
        .leases
        .with_lease(lease_id, required_mode, operation)
        .map_err(ProtectedOperationError::Lease)?
        .map_err(ProtectedOperationError::Io)
}

fn protected_error(error: ProtectedOperationError) -> Response {
    match error {
        ProtectedOperationError::Lease(error) => {
            lease_error(error).into_response()
        }
        ProtectedOperationError::Io(error) => io_error(error),
    }
}
fn io_failure(error: io::Error) -> ApiFailure {
    let status = match error.kind() {
        io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        io::ErrorKind::AlreadyExists => StatusCode::PRECONDITION_FAILED,
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => {
            StatusCode::BAD_REQUEST
        }
        io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let problem_type = match error.kind() {
        io::ErrorKind::NotFound => "urn:rdedup:problem:not-found",
        io::ErrorKind::AlreadyExists => "urn:rdedup:problem:object-conflict",
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => {
            "urn:rdedup:problem:invalid-request"
        }
        io::ErrorKind::PermissionDenied => "urn:rdedup:problem:forbidden",
        _ => "urn:rdedup:problem:storage-failure",
    };
    ApiFailure::with_type(status, problem_type, error.to_string())
}

fn io_error(error: io::Error) -> Response {
    io_failure(error).into_response()
}

#[derive(Debug)]
struct ApiFailure {
    status: StatusCode,
    problem_type: &'static str,
    message: String,
}

impl ApiFailure {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        ApiFailure {
            status,
            problem_type: match status {
                StatusCode::UNAUTHORIZED => "urn:rdedup:problem:unauthorized",
                StatusCode::FORBIDDEN => "urn:rdedup:problem:forbidden",
                StatusCode::NOT_FOUND => "urn:rdedup:problem:not-found",
                StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED => {
                    "urn:rdedup:problem:conflict"
                }
                StatusCode::GONE => "urn:rdedup:problem:gone",
                StatusCode::PAYLOAD_TOO_LARGE => {
                    "urn:rdedup:problem:payload-too-large"
                }
                _ => "urn:rdedup:problem:invalid-request",
            },
            message: message.into(),
        }
    }

    fn with_type(
        status: StatusCode,
        problem_type: &'static str,
        message: impl Into<String>,
    ) -> Self {
        ApiFailure {
            status,
            problem_type,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ProblemDetails {
            #[serde(rename = "type")]
            problem_type: &'static str,
            title: &'static str,
            status: u16,
            detail: String,
        }

        let title = self.status.canonical_reason().unwrap_or("Request failed");
        let mut response = (
            self.status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            Json(ProblemDetails {
                problem_type: self.problem_type,
                title,
                status: self.status.as_u16(),
                detail: self.message,
            }),
        )
            .into_response();
        *response.status_mut() = self.status;
        response
    }
}

async fn read_object(
    State(state): State<ServerState>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(error) = require_read(&headers, &state) {
        return error.into_response();
    }
    if let Err(error) = validate_lease(&headers, &state, LeaseMode::Shared) {
        return error.into_response();
    }
    let path = match safe_path(&path, false) {
        Ok(path) => path,
        Err(error) => return error.into_response(),
    };
    let lease_id = lease_id(&headers).expect("lease was validated above");
    match tokio::task::spawn_blocking(move || {
        protected_operation(&state, lease_id, LeaseMode::Shared, || {
            let mut backend = backend_thread(&state)?;
            backend.read(path)
        })
    })
    .await
    {
        Ok(Ok(data)) => {
            (StatusCode::OK, data.into_linear_vec()).into_response()
        }
        Ok(Err(error)) => protected_error(error),
        Err(error) => io_error(io::Error::other(error)),
    }
}

async fn write_object(
    State(state): State<ServerState>,
    Path(path): Path<String>,
    request: Request,
) -> Response {
    let headers = request.headers().clone();
    if let Err(error) = require_write(&headers, &state) {
        return error.into_response();
    }
    if let Err(error) = validate_lease(&headers, &state, LeaseMode::Shared) {
        return error.into_response();
    }
    let path = match safe_path(&path, false) {
        Ok(path) => path,
        Err(error) => return error.into_response(),
    };
    let idempotent = headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|value| value == "*");
    let lease_id = lease_id(&headers).expect("lease was validated above");
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_OBJECT_SIZE)
    {
        return ApiFailure::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "object exceeds the maximum size",
        )
        .into_response();
    }
    let body = match read_body_under_lease(
        request.into_body(),
        &state,
        lease_id,
    )
    .await
    {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    match tokio::task::spawn_blocking(move || {
        protected_operation(&state, lease_id, LeaseMode::Shared, || {
            let mut backend = backend_thread(&state)?;
            backend.write(
                path,
                sgdata::SGData::from_single(body.to_vec()),
                idempotent,
            )
        })
    })
    .await
    {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(error)) => protected_error(error),
        Err(error) => io_error(io::Error::other(error)),
    }
}

const MAX_OBJECT_SIZE: usize = 1usize << 30;

async fn read_body_under_lease(
    body: Body,
    state: &ServerState,
    lease_id: Uuid,
) -> Result<Bytes, ApiFailure> {
    let mut stream = body.into_data_stream();
    let mut body_bytes = Vec::new();
    loop {
        tokio::select! {
            chunk = stream.next() => match chunk {
                Some(Ok(chunk)) => {
                    if let Err(error) = state.leases.validate(lease_id, LeaseMode::Shared) {
                        return Err(lease_error(error));
                    }
                    if chunk.len() > MAX_OBJECT_SIZE.saturating_sub(body_bytes.len()) {
                        return Err(ApiFailure::new(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "object exceeds the maximum size",
                        ));
                    }
                    body_bytes.extend_from_slice(&chunk);
                }
                Some(Err(error)) => return Err(io_failure(io::Error::other(error))),
                None => return Ok(Bytes::from(body_bytes)),
            },
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if let Err(error) = state.leases.validate(lease_id, LeaseMode::Shared) {
                    return Err(lease_error(error));
                }
            }
        }
    }
}

async fn remove_object(
    State(state): State<ServerState>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(error) = require_write(&headers, &state) {
        return error.into_response();
    }
    if let Err(error) = validate_lease(&headers, &state, LeaseMode::Exclusive) {
        return error.into_response();
    }
    let path = match safe_path(&path, false) {
        Ok(path) => path,
        Err(error) => return error.into_response(),
    };
    let lease_id = lease_id(&headers).expect("lease was validated above");
    match tokio::task::spawn_blocking(move || {
        protected_operation(&state, lease_id, LeaseMode::Exclusive, || {
            let mut backend = backend_thread(&state)?;
            backend.remove(path)
        })
    })
    .await
    {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(error)) => protected_error(error),
        Err(error) => io_error(io::Error::other(error)),
    }
}

async fn read_metadata(
    State(state): State<ServerState>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(error) = require_read(&headers, &state) {
        return error.into_response();
    }
    if let Err(error) = validate_lease(&headers, &state, LeaseMode::Shared) {
        return error.into_response();
    }
    let path = match safe_path(&path, false) {
        Ok(path) => path,
        Err(error) => return error.into_response(),
    };
    let lease_id = lease_id(&headers).expect("lease was validated above");
    match tokio::task::spawn_blocking(move || {
        protected_operation(&state, lease_id, LeaseMode::Shared, || {
            let mut backend = backend_thread(&state)?;
            backend.read_metadata(path)
        })
    })
    .await
    {
        Ok(Ok(metadata)) => Json(metadata).into_response(),
        Ok(Err(error)) => protected_error(error),
        Err(error) => io_error(io::Error::other(error)),
    }
}

async fn list_directory(
    State(state): State<ServerState>,
    path: Option<Path<String>>,
    Query(query): Query<DirectoryQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(error) = require_read(&headers, &state) {
        return error.into_response();
    }
    if let Err(error) = validate_lease(&headers, &state, LeaseMode::Shared) {
        return error.into_response();
    }
    let path =
        match safe_path(path.as_ref().map_or("", |Path(value)| value), true) {
            Ok(path) => path,
            Err(error) => return error.into_response(),
        };
    let recursive = query.recursive.unwrap_or(false);
    let lease_id = lease_id(&headers).expect("lease was validated above");
    match tokio::task::spawn_blocking(move || {
        protected_operation(&state, lease_id, LeaseMode::Shared, || {
            list_entries(&state, path, recursive)
        })
    })
    .await
    {
        Ok(Ok(entries)) => Json(entries).into_response(),
        Ok(Err(error)) => protected_error(error),
        Err(error) => io_error(io::Error::other(error)),
    }
}

fn list_entries(
    state: &ServerState,
    path: PathBuf,
    recursive: bool,
) -> io::Result<Vec<DirectoryEntry>> {
    let root = state.config.repository_path.clone();
    let mut backend = backend_thread(state)?;
    if recursive {
        let (tx, rx) = std::sync::mpsc::channel();
        backend.list_recursively(path, tx);
        let mut entries = Vec::new();
        for batch in rx {
            for path in batch? {
                let relative = path.strip_prefix(&root).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "backend returned path outside repository",
                    )
                })?;
                entries.push(DirectoryEntry {
                    name: relative
                        .to_str()
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "repository path is not UTF-8",
                            )
                        })?
                        .replace('\\', "/"),
                    kind: "file",
                });
            }
        }
        return Ok(entries);
    }

    let children = backend.list(path)?;
    children
        .into_iter()
        .map(|path| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "repository entry name is not UTF-8",
                    )
                })?
                .to_owned();
            let metadata = std::fs::symlink_metadata(path)?;
            if metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "symbolic links are not served",
                ));
            }
            Ok(DirectoryEntry {
                name,
                kind: if metadata.is_dir() {
                    "directory"
                } else {
                    "file"
                },
            })
        })
        .collect()
}

async fn remove_directory(
    State(state): State<ServerState>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(error) = require_write(&headers, &state) {
        return error.into_response();
    }
    if let Err(error) = validate_lease(&headers, &state, LeaseMode::Exclusive) {
        return error.into_response();
    }
    let path = match safe_path(&path, false) {
        Ok(path) => path,
        Err(error) => return error.into_response(),
    };
    let lease_id = lease_id(&headers).expect("lease was validated above");
    match tokio::task::spawn_blocking(move || {
        protected_operation(&state, lease_id, LeaseMode::Exclusive, || {
            let mut backend = backend_thread(&state)?;
            backend.remove_dir_all(path)
        })
    })
    .await
    {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(error)) => protected_error(error),
        Err(error) => io_error(io::Error::other(error)),
    }
}

async fn rename_object(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<RenameRequest>,
) -> Response {
    if let Err(error) = require_write(&headers, &state) {
        return error.into_response();
    }
    if let Err(error) = validate_lease(&headers, &state, LeaseMode::Exclusive) {
        return error.into_response();
    }
    let source = match safe_path(&request.source, false) {
        Ok(path) => path,
        Err(error) => return error.into_response(),
    };
    let destination = match safe_path(&request.destination, false) {
        Ok(path) => path,
        Err(error) => return error.into_response(),
    };
    let lease_id = lease_id(&headers).expect("lease was validated above");
    match tokio::task::spawn_blocking(move || {
        protected_operation(&state, lease_id, LeaseMode::Exclusive, || {
            let mut backend = backend_thread(&state)?;
            backend.rename(source, destination)
        })
    })
    .await
    {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(error)) => protected_error(error),
        Err(error) => io_error(io::Error::other(error)),
    }
}

async fn acquire_lease(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<AcquireRequest>,
) -> Response {
    let mode = match request.mode.as_str() {
        "shared" => LeaseMode::Shared,
        "exclusive" => LeaseMode::Exclusive,
        _ => {
            return ApiFailure::new(
                StatusCode::BAD_REQUEST,
                "lease mode must be shared or exclusive",
            )
            .into_response()
        }
    };
    if mode == LeaseMode::Exclusive {
        if let Err(error) = require_write(&headers, &state) {
            return error.into_response();
        }
    } else if let Err(error) = require_read(&headers, &state) {
        return error.into_response();
    }
    let Some(key) = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 128)
    else {
        return ApiFailure::new(
            StatusCode::BAD_REQUEST,
            "missing idempotency-key",
        )
        .into_response();
    };
    match state.leases.acquire(mode, key.to_owned()) {
        AcquireResult::Granted(grant) => {
            (StatusCode::CREATED, Json(LeaseResponse::from(grant)))
                .into_response()
        }
        AcquireResult::Pending(request_id) => {
            let mut response = (
                StatusCode::ACCEPTED,
                Json(PendingResponse {
                    request_id,
                    status: "pending",
                }),
            )
                .into_response();
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            response
        }
    }
}

async fn lease_request_status(
    State(state): State<ServerState>,
    Path(request_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(request_id) = Uuid::parse_str(&request_id) else {
        return ApiFailure::new(StatusCode::BAD_REQUEST, "invalid request id")
            .into_response();
    };
    let mode = match state.leases.request_mode(request_id) {
        Ok(mode) => mode,
        Err(error) => return lease_error(error).into_response(),
    };
    if let Err(error) = require_lease_auth(&headers, &state, mode) {
        return error.into_response();
    }
    match state.leases.status(request_id) {
        Ok(RequestStatus::Pending) => {
            let mut response = (
                StatusCode::ACCEPTED,
                Json(RequestStatusResponse {
                    status: "pending",
                    lease: None,
                }),
            )
                .into_response();
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            response
        }
        Ok(RequestStatus::Granted(grant)) => (
            StatusCode::OK,
            Json(RequestStatusResponse {
                status: "granted",
                lease: Some(grant.into()),
            }),
        )
            .into_response(),
        Ok(RequestStatus::Gone) => ApiFailure::with_type(
            StatusCode::GONE,
            "urn:rdedup:problem:lease-request-expired",
            "lease request expired before it was granted",
        )
        .into_response(),
        Err(error) => lease_error(error).into_response(),
    }
}

async fn cancel_lease_request(
    State(state): State<ServerState>,
    Path(request_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(request_id) = Uuid::parse_str(&request_id) else {
        return ApiFailure::new(StatusCode::BAD_REQUEST, "invalid request id")
            .into_response();
    };
    let mode = match state.leases.request_mode(request_id) {
        Ok(mode) => mode,
        Err(error) => return lease_error(error).into_response(),
    };
    if let Err(error) = require_lease_auth(&headers, &state, mode) {
        return error.into_response();
    }
    match state.leases.cancel(request_id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => lease_error(error).into_response(),
    }
}

async fn renew_lease(
    State(state): State<ServerState>,
    Path(lease_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(lease_id) = Uuid::parse_str(&lease_id) else {
        return ApiFailure::new(StatusCode::BAD_REQUEST, "invalid lease id")
            .into_response();
    };
    let mode = match state.leases.lease_mode(lease_id) {
        Ok(mode) => mode,
        Err(error) => return lease_error(error).into_response(),
    };
    if let Err(error) = require_lease_auth(&headers, &state, mode) {
        return error.into_response();
    }
    match state.leases.renew(lease_id) {
        Ok(grant) => Json(LeaseResponse::from(grant)).into_response(),
        Err(error) => lease_error(error).into_response(),
    }
}

async fn release_lease(
    State(state): State<ServerState>,
    Path(lease_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(lease_id) = Uuid::parse_str(&lease_id) else {
        return ApiFailure::new(StatusCode::BAD_REQUEST, "invalid lease id")
            .into_response();
    };
    let mode = match state.leases.lease_mode(lease_id) {
        Ok(mode) => mode,
        Err(error) => return lease_error(error).into_response(),
    };
    if let Err(error) = require_lease_auth(&headers, &state, mode) {
        return error.into_response();
    }
    match state.leases.release(lease_id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => lease_error(error).into_response(),
    }
}
