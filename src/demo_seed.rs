//! Pure decision logic behind `src/bin/server.rs`'s optional demo-traffic
//! loop (`SEED_DEMO_TRAFFIC=1`) - the same pure-logic/binary split
//! `alerting.rs`/`src/bin/alerter.rs` and `scheduling.rs`/
//! `src/bin/scheduler.rs` already use: this module decides *what* fake
//! REST call to make next and how the crate's own tracked idea of each
//! fake ticket's status should change once the real server has answered;
//! `server.rs` is the only place that actually performs any I/O.
//!
//! The point of this whole module is cosmetic - giving a freshly booted
//! demo something moving to look at in a dashboard without a person
//! driving curl by hand - so it deliberately doesn't reach for a real
//! source of randomness. `rand` isn't a dependency anywhere in this
//! workspace (see `src/bin/alerter.rs`'s own "deliberately minimal - no
//! config-loading crate" preference for the same reasoning applied
//! elsewhere), so [`Rng`] is a tiny splitmix64-style PRNG instead - not
//! cryptographically anything, just enough spread to avoid every run
//! looking identical.

use crate::helpdesk::TicketPriority;

// --- a tiny, dependency-free PRNG ---

/// splitmix64 - deterministic given a seed, which is what makes this
/// module's own tests reproducible without needing a fixed external
/// dependency's exact algorithm to match against.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn seeded(seed: u64) -> Self {
        Rng(seed)
    }

    /// Seeds from the wall clock - `server.rs`'s own real entry point
    /// into this module; every other constructor here is for tests.
    pub fn from_clock() -> Self {
        Self::from_clock_and_worker(0)
    }

    /// Same as [`Self::from_clock`], additionally mixed with a worker
    /// index - `server.rs`'s own `SEED_DEMO_CONCURRENCY` spawns several
    /// workers within the same tick of `main`, so seeding every one from
    /// the clock alone would give them near-identical PRNG state (same
    /// choices, in lockstep) - defeating the point of concurrency being
    /// *varied* load, not `concurrency` copies of one stream.
    pub fn from_clock_and_worker(worker_index: usize) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        let worker_mix = (worker_index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        Rng(nanos ^ 0x9E37_79B9_7F4A_7C15 ^ worker_mix)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n` - panics on `n == 0`, same as `%` would.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// `true` with probability `numerator / denominator`.
    pub fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        self.next_u64() % denominator < numerator
    }
}

// --- fake companies/customers/staff - see server.rs's own doc comment
// for why these are a distinct cast from the README's own `acme` ---

pub const DEMO_COMPANIES: &[&str] = &["wonka-industries", "stark-labs", "hooli"];

const CUSTOMER_HANDLES: &[&str] = &[
    "seed-customer-1",
    "seed-customer-2",
    "seed-customer-3",
    "seed-customer-4",
];
const STAFF_HANDLES: &[&str] = &["seed-staff-1", "seed-staff-2", "seed-staff-3"];

const TITLES: &[&str] = &[
    "Can't log in after password reset",
    "Invoice shows the wrong plan",
    "Export button does nothing",
    "Dashboard chart is empty",
    "Webhook stopped firing",
    "Feature request: dark mode",
    "Billing address needs updating",
    "API returns 500 on large payloads",
];

const REQUEST_INFO_MESSAGES: &[&str] = &[
    "Could you share a screenshot?",
    "Which browser and version are you on?",
    "Can you paste the exact error message?",
    "Does this happen every time, or only sometimes?",
];

const CUSTOMER_REPLY_MESSAGES: &[&str] = &[
    "Here you go - let me know if you need anything else.",
    "Sure, attached below.",
    "Still happening, same as before.",
    "Thanks for looking into this!",
];

fn random_priority(rng: &mut Rng) -> TicketPriority {
    // ~12% urgent - enough that `alerter` (which only ever pages on
    // `rule UrgentTicketNeedsImmediateAttention`, see `alerting.rs`'s
    // own doc comment) has something to page on somewhat regularly,
    // without every other ticket being a false emergency.
    match rng.below(100) {
        0..=11 => TicketPriority::Urgent,
        12..=34 => TicketPriority::High,
        35..=69 => TicketPriority::Medium,
        _ => TicketPriority::Low,
    }
}

// --- tracked state ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedTicketStatus {
    Open,
    InProgress,
    WaitingOnCustomer,
    Resolved,
}

#[derive(Debug, Clone)]
pub struct SeedTicket {
    pub ticket_id: String,
    pub company_id: String,
    pub requester_id: String,
    pub staff_id: Option<String>,
    pub status: SeedTicketStatus,
}

/// This module's own idea of what's out there - deliberately separate
/// from the real projections `helpdesk.rs` builds: the whole point is a
/// cheap, in-memory tracker so the loop knows which REST call is legal
/// to try next, not another consumer of skilj's own read side.
pub struct SeedState {
    companies: Vec<String>,
    ticket_id_prefix: String,
    tickets: Vec<SeedTicket>,
    next_seq: u64,
}

impl SeedState {
    /// `ticket_id_prefix` exists for exactly one reason: `server.rs`'s
    /// own `SEED_DEMO_CONCURRENCY` runs several of these loops
    /// concurrently (see that file's own doc comment) - each with its
    /// own independent `SeedState`, deliberately not shared, so no
    /// locking is needed anywhere. Without a distinct prefix per worker,
    /// two workers' own `next_seq` counters would both mint
    /// `seed-ticket-0` and race on ticket_id - a real ticket_id is
    /// globally unique (`CreateTicket`'s own `ticket_already_exists`
    /// guard in `helpdesk.rs`), so the loser's `CreateTicket` would be
    /// rejected forever, and since a worker only ever advances tickets
    /// *it* successfully created (see `apply_outcome` below), a
    /// permanently-empty, permanently-colliding worker would spin doing
    /// nothing else - a real bug caught by actually reasoning through
    /// concurrent workers before shipping this, not observed by running
    /// it.
    pub fn new(companies: Vec<String>, ticket_id_prefix: impl Into<String>) -> Self {
        Self {
            companies,
            ticket_id_prefix: ticket_id_prefix.into(),
            tickets: Vec::new(),
            next_seq: 0,
        }
    }

    pub fn tickets(&self) -> &[SeedTicket] {
        &self.tickets
    }

    fn next_ticket_id(&self) -> String {
        format!("{}-{}", self.ticket_id_prefix, self.next_seq)
    }

    fn ticket_mut(&mut self, ticket_id: &str) -> Option<&mut SeedTicket> {
        self.tickets.iter_mut().find(|t| t.ticket_id == ticket_id)
    }
}

// --- actions ---

#[derive(Debug, Clone, PartialEq)]
pub enum SeedAction {
    CreateTicket {
        ticket_id: String,
        company_id: String,
        requester_id: String,
        title: String,
        description: String,
        priority: TicketPriority,
    },
    AssignTicket {
        ticket_id: String,
        staff_id: String,
    },
    ResolveTicket {
        ticket_id: String,
    },
    RequestInfo {
        ticket_id: String,
        staff_id: String,
        message: String,
    },
    CustomerResponds {
        ticket_id: String,
        requester_id: String,
        message: String,
    },
    ReopenTicket {
        ticket_id: String,
    },
}

/// Picks the next fake REST call to make. Never mutates `state` itself -
/// [`apply_outcome`] does that, and only once the real server has said
/// whether the call was actually accepted (see that function's own doc
/// comment for why this split matters).
///
/// Roughly 1 in 8 times a ticket is picked to advance, this
/// deliberately picks an action that doesn't match its tracked status
/// (e.g. assigning an already-assigned ticket) - a real, already-tested
/// rejection path (`tests/ticket_create_assign_resolve.rs`'s own
/// `assigning_an_already_assigned_ticket_is_rejected`-shaped case), on
/// purpose: without this, `skilj.commands.processed{outcome="rejected"}`
/// would sit at zero forever, which makes for a much less convincing
/// "error rate" panel than a real deployment - where rejections are
/// business-as-usual - would ever show.
pub fn next_action(state: &SeedState, rng: &mut Rng) -> SeedAction {
    if state.tickets.is_empty() || rng.chance(2, 5) {
        let company_id = state.companies[rng.below(state.companies.len())].clone();
        let ticket_id = state.next_ticket_id();
        return SeedAction::CreateTicket {
            ticket_id,
            company_id,
            requester_id: CUSTOMER_HANDLES[rng.below(CUSTOMER_HANDLES.len())].to_string(),
            title: TITLES[rng.below(TITLES.len())].to_string(),
            description: "filed by skilj-helpdesk's own demo traffic generator".to_string(),
            priority: random_priority(rng),
        };
    }

    let ticket = &state.tickets[rng.below(state.tickets.len())];
    let ticket_id = ticket.ticket_id.clone();
    let staff_id = STAFF_HANDLES[rng.below(STAFF_HANDLES.len())].to_string();
    let provoke_rejection = rng.chance(1, 8);

    match (ticket.status, provoke_rejection) {
        (SeedTicketStatus::Open, false) => SeedAction::AssignTicket { ticket_id, staff_id },
        (SeedTicketStatus::Open, true) => SeedAction::ResolveTicket { ticket_id },

        (SeedTicketStatus::InProgress, false) if rng.chance(2, 3) => {
            SeedAction::ResolveTicket { ticket_id }
        }
        (SeedTicketStatus::InProgress, false) => SeedAction::RequestInfo {
            ticket_id,
            staff_id,
            message: REQUEST_INFO_MESSAGES[rng.below(REQUEST_INFO_MESSAGES.len())].to_string(),
        },
        (SeedTicketStatus::InProgress, true) => SeedAction::AssignTicket { ticket_id, staff_id },

        (SeedTicketStatus::WaitingOnCustomer, false) => SeedAction::CustomerResponds {
            requester_id: ticket.requester_id.clone(),
            ticket_id,
            message: CUSTOMER_REPLY_MESSAGES[rng.below(CUSTOMER_REPLY_MESSAGES.len())].to_string(),
        },
        (SeedTicketStatus::WaitingOnCustomer, true) => SeedAction::ResolveTicket { ticket_id },

        (SeedTicketStatus::Resolved, false) if rng.chance(1, 5) => {
            SeedAction::ReopenTicket { ticket_id }
        }
        (SeedTicketStatus::Resolved, false) => SeedAction::CreateTicket {
            ticket_id: state.next_ticket_id(),
            company_id: state.companies[rng.below(state.companies.len())].clone(),
            requester_id: CUSTOMER_HANDLES[rng.below(CUSTOMER_HANDLES.len())].to_string(),
            title: TITLES[rng.below(TITLES.len())].to_string(),
            description: "filed by skilj-helpdesk's own demo traffic generator".to_string(),
            priority: random_priority(rng),
        },
        (SeedTicketStatus::Resolved, true) => SeedAction::AssignTicket { ticket_id, staff_id },
    }
}

/// Updates `state` to match what the real server just did - called with
/// the `accepted` flag straight off the REST response's own
/// `CommandTriggerResponse.accepted` field
/// (`tests/support/mod.rs`'s own `accepted()` reads the identical
/// field). A rejected call is always a no-op here: the real server's
/// own state didn't change either, so neither should this tracker's -
/// this is exactly what keeps [`next_action`]'s deliberate rejections
/// (above) from desyncing the tracker from reality.
pub fn apply_outcome(state: &mut SeedState, action: &SeedAction, accepted: bool) {
    if !accepted {
        return;
    }
    match action {
        SeedAction::CreateTicket {
            ticket_id,
            company_id,
            requester_id,
            ..
        } => {
            state.next_seq += 1;
            state.tickets.push(SeedTicket {
                ticket_id: ticket_id.clone(),
                company_id: company_id.clone(),
                requester_id: requester_id.clone(),
                staff_id: None,
                status: SeedTicketStatus::Open,
            });
        }
        SeedAction::AssignTicket { ticket_id, staff_id } => {
            if let Some(t) = state.ticket_mut(ticket_id) {
                t.status = SeedTicketStatus::InProgress;
                t.staff_id = Some(staff_id.clone());
            }
        }
        SeedAction::ResolveTicket { ticket_id } => {
            if let Some(t) = state.ticket_mut(ticket_id) {
                t.status = SeedTicketStatus::Resolved;
            }
        }
        SeedAction::RequestInfo { ticket_id, .. } => {
            if let Some(t) = state.ticket_mut(ticket_id) {
                t.status = SeedTicketStatus::WaitingOnCustomer;
            }
        }
        SeedAction::CustomerResponds { ticket_id, .. } => {
            if let Some(t) = state.ticket_mut(ticket_id) {
                t.status = SeedTicketStatus::InProgress;
            }
        }
        SeedAction::ReopenTicket { ticket_id } => {
            if let Some(t) = state.ticket_mut(ticket_id) {
                t.status = SeedTicketStatus::InProgress;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic_given_a_seed() {
        let mut a = Rng::seeded(42);
        let mut b = Rng::seeded(42);
        for _ in 0..50 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn below_never_reaches_n() {
        let mut rng = Rng::seeded(7);
        for _ in 0..500 {
            assert!(rng.below(4) < 4);
        }
    }

    #[test]
    fn chance_zero_of_n_never_fires() {
        let mut rng = Rng::seeded(123);
        for _ in 0..200 {
            assert!(!rng.chance(0, 10));
        }
    }

    #[test]
    fn chance_all_of_n_always_fires() {
        let mut rng = Rng::seeded(123);
        for _ in 0..200 {
            assert!(rng.chance(10, 10));
        }
    }

    #[test]
    fn two_workers_never_mint_the_same_ticket_id() {
        // The regression this guards: before SeedState carried its own
        // ticket_id_prefix, two independent workers both starting at
        // next_seq=0 would both propose "seed-ticket-0" for their first
        // CreateTicket - see SeedState::new's own doc comment.
        let worker_a = SeedState::new(vec!["acme".into()], "seed-ticket-w0");
        let worker_b = SeedState::new(vec!["acme".into()], "seed-ticket-w1");
        assert_ne!(worker_a.next_ticket_id(), worker_b.next_ticket_id());
    }

    #[test]
    fn first_action_on_empty_state_always_creates_a_ticket() {
        let state = SeedState::new(vec!["acme".into()], "t");
        for seed in 0..20 {
            let mut rng = Rng::seeded(seed);
            assert!(matches!(
                next_action(&state, &mut rng),
                SeedAction::CreateTicket { .. }
            ));
        }
    }

    #[test]
    fn accepted_create_ticket_is_tracked_as_open() {
        let mut state = SeedState::new(vec!["acme".into()], "t");
        let action = SeedAction::CreateTicket {
            ticket_id: "t1".into(),
            company_id: "acme".into(),
            requester_id: "cust".into(),
            title: "title".into(),
            description: "desc".into(),
            priority: TicketPriority::Low,
        };
        apply_outcome(&mut state, &action, true);
        assert_eq!(state.tickets().len(), 1);
        assert_eq!(state.tickets()[0].status, SeedTicketStatus::Open);
        assert_eq!(state.tickets()[0].staff_id, None);
    }

    #[test]
    fn rejected_action_never_changes_tracked_state() {
        let mut state = SeedState::new(vec!["acme".into()], "t");
        apply_outcome(
            &mut state,
            &SeedAction::CreateTicket {
                ticket_id: "t1".into(),
                company_id: "acme".into(),
                requester_id: "cust".into(),
                title: "title".into(),
                description: "desc".into(),
                priority: TicketPriority::Low,
            },
            true,
        );
        let before = state.tickets()[0].status;
        apply_outcome(
            &mut state,
            &SeedAction::AssignTicket {
                ticket_id: "t1".into(),
                staff_id: "staff".into(),
            },
            false,
        );
        assert_eq!(state.tickets()[0].status, before);
        assert_eq!(state.tickets()[0].staff_id, None);
    }

    #[test]
    fn full_lifecycle_tracks_correctly_through_every_step() {
        let mut state = SeedState::new(vec!["acme".into()], "t");
        apply_outcome(
            &mut state,
            &SeedAction::CreateTicket {
                ticket_id: "t1".into(),
                company_id: "acme".into(),
                requester_id: "cust".into(),
                title: "title".into(),
                description: "desc".into(),
                priority: TicketPriority::Low,
            },
            true,
        );
        apply_outcome(
            &mut state,
            &SeedAction::AssignTicket {
                ticket_id: "t1".into(),
                staff_id: "staff".into(),
            },
            true,
        );
        assert_eq!(state.tickets()[0].status, SeedTicketStatus::InProgress);

        apply_outcome(
            &mut state,
            &SeedAction::RequestInfo {
                ticket_id: "t1".into(),
                staff_id: "staff".into(),
                message: "?".into(),
            },
            true,
        );
        assert_eq!(state.tickets()[0].status, SeedTicketStatus::WaitingOnCustomer);

        apply_outcome(
            &mut state,
            &SeedAction::CustomerResponds {
                ticket_id: "t1".into(),
                requester_id: "cust".into(),
                message: "!".into(),
            },
            true,
        );
        assert_eq!(state.tickets()[0].status, SeedTicketStatus::InProgress);

        apply_outcome(
            &mut state,
            &SeedAction::ResolveTicket {
                ticket_id: "t1".into(),
            },
            true,
        );
        assert_eq!(state.tickets()[0].status, SeedTicketStatus::Resolved);

        apply_outcome(
            &mut state,
            &SeedAction::ReopenTicket {
                ticket_id: "t1".into(),
            },
            true,
        );
        assert_eq!(state.tickets()[0].status, SeedTicketStatus::InProgress);
    }
}
