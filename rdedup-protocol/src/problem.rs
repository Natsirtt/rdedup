//! Stable machine-readable failure categories; callers never classify prose.
use http::StatusCode;
use serde::{Deserialize, Serialize};

/// Stable problem identifiers shared by HTTP client and server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProblemKind {
    /// Malformed parameters or repository object paths.
    #[serde(rename = "urn:rdedup:problem:invalid-request")]
    InvalidRequest,
    /// Missing or invalid credentials.
    #[serde(rename = "urn:rdedup:problem:unauthorized")]
    Unauthorized,
    /// The authenticated principal cannot perform this operation.
    #[serde(rename = "urn:rdedup:problem:forbidden")]
    Forbidden,
    /// The requested object is absent.
    #[serde(rename = "urn:rdedup:problem:not-found")]
    NotFound,
    /// Atomic creation retained an existing destination.
    #[serde(rename = "urn:rdedup:problem:already-exists")]
    AlreadyExists,
    /// A request identity was reused with different acquisition contents.
    #[serde(rename = "urn:rdedup:problem:request-conflict")]
    RequestConflict,
    /// A protected object operation omitted its lease identity.
    #[serde(rename = "urn:rdedup:problem:lease-required")]
    LeaseRequired,
    /// The lease is absent, released, or expired, including after restart.
    #[serde(rename = "urn:rdedup:problem:lease-expired")]
    LeaseExpired,
    /// A waiting exclusive operation prevents further renewal.
    #[serde(rename = "urn:rdedup:problem:lease-renewal-closed")]
    RenewalClosed,
    /// The operation requires exclusive protection.
    #[serde(rename = "urn:rdedup:problem:lease-mode")]
    LeaseMode,
    /// The server could not complete a storage operation.
    #[serde(rename = "urn:rdedup:problem:storage-failure")]
    StorageFailure,
    /// Bounded server resources cannot currently admit more work.
    #[serde(rename = "urn:rdedup:problem:unavailable")]
    Unavailable,
    /// An unrecognized future category is never assumed retryable.
    #[serde(other)]
    Unknown,
}

impl ProblemKind {
    /// HTTP status matching this problem category.
    pub fn status(self) -> StatusCode {
        match self {
            Self::InvalidRequest => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden | Self::LeaseMode => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::AlreadyExists
            | Self::RequestConflict
            | Self::RenewalClosed => StatusCode::CONFLICT,
            Self::LeaseRequired => StatusCode::PRECONDITION_REQUIRED,
            Self::LeaseExpired => StatusCode::GONE,
            Self::StorageFailure | Self::Unknown => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// Whether an idempotent request may be retried within a bounded policy.
    pub fn is_transient(self) -> bool {
        matches!(self, Self::Unavailable)
    }
}

/// An HTTP problem body carrying a stable category and human-readable context.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Problem {
    /// Classification used for programmatic handling, serialized as `type`.
    #[serde(rename = "type")]
    pub kind: ProblemKind,
    /// Context for people, never an input to retry classification.
    pub detail: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lease_loss_is_not_a_transient_request_retry() {
        assert!(!ProblemKind::LeaseExpired.is_transient());
        assert!(!ProblemKind::RenewalClosed.is_transient());
        assert!(!ProblemKind::AlreadyExists.is_transient());
        assert!(ProblemKind::Unavailable.is_transient());
    }
    #[test]
    fn unknown_problem_remains_non_retryable() {
        let problem: Problem = serde_json::from_str(
            r#"{"type":"urn:future:unknown","detail":"later protocol"}"#,
        )
        .unwrap();
        assert_eq!(problem.kind, ProblemKind::Unknown);
        assert!(!problem.kind.is_transient());
    }
}
