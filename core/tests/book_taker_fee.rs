//! Integration tests for the taker-fee rise on an allocation book (#11) — real Postgres
//! (the unit ledger is not touched, so no TigerBeetle replica is needed). They run when `DATABASE_URL` is set, skip otherwise, and fail instead of
//! skipping under `CI`.
//!
//! What is pinned is the LIFE of a rise: it is scheduled and the fee in force does not move;
//! it binds only once its notice period is over, on the sweeper's promotion; every holder is
//! owed a notice when it is scheduled and the requester has to acknowledge them; a cut binds
//! at once, tells nobody and withdraws a rise on its way; one rise at a time; a step has a
//! ceiling; and a pending rise is a row, so a restart does not lose it. Time is a parameter
//! of every call, so the notice period is driven, not waited for.

mod common;

use invest_core::{
	application::{allocations as allocations_app, book as book_app},
	infrastructure::{allocations::PgAllocations, book::PgBook, money::fake::InMemoryMoneyLedger},
	ports::{
		AllocationRegistry,
		book::{BookPolicyRecord, BookStore, TakerFeeChange},
		money::ScopeId,
	},
};
use invest_domain::{
	allocations::{AllocationAccess, AllocationIcon},
	balance::ServiceId,
	book::{BookPolicy, TakerFeeRiseTerms},
	error::DomainError,
};
use sqlx::PgPool;
use uuid::Uuid;

const DELAY: i64 = 3_600;
const CEILING: u32 = 25;
const T0: i64 = 1_800_000_000;

/// Postgres only: a rise never touches the unit ledger, so these tests do not need a
/// TigerBeetle replica (the suites that move units do, and skip without one).
struct Harness {
	pool: PgPool,
	allocations: PgAllocations,
	money: InMemoryMoneyLedger,
	book: PgBook,
	terms: TakerFeeRiseTerms,
}

async fn harness() -> Option<Harness> {
	let pool = common::pool().await?;
	Some(Harness {
		allocations: PgAllocations::new(pool.clone()),
		money: InMemoryMoneyLedger::new(),
		book: PgBook::new(pool.clone()),
		terms: TakerFeeRiseTerms::new(DELAY, CEILING).unwrap(),
		pool,
	})
}

fn policy(fee_bps: u32) -> BookPolicy {
	BookPolicy::new(true, fee_bps, BookPolicy::DEFAULT_PRICE_TICK, BookPolicy::DEFAULT_LOT_SIZE, 500, true).unwrap()
}

/// A registered product whose book is open at `fee_bps`, seeded through the seeding path
/// (`set_policy`), which is not paced.
async fn product_at(h: &Harness, fee_bps: u32) -> ServiceId {
	let service = ServiceId::parse(&format!("tf-{}", Uuid::new_v4())).unwrap();
	allocations_app::register(&h.allocations, service.clone(), &ScopeId::invest(), "Service Arb", "Arbitrage", AllocationIcon::Arbitrage)
		.await
		.unwrap();
	allocations_app::open(&h.allocations, &h.money, &service).await.unwrap();
	h.allocations.set_access(&service, AllocationAccess::Invest).await.unwrap();
	book_app::set_policy(&h.allocations, &h.book, &service, policy(fee_bps)).await.unwrap();
	service
}

/// A position with units, which is all the notice roster looks at.
async fn holder(h: &Harness, service: &ServiceId) -> Uuid {
	let user = Uuid::new_v4();
	sqlx::query("INSERT INTO fund_positions (user_id, service, cost_basis, units, high_water_mark) VALUES ($1, $2, '1000000000', '1000000000000000000000', '1000000000')")
		.bind(user)
		.bind(service.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	user
}

async fn change(h: &Harness, store: &PgBook, service: &ServiceId, next: BookPolicy, now_unix: i64, acknowledged: bool) -> Result<BookPolicyRecord, DomainError> {
	let request = TakerFeeChange {
		id: Uuid::new_v4(),
		by: "operator-1",
		now_unix,
		terms: &h.terms,
		acknowledged_undelivered_notices: acknowledged,
	};
	book_app::change_policy(&h.allocations, store, service, next, &request).await
}

async fn live_fee(h: &Harness, service: &ServiceId) -> u32 {
	book_app::policy(&h.book, service).await.unwrap().policy.taker_fee_bps()
}

async fn notice_count(h: &Harness, service: &ServiceId, live_only: bool) -> i64 {
	let sql = if live_only {
		"SELECT COUNT(*) FROM book_taker_fee_notices n JOIN book_taker_fee_changes c ON c.id = n.change_id WHERE c.service = $1 AND n.withdrawn_at IS NULL"
	} else {
		"SELECT COUNT(*) FROM book_taker_fee_notices n JOIN book_taker_fee_changes c ON c.id = n.change_id WHERE c.service = $1"
	};
	sqlx::query_scalar(sql).bind(service.as_str()).fetch_one(&h.pool).await.unwrap()
}

async fn change_state(h: &Harness, id: Uuid) -> String {
	sqlx::query_scalar("SELECT state FROM book_taker_fee_changes WHERE id = $1")
		.bind(id)
		.fetch_one(&h.pool)
		.await
		.unwrap()
}

#[tokio::test]
async fn a_rise_is_scheduled_and_the_fee_in_force_does_not_move() {
	let Some(h) = harness().await else { return };
	let service = product_at(&h, 10).await;
	holder(&h, &service).await;

	let record = change(&h, &h.book, &service, policy(30), T0, true).await.unwrap();
	assert_eq!(record.policy.taker_fee_bps(), 10, "the fee in force is the old one");
	let pending = record.pending_rise.expect("the rise is on its way");
	assert_eq!((pending.taker_fee_bps, pending.effective_from_unix), (30, T0 + DELAY));
	assert_eq!(pending.acknowledged_by.as_deref(), Some("operator-1"));
	assert_eq!(live_fee(&h, &service).await, 10);
	let read = book_app::policy(&h.book, &service).await.unwrap();
	assert_eq!(read.pending_rise, Some(pending.clone()), "the policy read shows the rise to traders");

	// Not due until the notice is over, to the second.
	assert!(!h.book.promote_taker_fee_rise(pending.id, T0 + DELAY - 1).await.unwrap());
	assert_eq!(live_fee(&h, &service).await, 10);
}

#[tokio::test]
async fn a_rise_binds_after_the_delay_on_the_sweepers_promotion() {
	let Some(h) = harness().await else { return };
	let service = product_at(&h, 10).await;
	holder(&h, &service).await;
	let pending = change(&h, &h.book, &service, policy(30), T0, true).await.unwrap().pending_rise.unwrap();

	assert!(h.book.due_taker_fee_rises(T0 + DELAY - 1).await.unwrap().iter().all(|id| *id != pending.id));
	assert!(h.book.due_taker_fee_rises(T0 + DELAY).await.unwrap().contains(&pending.id));
	let mut failures = std::collections::HashMap::new();
	book_app::promote_due_rises(&h.book, T0 + DELAY, &mut failures).await.unwrap();

	assert_eq!(live_fee(&h, &service).await, 30);
	assert_eq!(book_app::policy(&h.book, &service).await.unwrap().pending_rise, None);
	assert_eq!(change_state(&h, pending.id).await, "applied");
	assert!(!h.book.promote_taker_fee_rise(pending.id, T0 + DELAY).await.unwrap(), "a bound rise is not bound twice");
}

#[tokio::test]
async fn a_cut_binds_at_once_and_tells_nobody() {
	let Some(h) = harness().await else { return };
	let service = product_at(&h, 30).await;
	holder(&h, &service).await;

	let record = change(&h, &h.book, &service, policy(5), T0, false).await.unwrap();
	assert_eq!(record.policy.taker_fee_bps(), 5);
	assert_eq!(record.pending_rise, None);
	assert_eq!(notice_count(&h, &service, false).await, 0, "a cut owes the holders no notice");
}

#[tokio::test]
async fn a_cut_withdraws_the_rise_on_its_way_and_its_notices() {
	let Some(h) = harness().await else { return };
	let service = product_at(&h, 10).await;
	holder(&h, &service).await;
	let pending = change(&h, &h.book, &service, policy(30), T0, true).await.unwrap().pending_rise.unwrap();
	assert_eq!(notice_count(&h, &service, true).await, 1);

	let record = change(&h, &h.book, &service, policy(5), T0 + 1, false).await.unwrap();
	assert_eq!((record.policy.taker_fee_bps(), record.pending_rise), (5, None));
	assert_eq!(change_state(&h, pending.id).await, "cancelled");
	assert_eq!(notice_count(&h, &service, true).await, 0, "no notifier may tell holders about a fee that will never bind");
	assert!(!h.book.promote_taker_fee_rise(pending.id, T0 + DELAY).await.unwrap());
	assert_eq!(live_fee(&h, &service).await, 5);
}

#[tokio::test]
async fn a_rise_above_the_per_rise_ceiling_is_refused_and_writes_nothing() {
	let Some(h) = harness().await else { return };
	let service = product_at(&h, 10).await;

	let err = change(&h, &h.book, &service, policy(10 + CEILING + 1), T0, true).await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "{err:?}");
	assert_eq!(book_app::policy(&h.book, &service).await.unwrap().pending_rise, None);
	assert_eq!(live_fee(&h, &service).await, 10);
	// The step itself is allowed at the ceiling.
	assert!(change(&h, &h.book, &service, policy(10 + CEILING), T0, true).await.unwrap().pending_rise.is_some());
}

#[tokio::test]
async fn holders_get_a_stored_notice_on_a_rise() {
	let Some(h) = harness().await else { return };
	let service = product_at(&h, 10).await;
	let first = holder(&h, &service).await;
	let second = holder(&h, &service).await;

	change(&h, &h.book, &service, policy(20), T0, true).await.unwrap();
	let users: Vec<Uuid> = sqlx::query_scalar(
		"SELECT n.user_id FROM book_taker_fee_notices n JOIN book_taker_fee_changes c ON c.id = n.change_id WHERE c.service = $1 AND n.given_up_at IS NOT NULL ORDER BY n.user_id",
	)
	.bind(service.as_str())
	.fetch_all(&h.pool)
	.await
	.unwrap();
	let mut expected = vec![first, second];
	expected.sort();
	assert_eq!(users, expected);
}

#[tokio::test]
async fn the_requester_must_acknowledge_the_notices_that_cannot_be_delivered() {
	let Some(h) = harness().await else { return };
	let service = product_at(&h, 10).await;
	holder(&h, &service).await;

	let open_with_tick = BookPolicy::new(true, 30, BookPolicy::DEFAULT_PRICE_TICK, BookPolicy::DEFAULT_LOT_SIZE, 700, true).unwrap();
	let err = change(&h, &h.book, &service, open_with_tick, T0, false).await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(_)), "{err:?}");
	let stored = book_app::policy(&h.book, &service).await.unwrap();
	assert_eq!(
		(stored.pending_rise, stored.policy.market_slippage_bps()),
		(None, 500),
		"a refused request writes nothing, the other terms included"
	);

	// Nobody owed a notice, nothing to acknowledge.
	let empty = product_at(&h, 10).await;
	let record = change(&h, &h.book, &empty, policy(20), T0, false).await.unwrap();
	assert_eq!(record.pending_rise.unwrap().acknowledged_by, None);
}

#[tokio::test]
async fn the_other_terms_bind_at_once_beside_a_rise() {
	let Some(h) = harness().await else { return };
	let service = product_at(&h, 10).await;
	let wider = BookPolicy::new(true, 20, BookPolicy::DEFAULT_PRICE_TICK, BookPolicy::DEFAULT_LOT_SIZE, 900, true).unwrap();

	let record = change(&h, &h.book, &service, wider, T0, true).await.unwrap();
	assert_eq!((record.policy.market_slippage_bps(), record.policy.taker_fee_bps()), (900, 10));
	assert_eq!(record.pending_rise.unwrap().taker_fee_bps, 20);
}

#[tokio::test]
async fn one_rise_at_a_time_and_a_cancel_clears_the_way() {
	let Some(h) = harness().await else { return };
	let service = product_at(&h, 10).await;
	holder(&h, &service).await;
	let first = change(&h, &h.book, &service, policy(20), T0, true).await.unwrap().pending_rise.unwrap();

	let err = change(&h, &h.book, &service, policy(30), T0, true).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "{err:?}");
	// Restating the fee in force is not a rise: the pending one stands.
	let restated = change(&h, &h.book, &service, policy(10), T0, false).await.unwrap();
	assert_eq!(restated.pending_rise.map(|rise| rise.id), Some(first.id));

	let cancelled = book_app::cancel_taker_fee_rise(&h.allocations, &h.book, &service, "operator-2").await.unwrap();
	assert_eq!((cancelled.pending_rise, cancelled.policy.taker_fee_bps()), (None, 10));
	assert_eq!(change_state(&h, first.id).await, "cancelled");
	assert_eq!(notice_count(&h, &service, true).await, 0, "the undelivered notices go with the rise");
	let again = book_app::cancel_taker_fee_rise(&h.allocations, &h.book, &service, "operator-2").await.unwrap();
	assert_eq!(again.pending_rise, None, "cancelling twice is not an error");
	assert!(!h.book.promote_taker_fee_rise(first.id, T0 + DELAY).await.unwrap());

	assert!(change(&h, &h.book, &service, policy(30), T0, true).await.unwrap().pending_rise.is_some());
}

#[tokio::test]
async fn a_pending_rise_survives_a_restart() {
	let Some(h) = harness().await else { return };
	let service = product_at(&h, 10).await;
	holder(&h, &service).await;
	let pending = change(&h, &h.book, &service, policy(30), T0, true).await.unwrap().pending_rise.unwrap();

	// A new process is a new adapter over the same database: nothing is held in memory.
	let restarted = PgBook::new(h.pool.clone());
	assert_eq!(book_app::policy(&restarted, &service).await.unwrap().pending_rise, Some(pending.clone()));
	let mut failures = std::collections::HashMap::new();
	book_app::promote_due_rises(&restarted, T0 + DELAY, &mut failures).await.unwrap();
	assert_eq!(book_app::policy(&restarted, &service).await.unwrap().policy.taker_fee_bps(), 30);
}

#[tokio::test]
async fn a_holder_the_acknowledgement_does_not_cover_holds_the_rise() {
	let Some(h) = harness().await else { return };
	let service = product_at(&h, 10).await;
	holder(&h, &service).await;
	let pending = change(&h, &h.book, &service, policy(30), T0, true).await.unwrap().pending_rise.unwrap();
	// A holder with a notice the acknowledgement was not given for.
	let late = holder(&h, &service).await;
	sqlx::query("INSERT INTO book_taker_fee_notices (change_id, user_id) VALUES ($1, $2)")
		.bind(pending.id)
		.bind(late)
		.execute(&h.pool)
		.await
		.unwrap();

	let err = h.book.promote_taker_fee_rise(pending.id, T0 + DELAY).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "{err:?}");
	assert_eq!(live_fee(&h, &service).await, 10);
	assert_eq!(change_state(&h, pending.id).await, "scheduled", "it waits, it is not lost");

	sqlx::query("UPDATE book_taker_fee_notices SET delivered_at = now() WHERE change_id = $1 AND user_id = $2")
		.bind(pending.id)
		.bind(late)
		.execute(&h.pool)
		.await
		.unwrap();
	assert!(h.book.promote_taker_fee_rise(pending.id, T0 + DELAY).await.unwrap());
	assert_eq!(live_fee(&h, &service).await, 30);
}

#[tokio::test]
async fn the_hard_cap_still_holds_whatever_the_ceiling() {
	let Some(h) = harness().await else { return };
	let service = product_at(&h, 90).await;
	assert!(BookPolicy::new(true, BookPolicy::MAX_TAKER_FEE_BPS + 1, BookPolicy::DEFAULT_PRICE_TICK, BookPolicy::DEFAULT_LOT_SIZE, 500, true).is_err());
	let record = change(&h, &h.book, &service, policy(BookPolicy::MAX_TAKER_FEE_BPS), T0, true).await.unwrap();
	assert_eq!(record.pending_rise.unwrap().taker_fee_bps, BookPolicy::MAX_TAKER_FEE_BPS);
}
