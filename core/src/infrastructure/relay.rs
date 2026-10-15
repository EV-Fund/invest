//! Outbox relay — the saga dispatcher.
//!
//! A single worker drains undispatched [`outbox`] rows in strict `seq` order and applies
//! each to TigerBeetle **after** the control-plane commit (Write-Last). "Single worker" is
//! an enforced invariant, not a deploy assumption: [`Relay::run`] holds a fixed-key session
//! `pg_advisory_lock` and only drains while held, so a second invest-core instance blocks
//! on the lock and never touches the outbox (see `OUTBOX_LOCK_KEY`); losing the lock's
//! connection re-enters that acquisition (with backoff) rather than exiting, so a DB blip
//! never cascades into a whole-process teardown. The lock's liveness is re-probed on its
//! own connection before **every** dispatched row (a session lock lives exactly as long
//! as its backend), so a backend reaped mid-catch-up stops the drain within one row.
//!
//! Delivery is at-least-once, so every op is idempotent: ledger transfer ids are
//! deterministic (derived from the aggregate id and a per-leg salt), and the gateway
//! treats `Exists`/`AlreadyPosted`/`AlreadyVoided` as success.
//!
//! Strict order means a reservation's pending is always applied before its completion.
//! Transient failures (`Unavailable`/`Retryable`) stop the cycle and retry from the same
//! `seq`; a genuine inconsistency or `InsufficientFunds` is **parked** — moved to a
//! distinct `parked_at` terminal state (NOT `dispatched_at`) so the row stays queryable
//! yet is excluded from the drain, with a loud (Sentry-shipped) `error!` + `last_error`.
//! One bad event can't wedge the queue; a park *after* an earlier leg posted is flagged
//! half-applied (`compensated_at`).
//!
//! Moved from banking 9ffafce3 (`piggybank/core/src/infrastructure/relay.rs`). An in-kind
//! issuance is planned here as banking planned it; every event with a cash leg — a
//! subscription, a redemption, a fill, a fee settlement — is a saga across two ledgers,
//! carried out in [`sagas`]: the units on invest's TigerBeetle, the cash through the
//! [`MoneyLedger`] (banking), each step idempotent by a deterministic transfer id or a
//! money key derived from the aggregate, so a redelivery after any crash repeats nothing.
//! A money call that cannot be made right now (banking unreachable, or not serving the RPC
//! yet) retries from the same `seq`, exactly as an unreachable TigerBeetle does.

use std::{sync::Arc, time::Duration};

use invest_domain::{
	architecture::DomainEvent,
	balance::{LedgerAccountKey, TransferCode},
	book::BookEvent,
	fees::FeeEvent,
	issuance::{IssuanceEvent, IssuanceSource},
	redemptions::RedemptionEvent,
	subscriptions::SubscriptionEvent,
};
use sqlx::{PgPool, pool::PoolConnection, postgres::Postgres};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
	infrastructure::{
		book, issuance,
		outbox::{self, OutboxRow},
	},
	ports::{
		ledger::{Ledger, LedgerError, LedgerTransfer},
		money::MoneyLedger,
	},
};

mod sagas;

/// Salt for an in-kind issuance's single posted leg, the mint `Dr <holder shares> / Cr
/// shares-outstanding`. Derived from the issuance id (not the event id) so an operator
/// can recompute the transfer from the row alone when reconciling supply against the
/// issuance table. FROZEN — banking's salt, so a transfer moved over with the data wave
/// keeps the id its row recomputes.
const ISSUE_MINT: &[u8] = b"issue:mint";
/// The same, for a retirement: the burn `Dr shares-outstanding / Cr <holder shares>`.
/// Its own salt for the same reason — the transfer id alone must say which way the row
/// moved the supply.
const ISSUE_RETIRE: &[u8] = b"issue:retire";

/// Bound on consecutive `RetryBounded` attempts before a never-resolving retryable (a
/// completion whose pending was itself parked, so it can never be found) is parked
/// rather than wedging the single-worker queue forever.
const MAX_RETRYABLE_ATTEMPTS: i32 = 25;

/// Fixed key for the session-level `pg_advisory_lock` the relay holds while draining.
/// The whole ordering argument (strict `seq`, reserve-before-complete) is valid *only*
/// with a single drainer; this lock makes that singleton an enforced invariant rather
/// than a deploy assumption. The value is an arbitrary stable constant (ASCII
/// "EVINOBX_"); only stability matters — changing it would let two cohorts drain
/// concurrently across a deploy that mixed old and new keys.
const OUTBOX_LOCK_KEY: i64 = 0x4556_494e_4f42_585f_u64 as i64;

/// Salt deriving an issuance's `applied` stamp + cost-basis projection marker, distinct
/// from any TB transfer salt so the `saga_steps.tb_transfer_id` unique constraint never
/// aliases a real transfer.
const ISSUE_APPLIED: &[u8] = b"issue:applied";

/// The relay task: drains the outbox to the ledgers. Cloneable handles (`pool`, `ledger`,
/// `money`, `notify`) so command handlers can `notify` it to dispatch promptly.
pub struct Relay {
	pool: PgPool,
	ledger: Arc<dyn Ledger>,
	money: Arc<dyn MoneyLedger>,
	notify: Arc<Notify>,
}

impl Relay {
	pub fn new(pool: PgPool, ledger: Arc<dyn Ledger>, money: Arc<dyn MoneyLedger>, notify: Arc<Notify>) -> Self {
		Self { pool, ledger, money, notify }
	}

	/// Run until `shutdown` is cancelled. Take the singleton outbox lock on a dedicated
	/// connection (blocking until acquired — a second instance idles here, owning
	/// nothing), then drain the backlog and wait for a nudge or a poll-fallback. The held
	/// connection keeps the session-level lock for as long as it lives; dropping it
	/// (process exit, or a lost backend) releases it so a standby can take over.
	///
	/// A transient DB failure — the lock acquisition erroring, or the lock-holding
	/// connection dropping mid-run — must NOT return: the composition root cancels every
	/// sibling task when any branch completes, so an early return turns a DB blip into a
	/// full money-plane teardown. Instead the outer loop re-acquires the lock with capped
	/// exponential backoff (a standby may legitimately win it first — we then block on it,
	/// which is exactly the singleton hand-off working).
	///
	/// Graceful shutdown is cooperative: cancellation is observed during the lock/backoff
	/// waits, at the wait points between drains, and at every batch boundary inside a
	/// drain — never mid-batch. Each batch runs to completion before exit (it is already
	/// crash-safe to stop between rows), so an in-flight dispatch is never torn down
	/// partway, and a multi-minute catch-up backlog can no longer delay a deploy past the
	/// current batch.
	pub async fn run(self, shutdown: CancellationToken) {
		const MAX_BACKOFF: Duration = Duration::from_secs(30);
		let mut backoff = Duration::from_millis(500);
		'acquire: loop {
			let mut lock = tokio::select! {
				biased;
				() = shutdown.cancelled() => return,
				lock = self.acquire_outbox_lock() => match lock {
					Ok(lock) => lock,
					Err(err) => {
						error!("relay: could not acquire the outbox lock (retrying in {backoff:?}): {err}");
						tokio::select! {
							() = shutdown.cancelled() => return,
							() = tokio::time::sleep(backoff) => {},
						}
						backoff = (backoff * 2).min(MAX_BACKOFF);
						continue 'acquire;
					}
				},
			};
			backoff = Duration::from_millis(500);
			info!("relay: acquired the outbox lock — draining as the singleton worker");
			loop {
				// Drain batch-by-batch so the singleton invariant is re-validated while
				// catching up: `drain_batch` probes the lock connection before every row,
				// and cancellation is observed here between batches. A long catch-up drain
				// otherwise never re-checks the lock — a reaped idle backend releases it, a
				// standby wins it, and two lockless drainers race the same `Dispatched`
				// rows (the on-chain double-send).
				let throttle = loop {
					match self.drain_batch(Some(&mut lock)).await {
						DrainStep::Drained => break false,
						DrainStep::Throttle => break true,
						DrainStep::LostLock => {
							error!("relay: lost the outbox lock mid-drain — re-acquiring");
							// Close, never drop: a drop returns the connection to the pool, and
							// if the probe failed non-fatally (a cancelled query, not a dead
							// backend) that pooled session would keep holding the advisory lock
							// forever, wedging the re-acquisition below against ourselves.
							let _ = lock.close().await;
							continue 'acquire;
						}
						DrainStep::More =>
							if shutdown.is_cancelled() {
								info!("relay: shutdown requested — batch complete, stopping");
								return;
							},
					}
				};
				if shutdown.is_cancelled() {
					info!("relay: shutdown requested — drain iteration complete, stopping");
					return;
				}
				let wait = if throttle { Duration::from_secs(2) } else { Duration::from_millis(500) };
				tokio::select! {
					() = shutdown.cancelled() => {
						info!("relay: shutdown requested — stopping");
						return;
					},
					() = self.notify.notified() => {},
					() = tokio::time::sleep(wait) => {},
				}
				// Touch the lock-holding connection so a dropped backend surfaces here rather
				// than letting a lockless relay silently keep draining. Postgres released the
				// lock with the dead session, so re-acquiring (not exiting) is the recovery.
				if let Err(err) = sqlx::query("SELECT 1").execute(lock.as_mut()).await {
					error!("relay: lost the outbox lock connection — re-acquiring: {err}");
					// Same close-never-drop rule as the mid-drain fence above.
					let _ = lock.close().await;
					continue 'acquire;
				}
			}
		}
	}

	/// Block on a dedicated pooled connection until the fixed-key session advisory lock
	/// is held, then return that connection — its lifetime *is* the lock's. The first
	/// instance returns immediately; any other blocks inside Postgres until the holder's
	/// connection closes.
	pub async fn acquire_outbox_lock(&self) -> Result<PoolConnection<Postgres>, sqlx::Error> {
		let mut conn = self.pool.acquire().await?;
		sqlx::query("SELECT pg_advisory_lock($1)").bind(OUTBOX_LOCK_KEY).execute(conn.as_mut()).await?;
		Ok(conn)
	}

	/// Non-blocking sibling of [`acquire_outbox_lock`](Self::acquire_outbox_lock): take
	/// the lock if free (returning the holding connection), else `None` — used by tests
	/// to observe that a held lock makes a second drainer back off without blocking.
	pub async fn try_acquire_outbox_lock(&self) -> Result<Option<PoolConnection<Postgres>>, sqlx::Error> {
		let mut conn = self.pool.acquire().await?;
		let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)").bind(OUTBOX_LOCK_KEY).fetch_one(conn.as_mut()).await?;
		Ok(acquired.then_some(conn))
	}

	/// Drain the current backlog in `seq` order. Returns `true` if it stopped on a
	/// transient failure (caller should back off before retrying). Unfenced — no lock
	/// probe between rows — so only for tests, which own the sole drainer by
	/// construction; [`run`](Self::run) always drains through the fenced
	/// [`drain_batch`](Self::drain_batch). Public so integration tests can apply
	/// committed events deterministically (one call processes the whole backlog).
	pub async fn drain(&self) -> bool {
		loop {
			match self.drain_batch(None).await {
				DrainStep::Drained => return false,
				DrainStep::Throttle => return true,
				DrainStep::More => {}
				DrainStep::LostLock => unreachable!("no fence connection was passed"),
			}
		}
	}

	/// Apply at most one batch (128 rows) of the backlog in `seq` order, probing `fence`
	/// — the lock-holding connection — before every row. A session advisory lock lives
	/// exactly as long as its backend, so a successful probe proves this relay held the
	/// lock *at probe time*; a failed one means Postgres already freed the lock (a
	/// standby may be draining) and the caller must stop **immediately**: two drainers
	/// would both apply the same row. The probe narrows the lockless overlap from a whole catch-up drain
	/// to the one dispatch in flight when the backend dies; closing that residual window
	/// needs a fencing token checked at `mark_dispatched`, a follow-up. Public for the
	/// integration tests that exercise the fence.
	pub async fn drain_batch(&self, mut fence: Option<&mut PoolConnection<Postgres>>) -> DrainStep {
		let batch = match outbox::next_batch(&self.pool, 128).await {
			Ok(batch) => batch,
			Err(err) => {
				warn!("relay: reading the outbox failed: {err}");
				return DrainStep::Throttle;
			}
		};
		if batch.is_empty() {
			return DrainStep::Drained;
		}
		let mut advanced = false;
		for row in &batch {
			if let Some(lock) = fence.as_mut()
				&& let Err(err) = sqlx::query("SELECT 1").execute(lock.as_mut()).await
			{
				warn!(seq = row.seq, "relay: outbox lock probe failed before dispatch: {err}");
				return DrainStep::LostLock;
			}
			match self.dispatch(row).await {
				Outcome::Done => {
					if let Err(err) = outbox::mark_dispatched(&self.pool, row.seq).await {
						warn!(seq = row.seq, "relay: failed to mark dispatched (event will re-deliver): {err}");
					}
					advanced = true;
				}
				Outcome::Park { reason, applied_legs } => {
					self.park(row, &reason, applied_legs).await;
					advanced = true;
				}
				Outcome::Retry(reason) => {
					warn!(seq = row.seq, "relay: transient failure (unbounded), retrying from here: {reason}");
					if let Err(err) = outbox::record_failure(&self.pool, row.seq, &reason).await {
						warn!(seq = row.seq, "relay: failed to record the retry attempt: {err}");
					}
					return DrainStep::Throttle;
				}
				Outcome::RetryBounded(reason) => {
					// A retryable that can never resolve (a completion whose pending was
					// itself parked, so it is never found) must not wedge the single-worker
					// queue forever — park it after a bound so later events keep flowing.
					if row.attempts + 1 >= MAX_RETRYABLE_ATTEMPTS {
						self.park(row, &format!("retryable exhausted after {} attempts: {reason}", row.attempts + 1), 0).await;
						advanced = true;
					} else {
						if let Err(err) = outbox::record_failure(&self.pool, row.seq, &reason).await {
							warn!(seq = row.seq, "relay: failed to record the retry attempt: {err}");
						}
						warn!(seq = row.seq, attempts = row.attempts + 1, "relay: retryable, retrying from here: {reason}");
						return DrainStep::Throttle;
					}
				}
			}
		}
		if advanced { DrainStep::More } else { DrainStep::Drained }
	}

	/// Park a non-retryable event into its distinct terminal state (NOT dispatched), so it
	/// stays queryable instead of being silently dropped. If any earlier leg of a multi-leg
	/// event already applied (`applied_legs > 0`), the event is half-applied: stamp a
	/// compensation marker and alert at `error!` (which the Sentry `tracing_layer` ships)
	/// so the aggregate's PG-vs-TB divergence is surfaced.
	async fn park(&self, row: &OutboxRow, reason: &str, applied_legs: usize) {
		if let Err(err) = outbox::mark_parked(&self.pool, row.seq, reason).await {
			error!(seq = row.seq, "relay: failed to stamp parked_at (event will re-deliver): {err}");
		}
		// A parked escrow lock is an order standing on the book with nothing behind it —
		// the one park that must ALSO change control-plane state, because the matcher
		// would otherwise keep filling an order whose settlement is bound to park too.
		if row.kind == BookEvent::KIND
			&& let Ok(BookEvent::OrderPlaced { order_id, .. }) = serde_json::from_str::<BookEvent>(&row.payload)
			&& let Err(err) = book::mark_rejected(&self.pool, order_id.raw(), &format!("ledger refused the escrow: {reason}")).await
		{
			error!(seq = row.seq, order = %order_id, "relay: failed to mark the order rejected after its lock parked: {err}");
		}
		if applied_legs > 0 {
			error!(seq = row.seq, event_id = %row.event_id, aggregate = %row.aggregate, applied_legs, "relay: PARKED HALF-APPLIED event (compensation owed): {reason}");
			if let Err(err) = outbox::mark_compensated(&self.pool, row.seq).await {
				error!(seq = row.seq, "relay: failed to stamp compensated_at: {err}");
			}
		} else {
			error!(seq = row.seq, event_id = %row.event_id, aggregate = %row.aggregate, "relay: parking event (needs intervention): {reason}");
		}
	}

	async fn dispatch(&self, row: &OutboxRow) -> Outcome {
		match row.kind.as_str() {
			SubscriptionEvent::KIND => self.subscription(row).await,
			RedemptionEvent::KIND => self.redemption(row).await,
			BookEvent::KIND => self.book(row).await,
			FeeEvent::KIND => self.fee(row).await,
			_ => self.dispatch_planned(row).await,
		}
	}

	/// An event whose every leg is a posted unit transfer, planned up front ([`plan`]).
	async fn dispatch_planned(&self, row: &OutboxRow) -> Outcome {
		let ops = match plan(row) {
			Ok(ops) => ops,
			Err(reason) => return Outcome::park(format!("unplannable event: {reason}")),
		};
		// Track applied legs so a park *after* an earlier leg posted (`applied > 0`) is
		// flagged half-applied. A first-leg park leaves nothing applied.
		let mut applied = 0usize;
		for (leg, op) in ops.iter().enumerate() {
			let result = match &op.action {
				LedgerAction::Post(transfer) => self.ledger.post(transfer).await,
			};
			match result {
				Ok(()) => {
					// The step record is load-bearing (reconciliation reads it), so a failed
					// insert retries the whole event: every leg is idempotent (`Exists` is
					// success), so the redelivery re-applies cleanly and records the step.
					if let Err(err) = record_saga_step(&self.pool, row.event_id, leg as i32, op.role, op.transfer_id).await {
						return Outcome::Retry(format!("saga step record: {err}"));
					}
					applied += 1;
				}
				Err(LedgerError::Unavailable(err)) => return Outcome::Retry(err),
				Err(LedgerError::Retryable(err)) => return Outcome::RetryBounded(err),
				Err(LedgerError::InsufficientFunds) => {
					return Outcome::Park {
						reason: "insufficient funds".into(),
						applied_legs: applied,
					};
				}
				Err(LedgerError::Conflict(err)) => {
					return Outcome::Park {
						reason: format!("ledger conflict: {err}"),
						applied_legs: applied,
					};
				}
			}
		}
		// An issuance's `applied` stamp (and, for a user holder, its cost-basis projection)
		// is written here, after the leg posted, never on the open path — a parked mint must
		// leave the row `queued`, not claiming units it never got.
		if row.kind == IssuanceEvent::KIND
			&& let Err(err) = project_issuance(&self.pool, row).await
		{
			return Outcome::Retry(format!("issuance applied projection: {err}"));
		}
		Outcome::Done
	}
}

/// Stamp an issuance `applied` and, when the holder is a user, add its cost basis to
/// their `fund_positions` projection — idempotently, under a per-event `saga_steps`
/// marker, in one transaction. The
/// projection carries the issuance's `nav` as the high-water mark blend, exactly as a
/// subscription at that NAV would: an investor handed units in kind is measured for
/// performance fees from the price they were handed them at. An allocation holder gets no
/// projection — there is no investor to report P&L or charge fees to. The projection
/// itself is [`issuance::project_holder_position`]: one place says what a holder's
/// position gains from a mint.
async fn project_issuance(pool: &PgPool, row: &OutboxRow) -> Result<(), sqlx::Error> {
	const PROJECTION_LEG: i32 = 100;
	let IssuanceEvent::Issued {
		holder,
		source,
		service,
		units,
		nav,
		cost_basis,
		..
	} = serde_json::from_str(&row.payload).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
	let marker_id = tid(row.aggregate_id, ISSUE_APPLIED);
	let mut tx = pool.begin().await?;
	let marked = sqlx::query("INSERT INTO saga_steps (event_id, leg, role, tb_transfer_id) VALUES ($1, $2, 'issue_applied', $3) ON CONFLICT (event_id, leg) DO NOTHING")
		.bind(row.event_id)
		.bind(PROJECTION_LEG)
		.bind(&marker_id.to_be_bytes()[..])
		.execute(&mut *tx)
		.await?
		.rows_affected();
	if marked == 1 {
		sqlx::query("UPDATE unit_issuances SET state = 'applied', applied_at = now() WHERE id = $1 AND state = 'queued'")
			.bind(row.aggregate_id)
			.execute(&mut *tx)
			.await?;
		issuance::project_holder_position(&mut tx, &holder, source, &service, units, nav, cost_basis).await?;
	}
	tx.commit().await?;
	Ok(())
}

/// One [`Relay::drain_batch`] step — how the caller decides to continue, idle, back
/// off, or abandon the drain.
pub enum DrainStep {
	/// The backlog is empty (or nothing can advance) — idle until a nudge/poll.
	Drained,
	/// A full batch applied and more rows may remain — re-check shutdown, keep draining.
	More,
	/// Stopped on a transient failure — back off, then retry from the same `seq`.
	Throttle,
	/// The fence probe failed: the lock backend is gone and the session lock with it — a
	/// standby may already be draining. Stop and re-acquire; **never** keep applying.
	LostLock,
}

enum Outcome {
	Done,
	/// Transient infra outage (TB unreachable) — stop and retry from this `seq`
	/// **unbounded** (it resolves when the dependency recovers).
	Retry(String),
	/// A retryable ledger state (a pending not yet visible) — retry from this `seq`, but
	/// **bounded**: a pending that can never appear (its reserve was itself parked) must
	/// not wedge the single-worker queue forever, so park after `MAX_RETRYABLE_ATTEMPTS`.
	RetryBounded(String),
	/// Terminal for this event — move it to the distinct **parked** state (stays queryable,
	/// excluded from the drain). `applied_legs` is how many legs of this (possibly
	/// multi-leg) event already posted before the park: `> 0` means it half-applied and a
	/// compensation is owed (reconciliation catches it; auto-reversal is a follow-up).
	Park {
		reason: String,
		applied_legs: usize,
	},
}

impl Outcome {
	/// Park before any leg applied — the common case (unplannable, pre-check shortfall).
	fn park(reason: String) -> Self {
		Outcome::Park { reason, applied_legs: 0 }
	}
}

struct PlannedOp {
	role: &'static str,
	transfer_id: u128,
	action: LedgerAction,
}

/// What one leg of a planned (single-ledger) event does. Only posted transfers: every
/// two-phase leg belongs to a money saga, which drives the ledger itself (`sagas`).
enum LedgerAction {
	Post(LedgerTransfer),
}

/// Map an outbox event to the ledger op(s) it performs. `reference` (stamped into
/// `user_data_128`) is always the aggregate id, tying every TB transfer back to its row.
/// Only an issuance is planned here; the sagas are dispatched before this is reached.
fn plan(row: &OutboxRow) -> Result<Vec<PlannedOp>, String> {
	let reference = row.aggregate_id.as_u128();
	match row.kind.as_str() {
		IssuanceEvent::KIND => {
			let event: IssuanceEvent = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
			plan_issuance(event, row.aggregate_id, reference).map(|op| vec![op])
		}
		// A kind with no ledger op reached the outbox — nothing in invest relays one, so it
		// is a bug upstream, and an applied no-op would hide it.
		other => Err(format!("no ledger plan for event kind '{other}'")),
	}
}

/// An in-kind issuance is one posted transfer on the Share ledger, by `source`:
/// - **Mint** — a subscription's mint leg with no cash leg in front of it, `Dr <holder
///   shares> / Cr shares-outstanding`, under its own [`TransferCode::UnitIssue`] so
///   supply growth the fund's cash never paid for is distinguishable from a
///   subscription's on the Share ledger alone. It cannot fail for funds (both accounts
///   are supply we control), so the only park is a genuine conflict.
/// - **Retire** — the mint reversed, `Dr shares-outstanding / Cr <holder shares>` under
///   [`TransferCode::UnitRetire`]: supply shrinks by `units`, no cash moves. The
///   holder's account is debit-normal with the non-negative flag, so retiring more than
///   it holds parks — the backstop under the use case's Read-First on `available()`.
///
/// A holder with no account in the product (an allocation other than `fee`) is
/// unplannable rather than a
/// transfer TigerBeetle would refuse anyway: the aggregate cannot build one, so the park
/// reason should say "corrupt payload", not "accounts must differ".
fn plan_issuance(event: IssuanceEvent, aggregate_id: Uuid, reference: u128) -> Result<PlannedOp, String> {
	let IssuanceEvent::Issued { holder, source, service, units, .. } = event;
	let (role, salt, debit, credit, code) = match source {
		IssuanceSource::Mint => (
			"issue_mint",
			ISSUE_MINT,
			holder.shares_key(&service).map_err(|err| err.to_string())?,
			LedgerAccountKey::SharesOutstanding(service),
			TransferCode::UnitIssue,
		),
		IssuanceSource::Retire => (
			"issue_retire",
			ISSUE_RETIRE,
			LedgerAccountKey::SharesOutstanding(service.clone()),
			holder.shares_key(&service).map_err(|err| err.to_string())?,
			TransferCode::UnitRetire,
		),
	};
	Ok(PlannedOp {
		role,
		transfer_id: tid(aggregate_id, salt),
		action: LedgerAction::Post(LedgerTransfer {
			id: tid(aggregate_id, salt),
			debit,
			credit,
			amount: units.base_units(),
			code,
			reference,
		}),
	})
}

/// A deterministic TigerBeetle transfer id for one leg of an aggregate's saga, derived
/// from the (stable) aggregate id + a per-leg salt, so a retried delivery recomputes the
/// same id.
fn tid(aggregate_id: Uuid, salt: &[u8]) -> u128 {
	Uuid::new_v5(&aggregate_id, salt).as_u128()
}

/// The transfer id of an issuance's mint leg, derived from the issuance id the way
/// [`plan_issuance`] derives it — exposed so a data move that posts its mints itself
/// stamps the ids the relay would have, and a reconciler recomputing a transfer from a
/// `unit_issuances` row finds them like any other.
pub fn issuance_mint_id(issuance_id: Uuid) -> u128 {
	tid(issuance_id, ISSUE_MINT)
}

async fn record_saga_step(pool: &PgPool, event_id: Uuid, leg: i32, role: &str, transfer_id: u128) -> Result<(), sqlx::Error> {
	sqlx::query("INSERT INTO saga_steps (event_id, leg, role, tb_transfer_id) VALUES ($1, $2, $3, $4) ON CONFLICT (event_id, leg) DO NOTHING")
		.bind(event_id)
		.bind(leg)
		.bind(role)
		.bind(&transfer_id.to_be_bytes()[..])
		.execute(pool)
		.await?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use invest_domain::{
		balance::ServiceId,
		issuance::{UnitHolder, UnitIssuanceId},
		money::{Nav, Shares, Usdt},
		users::UserId,
	};

	use super::*;

	// A mint grows supply and a retirement shrinks it: same event, two legs, told apart by
	// the source — and by the transfer id, so a reconciler can tell them from the row.
	#[test]
	fn an_issuance_mints_or_retires_by_its_source() {
		let user = UserId::new();
		let service = ServiceId::parse("service_arb").unwrap();
		let fee = UnitHolder::Allocation(ServiceId::fee());
		let issuance_id = UnitIssuanceId::new();
		let event = |holder, source| IssuanceEvent::Issued {
			issuance_id,
			service: service.clone(),
			holder,
			source,
			units: Shares::parse_decimal("13000").unwrap(),
			nav: Nav::SEED,
			cost_basis: Usdt::parse_decimal("13000").unwrap(),
		};
		let plan = |holder, source| plan_issuance(event(holder, source), issuance_id.raw(), issuance_id.raw().as_u128());

		// The fee allocation's holding of a product is its fee class: the mint lands on
		// `FeeShares`, the same account a clawback moves units into.
		let mint = plan(fee.clone(), IssuanceSource::Mint).unwrap();
		let LedgerAction::Post(leg) = &mint.action;
		assert_eq!(
			(leg.debit.clone(), leg.credit.clone()),
			(LedgerAccountKey::FeeShares(service.clone()), LedgerAccountKey::SharesOutstanding(service.clone()))
		);
		assert_eq!(leg.code, TransferCode::UnitIssue);
		assert_eq!(mint.transfer_id, tid(issuance_id.raw(), ISSUE_MINT));

		// A holder with no account in the product (the fund allocation, phase 1) is not a
		// leg the ledger should even see.
		assert!(plan(UnitHolder::Allocation(ServiceId::fund()), IssuanceSource::Mint).is_err());
		assert!(plan(UnitHolder::Allocation(ServiceId::fund()), IssuanceSource::Retire).is_err());

		// A retirement is the mint reversed — same two accounts, swapped sides — for either
		// holder, under its own code and its own transfer id.
		for holder in [UnitHolder::User(user), fee] {
			let retire = plan(holder.clone(), IssuanceSource::Retire).unwrap();
			let LedgerAction::Post(leg) = &retire.action;
			assert_eq!(
				(leg.debit.clone(), leg.credit.clone()),
				(LedgerAccountKey::SharesOutstanding(service.clone()), holder.shares_key(&service).unwrap())
			);
			assert_eq!(leg.code, TransferCode::UnitRetire);
			assert_eq!(leg.amount, Shares::parse_decimal("13000").unwrap().base_units(), "the row's units are the magnitude");
			assert_eq!(retire.role, "issue_retire");
			assert_eq!(retire.transfer_id, tid(issuance_id.raw(), ISSUE_RETIRE));
			assert_ne!(retire.transfer_id, mint.transfer_id, "a burn must never alias the mint it reverses");
		}
	}

	// Nothing but an issuance is relayed today; any other kind reaching the outbox is a
	// bug, and the relay parks it rather than marking it dispatched with nothing done.
	#[test]
	fn an_event_kind_with_no_plan_is_unplannable_not_a_no_op() {
		let row = OutboxRow {
			seq: 1,
			event_id: Uuid::new_v4(),
			aggregate: "allocation".into(),
			aggregate_id: Uuid::new_v4(),
			kind: "allocations".into(),
			payload: "{}".into(),
			attempts: 0,
		};
		assert!(plan(&row).is_err());
	}
}
