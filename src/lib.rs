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
pub mod scheduling;
pub mod telemetry;

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
/// explicitly. No `CancelDeadline` registrations - `helpdesk.rs`'s own
/// deadline reactors deliberately have none (see their own doc
/// comments for why).
pub fn register(builder: skilj::SkiljBuilder) -> skilj::SkiljBuilder {
    builder
        .auto_register()
        .cross_context_route::<marketing::HelpdeskExpiryToTrialLapse>()
        .cross_context_route::<marketing::HelpdeskActivationToTrialConversion>()
        .cross_context_route::<marketing::ActivityEngagementDeclineToMarketingFlag>()
        .schedule_deadline::<helpdesk::ScheduleCompanyTrialConversion>()
        .schedule_deadline::<helpdesk::ScheduleCompanyTrialExpiry>()
        .schedule_deadline::<helpdesk::ScheduleTicketAutoClose>()
}
