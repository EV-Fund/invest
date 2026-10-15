//! An in-memory [`MoneyLedger`] for tests — the contract of `scope.v1.ScopeLedgerService`
//! without banking.
//!
//! banking serves `OpenAccount`, `GetBalances` and `Transfer` today and answers every saga
//! RPC UNIMPLEMENTED (S2-4), so the flows built on them are tested against this. It keeps
//! the parts of the contract a saga leans on — the idempotency key (same terms → the
//! original answer, other terms → ALREADY_EXISTS), a hold that cannot be over-captured,
//! a compartment that cannot go below zero, NOT_FOUND for an account invest does not own —
//! and none of banking's policy (mandates, ceilings, grants). Banking's side of a fee sweep
//! is played too: what lands on a scope's `fee` collector moves on to `(invest, alloc:fee)`
//! at once, from every scope (plan §3.9).
//!
//! A test steers it with [`InMemoryMoneyLedger::unimplemented`] (an RPC banking does not
//! serve), [`InMemoryMoneyLedger::refuse`] (a refusal banking would give) and
//! [`InMemoryMoneyLedger::outage`] (a transient failure for the next few calls) — all at
//! run time, since the ledger is shared with the relay behind an `Arc`.
//!
//! Never wired into a running server: a fake ledger behind a real price is worse than none.

use std::{
	collections::{HashMap, HashSet},
	sync::Mutex,
};

use async_trait::async_trait;
use invest_domain::{architecture::Gateway, balance::ServiceId, money::Usdt};

use crate::ports::money::{
	AccountLabel, CaptureRequest, CaptureTo, FeeSweepRequest, Hold, HoldId, HoldState, MoneyKey, MoneyLedger, MoneyLedgerError, Payout, PayoutRequest, Refusal, ReserveRequest, ScopeAccount,
	ScopeId, Transfer, VoidRequest,
};

#[derive(Default)]
pub struct InMemoryMoneyLedger {
	state: Mutex<State>,
}

#[derive(Default)]
struct State {
	/// invest's own accounts; `fee` collectors are banking's and live in `collected`.
	accounts: HashMap<(ScopeId, AccountLabel), Usdt>,
	/// What each scope's `fee` collector received from sweeps (before banking forwarded it).
	collected: HashMap<ScopeId, Usdt>,
	compartments: HashMap<(ScopeId, String), Usdt>,
	holds: HashMap<HoldId, Hold>,
	/// (rpc, scope, key) → (the terms first asked, the answer given).
	keys: HashMap<(&'static str, ScopeId, String), (String, Answer)>,
	next_id: u64,
	unimplemented: HashSet<&'static str>,
	refusals: HashMap<&'static str, Refusal>,
	/// rpc → how many more calls answer UNAVAILABLE.
	outages: HashMap<&'static str, u32>,
}

#[derive(Clone)]
enum Answer {
	Account(ScopeAccount),
	Hold(Hold),
	Payout(Payout),
	Transfer(Transfer),
}

impl InMemoryMoneyLedger {
	pub fn new() -> Self {
		Self::default()
	}

	/// Answer these RPCs UNIMPLEMENTED, as banking does until it serves them.
	pub fn unimplemented(self, rpcs: &[&'static str]) -> Self {
		self.lock().unimplemented.extend(rpcs);
		self
	}

	/// Refuse every further call of `rpc` with `refusal`, before it touches anything — the
	/// way banking refuses on policy (a frozen scope, a missing mandate, a ceiling). Lifted
	/// by [`Self::serve`].
	pub fn refuse(&self, rpc: &'static str, refusal: Refusal) {
		self.lock().refusals.insert(rpc, refusal);
	}

	/// Answer the next `calls` calls of `rpc` UNAVAILABLE without acting — banking away for a
	/// moment. The call after them is served.
	pub fn outage(&self, rpc: &'static str, calls: u32) {
		self.lock().outages.insert(rpc, calls);
	}

	/// Serve `rpc` again: lift a [`Self::refuse`] or an [`Self::outage`] still running.
	pub fn serve(&self, rpc: &'static str) {
		let mut state = self.lock();
		state.refusals.remove(rpc);
		state.outages.remove(rpc);
	}

	/// Credit an account invest owns, opening it if needed — money arriving from outside
	/// the flows under test (a seed).
	pub fn credit(&self, scope: &ScopeId, label: &AccountLabel, amount: Usdt) {
		let mut state = self.lock();
		let balance = state.accounts.entry((scope.clone(), label.clone())).or_default();
		*balance = balance.checked_add(amount).unwrap_or(Usdt::from_base_units(u128::MAX));
	}

	/// The balance of an account invest owns, zero when it was never opened.
	pub fn account(&self, scope: &ScopeId, label: &AccountLabel) -> Usdt {
		self.lock().accounts.get(&(scope.clone(), label.clone())).copied().unwrap_or_default()
	}

	/// Put money in a user's compartment in `scope`.
	pub fn fund(&self, scope: &ScopeId, auth_subject: &str, amount: Usdt) {
		let mut state = self.lock();
		let balance = state.compartments.entry((scope.clone(), auth_subject.to_owned())).or_default();
		*balance = balance.checked_add(amount).unwrap_or(Usdt::from_base_units(u128::MAX));
	}

	/// A user's compartment balance in `scope`.
	pub fn compartment(&self, scope: &ScopeId, auth_subject: &str) -> Usdt {
		self.lock().compartments.get(&(scope.clone(), auth_subject.to_owned())).copied().unwrap_or_default()
	}

	/// What `scope`'s `fee` collector has received, all of it since forwarded.
	pub fn collected(&self, scope: &ScopeId) -> Usdt {
		self.lock().collected.get(scope).copied().unwrap_or_default()
	}

	/// The holds placed for `subject_ref` (`subscription:<uuid>`, `book_order:<uuid>`), in
	/// the order placed.
	pub fn holds_for(&self, subject_ref: &str) -> Vec<Hold> {
		let state = self.lock();
		let mut holds: Vec<Hold> = state.holds.values().filter(|hold| hold.subject_ref == subject_ref).cloned().collect();
		holds.sort_by_key(|hold| hold.id.0.trim_start_matches("hold-").parse::<u64>().unwrap_or(u64::MAX));
		holds
	}

	fn lock(&self) -> std::sync::MutexGuard<'_, State> {
		// A test that panicked while holding the lock already failed; the state it left is
		// still the best answer for the assertions that follow.
		self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	/// Whether banking serves `rpc` right now, as the test steered it.
	fn served(&self, rpc: &'static str) -> Result<(), MoneyLedgerError> {
		let mut state = self.lock();
		if state.unimplemented.contains(rpc) {
			return Err(MoneyLedgerError::Unimplemented { rpc });
		}
		if let Some(refusal) = state.refusals.get(rpc) {
			return Err(MoneyLedgerError::Refused(refusal.clone()));
		}
		if let Some(left) = state.outages.get_mut(rpc) {
			*left -= 1;
			if *left == 0 {
				state.outages.remove(rpc);
			}
			return Err(MoneyLedgerError::Unavailable(format!("{rpc}: injected outage")));
		}
		Ok(())
	}
}

impl State {
	/// The answer a key already holds: the original for the same terms, a conflict for
	/// others, `None` for a first use.
	fn replay(&self, rpc: &'static str, scope: &ScopeId, key: &MoneyKey, terms: &str) -> Option<Result<Answer, MoneyLedgerError>> {
		let (first_terms, answer) = self.keys.get(&(rpc, scope.clone(), key.as_str().to_owned()))?;
		Some(if first_terms == terms {
			Ok(answer.clone())
		} else {
			Err(MoneyLedgerError::IdempotencyConflict(format!("{rpc}: key {} was used for other terms", key.as_str())))
		})
	}

	fn remember(&mut self, rpc: &'static str, scope: &ScopeId, key: &MoneyKey, terms: String, answer: Answer) {
		self.keys.insert((rpc, scope.clone(), key.as_str().to_owned()), (terms, answer));
	}

	fn mint_id(&mut self, kind: &str) -> String {
		self.next_id += 1;
		format!("{kind}-{}", self.next_id)
	}

	fn debit_account(&mut self, scope: &ScopeId, label: &AccountLabel, amount: Usdt) -> Result<(), MoneyLedgerError> {
		let balance = self
			.accounts
			.get_mut(&(scope.clone(), label.clone()))
			.ok_or(MoneyLedgerError::Refused(Refusal::NotAccountOwner))?;
		*balance = balance.checked_sub(amount).ok_or(MoneyLedgerError::Refused(Refusal::InsufficientFunds))?;
		Ok(())
	}

	fn credit_compartment(&mut self, scope: &ScopeId, auth_subject: &str, amount: Usdt) -> Result<(), MoneyLedgerError> {
		let balance = self.compartments.entry((scope.clone(), auth_subject.to_owned())).or_default();
		*balance = balance.checked_add(amount).ok_or_else(|| MoneyLedgerError::Invalid("compartment overflows".into()))?;
		Ok(())
	}
}

fn unexpected(rpc: &str) -> MoneyLedgerError {
	MoneyLedgerError::Protocol(format!("{rpc}: the key recorded an answer of another kind"))
}

impl Gateway for InMemoryMoneyLedger {}

#[async_trait]
impl MoneyLedger for InMemoryMoneyLedger {
	async fn open_account(&self, scope: &ScopeId, label: &AccountLabel, key: &MoneyKey) -> Result<ScopeAccount, MoneyLedgerError> {
		self.served("OpenAccount")?;
		let mut state = self.lock();
		let terms = label.as_str().to_owned();
		if let Some(answer) = state.replay("OpenAccount", scope, key, &terms) {
			return match answer? {
				Answer::Account(account) => Ok(account),
				_ => Err(unexpected("OpenAccount")),
			};
		}
		let balance = *state.accounts.entry((scope.clone(), label.clone())).or_default();
		let account = ScopeAccount {
			scope: scope.clone(),
			label: label.clone(),
			balance,
			opened_at: 0,
		};
		state.remember("OpenAccount", scope, key, terms, Answer::Account(account.clone()));
		Ok(account)
	}

	async fn balances(&self, scope: &ScopeId, labels: &[AccountLabel]) -> Result<Vec<ScopeAccount>, MoneyLedgerError> {
		self.served("GetBalances")?;
		let state = self.lock();
		let mut seen = HashSet::new();
		labels
			.iter()
			.filter(|label| seen.insert((*label).clone()))
			.map(|label| {
				let balance = state.accounts.get(&(scope.clone(), label.clone())).ok_or(MoneyLedgerError::NotFound)?;
				Ok(ScopeAccount {
					scope: scope.clone(),
					label: label.clone(),
					balance: *balance,
					opened_at: 0,
				})
			})
			.collect()
	}

	async fn reserve(&self, request: &ReserveRequest) -> Result<Hold, MoneyLedgerError> {
		self.served("Reserve")?;
		let mut state = self.lock();
		let terms = format!("{}|{}|{}", request.auth_subject, request.amount, request.subject_ref.as_str());
		if let Some(answer) = state.replay("Reserve", &request.scope, &request.key, &terms) {
			return match answer? {
				Answer::Hold(hold) => Ok(hold),
				_ => Err(unexpected("Reserve")),
			};
		}
		let balance = state.compartments.entry((request.scope.clone(), request.auth_subject.clone())).or_default();
		*balance = balance.checked_sub(request.amount).ok_or(MoneyLedgerError::Refused(Refusal::InsufficientFunds))?;
		let hold = Hold {
			id: HoldId(state.mint_id("hold")),
			scope: request.scope.clone(),
			auth_subject: request.auth_subject.clone(),
			amount: request.amount,
			captured: Usdt::ZERO,
			remaining: request.amount,
			state: HoldState::Active,
			subject_ref: request.subject_ref.as_str().to_owned(),
		};
		state.holds.insert(hold.id.clone(), hold.clone());
		state.remember("Reserve", &request.scope, &request.key, terms, Answer::Hold(hold.clone()));
		Ok(hold)
	}

	async fn capture(&self, request: &CaptureRequest) -> Result<Hold, MoneyLedgerError> {
		self.served("Capture")?;
		let mut state = self.lock();
		let terms = format!("{}|{:?}|{:?}", request.hold_id.0, request.to, request.amount);
		if let Some(answer) = state.replay("Capture", &request.scope, &request.key, &terms) {
			return match answer? {
				Answer::Hold(hold) => Ok(hold),
				_ => Err(unexpected("Capture")),
			};
		}
		let mut hold = state
			.holds
			.get(&request.hold_id)
			.filter(|hold| hold.scope == request.scope)
			.cloned()
			.ok_or(MoneyLedgerError::NotFound)?;
		if hold.state != HoldState::Active {
			return Err(MoneyLedgerError::Refused(Refusal::HoldNotActive));
		}
		let take = request.amount.unwrap_or(hold.remaining);
		hold.remaining = hold.remaining.checked_sub(take).ok_or(MoneyLedgerError::Refused(Refusal::InsufficientFunds))?;
		hold.captured = hold.captured.checked_add(take).ok_or_else(|| MoneyLedgerError::Invalid("capture overflows".into()))?;
		if hold.remaining.is_zero() {
			hold.state = HoldState::Captured;
		}
		match &request.to {
			CaptureTo::Account(label) => {
				let balance = state
					.accounts
					.get_mut(&(request.scope.clone(), label.clone()))
					.ok_or(MoneyLedgerError::Refused(Refusal::NotAccountOwner))?;
				*balance = balance.checked_add(take).ok_or_else(|| MoneyLedgerError::Invalid("account overflows".into()))?;
			}
			CaptureTo::User(auth_subject) => {
				if *auth_subject == hold.auth_subject {
					return Err(MoneyLedgerError::Invalid("Capture into the held user's own compartment is a Void".into()));
				}
				state.credit_compartment(&request.scope, auth_subject, take)?;
			}
		}
		state.holds.insert(hold.id.clone(), hold.clone());
		state.remember("Capture", &request.scope, &request.key, terms, Answer::Hold(hold.clone()));
		Ok(hold)
	}

	async fn void(&self, request: &VoidRequest) -> Result<Hold, MoneyLedgerError> {
		self.served("Void")?;
		let mut state = self.lock();
		let terms = request.hold_id.0.clone();
		if let Some(answer) = state.replay("Void", &request.scope, &request.key, &terms) {
			return match answer? {
				Answer::Hold(hold) => Ok(hold),
				_ => Err(unexpected("Void")),
			};
		}
		let mut hold = state
			.holds
			.get(&request.hold_id)
			.filter(|hold| hold.scope == request.scope)
			.cloned()
			.ok_or(MoneyLedgerError::NotFound)?;
		if hold.state == HoldState::Active {
			let released = hold.remaining;
			state.credit_compartment(&request.scope, &hold.auth_subject, released)?;
			hold.remaining = Usdt::ZERO;
			hold.state = HoldState::Voided;
			state.holds.insert(hold.id.clone(), hold.clone());
		}
		state.remember("Void", &request.scope, &request.key, terms, Answer::Hold(hold.clone()));
		Ok(hold)
	}

	async fn hold(&self, scope: &ScopeId, hold_id: &HoldId) -> Result<Hold, MoneyLedgerError> {
		self.served("GetHold")?;
		self.lock().holds.get(hold_id).filter(|hold| hold.scope == *scope).cloned().ok_or(MoneyLedgerError::NotFound)
	}

	async fn payout(&self, request: &PayoutRequest) -> Result<Payout, MoneyLedgerError> {
		self.served("Payout")?;
		let mut state = self.lock();
		let terms = format!("{}|{:?}|{:?}", request.from, request.lines, request.reason);
		if let Some(answer) = state.replay("Payout", &request.scope, &request.key, &terms) {
			return match answer? {
				Answer::Payout(payout) => Ok(payout),
				_ => Err(unexpected("Payout")),
			};
		}
		if request.lines.is_empty() || request.lines.len() > 1000 {
			return Err(MoneyLedgerError::Invalid("a payout carries 1..1000 lines".into()));
		}
		let total = request
			.lines
			.iter()
			.try_fold(Usdt::ZERO, |sum, line| sum.checked_add(line.amount))
			.ok_or_else(|| MoneyLedgerError::Invalid("payout total overflows".into()))?;
		// Atomic: the debit is checked whole before any line is credited.
		state.debit_account(&request.scope, &request.from, total)?;
		for line in &request.lines {
			state.credit_compartment(&request.scope, &line.auth_subject, line.amount)?;
		}
		let payout = Payout { id: state.mint_id("payout"), total };
		state.remember("Payout", &request.scope, &request.key, terms, Answer::Payout(payout.clone()));
		Ok(payout)
	}

	async fn sweep_fees(&self, request: &FeeSweepRequest) -> Result<Transfer, MoneyLedgerError> {
		self.served("Transfer")?;
		let mut state = self.lock();
		let terms = format!("{}|{}", request.from, request.amount);
		if let Some(answer) = state.replay("Transfer", &request.scope, &request.key, &terms) {
			return match answer? {
				Answer::Transfer(transfer) => Ok(transfer),
				_ => Err(unexpected("Transfer")),
			};
		}
		state.debit_account(&request.scope, &request.from, request.amount)?;
		let collected = state.collected.entry(request.scope.clone()).or_default();
		*collected = collected.checked_add(request.amount).ok_or_else(|| MoneyLedgerError::Invalid("collector overflows".into()))?;
		// banking's whitelisted route: every scope's fee collector forwards to invest's `fee`
		// allocation, which banking opens for invest if it has to.
		let fee_label = AccountLabel::allocation(&ServiceId::fee()).map_err(|err| MoneyLedgerError::Invalid(err.to_string()))?;
		let forwarded = state.accounts.entry((ScopeId::invest(), fee_label)).or_default();
		*forwarded = forwarded.checked_add(request.amount).ok_or_else(|| MoneyLedgerError::Invalid("alloc:fee overflows".into()))?;
		let transfer = Transfer {
			id: state.mint_id("transfer"),
			amount: request.amount,
		};
		state.remember("Transfer", &request.scope, &request.key, terms, Answer::Transfer(transfer.clone()));
		Ok(transfer)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ports::money::{PayoutLine, PayoutReason, SubjectRef};

	fn usdt(raw: &str) -> Usdt {
		Usdt::parse_decimal(raw).unwrap()
	}

	fn key(raw: &str) -> MoneyKey {
		MoneyKey::parse(raw).unwrap()
	}

	fn subject() -> SubjectRef {
		SubjectRef::new("subscription", uuid::Uuid::new_v4()).unwrap()
	}

	fn alloc() -> AccountLabel {
		AccountLabel::parse("alloc:service_arb").unwrap()
	}

	// The subscription saga's cash leg (plan §6.3): hold the user's money, capture it into
	// the allocation; a retry with the same key is the original answer, not a second move.
	#[tokio::test]
	async fn a_hold_captured_into_the_allocation_moves_the_cash_once() {
		let ledger = InMemoryMoneyLedger::new();
		let scope = ScopeId::invest();
		ledger.fund(&scope, "alice", usdt("100"));
		ledger.open_account(&scope, &alloc(), &key("open")).await.unwrap();
		let reserve = ReserveRequest {
			scope: scope.clone(),
			auth_subject: "alice".into(),
			amount: usdt("60"),
			ttl_seconds: None,
			key: key("r1"),
			subject_ref: subject(),
		};
		let hold = ledger.reserve(&reserve).await.unwrap();
		assert_eq!(ledger.reserve(&reserve).await.unwrap(), hold, "a retry is the original hold");
		assert_eq!(ledger.compartment(&scope, "alice"), usdt("40"));
		let capture = CaptureRequest {
			scope: scope.clone(),
			hold_id: hold.id.clone(),
			to: CaptureTo::Account(alloc()),
			amount: None,
			key: key("c1"),
			subject_ref: subject(),
		};
		let captured = ledger.capture(&capture).await.unwrap();
		assert_eq!((captured.state, captured.captured), (HoldState::Captured, usdt("60")));
		ledger.capture(&capture).await.unwrap();
		assert_eq!(ledger.balances(&scope, &[alloc()]).await.unwrap()[0].balance, usdt("60"), "the retry moved nothing");
		let again = CaptureRequest { key: key("c2"), ..capture };
		assert!(matches!(ledger.capture(&again).await, Err(MoneyLedgerError::Refused(Refusal::HoldNotActive))));
	}

	#[tokio::test]
	async fn a_void_returns_the_rest_and_a_short_compartment_is_refused() {
		let ledger = InMemoryMoneyLedger::new();
		let scope = ScopeId::invest();
		ledger.fund(&scope, "bob", usdt("10"));
		let mut reserve = ReserveRequest {
			scope: scope.clone(),
			auth_subject: "bob".into(),
			amount: usdt("11"),
			ttl_seconds: None,
			key: key("r1"),
			subject_ref: subject(),
		};
		assert!(matches!(ledger.reserve(&reserve).await, Err(MoneyLedgerError::Refused(Refusal::InsufficientFunds))));
		reserve.amount = usdt("10");
		reserve.key = key("r2");
		let hold = ledger.reserve(&reserve).await.unwrap();
		let void = VoidRequest {
			scope: scope.clone(),
			hold_id: hold.id,
			key: key("v1"),
			subject_ref: subject(),
		};
		assert_eq!(ledger.void(&void).await.unwrap().state, HoldState::Voided);
		assert_eq!(ledger.compartment(&scope, "bob"), usdt("10"));
	}

	#[tokio::test]
	async fn a_key_reused_for_other_terms_is_a_conflict() {
		let ledger = InMemoryMoneyLedger::new();
		let scope = ScopeId::invest();
		ledger.credit(&scope, &alloc(), usdt("5"));
		let payout = |amount: &str| PayoutRequest {
			scope: scope.clone(),
			from: alloc(),
			lines: vec![PayoutLine {
				auth_subject: "carol".into(),
				amount: usdt(amount),
			}],
			reason: PayoutReason::Redemption,
			key: key("p1"),
			subject_ref: SubjectRef::new("redemption", uuid::Uuid::nil()).unwrap(),
		};
		ledger.payout(&payout("2")).await.unwrap();
		assert!(matches!(ledger.payout(&payout("3")).await, Err(MoneyLedgerError::IdempotencyConflict(_))));
		assert_eq!(ledger.compartment(&scope, "carol"), usdt("2"));
	}

	#[tokio::test]
	async fn an_unopened_account_is_not_found_and_an_unserved_rpc_is_unimplemented() {
		let ledger = InMemoryMoneyLedger::new().unimplemented(&["Reserve"]);
		assert!(matches!(ledger.balances(&ScopeId::invest(), &[alloc()]).await, Err(MoneyLedgerError::NotFound)));
		let reserve = ReserveRequest {
			scope: ScopeId::invest(),
			auth_subject: "dave".into(),
			amount: usdt("1"),
			ttl_seconds: None,
			key: key("r"),
			subject_ref: subject(),
		};
		assert!(matches!(ledger.reserve(&reserve).await, Err(MoneyLedgerError::Unimplemented { rpc: "Reserve" })));
	}

	// plan §3.9: a fee swept in any scope — `sa` as much as `invest` — ends on invest's own
	// `fee` allocation, never in banking's treasury.
	#[tokio::test]
	async fn a_fee_swept_in_any_scope_lands_on_invest_s_fee_allocation() {
		let ledger = InMemoryMoneyLedger::new();
		let sa = ScopeId::parse("sa").unwrap();
		let fee = AccountLabel::allocation(&ServiceId::fee()).unwrap();
		ledger.credit(&sa, &alloc(), usdt("10"));
		let sweep = FeeSweepRequest {
			scope: sa.clone(),
			from: alloc(),
			amount: usdt("4"),
			key: key("s1"),
			subject_ref: SubjectRef::new("fee_settlement", uuid::Uuid::nil()).unwrap(),
		};
		ledger.sweep_fees(&sweep).await.unwrap();
		ledger.sweep_fees(&sweep).await.unwrap();
		assert_eq!(ledger.account(&sa, &alloc()), usdt("6"), "the retry moved nothing");
		assert_eq!(ledger.collected(&sa), usdt("4"));
		assert_eq!(ledger.account(&ScopeId::invest(), &fee), usdt("4"));
	}

	#[tokio::test]
	async fn a_refusal_holds_until_lifted_and_an_outage_ends_by_itself() {
		let ledger = InMemoryMoneyLedger::new();
		let scope = ScopeId::invest();
		ledger.fund(&scope, "erin", usdt("5"));
		let reserve = |raw: &str| ReserveRequest {
			scope: scope.clone(),
			auth_subject: "erin".into(),
			amount: usdt("1"),
			ttl_seconds: None,
			key: key(raw),
			subject_ref: subject(),
		};
		ledger.refuse("Reserve", Refusal::MandateRequired);
		for raw in ["r1", "r2"] {
			assert!(matches!(ledger.reserve(&reserve(raw)).await, Err(MoneyLedgerError::Refused(Refusal::MandateRequired))));
		}
		ledger.serve("Reserve");
		ledger.outage("Reserve", 2);
		for raw in ["r3", "r4"] {
			assert!(matches!(ledger.reserve(&reserve(raw)).await, Err(MoneyLedgerError::Unavailable(_))));
		}
		ledger.reserve(&reserve("r5")).await.unwrap();
		assert_eq!(ledger.compartment(&scope, "erin"), usdt("4"), "only the served call held money");
	}
}
