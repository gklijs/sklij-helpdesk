//! Keeping a company's access grants in its own tenant in step with the
//! ones recorded against it in the shared `helpdesk` context.
//!
//! ## Why this has to exist before any traffic is routed
//!
//! `skilj-graphql`'s `submitCommand` authorizes against the caller's own
//! `RoleAccessMapping` on the *named* bounded context
//! (`get_active_role_access_mapping` inside that resolver) - it never
//! consults the shared context. So the moment `routing.rs` starts sending
//! a company's Ticket commands at that company's tenant, the customer's
//! grant has to exist **on the tenant**, or every one of their commands is
//! rejected. Nothing about routing creates it:
//! `createBoundedContextFromTemplate` grants exactly one Role (the ops
//! identity the provisioner runs as), and `SignUpCompany` creates no Roles
//! at all.
//!
//! ## The authority question
//!
//! The shared `helpdesk` context stays authoritative for *who belongs to a
//! company*; a tenant is a faithful projection of that, never a source in
//! its own right. That makes this a reconciliation in one direction, with
//! revocation - see `sync_plan`'s own doc comment for why the revocation
//! half is the one that matters for safety rather than a tidy-up.
//!
//! ## What is and isn't copied
//!
//! Only mappings whose `scope` is exactly this company's id - that is, the
//! company's own roles (a customer, say). A mapping with `scope: None` is
//! *cross-company* by definition (`RoleAccessMapping::scope`'s own doc
//! comment: `None` means unrestricted within the bounded context), and
//! `staff-lead` is exactly that: real support staff serving every company.
//! Copying a `None`-scoped mapping into one company's tenant would look
//! like a faithful sync while quietly *narrowing* a deliberately
//! unrestricted staff grant to a single company. Those are left alone
//! entirely, and the cross-company case stays the open Phase 4 question
//! (see this crate's `README.md`) rather than something this pass guesses
//! at.

use crate::helpdesk::BOUNDED_CONTEXT;
use skilj_core::access_control::{self, AccessLevel, Role, RoleAccessMapping};
use skilj_core::db;

/// One grant/revoke the reconciler intends to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    /// This Role has a company-scoped grant on the shared context but none
    /// on the tenant. Carries the shared grant's level and sensitive-read
    /// flag so the tenant copy is never weaker than the authority.
    Grant {
        role_id: String,
        level: AccessLevel,
        can_read_sensitive: bool,
    },
    /// The tenant still holds a company-scoped grant whose shared-context
    /// counterpart is gone. Only ever proposed for a mapping scoped to
    /// *this* company - see `sync_plan`.
    Revoke { role_id: String },
}

/// The reconciler's whole decision, as data: the grants and revokes that
/// would make one company's tenant grants match the shared context's.
///
/// Pure on purpose. This is the part worth testing exhaustively - in
/// particular its negative cases, where getting it wrong leaks access -
/// and none of it needs a database.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncPlan {
    pub grants: Vec<SyncAction>,
    pub revokes: Vec<SyncAction>,
}

impl SyncPlan {
    /// True when the tenant already matches the authority, which is the
    /// overwhelmingly common case. Callers use it to stay quiet rather
    /// than log an empty tick every interval.
    pub fn is_noop(&self) -> bool {
        self.grants.is_empty() && self.revokes.is_empty()
    }
}

/// Compute the grants/revokes needed to project one company's access from
/// the shared context into its tenant.
///
/// `shared_mappings` and `tenant_mappings` are *all* active mappings on
/// each of those bounded contexts, not just this company's - filtering is
/// this function's job, so a caller can't forget it and sync the wrong
/// company's grants.
///
/// **Revocation is deliberately scoped.** Only a tenant mapping whose
/// `scope` equals `company_id` is ever proposed for revocation, and only
/// when the shared context has no active company-scoped mapping for that
/// same role. Two consequences, both intended:
///
///   - The ops Role `createBoundedContextFromTemplate` granted is
///     `scope: None`, so it is never revoked here. This reconciler
///     projects *company membership*, not the provisioning identity, and
///     revoking the identity that runs the provisioner would be a
///     self-inflicted outage.
///   - A role that only ever had an *unscoped* tenant grant is likewise
///     left alone, because "absent from shared" is not evidence that a
///     shared grant was revoked - it may never have had one. Only scope
///     equality makes the absence meaningful: a scoped mapping is a
///     positive statement "this role belongs to this company", and if
///     that statement is gone from the authority it must not survive in
///     the projection.
pub fn sync_plan(
    company_id: &str,
    shared_mappings: &[RoleAccessMapping],
    tenant_mappings: &[RoleAccessMapping],
) -> SyncPlan {
    let scoped_to_company = |m: &&RoleAccessMapping| m.scope.as_deref() == Some(company_id);

    let shared_for_company: Vec<&RoleAccessMapping> =
        shared_mappings.iter().filter(scoped_to_company).collect();
    let tenant_for_company: Vec<&RoleAccessMapping> =
        tenant_mappings.iter().filter(scoped_to_company).collect();

    let mut plan = SyncPlan::default();

    for shared in &shared_for_company {
        if !tenant_for_company
            .iter()
            .any(|t| t.role.id == shared.role.id)
        {
            plan.grants.push(SyncAction::Grant {
                role_id: shared.role.id.clone(),
                level: shared.level,
                can_read_sensitive: shared.can_read_sensitive,
            });
        }
    }

    for tenant in &tenant_for_company {
        if !shared_for_company
            .iter()
            .any(|s| s.role.id == tenant.role.id)
        {
            plan.revokes.push(SyncAction::Revoke {
                role_id: tenant.role.id.clone(),
            });
        }
    }

    plan
}

/// What a reconciliation actually did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    pub company_id: String,
    pub tenant_name: String,
    pub granted: Vec<String>,
    pub revoked: Vec<String>,
    /// Something this reconciler declined to do or could not do. Non-empty
    /// means the tenant is *not* a faithful copy of the authority, which
    /// is worth surfacing rather than leaving access quietly wrong.
    pub skipped: Vec<String>,
}

/// Project one company's access from the shared `helpdesk` context into
/// `tenant_name`.
///
/// `ops_role` must be an active superadmin - the same identity that could
/// call `grantRoleAccessMapping` directly, since skilj's own
/// `grant_role_access_mapping` requires one.
///
/// **One bad company does not stop the rest.** A failure on any single
/// grant or revoke is recorded in `SyncOutcome::skipped` and the loop
/// continues: a tenant missing one customer's grant is a far smaller
/// problem than a reconciler that wedges on the first company it can't
/// write and then stops syncing every other company. The `skipped` list is
/// how that stays visible instead of silent.
pub async fn reconcile_company_access(
    pool: &db::Pool,
    ops_role: &Role,
    company_id: &str,
    tenant_name: &str,
) -> SyncOutcome {
    let mut outcome = SyncOutcome {
        company_id: company_id.to_string(),
        tenant_name: tenant_name.to_string(),
        ..SyncOutcome::default()
    };

    let skip = |outcome: &mut SyncOutcome, message: String| {
        outcome.skipped.push(message);
    };

    let Some(shared_bc) = db::get_bounded_context(pool, BOUNDED_CONTEXT)
        .await
        .ok()
        .flatten()
    else {
        skip(
            &mut outcome,
            "shared helpdesk bounded context not found".into(),
        );
        return outcome;
    };
    let Some(tenant_bc) = db::get_bounded_context(pool, tenant_name)
        .await
        .ok()
        .flatten()
    else {
        skip(&mut outcome, format!("tenant {tenant_name:?} not found"));
        return outcome;
    };

    let shared_mappings =
        match db::list_active_role_access_mappings_for_bounded_context(pool, &shared_bc).await {
            Ok(m) => m,
            Err(e) => {
                skip(
                    &mut outcome,
                    format!("couldn't list shared-context mappings: {e}"),
                );
                return outcome;
            }
        };
    let tenant_mappings =
        match db::list_active_role_access_mappings_for_bounded_context(pool, &tenant_bc).await {
            Ok(m) => m,
            Err(e) => {
                skip(&mut outcome, format!("couldn't list tenant mappings: {e}"));
                return outcome;
            }
        };

    let plan = sync_plan(company_id, &shared_mappings, &tenant_mappings);

    for action in plan.grants {
        let SyncAction::Grant {
            role_id,
            level,
            can_read_sensitive,
        } = action
        else {
            unreachable!("sync_plan only ever puts Grant in `grants`")
        };
        match grant_one(
            pool,
            ops_role,
            &role_id,
            &tenant_bc,
            level,
            can_read_sensitive,
            company_id,
        )
        .await
        {
            Ok(()) => outcome.granted.push(role_id),
            Err(e) => skip(
                &mut outcome,
                format!("grant of {role_id} on {tenant_name:?} failed: {e}"),
            ),
        }
    }

    for action in plan.revokes {
        let SyncAction::Revoke { role_id } = action else {
            unreachable!("sync_plan only ever puts Revoke in `revokes`")
        };
        match revoke_one(pool, &role_id, tenant_name, company_id).await {
            Ok(()) => outcome.revoked.push(role_id),
            Err(e) => skip(
                &mut outcome,
                format!("revocation of {role_id} on {tenant_name:?} failed: {e}"),
            ),
        }
    }

    outcome
}

#[allow(clippy::too_many_arguments)]
async fn grant_one(
    pool: &db::Pool,
    ops_role: &Role,
    role_id: &str,
    tenant_bc: &skilj_core::event_store::BoundedContext,
    level: AccessLevel,
    can_read_sensitive: bool,
    scope: &str,
) -> Result<(), String> {
    let Some(role) = db::get_role(pool, role_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Err("role no longer exists".to_string());
    };
    // Re-read the tenant's mappings immediately before granting rather
    // than trusting the earlier listing: `grant_role_access_mapping`
    // rejects a duplicate active grant, and a second reconciler pass
    // landing between the two would otherwise surface as a hard failure
    // on what is really a no-op.
    let existing = db::list_active_role_access_mappings_for_bounded_context(pool, tenant_bc)
        .await
        .map_err(|e| e.to_string())?;
    let granted = access_control::grant_role_access_mapping(
        ops_role,
        &role,
        tenant_bc,
        level,
        can_read_sensitive,
        Some(scope.to_string()),
        &existing,
        chrono::Utc::now(),
    )
    .map_err(|e| e.to_string())?;
    db::insert_role_access_mapping(pool, &granted)
        .await
        .map_err(|e| e.to_string())
}

async fn revoke_one(
    pool: &db::Pool,
    role_id: &str,
    tenant_name: &str,
    scope: &str,
) -> Result<(), String> {
    // Confirm the mapping really is this company's before revoking. The
    // scope check is repeated here rather than trusted from the plan
    // because this is the one write that *removes* access: the plan's
    // filter is a caller-side convenience, and this is where it has to be
    // right.
    let Some(existing) = db::get_active_role_access_mapping(pool, role_id, tenant_name)
        .await
        .map_err(|e| e.to_string())?
    else {
        // Already gone - not an error, and what makes a concurrent double
        // reconcile (two loops, or a retry after a partial failure)
        // harmless.
        return Ok(());
    };
    if existing.scope.as_deref() != Some(scope) {
        return Err(format!(
            "refusing to revoke {role_id} on {tenant_name:?}: its grant is scoped {:?}, not {scope:?}",
            existing.scope
        ));
    }
    db::revoke_active_role_access_mapping(pool, role_id, tenant_name, chrono::Utc::now())
        .await
        .map_err(|e| e.to_string())
}

/// Every `(company_id, tenant_name)` pair the shared context has recorded,
/// newest record per company.
///
/// Reads `CompanyTenantProvisioned` events directly rather than through
/// the `TenantDirectory` projection because a projection has to be read by
/// *key*, and the company ids are exactly what's being looked up here -
/// reading the events is what makes "which companies have tenants"
/// answerable without already knowing a company to ask about.
pub async fn recorded_company_tenants(
    pool: &db::Pool,
) -> Result<Vec<(String, String)>, skilj_core::error::Error> {
    let events = db::list_events_for_bounded_context(pool, BOUNDED_CONTEXT).await?;
    let mut latest: std::collections::HashMap<String, (i64, String)> =
        std::collections::HashMap::new();
    for event in &events {
        if event.event_type.name != "CompanyTenantProvisioned" {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.payload) else {
            continue;
        };
        let (Some(company_id), Some(tenant_name)) = (
            payload["company_id"].as_str(),
            payload["tenant_name"].as_str(),
        ) else {
            continue;
        };
        // `RecordCompanyTenant`'s own guard refuses a second record per
        // company, so taking the latest is belt-and-braces - but a
        // hand-written or imported history shouldn't silently resolve a
        // company to whichever tenant happened to be written first.
        latest.insert(
            company_id.to_string(),
            (event.sequence, tenant_name.to_string()),
        );
    }
    let mut pairs: Vec<(i64, String, String)> = latest
        .into_iter()
        .map(|(company_id, (sequence, tenant_name))| (sequence, company_id, tenant_name))
        .collect();
    pairs.sort_by_key(|(sequence, _, _)| *sequence);
    Ok(pairs
        .into_iter()
        .map(|(_, company_id, tenant_name)| (company_id, tenant_name))
        .collect())
}

/// Reconcile every recorded company/tenant pair. Cheap enough to call on
/// an interval: a no-op company is two index reads and a comparison.
pub async fn reconcile_all_tenants(pool: &db::Pool, ops_role: &Role) -> Vec<SyncOutcome> {
    let pairs = match recorded_company_tenants(pool).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("tenant-access: couldn't list recorded company tenants: {e}");
            return Vec::new();
        }
    };
    let mut outcomes = Vec::with_capacity(pairs.len());
    for (company_id, tenant_name) in pairs {
        let outcome = reconcile_company_access(pool, ops_role, &company_id, &tenant_name).await;
        if !outcome.granted.is_empty() || !outcome.revoked.is_empty() {
            println!(
                "tenant-access: company {} / tenant {}: granted {}, revoked {}",
                outcome.company_id,
                outcome.tenant_name,
                outcome.granted.len(),
                outcome.revoked.len()
            );
        }
        for skipped in &outcome.skipped {
            eprintln!(
                "tenant-access: company {} / tenant {}: {skipped}",
                outcome.company_id, outcome.tenant_name
            );
        }
        outcomes.push(outcome);
    }
    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;
    use skilj_core::access_control::RoleStatus;
    use skilj_core::bootstrap::ContextCreator;
    use skilj_core::event_store::{BoundedContext, BoundedContextStatus};

    fn role(id: &str) -> Role {
        Role {
            id: id.to_string(),
            external_subject: format!("sub-{id}"),
            name: format!("role-{id}"),
            superadmin: false,
            status: RoleStatus::Active,
            created_at: chrono::Utc::now(),
            revoked_at: None,
        }
    }

    fn context(name: &str) -> BoundedContext {
        BoundedContext {
            name: name.to_string(),
            status: BoundedContextStatus::Active,
            created_at: chrono::Utc::now(),
            created_by: ContextCreator::SystemCreator,
            template: None,
        }
    }

    fn mapping(role_id: &str, context_name: &str, scope: Option<&str>) -> RoleAccessMapping {
        RoleAccessMapping {
            role: role(role_id),
            bounded_context: context(context_name),
            level: AccessLevel::Write,
            can_read_sensitive: false,
            scope: scope.map(str::to_string),
            status: RoleStatus::Active,
            created_at: chrono::Utc::now(),
            revoked_at: None,
        }
    }

    fn grant(role_id: &str) -> SyncAction {
        SyncAction::Grant {
            role_id: role_id.into(),
            level: AccessLevel::Write,
            can_read_sensitive: false,
        }
    }

    fn revoke(role_id: &str) -> SyncAction {
        SyncAction::Revoke {
            role_id: role_id.into(),
        }
    }

    #[test]
    fn a_company_scoped_shared_grant_is_copied_into_the_tenant() {
        let plan = sync_plan(
            "acme",
            &[mapping("cust", BOUNDED_CONTEXT, Some("acme"))],
            &[],
        );
        assert_eq!(plan.grants, vec![grant("cust")]);
        assert!(plan.revokes.is_empty());
    }

    #[test]
    fn the_grant_carries_the_shared_ones_level_and_sensitive_flag() {
        // A tenant copy weaker than the shared grant would silently
        // downgrade a customer rather than fail loudly.
        let mut shared = mapping("cust", BOUNDED_CONTEXT, Some("acme"));
        shared.level = AccessLevel::Admin;
        shared.can_read_sensitive = true;
        let plan = sync_plan("acme", &[shared], &[]);
        assert_eq!(
            plan.grants,
            vec![SyncAction::Grant {
                role_id: "cust".into(),
                level: AccessLevel::Admin,
                can_read_sensitive: true,
            }]
        );
    }

    #[test]
    fn reconciling_an_already_synced_tenant_is_a_noop() {
        let plan = sync_plan(
            "acme",
            &[mapping("cust", BOUNDED_CONTEXT, Some("acme"))],
            &[mapping("cust", "company-acme", Some("acme"))],
        );
        assert!(plan.is_noop(), "unexpected plan: {plan:?}");
    }

    #[test]
    fn another_companys_grants_are_never_touched() {
        // The safety-critical negative case: reconciling company A must not
        // see, copy, or revoke company B's grants.
        let shared = vec![
            mapping("cust-a", BOUNDED_CONTEXT, Some("acme")),
            mapping("cust-b", BOUNDED_CONTEXT, Some("globex")),
        ];
        let tenant = vec![mapping("cust-a", "company-acme", Some("globex"))];

        let plan = sync_plan("acme", &shared, &tenant);
        assert_eq!(
            plan.grants,
            vec![grant("cust-a")],
            "only acme's own shared grant is a candidate"
        );
        assert!(
            plan.revokes.is_empty(),
            "globex's tenant grant must not be revocable while reconciling acme: {plan:?}"
        );
    }

    #[test]
    fn a_revoke_on_the_shared_context_is_mirrored_into_the_tenant() {
        // The reason this reconciles rather than copying once: a customer
        // removed on the authority must lose access in the tenant too, or
        // revoking them would be a no-op in practice.
        let plan = sync_plan(
            "acme",
            &[],
            &[mapping("gone-cust", "company-acme", Some("acme"))],
        );
        assert_eq!(plan.revokes, vec![revoke("gone-cust")]);
        assert!(plan.grants.is_empty());
    }

    #[test]
    fn the_ops_identity_is_never_revoked() {
        // `createBoundedContextFromTemplate`'s grant is `scope: None`, so
        // it is outside what this reconciler projects - revoking it would
        // lock the provisioner out of the tenants it maintains.
        let plan = sync_plan(
            "acme",
            &[mapping("cust", BOUNDED_CONTEXT, Some("acme"))],
            &[
                mapping("ops", "company-acme", None),
                mapping("cust", "company-acme", Some("acme")),
            ],
        );
        assert!(plan.is_noop(), "unexpected plan: {plan:?}");
    }

    #[test]
    fn an_unscoped_role_on_the_shared_context_is_never_copied() {
        // staff-lead is cross-company (`scope: None`). Copying it into one
        // company's tenant would look like a sync while quietly narrowing
        // a deliberately unrestricted staff grant to a single company.
        let plan = sync_plan("acme", &[mapping("staff-lead", BOUNDED_CONTEXT, None)], &[]);
        assert!(
            plan.is_noop(),
            "a cross-company grant must not be projected into one tenant: {plan:?}"
        );
    }

    #[test]
    fn an_unscoped_tenant_grant_with_no_shared_counterpart_survives() {
        // "Absent from shared" is only evidence of a revocation for a
        // mapping that was company-scoped to begin with. An unscoped grant
        // says nothing about company membership, so its absence from the
        // authority says nothing either.
        let plan = sync_plan(
            "acme",
            &[],
            &[mapping("locally-granted", "company-acme", None)],
        );
        assert!(
            plan.revokes.is_empty(),
            "an unscoped tenant grant isn't this reconciler's business: {plan:?}"
        );
    }

    #[test]
    fn several_roles_reconcile_in_one_pass() {
        let plan = sync_plan(
            "acme",
            &[
                mapping("cust-1", BOUNDED_CONTEXT, Some("acme")),
                mapping("cust-2", BOUNDED_CONTEXT, Some("acme")),
            ],
            &[mapping("cust-1", "company-acme", Some("acme"))],
        );
        assert_eq!(plan.grants, vec![grant("cust-2")]);
        assert!(plan.revokes.is_empty());
    }
}
