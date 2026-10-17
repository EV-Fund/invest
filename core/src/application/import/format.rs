//! The data-wave file: JSON-Lines, one header and then one record per line. Parsing and
//! validation are pure — no database, no ledger — so `--dry-run` is exactly this plus
//! [`super::reconcile`]. The format is specified in `docs/MIGRATION-IMPORT.md`; every field
//! here is a column of the table (or a part of the ledger key) it lands in.
//!
//! Every defect is reported with its line number and all of them are collected: an exporter
//! author fixes a file in one pass, and nothing is skipped quietly.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use invest_domain::balance::{AccountCode, LedgerAccountKey, ServiceId};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

/// The only schema version this build reads.
pub const SCHEMA: u64 = 1;

/// Namespace of the content digest. Fixed forever: changing it re-hashes every record and
/// every re-run would read as a conflict.
const HASH_NAMESPACE: Uuid = Uuid::from_u128(0x1a6c_5b0e_7d3f_5e21_9c44_0b7d_a1f2_3e90);

/// A defect in the file, tied to the line that has it (line 0: the file as a whole).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineError {
	pub line: usize,
	pub message: String,
}

impl std::fmt::Display for LineError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		if self.line == 0 {
			write!(f, "file: {}", self.message)
		} else {
			write!(f, "line {}: {}", self.line, self.message)
		}
	}
}

/// Every defect of a file, in line order.
#[derive(Debug, thiserror::Error)]
#[error("{}", .0.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n"))]
pub struct FileErrors(pub Vec<LineError>);

#[derive(Clone, Debug)]
pub struct Header {
	pub exported_at: String,
	pub source: String,
	pub source_sha: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllocationRec {
	pub id: String,
	pub service: String,
	pub title: String,
	#[serde(default)]
	pub summary: String,
	pub state: String,
	pub unit_cap: String,
	pub icon: String,
	pub access: String,
	pub backing: String,
	pub scope: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeePolicyRec {
	pub service: String,
	pub management_bps: u32,
	pub performance_bps: u32,
	pub hurdle_bps: u32,
	pub basis: String,
	pub crystallization: String,
	pub updated_by: String,
	pub updated_at: String,
	pub version: u32,
	pub effective_from: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookPolicyRec {
	pub service: String,
	pub book_open: bool,
	pub taker_fee_bps: u32,
	pub price_tick: String,
	pub lot_size: String,
	pub market_slippage_bps: u32,
	pub allow_unbacked_trading: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValuationRec {
	pub id: String,
	pub service: String,
	pub aum: String,
	pub units_outstanding: String,
	pub nav: String,
	pub posted_by: String,
	pub posted_at: String,
	/// Not a column: the allocation's cash in banking at the mark, which the `fee`
	/// allocation's NAV recomputation needs and invest cannot read from a file.
	pub cash: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssessmentRec {
	pub id: String,
	pub user_id: String,
	pub service: String,
	pub trigger_kind: String,
	pub nav: String,
	pub management: String,
	pub performance: String,
	pub debt_opening: String,
	pub charged_units: String,
	pub charged_cash: String,
	pub debt_carried: String,
	pub high_water_mark: String,
	pub assessed_at: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettlementRec {
	pub id: String,
	pub service: String,
	pub units: String,
	pub nav: String,
	pub cash: String,
	pub settled_by: String,
	pub settled_at: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldingRec {
	pub service: String,
	pub account_code: u16,
	pub holder_user: Option<String>,
	pub holder_allocation: Option<String>,
	pub units: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnershipRec {
	pub allocation: String,
	pub holder_user: String,
	pub units: String,
}

#[derive(Clone, Debug)]
pub enum Record {
	Allocation(AllocationRec),
	FeePolicy(FeePolicyRec),
	BookPolicy(BookPolicyRec),
	Valuation(ValuationRec),
	FeeAssessment(AssessmentRec),
	FeeSettlement(SettlementRec),
	Holding(HoldingRec),
	Ownership(OwnershipRec),
}

impl Record {
	/// The `import_ledger.kind` the record is filed under.
	pub fn kind(&self) -> &'static str {
		match self {
			Self::Allocation(_) => "allocation",
			Self::FeePolicy(_) | Self::BookPolicy(_) => "policy",
			Self::Valuation(_) => "valuation",
			Self::FeeAssessment(_) | Self::FeeSettlement(_) => "fee_event",
			Self::Holding(_) => "share_holding",
			Self::Ownership(_) => "ownership",
		}
	}

	/// The natural id the exporter must have written: derived, so a mapping mistake in the
	/// exporter is a line error instead of a record that can never be matched on re-run.
	fn expected_natural_id(&self) -> Result<String, String> {
		Ok(match self {
			Self::Allocation(r) => r.service.clone(),
			Self::FeePolicy(r) => format!("fee:{}", r.service),
			Self::BookPolicy(r) => format!("book:{}", r.service),
			Self::Valuation(r) => r.id.clone(),
			Self::FeeAssessment(r) => format!("assessment:{}", r.id),
			Self::FeeSettlement(r) => format!("settlement:{}", r.id),
			Self::Holding(r) => holding_key(r)?.logical_key(),
			Self::Ownership(r) => format!("{}:{}", r.allocation, r.holder_user),
		})
	}
}

/// One parsed, validated line.
#[derive(Clone, Debug)]
pub struct Entry {
	pub line: usize,
	pub natural_id: String,
	pub content_hash: String,
	pub record: Record,
}

#[derive(Clone, Debug)]
pub struct ImportFile {
	pub header: Header,
	pub entries: Vec<Entry>,
}

/// The ledger account a `share_holding` record names.
pub fn holding_key(rec: &HoldingRec) -> Result<LedgerAccountKey, String> {
	let service = ServiceId::parse(&rec.service).map_err(|e| e.to_string())?;
	let account = AccountCode::from_code(rec.account_code).ok_or_else(|| format!("unknown account_code {}", rec.account_code))?;
	let user = |what: &str| -> Result<(), String> {
		match (&rec.holder_user, &rec.holder_allocation) {
			(Some(u), None) => uuid(u).map(drop),
			_ => Err(format!("{what} needs holder_user and no holder_allocation")),
		}
	};
	match account {
		AccountCode::UserShares | AccountCode::BookShares => {
			user(&format!("account_code {}", rec.account_code))?;
			let holder = rec.holder_user.as_deref().unwrap_or_default();
			let prefix = if account == AccountCode::UserShares { "shares" } else { "book_shares" };
			LedgerAccountKey::parse_logical_key(&format!("{prefix}:{service}:{holder}")).map_err(|e| e.to_string())
		}
		AccountCode::FeeShares => match (&rec.holder_user, rec.holder_allocation.as_deref()) {
			(None, Some("fee")) => Ok(LedgerAccountKey::FeeShares(service)),
			_ => Err("FeeShares (62) needs holder_allocation \"fee\" and no holder_user".into()),
		},
		AccountCode::SharesOutstanding => {
			if rec.holder_user.is_some() || rec.holder_allocation.is_some() {
				return Err("SharesOutstanding (61) takes no holder".into());
			}
			Ok(LedgerAccountKey::SharesOutstanding(service))
		}
		AccountCode::RetiredCompanyStake | AccountCode::BookCash => Err(format!("account_code {} is retired and carries no units here", rec.account_code)),
	}
}

/// Parse and validate a whole file. All defects come back together.
pub fn parse(text: &str) -> Result<ImportFile, FileErrors> {
	let mut errors = Vec::new();
	let mut lines = text.lines().enumerate().map(|(i, l)| (i + 1, l));
	let header = match lines.next() {
		None => {
			errors.push(LineError {
				line: 0,
				message: "file is empty".into(),
			});
			None
		}
		Some((n, raw)) => parse_header(raw).map_err(|message| errors.push(LineError { line: n, message })).ok(),
	};
	// A bad header means the schema is not one we know: the rest cannot be read honestly.
	let Some(header) = header else {
		return Err(FileErrors(errors));
	};

	let mut entries = Vec::new();
	for (n, raw) in lines {
		if raw.trim().is_empty() {
			errors.push(LineError {
				line: n,
				message: "blank line".into(),
			});
			continue;
		}
		match parse_entry(n, raw) {
			Ok(entry) => entries.push(entry),
			Err(message) => errors.push(LineError { line: n, message }),
		}
	}
	validate(&entries, &mut errors);
	if errors.is_empty() {
		Ok(ImportFile { header, entries })
	} else {
		errors.sort_by_key(|e| e.line);
		Err(FileErrors(errors))
	}
}

fn parse_header(raw: &str) -> Result<Header, String> {
	let value: Value = serde_json::from_str(raw).map_err(|e| format!("malformed header: {e}"))?;
	let obj = value.as_object().ok_or("header must be a JSON object")?;
	match obj.get("schema").and_then(Value::as_u64) {
		Some(SCHEMA) => {}
		Some(other) => return Err(format!("unsupported schema {other} (this build reads {SCHEMA})")),
		None => return Err("header needs an integer \"schema\"".into()),
	}
	let text = |key: &str| -> Result<String, String> {
		match obj.get(key).and_then(Value::as_str) {
			Some(s) if !s.is_empty() => Ok(s.to_owned()),
			_ => Err(format!("header needs a non-empty string \"{key}\"")),
		}
	};
	if let Some(extra) = obj.keys().find(|k| !matches!(k.as_str(), "schema" | "exported_at" | "source" | "source_sha")) {
		return Err(format!("unknown header field \"{extra}\""));
	}
	let exported_at = text("exported_at")?;
	timestamp(&exported_at).map_err(|e| format!("exported_at: {e}"))?;
	Ok(Header {
		exported_at,
		source: text("source")?,
		source_sha: text("source_sha")?,
	})
}

fn parse_entry(line: usize, raw: &str) -> Result<Entry, String> {
	let value: Value = serde_json::from_str(raw).map_err(|e| format!("malformed JSON: {e}"))?;
	let Value::Object(mut obj) = value.clone() else {
		return Err("a record must be a JSON object".into());
	};
	let take = |obj: &mut serde_json::Map<String, Value>, key: &str| -> Result<String, String> {
		match obj.remove(key) {
			Some(Value::String(s)) if !s.is_empty() => Ok(s),
			_ => Err(format!("missing or non-string \"{key}\"")),
		}
	};
	let kind = take(&mut obj, "kind")?;
	let natural_id = take(&mut obj, "natural_id")?;
	if natural_id.len() > 200 {
		return Err("natural_id is longer than 200 bytes".into());
	}
	let body = Value::Object(std::mem::take(&mut obj));
	let ctx = |e: serde_json::Error| format!("{kind}: {e}");
	let record = match kind.as_str() {
		"allocation" => Record::Allocation(serde_json::from_value(body).map_err(ctx)?),
		"policy" => {
			let mut body = body;
			match body.as_object_mut().and_then(|o| o.remove("policy")) {
				Some(Value::String(p)) if p == "fee" => Record::FeePolicy(serde_json::from_value(body).map_err(ctx)?),
				Some(Value::String(p)) if p == "book" => Record::BookPolicy(serde_json::from_value(body).map_err(ctx)?),
				_ => return Err("policy needs \"policy\": \"fee\" | \"book\"".into()),
			}
		}
		"valuation" => Record::Valuation(serde_json::from_value(body).map_err(ctx)?),
		"fee_event" => {
			let mut body = body;
			match body.as_object_mut().and_then(|o| o.remove("event")) {
				Some(Value::String(p)) if p == "assessment" => Record::FeeAssessment(serde_json::from_value(body).map_err(ctx)?),
				Some(Value::String(p)) if p == "settlement" => Record::FeeSettlement(serde_json::from_value(body).map_err(ctx)?),
				_ => return Err("fee_event needs \"event\": \"assessment\" | \"settlement\"".into()),
			}
		}
		"share_holding" => Record::Holding(serde_json::from_value(body).map_err(ctx)?),
		"ownership" => Record::Ownership(serde_json::from_value(body).map_err(ctx)?),
		other => return Err(format!("unknown kind \"{other}\"")),
	};
	check_record(&record)?;
	let expected = record.expected_natural_id()?;
	if natural_id != expected {
		return Err(format!("natural_id must be \"{expected}\", got \"{natural_id}\""));
	}
	Ok(Entry {
		line,
		natural_id,
		content_hash: content_hash(&value),
		record,
	})
}

/// Canonical form: object keys sorted at every depth, so a re-ordered but equal record
/// hashes the same.
fn canonical(value: &Value, out: &mut String) {
	match value {
		Value::Object(map) => {
			let sorted: BTreeMap<_, _> = map.iter().collect();
			out.push('{');
			for (i, (k, v)) in sorted.into_iter().enumerate() {
				if i > 0 {
					out.push(',');
				}
				out.push_str(&Value::String(k.clone()).to_string());
				out.push(':');
				canonical(v, out);
			}
			out.push('}');
		}
		Value::Array(items) => {
			out.push('[');
			for (i, v) in items.iter().enumerate() {
				if i > 0 {
					out.push(',');
				}
				canonical(v, out);
			}
			out.push(']');
		}
		scalar => out.push_str(&scalar.to_string()),
	}
}

/// 32 hex digits. A UUIDv5 (SHA-1) of the canonical record: this detects a record that
/// changed between exports, which is all the ledger needs it for, and needs no hashing
/// dependency beyond the uuid crate the crate already links.
fn content_hash(value: &Value) -> String {
	let mut text = String::new();
	canonical(value, &mut text);
	Uuid::new_v5(&HASH_NAMESPACE, text.as_bytes()).simple().to_string()
}

// ── field validators — each mirrors the CHECK of the column it lands in ──────────────────

fn uuid(raw: &str) -> Result<Uuid, String> {
	let parsed = Uuid::parse_str(raw).map_err(|_| format!("\"{raw}\" is not a UUID"))?;
	if parsed.to_string() != raw {
		return Err(format!("UUID \"{raw}\" must be lowercase and hyphenated"));
	}
	Ok(parsed)
}

fn slug(raw: &str) -> Result<ServiceId, String> {
	let id = ServiceId::parse(raw).map_err(|e| e.to_string())?;
	if id.as_str() != raw {
		return Err(format!("service \"{raw}\" has surrounding whitespace"));
	}
	Ok(id)
}

/// Base units as decimal digits; fits `u128` because the ledger takes no more.
pub fn units(field: &str, raw: &str, positive: bool) -> Result<u128, String> {
	if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
		return Err(format!("{field} must be an integer string of digits, got \"{raw}\""));
	}
	let value: u128 = raw.parse().map_err(|_| format!("{field} \"{raw}\" does not fit 128 bits"))?;
	if raw.len() > 1 && raw.starts_with('0') {
		return Err(format!("{field} \"{raw}\" has leading zeros"));
	}
	if positive && value == 0 {
		return Err(format!("{field} must be positive"));
	}
	Ok(value)
}

fn one_of(field: &str, raw: &str, allowed: &[&str]) -> Result<(), String> {
	if allowed.contains(&raw) {
		Ok(())
	} else {
		Err(format!("{field} \"{raw}\" is not one of {allowed:?}"))
	}
}

fn bps(field: &str, value: u32, max: u32) -> Result<(), String> {
	if value <= max { Ok(()) } else { Err(format!("{field} {value} is above {max}")) }
}

fn text(field: &str, raw: &str, max: usize) -> Result<(), String> {
	let len = raw.chars().count();
	if (1..=max).contains(&len) {
		Ok(())
	} else {
		Err(format!("{field} must be 1..{max} characters"))
	}
}

/// An RFC 3339 instant in UTC (`…Z`), returned as a string that sorts chronologically.
/// UTC only: the exporter converts, and ordering never depends on offsets.
pub fn timestamp(raw: &str) -> Result<String, String> {
	let bad = || format!("\"{raw}\" is not an RFC 3339 UTC instant like 2026-10-16T12:00:00Z");
	let body = raw.strip_suffix('Z').ok_or_else(bad)?;
	let (main, frac) = match body.split_once('.') {
		Some((m, f)) if !f.is_empty() && f.len() <= 9 && f.bytes().all(|b| b.is_ascii_digit()) => (m, f),
		Some(_) => return Err(bad()),
		None => (body, ""),
	};
	let b = main.as_bytes();
	let digit = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
	let shape =
		b.len() == 19 && [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18].iter().all(|&i| digit(i)) && b[4] == b'-' && b[7] == b'-' && b[10] == b'T' && b[13] == b':' && b[16] == b':';
	if !shape {
		return Err(bad());
	}
	let n = |r: std::ops::Range<usize>| main[r].parse::<u32>().unwrap_or(99);
	if !((1..=12).contains(&n(5..7)) && (1..=31).contains(&n(8..10)) && n(11..13) < 24 && n(14..16) < 60 && n(17..19) < 61) {
		return Err(bad());
	}
	Ok(format!("{main}.{frac:0<9}"))
}

fn check_record(record: &Record) -> Result<(), String> {
	match record {
		Record::Allocation(r) => {
			uuid(&r.id)?;
			let service = slug(&r.service)?;
			text("title", &r.title, 120)?;
			if r.summary.chars().count() > 280 {
				return Err("summary is longer than 280 characters".into());
			}
			one_of("state", &r.state, &["draft", "open", "closed"])?;
			units("unit_cap", &r.unit_cap, true)?;
			one_of(
				"icon",
				&r.icon,
				&["fund", "real_estate", "trading", "yield", "venture", "treasury", "commodity", "credit", "index", "arbitrage"],
			)?;
			one_of("access", &r.access, &["hidden", "view", "invest"])?;
			one_of("backing", &r.backing, &["cash", "in_kind"])?;
			let scope_ok =
				r.scope.len() <= 32 && r.scope.chars().next().is_some_and(|c| c.is_ascii_lowercase()) && r.scope.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
			if !scope_ok {
				return Err(format!("scope \"{}\" is not a scope slug", r.scope));
			}
			if service.is_reserved() && r.scope != "invest" {
				return Err(format!("reserved allocation \"{service}\" lives in scope invest"));
			}
			Ok(())
		}
		Record::FeePolicy(r) => {
			slug(&r.service)?;
			bps("management_bps", r.management_bps, 500)?;
			bps("performance_bps", r.performance_bps, 5000)?;
			bps("hurdle_bps", r.hurdle_bps, 10000)?;
			one_of("basis", &r.basis, &["invested_capital", "market_value"])?;
			one_of("crystallization", &r.crystallization, &["monthly", "quarterly", "semi_annual", "annual"])?;
			text("updated_by", &r.updated_by, 256)?;
			timestamp(&r.updated_at)?;
			timestamp(&r.effective_from)?;
			if r.version == 0 { Err("version must be >= 1".into()) } else { Ok(()) }
		}
		Record::BookPolicy(r) => {
			slug(&r.service)?;
			bps("taker_fee_bps", r.taker_fee_bps, 10000)?;
			bps("market_slippage_bps", r.market_slippage_bps, 10000)?;
			units("price_tick", &r.price_tick, true)?;
			units("lot_size", &r.lot_size, true).map(drop)
		}
		Record::Valuation(r) => {
			uuid(&r.id)?;
			slug(&r.service)?;
			units("aum", &r.aum, false)?;
			units("units_outstanding", &r.units_outstanding, true)?;
			units("nav", &r.nav, false)?;
			text("posted_by", &r.posted_by, 256)?;
			timestamp(&r.posted_at)?;
			r.cash.as_deref().map_or(Ok(0), |c| units("cash", c, false)).map(drop)
		}
		Record::FeeAssessment(r) => {
			uuid(&r.id)?;
			uuid(&r.user_id)?;
			slug(&r.service)?;
			one_of("trigger_kind", &r.trigger_kind, &["period", "redemption"])?;
			units("nav", &r.nav, true)?;
			for (field, raw) in [
				("management", &r.management),
				("performance", &r.performance),
				("debt_opening", &r.debt_opening),
				("charged_units", &r.charged_units),
				("charged_cash", &r.charged_cash),
				("debt_carried", &r.debt_carried),
				("high_water_mark", &r.high_water_mark),
			] {
				units(field, raw, false)?;
			}
			timestamp(&r.assessed_at).map(drop)
		}
		Record::FeeSettlement(r) => {
			uuid(&r.id)?;
			slug(&r.service)?;
			units("units", &r.units, true)?;
			units("nav", &r.nav, true)?;
			units("cash", &r.cash, true)?;
			text("settled_by", &r.settled_by, 256)?;
			timestamp(&r.settled_at).map(drop)
		}
		Record::Holding(r) => {
			holding_key(r)?;
			let held = units("units", &r.units, false)?;
			if r.account_code == AccountCode::BookShares.code() && held != 0 {
				return Err("BookShares (64) must be 0: open orders are cancelled before the export, and their orders are not imported".into());
			}
			Ok(())
		}
		Record::Ownership(r) => {
			if !matches!(r.allocation.as_str(), "fee" | "fund") {
				return Err("ownership.allocation must be \"fee\" or \"fund\"".into());
			}
			uuid(&r.holder_user)?;
			units("units", &r.units, true).map(drop)
		}
	}
}

/// Rules that need more than one record.
fn validate(entries: &[Entry], errors: &mut Vec<LineError>) {
	let mut err = |line: usize, message: String| errors.push(LineError { line, message });

	let mut seen: HashMap<(&'static str, &str), usize> = HashMap::new();
	for e in entries {
		if let Some(first) = seen.insert((e.record.kind(), e.natural_id.as_str()), e.line) {
			err(e.line, format!("duplicate {} \"{}\" (first on line {first})", e.record.kind(), e.natural_id));
		}
	}

	let mut scopes: HashMap<&str, &str> = HashMap::from([("fee", "invest"), ("fund", "invest")]);
	for e in entries {
		if let Record::Allocation(a) = &e.record {
			scopes.insert(&a.service, &a.scope);
		}
	}
	let known = |service: &str| scopes.contains_key(service);

	let mut with_units = BTreeSet::new();
	let mut with_supply = BTreeSet::new();
	for e in entries {
		let service = match &e.record {
			Record::Allocation(_) => continue,
			Record::FeePolicy(r) => &r.service,
			Record::BookPolicy(r) => &r.service,
			Record::Valuation(r) => &r.service,
			Record::FeeAssessment(r) => &r.service,
			Record::FeeSettlement(r) => &r.service,
			Record::Holding(r) => &r.service,
			Record::Ownership(r) => &r.allocation,
		};
		if !known(service) {
			err(e.line, format!("service \"{service}\" has no allocation record"));
			continue;
		}
		match &e.record {
			// Reserved allocations take no fee terms either way: their fee class is the fee.
			Record::FeePolicy(r) if scopes.get(r.service.as_str()) != Some(&"invest") || ServiceId::parse(&r.service).is_ok_and(|s| s.is_reserved()) => {
				err(e.line, format!("fee policy for \"{}\": only products of scope invest take fee terms", r.service));
			}
			Record::Holding(r) => {
				let held = r.units.parse::<u128>().unwrap_or(0);
				let not_invest = scopes.get(r.service.as_str()) != Some(&"invest") || ServiceId::parse(&r.service).is_ok_and(|s| s.is_reserved());
				if r.account_code == AccountCode::FeeShares.code() && not_invest {
					err(
						e.line,
						format!(
							"FeeShares (62) of \"{}\": fee units exist only for products of scope invest — burn them in banking before the export",
							r.service
						),
					);
				}
				if r.account_code == AccountCode::SharesOutstanding.code() {
					with_supply.insert(r.service.clone());
				} else if held > 0 {
					with_units.insert(r.service.clone());
				}
			}
			_ => {}
		}
	}
	for service in with_units.difference(&with_supply) {
		err(0, format!("\"{service}\" has holdings but no SharesOutstanding (61) record"));
	}
}

impl ImportFile {
	/// Allocation records by slug, plus the reserved pair, which exist from migration 0001.
	pub fn scope_of(&self) -> HashMap<String, String> {
		let mut map = HashMap::from([("fee".to_owned(), "invest".to_owned()), ("fund".to_owned(), "invest".to_owned())]);
		for e in &self.entries {
			if let Record::Allocation(a) = &e.record {
				map.insert(a.service.clone(), a.scope.clone());
			}
		}
		map
	}

	/// Every `share_holding` as `(account, base units)`, in file order.
	pub fn holdings(&self) -> impl Iterator<Item = (LedgerAccountKey, u128, &Entry)> {
		self.entries.iter().filter_map(|e| match &e.record {
			// `check_record` already proved the key and the units parse.
			Record::Holding(h) => Some((holding_key(h).ok()?, h.units.parse().ok()?, e)),
			_ => None,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const HEADER: &str = r#"{"schema":1,"exported_at":"2026-10-16T12:00:00Z","source":"banking","source_sha":"abc"}"#;
	const USER: &str = "11111111-1111-4111-8111-111111111111";

	fn alloc(service: &str, scope: &str) -> String {
		format!(
			r#"{{"kind":"allocation","natural_id":"{service}","id":"22222222-2222-4222-8222-222222222222","service":"{service}","title":"T","state":"open","unit_cap":"1000","icon":"fund","access":"view","backing":"cash","scope":"{scope}"}}"#
		)
	}

	fn file(lines: &[String]) -> String {
		std::iter::once(HEADER.to_owned()).chain(lines.iter().cloned()).collect::<Vec<_>>().join("\n")
	}

	#[test]
	fn a_good_file_parses() {
		let user = format!(r#"{{"kind":"share_holding","natural_id":"shares:alpha:{USER}","service":"alpha","account_code":60,"holder_user":"{USER}","units":"5"}}"#);
		let supply = r#"{"kind":"share_holding","natural_id":"shares_outstanding:alpha","service":"alpha","account_code":61,"units":"5"}"#.to_owned();
		let parsed = parse(&file(&[alloc("alpha", "invest"), user, supply])).expect("a good file parses");
		assert_eq!(parsed.entries.len(), 3);
		assert_eq!(parsed.header.source, "banking");
	}

	#[test]
	fn an_unknown_schema_is_refused() {
		let err = parse(&HEADER.replace("\"schema\":1", "\"schema\":2")).unwrap_err();
		assert_eq!(err.0[0].line, 1);
		assert!(err.0[0].message.contains("unsupported schema 2"));
	}

	#[test]
	fn an_unknown_kind_names_its_line() {
		let err = parse(&file(&[alloc("alpha", "invest"), r#"{"kind":"mystery","natural_id":"x"}"#.into()])).unwrap_err();
		assert_eq!(err.0.len(), 1);
		assert_eq!(err.0[0].line, 3);
		assert!(err.0[0].message.contains("unknown kind"));
	}

	#[test]
	fn malformed_lines_are_reported_with_their_numbers_not_skipped() {
		let err = parse(&file(&["{not json".into(), alloc("alpha", "invest"), r#"{"kind":"allocation"}"#.into()])).unwrap_err();
		let lines: Vec<usize> = err.0.iter().map(|e| e.line).collect();
		assert_eq!(lines, vec![2, 4]);
	}

	#[test]
	fn an_unknown_field_is_refused() {
		let line = alloc("alpha", "invest").replace("\"scope\"", "\"extra\":1,\"scope\"");
		assert!(parse(&file(&[line])).unwrap_err().0[0].message.contains("unknown field"));
	}

	#[test]
	fn fee_shares_of_a_non_invest_product_are_a_hard_error() {
		let fee = r#"{"kind":"share_holding","natural_id":"shares_fee:sa_prod","service":"sa_prod","account_code":62,"holder_allocation":"fee","units":"1"}"#.to_owned();
		let err = parse(&file(&[alloc("sa_prod", "sa"), fee])).unwrap_err();
		assert!(err.0.iter().any(|e| e.line == 3 && e.message.contains("only for products of scope invest")), "{err}");
	}

	#[test]
	fn the_natural_id_is_checked_against_the_record() {
		let line = alloc("alpha", "invest").replace("\"natural_id\":\"alpha\"", "\"natural_id\":\"beta\"");
		assert!(parse(&file(&[line])).unwrap_err().0[0].message.contains("natural_id must be \"alpha\""));
	}

	#[test]
	fn key_order_does_not_change_the_hash() {
		let a: Value = serde_json::from_str(r#"{"a":1,"b":{"c":2,"d":3}}"#).unwrap();
		let b: Value = serde_json::from_str(r#"{"b":{"d":3,"c":2},"a":1}"#).unwrap();
		assert_eq!(content_hash(&a), content_hash(&b));
	}

	#[test]
	fn timestamps_are_utc_rfc3339() {
		assert!(timestamp("2026-10-16T12:00:00Z").is_ok());
		assert!(timestamp("2026-10-16T12:00:00.5Z").is_ok());
		assert!(timestamp("2026-10-16T12:00:00+02:00").is_err());
		assert!(timestamp("2026-13-16T12:00:00Z").is_err());
	}
}
