//! Conservative local lease validity uses request start time, never wall clocks.
use super::error::Error;
use rdedup_protocol::lease::{LeaseGrant, LeaseId, LeaseMode, RenewalWindow};
use rdedup_protocol::problem::ProblemKind;
use std::time::{Duration, Instant};

pub(super) struct Timing {
    id: LeaseId,
    mode: LeaseMode,
    expires: Instant,
    cutoff: Option<Instant>,
}
impl Timing {
    pub(super) fn new(
        grant: &LeaseGrant,
        started: Instant,
    ) -> Result<Self, Error> {
        let expires = started
            .checked_add(grant.remaining)
            .ok_or(Error::Protocol)?;
        let mut timing = Self {
            id: grant.id,
            mode: grant.mode,
            expires,
            cutoff: None,
        };
        timing.update(grant, started)?;
        Ok(timing)
    }
    pub(super) fn update(
        &mut self,
        grant: &LeaseGrant,
        started: Instant,
    ) -> Result<(), Error> {
        if grant.id != self.id || grant.mode != self.mode {
            return Err(Error::Protocol);
        }
        let expires = started
            .checked_add(grant.remaining)
            .ok_or(Error::Protocol)?;
        if let RenewalWindow::Closing { remaining } = grant.renewal {
            let cutoff =
                started.checked_add(remaining).ok_or(Error::Protocol)?;
            self.cutoff = Some(
                self.cutoff.map_or(cutoff, |previous| previous.min(cutoff)),
            );
        }
        // A delayed reply cannot revoke validity already established by an earlier grant.
        self.expires = self.expires.max(expires);
        if let Some(cutoff) = self.cutoff {
            self.expires = self.expires.min(cutoff);
        }
        Ok(())
    }
    pub(super) fn ensure_active(&self, now: Instant) -> Result<(), Error> {
        if now >= self.expires {
            Err(Error::Problem(ProblemKind::LeaseExpired))
        } else {
            Ok(())
        }
    }
    pub(super) fn renewal_delay(&self, now: Instant) -> Duration {
        let remaining = self.expires.saturating_duration_since(now);
        (remaining / 2)
            .max(Duration::from_millis(100))
            .min(remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delayed_renewal_cannot_move_a_known_cutoff_or_change_identity() {
        let started = Instant::now();
        let mut grant = LeaseGrant {
            id: LeaseId::generate(),
            mode: LeaseMode::Shared,
            remaining: Duration::from_secs(10),
            renewal: RenewalWindow::Closing {
                remaining: Duration::from_secs(20),
            },
        };
        let mut timing = Timing::new(&grant, started).unwrap();
        grant.remaining = Duration::from_secs(50);
        grant.renewal = RenewalWindow::Closing {
            remaining: Duration::from_secs(50),
        };
        timing
            .update(&grant, started + Duration::from_secs(5))
            .unwrap();
        assert!(timing
            .ensure_active(started + Duration::from_secs(19))
            .is_ok());
        assert!(timing
            .ensure_active(started + Duration::from_secs(20))
            .is_err());
        grant.id = LeaseId::generate();
        assert!(timing.update(&grant, started).is_err());
    }
    #[test]
    fn renewal_preserves_established_validity_and_cannot_reopen_a_cutoff() {
        let started = Instant::now();
        let mut grant = LeaseGrant {
            id: LeaseId::generate(),
            mode: LeaseMode::Shared,
            remaining: Duration::from_secs(20),
            renewal: RenewalWindow::Closing {
                remaining: Duration::from_secs(30),
            },
        };
        let mut timing = Timing::new(&grant, started).unwrap();
        grant.remaining = Duration::from_secs(1);
        timing
            .update(&grant, started + Duration::from_secs(5))
            .unwrap();
        assert!(timing
            .ensure_active(started + Duration::from_secs(19))
            .is_ok());
        grant.remaining = Duration::from_secs(100);
        grant.renewal = RenewalWindow::Open;
        timing
            .update(&grant, started + Duration::from_secs(10))
            .unwrap();
        assert!(timing
            .ensure_active(started + Duration::from_secs(30))
            .is_err());
    }
}
