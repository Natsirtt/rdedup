use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseMode {
    Shared,
    Exclusive,
}

#[derive(Clone, Debug)]
pub struct LeaseGrant {
    pub id: Uuid,
    pub mode: LeaseMode,
    pub expires_at: Instant,
    pub renewal_deadline: Option<Instant>,
}

#[derive(Clone, Debug)]
pub enum AcquireResult {
    Granted(LeaseGrant),
    Pending(Uuid),
}

#[derive(Clone, Debug)]
pub enum RequestStatus {
    Pending,
    Granted(LeaseGrant),
    Gone,
}

#[derive(Debug, Error)]
pub enum LeaseError {
    #[error("lease does not exist or has expired")]
    Gone,
    #[error("lease renewal is no longer allowed while an exclusive request is waiting")]
    RenewalClosed,
    #[error("lease request does not exist")]
    RequestNotFound,
}

struct LeaseRecord {
    grant: LeaseGrant,
}

struct RequestRecord {
    mode: LeaseMode,
    created_at: Instant,
    status: StoredRequestStatus,
}

enum StoredRequestStatus {
    Pending,
    Granted(Uuid),
    Gone,
}

#[derive(Default)]
struct LeaseState {
    leases: HashMap<Uuid, LeaseRecord>,
    requests: HashMap<Uuid, RequestRecord>,
    requests_by_key: HashMap<String, Uuid>,
    queue: VecDeque<Uuid>,
    renewal_deadline: Option<Instant>,
}

pub struct LeaseManager {
    state: Mutex<LeaseState>,
    lease_ttl: Duration,
    renewal_grace: Duration,
    request_ttl: Duration,
}

impl LeaseManager {
    pub fn new(
        lease_ttl: Duration,
        renewal_grace: Duration,
        request_ttl: Duration,
    ) -> Self {
        LeaseManager {
            state: Mutex::new(LeaseState::default()),
            lease_ttl,
            renewal_grace,
            request_ttl,
        }
    }

    pub fn acquire(
        &self,
        mode: LeaseMode,
        idempotency_key: String,
    ) -> AcquireResult {
        let now = Instant::now();
        let mut state = self.state.lock().expect("lease state lock poisoned");
        self.expire_and_promote(&mut state, now);

        if let Some(request_id) = state.requests_by_key.get(&idempotency_key) {
            return Self::request_result(&state, *request_id);
        }

        let request_id = Uuid::new_v4();
        let record = RequestRecord {
            mode,
            created_at: now,
            status: StoredRequestStatus::Pending,
        };
        state.requests.insert(request_id, record);
        state.requests_by_key.insert(idempotency_key, request_id);

        if state.queue.is_empty() && self.can_grant(&state, mode) {
            self.grant_request(&mut state, request_id, now);
            return Self::request_result(&state, request_id);
        }

        if mode == LeaseMode::Exclusive && state.renewal_deadline.is_none() {
            state.renewal_deadline = Some(now + self.renewal_grace);
        }
        state.queue.push_back(request_id);
        AcquireResult::Pending(request_id)
    }

    pub fn status(
        &self,
        request_id: Uuid,
    ) -> Result<RequestStatus, LeaseError> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("lease state lock poisoned");
        self.expire_and_promote(&mut state, now);
        let request = state
            .requests
            .get(&request_id)
            .ok_or(LeaseError::RequestNotFound)?;

        Ok(match request.status {
            StoredRequestStatus::Pending => RequestStatus::Pending,
            StoredRequestStatus::Gone => RequestStatus::Gone,
            StoredRequestStatus::Granted(lease_id) => state
                .leases
                .get(&lease_id)
                .map(|lease| RequestStatus::Granted(lease.grant.clone()))
                .unwrap_or(RequestStatus::Gone),
        })
    }

    pub fn request_mode(
        &self,
        request_id: Uuid,
    ) -> Result<LeaseMode, LeaseError> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("lease state lock poisoned");
        self.expire_and_promote(&mut state, now);
        state
            .requests
            .get(&request_id)
            .map(|request| request.mode)
            .ok_or(LeaseError::RequestNotFound)
    }

    pub fn lease_mode(&self, lease_id: Uuid) -> Result<LeaseMode, LeaseError> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("lease state lock poisoned");
        self.expire_and_promote(&mut state, now);
        state
            .leases
            .get(&lease_id)
            .map(|lease| lease.grant.mode)
            .ok_or(LeaseError::Gone)
    }

    pub fn renew(&self, lease_id: Uuid) -> Result<LeaseGrant, LeaseError> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("lease state lock poisoned");
        self.expire_and_promote(&mut state, now);
        let renewal_deadline = state.renewal_deadline;
        let lease = state.leases.get_mut(&lease_id).ok_or(LeaseError::Gone)?;
        if let Some(deadline) = renewal_deadline {
            if now >= deadline {
                return Err(LeaseError::RenewalClosed);
            }
        }

        let mut next_expiry = now + self.lease_ttl;
        if let Some(deadline) = renewal_deadline {
            next_expiry = next_expiry.min(deadline);
        }
        lease.grant.expires_at = lease.grant.expires_at.max(next_expiry);
        lease.grant.renewal_deadline = renewal_deadline;
        Ok(lease.grant.clone())
    }

    pub fn release(&self, lease_id: Uuid) -> Result<(), LeaseError> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("lease state lock poisoned");
        self.expire_and_promote(&mut state, now);
        if state.leases.remove(&lease_id).is_none() {
            return Err(LeaseError::Gone);
        }
        self.promote_queue(&mut state, now);
        Ok(())
    }

    pub fn cancel(&self, request_id: Uuid) -> Result<(), LeaseError> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("lease state lock poisoned");
        self.expire_and_promote(&mut state, now);
        let request = state
            .requests
            .get_mut(&request_id)
            .ok_or(LeaseError::RequestNotFound)?;
        if matches!(request.status, StoredRequestStatus::Pending) {
            request.status = StoredRequestStatus::Gone;
            state.queue.retain(|queued_id| *queued_id != request_id);
        }
        self.promote_queue(&mut state, now);
        Ok(())
    }

    pub fn with_lease<T>(
        &self,
        lease_id: Uuid,
        required_mode: LeaseMode,
        operation: impl FnOnce() -> T,
    ) -> Result<T, LeaseError> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("lease state lock poisoned");
        self.expire_and_promote(&mut state, now);
        let lease = state.leases.get(&lease_id).ok_or(LeaseError::Gone)?;
        if required_mode == LeaseMode::Exclusive
            && lease.grant.mode != LeaseMode::Exclusive
        {
            return Err(LeaseError::Gone);
        }
        let result = operation();
        self.expire_and_promote(&mut state, Instant::now());
        Ok(result)
    }
    pub fn validate(
        &self,
        lease_id: Uuid,
        required_mode: LeaseMode,
    ) -> Result<(), LeaseError> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("lease state lock poisoned");
        self.expire_and_promote(&mut state, now);
        let lease = state.leases.get(&lease_id).ok_or(LeaseError::Gone)?;
        if required_mode == LeaseMode::Exclusive
            && lease.grant.mode != LeaseMode::Exclusive
        {
            return Err(LeaseError::Gone);
        }
        Ok(())
    }

    pub fn active_lease_ids(&self) -> Vec<Uuid> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("lease state lock poisoned");
        self.expire_and_promote(&mut state, now);
        state.leases.keys().copied().collect()
    }

    fn can_grant(&self, state: &LeaseState, mode: LeaseMode) -> bool {
        match mode {
            LeaseMode::Shared => !state
                .leases
                .values()
                .any(|lease| lease.grant.mode == LeaseMode::Exclusive),
            LeaseMode::Exclusive => state.leases.is_empty(),
        }
    }

    fn grant_request(
        &self,
        state: &mut LeaseState,
        request_id: Uuid,
        now: Instant,
    ) {
        let Some(request) = state.requests.get_mut(&request_id) else {
            return;
        };
        let lease_id = Uuid::new_v4();
        let mode = request.mode;
        let grant = LeaseGrant {
            id: lease_id,
            mode,
            expires_at: now + self.lease_ttl,
            renewal_deadline: state.renewal_deadline,
        };
        request.status = StoredRequestStatus::Granted(lease_id);
        state.leases.insert(lease_id, LeaseRecord { grant });
    }

    fn promote_queue(&self, state: &mut LeaseState, now: Instant) {
        loop {
            let Some(request_id) = state.queue.front().copied() else {
                state.renewal_deadline = None;
                return;
            };
            let Some(mode) = state.requests.get(&request_id).map(|r| r.mode)
            else {
                state.queue.pop_front();
                continue;
            };
            if !matches!(
                state.requests.get(&request_id).map(|r| &r.status),
                Some(StoredRequestStatus::Pending)
            ) {
                state.queue.pop_front();
                continue;
            }
            if !self.can_grant(state, mode) {
                return;
            }
            if mode == LeaseMode::Exclusive {
                if !state.leases.is_empty() {
                    return;
                }
                state.renewal_deadline = None;
            }
            state.queue.pop_front();
            self.grant_request(state, request_id, now);
            if mode == LeaseMode::Exclusive {
                return;
            }
        }
    }

    fn expire_and_promote(&self, state: &mut LeaseState, now: Instant) {
        state.leases.retain(|_, lease| lease.grant.expires_at > now);
        for request in state.requests.values_mut() {
            if request.created_at + self.request_ttl <= now
                && matches!(request.status, StoredRequestStatus::Pending)
            {
                request.status = StoredRequestStatus::Gone;
            }
        }
        state.requests_by_key.retain(|_, request_id| {
            state.requests.get(request_id).is_some_and(|request| {
                request.created_at + self.request_ttl > now
            })
        });
        state.requests.retain(|_, request| {
            request.created_at + self.request_ttl.saturating_mul(2) > now
        });
        state.queue.retain(|request_id| {
            matches!(
                state
                    .requests
                    .get(request_id)
                    .map(|request| &request.status),
                Some(StoredRequestStatus::Pending)
            )
        });
        self.promote_queue(state, now);
    }

    fn request_result(state: &LeaseState, request_id: Uuid) -> AcquireResult {
        match state
            .requests
            .get(&request_id)
            .map(|request| &request.status)
        {
            Some(StoredRequestStatus::Granted(lease_id)) => state
                .leases
                .get(lease_id)
                .map(|lease| AcquireResult::Granted(lease.grant.clone()))
                .unwrap_or(AcquireResult::Pending(request_id)),
            _ => AcquireResult::Pending(request_id),
        }
    }
}

pub fn unix_milliseconds(instant: Instant) -> u128 {
    let remaining = instant.saturating_duration_since(Instant::now());
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_add(remaining)
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::{AcquireResult, LeaseManager, LeaseMode, RequestStatus};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn exclusive_requests_queue_later_shared_leases() {
        let manager = LeaseManager::new(
            Duration::from_secs(30),
            Duration::from_secs(60),
            Duration::from_secs(120),
        );
        let shared = match manager
            .acquire(LeaseMode::Shared, "first-reader".to_owned())
        {
            AcquireResult::Granted(grant) => grant,
            AcquireResult::Pending(_) => panic!("first shared lease queued"),
        };
        let exclusive_request = match manager
            .acquire(LeaseMode::Exclusive, "exclusive".to_owned())
        {
            AcquireResult::Pending(request_id) => request_id,
            AcquireResult::Granted(_) => {
                panic!("exclusive lease bypassed reader")
            }
        };
        let later_shared_request = match manager
            .acquire(LeaseMode::Shared, "later-reader".to_owned())
        {
            AcquireResult::Pending(request_id) => request_id,
            AcquireResult::Granted(_) => {
                panic!("reader bypassed exclusive request")
            }
        };

        manager.release(shared.id).unwrap();
        let exclusive = match manager.status(exclusive_request).unwrap() {
            RequestStatus::Granted(grant) => grant,
            _ => panic!("exclusive request was not promoted"),
        };
        assert!(matches!(
            manager.status(later_shared_request).unwrap(),
            RequestStatus::Pending
        ));

        manager.release(exclusive.id).unwrap();
        assert!(matches!(
            manager.status(later_shared_request).unwrap(),
            RequestStatus::Granted(_)
        ));
    }

    #[test]
    fn expired_shared_lease_allows_waiting_exclusive_request() {
        let manager = LeaseManager::new(
            Duration::from_millis(10),
            Duration::from_millis(100),
            Duration::from_secs(1),
        );
        let shared =
            match manager.acquire(LeaseMode::Shared, "reader".to_owned()) {
                AcquireResult::Granted(grant) => grant,
                AcquireResult::Pending(_) => panic!("shared lease queued"),
            };
        let request =
            match manager.acquire(LeaseMode::Exclusive, "gc".to_owned()) {
                AcquireResult::Pending(request_id) => request_id,
                AcquireResult::Granted(_) => {
                    panic!("exclusive lease bypassed reader")
                }
            };

        thread::sleep(Duration::from_millis(20));
        assert!(matches!(
            manager.status(request).unwrap(),
            RequestStatus::Granted(_)
        ));
        assert!(manager.renew(shared.id).is_err());
    }
}
