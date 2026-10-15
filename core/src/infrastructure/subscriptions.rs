//! Postgres adapter for the [`SubscriptionRepository`] port — the record of the subscription
//! saga.
//!
//! `open` writes the row `reserving` before banking is asked for the hold; `reserved`
//! stamps the hold and drains the `Reserved` event to the outbox in one transaction;
//! `refused` closes a subscription banking would not hold cash for. The per-investor
//! `fund_positions` cost-basis projection is **not** written here: the relay applies it
//! only after the cash was captured and the units posted (see
//! [`super::relay`]), so a rolled-back subscription can never strand a phantom position.

use async_trait::async_trait;
use invest_domain::{
	architecture::Repository,
	balance::ServiceId,
	error::DomainError,
	money::{Nav, Shares, Usdt},
	subscriptions::{Subscription, SubscriptionId, SubscriptionState},
	users::UserId,
};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
	infrastructure::{fees::parse_units, outbox},
	ports::{
		SubscriptionRecord, SubscriptionRepository,
		money::{HoldId, ScopeId},
	},
};

pub struct PgSubscriptions {
	pool: PgPool,
}

impl PgSubscriptions {
	pub fn new(pool: PgPool) -> Self {
		Self { pool }
	}
}

impl Repository for PgSubscriptions {
	type Aggregate = Subscription;
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

/// The column list every read selects — a macro, not a `const`, because sqlx 0.9 takes
/// only `&'static str` SQL and `concat!` keeps it one.
macro_rules! columns {
	() => {
		"id, user_id, service, cash, nav, units, state, scope, hold_id, failure_reason, EXTRACT(EPOCH FROM created_at)::bigint AS created_at_unix"
	};
}

#[derive(sqlx::FromRow)]
struct SubscriptionRow {
	id: Uuid,
	user_id: Uuid,
	service: String,
	cash: String,
	nav: String,
	units: String,
	state: String,
	scope: String,
	hold_id: Option<String>,
	failure_reason: Option<String>,
	created_at_unix: i64,
}

impl SubscriptionRow {
	fn into_record(self) -> Result<SubscriptionRecord, DomainError> {
		let subscription = Subscription::rehydrate(
			SubscriptionId::from_raw(self.id),
			UserId::from_raw(self.user_id),
			ServiceId::parse(&self.service)?,
			Usdt::from_base_units(parse_units(&self.cash, "subscription cash")?),
			Nav::from_base_units(parse_units(&self.nav, "subscription nav")?),
			Shares::from_base_units(parse_units(&self.units, "subscription units")?),
			SubscriptionState::parse(&self.state)?,
		);
		Ok(SubscriptionRecord {
			subscription,
			scope: ScopeId::parse(&self.scope).map_err(|err| DomainError::Repository(format!("subscriptions.scope: {err}")))?,
			hold_id: self.hold_id.map(HoldId),
			failure_reason: self.failure_reason,
			created_at: self.created_at_unix,
		})
	}
}

/// Load the row `FOR UPDATE` and check it is the one the caller holds — the opening move
/// of both outcomes, so a second answer for the same subscription serializes behind the
/// first and finds it already applied.
async fn lock_state(conn: &mut PgConnection, id: SubscriptionId) -> Result<SubscriptionState, DomainError> {
	let state: String = sqlx::query_scalar("SELECT state FROM subscriptions WHERE id = $1 FOR UPDATE")
		.bind(id.raw())
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?
		.ok_or_else(|| DomainError::NotFound {
			entity: "subscription",
			id: id.to_string(),
		})?;
	SubscriptionState::parse(&state)
}

#[async_trait]
impl SubscriptionRepository for PgSubscriptions {
	async fn open(&self, subscription: &Subscription, scope: &ScopeId) -> Result<(), DomainError> {
		sqlx::query("INSERT INTO subscriptions (id, user_id, service, cash, nav, units, state, scope) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)")
			.bind(subscription.id().raw())
			.bind(subscription.user().raw())
			.bind(subscription.service().as_str())
			.bind(subscription.cash().base_units().to_string())
			.bind(subscription.nav().base_units().to_string())
			.bind(subscription.units().base_units().to_string())
			.bind(subscription.state().as_str())
			.bind(scope.as_str())
			.execute(&self.pool)
			.await
			.map_err(repo_err)?;
		Ok(())
	}

	async fn reserved(&self, subscription: &mut Subscription, hold: &HoldId) -> Result<(), DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		if lock_state(&mut tx, subscription.id()).await? == SubscriptionState::Reserved {
			// Already recorded by an earlier answer for the same hold: nothing to drain twice.
			return Ok(());
		}
		subscription.reserved()?;
		sqlx::query("UPDATE subscriptions SET state = $2, hold_id = $3, updated_at = now() WHERE id = $1")
			.bind(subscription.id().raw())
			.bind(subscription.state().as_str())
			.bind(&hold.0)
			.execute(&mut *tx)
			.await
			.map_err(repo_err)?;
		outbox::drain_to_outbox(&mut tx, subscription, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(())
	}

	async fn refused(&self, subscription: &mut Subscription, reason: &str) -> Result<(), DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		lock_state(&mut tx, subscription.id()).await?;
		subscription.refuse()?;
		sqlx::query("UPDATE subscriptions SET state = $2, failure_reason = $3, updated_at = now() WHERE id = $1 AND state = 'reserving'")
			.bind(subscription.id().raw())
			.bind(subscription.state().as_str())
			.bind(reason)
			.execute(&mut *tx)
			.await
			.map_err(repo_err)?;
		tx.commit().await.map_err(repo_err)?;
		Ok(())
	}

	async fn find_by_id(&self, id: SubscriptionId) -> Result<Option<SubscriptionRecord>, DomainError> {
		let row = sqlx::query_as::<_, SubscriptionRow>(concat!("SELECT ", columns!(), " FROM subscriptions WHERE id = $1"))
			.bind(id.raw())
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?;
		row.map(SubscriptionRow::into_record).transpose()
	}

	async fn list_by_user(&self, user: UserId) -> Result<Vec<SubscriptionRecord>, DomainError> {
		let rows = sqlx::query_as::<_, SubscriptionRow>(concat!("SELECT ", columns!(), " FROM subscriptions WHERE user_id = $1 ORDER BY created_at DESC, id"))
			.bind(user.raw())
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?;
		rows.into_iter().map(SubscriptionRow::into_record).collect()
	}
}
