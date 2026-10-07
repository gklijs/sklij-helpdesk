//! Watching async projection lag.
//!
//! An async projection (`sync() == false`, `CompanyActiveTickets` here)
//! is folded by skilj's background catch-up, not in the command's own
//! commit, so it trails the bounded context's latest event. A read that
//! needs its own write waits for it with `waitForSequence`; everything
//! else sees state that is this far behind.
//!
//! skilj records no metric for this, so [`sample`] measures, per
//! bounded context and async projection, the latest committed sequence
//! minus the projection's `caught_up_to`, and `src/bin/server.rs` records
//! it as the `skilj_helpdesk.projection_lag` gauge. For a partitioned
//! projection `caught_up_to` is already its slowest partition's position.
//! Sync projections are left out: they are folded in the command's own
//! commit, so their lag is 0 by construction.

use skilj_core::db::{self, Pool};
use std::collections::BTreeMap;

/// One gauge series: one async projection in one bounded context.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SeriesKey {
    pub bounded_context: String,
    pub projection: String,
}

/// How many sequences `caught_up_to` is behind `latest`. `None` for
/// either means no events yet, or nothing folded yet: every event up to
/// `latest` is still to be folded.
pub fn lag(latest: Option<i64>, caught_up_to: Option<i64>) -> u64 {
    let Some(latest) = latest else {
        return 0;
    };
    // Sequences start at 1, so "nothing folded" is the same as 0.
    let caught_up_to = caught_up_to.unwrap_or(0);
    latest.saturating_sub(caught_up_to).max(0) as u64
}

/// The lag of each async projection in `bounded_context`. No query for
/// the latest sequence when the context has no async projection.
pub async fn lag_in(
    pool: &Pool,
    bounded_context: &str,
) -> skilj_core::error::Result<BTreeMap<SeriesKey, u64>> {
    let projections = db::list_projections_for_bounded_context(pool, bounded_context).await?;
    let mut lags = BTreeMap::new();
    if projections.iter().all(|p| p.sync) {
        return Ok(lags);
    }
    // Read after the projections, so a projection can only look further
    // behind than it is, never ahead.
    let latest = db::latest_sequence(pool, bounded_context).await?;
    for projection in projections.into_iter().filter(|p| !p.sync) {
        lags.insert(
            SeriesKey {
                bounded_context: bounded_context.to_string(),
                projection: projection.name,
            },
            lag(latest, projection.caught_up_to),
        );
    }
    Ok(lags)
}

/// Async projection lag across every bounded context, shared and tenant
/// alike: each tenant context has its own `CompanyActiveTickets`.
///
/// A context that can't be read is skipped with a warning, as in
/// `parked_deliveries::sample`.
pub async fn sample(pool: &Pool) -> skilj_core::error::Result<BTreeMap<SeriesKey, u64>> {
    let mut lags = BTreeMap::new();
    for bounded_context in db::list_bounded_contexts(pool).await? {
        match lag_in(pool, &bounded_context.name).await {
            Ok(found) => lags.extend(found),
            Err(e) => tracing::warn!(
                bounded_context = %bounded_context.name,
                error = %e,
                "projection lag: couldn't read this context, skipping it"
            ),
        }
    }
    Ok(lags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caught_up_is_zero() {
        assert_eq!(lag(Some(42), Some(42)), 0);
    }

    #[test]
    fn behind_is_the_gap_in_sequences() {
        assert_eq!(lag(Some(42), Some(30)), 12);
    }

    #[test]
    fn nothing_folded_yet_is_every_event() {
        assert_eq!(lag(Some(42), None), 42);
    }

    #[test]
    fn no_events_is_zero() {
        assert_eq!(lag(None, None), 0);
    }

    #[test]
    fn a_position_past_the_read_head_is_zero_not_negative() {
        assert_eq!(lag(Some(40), Some(42)), 0);
    }
}
