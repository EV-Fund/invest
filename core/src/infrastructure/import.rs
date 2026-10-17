//! Applies a data-wave file (plan §6.5, step 3): rows into Postgres in one transaction, units
//! into the ledger through the [`Ledger`] port, then the reconciliation against what the
//! ledger now says.
//!
//! Idempotence has two halves because the ledger cannot join the transaction. The relational
//! half commits with its `import_ledger` rows, so it is all-or-nothing and a re-run skips it.
//! The unit half posts under deterministic transfer ids (a re-post is `Exists`, i.e. fine)
//! and files its `import_ledger` rows only afterwards, so a crash in between re-posts rather
//! than loses units. Units are issued as `Dr <holder> / Cr SharesOutstanding` — the
//! in-kind-issuance leg — so the supply invariant holds by construction and the file's own
//! `SharesOutstanding` rows are checked against it, never written.

use std::collections::{BTreeSet, HashMap};

use invest_domain::balance::{LedgerAccountKey, ServiceId, TransferCode};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::{
	application::import::{
		self,
		format::{Entry, ImportFile, Record},
		reconcile::{self, Anchor, Balances, Mode, Report},
	},
	ports::ledger::{Ledger, LedgerError, LedgerTransfer},
};

/// Namespace of the deterministic transfer ids; fixed forever, like the ids it derives.
const TRANSFER_NAMESPACE: Uuid = Uuid::from_u128(0x7e41_90a2_3c5d_5b86_8f10_c2d4_6a9e_b1f7);

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
	#[error("records changed since they were imported (same natural_id, different content):\n{}", .0.join("\n"))]
	Conflict(Vec<String>),
	#[error("{context}: {source}")]
	Database { context: String, source: sqlx::Error },
	#[error("{context}: {source}")]
	Ledger { context: String, source: LedgerError },
	#[error("{0}")]
	Precondition(String),
}

fn db(context: impl Into<String>) -> impl FnOnce(sqlx::Error) -> ImportError {
	let context = context.into();
	move |source| ImportError::Database { context, source }
}

/// What a real run did.
#[derive(Debug)]
pub struct Outcome {
	/// False when the file's own reconciliation failed and nothing was written.
	pub written: bool,
	pub imported: usize,
	pub already_imported: usize,
	pub report: Report,
}

pub async fn run(pool: &PgPool, ledger: &dyn Ledger, file: &ImportFile, anchors: &[Anchor]) -> Result<Outcome, ImportError> {
	// Never write a file that already disagrees with itself: the damage would be units in a
	// ledger that cannot be un-posted.
	let gate = import::dry_run(file, anchors);
	if !gate.ok {
		return Ok(Outcome {
			written: false,
			imported: 0,
			already_imported: 0,
			report: Report { mode: Mode::Applied, ..gate },
		});
	}

	let known: HashMap<(String, String), String> = sqlx::query_as::<_, (String, String, String)>("SELECT kind, natural_id, content_hash FROM import_ledger")
		.fetch_all(pool)
		.await
		.map_err(db("read import_ledger (is migration 0007 applied? start the new invest-core release once first)"))?
		.into_iter()
		.map(|(kind, id, hash)| ((kind, id), hash))
		.collect();

	let conflicts: Vec<String> = file
		.entries
		.iter()
		.filter(|e| known.get(&(e.record.kind().to_owned(), e.natural_id.clone())).is_some_and(|hash| *hash != e.content_hash))
		.map(|e| format!("line {}: {} \"{}\"", e.line, e.record.kind(), e.natural_id))
		.collect();
	if !conflicts.is_empty() {
		return Err(ImportError::Conflict(conflicts));
	}
	let pending: Vec<&Entry> = file.entries.iter().filter(|e| !known.contains_key(&(e.record.kind().to_owned(), e.natural_id.clone()))).collect();

	// Parents before children: policies reference allocations.
	let rank = |e: &Entry| match e.record {
		Record::Allocation(_) => 0,
		_ => 1,
	};
	let mut relational: Vec<&Entry> = pending.iter().copied().filter(|e| !matches!(e.record, Record::Holding(_))).collect();
	relational.sort_by_key(|e| (rank(e), e.line));
	let mut tx = pool.begin().await.map_err(db("begin the import transaction"))?;
	for entry in &relational {
		insert(&mut tx, entry)
			.await
			.map_err(db(format!("line {}: {} \"{}\"", entry.line, entry.record.kind(), entry.natural_id)))?;
		file_in_ledger(&mut tx, entry)
			.await
			.map_err(db(format!("line {}: record the import of \"{}\"", entry.line, entry.natural_id)))?;
	}
	tx.commit().await.map_err(db("commit the import transaction"))?;

	let holdings: Vec<&Entry> = pending.iter().copied().filter(|e| matches!(e.record, Record::Holding(_))).collect();
	post_units(ledger, file, &holdings).await?;
	let mut tx = pool.begin().await.map_err(db("begin the holdings bookkeeping"))?;
	for entry in &holdings {
		file_in_ledger(&mut tx, entry)
			.await
			.map_err(db(format!("line {}: record the import of \"{}\"", entry.line, entry.natural_id)))?;
	}
	tx.commit().await.map_err(db("commit the holdings bookkeeping"))?;

	let after = read_after(pool, ledger, file).await?;
	let report = reconcile::reconcile(file, &after, anchors, Mode::Applied);
	Ok(Outcome {
		written: true,
		imported: pending.len(),
		already_imported: file.entries.len() - pending.len(),
		report,
	})
}

/// Transfer id of a holding: derived from the natural id alone, so an edited file that
/// keeps the id can never post a second leg for it (the content-hash check refuses it).
fn transfer_id(natural_id: &str) -> u128 {
	Uuid::new_v5(&TRANSFER_NAMESPACE, natural_id.as_bytes()).as_u128()
}

async fn post_units(ledger: &dyn Ledger, file: &ImportFile, holdings: &[&Entry]) -> Result<(), ImportError> {
	let wanted: BTreeSet<&str> = holdings.iter().map(|e| e.natural_id.as_str()).collect();
	let mut supplies = BTreeSet::new();
	for (key, held, entry) in file.holdings().filter(|(_, _, e)| wanted.contains(e.natural_id.as_str())) {
		let context = |what: &str| format!("line {}: {what} \"{}\"", entry.line, entry.natural_id);
		let supply_service = ServiceId::parse(&service_of(&key)).map_err(|e| ImportError::Precondition(e.to_string()))?;
		let supply = LedgerAccountKey::SharesOutstanding(supply_service);
		if supplies.insert(supply.logical_key()) {
			ledger.ensure_account(&supply).await.map_err(|source| ImportError::Ledger {
				context: context("open the supply account for"),
				source,
			})?;
		}
		if matches!(key, LedgerAccountKey::SharesOutstanding(_)) {
			continue;
		}
		if held == 0 {
			ledger.ensure_account(&key).await.map_err(|source| ImportError::Ledger { context: context("open"), source })?;
			continue;
		}
		let id = transfer_id(&entry.natural_id);
		let transfer = LedgerTransfer {
			id,
			debit: key,
			credit: supply,
			amount: held,
			code: TransferCode::UnitIssue,
			reference: id,
		};
		ledger.post(&transfer).await.map_err(|source| ImportError::Ledger { context: context("post"), source })?;
	}
	Ok(())
}

fn service_of(key: &LedgerAccountKey) -> String {
	match key {
		LedgerAccountKey::UserShares(s, _) | LedgerAccountKey::BookShares(s, _) | LedgerAccountKey::SharesOutstanding(s) | LedgerAccountKey::FeeShares(s) => s.to_string(),
	}
}

async fn read_after(pool: &PgPool, ledger: &dyn Ledger, file: &ImportFile) -> Result<Balances, ImportError> {
	let extra: Vec<String> = sqlx::query_scalar("SELECT service FROM allocations WHERE scope <> 'invest'")
		.fetch_all(pool)
		.await
		.map_err(db("list the products outside scope invest"))?;
	let keys = reconcile::watched_keys(file, &extra);
	let balances = ledger.balances(&keys).await.map_err(|source| ImportError::Ledger {
		context: "read the balances after the import".into(),
		source,
	})?;
	Ok(keys.iter().zip(balances).map(|(k, b)| (k.logical_key(), b.posted)).collect())
}

async fn file_in_ledger(tx: &mut Transaction<'_, Postgres>, entry: &Entry) -> Result<(), sqlx::Error> {
	sqlx::query("INSERT INTO import_ledger (kind, natural_id, content_hash) VALUES ($1, $2, $3)")
		.bind(entry.record.kind())
		.bind(&entry.natural_id)
		.bind(&entry.content_hash)
		.execute(&mut **tx)
		.await
		.map(drop)
}

/// One record into its table. A plain INSERT on purpose: a row that is already there but
/// was not imported by this ledger is a conflict to look at, not something to overwrite.
async fn insert(tx: &mut Transaction<'_, Postgres>, entry: &Entry) -> Result<(), sqlx::Error> {
	let conn = &mut **tx;
	match &entry.record {
		Record::Allocation(a) => {
			if ServiceId::parse(&a.service).is_ok_and(|s| s.is_reserved()) {
				// The reserved rows ship with migration 0001 under banking's ids; the file
				// must name those, nothing is rewritten.
				let id: Option<String> = sqlx::query_scalar("SELECT id::text FROM allocations WHERE service = $1")
					.bind(&a.service)
					.fetch_optional(&mut *conn)
					.await?;
				return match id {
					Some(id) if id == a.id => Ok(()),
					other => Err(sqlx::Error::Protocol(format!("reserved allocation {} is {other:?} in invest, the file says {}", a.service, a.id))),
				};
			}
			sqlx::query("INSERT INTO allocations (id, service, title, summary, state, unit_cap, icon, access, backing, scope) VALUES ($1::uuid, $2, $3, $4, $5, $6, $7, $8, $9, $10)")
				.bind(&a.id)
				.bind(&a.service)
				.bind(&a.title)
				.bind(&a.summary)
				.bind(&a.state)
				.bind(&a.unit_cap)
				.bind(&a.icon)
				.bind(&a.access)
				.bind(&a.backing)
				.bind(&a.scope)
				.execute(conn)
				.await
				.map(drop)
		}
		Record::FeePolicy(p) => sqlx::query(
			"INSERT INTO fee_policies (service, management_bps, performance_bps, hurdle_bps, basis, crystallization, updated_by, updated_at, version, effective_from) \
			 VALUES ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz, $9, $10::timestamptz)",
		)
		.bind(&p.service)
		.bind(i32::try_from(p.management_bps).unwrap_or(i32::MAX))
		.bind(i32::try_from(p.performance_bps).unwrap_or(i32::MAX))
		.bind(i32::try_from(p.hurdle_bps).unwrap_or(i32::MAX))
		.bind(&p.basis)
		.bind(&p.crystallization)
		.bind(&p.updated_by)
		.bind(&p.updated_at)
		.bind(i32::try_from(p.version).unwrap_or(i32::MAX))
		.bind(&p.effective_from)
		.execute(conn)
		.await
		.map(drop),
		Record::BookPolicy(p) => sqlx::query(
			"INSERT INTO book_policies (service, book_open, taker_fee_bps, price_tick, lot_size, market_slippage_bps, allow_unbacked_trading) VALUES ($1, $2, $3, $4, $5, $6, $7)",
		)
		.bind(&p.service)
		.bind(p.book_open)
		.bind(i32::try_from(p.taker_fee_bps).unwrap_or(i32::MAX))
		.bind(&p.price_tick)
		.bind(&p.lot_size)
		.bind(i32::try_from(p.market_slippage_bps).unwrap_or(i32::MAX))
		.bind(p.allow_unbacked_trading)
		.execute(conn)
		.await
		.map(drop),
		Record::Valuation(v) =>
			sqlx::query("INSERT INTO fund_valuations (id, service, aum, units_outstanding, nav, posted_by, posted_at) VALUES ($1::uuid, $2, $3, $4, $5, $6, $7::timestamptz)")
				.bind(&v.id)
				.bind(&v.service)
				.bind(&v.aum)
				.bind(&v.units_outstanding)
				.bind(&v.nav)
				.bind(&v.posted_by)
				.bind(&v.posted_at)
				.execute(conn)
				.await
				.map(drop),
		Record::FeeAssessment(f) => sqlx::query(
			"INSERT INTO fee_assessments (id, user_id, service, trigger_kind, nav, management, performance, debt_opening, charged_units, charged_cash, debt_carried, high_water_mark, assessed_at) \
			 VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13::timestamptz)",
		)
		.bind(&f.id)
		.bind(&f.user_id)
		.bind(&f.service)
		.bind(&f.trigger_kind)
		.bind(&f.nav)
		.bind(&f.management)
		.bind(&f.performance)
		.bind(&f.debt_opening)
		.bind(&f.charged_units)
		.bind(&f.charged_cash)
		.bind(&f.debt_carried)
		.bind(&f.high_water_mark)
		.bind(&f.assessed_at)
		.execute(conn)
		.await
		.map(drop),
		Record::FeeSettlement(f) => sqlx::query("INSERT INTO fee_settlements (id, service, units, nav, cash, settled_by, settled_at) VALUES ($1::uuid, $2, $3, $4, $5, $6, $7::timestamptz)")
			.bind(&f.id)
			.bind(&f.service)
			.bind(&f.units)
			.bind(&f.nav)
			.bind(&f.cash)
			.bind(&f.settled_by)
			.bind(&f.settled_at)
			.execute(conn)
			.await
			.map(drop),
		// Ownership is a statement about the holdings, checked in the reconciliation; it has
		// no table of its own — the ledger row is its whole footprint.
		Record::Ownership(_) | Record::Holding(_) => Ok(()),
	}
}
