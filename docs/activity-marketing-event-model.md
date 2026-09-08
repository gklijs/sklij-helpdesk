# Activity & Marketing - event model

Two new bounded contexts, designed together because the whole reason
they exist is to showcase skilj 0.0.4's `CrossContextRoute` (a
single-hop reaction: an `EventType` commits in one bounded context, a
`CommandType`'s payload gets submitted into a different one, through
that command's real `decide()`). `activity` tracks per-person daily
activity and detects declining customer engagement; `marketing`
receives a staff-visible record of company signals worth a marketing
follow-up. Neither touches `helpdesk`'s own existing Company/Ticket
event streams or the `CreateTicket` trial guard - `helpdesk` is only
ever a *source* of routed events here, never modified.

## Cross-context wiring

Not itself an event or command, but the reason the two contexts below
exist at all - three single-hop `CrossContextRoute`s:

- `helpdesk.CompanyExpired` -> `marketing.RecordTrialLapse`
- `helpdesk.CompanyActivated` -> `marketing.RecordTrialConversion`
- `activity.CompanyEngagementDeclined` -> `marketing.RecordEngagementDecline`

(`CompanyExpired`/`CompanyActivated` already exist in `helpdesk` today -
nothing new needed on that side.)

## `activity` context

Tracks who's actually using the product, per company, per day - and,
from that, whether a company's own customers have gone quiet.

### Events

#### DailyActivityRecorded

- **Payload sketch**: company_id: string, person_id: string, person_kind: enum(customer, staff), day: date
- **Triggered by**: RecordDailyActivity
- **Tag candidates**: company: company_id, person: person_id, day: day

#### CompanyEngagementDeclined

- **Payload sketch**: company_id: string, flagged_at: date
- **Triggered by**: RecordEngagementDecline (`activity`'s own command,
  submitted by a new scheduled binary - not a human or API caller,
  the same "external process, not decide() itself, makes the call"
  shape `alerter.rs`/`scheduler.rs` already use for urgent-ticket
  paging and trial-deadline/auto-close)
- **Tag candidates**: company: company_id

### Commands

#### RecordDailyActivity

- **Payload sketch**: company_id: string, person_id: string, person_kind: enum(customer, staff), day: date
- **Tag candidates**: company: company_id, person: person_id, day: day
- **Accepted when**: no `DailyActivityRecorded` already exists for this
  exact company+person+day -> emits DailyActivityRecorded
- **Rejected when**: this exact company+person+day was already recorded
  (`already_recorded_today`) - the frontend can call this unconditionally
  on every dashboard load (customer and staff alike) and let `decide()`
  do the throttling, the same idempotent-by-rejection convention
  `SignUpCompany` already uses for a duplicate signup

#### RecordEngagementDecline

- **Payload sketch**: company_id: string, flagged_at: date
- **Tag candidates**: company: company_id
- **Accepted when**: this company has never been flagged before -> emits
  CompanyEngagementDeclined
- **Rejected when**: this company was already flagged once before
  (`already_flagged`) - confirmed: fires once, not once per check while
  the condition holds

## `marketing` context

Purely a record of "something worth a marketing follow-up happened" -
no outreach automation, no paging (confirmed: not urgent enough for
that). Staff-visible, no separate marketing role/login.

### Events

#### TrialLapsed

- **Payload sketch**: company_id: string, lapsed_at: date
- **Triggered by**: RecordTrialLapse (via CrossContextRoute from
  `helpdesk.CompanyExpired`)
- **Tag candidates**: company: company_id

#### TrialConverted

- **Payload sketch**: company_id: string, converted_at: date
- **Triggered by**: RecordTrialConversion (via CrossContextRoute from
  `helpdesk.CompanyActivated`)
- **Tag candidates**: company: company_id

#### EngagementDeclineFlagged

- **Payload sketch**: company_id: string, flagged_at: date
- **Triggered by**: RecordEngagementDecline (`marketing`'s own command -
  see naming note below; via CrossContextRoute from
  `activity.CompanyEngagementDeclined`)
- **Tag candidates**: company: company_id

### Commands

#### RecordTrialLapse

- **Payload sketch**: company_id: string, lapsed_at: date
- **Tag candidates**: company: company_id
- **Accepted when**: always -> emits TrialLapsed
- **Rejected when**: none identified yet - see open question on repeat
  lapses below

#### RecordTrialConversion

- **Payload sketch**: company_id: string, converted_at: date
- **Tag candidates**: company: company_id
- **Accepted when**: always -> emits TrialConverted
- **Rejected when**: none identified yet - same open question

#### RecordEngagementDecline (marketing's own - distinct type from
`activity`'s command of the same name; they live in different bounded
contexts so this isn't a real collision, just a naming note worth
resolving before writing Rust)

- **Payload sketch**: company_id: string, flagged_at: date
- **Tag candidates**: company: company_id
- **Accepted when**: always -> emits EngagementDeclineFlagged
- **Rejected when**: none identified yet

### Projection (not an event/command, but what "lands in the UI" needs)

#### MarketingOutreachSignals

- Keyed by: company_id
- Folds: TrialLapsed, TrialConverted, EngagementDeclineFlagged
- Exposes enough for a new staff-visible dashboard section: which
  company, which signal, when - a plain record to react to by hand, not
  a queue with its own workflow

## Open questions

- What exactly makes the engagement-decline check decide a company's
  "gone quiet"? (Not modeled here on purpose - this is the actual
  business rule inside the new scheduled binary's own decision logic,
  the same kind of thing `scheduling.rs`'s trial-duration/auto-close
  thresholds already are, not an event/command shape question.)
- Does that scheduled binary need to read `helpdesk`'s own
  `TicketCreated`/`TicketCustomerResponded` feed too, or is customer
  `DailyActivityRecorded` alone enough for a first pass? (Confirmed
  customer-side-only for what *counts*; not yet confirmed whether
  ticket activity is part of that customer-side signal or deferred.)
- `RecordTrialLapse`/`RecordTrialConversion`: a company can already
  `ReactivateCompany` after expiring in `helpdesk` today, then lapse
  again later - should marketing's own events be allowed to recur per
  company (each real lapse/conversion cycle produces its own event), or
  once-ever like the engagement-decline flag? Unlike engagement decline,
  these mirror a `helpdesk` lifecycle transition that can legitimately
  repeat, so recurring seems more correct - flagged rather than assumed.
- The `activity`/`marketing` naming collision above (`RecordEngagementDecline`
  as a command name in both contexts, `CompanyEngagementDeclined` vs.
  `EngagementDeclineFlagged` as the event names) - fine to leave as is,
  or rename one side for clarity when this becomes real Rust?
- Exact `CrossContextRoute` payload mapping (what of the source event's
  own payload becomes the target command's payload) - a `skilj` skill /
  implementation-time detail, not named here.
- What should the new scheduled binary be called (`engagement-watcher`?),
  and does it need its own set of `EventReadToken`s printed by
  `server.rs` the way `alerter.rs`/`scheduler.rs` already do?

## Deferred

- Staff `DailyActivityRecorded` events are modeled (there's real value
  in them - unique daily users, support-coverage visibility) but nothing
  reads them yet; no projection/UI for staff-side activity in this pass.
- The marketing dashboard section's own look/interaction design - this
  document only names what data it needs, not how it's laid out.
- Any actual outreach automation (email, CRM integration) - explicitly
  out of scope; this is a record for a human to act on.
