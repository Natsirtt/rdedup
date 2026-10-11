//! HTTP repository service with operation-owned protection.
#![deny(missing_docs)]
pub mod lease;

mod authentication;
pub mod configuration;
mod setting;

mod object;
mod response;
pub mod server;
mod transfer;
