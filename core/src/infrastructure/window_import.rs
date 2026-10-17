//! Applies a [`Plan`] (plan §6.5, step 3): the registry rows into Postgres in ONE transaction
//! together with the `window_import` marker, then the units into the ledger through the
//! [`Ledger`] port, then a read-back of the ledger against the file.
//!
//! Idempotence has two halves because the ledger cannot join the transaction. The relational
//! half is all-or-nothing and remembered by the marker row, so a re-run skips it. The unit
//! half posts under transfer ids derived from the account's logical key, so a re-post is
//! `Exists` — fine — and a crash between the halves is healed by the next run. The marker
//! turns `complete` only after the read-back matched, so a run that did not verify keeps
//! verifying on every re-run. Units are issued as `Dr <holder> / Cr SharesOutstanding` (the
//! in-kind-issuance leg), so the supply invariant holds by construction and the file's own
//! supply is what the read-back is compared with, never written.
//!
//! Posts are sequential on purpose: a window holds a handful of holders, and a bounded,
//! ordered run is easier to read in a log than a fan-out.

use std::collections::BTreeMap;

use invest_domain::balance::{LedgerAccountKey, TransferCode};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
	application::window_import::{
		AnchorHolder, Plan, is_digest,
		report::{Check, Mode, Report, human},
	},
	ports::ledger::{HoldingScope, Ledger, LedgerError, LedgerTransfer},
};

/// Namespace of the deterministic transfer ids; fixed forever, like the ids it derives.
const TRANSFER_NAMESPACE: Uuid = Uuid::from_u128(0x5c2a_71e4_08b9_5d36_a4f1_9e07_d3b8_6c52);

/// `pg_advisory_xact_lock` key: two concurrent applies of the window queue up behind it.
const APPLY_LOCK: i64 = 0x0057_494e_444f_5731; // "WINDOW1"

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
	/// The database is not in a state the window can be applied to.
	#[error("{0}")]
	Refused(String),
	#[error("{context}: {source}")]
	Database { context: String, source: sqlx::Error },
	#[error("{context}: {source}")]
	Ledger { context: String, source: LedgerError },
}

fn db(context: impl Into<String>) -> impl FnOnce(sqlx::Error) -> ApplyError {
	let context = context.into();
	move |source| ApplyError::Database { context, source }
}

fn ledger_err(context: impl Into<String>) -> impl FnOnce(LedgerError) -> ApplyError {
	let context = context.into();
	move |source| ApplyError::Ledger { context, source }
}

#[derive(Debug)]
pub struct Outcome {
	pub report: Report,
	/// The registry rows were written by this run (false: an earlier run already had).
	pub rows_written: bool,
	/// Unit transfers submitted by this run (a re-submit of an applied one is a no-op).
	pub transfers_submitted: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Marker {
	/// This run wrote the rows.
	Written,
	/// An earlier run wrote the rows; the units may or may not be in the ledger.
	RowsThere,
	/// An earlier run finished and verified.
	Complete,
}

/// What `--apply` is bound to before it may write anything.
pub struct Guards<'a> {
	/// The digest the operator read off the dry-run (or banking's side).
	pub expect_digest: &'a str,
	/// The `service_arb` holders the operator expects.
	pub anchors: &'a [AnchorHolder],
	/// `TIGERBEETLE_CLUSTER_ID` of THIS process. An export taken from the very cluster we
	/// would write to is not an export from banking.
	pub own_cluster_id: &'a str,
}

fn same_cluster(a: &str, b: &str) -> bool {
	match (a.trim().parse::<u128>(), b.trim().parse::<u128>()) {
		(Ok(a), Ok(b)) => a == b,
		_ => a.trim() == b.trim(),
	}
}

/// Every refusal that needs no write to be known, in one place and before the first one.
async fn preflight(ledger: &dyn Ledger, plan: &Plan, guards: &Guards<'_>) -> Result<(), ApplyError> {
	if !is_digest(guards.expect_digest) {
		return Err(ApplyError::Refused("--expect-digest must be 64 lower-case hex characters".into()));
	}
	if guards.expect_digest != plan.digest {
		return Err(ApplyError::Refused(format!(
			"the file's SHA-256 is {}, --expect-digest says {}; nothing was written",
			plan.digest, guards.expect_digest
		)));
	}
	if same_cluster(&plan.header.tigerbeetle_cluster_id, guards.own_cluster_id) {
		return Err(ApplyError::Refused(format!(
			"the export was read from TigerBeetle cluster {}, which is the cluster this run would write to; the file is not from banking's ledger",
			plan.header.tigerbeetle_cluster_id
		)));
	}
	if let crate::application::window_import::Anchor::Broken(why) = plan.anchor(guards.anchors) {
		return Err(ApplyError::Refused(format!("anchor BROKEN: {why}")));
	}
	check_ledger_untouched(ledger, plan).await
}

/// Before the first write, invest's ledger holds nothing of the window's products, or holds
/// exactly what the file says (a finished run, re-run). Anything in between is another
/// source of units we cannot reason about: refuse rather than mix.
async fn check_ledger_untouched(ledger: &dyn Ledger, plan: &Plan) -> Result<(), ApplyError> {
	for t in &plan.totals {
		let service = invest_domain::balance::ServiceId::parse(&t.service).map_err(|e| ApplyError::Refused(e.to_string()))?;
		let expected: BTreeMap<String, u128> = plan
			.holdings
			.iter()
			.filter(|h| h.service == service && h.units != 0)
			.map(|h| (h.key.logical_key(), h.units))
			.collect();
		let actual: BTreeMap<String, u128> = ledger
			.share_holdings(&HoldingScope::Product(service.clone()))
			.await
			.map_err(ledger_err(format!("read the holdings of \"{}\"", t.service)))?
			.into_iter()
			.filter(|(_, units)| *units != 0)
			.map(|(key, units)| (key.logical_key(), units))
			.collect();
		let supply = ledger
			.balance(&LedgerAccountKey::SharesOutstanding(service))
			.await
			.map_err(ledger_err(format!("read the supply of \"{}\"", t.service)))?
			.posted;
		let empty = actual.is_empty() && supply == 0;
		let exact = actual == expected && supply == t.outstanding;
		if !empty && !exact {
			return Err(ApplyError::Refused(format!(
				"invest's ledger already holds units of \"{}\" (supply {}, {} holdings) that are neither nothing nor exactly the file's; nothing was written",
				t.service,
				human(supply),
				actual.len()
			)));
		}
	}
	Ok(())
}

pub async fn apply(pool: &PgPool, ledger: &dyn Ledger, plan: &Plan, guards: &Guards<'_>) -> Result<Outcome, ApplyError> {
	preflight(ledger, plan, guards).await?;
	let marker = write_rows(pool, plan).await?;
	let mut transfers_submitted = 0;
	if marker != Marker::Complete {
		transfers_submitted = post_units(ledger, plan).await?;
	}

	let mode = if marker == Marker::Complete { Mode::AlreadyApplied } else { Mode::Applied };
	let checks = verify(ledger, plan).await?;
	let mut report = Report::of_plan(plan, mode, guards.anchors).with_checks(checks);
	report.warnings.extend(holders_unknown_to_the_projection(pool, plan).await?);

	if report.ok && marker != Marker::Complete {
		sqlx::query("UPDATE window_import SET state = 'complete', completed_at = now() WHERE file_digest = $1")
			.bind(&plan.digest)
			.execute(pool)
			.await
			.map_err(db("mark the window import complete"))?;
	}
	Ok(Outcome {
		report,
		rows_written: marker == Marker::Written,
		transfers_submitted,
	})
}

async fn write_rows(pool: &PgPool, plan: &Plan) -> Result<Marker, ApplyError> {
	let mut tx = pool.begin().await.map_err(db("begin the window import transaction"))?;
	sqlx::query("SELECT pg_advisory_xact_lock($1)")
		.bind(APPLY_LOCK)
		.execute(&mut *tx)
		.await
		.map_err(db("take the window import lock"))?;
	let marker: Option<(String, String)> = sqlx::query_as("SELECT file_digest, state FROM window_import")
		.fetch_optional(&mut *tx)
		.await
		.map_err(db("read window_import (is migration 0008 applied? start the new invest-core release once first)"))?;
	match marker {
		Some((digest, _)) if digest != plan.digest => {
			return Err(ApplyError::Refused(format!(
				"another export was already imported (file digest {digest}, this file {}); one window, one file. Nothing was written",
				plan.digest
			)));
		}
		Some((_, state)) if state == "complete" => return Ok(Marker::Complete),
		Some(_) => return Ok(Marker::RowsThere),
		None => {}
	}

	insert_all(&mut tx, plan).await?;
	sqlx::query("INSERT INTO window_import (file_digest, source, exported_at, tigerbeetle_cluster_id, state) VALUES ($1, $2, $3, $4, 'rows_written')")
		.bind(&plan.digest)
		.bind(&plan.header.source)
		.bind(&plan.header.exported_at)
		.bind(&plan.header.tigerbeetle_cluster_id)
		.execute(&mut *tx)
		.await
		.map_err(db("record the window import"))?;
	tx.commit().await.map_err(db("commit the window import transaction"))?;
	Ok(Marker::Written)
}

fn bps(context: &str, value: u32) -> Result<i32, ApplyError> {
	i32::try_from(value).map_err(|_| ApplyError::Refused(format!("{context}: {value} does not fit a basis-points column")))
}

fn concierge(plan: &Plan, banking_user: &str) -> Result<Uuid, ApplyError> {
	plan.concierge(banking_user).map_err(ApplyError::Refused)
}

/// Plain INSERTs on purpose: a row that is already there is a conflict to look at, not
/// something to overwrite; the transaction rolls back and nothing is written.
async fn insert_all(conn: &mut PgConnection, plan: &Plan) -> Result<(), ApplyError> {
	for a in &plan.allocations {
		let context = format!("allocation \"{}\"", a.service);
		if invest_domain::balance::ServiceId::parse(&a.service).is_ok_and(|s| s.is_reserved()) {
			// The reserved rows ship with migration 0001 under banking's ids; nothing is
			// rewritten, but the file must name those.
			let id: Option<String> = sqlx::query_scalar("SELECT id::text FROM allocations WHERE service = $1")
				.bind(&a.service)
				.fetch_optional(&mut *conn)
				.await
				.map_err(db(&context))?;
			if id.as_deref() != Some(a.id.as_str()) {
				return Err(ApplyError::Refused(format!("{context}: invest has {id:?}, the file says {}", a.id)));
			}
			continue;
		}
		sqlx::query(
			"INSERT INTO allocations (id, service, title, summary, state, unit_cap, icon, access, backing, scope, created_at, updated_at) \
			 VALUES ($1::uuid, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::timestamptz, $12::timestamptz)",
		)
		.bind(&a.id)
		.bind(&a.service)
		.bind(&a.title)
		.bind(&a.summary)
		.bind(&a.state)
		.bind(&a.unit_cap)
		.bind(&a.icon)
		.bind(&a.access)
		.bind(&a.backing)
		.bind(crate::application::window_import::scope_of(&a.service))
		.bind(&a.created_at)
		.bind(&a.updated_at)
		.execute(&mut *conn)
		.await
		.map_err(db(context))?;
	}

	for p in &plan.fee_policies {
		let context = format!("fee_policy of \"{}\"", p.service);
		sqlx::query(
			"INSERT INTO fee_policies (service, management_bps, performance_bps, hurdle_bps, basis, crystallization, updated_by, updated_at, version, effective_from) \
			 VALUES ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz, $9, $10::timestamptz)",
		)
		.bind(&p.service)
		.bind(bps(&context, p.management_bps)?)
		.bind(bps(&context, p.performance_bps)?)
		.bind(bps(&context, p.hurdle_bps)?)
		.bind(&p.basis)
		.bind(&p.crystallization)
		.bind(&p.updated_by)
		.bind(&p.updated_at)
		.bind(bps(&context, p.version)?)
		.bind(&p.effective_from)
		.execute(&mut *conn)
		.await
		.map_err(db(context))?;
	}

	for c in &plan.fee_policy_changes {
		let context = format!("fee_policy_change {} of \"{}\"", c.id, c.service);
		let waived = c
			.notices_waived_users
			.as_ref()
			.map(|users| users.iter().map(|u| concierge(plan, u)).collect::<Result<Vec<_>, _>>())
			.transpose()?;
		sqlx::query(
			"INSERT INTO fee_policy_changes (id, service, version, management_bps, performance_bps, hurdle_bps, basis, crystallization, state, requirement, \
			 effective_from, requested_by, requested_at, reason, consilium_id, scheduled_at, applied_at, closed_reason, notices_waived_by, notices_waived_at, notices_waived_users) \
			 VALUES ($1::uuid, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::timestamptz, $12, $13::timestamptz, $14, $15::uuid, $16::timestamptz, $17::timestamptz, $18, $19, $20::timestamptz, $21)",
		)
		.bind(&c.id)
		.bind(&c.service)
		.bind(bps(&context, c.version)?)
		.bind(bps(&context, c.management_bps)?)
		.bind(bps(&context, c.performance_bps)?)
		.bind(bps(&context, c.hurdle_bps)?)
		.bind(&c.basis)
		.bind(&c.crystallization)
		.bind(&c.state)
		.bind(&c.requirement)
		.bind(&c.effective_from)
		.bind(&c.requested_by)
		.bind(&c.requested_at)
		.bind(&c.reason)
		.bind(&c.consilium_id)
		.bind(&c.scheduled_at)
		.bind(&c.applied_at)
		.bind(&c.closed_reason)
		.bind(&c.notices_waived_by)
		.bind(&c.notices_waived_at)
		.bind(waived)
		.execute(&mut *conn)
		.await
		.map_err(db(context))?;
	}

	for p in &plan.book_policies {
		let context = format!("book_policy of \"{}\"", p.service);
		sqlx::query("INSERT INTO book_policies (service, book_open, taker_fee_bps, price_tick, lot_size, market_slippage_bps, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz)")
			.bind(&p.service)
			.bind(p.book_open)
			.bind(bps(&context, p.taker_fee_bps)?)
			.bind(&p.price_tick)
			.bind(&p.lot_size)
			.bind(bps(&context, p.market_slippage_bps)?)
			.bind(&p.updated_at)
			.execute(&mut *conn)
			.await
			.map_err(db(context))?;
	}

	for v in &plan.valuations {
		sqlx::query("INSERT INTO fund_valuations (id, service, aum, units_outstanding, nav, posted_by, posted_at) VALUES ($1::uuid, $2, $3, $4, $5, $6, $7::timestamptz)")
			.bind(&v.id)
			.bind(&v.service)
			.bind(&v.aum)
			.bind(&v.units_outstanding)
			.bind(&v.nav)
			.bind(&v.posted_by)
			.bind(&v.posted_at)
			.execute(&mut *conn)
			.await
			.map_err(db(format!("valuation {} of \"{}\"", v.id, v.service)))?;
	}

	for p in &plan.positions {
		sqlx::query(
			"INSERT INTO fund_positions (user_id, service, cost_basis, high_water_mark, units, fee_debt, fees_accrued_at, crystallized_at, created_at, updated_at) \
			 VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz, $8::timestamptz, $9::timestamptz, $10::timestamptz)",
		)
		.bind(concierge(plan, &p.user_id)?)
		.bind(&p.service)
		.bind(&p.cost_basis)
		.bind(&p.high_water_mark)
		.bind(&p.units)
		.bind(&p.fee_debt)
		.bind(&p.fees_accrued_at)
		.bind(&p.crystallized_at)
		.bind(&p.created_at)
		.bind(&p.updated_at)
		.execute(&mut *conn)
		.await
		.map_err(db(format!("position of {} in \"{}\"", p.user_id, p.service)))?;
	}

	for f in &plan.fee_assessments {
		sqlx::query(
			"INSERT INTO fee_assessments (id, user_id, service, trigger_kind, nav, management, performance, debt_opening, charged_units, charged_cash, debt_carried, high_water_mark, assessed_at) \
			 VALUES ($1::uuid, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13::timestamptz)",
		)
		.bind(&f.id)
		.bind(concierge(plan, &f.user_id)?)
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
		.execute(&mut *conn)
		.await
		.map_err(db(format!("fee_assessment {} of \"{}\"", f.id, f.service)))?;
	}

	for f in &plan.fee_settlements {
		sqlx::query("INSERT INTO fee_settlements (id, service, units, nav, cash, settled_by, settled_at) VALUES ($1::uuid, $2, $3, $4, $5, $6, $7::timestamptz)")
			.bind(&f.id)
			.bind(&f.service)
			.bind(&f.units)
			.bind(&f.nav)
			.bind(&f.cash)
			.bind(&f.settled_by)
			.bind(&f.settled_at)
			.execute(&mut *conn)
			.await
			.map_err(db(format!("fee_settlement {} of \"{}\"", f.id, f.service)))?;
	}
	Ok(())
}

/// Transfer id of a holding: derived from the account's logical key alone, so a re-run — or
/// a run of a re-exported file — can never post a second leg for the same account.
fn transfer_id(key: &LedgerAccountKey) -> u128 {
	Uuid::new_v5(&TRANSFER_NAMESPACE, key.logical_key().as_bytes()).as_u128()
}

async fn post_units(ledger: &dyn Ledger, plan: &Plan) -> Result<usize, ApplyError> {
	for t in &plan.totals {
		let supply = LedgerAccountKey::SharesOutstanding(invest_domain::balance::ServiceId::parse(&t.service).map_err(|e| ApplyError::Refused(e.to_string()))?);
		ledger
			.ensure_account(&supply)
			.await
			.map_err(ledger_err(format!("open the supply account of \"{}\"", t.service)))?;
	}
	let mut submitted = 0;
	for holding in &plan.holdings {
		if holding.units == 0 {
			continue;
		}
		let id = transfer_id(&holding.key);
		let transfer = LedgerTransfer {
			id,
			debit: holding.key.clone(),
			credit: LedgerAccountKey::SharesOutstanding(holding.service.clone()),
			amount: holding.units,
			code: TransferCode::UnitIssue,
			reference: id,
		};
		ledger.post(&transfer).await.map_err(ledger_err(format!("post {}", holding.key.logical_key())))?;
		submitted += 1;
	}
	Ok(submitted)
}

/// The ledger, read back, against the file: every holder's units, the supply, and — the part
/// a plain "did my posts succeed" would miss — units that were already in the ledger and are
/// not in the file.
async fn verify(ledger: &dyn Ledger, plan: &Plan) -> Result<Vec<Check>, ApplyError> {
	let mut checks = Vec::new();
	for t in &plan.totals {
		let service = invest_domain::balance::ServiceId::parse(&t.service).map_err(|e| ApplyError::Refused(e.to_string()))?;
		let expected: BTreeMap<String, u128> = plan
			.holdings
			.iter()
			.filter(|h| h.service == service && h.units != 0)
			.map(|h| (h.key.logical_key(), h.units))
			.collect();
		let actual: BTreeMap<String, u128> = ledger
			.share_holdings(&HoldingScope::Product(service.clone()))
			.await
			.map_err(ledger_err(format!("read the holdings of \"{}\"", t.service)))?
			.into_iter()
			.filter(|(_, units)| *units != 0)
			.map(|(key, units)| (key.logical_key(), units))
			.collect();

		for (key, units) in &expected {
			let got = actual.get(key).copied().unwrap_or(0);
			checks.push(Check {
				check: "holding_after",
				subject: key.clone(),
				expected: human(*units),
				actual: human(got),
				ok: got == *units,
			});
		}
		for (key, units) in actual.iter().filter(|(key, _)| !expected.contains_key(*key)) {
			checks.push(Check {
				check: "unexpected_holding_after",
				subject: key.clone(),
				expected: "0".into(),
				actual: human(*units),
				ok: false,
			});
		}
		let held = actual.values().try_fold(0u128, |a, b| a.checked_add(*b));
		checks.push(Check {
			check: "held_after",
			subject: t.service.clone(),
			expected: t.held().map_or_else(|| "overflow".into(), human),
			actual: held.map_or_else(|| "overflow".into(), human),
			ok: held.is_some() && held == t.held(),
		});
		let supply = ledger
			.balance(&LedgerAccountKey::SharesOutstanding(service))
			.await
			.map_err(ledger_err(format!("read the supply of \"{}\"", t.service)))?
			.posted;
		checks.push(Check {
			check: "supply_after",
			subject: t.service.clone(),
			expected: human(t.outstanding),
			actual: human(supply),
			ok: supply == t.outstanding,
		});
	}
	Ok(checks)
}

/// A holder the `users` projection has not heard of reads as no holding at all (fail
/// closed). Not the importer's to write — concierge's stream is the projection's one writer —
/// but the operator should know before the cutover.
async fn holders_unknown_to_the_projection(pool: &PgPool, plan: &Plan) -> Result<Vec<String>, ApplyError> {
	let wanted: Vec<Uuid> = plan
		.totals
		.iter()
		.flat_map(|t| t.holders.keys().copied())
		.collect::<std::collections::BTreeSet<_>>()
		.into_iter()
		.collect();
	let known: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM users WHERE id = ANY($1)")
		.bind(&wanted)
		.fetch_all(pool)
		.await
		.map_err(db("look the holders up in the users projection"))?;
	Ok(wanted
		.into_iter()
		.filter(|u| !known.contains(u))
		.map(|u| format!("holder {u} is not in invest's users projection yet; until concierge's stream delivers them they see no holding"))
		.collect())
}
