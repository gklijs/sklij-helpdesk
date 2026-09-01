//! The pure decision logic behind `src/bin/scheduler.rs` - the two
//! rules that mutate real domain state on a deadline
//! (`specs/skilj-helpdesk.allium`'s `rule TrialPeriodEnds`/
//! `TicketAutoCloses`), split out the same way `src/alerting.rs` splits
//! from `src/bin/alerter.rs`: the deadline check and the mocked payment
//! outcome are pure and tested here; the actual event-stream tracking
//! and command submission live in the binary.

use chrono::{DateTime, Duration, Utc};

/// `rule TrialPeriodEnds`'s own trigger condition: `Company.
/// trial_started_at + config.trial_duration <= now`. `trial_started_at`
/// itself is never a payload field this crate stores (see
/// `scheduler.rs`'s own doc comment) - it's read off `CompanySignedUp`'s
/// own event metadata, which skilj stamps itself.
pub fn trial_period_has_ended(
    signed_up_at: DateTime<Utc>,
    now: DateTime<Utc>,
    trial_duration: Duration,
) -> bool {
    signed_up_at + trial_duration <= now
}

/// `rule TicketAutoCloses`'s own trigger condition: `Ticket.resolved_at
/// + config.auto_close_after <= now`.
pub fn should_auto_close(
    resolved_at: DateTime<Utc>,
    now: DateTime<Utc>,
    auto_close_after: Duration,
) -> bool {
    resolved_at + auto_close_after <= now
}

/// The one mocked call in this whole path -
/// `specs/skilj-helpdesk.allium`'s own `PaymentGateway.charge` black
/// box (see `TrialPeriodEnds`/`CompanySubscribes` in the spec). Always
/// succeeds: this is a showcase, not a real billing integration - swap
/// this one function for a real gateway call and nothing else in
/// `scheduler.rs` needs to change.
pub fn mock_charge_succeeds() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trial_ends_at_exactly_the_configured_duration() {
        let signed_up_at = Utc::now() - Duration::days(30);
        assert!(trial_period_has_ended(
            signed_up_at,
            Utc::now(),
            Duration::days(30)
        ));
    }

    #[test]
    fn trial_has_not_ended_before_the_duration() {
        let signed_up_at = Utc::now() - Duration::days(29);
        assert!(!trial_period_has_ended(
            signed_up_at,
            Utc::now(),
            Duration::days(30)
        ));
    }

    #[test]
    fn ticket_auto_closes_after_the_configured_duration() {
        let resolved_at = Utc::now() - Duration::days(7);
        assert!(should_auto_close(resolved_at, Utc::now(), Duration::days(7)));
    }

    #[test]
    fn ticket_does_not_auto_close_before_the_duration() {
        let resolved_at = Utc::now() - Duration::days(6);
        assert!(!should_auto_close(resolved_at, Utc::now(), Duration::days(7)));
    }
}
