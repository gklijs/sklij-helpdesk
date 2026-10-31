//! skilj-helpdesk: a showcase SaaS helpdesk built on skilj. See
//! `specs/skilj-helpdesk.allium` for the domain spec, and
//! `helpdesk/mod.rs`'s own module doc comment for what this pass of the
//! implementation covers versus defers.
//!
//! `activity`/`marketing` are two more bounded contexts alongside
//! `helpdesk` - see `specs/activity.allium`/`specs/marketing.allium` for
//! their own domain specs, and `marketing.rs`'s own doc comment for why
//! `register()` below needs more than the one `auto_register()` call
//! `helpdesk` alone used to need. `knowledge_base` is a fourth, fed the
//! same way from `helpdesk.TicketResolved` (see `specs/knowledge_base.allium`).

pub mod activity;
pub mod activity_scheduling;
pub mod alerting;
pub mod demo_seed;
pub mod email_channel;
pub mod helpdesk;
pub mod knowledge_base;
pub mod marketing;
pub mod parked_deliveries;
pub mod projection_lag;
pub mod routing;
pub mod routing_guard;
pub mod scheduling;
pub mod sla;
pub mod telemetry;
pub mod tenant_access;

/// The local-JWKS-shortcut test key the sibling binaries (provisioner,
/// lifecycle-replicator, alerter) sign their superadmin JWTs with, as a
/// ready-to-serve JWKS document.
///
/// `src/bin/server.rs` generates its own fresh keypair at startup for the
/// JWTs it prints, but the siblings can't use that key - it never leaves
/// the server process. So the server's own verified JWKS must trust this
/// shared test key too; `src/bin/server.rs` prints this document at
/// startup in local-shortcut mode for exactly that wiring. Same key
/// material as `tests/support/mod.rs`'s `serve_jwks` (test-only copy of
/// the same shortcut), never a real secret.
pub const LOCAL_JWKS_TEST_JWKS: &str = "{\"keys\":[{\"kty\":\"RSA\",\"use\":\"sig\",\"alg\":\"RS256\",\"kid\":\"test-key-1\",\"n\":\"zx1RbFB4ll0mxv0wooNXE0BzU-hZ6GKGbTBI7w4kAK7Di_3RaDPaZhkTPQnxLUBjbpuE7Ipj7YCXnGsH3T51tLiQkZSBfcBpmHYdpmDQSsPI6scKeuuBnebqh-S-gOKTfG3xju_tp4g-qfLdHDXsmDndD7rGwBA3KcwlGZq-ikMjOYvP8JOaDvfIJ7afmaTI1GWmwA-kfA8oTCGZleuOY6OTTyRu87ks_t3ZipDWNPxl8_GML2drMKppjgDUWRx3zdb4D9snCtNlmeMoocehIrkAg7HnXQcPsYrfSkr0quhsJsJ1dalaTFTZUhhjnlV-7-yUN8_wTW0POeaerPxY4Q\",\"e\":\"AQAB\"}]}";

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
///
/// 8: skilj 0.2.0 - the staff-only projections declare
/// `REQUIRES_CAPABILITY` and the internal-note private fields are
/// capability-kind, where 7 stored `TEAM_ONLY` and team-kind ones.
pub const APPLICATION_VERSION: u64 = 8;

/// The encryption setup `src/bin/server.rs` reads from its environment:
/// where the master key lives and when it rotates.
///
/// `helpdesk`'s customer fields are encrypted under per-customer data
/// keys, which skilj wraps with a master key held by a key provider (skilj
/// 0.2.0, docs/architecture.md §230 in the skilj repo). This deployment
/// uses `KeyFileProvider`: a JSON key file skilj creates on first start
/// and appends a version to on each rotation. Every instance must read the
/// same file - lose it and every customer's stored details become
/// unreadable for good, which is why the path is required rather than
/// defaulted somewhere an operator might not back up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptionConfig {
    /// `ENCRYPTION_KEY_FILE`.
    pub key_file: std::path::PathBuf,
    /// `ENCRYPTION_KEY_ROTATE_EVERY_DAYS`: rotate the master key on this
    /// schedule. Unset rotates only on request (`rotateMasterKey`).
    pub rotate_every_days: Option<u64>,
    /// `ENCRYPTION_KEY_RETIRE_AFTER_DAYS`: how long after a rotation's
    /// rewrap the old version is retired - after which a database backup
    /// from before the rotation no longer opens. skilj's 30 by default;
    /// should cover the backup retention a restore may need.
    pub retire_after_days: Option<u64>,
}

impl EncryptionConfig {
    /// Reads the three variables through `var` (`std::env::var` in the
    /// server; a map in tests).
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let key_file = var("ENCRYPTION_KEY_FILE")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                "ENCRYPTION_KEY_FILE must be set: the path of the master key file skilj \
                 creates on first start - shared by every instance, kept and backed up \
                 (losing it makes every customer's data unreadable)"
                    .to_string()
            })?;
        let days = |name: &str| -> Result<Option<u64>, String> {
            match var(name).map(|v| v.trim().to_string()) {
                None => Ok(None),
                Some(v) if v.is_empty() => Ok(None),
                Some(v) => match v.parse::<u64>() {
                    Ok(0) | Err(_) => Err(format!(
                        "{name}: expected a whole number of days above 0, got {v:?}"
                    )),
                    Ok(n) => Ok(Some(n)),
                },
            }
        };
        Ok(Self {
            key_file: key_file.into(),
            rotate_every_days: days("ENCRYPTION_KEY_ROTATE_EVERY_DAYS")?,
            retire_after_days: days("ENCRYPTION_KEY_RETIRE_AFTER_DAYS")?,
        })
    }

    /// The `SkiljBuilder::master_key_rotation` this configures.
    pub fn rotation(&self) -> skilj::MasterKeyRotation {
        const DAY: std::time::Duration = std::time::Duration::from_secs(24 * 3600);
        let mut rotation = skilj::MasterKeyRotation::new();
        if let Some(days) = self.rotate_every_days {
            rotation = rotation.every(DAY * days as u32);
        }
        if let Some(days) = self.retire_after_days {
            rotation = rotation.retire_after(DAY * days as u32);
        }
        rotation
    }
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
/// own doc comment on why for the former; `helpdesk/`'s own
/// `ScheduleCompanyTrialConversion` doc comment for the latter), so each
/// of `specs/marketing.allium`'s three `CrossContextRoute`s,
/// `specs/knowledge_base.allium`'s one, and
/// `helpdesk/`'s five `ScheduleDeadline`s is wired in here, by name,
/// explicitly, as are its five `CancelDeadline`s
/// (`CancelTicketAutoCloseOnReopen` and the four SLA cancels - see
/// `helpdesk/sla_policy.rs`). The trial deadlines have no cancel on
/// purpose - see `ScheduleCompanyTrialConversion`'s own doc comment.
pub fn register(builder: skilj::SkiljBuilder) -> skilj::SkiljBuilder {
    builder
        .auto_register()
        .cross_context_route::<marketing::HelpdeskExpiryToTrialLapse>()
        .cross_context_route::<marketing::HelpdeskActivationToTrialConversion>()
        .cross_context_route::<marketing::ActivityEngagementDeclineToMarketingFlag>()
        .cross_context_route::<knowledge_base::HelpdeskResolutionToArticleSuggestion>()
        .schedule_deadline::<helpdesk::ScheduleCompanyTrialConversion>()
        .schedule_deadline::<helpdesk::ScheduleCompanyTrialExpiry>()
        .schedule_deadline::<helpdesk::ScheduleTicketAutoClose>()
        .cancel_deadline::<helpdesk::CancelTicketAutoCloseOnReopen>()
        .schedule_deadline::<helpdesk::ScheduleFirstResponseSla>()
        .schedule_deadline::<helpdesk::ScheduleResolutionSla>()
        .cancel_deadline::<helpdesk::CancelFirstResponseSlaOnStaffReply>()
        .cancel_deadline::<helpdesk::CancelFirstResponseSlaOnMacroReply>()
        .cancel_deadline::<helpdesk::CancelFirstResponseSlaOnResolve>()
        .cancel_deadline::<helpdesk::CancelResolutionSlaOnResolve>()
}

#[cfg(test)]
mod tests {
    use super::EncryptionConfig;
    use std::collections::HashMap;

    fn config(vars: &[(&str, &str)]) -> Result<EncryptionConfig, String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        EncryptionConfig::from_vars(|name| vars.get(name).cloned())
    }

    #[test]
    fn the_key_file_is_required() {
        assert!(config(&[]).is_err());
        assert!(config(&[("ENCRYPTION_KEY_FILE", "  ")]).is_err());
        let ok = config(&[("ENCRYPTION_KEY_FILE", "/var/lib/helpdesk/keys.json")]).unwrap();
        assert_eq!(
            ok.key_file,
            std::path::PathBuf::from("/var/lib/helpdesk/keys.json")
        );
        assert_eq!((ok.rotate_every_days, ok.retire_after_days), (None, None));
    }

    #[test]
    fn rotation_days_are_positive_whole_numbers() {
        let file = ("ENCRYPTION_KEY_FILE", "keys.json");
        let ok = config(&[
            file,
            ("ENCRYPTION_KEY_ROTATE_EVERY_DAYS", "30"),
            ("ENCRYPTION_KEY_RETIRE_AFTER_DAYS", "45"),
        ])
        .unwrap();
        assert_eq!(
            (ok.rotate_every_days, ok.retire_after_days),
            (Some(30), Some(45))
        );
        for bad in ["0", "-1", "1.5", "monthly"] {
            assert!(
                config(&[file, ("ENCRYPTION_KEY_ROTATE_EVERY_DAYS", bad)]).is_err(),
                "{bad}"
            );
            assert!(
                config(&[file, ("ENCRYPTION_KEY_RETIRE_AFTER_DAYS", bad)]).is_err(),
                "{bad}"
            );
        }
    }
}
