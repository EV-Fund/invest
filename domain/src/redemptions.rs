//! `redemptions` bounded context — a user redeeming fund units back to cash.
//!
//! Redeeming burns `units` of the **service currency** and pays the holder
//! `cash = units × NAV` out of the allocation's account in banking, `(<scope>,
//! alloc:<service>)`. It is the constrained direction (the fund's realizable cash may be
//! short of marked value), so it is a **saga with an explicit queue**. The units are
//! reserved (a TB **pending burn**) the instant the request is recorded, which is what
//! locks them against double-redeem; the cash is paid only at **settle**, gated by the
//! account's liquidity (**accept & queue** when short).
//!
//! - **Settle is a step, not a moment** — the payout is banking's (`Payout`), the burn is
//!   invest's, and they cannot share a transaction. `settle` prices the redemption and
//!   moves it to `Settling`; the relay pays out, then posts the burn and stamps it
//!   `Completed`. A payout banking refuses (the account is short, the holder is frozen)
//!   voids the burn and stamps it `Failed`: the units go back.
//! - **Past the automatic ceilings the owners decide.** banking answers such a payout
//!   `approval_required`; the relay asks for it instead (`RequestPayout`) and parks the
//!   redemption `AwaitingApproval`, the burn still pending. banking's verdict comes back
//!   through its money events: executed → [`Redemption::payout_executed`] (the relay posts
//!   the burn), rejected or failed → [`Redemption::payout_refused`] (the relay voids it).
//! - **Settle-time pricing** — `units` are fixed at request, but `cash = units × NAV` is
//!   computed from the NAV *at settle* (execution-day pricing), so a queue that drains
//!   after a NAV drop doesn't overpay the redeeming holder at the others' expense.
//!
//! Pure and wasm-safe: ids minted by the application layer, no clock, no I/O. The relay
//! maps each event to its steps (payout first, burn second); that lives in the adapter.

use ev::architecture::{AggregateRoot, DomainEvent, EmitsEvents, Entity, Id};
use serde::{Deserialize, Serialize};

use crate::{
	balance::ServiceId,
	error::DomainError,
	money::{Nav, Shares, Usdt},
	users::UserId,
};

/// A unique redemption id (UUID). Minted by the application layer.
pub type RedemptionId = Id<RedemptionTag>;
/// Phantom tag making [`RedemptionId`] a distinct, incompatible identity type.
pub struct RedemptionTag;

/// Lifecycle of a redemption. `Queued`, `Settling` and `AwaitingApproval` are in flight
/// (the units are reserved as a TB pending burn); the rest are terminal. Cancel/fail are
/// legal **only while queued** — once settling, the payout may already have left (or be
/// before the owners), and only banking's answer may roll it back.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RedemptionState {
	/// Accepted; units reserved (pending burn), awaiting fund liquidity to pay out.
	Queued,
	/// Priced; the relay is paying out and burning.
	Settling,
	/// The payout is past banking's automatic ceilings and waits on the owners' approval
	/// (`RequestPayout`); the burn is still pending.
	AwaitingApproval,
	/// Paid out and burned (terminal).
	Completed,
	/// Released by an operator while queued, or the payout was refused at settle; the
	/// reservation was voided (terminal).
	Failed,
	/// Cancelled by the user while queued; the reservation was voided (terminal).
	Cancelled,
}

impl RedemptionState {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Queued => "queued",
			Self::Settling => "settling",
			Self::AwaitingApproval => "awaiting_approval",
			Self::Completed => "completed",
			Self::Failed => "failed",
			Self::Cancelled => "cancelled",
		}
	}

	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		match raw {
			"queued" => Ok(Self::Queued),
			"settling" => Ok(Self::Settling),
			"awaiting_approval" => Ok(Self::AwaitingApproval),
			"completed" => Ok(Self::Completed),
			"failed" => Ok(Self::Failed),
			"cancelled" => Ok(Self::Cancelled),
			other => Err(DomainError::Validation(format!("unknown redemption state: {other}"))),
		}
	}
}

/// The redemption aggregate — the saga's coordinator. Construct via
/// [`Redemption::request`] (raises [`RedemptionEvent::Requested`]) or
/// [`Redemption::rehydrate`]. `nav`/`cash` are `None` until settle (settle-time pricing).
#[derive(Clone, Debug)]
pub struct Redemption {
	id: RedemptionId,
	user: UserId,
	service: ServiceId,
	units: Shares,
	nav: Option<Nav>,
	cash: Option<Usdt>,
	state: RedemptionState,
	pending: Vec<RedemptionEvent>,
}

impl Redemption {
	/// Open a redemption of `units` from `service`. Raises `Requested`; the relay
	/// reserves a pending burn of the units (TigerBeetle rejects it atomically if it
	/// would exceed the user's holding). Starts `Queued`.
	pub fn request(id: RedemptionId, user: UserId, service: ServiceId, units: Shares) -> Result<Self, DomainError> {
		if units.is_zero() {
			return Err(DomainError::Validation("redemption units must be positive".into()));
		}
		let mut redemption = Self {
			id,
			user,
			service: service.clone(),
			units,
			nav: None,
			cash: None,
			state: RedemptionState::Queued,
			pending: Vec::new(),
		};
		redemption.pending.push(RedemptionEvent::Requested {
			redemption_id: id,
			user,
			service,
			units,
		});
		Ok(redemption)
	}

	/// Reconstitute from the store. Raises no events.
	pub fn rehydrate(id: RedemptionId, user: UserId, service: ServiceId, units: Shares, nav: Option<Nav>, cash: Option<Usdt>, state: RedemptionState) -> Self {
		Self {
			id,
			user,
			service,
			units,
			nav,
			cash,
			state,
			pending: Vec::new(),
		}
	}

	/// Settle a queued redemption at the **settle-time** `nav`: prices
	/// `cash = units × nav` and hands it to the relay. Idempotent once past pricing
	/// (settling, awaiting approval, completed); only a queued redemption can be settled.
	/// Raises `Settled` (relay: pay the cash out first, then post the pending burn).
	pub fn settle(&mut self, nav: Nav) -> Result<(), DomainError> {
		if self.is_priced() {
			return Ok(());
		}
		if self.state != RedemptionState::Queued {
			return Err(DomainError::Conflict(format!("redemption is {}, not settleable", self.state.as_str())));
		}
		let cash = nav.value(self.units)?;
		if cash.is_zero() {
			return Err(DomainError::Validation("redemption values to zero at the current nav".into()));
		}
		self.nav = Some(nav);
		self.cash = Some(cash);
		self.state = RedemptionState::Settling;
		self.pending.push(RedemptionEvent::Settled {
			redemption_id: self.id,
			user: self.user,
			service: self.service.clone(),
			units: self.units,
			nav,
			cash,
		});
		Ok(())
	}

	/// Whether the cash is already priced and handed to banking — a settle repeat is a no-op.
	pub fn is_priced(&self) -> bool {
		matches!(self.state, RedemptionState::Settling | RedemptionState::AwaitingApproval | RedemptionState::Completed)
	}

	/// banking executed the approved payout: the cash has left, so the burn is owed. Raises
	/// `PayoutExecuted` (relay: post the pending burn) and moves back to `Settling` until the
	/// relay stamps it `Completed`. Idempotent once settling or completed; legal only while
	/// awaiting approval.
	pub fn payout_executed(&mut self) -> Result<(), DomainError> {
		match self.state {
			RedemptionState::Settling | RedemptionState::Completed => Ok(()),
			RedemptionState::AwaitingApproval => {
				self.state = RedemptionState::Settling;
				self.pending.push(RedemptionEvent::PayoutExecuted {
					redemption_id: self.id,
					user: self.user,
					service: self.service.clone(),
					units: self.units,
				});
				Ok(())
			}
			RedemptionState::Queued | RedemptionState::Failed | RedemptionState::Cancelled =>
				Err(DomainError::Conflict(format!("redemption is {}, not awaiting approval", self.state.as_str()))),
		}
	}

	/// The approval said no, lapsed, or the approved payout could not execute: nothing left,
	/// so the units go back. Raises `Failed` (relay: void the pending burn). Idempotent once
	/// failed; legal only while awaiting approval.
	pub fn payout_refused(&mut self) -> Result<(), DomainError> {
		match self.state {
			RedemptionState::Failed => Ok(()),
			RedemptionState::AwaitingApproval => {
				self.state = RedemptionState::Failed;
				self.pending.push(RedemptionEvent::Failed {
					redemption_id: self.id,
					user: self.user,
					service: self.service.clone(),
					units: self.units,
				});
				Ok(())
			}
			RedemptionState::Queued | RedemptionState::Settling | RedemptionState::Completed | RedemptionState::Cancelled =>
				Err(DomainError::Conflict(format!("redemption is {}, not awaiting approval", self.state.as_str()))),
		}
	}

	/// Fail a queued redemption (operator): void the reservation, returning the units.
	/// Idempotent if already failed; legal only while queued (never after settle).
	pub fn fail(&mut self) -> Result<(), DomainError> {
		if self.leave_queue(RedemptionState::Failed, "failable")? {
			self.pending.push(RedemptionEvent::Failed {
				redemption_id: self.id,
				user: self.user,
				service: self.service.clone(),
				units: self.units,
			});
		}
		Ok(())
	}

	/// Cancel a queued redemption (the user changed their mind): void the reservation,
	/// returning the units. Idempotent if already cancelled; legal only while queued.
	pub fn cancel(&mut self) -> Result<(), DomainError> {
		if self.leave_queue(RedemptionState::Cancelled, "cancellable")? {
			self.pending.push(RedemptionEvent::Cancelled {
				redemption_id: self.id,
				user: self.user,
				service: self.service.clone(),
				units: self.units,
			});
		}
		Ok(())
	}

	/// Move a queued redemption to `to` — `false` when it is there already (the repeat is a
	/// no-op), a conflict from any other state: once settling, the units are being paid for.
	fn leave_queue(&mut self, to: RedemptionState, what: &str) -> Result<bool, DomainError> {
		if self.state == to {
			return Ok(false);
		}
		if self.state != RedemptionState::Queued {
			return Err(DomainError::Conflict(format!("redemption is {}, not {what}", self.state.as_str())));
		}
		self.state = to;
		Ok(true)
	}

	pub fn id(&self) -> RedemptionId {
		self.id
	}

	pub fn user(&self) -> UserId {
		self.user
	}

	pub fn service(&self) -> &ServiceId {
		&self.service
	}

	pub fn units(&self) -> Shares {
		self.units
	}

	pub fn nav(&self) -> Option<Nav> {
		self.nav
	}

	pub fn cash(&self) -> Option<Usdt> {
		self.cash
	}

	pub fn state(&self) -> RedemptionState {
		self.state
	}
}

impl Entity for Redemption {
	type Id = RedemptionId;

	fn id(&self) -> RedemptionId {
		self.id
	}
}

impl AggregateRoot for Redemption {
	const NAME: &'static str = "redemption";
}

/// Facts raised by the [`Redemption`] aggregate. Each carries the saga-relevant data
/// (user, service, units; plus nav/cash at settle) so the relay maps an event to its
/// ledger ops with no extra read. Internally tagged so the stored JSON is self-describing.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RedemptionEvent {
	/// Accepted; reserve a pending burn `Dr SharesOutstanding / Cr UserShares` for `units`.
	Requested {
		redemption_id: RedemptionId,
		user: UserId,
		service: ServiceId,
		units: Shares,
	},
	/// Priced (relay: pay `cash` out of `(<scope>, alloc:<service>)` into the user's
	/// compartment through banking, then post the pending burn). Pay-first because the
	/// payout is the leg banking may refuse; the pending burn already holds the units, so
	/// the burn cannot fail for want of them once the cash has left.
	Settled {
		redemption_id: RedemptionId,
		user: UserId,
		service: ServiceId,
		units: Shares,
		nav: Nav,
		cash: Usdt,
	},
	/// Released while queued, or its approved payout refused — void the pending burn
	/// (units returned).
	Failed {
		redemption_id: RedemptionId,
		user: UserId,
		service: ServiceId,
		units: Shares,
	},
	/// Cancelled while queued — void the pending burn (units returned).
	Cancelled {
		redemption_id: RedemptionId,
		user: UserId,
		service: ServiceId,
		units: Shares,
	},
	/// banking executed the payout the owners approved — the cash has left; post the
	/// pending burn and reduce the cost basis.
	PayoutExecuted {
		redemption_id: RedemptionId,
		user: UserId,
		service: ServiceId,
		units: Shares,
	},
}

impl DomainEvent for RedemptionEvent {
	const KIND: &'static str = "redemptions";
}

impl EmitsEvents for Redemption {
	type Event = RedemptionEvent;

	fn drain_events(&mut self) -> Vec<RedemptionEvent> {
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
	fn units(raw: &str) -> Shares {
		Shares::parse_decimal(raw).unwrap()
	}

	#[test]
	fn request_sets_queued_and_emits_requested() {
		let mut r = Redemption::request(RedemptionId::new(), user(), svc(), units("100")).unwrap();
		assert_eq!(r.state(), RedemptionState::Queued);
		let events = r.drain_events();
		assert_eq!(events.len(), 1);
		assert!(matches!(events[0], RedemptionEvent::Requested { .. }));
		assert!(r.drain_events().is_empty());
	}

	#[test]
	fn zero_units_is_rejected() {
		assert!(Redemption::request(RedemptionId::new(), user(), svc(), Shares::ZERO).is_err());
	}

	#[test]
	fn settle_prices_at_settle_time_nav_and_is_idempotent() {
		let mut r = Redemption::request(RedemptionId::new(), user(), svc(), units("100")).unwrap();
		r.drain_events();
		// 100 units at settle-NAV 1.5 → 150 cash.
		r.settle(Nav::parse_decimal("1.5").unwrap()).unwrap();
		assert_eq!(r.state(), RedemptionState::Settling);
		assert_eq!(r.cash(), Some(Usdt::parse_decimal("150").unwrap()));
		assert!(matches!(r.drain_events()[0], RedemptionEvent::Settled { .. }));
		// Idempotent: second settle is a no-op.
		r.settle(Nav::parse_decimal("2").unwrap()).unwrap();
		assert!(r.drain_events().is_empty());
		// A settling redemption can be neither cancelled nor failed (fix #8).
		assert!(r.cancel().is_err());
		assert!(r.fail().is_err());
	}

	#[test]
	fn queued_cancels_and_fails_then_locks() {
		let mut r = Redemption::request(RedemptionId::new(), user(), svc(), units("100")).unwrap();
		r.drain_events();
		r.cancel().unwrap();
		assert_eq!(r.state(), RedemptionState::Cancelled);
		assert!(matches!(r.drain_events()[0], RedemptionEvent::Cancelled { .. }));
		// Idempotent cancel; cannot then settle or fail.
		r.cancel().unwrap();
		assert!(r.drain_events().is_empty());
		assert!(r.settle(Nav::SEED).is_err());
		assert!(r.fail().is_err());
	}

	fn awaiting(r: &Redemption) -> Redemption {
		Redemption::rehydrate(r.id(), r.user(), r.service().clone(), r.units(), r.nav(), r.cash(), RedemptionState::AwaitingApproval)
	}

	#[test]
	fn an_approved_payout_settles_once_and_a_refused_one_fails_once() {
		let mut r = Redemption::request(RedemptionId::new(), user(), svc(), units("100")).unwrap();
		r.settle(Nav::SEED).unwrap();
		let mut approved = awaiting(&r);
		// Past pricing: a repeated settle changes nothing, and the holder cannot pull out.
		approved.settle(Nav::parse_decimal("2").unwrap()).unwrap();
		assert!(approved.cancel().is_err());
		assert!(approved.fail().is_err());
		approved.payout_executed().unwrap();
		assert_eq!(approved.state(), RedemptionState::Settling);
		assert!(matches!(approved.drain_events()[..], [RedemptionEvent::PayoutExecuted { .. }]));
		approved.payout_executed().unwrap();
		assert!(approved.drain_events().is_empty(), "a repeat raises nothing");
		assert!(approved.payout_refused().is_err(), "the cash has left");

		let mut refused = awaiting(&r);
		refused.payout_refused().unwrap();
		assert_eq!(refused.state(), RedemptionState::Failed);
		assert!(matches!(refused.drain_events()[..], [RedemptionEvent::Failed { .. }]));
		refused.payout_refused().unwrap();
		assert!(refused.drain_events().is_empty(), "a repeat raises nothing");
		assert!(refused.payout_executed().is_err(), "the units went back");
	}

	#[test]
	fn awaiting_approval_round_trips_as_a_state() {
		assert_eq!(RedemptionState::parse("awaiting_approval").unwrap(), RedemptionState::AwaitingApproval);
		assert_eq!(RedemptionState::AwaitingApproval.as_str(), "awaiting_approval");
	}

	#[test]
	fn event_round_trips_through_json() {
		let mut r = Redemption::request(RedemptionId::new(), user(), svc(), units("100")).unwrap();
		let event = r.drain_events().pop().unwrap();
		let json = serde_json::to_string(&event).unwrap();
		let back: RedemptionEvent = serde_json::from_str(&json).unwrap();
		assert!(matches!(back, RedemptionEvent::Requested { .. }));
	}
}
