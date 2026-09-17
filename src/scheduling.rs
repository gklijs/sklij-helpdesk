//! The pure/env-driven bits behind `src/helpdesk.rs`'s two deadline
//! reactors (`ScheduleCompanyTrialConversion`/`ScheduleCompanyTrialExpiry`/
//! `ScheduleTicketAutoClose`) - `specs/skilj-helpdesk.allium`'s `rule
//! TrialPeriodEnds`/`TicketAutoCloses`. Used to also hold the deadline
//! *comparison* itself (`trial_period_has_ended`/`should_auto_close`)
//! back when `src/bin/scheduler.rs` polled for due entities by hand;
//! skilj 0.0.7's native per-entity deadline mechanism
//! (docs/architecture.md §46) now does that comparison internally
//! (`fire_at <= now()`), so all that's left here is *how long* each
//! deadline runs and the one mocked business call both reactors need.

use chrono::Duration;

/// How long a company's free trial runs before `ScheduleCompanyTrialConversion`/
/// `ScheduleCompanyTrialExpiry` fire - `specs/skilj-helpdesk.allium`'s own
/// `config.trial_duration = 1.month`, overridable the same way
/// `scheduler.rs`'s own `TRIAL_DURATION_DAYS` env var used to be (handy
/// for demoing a short trial live - see README.md's own "Want more
/// load?" section).
pub fn trial_duration() -> Duration {
    let days = std::env::var("TRIAL_DURATION_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    Duration::days(days)
}

/// How long a resolved ticket waits before `ScheduleTicketAutoClose`
/// fires - `specs/skilj-helpdesk.allium`'s own `config.auto_close_after`,
/// same overridable-via-env-var treatment as `trial_duration` above
/// (`AUTO_CLOSE_AFTER_DAYS`, matching `scheduler.rs`'s own former
/// default of 7).
pub fn auto_close_after() -> Duration {
    let days = std::env::var("AUTO_CLOSE_AFTER_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7);
    Duration::days(days)
}

/// The one mocked call in this whole path -
/// `specs/skilj-helpdesk.allium`'s own `PaymentGateway.charge` black
/// box (see `TrialPeriodEnds`/`CompanySubscribes` in the spec). Always
/// succeeds: this is a showcase, not a real billing integration - swap
/// this one function for a real gateway call and nothing else in
/// `ScheduleCompanyTrialConversion`/`ScheduleCompanyTrialExpiry` needs
/// to change.
pub fn mock_charge_succeeds() -> bool {
    true
}
