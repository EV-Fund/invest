//! `subscriptions` bounded context — a user buying fund units (shares).
//!
//! Subscribing directs `cash` of a user's compartment in the allocation's scope into the
//! fund and mints `units = cash / NAV` of the **service currency** at the current price.
//! The cash is banking's and the units are invest's, so the two legs cannot share a
//! transaction; a subscription is a short **saga** (plan §6.3):
//!
//! 1. `Reserving` — the record is written, then banking is asked to hold `cash` on the
//!    user's compartment (`Reserve`). A refusal (no mandate, not enough money) fails the
//!    subscription before anything moved.
//! 2. `Reserved` — the hold stands; the relay mints the units as a **pending** transfer
//!    `Dr UserShares / Cr SharesOutstanding`, captures the hold into the allocation's
//!    account `(<scope>, alloc:<service>)`, and only then posts the units.
//! 3. `Settled`, or `Failed` — a capture banking refuses (the hold expired or was voided)
//!    voids what is left of the hold and the pending units: never units without cash,
//!    never cash without units.
//!
//! Pure and wasm-safe: ids are minted by the application layer, no clock, no I/O. NAV is
//! supplied by the caller (read from the valuation control plane), never derived here.

use ev::architecture::{AggregateRoot, DomainEvent, EmitsEvents, Entity, Id};
use serde::{Deserialize, Serialize};

use crate::{
	balance::ServiceId,
	error::DomainError,
	money::{Nav, Shares, Usdt},
	users::UserId,
};

/// A unique subscription id (UUID). Minted by the application layer.
pub type SubscriptionId = Id<SubscriptionTag>;
/// Phantom tag making [`SubscriptionId`] a distinct, incompatible identity type.
pub struct SubscriptionTag;

/// Where a subscription's saga stands. `Reserving` and `Reserved` are in flight; the rest
/// are terminal.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionState {
	/// Recorded; banking has not (yet) confirmed the hold on the user's cash.
	Reserving,
	/// The hold stands; the relay mints and captures.
	Reserved,
	/// Cash captured into the allocation, units posted (terminal).
	Settled,
	/// Refused or rolled back; no units and no cash moved for good (terminal).
	Failed,
}

wire_vocabulary!(SubscriptionState, "subscription state", {
	Reserving => "reserving",
	Reserved => "reserved",
	Settled => "settled",
	Failed => "failed",
});

/// The subscription aggregate — the saga's record of one mint. Construct via
/// [`Subscription::open`] or [`Subscription::rehydrate`] (load from the store, no
/// events).
#[derive(Clone, Debug)]
pub struct Subscription {
	id: SubscriptionId,
	user: UserId,
	service: ServiceId,
	cash: Usdt,
	nav: Nav,
	units: Shares,
	state: SubscriptionState,
	pending: Vec<SubscriptionEvent>,
}

impl Subscription {
	/// Subscribe `cash` into `service` at `nav`, minting `floor(cash / nav)` units.
	/// Rejects a zero `cash` and a `cash` too small to buy a single base-unit of share
	/// (`units == 0`). Starts `Reserving` and raises nothing: no unit may move before
	/// banking holds the cash.
	pub fn open(id: SubscriptionId, user: UserId, service: ServiceId, cash: Usdt, nav: Nav) -> Result<Self, DomainError> {
		if cash.is_zero() {
			return Err(DomainError::Validation("subscription amount must be positive".into()));
		}
		let units = Shares::from_cash(cash, nav)?;
		if units.is_zero() {
			return Err(DomainError::Validation("amount too small to buy a share at the current nav".into()));
		}
		Ok(Self {
			id,
			user,
			service,
			cash,
			nav,
			units,
			state: SubscriptionState::Reserving,
			pending: Vec::new(),
		})
	}

	/// Reconstitute from the store. Raises no events.
	pub fn rehydrate(id: SubscriptionId, user: UserId, service: ServiceId, cash: Usdt, nav: Nav, units: Shares, state: SubscriptionState) -> Self {
		Self {
			id,
			user,
			service,
			cash,
			nav,
			units,
			state,
			pending: Vec::new(),
		}
	}

	/// banking holds the cash: raise `Reserved`, which the relay turns into the mint and
	/// the capture. Idempotent once reserved; only a `Reserving` subscription can be.
	pub fn reserved(&mut self) -> Result<(), DomainError> {
		match self.state {
			SubscriptionState::Reserved => Ok(()),
			SubscriptionState::Reserving => {
				self.state = SubscriptionState::Reserved;
				self.pending.push(SubscriptionEvent::Reserved {
					subscription_id: self.id,
					user: self.user,
					service: self.service.clone(),
					cash: self.cash,
					nav: self.nav,
					units: self.units,
				});
				Ok(())
			}
			SubscriptionState::Settled | SubscriptionState::Failed => Err(DomainError::Conflict(format!("subscription is {}, not reservable", self.state.as_str()))),
		}
	}

	/// banking refused the hold: the subscription ends before anything moved. Legal only
	/// while `Reserving` — once reserved, only the relay may roll it back, because only it
	/// knows whether the units leg was applied.
	pub fn refuse(&mut self) -> Result<(), DomainError> {
		match self.state {
			SubscriptionState::Failed => Ok(()),
			SubscriptionState::Reserving => {
				self.state = SubscriptionState::Failed;
				Ok(())
			}
			SubscriptionState::Reserved | SubscriptionState::Settled => Err(DomainError::Conflict(format!("subscription is {}, not refusable", self.state.as_str()))),
		}
	}

	pub fn id(&self) -> SubscriptionId {
		self.id
	}

	pub fn user(&self) -> UserId {
		self.user
	}

	pub fn service(&self) -> &ServiceId {
		&self.service
	}

	pub fn cash(&self) -> Usdt {
		self.cash
	}

	pub fn nav(&self) -> Nav {
		self.nav
	}

	pub fn units(&self) -> Shares {
		self.units
	}

	pub fn state(&self) -> SubscriptionState {
		self.state
	}
}

impl Entity for Subscription {
	type Id = SubscriptionId;

	fn id(&self) -> SubscriptionId {
		self.id
	}
}

impl AggregateRoot for Subscription {
	const NAME: &'static str = "subscription";
}

/// Facts raised by the [`Subscription`] aggregate. `Reserved` carries everything the
/// relay needs for the units leg with no extra read; the hold it captures is the store's
/// (it is banking's id, not a domain fact). Internally tagged so the stored JSON is
/// self-describing.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SubscriptionEvent {
	/// The cash is held (relay: pending `Dr UserShares / Cr SharesOutstanding` for
	/// `units`, capture the hold into the allocation's account, post the units).
	Reserved {
		subscription_id: SubscriptionId,
		user: UserId,
		service: ServiceId,
		cash: Usdt,
		nav: Nav,
		units: Shares,
	},
}

impl DomainEvent for SubscriptionEvent {
	const KIND: &'static str = "subscriptions";
}

impl EmitsEvents for Subscription {
	type Event = SubscriptionEvent;

	fn drain_events(&mut self) -> Vec<SubscriptionEvent> {
		core::mem::take(&mut self.pending)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn user() -> UserId {
		UserId::new()
	}
	fn svc() -> ServiceId {
		ServiceId::parse("trading").unwrap()
	}

	#[test]
	fn open_prices_the_units_and_raises_nothing_until_the_cash_is_held() {
		// 300 USDT at NAV 1.5 → 200 units.
		let mut s = Subscription::open(SubscriptionId::new(), user(), svc(), Usdt::parse_decimal("300").unwrap(), Nav::parse_decimal("1.5").unwrap()).unwrap();
		assert_eq!(s.units(), Shares::parse_decimal("200").unwrap());
		assert_eq!(s.state(), SubscriptionState::Reserving);
		assert!(s.drain_events().is_empty(), "no unit moves before banking holds the cash");
		s.reserved().unwrap();
		let events = s.drain_events();
		assert_eq!(events.len(), 1);
		assert!(matches!(events[0], SubscriptionEvent::Reserved { .. }));
		s.reserved().unwrap();
		assert!(s.drain_events().is_empty(), "reserving twice raises nothing twice");
		assert!(s.refuse().is_err(), "a reserved subscription is the relay's to roll back");
	}

	#[test]
	fn a_refused_hold_fails_the_subscription_for_good() {
		let mut s = Subscription::open(SubscriptionId::new(), user(), svc(), Usdt::parse_decimal("10").unwrap(), Nav::SEED).unwrap();
		s.refuse().unwrap();
		assert_eq!(s.state(), SubscriptionState::Failed);
		assert!(s.reserved().is_err());
		assert!(s.drain_events().is_empty());
	}

	#[test]
	fn seed_nav_mints_one_for_one() {
		let s = Subscription::open(SubscriptionId::new(), user(), svc(), Usdt::parse_decimal("100").unwrap(), Nav::SEED).unwrap();
		assert_eq!(s.units(), Shares::parse_decimal("100").unwrap());
	}

	#[test]
	fn zero_cash_and_dust_are_rejected() {
		assert!(Subscription::open(SubscriptionId::new(), user(), svc(), Usdt::ZERO, Nav::SEED).is_err());
		// 2 base units of USDT at NAV 3 floors to 0 units → rejected.
		assert!(Subscription::open(SubscriptionId::new(), user(), svc(), Usdt::from_base_units(2), Nav::parse_decimal("3").unwrap()).is_err());
	}

	#[test]
	fn event_round_trips_through_json() {
		let mut s = Subscription::open(SubscriptionId::new(), user(), svc(), Usdt::parse_decimal("100").unwrap(), Nav::SEED).unwrap();
		s.reserved().unwrap();
		let event = s.drain_events().pop().unwrap();
		let json = serde_json::to_string(&event).unwrap();
		let back: SubscriptionEvent = serde_json::from_str(&json).unwrap();
		assert!(matches!(back, SubscriptionEvent::Reserved { .. }));
	}
}
