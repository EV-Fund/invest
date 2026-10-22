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
//! The fee chain itself runs here too: an invest product's fee class is swept, at
//! settlement, out of `(invest, alloc:<product>)` into `(invest, fee)`, which banking
//! forwards to `(invest, alloc:fee)` — the in-memory ledger plays that route. A product in
//! another scope (`sa`) charges the `fee` holders nothing: its fees are banking's (D24(3)).
//!
//! Moved from banking 9ffafce3 (`piggybank/core/tests/ownership_fee.rs`). The
//! withdrawal-fee test stays in banking: a withdrawal and its retained fee are banking's
//! own, and land in banking's treasury, never in `fee` (D17).

use invest_contracts::invest::v1::{self as pb, allocations_service_server::AllocationsService};
use invest_core::{
	application::{allocations as allocations_app, fees as fee_app, funds as funds_app, issuance as issuance_app, treasury::DEFAULT_TOP_HOLDERS},
	infrastructure::money::Unconfigured,
	ports::{
		RedemptionRepository,
		fees::{FeePolicyChanges, FeeSettlements, PositionAccruals},
		money::{AccountLabel, ScopeId},
		nav::NavMarks,
	},
	services::allocations::AllocationsSvc,
};
use invest_domain::{
	allocations::{AllocationAccess, AllocationIcon},
	balance::{LedgerAccountKey, ServiceId, ValuationId},
	error::DomainError,
	fees::FeePolicy,
	issuance::UnitHolder,
	money::{Nav, Shares, Usdt},
	redemptions::RedemptionState,
	users::UserId,
};
use uuid::Uuid;

mod common;

const SUITE: &str = "ownership fee test";

type Harness = common::Engine;

async fn harness() -> Option<Harness> {
	common::engine(SUITE).await
}

fn fund_ports(h: &Harness) -> funds_app::FundPorts<'_> {
	h.fund_ports()
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
	funds_app::nav_of(&h.nav, h.ledger.as_ref(), h.money.as_ref(), &ServiceId::fee()).await.unwrap().nav
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
	let request = common::mint_request(&ServiceId::fee(), UnitHolder::User(holder), units, None, &format!("grant-{}", Uuid::new_v4()));
	issuance_app::grant_units(&fund_ports(h), &h.issuances, &h.users, request, now_unix()).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

const YEAR: i64 = 365 * 24 * 60 * 60;

fn fee_account() -> AccountLabel {
	AccountLabel::allocation(&ServiceId::fee()).unwrap()
}

/// The `fee` allocation's cash: its account in invest's scope.
fn fee_cash(h: &Harness) -> Usdt {
	h.money.account(&ScopeId::invest(), &fee_account())
}

/// A product's cash: its account in its own scope.
fn product_cash(h: &Harness, scope: &ScopeId, service: &ServiceId) -> Usdt {
	h.money.account(scope, &AccountLabel::allocation(service).unwrap())
}

/// A registered, open product in `scope` admitting every investor, with the house 2-and-20
/// terms when it is in invest's scope — a product elsewhere takes no fee terms.
async fn open_fund(h: &Harness, scope: &ScopeId, service: &ServiceId) {
	allocations_app::register(&h.allocations, service.clone(), scope, "EV Trading", "Systematic crypto", AllocationIcon::default())
		.await
		.unwrap();
	allocations_app::open(&h.allocations, h.money.as_ref(), service).await.unwrap();
	allocations_app::set_access(&h.allocations, service, AllocationAccess::Invest).await.unwrap();
	if !scope.is_invest() {
		return;
	}
	let request = fee_app::PolicyChangeRequest {
		service: service.clone(),
		policy: FeePolicy::HOUSE,
		requested_effective_from_unix: 0,
		reason: String::new(),
		requester_proof: None,
	};
	let change = fee_app::schedule_policy(&fee_ports(h), UserId::new(), request, now_unix()).await.unwrap();
	assert!(h.changes.promote(change.id, now_unix()).await.unwrap(), "a fund with no holders takes new terms at once");
}

fn fee_ports(h: &Harness) -> fee_app::FeePolicyPorts<'_> {
	h.fee_policy_ports()
}

fn unique_service() -> ServiceId {
	ServiceId::parse(&format!("own-{}", Uuid::new_v4())).unwrap()
}

/// An investor with `amount` in their compartment of `scope`, subscribed with all of it —
/// and waited for until the position projection (where the accrual clocks live) landed.
async fn invest(h: &Harness, scope: &ScopeId, service: &ServiceId, amount: &str) -> UserId {
	let investor = person(h).await;
	h.money.fund(scope, &common::auth_subject(investor), usdt(amount));
	funds_app::subscribe(&fund_ports(h), &h.subscriptions, &h.users, investor, service.clone(), usdt(amount), now_unix())
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let accrual = h.accruals.find(investor, service).await.unwrap();
	assert!(accrual.is_some_and(|accrual| !accrual.cost_basis.is_zero()), "the subscription's position projection landed");
	investor
}

/// A year of 2 % on `investor`'s holding, charged into the product's fee class.
async fn charge_a_year(h: &Harness, investor: UserId, service: &ServiceId) -> Shares {
	sqlx::query(
		"UPDATE fund_positions SET fees_accrued_at = now() - make_interval(secs => $3), \
		 crystallized_at = now() - make_interval(secs => $3) WHERE user_id = $1 AND service = $2",
	)
	.bind(investor.raw())
	.bind(service.as_str())
	.bind(YEAR as f64)
	.execute(&h.pool)
	.await
	.unwrap();
	let assessment = common::assess_period(h, investor, service, now_unix()).await.unwrap().expect("a year owes a fee");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assessment.charge().charged_units
}

async fn settle_fee_shares(h: &Harness, service: &ServiceId, settler: UserId) -> Result<invest_domain::fees::FeeSettlement, DomainError> {
	let settlement = common::settle_fee_class(h, service, None, &common::fresh_key(), settler).await?;
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	Ok(settlement)
}

/// Units worth no less than `cash` at `price`: `Shares::from_cash` floors, so one more base
/// unit closes the gap whatever the price is.
fn units_worth_at_least(cash: Usdt, price: Nav) -> Shares {
	Shares::from_base_units(Shares::from_cash(cash, price).unwrap().base_units() + 1)
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
/// validation, before anything is written — and it charges no fee of its own.
#[tokio::test]
async fn a_reserved_allocation_takes_no_mark_and_no_fee_policy() {
	let Some(h) = harness().await else { return };
	for reserved in [ServiceId::fee(), ServiceId::fund()] {
		let err = funds_app::post_fund_valuation(&h.allocations, &h.nav, h.ledger.as_ref(), reserved.clone(), usdt("100"), "itest", now_unix())
			.await
			.unwrap_err();
		assert!(matches!(err, DomainError::Validation(ref m) if m.contains("reserved")), "{reserved}: {err:?}");
		assert!(h.nav.current(&reserved).await.unwrap().is_none(), "{reserved}: no mark was written");

		let request = fee_app::PolicyChangeRequest {
			service: reserved.clone(),
			policy: FeePolicy::HOUSE,
			requested_effective_from_unix: 0,
			reason: String::new(),
			requester_proof: None,
		};
		let err = fee_app::schedule_policy(&fee_ports(&h), UserId::new(), request, now_unix()).await.unwrap_err();
		assert!(matches!(err, DomainError::Validation(ref m) if m.contains("reserved")), "{reserved}: {err:?}");
		assert!(h.changes.pending(&reserved).await.unwrap().is_none(), "{reserved}: no change was scheduled");
	}
	// With nothing outstanding the price is the seed NAV — the first units issued are
	// worth exactly what stands behind them, and nothing ever divides by zero.
	let fund = funds_app::nav_of(&h.nav, h.ledger.as_ref(), h.money.as_ref(), &ServiceId::fund()).await.unwrap();
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
	let before = funds_app::nav_of(&h.nav, h.ledger.as_ref(), h.money.as_ref(), &ServiceId::fee()).await.unwrap();

	fund_fee_claim(&h, "25").await;
	let after = funds_app::nav_of(&h.nav, h.ledger.as_ref(), h.money.as_ref(), &ServiceId::fee()).await.unwrap();
	let value_before = before.aum.expect("a reserved allocation's value is always a sum");
	let value_after = after.aum.expect("a reserved allocation's value is always a sum");
	assert_eq!(value_after, value_before.checked_add(usdt("25")).unwrap(), "the new cash and nothing else");
	assert_eq!(after.nav, Nav::from_aum(value_after, outstanding).unwrap(), "over the same supply");
	assert_eq!(units_of(&h, LedgerAccountKey::SharesOutstanding(ServiceId::fee())).await, outstanding, "no unit moved");
	assert!(after.nav > before.nav);
}

/// The product's fee class settles into the `fee` allocation's cash, and the people holding
/// `fee` units own it: their price is the fee class at the product's NAV while the units
/// are held, and the same value in cash once they are settled. Marking the product up
/// raises the fee holders' NAV before a single USDT has moved.
#[tokio::test]
async fn settled_fee_cash_lands_in_the_fee_allocation_and_its_holders_nav_rises() {
	let Some(h) = harness().await else { return };
	let scope = ScopeId::invest();
	let service = unique_service();
	open_fund(&h, &scope, &service).await;
	let investor = invest(&h, &scope, &service, "1000").await;
	let owner = person(&h).await;
	let fee_class = charge_a_year(&h, investor, &service).await;
	assert!(fee_class > Shares::ZERO, "the charge accumulated fee units");
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await, fee_class);

	let fee_units_before = units_of(&h, LedgerAccountKey::SharesOutstanding(ServiceId::fee())).await;
	let cash_before = fee_cash(&h);
	let value_before = funds_app::nav_of(&h.nav, h.ledger.as_ref(), h.money.as_ref(), &ServiceId::fee())
		.await
		.unwrap()
		.aum
		.expect("a computed value");
	assert!(
		value_before >= cash_before.checked_add(Nav::SEED.value(fee_class).unwrap()).unwrap(),
		"the fee class is counted at the seed NAV"
	);

	let granted = Shares::from_cash(value_before, Nav::SEED).unwrap();
	grant_fee_units(&h, owner, &granted.to_decimal_string()).await;
	let supply = fee_units_before.checked_add(granted).unwrap();
	assert_eq!(units_of(&h, LedgerAccountKey::SharesOutstanding(ServiceId::fee())).await, supply);
	let nav_at_grant = fee_nav(&h).await;
	assert_eq!(nav_at_grant, Nav::from_aum(value_before, supply).unwrap());

	// Mark the product up 2x (through the owners' writer, past the single-poster guard).
	let aum = product_cash(&h, &scope, &service);
	funds_app::record_valuation(&h.nav, h.ledger.as_ref(), ValuationId::new(), service.clone(), aum.checked_add(aum).unwrap(), "itest")
		.await
		.unwrap();
	let marked_up = fee_nav(&h).await;
	assert!(marked_up > nav_at_grant, "the fee holders' NAV rises with the product's mark: {nav_at_grant} -> {marked_up}");
	let owner_position = position_of(&h, owner, &ServiceId::fee()).await.expect("the grant projected a position");
	assert_eq!(owner_position.nav, marked_up, "the position card prices at the computed NAV");

	// Settle: the product buys its fee class back at today's NAV. The cash leaves the
	// product's account, lands on the scope's fee collector, and banking forwards it to
	// `(invest, alloc:fee)`; the holders' NAV is unchanged — the units left and their exact
	// value arrived.
	let product_before = product_cash(&h, &scope, &service);
	let settlement = settle_fee_shares(&h, &service, UserId::new()).await.unwrap();
	let cash = settlement.cash();
	assert_eq!(cash, Nav::parse_decimal("2").unwrap().value(fee_class).unwrap(), "priced at the day's NAV");
	assert_eq!(product_cash(&h, &scope, &service), product_before.checked_sub(cash).unwrap(), "out of the product's account");
	assert_eq!(h.money.collected(&scope), cash, "through the scope's fee collector");
	assert_eq!(fee_cash(&h), cash_before.checked_add(cash).unwrap(), "onto the fee allocation's account");
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await, Shares::ZERO);
	let settled = fee_nav(&h).await;
	assert!(
		settled.base_units().abs_diff(marked_up.base_units()) <= 1,
		"settling crystallizes value, it does not move it: {marked_up} -> {settled}"
	);
	assert_eq!(h.settlements.list_by_service(&service).await.unwrap().len(), 1);
}

/// D24(3): only invest's own fees reach the `fee` allocation. A product in another scope
/// (`sa`) is never assessed — no fee units move to the fee class its `fee` holders own, even
/// where a fee policy came in with the data — its terms are refused, and a settlement
/// burns nothing: the holders would lose units against cash that goes to banking's treasury.
#[tokio::test]
async fn a_product_in_another_scope_charges_the_fee_holders_nothing() {
	let Some(h) = harness().await else { return };
	let sa = ScopeId::parse("sa").unwrap();
	let service = unique_service();
	open_fund(&h, &sa, &service).await;

	let request = fee_app::PolicyChangeRequest {
		service: service.clone(),
		policy: FeePolicy::HOUSE,
		requested_effective_from_unix: 0,
		reason: String::new(),
		requester_proof: None,
	};
	let err = fee_app::schedule_policy(&fee_ports(&h), UserId::new(), request, now_unix()).await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref reason) if reason.contains("outside invest's scope")), "{err:?}");

	// Terms that came in with the data are never charged.
	sqlx::query(
		"INSERT INTO fee_policies (service, management_bps, performance_bps, hurdle_bps, basis, crystallization, updated_by) \
		 VALUES ($1, 200, 2000, 0, 'invested_capital', 'annual', 'data wave')",
	)
	.bind(service.as_str())
	.execute(&h.pool)
	.await
	.unwrap();
	let investor = invest(&h, &sa, &service, "1000").await;
	sqlx::query("UPDATE fund_positions SET fees_accrued_at = now() - make_interval(secs => $3), crystallized_at = now() - make_interval(secs => $3) WHERE user_id = $1 AND service = $2")
		.bind(investor.raw())
		.bind(service.as_str())
		.bind(YEAR as f64)
		.execute(&h.pool)
		.await
		.unwrap();
	let charged = common::assess_period(&h, investor, &service, now_unix()).await.unwrap();
	assert!(charged.is_none(), "a year on 1000 owes the fee holders nothing: {charged:?}");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await, Shares::ZERO, "no fee class");
	assert_eq!(
		units_of(&h, LedgerAccountKey::UserShares(service.clone(), investor)).await,
		shares("1000"),
		"the investor keeps every unit"
	);
	let disclosed = fee_app::accrued_fees(
		&h.allocations,
		&h.policies,
		&h.accruals,
		h.ledger.as_ref(),
		h.money.as_ref(),
		&h.nav,
		investor,
		&service,
		now_unix(),
	)
	.await
	.unwrap();
	assert!(disclosed.is_none(), "and is shown no fee to net off");
	let due = h.accruals.due(now_unix() + 1, 500).await.unwrap();
	assert!(due.iter().all(|accrual| accrual.service != service), "the sweeper's queue never lists it");

	// Nothing to settle, and a settlement is refused outright rather than burning anything.
	let err = settle_fee_shares(&h, &service, UserId::new()).await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref reason) if reason.contains("banking's treasury")), "{err:?}");
	assert_eq!(h.money.collected(&sa), Usdt::ZERO);
	assert_eq!(h.money.treasury(), Usdt::ZERO);
	assert_eq!(fee_cash(&h), Usdt::ZERO, "nothing reached the fee allocation");
}

/// banking refusing the sweep leaves the fee class whole: the burn comes after the sweep,
/// so nothing is crystallized that was not paid.
#[tokio::test]
async fn a_refused_fee_sweep_parks_the_settlement_and_burns_nothing() {
	let Some(h) = harness().await else { return };
	let scope = ScopeId::invest();
	let service = unique_service();
	open_fund(&h, &scope, &service).await;
	let investor = invest(&h, &scope, &service, "1000").await;
	let fee_class = charge_a_year(&h, investor, &service).await;
	let cash_before = fee_cash(&h);

	h.money.refuse("Transfer", invest_core::ports::money::Refusal::ScopeFrozen);
	let settlement = common::settle_fee_class(&h, &service, None, &common::fresh_key(), UserId::new()).await.unwrap();
	// The park IS what this test pins, so one pass instead of `drain_to_quiescence`, which
	// reports a park raised during the call as a failure.
	h.relay.drain().await;
	h.money.serve("Transfer");
	let parked: Option<String> = sqlx::query_scalar("SELECT last_error FROM outbox WHERE parked_at IS NOT NULL AND aggregate_id = $1")
		.bind(settlement.id().raw())
		.fetch_optional(&h.pool)
		.await
		.unwrap()
		.flatten();
	assert!(parked.is_some_and(|reason| reason.contains("scope_frozen")), "parked for an operator with banking's reason");
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await, fee_class, "the fee class is unburned");
	assert_eq!(fee_cash(&h), cash_before, "and no cash moved");
	assert_eq!(product_cash(&h, &scope, &service), usdt("1000"));
}

/// A holder of `fee` units exits like any investor — priced at the allocation's NAV, paid
/// out of its account — except that a shortfall is **refused, never queued**: nobody tops
/// the fee allocation up on request. Settling the product's fee class puts the cash there,
/// and the same redemption then completes at once.
#[tokio::test]
async fn a_redemption_of_fee_units_the_allocations_cash_cannot_cover_is_refused_not_queued() {
	let Some(h) = harness().await else { return };
	let scope = ScopeId::invest();
	let service = unique_service();
	open_fund(&h, &scope, &service).await;
	// A large position, so the year's fee class is worth far more than the fee
	// allocation's cash — the shortfall below is by construction.
	let investor = invest(&h, &scope, &service, "1000000").await;
	let owner = person(&h).await;
	let fee_class = charge_a_year(&h, investor, &service).await;

	let value = funds_app::nav_of(&h.nav, h.ledger.as_ref(), h.money.as_ref(), &ServiceId::fee()).await.unwrap().aum.unwrap();
	let cash_before = fee_cash(&h);
	let granted = Shares::from_cash(value, Nav::SEED).unwrap();
	grant_fee_units(&h, owner, &granted.to_decimal_string()).await;
	let price = fee_nav(&h).await;
	let uncovered = units_worth_at_least(cash_before.checked_add(usdt("1")).unwrap(), price);
	assert!(uncovered <= granted, "the owner's stake ({granted}) outweighs the cash ({cash_before})");

	let err = funds_app::request_redemption(&fund_ports(&h), &h.redemptions, &h.users, owner, ServiceId::fee(), uncovered, now_unix())
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::Validation(ref m) if m.contains("cannot cover")), "refused as a shortfall: {err:?}");
	assert!(h.redemptions.list_by_user(owner).await.unwrap().is_empty(), "refused, not queued");
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(ServiceId::fee(), owner)).await, granted, "nothing was reserved");

	let settlement = settle_fee_shares(&h, &service, UserId::new()).await.unwrap();
	assert_eq!(settlement.cash(), Nav::SEED.value(fee_class).unwrap());
	let redemption = funds_app::request_redemption(&fund_ports(&h), &h.redemptions, &h.users, owner, ServiceId::fee(), uncovered, now_unix())
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let redemption = h.redemptions.find_by_id(redemption.id()).await.unwrap().unwrap();
	assert_eq!(redemption.state(), RedemptionState::Completed, "covered, so settled at once");
	assert_eq!(redemption.nav(), Some(price), "priced at the allocation's own NAV");
	let paid = redemption.cash().unwrap();
	assert_eq!(h.money.compartment(&scope, &common::auth_subject(owner)), paid, "paid into the person's compartment");
	assert_eq!(
		fee_cash(&h),
		cash_before.checked_add(settlement.cash()).unwrap().checked_sub(paid).unwrap(),
		"out of the allocation's account"
	);
	assert_eq!(units_of(&h, LedgerAccountKey::UserShares(ServiceId::fee(), owner)).await, granted.checked_sub(uncovered).unwrap());
}

/// H-3 of the #245 security review: the cooldown that binds a product's poster binds them
/// on `fee` too — a fee holder must not mark the product up and cash `fee` units out at the
/// price they set. Someone who did not mark redeems as before, and the poster is free once
/// the mark has aged out.
#[tokio::test]
async fn a_fee_holder_who_marked_a_product_cannot_redeem_fee_units_for_seven_days() {
	let Some(h) = harness().await else { return };
	let scope = ScopeId::invest();
	let service = unique_service();
	open_fund(&h, &scope, &service).await;
	let investor = invest(&h, &scope, &service, "1000").await;
	let (poster, bystander) = (person(&h).await, person(&h).await);
	assert!(charge_a_year(&h, investor, &service).await > Shares::ZERO, "the fee allocation holds the product's fee class");
	// Cash behind the allocation, so the one-unit redemptions below are covered and the
	// only thing standing between the poster and their cash is the cooldown.
	fund_fee_claim(&h, "100").await;
	grant_fee_units(&h, poster, "10").await;
	grant_fee_units(&h, bystander, "10").await;

	let aum = product_cash(&h, &scope, &service);
	funds_app::post_fund_valuation(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), aum, &poster.to_string(), now_unix())
		.await
		.unwrap();

	let err = funds_app::request_redemption(&fund_ports(&h), &h.redemptions, &h.users, poster, ServiceId::fee(), shares("1"), now_unix())
		.await
		.expect_err("the poster's fee units are priced off their own mark");
	assert!(
		matches!(err, DomainError::Precondition(ref m) if m.contains("posted a valuation") && m.contains(service.as_str())),
		"refused by the cooldown, naming the product: {err:?}"
	);
	assert!(h.redemptions.list_by_user(poster).await.unwrap().is_empty(), "refused, not queued");

	let redemption = funds_app::request_redemption(&fund_ports(&h), &h.redemptions, &h.users, bystander, ServiceId::fee(), shares("1"), now_unix())
		.await
		.expect("the cooldown binds the poster, not the allocation");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(h.redemptions.find_by_id(redemption.id()).await.unwrap().unwrap().state(), RedemptionState::Completed);

	sqlx::query("UPDATE fund_valuations SET posted_at = posted_at - interval '8 days' WHERE service = $1 AND posted_by = $2")
		.bind(service.as_str())
		.bind(poster.to_string())
		.execute(&h.pool)
		.await
		.unwrap();
	// The aged mark makes the product's price stale; a fresh mark by somebody else
	// restores it without touching the poster's cooldown.
	funds_app::post_fund_valuation(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), aum, "another-operator", now_unix())
		.await
		.unwrap();
	funds_app::request_redemption(&fund_ports(&h), &h.redemptions, &h.users, poster, ServiceId::fee(), shares("1"), now_unix())
		.await
		.expect("an aged-out mark no longer binds its poster");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
}

/// M-1 of the #245 security review: settling a product's fee class turns the poster's mark
/// into the `fee` holders' cash one step before the redeem cooldown would catch them, so
/// the same cooldown binds the settler; another administrator settles as before.
#[tokio::test]
async fn the_admin_who_marked_a_product_cannot_settle_its_fee_class_for_seven_days() {
	let Some(h) = harness().await else { return };
	let scope = ScopeId::invest();
	let service = unique_service();
	open_fund(&h, &scope, &service).await;
	let investor = invest(&h, &scope, &service, "1000").await;
	let (poster, colleague) = (UserId::new(), UserId::new());
	let fee_class = charge_a_year(&h, investor, &service).await;
	let cash_before = fee_cash(&h);

	let aum = product_cash(&h, &scope, &service);
	funds_app::post_fund_valuation(&h.allocations, &h.nav, h.ledger.as_ref(), service.clone(), aum, &poster.to_string(), now_unix())
		.await
		.unwrap();

	let err = settle_fee_shares(&h, &service, poster).await.expect_err("the poster must not crystallize their own mark");
	assert!(
		matches!(err, DomainError::Precondition(ref m) if m.contains("posted a valuation")),
		"refused by the cooldown: {err:?}"
	);
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await, fee_class, "the fee class is untouched");
	assert_eq!(fee_cash(&h), cash_before, "and no cash moved");
	assert!(h.settlements.list_by_service(&service).await.unwrap().is_empty(), "no settlement was recorded");

	let settlement = settle_fee_shares(&h, &service, colleague)
		.await
		.expect("an administrator who did not mark the product settles it");
	assert_eq!(settlement.units(), fee_class);
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await, Shares::ZERO);
	assert_eq!(fee_cash(&h), cash_before.checked_add(settlement.cash()).unwrap());
}

/// Two settlements of one fee class asked for before the relay applied the first: the
/// second must not sweep the same value out of the product's account again.
#[tokio::test]
async fn a_fee_class_is_swept_once_however_often_it_is_settled() {
	let Some(h) = harness().await else { return };
	let scope = ScopeId::invest();
	let service = unique_service();
	open_fund(&h, &scope, &service).await;
	let investor = invest(&h, &scope, &service, "1000").await;
	let fee_class = charge_a_year(&h, investor, &service).await;
	let product_before = product_cash(&h, &scope, &service);

	let first = settle_without_draining(&h, &service).await.expect("the first settlement is accepted");
	assert_eq!(first.units(), fee_class);
	let second = settle_without_draining(&h, &service).await;
	assert!(second.is_err(), "the fee class is already on its way to be settled: {second:?}");
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	assert_eq!(product_cash(&h, &scope, &service), product_before.checked_sub(first.cash()).unwrap(), "swept once");
	assert_eq!(units_of(&h, LedgerAccountKey::FeeShares(service.clone())).await, Shares::ZERO);
}

/// The fee units a settlement names are gone by the time the relay carries it: it must not
/// sweep cash for units it cannot burn.
#[tokio::test]
async fn a_settlement_whose_fee_units_are_gone_sweeps_nothing() {
	let Some(h) = harness().await else { return };
	let scope = ScopeId::invest();
	let service = unique_service();
	open_fund(&h, &scope, &service).await;
	let investor = invest(&h, &scope, &service, "1000").await;
	let fee_class = charge_a_year(&h, investor, &service).await;
	let product_before = product_cash(&h, &scope, &service);

	settle_without_draining(&h, &service).await.unwrap();
	h.ledger
		.post(&invest_core::ports::ledger::LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::UserShares(service.clone(), investor),
			credit: LedgerAccountKey::FeeShares(service.clone()),
			amount: fee_class.base_units(),
			code: invest_domain::balance::TransferCode::FeeClawback,
			reference: 0,
		})
		.await
		.unwrap();
	// The park IS what this test pins, so one pass instead of `drain_to_quiescence`.
	h.relay.drain().await;

	assert_eq!(product_cash(&h, &scope, &service), product_before, "nothing swept");
	assert_eq!(h.money.collected(&scope), Usdt::ZERO);
}

async fn settle_without_draining(h: &Harness, service: &ServiceId) -> Result<invest_domain::fees::FeeSettlement, DomainError> {
	common::settle_fee_class(h, service, None, &common::fresh_key(), UserId::new()).await
}

/// A retried settlement — the same idempotency key — is the settlement the first call made,
/// and moves no cash again.
#[tokio::test]
async fn a_retried_settlement_returns_the_first_and_sweeps_once() {
	let Some(h) = harness().await else { return };
	let scope = ScopeId::invest();
	let service = unique_service();
	open_fund(&h, &scope, &service).await;
	let investor = invest(&h, &scope, &service, "1000").await;
	charge_a_year(&h, investor, &service).await;
	let product_before = product_cash(&h, &scope, &service);
	let key = invest_domain::issuance::IdempotencyKey::parse(&format!("retry-{}", Uuid::new_v4())).unwrap();
	let settle = || common::settle_fee_class(&h, &service, None, &key, UserId::new());
	let first = settle().await.unwrap();
	let again = settle().await.unwrap();
	assert_eq!(again.id(), first.id(), "the key names the settlement");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let after_drain = settle().await.unwrap();
	assert_eq!(after_drain.id(), first.id());
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(product_cash(&h, &scope, &service), product_before.checked_sub(first.cash()).unwrap(), "swept once");
	assert_eq!(h.settlements.list_by_service(&service).await.unwrap().len(), 1);
}

// ---- ListAllocationTreasury (#36) ----------------------------------------------------

fn treasury_service(h: &Harness) -> AllocationsSvc {
	AllocationsSvc::new(h.app_state())
}

fn treasury_request(service: &str, holder_limit: u32) -> pb::ListAllocationTreasuryRequest {
	pb::ListAllocationTreasuryRequest {
		service: service.to_owned(),
		holder_limit,
	}
}

/// The console's table holds every allocation — the hidden `fee` and `fund` beside the
/// products, in every scope — and each line's cash is the balance of the allocation's
/// own account in banking, read through the money port: for `fee` that is its cash alone,
/// not the value its price counts.
#[tokio::test]
async fn the_treasury_lists_every_allocation_with_its_cash_from_banking() {
	let Some(h) = harness().await else { return };
	let svc = treasury_service(&h);
	let ops = common::operator(&h).await;
	let sa = ScopeId::parse("sa").unwrap();
	let (product, elsewhere) = (unique_service(), unique_service());
	open_fund(&h, &ScopeId::invest(), &product).await;
	open_fund(&h, &sa, &elsewhere).await;
	h.money.credit(&ScopeId::invest(), &AccountLabel::allocation(&product).unwrap(), usdt("123.45"));
	h.money.credit(&sa, &AccountLabel::allocation(&elsewhere).unwrap(), usdt("7"));
	fund_fee_claim(&h, "40").await;
	let owner = person(&h).await;
	grant_fee_units(&h, owner, "10").await;

	// Every holder, so the owner seated here is listed whatever this binary's other tests seated.
	let lines = svc
		.list_allocation_treasury(common::as_caller(ops, treasury_request("", 1000)))
		.await
		.unwrap()
		.into_inner()
		.allocations;
	let slugs: Vec<&str> = lines.iter().map(|line| line.service.as_str()).collect();
	let mut sorted = slugs.clone();
	sorted.sort_unstable();
	assert_eq!(slugs, sorted, "ordered by service");
	let line = |slug: &str| lines.iter().find(|line| line.service == slug).unwrap_or_else(|| panic!("no line for {slug}"));

	let fee = line("fee");
	assert_eq!((fee.access.as_str(), fee.scope.as_str()), ("hidden", "invest"), "hidden from the catalog, not from the table");
	assert_eq!(fee.cash, fee_cash(&h).to_decimal_string(), "the cash on (invest, alloc:fee), nothing else");
	assert_eq!(fee.nav, fee_nav(&h).await.to_decimal_string(), "the price every position reads");
	assert_eq!(
		fee.units_outstanding,
		units_of(&h, LedgerAccountKey::SharesOutstanding(ServiceId::fee())).await.to_decimal_string()
	);
	assert!(fee.top_holders.iter().any(|holding| holding.holder.as_ref().is_some_and(|holder| holder.id == owner.to_string())));
	let fund = line("fund");
	assert_eq!(fund.access, "hidden");

	let product_line = line(product.as_str());
	assert_eq!((product_line.cash.as_str(), product_line.scope.as_str()), ("123.45", "invest"));
	assert_eq!((product_line.units_outstanding.as_str(), product_line.holder_count), ("0", 0), "nobody holds it yet");
	assert_eq!(product_line.nav_posted_at, 0, "never marked");
	assert!(!product_line.nav_stale, "a price never marked has nothing to be stale against");
	let elsewhere_line = line(elsewhere.as_str());
	assert_eq!((elsewhere_line.cash.as_str(), elsewhere_line.scope.as_str()), ("7", "sa"), "read in the product's own scope");
}

/// A line carries at most the largest holders it was asked for and counts the rest, so a
/// cell can fold them away without a second read.
#[tokio::test]
async fn a_treasury_line_carries_the_largest_holders_and_counts_the_rest() {
	let Some(h) = harness().await else { return };
	let svc = treasury_service(&h);
	let ops = common::operator(&h).await;
	fund_fee_claim(&h, "10").await;
	let (big, mid, small) = (person(&h).await, person(&h).await, person(&h).await);
	grant_fee_units(&h, big, "1000000").await;
	grant_fee_units(&h, mid, "500000").await;
	grant_fee_units(&h, small, "1").await;

	let lines = svc
		.list_allocation_treasury(common::as_caller(ops, treasury_request("fee", 2)))
		.await
		.unwrap()
		.into_inner()
		.allocations;
	assert_eq!(lines.len(), 1, "the one allocation named");
	let fee = &lines[0];
	let cap_table = issuance_app::unit_holders(&h.allocations, h.ledger.as_ref(), &h.issuances, ServiceId::fee()).await.unwrap();
	assert_eq!(fee.top_holders.len(), 2);
	assert_eq!(fee.holder_count as usize, cap_table.holders.len(), "every holder is counted, the folded ones too");
	let shown: Vec<String> = fee.top_holders.iter().map(|holding| holding.holder.as_ref().unwrap().id.clone()).collect();
	let largest: Vec<String> = cap_table
		.holders
		.iter()
		.take(2)
		.map(|line| match &line.holder {
			UnitHolder::User(user) => user.to_string(),
			UnitHolder::Allocation(service) => service.to_string(),
		})
		.collect();
	assert_eq!(shown, largest, "the cap table's own order, cut");
	assert!(!shown.contains(&small.to_string()));

	let by_default = svc
		.list_allocation_treasury(common::as_caller(ops, treasury_request("fee", 0)))
		.await
		.unwrap()
		.into_inner()
		.allocations;
	assert_eq!(by_default[0].top_holders.len(), cap_table.holders.len().min(DEFAULT_TOP_HOLDERS), "0 asks for the default");
}

/// The table is the platform's money: an investor is refused it, and a request it cannot
/// answer honestly is refused rather than trimmed.
#[tokio::test]
async fn the_treasury_is_the_managers_and_refuses_what_it_cannot_answer() {
	let Some(h) = harness().await else { return };
	let svc = treasury_service(&h);
	let investor = person(&h).await;
	let refused = svc.list_allocation_treasury(common::as_caller(investor, treasury_request("", 0))).await.unwrap_err();
	assert_eq!(refused.code(), tonic::Code::PermissionDenied);

	let ops = common::operator(&h).await;
	let too_many = svc.list_allocation_treasury(common::as_caller(ops, treasury_request("", 1001))).await.unwrap_err();
	assert_eq!(too_many.code(), tonic::Code::InvalidArgument);
	let unknown = svc
		.list_allocation_treasury(common::as_caller(ops, treasury_request(unique_service().as_str(), 0)))
		.await
		.unwrap_err();
	assert_eq!(unknown.code(), tonic::Code::NotFound);
}
