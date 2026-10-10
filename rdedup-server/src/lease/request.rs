//! Bounded acquisition bookkeeping and retirement of abandoned requests.
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

/// Invalid acquisition bookkeeping settings.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Queued requests must have time to be polled.
    #[error("request idle timeout must be positive")]
    ZeroIdleTimeout,
    /// Terminal requests must remain recognizable across short lost responses.
    #[error("request retention must be at least its idle timeout")]
    ShortRetention,
    /// Settings exceed the monotonic clock's representable range.
    #[error("request duration exceeds the monotonic clock range")]
    ClockRange,
}

/// Maximum resident acquisition records, including retained terminal responses.
#[derive(Clone, Copy, Debug)]
pub struct RequestCapacity(NonZeroUsize);

impl RequestCapacity {
    /// Configure a positive limit on queued, active and retained request records.
    pub fn new(count: NonZeroUsize) -> Self {
        Self(count)
    }
    /// Maximum number of resident records admitted by this budget.
    pub fn as_count(self) -> usize {
        self.0.get()
    }
}

/// Bound memory and abandoned queue occupancy independently of lease renewal.
#[derive(Clone, Copy, Debug)]
pub struct RequestPolicy {
    capacity: RequestCapacity,
    idle_timeout: Duration,
    retention: Duration,
}

impl RequestPolicy {
    /// Validate request admission and terminal response retention settings.
    ///
    /// # Errors
    /// Rejects zero idle timeout, shorter retention, and clock overflow.
    pub fn new(
        capacity: RequestCapacity,
        idle_timeout: Duration,
        retention: Duration,
        now: Instant,
    ) -> Result<Self, Error> {
        if idle_timeout.is_zero() {
            return Err(Error::ZeroIdleTimeout);
        }
        if retention < idle_timeout {
            return Err(Error::ShortRetention);
        }
        now.checked_add(retention).ok_or(Error::ClockRange)?;
        Ok(Self {
            capacity,
            idle_timeout,
            retention,
        })
    }

    pub(super) fn has_capacity(self, records: usize) -> bool {
        records < self.capacity.as_count()
    }

    pub(super) fn is_abandoned(
        self,
        last_contact: Instant,
        now: Instant,
    ) -> bool {
        now.saturating_duration_since(last_contact) >= self.idle_timeout
    }

    pub(super) fn can_retire(self, closed_at: Instant, now: Instant) -> bool {
        now.saturating_duration_since(closed_at) >= self.retention
    }
}

impl Default for RequestPolicy {
    fn default() -> Self {
        Self {
            capacity: RequestCapacity(
                NonZeroUsize::new(4096).expect("4096 is nonzero"),
            ),
            idle_timeout: Duration::from_secs(60),
            retention: Duration::from_secs(600),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_and_retention_are_independent_of_lease_lifetime() {
        let now = Instant::now();
        let policy = RequestPolicy::new(
            RequestCapacity::new(NonZeroUsize::new(2).unwrap()),
            Duration::from_secs(5),
            Duration::from_secs(10),
            now,
        )
        .unwrap();
        assert!(policy.has_capacity(1));
        assert!(!policy.has_capacity(2));
        assert!(!policy.is_abandoned(now, now + Duration::from_secs(4)));
        assert!(policy.is_abandoned(now, now + Duration::from_secs(5)));
        assert!(!policy.can_retire(now, now + Duration::from_secs(9)));
        assert!(policy.can_retire(now, now + Duration::from_secs(10)));
    }
}
