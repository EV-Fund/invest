//! Postgres adapter for the [`RedemptionRepository`] port.
//!
//! Row-locked commands that apply the aggregate transition and drain its events in one
//! transaction. `open` takes a `FOR UPDATE` lock on the user's `fund_positions` row to
//! serialize concurrent requests. The cost-basis reduction a settle owes is NOT made here:
//! the payout is banking's and may still be refused, so the relay makes it once the cash
//! has left ([`reduce_cost_basis`], under the saga's marker) — a refused payout then has no
//! basis to give back.

use async_trait::async_trait;
use invest_domain::{
	architecture::{Reader, Repository},
	balance::ServiceId,
	error::DomainError,
	money::{Nav, Shares, Usdt},
	redemptions::{Redemption, RedemptionId, RedemptionState},
	users::UserId,
};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
	infrastructure::{fee_accrual, outbox},
	ports::{QueuedRedemption, RedemptionRecord, RedemptionRepository},
};

const SELECT_BY_ID: &str = "SELECT id, user_id, service, units, nav, cash, state, failure_reason FROM redemptions WHERE id = $1";
const SELECT_BY_ID_FOR_UPDATE: &str = "SELECT id, user_id, service, units, nav, cash, state, failure_reason FROM redemptions WHERE id = $1 FOR UPDATE";
const SELECT_BY_USER: &str = "SELECT id, user_id, service, units, nav, cash, state, failure_reason FROM redemptions WHERE user_id = $1 ORDER BY created_at DESC";

pub struct PgRedemptions {
	pool: PgPool,
}

impl PgRedemptions {
	pub fn new(pool: PgPool) -> Self {
		Self { pool }
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

#[async_trait]
impl RedemptionRepository for PgRedemptions {
	async fn open(&self, redemption: &mut Redemption) -> Result<(), DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		// Serialize concurrent requests on this position (TB is the over-redeem backstop).
		sqlx::query("SELECT 1 FROM fund_positions WHERE user_id = $1 AND service = $2 FOR UPDATE")
			.bind(redemption.user().raw())
			.bind(redemption.service().as_str())
			.fetch_optional(&mut *tx)
			.await
			.map_err(repo_err)?;
		insert_row(&mut tx, redemption).await?;
		outbox::drain_to_outbox(&mut tx, redemption, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(())
	}

	async fn settle(&self, id: RedemptionId, nav: Nav) -> Result<Redemption, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let mut redemption = load_for_update(&mut tx, id).await?;
		// A repeat settle is a full no-op under the row lock: the price is fixed by the first.
		if matches!(redemption.state(), RedemptionState::Settling | RedemptionState::Completed) {
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
