//! Which bounded context a command belongs in, as pure logic with no I/O.
//!
//! This module answers exactly one question - given a command type name, a
//! cutover mode, and whatever is known about a company's tenant, which
//! bounded context should execute it - and answers it with no database, no
//! HTTP, and no clock. Everything dynamic (reading `TenantDirectory`,
//! minting credentials) lives in the callers; this is the part that is
//! worth unit-testing exhaustively, because it is the part that decides
//! where a company's data lands.
//!
//! ## The split it encodes
//!
//! Ticket traffic is per-company and belongs in that company's tenant.
//! Company *lifecycle* traffic is not per-company at all - it is the
//! authority every tenant's mirrored copy is derived from (see
//! `helpdesk.rs`'s `CompanyLifecycleMirrored`) - so it stays in the shared
//! context, and routing it anywhere else would let a tenant move its own
//! company between states.
//!
//! ## Why the fallback is a fallback and not an error
//!
//! A company with no recorded tenant (never signed up, or the provisioner
//! hasn't reacted yet) still has to be able to file tickets. Routing it to
//! the shared context is exactly today's behaviour, so the fallback can
//! only ever preserve working behaviour - it can never introduce a new
//! failure. That asymmetry is the whole safety argument for flipping the
//! cutover on incrementally: at every point in the rollout, every company
//! can file tickets somewhere.

use crate::helpdesk::BOUNDED_CONTEXT;

/// Where a command should execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A company's own tenant bounded context.
    Tenant(String),
    /// The shared `helpdesk` context, by name.
    Shared,
}

impl Target {
    /// The bounded-context name to dispatch against - what a `CommandToken`
    /// is minted for, or what a GraphQL `boundedContext:` argument names.
    pub fn bounded_context(&self) -> &str {
        match self {
            Target::Tenant(name) => name,
            Target::Shared => BOUNDED_CONTEXT,
        }
    }
}

/// What kind of traffic a command type carries, which is what decides
/// whether it can follow a company into its tenant at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandClass {
    /// Per-company Ticket traffic - the whole point of the cutover.
    Ticket,
    /// Company lifecycle, authoritative in the shared context only.
    CompanyLifecycle,
    /// Per-tenant infrastructure (the replicator's own mirror command,
    /// the provisioner's own recording command). Like `Ticket` these are
    /// dispatched *at* a tenant rather than *into* the shared context, so
    /// they must never be resolved by looking a company up in
    /// `TenantDirectory` - that lookup is the shared context's own
    /// projection, and asking it where to send a command that maintains it
    /// would be circular. They are pinned by their caller instead.
    TenantInfrastructure,
}

/// Every `rest_trigger_allowed`/`submitCommand`-reachable command type in
/// `helpdesk.rs`, classified. An exhaustive `match` rather than a lookup
/// table so that adding a new command to `helpdesk.rs` without deciding
/// its class is a compile error here - the failure mode this exists to
/// prevent is a new command silently defaulting to "goes wherever the
/// company goes" when nobody ever thought about it.
///
/// Returns `Err` for a name that isn't in the table rather than guessing:
/// a caller reaching this with an unrecognised command type has a bug,
/// and inventing a bounded context for it would turn that bug into data in
/// the wrong place.
pub fn classify(command_type_name: &str) -> Result<CommandClass, UnclassifiedCommand> {
    let class = match command_type_name {
        // Per-company Ticket traffic.
        "CreateTicket"
        | "AssignTicket"
        | "ResolveTicket"
        | "ReopenTicket"
        | "RequestInfoFromCustomer"
        | "CustomerRespondsToTicket"
        | "CloseTicket"
        | "EscalateTicket"
        | "MergeTickets"
        | "RateTicket"
        | "AddInternalNote" => CommandClass::Ticket,
        // Lifecycle: authoritative in the shared context, never routed.
        "SignUpCompany" | "ConvertCompanyTrial" | "ExpireCompanyTrial" | "ReactivateCompany" => {
            CommandClass::CompanyLifecycle
        }
        // Per-tenant infrastructure, pinned by its caller.
        "RecordCompanyTenant" | "RecordTenantLifecycle" => CommandClass::TenantInfrastructure,
        other => return Err(UnclassifiedCommand::Unclassified(other.to_string())),
    };
    Ok(class)
}

/// Whether Ticket traffic follows companies into their tenants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingMode {
    /// Every company, tenant or not, is served from the shared context -
    /// the behaviour before the cutover, and the safe default.
    Shared,
    /// Ticket traffic follows a company into its tenant when one exists,
    /// and falls back to the shared context when it doesn't.
    Tenant,
}

impl RoutingMode {
    /// `TICKET_ROUTING=tenant` opts in; anything else (including unset)
    /// stays on `Shared`. Deliberately opt-*in* rather than opt-out: the
    /// cutover moves where a company's existing history lives, so the
    /// safe state has to be the one you get by doing nothing.
    pub fn from_env_value(value: Option<&str>) -> Self {
        match value {
            Some(v) if v.eq_ignore_ascii_case("tenant") => RoutingMode::Tenant,
            _ => RoutingMode::Shared,
        }
    }
}

/// Why a command ended up where it did - carried alongside the decision
/// so a rejection or a fallback can be logged with its reason rather than
/// as an unexplained context name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteReason {
    /// Cutover is off; everything stays shared.
    CutoverDisabled,
    /// Not per-company traffic, so it stays with the authority.
    NotPerCompany,
    /// Pinned to a tenant by the caller (the replicator/provisioner).
    PinnedToTenant,
    /// Followed the company into its own tenant.
    FollowedCompanyToTenant,
    /// The company has no tenant yet, so it stayed shared.
    NoTenantRecorded,
}

/// The decision, plus enough context to explain it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDecision {
    pub target: Target,
    pub reason: RouteReason,
}

/// Decide where `command_type_name` should execute.
///
/// `tenant_name` is what the caller knows about the target company right
/// now - `Some` only if `TenantDirectory` has an entry for it. It is
/// deliberately a plain argument rather than something looked up here, so
/// this function stays I/O-free and a caller that already knows (the
/// replicator addressing one tenant directly) doesn't have to pretend it
/// doesn't.
pub fn route(
    mode: RoutingMode,
    command_type_name: &str,
    tenant_name: Option<&str>,
) -> Result<RouteDecision, UnclassifiedCommand> {
    let class = classify(command_type_name)?;
    let decide = |target, reason| Ok(RouteDecision { target, reason });
    match class {
        // Pinned before the mode is even consulted: these are addressed
        // at a tenant by construction, so "is the cutover on" is not a
        // question about them.
        CommandClass::TenantInfrastructure => {
            // A caller that knows the tenant passes it; one that doesn't
            // is misconfigured, and saying so beats silently sending a
            // per-tenant command to the shared context where it would
            // mirror a lifecycle fact into the wrong place.
            match tenant_name {
                Some(name) => decide(
                    Target::Tenant(name.to_string()),
                    RouteReason::PinnedToTenant,
                ),
                None => Err(UnclassifiedCommand::NoTenantForPinnedCommand(
                    command_type_name.to_string(),
                )),
            }
        }
        CommandClass::CompanyLifecycle => decide(Target::Shared, RouteReason::NotPerCompany),
        CommandClass::Ticket => match (mode, tenant_name) {
            (RoutingMode::Shared, _) => decide(Target::Shared, RouteReason::CutoverDisabled),
            (RoutingMode::Tenant, None) => decide(Target::Shared, RouteReason::NoTenantRecorded),
            (RoutingMode::Tenant, Some(name)) => decide(
                Target::Tenant(name.to_string()),
                RouteReason::FollowedCompanyToTenant,
            ),
        },
    }
}

/// Derive a valid bounded-context name for a company's tenant from its
/// `company_id`.
///
/// **This exists because the obvious `format!("company-{company_id}")` is
/// wrong**, and was, in `provisioner.rs`, until this function replaced it.
/// Two independent reasons it breaks:
///
///   - **Validity.** A bounded context name must match
///     `[a-z][a-z0-9_]*` and be at most 40 characters
///     (`AddBoundedContext`'s own validation). `company_id` is a
///     free-form caller-supplied string - `SignUpCompanyPayload::company_id`
///     validates nothing - so `"acme-corp"` (a dash), `"ACME"` (uppercase)
///     or any id longer than 32 characters produces a name skilj rejects.
///     `createBoundedContextFromTemplate` then fails for that company, the
///     provisioner logs it and moves on, and the company silently keeps
///     running in the shared context: no error, no isolation.
///   - **Uniqueness.** Any scheme based only on the characters of
///     `company_id` can map two different companies onto one name.
///
/// So: normalise to the legal character set, then append a short
/// deterministic digest of the **original** id. The digest is what makes
/// the mapping total - `"acme-corp"` and `"acme corp"` normalise to the
/// same prefix and stay distinct tenants.
///
/// The digest is FNV-1a written out here rather than
/// `DefaultHasher`, whose output is explicitly not stable across Rust
/// versions: this name is written into `CompanyTenantProvisioned` and read
/// back forever after, so it has to be reproducible by any future build,
/// not just this one.
///
/// **Already-provisioned companies are unaffected.** Nothing re-derives a
/// recorded tenant's name - `TenantDirectory` and
/// `recorded_company_tenants` both read the name `RecordCompanyTenant`
/// recorded - so this only decides the name for companies provisioned from
/// now on. Existing tenants keep the names they have.
pub fn tenant_name_for(company_id: &str) -> String {
    const PREFIX: &str = "company";
    /// Room for `company` + `_` + normalised id + `_` + a 16-digit hex
    /// digest, under the 40 character cap. Every separator is counted:
    /// `company_<15 chars>_<16 hex>` is exactly 40.
    const MAX_NORMALISED: usize = 40 - PREFIX.len() - 1 - 1 - 16;

    let mut normalised = String::with_capacity(MAX_NORMALISED);
    let mut last_was_separator = false;
    for ch in company_id.chars() {
        if normalised.chars().count() >= MAX_NORMALISED {
            break;
        }
        match ch {
            'a'..='z' | '0'..='9' => {
                normalised.push(ch);
                last_was_separator = false;
            }
            'A'..='Z' => {
                normalised.push(ch.to_ascii_lowercase());
                last_was_separator = false;
            }
            _ => {
                // Everything else - dashes, dots, spaces, non-ASCII -
                // collapses to a single underscore, so `"acme corp"` and
                // `"acme-corp"` normalise identically (and stay distinct
                // only because of the digest).
                if !last_was_separator && !normalised.is_empty() {
                    normalised.push('_');
                    last_was_separator = true;
                }
            }
        }
    }
    while normalised.ends_with('_') {
        normalised.pop();
    }
    if normalised.is_empty() {
        normalised.push_str("unnamed");
    }

    format!(
        "{PREFIX}_{normalised}_{:016x}",
        fnv1a64(company_id.as_bytes())
    )
}

/// FNV-1a, 64-bit. Small, dependency-free, and - the point here -
/// permanently fixed by this implementation rather than by whatever a
/// standard library happens to guarantee this year.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// A routing input this module refuses to guess at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnclassifiedCommand {
    /// A command type nobody has decided a class for.
    Unclassified(String),
    /// A per-tenant infrastructure command arrived with no tenant to pin
    /// it to.
    NoTenantForPinnedCommand(String),
}

impl std::fmt::Display for UnclassifiedCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnclassifiedCommand::Unclassified(name) => {
                write!(f, "command type {name:?} is not classified for routing")
            }
            UnclassifiedCommand::NoTenantForPinnedCommand(name) => write!(
                f,
                "{name:?} is addressed at a specific tenant, but no tenant was given"
            ),
        }
    }
}

impl std::error::Error for UnclassifiedCommand {}

#[cfg(test)]
mod tests {
    use super::*;

    const TICKETS: &[&str] = &[
        "CreateTicket",
        "AssignTicket",
        "ResolveTicket",
        "ReopenTicket",
        "RequestInfoFromCustomer",
        "CustomerRespondsToTicket",
        "CloseTicket",
        "EscalateTicket",
        "MergeTickets",
        "RateTicket",
        "AddInternalNote",
    ];
    const LIFECYCLE: &[&str] = &[
        "SignUpCompany",
        "ConvertCompanyTrial",
        "ExpireCompanyTrial",
        "ReactivateCompany",
    ];

    #[test]
    fn with_the_cutover_off_everything_stays_shared() {
        // The target is the assertion that matters here; the reason is
        // checked per class, because lifecycle is pinned *before* the
        // mode is consulted (it never moves, so "the cutover is off"
        // isn't a fact about it) while Ticket traffic moves only
        // because the cutover is off.
        for name in TICKETS {
            let decision = route(RoutingMode::Shared, name, Some("company-acme")).unwrap();
            assert_eq!(
                decision.target,
                Target::Shared,
                "{name} must stay shared while the cutover is off"
            );
            assert_eq!(decision.reason, RouteReason::CutoverDisabled);
        }
        for name in LIFECYCLE {
            let decision = route(RoutingMode::Shared, name, Some("company-acme")).unwrap();
            assert_eq!(decision.target, Target::Shared, "{name} must stay shared");
            assert_eq!(decision.reason, RouteReason::NotPerCompany);
        }
    }

    #[test]
    fn lifecycle_never_follows_a_company_into_its_tenant() {
        // Even with the cutover fully on and a tenant known. This is the
        // single most important assertion in the module: routing a
        // lifecycle command into a tenant would let that tenant decide
        // its own company's status, which is exactly the authority
        // `CompanyLifecycleMirrored` exists to keep in one place.
        for name in LIFECYCLE {
            let decision = route(RoutingMode::Tenant, name, Some("company-acme")).unwrap();
            assert_eq!(decision.target, Target::Shared, "{name} must stay shared");
            assert_eq!(decision.reason, RouteReason::NotPerCompany);
        }
    }

    #[test]
    fn ticket_traffic_follows_the_company_when_the_cutover_is_on() {
        for name in TICKETS {
            let decision = route(RoutingMode::Tenant, name, Some("company-acme")).unwrap();
            assert_eq!(
                decision.target,
                Target::Tenant("company-acme".to_string()),
                "{name} should follow its company"
            );
            assert_eq!(decision.reason, RouteReason::FollowedCompanyToTenant);
        }
    }

    #[test]
    fn a_company_with_no_tenant_falls_back_to_shared() {
        for name in TICKETS {
            let decision = route(RoutingMode::Tenant, name, None).unwrap();
            assert_eq!(
                decision.target,
                Target::Shared,
                "{name} for a company with no tenant must still work"
            );
            assert_eq!(decision.reason, RouteReason::NoTenantRecorded);
        }
    }

    #[test]
    fn per_tenant_infrastructure_commands_are_pinned_never_looked_up() {
        for name in ["RecordCompanyTenant", "RecordTenantLifecycle"] {
            let decision = route(RoutingMode::Shared, name, Some("company-acme")).unwrap();
            assert_eq!(
                decision.target,
                Target::Tenant("company-acme".to_string()),
                "{name} is addressed at a tenant regardless of the cutover mode"
            );
            assert_eq!(decision.reason, RouteReason::PinnedToTenant);
        }
    }

    #[test]
    fn a_pinned_command_with_no_tenant_is_refused_rather_than_defaulted() {
        let err = route(RoutingMode::Tenant, "RecordTenantLifecycle", None).unwrap_err();
        assert_eq!(
            err,
            UnclassifiedCommand::NoTenantForPinnedCommand("RecordTenantLifecycle".into())
        );
    }

    #[test]
    fn the_mode_is_opt_in() {
        assert_eq!(RoutingMode::from_env_value(None), RoutingMode::Shared);
        assert_eq!(RoutingMode::from_env_value(Some("")), RoutingMode::Shared);
        assert_eq!(
            RoutingMode::from_env_value(Some("nonsense")),
            RoutingMode::Shared
        );
        assert_eq!(
            RoutingMode::from_env_value(Some("shared")),
            RoutingMode::Shared
        );
        assert_eq!(
            RoutingMode::from_env_value(Some("tenant")),
            RoutingMode::Tenant
        );
        assert_eq!(
            RoutingMode::from_env_value(Some("TENANT")),
            RoutingMode::Tenant
        );
    }

    #[test]
    fn an_unknown_command_type_is_refused_rather_than_guessed() {
        let err = route(RoutingMode::Tenant, "NotARealCommand", Some("company-acme")).unwrap_err();
        assert_eq!(
            err,
            UnclassifiedCommand::Unclassified("NotARealCommand".into())
        );
        assert!(err.to_string().contains("NotARealCommand"));
    }

    #[test]
    fn the_target_name_is_what_a_credential_would_be_minted_for() {
        assert_eq!(Target::Shared.bounded_context(), BOUNDED_CONTEXT);
        assert_eq!(
            Target::Tenant("company-acme".into()).bounded_context(),
            "company-acme"
        );
    }

    // --- tenant_name_for: the naming rules skilj actually enforces ---

    /// The exact constraint `AddBoundedContext` validates, restated here
    /// so a change to that rule shows up as a failing test rather than as
    /// a provisioner that mysteriously stops provisioning some companies.
    fn assert_valid_bounded_context_name(name: &str) {
        assert!(
            !name.is_empty() && name.len() <= 40,
            "{name:?} must be 1..=40 characters (got {})",
            name.len()
        );
        let mut chars = name.chars();
        let first = chars.next().expect("non-empty");
        assert!(
            first.is_ascii_lowercase(),
            "{name:?} must start with a lowercase letter"
        );
        for ch in chars {
            assert!(
                ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_',
                "{name:?} contains {ch:?}, which is not [a-z0-9_]"
            );
        }
    }

    #[test]
    fn derived_tenant_names_are_always_legal_bounded_context_names() {
        // The inputs that broke the old `company-{company_id}` scheme,
        // plus the boundaries either side of them.
        for company_id in [
            "acme",
            "acme-corp",
            "acme corp",
            "ACME",
            "Acme Corp, Inc.",
            "ünïcödé",
            "",
            "   ",
            "---",
            "9lives",
            &"x".repeat(200),
            &format!("{}-{}", "a".repeat(30), "b".repeat(30)),
        ] {
            let name = tenant_name_for(company_id);
            assert_valid_bounded_context_name(&name);
        }
    }

    #[test]
    fn distinct_company_ids_never_collide_on_one_tenant() {
        // The two that normalise to the same prefix - a slug-based scheme
        // without the digest would merge these two companies into one
        // tenant, which is a cross-company data leak rather than a
        // cosmetic problem.
        assert_ne!(
            tenant_name_for("acme-corp"),
            tenant_name_for("acme corp"),
            "two companies must never be given the same tenant"
        );
        assert_ne!(
            tenant_name_for("acme"),
            tenant_name_for("acme2"),
            "the digest must depend on the whole id, not just its prefix"
        );
        // And the same id must always give the same tenant, or a restart
        // would provision a second context for one company.
        assert_eq!(tenant_name_for("acme"), tenant_name_for("acme"));
    }

    #[test]
    fn the_digest_is_a_correct_fnv1a() {
        // Published FNV-1a 64-bit vectors. Without these the stability
        // test below would only prove the implementation is consistent
        // with itself, which is not the same as being correct - a wrong
        // hash would be just as permanently wrong.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn a_tenant_name_is_stable_across_processes() {
        // Pinned, not recomputed: this exact name goes into
        // `CompanyTenantProvisioned` and is read back forever, so a
        // change to the scheme or the digest would strand every already
        // -provisioned company's recorded tenant on a name nothing
        // derives any more.
        assert_eq!(
            tenant_name_for("acme"),
            "company_acme_0724d383f4f6de0f",
            "the derivation for `acme` changed - existing tenants would be orphaned"
        );
        assert_eq!(
            tenant_name_for("globex"),
            "company_globex_7fc12157b54bb22e",
            "the derivation for `globex` changed - existing tenants would be orphaned"
        );
    }

    #[test]
    fn a_readable_id_stays_readable_in_its_tenant_name() {
        // The digest is there for uniqueness, not to make names opaque:
        // an operator reading `CompanyTenantProvisioned` should still be
        // able to tell which company a tenant belongs to.
        let name = tenant_name_for("acme");
        assert!(
            name.starts_with("company_acme_"),
            "expected a recognisable prefix, got {name:?}"
        );
    }
}
