//! skilj-helpdesk: a showcase SaaS helpdesk built on skilj. See
//! `specs/skilj-helpdesk.allium` for the domain spec, and
//! `helpdesk.rs`'s own module doc comment for what this pass of the
//! implementation covers versus defers.
//!
//! `activity`/`marketing` are two more bounded contexts alongside
//! `helpdesk` - see `specs/activity.allium`/`specs/marketing.allium` for
//! their own domain specs, and `marketing.rs`'s own doc comment for why
//! `register()` below needs more than the one `auto_register()` call
//! `helpdesk` alone used to need.

pub mod activity;
pub mod activity_scheduling;
pub mod alerting;
pub mod demo_seed;
pub mod helpdesk;
pub mod marketing;
pub mod routing;
pub mod routing_guard;
pub mod scheduling;
pub mod telemetry;
pub mod tenant_access;

/// This build's `SkiljBuilder::application_version` (docs/architecture.md
/// §104 in the skilj repo), set by `src/bin/server.rs`. Five binaries and
/// three bounded contexts share one database, so during a rolling deploy
/// an old and a new `server` start against the same registration tables:
/// with this set, a lower-versioned startup leaves the newer one's
/// `EventType`/`CommandType`/`Projection` rows exactly as they are
/// (reported in `ReconciliationReport::kept_newer`) instead of reverting
/// them - a projection's consumed event types flipping back and forth is
/// a full rebuild each time. `tests/application_version.rs` proves it.
///
/// Must only ever increase: bump it in any change that alters a
/// registered shape (a new consumed event type, a flag, a new field).
pub const APPLICATION_VERSION: u64 = 1;

/// Registers every bounded context this crate defines. `auto_register()`
/// alone still covers every `EventType`/`CommandType` in `helpdesk`,
/// `activity` and `marketing` - each `#[auto_register]`-tagged type
/// finds its own bounded context via its own module's `BOUNDED_CONTEXT`
/// const, exactly as it did when this crate only had one. The
/// `.cross_context_route::<R>()`/`.schedule_deadline::<S>()` calls are
/// what `auto_register()` can't do on its own: unlike `EventType`/
/// `CommandType`/`Projection`, neither `CrossContextRoute` nor
/// `ScheduleDeadline` has `#[auto_register]` support (see `marketing.rs`'s
/// own doc comment on why for the former; `helpdesk.rs`'s own
/// `ScheduleCompanyTrialConversion` doc comment for the latter), so each
/// of `specs/marketing.allium`'s three `CrossContextRoute`s and
/// `helpdesk.rs`'s three `ScheduleDeadline`s is wired in here, by name,
/// explicitly, as is the one `CancelDeadline` (`helpdesk.rs`'s
/// `CancelTicketAutoCloseOnReopen`). The trial deadlines have no cancel
/// on purpose - see `ScheduleCompanyTrialConversion`'s own doc comment.
pub fn register(builder: skilj::SkiljBuilder) -> skilj::SkiljBuilder {
    builder
        .auto_register()
        .cross_context_route::<marketing::HelpdeskExpiryToTrialLapse>()
        .cross_context_route::<marketing::HelpdeskActivationToTrialConversion>()
        .cross_context_route::<marketing::ActivityEngagementDeclineToMarketingFlag>()
        .schedule_deadline::<helpdesk::ScheduleCompanyTrialConversion>()
        .schedule_deadline::<helpdesk::ScheduleCompanyTrialExpiry>()
        .schedule_deadline::<helpdesk::ScheduleTicketAutoClose>()
        .cancel_deadline::<helpdesk::CancelTicketAutoCloseOnReopen>()
}
