//! Stable HTTP problems hide storage paths and never classify error prose.
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rdedup_protocol::problem::{Problem, ProblemKind};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("request rejected: {0:?}")]
    Problem(ProblemKind),
    #[error(transparent)]
    Lease(#[from] crate::lease::Error),
    #[error(transparent)]
    Object(#[from] crate::lease::ObjectError),
    #[error("storage operation failed")]
    Storage(#[from] std::io::Error),
    #[error("blocking storage task failed")]
    Task(#[from] tokio::task::JoinError),
}

impl Error {
    pub(crate) fn kind(&self) -> ProblemKind {
        match self {
            Self::Problem(kind) => *kind,
            Self::Object(crate::lease::ObjectError::Lease(error)) => {
                match error {
                    crate::lease::Error::Rejected(kind) => *kind,
                    crate::lease::Error::Stopped => ProblemKind::Unavailable,
                    crate::lease::Error::Storage(_) => {
                        ProblemKind::StorageFailure
                    }
                }
            }
            Self::Object(crate::lease::ObjectError::Stream(error)) => {
                use rdedup_lib::backends::local::StreamError;
                let source = match error {
                    StreamError::Prepare { source, .. }
                    | StreamError::Open { source, .. }
                    | StreamError::Synchronize { source, .. }
                    | StreamError::Publish { source, .. } => source,
                };
                storage_kind(source)
            }
            Self::Object(crate::lease::ObjectError::Storage(error)) => {
                storage_kind(error)
            }
            Self::Lease(crate::lease::Error::Rejected(kind)) => *kind,
            Self::Lease(crate::lease::Error::Stopped) => {
                ProblemKind::Unavailable
            }
            Self::Storage(error) => storage_kind(error),
            Self::Lease(crate::lease::Error::Storage(_)) | Self::Task(_) => {
                ProblemKind::StorageFailure
            }
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let kind = self.kind();
        let status = kind.status();
        let detail = match kind {
            ProblemKind::LeaseExpired => {
                "lease is no longer active; restart the repository operation"
            }
            ProblemKind::RenewalClosed => {
                "the fixed renewal deadline has been reached"
            }
            ProblemKind::RequestConflict => {
                "request identity has different acquisition contents"
            }
            _ => status.canonical_reason().unwrap_or("request failed"),
        };
        problem_response(status, kind, detail)
    }
}

pub(crate) async fn not_found() -> Error {
    Error::Problem(ProblemKind::NotFound)
}

pub(crate) async fn method_not_allowed() -> Response {
    problem_response(
        StatusCode::METHOD_NOT_ALLOWED,
        ProblemKind::InvalidRequest,
        "method not supported by this endpoint",
    )
}

fn problem_response(
    status: StatusCode,
    kind: ProblemKind,
    detail: &'static str,
) -> Response {
    let problem = Problem {
        kind,
        detail: detail.to_owned(),
    };
    // The problem contains a fixed identifier and a fixed ASCII message. JSON
    // string quoting keeps the header free of HTTP control characters.
    let encoded = serde_json::to_string(&problem)
        .expect("Problem contains only serializable strings");
    let value = HeaderValue::from_str(&encoded)
        .expect("protocol problem identifiers and messages are fixed ASCII");
    let mut response = (
        status,
        [(header::CONTENT_TYPE, "application/problem+json")],
        Json(problem),
    )
        .into_response();
    response
        .headers_mut()
        .insert(rdedup_protocol::route::PROBLEM_HEADER, value);
    response
}

fn storage_kind(error: &std::io::Error) -> ProblemKind {
    match error.kind() {
        std::io::ErrorKind::NotFound => ProblemKind::NotFound,
        std::io::ErrorKind::AlreadyExists => ProblemKind::AlreadyExists,
        _ => ProblemKind::StorageFailure,
    }
}
