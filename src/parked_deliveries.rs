//! Watching parked deliveries.
//!
//! Three `CrossContextRoute`s (`marketing.rs`) and three
//! `ScheduleDeadline`s (`helpdesk.rs`) submit commands from skilj's own
//! background loops. When one of those submissions keeps *failing* - an
//! error, not a rejection, which skilj treats as a legitimate answer - it
//! is retried with backoff and then parked in the target bounded
//! context's `parked_deliveries` table, and the loop moves on. Nothing
//! happens to it after that until an operator calls
//! `retryParkedDelivery` or `discardParkedDelivery`, so a park that no one
//! notices is a lost trial-lapse record or a ticket that never
//! auto-closes.
//!
//! skilj records no metric for this, so [`sample`] counts the rows per
//! bounded context and `src/bin/server.rs` records the result as the
//! `skilj_helpdesk.parked_deliveries` gauge, which the Grafana dashboard
//! alerts on. `tests/parked_deliveries.rs` forces a real park and redrives
//! it.

use skilj_core::db::{self, ParkedDeliveryCursor, ParkedDeliveryKind, Pool};
use std::collections::BTreeMap;

/// One gauge series: everything parked in `bounded_context` by one
/// `source` (`skilj`'s own `"<prefix><route or schedule name>"`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SeriesKey {
    pub bounded_context: String,
    pub kind: &'static str,
    pub source: String,
}

/// Rows read per query. Parked rows carry their full request payload, so
/// this bounds memory per round trip rather than per sample.
const PAGE_SIZE: i64 = 500;

pub fn kind_label(kind: ParkedDeliveryKind) -> &'static str {
    match kind {
        ParkedDeliveryKind::CrossContextRoute => "cross_context_route",
        ParkedDeliveryKind::ExternalEvent => "external_event",
        ParkedDeliveryKind::CommandTrigger => "command_trigger",
        ParkedDeliveryKind::Deadline => "deadline",
    }
}

/// Parked deliveries in `bounded_context`, counted by kind and source.
pub async fn count_in(
    pool: &Pool,
    bounded_context: &str,
) -> skilj_core::error::Result<BTreeMap<SeriesKey, u64>> {
    let mut counts = BTreeMap::new();
    let mut after: Option<ParkedDeliveryCursor> = None;
    loop {
        let page =
            db::list_parked_deliveries_page(pool, bounded_context, after.as_ref(), PAGE_SIZE)
                .await?;
        for delivery in &page {
            let key = SeriesKey {
                bounded_context: bounded_context.to_string(),
                kind: kind_label(delivery.kind),
                source: delivery.source.clone(),
            };
            *counts.entry(key).or_insert(0) += 1;
        }
        match page.last() {
            Some(last) if page.len() as i64 == PAGE_SIZE => {
                after = Some(ParkedDeliveryCursor::of(last))
            }
            _ => return Ok(counts),
        }
    }
}

/// Parked deliveries across every bounded context, shared and tenant
/// alike. Tenant contexts are where ticket auto-close deadlines park, so
/// listing a fixed set of names would miss most of them.
///
/// A context that can't be read is skipped with a warning rather than
/// failing the whole sample: one broken schema shouldn't blank the gauge
/// for every other context.
pub async fn sample(pool: &Pool) -> skilj_core::error::Result<BTreeMap<SeriesKey, u64>> {
    let mut counts = BTreeMap::new();
    for bounded_context in db::list_bounded_contexts(pool).await? {
        match count_in(pool, &bounded_context.name).await {
            Ok(found) => counts.extend(found),
            Err(e) => tracing::warn!(
                bounded_context = %bounded_context.name,
                error = %e,
                "parked deliveries: couldn't count this context, skipping it"
            ),
        }
    }
    Ok(counts)
}

/// What to record this tick: every series in `current`, plus a zero for
/// each series reported last tick that has no rows any more. Without the
/// zero, a gauge keeps exporting its last value, so a redriven delivery
/// would stay "parked" on the dashboard until the process restarted.
pub fn with_cleared(
    previous: &BTreeMap<SeriesKey, u64>,
    current: &BTreeMap<SeriesKey, u64>,
) -> Vec<(SeriesKey, u64)> {
    let cleared = previous
        .keys()
        .filter(|key| !current.contains_key(*key))
        .map(|key| (key.clone(), 0));
    current
        .iter()
        .map(|(key, count)| (key.clone(), *count))
        .chain(cleared)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(source: &str) -> SeriesKey {
        SeriesKey {
            bounded_context: "marketing".to_string(),
            kind: "cross_context_route",
            source: source.to_string(),
        }
    }

    #[test]
    fn a_series_that_emptied_is_recorded_as_zero() {
        let previous = BTreeMap::from([(key("a"), 2), (key("b"), 1)]);
        let current = BTreeMap::from([(key("b"), 3)]);
        let mut recorded = with_cleared(&previous, &current);
        recorded.sort();
        assert_eq!(recorded, vec![(key("a"), 0), (key("b"), 3)]);
    }

    #[test]
    fn nothing_parked_and_nothing_before_records_nothing() {
        assert!(with_cleared(&BTreeMap::new(), &BTreeMap::new()).is_empty());
    }
}
