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
pub const APPLICATION_VERSION: u64 = 2;

/// Parses `ENCRYPTION_MASTER_KEY`'s value - 64 hex characters, 32 bytes
/// (`openssl rand -hex 32`) - for `SkiljBuilder::encryption_master_key`.
/// `helpdesk`'s customer contact fields are encrypted under per-customer
/// keys that this one wraps (see `helpdesk::CUSTOMER_SUBJECT`), so it must
/// stay the same across restarts: lose it and every customer's stored
/// contact details become unreadable for good.
pub fn parse_encryption_master_key(hex: &str) -> Result<skilj::EncryptionMasterKey, String> {
    let hex = hex.trim();
    if hex.len() != 64 || !hex.is_ascii() {
        return Err(format!(
            "expected 64 hex characters (32 bytes), got {} characters",
            hex.len()
        ));
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
            .map_err(|_| format!("{:?} is not a hex byte", &hex[2 * i..2 * i + 2]))?;
    }
    Ok(skilj::EncryptionMasterKey::from_bytes(bytes))
}

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

#[cfg(test)]
mod tests {
    use super::parse_encryption_master_key;

    #[test]
    fn a_master_key_is_exactly_64_hex_characters() {
        assert!(parse_encryption_master_key(&"ab".repeat(32)).is_ok());
        assert!(parse_encryption_master_key(&format!(" {}\n", "0F".repeat(32))).is_ok());
        assert!(parse_encryption_master_key(&"ab".repeat(31)).is_err());
        assert!(parse_encryption_master_key(&"zz".repeat(32)).is_err());
        assert!(parse_encryption_master_key(&"é".repeat(32)).is_err());
    }
}
