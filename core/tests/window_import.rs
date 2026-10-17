//! The migration-window import (`invest-core window-import`) against banking's export format.
//! The planning half is pure and always runs; the apply half needs `DATABASE_URL` (a suite
//! without it is skipped locally and fails under `CI=true`). The unit ledger is an in-memory
//! fake on purpose: what is under test is the importer's bookkeeping — one transaction,
//! idempotence, the read-back — and it reaches TigerBeetle only through the `Ledger` port,
//! whose real adapter has its own suites.

use std::{
	collections::{HashMap, HashSet},
	sync::Mutex,
};

use async_trait::async_trait;
use banking_domain::window_export::{
	Allocation, AllocationTotal, BookPolicy, FeeAssessment, FeePolicy, FeePolicyChange, FeeSettlement, Header, HeldProductUnits, HoldingKind, OwnedUnits, Ownership, Position, Record,
	SCHEMA_VERSION, ShareHolding, Supply, Trailer, UserRef, Valuation,
};
use invest_core::{
	application::window_import::{
		self,
		report::{Mode, Report},
	},
	infrastructure::window_import as apply,
	ports::ledger::{HoldingScope, Ledger, LedgerBalance, LedgerError, LedgerTransfer, PendingCompletion},
};
use invest_domain::{
	architecture::Gateway,
	balance::{LedgerAccountKey, Normal},
};
use sqlx::PgPool;

mod common;

const E18: u128 = 1_000_000_000_000_000_000;
const VALERATRADES: u128 = 13_000 * E18;
const ININTEL44: u128 = 3_250 * E18;

// banking user ids and the concierge ids they map to.
const B_VAL: &str = "b0000000-0000-4000-8000-000000000001";
const B_INT: &str = "b0000000-0000-4000-8000-000000000002";
const B_FEE: &str = "b0000000-0000-4000-8000-000000000003";
const B_INV: &str = "b0000000-0000-4000-8000-000000000004";
const C_VAL: &str = "c0000000-0000-4000-8000-000000000001";
const C_INT: &str = "c0000000-0000-4000-8000-000000000002";
const C_FEE: &str = "c0000000-0000-4000-8000-000000000003";
const C_INV: &str = "c0000000-0000-4000-8000-000000000004";

const TS: &str = "2026-10-16T12:00:00Z";

fn s(units: u128) -> String {
	units.to_string()
}

fn allocation(id: &str, service: &str) -> Record {
	Record::Allocation(Allocation {
		id: id.into(),
		service: service.into(),
		title: format!("{service} title"),
		summary: String::new(),
		state: "open".into(),
		access: "view".into(),
		backing: "cash".into(),
		icon: "fund".into(),
		unit_cap: s(1_000_000 * E18),
		created_at: TS.into(),
		updated_at: TS.into(),
	})
}

fn holding(service: &str, kind: HoldingKind, user: Option<&str>, units: u128) -> Record {
	Record::ShareHolding(ShareHolding {
		service: service.into(),
		kind,
		user_id: user.map(Into::into),
		units: s(units),
	})
}

fn supply(service: &str, units: u128) -> Record {
	Record::Supply(Supply {
		service: service.into(),
		shares_outstanding: s(units),
		claim_posted: "0".into(),
		claim_pending: "0".into(),
		claim_locked: "0".into(),
	})
}

fn user(banking: &str, concierge: &str) -> Record {
	Record::User(UserRef {
		user_id: banking.into(),
		concierge_user_id: Some(concierge.into()),
	})
}

fn header() -> Header {
	Header {
		schema_version: SCHEMA_VERSION,
		source: "banking".into(),
		exported_at: TS.into(),
		operations_read_only: true,
		live: false,
		tigerbeetle_cluster_id: "0".into(),
	}
}

/// The records between header and trailer: `service_arb` held by the two SA clients, an invest
/// product with a fee class, the `fee` allocation held by one person, an empty `fund`.
fn body() -> Vec<Record> {
	vec![
		allocation("680240f5-1c41-58e2-987c-e52dbe2faa01", "fee"),
		allocation("7533b905-9200-5ffb-b053-844c9c8da99a", "fund"),
		allocation("a0000000-0000-4000-8000-000000000001", "imp_inv"),
		allocation("a0000000-0000-4000-8000-000000000002", "service_arb"),
		Record::FeePolicy(FeePolicy {
			service: "imp_inv".into(),
			management_bps: 200,
			performance_bps: 2000,
			hurdle_bps: 0,
			basis: "invested_capital".into(),
			crystallization: "quarterly".into(),
			version: 1,
			effective_from: TS.into(),
			updated_by: "admin".into(),
			updated_at: TS.into(),
		}),
		// banking still carries terms for service_arb; invest takes none for scope sa.
		Record::FeePolicy(FeePolicy {
			service: "service_arb".into(),
			management_bps: 0,
			performance_bps: 2000,
			hurdle_bps: 0,
			basis: "invested_capital".into(),
			crystallization: "annual".into(),
			version: 1,
			effective_from: TS.into(),
			updated_by: "admin".into(),
			updated_at: TS.into(),
		}),
		Record::FeePolicyChange(FeePolicyChange {
			id: "d0000000-0000-4000-8000-000000000001".into(),
			service: "imp_inv".into(),
			version: 1,
			management_bps: 200,
			performance_bps: 2000,
			hurdle_bps: 0,
			basis: "invested_capital".into(),
			crystallization: "quarterly".into(),
			state: "active".into(),
			requirement: "admin".into(),
			effective_from: TS.into(),
			requested_by: "admin".into(),
			requested_at: TS.into(),
			reason: String::new(),
			consilium_id: None,
			scheduled_at: Some(TS.into()),
			applied_at: Some(TS.into()),
			closed_reason: None,
			notices_waived_by: Some("admin".into()),
			notices_waived_at: Some(TS.into()),
			notices_waived_users: Some(vec![B_INV.into()]),
		}),
		Record::BookPolicy(BookPolicy {
			service: "imp_inv".into(),
			book_open: true,
			taker_fee_bps: 25,
			price_tick: "10000000000000000".into(),
			lot_size: "100000000000000".into(),
			market_slippage_bps: 500,
			updated_at: TS.into(),
		}),
		Record::Valuation(Valuation {
			id: "e0000000-0000-4000-8000-000000000001".into(),
			service: "imp_inv".into(),
			aum: s(10 * E18),
			units_outstanding: s(10 * E18),
			nav: s(E18),
			posted_by: "admin".into(),
			posted_at: TS.into(),
		}),
		Record::Valuation(Valuation {
			id: "e0000000-0000-4000-8000-000000000002".into(),
			service: "service_arb".into(),
			aum: s(16_250 * E18),
			units_outstanding: s(16_250 * E18),
			nav: s(E18),
			posted_by: "admin".into(),
			posted_at: TS.into(),
		}),
		position(B_VAL, "service_arb", VALERATRADES),
		position(B_INT, "service_arb", ININTEL44),
		position(B_INV, "imp_inv", 8 * E18),
		Record::FeeAssessment(FeeAssessment {
			id: "f0000000-0000-4000-8000-000000000001".into(),
			user_id: B_INV.into(),
			service: "imp_inv".into(),
			trigger_kind: "period".into(),
			nav: s(E18),
			management: "1".into(),
			performance: "0".into(),
			debt_opening: "0".into(),
			charged_units: "1".into(),
			charged_cash: "1".into(),
			debt_carried: "0".into(),
			high_water_mark: s(E18),
			assessed_at: TS.into(),
		}),
		Record::FeeSettlement(FeeSettlement {
			id: "f0000000-0000-4000-8000-000000000002".into(),
			service: "imp_inv".into(),
			units: s(E18),
			nav: s(E18),
			cash: s(E18),
			settled_by: "admin".into(),
			settled_at: TS.into(),
		}),
		user(B_VAL, C_VAL),
		user(B_INT, C_INT),
		user(B_FEE, C_FEE),
		user(B_INV, C_INV),
		holding("imp_inv", HoldingKind::UserShares, Some(B_INV), 8 * E18),
		holding("imp_inv", HoldingKind::FeeShares, None, 2 * E18),
		supply("imp_inv", 10 * E18),
		holding("service_arb", HoldingKind::UserShares, Some(B_VAL), VALERATRADES),
		holding("service_arb", HoldingKind::UserShares, Some(B_INT), ININTEL44),
		supply("service_arb", VALERATRADES + ININTEL44),
		holding("fee", HoldingKind::UserShares, Some(B_FEE), 5 * E18),
		supply("fee", 5 * E18),
		Record::Ownership(Ownership {
			service: "fee".into(),
			units_outstanding: s(5 * E18),
			claim_posted: "0".into(),
			holders: vec![OwnedUnits {
				user_id: B_FEE.into(),
				units: s(5 * E18),
			}],
			product_units: vec![HeldProductUnits {
				service: "imp_inv".into(),
				units: s(2 * E18),
			}],
		}),
		supply("fund", 0),
		Record::Ownership(Ownership {
			service: "fund".into(),
			units_outstanding: "0".into(),
			claim_posted: "0".into(),
			holders: vec![],
			product_units: vec![],
		}),
	]
}

fn position(banking: &str, service: &str, units: u128) -> Record {
	Record::Position(Position {
		user_id: banking.into(),
		service: service.into(),
		cost_basis: s(units),
		high_water_mark: s(E18),
		units: s(units),
		fee_debt: "0".into(),
		fees_accrued_at: TS.into(),
		crystallized_at: TS.into(),
		created_at: TS.into(),
		updated_at: TS.into(),
	})
}

/// The trailer an honest exporter would write for `records` (header and trailer excluded).
fn trailer_of(records: &[Record]) -> Trailer {
	let mut counts = std::collections::BTreeMap::new();
	counts.insert("header".to_owned(), 1);
	counts.insert("trailer".to_owned(), 1);
	let mut totals: std::collections::BTreeMap<String, [u128; 4]> = std::collections::BTreeMap::new();
	for r in records {
		*counts.entry(r.kind().to_owned()).or_default() += 1;
		match r {
			Record::Allocation(a) => {
				totals.entry(a.service.clone()).or_default();
			}
			Record::ShareHolding(h) => {
				let t = totals.entry(h.service.clone()).or_default();
				let slot = match h.kind {
					HoldingKind::UserShares => 0,
					HoldingKind::BookShares => 1,
					HoldingKind::FeeShares => 2,
				};
				t[slot] += h.units.parse::<u128>().unwrap();
			}
			Record::Supply(sup) => {
				totals.entry(sup.service.clone()).or_default()[3] = sup.shares_outstanding.parse().unwrap();
			}
			_ => {}
		}
	}
	let allocations: Vec<AllocationTotal> = totals
		.into_iter()
		.map(|(service, [user, book, fee, outstanding])| AllocationTotal {
			service,
			user_shares: s(user),
			book_shares: s(book),
			fee_shares: s(fee),
			held: s(user + book + fee),
			shares_outstanding: s(outstanding),
			balanced: user + book + fee == outstanding,
		})
		.collect();
	Trailer {
		schema_version: SCHEMA_VERSION,
		counts,
		total_held: s(allocations.iter().map(|a| a.held.parse::<u128>().unwrap()).sum()),
		total_outstanding: s(allocations.iter().map(|a| a.shares_outstanding.parse::<u128>().unwrap()).sum()),
		allocations,
		outbox_undispatched: 0,
		clean: true,
	}
}

fn file_of(header: Header, body: &[Record], tweak: impl FnOnce(&mut Trailer)) -> String {
	let mut trailer = trailer_of(body);
	tweak(&mut trailer);
	let mut lines = vec![serde_json::to_string(&Record::Header(header)).unwrap()];
	lines.extend(body.iter().map(|r| serde_json::to_string(r).unwrap()));
	lines.push(serde_json::to_string(&Record::Trailer(trailer)).unwrap());
	lines.join("\n") + "\n"
}

fn good_file() -> String {
	file_of(header(), &body(), |_| {})
}

fn refusal_of(file: &str) -> String {
	window_import::plan(file.as_bytes()).expect_err("the file must be refused").to_string()
}

// ── planning: pure, no database ────────────────────────────────────────────

#[test]
fn the_service_arb_holders_and_their_sum_are_read_as_exported() {
	let plan = window_import::plan(good_file().as_bytes()).expect("a good file plans");

	let sa = plan.totals.iter().find(|t| t.service == "service_arb").unwrap();
	assert_eq!(sa.scope, "sa");
	assert_eq!(sa.holders[&C_VAL.parse().unwrap()], VALERATRADES);
	assert_eq!(sa.holders[&C_INT.parse().unwrap()], ININTEL44);
	assert_eq!(sa.held(), 16_250 * E18);
	assert_eq!(sa.held(), sa.outstanding);
	assert_eq!(plan.totals.iter().find(|t| t.service == "imp_inv").unwrap().scope, "invest");
}

#[test]
fn a_dry_run_report_shows_the_sums_and_what_it_leaves_out() {
	let plan = window_import::plan(good_file().as_bytes()).unwrap();

	let report = Report::of_plan(&plan, Mode::DryRun);

	assert!(report.ok);
	let rendered = report.render();
	assert!(rendered.contains("service_arb"), "{rendered}");
	assert!(rendered.contains("13000"), "{rendered}");
	assert!(rendered.contains("3250"), "{rendered}");
	assert!(rendered.contains("16250 (outstanding 16250) balanced"), "{rendered}");
	assert!(report.notes.iter().any(|n| n.contains("service_arb") && n.contains("takes no fee terms")), "{:?}", report.notes);
	assert!(plan.fee_policies.iter().all(|p| p.service != "service_arb"));
}

#[test]
fn a_foreign_schema_version_is_refused_before_anything_else() {
	let mut h = header();
	h.schema_version = SCHEMA_VERSION + 1;
	let err = refusal_of(&file_of(h, &body(), |_| {}));
	assert!(err.contains("schema_version 2"), "{err}");
}

#[test]
fn a_live_export_is_refused() {
	let mut h = header();
	h.live = true;
	assert!(refusal_of(&file_of(h, &body(), |_| {})).contains("live = true"));
}

#[test]
fn an_export_from_another_source_is_refused() {
	let mut h = header();
	h.source = "someone-else".into();
	assert!(refusal_of(&file_of(h, &body(), |_| {})).contains("only \"banking\""));
}

#[test]
fn a_trailer_that_says_not_clean_is_refused() {
	let err = refusal_of(&file_of(header(), &body(), |t| t.clean = false));
	assert!(err.contains("clean = false"), "{err}");
	let err = refusal_of(&file_of(header(), &body(), |t| t.outbox_undispatched = 3));
	assert!(err.contains("3 outbox events"), "{err}");
}

#[test]
fn counts_that_do_not_match_the_lines_are_refused() {
	let err = refusal_of(&file_of(header(), &body(), |t| *t.counts.get_mut("share_holding").unwrap() += 1));
	assert!(err.contains("counts[share_holding]"), "{err}");

	// A line lost on the way: the trailer still counts it.
	let file = good_file();
	let kept: Vec<&str> = file.lines().filter(|l| !l.contains(r#""record":"fee_settlement""#)).collect();
	let err = refusal_of(&(kept.join("\n") + "\n"));
	assert!(err.contains("counts[fee_settlement] = 1"), "{err}");
}

#[test]
fn a_truncated_file_is_refused() {
	let file = good_file();
	let cut: Vec<&str> = file.lines().take(file.lines().count() - 1).collect();
	assert!(refusal_of(&(cut.join("\n") + "\n")).contains("no trailer"));
}

#[test]
fn a_damaged_line_is_refused_with_its_line_number() {
	let file = good_file();
	let mut lines: Vec<String> = file.lines().map(str::to_owned).collect();
	lines[5] = r#"{"record":"valuation","id":"#.into();
	let err = refusal_of(&(lines.join("\n") + "\n"));
	assert!(err.contains("line 6:"), "{err}");
}

#[test]
fn a_trailer_total_that_differs_from_the_holdings_is_refused() {
	let err = refusal_of(&file_of(header(), &body(), |t| {
		let sa = t.allocations.iter_mut().find(|a| a.service == "service_arb").unwrap();
		sa.user_shares = s(VALERATRADES);
	}));
	assert!(err.contains("service_arb user_shares"), "{err}");
}

#[test]
fn a_supply_its_holders_do_not_add_up_to_is_refused() {
	let mut records = body();
	for r in &mut records {
		if let Record::Supply(sup) = r
			&& sup.service == "service_arb"
		{
			sup.shares_outstanding = s(VALERATRADES + ININTEL44 + 1);
		}
	}
	let err = refusal_of(&file_of(header(), &records, |_| {}));
	assert!(err.contains("allocation \"service_arb\""), "{err}");
}

#[test]
fn a_holder_without_a_concierge_id_is_refused() {
	let mut records = body();
	records.retain(|r| !matches!(r, Record::User(u) if u.user_id == B_INT));
	let err = refusal_of(&file_of(header(), &records, |_| {}));
	assert!(err.contains(B_INT) && err.contains("concierge_user_id"), "{err}");
}

#[test]
fn units_resting_in_sell_orders_are_refused() {
	let mut records = body();
	records.push(holding("imp_inv", HoldingKind::BookShares, Some(B_INV), E18));
	let err = refusal_of(&file_of(header(), &records, |_| {}));
	assert!(err.contains("sell orders"), "{err}");
}

#[test]
fn fee_class_units_of_a_product_outside_invest_are_refused() {
	let mut records = body();
	records.push(holding("service_arb", HoldingKind::FeeShares, None, E18));
	assert!(refusal_of(&file_of(header(), &records, |_| {})).contains("only products of scope invest"));
}

#[test]
fn an_ownership_view_that_disagrees_with_the_holdings_is_refused() {
	let mut records = body();
	for r in &mut records {
		if let Record::Ownership(o) = r
			&& o.service == "fee"
		{
			o.holders[0].units = s(4 * E18);
		}
	}
	assert!(refusal_of(&file_of(header(), &records, |_| {})).contains("ownership of \"fee\""));
}

#[test]
fn a_reserved_allocation_under_another_id_is_refused() {
	let mut records = body();
	records[0] = allocation("a0000000-0000-4000-8000-0000000000ff", "fee");
	assert!(refusal_of(&file_of(header(), &records, |_| {})).contains("reserved allocation \"fee\""));
}

#[test]
fn a_position_that_disagrees_with_the_ledger_is_a_warning_not_a_refusal() {
	let mut records = body();
	for r in &mut records {
		if let Record::Position(p) = r
			&& p.user_id == B_INV
		{
			p.units = s(9 * E18);
		}
	}
	let plan = window_import::plan(file_of(header(), &records, |_| {}).as_bytes()).unwrap();
	assert!(plan.warnings.iter().any(|w| w.contains("says 9 units, the ledger holds 8")), "{:?}", plan.warnings);
}

// ── applying: Postgres + an in-memory unit ledger ──────────────────────────

#[derive(Default)]
struct FakeLedger {
	sides: Mutex<HashMap<String, (u128, u128)>>,
	seen: Mutex<HashSet<u128>>,
	posts: Mutex<usize>,
	fail_posts: Mutex<bool>,
}

impl FakeLedger {
	fn touched(&self) -> usize {
		self.sides.lock().unwrap().len() + *self.posts.lock().unwrap()
	}

	fn move_units(&self, t: &LedgerTransfer) {
		let mut sides = self.sides.lock().unwrap();
		sides.entry(t.debit.logical_key()).or_default().0 += t.amount;
		sides.entry(t.credit.logical_key()).or_default().1 += t.amount;
	}

	fn posted(&self, logical_key: &str) -> u128 {
		let key = LedgerAccountKey::parse_logical_key(logical_key).unwrap();
		let (d, c) = self.sides.lock().unwrap().get(logical_key).copied().unwrap_or_default();
		match key.normal() {
			Normal::Debit => d - c,
			Normal::Credit => c - d,
		}
	}
}

impl Gateway for FakeLedger {}

#[async_trait]
impl Ledger for FakeLedger {
	async fn ensure_account(&self, key: &LedgerAccountKey) -> Result<(), LedgerError> {
		self.sides.lock().unwrap().entry(key.logical_key()).or_default();
		Ok(())
	}

	async fn balance(&self, key: &LedgerAccountKey) -> Result<LedgerBalance, LedgerError> {
		Ok(LedgerBalance {
			posted: self.posted(&key.logical_key()),
			pending: 0,
			locked: 0,
		})
	}

	async fn transfer_exists(&self, id: u128) -> Result<bool, LedgerError> {
		Ok(self.seen.lock().unwrap().contains(&id))
	}

	async fn post(&self, t: &LedgerTransfer) -> Result<(), LedgerError> {
		if *self.fail_posts.lock().unwrap() {
			return Err(LedgerError::Unavailable("the ledger went away".into()));
		}
		// A re-submitted id is `Exists` in TigerBeetle, which the port reports as success.
		if self.seen.lock().unwrap().insert(t.id) {
			*self.posts.lock().unwrap() += 1;
			self.move_units(t);
		}
		Ok(())
	}

	async fn post_linked(&self, _: &[LedgerTransfer]) -> Result<(), LedgerError> {
		Err(LedgerError::Conflict("not used by the importer".into()))
	}

	async fn reserve(&self, _: &LedgerTransfer) -> Result<(), LedgerError> {
		Err(LedgerError::Conflict("not used by the importer".into()))
	}

	async fn complete(&self, _: &PendingCompletion) -> Result<(), LedgerError> {
		Err(LedgerError::Conflict("not used by the importer".into()))
	}

	async fn share_holdings(&self, scope: &HoldingScope) -> Result<Vec<(LedgerAccountKey, u128)>, LedgerError> {
		let keys: Vec<String> = self.sides.lock().unwrap().keys().cloned().collect();
		Ok(keys
			.into_iter()
			.filter_map(|k| LedgerAccountKey::parse_logical_key(&k).ok())
			.filter(|k| scope.admits(k))
			.map(|k| {
				let units = self.posted(&k.logical_key());
				(k, units)
			})
			.collect())
	}
}

async fn count(pool: &PgPool, table: &str) -> i64 {
	// The table name is a literal from this file.
	sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}"))).fetch_one(pool).await.unwrap()
}

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A clean slate: the tests share one database per binary.
async fn fresh() -> Option<(PgPool, tokio::sync::MutexGuard<'static, ()>)> {
	let pool = common::pool().await?;
	let guard = SERIAL.lock().await;
	for table in [
		"window_import", "fund_valuations", "fund_positions", "fee_settlements", "fee_assessments", "fee_policy_changes", "fee_policies", "book_policies",
	] {
		sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}"))).execute(&pool).await.unwrap();
	}
	sqlx::query("DELETE FROM allocations WHERE service NOT IN ('fee', 'fund')").execute(&pool).await.unwrap();
	Some((pool, guard))
}

fn plan_of(file: &str) -> window_import::Plan {
	window_import::plan(file.as_bytes()).expect("the fixture plans")
}

#[tokio::test]
async fn an_apply_writes_everything_and_the_holders_read_back_unchanged() {
	let Some((pool, _g)) = fresh().await else { return };
	let ledger = FakeLedger::default();
	let plan = plan_of(&good_file());

	let outcome = apply::apply(&pool, &ledger, &plan).await.expect("apply");

	assert!(outcome.rows_written && outcome.report.ok, "{}", outcome.report.render());
	assert_eq!(outcome.report.mode, Mode::Applied);
	// The anchor: the two SA clients and their sum, before (the file) and after (the ledger).
	assert_eq!(ledger.posted(&format!("shares:service_arb:{C_VAL}")), VALERATRADES);
	assert_eq!(ledger.posted(&format!("shares:service_arb:{C_INT}")), ININTEL44);
	assert_eq!(ledger.posted("shares_outstanding:service_arb"), 16_250 * E18);
	assert_eq!(ledger.posted("shares_fee:imp_inv"), 2 * E18);
	assert_eq!(ledger.posted("shares_outstanding:imp_inv"), 10 * E18);
	for who in [C_VAL, C_INT] {
		let check = outcome.report.checks.iter().find(|c| c.check == "holding_after" && c.subject.ends_with(who)).unwrap();
		assert!(check.ok, "{check:?}");
	}

	assert_eq!(count(&pool, "allocations").await, 4, "fee, fund, imp_inv, service_arb");
	let scope: String = sqlx::query_scalar("SELECT scope FROM allocations WHERE service = 'service_arb'").fetch_one(&pool).await.unwrap();
	assert_eq!(scope, "sa");
	assert_eq!(count(&pool, "fee_policies").await, 1, "service_arb takes no fee terms");
	assert_eq!(count(&pool, "fee_policy_changes").await, 1);
	assert_eq!(count(&pool, "book_policies").await, 1);
	assert_eq!(count(&pool, "fund_valuations").await, 2);
	assert_eq!(count(&pool, "fund_positions").await, 3);
	assert_eq!(count(&pool, "fee_assessments").await, 1);
	assert_eq!(count(&pool, "fee_settlements").await, 1);
	let mapped: i64 = sqlx::query_scalar("SELECT count(*) FROM fund_positions WHERE user_id = $1::uuid AND service = 'service_arb'")
		.bind(C_VAL)
		.fetch_one(&pool)
		.await
		.unwrap();
	assert_eq!(mapped, 1, "positions are keyed by the concierge id, not banking's");
	let state: String = sqlx::query_scalar("SELECT state FROM window_import").fetch_one(&pool).await.unwrap();
	assert_eq!(state, "complete");
}

#[tokio::test]
async fn applying_the_same_file_again_changes_nothing() {
	let Some((pool, _g)) = fresh().await else { return };
	let ledger = FakeLedger::default();
	let plan = plan_of(&good_file());
	apply::apply(&pool, &ledger, &plan).await.expect("first apply");
	let touched = ledger.touched();
	let valuations = count(&pool, "fund_valuations").await;

	let again = apply::apply(&pool, &ledger, &plan_of(&good_file())).await.expect("second apply");

	assert!(!again.rows_written);
	assert_eq!(again.transfers_submitted, 0);
	assert_eq!(again.report.mode, Mode::AlreadyApplied);
	assert!(again.report.ok, "{}", again.report.render());
	assert_eq!(ledger.touched(), touched, "a re-run must not touch the ledger");
	assert_eq!(count(&pool, "fund_valuations").await, valuations);
	assert_eq!(count(&pool, "window_import").await, 1);
}

#[tokio::test]
async fn a_run_that_died_between_the_rows_and_the_units_is_finished_by_the_next() {
	let Some((pool, _g)) = fresh().await else { return };
	let ledger = FakeLedger::default();
	*ledger.fail_posts.lock().unwrap() = true;
	let plan = plan_of(&good_file());

	let err = apply::apply(&pool, &ledger, &plan).await.unwrap_err();
	assert!(matches!(err, apply::ApplyError::Ledger { .. }), "{err}");
	let state: String = sqlx::query_scalar("SELECT state FROM window_import").fetch_one(&pool).await.unwrap();
	assert_eq!(state, "rows_written");

	*ledger.fail_posts.lock().unwrap() = false;
	let outcome = apply::apply(&pool, &ledger, &plan).await.expect("second apply");

	assert!(!outcome.rows_written, "the rows were already there");
	assert!(outcome.transfers_submitted > 0);
	assert!(outcome.report.ok, "{}", outcome.report.render());
	assert_eq!(ledger.posted("shares_outstanding:service_arb"), 16_250 * E18);
	let state: String = sqlx::query_scalar("SELECT state FROM window_import").fetch_one(&pool).await.unwrap();
	assert_eq!(state, "complete");
}

#[tokio::test]
async fn a_different_export_is_refused_once_one_was_imported() {
	let Some((pool, _g)) = fresh().await else { return };
	let ledger = FakeLedger::default();
	apply::apply(&pool, &ledger, &plan_of(&good_file())).await.expect("first apply");
	let mut other = header();
	other.exported_at = "2026-10-17T12:00:00Z".into();

	let err = apply::apply(&pool, &ledger, &plan_of(&file_of(other, &body(), |_| {}))).await.unwrap_err();

	assert!(matches!(&err, apply::ApplyError::Refused(m) if m.contains("another export")), "{err}");
}

#[tokio::test]
async fn a_damaged_file_never_reaches_the_database() {
	let Some((pool, _g)) = fresh().await else { return };
	let damaged = good_file().replace(r#""record":"valuation""#, r#""record":"valuatio""#);

	assert!(window_import::plan(damaged.as_bytes()).is_err());

	assert_eq!(count(&pool, "window_import").await, 0);
	assert_eq!(count(&pool, "fund_valuations").await, 0);
}

#[tokio::test]
async fn units_already_in_the_ledger_fail_the_read_back_and_the_marker_stays_open() {
	let Some((pool, _g)) = fresh().await else { return };
	let ledger = FakeLedger::default();
	let stray = LedgerTransfer {
		id: 1,
		debit: LedgerAccountKey::parse_logical_key(&format!("shares:service_arb:{C_VAL}")).unwrap(),
		credit: LedgerAccountKey::parse_logical_key("shares_outstanding:service_arb").unwrap(),
		amount: E18,
		code: invest_domain::balance::TransferCode::UnitIssue,
		reference: 1,
	};
	ledger.post(&stray).await.unwrap();

	let outcome = apply::apply(&pool, &ledger, &plan_of(&good_file())).await.unwrap();

	assert!(!outcome.report.ok, "{}", outcome.report.render());
	assert!(outcome.report.checks.iter().any(|c| c.check == "holding_after" && !c.ok && c.subject.ends_with(C_VAL)));
	let state: String = sqlx::query_scalar("SELECT state FROM window_import").fetch_one(&pool).await.unwrap();
	assert_eq!(state, "rows_written", "a read-back that disagrees must not be marked complete");
}

#[tokio::test]
async fn a_row_that_violates_the_schema_rolls_the_whole_import_back() {
	let Some((pool, _g)) = fresh().await else { return };
	let ledger = FakeLedger::default();
	let mut records = body();
	for r in &mut records {
		if let Record::FeeSettlement(f) = r {
			f.units = "0".into(); // the column wants a positive amount
		}
	}

	let err = apply::apply(&pool, &ledger, &plan_of(&file_of(header(), &records, |_| {}))).await.unwrap_err();

	assert!(matches!(err, apply::ApplyError::Database { .. }), "{err}");
	assert_eq!(count(&pool, "window_import").await, 0);
	assert_eq!(count(&pool, "fund_valuations").await, 0);
	assert_eq!(count(&pool, "allocations").await, 2, "only the reserved rows");
	assert_eq!(ledger.touched(), 0, "no unit is posted for rows that did not commit");
}

// ── the command line ───────────────────────────────────────────────────────

fn run_cli(args: &[&str], file_text: &str) -> (Option<i32>, String, String) {
	let dir = std::env::temp_dir().join(format!("invest-window-import-{}-{:x}", std::process::id(), file_text.len()));
	std::fs::create_dir_all(&dir).unwrap();
	let path = dir.join("export.jsonl");
	std::fs::write(&path, file_text).unwrap();
	// No DATABASE_URL and no TigerBeetle in the child: a dry run must not need either.
	let out = std::process::Command::new(env!("CARGO_BIN_EXE_invest-core"))
		.arg("window-import")
		.arg("--file")
		.arg(&path)
		.args(args)
		.env_remove("DATABASE_URL")
		.output()
		.expect("run invest-core");
	std::fs::remove_dir_all(&dir).unwrap();
	(
		out.status.code(),
		String::from_utf8_lossy(&out.stdout).into_owned(),
		String::from_utf8_lossy(&out.stderr).into_owned(),
	)
}

#[test]
fn the_command_dry_runs_a_good_file_without_any_configuration() {
	let (code, stdout, stderr) = run_cli(&[], &good_file());

	assert_eq!(code, Some(0), "{stderr}");
	assert!(stdout.contains("16250 (outstanding 16250) balanced"), "{stdout}");
	assert!(stdout.contains("\"mode\": \"dry_run\""), "{stdout}");
}

#[test]
fn the_command_exits_2_on_a_damaged_file_and_prints_why() {
	let (code, stdout, stderr) = run_cli(&["--dry-run"], &good_file().replace("\"live\":false", "\"live\":true"));

	assert_eq!(code, Some(2));
	assert!(stderr.contains("live = true"), "{stderr}");
	assert!(stdout.is_empty(), "{stdout}");
}
