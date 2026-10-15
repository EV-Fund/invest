//! The money sagas — every relayed event that moves cash as well as units (plan §6.3).
//!
//! The units are invest's (TigerBeetle, written here); the cash is banking's (the
//! [`MoneyLedger`], asked here). The two cannot share a transaction, so each saga orders
//! its steps so that the leg banking may refuse comes while the unit leg is still
//! revocable — a **pending** transfer — and the unit leg is posted only once the cash has
//! moved, or voided when banking refused:
//!
//! - **Subscription** — pending mint `Dr UserShares / Cr SharesOutstanding`, `Capture` the
//!   subscriber's hold into `(<scope>, alloc:<service>)`, post the mint. A refused capture
//!   (the hold expired, was voided) voids what is left of the hold and the pending mint,
//!   and fails the subscription.
//! - **Redemption** — the burn was reserved as a pending transfer when it was requested;
//!   at settle, `Payout` from `(<scope>, alloc:<service>)` into the holder's compartment,
//!   then post the burn. A refused payout (the account is short, the amount is past the
//!   automatic ceilings) voids the burn — the units go back — and fails the redemption.
//! - **Fill** — pending `Dr UserShares(buyer) / Cr BookShares(seller)`, capture the
//!   buyer's hold into the seller's compartment (the notional, less a selling taker's
//!   fee), post the units; then the taker's fee: captured into the product's account and
//!   swept on into the scope's fee route. A refused cash capture voids the units leg.
//! - **Fee settlement** — sweep the fee class's value out of the product's account into
//!   `(<scope>, fee)`, then burn the fee units.
//!
//! Every step is idempotent on redelivery: a TigerBeetle leg by its deterministic id
//! (derived from the aggregate id and a FROZEN salt — banking's where banking had the
//! leg), a money call by a key derived the same way, which banking answers with the
//! original result. A money call banking cannot take right now (unreachable, or the RPC
//! not served yet) is a retry from the same `seq`; a refusal is the saga's own branch; a
//! malformed exchange parks for an operator.

use invest_domain::{
	balance::{LedgerAccountKey, ServiceId, TransferCode},
	book::{BookEvent, Locked, Side},
	fees::FeeEvent,
	money::{Shares, Usdt},
	redemptions::RedemptionEvent,
	subscriptions::SubscriptionEvent,
	users::UserId,
};
use sqlx::PgPool;
use uuid::Uuid;

use super::{Outcome, Relay, record_saga_step, tid};
use crate::{
	infrastructure::{fee_accrual, now_unix, outbox::OutboxRow, redemptions},
	ports::{
		ledger::{CompletionKind, LedgerError, LedgerTransfer, PendingCompletion},
		money::{AccountLabel, CaptureRequest, CaptureTo, FeeSweepRequest, HoldId, MoneyKey, MoneyLedgerError, PayoutLine, PayoutReason, PayoutRequest, ScopeId, SubjectRef, VoidRequest},
	},
};

/// A subscription's units: the pending mint, then its post or its void. `subscribe:mint`
/// is banking's salt for the (then posted) mint; the completions are invest's own.
const SUBSCRIBE_MINT: &[u8] = b"subscribe:mint";
const SUBSCRIBE_MINT_POST: &[u8] = b"subscribe:mint:post";
const SUBSCRIBE_MINT_VOID: &[u8] = b"subscribe:mint:void";
/// The subscription's cost-basis projection marker (banking's).
const SUBSCRIBE_POSITION: &[u8] = b"subscribe:position";

/// A redemption's burn — banking's salts — plus the void a refused payout issues.
const BURN_RESERVE: &[u8] = b"redeem:burn";
const BURN_SETTLE: &[u8] = b"redeem:burn:settle";
const BURN_VOID_FAIL: &[u8] = b"redeem:burn:void:fail";
const BURN_VOID_CANCEL: &[u8] = b"redeem:burn:void:cancel";
const BURN_VOID_REFUSED: &[u8] = b"redeem:burn:void:refused";
/// The redemption's cost-basis reduction marker.
const REDEEM_POSITION: &[u8] = b"redeem:position";

/// The fee class burnt by a settlement (banking's).
const FEE_SETTLE_BURN: &[u8] = b"fee:settle:burn";

/// The book: an order's lock and release (banking's, derived from the ORDER id), a
/// fill's pending units leg (banking's salt, then a posted leg of a linked chain) and its
/// completions, derived from the TRADE id.
const BOOK_LOCK: &[u8] = b"book:lock";
const BOOK_RELEASE: &[u8] = b"book:release";
const BOOK_FILL_UNITS: &[u8] = b"book:fill:units";
const BOOK_FILL_UNITS_POST: &[u8] = b"book:fill:units:post";
const BOOK_FILL_UNITS_VOID: &[u8] = b"book:fill:units:void";
/// The trade's cost-basis projection marker (banking's).
const BOOK_TRADE_POSITION: &[u8] = b"book:trade:position";

/// The `saga_steps.leg` a projection marker is filed under — past every ledger leg.
const PROJECTION_LEG: i32 = 100;

/// The idempotency key of one money step: `<aggregate type>:<id>:<step>`. Derived, never
/// minted, so a redelivery asks banking the same question and gets the original answer.
fn money_key(kind: &str, id: Uuid, step: &str) -> Result<MoneyKey, Outcome> {
	MoneyKey::parse(&format!("{kind}:{}:{step}", id.hyphenated())).map_err(|err| Outcome::park(format!("money key: {err}")))
}

fn subject_ref(kind: &str, id: Uuid) -> Result<SubjectRef, Outcome> {
	SubjectRef::new(kind, id).map_err(|err| Outcome::park(format!("subject ref: {err}")))
}

fn alloc_label(service: &ServiceId) -> Result<AccountLabel, Outcome> {
	AccountLabel::allocation(service).map_err(|err| Outcome::park(format!("allocation account: {err}")))
}

/// A money step that is neither done nor refused: retry what banking could not answer,
/// park what it answered in a way the protocol does not allow for this step.
fn money_failure(step: &str, err: MoneyLedgerError, applied_legs: usize) -> Outcome {
	match err {
		MoneyLedgerError::Unavailable(_) | MoneyLedgerError::Unimplemented { .. } => Outcome::Retry(format!("{step}: {err}")),
		MoneyLedgerError::Refused(_) | MoneyLedgerError::NotFound | MoneyLedgerError::IdempotencyConflict(_) | MoneyLedgerError::Invalid(_) | MoneyLedgerError::Protocol(_) =>
			Outcome::Park {
				reason: format!("{step}: {err}"),
				applied_legs,
			},
	}
}

/// What banking said "no" to, as a saga reads it: a protocol refusal, or a hold / account
/// banking does not know (an account invest never opened holds nothing to pay from).
fn refusal_reason(err: &MoneyLedgerError) -> Option<String> {
	match err {
		MoneyLedgerError::Refused(refusal) => Some(refusal.reason().to_owned()),
		MoneyLedgerError::NotFound => Some("not_found".to_owned()),
		MoneyLedgerError::Unavailable(_)
		| MoneyLedgerError::Unimplemented { .. }
		| MoneyLedgerError::IdempotencyConflict(_)
		| MoneyLedgerError::Invalid(_)
		| MoneyLedgerError::Protocol(_) => None,
	}
}

fn ledger_failure(step: &str, err: LedgerError, applied_legs: usize) -> Outcome {
	match err {
		LedgerError::Unavailable(detail) => Outcome::Retry(format!("{step}: {detail}")),
		LedgerError::Retryable(detail) => Outcome::RetryBounded(format!("{step}: {detail}")),
		LedgerError::InsufficientFunds => Outcome::Park {
			reason: format!("{step}: insufficient funds"),
			applied_legs,
		},
		LedgerError::Conflict(detail) => Outcome::Park {
			reason: format!("{step}: ledger conflict: {detail}"),
			applied_legs,
		},
	}
}

fn decode<T: serde::de::DeserializeOwned>(row: &OutboxRow) -> Result<T, Outcome> {
	serde_json::from_str(&row.payload).map_err(|err| Outcome::park(format!("unplannable event: {err}")))
}

/// The pending transfer and its two completions, built from one description so the three
/// can never disagree on accounts, amount or code.
struct TwoPhase {
	pending: LedgerTransfer,
}

impl TwoPhase {
	fn completion(&self, id: u128, kind: CompletionKind) -> PendingCompletion {
		PendingCompletion {
			id,
			pending_id: self.pending.id,
			kind,
			debit: self.pending.debit.clone(),
			credit: self.pending.credit.clone(),
			amount: self.pending.amount,
			code: self.pending.code,
			reference: self.pending.reference,
		}
	}
}

/// `?` for a step that already decided the event's outcome.
macro_rules! step {
	($expr:expr) => {
		match $expr {
			Ok(value) => value,
			Err(outcome) => return outcome,
		}
	};
}

impl Relay {
	async fn reserve_leg(&self, row: &OutboxRow, leg: i32, role: &str, transfer: &LedgerTransfer, applied: usize) -> Result<(), Outcome> {
		self.ledger.reserve(transfer).await.map_err(|err| ledger_failure(role, err, applied))?;
		self.record(row, leg, role, transfer.id).await
	}

	async fn post_leg(&self, row: &OutboxRow, leg: i32, role: &str, transfer: &LedgerTransfer, applied: usize) -> Result<(), Outcome> {
		self.ledger.post(transfer).await.map_err(|err| ledger_failure(role, err, applied))?;
		self.record(row, leg, role, transfer.id).await
	}

	async fn complete_leg(&self, row: &OutboxRow, leg: i32, role: &str, completion: &PendingCompletion, applied: usize) -> Result<(), Outcome> {
		self.ledger.complete(completion).await.map_err(|err| ledger_failure(role, err, applied))?;
		self.record(row, leg, role, completion.id).await
	}

	/// The step record is load-bearing (reconciliation reads it), so a failed insert retries
	/// the whole event: every leg is idempotent, and the redelivery records the step it owes.
	async fn record(&self, row: &OutboxRow, leg: i32, role: &str, transfer_id: u128) -> Result<(), Outcome> {
		record_saga_step(&self.pool, row.event_id, leg, role, transfer_id)
			.await
			.map_err(|err| Outcome::Retry(format!("saga step record: {err}")))
	}

	// ── subscriptions ─────────────────────────────────────────────────────────

	pub(super) async fn subscription(&self, row: &OutboxRow) -> Outcome {
		let SubscriptionEvent::Reserved {
			subscription_id,
			user,
			service,
			cash,
			nav,
			units,
		} = step!(decode(row));
		let id = subscription_id.raw();
		let saga = match subscription_saga(&self.pool, id).await {
			Ok(Some(saga)) => saga,
			Ok(None) => return Outcome::park("subscription row is missing".into()),
			Err(err) => return Outcome::Retry(format!("subscription read: {err}")),
		};
		match saga.state.as_str() {
			"reserved" => {}
			// A redelivery after the saga finished: its outcome is on the row.
			"settled" | "failed" => return Outcome::Done,
			other => return Outcome::park(format!("subscription is {other}, not reserved")),
		}
		let Some(hold) = saga.hold_id else {
			return Outcome::park("reserved subscription names no hold".into());
		};
		let scope = step!(ScopeId::parse(&saga.scope).map_err(|err| Outcome::park(format!("subscription scope: {err}"))));
		let reference = id.as_u128();
		let mint = TwoPhase {
			pending: LedgerTransfer {
				id: tid(id, SUBSCRIBE_MINT),
				debit: LedgerAccountKey::UserShares(service.clone(), user),
				credit: LedgerAccountKey::SharesOutstanding(service.clone()),
				amount: units.base_units(),
				code: TransferCode::ShareMint,
				reference,
			},
		};
		step!(self.reserve_leg(row, 0, "subscribe_mint_pending", &mint.pending, 0).await);

		let capture = CaptureRequest {
			scope: scope.clone(),
			hold_id: HoldId(hold.clone()),
			to: CaptureTo::Account(step!(alloc_label(&service))),
			amount: Some(cash),
			key: step!(money_key("subscription", id, "capture")),
			subject_ref: step!(subject_ref("subscription", id)),
		};
		match self.money.capture(&capture).await {
			Ok(_) => {}
			Err(err) => {
				let Some(reason) = refusal_reason(&err) else {
					return money_failure("subscription capture", err, 1);
				};
				// Roll back: release what the hold still holds, then the pending units.
				step!(self.void_hold(&scope, &hold, "subscription", id).await);
				step!(
					self.complete_leg(row, 1, "subscribe_mint_void", &mint.completion(tid(id, SUBSCRIBE_MINT_VOID), CompletionKind::Void), 1)
						.await
				);
				return match fail_subscription(&self.pool, id, &format!("capture refused: {reason}")).await {
					Ok(()) => Outcome::Done,
					Err(err) => Outcome::Retry(format!("subscription failed stamp: {err}")),
				};
			}
		}
		step!(
			self.complete_leg(row, 1, "subscribe_mint_post", &mint.completion(tid(id, SUBSCRIBE_MINT_POST), CompletionKind::Post), 1)
				.await
		);
		match settle_subscription(&self.pool, row, user, &service, cash, nav, units).await {
			Ok(()) => Outcome::Done,
			Err(err) => Outcome::Retry(format!("subscribe cost-basis projection: {err}")),
		}
	}

	/// Release whatever `hold` still holds. A hold that is no longer active (expired,
	/// already voided or captured) has nothing to release, and is not a failure.
	async fn void_hold(&self, scope: &ScopeId, hold: &str, kind: &str, id: Uuid) -> Result<(), Outcome> {
		let void = VoidRequest {
			scope: scope.clone(),
			hold_id: HoldId(hold.to_owned()),
			key: money_key(kind, id, "void")?,
			subject_ref: subject_ref(kind, id)?,
		};
		match self.money.void(&void).await {
			Ok(_) | Err(MoneyLedgerError::Refused(crate::ports::money::Refusal::HoldNotActive)) => Ok(()),
			Err(err) => Err(money_failure(&format!("{kind} void"), err, 0)),
		}
	}

	// ── redemptions ───────────────────────────────────────────────────────────

	pub(super) async fn redemption(&self, row: &OutboxRow) -> Outcome {
		let event: RedemptionEvent = step!(decode(row));
		let id = row.aggregate_id;
		let reference = id.as_u128();
		let burn = |user: UserId, service: &ServiceId, units: Shares| TwoPhase {
			pending: LedgerTransfer {
				id: tid(id, BURN_RESERVE),
				debit: LedgerAccountKey::SharesOutstanding(service.clone()),
				credit: LedgerAccountKey::UserShares(service.clone(), user),
				amount: units.base_units(),
				code: TransferCode::ShareBurn,
				reference,
			},
		};
		match event {
			RedemptionEvent::Requested { user, service, units, .. } => {
				step!(self.reserve_leg(row, 0, "redeem_reserve", &burn(user, &service, units).pending, 0).await);
				Outcome::Done
			}
			RedemptionEvent::Failed { user, service, units, .. } => {
				step!(
					self.complete_leg(row, 0, "redeem_void", &burn(user, &service, units).completion(tid(id, BURN_VOID_FAIL), CompletionKind::Void), 0)
						.await
				);
				Outcome::Done
			}
			RedemptionEvent::Cancelled { user, service, units, .. } => {
				step!(
					self.complete_leg(row, 0, "redeem_void", &burn(user, &service, units).completion(tid(id, BURN_VOID_CANCEL), CompletionKind::Void), 0)
						.await
				);
				Outcome::Done
			}
			RedemptionEvent::Settled { user, service, units, cash, .. } => {
				let state: Option<String> = match sqlx::query_scalar("SELECT state FROM redemptions WHERE id = $1").bind(id).fetch_optional(&self.pool).await {
					Ok(state) => state,
					Err(err) => return Outcome::Retry(format!("redemption read: {err}")),
				};
				match state.as_deref() {
					Some("settling") => {}
					Some("completed" | "failed") => return Outcome::Done,
					Some(other) => return Outcome::park(format!("redemption is {other}, not settling")),
					None => return Outcome::park("redemption row is missing".into()),
				}
				let (scope, subject) = match scope_and_subject(&self.pool, &service, user).await {
					Ok(Some(found)) => found,
					Ok(None) => return Outcome::park("redemption: the holder has no banking subject".into()),
					Err(err) => return Outcome::Retry(format!("redemption scope read: {err}")),
				};
				let scope = step!(ScopeId::parse(&scope).map_err(|err| Outcome::park(format!("allocation scope: {err}"))));
				let payout = PayoutRequest {
					scope,
					from: step!(alloc_label(&service)),
					lines: vec![PayoutLine {
						auth_subject: subject,
						amount: cash,
					}],
					reason: PayoutReason::Redemption,
					key: step!(money_key("redemption", id, "payout")),
					subject_ref: step!(subject_ref("redemption", id)),
				};
				let burn = burn(user, &service, units);
				match self.money.payout(&payout).await {
					Ok(_) => {}
					Err(err) => {
						let Some(reason) = refusal_reason(&err) else {
							return money_failure("redemption payout", err, 0);
						};
						// TODO(banking RequestPayout): past the automatic ceilings
						// (`approval_required`) the payout is banking's to request from the
						// owners; until invest asks, the redemption fails and the units go back.
						step!(
							self.complete_leg(row, 0, "redeem_void", &burn.completion(tid(id, BURN_VOID_REFUSED), CompletionKind::Void), 0)
								.await
						);
						return match fail_redemption(&self.pool, id, &format!("payout refused: {reason}")).await {
							Ok(()) => Outcome::Done,
							Err(err) => Outcome::Retry(format!("redemption failed stamp: {err}")),
						};
					}
				}
				// The cash has left; the burn can no longer be refused — its pending already
				// holds the units.
				step!(self.complete_leg(row, 0, "redeem_burn", &burn.completion(tid(id, BURN_SETTLE), CompletionKind::Post), 1).await);
				match complete_redemption(&self.pool, row, user, &service, units).await {
					Ok(()) => Outcome::Done,
					Err(err) => Outcome::Retry(format!("redemption cost-basis reduction: {err}")),
				}
			}
		}
	}

	// ── the book ──────────────────────────────────────────────────────────────

	pub(super) async fn book(&self, row: &OutboxRow) -> Outcome {
		let event: BookEvent = step!(decode(row));
		let reference = row.aggregate_id.as_u128();
		match event {
			// A buy's cash was held in banking before the order was written; there is nothing
			// to move.
			BookEvent::OrderPlaced { locked: Locked::Cash(_), .. } => Outcome::Done,
			BookEvent::OrderPlaced {
				order_id,
				service,
				user,
				locked: Locked::Units(units),
				..
			} => {
				let lock = LedgerTransfer {
					id: tid(order_id.raw(), BOOK_LOCK),
					debit: LedgerAccountKey::BookShares(service.clone(), user),
					credit: LedgerAccountKey::UserShares(service, user),
					amount: units.base_units(),
					code: TransferCode::BookLock,
					reference,
				};
				step!(self.post_leg(row, 0, "book_lock", &lock, 0).await);
				Outcome::Done
			}
			BookEvent::OrderReleased {
				order_id,
				service,
				user,
				released: Locked::Units(units),
				..
			} => {
				let release = LedgerTransfer {
					id: tid(order_id.raw(), BOOK_RELEASE),
					debit: LedgerAccountKey::UserShares(service.clone(), user),
					credit: LedgerAccountKey::BookShares(service, user),
					amount: units.base_units(),
					code: TransferCode::BookRelease,
					reference,
				};
				step!(self.post_leg(row, 0, "book_release", &release, 0).await);
				Outcome::Done
			}
			BookEvent::OrderReleased {
				order_id,
				released: Locked::Cash(_),
				..
			} => {
				let escrow = match order_escrow(&self.pool, order_id.raw()).await {
					Ok(Some(escrow)) => escrow,
					Ok(None) => return Outcome::park("released buy order names no hold".into()),
					Err(err) => return Outcome::Retry(format!("order escrow read: {err}")),
				};
				step!(self.void_hold(&escrow.0, &escrow.1, "book_order", order_id.raw()).await);
				Outcome::Done
			}
			BookEvent::TradeExecuted {
				trade_id,
				service,
				buyer,
				seller,
				buy_order_id,
				taker_side,
				size,
				notional,
				fee,
				nav,
				..
			} => {
				let id = trade_id.raw();
				let (scope, hold) = match order_escrow(&self.pool, buy_order_id.raw()).await {
					Ok(Some(escrow)) => escrow,
					Ok(None) => return Outcome::park("the buy order of this trade names no hold".into()),
					Err(err) => return Outcome::Retry(format!("order escrow read: {err}")),
				};
				let seller_subject = match auth_subject(&self.pool, seller).await {
					Ok(Some(subject)) => subject,
					Ok(None) => return Outcome::park("the seller has no banking subject".into()),
					Err(err) => return Outcome::Retry(format!("seller subject read: {err}")),
				};
				let units = TwoPhase {
					pending: LedgerTransfer {
						id: tid(id, BOOK_FILL_UNITS),
						debit: LedgerAccountKey::UserShares(service.clone(), buyer),
						credit: LedgerAccountKey::BookShares(service.clone(), seller),
						amount: size.base_units(),
						code: TransferCode::BookFill,
						reference,
					},
				};
				step!(self.reserve_leg(row, 0, "book_fill_pending", &units.pending, 0).await);

				// A selling taker pays the fee out of what the buyer pays; a buying taker on
				// top of it (its hold reserved notional plus the fee).
				let seller_fee = if taker_side == Side::Sell { fee } else { Usdt::ZERO };
				let to_seller = step!(notional.checked_sub(seller_fee).ok_or_else(|| Outcome::park("the taker fee exceeds the notional".into())));
				if !to_seller.is_zero() {
					let capture = CaptureRequest {
						scope: scope.clone(),
						hold_id: HoldId(hold.clone()),
						to: CaptureTo::User(seller_subject),
						amount: Some(to_seller),
						key: step!(money_key("book_trade", id, "cash")),
						subject_ref: step!(subject_ref("book_trade", id)),
					};
					if let Err(err) = self.money.capture(&capture).await {
						let Some(reason) = refusal_reason(&err) else {
							return money_failure("fill cash capture", err, 1);
						};
						// The buyer's hold no longer stands behind the order (voided by a
						// revoked mandate, or banking lost it): no cash, so no units. The trade
						// stays on the tape and the orders filled — that is the operator's.
						step!(
							self.complete_leg(row, 1, "book_fill_void", &units.completion(tid(id, BOOK_FILL_UNITS_VOID), CompletionKind::Void), 1)
								.await
						);
						return Outcome::park(format!("fill cash capture refused: {reason} — units voided, the trade needs an operator"));
					}
				}
				step!(
					self.complete_leg(row, 1, "book_fill_post", &units.completion(tid(id, BOOK_FILL_UNITS_POST), CompletionKind::Post), 1)
						.await
				);
				if !fee.is_zero() {
					step!(self.collect_fee(&scope, &service, &hold, fee, id).await);
				}
				match project_trade(&self.pool, row, &service, buyer, seller, taker_side, size, notional, fee, nav).await {
					Ok(()) => Outcome::Done,
					Err(err) => Outcome::Retry(format!("book trade projection: {err}")),
				}
			}
		}
	}

	/// The taker's fee on a fill: captured out of the buyer's hold into the product's
	/// account, then swept on into the scope's fee route (`(<scope>, fee)`, which banking
	/// forwards to `(invest, alloc:fee)` from every scope, `sa` included). Both after the parties
	/// settled, so a refusal here costs the fee route, never a party.
	async fn collect_fee(&self, scope: &ScopeId, service: &ServiceId, hold: &str, fee: Usdt, trade: Uuid) -> Result<(), Outcome> {
		let label = alloc_label(service)?;
		let capture = CaptureRequest {
			scope: scope.clone(),
			hold_id: HoldId(hold.to_owned()),
			to: CaptureTo::Account(label.clone()),
			amount: Some(fee),
			key: money_key("book_trade", trade, "fee")?,
			subject_ref: subject_ref("book_trade", trade)?,
		};
		self.money.capture(&capture).await.map_err(|err| money_failure("fill fee capture", err, 2))?;
		let sweep = FeeSweepRequest {
			scope: scope.clone(),
			from: label,
			amount: fee,
			key: money_key("book_trade", trade, "fee_sweep")?,
			subject_ref: subject_ref("book_trade", trade)?,
		};
		self.money.sweep_fees(&sweep).await.map_err(|err| money_failure("fill fee sweep", err, 2))?;
		Ok(())
	}

	// ── fees ──────────────────────────────────────────────────────────────────

	pub(super) async fn fee(&self, row: &OutboxRow) -> Outcome {
		let event: FeeEvent = step!(decode(row));
		let reference = row.aggregate_id.as_u128();
		match event {
			// In kind, entirely on invest's ledger: units between two holders, supply
			// unchanged. A single leg, so it takes the event id itself, as banking's did.
			FeeEvent::Charged { user, service, units, .. } => {
				let clawback = LedgerTransfer {
					id: row.event_id.as_u128(),
					debit: LedgerAccountKey::FeeShares(service.clone()),
					credit: LedgerAccountKey::UserShares(service, user),
					amount: units.base_units(),
					code: TransferCode::FeeClawback,
					reference,
				};
				step!(self.post_leg(row, 0, "fee_clawback", &clawback, 0).await);
				Outcome::Done
			}
			FeeEvent::SharesSettled {
				settlement_id,
				service,
				units,
				cash,
				..
			} => {
				let id = settlement_id.raw();
				let scope: Option<String> = match sqlx::query_scalar("SELECT scope FROM allocations WHERE service = $1")
					.bind(service.as_str())
					.fetch_optional(&self.pool)
					.await
				{
					Ok(scope) => scope,
					Err(err) => return Outcome::Retry(format!("allocation scope read: {err}")),
				};
				let Some(scope) = scope else {
					return Outcome::park("fee settlement: the allocation is not registered".into());
				};
				let sweep = FeeSweepRequest {
					scope: step!(ScopeId::parse(&scope).map_err(|err| Outcome::park(format!("allocation scope: {err}")))),
					from: step!(alloc_label(&service)),
					amount: cash,
					key: step!(money_key("fee_settlement", id, "sweep")),
					subject_ref: step!(subject_ref("fee_settlement", id)),
				};
				// Pay first: the sweep is the leg banking may refuse, and a refused one must
				// leave the fee class unburned.
				if let Err(err) = self.money.sweep_fees(&sweep).await {
					return money_failure("fee settlement sweep", err, 0);
				}
				let burn = LedgerTransfer {
					id: tid(id, FEE_SETTLE_BURN),
					debit: LedgerAccountKey::SharesOutstanding(service.clone()),
					credit: LedgerAccountKey::FeeShares(service),
					amount: units.base_units(),
					code: TransferCode::FeeSettle,
					reference,
				};
				step!(self.post_leg(row, 0, "fee_burn", &burn, 1).await);
				Outcome::Done
			}
		}
	}
}

/// What the subscription saga needs of its row.
struct SubscriptionSaga {
	state: String,
	scope: String,
	hold_id: Option<String>,
}

async fn subscription_saga(pool: &PgPool, id: Uuid) -> Result<Option<SubscriptionSaga>, sqlx::Error> {
	let row: Option<(String, String, Option<String>)> = sqlx::query_as("SELECT state, scope, hold_id FROM subscriptions WHERE id = $1")
		.bind(id)
		.fetch_optional(pool)
		.await?;
	Ok(row.map(|(state, scope, hold_id)| SubscriptionSaga { state, scope, hold_id }))
}

async fn fail_subscription(pool: &PgPool, id: Uuid, reason: &str) -> Result<(), sqlx::Error> {
	sqlx::query("UPDATE subscriptions SET state = 'failed', failure_reason = $2, updated_at = now() WHERE id = $1 AND state = 'reserved'")
		.bind(id)
		.bind(reason)
		.execute(pool)
		.await?;
	Ok(())
}

/// Stamp a subscription `settled` and apply its cost-basis projection (`cost_basis += cash`,
/// `units += units`, high-water mark = max) — once, under a per-event `saga_steps` marker,
/// in one transaction, after the units posted. Never on the open path, where a later
/// refused capture would leave a phantom `fund_positions` row (basis without units).
async fn settle_subscription(pool: &PgPool, row: &OutboxRow, user: UserId, service: &ServiceId, cash: Usdt, nav: invest_domain::money::Nav, units: Shares) -> Result<(), sqlx::Error> {
	let mut tx = pool.begin().await?;
	if mark_projection(&mut tx, row, "subscribe_position", SUBSCRIBE_POSITION).await? {
		// The basis is about to move, so whatever the OLD basis accrued has to be settled
		// first — see [`fee_accrual`].
		fee_accrual::carry_accrual(&mut tx, user.raw(), service.as_str(), now_unix())
			.await
			.map_err(|err| sqlx::Error::Protocol(format!("carry fee accrual before subscribe basis change: {err}")))?;
		sqlx::query(
			"INSERT INTO fund_positions (user_id, service, cost_basis, units, high_water_mark) VALUES ($1, $2, $3, $4, $5) \
			 ON CONFLICT (user_id, service) DO UPDATE SET \
			 cost_basis = (fund_positions.cost_basis::numeric + EXCLUDED.cost_basis::numeric)::text, \
			 units = (fund_positions.units::numeric + EXCLUDED.units::numeric)::text, \
			 high_water_mark = GREATEST(fund_positions.high_water_mark::numeric, EXCLUDED.high_water_mark::numeric)::text, \
			 updated_at = now()",
		)
		.bind(user.raw())
		.bind(service.as_str())
		.bind(cash.base_units().to_string())
		.bind(units.base_units().to_string())
		.bind(nav.base_units().to_string())
		.execute(&mut *tx)
		.await?;
	}
	sqlx::query("UPDATE subscriptions SET state = 'settled', updated_at = now() WHERE id = $1 AND state = 'reserved'")
		.bind(row.aggregate_id)
		.execute(&mut *tx)
		.await?;
	tx.commit().await
}

async fn fail_redemption(pool: &PgPool, id: Uuid, reason: &str) -> Result<(), sqlx::Error> {
	sqlx::query("UPDATE redemptions SET state = 'failed', failure_reason = $2, updated_at = now() WHERE id = $1 AND state = 'settling'")
		.bind(id)
		.bind(reason)
		.execute(pool)
		.await?;
	Ok(())
}

/// Stamp a redemption `completed` and reduce the holder's cost basis — once, under the
/// saga's marker, in one transaction, after the cash left and the burn posted.
async fn complete_redemption(pool: &PgPool, row: &OutboxRow, user: UserId, service: &ServiceId, units: Shares) -> Result<(), sqlx::Error> {
	let mut tx = pool.begin().await?;
	if mark_projection(&mut tx, row, "redeem_position", REDEEM_POSITION).await? {
		redemptions::reduce_cost_basis(&mut tx, user, service, units, now_unix())
			.await
			.map_err(|err| sqlx::Error::Protocol(format!("reduce the redeemed cost basis: {err}")))?;
	}
	sqlx::query("UPDATE redemptions SET state = 'completed', updated_at = now() WHERE id = $1 AND state = 'settling'")
		.bind(row.aggregate_id)
		.execute(&mut *tx)
		.await?;
	tx.commit().await
}

/// Claim this event's projection marker on the open transaction: `true` the first time,
/// `false` on a redelivery that already applied it. The marker's id is derived from the
/// aggregate with its own salt, distinct from every transfer salt, so the
/// `saga_steps.tb_transfer_id` unique key never aliases a real transfer.
async fn mark_projection(tx: &mut sqlx::PgConnection, row: &OutboxRow, role: &str, salt: &[u8]) -> Result<bool, sqlx::Error> {
	let marker_id = tid(row.aggregate_id, salt);
	let marked = sqlx::query("INSERT INTO saga_steps (event_id, leg, role, tb_transfer_id) VALUES ($1, $2, $3, $4) ON CONFLICT (event_id, leg) DO NOTHING")
		.bind(row.event_id)
		.bind(PROJECTION_LEG)
		.bind(role)
		.bind(&marker_id.to_be_bytes()[..])
		.execute(&mut *tx)
		.await?
		.rows_affected();
	Ok(marked == 1)
}

/// The scope a product's cash lives in, and the subject banking names `user` by.
async fn scope_and_subject(pool: &PgPool, service: &ServiceId, user: UserId) -> Result<Option<(String, String)>, sqlx::Error> {
	let scope: Option<String> = sqlx::query_scalar("SELECT scope FROM allocations WHERE service = $1")
		.bind(service.as_str())
		.fetch_optional(pool)
		.await?;
	let subject = auth_subject(pool, user).await?;
	Ok(scope.zip(subject))
}

async fn auth_subject(pool: &PgPool, user: UserId) -> Result<Option<String>, sqlx::Error> {
	let subject: Option<Option<String>> = sqlx::query_scalar("SELECT auth_subject FROM users WHERE id = $1").bind(user.raw()).fetch_optional(pool).await?;
	Ok(subject.flatten())
}

/// A buy order's escrow: the scope it settles in and banking's hold.
async fn order_escrow(pool: &PgPool, order: Uuid) -> Result<Option<(ScopeId, String)>, sqlx::Error> {
	let row: Option<(String, Option<String>)> = sqlx::query_as("SELECT scope, hold_id FROM book_orders WHERE id = $1").bind(order).fetch_optional(pool).await?;
	let Some((scope, Some(hold))) = row else {
		return Ok(None);
	};
	let scope = ScopeId::parse(&scope).map_err(|err| sqlx::Error::Protocol(format!("book_orders.scope: {err}")))?;
	Ok(Some((scope, hold)))
}

/// Apply a trade's cost-basis projection to both parties, idempotently under a per-event
/// `saga_steps` marker, in one transaction. The buyer's basis grows by what they paid —
/// the notional plus the fee when they were the taker — and their units by the size,
/// with the high-water mark blended at the fund's NAV of the moment (the accounting
/// price, not the quote: performance is still measured against NAV, so the mark has to
/// be one). The seller's basis shrinks pro rata, as a redemption's does.
///
/// The seller's reduction clamps at zero rather than refusing: a refusal would be a
/// `Retry` that wedges the single-worker relay on a projection that can never catch up,
/// and every unit a seller can lock came through this same relay in an earlier `seq` — so
/// a short projection is a fee clawback or a drift already reconciled elsewhere, and an
/// understated basis is the recoverable side of that.
// One trade's figures, read off its event; a struct would exist for this call alone.
#[allow(clippy::too_many_arguments)]
async fn project_trade(
	pool: &PgPool,
	row: &OutboxRow,
	service: &ServiceId,
	buyer: UserId,
	seller: UserId,
	taker_side: Side,
	size: Shares,
	notional: Usdt,
	fee: Usdt,
	nav: invest_domain::money::Nav,
) -> Result<(), sqlx::Error> {
	let mut tx = pool.begin().await?;
	if mark_projection(&mut tx, row, "book_trade_position", BOOK_TRADE_POSITION).await? {
		let buyer_paid = match taker_side {
			Side::Buy => notional.checked_add(fee).ok_or_else(|| sqlx::Error::Protocol("trade cost overflows".into()))?,
			Side::Sell => notional,
		};
		let now = now_unix();
		// Both positions settle what their old basis accrued before it moves (see
		// [`fee_accrual`]); the buyer's row may not exist yet, which is fine.
		fee_accrual::carry_accrual(&mut tx, buyer.raw(), service.as_str(), now)
			.await
			.map_err(|err| sqlx::Error::Protocol(format!("carry fee accrual before buy basis change: {err}")))?;
		sqlx::query(
			"INSERT INTO fund_positions (user_id, service, cost_basis, units, high_water_mark) VALUES ($1, $2, $3, $4, $5) \
			 ON CONFLICT (user_id, service) DO UPDATE SET \
			 cost_basis = (fund_positions.cost_basis::numeric + EXCLUDED.cost_basis::numeric)::text, \
			 units = (fund_positions.units::numeric + EXCLUDED.units::numeric)::text, \
			 high_water_mark = GREATEST(fund_positions.high_water_mark::numeric, EXCLUDED.high_water_mark::numeric)::text, \
			 updated_at = now()",
		)
		.bind(buyer.raw())
		.bind(service.as_str())
		.bind(buyer_paid.base_units().to_string())
		.bind(size.base_units().to_string())
		.bind(nav.base_units().to_string())
		.execute(&mut *tx)
		.await?;
		fee_accrual::carry_accrual(&mut tx, seller.raw(), service.as_str(), now)
			.await
			.map_err(|err| sqlx::Error::Protocol(format!("carry fee accrual before sell basis change: {err}")))?;
		sqlx::query(
			"UPDATE fund_positions SET \
			 cost_basis = CASE WHEN units::numeric > $3::numeric THEN trunc(cost_basis::numeric * (units::numeric - $3::numeric) / units::numeric)::text ELSE '0' END, \
			 units = GREATEST(units::numeric - $3::numeric, 0)::text, \
			 updated_at = now() \
			 WHERE user_id = $1 AND service = $2",
		)
		.bind(seller.raw())
		.bind(service.as_str())
		.bind(size.base_units().to_string())
		.execute(&mut *tx)
		.await?;
	}
	tx.commit().await
}
