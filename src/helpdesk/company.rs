//! Company signup and the trial/subscription lifecycle: the company
//! events, the `company_status`/`CompanyFacts` folds the guards read,
//! the commands and deadlines that move a company between states, and
//! the `TenantDirectory` read model.

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompanySignedUpPayload {
    pub company_id: String,
    pub name: String,
    pub contact_email: String,
}

pub struct CompanySignedUp;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for CompanySignedUp {
    type Payload = CompanySignedUpPayload;
    const NAME: &'static str = "CompanySignedUp";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    /// `src/bin/provisioner.rs` reads this via an `EventReadToken` to
    /// provision each new company's own tenant - see
    /// `TicketCreated::event_read_allowed`'s own doc comment for why
    /// this override exists at all.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompanyTenantProvisionedPayload {
    pub company_id: String,
    pub tenant_name: String,
}

pub struct CompanyTenantProvisioned;

/// `RecordCompanyTenant`'s own outcome - see that command's doc comment.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for CompanyTenantProvisioned {
    type Payload = CompanyTenantProvisionedPayload;
    const NAME: &'static str = "CompanyTenantProvisioned";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    /// Phase 4 multi-tenant alerter discovers tenants by reading this
    /// event off the shared context's own REST feed - the alerter has no
    /// other way to enumerate which companies have been provisioned a
    /// tenant at runtime, since `TenantDirectory` is keyed by company_id
    /// and `CrossContextRoute` can't fan out to a runtime-created set of
    /// contexts. The event carries nothing `company_id`-scoped beyond what
    /// skilj's own `event_read_allowed` gate exists to control: it carries
    /// a `tenant_name`, not a `ticket_id` or any per-company data.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompanyActivatedPayload {
    pub company_id: String,
}

pub struct CompanyActivated;

/// One event for both `specs/skilj-helpdesk.allium`'s `rule
/// TrialPeriodEnds`'s success branch (`trialing -> active`) and `rule
/// CompanySubscribes`'s success branch (`expired -> active`) - the spec
/// keeps them as two rules because they're two different triggers (a
/// trial deadline firing vs. a company choosing to pay), but the
/// resulting domain fact is identical ("this company is now active"), so
/// one event type covers both here, the same simplification
/// `CreateTicket` already makes for its own two triggering rules.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for CompanyActivated {
    type Payload = CompanyActivatedPayload;
    const NAME: &'static str = "CompanyActivated";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompanyExpiredPayload {
    pub company_id: String,
}

pub struct CompanyExpired;

/// `specs/skilj-helpdesk.allium`'s `rule TrialPeriodEnds`'s failure
/// branch: `trialing -> expired`.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for CompanyExpired {
    type Payload = CompanyExpiredPayload;
    const NAME: &'static str = "CompanyExpired";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    /// `src/bin/lifecycle-replicator.rs` reads this to mirror a
    /// company's `trialing -> expired` transition into its own tenant -
    /// see `CompanySignedUp::event_read_allowed`'s own doc comment for
    /// why the override exists at all.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompanyLifecycleMirroredPayload {
    pub company_id: String,
    /// `CompanyStatus` needs the serde/schemars derives `TicketPriority`
    /// above already carries (and `rename_all = "snake_case"`, so the
    /// wire values are `"trialing"`/`"active"`/`"expired"` rather than
    /// Rust's default PascalCase) - this payload is what makes it a
    /// value that can cross a process boundary at all. Before this pass
    /// `CompanyStatus` was a pure in-process `decide()` fold result and
    /// needed no derives; that stopped being true the moment a tenant's
    /// own history has to record one.
    pub status: CompanyStatus,
    /// The name of the shared-context event this fact was derived from
    /// (`"CompanySignedUp"`/`"CompanyActivated"`/`"CompanyExpired"`),
    /// recorded rather than inferred so an operator reading a tenant's
    /// own event log can tell a replayed lifecycle fact from a locally
    /// originated one. Never a routing input - `company_status` below
    /// folds `status` only, so this field is provenance, not behaviour.
    pub source_event_type: String,
}

pub struct CompanyLifecycleMirrored;

/// A company's lifecycle state, **mirrored** into its own tenant - not
/// a lifecycle transition that happened there.
///
/// **Why this event exists at all.** `CreateTicket` and every other
/// Ticket command opens with a `company_status` guard read, and that
/// read is one *same-context* DCB query: skilj hands `decide()` only
/// the events carrying the command's own tags, read from that bounded
/// context's own `bc_<name>` schema. A tenant's own history therefore
/// contains nothing about its company's lifecycle, because signup
/// happens in the shared `helpdesk` context, so a `CreateTicket` routed
/// at a tenant would be rejected `company_not_found` for a company that
/// demonstrably exists.
///
/// **The alternative, and why it isn't this.** skilj's own
/// `CrossContextRoute` cannot express "fan this one event out to every
/// tenant": `Target: CommandType` is a single type whose
/// `BOUNDED_CONTEXT` is a compile-time const
/// (`skilj_core::plugin::CrossContextRoute`'s own doc comment), and the
/// background route loop reads its route list *once at startup*
/// (`skilj::Skilj`'s `cross_context_routes` loop), with no runtime
/// registration surface for a newly-created tenant. Deadlines have the
/// dynamic fan-out `routes` lack (`deadline_fire_tick` re-lists bounded
/// contexts every tick), but no `ScheduleDeadline` fires off another
/// bounded context's events either - both source types are static consts
/// too.
///
/// So the fan-out has to happen in application code, which is what
/// `src/bin/lifecycle-replicator.rs` is: it reads lifecycle events off
/// the REST event feed and submits `RecordTenantLifecycle` below into
/// the right tenant, following the same "I/O happens outside `decide()`
/// in its own binary reacting to the event feed" split
/// `provisioner.rs`'s own module doc comment describes for tenant
/// creation. This event is what such a submission commits.
///
/// **Why it is safe to trust.** It is not a general "write anything you
/// like into a tenant" door: `RecordTenantLifecycle::decide` below
/// accepts only the three real lifecycle states, and (more importantly)
/// the *shared* `helpdesk` context stays the single authority for
/// lifecycle transitions - nothing in a tenant can move a company
/// between states on its own, so a compromised or buggy tenant client
/// can at worst make its own (already-authoritative-shared) view stale,
/// which `RecordTenantLifecycle`'s own guard below bounds to "never
/// regress to an earlier state".
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for CompanyLifecycleMirrored {
    type Payload = CompanyLifecycleMirroredPayload;
    const NAME: &'static str = "CompanyLifecycleMirrored";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
}

/// `specs/skilj-helpdesk.allium`'s `Company.status`, in full.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompanyStatus {
    Trialing,
    Active,
    Expired,
}

/// Folds this company's own status - same technique as `TicketFacts`
/// above. `None` means the company doesn't exist (no `CompanySignedUp`
/// found).
///
/// **Two sources, one answer.** In the shared `helpdesk` context the
/// status comes from the real `CompanySignedUp`/`CompanyActivated`/
/// `CompanyExpired` events. In a tenant it comes from
/// `CompanyLifecycleMirrored` instead, because a tenant's own history
/// holds none of the three - see that event's own doc comment for why
/// the mirror exists. Both are folded here, in the same pass, in stored
/// sequence order, so a mixed history (a tenant that also happened to
/// sign the company up locally, as `tests/multi_tenant_provisioning.rs`
/// does) resolves by recency exactly the way a single-source history
/// would - no special-casing, no "prefer the mirror" branch to get
/// wrong.
pub(super) fn company_status(
    matching_events: &[HelpdeskEvent],
    company_id: &str,
) -> Option<CompanyStatus> {
    let mut status = None;
    for event in matching_events {
        match event {
            HelpdeskEvent::CompanySignedUp(p) if p.company_id == company_id => {
                status = Some(CompanyStatus::Trialing);
            }
            HelpdeskEvent::CompanyActivated(p) if p.company_id == company_id => {
                status = Some(CompanyStatus::Active);
            }
            HelpdeskEvent::CompanyExpired(p) if p.company_id == company_id => {
                status = Some(CompanyStatus::Expired);
            }
            // The mirror, folded identically to the event it stands in
            // for. Deliberately *not* matched on `source_event_type`:
            // that field is provenance for a human reading the log, and
            // trusting it here would mean a payload that disagrees with
            // its own `status` field silently decides a Ticket command's
            // guard. `status` is the single field this reads.
            HelpdeskEvent::CompanyLifecycleMirrored(p) if p.company_id == company_id => {
                status = Some(p.status);
            }
            _ => {}
        }
    }
    status
}

/// What `CreateTicket` decides from: the company's status, which ticket
/// ids are already used, and its SLA policy, folded from its consistency
/// query's events (the company's latest lifecycle event, this ticket id's
/// `TicketCreated` if any, whichever company filed it, and its latest
/// `CompanySlaPolicySet`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CompanyFacts {
    /// Which company this is - events of any other are skipped.
    pub company_id: String,
    /// `None` means the company doesn't exist - see `company_status`.
    pub status: Option<CompanyStatus>,
    /// Every ticket id a `TicketCreated` used - of any company, see `apply`.
    pub ticket_ids: std::collections::BTreeSet<String>,
    /// Its latest `CompanySlaPolicySet`'s policy - `None` until it sets
    /// one, which `CreateTicket` reads as `SlaPolicy::default()`.
    pub sla_policy: Option<crate::sla::SlaPolicy>,
}

impl CompanyFacts {
    /// Folded from `company_id`'s slice of `matching_events`.
    pub fn of(matching_events: &[HelpdeskEvent], company_id: &str) -> Self {
        let mut facts = Self {
            company_id: company_id.to_string(),
            ..Self::default()
        };
        for event in matching_events {
            facts.apply(event);
        }
        facts
    }

    fn apply(&mut self, event: &HelpdeskEvent) {
        // A ticket id is used once across every company, not once per
        // company: the ticket's tag, its `TicketSnapshot` and every
        // ticket-keyed read model know it by id alone, so a second
        // company's `TicketCreated` under a used id would land on the
        // first company's ticket (and reset it to `open`, even out of
        // `closed`/`merged`). The frontend's `tk-<millis>` ids make that
        // an accident waiting to happen, not just an attack - see
        // docs/allium-spec-drift-2026-10-12.md.
        if let HelpdeskEvent::TicketCreated(p) = event {
            self.ticket_ids.insert(p.ticket_id.clone());
            return;
        }
        let company_id = match event {
            HelpdeskEvent::CompanySignedUp(p) => &p.company_id,
            HelpdeskEvent::CompanyActivated(p) => &p.company_id,
            HelpdeskEvent::CompanyExpired(p) => &p.company_id,
            HelpdeskEvent::CompanyLifecycleMirrored(p) => &p.company_id,
            HelpdeskEvent::CompanySlaPolicySet(p) => &p.company_id,
            _ => return,
        };
        if *company_id != self.company_id {
            return;
        }
        if let HelpdeskEvent::CompanySlaPolicySet(p) = event {
            self.sla_policy = Some(p.policy.clone());
        } else {
            self.status = company_status(std::slice::from_ref(event), company_id).or(self.status);
        }
    }
}

/// This company's own provisioned tenant name, if `RecordCompanyTenant`
/// has already recorded one - `None` covers both "hasn't signed up yet"
/// and "signed up but the provisioner hasn't reacted yet", same
/// approach as `company_status`'s own single `Option` for two not-yet
/// states; `RecordCompanyTenant::decide` below tells them apart itself
/// via `company_status`.
fn company_tenant(matching_events: &[HelpdeskEvent], company_id: &str) -> Option<String> {
    matching_events.iter().find_map(|event| match event {
        HelpdeskEvent::CompanyTenantProvisioned(p) if p.company_id == company_id => {
            Some(p.tenant_name.clone())
        }
        _ => None,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SignUpCompanyPayload {
    pub company_id: String,
    pub name: String,
    pub contact_email: String,
}

pub struct SignUpCompany;

/// `specs/skilj-helpdesk.allium`'s `rule CompanySignsUp`. What the spec
/// also does here - provisioning the company's own skilj tenant via
/// `CreateBoundedContextFromTemplate` - stays out of `decide()` (pure
/// and I/O-free by contract), but is no longer skipped: it now happens
/// as a real side effect, driven by `src/bin/provisioner.rs` reacting to
/// the `CompanySignedUp` this emits and reporting back via
/// `RecordCompanyTenant` below.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for SignUpCompany {
    type Payload = SignUpCompanyPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "SignUpCompany";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![company_status_query()]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        if payload.company_id.is_empty() {
            return CommandDecision::Rejected {
                reason: "company_id must not be empty".into(),
                kind: "empty_company_id".into(),
            };
        }
        if payload.company_id.len() > 256 {
            return CommandDecision::Rejected {
                reason: "company_id is too long (max 256 characters)".into(),
                kind: "company_id_too_long".into(),
            };
        }
        if payload.name.trim().is_empty() {
            return CommandDecision::Rejected {
                reason: "company name must not be empty".into(),
                kind: "empty_company_name".into(),
            };
        }
        if payload.name.len() > 256 {
            return CommandDecision::Rejected {
                reason: "company name is too long (max 256 characters)".into(),
                kind: "company_name_too_long".into(),
            };
        }
        let valid_email = match payload.contact_email.split_once('@') {
            Some((local, domain)) => {
                !local.is_empty()
                    && !domain.is_empty()
                    && !domain.contains('@')
                    && domain.contains('.')
                    && !domain.starts_with('.')
                    && !domain.ends_with('.')
                    && !payload.contact_email.chars().any(char::is_whitespace)
            }
            None => false,
        };
        if !valid_email {
            return CommandDecision::Rejected {
                reason: format!(
                    "contact_email {:?} is not a valid email address",
                    payload.contact_email
                ),
                kind: "invalid_email".into(),
            };
        }
        if company_status(matching_events, &payload.company_id).is_some() {
            return CommandDecision::Rejected {
                reason: format!("company {} has already signed up", payload.company_id),
                kind: "already_signed_up".into(),
            };
        }
        CommandDecision::Accepted {
            events: vec![EventSpec {
                event_type: "CompanySignedUp".into(),
                payload: serde_json::json!({
                    "company_id": payload.company_id,
                    "name": payload.name,
                    "contact_email": payload.contact_email,
                }),
            }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecordCompanyTenantPayload {
    pub company_id: String,
    pub tenant_name: String,
}

pub struct RecordCompanyTenant;

/// The real side effect `SignUpCompany`'s own doc comment above
/// describes, reported back: `src/bin/provisioner.rs` reads
/// `CompanySignedUp` off skilj's REST event feed, calls skilj's own
/// `createBoundedContextFromTemplate` GraphQL mutation (superadmin-gated
/// - see `tests/multi_tenant_provisioning.rs`, which proves this exact
/// mechanism first) to stamp a brand-new tenant from this `helpdesk`
/// context as the template, then submits this command to make the
/// result durable - the same "I/O happens outside decide(), a pure
/// command records the outcome" split `ScheduleCompanyTrialConversion`'s
/// own mocked `PaymentGateway.charge` uses, just with a real skilj call
/// in place of a mock.
///
/// This is one company's tenant coming into real existence, not a
/// no-op: `provisioner`'s own doc comment and this crate's `README.md`
/// are explicit that Ticket commands/queries for that company still run
/// against this shared context, not the tenant just created for it.
/// The `company_status` read those commands' guards depend on used to be
/// the reason that couldn't change; it no longer is, and the mechanism
/// is `CompanyLifecycleMirrored` above plus
/// `src/bin/lifecycle-replicator.rs`. The cutover itself - actually
/// routing those commands - is still not done, and is called out in this
/// crate's `README.md`.
///
/// Idempotent by construction, same shape as `EscalateTicket`'s own
/// `already_escalated` guard: a redelivered `CompanySignedUp` (the REST
/// feed's own accepted "occasional missed/redelivered events" tradeoff -
/// see `specs/skilj-helpdesk.allium`'s resolved alerting design note,
/// which `provisioner.rs` accepts for the identical reason) would
/// otherwise record two conflicting tenant names for one company.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for RecordCompanyTenant {
    type Payload = RecordCompanyTenantPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "RecordCompanyTenant";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![
            company_status_query(),
            QueryItemMapping::types(&["CompanyTenantProvisioned"]).tagged(company_tag()),
        ]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        if company_status(matching_events, &payload.company_id).is_none() {
            return CommandDecision::Rejected {
                reason: format!("company {} hasn't signed up", payload.company_id),
                kind: "company_not_found".into(),
            };
        }
        if let Some(existing) = company_tenant(matching_events, &payload.company_id) {
            return CommandDecision::Rejected {
                reason: format!(
                    "company {} already has tenant {existing:?} recorded",
                    payload.company_id
                ),
                kind: "tenant_already_provisioned".into(),
            };
        }
        CommandDecision::Accepted {
            events: vec![EventSpec {
                event_type: "CompanyTenantProvisioned".into(),
                payload: serde_json::json!({
                    "company_id": payload.company_id,
                    "tenant_name": payload.tenant_name,
                }),
            }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecordTenantLifecyclePayload {
    pub company_id: String,
    pub status: CompanyStatus,
    /// See `CompanyLifecycleMirroredPayload::source_event_type` - the
    /// same provenance string, carried through untouched.
    pub source_event_type: String,
}

pub struct RecordTenantLifecycle;

/// Writes a `CompanyLifecycleMirrored` fact into **this** bounded
/// context's own history - the write half of
/// `CompanyLifecycleMirrored`'s doc comment above, submitted by
/// `src/bin/lifecycle-replicator.rs` into the tenant named by
/// `RecordCompanyTenant`'s own recorded mapping.
///
/// **Not a lifecycle transition, and deliberately unidirectional.** The
/// shared `helpdesk` context remains the only place a company's status
/// actually changes; this command cannot move one, it can only record
/// what the shared context already decided. That asymmetry is the whole
/// safety argument, and it is why the guard below rejects a regression
/// rather than trying to validate a transition table of its own: a
/// tenant client that can only ever write the state it was last told,
/// and can never move backwards, cannot disagree with the authority in
/// a way that widens access.
///
/// **Why `status` is taken as already-resolved, not recomputed.** The
/// replicator derives it from the shared event's *type*
/// (`CompanySignedUp` -> `Trialing`), and this command trusts that
/// derivation rather than re-deriving it from a source event it cannot
/// read - the tenant has no copy of the shared event to re-read. The
/// compensating control is not a re-derivation but the caller shape:
/// this is a `rest_trigger_allowed` command reachable only with a
/// `CommandToken` minted against *this* tenant, which in this
/// deployment only `lifecycle-replicator.rs` (and tests) hold - see
/// `server.rs`'s own doc comment on how those tokens are minted.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for RecordTenantLifecycle {
    type Payload = RecordTenantLifecyclePayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = RECORD_TENANT_LIFECYCLE_COMMAND;
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![company_status_query()]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        // The shared context is authoritative, but a tenant's own mirror
        // history is not *ordered* by anything the shared context
        // guarantees: the replicator reads three separate REST event
        // feeds (`CompanySignedUp`/`CompanyActivated`/`CompanyExpired`,
        // each its own `EventReadToken` and therefore its own cursor), so
        // a company that converts and expires close together can deliver
        // `Expired` before `Activated`. Folding by sequence alone would
        // then leave the tenant permanently reporting the older state.
        //
        // So rank the states and refuse to move backwards, which makes
        // the fold order-independent for every ordering the three feeds
        // can actually produce. `Active` and `Expired` are both
        // terminal-ish ranks above `Trialing`, and `Active` outranks
        // `Expired` only because `ReactivateCompany`'s own
        // `expired -> active` transition makes "active" the later real
        // outcome of that pair; a company that genuinely expires *after*
        // activating is not a reachable sequence (the trial deadline is
        // cancelled on conversion - see `ScheduleCompanyTrialConversion`).
        let current = company_status(matching_events, &payload.company_id);
        if let Some(current) = current {
            if current == payload.status {
                return CommandDecision::Rejected {
                    reason: format!(
                        "company {} is already mirrored as {:?}",
                        payload.company_id, payload.status
                    ),
                    // Idempotency, same shape as `EscalateTicket`'s own
                    // `already_escalated` guard: the REST feed's
                    // `mode=auto` cursor can redeliver on a crash
                    // mid-tick (skilj-rest's own architecture docs §7.4),
                    // and `lifecycle-replicator.rs` logs a rejection and
                    // moves on rather than treating it as a failure.
                    kind: "lifecycle_already_mirrored".into(),
                };
            }
            if lifecycle_rank(payload.status) < lifecycle_rank(current) {
                return CommandDecision::Rejected {
                    reason: format!(
                        "refusing to mirror company {} backwards from {:?} to {:?}",
                        payload.company_id, current, payload.status
                    ),
                    kind: "lifecycle_regression".into(),
                };
            }
        }
        CommandDecision::Accepted {
            events: vec![EventSpec {
                event_type: "CompanyLifecycleMirrored".into(),
                payload: serde_json::json!({
                    "company_id": payload.company_id,
                    "status": payload.status,
                    "source_event_type": payload.source_event_type,
                }),
            }],
        }
    }
}

/// Total order over the lifecycle states, for
/// `RecordTenantLifecycle`'s own regression guard above - deliberately
/// hand-written rather than derived from the enum's declaration order,
/// so reordering the variants can't silently change what "backwards"
/// means. `Trialing` is first (everything starts there); `Active` above
/// `Expired` for the reason given at the guard.
fn lifecycle_rank(status: CompanyStatus) -> u8 {
    match status {
        CompanyStatus::Trialing => 0,
        CompanyStatus::Expired => 1,
        CompanyStatus::Active => 2,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConvertCompanyTrialPayload {
    pub company_id: String,
}

pub struct ConvertCompanyTrial;

/// `specs/skilj-helpdesk.allium`'s `rule TrialPeriodEnds`'s success
/// branch: `trialing -> active`. Submitted by skilj's own native
/// per-entity deadline mechanism (`ScheduleCompanyTrialConversion`
/// below, docs/architecture.md §46), not a person - see that reactor's
/// own doc comment for why this pass implements the *state change* as
/// an ordinary command rather than skilj's `system_triggered`
/// scheduling (a global-cron mechanism, not suited to a per-company
/// deadline like this one).
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for ConvertCompanyTrial {
    type Payload = ConvertCompanyTrialPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "ConvertCompanyTrial";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![company_status_query()]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        match company_status(matching_events, &payload.company_id) {
            Some(CompanyStatus::Trialing) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "CompanyActivated".into(),
                    payload: serde_json::json!({ "company_id": payload.company_id }),
                }],
            },
            None => CommandDecision::Rejected {
                reason: format!("company {} does not exist", payload.company_id),
                kind: "company_not_found".into(),
            },
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "company {} is {other:?}, not trialing - nothing to convert",
                    payload.company_id
                ),
                kind: "company_not_trialing".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExpireCompanyTrialPayload {
    pub company_id: String,
}

pub struct ExpireCompanyTrial;

/// `specs/skilj-helpdesk.allium`'s `rule TrialPeriodEnds`'s failure
/// branch: `trialing -> expired`. Same submitter and reasoning as
/// `ConvertCompanyTrial` above - `ScheduleCompanyTrialExpiry` below
/// schedules this one instead, whenever the mocked charge outcome says
/// to.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for ExpireCompanyTrial {
    type Payload = ExpireCompanyTrialPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "ExpireCompanyTrial";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![company_status_query()]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        match company_status(matching_events, &payload.company_id) {
            Some(CompanyStatus::Trialing) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "CompanyExpired".into(),
                    payload: serde_json::json!({ "company_id": payload.company_id }),
                }],
            },
            None => CommandDecision::Rejected {
                reason: format!("company {} does not exist", payload.company_id),
                kind: "company_not_found".into(),
            },
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "company {} is {other:?}, not trialing - nothing to expire",
                    payload.company_id
                ),
                kind: "company_not_trialing".into(),
            },
        }
    }
}

/// `specs/skilj-helpdesk.allium`'s `rule TrialPeriodEnds`, ported off
/// `src/bin/scheduler.rs`'s own hand-rolled polling loop onto skilj
/// 0.0.7's native per-entity deadline (`ScheduleDeadline`/
/// `CancelDeadline`, docs/architecture.md §46) - exactly the gap that
/// binary's own doc comment named before this feature existed ("neither
/// of which skilj's own `system_triggered` scheduling fits ... a
/// per-company deadline like this one").
///
/// The mocked `PaymentGateway.charge` outcome (`scheduling::mock_charge_succeeds`)
/// is decided here, at schedule time (when `CompanySignedUp` commits),
/// rather than at fire time inside `ConvertCompanyTrial::decide()` -
/// `ScheduleDeadline::Target` is one fixed command per schedule, so the
/// two outcomes need two independently-deciding schedules (this one and
/// `ScheduleCompanyTrialExpiry` below) rather than one branching inside
/// `decide()`. Harmless since the mock is deterministic (always `true` -
/// see its own doc comment); a real gateway integration would need the
/// charge attempt moved to fire time instead, inside the target
/// command's own `decide()` - exactly what docs/architecture.md §46
/// means by "whether a deadline is still relevant is a decision
/// `Target::decide()` gets to make against *current* state at fire
/// time, not one baked in back when the timer was scheduled."
///
/// `fire_at` uses `Utc::now()` rather than `CompanySignedUp`'s own
/// commit time, since `schedule()` only ever receives the event's
/// payload, never its metadata (unlike old `scheduler.rs`, which read
/// `metadata.createdAt` off the REST feed directly). Accurate as long
/// as this runs close to when the event actually committed - true for
/// real-time operation (background poll default 500ms) and every test
/// in this suite (each signs a company up and waits on its own next
/// tick) - which is also why `START_FROM` is `Latest`, not the
/// `Beginning` default: backfilling pre-existing `CompanySignedUp`
/// history on a fresh deploy would stamp every older signup with
/// today's date instead of its real one, worse than not scheduling it
/// at all.
///
/// No `CancelDeadline` counterpart, deliberately: a stale pending row
/// fires once and `ConvertCompanyTrial`/`ExpireCompanyTrial` reject it
/// ("not trialing") against a company that converted, expired, or
/// reactivated by some other path before its own deadline came due.
/// Unlike a ticket (see `CancelTicketAutoCloseOnReopen`), a company
/// never returns to `trialing`, so an old deadline can't land on a newer
/// trial.
pub struct ScheduleCompanyTrialConversion;

impl ScheduleDeadline for ScheduleCompanyTrialConversion {
    type Source = CompanySignedUp;
    type Target = ConvertCompanyTrial;
    const NAME: &'static str = "ScheduleCompanyTrialConversion";
    const START_FROM: DeadlinePollStartFrom = DeadlinePollStartFrom::Latest;
    fn schedule(
        source_payload: &CompanySignedUpPayload,
    ) -> Option<DeadlineSpec<ConvertCompanyTrialPayload>> {
        if !scheduling::mock_charge_succeeds() {
            return None;
        }
        Some(DeadlineSpec {
            fire_at: chrono::Utc::now() + scheduling::trial_duration(),
            tags: vec![company_tag_value(&source_payload.company_id)],
            payload: ConvertCompanyTrialPayload {
                company_id: source_payload.company_id.clone(),
            },
        })
    }
}

/// `rule TrialPeriodEnds`'s failure branch - see
/// `ScheduleCompanyTrialConversion`'s own doc comment for why this is a
/// second, independently-deciding schedule rather than a branch inside
/// one. Never actually fires in this showcase (`mock_charge_succeeds`
/// always returns `true`) - kept anyway so `ExpireCompanyTrial` stays
/// reachable the way it always has, both directly
/// (`rest_trigger_allowed`, used by this crate's own tests to force a
/// company into `expired` without waiting on a deadline) and as the
/// intended path once a real gateway integration flips the mock.
pub struct ScheduleCompanyTrialExpiry;

impl ScheduleDeadline for ScheduleCompanyTrialExpiry {
    type Source = CompanySignedUp;
    type Target = ExpireCompanyTrial;
    const NAME: &'static str = "ScheduleCompanyTrialExpiry";
    const START_FROM: DeadlinePollStartFrom = DeadlinePollStartFrom::Latest;
    fn schedule(
        source_payload: &CompanySignedUpPayload,
    ) -> Option<DeadlineSpec<ExpireCompanyTrialPayload>> {
        if scheduling::mock_charge_succeeds() {
            return None;
        }
        Some(DeadlineSpec {
            fire_at: chrono::Utc::now() + scheduling::trial_duration(),
            tags: vec![company_tag_value(&source_payload.company_id)],
            payload: ExpireCompanyTrialPayload {
                company_id: source_payload.company_id.clone(),
            },
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReactivateCompanyPayload {
    pub company_id: String,
}

pub struct ReactivateCompany;

/// `specs/skilj-helpdesk.allium`'s `rule CompanySubscribes`: `expired ->
/// active`. Unlike `ConvertCompanyTrial`/`ExpireCompanyTrial`, this one
/// really is person-submitted (an expired company choosing to pay) -
/// still a mocked `PaymentGateway.charge`, but the caller is a real
/// customer-facing surface, not a deadline reactor. Kept unconditionally
/// successful here (no `charged.succeeded` branch) since a real payment
/// retry-on-failure UX is presentation-level, out of this pass's scope.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for ReactivateCompany {
    type Payload = ReactivateCompanyPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "ReactivateCompany";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![company_status_query()]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        match company_status(matching_events, &payload.company_id) {
            Some(CompanyStatus::Expired) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "CompanyActivated".into(),
                    payload: serde_json::json!({ "company_id": payload.company_id }),
                }],
            },
            None => CommandDecision::Rejected {
                reason: format!("company {} does not exist", payload.company_id),
                kind: "company_not_found".into(),
            },
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "company {} is {other:?}, not expired - nothing to reactivate",
                    payload.company_id
                ),
                kind: "company_not_expired".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TenantDirectoryState {
    /// The tenant bounded context provisioned for this company, if
    /// `RecordCompanyTenant` has committed one. `None` covers both
    /// "hasn't signed up yet" and "signed up, provisioner hasn't reacted
    /// yet" - the same conflation `company_tenant`'s own `Option` makes
    /// on the event-history side, kept here so the two agree on what
    /// absence means.
    pub tenant_name: Option<String>,
}

/// Keyed by `company_id`.
pub struct TenantDirectory;

/// `company_id -> tenant_name`, as a queryable read model - the lookup
/// every piece of per-tenant routing needs, and the reason this pass
/// doesn't need a bespoke "resolve tenant" query bolted onto the API
/// layer.
///
/// **Why a Projection and not a `company_tenant`-style history fold.**
/// `company_tenant` above reads the answer off a command's
/// `matching_events`, which is the right shape for a `decide()` guard
/// but the wrong shape for a router: routing happens *before* any
/// command is dispatched, so there is no `matching_events` to read. A
/// caller would otherwise have to page the shared context's own event
/// history itself, re-implementing tag filtering and sequence ordering
/// that skilj already does correctly. This projection is that same
/// answer, maintained incrementally and reachable through the existing
/// `projection(boundedContext, name, key)` GraphQL query.
///
/// **`sync()`, deliberately.** Every other projection in this file is
/// `sync()` and so is this one, which for this particular projection is
/// a correctness requirement rather than a latency preference: a router
/// that reads a `stale` (or absent) entry for a company whose tenant was
/// provisioned seconds ago would route that company's first real command
/// to the shared context while a later one went to the tenant - splitting
/// one company's ticket history across two bounded contexts with nothing
/// recording that it happened. `sync()` bounds that window to "not yet
/// committed", which `RecordTenantLifecycle`'s own guard downstream is
/// built to tolerate. Async would buy nothing back either (issue #12): it
/// folds only `CompanyTenantProvisioned`, once per company, so it adds
/// nothing to an ordinary command's commit.
///
/// **Not an access-control boundary.** It carries no `OWNER_TAG_KEY`,
/// deliberately: this is an infrastructure mapping, and unlike
/// `CompanyActiveTickets`/`TicketSummary` there is no company data in it to
/// protect - it says which bounded context holds a company, not anything
/// about the company. It is readable by anyone with an Admin mapping on
/// the shared `helpdesk` context, which is the same audience that can
/// already call `listBoundedContexts` and mint tokens against any
/// context (`createCommandToken`'s own resolver requires exactly that
/// mapping), so gating it more tightly would add a check without closing
/// a real path.
#[auto_register(BOUNDED_CONTEXT)]
impl Projection for TenantDirectory {
    type State = TenantDirectoryState;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "TenantDirectory";
    fn consumed_event_types() -> Vec<&'static str> {
        vec!["CompanyTenantProvisioned"]
    }
    fn sync() -> bool {
        true
    }
    fn keys(event: &Self::Event) -> Vec<String> {
        match event {
            HelpdeskEvent::CompanyTenantProvisioned(p) => vec![p.company_id.clone()],
            _ => vec![],
        }
    }
    fn project(state: &mut Self::State, event: &Self::Event, _key: &str) {
        if let HelpdeskEvent::CompanyTenantProvisioned(p) = event {
            state.tenant_name = Some(p.tenant_name.clone());
        }
    }
}

#[cfg(test)]
mod sign_up_validation_tests {
    use super::*;

    fn decide(company_id: &str, name: &str, contact_email: &str) -> CommandDecision {
        SignUpCompany::decide(
            &SignUpCompanyPayload {
                company_id: company_id.into(),
                name: name.into(),
                contact_email: contact_email.into(),
            },
            &[],
        )
    }

    fn rejection_kind(decision: CommandDecision) -> Option<String> {
        match decision {
            CommandDecision::Rejected { kind, .. } => Some(kind),
            _ => None,
        }
    }

    #[test]
    fn a_well_formed_sign_up_is_not_rejected() {
        assert_eq!(
            rejection_kind(decide("acme", "Acme", "a@acme.example")),
            None
        );
    }

    #[test]
    fn malformed_contact_emails_are_rejected() {
        for email in [
            "",
            "acme.example",
            "@acme.example",
            "a@",
            "a@localhost",
            "a@b@acme.example",
            "a@.acme",
            "a@acme.",
            "a b@acme.example",
        ] {
            assert_eq!(
                rejection_kind(decide("acme", "Acme", email)).as_deref(),
                Some("invalid_email"),
                "{email:?} should be rejected"
            );
        }
    }

    #[test]
    fn empty_ids_and_names_are_rejected() {
        assert_eq!(
            rejection_kind(decide("", "Acme", "a@acme.example")).as_deref(),
            Some("empty_company_id")
        );
        assert_eq!(
            rejection_kind(decide("acme", "  ", "a@acme.example")).as_deref(),
            Some("empty_company_name")
        );
    }
}
