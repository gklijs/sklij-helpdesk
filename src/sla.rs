//! SLA policies (issue #42): first-response and resolution targets per
//! ticket priority, counted in business hours - `specs/skilj-helpdesk.allium`'s
//! `rule TicketBreachesSla`. The types here are what a company configures
//! (`SetCompanySlaPolicy`) and what a ticket carries from creation
//! (`TicketCreatedPayload::sla`); the one piece of real arithmetic,
//! `add_business_minutes`, is pure and unit-tested below, the same split
//! `activity_scheduling.rs` makes. The deadlines themselves are skilj's
//! native `ScheduleDeadline`/`CancelDeadline` - see
//! `ScheduleFirstResponseSla` in `src/helpdesk/`.
//!
//! **Business hours are UTC.** A company's window is a UTC hour range on
//! a set of ISO weekdays, not a local time in a named timezone. That keeps
//! `add_business_minutes` a function of its arguments alone (no tz
//! database, no DST gaps or repeated hours to define behaviour for), at
//! the cost of a company outside UTC entering its hours shifted - and,
//! across a DST change, being an hour off for half the year. Good enough
//! for a showcase; a real product would store an IANA zone and convert
//! each day's window through `chrono-tz` inside the same loop.

use chrono::{DateTime, Datelike, Duration, NaiveTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::helpdesk::TicketPriority;

/// The longest target a policy may set: one calendar year of minutes. A
/// bound, so `add_business_minutes` can never walk an absurd number of
/// days, not a business rule anyone is expected to approach.
pub const MAX_TARGET_MINUTES: u32 = 365 * 24 * 60;

/// When the SLA clock runs: from `open_hour` up to (not including)
/// `close_hour`, UTC, on each of `working_days`. See this module's doc
/// comment for why UTC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BusinessHours {
    /// 0..=23.
    pub open_hour: u8,
    /// 1..=24, after `open_hour`; 24 means midnight at the end of the day.
    pub close_hour: u8,
    /// ISO weekday numbers: 1 = Monday ... 7 = Sunday.
    pub working_days: Vec<u8>,
}

impl Default for BusinessHours {
    /// 09:00-17:00 UTC, Monday to Friday.
    fn default() -> Self {
        Self {
            open_hour: 9,
            close_hour: 17,
            working_days: vec![1, 2, 3, 4, 5],
        }
    }
}

impl BusinessHours {
    /// Why these hours can't be used, if they can't.
    pub fn validate(&self) -> Result<(), String> {
        if self.open_hour > 23 || self.close_hour > 24 || self.open_hour >= self.close_hour {
            return Err(format!(
                "business hours {}-{} are not a range within one day (0 <= open < close <= 24)",
                self.open_hour, self.close_hour
            ));
        }
        if self.working_days.is_empty() {
            return Err("business hours need at least one working day".into());
        }
        let mut seen = [false; 8];
        for &day in &self.working_days {
            if !(1..=7).contains(&day) {
                return Err(format!("working day {day} is not an ISO weekday (1-7)"));
            }
            if std::mem::replace(&mut seen[day as usize], true) {
                return Err(format!("working day {day} is listed twice"));
            }
        }
        Ok(())
    }

    fn is_working_day(&self, date: chrono::NaiveDate) -> bool {
        let iso = date.weekday().number_from_monday();
        self.working_days.iter().any(|&day| u32::from(day) == iso)
    }
}

/// One priority's targets, in business minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SlaTarget {
    pub first_response_minutes: u32,
    pub resolution_minutes: u32,
}

impl SlaTarget {
    fn validate(&self, priority: &str) -> Result<(), String> {
        if self.first_response_minutes > MAX_TARGET_MINUTES
            || self.resolution_minutes > MAX_TARGET_MINUTES
        {
            return Err(format!(
                "{priority} targets exceed {MAX_TARGET_MINUTES} minutes"
            ));
        }
        if self.first_response_minutes > self.resolution_minutes {
            return Err(format!(
                "{priority} first response target is later than its resolution target"
            ));
        }
        Ok(())
    }
}

/// A company's SLA policy - `SetCompanySlaPolicy` replaces it whole.
/// A company that never set one gets `SlaPolicy::default()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SlaPolicy {
    pub business_hours: BusinessHours,
    pub low: SlaTarget,
    pub medium: SlaTarget,
    pub high: SlaTarget,
    pub urgent: SlaTarget,
}

const fn target(first_response_hours: u32, resolution_hours: u32) -> SlaTarget {
    SlaTarget {
        first_response_minutes: first_response_hours * 60,
        resolution_minutes: resolution_hours * 60,
    }
}

impl Default for SlaPolicy {
    /// Business-day multiples of an 8-hour day, in the range Zendesk and
    /// Freshdesk ship as their own starter policies: urgent answered within
    /// the hour and fixed the same day, low answered within two days and
    /// fixed within two weeks.
    fn default() -> Self {
        Self {
            business_hours: BusinessHours::default(),
            low: target(16, 80),
            medium: target(8, 40),
            high: target(4, 16),
            urgent: target(1, 8),
        }
    }
}

impl SlaPolicy {
    pub fn validate(&self) -> Result<(), String> {
        self.business_hours.validate()?;
        self.low.validate("low")?;
        self.medium.validate("medium")?;
        self.high.validate("high")?;
        self.urgent.validate("urgent")
    }

    /// The SLA a new ticket of `priority` runs under.
    pub fn for_priority(&self, priority: TicketPriority) -> TicketSla {
        let target = match priority {
            TicketPriority::Low => self.low,
            TicketPriority::Medium => self.medium,
            TicketPriority::High => self.high,
            TicketPriority::Urgent => self.urgent,
        };
        TicketSla {
            business_hours: self.business_hours.clone(),
            first_response_minutes: target.first_response_minutes,
            resolution_minutes: target.resolution_minutes,
        }
    }
}

/// The SLA one ticket runs under, fixed when it is created: its company's
/// policy at that moment, for its priority at that moment. Carried on
/// `TicketCreated` because `ScheduleDeadline::schedule` sees only its
/// source event's payload - it can't look the company's policy up. A later
/// policy change or escalation doesn't move a ticket's deadlines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketSla {
    pub business_hours: BusinessHours,
    pub first_response_minutes: u32,
    pub resolution_minutes: u32,
}

/// The two clocks an SLA runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SlaKind {
    FirstResponse,
    Resolution,
}

/// `start` plus `minutes` of business time: the clock only runs inside
/// `hours`' daily window on its working days. Time before the window
/// opens, after it closes, and on non-working days doesn't count, so a
/// ticket filed on a Saturday with a one-hour target is due Monday 10:00
/// under the default hours.
///
/// Zero minutes gives the first business moment at or after `start` -
/// `start` itself when it falls inside the window. The window's close is
/// not inside it: an hour that ends exactly at closing time is due at
/// closing time, but zero minutes from closing time is the next opening.
///
/// Hours that fail `BusinessHours::validate` would never let the clock
/// run; rather than loop forever, they fall back to plain calendar time.
pub fn add_business_minutes(
    start: DateTime<Utc>,
    minutes: u32,
    hours: &BusinessHours,
) -> DateTime<Utc> {
    if hours.validate().is_err() {
        return start + Duration::minutes(i64::from(minutes));
    }
    let open_time = NaiveTime::from_hms_opt(u32::from(hours.open_hour), 0, 0)
        .expect("validated: open_hour <= 23");
    let mut remaining = Duration::minutes(i64::from(minutes));
    let mut now = start;
    loop {
        let date = now.date_naive();
        if hours.is_working_day(date) {
            let open = date.and_time(open_time).and_utc();
            let close = date.and_time(NaiveTime::MIN).and_utc()
                + Duration::hours(i64::from(hours.close_hour));
            if now < open {
                now = open;
            }
            if now < close {
                let available = close - now;
                if remaining <= available {
                    return now + remaining;
                }
                remaining -= available;
            }
        }
        now = date
            .succ_opt()
            .expect("an SLA target is bounded far below chrono's date range")
            .and_time(NaiveTime::MIN)
            .and_utc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// 2026-10-12 is a Monday.
    fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, day, hour, minute, 0)
            .unwrap()
    }

    fn office() -> BusinessHours {
        BusinessHours::default()
    }

    #[test]
    fn within_one_business_day_it_is_plain_addition() {
        assert_eq!(
            add_business_minutes(at(12, 10, 0), 90, &office()),
            at(12, 11, 30)
        );
    }

    #[test]
    fn a_target_ending_exactly_at_closing_time_is_due_at_closing_time() {
        assert_eq!(
            add_business_minutes(at(12, 16, 0), 60, &office()),
            at(12, 17, 0)
        );
    }

    #[test]
    fn the_clock_stops_overnight() {
        // 16:00 Monday + 2h: one hour Monday, one Tuesday morning.
        assert_eq!(
            add_business_minutes(at(12, 16, 0), 120, &office()),
            at(13, 10, 0)
        );
    }

    #[test]
    fn a_start_before_opening_waits_for_opening() {
        assert_eq!(
            add_business_minutes(at(13, 3, 15), 30, &office()),
            at(13, 9, 30)
        );
    }

    #[test]
    fn a_start_after_closing_waits_for_the_next_opening() {
        assert_eq!(
            add_business_minutes(at(13, 22, 0), 30, &office()),
            at(14, 9, 30)
        );
    }

    #[test]
    fn the_clock_stops_over_the_weekend() {
        // Friday 16:30 + 1h: 30 minutes Friday, 30 on Monday.
        assert_eq!(
            add_business_minutes(at(16, 16, 30), 60, &office()),
            at(19, 9, 30)
        );
        // Filed on Saturday: nothing counts until Monday.
        assert_eq!(
            add_business_minutes(at(17, 11, 0), 60, &office()),
            at(19, 10, 0)
        );
        // Sunday 23:59 is still the weekend.
        assert_eq!(
            add_business_minutes(at(18, 23, 59), 1, &office()),
            at(19, 9, 1)
        );
    }

    #[test]
    fn multi_day_targets_skip_every_night_and_weekend_in_between() {
        // 5 business days (40h) from Wednesday 13:00 is the next Wednesday 13:00.
        assert_eq!(
            add_business_minutes(at(14, 13, 0), 40 * 60, &office()),
            at(21, 13, 0)
        );
        // 10 business days from Monday 09:00 is two weeks on, Friday 17:00.
        assert_eq!(
            add_business_minutes(at(12, 9, 0), 80 * 60, &office()),
            at(23, 17, 0)
        );
    }

    #[test]
    fn zero_minutes_is_the_first_business_moment() {
        assert_eq!(
            add_business_minutes(at(12, 10, 7), 0, &office()),
            at(12, 10, 7)
        );
        assert_eq!(
            add_business_minutes(at(12, 17, 0), 0, &office()),
            at(13, 9, 0)
        );
        assert_eq!(
            add_business_minutes(at(17, 12, 0), 0, &office()),
            at(19, 9, 0)
        );
    }

    #[test]
    fn round_the_clock_hours_are_calendar_time() {
        let always = BusinessHours {
            open_hour: 0,
            close_hour: 24,
            working_days: (1..=7).collect(),
        };
        let start = at(16, 23, 30);
        assert_eq!(add_business_minutes(start, 0, &always), start);
        assert_eq!(
            add_business_minutes(start, 3 * 24 * 60, &always),
            at(19, 23, 30)
        );
    }

    #[test]
    fn a_window_ending_at_midnight_runs_through_to_the_next_working_day() {
        let evenings = BusinessHours {
            open_hour: 20,
            close_hour: 24,
            working_days: vec![1, 3],
        };
        // Monday 23:00 + 2h: one hour Monday, then Wednesday from 20:00.
        assert_eq!(
            add_business_minutes(at(12, 23, 0), 120, &evenings),
            at(14, 21, 0)
        );
    }

    #[test]
    fn invalid_hours_fall_back_to_calendar_time_instead_of_looping() {
        let never = BusinessHours {
            open_hour: 9,
            close_hour: 17,
            working_days: vec![],
        };
        assert_eq!(
            add_business_minutes(at(12, 9, 0), 60, &never),
            at(12, 10, 0)
        );
    }

    #[test]
    fn validation_rejects_what_the_arithmetic_cannot_use() {
        assert!(SlaPolicy::default().validate().is_ok());
        for (open_hour, close_hour, working_days) in [
            (17, 9, vec![1]),
            (9, 9, vec![1]),
            (0, 25, vec![1]),
            (9, 17, vec![]),
            (9, 17, vec![0]),
            (9, 17, vec![8]),
            (9, 17, vec![1, 1]),
        ] {
            let hours = BusinessHours {
                open_hour,
                close_hour,
                working_days,
            };
            assert!(hours.validate().is_err(), "{hours:?}");
        }
        let mut policy = SlaPolicy::default();
        policy.high.first_response_minutes = policy.high.resolution_minutes + 1;
        assert!(policy.validate().is_err());
        policy = SlaPolicy::default();
        policy.low.resolution_minutes = MAX_TARGET_MINUTES + 1;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn a_ticket_takes_its_prioritys_targets() {
        let policy = SlaPolicy::default();
        let sla = policy.for_priority(TicketPriority::Urgent);
        assert_eq!(sla.first_response_minutes, 60);
        assert_eq!(sla.resolution_minutes, 8 * 60);
        assert_eq!(sla.business_hours, BusinessHours::default());
        assert_eq!(
            policy.for_priority(TicketPriority::Low).resolution_minutes,
            80 * 60
        );
    }
}
