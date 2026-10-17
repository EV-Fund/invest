//! Hold reaper — gives back the cash of a buy that never became an order.
//!
//! A buy places its hold in banking (`Reserve`, subject `book_order:<order id>`) before the
//! order row is written. If the answer to `Reserve` is lost and the user never retries with
//! the same client order id, the hold has no order, no TTL and nobody who would release it.
//! Every [`REAP_INTERVAL`] this lists the active `book_order:` holds and voids those that are
//! older than the configured age and have no order row.
//!
//! ## Why it is safe to void
//!
//! - **Age.** A placement takes milliseconds between its `Reserve` and its order row; the
//!   age floor (at least a minute, enforced at boot) is orders of magnitude above that, so a
//!   hold that is still being placed is never old enough to be taken. The age is banking's
//!   own `created_at` — a clock this process keeps would restart with every deploy and
//!   forget how long a hold had stood.
//! - **Re-check.** The order row is looked up for each hold right before its `Void`, not
//!   once per pass, so a row that landed after the list is respected.
//! - **Idempotency.** The `Void` key is `reap:<hold id>`: a repeat after a lost answer is
//!   banking's original result. A voided hold leaves the list, so the next pass does not see
//!   it again.
//!
//! A failure never stops the loop: a failed list is warned and retried next tick, a failed
//! lookup or `Void` skips that hold only, and it is picked up again on the next pass.

use std::{sync::Arc, time::Duration};

use invest_domain::book::OrderId;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::now_unix;
use crate::ports::{
	book::OrderPresence,
	money::{HoldState, MoneyKey, MoneyLedger, MoneyLedgerError, SubjectRef, VoidRequest},
};

/// How often the reaper looks. The age floor is a minute, so a tick of the same size finds
/// an orphan at most one floor late.
const REAP_INTERVAL: Duration = Duration::from_secs(60);

/// The subject prefix of a buy order's hold (`SubjectRef::new("book_order", id)`).
const SUBJECT_PREFIX: &str = "book_order:";

pub struct HoldReaper {
	money: Arc<dyn MoneyLedger>,
	orders: Arc<dyn OrderPresence>,
	/// A hold younger than this is left alone.
	after: Duration,
}

impl HoldReaper {
	pub fn new(money: Arc<dyn MoneyLedger>, orders: Arc<dyn OrderPresence>, after: Duration) -> Self {
		Self { money, orders, after }
	}

	pub async fn run(self, shutdown: CancellationToken) {
		info!("hold reaper: voiding order-less buy holds older than {:?}, every {REAP_INTERVAL:?}", self.after);
		loop {
			match self.reap(now_unix()).await {
				Ok(voided) if voided > 0 => info!(voided, "hold reaper: voided holds of buys that were never placed"),
				Ok(_) => {}
				Err(err) => warn!("hold reaper: could not list holds (will retry): {err}"),
			}
			tokio::select! {
				() = shutdown.cancelled() => return,
				() = tokio::time::sleep(REAP_INTERVAL) => {},
			}
		}
	}

	/// One pass at `now` (unix seconds): the number of holds voided. Only the list can fail
	/// the pass; a hold that cannot be checked or voided is warned and left for the next one.
	pub async fn reap(&self, now: i64) -> Result<u32, MoneyLedgerError> {
		let after = i64::try_from(self.after.as_secs()).unwrap_or(i64::MAX);
		let mut voided = 0;
		for hold in self.money.list_holds(SUBJECT_PREFIX).await? {
			let Some(order) = hold.subject_ref.strip_prefix(SUBJECT_PREFIX).and_then(|raw| Uuid::parse_str(raw).ok()) else {
				warn!(hold = %hold.id.0, subject = %hold.subject_ref, "hold reaper: a book_order hold whose subject names no order id — left alone");
				continue;
			};
			// 0 is "banking did not say": the age is unknown, and an unknown age is not old.
			if hold.state != HoldState::Active || hold.created_at <= 0 || now.saturating_sub(hold.created_at) < after {
				continue;
			}
			let order = OrderId::from_raw(order);
			match self.orders.order_exists(order).await {
				Ok(false) => {}
				Ok(true) => continue,
				Err(err) => {
					warn!(hold = %hold.id.0, %order, "hold reaper: could not look the order up (will retry): {err}");
					continue;
				}
			}
			let request = match (MoneyKey::parse(&format!("reap:{}", hold.id.0)), SubjectRef::new("book_order", order.raw())) {
				(Ok(key), Ok(subject_ref)) => VoidRequest {
					scope: hold.scope.clone(),
					hold_id: hold.id.clone(),
					key,
					subject_ref,
				},
				(key, subject) => {
					warn!(hold = %hold.id.0, %order, "hold reaper: could not name the void: {key:?} {subject:?}");
					continue;
				}
			};
			match self.money.void(&request).await {
				Ok(_) => {
					voided += 1;
					info!(hold = %hold.id.0, %order, scope = %hold.scope, "hold reaper: voided the hold of a buy that was never placed");
				}
				Err(err) => warn!(hold = %hold.id.0, %order, "hold reaper: could not void (will retry): {err}"),
			}
		}
		debug!(voided, "hold reaper: pass done");
		Ok(voided)
	}
}

#[cfg(test)]
mod tests {
	use std::{collections::HashSet, sync::Mutex};

	use async_trait::async_trait;
	use invest_domain::{error::DomainError, money::Usdt};

	use super::*;
	use crate::{
		infrastructure::money::fake::InMemoryMoneyLedger,
		ports::money::{Hold, MoneyEventKind, MoneyEventSource, Refusal, ReserveRequest, ScopeId},
	};

	const AFTER: Duration = Duration::from_secs(900);
	const SUBJECT: &str = "itest-reaped";

	/// The book as far as the reaper sees it: the ids with a row. `lands_on_lookup` makes a
	/// row appear at the first lookup of an id — a placement that commits after the list.
	#[derive(Default)]
	struct Orders {
		rows: Mutex<HashSet<OrderId>>,
		lands_on_lookup: Mutex<HashSet<OrderId>>,
		down: Mutex<bool>,
	}

	#[async_trait]
	impl OrderPresence for Orders {
		async fn order_exists(&self, id: OrderId) -> Result<bool, DomainError> {
			if *self.down.lock().unwrap() {
				return Err(DomainError::Precondition("book unavailable".into()));
			}
			let mut rows = self.rows.lock().unwrap();
			if self.lands_on_lookup.lock().unwrap().remove(&id) {
				rows.insert(id);
			}
			Ok(rows.contains(&id))
		}
	}

	struct Rig {
		money: Arc<InMemoryMoneyLedger>,
		orders: Arc<Orders>,
		reaper: HoldReaper,
	}

	fn rig() -> Rig {
		let money = Arc::new(InMemoryMoneyLedger::new());
		money.fund(&ScopeId::invest(), SUBJECT, Usdt::parse_decimal("1000").unwrap());
		let orders = Arc::new(Orders::default());
		let reaper = HoldReaper::new(money.clone(), orders.clone(), AFTER);
		Rig { money, orders, reaper }
	}

	/// A buy's hold, placed `age` seconds ago, for a fresh order id.
	async fn hold_aged(rig: &Rig, age: i64) -> (OrderId, Hold) {
		let order = OrderId::from_raw(Uuid::new_v4());
		let hold = rig
			.money
			.reserve(&ReserveRequest {
				scope: ScopeId::invest(),
				auth_subject: SUBJECT.into(),
				amount: Usdt::parse_decimal("10").unwrap(),
				ttl_seconds: None,
				key: MoneyKey::parse(&format!("book_order:{}:hold", order.raw().hyphenated())).unwrap(),
				subject_ref: SubjectRef::new("book_order", order.raw()).unwrap(),
			})
			.await
			.unwrap();
		rig.money.age_hold(&hold.id, age);
		(order, hold)
	}

	async fn state_of(rig: &Rig, hold: &Hold) -> HoldState {
		rig.money.hold(&hold.scope, &hold.id).await.unwrap().state
	}

	fn free_cash(rig: &Rig) -> Usdt {
		rig.money.compartment(&ScopeId::invest(), SUBJECT)
	}

	#[tokio::test]
	async fn an_orphan_older_than_the_age_is_voided_and_its_cash_comes_back() {
		let rig = rig();
		let (_, hold) = hold_aged(&rig, 901).await;
		assert_eq!(free_cash(&rig), Usdt::parse_decimal("990").unwrap());

		assert_eq!(rig.reaper.reap(now_unix()).await.unwrap(), 1);

		assert_eq!(state_of(&rig, &hold).await, HoldState::Voided);
		assert_eq!(free_cash(&rig), Usdt::parse_decimal("1000").unwrap());
	}

	#[tokio::test]
	async fn an_orphan_younger_than_the_age_is_kept() {
		let rig = rig();
		let (_, hold) = hold_aged(&rig, 899).await;

		assert_eq!(rig.reaper.reap(now_unix()).await.unwrap(), 0);

		assert_eq!(state_of(&rig, &hold).await, HoldState::Active);
	}

	#[tokio::test]
	async fn a_hold_with_an_order_row_is_kept_however_old() {
		let rig = rig();
		let (order, hold) = hold_aged(&rig, 100_000).await;
		rig.orders.rows.lock().unwrap().insert(order);

		assert_eq!(rig.reaper.reap(now_unix()).await.unwrap(), 0);

		assert_eq!(state_of(&rig, &hold).await, HoldState::Active);
	}

	#[tokio::test]
	async fn an_order_that_lands_after_the_list_is_respected() {
		let rig = rig();
		let (order, hold) = hold_aged(&rig, 100_000).await;
		rig.orders.lands_on_lookup.lock().unwrap().insert(order);

		assert_eq!(rig.reaper.reap(now_unix()).await.unwrap(), 0);

		assert_eq!(state_of(&rig, &hold).await, HoldState::Active);
	}

	#[tokio::test]
	async fn a_hold_of_another_subject_is_never_touched() {
		let rig = rig();
		let hold = rig
			.money
			.reserve(&ReserveRequest {
				scope: ScopeId::invest(),
				auth_subject: SUBJECT.into(),
				amount: Usdt::parse_decimal("10").unwrap(),
				ttl_seconds: None,
				key: MoneyKey::parse("subscription:x:hold").unwrap(),
				subject_ref: SubjectRef::new("subscription", Uuid::new_v4()).unwrap(),
			})
			.await
			.unwrap();
		rig.money.age_hold(&hold.id, 100_000);

		assert_eq!(rig.reaper.reap(now_unix()).await.unwrap(), 0);

		assert_eq!(state_of(&rig, &hold).await, HoldState::Active);
	}

	#[tokio::test]
	async fn a_failing_ledger_stops_neither_the_pass_nor_the_next_one() {
		let rig = rig();
		let (_, first) = hold_aged(&rig, 1_000).await;
		let (_, second) = hold_aged(&rig, 1_000).await;

		// The list down: the pass fails as a whole and touches nothing.
		rig.money.outage("ListHolds", 1);
		assert!(matches!(rig.reaper.reap(now_unix()).await, Err(MoneyLedgerError::Unavailable(_))));
		assert_eq!(state_of(&rig, &first).await, HoldState::Active);

		// The first Void refused: the other hold is still voided, and the first is retried
		// by the next pass.
		rig.money.outage("Void", 1);
		assert_eq!(rig.reaper.reap(now_unix()).await.unwrap(), 1);
		let states = [state_of(&rig, &first).await, state_of(&rig, &second).await];
		assert_eq!(states.iter().filter(|s| **s == HoldState::Voided).count(), 1, "{states:?}");

		assert_eq!(rig.reaper.reap(now_unix()).await.unwrap(), 1);
		assert_eq!(state_of(&rig, &first).await, HoldState::Voided);
		assert_eq!(state_of(&rig, &second).await, HoldState::Voided);
	}

	#[tokio::test]
	async fn a_refused_void_or_an_unreadable_book_is_retried_not_fatal() {
		let rig = rig();
		let (_, hold) = hold_aged(&rig, 1_000).await;

		*rig.orders.down.lock().unwrap() = true;
		assert_eq!(rig.reaper.reap(now_unix()).await.unwrap(), 0);
		assert_eq!(state_of(&rig, &hold).await, HoldState::Active);
		*rig.orders.down.lock().unwrap() = false;

		rig.money.refuse("Void", Refusal::HoldNotActive);
		assert_eq!(rig.reaper.reap(now_unix()).await.unwrap(), 0);
		assert_eq!(state_of(&rig, &hold).await, HoldState::Active);
		rig.money.serve("Void");

		assert_eq!(rig.reaper.reap(now_unix()).await.unwrap(), 1);
		assert_eq!(state_of(&rig, &hold).await, HoldState::Voided);
	}

	#[tokio::test]
	async fn a_repeat_pass_voids_nothing_twice() {
		let rig = rig();
		let (_, hold) = hold_aged(&rig, 1_000).await;

		assert_eq!(rig.reaper.reap(now_unix()).await.unwrap(), 1);
		assert_eq!(rig.reaper.reap(now_unix()).await.unwrap(), 0);

		let page = rig.money.pull(0, 100).await.unwrap();
		let voids = page.events.iter().filter(|event| event.kind == MoneyEventKind::HoldVoided && event.ref_id == hold.id.0).count();
		assert_eq!(voids, 1);
		assert_eq!(free_cash(&rig), Usdt::parse_decimal("1000").unwrap());
	}
}
