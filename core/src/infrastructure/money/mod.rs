//! Adapters for the [`MoneyLedger`](crate::ports::money::MoneyLedger) port.
//!
//! - [`grpc::ScopeLedgerClient`] — banking's `scope.v1.ScopeLedgerService` over mTLS with a
//!   service token. The production adapter.
//! - [`Unconfigured`] — what invest-core runs with when no scope ledger is configured:
//!   every call is [`MoneyLedgerError::Unavailable`], so nothing priced off cash is ever
//!   priced off a guess.
//! - [`fake::InMemoryMoneyLedger`] — the same contract in memory, for tests: the sagas run
//!   against it with a real Postgres and TigerBeetle, and no banking behind them.

use async_trait::async_trait;
use invest_domain::architecture::Gateway;

use crate::ports::money::{
	AccountLabel, CaptureRequest, FeeSweepRequest, Hold, HoldId, MoneyKey, MoneyLedger, MoneyLedgerError, OperationId, Payout, PayoutRequest, PendingOperation, RequestPayoutRequest,
	ReserveRequest, ScopeAccount, ScopeId, Transfer, VoidRequest,
};

pub mod fake;
pub mod grpc;

/// The money port with no ledger behind it.
pub struct Unconfigured;

impl Gateway for Unconfigured {}

fn unconfigured<T>() -> Result<T, MoneyLedgerError> {
	Err(MoneyLedgerError::Unavailable("no scope ledger configured (SCOPE_LEDGER_ADDR is unset)".into()))
}

#[async_trait]
impl MoneyLedger for Unconfigured {
	async fn open_account(&self, _: &ScopeId, _: &AccountLabel, _: &MoneyKey) -> Result<ScopeAccount, MoneyLedgerError> {
		unconfigured()
	}

	async fn balances(&self, _: &ScopeId, _: &[AccountLabel]) -> Result<Vec<ScopeAccount>, MoneyLedgerError> {
		unconfigured()
	}

	async fn reserve(&self, _: &ReserveRequest) -> Result<Hold, MoneyLedgerError> {
		unconfigured()
	}

	async fn capture(&self, _: &CaptureRequest) -> Result<Hold, MoneyLedgerError> {
		unconfigured()
	}

	async fn void(&self, _: &VoidRequest) -> Result<Hold, MoneyLedgerError> {
		unconfigured()
	}

	async fn hold(&self, _: &ScopeId, _: &HoldId) -> Result<Hold, MoneyLedgerError> {
		unconfigured()
	}

	async fn payout(&self, _: &PayoutRequest) -> Result<Payout, MoneyLedgerError> {
		unconfigured()
	}

	async fn sweep_fees(&self, _: &FeeSweepRequest) -> Result<Transfer, MoneyLedgerError> {
		unconfigured()
	}

	async fn request_payout(&self, _: &RequestPayoutRequest) -> Result<PendingOperation, MoneyLedgerError> {
		unconfigured()
	}

	async fn pending_operation(&self, _: &ScopeId, _: &OperationId) -> Result<PendingOperation, MoneyLedgerError> {
		unconfigured()
	}
}
