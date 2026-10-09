//! Lease admission and lifetime policy, separated from transport and storage.
mod coordinator;
mod identity;
mod object;
mod request;
mod schedule;
mod timing;
pub use coordinator::{ReadOnly, Writable};

pub use coordinator::{
    Error, ExclusivePermit, LeaseCheck, LeaseService, Principal, SharedPermit,
};
pub use identity::{CredentialName, Error as CredentialNameError};
pub use object::{Error as ObjectError, LeaseReader, LeaseWriter};
pub use request::{
    Error as RequestPolicyError, RequestCapacity, RequestPolicy,
};
pub use timing::{Error as PolicyError, LeasePolicy};
