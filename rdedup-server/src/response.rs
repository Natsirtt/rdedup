//! Stable HTTP problems hide storage paths and never classify error prose.
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rdedup_protocol::problem::{Problem, ProblemKind};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("request rejected: {0:?}")]
    Problem(ProblemKind),
    #[error(transparent)]
    Lease(#[from] crate::lease::Error),
    #[error("storage operation failed")]
    Storage(#[from] std::io::Error),
    #[error("blocking storage task failed")]
    Task(#[from] tokio::task::JoinError),
}

impl Error {
    fn kind(&self) -> ProblemKind {
        match self {
            Self::Problem(kind) => *kind,
            Self::Lease(crate::lease::Error::Rejected(kind)) => *kind,
            Self::Lease(crate::lease::Error::Stopped) => {
                ProblemKind::Unavailable
            }
            Self::Storage(error) => match error.kind() {
                std::io::ErrorKind::NotFound => ProblemKind::NotFound,
                std::io::ErrorKind::AlreadyExists => ProblemKind::AlreadyExists,
                _ => ProblemKind::StorageFailure,
            },
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
        (
            status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            Json(Problem {
                kind,
                detail: detail.to_owned(),
            }),
        )
            .into_response()
    }
}

pub(crate) async fn not_found() -> Error {
    Error::Problem(ProblemKind::NotFound)
}

pub(crate) async fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::CONTENT_TYPE, "application/problem+json")],
        Json(Problem {
            kind: ProblemKind::InvalidRequest,
            detail: "method not supported by this endpoint".to_owned(),
        }),
    )
        .into_response()
}
