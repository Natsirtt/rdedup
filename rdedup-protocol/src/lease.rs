//! Lease acquisition and lifecycle messages with distinct request and lease identities.
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use std::time::Duration;
use uuid::Uuid;

/// Invalid externally supplied lease identifiers.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A lease identifier is not a UUID.
    #[error("invalid lease identifier")]
    LeaseId(#[source] uuid::Error),
    /// An acquisition request identifier is not a UUID.
    #[error("invalid lease request identifier")]
    RequestId(#[source] uuid::Error),
}

/// Identity of one granted lease, never an acquisition request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LeaseId(Uuid);
impl LeaseId {
    /// Generate an unpredictable identity for a new grant.
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }
    /// Attach lease identity to a UUID supplied by an identity source.
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }
}
impl fmt::Display for LeaseId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}
impl FromStr for LeaseId {
    type Err = Error;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(text).map(Self).map_err(Error::LeaseId)
    }
}

/// Stable client identity for an idempotent lease acquisition attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LeaseRequestId(Uuid);
impl LeaseRequestId {
    /// Generate an acquisition identity to reuse across lost responses.
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }
    /// Attach acquisition identity to a UUID supplied by an identity source.
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }
}
impl fmt::Display for LeaseRequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}
impl FromStr for LeaseRequestId {
    type Err = Error;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(text).map(Self).map_err(Error::RequestId)
    }
}

/// Repository operations admitted by a lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseMode {
    /// Concurrent reads and additive writes.
    Shared,
    /// Removal, GC, and replacement excluding other repository operations.
    Exclusive,
}

/// Contents of an idempotent acquisition request; lifetime is server-controlled.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcquireLease {
    /// Protection required by the complete repository operation.
    pub mode: LeaseMode,
}

/// Renewal availability relative to the server's response time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RenewalWindow {
    /// No waiting exclusive request has established a cutoff.
    Open,
    /// A fixed server deadline limits renewal. Later replies cannot move it.
    Closing {
        /// Time remaining until that deadline, measured on the server.
        remaining: Duration,
    },
}

/// A lease grant with relative timing, independent of cross-machine wall clocks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseGrant {
    /// Identity required by object requests and renewal/release operations.
    pub id: LeaseId,
    /// The protection actually granted.
    pub mode: LeaseMode,
    /// Remaining validity when the server produced this response.
    /// Clients account for request transit time conservatively.
    pub remaining: Duration,
    /// The fixed renewal cutoff, if an exclusive request has established one.
    pub renewal: RenewalWindow,
}

/// Acquisition states cannot contain both a pending request and a grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Acquisition {
    /// The request is queued behind incompatible operations.
    Queued,
    /// The request owns the indicated lease.
    Granted {
        /// Protection and timing for the admitted operation.
        lease: LeaseGrant,
    },
    /// The retained request was cancelled or expired. Clients start a new
    /// attempt with a fresh request identity, never reuse a terminal one.
    Closed,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grant_round_trip_preserves_relative_cutoff() {
        let grant = Acquisition::Granted {
            lease: LeaseGrant {
                id: LeaseId::from_uuid(Uuid::from_u128(7)),
                mode: LeaseMode::Shared,
                remaining: Duration::from_secs(30),
                renewal: RenewalWindow::Closing {
                    remaining: Duration::from_secs(60),
                },
            },
        };
        let encoded = serde_json::to_string(&grant).unwrap();
        assert_eq!(
            serde_json::from_str::<Acquisition>(&encoded).unwrap(),
            grant
        );
    }
    #[test]
    fn malformed_identity_and_mode_are_rejected() {
        assert!("invalid".parse::<LeaseId>().is_err());
        assert!(serde_json::from_str::<AcquireLease>(
            r#"{"mode":"unprotected"}"#
        )
        .is_err());
    }
}
