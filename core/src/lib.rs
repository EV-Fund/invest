//! `invest-core` — the allocation engine.
//!
//! The registry of investable products, the unit ledger (invest's own TigerBeetle
//! cluster, D19), in-kind issuance, ownership and NAV, served over gRPC. Cash is not
//! here: it is banking's, reached through the money port. Moved from banking 9ffafce3
//! (`piggybank/core`), cut to what the allocation engine owns (plan §6.1, S2-3).
//!
//! Hexagonal layout over the shared `invest_domain` — each module is named for its role,
//! not its transport:
//!   services        — driving adapter (the gRPC service surface)
//!   application     — use cases (CQRS command/query handlers)
//!   ports           — driven ports (repositories, the unit ledger, the money ledger)
//!   infrastructure  — driven adapters (Postgres, TigerBeetle, banking's scope ledger)

use std::sync::Arc;

use application::{book::BookFeed, funds::FundPorts};
use invest_domain::book::{MatchingEngine, TakerFeeRiseTerms};
use ports::{
	AllocationRegistry, FundPositionReader, Ledger, MoneyLedger, NavMarks, RedemptionRepository, SubscriptionRepository, UnitIssuanceRepository, UserDirectory, book::BookStore,
	fees::FeePorts,
};
use sqlx::PgPool;
use tokio::sync::Notify;

pub mod application;
pub mod config;
pub mod crypto;
pub mod infrastructure;
pub mod ports;
pub mod services;

/// Shared, cheaply-cloneable handles injected into the gRPC services. The Postgres pool
/// is the **control plane** (registry, event log/outbox, projections); the [`Ledger`] is
/// the **unit plane** (authoritative units in TigerBeetle); the [`MoneyLedger`] is banking,
/// where the cash is. Command handlers `notify` the [`relay_notify`](AppState::relay_notify)
/// after a commit so the outbox relay moves units promptly.
#[derive(Clone)]
pub struct AppState {
	pub pool: PgPool,
	pub ledger: Arc<dyn Ledger>,
	pub money: Arc<dyn MoneyLedger>,
	/// The people concierge knows — who may hold, and their `invest:*` permissions.
	pub users: Arc<dyn UserDirectory>,
	/// The registry of investable products — the gate every deal and issuance resolves its
	/// service through.
	pub allocations: Arc<dyn AllocationRegistry>,
	/// An operator's in-kind mints and retirements, idempotent by key.
	pub issuances: Arc<dyn UnitIssuanceRepository>,
	/// The subscription saga's records — a mint paid for by a hold banking captures.
	pub subscriptions: Arc<dyn SubscriptionRepository>,
	/// The redemption saga's records — accept-and-queue, paid out at settle.
	pub redemptions: Arc<dyn RedemptionRepository>,
	/// The allocation book — orders, fills and per-product terms; the escrow and the
	/// settlement of every fill go through the relay.
	pub book: Arc<dyn BookStore>,
	/// The matching rule the book runs under its write lock. Wired here so the composition
	/// root, not the store, decides how orders match.
	pub book_engine: Arc<dyn MatchingEngine>,
	/// How a taker-fee rise is paced — the notice period and the largest step.
	pub book_taker_fee_rise: TakerFeeRiseTerms,
	/// The in-process fan-out every `WatchBook` subscriber hangs on.
	pub book_feed: Arc<BookFeed>,
	/// The fee plane — a product's terms and their history, the accrual clocks, the
	/// charges and the settlement of fee units into cash.
	pub fees: FeePorts,
	/// Valuation marks → the derived NAV (the operator-posted AUM history).
	pub nav: Arc<dyn NavMarks>,
	/// The per-holder cost-basis projection.
	pub positions: Arc<dyn FundPositionReader>,
	/// Nudges the outbox relay to dispatch right after a command commits.
	pub relay_notify: Arc<Notify>,
}

impl AppState {
	/// The ports a dealing use case borrows, from this state.
	pub fn fund_ports(&self) -> FundPorts<'_> {
		FundPorts {
			allocations: self.allocations.as_ref(),
			ledger: self.ledger.as_ref(),
			money: self.money.as_ref(),
			nav: self.nav.as_ref(),
			relay: &self.relay_notify,
		}
	}
}
