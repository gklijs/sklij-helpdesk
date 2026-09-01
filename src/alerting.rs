//! The alerting decision logic - the piece of `specs/skilj-helpdesk.allium`
//! this crate's own design note (in the spec file) resolved as needing
//! no change to skilj itself: a separately-deployable consumer of
//! skilj's own REST event feed (`GET /v1/events/consume`, an
//! `EventReadToken` per event type - docs/architecture.md §7.4). See
//! `src/bin/alerter.rs` for the actual polling loop that calls this;
//! this module holds only the pure decision, so it's testable without
//! any HTTP or Postgres involved at all.
//!
//! Covers `rule UrgentTicketNeedsImmediateAttention` in full - it's an
//! entity-creation trigger (`when: ticket: Ticket.created`), not a
//! temporal one, so nothing about it needs scheduling.
//!
//! `rule TicketBecomesOverdue` (an unhandled ticket, 4+ hours old) is
//! deliberately not built out here beyond `is_overdue` below: the
//! trigger check itself is a one-line comparison (shown, and tested),
//! but firing it for real needs the alerter to track each ticket's own
//! `created_at` and current status as it consumes the event stream, and
//! sweep that tracked set on a timer - straightforward with the same
//! technique `TicketSummary`'s own fold already uses, just not wired up
//! this pass (see `Cargo.toml`'s own doc comment).

use crate::helpdesk::{TicketCreatedPayload, TicketPriority};
use chrono::{DateTime, Duration, Utc};

/// One ticket worth paging a lead about, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub ticket_id: String,
    pub company_id: String,
    pub reason: AlertReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertReason {
    /// `rule UrgentTicketNeedsImmediateAttention`.
    Urgent,
    /// `rule TicketBecomesOverdue` - see this module's own doc comment
    /// for what's still needed to actually fire this in a running
    /// alerter, beyond the check itself.
    Overdue,
}

/// `rule UrgentTicketNeedsImmediateAttention`, in full: a ticket alerts
/// immediately, at creation, if and only if its priority is `urgent`.
pub fn evaluate_ticket_created(payload: &TicketCreatedPayload) -> Option<Alert> {
    (payload.priority == TicketPriority::Urgent).then(|| Alert {
        ticket_id: payload.ticket_id.clone(),
        company_id: payload.company_id.clone(),
        reason: AlertReason::Urgent,
    })
}

/// `rule TicketBecomesOverdue`'s own trigger condition:
/// `Ticket.created_at + config.unhandled_alert_after <= now`. A running
/// alerter would call this once per still-unhandled ticket it's
/// tracking, each sweep.
pub fn is_overdue(created_at: DateTime<Utc>, now: DateTime<Utc>, threshold: Duration) -> bool {
    created_at + threshold <= now
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(priority: TicketPriority) -> TicketCreatedPayload {
        TicketCreatedPayload {
            ticket_id: "ticket_1".into(),
            company_id: "company_1".into(),
            requester_id: "customer_1".into(),
            logged_by_staff_id: None,
            title: "t".into(),
            description: "d".into(),
            priority,
        }
    }

    #[test]
    fn urgent_ticket_creation_alerts() {
        let alert = evaluate_ticket_created(&payload(TicketPriority::Urgent));
        assert_eq!(
            alert,
            Some(Alert {
                ticket_id: "ticket_1".into(),
                company_id: "company_1".into(),
                reason: AlertReason::Urgent,
            })
        );
    }

    #[test]
    fn non_urgent_ticket_creation_does_not_alert() {
        for priority in [TicketPriority::Low, TicketPriority::Medium, TicketPriority::High] {
            assert_eq!(evaluate_ticket_created(&payload(priority)), None);
        }
    }

    #[test]
    fn overdue_threshold_is_inclusive_at_the_boundary() {
        let created_at = Utc::now() - Duration::hours(4);
        let threshold = Duration::hours(4);
        assert!(is_overdue(created_at, Utc::now(), threshold));
    }

    #[test]
    fn not_yet_overdue_before_the_threshold() {
        let created_at = Utc::now() - Duration::hours(3);
        let threshold = Duration::hours(4);
        assert!(!is_overdue(created_at, Utc::now(), threshold));
    }
}
