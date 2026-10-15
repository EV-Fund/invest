//! The consumer of banking's money events (`scope.v1.MoneyEvents`, S2-18): what banking did
//! to invest's money on its own, applied to invest's records.
//!
//! | Event | invest |
//! | ----- | ------ |
//! | HOLD_VOIDED / HOLD_EXPIRED of a `book_order:` hold | the resting buy comes off the book `rejected` — its cash is gone ([`book::reject_buy_on_released_hold`]) |
//! | OPERATION_REJECTED / OPERATION_FAILED, or PAID_OUT of a `redemption:` waiting on an operation | the operation's verdict, read back with `GetPendingOperation`, completes or fails the redemption ([`redemptions::apply_payout_verdict`]) |
//! | a kind this build cannot name | parked, the cursor moves on (events.proto: "must park that event and keep its cursor moving") |
//! | every other kind | nothing — read and passed over |
//!
//! **Exactly once against Postgres.** An event that acts is applied in one transaction with
//! its dedupe row (`money_events_applied`, banking's `event_id`) and the cursor, so a crash
//! leaves either all three or none, and a redelivery — the same event at a later position,
//! or the same page pulled again — finds its id and does nothing. Every effect is also
//! guarded by the state it moves from (a resting buy, a redemption awaiting approval), so a
//! stale event can never move anything backwards; that is why the per-aggregate
//! `aggregate_version` is not kept. The redemption's verdict is never taken from the event
//! itself: `GetPendingOperation` is asked, and the event is only the nudge.
//!
//! **A sweep backs the events up.** Every [`VERDICT_SWEEP`] the redemptions still awaiting
//! approval are re-asked about, so a verdict whose event was missed (a consumer that was not
//! running, a page it parked) still lands.
//!
//! **One consumer.** Run under a session advisory lock ([`MONEY_EVENTS_LOCK_KEY`]), like the
//! relay and the users projection: two would both pass the dedupe read before either wrote.
//! What it changes it hands on: the relay is nudged for the redemption events it wrote, and
//! the book's live feed told the revision a rejection took.

use std::{
	sync::Arc,
	time::{Duration, Instant},
};

use invest_domain::{balance::ServiceId, error::DomainError};
use sqlx::{PgConnection, PgPool, Postgres, pool::PoolConnection};
use thiserror::Error;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::{
	book,
	redemptions::{self, AwaitingPayout, PayoutVerdict},
	singleton,
};
use crate::{
	application::book::BookFeed,
	ports::money::{MoneyEvent, MoneyEventKind, MoneyEventSource, MoneyLedger, MoneyLedgerError, OperationId, PendingOperationState, ScopeId, machine_reason},
};

/// Fixed key for the consumer's session-level `pg_advisory_lock`. ASCII "EVINMNY_"; only
/// stability matters, as for the relay's key.
const MONEY_EVENTS_LOCK_KEY: i64 = 0x4556_494e_4d4e_595f_u64 as i64;

/// Events asked for per pull. banking caps it at its own ceiling anyway.
const PAGE: u32 = 256;

/// Tries an event gets before it is parked and the stream moves on.
pub const MAX_EVENT_ATTEMPTS: u32 = 5;

/// The wait between pulls once caught up.
const POLL: Duration = Duration::from_secs(2);

/// How often the redemptions awaiting approval are re-asked about (see the module doc).
pub const VERDICT_SWEEP: Duration = Duration::from_secs(60);

/// What stopped a pass: banking or Postgres could not answer. The pass is retried from the
/// stored cursor; nothing it applied is applied twice.
#[derive(Debug, Error)]
pub enum ConsumerError {
	#[error("money events: banking: {0}")]
	Banking(#[from] MoneyLedgerError),
	#[error("money events: postgres: {0}")]
	Db(#[from] sqlx::Error),
	#[error("money events: {0}")]
	Domain(#[from] DomainError),
}

/// What one pass did, for the tests and the log.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Pass {
	/// Events acted on for the first time — whether or not an order or a redemption still
	/// stood to be moved by them.
	pub applied: u64,
	/// Events met before — their id was already applied or parked.
	pub duplicates: u64,
	pub parked: u64,
}

pub struct MoneyEventsConsumer {
	pool: PgPool,
	source: Arc<dyn MoneyEventSource>,
	money: Arc<dyn MoneyLedger>,
	relay: Arc<Notify>,
	feed: Arc<BookFeed>,
}

/// What one event asks of invest, decided before the transaction that applies it.
enum Effect {
	/// Nothing of invest's: move the cursor past it.
	Pass,
	/// Take the buy standing on this hold off the book.
	RejectBuy { order: Uuid, hold: String, reason: String },
	/// Apply the operation's verdict to the redemption waiting on it.
	Verdict { operation: String, verdict: OwnedVerdict },
	/// Not readable by this build: park it.
	Park { code: i32 },
}

enum OwnedVerdict {
	Executed,
	Refused(String),
}

impl MoneyEventsConsumer {
	pub fn new(pool: PgPool, source: Arc<dyn MoneyEventSource>, money: Arc<dyn MoneyLedger>, relay: Arc<Notify>, feed: Arc<BookFeed>) -> Self {
		Self { pool, source, money, relay, feed }
	}

	/// Run until `shutdown` is cancelled: take the singleton lock on a dedicated connection
	/// (a second invest-core blocks here, owning nothing), replay what is parked, then pull
	/// and sweep while the lock connection is probed. A lost lock or a failed acquisition
	/// re-enters the acquisition with backoff and never returns — a database blip must not
	/// take the gRPC server with it (the composition root ends every branch when one ends).
	///
	/// The work is dropped mid-pass on shutdown or a lost lock. That is safe: an event's
	/// effect, dedupe row and cursor commit together, so a cut-off transaction rolls back
	/// and the event is pulled again.
	pub async fn run(self, shutdown: CancellationToken) {
		singleton::run_under_lock("money events", MONEY_EVENTS_LOCK_KEY, &self.pool, shutdown, || self.work()).await;
	}

	/// The consumer's loop while it holds the lock. Never returns.
	async fn work(&self) {
		match self.replay_parked().await {
			Ok(0) => {}
			Ok(replayed) => info!(replayed, "money events: applied parked events this build can now read"),
			Err(err) => warn!("money events: replaying parked events failed (left parked): {err}"),
		}
		let mut swept: Option<Instant> = None;
		loop {
			let wait = match self.pass().await {
				Ok(pass) => {
					if pass != Pass::default() {
						debug!(?pass, "money events: pass");
					}
					POLL
				}
				Err(err) => {
					warn!("money events: pass stopped (retrying from the cursor): {err}");
					POLL * 5
				}
			};
			if swept.is_none_or(|at| at.elapsed() >= VERDICT_SWEEP) {
				if let Err(err) = self.sweep_awaiting().await {
					warn!("money events: re-asking banking about awaiting payouts failed: {err}");
				}
				swept = Some(Instant::now());
			}
			tokio::time::sleep(wait).await;
		}
	}

	/// Block on a dedicated pooled connection until the session advisory lock is held; the
	/// connection's lifetime is the lock's.
	pub async fn acquire_lock(&self) -> Result<PoolConnection<Postgres>, sqlx::Error> {
		singleton::acquire(&self.pool, MONEY_EVENTS_LOCK_KEY).await
	}

	/// Take the lock if it is free, else `None` — for tests that observe the singleton.
	pub async fn try_acquire_lock(&self) -> Result<Option<PoolConnection<Postgres>>, sqlx::Error> {
		singleton::try_acquire(&self.pool, MONEY_EVENTS_LOCK_KEY).await
	}

	/// Pull and apply until caught up, from the stored cursor. Unfenced — the tests own the
	/// sole consumer by construction; [`run`](Self::run) always holds the lock.
	pub async fn pass(&self) -> Result<Pass, ConsumerError> {
		let mut pass = Pass::default();
		let mut cursor = read_cursor(&self.pool).await?;
		loop {
			let page = self.source.pull(cursor, PAGE).await?;
			for event in &page.events {
				if let Err(err) = self.apply(event, false, &mut pass).await {
					// banking being away is not this event's fault; a failure of invest's own
					// side is, and one event must not hold the stream behind it for ever.
					if matches!(err, ConsumerError::Banking(_)) || !self.give_up_on(event, &err, &mut pass).await? {
						return Err(err);
					}
				}
			}
			let caught_up = page.events.is_empty();
			if page.next_position > cursor {
				write_cursor(&mut *self.pool.acquire().await?, page.next_position).await?;
				cursor = page.next_position;
			}
			if caught_up {
				return Ok(pass);
			}
		}
	}

	/// Pull every parked event again by its position and apply the ones this build can now
	/// read; the rest stay parked. Answers how many left the park.
	pub async fn replay_parked(&self) -> Result<u64, ConsumerError> {
		let parked: Vec<(String, i64)> = sqlx::query_as("SELECT event_id, position FROM money_events_parked ORDER BY position")
			.fetch_all(&self.pool)
			.await?;
		let mut pass = Pass::default();
		for (event_id, position) in parked {
			// Positions are banking's and never reused, so the event is still there.
			let page = self.source.pull(position.saturating_sub(1), 1).await?;
			let Some(event) = page.events.into_iter().find(|event| event.event_id == event_id) else {
				warn!(%event_id, position, "money events: a parked event is no longer served at its position; left parked");
				continue;
			};
			if matches!(event.kind, MoneyEventKind::Unknown(_)) {
				continue;
			}
			// One that still fails stays parked; it must not keep the others from replaying.
			if let Err(err) = self.apply(&event, true, &mut pass).await {
				warn!(%event_id, "money events: a parked event still cannot be applied (left parked): {err}");
			}
		}
		Ok(pass.applied + pass.duplicates)
	}

	/// Re-ask banking about every redemption awaiting approval, and apply each verdict that
	/// is in. Sequential: banking keeps at most 20 of a client's operations awaiting per
	/// scope, so the list is short. Answers how many verdicts landed.
	pub async fn sweep_awaiting(&self) -> Result<u64, ConsumerError> {
		let mut landed = 0;
		for awaiting in redemptions::awaiting_payouts(&self.pool).await? {
			// One record banking cannot answer for, or one that does not apply, is skipped and
			// read again on the next sweep; the others still land.
			let verdict = match self.verdict(&awaiting).await {
				Ok(Some(verdict)) => verdict,
				Ok(None) => continue,
				Err(err) => {
					warn!(redemption = %awaiting.id, operation = %awaiting.operation, "money events: could not read a payout's verdict (next sweep): {err}");
					continue;
				}
			};
			match self.land(&awaiting, &verdict).await {
				Ok(true) => {
					landed += 1;
					self.relay.notify_one();
				}
				Ok(false) => {}
				Err(err) => warn!(redemption = %awaiting.id, operation = %awaiting.operation, "money events: could not apply a payout's verdict (next sweep): {err}"),
			}
		}
		Ok(landed)
	}

	/// Count a failed try of `event`; at [`MAX_EVENT_ATTEMPTS`] park it (reason `failed N
	/// times: …`) so the stream moves on. `true` when it was parked — the pass goes on —
	/// `false` while it will be tried again.
	async fn give_up_on(&self, event: &MoneyEvent, err: &ConsumerError, pass: &mut Pass) -> Result<bool, ConsumerError> {
		let attempts: i32 = sqlx::query_scalar(
			"INSERT INTO money_events_attempts (event_id, attempts) VALUES ($1, 1) \
			 ON CONFLICT (event_id) DO UPDATE SET attempts = money_events_attempts.attempts + 1, updated_at = now() RETURNING attempts",
		)
		.bind(&event.event_id)
		.fetch_one(&self.pool)
		.await?;
		if u32::try_from(attempts).unwrap_or(u32::MAX) < MAX_EVENT_ATTEMPTS {
			return Ok(false);
		}
		let mut tx = self.pool.begin().await?;
		sqlx::query("INSERT INTO money_events_parked (event_id, position, kind, scope, reason) VALUES ($1, $2, 0, $3, $4) ON CONFLICT (event_id) DO NOTHING")
			.bind(&event.event_id)
			.bind(event.position)
			.bind(&event.scope)
			.bind(format!("failed {attempts} times: {err}"))
			.execute(&mut *tx)
			.await?;
		sqlx::query("DELETE FROM money_events_attempts WHERE event_id = $1")
			.bind(&event.event_id)
			.execute(&mut *tx)
			.await?;
		tx.commit().await?;
		error!(event_id = %event.event_id, position = event.position, kind = ?event.kind, attempts, "money events: an event kept failing — parked, the stream moves on (needs intervention): {err}");
		pass.parked += 1;
		Ok(true)
	}

	async fn land(&self, awaiting: &AwaitingPayout, verdict: &OwnedVerdict) -> Result<bool, ConsumerError> {
		let mut tx = self.pool.begin().await?;
		let applied = redemptions::apply_payout_verdict(&mut tx, &awaiting.operation, verdict.as_ref()).await?;
		tx.commit().await?;
		Ok(applied)
	}

	/// Apply one event: decide its effect (asking banking when the event is a nudge), then
	/// commit the effect, its dedupe row and the cursor together.
	async fn apply(&self, event: &MoneyEvent, from_park: bool, pass: &mut Pass) -> Result<(), ConsumerError> {
		let effect = self.effect(event).await?;
		if matches!(effect, Effect::Pass) && !from_park {
			return Ok(());
		}
		let mut tx = self.pool.begin().await?;
		if from_park {
			sqlx::query("DELETE FROM money_events_parked WHERE event_id = $1").bind(&event.event_id).execute(&mut *tx).await?;
		}
		let mut published = None;
		let mut nudge_relay = false;
		match effect {
			Effect::Pass => {}
			Effect::Park { code } => {
				let same_kind: i64 = sqlx::query_scalar("SELECT count(*) FROM money_events_parked WHERE kind = $1")
					.bind(code)
					.fetch_one(&mut *tx)
					.await?;
				let parked = sqlx::query("INSERT INTO money_events_parked (event_id, position, kind, scope, reason) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (event_id) DO NOTHING")
					.bind(&event.event_id)
					.bind(event.position)
					.bind(code)
					.bind(&event.scope)
					.bind(format!("money event kind {code} is unknown to this build"))
					.execute(&mut *tx)
					.await?
					.rows_affected();
				if parked == 1 {
					// Loud once per kind: a new kind that is frequent would otherwise page for
					// every event while the first alert already says what to do.
					if same_kind == 0 {
						error!(event_id = %event.event_id, position = event.position, kind = code, "money events: an event of a kind this build cannot name — parked (needs a newer invest-core)");
					} else {
						debug!(event_id = %event.event_id, kind = code, "money events: another event of an unknown kind parked");
					}
					pass.parked += 1;
				} else {
					pass.duplicates += 1;
				}
			}
			Effect::RejectBuy { order, hold, reason } =>
				if first_time(&mut tx, event).await? {
					published = book::reject_buy_on_released_hold(&mut tx, order, &event.scope, &hold, &reason).await?;
					if published.is_some() {
						info!(%order, %hold, %reason, "money events: banking released a resting buy's hold — the order is off the book");
					}
					pass.applied += 1;
				} else {
					pass.duplicates += 1;
				},
			Effect::Verdict { operation, verdict } =>
				if first_time(&mut tx, event).await? {
					nudge_relay = redemptions::apply_payout_verdict(&mut tx, &operation, verdict.as_ref()).await?;
					pass.applied += 1;
				} else {
					pass.duplicates += 1;
				},
		}
		write_cursor(&mut tx, event.position).await?;
		tx.commit().await?;
		if let Some((service, revision)) = published {
			match (ServiceId::parse(&service), u64::try_from(revision)) {
				(Ok(service), Ok(revision)) => self.feed.publish(&service, revision),
				// Postgres holds both as the book wrote them; a mismatch only costs a watcher
				// its prompt refresh, never the rejection itself.
				(service, revision) => warn!(?service, ?revision, "money events: could not tell the live feed about a rejected buy"),
			}
		}
		if nudge_relay {
			self.relay.notify_one();
		}
		Ok(())
	}

	async fn effect(&self, event: &MoneyEvent) -> Result<Effect, ConsumerError> {
		Ok(match event.kind {
			MoneyEventKind::HoldVoided | MoneyEventKind::HoldExpired => match subject_id(&event.subject_ref, "book_order") {
				Some(order) => Effect::RejectBuy {
					order,
					hold: event.ref_id.clone(),
					reason: released_reason(event),
				},
				// A subscription's hold is the relay's: its capture finds the hold gone and
				// fails the subscription. A void invest asked for names its own subject too.
				None => Effect::Pass,
			},
			MoneyEventKind::OperationRejected | MoneyEventKind::OperationFailed => {
				let awaiting = sqlx::query_as::<_, (Uuid, String, String)>(
					"SELECT r.id, a.scope, r.payout_operation_id FROM redemptions r JOIN allocations a ON a.service = r.service \
					 WHERE r.payout_operation_id = $1 AND r.state = 'awaiting_approval'",
				)
				.bind(&event.ref_id)
				.fetch_optional(&self.pool)
				.await?
				.map(|(id, scope, operation)| AwaitingPayout { id, scope, operation });
				self.verdict_effect(awaiting).await?
			}
			// An executed operation shows as PAID_OUT with the request's subject — and so does
			// every automatic payout, which finds no redemption awaiting and passes.
			MoneyEventKind::PaidOut => match subject_id(&event.subject_ref, "redemption") {
				Some(id) => self.verdict_effect(redemptions::awaiting_payout(&self.pool, id).await?).await?,
				None => Effect::Pass,
			},
			MoneyEventKind::Unknown(code) => Effect::Park { code },
			MoneyEventKind::AccountOpened
			| MoneyEventKind::CompartmentIn
			| MoneyEventKind::CompartmentOut
			| MoneyEventKind::HoldPlaced
			| MoneyEventKind::HoldCaptured
			| MoneyEventKind::Charged
			| MoneyEventKind::Transferred
			| MoneyEventKind::InvoiceCreated
			| MoneyEventKind::InvoicePaid
			| MoneyEventKind::InvoiceCancelled
			| MoneyEventKind::InvoiceExpired
			| MoneyEventKind::MandateGranted
			| MoneyEventKind::MandateRevoked
			| MoneyEventKind::OperationRequested => Effect::Pass,
		})
	}

	async fn verdict_effect(&self, awaiting: Option<AwaitingPayout>) -> Result<Effect, ConsumerError> {
		let Some(awaiting) = awaiting else {
			return Ok(Effect::Pass);
		};
		Ok(match self.verdict(&awaiting).await {
			Ok(Some(verdict)) => Effect::Verdict {
				operation: awaiting.operation,
				verdict,
			},
			Ok(None) => Effect::Pass,
			// The event is only a nudge: the sweep asks again, so a verdict banking cannot give
			// right now must not hold the stream — or park what is behind it.
			Err(err) => {
				warn!(redemption = %awaiting.id, operation = %awaiting.operation, "money events: could not read a payout's verdict; left to the sweep: {err}");
				Effect::Pass
			}
		})
	}

	/// banking's verdict on the operation `awaiting` waits on — `None` while the owners have
	/// not decided. A transient failure is an error for the caller to defer; an operation
	/// banking does not know, answers malformed, or answers for another operation or
	/// redemption is left awaiting and logged, for an operator: failing the redemption on it
	/// would give the units back while the payout may still execute.
	// TODO(F4): a redemption can wait here for ever when banking never answers. It needs an
	// operator RPC `ResolveAwaitingRedemption(id, outcome)` under `invest:allocation:manage`
	// that re-polls banking, and a metric for the oldest awaiting one.
	async fn verdict(&self, awaiting: &AwaitingPayout) -> Result<Option<OwnedVerdict>, ConsumerError> {
		let scope = match ScopeId::parse(&awaiting.scope) {
			Ok(scope) => scope,
			Err(err) => {
				error!(redemption = %awaiting.id, "money events: the redemption's allocation has no valid scope ({err}); left awaiting (needs intervention)");
				return Ok(None);
			}
		};
		let operation = match self.money.pending_operation(&scope, &OperationId(awaiting.operation.clone())).await {
			Ok(operation) => operation,
			Err(err @ (MoneyLedgerError::Unavailable(_) | MoneyLedgerError::Unimplemented { .. })) => return Err(err.into()),
			Err(
				err @ (MoneyLedgerError::NotFound | MoneyLedgerError::Refused(_) | MoneyLedgerError::IdempotencyConflict(_) | MoneyLedgerError::Invalid(_) | MoneyLedgerError::Protocol(_)),
			) => {
				error!(redemption = %awaiting.id, operation = %awaiting.operation, "money events: banking cannot tell the requested payout's verdict ({err}); left awaiting (needs intervention)");
				return Ok(None);
			}
		};
		// The verdict is acted on for THIS redemption: what banking sends back must be the
		// operation asked about, requested for this redemption.
		if operation.id.0 != awaiting.operation || operation.subject_ref != format!("redemption:{}", awaiting.id.hyphenated()) {
			error!(redemption = %awaiting.id, operation = %awaiting.operation, answered = %operation.id.0, subject_ref = %operation.subject_ref, "money events: banking answered for another operation or redemption; left awaiting (needs intervention)");
			return Ok(None);
		}
		Ok(match operation.state {
			PendingOperationState::AwaitingApproval => None,
			PendingOperationState::Executed => Some(OwnedVerdict::Executed),
			PendingOperationState::Rejected => Some(OwnedVerdict::Refused(machine_reason(&operation.failure_reason, "approval_rejected"))),
			PendingOperationState::Failed => Some(OwnedVerdict::Refused(machine_reason(&operation.failure_reason, "payout_failed"))),
		})
	}
}

impl OwnedVerdict {
	fn as_ref(&self) -> PayoutVerdict<'_> {
		match self {
			Self::Executed => PayoutVerdict::Executed,
			Self::Refused(reason) => PayoutVerdict::Refused(reason),
		}
	}
}

/// Claim `event` on the open transaction: `true` the first time, `false` for a redelivery.
async fn first_time(conn: &mut PgConnection, event: &MoneyEvent) -> Result<bool, sqlx::Error> {
	let kind = format!("{:?}", event.kind);
	let claimed = sqlx::query("INSERT INTO money_events_applied (event_id, position, kind, ref_id) VALUES ($1, $2, $3, $4) ON CONFLICT (event_id) DO NOTHING")
		.bind(&event.event_id)
		.bind(event.position)
		.bind(kind)
		.bind(&event.ref_id)
		.execute(&mut *conn)
		.await?
		.rows_affected();
	Ok(claimed == 1)
}

async fn read_cursor(pool: &PgPool) -> Result<i64, sqlx::Error> {
	Ok(sqlx::query_scalar("SELECT position FROM money_events_cursor").fetch_optional(pool).await?.unwrap_or(0))
}

/// Move the cursor forward to `position`; never back (a parked event replayed later sits
/// behind it).
async fn write_cursor(conn: &mut PgConnection, position: i64) -> Result<(), sqlx::Error> {
	sqlx::query(
		"INSERT INTO money_events_cursor (singleton, position) VALUES (TRUE, $1) \
		 ON CONFLICT (singleton) DO UPDATE SET position = GREATEST(money_events_cursor.position, EXCLUDED.position), updated_at = now()",
	)
	.bind(position)
	.execute(&mut *conn)
	.await?;
	Ok(())
}

/// The id in invest's own `subject_ref` of `kind` — `<kind>:<lowercase uuid>`, the only
/// spelling invest writes ([`SubjectRef`](crate::ports::money::SubjectRef)).
fn subject_id(subject_ref: &str, kind: &str) -> Option<Uuid> {
	let raw = subject_ref.strip_prefix(kind)?.strip_prefix(':')?;
	Uuid::parse_str(raw).ok().filter(|id| id.hyphenated().to_string() == raw)
}

/// Why a resting buy came off the book: banking's reason when it gave one (`ledger_refused`),
/// else what happened to the hold.
fn released_reason(event: &MoneyEvent) -> String {
	let fallback = if event.kind == MoneyEventKind::HoldExpired { "hold_expired" } else { "hold_voided" };
	if event.failure_reason.is_empty() {
		fallback.to_owned()
	} else {
		machine_reason(&event.failure_reason, fallback)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn only_invest_s_own_spelling_of_a_subject_names_an_id() {
		let id = Uuid::new_v4();
		assert_eq!(subject_id(&format!("book_order:{id}"), "book_order"), Some(id));
		assert_eq!(subject_id(&format!("redemption:{id}"), "book_order"), None);
		assert_eq!(subject_id(&format!("book_order:{}", id.to_string().to_uppercase()), "book_order"), None);
		assert_eq!(subject_id(&format!("book_orderx:{id}"), "book_order"), None);
		assert_eq!(subject_id("", "book_order"), None);
	}

	#[test]
	fn a_released_hold_says_why_in_a_machine_reason() {
		let event = |kind, reason: &str| MoneyEvent {
			event_id: String::new(),
			position: 1,
			scope: "invest".into(),
			kind,
			ref_id: String::new(),
			subject_ref: String::new(),
			failure_reason: reason.into(),
		};
		assert_eq!(released_reason(&event(MoneyEventKind::HoldExpired, "")), "hold_expired");
		assert_eq!(released_reason(&event(MoneyEventKind::HoldVoided, "")), "hold_voided");
		assert_eq!(released_reason(&event(MoneyEventKind::HoldVoided, "ledger_refused")), "ledger_refused");
		assert_eq!(released_reason(&event(MoneyEventKind::HoldVoided, "Some prose")), "hold_voided");
	}
}
