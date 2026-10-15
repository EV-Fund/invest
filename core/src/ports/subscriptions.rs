//! Persistence + read port for the [`Subscription`] aggregate — the record of the
//! subscription saga (`invest_domain::subscriptions`).
//!
//! The use case writes the record (`Reserving`) BEFORE it asks banking for the hold, so a
//! hold banking placed is always traceable to a row; then stamps the outcome — the hold's
//! id and the `Reserved` event (whose outbox row the relay turns into the mint and the
//! capture), or the refusal. Each method is atomic. The relay writes the terminal states
//! itself, after the ledgers moved.

use async_trait::async_trait;
use invest_domain::{
	architecture::Repository,
	error::DomainError,
	subscriptions::{Subscription, SubscriptionId},
	users::UserId,
};

use crate::ports::money::{HoldId, ScopeId};

#[async_trait]
pub trait SubscriptionRepository: Repository<Aggregate = Subscription> {
	/// Persist a brand-new subscription, `Reserving`, in `scope` — the scope its hold will
	/// be placed in and captured from.
	async fn open(&self, subscription: &Subscription, scope: &ScopeId) -> Result<(), DomainError>;

	/// banking holds the cash: apply [`Subscription::reserved`], record `hold`, and drain
	/// the `Reserved` event into the outbox — in one transaction.
	async fn reserved(&self, subscription: &mut Subscription, hold: &HoldId) -> Result<(), DomainError>;

	/// banking refused the hold (or could not be asked): apply [`Subscription::refuse`]
	/// and record why.
	async fn refused(&self, subscription: &mut Subscription, reason: &str) -> Result<(), DomainError>;

	/// Load a subscription by id (no lock; for queries).
	async fn find_by_id(&self, id: SubscriptionId) -> Result<Option<SubscriptionRecord>, DomainError>;

	/// A user's subscriptions, newest first.
	async fn list_by_user(&self, user: UserId) -> Result<Vec<SubscriptionRecord>, DomainError>;
}

/// A subscription as the store holds it: the aggregate plus what the saga recorded about
/// it.
#[derive(Clone, Debug)]
pub struct SubscriptionRecord {
	pub subscription: Subscription,
	pub scope: ScopeId,
	pub hold_id: Option<HoldId>,
	/// Why it failed — banking's refusal reason, or the relay's rollback reason.
	pub failure_reason: Option<String>,
	/// Unix seconds.
	pub created_at: i64,
}
