//! Postgres adapter for the [`FundPositionReader`] port — reads the `fund_positions`
//! projection (cost basis + high-water mark) the relay maintains, and the holder's unit
//! flows merged from the records of every channel that moves units — subscriptions,
//! redemptions, fee charges, in-kind issuances and book fills.

use async_trait::async_trait;
use invest_domain::{
	balance::ServiceId,
	error::DomainError,
	money::{Nav, Usdt},
	users::UserId,
};
use sqlx::{PgPool, Row};

use crate::ports::positions::{FundPosition, FundPositionReader, UnitFlow};

/// A holder's unit moves in one fund, merged in the database like the operation feed
/// is. Each branch is the control-plane record the write side already keeps, stamped at
/// the moment the units actually moved: a subscription once settled (`updated_at` once
/// `settled` — a hold banking refused or a capture rolled back moved no units), a
/// redemption once completed (`updated_at` once `completed` — the reserve at request leaves
/// the units owned), a fee at assessment, an issuance when the
/// relay applied it, a book fill at execution — the buyer gains `size`, the seller loses
/// it. `delta` is signed TEXT: the tables store unsigned base units as TEXT (never a
/// number SQL reasons about), and the sign is the one thing added here. Seconds are
/// floored like the marks' (`nav.rs`), so a flow and a mark in the same second agree.
const UNIT_FLOWS_SQL: &str = "SELECT FLOOR(EXTRACT(EPOCH FROM ts))::bigint AS at_unix, delta FROM (
    SELECT updated_at AS ts, units AS delta
      FROM subscriptions WHERE user_id = $1 AND service = $2 AND state = 'settled'
    UNION ALL
    SELECT updated_at, '-' || units
      FROM redemptions WHERE user_id = $1 AND service = $2 AND state = 'completed'
    UNION ALL
    SELECT assessed_at, '-' || charged_units
      FROM fee_assessments WHERE user_id = $1 AND service = $2 AND charged_units <> '0'
    UNION ALL
    SELECT applied_at, units
      FROM unit_issuances WHERE holder_kind = 'user' AND holder_id = $1 AND service = $2 AND state = 'applied'
    UNION ALL
    SELECT executed_at, size
      FROM book_trades WHERE buyer_id = $1 AND service = $2 AND busted_at IS NULL
    UNION ALL
    SELECT executed_at, '-' || size
      FROM book_trades WHERE seller_id = $1 AND service = $2 AND busted_at IS NULL
) AS flows
ORDER BY ts ASC";

pub struct PgFundPositions {
	pool: PgPool,
}

impl PgFundPositions {
	pub fn new(pool: PgPool) -> Self {
		Self { pool }
	}
}

#[async_trait]
impl FundPositionReader for PgFundPositions {
	async fn find(&self, user: UserId, service: &ServiceId) -> Result<Option<FundPosition>, DomainError> {
		let row = sqlx::query("SELECT service, cost_basis, high_water_mark FROM fund_positions WHERE user_id = $1 AND service = $2")
			.bind(user.raw())
			.bind(service.as_str())
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?;
		row.map(|row| position_from_row(&row)).transpose()
	}

	async fn list(&self, user: UserId) -> Result<Vec<FundPosition>, DomainError> {
		let rows = sqlx::query("SELECT service, cost_basis, high_water_mark FROM fund_positions WHERE user_id = $1 ORDER BY service")
			.bind(user.raw())
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?;
		rows.iter().map(position_from_row).collect()
	}

	async fn unit_flows(&self, user: UserId, service: &ServiceId) -> Result<Vec<UnitFlow>, DomainError> {
		let rows = sqlx::query_as::<_, (i64, String)>(UNIT_FLOWS_SQL)
			.bind(user.raw())
			.bind(service.as_str())
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?;
		rows.into_iter()
			.map(|(at_unix, delta)| {
				let delta = delta.parse::<i128>().map_err(|_| DomainError::Repository("malformed base-unit amount in a unit flow".into()))?;
				Ok(UnitFlow { at_unix, delta })
			})
			.collect()
	}
}

fn position_from_row(row: &sqlx::postgres::PgRow) -> Result<FundPosition, DomainError> {
	Ok(FundPosition {
		service: ServiceId::parse(row.try_get::<String, _>("service").map_err(repo_err)?.as_str())?,
		cost_basis: Usdt::from_base_units(parse_base_units(row.try_get("cost_basis").map_err(repo_err)?)?),
		high_water_mark: Nav::from_base_units(parse_base_units(row.try_get("high_water_mark").map_err(repo_err)?)?),
	})
}

fn parse_base_units(raw: String) -> Result<u128, DomainError> {
	raw.parse::<u128>().map_err(|_| DomainError::Repository("malformed base-unit amount in fund_positions".into()))
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

/// A holder's position gains `units` bought for `cost_basis` at `nav`, on the caller's
/// transaction: basis and units add up, and the high-water mark is the higher of the two —
/// the one writer of a position's growth, for a subscription, an in-kind mint and a buy.
/// The caller has carried the accrual of the old basis first (`fee_accrual`).
pub(crate) async fn add_to_position(
	conn: &mut sqlx::PgConnection,
	user: UserId,
	service: &ServiceId,
	cost_basis: Usdt,
	units: invest_domain::money::Shares,
	nav: Nav,
) -> Result<(), sqlx::Error> {
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
	.bind(cost_basis.base_units().to_string())
	.bind(units.base_units().to_string())
	.bind(nav.base_units().to_string())
	.execute(conn)
	.await?;
	Ok(())
}
