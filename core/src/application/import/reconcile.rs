//! Reconciliation of a data-wave file: pure arithmetic over the file and a view of the
//! balances "after". A dry run builds the view from the file itself, so it proves the file is
//! internally sound without touching anything; a real run builds it from the ledger once the
//! units landed, so the same checks then prove the import did what the file says.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use invest_domain::{
	balance::{AccountCode, LedgerAccountKey, ServiceId},
	money::{Nav, Shares, Usdt},
};
use serde::Serialize;

use super::format::{ImportFile, Record, timestamp, units};

/// Logical account key → posted balance in base units. A key that is absent holds zero.
pub type Balances = BTreeMap<String, u128>;

/// "These holders must be the same before and after": `service=user:units`, units in the
/// decimal notation of the wire (`13000`, `3250.5`). Passed by the runbook, never baked in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Anchor {
	pub service: String,
	pub user: String,
	pub units: u128,
}

impl std::str::FromStr for Anchor {
	type Err = String;

	fn from_str(raw: &str) -> Result<Self, String> {
		let usage = || format!("anchor \"{raw}\" must look like <slug>=<user-uuid>:<units>");
		let (service, rest) = raw.split_once('=').ok_or_else(usage)?;
		let (user, amount) = rest.split_once(':').ok_or_else(usage)?;
		ServiceId::parse(service).map_err(|e| format!("anchor \"{raw}\": {e}"))?;
		uuid::Uuid::parse_str(user).map_err(|_| format!("anchor \"{raw}\": the holder is a user id (UUID), got \"{user}\""))?;
		let units = Shares::parse_decimal(amount).map_err(|e| format!("anchor \"{raw}\": {e}"))?.base_units();
		Ok(Self {
			service: service.to_owned(),
			user: user.to_lowercase(),
			units,
		})
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
	Ok,
	/// Something is wrong; the run exits 1.
	Mismatch,
	/// Could not be verified from what the file carries; reported, not failed.
	Unverified,
}

#[derive(Clone, Debug, Serialize)]
pub struct Check {
	pub check: &'static str,
	pub subject: String,
	pub expected: String,
	pub actual: String,
	pub status: Status,
}

#[derive(Clone, Debug, Serialize)]
pub struct CodeRow {
	pub code: u16,
	pub account: &'static str,
	pub file: String,
	pub after: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct AllocationRow {
	pub service: String,
	pub scope: String,
	pub accounts: Vec<CodeRow>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
	DryRun,
	Applied,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
	pub mode: Mode,
	pub source_sha: String,
	pub ok: bool,
	pub allocations: Vec<AllocationRow>,
	pub checks: Vec<Check>,
}

impl Report {
	/// Table for people, then the JSON for machines — both on stdout, one run.
	pub fn render(&self) -> String {
		let mut out = format!("mode: {:?}  source_sha: {}\n\n", self.mode, self.source_sha);
		out.push_str(&format!("{:<24} {:<8} {:>5} {:<18} {:>28} {:>28}\n", "allocation", "scope", "code", "account", "file", "after"));
		for row in &self.allocations {
			for acc in &row.accounts {
				out.push_str(&format!(
					"{:<24} {:<8} {:>5} {:<18} {:>28} {:>28}\n",
					row.service, row.scope, acc.code, acc.account, acc.file, acc.after
				));
			}
		}
		out.push_str("\nchecks that are not ok:\n");
		let bad: Vec<_> = self.checks.iter().filter(|c| c.status != Status::Ok).collect();
		if bad.is_empty() {
			out.push_str("  none\n");
		}
		for c in bad {
			out.push_str(&format!("  {:?} {} [{}] expected {} actual {}\n", c.status, c.check, c.subject, c.expected, c.actual));
		}
		out.push_str(&format!("\nchecks: {} total, result: {}\n", self.checks.len(), if self.ok { "OK" } else { "MISMATCH" }));
		out
	}
}

/// The balances the file itself states.
pub fn balances_from_file(file: &ImportFile) -> Balances {
	file.holdings().map(|(key, held, _)| (key.logical_key(), held)).collect()
}

/// Every account whose balance the "after" view needs: the file's own, each allocation's
/// supply, and the fee class of every product outside scope invest (which must be empty
/// even when the file says nothing of it). `extra_non_invest` carries such products the
/// database knows and the file does not.
pub fn watched_keys(file: &ImportFile, extra_non_invest: &[String]) -> Vec<LedgerAccountKey> {
	let mut keys: BTreeMap<String, LedgerAccountKey> = file.holdings().map(|(k, ..)| (k.logical_key(), k)).collect();
	for (service, scope) in file.scope_of() {
		let Ok(id) = ServiceId::parse(&service) else { continue };
		let supply = LedgerAccountKey::SharesOutstanding(id.clone());
		keys.insert(supply.logical_key(), supply);
		if scope != "invest" {
			let fee = LedgerAccountKey::FeeShares(id);
			keys.insert(fee.logical_key(), fee);
		}
	}
	for service in extra_non_invest {
		if let Ok(id) = ServiceId::parse(service) {
			let fee = LedgerAccountKey::FeeShares(id);
			keys.insert(fee.logical_key(), fee);
		}
	}
	keys.into_values().collect()
}

const CODES: [(AccountCode, &str); 4] = [
	(AccountCode::UserShares, "UserShares"),
	(AccountCode::SharesOutstanding, "SharesOutstanding"),
	(AccountCode::FeeShares, "FeeShares"),
	(AccountCode::BookShares, "BookShares"),
];

fn of_service(key: &LedgerAccountKey) -> &str {
	match key {
		LedgerAccountKey::UserShares(s, _) | LedgerAccountKey::BookShares(s, _) | LedgerAccountKey::SharesOutstanding(s) | LedgerAccountKey::FeeShares(s) => s.as_str(),
	}
}

/// Σ of one account code of one service over `view`, restricted to the accounts `keys` names.
fn sum_over<'a>(view: &Balances, keys: impl Iterator<Item = &'a LedgerAccountKey>, service: &str, code: AccountCode) -> u128 {
	keys.filter(|k| of_service(k) == service && k.account_code() == code)
		.fold(0u128, |acc, k| acc.saturating_add(view.get(&k.logical_key()).copied().unwrap_or(0)))
}

pub fn reconcile(file: &ImportFile, after: &Balances, anchors: &[Anchor], mode: Mode) -> Report {
	let from_file = balances_from_file(file);
	let scopes = file.scope_of();
	let mut keys: Vec<LedgerAccountKey> = watched_keys(file, &[]);
	// Fee class of products the database holds beyond the file shows up in `after` only.
	for logical in after.keys() {
		if let Ok(key) = LedgerAccountKey::parse_logical_key(logical)
			&& !keys.contains(&key)
		{
			keys.push(key);
		}
	}

	let mut checks = Vec::new();
	let mut push = |check: &'static str, subject: String, expected: String, actual: String| {
		let status = if expected == actual { Status::Ok } else { Status::Mismatch };
		checks.push(Check {
			check,
			subject,
			expected,
			actual,
			status,
		});
	};

	let mut allocations = Vec::new();
	let services: BTreeSet<&String> = scopes.keys().collect();
	for service in services {
		let sum = |view: &Balances, code| sum_over(view, keys.iter(), service, code);
		let mut accounts = Vec::new();
		for (code, name) in CODES {
			let (f, a) = (sum(&from_file, code), sum(after, code));
			accounts.push(CodeRow {
				code: code.code(),
				account: name,
				file: f.to_string(),
				after: a.to_string(),
			});
			push("units_file_vs_after", format!("{service}/{name}"), f.to_string(), a.to_string());
		}
		for (label, view) in [("file", &from_file), ("after", after)] {
			let held = [AccountCode::UserShares, AccountCode::FeeShares, AccountCode::BookShares]
				.into_iter()
				.fold(0u128, |acc, c| acc.saturating_add(sum(view, c)));
			let supply = sum(view, AccountCode::SharesOutstanding);
			push(
				if label == "file" { "supply_equals_holders_file" } else { "supply_equals_holders_after" },
				service.clone(),
				held.to_string(),
				supply.to_string(),
			);
		}
		allocations.push(AllocationRow {
			service: service.clone(),
			scope: scopes[service].clone(),
			accounts,
		});
	}

	// #17: fee units exist only for products of scope invest — also in what the ledger holds.
	for (service, scope) in scopes.iter().filter(|(_, scope)| scope.as_str() != "invest") {
		push(
			"fee_shares_only_for_invest",
			format!("{service} (scope {scope})"),
			"0".into(),
			sum_over(after, keys.iter(), service, AccountCode::FeeShares).to_string(),
		);
	}
	for key in &keys {
		if key.account_code() == AccountCode::FeeShares && !scopes.contains_key(of_service(key)) {
			push(
				"fee_shares_only_for_invest",
				format!("{} (not in the file)", of_service(key)),
				"0".into(),
				after.get(&key.logical_key()).copied().unwrap_or(0).to_string(),
			);
		}
	}

	ownership_checks(file, &from_file, &mut push);
	let nav_check = fee_nav(file, &from_file);

	for a in anchors {
		let key = LedgerAccountKey::parse_logical_key(&format!("shares:{}:{}", a.service, a.user))
			.map(|k| k.logical_key())
			.unwrap_or_default();
		push(
			"anchor_file",
			format!("{}/{}", a.service, a.user),
			a.units.to_string(),
			from_file.get(&key).copied().unwrap_or(0).to_string(),
		);
		push(
			"anchor_after",
			format!("{}/{}", a.service, a.user),
			a.units.to_string(),
			after.get(&key).copied().unwrap_or(0).to_string(),
		);
	}
	checks.push(nav_check);

	let ok = checks.iter().all(|c| c.status != Status::Mismatch);
	Report {
		mode,
		source_sha: file.header.source_sha.clone(),
		ok,
		allocations,
		checks,
	}
}

/// `ownership` records declare the holders of `fee` and `fund`; they must be exactly the
/// `UserShares` of those two allocations — two exports of one fact must agree.
fn ownership_checks(file: &ImportFile, from_file: &Balances, push: &mut impl FnMut(&'static str, String, String, String)) {
	let mut declared: HashMap<String, u128> = HashMap::new();
	for e in &file.entries {
		if let Record::Ownership(o) = &e.record {
			declared.insert(format!("shares:{}:{}", o.allocation, o.holder_user), o.units.parse().unwrap_or(0));
		}
	}
	for (key, units) in &declared {
		push("ownership_matches_holdings", key.clone(), units.to_string(), from_file.get(key).copied().unwrap_or(0).to_string());
	}
	for (key, units) in from_file {
		let reserved_holding = key.starts_with("shares:fee:") || key.starts_with("shares:fund:");
		if reserved_holding && *units > 0 && !declared.contains_key(key) {
			push("ownership_matches_holdings", key.clone(), "0".into(), units.to_string());
		}
	}
}

/// The `fee` allocation's NAV recomputed from the units that remain: the cash it holds plus
/// each invest product's fee class at that product's latest mark, over the units held in
/// `fee`. It must equal the file's latest `fee` valuation. When the file cannot support the
/// arithmetic (no mark, no cash) the check says so instead of passing.
fn fee_nav(file: &ImportFile, from_file: &Balances) -> Check {
	let subject = "fee".to_owned();
	let unverified = |why: &str| Check {
		check: "fee_nav_recomputed",
		subject: subject.clone(),
		expected: "the file's latest fee valuation".into(),
		actual: why.to_owned(),
		status: Status::Unverified,
	};

	// Latest mark per service; a UTC timestamp sorts chronologically as a string.
	let mut latest: HashMap<&str, (String, &super::format::ValuationRec)> = HashMap::new();
	for e in &file.entries {
		if let Record::Valuation(v) = &e.record {
			let Ok(at) = timestamp(&v.posted_at) else { continue };
			if latest.get(v.service.as_str()).is_none_or(|(prev, _)| *prev < at) {
				latest.insert(&v.service, (at, v));
			}
		}
	}
	let Some((_, fee_mark)) = latest.get("fee") else {
		return unverified("no valuation of fee in the file");
	};
	let Some(cash) = fee_mark.cash.as_deref().and_then(|c| c.parse::<u128>().ok()) else {
		return unverified("the fee valuation carries no cash, so its NAV cannot be recomputed");
	};

	let scopes = file.scope_of();
	let mut aum = Usdt::from_base_units(cash);
	for (service, scope) in &scopes {
		let Ok(id) = ServiceId::parse(service) else { continue };
		let class = from_file.get(&LedgerAccountKey::FeeShares(id).logical_key()).copied().unwrap_or(0);
		if scope != "invest" || id_is_reserved(service) || class == 0 {
			continue;
		}
		let Some((_, mark)) = latest.get(service.as_str()) else {
			return unverified(&format!("fee holds {service} units but the file has no valuation of {service}"));
		};
		let Some(value) = mark.nav.parse().ok().map(Nav::from_base_units).and_then(|nav| nav.value(Shares::from_base_units(class)).ok()) else {
			return unverified(&format!("cannot price the {service} fee class"));
		};
		let Some(sum) = aum.checked_add(value) else {
			return unverified("fee AUM overflows");
		};
		aum = sum;
	}

	let holders: u128 = from_file
		.iter()
		.filter(|(k, _)| k.starts_with("shares:fee:") || k.starts_with("book_shares:fee:"))
		.fold(0u128, |acc, (_, v)| acc.saturating_add(*v));
	let Ok(nav) = Nav::from_aum(aum, Shares::from_base_units(holders)) else {
		return unverified("fee has no units outstanding");
	};
	let marked = units("nav", &fee_mark.nav, false).unwrap_or(0);
	Check {
		check: "fee_nav_recomputed",
		subject,
		expected: marked.to_string(),
		actual: nav.base_units().to_string(),
		status: if marked == nav.base_units() { Status::Ok } else { Status::Mismatch },
	}
}

fn id_is_reserved(service: &str) -> bool {
	ServiceId::parse(service).is_ok_and(|s| s.is_reserved())
}
