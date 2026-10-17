//! The data-wave importer against a real Postgres and an in-memory unit ledger. The ledger is
//! a fake on purpose: what is under test is the importer's bookkeeping (idempotence,
//! conflicts, dry-run purity, reconciliation) and the importer only reaches TigerBeetle
//! through the `Ledger` port, whose real adapter has its own suites.

use std::{
	collections::{HashMap, HashSet},
	sync::Mutex,
};

use async_trait::async_trait;
use invest_core::{
	application::import::{
		self,
		format::{self, ImportFile},
		reconcile::{Anchor, Status},
	},
	infrastructure::import as apply,
	ports::ledger::{HoldingScope, Ledger, LedgerBalance, LedgerError, LedgerTransfer, PendingCompletion},
};
use invest_domain::{
	architecture::Gateway,
	balance::{LedgerAccountKey, Normal},
};
use sqlx::PgPool;

mod common;

const U1: &str = "11111111-1111-4111-8111-111111111111";
const U3: &str = "33333333-3333-4333-8333-333333333333";
const E18: u128 = 1_000_000_000_000_000_000;

#[derive(Default)]
struct FakeLedger {
	sides: Mutex<HashMap<String, (u128, u128)>>,
	seen: Mutex<HashSet<u128>>,
	posts: Mutex<usize>,
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
}

impl Gateway for FakeLedger {}

#[async_trait]
impl Ledger for FakeLedger {
	async fn ensure_account(&self, key: &LedgerAccountKey) -> Result<(), LedgerError> {
		self.sides.lock().unwrap().entry(key.logical_key()).or_default();
		Ok(())
	}

	async fn balance(&self, key: &LedgerAccountKey) -> Result<LedgerBalance, LedgerError> {
		let (debits, credits) = self.sides.lock().unwrap().get(&key.logical_key()).copied().unwrap_or_default();
		let posted = match key.normal() {
			Normal::Debit => debits - credits,
			Normal::Credit => credits - debits,
		};
		Ok(LedgerBalance { posted, pending: 0, locked: 0 })
	}

	async fn transfer_exists(&self, id: u128) -> Result<bool, LedgerError> {
		Ok(self.seen.lock().unwrap().contains(&id))
	}

	async fn post(&self, t: &LedgerTransfer) -> Result<(), LedgerError> {
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

	async fn share_holdings(&self, _: &HoldingScope) -> Result<Vec<(LedgerAccountKey, u128)>, LedgerError> {
		Ok(Vec::new())
	}
}

fn holding(service: &str, code: u16, holder: &str, units: u128) -> String {
	let (key, who) = match code {
		60 => (format!("shares:{service}:{holder}"), format!(r#""holder_user":"{holder}","#)),
		61 => (format!("shares_outstanding:{service}"), String::new()),
		_ => (format!("shares_fee:{service}"), r#""holder_allocation":"fee","#.to_owned()),
	};
	format!(r#"{{"kind":"share_holding","natural_id":"{key}","service":"{service}","account_code":{code},{who}"units":"{units}"}}"#)
}

fn allocation(id: &str, service: &str, scope: &str, title: &str) -> String {
	format!(
		r#"{{"kind":"allocation","natural_id":"{service}","id":"{id}","service":"{service}","title":"{title}","state":"open","unit_cap":"{}","icon":"fund","access":"view","backing":"cash","scope":"{scope}"}}"#,
		1000 * E18
	)
}

/// A file that reconciles: an invest product with a fee class, the `fee` allocation held by
/// one person, and an `sa` product with 13 000 units held by one person.
fn lines(title: &str, inv_supply: u128) -> Vec<String> {
	vec![
		r#"{"schema":1,"exported_at":"2026-10-16T12:00:00Z","source":"banking","source_sha":"deadbeef"}"#.into(),
		allocation("680240f5-1c41-58e2-987c-e52dbe2faa01", "fee", "invest", "Fee allocation"),
		allocation("aaaaaaaa-0000-4000-8000-000000000001", "imp_inv", "invest", title),
		allocation("aaaaaaaa-0000-4000-8000-000000000002", "imp_sa", "sa", "Service arb"),
		r#"{"kind":"policy","policy":"fee","natural_id":"fee:imp_inv","service":"imp_inv","management_bps":200,"performance_bps":2000,"hurdle_bps":0,"basis":"invested_capital","crystallization":"quarterly","updated_by":"admin","updated_at":"2026-09-01T00:00:00Z","version":1,"effective_from":"2026-09-01T00:00:00Z"}"#.into(),
		format!(r#"{{"kind":"valuation","natural_id":"bbbbbbbb-0000-4000-8000-000000000001","id":"bbbbbbbb-0000-4000-8000-000000000001","service":"imp_inv","aum":"{}","units_outstanding":"{}","nav":"{E18}","posted_by":"admin","posted_at":"2026-10-15T00:00:00Z"}}"#, 10 * E18, 10 * E18),
		format!(r#"{{"kind":"valuation","natural_id":"bbbbbbbb-0000-4000-8000-000000000002","id":"bbbbbbbb-0000-4000-8000-000000000002","service":"fee","aum":"{}","units_outstanding":"{}","nav":"{E18}","posted_by":"admin","posted_at":"2026-10-15T00:00:00Z","cash":"{}"}}"#, 5 * E18, 5 * E18, 3 * E18),
		format!(r#"{{"kind":"fee_event","event":"settlement","natural_id":"settlement:cccccccc-0000-4000-8000-000000000001","id":"cccccccc-0000-4000-8000-000000000001","service":"imp_inv","units":"{E18}","nav":"{E18}","cash":"{E18}","settled_by":"admin","settled_at":"2026-10-10T00:00:00Z"}}"#),
		holding("imp_inv", 60, U1, 8 * E18),
		holding("imp_inv", 62, "", 2 * E18),
		holding("imp_inv", 61, "", inv_supply),
		holding("imp_sa", 60, U1, 13000 * E18),
		holding("imp_sa", 61, "", 13000 * E18),
		holding("fee", 60, U3, 5 * E18),
		holding("fee", 61, "", 5 * E18),
		format!(r#"{{"kind":"ownership","natural_id":"fee:{U3}","allocation":"fee","holder_user":"{U3}","units":"{}"}}"#, 5 * E18),
	]
}

fn parse(lines: &[String]) -> ImportFile {
	format::parse(&lines.join("\n")).unwrap_or_else(|e| panic!("fixture must parse:\n{e}"))
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
	for table in ["import_ledger", "fund_valuations", "fee_settlements", "fee_assessments", "fee_policies", "book_policies"] {
		sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}"))).execute(&pool).await.unwrap();
	}
	sqlx::query("DELETE FROM allocations WHERE service NOT IN ('fee', 'fund')").execute(&pool).await.unwrap();
	Some((pool, guard))
}

fn anchor(units: &str) -> Anchor {
	format!("imp_sa={U1}:{units}").parse().unwrap()
}

#[tokio::test]
async fn dry_run_writes_nothing() {
	let Some((pool, _g)) = fresh().await else { return };
	let file = parse(&lines("Inv", 10 * E18));
	let before = (count(&pool, "import_ledger").await, count(&pool, "allocations").await, count(&pool, "fund_valuations").await);

	let report = import::dry_run(&file, &[anchor("13000")]);

	assert!(report.ok, "{}", report.render());
	assert_eq!(report.checks.iter().find(|c| c.check == "fee_nav_recomputed").unwrap().status, Status::Ok);
	let after = (count(&pool, "import_ledger").await, count(&pool, "allocations").await, count(&pool, "fund_valuations").await);
	assert_eq!(before, after);
}

#[tokio::test]
async fn a_real_run_imports_and_a_rerun_imports_nothing() {
	let Some((pool, _g)) = fresh().await else { return };
	let ledger = FakeLedger::default();
	let file = parse(&lines("Inv", 10 * E18));
	let anchors = [anchor("13000")];

	let first = apply::run(&pool, &ledger, &file, &anchors).await.expect("first run");
	assert!(first.written && first.report.ok, "{}", first.report.render());
	assert_eq!(first.imported, file.entries.len());
	assert_eq!(count(&pool, "allocations").await, 4, "fee, fund, imp_inv, imp_sa");
	assert_eq!(count(&pool, "fee_policies").await, 1);
	assert_eq!(count(&pool, "fund_valuations").await, 2);
	assert_eq!(count(&pool, "fee_settlements").await, 1);
	assert_eq!(count(&pool, "import_ledger").await, i64::try_from(file.entries.len()).unwrap());
	let settled = ledger.touched();

	let second = apply::run(&pool, &ledger, &file, &anchors).await.expect("second run");
	assert_eq!(second.imported, 0);
	assert_eq!(second.already_imported, file.entries.len());
	assert!(second.report.ok);
	assert_eq!(ledger.touched(), settled, "a re-run must not touch the ledger");
	assert_eq!(count(&pool, "fund_valuations").await, 2);
}

#[tokio::test]
async fn the_same_natural_id_with_other_content_is_a_hard_error() {
	let Some((pool, _g)) = fresh().await else { return };
	let ledger = FakeLedger::default();
	apply::run(&pool, &ledger, &parse(&lines("Inv", 10 * E18)), &[]).await.expect("first run");

	let err = apply::run(&pool, &ledger, &parse(&lines("Renamed", 10 * E18)), &[]).await.unwrap_err();

	assert!(matches!(&err, apply::ImportError::Conflict(ids) if ids.iter().any(|i| i.contains("\"imp_inv\""))), "{err}");
	let title: String = sqlx::query_scalar("SELECT title FROM allocations WHERE service = 'imp_inv'").fetch_one(&pool).await.unwrap();
	assert_eq!(title, "Inv");
}

#[tokio::test]
async fn a_supply_that_is_not_the_sum_of_holders_is_detected_and_nothing_is_written() {
	let Some((pool, _g)) = fresh().await else { return };
	let ledger = FakeLedger::default();
	let file = parse(&lines("Inv", 11 * E18));

	let dry = import::dry_run(&file, &[]);
	assert!(!dry.ok);
	assert!(
		dry.checks
			.iter()
			.any(|c| c.check == "supply_equals_holders_file" && c.subject == "imp_inv" && c.status == Status::Mismatch)
	);

	let outcome = apply::run(&pool, &ledger, &file, &[]).await.unwrap();
	assert!(!outcome.written && !outcome.report.ok);
	assert_eq!(count(&pool, "import_ledger").await, 0);
	assert_eq!(ledger.touched(), 0);
}

#[tokio::test]
async fn an_anchor_that_differs_is_detected() {
	let file = parse(&lines("Inv", 10 * E18));
	let report = import::dry_run(&file, &[anchor("13001")]);
	assert!(!report.ok);
	assert!(report.checks.iter().any(|c| c.check == "anchor_file" && c.status == Status::Mismatch));
}

#[tokio::test]
async fn fee_shares_of_a_non_invest_product_are_refused_at_parse() {
	let mut file = lines("Inv", 10 * E18);
	file.push(holding("imp_sa", 62, "", E18));
	let err = format::parse(&file.join("\n")).unwrap_err();
	assert!(err.0.iter().any(|e| e.message.contains("only for products of scope invest")), "{err}");
}

#[tokio::test]
async fn units_already_in_the_ledger_show_up_as_an_after_mismatch() {
	let Some((pool, _g)) = fresh().await else { return };
	let ledger = FakeLedger::default();
	let stray = LedgerTransfer {
		id: 1,
		debit: LedgerAccountKey::parse_logical_key(&format!("shares:imp_inv:{U1}")).unwrap(),
		credit: LedgerAccountKey::parse_logical_key("shares_outstanding:imp_inv").unwrap(),
		amount: E18,
		code: invest_domain::balance::TransferCode::UnitIssue,
		reference: 1,
	};
	ledger.post(&stray).await.unwrap();

	let outcome = apply::run(&pool, &ledger, &parse(&lines("Inv", 10 * E18)), &[]).await.unwrap();

	assert!(outcome.written && !outcome.report.ok);
	assert!(
		outcome
			.report
			.checks
			.iter()
			.any(|c| c.check == "units_file_vs_after" && c.subject == "imp_inv/UserShares" && c.status == Status::Mismatch)
	);
}
