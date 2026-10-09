//! Monotonic lease lifetime and irreversible renewal cutoff.
use rdedup_protocol::lease::{LeaseGrant, LeaseId, LeaseMode, RenewalWindow};
use std::time::{Duration, Instant};

/// Invalid lease policy or an interval no longer admitting work.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    /// A grant must allow a positive interval of work.
    #[error("lease lifetime must be positive")]
    ZeroLifetime,
    /// Establishing a cutoff must not revoke already granted validity.
    #[error("exclusive grace must be at least the lease lifetime")]
    ShortGrace,
    /// The supplied duration cannot be represented on the monotonic clock.
    #[error("lease duration exceeds the monotonic clock range")]
    ClockRange,
    /// Granted validity has ended and cannot be renewed retroactively.
    #[error("lease has expired")]
    Expired,
    /// The fixed grace deadline prevents further renewal.
    #[error("the fixed renewal deadline has been reached")]
    RenewalClosed,
}

/// Validated server policy: renewal cannot revoke already granted time.
#[derive(Clone, Copy, Debug)]
pub struct LeasePolicy {
    lifetime: Duration,
    exclusive_grace: Duration,
}

impl LeasePolicy {
    /// Validate positive lifetime and a grace period that cannot shorten a grant.
    ///
    /// # Errors
    /// Rejects zero lifetime, grace shorter than lifetime, and clock overflow.
    pub fn new(
        lifetime: Duration,
        exclusive_grace: Duration,
        now: Instant,
    ) -> Result<Self, Error> {
        if lifetime.is_zero() {
            return Err(Error::ZeroLifetime);
        }
        if exclusive_grace < lifetime {
            return Err(Error::ShortGrace);
        }
        now.checked_add(exclusive_grace).ok_or(Error::ClockRange)?;
        Ok(Self {
            lifetime,
            exclusive_grace,
        })
    }
}

/// A granted interval with an optional, permanent upper bound on renewal.
#[derive(Debug)]
pub(crate) struct LeaseTiming {
    expires: Instant,
    renewal: Renewal,
}

#[derive(Debug)]
enum Renewal {
    Open,
    Closing { deadline: Instant },
}

impl LeaseTiming {
    pub(crate) fn grant(
        now: Instant,
        policy: LeasePolicy,
    ) -> Result<Self, Error> {
        Ok(Self {
            expires: now
                .checked_add(policy.lifetime)
                .ok_or(Error::ClockRange)?,
            renewal: Renewal::Open,
        })
    }

    pub(crate) fn close_renewal(
        &mut self,
        now: Instant,
        policy: LeasePolicy,
    ) -> Result<(), Error> {
        if matches!(self.renewal, Renewal::Open) {
            let deadline = now
                .checked_add(policy.exclusive_grace)
                .ok_or(Error::ClockRange)?;
            self.renewal = Renewal::Closing {
                deadline: deadline.max(self.expires),
            };
        }
        Ok(())
    }

    pub(crate) fn renew(
        &mut self,
        now: Instant,
        policy: LeasePolicy,
    ) -> Result<(), Error> {
        self.ensure_active(now)?;
        let requested =
            now.checked_add(policy.lifetime).ok_or(Error::ClockRange)?;
        let renewed = match self.renewal {
            Renewal::Open => requested,
            Renewal::Closing { deadline } => {
                if now >= deadline {
                    return Err(Error::RenewalClosed);
                }
                requested.min(deadline)
            }
        };
        self.expires = self.expires.max(renewed);
        Ok(())
    }

    pub(crate) fn ensure_active(&self, now: Instant) -> Result<(), Error> {
        if now >= self.expires {
            return Err(Error::Expired);
        }
        Ok(())
    }

    pub(crate) fn expires(&self) -> Instant {
        self.expires
    }

    pub(crate) fn describe(
        &self,
        id: LeaseId,
        mode: LeaseMode,
        now: Instant,
    ) -> Result<LeaseGrant, Error> {
        self.ensure_active(now)?;
        Ok(LeaseGrant {
            id,
            mode,
            remaining: self.expires.saturating_duration_since(now),
            renewal: match self.renewal {
                Renewal::Open => RenewalWindow::Open,
                Renewal::Closing { deadline } => RenewalWindow::Closing {
                    remaining: deadline.saturating_duration_since(now),
                },
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(now: Instant) -> LeasePolicy {
        LeasePolicy::new(Duration::from_secs(30), Duration::from_secs(90), now)
            .unwrap()
    }

    #[test]
    fn invalid_configuration_is_rejected_before_granting_leases() {
        let now = Instant::now();
        assert_eq!(
            LeasePolicy::new(Duration::ZERO, Duration::from_secs(90), now)
                .unwrap_err(),
            Error::ZeroLifetime
        );
        assert_eq!(
            LeasePolicy::new(
                Duration::from_secs(30),
                Duration::from_secs(29),
                now
            )
            .unwrap_err(),
            Error::ShortGrace
        );
    }

    #[test]
    fn renewal_preserves_existing_time_and_cannot_resurrect_expiration() {
        let now = Instant::now();
        let policy = policy(now);
        let mut lease = LeaseTiming::grant(now, policy).unwrap();
        lease.renew(now, policy).unwrap();
        assert_eq!(lease.expires(), now + Duration::from_secs(30));
        lease.renew(now + Duration::from_secs(20), policy).unwrap();
        assert_eq!(lease.expires(), now + Duration::from_secs(50));
        assert_eq!(
            lease.renew(now + Duration::from_secs(50), policy),
            Err(Error::Expired)
        );
    }

    #[test]
    fn later_exclusive_requests_cannot_move_an_advertised_cutoff() {
        let now = Instant::now();
        let policy = policy(now);
        let mut lease = LeaseTiming::grant(now, policy).unwrap();
        lease.close_renewal(now, policy).unwrap();
        for seconds in [20, 40, 60, 80] {
            let later = now + Duration::from_secs(seconds);
            lease.close_renewal(later, policy).unwrap();
            lease.renew(later, policy).unwrap();
        }
        assert_eq!(lease.expires(), now + Duration::from_secs(90));
        assert_eq!(
            lease.ensure_active(now + Duration::from_secs(90)),
            Err(Error::Expired)
        );
    }
}
