//! Given/when/then tests for every `decide()`/`project()` in this crate,
//! run in-process through `skilj-test-fixture` - no database, no HTTP,
//! so unlike every other `tests/*.rs` suite these can never silently
//! skip for want of a reachable Postgres.
//!
//! What they don't cover, on purpose: which events skilj actually hands
//! `decide()` (the DCB tag scoping), conflicting concurrent writes,
//! persistence, and access control. Those stay with the Postgres-backed
//! suites. Here each test hands `decide()` exactly the `matching_events`
//! it would see, by construction.
//!
//! One binary rather than one per file (see `tests/company.rs`'s own doc
//! comment for why the DB suites are split): nothing here holds a pool,
//! so there's no reason to pay for several links.

mod activity_marketing;
mod company;
mod company_snapshot;
mod events;
mod projections;
mod ticket;
mod ticket_snapshot;
