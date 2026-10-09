//! One channel-owned lease schedule; filesystem locking runs off the async executor.
use super::schedule::{self, Admission, Schedule};
use super::timing::LeasePolicy;
use super::RequestPolicy;
use rdedup_lib::backends::local::{Local, LocalOperation};
use rdedup_lib::backends::{Exclusive, Shared};
use rdedup_protocol::lease::{Acquisition, LeaseId, LeaseMode, LeaseRequestId};
use rdedup_protocol::problem::ProblemKind;
use std::collections::HashMap;
use std::io;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

/// The authenticated credential identity, or the public-read principal.
/// Tokens themselves never enter scheduling or diagnostic state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Principal {
    /// Public reads and public shared leases belong to this principal.
    Anonymous,
    /// A configured credential's name, after authentication.
    Credential(super::CredentialName),
}

/// Failures callers can distinguish without parsing error prose.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The coordinator has stopped, invalidating all its grants.
    #[error("lease coordinator is stopped")]
    Stopped,
    /// A stable protocol rejection such as lease loss or wrong ownership.
    #[error("lease operation rejected: {0:?}")]
    Rejected(ProblemKind),
    /// Local filesystem protection could not be acquired.
    #[error("local repository protection failed")]
    Storage(#[source] io::Error),
}

impl From<schedule::Error> for Error {
    fn from(error: schedule::Error) -> Self {
        let kind = match error {
            schedule::Error::RequestConflict => ProblemKind::RequestConflict,
            schedule::Error::RequestAbsent => ProblemKind::NotFound,
            schedule::Error::LeaseAbsent => ProblemKind::LeaseExpired,
            schedule::Error::Timing(super::timing::Error::RenewalClosed) => {
                ProblemKind::RenewalClosed
            }
            schedule::Error::Timing(super::timing::Error::Expired) => {
                ProblemKind::LeaseExpired
            }
            schedule::Error::NotEligible
            | schedule::Error::DuplicateLease
            | schedule::Error::Timing(_) => ProblemKind::StorageFailure,
        };
        Self::Rejected(kind)
    }
}

#[derive(Clone, Copy, Debug)]
enum Validity {
    Active { expires: Instant },
    Expired,
}

/// A receiver observes extensions of this lease only; it never selects another lease.
#[derive(Clone)]
pub struct LeaseCheck {
    validity: watch::Receiver<Validity>,
}

impl LeaseCheck {
    /// Check permission immediately before an object I/O step or publication.
    ///
    /// # Errors
    /// Returns lease loss after expiry, release, or coordinator shutdown.
    pub fn ensure_active(&self) -> Result<(), Error> {
        if self.validity.has_changed().is_err() {
            return Err(Error::Stopped);
        }
        match *self.validity.borrow() {
            Validity::Active { expires } if Instant::now() < expires => Ok(()),
            _ => Err(Error::Rejected(ProblemKind::LeaseExpired)),
        }
    }
}

/// Permission to read objects under a lease, including anonymous public reads.
pub struct ReadOnly;

/// Permission to add objects, obtained only for a credential-owned lease.
pub struct Writable;

/// Storage permission retaining the exact lease and its local OS lock.
///
/// Public-read permits cannot publish objects:
/// ```compile_fail
/// use rdedup_server::lease::{SharedPermit, ReadOnly};
/// fn publish(permit: SharedPermit<ReadOnly>) {
///     permit.create_object(std::path::Path::new("object")).unwrap();
/// }
/// ```
pub struct SharedPermit<Access> {
    access: PhantomData<Access>,
    pub(super) operation: Arc<LocalOperation<Shared>>,
    pub(super) validity: LeaseCheck,
}

/// Destructive storage permission unavailable from a shared lease.
pub struct ExclusivePermit {
    pub(super) operation: Arc<LocalOperation<Exclusive>>,
    pub(super) validity: LeaseCheck,
}

enum Protection {
    Shared(Arc<LocalOperation<Shared>>),
    Exclusive(Arc<LocalOperation<Exclusive>>),
}

struct HeldLease {
    request: LeaseRequestId,
    protection: Protection,
    validity: watch::Sender<Validity>,
}

struct OwnedRequest {
    principal: Principal,
    failure: Option<io::Error>,
    last_contact: Instant,
    closed_at: Option<Instant>,
}

impl OwnedRequest {
    fn authorize(&self, principal: &Principal) -> Result<(), Error> {
        if &self.principal != principal {
            return Err(Error::Rejected(ProblemKind::Forbidden));
        }
        if let Some(error) = &self.failure {
            return Err(Error::Storage(io::Error::new(
                error.kind(),
                error.to_string(),
            )));
        }
        Ok(())
    }
}

enum Command {
    Acquire(Acquire),
    Status(Status),
    Cancel(Cancel),
    Renew(Renew),
    Release(Release),
    Shared(SharedAccess),
    Writable(WritableAccess),
    Exclusive(ExclusiveAccess),
}
struct Acquire {
    principal: Principal,
    request: LeaseRequestId,
    mode: LeaseMode,
    reply: oneshot::Sender<Result<Acquisition, Error>>,
}
struct Status {
    principal: Principal,
    request: LeaseRequestId,
    reply: oneshot::Sender<Result<Acquisition, Error>>,
}
struct Cancel {
    principal: Principal,
    request: LeaseRequestId,
    reply: oneshot::Sender<Result<(), Error>>,
}
struct Renew {
    principal: Principal,
    lease: LeaseId,
    reply: oneshot::Sender<Result<Acquisition, Error>>,
}
struct Release {
    principal: Principal,
    lease: LeaseId,
    reply: oneshot::Sender<Result<(), Error>>,
}
struct SharedAccess {
    principal: Principal,
    lease: LeaseId,
    reply: oneshot::Sender<Result<SharedPermit<ReadOnly>, Error>>,
}
struct WritableAccess {
    principal: Principal,
    lease: LeaseId,
    reply: oneshot::Sender<Result<SharedPermit<Writable>, Error>>,
}
struct ExclusiveAccess {
    principal: Principal,
    lease: LeaseId,
    reply: oneshot::Sender<Result<ExclusivePermit, Error>>,
}

/// Cloneable command endpoint. Dropping every endpoint stops the actor and its grants.
#[derive(Clone)]
pub struct LeaseService {
    commands: mpsc::Sender<Command>,
}

impl LeaseService {
    /// Start one repository's coordinator in the current Tokio runtime.
    ///
    /// # Panics
    /// Requires an active Tokio runtime with time support.
    pub fn start(path: PathBuf, policy: LeasePolicy) -> Self {
        Self::with_request_policy(path, policy, RequestPolicy::default())
    }

    /// Start with explicit bounds for queued and retained acquisition records.
    ///
    /// # Panics
    /// Requires an active Tokio runtime with time support.
    pub fn with_request_policy(
        path: PathBuf,
        policy: LeasePolicy,
        request_policy: RequestPolicy,
    ) -> Self {
        let (commands, receiver) = mpsc::channel(128);
        tokio::spawn(
            Coordinator {
                local: Arc::new(Local::new(path)),
                request_policy,
                schedule: Schedule::new(policy),
                requests: HashMap::new(),
                leases: HashMap::new(),
                pending: None,
            }
            .run(receiver),
        );
        Self { commands }
    }

    /// Enqueue an idempotent acquisition; polling returns the eventual grant.
    /// Queued requests need polling within the configured idle timeout. Terminal
    /// responses remain recognizable for the configured retention interval.
    /// Clients use fresh request identities for new attempts.
    ///
    /// # Errors
    /// Returns coordinator shutdown, rejected permission/state, or storage failure.
    pub async fn acquire(
        &self,
        principal: Principal,
        request: LeaseRequestId,
        mode: LeaseMode,
    ) -> Result<Acquisition, Error> {
        let (reply, response) = oneshot::channel();
        self.send(Command::Acquire(Acquire {
            principal,
            request,
            mode,
            reply,
        }))
        .await?;
        response.await.map_err(|_| Error::Stopped)?
    }
    /// Observe the same request after a lost response or while waiting.
    ///
    /// # Errors
    /// Returns coordinator shutdown, rejected permission/state, or storage failure.
    pub async fn status(
        &self,
        principal: Principal,
        request: LeaseRequestId,
    ) -> Result<Acquisition, Error> {
        let (reply, response) = oneshot::channel();
        self.send(Command::Status(Status {
            principal,
            request,
            reply,
        }))
        .await?;
        response.await.map_err(|_| Error::Stopped)?
    }
    /// Cancel a queued request or release its grant.
    ///
    /// # Errors
    /// Returns coordinator shutdown, rejected permission/state, or storage failure.
    pub async fn cancel(
        &self,
        principal: Principal,
        request: LeaseRequestId,
    ) -> Result<(), Error> {
        let (reply, response) = oneshot::channel();
        self.send(Command::Cancel(Cancel {
            principal,
            request,
            reply,
        }))
        .await?;
        response.await.map_err(|_| Error::Stopped)?
    }
    /// Extend this lease without changing its identity or fixed cutoff.
    ///
    /// # Errors
    /// Returns coordinator shutdown, rejected permission/state, or storage failure.
    pub async fn renew(
        &self,
        principal: Principal,
        lease: LeaseId,
    ) -> Result<Acquisition, Error> {
        let (reply, response) = oneshot::channel();
        self.send(Command::Renew(Renew {
            principal,
            lease,
            reply,
        }))
        .await?;
        response.await.map_err(|_| Error::Stopped)?
    }
    /// Release a grant early; any in-flight object still retains its OS lock.
    ///
    /// # Errors
    /// Returns coordinator shutdown, rejected permission/state, or storage failure.
    pub async fn release(
        &self,
        principal: Principal,
        lease: LeaseId,
    ) -> Result<(), Error> {
        let (reply, response) = oneshot::channel();
        self.send(Command::Release(Release {
            principal,
            lease,
            reply,
        }))
        .await?;
        response.await.map_err(|_| Error::Stopped)?
    }
    /// Obtain read access tied to exactly the supplied active lease.
    ///
    /// # Errors
    /// Returns coordinator shutdown, rejected permission/state, or storage failure.
    pub async fn shared(
        &self,
        principal: Principal,
        lease: LeaseId,
    ) -> Result<SharedPermit<ReadOnly>, Error> {
        let (reply, response) = oneshot::channel();
        self.send(Command::Shared(SharedAccess {
            principal,
            lease,
            reply,
        }))
        .await?;
        response.await.map_err(|_| Error::Stopped)?
    }
    /// Obtain additive access for a credential-owned lease tied to exactly the supplied active lease.
    ///
    /// # Errors
    /// Returns coordinator shutdown, rejected permission/state, or storage failure.
    pub async fn writable(
        &self,
        principal: Principal,
        lease: LeaseId,
    ) -> Result<SharedPermit<Writable>, Error> {
        let (reply, response) = oneshot::channel();
        self.send(Command::Writable(WritableAccess {
            principal,
            lease,
            reply,
        }))
        .await?;
        response.await.map_err(|_| Error::Stopped)?
    }
    /// Obtain destructive access only from an active exclusive lease.
    ///
    /// # Errors
    /// Returns coordinator shutdown, rejected permission/state, or storage failure.
    pub async fn exclusive(
        &self,
        principal: Principal,
        lease: LeaseId,
    ) -> Result<ExclusivePermit, Error> {
        let (reply, response) = oneshot::channel();
        self.send(Command::Exclusive(ExclusiveAccess {
            principal,
            lease,
            reply,
        }))
        .await?;
        response.await.map_err(|_| Error::Stopped)?
    }
    async fn send(&self, command: Command) -> Result<(), Error> {
        self.commands
            .send(command)
            .await
            .map_err(|_| Error::Stopped)
    }
}

struct PendingLock {
    admission: Admission,
    result: JoinHandle<io::Result<Protection>>,
}

struct Coordinator {
    local: Arc<Local>,
    request_policy: RequestPolicy,
    schedule: Schedule,
    requests: HashMap<LeaseRequestId, OwnedRequest>,
    leases: HashMap<LeaseId, HeldLease>,
    pending: Option<PendingLock>,
}

impl Coordinator {
    async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        let mut interval = tokio::time::interval(Duration::from_millis(50));
        interval
            .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    self.refresh(Instant::now());
                    self.handle(command);
                }
                result = async {
                    match &mut self.pending {
                        Some(pending) => (&mut pending.result).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(pending) = self.pending.take() {
                        self.complete_lock(pending.admission, result);
                    }
                }
                _ = interval.tick() => {
                    self.refresh(Instant::now());
                    self.try_admission();
                }
            }
        }
        // Closing watch senders invalidates outstanding permits even if their
        // local file handles must remain alive until an I/O step returns.
    }

    fn refresh(&mut self, now: Instant) {
        self.schedule.expire(now);
        self.requests.retain(|id, request| {
            if matches!(self.schedule.status(*id, now), Ok(Acquisition::Queued))
                && self.request_policy.is_abandoned(request.last_contact, now)
            {
                let _ = self.schedule.cancel(*id);
            }
            if matches!(self.schedule.status(*id, now), Ok(Acquisition::Closed))
            {
                let closed_at = *request.closed_at.get_or_insert(now);
                if self.request_policy.can_retire(closed_at, now) {
                    self.schedule.forget_closed(*id);
                    return false;
                }
            }
            true
        });
        self.leases.retain(|id, held| {
            if !self.schedule.has_active(*id, now) {
                held.validity.send_replace(Validity::Expired);
                return false;
            }
            if let Ok(Acquisition::Granted { lease }) =
                self.schedule.status(held.request, now)
            {
                if let Some(expires) = now.checked_add(lease.remaining) {
                    held.validity.send_replace(Validity::Active { expires });
                }
            }
            true
        });
    }

    fn try_admission(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let Some(admission) = self.schedule.next_admission(Instant::now())
        else {
            return;
        };
        let local = Arc::clone(&self.local);
        self.pending = Some(PendingLock {
            admission,
            result: tokio::task::spawn_blocking(move || match admission.mode {
                LeaseMode::Shared => local
                    .try_shared_operation()
                    .map(|operation| Protection::Shared(Arc::new(operation))),
                LeaseMode::Exclusive => {
                    local.try_exclusive_operation().map(|operation| {
                        Protection::Exclusive(Arc::new(operation))
                    })
                }
            }),
        });
    }

    fn complete_lock(
        &mut self,
        admission: Admission,
        result: Result<io::Result<Protection>, tokio::task::JoinError>,
    ) {
        let protection = match result {
            Ok(Ok(protection)) => protection,
            Ok(Err(error))
                if error.raw_os_error()
                    == fs2::lock_contended_error().raw_os_error() =>
            {
                return
            }
            result => {
                let error = match result {
                    Ok(Err(error)) => error,
                    Err(error) => io::Error::other(error),
                    Ok(Ok(_)) => unreachable!(
                        "successful lock acquisition returned above"
                    ),
                };
                if let Some(request) = self.requests.get_mut(&admission.request)
                {
                    request.failure = Some(error);
                }
                let _ = self.schedule.cancel(admission.request);
                return;
            }
        };
        let now = Instant::now();
        let id = LeaseId::generate();
        if let Ok(Acquisition::Granted { lease }) =
            self.schedule.grant(admission, id, now)
        {
            let Some(expires) = now.checked_add(lease.remaining) else {
                self.schedule.release(id);
                return;
            };
            let (validity, _) = watch::channel(Validity::Active { expires });
            self.leases.insert(
                id,
                HeldLease {
                    request: admission.request,
                    protection,
                    validity,
                },
            );
        }
    }

    fn own_request(
        &self,
        principal: &Principal,
        id: LeaseRequestId,
    ) -> Result<(), Error> {
        let request = self
            .requests
            .get(&id)
            .ok_or(Error::Rejected(ProblemKind::NotFound))?;
        request.authorize(principal)
    }

    fn own_lease(
        &self,
        principal: &Principal,
        id: LeaseId,
    ) -> Result<&HeldLease, Error> {
        let held = self
            .leases
            .get(&id)
            .ok_or(Error::Rejected(ProblemKind::LeaseExpired))?;
        self.own_request(principal, held.request)?;
        Ok(held)
    }

    fn handle(&mut self, command: Command) {
        let now = Instant::now();
        match command {
            Command::Acquire(command) => {
                let has_capacity =
                    self.request_policy.has_capacity(self.requests.len());
                let result = match self.requests.entry(command.request) {
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        entry.get().authorize(&command.principal).map(|()| {
                            entry.get_mut().last_contact = now;
                        })
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        if !has_capacity {
                            Err(Error::Rejected(ProblemKind::Unavailable))
                        } else if command.principal == Principal::Anonymous
                            && command.mode == LeaseMode::Exclusive
                        {
                            Err(Error::Rejected(ProblemKind::Unauthorized))
                        } else {
                            entry.insert(OwnedRequest {
                                principal: command.principal,
                                failure: None,
                                last_contact: now,
                                closed_at: None,
                            });
                            Ok(())
                        }
                    }
                }
                .and_then(|()| {
                    self.schedule
                        .request(command.request, command.mode, now)
                        .map_err(Error::from)
                });
                let _ = command.reply.send(result);
            }
            Command::Status(command) => {
                if self
                    .own_request(&command.principal, command.request)
                    .is_ok()
                {
                    if let Some(request) =
                        self.requests.get_mut(&command.request)
                    {
                        request.last_contact = now;
                    }
                }
                let result = self
                    .own_request(&command.principal, command.request)
                    .and_then(|()| {
                        self.schedule
                            .status(command.request, now)
                            .map_err(Error::from)
                    });
                let _ = command.reply.send(result);
            }
            Command::Cancel(command) => {
                let result = self
                    .own_request(&command.principal, command.request)
                    .and_then(|()| {
                        self.schedule
                            .cancel(command.request)
                            .map_err(Error::from)
                    });
                self.refresh(Instant::now());
                let _ = command.reply.send(result);
            }
            Command::Renew(command) => {
                let result = self
                    .own_lease(&command.principal, command.lease)
                    .map(|_| ())
                    .and_then(|()| {
                        self.schedule
                            .renew(command.lease, now)
                            .map_err(Error::from)
                    });
                self.refresh(Instant::now());
                let _ = command.reply.send(result);
            }
            Command::Release(command) => {
                let result = self
                    .own_lease(&command.principal, command.lease)
                    .map(|_| ());
                if result.is_ok() {
                    self.schedule.release(command.lease);
                }
                self.refresh(Instant::now());
                let _ = command.reply.send(result);
            }
            Command::Shared(command) => {
                let result = self
                    .own_lease(&command.principal, command.lease)
                    .map(|held| SharedPermit {
                        access: PhantomData,
                        operation: match &held.protection {
                            Protection::Shared(operation) => {
                                Arc::clone(operation)
                            }
                            Protection::Exclusive(operation) => {
                                Arc::new(operation.shared())
                            }
                        },
                        validity: LeaseCheck {
                            validity: held.validity.subscribe(),
                        },
                    });
                let _ = command.reply.send(result);
            }
            Command::Writable(command) => {
                if command.principal == Principal::Anonymous {
                    let _ = command
                        .reply
                        .send(Err(Error::Rejected(ProblemKind::Unauthorized)));
                    return;
                }
                let result = self
                    .own_lease(&command.principal, command.lease)
                    .map(|held| SharedPermit {
                        access: PhantomData,
                        operation: match &held.protection {
                            Protection::Shared(operation) => {
                                Arc::clone(operation)
                            }
                            Protection::Exclusive(operation) => {
                                Arc::new(operation.shared())
                            }
                        },
                        validity: LeaseCheck {
                            validity: held.validity.subscribe(),
                        },
                    });
                let _ = command.reply.send(result);
            }
            Command::Exclusive(command) => {
                let result = self
                    .own_lease(&command.principal, command.lease)
                    .and_then(|held| match &held.protection {
                        Protection::Exclusive(operation) => {
                            Ok(ExclusivePermit {
                                operation: Arc::clone(operation),
                                validity: LeaseCheck {
                                    validity: held.validity.subscribe(),
                                },
                            })
                        }
                        Protection::Shared(_) => {
                            Err(Error::Rejected(ProblemKind::LeaseMode))
                        }
                    });
                let _ = command.reply.send(result);
            }
        }
        self.refresh(Instant::now());
    }
}
