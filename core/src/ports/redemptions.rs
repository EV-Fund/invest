//! Persistence + read port for the [`Redemption`] aggregate.
//!
//! Each command is internally atomic and row-locked — load `FOR UPDATE`, apply the
//! aggregate command inside the lock, then persist the transition with the drained
//! events. The relay reserves the pending burn, then (at settle) asks banking for the
//! payout and posts the burn (Write-Last); the cost-basis reduction is the relay's too,
//! made once the cash has left, so a refused payout has no basis to give back.

use async_trait::async_trait;
use invest_domain::{
	architecture::{Reader, Repository},
	balance::ServiceId,
	error::DomainError,
	money::{Nav, Shares},
	redemptions::{Redemption, RedemptionId},
	users::UserId,
};

#[async_trait]
pub trait RedemptionRepository: Repository<Aggregate = Redemption> + Reader<Aggregate = Redemption> {
	/// Persist a brand-new redemption (`Queued`) + its `Requested` event — the relay
	/// reserves a pending burn of the units. Takes a `FOR UPDATE` lock on the user's
	/// `fund_positions` row to serialize concurrent requests at the Postgres layer
	/// (TigerBeetle's non-negative flag is the actual over-redeem backstop).
	async fn open(&self, redemption: &mut Redemption) -> Result<(), DomainError>;

	/// Settle a queued redemption at the settle-time `nav`: apply [`Redemption::settle`]
	/// under the row lock and persist + drain the `Settled` event — the relay pays the cash,
	/// posts the burn and reduces the cost basis. Idempotent; returns the aggregate.
	async fn settle(&self, id: RedemptionId, nav: Nav) -> Result<Redemption, DomainError>;

	/// Fail a queued redemption (operator): apply [`Redemption::fail`] under the lock,
	/// persist + drain the `Failed` event (the relay voids the burn, returning the units).
	async fn fail(&self, id: RedemptionId) -> Result<Redemption, DomainError>;

	/// Cancel a queued redemption (user): apply [`Redemption::cancel`] under the lock,
	/// persist + drain the `Cancelled` event (the relay voids the burn). Idempotent.
	async fn cancel(&self, id: RedemptionId) -> Result<Redemption, DomainError>;

	/// Load a redemption by id (no lock; for queries).
	async fn find_by_id(&self, id: RedemptionId) -> Result<Option<Redemption>, DomainError>;

	/// A user's redemptions (projection), newest first, each with the reason banking gave
	/// when it refused the payout.
	async fn list_by_user(&self, user: UserId) -> Result<Vec<RedemptionRecord>, DomainError>;

	/// The cross-user queue of redemptions awaiting settle (state `queued`), oldest
	/// first — the operator "clear the queue" read.
	async fn list_queued(&self) -> Result<Vec<QueuedRedemption>, DomainError>;
}

/// A redemption as the store holds it: the aggregate plus the reason the relay recorded
/// when banking refused its payout (`None` in every other case).
#[derive(Clone, Debug)]
pub struct RedemptionRecord {
	pub redemption: Redemption,
	pub failure_reason: Option<String>,
}

/// A queued redemption for the operator "clear the queue" surface — a lightweight read
/// projection that also carries the DB-managed `created_at`, which the [`Redemption`]
/// aggregate does not model.
pub struct QueuedRedemption {
	pub id: RedemptionId,
	pub user_id: UserId,
	pub service: ServiceId,
	pub units: Shares,
	/// Unix seconds the redemption was requested (for the "age" column).
	pub created_at: i64,
}
