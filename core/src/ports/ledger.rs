//! The TigerBeetle ledger gateway port.
//!
//! [`Ledger`] is a [`Gateway`] — the anti-corruption boundary to an external
//! transactional system that owns its own atomicity. The kernel gives `Gateway` no
//! `UnitOfWork` accessor, so "the ledger cannot join a Postgres transaction" is a
//! compile-discoverable fact: money is written **last**, in the relay, after the
//! control-plane commit.
//!
//! The port speaks the domain chart of accounts ([`LedgerAccountKey`],
//! [`TransferCode`]); the adapter resolves keys to `u128` TigerBeetle ids (via the
//! `tb_accounts` map) and creates accounts with the correct non-negative flag on
//! first touch. Transfer `id`s are **caller-assigned and deterministic** (derived
//! from the event id), so a retried submit returns `Exists` — idempotent by design.
//! Amounts are always **explicit** (never TB balancing flags), so a retry moves the
//! exact amount frozen into the event.

use async_trait::async_trait;
use invest_domain::{
	architecture::Gateway,
	balance::{LedgerAccountKey, ServiceId, TransferCode},
	error::DomainError,
	issuance::UnitHolder,
};
use thiserror::Error;

#[async_trait]
pub trait Ledger: Gateway {
	/// Ensure the account for `key` exists with the correct ledger, code, and
	/// non-negative flag. Idempotent; flags are set on first create and are
	/// immutable in TB thereafter, so the create must be right the first time.
	async fn ensure_account(&self, key: &LedgerAccountKey) -> Result<(), LedgerError>;

	/// Live balance for an account, normalized to its natural side (Read-First).
	async fn balance(&self, key: &LedgerAccountKey) -> Result<LedgerBalance, LedgerError>;

	/// [`Self::balance`] for several accounts, in the order asked. The adapter answers in
	/// one id-map query and one ledger lookup per chunk; this default — one read per key —
	/// is for wrappers that only intercept other calls.
	async fn balances(&self, keys: &[LedgerAccountKey]) -> Result<Vec<LedgerBalance>, LedgerError> {
		let mut out = Vec::with_capacity(keys.len());
		for key in keys {
			out.push(self.balance(key).await?);
		}
		Ok(out)
	}

	/// Whether a transfer with this (deterministic, caller-assigned) id has already
	/// applied. The relay's settle pre-check uses it to stay idempotent under
	/// at-least-once delivery: a redelivered event's already-applied leg re-submits as
	/// `Exists`, and the live balance already reflects its outflow — so its liquidity
	/// guard must be skipped, not re-checked against the post-outflow balance.
	async fn transfer_exists(&self, id: u128) -> Result<bool, LedgerError>;

	/// Apply a posted transfer with an explicit amount. Ensures both accounts exist
	/// first. Idempotent on the transfer `id` (a re-submit returns `Exists` ⇒ ok).
	async fn post(&self, transfer: &LedgerTransfer) -> Result<(), LedgerError>;

	/// Apply several posted transfers as ONE TigerBeetle linked chain: every leg lands or
	/// none does, and each leg's non-negative check sees the legs before it — so a chain
	/// can credit an account and debit it again in the same breath. Empty is a no-op.
	/// Idempotent on the FIRST leg's id: a chain applies atomically, so its first id
	/// existing means the whole chain already did, and a re-submit is `Ok(())` without
	/// touching the ledger. The book's delivery-versus-payment fill is the caller.
	async fn post_linked(&self, transfers: &[LedgerTransfer]) -> Result<(), LedgerError>;

	/// Apply a pending (two-phase) transfer with `timeout = 0` — the saga owns the
	/// lifecycle, never TB's clock (so a pending can't auto-void out from under it).
	async fn reserve(&self, transfer: &LedgerTransfer) -> Result<(), LedgerError>;

	/// Post or void a pending transfer. Already-posted/already-voided ⇒ success
	/// (idempotent); pending-not-found ⇒ [`LedgerError::Retryable`].
	async fn complete(&self, completion: &PendingCompletion) -> Result<(), LedgerError>;

	/// Every unit holding on the Share ledger inside `scope`, with its **posted** balance
	/// in raw base units — the accounts a product's supply is held on, or the accounts
	/// one holder's units sit in across every product. Read straight from TigerBeetle
	/// (the authoritative store) over the `tb_accounts` map, so a cap table and the
	/// `fee` allocation's price are sums of what is, never of what a projection thinks.
	/// Zero-balance holdings are included; the caller decides whether an emptied
	/// account still counts as a holder. Rows of a retired kind
	/// ([`AccountCode::is_retired`](invest_domain::balance::AccountCode::is_retired)) are skipped by their code, before any parsing.
	async fn share_holdings(&self, scope: &HoldingScope) -> Result<Vec<(LedgerAccountKey, u128)>, LedgerError>;
}

/// Which unit holdings a [`Ledger::share_holdings`] scan returns. The membership rule is
/// the domain's ([`UnitHolder::of_holding`]): a holding is a product's `UserShares`,
/// `BookShares` (a user's units resting in a sell are still theirs) or `FeeShares` (the
/// `fee` allocation's). Supply (`SharesOutstanding`) is never a holding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HoldingScope {
	/// Every holder's account in one product — the cap table.
	Product(ServiceId),
	/// One holder's account in every product — what an allocation owns, priced.
	Holder(UnitHolder),
}

impl HoldingScope {
	/// Whether `key` is a holding inside this scope.
	pub fn admits(&self, key: &LedgerAccountKey) -> bool {
		let Some((service, holder)) = UnitHolder::of_holding(key) else {
			return false;
		};
		match self {
			Self::Product(product) => &service == product,
			Self::Holder(wanted) => &holder == wanted,
		}
	}
}

/// Failure modes the relay and query handlers must distinguish — most importantly
/// `InsufficientFunds` (a real domain outcome from a non-negative-flag violation)
/// from `Retryable`/`Unavailable` (transient) and `Conflict` (a should-not-happen
/// the saga must surface, never silently absorb).
#[derive(Debug, Error)]
pub enum LedgerError {
	/// A non-negative invariant would be violated (TB `ExceedsCredits`/`ExceedsDebits`):
	/// the source account can't cover the transfer.
	#[error("insufficient funds")]
	InsufficientFunds,
	/// Transient — the ledger was unreachable or closed. Retry.
	#[error("ledger unavailable: {0}")]
	Unavailable(String),
	/// A two-phase post raced its pending create (`PendingTransferNotFound`). Retry
	/// after the pending lands.
	#[error("ledger retryable: {0}")]
	Retryable(String),
	/// A genuine inconsistency (overflow, expired pending, ledger mismatch). Park +
	/// alert; never treat as success.
	#[error("ledger conflict: {0}")]
	Conflict(String),
}

impl From<LedgerError> for DomainError {
	/// For a *query* read or a Read-First check: `InsufficientFunds` is a client-
	/// facing validation outcome; the rest are infrastructure faults (never leaked
	/// verbatim to clients — they map to `unavailable` at the gRPC boundary).
	fn from(err: LedgerError) -> Self {
		match err {
			LedgerError::InsufficientFunds => DomainError::Validation("insufficient funds".into()),
			LedgerError::Unavailable(detail) | LedgerError::Retryable(detail) | LedgerError::Conflict(detail) => DomainError::Repository(detail),
		}
	}
}

/// An account's live balance in **raw base units**, normalized to its natural side
/// (every field `>= 0`), in 18-dp units — callers wrap into the typed `Shares` at the
/// boundary. Zero when the account doesn't exist yet.
#[derive(Clone, Copy, Debug)]
pub struct LedgerBalance {
	/// The settled balance on the natural side (`debits − credits` for a holding).
	pub posted: u128,
	/// In-flight INFLOW on the natural side (pending debits for a holding) awaiting
	/// settlement — zero for the common one-sided pending.
	pub pending: u128,
	/// In-flight OUTFLOW reserved against this account (pending credits on a holding):
	/// units locked by an unsettled burn. Subtract from `posted` for what a new command
	/// may spend ([`LedgerBalance::available`]).
	pub locked: u128,
}

impl LedgerBalance {
	/// The settled balance not already reserved by an in-flight pending — what a new
	/// command may actually spend (Read-First). Saturating, so never negative.
	pub fn available(self) -> u128 {
		self.posted.saturating_sub(self.locked)
	}
}

/// A posted or to-be-pending transfer. `id` is caller-assigned and deterministic;
/// `amount` is in the ledger's base units (`Shares::base_units()`); `reference` is stamped into `user_data_128` (the aggregate id) for
/// reconciliation.
#[derive(Clone, Debug)]
pub struct LedgerTransfer {
	pub id: u128,
	pub debit: LedgerAccountKey,
	pub credit: LedgerAccountKey,
	pub amount: u128,
	pub code: TransferCode,
	pub reference: u128,
}

/// Whether a [`PendingCompletion`] posts (commits) or voids (releases) the pending.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionKind {
	Post,
	Void,
}

/// Completes a previously-created pending transfer. Carries the original accounts
/// and amount so the adapter can re-issue the completion idempotently on retry.
#[derive(Clone, Debug)]
pub struct PendingCompletion {
	/// The completion transfer's own deterministic id.
	pub id: u128,
	/// The original pending transfer's id (TB `pending_id`).
	pub pending_id: u128,
	pub kind: CompletionKind,
	pub debit: LedgerAccountKey,
	pub credit: LedgerAccountKey,
	pub amount: u128,
	pub code: TransferCode,
	pub reference: u128,
}
