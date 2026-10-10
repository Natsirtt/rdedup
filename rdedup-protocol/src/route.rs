//! Versioned endpoint roots and router patterns used by both sides.
/// Repository object endpoint root.
pub const OBJECTS: &str = "/api/v1/objects";
/// Directory listing and exclusive removal endpoint.
pub const DIRECTORIES: &str = "/api/v1/directories";
/// General exclusive rename endpoint.
pub const RENAMES: &str = "/api/v1/renames";
/// Additive chunk promotion endpoint.
pub const PROMOTIONS: &str = "/api/v1/promotions";
/// Idempotent acquisition request endpoint root.
pub const LEASE_REQUESTS: &str = "/api/v1/lease-requests";
/// Granted lease renewal and release endpoint root.
pub const LEASES: &str = "/api/v1/leases";
/// Header carrying the operation's lease identity.
pub const LEASE_HEADER: &str = "x-rdedup-lease";
/// Header carrying object metadata in a HEAD response.
pub const METADATA_HEADER: &str = "x-rdedup-object-metadata";

/// Object router pattern; clients append encoded relative path segments to `OBJECTS`.
pub fn object_pattern() -> String {
    format!("{OBJECTS}/{{*path}}")
}
/// Acquisition router pattern sharing the client endpoint root.
pub fn lease_request_pattern() -> String {
    format!("{LEASE_REQUESTS}/{{request_id}}")
}
/// Renewal/release router pattern sharing the client endpoint root.
pub fn lease_pattern() -> String {
    format!("{LEASES}/{{lease_id}}")
}
