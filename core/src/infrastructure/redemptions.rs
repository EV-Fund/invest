//! Postgres adapter for the [`RedemptionRepository`] port.
//!
//! Row-locked commands that apply the aggregate transition and drain its events in one
//! transaction. `open` serializes the requests on one holding under an advisory lock and
//! admits a redemption only for units free in the ledger and not claimed by an undrained
//! event. The cost-basis reduction a settle owes is NOT made here:
//! the payout is banking's and may still be refused, so the relay makes it once the cash
//! has left ([`reduce_cost_basis`], under the saga's marker) — a refused payout then has no
//! basis to give back.

use std::sync::Arc;

use async_trait::async_trait;
use invest_domain::{
	architecture::{Reader, Repository},
	balance::{LedgerAccountKey, ServiceId},
	error::DomainError,
	money::{Nav, Shares, Usdt},
	redemptions::{Redemption, RedemptionId, RedemptionState},
	users::UserId,
};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
	infrastructure::{fee_accrual, outbox},
	ports::{AwaitingOperation, QueuedRedemption, RedemptionRecord, RedemptionRepository, ledger::Ledger},
};

const SELECT_BY_ID: &str = "SELECT id, user_id, service, units, nav, cash, state, failure_reason FROM redemptions WHERE id = $1";
const SELECT_BY_ID_FOR_UPDATE: &str = "SELECT id, user_id, service, units, nav, cash, state, failure_reason FROM redemptions WHERE id = $1 FOR UPDATE";
const SELECT_BY_USER: &str = "SELECT id, user_id, service, units, nav, cash, state, failure_reason FROM redemptions WHERE user_id = $1 ORDER BY created_at DESC";

pub struct PgRedemptions {
	pool: PgPool,
	/// Read under the position lock in [`RedemptionRepository::open`]: the holding the
	/// request is judged against.
	ledger: Arc<dyn Ledger>,
}

impl PgRedemptions {
	pub fn new(pool: PgPool, ledger: Arc<dyn Ledger>) -> Self {
		Self { pool, ledger }
	}
}

impl Repository for PgRedemptions {
	type Aggregate = Redemption;
}

impl Reader for PgRedemptions {
	type Aggregate = Redemption;
}

#[derive(sqlx::FromRow)]
struct RedemptionRow {
	id: Uuid,
	user_id: Uuid,
	service: String,
	units: String,
	nav: Option<String>,
	cash: Option<String>,
	state: String,
	failure_reason: Option<String>,
}

impl RedemptionRow {
	fn into_record(self) -> Result<RedemptionRecord, DomainError> {
		let failure_reason = self.failure_reason.clone();
		Ok(RedemptionRecord {
			redemption: self.into_domain()?,
			failure_reason,
		})
	}

	fn into_domain(self) -> Result<Redemption, DomainError> {
		let nav = self.nav.as_deref().map(|s| parse_units(s, "redemption nav")).transpose()?.map(Nav::from_base_units);
		let cash = self.cash.as_deref().map(|s| parse_units(s, "redemption cash")).transpose()?.map(Usdt::from_base_units);
		Ok(Redemption::rehydrate(
			RedemptionId::from_raw(self.id),
			UserId::from_raw(self.user_id),
			ServiceId::parse(&self.service)?,
			Shares::from_base_units(parse_units(&self.units, "redemption units")?),
			nav,
			cash,
			RedemptionState::parse(&self.state)?,
		))
	}
}

fn parse_units(raw: &str, what: &str) -> Result<u128, DomainError> {
	raw.parse::<u128>().map_err(|_| DomainError::Repository(format!("malformed {what}")))
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

async fn insert_row(conn: &mut PgConnection, redemption: &Redemption) -> Result<(), DomainError> {
	sqlx::query("INSERT INTO redemptions (id, user_id, service, units, nav, cash, state) VALUES ($1, $2, $3, $4, $5, $6, $7)")
		.bind(redemption.id().raw())
		.bind(redemption.user().raw())
		.bind(redemption.service().as_str())
		.bind(redemption.units().base_units().to_string())
		.bind(redemption.nav().map(|n| n.base_units().to_string()))
		.bind(redemption.cash().map(|c| c.base_units().to_string()))
		.bind(redemption.state().as_str())
		.execute(&mut *conn)
		.await
		.map_err(repo_err)?;
	Ok(())
}

/// Persist a state transition — `state`, and `nav`/`cash` set at settle. We hold the row
/// lock, so exactly one row must update.
async fn update_row(conn: &mut PgConnection, redemption: &Redemption) -> Result<(), DomainError> {
	let result = sqlx::query("UPDATE redemptions SET state = $2, nav = $3, cash = $4, updated_at = now() WHERE id = $1")
		.bind(redemption.id().raw())
		.bind(redemption.state().as_str())
		.bind(redemption.nav().map(|n| n.base_units().to_string()))
		.bind(redemption.cash().map(|c| c.base_units().to_string()))
		.execute(&mut *conn)
		.await
		.map_err(repo_err)?;
	if result.rows_affected() != 1 {
		return Err(DomainError::Repository("redemption row vanished under lock".into()));
	}
	Ok(())
}

/// Lock + load a redemption for a state transition.
async fn load_for_update(conn: &mut PgConnection, id: RedemptionId) -> Result<Redemption, DomainError> {
	let row = sqlx::query_as::<_, RedemptionRow>(SELECT_BY_ID_FOR_UPDATE)
		.bind(id.raw())
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?;
	row.ok_or_else(|| DomainError::NotFound {
		entity: "redemption",
		id: id.to_string(),
	})?
	.into_domain()
}

/// Reduce a position's average-cost basis by the redeemed fraction and decrement its
/// tracked units, on the relay's transaction once the payout has gone through:
/// `cost_basis ← trunc(cost_basis × (units − redeemed) / units)` then `units ← units −
/// redeemed`. The denominator is the position's *own* `units` (not a relay-lagging TB
/// balance), and the relay is single and ordered, so back-to-back redemptions compound
/// deterministically and a subscription's projection always precedes the redemption of its
/// units. The reduction clamps at zero rather than refusing, for the reason a trade's does
/// (`relay::project_trade`): a refusal here would be a retry that wedges the relay on a
/// projection that can never catch up, and an understated basis is the recoverable side.
pub(crate) async fn reduce_cost_basis(conn: &mut PgConnection, user: UserId, service: &ServiceId, units: Shares, now_unix: i64) -> Result<(), DomainError> {
	// Settle what the pre-redemption basis accrued before shrinking it. The direction is
	// the investor's favour here rather than the manager's — a smaller basis under-bills
	// the elapsed window — but the asymmetry is the bug either way, and the same call
	// keeps both sides of a basis move honest. See [`super::fee_accrual`].
	fee_accrual::carry_accrual(&mut *conn, user.raw(), service.as_str(), now_unix).await?;
	sqlx::query(
		"UPDATE fund_positions SET \
		 cost_basis = CASE WHEN units::numeric > $3::numeric THEN trunc(cost_basis::numeric * (units::numeric - $3::numeric) / units::numeric)::text ELSE '0' END, \
		 units = GREATEST(units::numeric - $3::numeric, 0)::text, \
		 updated_at = now() \
		 WHERE user_id = $1 AND service = $2",
	)
	.bind(user.raw())
	.bind(service.as_str())
	.bind(units.base_units().to_string())
	.execute(&mut *conn)
	.await
	.map_err(repo_err)?;
	Ok(())
}

/// banking's verdict on a redemption's requested payout (`RequestPayout`).
#[derive(Clone, Copy, Debug)]
pub enum PayoutVerdict<'a> {
	/// Executed: the cash has left — raise `PayoutExecuted`, the relay posts the burn.
	Executed,
	/// Rejected, lapsed or failed, with banking's machine reason — fail the redemption and
	/// raise `Failed`, the relay voids the burn.
	Refused(&'a str),
}

/// Apply `verdict` to the redemption waiting on `operation`, on the caller's transaction
/// (the money-event consumer's, which moves its cursor in the same commit). `false` when no
/// redemption waits on it any more — the verdict landed before, so a repeat does nothing.
pub(crate) async fn apply_payout_verdict(conn: &mut PgConnection, operation: &str, verdict: PayoutVerdict<'_>) -> Result<bool, DomainError> {
	let id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM redemptions WHERE payout_operation_id = $1 AND state = 'awaiting_approval' FOR UPDATE")
		.bind(operation)
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?;
	let Some(id) = id else {
		return Ok(false);
	};
	apply_locked(conn, id, verdict).await?;
	Ok(true)
}

/// Move the locked, awaiting redemption `id` by `verdict` and drain its event.
async fn apply_locked(conn: &mut PgConnection, id: Uuid, verdict: PayoutVerdict<'_>) -> Result<(), DomainError> {
	let mut redemption = load_for_update(conn, RedemptionId::from_raw(id)).await?;
	match verdict {
		PayoutVerdict::Executed => redemption.payout_executed()?,
		PayoutVerdict::Refused(reason) => {
			redemption.payout_refused()?;
			sqlx::query("UPDATE redemptions SET failure_reason = $2 WHERE id = $1")
				.bind(id)
				.bind(reason)
				.execute(&mut *conn)
				.await
				.map_err(repo_err)?;
		}
	}
	update_row(conn, &redemption).await?;
	outbox::drain_to_outbox(conn, &mut redemption, true).await?;
	Ok(())
}

/// A redemption waiting on the owners: its id, and the scope and operation to ask banking
/// about.
#[derive(Clone, Debug)]
pub struct AwaitingPayout {
	pub id: Uuid,
	pub scope: String,
	pub operation: String,
}

/// Every redemption awaiting approval, oldest first — what the consumer re-asks banking
/// about when no event has told it the verdict.
pub(crate) async fn awaiting_payouts(pool: &PgPool) -> Result<Vec<AwaitingPayout>, DomainError> {
	let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
		"SELECT r.id, a.scope, r.payout_operation_id FROM redemptions r JOIN allocations a ON a.service = r.service \
		 WHERE r.state = 'awaiting_approval' ORDER BY r.updated_at, r.id",
	)
	.fetch_all(pool)
	.await
	.map_err(repo_err)?;
	Ok(rows.into_iter().map(|(id, scope, operation)| AwaitingPayout { id, scope, operation }).collect())
}

/// The redemption a payout's `subject_ref` names, when it waits on an operation: the scope
/// and operation to ask banking about.
pub(crate) async fn awaiting_payout(pool: &PgPool, id: Uuid) -> Result<Option<AwaitingPayout>, DomainError> {
	let row: Option<(Uuid, String, String)> = sqlx::query_as(
		"SELECT r.id, a.scope, r.payout_operation_id FROM redemptions r JOIN allocations a ON a.service = r.service \
		 WHERE r.id = $1 AND r.state = 'awaiting_approval'",
	)
	.bind(id)
	.fetch_optional(pool)
	.await
	.map_err(repo_err)?;
	Ok(row.map(|(id, scope, operation)| AwaitingPayout { id, scope, operation }))
}

/// Age in seconds of the oldest redemption awaiting approval, 0 when none — the number a
/// stuck banking verdict shows up in (see the sweep in `money_events`).
pub(crate) async fn oldest_awaiting_age_secs(pool: &PgPool) -> Result<i64, DomainError> {
	sqlx::query_scalar("SELECT COALESCE(EXTRACT(EPOCH FROM now() - MIN(updated_at)), 0)::bigint FROM redemptions WHERE state = 'awaiting_approval'")
		.fetch_one(pool)
		.await
		.map_err(repo_err)
}

#[async_trait]
impl RedemptionRepository for PgRedemptions {
	async fn open(&self, redemption: &mut Redemption) -> Result<(), DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let (user, service) = (redemption.user(), redemption.service().clone());
		// Serialize the requests on this holding, then decide on what is free once the
		// requests before this one have spoken: the ledger's `available` minus the units their
		// undrained events already claim. Read after the lock, so a sibling committed while we
		// waited is counted either way — as an event, or in the ledger once relayed. The
		// ledger's non-negative flag stays the backstop; this keeps the relay from having to use it.
		sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('units:' || $1 || ':' || $2, 0))")
			.bind(user.raw().to_string())
			.bind(service.as_str())
			.execute(&mut *tx)
			.await
			.map_err(repo_err)?;
		let available = self.ledger.balance(&LedgerAccountKey::UserShares(service.clone(), user)).await?.available();
		let claimed = outbox::units_in_flight(&mut tx, user.raw(), service.as_str()).await?;
		if available.saturating_sub(claimed) < redemption.units().base_units() {
			return Err(DomainError::Validation("insufficient units to redeem".into()));
		}
		insert_row(&mut tx, redemption).await?;
		outbox::drain_to_outbox(&mut tx, redemption, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(())
	}

	async fn settle(&self, id: RedemptionId, nav: Nav) -> Result<Redemption, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let mut redemption = load_for_update(&mut tx, id).await?;
		// A repeat settle is a full no-op under the row lock: the price is fixed by the first.
		if redemption.is_priced() {
			return Ok(redemption);
		}
		redemption.settle(nav)?;
		update_row(&mut tx, &redemption).await?;
		outbox::drain_to_outbox(&mut tx, &mut redemption, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(redemption)
	}

	async fn fail(&self, id: RedemptionId) -> Result<Redemption, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let mut redemption = load_for_update(&mut tx, id).await?;
		redemption.fail()?;
		update_row(&mut tx, &redemption).await?;
		outbox::drain_to_outbox(&mut tx, &mut redemption, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(redemption)
	}

	async fn cancel(&self, id: RedemptionId) -> Result<Redemption, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let mut redemption = load_for_update(&mut tx, id).await?;
		redemption.cancel()?;
		update_row(&mut tx, &redemption).await?;
		outbox::drain_to_outbox(&mut tx, &mut redemption, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(redemption)
	}

	async fn awaiting_operation(&self, id: RedemptionId) -> Result<Option<AwaitingOperation>, DomainError> {
		Ok(awaiting_payout(&self.pool, id.raw()).await?.map(|awaiting| AwaitingOperation {
			scope: awaiting.scope,
			operation: awaiting.operation,
		}))
	}

	async fn refuse_awaiting(&self, id: RedemptionId, operation: &str, reason: &str) -> Result<RedemptionRecord, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		// The same lock and predicate as the money-event consumer's verdict: whichever
		// takes the row first decides, the other finds it moved.
		let locked: Option<Uuid> = sqlx::query_scalar("SELECT id FROM redemptions WHERE id = $1 AND payout_operation_id = $2 AND state = 'awaiting_approval' FOR UPDATE")
			.bind(id.raw())
			.bind(operation)
			.fetch_optional(&mut *tx)
			.await
			.map_err(repo_err)?;
		if locked.is_none() {
			let state: Option<String> = sqlx::query_scalar("SELECT state FROM redemptions WHERE id = $1")
				.bind(id.raw())
				.fetch_optional(&mut *tx)
				.await
				.map_err(repo_err)?;
			return Err(match state {
				Some(state) => DomainError::Conflict(format!("redemption is {state}, not awaiting approval: banking's verdict landed first")),
				None => DomainError::NotFound {
					entity: "redemption",
					id: id.to_string(),
				},
			});
		}
		apply_locked(&mut tx, id.raw(), PayoutVerdict::Refused(reason)).await?;
		tx.commit().await.map_err(repo_err)?;
		self.find_record(id).await?.ok_or_else(|| DomainError::NotFound {
			entity: "redemption",
			id: id.to_string(),
		})
	}

	async fn find_record(&self, id: RedemptionId) -> Result<Option<RedemptionRecord>, DomainError> {
		let row = sqlx::query_as::<_, RedemptionRow>(SELECT_BY_ID)
			.bind(id.raw())
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?;
		row.map(RedemptionRow::into_record).transpose()
	}

	async fn find_by_id(&self, id: RedemptionId) -> Result<Option<Redemption>, DomainError> {
		let row = sqlx::query_as::<_, RedemptionRow>(SELECT_BY_ID)
			.bind(id.raw())
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?;
		row.map(RedemptionRow::into_domain).transpose()
	}

	async fn list_by_user(&self, user: UserId) -> Result<Vec<RedemptionRecord>, DomainError> {
		let rows = sqlx::query_as::<_, RedemptionRow>(SELECT_BY_USER)
			.bind(user.raw())
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?;
		rows.into_iter().map(RedemptionRow::into_record).collect()
	}

	async fn list_queued(&self) -> Result<Vec<QueuedRedemption>, DomainError> {
		let rows = sqlx::query_as::<_, QueuedRow>(
			"SELECT id, user_id, service, units, EXTRACT(EPOCH FROM created_at)::BIGINT AS created_at \
			 FROM redemptions WHERE state = 'queued' ORDER BY created_at ASC",
		)
		.fetch_all(&self.pool)
		.await
		.map_err(repo_err)?;
		rows.into_iter()
			.map(|r| {
				Ok(QueuedRedemption {
					id: RedemptionId::from_raw(r.id),
					user_id: UserId::from_raw(r.user_id),
					service: ServiceId::parse(&r.service)?,
					units: Shares::from_base_units(parse_units(&r.units, "redemption units")?),
					created_at: r.created_at,
				})
			})
			.collect()
	}
}

/// A lightweight row for the operator queue read (with the DB-managed `created_at`).
#[derive(sqlx::FromRow)]
struct QueuedRow {
	id: Uuid,
	user_id: Uuid,
	service: String,
	units: String,
	created_at: i64,
}
