//! skilj-helpdesk: a showcase SaaS helpdesk built on skilj. See
//! `specs/skilj-helpdesk.allium` for the domain spec, and
//! `helpdesk.rs`'s own module doc comment for what this pass of the
//! implementation covers versus defers.

pub mod alerting;
pub mod helpdesk;
pub mod scheduling;

/// Registers the helpdesk bounded context - see `skilj-demo`'s own
/// `register()` (it has the same one-liner shape) for why this needs no
/// per-module wiring beyond one `auto_register()` call: every
/// `#[auto_register]`-tagged type in this crate finds its own bounded
/// context via `helpdesk::BOUNDED_CONTEXT`.
pub fn register(builder: skilj::SkiljBuilder) -> skilj::SkiljBuilder {
    builder.auto_register()
}
