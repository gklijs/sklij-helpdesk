//! The pure decision logic behind `src/bin/engagement-watcher.rs` -
//! `specs/activity.allium`'s own resolved "gone quiet" rule
//! (`config.quiet_after`), split out the same way `src/alerting.rs`
//! splits from `src/bin/alerter.rs`: the deadline check is pure and
//! tested here; the actual event-stream tracking and command submission
//! live in the binary. Unlike `src/helpdesk.rs`'s own trial/auto-close
//! deadlines (`scheduling.rs`, ported onto skilj's native
//! `ScheduleDeadline`, see `ScheduleCompanyTrialConversion`'s own doc
//! comment), "gone quiet" doesn't fit that one-shot-per-triggering-event
//! shape: it's a rolling window that resets on every new activity, not
//! a deadline scheduled once from a single source event, so it stays a
//! hand-rolled polling binary.

use chrono::{DateTime, Duration, Utc};

/// A company counts as "gone quiet" once this much time has passed
/// since its own last customer-kind `DailyActivityRecorded` - see
/// `specs/activity.allium`'s own resolved note on this rule (and why
/// staff activity/helpdesk's own ticket activity don't count).
pub fn is_quiet(last_customer_activity: DateTime<Utc>, now: DateTime<Utc>, quiet_after: Duration) -> bool {
    last_customer_activity + quiet_after <= now
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_at_exactly_the_configured_duration() {
        let last_activity = Utc::now() - Duration::days(14);
        assert!(is_quiet(last_activity, Utc::now(), Duration::days(14)));
    }

    #[test]
    fn not_yet_quiet_before_the_duration() {
        let last_activity = Utc::now() - Duration::days(13);
        assert!(!is_quiet(last_activity, Utc::now(), Duration::days(14)));
    }
}
