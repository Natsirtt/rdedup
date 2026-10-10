//! FIFO admission independent of file I/O, task scheduling, and clock sampling.
use super::timing::{self, LeasePolicy, LeaseTiming};
use rdedup_protocol::lease::{Acquisition, LeaseId, LeaseMode, LeaseRequestId};
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("request identity already has a different lease mode")]
    RequestConflict,
    #[error("lease request is absent")]
    RequestAbsent,
    #[error("lease is absent or closed")]
    LeaseAbsent,
    #[error("lease request is not eligible for admission")]
    NotEligible,
    #[error("lease identity is already active")]
    DuplicateLease,
    #[error(transparent)]
    Timing(#[from] timing::Error),
}

#[derive(Debug)]
struct Request {
    mode: LeaseMode,
    state: RequestState,
}

#[derive(Debug)]
enum RequestState {
    Queued,
    Granted(LeaseId),
    Closed,
}

#[derive(Debug)]
struct Lease {
    request: LeaseRequestId,
    timing: LeaseTiming,
}

/// Shared and exclusive grants cannot coexist in the schedule.
#[derive(Debug)]
enum Active {
    Idle,
    Shared(HashMap<LeaseId, Lease>),
    Exclusive { id: LeaseId, lease: Lease },
}

impl Active {
    fn get(&self, id: LeaseId) -> Option<&Lease> {
        match self {
            Self::Idle => None,
            Self::Shared(leases) => leases.get(&id),
            Self::Exclusive { id: active, lease } => {
                (*active == id).then_some(lease)
            }
        }
    }

    fn get_mut(&mut self, id: LeaseId) -> Option<&mut Lease> {
        match self {
            Self::Idle => None,
            Self::Shared(leases) => leases.get_mut(&id),
            Self::Exclusive { id: active, lease } => {
                (*active == id).then_some(lease)
            }
        }
    }
}

/// Eligibility to try the matching OS lock, not permission to use storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Admission {
    pub(super) request: LeaseRequestId,
    pub(super) mode: LeaseMode,
}

#[derive(Debug)]
pub(super) struct Schedule {
    policy: LeasePolicy,
    requests: HashMap<LeaseRequestId, Request>,
    queue: VecDeque<LeaseRequestId>,
    active: Active,
}

impl Schedule {
    pub(super) fn forget_closed(&mut self, id: LeaseRequestId) {
        if self.requests.get(&id).is_some_and(|request| {
            matches!(request.state, RequestState::Closed)
        }) {
            self.requests.remove(&id);
        }
    }

    pub(super) fn has_active(&self, id: LeaseId, now: Instant) -> bool {
        self.active
            .get(id)
            .is_some_and(|lease| lease.timing.ensure_active(now).is_ok())
    }

    pub(super) fn new(policy: LeasePolicy) -> Self {
        Self {
            policy,
            requests: HashMap::new(),
            queue: VecDeque::new(),
            active: Active::Idle,
        }
    }

    pub(super) fn request(
        &mut self,
        id: LeaseRequestId,
        mode: LeaseMode,
        now: Instant,
    ) -> Result<Acquisition, Error> {
        self.expire(now);
        if let Some(request) = self.requests.get(&id) {
            if request.mode != mode {
                return Err(Error::RequestConflict);
            }
            return self.status(id, now);
        }
        if mode == LeaseMode::Exclusive {
            self.close_shared_renewals(now)?;
        }
        self.requests.insert(
            id,
            Request {
                mode,
                state: RequestState::Queued,
            },
        );
        self.queue.push_back(id);
        Ok(Acquisition::Queued)
    }

    pub(super) fn status(
        &self,
        id: LeaseRequestId,
        now: Instant,
    ) -> Result<Acquisition, Error> {
        let request = self.requests.get(&id).ok_or(Error::RequestAbsent)?;
        match request.state {
            RequestState::Queued => Ok(Acquisition::Queued),
            RequestState::Closed => Ok(Acquisition::Closed),
            RequestState::Granted(id) => {
                let lease = self.active.get(id).ok_or(Error::LeaseAbsent)?;
                Ok(Acquisition::Granted {
                    lease: lease.timing.describe(id, request.mode, now)?,
                })
            }
        }
    }

    pub(super) fn next_admission(&mut self, now: Instant) -> Option<Admission> {
        self.expire(now);
        let request = *self.queue.front()?;
        let mode = self.requests.get(&request)?.mode;
        match (&self.active, mode) {
            (Active::Idle, _) | (Active::Shared(_), LeaseMode::Shared) => {
                Some(Admission { request, mode })
            }
            _ => None,
        }
    }

    /// Called only after the coordinator obtains matching local protection.
    /// A cancelled admission cannot become a grant after asynchronous lock I/O.
    pub(super) fn grant(
        &mut self,
        admission: Admission,
        id: LeaseId,
        now: Instant,
    ) -> Result<Acquisition, Error> {
        if self.next_admission(now) != Some(admission) {
            return Err(Error::NotEligible);
        }
        if self.active.get(id).is_some() {
            return Err(Error::DuplicateLease);
        }
        let mut timing = LeaseTiming::grant(now, self.policy)?;
        if admission.mode == LeaseMode::Shared
            && self.queue.iter().skip(1).any(|request| {
                self.requests
                    .get(request)
                    .is_some_and(|request| request.mode == LeaseMode::Exclusive)
            })
        {
            timing.close_renewal(now, self.policy)?;
        }
        let lease = Lease {
            request: admission.request,
            timing,
        };
        match (&mut self.active, admission.mode) {
            (Active::Idle, LeaseMode::Shared) => {
                self.active = Active::Shared(HashMap::from([(id, lease)]));
            }
            (Active::Shared(leases), LeaseMode::Shared) => {
                leases.insert(id, lease);
            }
            (Active::Idle, LeaseMode::Exclusive) => {
                self.active = Active::Exclusive { id, lease };
            }
            _ => return Err(Error::NotEligible),
        }
        self.queue.pop_front();
        self.requests
            .get_mut(&admission.request)
            .ok_or(Error::RequestAbsent)?
            .state = RequestState::Granted(id);
        self.status(admission.request, now)
    }

    pub(super) fn renew(
        &mut self,
        id: LeaseId,
        now: Instant,
    ) -> Result<Acquisition, Error> {
        self.expire(now);
        let lease = self.active.get_mut(id).ok_or(Error::LeaseAbsent)?;
        lease.timing.renew(now, self.policy)?;
        let request = lease.request;
        self.status(request, now)
    }

    pub(super) fn release(&mut self, id: LeaseId) {
        let request = match &mut self.active {
            Active::Idle => None,
            Active::Shared(leases) => {
                leases.remove(&id).map(|lease| lease.request)
            }
            Active::Exclusive { id: active, lease } => {
                (*active == id).then_some(lease.request)
            }
        };
        if let Some(request) = request {
            if let Some(record) = self.requests.get_mut(&request) {
                record.state = RequestState::Closed;
            }
            match &self.active {
                Active::Exclusive { .. } => self.active = Active::Idle,
                Active::Shared(leases) if leases.is_empty() => {
                    self.active = Active::Idle
                }
                _ => {}
            }
        }
    }

    pub(super) fn cancel(&mut self, id: LeaseRequestId) -> Result<(), Error> {
        let request = self.requests.get_mut(&id).ok_or(Error::RequestAbsent)?;
        match request.state {
            RequestState::Granted(lease) => self.release(lease),
            RequestState::Queued => {
                request.state = RequestState::Closed;
                self.queue.retain(|queued| *queued != id);
            }
            RequestState::Closed => {}
        }
        Ok(())
    }

    pub(super) fn expire(&mut self, now: Instant) -> Vec<LeaseId> {
        let expired: Vec<_> = match &self.active {
            Active::Idle => Vec::new(),
            Active::Shared(leases) => leases
                .iter()
                .filter_map(|(id, lease)| {
                    (now >= lease.timing.expires()).then_some(*id)
                })
                .collect(),
            Active::Exclusive { id, lease } => {
                if now >= lease.timing.expires() {
                    vec![*id]
                } else {
                    Vec::new()
                }
            }
        };
        for id in &expired {
            self.release(*id);
        }
        expired
    }

    fn close_shared_renewals(&mut self, now: Instant) -> Result<(), Error> {
        if let Active::Shared(leases) = &mut self.active {
            for lease in leases.values_mut() {
                lease.timing.close_renewal(now, self.policy)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdedup_protocol::lease::RenewalWindow;
    use std::time::Duration;
    use uuid::Uuid;

    fn request(number: u128) -> LeaseRequestId {
        LeaseRequestId::from_uuid(Uuid::from_u128(number))
    }

    fn lease(number: u128) -> LeaseId {
        LeaseId::from_uuid(Uuid::from_u128(number))
    }

    fn schedule(now: Instant) -> Schedule {
        Schedule::new(
            LeasePolicy::new(
                Duration::from_secs(30),
                Duration::from_secs(90),
                now,
            )
            .unwrap(),
        )
    }

    fn admit(
        schedule: &mut Schedule,
        number: u128,
        now: Instant,
    ) -> Acquisition {
        let admission = schedule.next_admission(now).unwrap();
        assert_eq!(admission.request, request(number));
        schedule.grant(admission, lease(number), now).unwrap()
    }

    #[test]
    fn exclusive_waits_for_every_shared_holder_and_blocks_new_readers() {
        let now = Instant::now();
        let mut schedule = schedule(now);
        for number in [1, 2] {
            schedule
                .request(request(number), LeaseMode::Shared, now)
                .unwrap();
            admit(&mut schedule, number, now);
        }
        schedule
            .request(request(3), LeaseMode::Exclusive, now)
            .unwrap();
        schedule
            .request(request(4), LeaseMode::Shared, now)
            .unwrap();
        assert_eq!(schedule.next_admission(now), None);
        schedule.release(lease(1));
        assert_eq!(schedule.next_admission(now), None);
        schedule.release(lease(2));
        admit(&mut schedule, 3, now);
        assert_eq!(schedule.next_admission(now), None);
        schedule.release(lease(3));
        admit(&mut schedule, 4, now);
    }

    #[test]
    fn repeated_acquisition_returns_the_same_grant_and_rejects_changed_contents(
    ) {
        let now = Instant::now();
        let mut schedule = schedule(now);
        schedule
            .request(request(1), LeaseMode::Shared, now)
            .unwrap();
        let granted = admit(&mut schedule, 1, now);
        assert_eq!(
            schedule
                .request(request(1), LeaseMode::Shared, now)
                .unwrap(),
            granted
        );
        assert!(matches!(
            schedule.request(request(1), LeaseMode::Exclusive, now),
            Err(Error::RequestConflict)
        ));
        schedule.release(lease(1));
        assert_eq!(
            schedule
                .request(request(1), LeaseMode::Shared, now)
                .unwrap(),
            Acquisition::Closed
        );
    }

    #[test]
    fn cancelled_exclusive_requests_do_not_reset_a_shared_cutoff() {
        let now = Instant::now();
        let mut schedule = schedule(now);
        schedule
            .request(request(1), LeaseMode::Shared, now)
            .unwrap();
        admit(&mut schedule, 1, now);
        schedule
            .request(request(2), LeaseMode::Exclusive, now)
            .unwrap();
        schedule.cancel(request(2)).unwrap();
        for seconds in [20, 40, 60, 80] {
            let later = now + Duration::from_secs(seconds);
            let renewed = schedule.renew(lease(1), later).unwrap();
            let Acquisition::Granted { lease: renewed } = renewed else {
                panic!("missing grant")
            };
            assert_eq!(
                renewed.renewal,
                RenewalWindow::Closing {
                    remaining: Duration::from_secs(90 - seconds),
                }
            );
        }
        let cutoff = now + Duration::from_secs(90);
        schedule
            .request(request(3), LeaseMode::Exclusive, cutoff)
            .unwrap();
        assert!(schedule.renew(lease(1), cutoff).is_err());
        admit(&mut schedule, 3, cutoff);
    }

    #[test]
    fn cancellation_while_obtaining_the_os_lock_prevents_granting() {
        let now = Instant::now();
        let mut schedule = schedule(now);
        schedule
            .request(request(1), LeaseMode::Exclusive, now)
            .unwrap();
        let admission = schedule.next_admission(now).unwrap();
        schedule.cancel(request(1)).unwrap();
        assert!(matches!(
            schedule.grant(admission, lease(1), now),
            Err(Error::NotEligible)
        ));
        schedule
            .request(request(2), LeaseMode::Shared, now)
            .unwrap();
        admit(&mut schedule, 2, now);
    }

    #[test]
    fn a_queued_reader_ahead_of_an_exclusive_has_its_own_fixed_grace() {
        let now = Instant::now();
        let mut schedule = schedule(now);
        schedule
            .request(request(1), LeaseMode::Exclusive, now)
            .unwrap();
        admit(&mut schedule, 1, now);
        schedule
            .request(request(2), LeaseMode::Shared, now)
            .unwrap();
        schedule
            .request(request(3), LeaseMode::Exclusive, now)
            .unwrap();
        let later = now + Duration::from_secs(30);
        let granted = admit(&mut schedule, 2, later);
        let Acquisition::Granted { lease } = granted else {
            panic!("missing grant")
        };
        assert_eq!(lease.remaining, Duration::from_secs(30));
        assert_eq!(
            lease.renewal,
            RenewalWindow::Closing {
                remaining: Duration::from_secs(90)
            }
        );
        assert_eq!(schedule.next_admission(later), None);
    }
}
