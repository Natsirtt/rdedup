//! HTTP failures retain machine-readable causes through the backend I/O boundary.
use rdedup_protocol::problem::ProblemKind;
use std::io;

/// HTTP configuration, transport, protocol and lease failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The URL cannot identify a repository without ambiguity.
    #[error("invalid HTTP repository URL")]
    RepositoryUrl,
    /// The configured secret has an invalid encoding.
    #[error("HTTP repository credential is invalid")]
    Credential,
    /// Connection, request or response decoding failed.
    #[error("HTTP request failed")]
    Request(#[from] reqwest::Error),
    /// Stable server classification, retained for caller decisions.
    #[error("HTTP repository rejected the operation: {0:?}")]
    Problem(ProblemKind),
    /// The response does not satisfy the shared wire contract.
    #[error("HTTP repository returned an invalid or incomplete response")]
    Protocol,
    /// Admission did not finish within the bounded acquisition interval.
    #[error("HTTP repository lease acquisition timed out")]
    AcquisitionTimeout,
    /// The operation can no longer establish that its lease is valid.
    #[error("HTTP repository lease coordination stopped")]
    CoordinatorStopped,
    /// Streaming or renewal-worker setup failed.
    #[error("HTTP response stream failed")]
    Stream(#[from] io::Error),
}
impl Error {
    pub(super) fn is_transient(&self) -> bool {
        match self {
            Self::Problem(kind) => kind.is_transient(),
            Self::Request(error) => {
                error.is_connect()
                    || error.is_timeout()
                    || error.is_body()
                    || error.is_request()
            }
            _ => false,
        }
    }
}
impl From<Error> for io::Error {
    fn from(error: Error) -> Self {
        if matches!(
            error,
            Error::Problem(
                ProblemKind::LeaseExpired | ProblemKind::RenewalClosed
            ) | Error::CoordinatorStopped
        ) {
            return crate::backends::ProtectionError::Lost {
                source: Box::new(error),
            }
            .into();
        }
        let kind = match &error {
            Error::RepositoryUrl | Error::Credential => {
                io::ErrorKind::InvalidInput
            }
            Error::Problem(ProblemKind::NotFound) => io::ErrorKind::NotFound,
            Error::Problem(ProblemKind::AlreadyExists) => {
                io::ErrorKind::AlreadyExists
            }
            Error::Problem(
                ProblemKind::Unauthorized
                | ProblemKind::Forbidden
                | ProblemKind::LeaseMode,
            ) => io::ErrorKind::PermissionDenied,
            Error::Problem(
                ProblemKind::LeaseExpired | ProblemKind::RenewalClosed,
            )
            | Error::CoordinatorStopped => io::ErrorKind::Interrupted,
            Error::Protocol => io::ErrorKind::InvalidData,
            Error::Stream(error) => error.kind(),
            _ => io::ErrorKind::Other,
        };
        io::Error::new(kind, error)
    }
}
