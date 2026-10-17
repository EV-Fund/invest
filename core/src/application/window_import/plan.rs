//! Reading and checking the export file. Every defect is collected, none is skipped quietly:
//! the operator fixes the export (or the window) in one pass. A file that fails here is never
//! partly applied — the plan either exists whole or does not.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use banking_domain::window_export::{Allocation, BookPolicy, FeeAssessment, FeePolicy, FeePolicyChange, FeeSettlement, HoldingKind, Position, Record, SCHEMA_VERSION, Valuation};
use invest_domain::{
	allocations::reserved_allocation_id,
	balance::{LedgerAccountKey, ServiceId},
	users::UserId,
};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use super::report::human;

/// The one product whose cash lives outside scope `invest` today (plan §6.5 step 4:
/// `service_arb` in `sa`, the rest in `invest`). The file does not carry a scope — banking's
/// registry has none — so the rule is stated here, once.
pub const SA_SERVICE: &str = "service_arb";

/// SHA-256 of the file's bytes, lower-case hex (64 chars). The same digest banking's
/// `close-share-accounts` checks, so the operator carries one value across both sides.
pub fn digest_of(bytes: &[u8]) -> String {
	use std::fmt::Write as _;
	Sha256::digest(bytes).iter().fold(String::with_capacity(64), |mut hex, b| {
		// Writing to a String cannot fail.
		let _ = write!(hex, "{b:02x}");
		hex
	})
}

/// Whether `raw` is a digest as [`digest_of`] writes it.
pub fn is_digest(raw: &str) -> bool {
	raw.len() == 64 && raw.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One holder the operator expects to see in `service_arb`, in whole units as typed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnchorHolder {
	pub holder: Uuid,
	pub units: u128,
}

impl std::str::FromStr for AnchorHolder {
	type Err = String;

	/// `<concierge-uuid>:<units>`, units a decimal with up to 18 places (`13000`, `3250.5`).
	fn from_str(raw: &str) -> Result<Self, String> {
		let usage = || format!("--anchor-holder \"{raw}\": expected <concierge_uuid>:<units>");
		let (holder, units) = raw.split_once(':').ok_or_else(usage)?;
		let holder = Uuid::parse_str(holder).map_err(|_| usage())?;
		let (whole, frac) = units.split_once('.').unwrap_or((units, ""));
		if whole.is_empty() || frac.len() > 18 || !whole.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
			return Err(usage());
		}
		let base = format!("{whole}{frac:0<18}")
			.parse::<u128>()
			.map_err(|_| format!("--anchor-holder \"{raw}\": units do not fit 128 bits"))?;
		Ok(Self { holder, units: base })
	}
}

/// The verdict on the `service_arb` test anchor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Anchor {
	/// `service_arb` is not in the file: nothing to anchor.
	NotApplicable,
	Ok,
	Broken(String),
}

impl Anchor {
	pub fn is_broken(&self) -> bool {
		matches!(self, Self::Broken(_))
	}
}

pub fn scope_of(service: &str) -> &'static str {
	if service == SA_SERVICE { "sa" } else { "invest" }
}

/// Everything wrong with a file, one defect per entry.
#[derive(Debug, thiserror::Error)]
#[error("{}", .0.join("\n"))]
pub struct Refusals(pub Vec<String>);

impl Refusals {
	fn one(message: impl Into<String>) -> Self {
		Self(vec![message.into()])
	}
}

/// One unit balance to post: `Dr key / Cr SharesOutstanding(service)`.
#[derive(Clone, Debug)]
pub struct Holding {
	pub service: ServiceId,
	pub key: LedgerAccountKey,
	pub units: u128,
}

/// Σ of one allocation's units by where they sit, recomputed from the holdings.
#[derive(Clone, Debug, Default)]
pub struct Totals {
	pub service: String,
	pub scope: &'static str,
	pub user_shares: u128,
	pub book_shares: u128,
	pub fee_shares: u128,
	pub outstanding: u128,
	/// Concierge user id → `user_shares + book_shares`.
	pub holders: BTreeMap<Uuid, u128>,
}

impl Totals {
	/// `None` on overflow; planning refuses such a file, so a plan's totals are always `Some`.
	pub fn held(&self) -> Option<u128> {
		self.user_shares.checked_add(self.book_shares)?.checked_add(self.fee_shares)
	}
}

/// `*slot += units`, or a defect on overflow.
fn add(problems: &mut Vec<String>, what: impl FnOnce() -> String, slot: &mut u128, units: u128) {
	match slot.checked_add(units) {
		Some(sum) => *slot = sum,
		None => problems.push(format!("{} overflows 128 bits", what())),
	}
}

#[derive(Debug)]
pub struct Plan {
	/// Identity of the file's bytes; what the apply remembers so a second, different export
	/// is never layered over the first.
	pub digest: String,
	pub header: banking_domain::window_export::Header,
	pub allocations: Vec<Allocation>,
	/// Only products of scope `invest`: another scope takes no fee terms (D24(3)).
	pub fee_policies: Vec<FeePolicy>,
	pub fee_policy_changes: Vec<FeePolicyChange>,
	pub book_policies: Vec<BookPolicy>,
	pub valuations: Vec<Valuation>,
	pub positions: Vec<Position>,
	pub fee_assessments: Vec<FeeAssessment>,
	pub fee_settlements: Vec<FeeSettlement>,
	/// banking user id → concierge user id, for every user the file names.
	pub users: BTreeMap<String, Uuid>,
	pub holdings: Vec<Holding>,
	pub totals: Vec<Totals>,
	pub notes: Vec<String>,
	pub warnings: Vec<String>,
}

impl Plan {
	/// The concierge id of a banking user. Every id the file mentions was resolved while
	/// planning, so a miss here is a bug in the caller, reported rather than guessed.
	/// Does `service_arb` hold exactly what the operator expects? Exactly the given holders,
	/// each with exactly their units in `UserShares`, nothing else (no book, no fee class),
	/// and the supply equal to their sum. No flags given is BROKEN: an unchecked anchor is
	/// not an anchor.
	pub fn anchor(&self, expected: &[AnchorHolder]) -> Anchor {
		let Some(sa) = self.totals.iter().find(|t| t.service == SA_SERVICE) else {
			return Anchor::NotApplicable;
		};
		if expected.is_empty() {
			return Anchor::Broken(format!("{SA_SERVICE} is in the file but no --anchor-holder was given"));
		}
		let wanted: BTreeMap<Uuid, u128> = expected.iter().map(|a| (a.holder, a.units)).collect();
		if wanted.len() != expected.len() {
			return Anchor::Broken("--anchor-holder names the same holder twice".into());
		}
		let mut problems = Vec::new();
		for (holder, units) in &wanted {
			let got = sa.holders.get(holder).copied().unwrap_or(0);
			if got != *units {
				problems.push(format!("{holder}: expected {}, the file has {}", human(*units), human(got)));
			}
		}
		for (holder, units) in sa.holders.iter().filter(|(h, u)| **u != 0 && !wanted.contains_key(*h)) {
			problems.push(format!("{holder}: {} units, not an expected holder", human(*units)));
		}
		let sum = wanted.values().try_fold(0u128, |a, b| a.checked_add(*b));
		if sum != Some(sa.outstanding) || sa.book_shares != 0 || sa.fee_shares != 0 {
			problems.push(format!("supply is {}, expected holders sum to {}", human(sa.outstanding), sum.map_or("overflow".into(), human)));
		}
		if problems.is_empty() { Anchor::Ok } else { Anchor::Broken(problems.join("; ")) }
	}

	pub fn concierge(&self, banking_user: &str) -> Result<Uuid, String> {
		self.users
			.get(banking_user)
			.copied()
			.ok_or_else(|| format!("banking user {banking_user} has no concierge id in the plan"))
	}
}

/// Parse and check `bytes` (the whole file). `Err` lists every defect found.
pub fn plan(bytes: &[u8]) -> Result<Plan, Refusals> {
	let text = std::str::from_utf8(bytes).map_err(|e| Refusals::one(format!("file: not UTF-8: {e}")))?;
	check_schema_version(text)?;

	let mut errors = Vec::new();
	let mut records = Vec::new();
	for (index, line) in text.lines().enumerate() {
		let n = index + 1;
		if line.trim().is_empty() {
			errors.push(format!("line {n}: blank line"));
			continue;
		}
		match serde_json::from_str::<Record>(line) {
			Ok(record) => records.push((n, record)),
			Err(e) => errors.push(format!("line {n}: {e}")),
		}
	}
	if !errors.is_empty() {
		return Err(Refusals(errors));
	}

	let mut b = Builder::default();
	b.read(records);
	b.finish(digest_of(bytes))
}

/// The version gate runs before any typed parse: a file of another version may well not
/// parse as this one, and "unknown schema_version" is the answer the operator needs.
fn check_schema_version(text: &str) -> Result<(), Refusals> {
	let first = text.lines().next().ok_or_else(|| Refusals::one("file: empty"))?;
	let value: serde_json::Value = serde_json::from_str(first).map_err(|e| Refusals::one(format!("line 1: not JSON: {e}")))?;
	if value.get("record").and_then(|r| r.as_str()) != Some("header") {
		return Err(Refusals::one("line 1: the first line must be the header record"));
	}
	match value.get("schema_version").and_then(|v| v.as_u64()) {
		Some(v) if v == u64::from(SCHEMA_VERSION) => Ok(()),
		Some(v) => Err(Refusals::one(format!("line 1: schema_version {v}, this build reads only {SCHEMA_VERSION}"))),
		None => Err(Refusals::one("line 1: the header has no schema_version")),
	}
}

fn digits(context: &str, raw: &str) -> Result<u128, String> {
	if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
		return Err(format!("{context}: \"{raw}\" is not a decimal digit string"));
	}
	raw.parse::<u128>().map_err(|_| format!("{context}: \"{raw}\" does not fit 128 bits"))
}

#[derive(Default)]
struct Builder {
	errors: Vec<String>,
	header: Option<(usize, banking_domain::window_export::Header)>,
	trailer: Option<(usize, banking_domain::window_export::Trailer)>,
	counts: BTreeMap<&'static str, u64>,
	last_line: usize,
	allocations: Vec<(usize, Allocation)>,
	fee_policies: Vec<(usize, FeePolicy)>,
	fee_policy_changes: Vec<(usize, FeePolicyChange)>,
	book_policies: Vec<(usize, BookPolicy)>,
	valuations: Vec<(usize, Valuation)>,
	positions: Vec<(usize, Position)>,
	fee_assessments: Vec<(usize, FeeAssessment)>,
	fee_settlements: Vec<(usize, FeeSettlement)>,
	user_refs: Vec<(usize, banking_domain::window_export::UserRef)>,
	share_holdings: Vec<(usize, banking_domain::window_export::ShareHolding)>,
	supplies: Vec<(usize, banking_domain::window_export::Supply)>,
	ownerships: Vec<(usize, banking_domain::window_export::Ownership)>,
}

impl Builder {
	fn fail(&mut self, message: impl Into<String>) {
		self.errors.push(message.into());
	}

	fn read(&mut self, records: Vec<(usize, Record)>) {
		for (n, record) in records {
			*self.counts.entry(record.kind()).or_default() += 1;
			self.last_line = n;
			match record {
				Record::Header(h) =>
					if self.header.replace((n, h)).is_some() || n != 1 {
						self.fail(format!("line {n}: a second header (or one that is not first)"));
					},
				Record::Trailer(t) =>
					if self.trailer.replace((n, t)).is_some() {
						self.fail(format!("line {n}: a second trailer"));
					},
				Record::Allocation(r) => self.allocations.push((n, r)),
				Record::FeePolicy(r) => self.fee_policies.push((n, r)),
				Record::FeePolicyChange(r) => self.fee_policy_changes.push((n, r)),
				Record::BookPolicy(r) => self.book_policies.push((n, r)),
				Record::Valuation(r) => self.valuations.push((n, r)),
				Record::Position(r) => self.positions.push((n, r)),
				Record::FeeAssessment(r) => self.fee_assessments.push((n, r)),
				Record::FeeSettlement(r) => self.fee_settlements.push((n, r)),
				Record::User(r) => self.user_refs.push((n, r)),
				Record::ShareHolding(r) => self.share_holdings.push((n, r)),
				Record::Supply(r) => self.supplies.push((n, r)),
				Record::Ownership(r) => self.ownerships.push((n, r)),
			}
		}
	}

	fn finish(mut self, digest: String) -> Result<Plan, Refusals> {
		let header = self.check_envelope();
		let services = self.check_allocations();
		let users = self.check_users();
		let (holdings, mut totals) = self.check_units(&services, &users);
		self.check_trailer(&totals);
		self.check_ownership(&totals, &users);

		let mut notes = Vec::new();
		self.check_rows(&services, &users, &totals, &mut notes);
		for t in &mut totals {
			t.scope = scope_of(&t.service);
		}

		let Some(header) = header else {
			return Err(Refusals(self.errors));
		};
		if !self.errors.is_empty() {
			return Err(Refusals(self.errors));
		}
		let invest_only = |service: &str| scope_of(service) == "invest";
		Ok(Plan {
			digest,
			header,
			allocations: self.allocations.into_iter().map(|(_, r)| r).collect(),
			fee_policies: self.fee_policies.into_iter().map(|(_, r)| r).filter(|r| invest_only(&r.service)).collect(),
			fee_policy_changes: self.fee_policy_changes.into_iter().map(|(_, r)| r).filter(|r| invest_only(&r.service)).collect(),
			book_policies: self.book_policies.into_iter().map(|(_, r)| r).collect(),
			valuations: self.valuations.into_iter().map(|(_, r)| r).collect(),
			positions: self.positions.into_iter().map(|(_, r)| r).collect(),
			fee_assessments: self.fee_assessments.into_iter().map(|(_, r)| r).collect(),
			fee_settlements: self.fee_settlements.into_iter().map(|(_, r)| r).collect(),
			users,
			holdings,
			totals,
			notes,
			warnings: Vec::new(),
		})
	}

	/// Header first, trailer last, and what the header and trailer say about the file.
	fn check_envelope(&mut self) -> Option<banking_domain::window_export::Header> {
		let header = self.header.take().map(|(_, h)| h);
		let Some(h) = &header else {
			self.fail("file: no header record");
			return None;
		};
		if h.schema_version != SCHEMA_VERSION {
			self.fail(format!("header: schema_version {}, this build reads only {SCHEMA_VERSION}", h.schema_version));
		}
		if h.source != "banking" {
			self.fail(format!("header: source is \"{}\", only \"banking\" is accepted", h.source));
		}
		if h.live {
			self.fail("header: live = true — the export was taken while banking still took writes (--allow-live); it is a rehearsal, not a consistent cut");
		}
		if !h.operations_read_only {
			self.fail("header: operations_read_only = false — banking was not in maintenance when the file was read");
		}
		let Some((tn, t)) = self.trailer.take() else {
			self.fail("file: no trailer — the export was cut short");
			return header;
		};
		if tn != self.last_line {
			self.fail(format!("line {tn}: the trailer is not the last line"));
		}
		if t.schema_version != SCHEMA_VERSION {
			self.fail(format!("trailer: schema_version {}, this build reads only {SCHEMA_VERSION}", t.schema_version));
		}
		if !t.clean {
			self.fail("trailer: clean = false — an allocation did not balance or the outbox was not drained");
		}
		if t.outbox_undispatched != 0 {
			self.fail(format!("trailer: {} outbox events were undispatched at the snapshot; the window needs 0", t.outbox_undispatched));
		}
		match digits("trailer: pending_or_locked", &t.pending_or_locked) {
			Ok(0) => {}
			Ok(n) => self.fail(format!("trailer: {} units were pending or locked at the snapshot; their posted balances are not final", human(n))),
			Err(e) => self.fail(e),
		}
		let actual: BTreeMap<String, u64> = self.counts.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect();
		let claimed: BTreeMap<String, u64> = t.counts.iter().filter(|(_, v)| **v != 0).map(|(k, v)| (k.clone(), *v)).collect();
		if actual != claimed {
			let kinds: BTreeSet<_> = actual.keys().chain(claimed.keys()).collect();
			for kind in kinds {
				let (a, c) = (actual.get(kind).copied().unwrap_or(0), claimed.get(kind).copied().unwrap_or(0));
				if a != c {
					self.fail(format!("trailer: counts[{kind}] = {c}, the file has {a} lines of it"));
				}
			}
		}
		self.trailer = Some((tn, t));
		header
	}

	/// The allocation registry: unique valid slugs, the reserved rows under their fixed ids.
	fn check_allocations(&mut self) -> BTreeSet<String> {
		let mut seen = BTreeSet::new();
		let mut problems = Vec::new();
		for (n, a) in &self.allocations {
			let Ok(service) = ServiceId::parse(&a.service) else {
				problems.push(format!("line {n}: allocation service \"{}\" is not a valid slug", a.service));
				continue;
			};
			if !seen.insert(a.service.clone()) {
				problems.push(format!("line {n}: allocation \"{}\" appears twice", a.service));
			}
			if Uuid::parse_str(&a.id).is_err() {
				problems.push(format!("line {n}: allocation \"{}\" has id \"{}\", not a UUID", a.service, a.id));
			}
			if let Some(fixed) = reserved_allocation_id(&service)
				&& Uuid::parse_str(&a.id).ok() != Some(fixed.raw())
			{
				problems.push(format!("line {n}: reserved allocation \"{}\" must carry id {}, the file says {}", a.service, fixed.raw(), a.id));
			}
			if let Err(e) = digits(&format!("line {n}: allocation \"{}\" unit_cap", a.service), &a.unit_cap) {
				problems.push(e);
			}
		}
		self.errors.extend(problems);
		seen
	}

	/// banking user id → concierge id, for every `user` record that names one.
	fn check_users(&mut self) -> BTreeMap<String, Uuid> {
		let mut users = BTreeMap::new();
		let mut by_concierge: HashMap<Uuid, String> = HashMap::new();
		let mut problems = Vec::new();
		for (n, u) in &self.user_refs {
			let Some(raw) = &u.concierge_user_id else {
				continue;
			};
			let Ok(concierge) = Uuid::parse_str(raw) else {
				problems.push(format!("line {n}: user {} has concierge_user_id \"{raw}\", not a UUID", u.user_id));
				continue;
			};
			if users.insert(u.user_id.clone(), concierge).is_some() {
				problems.push(format!("line {n}: user {} appears twice", u.user_id));
			}
			if let Some(other) = by_concierge.insert(concierge, u.user_id.clone())
				&& other != u.user_id
			{
				problems.push(format!(
					"line {n}: banking users {other} and {} are the same concierge user {concierge}; holdings would merge",
					u.user_id
				));
			}
		}
		self.errors.extend(problems);
		users
	}

	/// The holdings and supplies: parse, map the holders, recompute Σ per allocation.
	fn check_units(&mut self, services: &BTreeSet<String>, users: &BTreeMap<String, Uuid>) -> (Vec<Holding>, Vec<Totals>) {
		let mut problems = Vec::new();
		let mut totals: BTreeMap<String, Totals> = services
			.iter()
			.map(|s| {
				(
					s.clone(),
					Totals {
						service: s.clone(),
						..Totals::default()
					},
				)
			})
			.collect();
		let mut holdings = Vec::new();
		let mut keys = BTreeSet::new();

		for (n, h) in &self.share_holdings {
			let Some(t) = totals.get_mut(&h.service) else {
				problems.push(format!("line {n}: share_holding of \"{}\", which has no allocation record", h.service));
				continue;
			};
			let units = match digits(&format!("line {n}: share_holding units"), &h.units) {
				Ok(u) => u,
				Err(e) => {
					problems.push(e);
					continue;
				}
			};
			let Ok(service) = ServiceId::parse(&h.service) else { continue };
			let key = match (h.kind, &h.user_id) {
				(HoldingKind::FeeShares, None) => {
					if service.is_reserved() || scope_of(&h.service) != "invest" {
						problems.push(format!(
							"line {n}: fee-class units of \"{}\": only products of scope invest have a fee class (D24(3)); burn them in banking first",
							h.service
						));
						continue;
					}
					add(&mut problems, || format!("line {n}: fee_shares of \"{}\"", h.service), &mut t.fee_shares, units);
					LedgerAccountKey::FeeShares(service.clone())
				}
				(HoldingKind::FeeShares, Some(_)) => {
					problems.push(format!("line {n}: fee_shares of \"{}\" must not name a user", h.service));
					continue;
				}
				(kind, Some(banking)) => {
					let Some(concierge) = users.get(banking) else {
						problems.push(format!("line {n}: holder {banking} of \"{}\" has no concierge_user_id in the file's user records", h.service));
						continue;
					};
					let user = UserId::from_raw(*concierge);
					add(
						&mut problems,
						|| format!("line {n}: holder {concierge} of \"{}\"", h.service),
						t.holders.entry(*concierge).or_default(),
						units,
					);
					if kind == HoldingKind::UserShares {
						add(&mut problems, || format!("line {n}: user_shares of \"{}\"", h.service), &mut t.user_shares, units);
						LedgerAccountKey::UserShares(service.clone(), user)
					} else {
						if units != 0 {
							problems.push(format!(
								"line {n}: {} units of {banking} sit in resting sell orders of \"{}\"; cancel the open orders in banking first (orders are not exported)",
								human(units),
								h.service
							));
							continue;
						}
						add(&mut problems, || format!("line {n}: book_shares of \"{}\"", h.service), &mut t.book_shares, units);
						LedgerAccountKey::BookShares(service.clone(), user)
					}
				}
				(_, None) => {
					problems.push(format!("line {n}: a user holding of \"{}\" names no user", h.service));
					continue;
				}
			};
			if !keys.insert(key.logical_key()) {
				problems.push(format!("line {n}: {} appears twice", key.logical_key()));
			}
			holdings.push(Holding { service, key, units });
		}

		let mut supplied = BTreeSet::new();
		for (n, s) in &self.supplies {
			let Some(t) = totals.get_mut(&s.service) else {
				problems.push(format!("line {n}: supply of \"{}\", which has no allocation record", s.service));
				continue;
			};
			if !supplied.insert(s.service.clone()) {
				problems.push(format!("line {n}: supply of \"{}\" appears twice", s.service));
			}
			match digits(&format!("line {n}: supply of \"{}\"", s.service), &s.shares_outstanding) {
				Ok(u) => t.outstanding = u,
				Err(e) => problems.push(e),
			}
		}
		for service in services {
			if !supplied.contains(service) {
				problems.push(format!("allocation \"{service}\" has no supply record"));
			}
		}
		for t in totals.values() {
			if t.held() != Some(t.outstanding) {
				problems.push(format!(
					"allocation \"{}\": user {} + book {} + fee {} = {}, shares_outstanding is {}",
					t.service,
					human(t.user_shares),
					human(t.book_shares),
					human(t.fee_shares),
					t.held().map_or("overflow".into(), human),
					human(t.outstanding)
				));
			}
		}
		self.errors.extend(problems);
		(holdings, totals.into_values().collect())
	}

	/// The trailer's per-allocation Σ must be what the holdings add up to.
	fn check_trailer(&mut self, totals: &[Totals]) {
		let Some((_, t)) = &self.trailer else { return };
		let mut problems = Vec::new();
		let mine: HashMap<&str, &Totals> = totals.iter().map(|t| (t.service.as_str(), t)).collect();
		let theirs: BTreeSet<&str> = t.allocations.iter().map(|a| a.service.as_str()).collect();
		for service in mine.keys().filter(|s| !theirs.contains(*s)) {
			problems.push(format!("trailer: no total for allocation \"{service}\""));
		}
		for a in &t.allocations {
			let Some(m) = mine.get(a.service.as_str()) else {
				problems.push(format!("trailer: total for \"{}\", which has no allocation record", a.service));
				continue;
			};
			let claimed = [
				("user_shares", &a.user_shares, m.user_shares),
				("book_shares", &a.book_shares, m.book_shares),
				("fee_shares", &a.fee_shares, m.fee_shares),
				("held", &a.held, m.held().unwrap_or(u128::MAX)),
				("shares_outstanding", &a.shares_outstanding, m.outstanding),
			];
			for (name, raw, recomputed) in claimed {
				match digits(&format!("trailer: {} {name}", a.service), raw) {
					Ok(v) if v == recomputed => {}
					Ok(v) => problems.push(format!("trailer: {} {name} is {}, the holdings add up to {}", a.service, human(v), human(recomputed))),
					Err(e) => problems.push(e),
				}
			}
			if !a.balanced {
				problems.push(format!("trailer: allocation \"{}\" is not balanced", a.service));
			}
		}
		let held = totals.iter().try_fold(0u128, |a, t| a.checked_add(t.held()?));
		let outstanding = totals.iter().try_fold(0u128, |a, t| a.checked_add(t.outstanding));
		let (Some(held), Some(outstanding)) = (held, outstanding) else {
			problems.push("trailer: the totals overflow 128 bits".into());
			self.errors.extend(problems);
			return;
		};
		for (name, raw, recomputed) in [("total_held", &t.total_held, held), ("total_outstanding", &t.total_outstanding, outstanding)] {
			match digits(&format!("trailer: {name}"), raw) {
				Ok(v) if v == recomputed => {}
				Ok(v) => problems.push(format!("trailer: {name} is {}, the holdings add up to {}", human(v), human(recomputed))),
				Err(e) => problems.push(e),
			}
		}
		self.errors.extend(problems);
	}

	/// The ownership view of `fee` / `fund` is derived from the holdings; a disagreement
	/// means one of the two was damaged on the way out.
	fn check_ownership(&mut self, totals: &[Totals], users: &BTreeMap<String, Uuid>) {
		let mut problems = Vec::new();
		let mine: HashMap<&str, &Totals> = totals.iter().map(|t| (t.service.as_str(), t)).collect();
		for (n, o) in &self.ownerships {
			let Some(t) = mine.get(o.service.as_str()) else {
				problems.push(format!("line {n}: ownership of \"{}\", which has no allocation record", o.service));
				continue;
			};
			if digits("ownership units_outstanding", &o.units_outstanding).ok() != Some(t.outstanding) {
				problems.push(format!(
					"line {n}: ownership of \"{}\": units_outstanding {} is not the supply {}",
					o.service, o.units_outstanding, t.outstanding
				));
			}
			let mut listed: BTreeMap<Uuid, u128> = BTreeMap::new();
			for holder in &o.holders {
				match (users.get(&holder.user_id), digits("ownership holder units", &holder.units)) {
					(Some(c), Ok(u)) => {
						add(&mut problems, || format!("line {n}: ownership holder {}", holder.user_id), listed.entry(*c).or_default(), u);
					}
					(None, _) => problems.push(format!("line {n}: ownership holder {} has no concierge_user_id in the file's user records", holder.user_id)),
					(_, Err(e)) => problems.push(e),
				}
			}
			let held: BTreeMap<Uuid, u128> = t.holders.iter().filter(|(_, u)| **u != 0).map(|(c, u)| (*c, *u)).collect();
			if listed != held && problems.iter().all(|p| !p.contains("has no concierge_user_id")) {
				problems.push(format!(
					"line {n}: ownership of \"{}\" lists holders that are not the holdings' (people, free + order-locked summed)",
					o.service
				));
			}
		}
		self.errors.extend(problems);
	}

	/// Records that hang off an allocation or a person; what is left out on purpose goes to
	/// `notes`, what merely looks off is added to the report's warnings by the apply.
	fn check_rows(&mut self, services: &BTreeSet<String>, users: &BTreeMap<String, Uuid>, totals: &[Totals], notes: &mut Vec<String>) {
		let mut problems = Vec::new();
		let known = |what: &str, n: usize, service: &str, problems: &mut Vec<String>| {
			if !services.contains(service) {
				problems.push(format!("line {n}: {what} of \"{service}\", which has no allocation record"));
			}
		};
		let person = |what: &str, n: usize, id: &str, problems: &mut Vec<String>| {
			if !users.contains_key(id) {
				problems.push(format!("line {n}: {what}: user {id} has no concierge_user_id in the file's user records"));
			}
		};
		let amounts = |n: usize, what: &str, values: &[&str], problems: &mut Vec<String>| {
			for v in values {
				if let Err(e) = digits(&format!("line {n}: {what}"), v) {
					problems.push(e);
				}
			}
		};

		let mut skipped_policies: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
		for (n, p) in &self.fee_policies {
			known("fee_policy", *n, &p.service, &mut problems);
			if scope_of(&p.service) != "invest" {
				skipped_policies.entry(p.service.as_str()).or_default().0 += 1;
			} else if p.management_bps > 500 || p.performance_bps > 5000 {
				problems.push(format!(
					"line {n}: fee_policy of \"{}\" is above the ceilings (management 500 bps, performance 5000 bps)",
					p.service
				));
			}
		}
		for (n, c) in &self.fee_policy_changes {
			known("fee_policy_change", *n, &c.service, &mut problems);
			if scope_of(&c.service) != "invest" {
				skipped_policies.entry(c.service.as_str()).or_default().1 += 1;
			}
			for user in c.notices_waived_users.iter().flatten() {
				person("fee_policy_change notices_waived_users", *n, user, &mut problems);
			}
			if Uuid::parse_str(&c.id).is_err() {
				problems.push(format!("line {n}: fee_policy_change id \"{}\" is not a UUID", c.id));
			}
		}
		for (service, (policies, changes)) in skipped_policies {
			notes.push(format!(
				"\"{service}\" (scope {}) takes no fee terms: {policies} fee_policy and {changes} fee_policy_change records are not imported",
				scope_of(service)
			));
		}
		for (n, p) in &self.book_policies {
			known("book_policy", *n, &p.service, &mut problems);
		}
		if !self.book_policies.is_empty() {
			notes.push("book_policy has no allow_unbacked_trading in schema 1: every imported book policy starts with it false".into());
		}
		for (n, v) in &self.valuations {
			known("valuation", *n, &v.service, &mut problems);
			amounts(*n, "valuation", &[&v.aum, &v.units_outstanding, &v.nav], &mut problems);
		}
		for (n, f) in &self.fee_assessments {
			known("fee_assessment", *n, &f.service, &mut problems);
			person("fee_assessment", *n, &f.user_id, &mut problems);
			amounts(
				*n,
				"fee_assessment",
				&[
					&f.nav, &f.management, &f.performance, &f.debt_opening, &f.charged_units, &f.charged_cash, &f.debt_carried, &f.high_water_mark,
				],
				&mut problems,
			);
		}
		for (n, f) in &self.fee_settlements {
			known("fee_settlement", *n, &f.service, &mut problems);
			amounts(*n, "fee_settlement", &[&f.units, &f.nav, &f.cash], &mut problems);
		}

		let held: HashMap<(&str, Uuid), u128> = totals.iter().flat_map(|t| t.holders.iter().map(move |(u, v)| ((t.service.as_str(), *u), *v))).collect();
		for (n, p) in &self.positions {
			known("position", *n, &p.service, &mut problems);
			person("position", *n, &p.user_id, &mut problems);
			amounts(*n, "position", &[&p.cost_basis, &p.high_water_mark, &p.units, &p.fee_debt], &mut problems);
			if let (Some(concierge), Ok(projected)) = (users.get(&p.user_id), digits("position units", &p.units)) {
				let ledger = held.get(&(p.service.as_str(), *concierge)).copied().unwrap_or(0);
				if projected != ledger {
					problems.push(format!(
						"line {n}: position of {} in \"{}\" says {} units, the ledger holds {}; the projection and the ledger disagree, fix banking first",
						p.user_id,
						p.service,
						human(projected),
						human(ledger)
					));
				}
			}
		}
		self.errors.extend(problems);
	}
}
