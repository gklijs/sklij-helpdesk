//! The pure decision logic behind `src/bin/engagement-watcher.rs` -
//! `specs/activity.allium`'s own resolved "gone quiet" rule
//! (`config.quiet_after`), split out the same way `src/scheduling.rs`
//! splits from `src/bin/scheduler.rs`: the deadline check is pure and
//! tested here; the actual event-stream tracking and command submission
//! live in the binary.

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
