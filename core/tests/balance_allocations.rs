//! Integration tests for the unit ledger and the price — real Postgres **and** TigerBeetle
//! (no mocks of either). They run when `DATABASE_URL` is set and a TigerBeetle replica is
//! reachable (e.g. `nix run .#db` + `.#tb`), skip otherwise, and fail instead of skipping
//! under `CI`. Each test uses fresh random ids (user/service), so runs are isolated on
//! shared infrastructure; every test holds `common::outbox_serial()` for its whole life
//! and drains through `common::drain_to_quiescence` (see there for why).
//!
//! Moved from banking 9ffafce3 (`piggybank/core/tests/balance_allocations.rs`), cut to
//! the Share ledger and NAV: the deposit, withdrawal, subscription and redemption tests
//! of that suite drive cash, which is banking's — they come back with the money sagas
//! (PR 1b), against the money port.

use std::sync::Arc;

use invest_core::{
	application::{funds as funds_app, issuance as issuance_app},
	infrastructure::{allocations::PgAllocations, issuance::PgUnitIssuances, money::fake::InMemoryMoneyLedger, nav::PgNav, positions::PgFundPositions, relay::Relay, users::PgUsers},
	ports::{
		AllocationRegistry,
		ledger::{Ledger, LedgerError, LedgerTransfer},
		nav::NavMarks,
	},
};
use invest_domain::{
	allocations::{Allocation, AllocationAccess, AllocationIcon, AllocationId},
	balance::{LedgerAccountKey, ServiceId, TransferCode},
	error::DomainError,
	issuance::{IdempotencyKey, UnitHolder},
	money::{Nav, Shares, Usdt},
	users::UserId,
};
use sqlx::PgPool;
use tokio::sync::{MutexGuard, Notify};
use uuid::Uuid;

mod common;

struct Harness {
	pool: PgPool,
	allocations: PgAllocations,
	users: PgUsers,
	ledger: Arc<dyn Ledger>,
	money: InMemoryMoneyLedger,
	relay: Relay,
	notify: Arc<Notify>,
	/// Held for the test's whole life — declared last so it is released after the relay.
	_serial: MutexGuard<'static, ()>,
}

async fn harness() -> Option<Harness> {
	let serial = common::outbox_serial().await;
	let pool = common::pool().await?;
	let ledger = common::reachable_ledger(&pool, "unit-ledger test").await?;
	let notify = Arc::new(Notify::new());
	Some(Harness {
		relay: Relay::new(pool.clone(), ledger.clone(), notify.clone()),
		allocations: PgAllocations::new(pool.clone()),
		users: PgUsers::new(pool.clone()),
		money: InMemoryMoneyLedger::new(),
		pool,
		ledger,
		notify,
		_serial: serial,
	})
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn shares(decimal: &str) -> Shares {
	Shares::parse_decimal(decimal).unwrap()
}

fn unique_service() -> ServiceId {
	ServiceId::parse(&format!("svc-{}", Uuid::new_v4())).unwrap()
}

/// A unique service that is REGISTERED, OPEN and at `invest` access in the registry — the
/// state a product dealing with anyone is in. Tests that only poke the ledger directly use
/// the bare [`unique_service`].
async fn registered_service(h: &Harness) -> ServiceId {
	let service = unique_service();
	let mut allocation = Allocation::register(AllocationId::new(), service.clone(), "itest fund", "", AllocationIcon::default()).unwrap();
	h.allocations.register(&mut allocation).await.unwrap();
	h.allocations.open(&service).await.unwrap();
	h.allocations.set_access(&service, AllocationAccess::Invest).await.unwrap();
	service
}

async fn units(h: &Harness, key: &LedgerAccountKey) -> Shares {
	Shares::from_base_units(h.ledger.balance(key).await.unwrap().posted)
}

fn now_unix() -> i64 {
	use std::time::{SystemTime, UNIX_EPOCH};
	SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

#[tokio::test]
async fn transfer_id_is_idempotent_no_double_move() {
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	let mint = LedgerTransfer {
		id: Uuid::new_v4().as_u128(),
		debit: LedgerAccountKey::UserShares(service.clone(), user),
		credit: LedgerAccountKey::SharesOutstanding(service.clone()),
		amount: shares("30").base_units(),
		code: TransferCode::UnitIssue,
		reference: 0,
	};
	// Same deterministic id twice ⇒ the second is `Exists` ⇒ applied exactly once.
	h.ledger.post(&mint).await.unwrap();
	h.ledger.post(&mint).await.unwrap();

	assert_eq!(units(&h, &LedgerAccountKey::UserShares(service.clone(), user)).await, shares("30"), "no double move");
	assert_eq!(units(&h, &LedgerAccountKey::SharesOutstanding(service)).await, shares("30"));
}

#[tokio::test]
async fn share_ledger_mints_burns_and_rejects_over_redeem() {
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	let user_shares = LedgerAccountKey::UserShares(service.clone(), user);
	let outstanding = LedgerAccountKey::SharesOutstanding(service.clone());

	// Mint 100 units on the Share ledger: Dr UserShares / Cr SharesOutstanding.
	let mint = LedgerTransfer {
		id: Uuid::new_v4().as_u128(),
		debit: user_shares.clone(),
		credit: outstanding.clone(),
		amount: shares("100").base_units(),
		code: TransferCode::ShareMint,
		reference: 0,
	};
	h.ledger.post(&mint).await.unwrap();
	// Per-service invariant: SharesOutstanding == sum(UserShares). This (user, service) is
	// fresh, so both equal the minted amount exactly.
	assert_eq!(units(&h, &user_shares).await, shares("100"), "the user holds the minted units");
	assert_eq!(units(&h, &outstanding).await, shares("100"), "supply equals the user's holding");

	// Over-redeem: a *pending* burn of 150 (Dr SharesOutstanding / Cr UserShares) exceeds
	// the 100 minted — TigerBeetle rejects it atomically via the non-negative flag, even as
	// a reservation. The TB flag, not a row-lock, is the money backstop.
	let over_burn = LedgerTransfer {
		id: Uuid::new_v4().as_u128(),
		debit: outstanding.clone(),
		credit: user_shares.clone(),
		amount: shares("150").base_units(),
		code: TransferCode::ShareBurn,
		reference: 0,
	};
	let err = h.ledger.reserve(&over_burn).await.unwrap_err();
	assert!(matches!(err, LedgerError::InsufficientFunds), "TB rejects the over-redeem reserve, got {err:?}");

	// A burn within the holding posts and reduces both sides equally — the invariant holds.
	let burn = LedgerTransfer {
		id: Uuid::new_v4().as_u128(),
		debit: outstanding.clone(),
		credit: user_shares.clone(),
		amount: shares("40").base_units(),
		code: TransferCode::ShareBurn,
		reference: 0,
	};
	h.ledger.post(&burn).await.unwrap();
	assert_eq!(units(&h, &user_shares).await, shares("60"), "the user's units dropped by the burn");
	assert_eq!(units(&h, &outstanding).await, shares("60"), "supply dropped in lockstep");
}

#[tokio::test]
async fn fund_valuation_derives_nav_and_guards_fat_finger() {
	let Some(h) = harness().await else { return };
	let nav_repo = PgNav::new(h.pool.clone());
	let user = UserId::new();
	let service = registered_service(&h).await;

	// No mark yet → the current NAV is the bootstrap seed (1.0).
	assert_eq!(nav_repo.current(&service).await.unwrap().map(|v| v.nav).unwrap_or(Nav::SEED), Nav::SEED);
	// Posting AUM with zero units outstanding is rejected — NAV is undefined.
	assert!(
		funds_app::post_fund_valuation(&h.allocations, &nav_repo, h.ledger.as_ref(), service.clone(), usdt("100"), "op", now_unix())
			.await
			.is_err()
	);

	// Mint 100 units so SharesOutstanding(service) = 100, then NAV can be derived.
	let mint = LedgerTransfer {
		id: Uuid::new_v4().as_u128(),
		debit: LedgerAccountKey::UserShares(service.clone(), user),
		credit: LedgerAccountKey::SharesOutstanding(service.clone()),
		amount: shares("100").base_units(),
		code: TransferCode::ShareMint,
		reference: 0,
	};
	h.ledger.post(&mint).await.unwrap();

	// AUM 150 over 100 units → NAV 1.5, and it becomes the current price.
	let v = funds_app::post_fund_valuation(&h.allocations, &nav_repo, h.ledger.as_ref(), service.clone(), usdt("150"), "op", now_unix())
		.await
		.unwrap();
	assert_eq!(v.nav, Nav::parse_decimal("1.5").unwrap());
	assert_eq!(nav_repo.current(&service).await.unwrap().map(|v| v.nav).unwrap_or(Nav::SEED), Nav::parse_decimal("1.5").unwrap());

	// A 10x fat-finger (AUM 1500 → NAV 15, +900%) is rejected, and the refusal names the
	// only way past the guard. There is no flag on this path that lifts it (banking#232).
	let err = funds_app::post_fund_valuation(&h.allocations, &nav_repo, h.ledger.as_ref(), service.clone(), usdt("1500"), "op", now_unix())
		.await
		.unwrap_err();
	assert!(matches!(&err, DomainError::Validation(reason) if reason.contains("for the owners to approve")), "got {err:?}");
	assert_eq!(
		nav_repo.current(&service).await.unwrap().unwrap().nav,
		Nav::parse_decimal("1.5").unwrap(),
		"the refused mark was not recorded"
	);
}

/// The cap is measured against the rolling window's anchor as well as the previous mark,
/// so it cannot be walked past in steps that are each inside it.
#[tokio::test]
async fn the_nav_move_cap_cannot_be_stepped_past_inside_the_window() {
	let Some(h) = harness().await else { return };
	let nav_repo = PgNav::new(h.pool.clone());
	let service = registered_service(&h).await;
	let now = now_unix();
	h.ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::UserShares(service.clone(), UserId::new()),
			credit: LedgerAccountKey::SharesOutstanding(service.clone()),
			amount: shares("100").base_units(),
			code: TransferCode::ShareMint,
			reference: 0,
		})
		.await
		.unwrap();
	let post = |aum: &'static str, at: i64| funds_app::post_fund_valuation(&h.allocations, &nav_repo, h.ledger.as_ref(), service.clone(), usdt(aum), "op", at);

	// The first mark anchors the window: NAV 1.0.
	post("100", now).await.unwrap();
	// +45% against both the previous mark and the anchor: fine.
	assert_eq!(post("145", now).await.unwrap().nav, Nav::parse_decimal("1.45").unwrap());
	// Another +45% is inside the cap against the previous mark (1.45 → 2.10, +44.8%) and
	// was the whole of the stepping attack; against the anchor it is +110%, and refused.
	let err = post("210", now).await.unwrap_err();
	assert!(matches!(&err, DomainError::Validation(reason) if reason.contains("anchoring the rolling window")), "got {err:?}");
	// A small move that stays inside the cap against BOTH still lands: 1.48 is +2% on the
	// previous mark and +48% on the anchor.
	assert_eq!(post("148", now).await.unwrap().nav, Nav::parse_decimal("1.48").unwrap());
	assert_eq!(nav_repo.current(&service).await.unwrap().unwrap().nav, Nav::parse_decimal("1.48").unwrap());

	// The window slides. Once every mark so far is older than the window, the anchor is
	// the newest of them (1.48), and the same +42% that was refused above now passes.
	sqlx::query("UPDATE fund_valuations SET posted_at = posted_at - interval '8 days' WHERE service = $1")
		.bind(service.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	let anchor = nav_repo.anchor(&service, now - funds_app::NAV_MOVE_WINDOW_SECS).await.unwrap().unwrap();
	assert_eq!(anchor.nav, Nav::parse_decimal("1.48").unwrap(), "the newest mark outside the window anchors it");
	assert_eq!(post("210", now).await.unwrap().nav, Nav::parse_decimal("2.1").unwrap());
	// And a fund younger than the window is anchored by its first mark, never by nothing:
	// the earliest mark is the fallback, so a fresh fund cannot be stepped either.
	let young = registered_service(&h).await;
	assert!(nav_repo.anchor(&young, now).await.unwrap().is_none(), "never marked: no anchor and no previous mark either");
}

/// The performance chart's two series: the marks in the window, oldest first, and the
/// holder's participation walked back from the live holding — with the window's start
/// priced off the mark BEFORE it and a past `to` valued as of then, not as of now.
#[tokio::test]
async fn fund_nav_history_lists_the_window_and_values_the_holder_through_it() {
	let Some(h) = harness().await else { return };
	let nav_repo = PgNav::new(h.pool.clone());
	let positions = PgFundPositions::new(h.pool.clone());
	let issuances = PgUnitIssuances::new(h.pool.clone());
	let fund_ports = funds_app::FundPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		money: &h.money,
		nav: &nav_repo,
		relay: &h.notify,
	};
	let user = common::active_user(&h.users).await;
	let service = registered_service(&h).await;
	let history = |from: i64, to: i64, now: i64| funds_app::fund_nav_history(&fund_ports, &positions, service.clone(), user, false, from, to, now);

	// Never marked, never held: no marks and no participation — and not an error.
	let blank = history(0, 0, now_unix()).await.unwrap();
	assert!(
		blank.marks.is_empty() && blank.participation.is_empty() && !blank.truncated,
		"seed-only fund answers empty series"
	);

	// Day −4: 200 units issued at the seed NAV (banking drove this with a subscription; the
	// flow the chart reads is the same — a recorded arrival of units). Day −3: marked to 1.5.
	// Now: marked to 2.
	let day = 24 * 60 * 60;
	issuance_app::issue_units(
		&fund_ports,
		&issuances,
		&h.users,
		issuance_app::IssueUnitsRequest {
			service: service.clone(),
			holder: UnitHolder::User(user),
			units: shares("200"),
			cost_basis: None,
			idempotency_key: IdempotencyKey::parse("history").unwrap(),
		},
		now_unix(),
	)
	.await
	.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let sub_at = now_unix() - 4 * day;
	sqlx::query("UPDATE unit_issuances SET applied_at = to_timestamp($2) WHERE holder_id = $1")
		.bind(user.raw())
		.bind(sub_at as f64)
		.execute(&h.pool)
		.await
		.unwrap();
	funds_app::post_fund_valuation(&h.allocations, &nav_repo, h.ledger.as_ref(), service.clone(), usdt("300"), "op", now_unix())
		.await
		.unwrap();
	let mark1_at = now_unix() - 3 * day;
	sqlx::query("UPDATE fund_valuations SET posted_at = to_timestamp($3) WHERE service = $1 AND nav = $2")
		.bind(service.as_str())
		.bind(Nav::parse_decimal("1.5").unwrap().base_units().to_string())
		.bind(mark1_at as f64)
		.execute(&h.pool)
		.await
		.unwrap();
	let mark2 = funds_app::post_fund_valuation(&h.allocations, &nav_repo, h.ledger.as_ref(), service.clone(), usdt("400"), "op", now_unix())
		.await
		.unwrap();
	let now = now_unix();
	assert!(mark2.posted_at_unix <= now);

	// All-time: both marks, oldest first; the holder's line starts at their subscription.
	let all = history(0, 0, now).await.unwrap();
	assert_eq!(
		all.marks.iter().map(|m| (m.posted_at_unix, m.nav)).collect::<Vec<_>>(),
		vec![(mark1_at, Nav::parse_decimal("1.5").unwrap()), (mark2.posted_at_unix, mark2.nav)]
	);
	assert_eq!(all.marks[1].aum, usdt("400"));
	assert!(!all.truncated);
	let series: Vec<(i64, Usdt)> = all.participation.iter().map(|p| (p.at_unix, p.value)).collect();
	assert_eq!(series[0], (sub_at, usdt("200")), "200 units at the seed NAV when they arrived");
	assert_eq!(series[1], (mark1_at, usdt("300")), "revalued at the first mark");
	assert_eq!(series.last().copied(), Some((now, usdt("400"))), "ends now, at the live holding × the current mark");
	assert!(series.iter().filter(|(at, _)| *at >= mark2.posted_at_unix).all(|(_, v)| *v == usdt("400")));

	// A window opening on day −2: only the second mark, and the line opens at `from`
	// priced off the mark before the window (1.5), not the seed.
	let recent = history(now - 2 * day, 0, now).await.unwrap();
	assert_eq!(recent.marks.len(), 1);
	assert_eq!(recent.marks[0].nav, mark2.nav);
	assert_eq!(recent.participation.first().map(|p| (p.at_unix, p.value)), Some((now - 2 * day, usdt("300"))));
	assert_eq!(recent.participation.last().map(|p| p.value), Some(usdt("400")));

	// A window that ends yesterday: the first mark only, and the last point is AS OF then.
	let past = history(0, now - day, now).await.unwrap();
	assert_eq!(past.marks.iter().map(|m| m.nav).collect::<Vec<_>>(), vec![Nav::parse_decimal("1.5").unwrap()]);
	assert_eq!(past.participation.last().map(|p| (p.at_unix, p.value)), Some((now - day, usdt("300"))));

	// An inverted window is a client error, not an empty answer.
	assert!(matches!(history(now, now - day, now).await.unwrap_err(), DomainError::Validation(_)));

	// A stranger sees the marks but has no line.
	let stranger = funds_app::fund_nav_history(&fund_ports, &positions, service.clone(), UserId::new(), false, 0, 0, now)
		.await
		.unwrap();
	assert_eq!(stranger.marks.len(), 2);
	assert!(stranger.participation.is_empty());
}
