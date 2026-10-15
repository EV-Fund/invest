//! Infrastructure: driven adapters over the concrete external systems invest-core runs on.
//!
//! - [`db`] — Postgres **control plane**: pool and migrations.
//! - [`tigerbeetle`] — the connected TigerBeetle client (invest's own cluster).
//! - [`ledger`] — the unit `Ledger` [`Gateway`](invest_domain::architecture::Gateway) over
//!   TigerBeetle (the Share chart of accounts, transfers, two-phase ops).
//! - [`money`] — the `MoneyLedger` gateway: banking's scope ledger over gRPC, and the
//!   in-memory fake the tests use.
//! - [`allocations`] / [`issuance`] — repositories for the registry and the in-kind mint
//!   (atomic state + drained events); [`nav`] — the valuation marks; [`positions`] — the
//!   cost-basis projection; [`users`] — the projection of the people concierge knows.
//! - [`outbox`] — the transactional outbox written inside the same transaction as the
//!   state change, plus its drain side.
//! - [`relay`] — the single-worker dispatcher that drains the outbox and issues
//!   TigerBeetle transfers (Write-Last), idempotently.

pub mod allocations;
pub mod db;
pub mod issuance;
pub mod ledger;
pub mod money;
pub mod nav;
pub mod outbox;
pub mod positions;
pub mod relay;
pub mod tigerbeetle;
pub mod users;
