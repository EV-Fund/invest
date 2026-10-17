//! Integration tests for the unit ledger and the price — real Postgres **and** TigerBeetle
//! (no mocks of either). They run when `DATABASE_URL` is set and a TigerBeetle replica is
//! reachable (e.g. `nix run .#db` + `.#tb`), skip otherwise, and fail instead of skipping
//! under `CI`. Each test uses fresh random ids (user/service), so runs are isolated on
//! shared infrastructure; every test holds `common::outbox_serial()` for its whole life
//! and drains through `common::drain_to_quiescence` (see there for why).
//!
//! Moved from banking 9ffafce3 (`piggybank/core/tests/balance_allocations.rs`). The
//! deposit and withdrawal tests stayed in banking — the cash is banking's. The subscription
//! and redemption tests drive the money sagas against the in-memory money ledger, which
//! plays banking: the user's cash is a compartment, the product's an `alloc:` account, and
//! a refusal or an outage is injected where banking would give one.

use std::{sync::Arc, time::Duration};

use evconcierge_auth::{Claims, TokenType};
use invest_contracts::invest::v1::{self as pb, funds_service_server::FundsService};
use invest_core::{
	AppState,
	application::{
		allocations as allocations_app,
		book::{BookFeed, WatchStreams},
		funds as funds_app, issuance as issuance_app,
	},
	infrastructure::{
		allocations::PgAllocations,
		book::PgBook,
		fee_policy_changes::PgFeePolicyChanges,
		fees::{PgFeeAssessments, PgFeePolicies, PgFeeSettlements, PgPositionAccruals},
		issuance::PgUnitIssuances,
		money_events::Pass,
		nav::PgNav,
		positions::PgFundPositions,
		redemptions::PgRedemptions,
		subscriptions::PgSubscriptions,
		users::PgUsers,
	},
	ports::{
		AllocationRegistry, FundPositionReader, RedemptionRepository, SubscriptionRepository,
		fees::FeePorts,
		ledger::{CompletionKind, LedgerError, LedgerTransfer, PendingCompletion},
		money::{AccountLabel, HoldState, MoneyEventKind, Refusal, ScopeId},
		nav::NavMarks,
		users::UserStanding,
	},
};
use invest_domain::{
	allocations::{Allocation, AllocationAccess, AllocationIcon, AllocationId},
	balance::{LedgerAccountKey, ServiceId, TransferCode, ValuationId},
	book::PriceTimeEngine,
	error::DomainError,
	issuance::{IdempotencyKey, UnitHolder},
	money::{Nav, Shares, Usdt},
	redemptions::{RedemptionId, RedemptionState},
	subscriptions::SubscriptionState,
	users::UserId,
};
use uuid::Uuid;

mod common;

type Harness = common::Engine;

async fn harness() -> Option<Harness> {
	common::engine("unit-ledger test").await
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
	h.allocations.register(&mut allocation, &ScopeId::invest()).await.unwrap();
	allocations_app::open(&h.allocations, h.money.as_ref(), &service).await.unwrap();
	h.allocations.set_access(&service, AllocationAccess::Invest).await.unwrap();
	service
}

/// A person known to invest and to banking, with `cash` in their compartment of `scope`.
async fn funded_user(h: &Harness, scope: &ScopeId, cash: &str) -> UserId {
	let user = common::active_user(&h.users).await;
	h.money.fund(scope, &common::auth_subject(user), usdt(cash));
	user
}

fn compartment(h: &Harness, user: UserId) -> Usdt {
	h.money.compartment(&ScopeId::invest(), &common::auth_subject(user))
}

fn alloc_cash(h: &Harness, service: &ServiceId) -> Usdt {
	h.money.account(&ScopeId::invest(), &AccountLabel::allocation(service).unwrap())
}

async fn units_available(h: &Harness, key: &LedgerAccountKey) -> Shares {
	Shares::from_base_units(h.ledger.balance(key).await.unwrap().available())
}

async fn pending_units(h: &Harness, key: &LedgerAccountKey) -> u128 {
	h.ledger.balance(key).await.unwrap().pending
}

async fn cost_basis(h: &Harness, user: UserId, service: &ServiceId) -> Option<Usdt> {
	h.positions.find(user, service).await.unwrap().map(|position| position.cost_basis)
}

async fn subscribe(h: &Harness, user: UserId, service: &ServiceId, cash: &str, now: i64) -> Result<invest_core::ports::SubscriptionRecord, DomainError> {
	funds_app::subscribe(&h.fund_ports(), &h.subscriptions, &h.users, user, service.clone(), usdt(cash), now).await
}

async fn subscription_state(h: &Harness, id: invest_domain::subscriptions::SubscriptionId) -> (SubscriptionState, Option<String>) {
	let record = h.subscriptions.find_by_id(id).await.unwrap().expect("the subscription is recorded");
	(record.subscription.state(), record.failure_reason)
}

async fn redemption_record(h: &Harness, user: UserId, id: RedemptionId) -> invest_core::ports::RedemptionRecord {
	h.redemptions
		.list_by_user(user)
		.await
		.unwrap()
		.into_iter()
		.find(|record| record.redemption.id() == id)
		.expect("the redemption is listed")
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
		money: h.money.as_ref(),
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

// ── the subscription saga ───────────────────────────────────────────────────

#[tokio::test]
async fn subscribe_holds_the_cash_captures_it_and_mints_at_nav() {
	let Some(h) = harness().await else { return };
	let service = registered_service(&h).await;
	let user = funded_user(&h, &ScopeId::invest(), "400").await;
	let user_shares = LedgerAccountKey::UserShares(service.clone(), user);
	let outstanding = LedgerAccountKey::SharesOutstanding(service.clone());
	let now = now_unix();

	// The hold is banking's answer, before the relay has done anything.
	let record = subscribe(&h, user, &service, "200", now).await.unwrap();
	assert_eq!(record.subscription.state(), SubscriptionState::Reserved);
	assert_eq!(record.scope, ScopeId::invest());
	let hold = record.hold_id.clone().expect("a reserved subscription names its hold");
	assert_eq!(compartment(&h, user), usdt("200"), "the cash is held out of the compartment");
	assert_eq!(units(&h, &user_shares).await, Shares::ZERO, "no units before the relay");

	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(subscription_state(&h, record.subscription.id()).await, (SubscriptionState::Settled, None));
	assert_eq!(units(&h, &user_shares).await, shares("200"), "200 cash at the seed NAV");
	assert_eq!(units(&h, &outstanding).await, shares("200"));
	assert_eq!(pending_units(&h, &user_shares).await, 0, "the pending mint was posted, not left hanging");
	assert_eq!(alloc_cash(&h, &service), usdt("200"), "the hold was captured into the product's account");
	let holds = h.money.holds_for(&format!("subscription:{}", record.subscription.id().raw().hyphenated()));
	assert_eq!(holds.len(), 1);
	assert_eq!((&holds[0].id, holds[0].state), (&hold, HoldState::Captured));
	assert_eq!(cost_basis(&h, user, &service).await, Some(usdt("200")));
	assert_eq!(h.positions.unit_flows(user, &service).await.unwrap().len(), 1, "the settled mint is in the history");

	// Marked to NAV 2: the next 100 buys 50 units, and the cost basis follows the cash.
	funds_app::post_fund_valuation(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), usdt("400"), "op", now_unix())
		.await
		.unwrap();
	subscribe(&h, user, &service, "100", now).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units(&h, &user_shares).await, shares("250"));
	assert_eq!(compartment(&h, user), usdt("100"));
	assert_eq!(cost_basis(&h, user, &service).await, Some(usdt("300")));

	// A stale mark refuses before banking is asked for anything.
	let stale = now + funds_app::MAX_NAV_AGE_SECS + 100;
	assert!(subscribe(&h, user, &service, "50", stale).await.is_err());
	assert_eq!(compartment(&h, user), usdt("100"), "nothing was held for a refused subscription");

	let listed = funds_app::list_subscriptions(&h.subscriptions, user).await.unwrap();
	assert_eq!(listed.len(), 2, "the stale refusal never wrote a row");
	assert!(listed.iter().all(|r| r.subscription.state() == SubscriptionState::Settled));
}

/// banking's refusal of the hold is the cash gate: the caller hears banking's reason, the
/// subscription is recorded failed with it, and nothing reaches the relay.
#[tokio::test]
async fn a_refused_hold_fails_the_subscription_with_banking_s_reason() {
	let Some(h) = harness().await else { return };
	let service = registered_service(&h).await;
	let user = funded_user(&h, &ScopeId::invest(), "50").await;

	let err = subscribe(&h, user, &service, "100", now_unix()).await.unwrap_err();
	assert!(matches!(&err, DomainError::Precondition(reason) if reason == "insufficient_funds"), "{err:?}");
	h.money.refuse("Reserve", Refusal::MandateRequired);
	let err = subscribe(&h, user, &service, "10", now_unix()).await.unwrap_err();
	assert!(matches!(&err, DomainError::Precondition(reason) if reason == "mandate_required"), "{err:?}");

	let listed = funds_app::list_subscriptions(&h.subscriptions, user).await.unwrap();
	let reasons: Vec<_> = listed.iter().map(|r| (r.subscription.state(), r.failure_reason.clone())).collect();
	assert_eq!(
		reasons,
		vec![
			(SubscriptionState::Failed, Some("mandate_required".into())),
			(SubscriptionState::Failed, Some("insufficient_funds".into())),
		],
		"newest first, each with banking's reason"
	);
	let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox WHERE aggregate_id = ANY($1)")
		.bind(listed.iter().map(|r| r.subscription.id().raw()).collect::<Vec<_>>())
		.fetch_one(&h.pool)
		.await
		.unwrap();
	assert_eq!(queued, 0, "a subscription banking refused never reaches the relay");
	assert_eq!(compartment(&h, user), usdt("50"));
}

/// The saga's compensation: banking refuses the capture after the units went pending —
/// the hold is voided (the cash goes back), the pending mint is voided (no units), and
/// the subscription fails with the reason. No cost basis is left behind.
#[tokio::test]
async fn a_refused_capture_voids_the_hold_and_the_pending_units() {
	let Some(h) = harness().await else { return };
	let service = registered_service(&h).await;
	let user = funded_user(&h, &ScopeId::invest(), "100").await;
	let user_shares = LedgerAccountKey::UserShares(service.clone(), user);

	let record = subscribe(&h, user, &service, "100", now_unix()).await.unwrap();
	assert_eq!(compartment(&h, user), Usdt::ZERO);
	h.money.refuse("Capture", Refusal::ScopeFrozen);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	h.money.serve("Capture");

	let (state, reason) = subscription_state(&h, record.subscription.id()).await;
	assert_eq!((state, reason.as_deref()), (SubscriptionState::Failed, Some("scope_frozen")));
	assert_eq!(compartment(&h, user), usdt("100"), "the hold was voided — the cash is back");
	assert_eq!(alloc_cash(&h, &service), Usdt::ZERO);
	assert_eq!(units(&h, &user_shares).await, Shares::ZERO, "no units posted");
	assert_eq!(pending_units(&h, &user_shares).await, 0, "and none left pending");
	assert_eq!(units(&h, &LedgerAccountKey::SharesOutstanding(service.clone())).await, Shares::ZERO);
	assert!(cost_basis(&h, user, &service).await.is_none(), "a failed subscription leaves no basis");
	assert!(h.positions.unit_flows(user, &service).await.unwrap().is_empty(), "nor a line in the holder's history");
	let holds = h.money.holds_for(&format!("subscription:{}", record.subscription.id().raw().hyphenated()));
	assert_eq!(holds.iter().map(|hold| hold.state).collect::<Vec<_>>(), vec![HoldState::Voided]);
}

/// banking away for a moment is not a refusal: the relay retries from the same row, and
/// the retry captures once — the idempotency key, not luck, keeps the cash moving once.
#[tokio::test]
async fn a_capture_outage_is_retried_and_moves_the_cash_once() {
	let Some(h) = harness().await else { return };
	let service = registered_service(&h).await;
	let user = funded_user(&h, &ScopeId::invest(), "100").await;

	let record = subscribe(&h, user, &service, "60", now_unix()).await.unwrap();
	h.money.outage("Capture", 2);
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	assert_eq!(subscription_state(&h, record.subscription.id()).await, (SubscriptionState::Settled, None));
	assert_eq!(alloc_cash(&h, &service), usdt("60"));
	assert_eq!(compartment(&h, user), usdt("40"));
	assert_eq!(units(&h, &LedgerAccountKey::UserShares(service.clone(), user)).await, shares("60"));
}

/// A product in banking's other scope keeps its cash there: the hold, the capture and
/// the account are all `(sa, …)`, and invest's own scope is never touched.
#[tokio::test]
async fn a_product_in_another_scope_deals_in_that_scope() {
	let Some(h) = harness().await else { return };
	let sa = ScopeId::parse("sa").unwrap();
	let service = unique_service();
	allocations_app::register(&h.allocations, service.clone(), &sa, "sa fund", "", AllocationIcon::default())
		.await
		.unwrap();
	allocations_app::open(&h.allocations, h.money.as_ref(), &service).await.unwrap();
	h.allocations.set_access(&service, AllocationAccess::Invest).await.unwrap();
	let user = funded_user(&h, &sa, "30").await;

	let record = subscribe(&h, user, &service, "30", now_unix()).await.unwrap();
	assert_eq!(record.scope, sa);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(h.money.account(&sa, &AccountLabel::allocation(&service).unwrap()), usdt("30"));
	assert_eq!(alloc_cash(&h, &service), Usdt::ZERO, "nothing landed in invest's scope");
	assert_eq!(h.money.compartment(&sa, &common::auth_subject(user)), Usdt::ZERO);
}

// ── the redemption saga ─────────────────────────────────────────────────────

#[tokio::test]
async fn redeem_when_the_account_covers_it_pays_out_and_burns() {
	let Some(h) = harness().await else { return };
	let service = registered_service(&h).await;
	let user = funded_user(&h, &ScopeId::invest(), "100").await;
	let user_shares = LedgerAccountKey::UserShares(service.clone(), user);
	let now = now_unix();
	subscribe(&h, user, &service, "100", now).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// 40 units at the seed NAV: the account (100) covers 40, so it settles on request —
	// the cash is the relay's to pay, so the answer is `settling`, not yet `completed`.
	let r = funds_app::request_redemption(&h.fund_ports(), &h.redemptions, &h.users, user, service.clone(), shares("40"), now)
		.await
		.unwrap();
	assert_eq!(r.state(), RedemptionState::Settling);
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	let record = redemption_record(&h, user, r.id()).await;
	assert_eq!((record.redemption.state(), record.failure_reason), (RedemptionState::Completed, None));
	assert_eq!(units(&h, &user_shares).await, shares("60"), "units burned");
	assert_eq!(units(&h, &LedgerAccountKey::SharesOutstanding(service.clone())).await, shares("60"));
	assert_eq!(alloc_cash(&h, &service), usdt("60"), "the cash left the product's account");
	assert_eq!(compartment(&h, user), usdt("40"), "and landed in the holder's compartment");
	assert_eq!(cost_basis(&h, user, &service).await, Some(usdt("60")), "basis reduced pro rata after the payout");
}

/// A holder with 100 units, subscribed with 100 at the seed NAV, in a product marked up to
/// `aum` (through the shared mark writer, past the move guard) — then a request for
/// `units`, which the 100 in the account cannot cover, so it queues.
async fn queued_short_redemption(h: &Harness, user: UserId, service: &ServiceId, aum: &str, units: &str) -> RedemptionId {
	let now = now_unix();
	subscribe(h, user, service, "100", now).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	funds_app::record_valuation(&h.nav, h.ledger.as_ref(), ValuationId::new(), service.clone(), usdt(aum), "op")
		.await
		.unwrap();
	let r = funds_app::request_redemption(&h.fund_ports(), &h.redemptions, &h.users, user, service.clone(), shares(units), now)
		.await
		.unwrap();
	assert_eq!(r.state(), RedemptionState::Queued, "a short account queues the redemption");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	r.id()
}

#[tokio::test]
async fn a_queued_redemption_settles_at_the_settle_time_nav_once_the_account_tops_up() {
	let Some(h) = harness().await else { return };
	let service = registered_service(&h).await;
	let user = funded_user(&h, &ScopeId::invest(), "100").await;
	let user_shares = LedgerAccountKey::UserShares(service.clone(), user);

	let id = queued_short_redemption(&h, user, &service, "200", "100").await;
	assert_eq!(units(&h, &user_shares).await, shares("100"), "reserved, not burned");
	assert_eq!(units_available(&h, &user_shares).await, Shares::ZERO, "locked by the pending burn");
	assert_eq!(compartment(&h, user), Usdt::ZERO, "nothing paid while queued");
	let queue = h.redemptions.list_queued().await.unwrap();
	assert!(queue.iter().any(|q| q.id == id && q.user_id == user && q.units == shares("100")), "on the operator's queue");

	// The product realizes its profit into its account (banking's side of the world).
	h.money.credit(&ScopeId::invest(), &AccountLabel::allocation(&service).unwrap(), usdt("100"));
	let settled = funds_app::settle_redemption(&h.fund_ports(), &h.redemptions, id, now_unix()).await.unwrap();
	assert_eq!(settled.state(), RedemptionState::Settling);
	assert_eq!(settled.cash(), Some(usdt("200")), "priced at the settle-time NAV of 2");
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	assert_eq!(redemption_record(&h, user, id).await.redemption.state(), RedemptionState::Completed);
	assert_eq!(units(&h, &user_shares).await, Shares::ZERO);
	assert_eq!(alloc_cash(&h, &service), Usdt::ZERO);
	assert_eq!(compartment(&h, user), usdt("200"), "100 in, 200 out");
	assert!(!h.redemptions.list_queued().await.unwrap().iter().any(|q| q.id == id), "off the queue");
}

/// Settling while the account is still short: banking refuses the payout, the relay voids
/// the burn — the holder keeps every unit — and the redemption fails with banking's reason.
#[tokio::test]
async fn a_refused_payout_fails_the_redemption_and_returns_the_units() {
	let Some(h) = harness().await else { return };
	let service = registered_service(&h).await;
	let user = funded_user(&h, &ScopeId::invest(), "100").await;
	let user_shares = LedgerAccountKey::UserShares(service.clone(), user);

	let id = queued_short_redemption(&h, user, &service, "200", "100").await;
	funds_app::settle_redemption(&h.fund_ports(), &h.redemptions, id, now_unix()).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	let record = redemption_record(&h, user, id).await;
	assert_eq!(record.redemption.state(), RedemptionState::Failed);
	assert_eq!(record.failure_reason.as_deref(), Some("insufficient_funds"));
	assert_eq!(units(&h, &user_shares).await, shares("100"), "nothing burned");
	assert_eq!(units_available(&h, &user_shares).await, shares("100"), "and nothing still locked");
	assert_eq!(alloc_cash(&h, &service), usdt("100"), "the account is untouched");
	assert_eq!(compartment(&h, user), Usdt::ZERO);
	assert_eq!(cost_basis(&h, user, &service).await, Some(usdt("100")), "no basis given back for cash never paid");
}

/// A covered redemption past banking's automatic ceilings: 40 of 100 units, the account
/// holding 100 — settled on request, then `approval_required` on the payout.
async fn redemption_past_the_ceilings(h: &Harness, user: UserId, service: &ServiceId) -> RedemptionId {
	subscribe(h, user, service, "100", now_unix()).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	h.money.refuse("Payout", Refusal::ApprovalRequired);
	let r = funds_app::request_redemption(&h.fund_ports(), &h.redemptions, &h.users, user, service.clone(), shares("40"), now_unix())
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	h.money.serve("Payout");
	r.id()
}

/// Past the automatic ceilings the payout goes to the owners: invest asks for it once
/// (`RequestPayout`), the redemption waits `awaiting_approval` with its units still locked
/// and nothing paid, and neither the holder nor a repeated settle can move it. Approved and
/// executed, the money event completes it — the burn posted, the basis reduced — and the
/// same event again does nothing more.
#[tokio::test]
async fn a_payout_past_the_ceilings_awaits_the_owners_and_completes_once_executed() {
	let Some(h) = harness().await else { return };
	let consumer = h.money_events(BookFeed::new());
	let service = registered_service(&h).await;
	let user = funded_user(&h, &ScopeId::invest(), "100").await;
	let user_shares = LedgerAccountKey::UserShares(service.clone(), user);
	let id = redemption_past_the_ceilings(&h, user, &service).await;

	let subject = format!("redemption:{}", id.raw().hyphenated());
	let operations = h.money.operations_for(&subject);
	assert_eq!(operations.len(), 1, "asked for once");
	let record = redemption_record(&h, user, id).await;
	assert_eq!((record.redemption.state(), record.failure_reason), (RedemptionState::AwaitingApproval, None));
	assert_eq!(units_available(&h, &user_shares).await, shares("60"), "the 40 stay locked by the pending burn");
	assert_eq!((alloc_cash(&h, &service), compartment(&h, user)), (usdt("100"), Usdt::ZERO), "nothing paid yet");
	let again = funds_app::settle_redemption(&h.fund_ports(), &h.redemptions, id, now_unix()).await.unwrap();
	assert_eq!(again.state(), RedemptionState::AwaitingApproval, "a repeated settle changes nothing");
	assert!(funds_app::cancel_redemption(&h.redemptions, &h.notify, id, user).await.is_err(), "the holder cannot pull out");
	assert_eq!(consumer.pass().await.unwrap().applied, 0, "a requested operation is no verdict");
	assert_eq!(consumer.sweep_awaiting().await.unwrap(), 0, "nor is one still awaiting");

	h.money.approve(&operations[0].id);
	let pass = consumer.pass().await.unwrap();
	assert_eq!(pass.applied, 1, "{pass:?}");
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	let record = redemption_record(&h, user, id).await;
	assert_eq!((record.redemption.state(), record.failure_reason), (RedemptionState::Completed, None));
	assert_eq!(units(&h, &user_shares).await, shares("60"), "the 40 burned");
	assert_eq!(units(&h, &LedgerAccountKey::SharesOutstanding(service.clone())).await, shares("60"));
	assert_eq!((alloc_cash(&h, &service), compartment(&h, user)), (usdt("60"), usdt("40")), "paid once, by the owners' payout");
	assert_eq!(cost_basis(&h, user, &service).await, Some(usdt("60")));

	let paid_out = h
		.money
		.events()
		.iter()
		.find(|event| event.kind == MoneyEventKind::PaidOut && event.subject_ref == subject)
		.unwrap()
		.position;
	h.money.redeliver(paid_out);
	// Nothing awaits the operation any more, so the event asks for nothing.
	assert_eq!(consumer.pass().await.unwrap(), Pass::default());
	assert_eq!(consumer.sweep_awaiting().await.unwrap(), 0);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units(&h, &user_shares).await, shares("60"), "nothing burned twice");
	assert_eq!(cost_basis(&h, user, &service).await, Some(usdt("60")), "nor reduced twice");
}

/// The owners say no — or let it lapse: the burn is voided, the units are the holder's
/// again, and the redemption fails with banking's reason. The verdict lands whether its
/// event is read or only the sweep asks.
#[tokio::test]
async fn a_payout_the_owners_reject_fails_the_redemption_and_returns_the_units() {
	let Some(h) = harness().await else { return };
	let consumer = h.money_events(BookFeed::new());
	let service = registered_service(&h).await;
	let user = funded_user(&h, &ScopeId::invest(), "100").await;
	let user_shares = LedgerAccountKey::UserShares(service.clone(), user);

	let rejected = redemption_past_the_ceilings(&h, user, &service).await;
	let operation = h.money.operations_for(&format!("redemption:{}", rejected.raw().hyphenated())).remove(0);
	h.money.reject(&operation.id, "approval_rejected");
	assert_eq!(consumer.pass().await.unwrap().applied, 1);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let record = redemption_record(&h, user, rejected).await;
	assert_eq!(
		(record.redemption.state(), record.failure_reason.as_deref()),
		(RedemptionState::Failed, Some("approval_rejected"))
	);
	assert_eq!(units_available(&h, &user_shares).await, shares("100"), "the units are free again");
	assert_eq!((alloc_cash(&h, &service), compartment(&h, user)), (usdt("100"), Usdt::ZERO), "nothing paid");

	// The event is missed — the consumer passed it before the verdict existed — and the
	// sweep re-asks banking.
	h.money.refuse("Payout", Refusal::ApprovalRequired);
	let lapsed = funds_app::request_redemption(&h.fund_ports(), &h.redemptions, &h.users, user, service.clone(), shares("40"), now_unix())
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	h.money.serve("Payout");
	consumer.pass().await.unwrap();
	let operation = h.money.operations_for(&format!("redemption:{}", lapsed.id().raw().hyphenated())).remove(0);
	h.money.reject(&operation.id, "approval_expired");
	sqlx::query("UPDATE money_events_cursor SET position = position + 1").execute(&h.pool).await.unwrap();
	assert_eq!(consumer.pass().await.unwrap().applied, 0, "the event was skipped");
	assert_eq!(consumer.sweep_awaiting().await.unwrap(), 1);
	assert_eq!(consumer.sweep_awaiting().await.unwrap(), 0, "landed once");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let record = redemption_record(&h, user, lapsed.id()).await;
	assert_eq!((record.redemption.state(), record.failure_reason.as_deref()), (RedemptionState::Failed, Some("approval_expired")));
	assert_eq!(units_available(&h, &user_shares).await, shares("100"));
}

/// A crash between `RequestPayout` and the `awaiting_approval` stamp leaves the operation
/// standing and the redemption `settling` with no operation id. The retry must not ask for
/// the payout again as if nothing had happened: even when `Payout` would now answer something
/// else (here `user_frozen`), the operation exists, so the redemption goes back to waiting on
/// it — never voids its burn while the owners can still approve a payout. Paid once.
#[tokio::test]
async fn a_retry_after_the_payout_was_requested_waits_on_that_operation_and_pays_once() {
	let Some(h) = harness().await else { return };
	let consumer = h.money_events(BookFeed::new());
	let service = registered_service(&h).await;
	let user = funded_user(&h, &ScopeId::invest(), "100").await;
	let user_shares = LedgerAccountKey::UserShares(service.clone(), user);
	let id = redemption_past_the_ceilings(&h, user, &service).await;
	let operation = h.money.operations_for(&format!("redemption:{}", id.raw().hyphenated())).remove(0);

	// The crash: the stamp never landed and the event is delivered again.
	sqlx::query("UPDATE redemptions SET state = 'settling', payout_operation_id = NULL WHERE id = $1")
		.bind(id.raw())
		.execute(&h.pool)
		.await
		.unwrap();
	sqlx::query("UPDATE outbox SET dispatched_at = NULL WHERE aggregate_id = $1 AND payload->>'type' = 'settled'")
		.bind(id.raw())
		.execute(&h.pool)
		.await
		.unwrap();
	h.money.refuse("Payout", Refusal::UserFrozen);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	h.money.serve("Payout");

	let record = redemption_record(&h, user, id).await;
	assert_eq!((record.redemption.state(), record.failure_reason), (RedemptionState::AwaitingApproval, None));
	assert_eq!(
		h.money.operations_for(&format!("redemption:{}", id.raw().hyphenated())),
		vec![operation.clone()],
		"the same operation, not a second"
	);
	assert_eq!(units_available(&h, &user_shares).await, shares("60"), "the burn is still pending");

	h.money.approve(&operation.id);
	consumer.pass().await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(redemption_record(&h, user, id).await.redemption.state(), RedemptionState::Completed);
	assert_eq!((alloc_cash(&h, &service), compartment(&h, user)), (usdt("60"), usdt("40")), "paid once");
}

/// Two redemptions of one holder waiting on the owners: 40 and 20 of 100 units.
async fn two_awaiting(h: &Harness, user: UserId, service: &ServiceId) -> [RedemptionId; 2] {
	let first = redemption_past_the_ceilings(h, user, service).await;
	h.money.refuse("Payout", Refusal::ApprovalRequired);
	let second = funds_app::request_redemption(&h.fund_ports(), &h.redemptions, &h.users, user, service.clone(), shares("20"), now_unix())
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	h.money.serve("Payout");
	[first, second.id()]
}

fn approve_all(h: &Harness, ids: &[RedemptionId]) {
	for id in ids {
		let operation = h.money.operations_for(&format!("redemption:{}", id.raw().hyphenated())).remove(0);
		h.money.approve(&operation.id);
	}
}

/// One verdict banking cannot give right now must not stop the stream behind it: the event
/// passes, the sweep is the safety net.
#[tokio::test]
async fn a_verdict_banking_cannot_give_does_not_stop_the_stream() {
	let Some(h) = harness().await else { return };
	let consumer = h.money_events(BookFeed::new());
	let service = registered_service(&h).await;
	let user = funded_user(&h, &ScopeId::invest(), "100").await;
	let ids = two_awaiting(&h, user, &service).await;
	approve_all(&h, &ids);

	h.money.outage("GetPendingOperation", 1);
	consumer.pass().await.expect("the stream goes on past a verdict it could not read");
	assert_eq!(consumer.sweep_awaiting().await.unwrap(), 1, "the sweep lands what the first event could not");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	for id in ids {
		assert_eq!(redemption_record(&h, user, id).await.redemption.state(), RedemptionState::Completed);
	}
}

/// ... and one record the sweep cannot read does not stop it over the others.
#[tokio::test]
async fn a_verdict_banking_cannot_give_does_not_stop_the_sweep() {
	let Some(h) = harness().await else { return };
	let consumer = h.money_events(BookFeed::new());
	let service = registered_service(&h).await;
	let user = funded_user(&h, &ScopeId::invest(), "100").await;
	let ids = two_awaiting(&h, user, &service).await;
	consumer.pass().await.unwrap();
	approve_all(&h, &ids);
	// The events are skipped: only the sweep can tell.
	sqlx::query("UPDATE money_events_cursor SET position = position + 100").execute(&h.pool).await.unwrap();

	h.money.outage("GetPendingOperation", 1);
	assert_eq!(consumer.sweep_awaiting().await.expect("a record it cannot read is skipped"), 1);
	assert_eq!(consumer.sweep_awaiting().await.unwrap(), 1, "and read on the next sweep");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	for id in ids {
		assert_eq!(redemption_record(&h, user, id).await.redemption.state(), RedemptionState::Completed);
	}
}

/// banking refuses the request itself — the account fell short between the payout and the
/// request: no operation, the units back, the reason banking gave.
#[tokio::test]
async fn a_refused_payout_request_fails_the_redemption() {
	let Some(h) = harness().await else { return };
	let service = registered_service(&h).await;
	let user = funded_user(&h, &ScopeId::invest(), "100").await;
	h.money.refuse("RequestPayout", Refusal::UserFrozen);
	let id = redemption_past_the_ceilings(&h, user, &service).await;
	h.money.serve("RequestPayout");

	let record = redemption_record(&h, user, id).await;
	assert_eq!((record.redemption.state(), record.failure_reason.as_deref()), (RedemptionState::Failed, Some("user_frozen")));
	assert!(h.money.operations_for(&format!("redemption:{}", id.raw().hyphenated())).is_empty());
	assert_eq!(units_available(&h, &LedgerAccountKey::UserShares(service.clone(), user)).await, shares("100"));
}

#[tokio::test]
async fn cancelling_or_failing_a_queued_redemption_returns_the_units() {
	let Some(h) = harness().await else { return };
	let service = registered_service(&h).await;
	let user = funded_user(&h, &ScopeId::invest(), "100").await;
	let user_shares = LedgerAccountKey::UserShares(service.clone(), user);

	let id = queued_short_redemption(&h, user, &service, "200", "60").await;
	assert!(
		matches!(funds_app::cancel_redemption(&h.redemptions, &h.notify, id, UserId::new()).await, Err(DomainError::Forbidden(_))),
		"only the owner may cancel"
	);
	let cancelled = funds_app::cancel_redemption(&h.redemptions, &h.notify, id, user).await.unwrap();
	assert_eq!(cancelled.state(), RedemptionState::Cancelled);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_available(&h, &user_shares).await, shares("100"), "the cancel released the lock");

	let r = funds_app::request_redemption(&h.fund_ports(), &h.redemptions, &h.users, user, service.clone(), shares("60"), now_unix())
		.await
		.unwrap();
	assert_eq!(r.state(), RedemptionState::Queued);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let failed = funds_app::fail_redemption(&h.redemptions, &h.notify, r.id()).await.unwrap();
	assert_eq!(failed.state(), RedemptionState::Failed);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_available(&h, &user_shares).await, shares("100"), "the operator's fail released it too");
	assert_eq!(units(&h, &user_shares).await, shares("100"));
}

/// Back-to-back settles compound the basis reduction on the position's own units, and a
/// repeated settle — the documented-idempotent retry — reduces it once.
#[tokio::test]
async fn settles_compound_the_basis_and_a_repeat_settle_reduces_it_once() {
	let Some(h) = harness().await else { return };
	let service = registered_service(&h).await;
	let user = funded_user(&h, &ScopeId::invest(), "100").await;
	let now = now_unix();
	subscribe(&h, user, &service, "100", now).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	let first = funds_app::request_redemption(&h.fund_ports(), &h.redemptions, &h.users, user, service.clone(), shares("50"), now)
		.await
		.unwrap();
	let second = funds_app::request_redemption(&h.fund_ports(), &h.redemptions, &h.users, user, service.clone(), shares("25"), now)
		.await
		.unwrap();
	let again = funds_app::settle_redemption(&h.fund_ports(), &h.redemptions, first.id(), now).await.unwrap();
	assert_eq!(again.state(), RedemptionState::Settling, "a repeat settle is a no-op");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	for id in [first.id(), second.id()] {
		assert_eq!(redemption_record(&h, user, id).await.redemption.state(), RedemptionState::Completed);
	}
	// 100 × 50/100 = 50, then 50 × 25/50 = 25.
	assert_eq!(cost_basis(&h, user, &service).await, Some(usdt("25")));
	assert_eq!(compartment(&h, user), usdt("75"));
}

/// The cooldown: whoever posted a mark for a product cannot redeem from it inside the
/// window — at request, and at settle for a redemption queued before their mark.
#[tokio::test]
async fn a_valuation_poster_cannot_redeem_inside_the_cooldown() {
	let Some(h) = harness().await else { return };
	let service = registered_service(&h).await;
	let user = funded_user(&h, &ScopeId::invest(), "100").await;
	let queued = queued_short_redemption(&h, user, &service, "200", "60").await;

	// The holder marks the product themselves (NAV 2 → 2.5, inside the move guard).
	funds_app::post_fund_valuation(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), usdt("250"), &user.to_string(), now_unix())
		.await
		.unwrap();
	let err = funds_app::request_redemption(&h.fund_ports(), &h.redemptions, &h.users, user, service.clone(), shares("1"), now_unix())
		.await
		.unwrap_err();
	assert!(matches!(&err, DomainError::Precondition(m) if m.contains("posted a valuation")), "{err:?}");
	h.money.credit(&ScopeId::invest(), &AccountLabel::allocation(&service).unwrap(), usdt("1000"));
	let err = funds_app::settle_redemption(&h.fund_ports(), &h.redemptions, queued, now_unix()).await.unwrap_err();
	assert!(matches!(&err, DomainError::Precondition(m) if m.contains("posted a valuation")), "{err:?}");

	// Their mark aged out of the window: the operator's NAV-2 mark prices the settle again.
	sqlx::query("UPDATE fund_valuations SET posted_at = posted_at - make_interval(secs => $3) WHERE service = $1 AND posted_by = $2")
		.bind(service.as_str())
		.bind(user.to_string())
		.bind((funds_app::VALUATION_REDEEM_COOLDOWN_SECS + 60) as f64)
		.execute(&h.pool)
		.await
		.unwrap();
	let settled = funds_app::settle_redemption(&h.fund_ports(), &h.redemptions, queued, now_unix()).await.unwrap();
	assert_eq!(settled.cash(), Some(usdt("120")), "60 units at NAV 2");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(redemption_record(&h, user, queued).await.redemption.state(), RedemptionState::Completed);
}

// ── a redemption is paid only for units it actually holds ─────────────────────

/// The units a covered redemption names leave the holding by another road before the relay
/// reserves its burn (a move the outbox never saw). The reserve is refused, so the
/// redemption fails — and its settlement, already queued behind it, must not pay out for
/// units nobody burned.
#[tokio::test]
async fn a_redemption_whose_burn_could_not_be_reserved_is_never_paid() {
	let Some(h) = harness().await else { return };
	let service = registered_service(&h).await;
	let user = funded_user(&h, &ScopeId::invest(), "100").await;
	let user_shares = LedgerAccountKey::UserShares(service.clone(), user);
	subscribe(&h, user, &service, "100", now_unix()).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	let r = funds_app::request_redemption(&h.fund_ports(), &h.redemptions, &h.users, user, service.clone(), shares("100"), now_unix())
		.await
		.unwrap();
	assert_eq!(r.state(), RedemptionState::Settling, "covered: Requested and Settled are both queued");
	h.ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::BookShares(service.clone(), user),
			credit: user_shares.clone(),
			amount: shares("100").base_units(),
			code: TransferCode::BookLock,
			reference: 0,
		})
		.await
		.unwrap();
	// Parks are what this test pins, so single passes instead of `drain_to_quiescence`.
	h.relay.drain().await;
	h.relay.drain().await;

	let record = redemption_record(&h, user, r.id()).await;
	assert_eq!(record.redemption.state(), RedemptionState::Failed, "{:?}", record.failure_reason);
	assert!(record.failure_reason.is_some_and(|reason| reason.contains("units")), "says why");
	assert_eq!(compartment(&h, user), Usdt::ZERO, "nothing was paid for units nobody burned");
	assert_eq!(alloc_cash(&h, &service), usdt("100"));
	assert_eq!(units(&h, &LedgerAccountKey::SharesOutstanding(service.clone())).await, shares("100"), "nothing burned");
}

/// A sell order resting on units the relay has not escrowed yet: the ledger still shows
/// them free, but they are spoken for. A redemption of the same units is refused at the
/// door instead of racing the escrow to the ledger.
#[tokio::test]
async fn units_spoken_for_by_an_undrained_event_cannot_be_redeemed_again() {
	let Some(h) = harness().await else { return };
	let service = registered_service(&h).await;
	let user = funded_user(&h, &ScopeId::invest(), "100").await;
	subscribe(&h, user, &service, "100", now_unix()).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// The first redemption's Requested is still in the outbox when the second arrives.
	funds_app::request_redemption(&h.fund_ports(), &h.redemptions, &h.users, user, service.clone(), shares("60"), now_unix())
		.await
		.unwrap();
	let err = funds_app::request_redemption(&h.fund_ports(), &h.redemptions, &h.users, user, service.clone(), shares("60"), now_unix())
		.await
		.unwrap_err();
	assert!(matches!(&err, DomainError::Validation(m) if m.contains("insufficient units")), "{err:?}");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(compartment(&h, user), usdt("60"), "paid once");
	assert_eq!(units(&h, &LedgerAccountKey::UserShares(service.clone(), user)).await, shares("40"));
}

/// A completion that meets its pending already completed the OTHER way is a conflict, not
/// an idempotent success: a post of voided units, or a void of posted ones, would settle a
/// saga on a ledger that did the opposite.
#[tokio::test]
async fn a_completion_crossing_the_pendings_outcome_is_a_conflict() {
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	let mint = |id: u128| LedgerTransfer {
		id,
		debit: LedgerAccountKey::UserShares(service.clone(), user),
		credit: LedgerAccountKey::SharesOutstanding(service.clone()),
		amount: shares("5").base_units(),
		code: TransferCode::ShareMint,
		reference: 0,
	};
	let complete = |pending: &LedgerTransfer, kind: CompletionKind| PendingCompletion {
		id: Uuid::new_v4().as_u128(),
		pending_id: pending.id,
		kind,
		debit: pending.debit.clone(),
		credit: pending.credit.clone(),
		amount: pending.amount,
		code: pending.code,
		reference: 0,
	};
	let voided = mint(Uuid::new_v4().as_u128());
	h.ledger.reserve(&voided).await.unwrap();
	let void = complete(&voided, CompletionKind::Void);
	h.ledger.complete(&void).await.unwrap();
	h.ledger.complete(&void).await.expect("the same void again is idempotent");
	assert!(matches!(h.ledger.complete(&complete(&voided, CompletionKind::Void)).await, Ok(())), "another void of it too");
	assert!(matches!(h.ledger.complete(&complete(&voided, CompletionKind::Post)).await, Err(LedgerError::Conflict(_))));

	let posted = mint(Uuid::new_v4().as_u128());
	h.ledger.reserve(&posted).await.unwrap();
	h.ledger.complete(&complete(&posted, CompletionKind::Post)).await.unwrap();
	assert!(matches!(h.ledger.complete(&complete(&posted, CompletionKind::Void)).await, Err(LedgerError::Conflict(_))));
	assert_eq!(units(&h, &LedgerAccountKey::UserShares(service.clone(), user)).await, shares("5"), "only the posted one landed");
}

// ---- ResolveAwaitingRedemption -------------------------------------------------------

const MANAGE: &str = "invest:allocation:manage";

/// The gRPC handler over this harness's database, banking and ledger.
fn funds_service(h: &Harness) -> invest_core::services::funds::FundsSvc {
	let pool = h.pool.clone();
	let state = AppState {
		pool: pool.clone(),
		ledger: h.ledger.clone(),
		money: h.money.clone(),
		users: Arc::new(PgUsers::new(pool.clone())),
		allocations: Arc::new(PgAllocations::new(pool.clone())),
		issuances: Arc::new(PgUnitIssuances::new(pool.clone())),
		subscriptions: Arc::new(PgSubscriptions::new(pool.clone())),
		redemptions: Arc::new(PgRedemptions::new(pool.clone(), h.ledger.clone())),
		book: Arc::new(PgBook::new(pool.clone())),
		book_engine: Arc::new(PriceTimeEngine),
		book_feed: BookFeed::new(),
		watch_streams: WatchStreams::new(2, Duration::from_secs(60)).unwrap(),
		fees: FeePorts {
			policies: Arc::new(PgFeePolicies::new(pool.clone())),
			changes: Arc::new(PgFeePolicyChanges::new(pool.clone())),
			accruals: Arc::new(PgPositionAccruals::new(pool.clone())),
			assessments: Arc::new(PgFeeAssessments::new(pool.clone())),
			settlements: Arc::new(PgFeeSettlements::new(pool.clone(), h.ledger.clone())),
		},
		nav: Arc::new(PgNav::new(pool.clone())),
		positions: Arc::new(PgFundPositions::new(pool)),
		relay_notify: h.notify.clone(),
	};
	invest_core::services::funds::FundsSvc::new(state)
}

/// A caller the auth layer would have admitted: an access token for `user`.
fn as_caller<T>(user: UserId, message: T) -> tonic::Request<T> {
	let mut request = tonic::Request::new(message);
	request.extensions_mut().insert(Claims {
		sub: user.raw().to_string(),
		iss: "test".into(),
		aud: "invest".into(),
		exp: u64::MAX,
		iat: 0,
		typ: TokenType::Access,
		jti: None,
		token_version: 0,
	});
	request
}

/// An operator: an active seat holding `invest:allocation:manage`.
async fn operator(h: &Harness) -> UserId {
	let user = UserId::new();
	h.users
		.record(
			user,
			&UserStanding {
				active: true,
				permissions: Some(vec![MANAGE.into()]),
			},
			None,
		)
		.await
		.unwrap();
	user
}

fn resolve_request(id: RedemptionId, operation_id: &str) -> pb::ResolveAwaitingRedemptionRequest {
	pb::ResolveAwaitingRedemptionRequest {
		redemption_id: id.raw().to_string(),
		outcome: pb::AwaitingRedemptionOutcome::Fail as i32,
		operation_id: operation_id.to_owned(),
	}
}

fn the_operation(h: &Harness, id: RedemptionId) -> invest_core::ports::money::PendingOperation {
	h.money.operations_for(&format!("redemption:{}", id.raw().hyphenated())).remove(0)
}

/// A redemption awaiting approval, its holder and the units' account.
async fn awaiting_redemption(h: &Harness) -> (UserId, RedemptionId, LedgerAccountKey) {
	let service = registered_service(h).await;
	let user = funded_user(h, &ScopeId::invest(), "100").await;
	let id = redemption_past_the_ceilings(h, user, &service).await;
	(user, id, LedgerAccountKey::UserShares(service, user))
}

/// Rejected and Failed on banking's side: the operator's fail goes through at once, the burn
/// is voided and banking's machine reason is the failure reason.
#[tokio::test]
async fn an_operator_fails_a_redemption_banking_rejected_or_failed() {
	let Some(h) = harness().await else { return };
	let svc = funds_service(&h);
	let ops = operator(&h).await;
	for (reason, failed) in [("approval_rejected", false), ("insufficient_funds", true)] {
		let (user, id, user_shares) = awaiting_redemption(&h).await;
		let operation = the_operation(&h, id);
		if failed {
			h.money.fail_operation(&operation.id, reason);
		} else {
			h.money.reject(&operation.id, reason);
		}
		let answer = svc.resolve_awaiting_redemption(as_caller(ops, resolve_request(id, ""))).await.unwrap().into_inner();
		assert_eq!((answer.state.as_str(), answer.failure_reason.as_str()), ("failed", reason));
		common::drain_to_quiescence(&h.relay, &h.pool).await;
		assert_eq!(units_available(&h, &user_shares).await, shares("100"), "the burn is voided: units are free again");
		assert_eq!(compartment(&h, user), Usdt::ZERO, "nothing paid");
		let again = svc.resolve_awaiting_redemption(as_caller(ops, resolve_request(id, ""))).await.unwrap().into_inner();
		assert_eq!(again.state, "failed", "a repeat is idempotent");
	}
}

/// banking no longer knows the operation: the fail is refused unless the operator echoes the
/// operation id recorded on the redemption (empty or another id changes nothing); with the
/// echo the redemption fails with a machine reason.
#[tokio::test]
async fn a_redemption_banking_has_forgotten_needs_the_operators_echo_of_the_operation() {
	let Some(h) = harness().await else { return };
	let svc = funds_service(&h);
	let ops = operator(&h).await;
	let (_, id, user_shares) = awaiting_redemption(&h).await;
	let operation = the_operation(&h, id).id;
	h.money.forget_operation(&operation);

	for echo in ["", "op-unrelated"] {
		let refused = svc.resolve_awaiting_redemption(as_caller(ops, resolve_request(id, echo))).await.unwrap_err();
		assert_eq!(refused.code(), tonic::Code::FailedPrecondition, "echo {echo:?}: {refused}");
	}
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(h.redemptions.find_by_id(id).await.unwrap().unwrap().state(), RedemptionState::AwaitingApproval);
	assert_eq!(units_available(&h, &user_shares).await, shares("60"), "still locked");

	let answer = svc.resolve_awaiting_redemption(as_caller(ops, resolve_request(id, &operation.0))).await.unwrap().into_inner();
	assert_eq!((answer.state.as_str(), answer.failure_reason.as_str()), ("failed", "operator_resolved_not_found"));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_available(&h, &user_shares).await, shares("100"));
}

/// banking pays an operation the operator failed as "not found": the paid event finds the
/// redemption failed, flags it (error log) and moves nothing — the failure stands, no burn.
#[tokio::test]
async fn a_payout_after_an_operator_not_found_fail_moves_nothing() {
	let Some(h) = harness().await else { return };
	let svc = funds_service(&h);
	let ops = operator(&h).await;
	let consumer = h.money_events(BookFeed::new());
	let (user, id, user_shares) = awaiting_redemption(&h).await;
	let operation = the_operation(&h, id).id;
	h.money.approve(&operation);
	h.money.forget_operation(&operation);
	svc.resolve_awaiting_redemption(as_caller(ops, resolve_request(id, &operation.0))).await.unwrap();

	assert_eq!(consumer.pass().await.unwrap(), Pass::default(), "the paid event acts on nothing");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let record = redemption_record(&h, user, id).await;
	assert_eq!(
		(record.redemption.state(), record.failure_reason.as_deref()),
		(RedemptionState::Failed, Some("operator_resolved_not_found"))
	);
	assert_eq!(units_available(&h, &user_shares).await, shares("100"));
}

/// The owners have not decided: refused, the redemption keeps waiting and its units stay locked.
/// An outcome other than FAIL is refused too.
#[tokio::test]
async fn a_redemption_still_pending_at_banking_cannot_be_failed() {
	let Some(h) = harness().await else { return };
	let svc = funds_service(&h);
	let ops = operator(&h).await;
	let (_, id, user_shares) = awaiting_redemption(&h).await;

	let refused = svc.resolve_awaiting_redemption(as_caller(ops, resolve_request(id, "op-unrelated"))).await.unwrap_err();
	assert_eq!(refused.code(), tonic::Code::FailedPrecondition, "{refused}");
	let mut unspecified = resolve_request(id, "op-unrelated");
	unspecified.outcome = 0;
	assert_eq!(
		svc.resolve_awaiting_redemption(as_caller(ops, unspecified)).await.unwrap_err().code(),
		tonic::Code::InvalidArgument
	);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(h.redemptions.find_by_id(id).await.unwrap().unwrap().state(), RedemptionState::AwaitingApproval);
	assert_eq!(units_available(&h, &user_shares).await, shares("60"));
}

/// Paid wins. banking executed the payout: the operator's fail is refused (even with the
/// not-found confirmation) and the redemption completes. And when the verdict lands between
/// the operator's poll and the write, the conditional update finds the redemption moved and
/// changes nothing — the burn is posted, never voided.
#[tokio::test]
async fn a_verdict_that_arrives_while_the_operator_resolves_wins() {
	let Some(h) = harness().await else { return };
	let svc = funds_service(&h);
	let ops = operator(&h).await;
	let consumer = h.money_events(BookFeed::new());

	let (user, paid, user_shares) = awaiting_redemption(&h).await;
	let operation = the_operation(&h, paid);
	h.money.approve(&operation.id);
	let refused = svc.resolve_awaiting_redemption(as_caller(ops, resolve_request(paid, "op-unrelated"))).await.unwrap_err();
	assert_eq!(refused.code(), tonic::Code::FailedPrecondition, "{refused}");
	consumer.pass().await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(redemption_record(&h, user, paid).await.redemption.state(), RedemptionState::Completed);
	assert_eq!(units(&h, &user_shares).await, shares("60"), "burned once");

	// The race itself: the operator polled (banking said rejected), then the consumer's
	// verdict — here the paid one — landed before the operator's write.
	let (user, raced, user_shares) = awaiting_redemption(&h).await;
	let operation = the_operation(&h, raced);
	h.money.approve(&operation.id);
	consumer.pass().await.unwrap();
	let lost = h.redemptions.refuse_awaiting(raced, &operation.id.0, "approval_rejected").await.unwrap_err();
	assert!(matches!(lost, DomainError::Conflict(_)), "{lost:?}");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let record = redemption_record(&h, user, raced).await;
	assert_eq!((record.redemption.state(), record.failure_reason), (RedemptionState::Completed, None));
	assert_eq!(units(&h, &user_shares).await, shares("60"), "the burn is posted, not voided");
}

/// A caller without `invest:allocation:manage` — an investor, an unknown seat — is refused
/// before banking is asked, and the redemption is untouched.
#[tokio::test]
async fn resolving_an_awaiting_redemption_needs_the_manage_permission() {
	let Some(h) = harness().await else { return };
	let svc = funds_service(&h);
	let (user, id, _) = awaiting_redemption(&h).await;
	h.money.reject(&the_operation(&h, id).id, "approval_rejected");
	let stranger = UserId::new();

	for caller in [user, stranger] {
		let refused = svc.resolve_awaiting_redemption(as_caller(caller, resolve_request(id, "op-unrelated"))).await.unwrap_err();
		assert_eq!(refused.code(), tonic::Code::PermissionDenied, "{refused}");
	}
	assert_eq!(h.redemptions.find_by_id(id).await.unwrap().unwrap().state(), RedemptionState::AwaitingApproval);
}
