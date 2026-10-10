//! Shared, versioned HTTP messages, identifiers, routes, and problem types.
//! Client and server compile against this contract; storage implementation and
//! lease scheduling remain outside this crate.
#![deny(missing_docs)]

pub mod lease;
pub mod object;
pub mod problem;
pub mod route;
