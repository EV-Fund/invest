//! Integration tests for the allocation book — real Postgres **and** TigerBeetle (no
//! mocks of either); the cash is banking's, played by the in-memory money ledger. They run
//! when `DATABASE_URL` is set and a TigerBeetle replica is reachable (`nix run .#db` +
//! `.#tb`), skip otherwise, and fail instead of skipping under `CI`.
//!
//! The load-bearing behaviour is the money: a sell escrows units on invest's ledger, a buy
//! holds its worst-case cash in banking before the order is written, and a fill is
//! delivery-versus-payment across the two — the units go pending, the buyer's hold is
//! captured into the seller's compartment, then the units post; whatever the escrow did not
//! spend comes back. Every test drives the relay deterministically and then reads both
//! ledgers, because "the order row says filled" proves nothing about whose units and whose
//! cash they are now.
//!
//! Moved from banking 9ffafce3 (`piggybank/core/tests/book.rs`). The wallet test and the
//! read-only kill switch stayed there: both are banking's.

mod common;

use std::{ops::Deref, sync::Arc};

use invest_core::{
	application::{
		allocations as allocations_app,
		book::{self as book_app, BookFeed, BookPorts, PlaceOrderRequest},
		funds as funds_app, issuance as issuance_app,
	},
	infrastructure::book::PgBook,
	ports::{
		AllocationRegistry,
		book::{BookStore, OrderRecord},
		ledger::LedgerTransfer,
		money::{AccountLabel, HoldState, Refusal, ScopeId},
		users::UserStanding,
	},
};
use invest_domain::{
	allocations::{AllocationAccess, AllocationBacking, AllocationIcon},
	balance::{LedgerAccountKey, ServiceId, TransferCode},
	book::{BookPolicy, CancelReason, CandleResolution, ClientOrderId, Locked, OrderKind, OrderState, Price, PriceTimeEngine, Side, Tif},
	error::DomainError,
	issuance::{IdempotencyKey, UnitHolder},
	money::{Shares, Usdt},
	users::UserId,
};
use uuid::Uuid;

struct Harness {
	engine: common::Engine,
	book: PgBook,
	feed: Arc<BookFeed>,
}

impl Deref for Harness {
	type Target = common::Engine;

	fn deref(&self) -> &common::Engine {
		&self.engine
	}
}

async fn harness() -> Option<Harness> {
	let engine = common::engine("book tests").await?;
	Some(Harness {
		book: PgBook::new(engine.pool.clone()),
		feed: BookFeed::new(),
		engine,
	})
}

fn book_ports(h: &Harness) -> BookPorts<'_> {
	BookPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		money: h.money.as_ref(),
		nav: &h.nav,
		store: &h.book,
		engine: &PriceTimeEngine,
		users: &h.users,
		relay: &h.notify,
		feed: &h.feed,
	}
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn shares(decimal: &str) -> Shares {
	Shares::parse_decimal(decimal).unwrap()
}

fn price(decimal: &str) -> Price {
	Price::parse_decimal(decimal).unwrap()
}

fn unique_service() -> ServiceId {
	ServiceId::parse(&format!("book-{}", Uuid::new_v4())).unwrap()
}

fn now_unix() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// A person known to invest and to banking — orders reference the `users` row, and a buy
/// is held on the compartment banking names by their subject.
async fn provisioned_user(h: &Harness) -> UserId {
	common::active_user(&h.users).await
}

/// A registered, open, invest-to-everyone product whose book is open at `fee_bps`.
async fn tradable_product(h: &Harness, fee_bps: u32) -> ServiceId {
	let service = listed_product(h).await;
	open_book(h, &service, fee_bps).await;
	service
}

/// Registered in invest's scope, opened (its cash account with it) and at `invest` access.
async fn listed_product(h: &Harness) -> ServiceId {
	let service = unique_service();
	allocations_app::register(&h.allocations, service.clone(), &ScopeId::invest(), "Service Arb", "Arbitrage", AllocationIcon::Arbitrage)
		.await
		.unwrap();
	allocations_app::open(&h.allocations, h.money.as_ref(), &service).await.unwrap();
	h.allocations.set_access(&service, AllocationAccess::Invest).await.unwrap();
	service
}

/// Open with unbacked trading acknowledged: every seller here is seeded by `issue_units`,
/// an in-kind mint that flips the product to `in_kind` — the same shape as `service_arb` in
/// production — and an unacknowledged book on such a product takes no order.
async fn open_book(h: &Harness, service: &ServiceId, fee_bps: u32) {
	let policy = BookPolicy::new(true, fee_bps, BookPolicy::DEFAULT_PRICE_TICK, BookPolicy::DEFAULT_LOT_SIZE, 500, true).unwrap();
	book_app::set_policy(&h.allocations, &h.book, service, policy).await.unwrap();
}

/// Units in kind, applied: the seller's side of every test starts here.
async fn issue_units(h: &Harness, service: &ServiceId, user: UserId, units: &str) {
	issuance_app::issue_units(
		&h.fund_ports(),
		&h.issuances,
		&h.users,
		issuance_app::IssueUnitsRequest {
			service: service.clone(),
			holder: UnitHolder::User(user),
			units: shares(units),
			cost_basis: None,
			idempotency_key: IdempotencyKey::parse(&format!("issue-{}", Uuid::new_v4())).unwrap(),
		},
		now_unix(),
	)
	.await
	.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

/// Cash in the user's compartment of invest's scope: the buyer's side of every test.
fn fund_user(h: &Harness, user: UserId, amount: &str) {
	h.money.fund(&ScopeId::invest(), &common::auth_subject(user), usdt(amount));
}

/// What the user's compartment holds free — held cash is not in it.
fn cash_of(h: &Harness, user: UserId) -> Usdt {
	h.money.compartment(&ScopeId::invest(), &common::auth_subject(user))
}

/// The `fee` allocation's cash, where every taker fee ends.
fn fee_cash(h: &Harness) -> Usdt {
	h.money.account(&ScopeId::invest(), &AccountLabel::allocation(&ServiceId::fee()).unwrap())
}

fn product_cash(h: &Harness, service: &ServiceId) -> Usdt {
	h.money.account(&ScopeId::invest(), &AccountLabel::allocation(service).unwrap())
}

/// The hold a buy order placed in banking.
fn hold_of(h: &Harness, record: &OrderRecord) -> invest_core::ports::money::Hold {
	let mut holds = h.money.holds_for(&format!("book_order:{}", record.order.id().raw().hyphenated()));
	assert_eq!(holds.len(), 1, "one hold per buy order");
	holds.remove(0)
}

// The order's own fields, spelled out so each test reads as the order it places.
#[allow(clippy::too_many_arguments)]
async fn place(h: &Harness, user: UserId, service: &ServiceId, side: Side, kind: OrderKind, tif: Tif, at: Option<&str>, size: &str) -> Result<OrderRecord, DomainError> {
	place_keyed(h, user, service, side, kind, tif, at, size, &format!("c-{}", Uuid::new_v4())).await
}

// Same, plus the retry key for the idempotency test.
#[allow(clippy::too_many_arguments)]
async fn place_keyed(h: &Harness, user: UserId, service: &ServiceId, side: Side, kind: OrderKind, tif: Tif, at: Option<&str>, size: &str, key: &str) -> Result<OrderRecord, DomainError> {
	book_app::place_order(
		&book_ports(h),
		user,
		PlaceOrderRequest {
			service: service.clone(),
			side,
			kind,
			tif,
			price: at.map(price),
			size: shares(size),
			client_order_id: ClientOrderId::parse(key).unwrap(),
		},
	)
	.await
}

async fn sell(h: &Harness, user: UserId, service: &ServiceId, at: &str, size: &str) -> OrderRecord {
	place(h, user, service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some(at), size).await.unwrap()
}

/// A sell the matcher can fill: placed, and its units escrowed by the relay.
async fn sell_escrowed(h: &Harness, user: UserId, service: &ServiceId, at: &str, size: &str) -> OrderRecord {
	let order = sell(h, user, service, at, size).await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	order
}

async fn buy(h: &Harness, user: UserId, service: &ServiceId, at: &str, size: &str) -> OrderRecord {
	place(h, user, service, Side::Buy, OrderKind::Limit, Tif::Gtc, Some(at), size).await.unwrap()
}

async fn units_of(h: &Harness, key: LedgerAccountKey) -> Shares {
	Shares::from_base_units(h.ledger.balance(&key).await.unwrap().posted)
}

async fn order(h: &Harness, record: &OrderRecord) -> OrderRecord {
	h.book.find_order(record.order.id()).await.unwrap().expect("the order row stands")
}

async fn position(h: &Harness, user: UserId, service: &ServiceId) -> funds_app::PositionView {
	funds_app::get_position(&h.fund_ports(), &h.positions, user, service.clone()).await.unwrap()
}

async fn snapshot(h: &Harness, service: &ServiceId, now: i64) -> book_app::BookSnapshotView {
	book_app::snapshot(&h.book, &h.nav, h.ledger.as_ref(), h.money.as_ref(), service, 20, now).await.unwrap()
}

async fn watch_frame(h: &Harness, service: &ServiceId, caller: UserId) -> book_app::WatchFrame {
	book_app::watch_frame(&h.book, &h.nav, h.ledger.as_ref(), h.money.as_ref(), service, caller, 20, 20, now_unix())
		.await
		.unwrap()
}

/// The parked outbox rows for one aggregate, with their reasons.
async fn parked(h: &Harness, aggregate_id: Uuid) -> Vec<String> {
	sqlx::query_scalar::<_, Option<String>>("SELECT last_error FROM outbox WHERE aggregate_id = $1 AND parked_at IS NOT NULL ORDER BY seq")
		.bind(aggregate_id)
		.fetch_all(&h.pool)
		.await
		.unwrap()
		.into_iter()
		.map(Option::unwrap_or_default)
		.collect()
}

#[tokio::test]
async fn a_crossing_limit_buy_settles_delivery_versus_payment_with_the_takers_fee() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 100).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "100").await;
	fund_user(&h, buyer, "100");
	let revenue_before = fee_cash(&h);

	// The seller rests 10 at 1.50: its units leave the holding for the book's escrow.
	let ask = sell(&h, seller, &service, "1.5", "10").await;
	assert_eq!(ask.order.state(), OrderState::Open);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), seller)).await, shares("90"));
	assert_eq!(units_of(&h, LedgerAccountKey::BookShares(service.clone(), seller)).await, shares("10"));
	let resting = position(&h, seller, &service).await;
	assert_eq!((resting.units, resting.units_in_orders), (shares("90"), shares("10")), "the position shows what is in orders");
	assert_eq!(resting.value, usdt("100"), "units in orders are still valued as the holder's");

	// The buyer crosses it at the same price and pays 1 % as the taker: 15 + 0.15 is held
	// in banking before the order is written.
	let bid = buy(&h, buyer, &service, "1.5", "10").await;
	assert_eq!(hold_of(&h, &bid).amount, usdt("15.15"));
	assert_eq!(bid.order.state(), OrderState::Filled);
	assert_eq!(bid.order.cancel_reason(), None, "a filled order ended by filling, not by a cancel");
	assert_eq!(bid.order.average_fill_price(), Some(price("1.5")));
	assert_eq!(bid.order.fee_paid(), usdt("0.15"));
	assert_eq!(order(&h, &ask).await.order.state(), OrderState::Filled, "the maker filled too");
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// Delivery versus payment: 10 units to the buyer, 15 USDT to the seller, 0.15 through
	// the product's account and the scope's fee route to the `fee` allocation — and nothing
	// left in either escrow.
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), buyer)).await, shares("10"));
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), seller)).await, shares("90"));
	assert_eq!(units_of(&h, LedgerAccountKey::BookShares(service.clone(), seller)).await, Shares::ZERO);
	assert_eq!(cash_of(&h, seller), usdt("15"));
	assert_eq!(cash_of(&h, buyer), usdt("84.85"));
	let hold = hold_of(&h, &bid);
	assert_eq!((hold.state, hold.captured), (HoldState::Captured, usdt("15.15")), "the hold was spent whole");
	assert_eq!(
		fee_cash(&h).checked_sub(revenue_before),
		Some(usdt("0.15")),
		"the taker's fee landed on the fee allocation's account"
	);
	assert_eq!(h.money.collected(&ScopeId::invest()), usdt("0.15"), "through the scope's fee collector");
	assert_eq!(product_cash(&h, &service), Usdt::ZERO, "the product's account only passed the fee on");
	// Supply is untouched: the book never mints or burns.
	assert_eq!(units_of(&h, LedgerAccountKey::SharesOutstanding(service.clone())).await, shares("100"));
	assert_eq!(parked(&h, ask.order.id().raw()).await, Vec::<String>::new());
	assert_eq!(parked(&h, bid.order.id().raw()).await, Vec::<String>::new());

	// Cost basis moved on both sides: the buyer paid 15.15 for 10 units, the seller's
	// 100-unit basis of 100 shrank pro rata to 90.
	let bought = position(&h, buyer, &service).await;
	assert_eq!((bought.units, bought.cost_basis), (shares("10"), usdt("15.15")));
	let sold = position(&h, seller, &service).await;
	assert_eq!((sold.units, sold.cost_basis), (shares("90"), usdt("90")));

	// The tape, the caller's own view of it, and the snapshot after the trade.
	let tape = book_app::list_trades(&h.book, &service, 10).await.unwrap();
	assert_eq!(tape.len(), 1);
	assert_eq!((tape[0].trade.price, tape[0].trade.size, tape[0].trade.taker_side), (price("1.5"), shares("10"), Side::Buy));
	let mine = book_app::list_user_trades(&h.book, buyer, Some(&service), 10).await.unwrap();
	assert_eq!((mine[0].side, mine[0].order_id, mine[0].fee), (Side::Buy, bid.order.id(), usdt("0.15")));
	let theirs = book_app::list_user_trades(&h.book, seller, Some(&service), 10).await.unwrap();
	assert_eq!(
		(theirs[0].side, theirs[0].order_id, theirs[0].fee),
		(Side::Sell, ask.order.id(), Usdt::ZERO),
		"the maker paid nothing"
	);
	let snapshot = snapshot(&h, &service, now_unix()).await;
	assert!(snapshot.bids.is_empty() && snapshot.asks.is_empty());
	assert_eq!(snapshot.last, Some((price("1.5"), Side::Buy)));
	assert_eq!(snapshot.volume_24h, shares("10"));
	assert_eq!(snapshot.change_24h_pct.as_deref(), Some("0.00"), "one trade: the reference is itself");
	assert!(snapshot.revision >= 2, "a placement and a fill each bumped the revision");
	assert_eq!(book_app::list_open_orders(&h.book, seller, None).await.unwrap().len(), 0);
	assert_eq!(book_app::list_order_history(&h.book, seller, Some(&service), 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_buy_below_its_limit_gets_the_price_improvement_back() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 100).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "100");
	sell(&h, seller, &service, "1", "10").await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// Bid 1.20 for 10: the hold takes 12 + 1 % = 12.12, the fill lands at the maker's 1.00
	// for 10 + 0.10 fee, and the 2.02 difference is voided back on completion.
	let bid = buy(&h, buyer, &service, "1.2", "10").await;
	assert_eq!(bid.order.state(), OrderState::Filled);
	assert_eq!(bid.order.notional_filled(), usdt("10"));
	assert_eq!(bid.order.fee_paid(), usdt("0.1"));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(cash_of(&h, buyer), usdt("89.9"));
	let hold = hold_of(&h, &bid);
	assert_eq!((hold.state, hold.captured), (HoldState::Voided, usdt("10.1")), "the price improvement was released");
	assert_eq!(cash_of(&h, seller), usdt("10"));
}

#[tokio::test]
async fn a_partial_fill_leaves_the_maker_resting_with_the_rest() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "100");
	let ask = sell_escrowed(&h, seller, &service, "1", "10").await;
	let bid = buy(&h, buyer, &service, "1", "4").await;
	assert_eq!(bid.order.state(), OrderState::Filled);
	let ask = order(&h, &ask).await;
	assert_eq!(
		(ask.order.state(), ask.order.filled(), ask.order.remaining()),
		(OrderState::PartiallyFilled, shares("4"), shares("6"))
	);

	let snapshot = snapshot(&h, &service, now_unix()).await;
	assert_eq!(snapshot.asks.len(), 1);
	assert_eq!((snapshot.asks[0].price, snapshot.asks[0].size, snapshot.asks[0].orders), (price("1"), shares("6"), 1));
	assert_eq!(book_app::list_open_orders(&h.book, seller, Some(&service)).await.unwrap().len(), 1);

	common::drain_to_quiescence(&h.relay, &h.pool).await;
	// Six units still in escrow, four delivered.
	assert_eq!(units_of(&h, LedgerAccountKey::BookShares(service.clone(), seller)).await, shares("6"));
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), buyer)).await, shares("4"));
}

#[tokio::test]
async fn an_ioc_fills_what_it_can_and_releases_the_rest() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "5").await;
	fund_user(&h, buyer, "100");
	sell(&h, seller, &service, "1", "5").await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	let bid = place(&h, buyer, &service, Side::Buy, OrderKind::Limit, Tif::Ioc, Some("1"), "8").await.unwrap();
	assert_eq!(
		(bid.order.state(), bid.order.filled()),
		(OrderState::Cancelled, shares("5")),
		"the unfilled 3 are cancelled, never rested"
	);
	// `cancelled` with `filled > 0` is exactly what a user's cancel after a partial fill
	// looks like — the reason is what tells the two apart, and it is what the row stores.
	assert_eq!(bid.order.cancel_reason(), Some(CancelReason::IocRemainder));
	assert_eq!(order(&h, &bid).await.order.cancel_reason(), Some(CancelReason::IocRemainder));
	assert!(book_app::list_open_orders(&h.book, buyer, Some(&service)).await.unwrap().is_empty());
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	// 8 was held, 5 spent, 3 released.
	assert_eq!(cash_of(&h, buyer), usdt("95"));
	assert_eq!(hold_of(&h, &bid).state, HoldState::Voided);

	// An IOC against an empty side fills nothing, and what it held comes back.
	let nothing = place(&h, buyer, &service, Side::Buy, OrderKind::Limit, Tif::Ioc, Some("1"), "1").await.unwrap();
	assert_eq!((nothing.order.state(), nothing.order.filled()), (OrderState::Cancelled, Shares::ZERO));
	assert_eq!(nothing.order.cancel_reason(), Some(CancelReason::IocRemainder));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(cash_of(&h, buyer), usdt("95"));
}

#[tokio::test]
async fn a_post_only_order_rests_or_is_refused_and_a_self_trade_is_refused() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "100");
	fund_user(&h, seller, "100");
	sell_escrowed(&h, seller, &service, "1", "10").await;

	// Post-only under the ask rests; at the ask it would take and is refused outright.
	let posted = place(&h, buyer, &service, Side::Buy, OrderKind::Limit, Tif::Alo, Some("0.99"), "1").await.unwrap();
	assert_eq!(posted.order.state(), OrderState::Open);
	let err = place(&h, buyer, &service, Side::Buy, OrderKind::Limit, Tif::Alo, Some("1"), "1").await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref m) if m.contains("post-only")), "got {err:?}");

	// The seller bidding into their own ask is refused, whatever the time in force.
	let err = buy_result(&h, seller, &service, "1", "1").await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref m) if m.contains("own")), "got {err:?}");
	// Nothing was recorded for either refusal.
	assert_eq!(book_app::list_order_history(&h.book, buyer, Some(&service), 10).await.unwrap().len(), 1);
	assert_eq!(book_app::list_order_history(&h.book, seller, Some(&service), 10).await.unwrap().len(), 1);
}

async fn buy_result(h: &Harness, user: UserId, service: &ServiceId, at: &str, size: &str) -> Result<OrderRecord, DomainError> {
	place(h, user, service, Side::Buy, OrderKind::Limit, Tif::Gtc, Some(at), size).await
}

#[tokio::test]
async fn a_market_order_is_priced_off_the_best_quote_and_refused_on_an_empty_side() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "100");

	// No asks at all: nothing to price a market buy from.
	let err = place(&h, buyer, &service, Side::Buy, OrderKind::Market, Tif::Ioc, None, "1").await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref m) if m.contains("liquidity")), "got {err:?}");

	sell(&h, seller, &service, "1", "5").await;
	sell(&h, seller, &service, "1.04", "5").await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	// Best ask 1.00, 5 % slippage → limit 1.05, so both asks are reachable: 5 at 1.00,
	// then 3 at 1.04, and the derived limit is what the row records.
	let market = place(&h, buyer, &service, Side::Buy, OrderKind::Market, Tif::Ioc, None, "8").await.unwrap();
	assert_eq!(market.order.price(), price("1.05"));
	assert_eq!((market.order.state(), market.order.filled()), (OrderState::Filled, shares("8")));
	assert_eq!(market.order.notional_filled(), usdt("8.12"));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(cash_of(&h, buyer), usdt("91.88"));
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), buyer)).await, shares("8"));

	// Only 2 rest at 1.04 now. A market buy for 5 is priced 1.04 + 5 % → 1.10 (ceiled to
	// the tick), takes the 2 and ends cancelled for the 3 it could not reach — with the
	// reason saying so, not a user's cancel.
	let leftover = place(&h, buyer, &service, Side::Buy, OrderKind::Market, Tif::Ioc, None, "5").await.unwrap();
	assert_eq!(leftover.order.price(), price("1.1"));
	assert_eq!((leftover.order.state(), leftover.order.filled()), (OrderState::Cancelled, shares("2")));
	assert_eq!(leftover.order.cancel_reason(), Some(CancelReason::MarketRemainder));
	assert_eq!(order(&h, &leftover).await.order.cancel_reason(), Some(CancelReason::MarketRemainder));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(cash_of(&h, buyer), usdt("89.8"), "2 × 1.04 spent, the rest of the hold back");
	assert_eq!(hold_of(&h, &leftover).state, HoldState::Voided);

	// A market order names no price and is never good-till-cancelled.
	assert!(place(&h, buyer, &service, Side::Buy, OrderKind::Market, Tif::Ioc, Some("1"), "1").await.is_err());
	assert!(place(&h, buyer, &service, Side::Buy, OrderKind::Market, Tif::Gtc, None, "1").await.is_err());
}

#[tokio::test]
async fn cancelling_returns_the_escrow_and_is_idempotent() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let seller = provisioned_user(&h).await;
	let stranger = provisioned_user(&h).await;
	issue_units(&h, &service, seller, "10").await;
	let ask = sell(&h, seller, &service, "2", "10").await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_of(&h, LedgerAccountKey::BookShares(service.clone(), seller)).await, shares("10"));

	// Not the stranger's to cancel.
	let err = book_app::cancel_order(&h.book, &h.notify, &h.feed, ask.order.id(), stranger).await.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(_)), "got {err:?}");

	let cancelled = book_app::cancel_order(&h.book, &h.notify, &h.feed, ask.order.id(), seller).await.unwrap();
	assert_eq!(cancelled.order.state(), OrderState::Cancelled);
	assert_eq!(cancelled.order.cancel_reason(), Some(CancelReason::User));
	assert_eq!(order(&h, &ask).await.order.cancel_reason(), Some(CancelReason::User));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), seller)).await, shares("10"), "every unit came back");
	assert_eq!(units_of(&h, LedgerAccountKey::BookShares(service.clone(), seller)).await, Shares::ZERO);
	assert_eq!(position(&h, seller, &service).await.units_in_orders, Shares::ZERO);

	// Again: the documented no-op, and nothing new for the relay.
	let again = book_app::cancel_order(&h.book, &h.notify, &h.feed, ask.order.id(), seller).await.unwrap();
	assert_eq!(again.order.state(), OrderState::Cancelled);
	assert_eq!(again.order.cancel_reason(), Some(CancelReason::User));
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), seller)).await, shares("10"));
	assert_eq!(parked(&h, ask.order.id().raw()).await, Vec::<String>::new());
}

/// A resting buy holds its worst-case cash in banking — notional at the limit PLUS the
/// taker fee — and a cancel voids the hold; a resting sell's units stay the holder's.
#[tokio::test]
async fn a_resting_buy_holds_its_cash_in_banking_and_a_cancel_releases_it() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 100).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "100");

	let bid = buy(&h, buyer, &service, "1.2", "10").await;
	assert_eq!((bid.order.state(), bid.order.reserved()), (OrderState::Open, Locked::Cash(usdt("12.12"))));
	let hold = hold_of(&h, &bid);
	assert_eq!((hold.state, hold.amount, hold.remaining), (HoldState::Active, usdt("12.12"), usdt("12.12")));
	assert_eq!(cash_of(&h, buyer), usdt("87.88"), "exactly the reserve is held");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(cash_of(&h, buyer), usdt("87.88"), "a buy moves nothing on invest's ledger");

	// The seller's side: units in a resting sell are still theirs.
	let ask = sell(&h, seller, &service, "1.5", "4").await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), seller)).await, shares("6"));
	let resting = position(&h, seller, &service).await;
	assert_eq!((resting.units, resting.units_in_orders, resting.value), (shares("6"), shares("4"), usdt("10")));

	book_app::cancel_order(&h.book, &h.notify, &h.feed, bid.order.id(), buyer).await.unwrap();
	book_app::cancel_order(&h.book, &h.notify, &h.feed, ask.order.id(), seller).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(cash_of(&h, buyer), usdt("100"), "the hold was voided whole");
	assert_eq!(hold_of(&h, &bid).state, HoldState::Voided);
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), seller)).await, shares("10"));
}

/// banking's refusal of the buy's hold is the caller's answer, and nothing is written: no
/// order rests without cash behind it.
#[tokio::test]
async fn a_buy_banking_will_not_hold_is_refused_and_never_recorded() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let buyer = provisioned_user(&h).await;
	fund_user(&h, buyer, "100");

	h.money.refuse("Reserve", Refusal::MandateRequired);
	let err = place(&h, buyer, &service, Side::Buy, OrderKind::Limit, Tif::Gtc, Some("1"), "1").await.unwrap_err();
	h.money.serve("Reserve");
	assert!(matches!(&err, DomainError::Precondition(reason) if reason == "mandate_required"), "got {err:?}");
	assert!(book_app::list_order_history(&h.book, buyer, None, 10).await.unwrap().is_empty());
	assert_eq!(cash_of(&h, buyer), usdt("100"));
}

/// A buy rejected by the matching rule after banking held its cash — a self-trade — gives
/// the hold back at once: the order never stood, so nothing else will.
#[tokio::test]
async fn a_rejected_buy_releases_the_hold_it_took() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let trader = provisioned_user(&h).await;
	issue_units(&h, &service, trader, "10").await;
	fund_user(&h, trader, "100");
	sell_escrowed(&h, trader, &service, "1", "10").await;

	let key = format!("self-{}", Uuid::new_v4());
	let err = place_keyed(&h, trader, &service, Side::Buy, OrderKind::Limit, Tif::Gtc, Some("1"), "1", &key).await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref m) if m.contains("own")), "got {err:?}");
	assert_eq!(cash_of(&h, trader), usdt("100"), "the hold was voided");
	// The same client order id names the same (now released) hold: a retry is told to use
	// a new id rather than placing an order with no cash behind it.
	let err = place_keyed(&h, trader, &service, Side::Buy, OrderKind::Limit, Tif::Gtc, Some("1"), "1", &key).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
}

/// The fill's cash leg banking refuses (the buyer's hold gone — a revoked mandate): the
/// pending units are voided, so nobody gets units nobody paid for, and the trade is parked
/// for an operator with banking's reason.
#[tokio::test]
async fn a_fill_banking_refuses_to_pay_voids_the_units_and_parks() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "100");
	sell(&h, seller, &service, "1", "10").await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// The bid is for 20: 10 fill against the ask, 10 rest on the book behind the same hold.
	let bid = buy(&h, buyer, &service, "1", "20").await;
	h.money.refuse("Capture", Refusal::HoldNotActive);
	// The park IS what this test pins, so one pass instead of `drain_to_quiescence`.
	h.relay.drain().await;
	h.money.serve("Capture");

	let trade = book_app::list_trades(&h.book, &service, 1).await.unwrap().remove(0);
	let reasons = parked(&h, trade.trade.id.raw()).await;
	assert_eq!(reasons.len(), 1, "the trade is parked: {reasons:?}");
	assert!(reasons[0].contains("hold_not_active"), "with banking's reason: {reasons:?}");
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), buyer)).await, Shares::ZERO, "no units delivered");
	let buyer_pending = h.ledger.balance(&LedgerAccountKey::UserShares(service.clone(), buyer)).await.unwrap().pending;
	assert_eq!(buyer_pending, 0, "and none left pending");
	assert_eq!(cash_of(&h, seller), Usdt::ZERO, "the seller was not paid");
	// The trade is busted, not left half-done: the seller's units come back out of the
	// escrow, the rest of the buy comes off the book and its hold is released.
	assert_eq!(
		units_of(&h, LedgerAccountKey::UserShares(service.clone(), seller)).await,
		shares("10"),
		"the seller's units are theirs again"
	);
	assert_eq!(units_of(&h, LedgerAccountKey::BookShares(service.clone(), seller)).await, Shares::ZERO, "nothing stuck in escrow");
	assert_eq!(busted_reason(&h, trade.trade.id.raw()).await.as_deref().map(|r| r.contains("hold_not_active")), Some(true));
	assert_eq!(order(&h, &bid).await.order.state(), OrderState::Rejected, "no more fills against a hold that is gone");
	assert_eq!(hold_of(&h, &bid).state, HoldState::Voided);
	assert_eq!(cash_of(&h, buyer), usdt("100"), "the buyer's cash is free again");
}

/// Why a trade was busted, if it was.
async fn busted_reason(h: &Harness, trade: Uuid) -> Option<String> {
	sqlx::query_scalar::<_, Option<String>>("SELECT bust_reason FROM book_trades WHERE id = $1")
		.bind(trade)
		.fetch_one(&h.pool)
		.await
		.unwrap()
}

/// A fill that parks before any cash moved — here the seller's escrow never landed — must
/// not leave the buyer's cash held behind an order that is done.
#[tokio::test]
async fn a_fill_that_parks_releases_the_buyers_hold() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "100");
	let bid = buy(&h, buyer, &service, "1", "10").await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// The seller takes the bid; before the relay escrows their units, they leave by
	// another road, so the escrow and the fill both park.
	sell(&h, seller, &service, "1", "10").await;
	h.ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::SharesOutstanding(service.clone()),
			credit: LedgerAccountKey::UserShares(service.clone(), seller),
			amount: shares("10").base_units(),
			code: TransferCode::ShareBurn,
			reference: 0,
		})
		.await
		.unwrap();
	h.relay.drain().await;

	assert_eq!(hold_of(&h, &bid).state, HoldState::Voided, "the buyer's hold is released");
	assert_eq!(cash_of(&h, buyer), usdt("100"));
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(service.clone(), buyer)).await, Shares::ZERO);
}

/// The matcher fills only orders whose escrow is in place and whose owner may deal: a sell
/// the relay has not escrowed yet, and any order of a suspended holder, stay out of it.
#[tokio::test]
async fn the_matcher_skips_unescrowed_sells_and_suspended_holders() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "100");

	// Not yet escrowed: the relay has not run.
	let ask = sell(&h, seller, &service, "1", "5").await;
	let early = buy(&h, buyer, &service, "1", "5").await;
	assert_eq!(early.order.state(), OrderState::Open, "nothing to fill against yet");
	book_app::cancel_order(&h.book, &h.notify, &h.feed, early.order.id(), buyer).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// Escrowed, but its owner is suspended.
	let subject = common::auth_subject(seller);
	h.users.record(seller, &UserStanding { active: false, permissions: None }, Some(&subject)).await.unwrap();
	let blocked = buy(&h, buyer, &service, "1", "5").await;
	assert_eq!(blocked.order.state(), OrderState::Open, "a suspended holder's ask is not filled");
	book_app::cancel_order(&h.book, &h.notify, &h.feed, blocked.order.id(), buyer).await.unwrap();
	h.users.record(seller, &UserStanding { active: true, permissions: None }, Some(&subject)).await.unwrap();

	let filled = buy(&h, buyer, &service, "1", "5").await;
	assert_eq!(filled.order.state(), OrderState::Filled, "escrowed and active: it fills");
	assert_eq!(order(&h, &ask).await.order.state(), OrderState::Filled);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

#[tokio::test]
async fn the_gates_a_closed_book_view_access_hidden_and_suspension() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let seller = provisioned_user(&h).await;
	issue_units(&h, &service, seller, "10").await;

	// The operator closes the book: no new orders, whatever the allocation's state.
	let closed = BookPolicy::new(false, 0, BookPolicy::DEFAULT_PRICE_TICK, BookPolicy::DEFAULT_LOT_SIZE, 500, false).unwrap();
	book_app::set_policy(&h.allocations, &h.book, &service, closed).await.unwrap();
	let err = place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "1").await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref m) if m.contains("closed")), "got {err:?}");
	open_book(&h, &service, 0).await;

	// A product with no policy row at all has a closed book.
	let unset = tradable_product(&h, 0).await;
	sqlx::query("DELETE FROM book_policies WHERE service = $1").bind(unset.as_str()).execute(&h.pool).await.unwrap();
	assert!(!book_app::policy(&h.book, &unset).await.unwrap().policy.book_open());

	// Access: `view` may look but not trade; `hidden` cannot even find the book.
	h.allocations.set_access(&service, AllocationAccess::View).await.unwrap();
	let err = place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "1").await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref m) if m.contains("invest")), "got {err:?}");
	book_app::require_visible(&h.allocations, &service, seller, false).await.unwrap();
	h.allocations.set_access(&service, AllocationAccess::Hidden).await.unwrap();
	assert!(matches!(
		place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "1").await.unwrap_err(),
		DomainError::NotFound { entity: "allocation", .. }
	));
	assert!(matches!(
		book_app::require_visible(&h.allocations, &service, seller, false).await.unwrap_err(),
		DomainError::NotFound { entity: "allocation", .. }
	));
	book_app::require_visible(&h.allocations, &service, seller, true).await.expect("a manager still sees it");
	// A per-user grant lets the holder back in.
	h.allocations.grant_access(&service, seller, AllocationAccess::Invest, seller).await.unwrap();
	sell(&h, seller, &service, "1", "1").await;

	// The allocation's lifecycle is not a gate: holders of a closed product still trade.
	h.allocations.close(&service).await.unwrap();
	sell(&h, seller, &service, "1", "1").await;

	// A suspended owner may not place.
	let subject = common::auth_subject(seller);
	h.users.record(seller, &UserStanding { active: false, permissions: None }, Some(&subject)).await.unwrap();
	let refused = place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "1").await;
	h.users.record(seller, &UserStanding { active: true, permissions: None }, Some(&subject)).await.unwrap();
	assert!(matches!(refused, Err(DomainError::Forbidden(_))), "got {refused:?}");
}

#[tokio::test]
async fn the_grid_and_the_balance_are_checked_before_anything_is_written() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "10");

	assert!(
		place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1.005"), "1").await.is_err(),
		"off the 0.01 tick"
	);
	assert!(
		place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "0.00005").await.is_err(),
		"off the 0.0001 lot"
	);
	assert!(
		place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, None, "1").await.is_err(),
		"a limit order needs a price"
	);
	let err = place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "11").await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref m) if m.contains("units")), "more units than held: {err:?}");
	// More cash than the compartment holds is banking's refusal of the hold.
	let err = place(&h, buyer, &service, Side::Buy, OrderKind::Limit, Tif::Gtc, Some("1"), "11").await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref m) if m == "insufficient_funds"), "more cash than held: {err:?}");
	assert_eq!(cash_of(&h, buyer), usdt("10"));
	assert!(book_app::list_order_history(&h.book, seller, None, 10).await.unwrap().is_empty());
	assert!(book_app::list_order_history(&h.book, buyer, None, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_client_order_id_makes_a_retry_land_once_and_a_reuse_a_conflict() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let seller = provisioned_user(&h).await;
	issue_units(&h, &service, seller, "10").await;

	let key = format!("retry-{}", Uuid::new_v4());
	let first = place_keyed(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "3", &key).await.unwrap();
	let again = place_keyed(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "3", &key).await.unwrap();
	assert_eq!(again.order.id(), first.order.id(), "the retry is the same order");
	assert_eq!(book_app::list_open_orders(&h.book, seller, Some(&service)).await.unwrap().len(), 1);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_of(&h, LedgerAccountKey::BookShares(service.clone(), seller)).await, shares("3"), "escrowed once");

	let err = place_keyed(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "4", &key).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "the same key for a different order: {err:?}");
	let err = place_keyed(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("2"), "3", &key).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "the same key at another price is another order: {err:?}");
}

#[tokio::test]
async fn a_raced_over_lock_is_refused_by_the_ledger_and_the_order_marked_rejected() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let seller = provisioned_user(&h).await;
	issue_units(&h, &service, seller, "10").await;

	// Two sells of the whole holding before the relay moves the first: both pass the
	// optimistic Read-First (TigerBeetle still shows 10 free), both are recorded.
	let first = sell(&h, seller, &service, "1", "10").await;
	let second = sell(&h, seller, &service, "1", "10").await;
	assert_eq!(book_app::list_open_orders(&h.book, seller, Some(&service)).await.unwrap().len(), 2);

	// The holding's non-negative flag refuses the second escrow; the relay parks the
	// fact and takes the order off the book so nothing can ever fill against it.
	// The park IS what this test pins, so the one-pass drain stays here: `drain_to_quiescence`
	// reports a park raised during the call as a failure.
	h.relay.drain().await;
	assert_eq!(
		units_of(&h, LedgerAccountKey::BookShares(service.clone(), seller)).await,
		shares("10"),
		"exactly one lock applied"
	);
	assert_eq!(order(&h, &first).await.order.state(), OrderState::Open);
	let rejected = order(&h, &second).await.order;
	assert_eq!(rejected.state(), OrderState::Rejected);
	assert!(rejected.reject_reason().is_some_and(|reason| reason.contains("escrow")), "got {:?}", rejected.reject_reason());
	assert_eq!(parked(&h, second.order.id().raw()).await.len(), 1, "the lock is parked for reconciliation");
	assert!(
		book_app::cancel_order(&h.book, &h.notify, &h.feed, second.order.id(), seller).await.is_err(),
		"nothing to release"
	);
	assert_eq!(book_app::list_open_orders(&h.book, seller, Some(&service)).await.unwrap().len(), 1);
}

#[tokio::test]
async fn candles_aggregate_the_tape_by_bucket() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "100");
	// Three fills at 1.00, 1.10 and 0.90, in that order, all inside one minute.
	sell_escrowed(&h, seller, &service, "1", "1").await;
	buy(&h, buyer, &service, "1", "1").await;
	sell_escrowed(&h, seller, &service, "1.1", "2").await;
	buy(&h, buyer, &service, "1.1", "2").await;
	sell_escrowed(&h, seller, &service, "0.9", "3").await;
	buy(&h, buyer, &service, "0.9", "3").await;

	let now = now_unix();
	let candles = book_app::candles(&h.book, &service, CandleResolution::M1, now - 600, now + 60).await.unwrap();
	// One or two buckets (a minute boundary may fall between the fills); the aggregate
	// over them is what a chart would draw.
	assert!(!candles.is_empty() && candles.len() <= 2, "got {candles:?}");
	assert_eq!(candles.first().unwrap().open, price("1"));
	assert_eq!(candles.last().unwrap().close, price("0.9"));
	assert_eq!(candles.iter().map(|c| c.high).max(), Some(price("1.1")));
	assert_eq!(candles.iter().map(|c| c.low).min(), Some(price("0.9")));
	let volume = candles.iter().fold(Shares::ZERO, |acc, c| acc.checked_add(c.volume).unwrap());
	assert_eq!(volume, shares("6"));
	assert!(candles.windows(2).all(|pair| pair[0].time < pair[1].time), "oldest first");
	// A window wider than the bucket bound is refused rather than aggregated unbounded.
	assert!(book_app::candles(&h.book, &service, CandleResolution::M1, 0, now).await.is_err());
	assert!(book_app::candles(&h.book, &service, CandleResolution::M1, now, now).await.is_err());
	// The snapshot's 24h change measures the last fill against the first.
	let snapshot = snapshot(&h, &service, now).await;
	assert_eq!(snapshot.change_24h_pct.as_deref(), Some("-10.00"));
	assert_eq!(snapshot.volume_24h, shares("6"));
	// Settled here, against this test's money ledger: the outbox outlives the test, and a
	// sibling draining these fills would ask its own ledger for holds it never placed.
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

#[tokio::test]
async fn the_feed_frames_on_every_change_and_reports_where_the_callers_orders_moved() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let (seller, buyer) = (provisioned_user(&h).await, provisioned_user(&h).await);
	issue_units(&h, &service, seller, "10").await;
	fund_user(&h, buyer, "100");
	let mut receiver = h.feed.subscribe(&service);
	receiver.changed().await.unwrap();
	receiver.borrow_and_update();

	sell_escrowed(&h, seller, &service, "1", "10").await;
	receiver.changed().await.expect("a placement frames");
	let after_ask = *receiver.borrow_and_update();
	let frame = watch_frame(&h, &service, buyer).await;
	assert_eq!(frame.snapshot.revision, after_ask);
	assert_eq!(frame.orders_revision, 0, "the buyer has no orders yet");
	assert_eq!(frame.snapshot.asks.len(), 1);
	assert!(frame.trades.is_empty());

	buy(&h, buyer, &service, "1", "4").await;
	receiver.changed().await.expect("a fill frames");
	let after_fill = *receiver.borrow_and_update();
	assert!(after_fill > after_ask);
	let frame = watch_frame(&h, &service, buyer).await;
	assert_eq!(frame.orders_revision, after_fill, "the buyer's order moved at this revision");
	assert_eq!(frame.trades.len(), 1);
	let sellers = watch_frame(&h, &service, seller).await;
	assert_eq!(sellers.orders_revision, after_fill, "so did the maker's");
	assert_eq!(sellers.snapshot.mid, None, "one-sided book: no mid");
	assert!(!receiver.has_changed().unwrap(), "nothing more until the next change");
}

#[tokio::test]
async fn the_policy_is_operator_set_and_bounded() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 25).await;
	let record = book_app::policy(&h.book, &service).await.unwrap();
	assert!(record.policy.book_open());
	assert_eq!(record.policy.taker_fee_bps(), 25);
	assert!(record.updated_at > 0);
	// A policy for a product nobody registered is refused, not silently stored.
	let ghost = unique_service();
	let err = book_app::set_policy(&h.allocations, &h.book, &ghost, BookPolicy::default()).await.unwrap_err();
	assert!(matches!(err, DomainError::NotFound { entity: "allocation", .. }), "got {err:?}");
	// The grid can be widened, and the defaults answer for a product with no row.
	let coarse = BookPolicy::new(true, 0, price("0.1"), shares("1"), 1000, false).unwrap();
	let stored = book_app::set_policy(&h.allocations, &h.book, &service, coarse).await.unwrap();
	assert_eq!(
		(stored.policy.price_tick(), stored.policy.lot_size(), stored.policy.market_slippage_bps()),
		(price("0.1"), shares("1"), 1000)
	);
	let fresh = unique_service();
	allocations_app::register(&h.allocations, fresh.clone(), &ScopeId::invest(), "Fresh", "", AllocationIcon::default())
		.await
		.unwrap();
	let defaults = book_app::policy(&h.book, &fresh).await.unwrap();
	assert_eq!(defaults.policy, BookPolicy::default());
	assert_eq!(defaults.updated_at, 0);
	assert!(book_app::list_trades(&h.book, &fresh, 10).await.unwrap().is_empty());
	let snapshot = snapshot(&h, &fresh, now_unix()).await;
	assert_eq!(snapshot.revision, 0);
	assert_eq!(snapshot.last, None);
}

/// Opening the book on a product whose units are held in kind is refused until the
/// operator acknowledges unbacked trading — and the refusal writes nothing, so the terms in
/// force stay what they were. Closing needs no acknowledgement.
#[tokio::test]
async fn an_in_kind_book_does_not_open_without_the_acknowledgement() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let seller = provisioned_user(&h).await;
	issue_units(&h, &service, seller, "10").await;
	assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().backing(), AllocationBacking::InKind);
	let before = book_app::policy(&h.book, &service).await.unwrap();

	let unacknowledged = BookPolicy::new(true, 25, BookPolicy::DEFAULT_PRICE_TICK, BookPolicy::DEFAULT_LOT_SIZE, 500, false).unwrap();
	let err = book_app::set_policy(&h.allocations, &h.book, &service, unacknowledged).await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref m) if m.contains("allow_unbacked_trading")), "got {err:?}");
	let after = book_app::policy(&h.book, &service).await.unwrap();
	assert_eq!((after.policy, after.updated_at), (before.policy, before.updated_at), "a refused policy is not written");

	let closed = BookPolicy::new(false, 25, BookPolicy::DEFAULT_PRICE_TICK, BookPolicy::DEFAULT_LOT_SIZE, 500, false).unwrap();
	let stored = book_app::set_policy(&h.allocations, &h.book, &service, closed).await.expect("closing needs no acknowledgement");
	assert!(!stored.policy.book_open());
	assert!(!stored.policy.allow_unbacked_trading());
}

/// With the acknowledgement the book opens, and the flag comes back on the policy for the
/// terminal to show its notice.
#[tokio::test]
async fn an_in_kind_book_opens_once_unbacked_trading_is_acknowledged() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let seller = provisioned_user(&h).await;
	issue_units(&h, &service, seller, "10").await;

	let acknowledged = BookPolicy::new(true, 25, BookPolicy::DEFAULT_PRICE_TICK, BookPolicy::DEFAULT_LOT_SIZE, 500, true).unwrap();
	let stored = book_app::set_policy(&h.allocations, &h.book, &service, acknowledged).await.unwrap();
	assert!(stored.policy.book_open());
	assert!(stored.policy.allow_unbacked_trading());
	assert_eq!(stored.policy.taker_fee_bps(), 25);
	let read = book_app::policy(&h.book, &service).await.unwrap();
	assert!(read.policy.allow_unbacked_trading(), "the flag round-trips through the store");
	sell(&h, seller, &service, "1", "1").await;
}

/// The gate runs on every order, not only when the book opens: a book opened on a `cash`
/// product without the acknowledgement stops taking orders the moment the first in-kind
/// mint flips the product to `in_kind`, and resumes once the operator acknowledges.
#[tokio::test]
async fn a_book_opened_on_cash_stops_when_the_units_turn_in_kind_until_acknowledged() {
	let Some(h) = harness().await else { return };
	let service = listed_product(&h).await;
	let cash_only = BookPolicy::new(true, 0, BookPolicy::DEFAULT_PRICE_TICK, BookPolicy::DEFAULT_LOT_SIZE, 500, false).unwrap();
	book_app::set_policy(&h.allocations, &h.book, &service, cash_only)
		.await
		.expect("a cash product's book opens unacknowledged");

	let seller = provisioned_user(&h).await;
	issue_units(&h, &service, seller, "10").await;
	assert_eq!(h.allocations.find(&service).await.unwrap().unwrap().backing(), AllocationBacking::InKind);
	let err = place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "1").await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref m) if m.contains("allow_unbacked_trading")), "got {err:?}");
	assert!(book_app::list_open_orders(&h.book, seller, Some(&service)).await.unwrap().is_empty(), "nothing was recorded");

	open_book(&h, &service, 0).await;
	sell(&h, seller, &service, "1", "1").await;
	assert_eq!(book_app::list_open_orders(&h.book, seller, Some(&service)).await.unwrap().len(), 1);
}

/// banking held the buy's cash, then could not be asked about the hold: the order is not
/// written, and the hold it took is given back rather than left held behind nothing.
#[tokio::test]
async fn a_buy_that_fails_after_its_hold_releases_the_hold() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let buyer = provisioned_user(&h).await;
	fund_user(&h, buyer, "100");

	h.money.outage("GetHold", 1);
	let err = place(&h, buyer, &service, Side::Buy, OrderKind::Limit, Tif::Gtc, Some("1"), "10").await.unwrap_err();
	assert!(matches!(err, DomainError::Repository(_)), "{err:?}");
	assert!(book_app::list_order_history(&h.book, buyer, None, 10).await.unwrap().is_empty(), "nothing written");
	assert_eq!(cash_of(&h, buyer), usdt("100"), "the hold was released");
}

/// A seller banking cannot name has nowhere to be paid: the sell is refused at the door,
/// not matched and busted later.
#[tokio::test]
async fn a_seller_banking_cannot_name_may_not_sell() {
	let Some(h) = harness().await else { return };
	let service = tradable_product(&h, 0).await;
	let seller = provisioned_user(&h).await;
	issue_units(&h, &service, seller, "10").await;
	sqlx::query("UPDATE users SET auth_subject = NULL WHERE id = $1")
		.bind(seller.raw())
		.execute(&h.pool)
		.await
		.unwrap();

	let err = place(&h, seller, &service, Side::Sell, OrderKind::Limit, Tif::Gtc, Some("1"), "1").await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref m) if m.contains("money ledger")), "{err:?}");
	assert!(book_app::list_order_history(&h.book, seller, None, 10).await.unwrap().is_empty());
}
