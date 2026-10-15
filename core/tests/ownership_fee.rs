//! Integration tests for the `fee` allocation as the holder of every fee (issue #245) —
//! real Postgres **and** TigerBeetle (no mocks of either); the cash behind it is banking's,
//! on `(invest, alloc:fee)`, served here by the in-memory money ledger. They run when
//! `DATABASE_URL` is set and a TigerBeetle replica is reachable (`nix run .#db` + `.#tb`),
//! skip otherwise, and fail instead of skipping under `CI`.
//!
//! What these pin down is the ownership chain as far as this release carries it: **hidden
//! is not invisible to a holder** — the catalog never lists `fee`, but the person holding
//! its units reads its title and its price like any position, the price being what `fee`
//! holds (its cash in banking, its fee classes) over its supply; and a reserved
//! allocation's price is computed, never marked.
//!
//! Every test here reads the one `fee` supply of this binary's database, so the suite runs
//! serially under the shared outbox guard, and each test brackets its own effect.
//!
//! Moved from banking 9ffafce3 (`piggybank/core/tests/ownership_fee.rs`). The fee
//! settlement, the withdrawal-fee and the redemption tests of that suite need the fee plane
//! and the money sagas (PR 1b); the treasury one stays in banking (D17).

use std::sync::Arc;

use invest_core::{
	application::{allocations as allocations_app, funds as funds_app, issuance as issuance_app},
	infrastructure::{
		allocations::PgAllocations,
		issuance::PgUnitIssuances,
		money::{Unconfigured, fake::InMemoryMoneyLedger},
		nav::PgNav,
		positions::PgFundPositions,
		relay::Relay,
		users::PgUsers,
	},
	ports::{
		ledger::Ledger,
		money::{AccountLabel, ScopeId},
		nav::NavMarks,
	},
};
use invest_domain::{
	allocations::AllocationAccess,
	balance::{LedgerAccountKey, ServiceId},
	error::DomainError,
	issuance::{IdempotencyKey, UnitHolder},
	money::{Nav, Shares, Usdt},
	users::UserId,
};
use tokio::sync::{MutexGuard, Notify};
use uuid::Uuid;

mod common;

struct Harness {
	allocations: PgAllocations,
	users: PgUsers,
	issuances: PgUnitIssuances,
	positions: PgFundPositions,
	nav: PgNav,
	ledger: Arc<dyn Ledger>,
	money: InMemoryMoneyLedger,
	relay: Relay,
	notify: Arc<Notify>,
	pool: sqlx::PgPool,
	/// Held for the test's whole life — declared last so it is released after the relay.
	_serial: MutexGuard<'static, ()>,
}

async fn harness() -> Option<Harness> {
	let serial = common::outbox_serial().await;
	let pool = common::pool().await?;
	let ledger = common::reachable_ledger(&pool, "ownership fee test").await?;
	let notify = Arc::new(Notify::new());
	Some(Harness {
		allocations: PgAllocations::new(pool.clone()),
		users: PgUsers::new(pool.clone()),
		issuances: PgUnitIssuances::new(pool.clone()),
		positions: PgFundPositions::new(pool.clone()),
		nav: PgNav::new(pool.clone()),
		money: InMemoryMoneyLedger::new(),
		relay: Relay::new(pool.clone(), ledger.clone(), notify.clone()),
		ledger,
		notify,
		pool,
		_serial: serial,
	})
}

fn fund_ports(h: &Harness) -> funds_app::FundPorts<'_> {
	funds_app::FundPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		money: &h.money,
		nav: &h.nav,
		relay: &h.notify,
	}
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn shares(decimal: &str) -> Shares {
	Shares::parse_decimal(decimal).unwrap()
}

fn now_unix() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

async fn units_of(h: &Harness, key: LedgerAccountKey) -> Shares {
	Shares::from_base_units(h.ledger.balance(&key).await.unwrap().posted)
}

async fn fee_nav(h: &Harness) -> Nav {
	funds_app::nav_of(&h.nav, h.ledger.as_ref(), &h.money, &ServiceId::fee()).await.unwrap().nav
}

/// A person concierge told invest about — an issuance names it.
async fn person(h: &Harness) -> UserId {
	common::active_user(&h.users).await
}

/// Put `amount` of cash behind the `fee` allocation, on its account in banking — what a
/// settled fee class leaves there, minus the product, the investor and the year it takes
/// to earn one. A test that only needs the allocation to be *worth something* before it
/// mints units calls this instead of restaging the whole fee chain.
async fn fund_fee_claim(h: &Harness, amount: &str) {
	h.money.credit(&ScopeId::invest(), &AccountLabel::allocation(&ServiceId::fee()).unwrap(), usdt(amount));
}

/// Mint `units` of the `fee` allocation to `holder` — through the door the data wave and
/// an approved holder grant use (the operator's `issue_units` refuses a reserved
/// allocation) — priced at the allocation's own computed NAV.
async fn grant_fee_units(h: &Harness, holder: UserId, units: &str) {
	issuance_app::grant_units(
		&fund_ports(h),
		&h.issuances,
		&h.users,
		issuance_app::IssueUnitsRequest {
			service: ServiceId::fee(),
			holder: UnitHolder::User(holder),
			units: shares(units),
			cost_basis: None,
			idempotency_key: IdempotencyKey::parse(&format!("grant-{}", Uuid::new_v4())).unwrap(),
		},
		now_unix(),
	)
	.await
	.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

/// `holder`'s position in `service` as `ListPositions` shows it.
async fn position_of(h: &Harness, holder: UserId, service: &ServiceId) -> Option<funds_app::PositionView> {
	funds_app::list_positions(&fund_ports(h), &h.positions, holder)
		.await
		.unwrap()
		.into_iter()
		.find(|position| &position.service == service)
}

/// The catalog never lists `fee`, and a stranger asking for it is told it does not exist —
/// but the person holding its units reads its title and its price like any position they
/// own, through the same reads the position screen makes.
#[tokio::test]
async fn a_holder_of_the_hidden_fee_allocation_sees_their_position_with_its_title_and_price() {
	let Some(h) = harness().await else { return };
	let (owner, stranger) = (person(&h).await, person(&h).await);
	// Value first, units second — a position priced at zero is no position to read.
	fund_fee_claim(&h, "10").await;
	grant_fee_units(&h, owner, "10").await;

	// Not in anyone's catalog, holder or not.
	for user in [owner, stranger] {
		let catalog = allocations_app::list_for(&h.allocations, user, false).await.unwrap();
		assert!(!catalog.iter().any(|record| record.allocation.service().is_reserved()), "a reserved allocation is never listed");
		let err = allocations_app::get_for(&h.allocations, &ServiceId::fee(), user, false).await.unwrap_err();
		assert!(matches!(err, DomainError::NotFound { .. }), "the plain visibility gate still hides it: {err:?}");
	}

	// The holder reads it: the position, its title, its price.
	let position = position_of(&h, owner, &ServiceId::fee()).await.expect("the fee units are a position");
	assert_eq!(position.units, shares("10"));
	assert_eq!(position.nav, fee_nav(&h).await);
	let record = funds_app::allocation_for_holder(&h.allocations, h.ledger.as_ref(), &ServiceId::fee(), owner, false)
		.await
		.expect("a holder reads the hidden allocation");
	assert_eq!(record.allocation.title(), "Fee allocation");
	assert_eq!(record.caller_access, AllocationAccess::Hidden, "with their honest access level, not a courtesy");
	let view = funds_app::fund_nav_view(&fund_ports(&h), ServiceId::fee(), owner, false, now_unix())
		.await
		.expect("a holder reads the hidden allocation's price");
	assert_eq!(view.nav, position.nav);
	assert!(!view.stale, "a computed price over cash is never stale");

	// The stranger holds nothing and reads nothing — the probe answers as an unknown slug.
	assert!(position_of(&h, stranger, &ServiceId::fee()).await.is_none());
	let err = funds_app::allocation_for_holder(&h.allocations, h.ledger.as_ref(), &ServiceId::fee(), stranger, false)
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::NotFound { .. }), "{err:?}");
	let Err(err) = funds_app::fund_nav_view(&fund_ports(&h), ServiceId::fee(), stranger, false, now_unix()).await else {
		panic!("a stranger must not read the hidden allocation's price")
	};
	assert!(matches!(err, DomainError::NotFound { .. }), "{err:?}");
}

/// A reserved allocation's price is computed, never posted — the ask is refused as
/// validation, before anything is written. (That it charges no fee of its own is pinned
/// with the fee plane, PR 1b.)
#[tokio::test]
async fn a_reserved_allocation_takes_no_mark() {
	let Some(h) = harness().await else { return };
	for reserved in [ServiceId::fee(), ServiceId::fund()] {
		let err = funds_app::post_fund_valuation(&h.allocations, &h.nav, h.ledger.as_ref(), reserved.clone(), usdt("100"), "itest", now_unix())
			.await
			.unwrap_err();
		assert!(matches!(err, DomainError::Validation(ref m) if m.contains("reserved")), "{reserved}: {err:?}");
		assert!(h.nav.current(&reserved).await.unwrap().is_none(), "{reserved}: no mark was written");
	}
	// With nothing outstanding the price is the seed NAV — the first units issued are
	// worth exactly what stands behind them, and nothing ever divides by zero.
	let fund = funds_app::nav_of(&h.nav, h.ledger.as_ref(), &h.money, &ServiceId::fund()).await.unwrap();
	if units_of(&h, LedgerAccountKey::SharesOutstanding(ServiceId::fund())).await.is_zero() {
		assert_eq!(fund.nav, Nav::SEED);
	}
	assert_eq!(fund.posted_at_unix, 0, "cash only: never stale");
	// And the cash it is priced on is banking's: with no money ledger behind it the price is
	// unavailable, never a guessed zero.
	let unconfigured = funds_app::nav_of(&h.nav, h.ledger.as_ref(), &Unconfigured, &ServiceId::fund()).await.unwrap_err();
	assert!(matches!(unconfigured, DomainError::Repository(_)), "{unconfigured:?}");
}

/// The cash half of a reserved allocation's price is banking's (plan §6.2: NAV = units in
/// invest + `GetBalances(alloc:*)` in banking): cash landing on `(invest, alloc:fee)` raises
/// what `fee` is worth by exactly that, over the same supply, with no unit moving.
#[tokio::test]
async fn the_fee_allocation_is_priced_off_its_cash_in_banking() {
	let Some(h) = harness().await else { return };
	let holder = person(&h).await;
	fund_fee_claim(&h, "10").await;
	grant_fee_units(&h, holder, "10").await;
	let outstanding = units_of(&h, LedgerAccountKey::SharesOutstanding(ServiceId::fee())).await;
	let before = funds_app::nav_of(&h.nav, h.ledger.as_ref(), &h.money, &ServiceId::fee()).await.unwrap();

	fund_fee_claim(&h, "25").await;
	let after = funds_app::nav_of(&h.nav, h.ledger.as_ref(), &h.money, &ServiceId::fee()).await.unwrap();
	let value_before = before.aum.expect("a reserved allocation's value is always a sum");
	let value_after = after.aum.expect("a reserved allocation's value is always a sum");
	assert_eq!(value_after, value_before.checked_add(usdt("25")).unwrap(), "the new cash and nothing else");
	assert_eq!(after.nav, Nav::from_aum(value_after, outstanding).unwrap(), "over the same supply");
	assert_eq!(units_of(&h, LedgerAccountKey::SharesOutstanding(ServiceId::fee())).await, outstanding, "no unit moved");
	assert!(after.nav > before.nav);
}
