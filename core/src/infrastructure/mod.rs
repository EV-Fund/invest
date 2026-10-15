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
//!   cost-basis projection; [`users`] — the projection of the people concierge knows;
//!   [`concierge`] — its writer, the pull of concierge's lifecycle and scope streams.
//! - [`subscriptions`] / [`redemptions`] — the records of the money sagas; [`book`] — the
//!   order book and its matching transaction.
//! - [`fees`] — the fee plane's stores; [`fee_accrual`] — settling the management accrual
//!   before a basis moves; [`fee_policy_changes`] — the history of a product's terms and
//!   the only writer of the live ones; [`fee_sweeper`] — the worker that charges and
//!   promotes.
//! - [`outbox`] — the transactional outbox written inside the same transaction as the
//!   state change, plus its drain side.
//! - [`relay`] — the single-worker dispatcher that drains the outbox and issues
//!   TigerBeetle transfers (Write-Last), idempotently.

pub mod allocations;
pub mod book;
pub mod concierge;
pub mod db;
pub mod fee_accrual;
pub mod fee_policy_changes;
pub mod fee_sweeper;
pub mod fees;
pub mod issuance;
pub mod ledger;
pub mod money;
pub mod nav;
pub mod outbox;
pub mod positions;
pub mod redemptions;
pub mod relay;
pub mod subscriptions;
pub mod tigerbeetle;
pub mod users;

/// Unix seconds now, by the host clock — for the background workers and the relay's
/// projections, which stamp the moment they act. The request path takes its `now` from the
/// gRPC boundary instead, so a use case is a function of its inputs.
pub fn now_unix() -> i64 {
	// A host clock before 1970 is a broken host; 0 keeps every comparison defined.
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
}
