//! An operation owns one renewal worker and never substitutes another grant.
use super::error::Error;
use super::timing::Timing;
use super::transport::{Retry, Transport};
use rdedup_protocol::lease::{
    AcquireLease, Acquisition, LeaseGrant, LeaseId, LeaseMode, LeaseRequestId,
};
use rdedup_protocol::problem::ProblemKind;
use rdedup_protocol::route;
use reqwest::Method;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
enum Loss {
    Rejected(ProblemKind),
    InvalidResponse,
}
impl Loss {
    fn into_error(self) -> Error {
        match self {
            Self::Rejected(kind) => Error::Problem(kind),
            Self::InvalidResponse => Error::Protocol,
        }
    }
}
struct Check {
    reply: Sender<Result<(), Loss>>,
}
enum Command {
    Check(Check),
    Release,
}
pub(super) struct Lease {
    id: LeaseId,
    sender: Sender<Command>,
    worker: Option<JoinHandle<()>>,
}
impl Lease {
    pub(super) fn acquire(
        transport: Transport,
        mode: LeaseMode,
    ) -> Result<Self, Error> {
        let request = LeaseRequestId::generate();
        let mut pending = Pending {
            transport: transport.clone(),
            request: Some(request),
        };
        let url = transport
            .endpoint(route::LEASE_REQUESTS, Some(&request.to_string()))?;
        let deadline = Instant::now() + Duration::from_secs(300);
        let (grant, timing) = loop {
            let started = Instant::now();
            let response = transport.execute(Retry::Idempotent, || {
                Ok(transport
                    .request(Method::PUT, url.clone())
                    .json(&AcquireLease { mode }))
            })?;
            let acquisition: Acquisition = response.json()?;
            match acquisition {
                Acquisition::Granted { lease } => {
                    if lease.mode != mode {
                        return Err(Error::Protocol);
                    }
                    let timing = Timing::new(&lease, started)?;
                    timing.ensure_active(Instant::now())?;
                    break (lease, timing);
                }
                Acquisition::Closed => {
                    return Err(Error::Problem(ProblemKind::LeaseExpired))
                }
                Acquisition::Queued => {
                    if Instant::now() >= deadline {
                        return Err(Error::AcquisitionTimeout);
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        };
        let id = grant.id;
        let (sender, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("rdedup-http-lease".into())
            .spawn(move || {
                Renewal {
                    transport,
                    id,
                    timing,
                    receiver,
                }
                .run()
            })
            .map_err(Error::Stream)?;
        pending.request = None;
        Ok(Self {
            id,
            sender,
            worker: Some(worker),
        })
    }
    pub(super) fn id(&self) -> LeaseId {
        self.id
    }
    pub(super) fn check(&self) -> Result<(), Error> {
        let (reply, receiver) = mpsc::channel();
        self.sender
            .send(Command::Check(Check { reply }))
            .map_err(|_| Error::CoordinatorStopped)?;
        receiver
            .recv()
            .map_err(|_| Error::CoordinatorStopped)?
            .map_err(Loss::into_error)
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
struct Pending {
    transport: Transport,
    request: Option<LeaseRequestId>,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(request) = self.request {
            if let Ok(url) = self
                .transport
                .endpoint(route::LEASE_REQUESTS, Some(&request.to_string()))
            {
                let _ = self.transport.execute(Retry::Idempotent, || {
                    Ok(self.transport.request(Method::DELETE, url.clone()))
                });
            }
        }
    }
}
struct Renewal {
    transport: Transport,
    id: LeaseId,
    timing: Timing,
    receiver: Receiver<Command>,
}
impl Renewal {
    fn run(mut self) {
        let mut lost = None;
        let mut next =
            Instant::now() + self.timing.renewal_delay(Instant::now());
        loop {
            let now = Instant::now();
            if lost.is_none() && self.timing.ensure_active(now).is_err() {
                lost = Some(Loss::Rejected(ProblemKind::LeaseExpired));
            }
            if lost.is_none() && now >= next {
                match self.renew() {
                    Ok(()) => {
                        next = Instant::now()
                            + self.timing.renewal_delay(Instant::now())
                    }
                    Err(error) if error.is_transient() => {
                        next = Instant::now() + Duration::from_millis(100)
                    }
                    Err(Error::Problem(kind)) => {
                        lost = Some(Loss::Rejected(kind))
                    }
                    Err(_) => lost = Some(Loss::InvalidResponse),
                }
                continue;
            }
            let wait = if lost.is_some() {
                Duration::from_secs(60)
            } else {
                next.saturating_duration_since(now)
            };
            match self.receiver.recv_timeout(wait) {
                Ok(Command::Check(check)) => {
                    let result = lost.map_or_else(
                        || {
                            self.timing.ensure_active(Instant::now()).map_err(
                                |_| Loss::Rejected(ProblemKind::LeaseExpired),
                            )
                        },
                        Err,
                    );
                    let _ = check.reply.send(result);
                }
                Ok(Command::Release)
                | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        if let Ok(url) = self
            .transport
            .endpoint(route::LEASES, Some(&self.id.to_string()))
        {
            let _ = self.transport.execute(Retry::Idempotent, || {
                Ok(self.transport.request(Method::DELETE, url.clone()))
            });
        }
    }
    fn renew(&mut self) -> Result<(), Error> {
        let url = self
            .transport
            .endpoint(route::LEASES, Some(&self.id.to_string()))?;
        let started = Instant::now();
        let grant: LeaseGrant = self
            .transport
            .execute(Retry::Idempotent, || {
                Ok(self.transport.request(Method::PATCH, url.clone()))
            })?
            .json()?;
        self.timing.update(&grant, started)
    }
}
