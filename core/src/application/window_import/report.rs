//! What a `window-import` run says about itself: the Σ of units per allocation, who holds
//! them, and — after an apply — what the ledger said back.

use std::fmt::Write as _;

use serde::Serialize;

use super::plan::{Anchor, AnchorHolder, Plan, Totals};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
	DryRun,
	Applied,
	/// A re-run of an apply that had already completed: nothing written, the ledger re-read.
	AlreadyApplied,
}

/// One allocation's line. Units are 18-dp base-unit digit strings (`human` is for eyes).
#[derive(Clone, Debug, Serialize)]
pub struct AllocationLine {
	pub service: String,
	pub scope: String,
	pub user_shares: String,
	pub book_shares: String,
	pub fee_shares: String,
	pub held: String,
	pub shares_outstanding: String,
	pub balanced: bool,
	pub human_user_shares: String,
	pub human_book_shares: String,
	pub human_fee_shares: String,
	pub human_held: String,
	pub human_outstanding: String,
	/// Holder (concierge user id) → units in whole-unit notation, largest first.
	pub holders: Vec<HolderLine>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HolderLine {
	pub user: String,
	pub units: String,
	pub human_units: String,
}

/// One comparison of the file against the ledger after the write.
#[derive(Clone, Debug, Serialize)]
pub struct Check {
	pub check: &'static str,
	pub subject: String,
	pub expected: String,
	pub actual: String,
	pub ok: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
	pub mode: Mode,
	pub ok: bool,
	/// SHA-256 of the file's bytes (lower-case hex): what `--apply --expect-digest` and
	/// banking's `close-share-accounts` check.
	pub file_digest: String,
	/// `OK`, `BROKEN` or `n/a` — the `service_arb` test anchor.
	pub anchor: String,
	pub anchor_detail: Option<String>,
	pub source: String,
	pub exported_at: String,
	pub tigerbeetle_cluster_id: String,
	pub allocations: Vec<AllocationLine>,
	/// Things the importer did not carry over, on purpose.
	pub notes: Vec<String>,
	/// Things that look wrong but do not stop the import.
	pub warnings: Vec<String>,
	pub checks: Vec<Check>,
}

/// Base units as whole-unit notation: `13000000000000000000000` → `13000`.
pub fn human(units: u128) -> String {
	const ONE: u128 = 1_000_000_000_000_000_000;
	let (whole, frac) = (units / ONE, units % ONE);
	if frac == 0 {
		return whole.to_string();
	}
	let frac = format!("{frac:018}");
	format!("{whole}.{}", frac.trim_end_matches('0'))
}

fn line(totals: &Totals) -> AllocationLine {
	let mut holders: Vec<_> = totals.holders.iter().collect();
	holders.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
	AllocationLine {
		service: totals.service.clone(),
		scope: totals.scope.to_owned(),
		user_shares: totals.user_shares.to_string(),
		book_shares: totals.book_shares.to_string(),
		fee_shares: totals.fee_shares.to_string(),
		held: totals.held().map_or_else(|| "overflow".into(), |h| h.to_string()),
		shares_outstanding: totals.outstanding.to_string(),
		balanced: totals.held() == Some(totals.outstanding),
		human_user_shares: human(totals.user_shares),
		human_book_shares: human(totals.book_shares),
		human_fee_shares: human(totals.fee_shares),
		human_held: totals.held().map_or_else(|| "overflow".into(), human),
		human_outstanding: human(totals.outstanding),
		holders: holders
			.into_iter()
			.map(|(user, units)| HolderLine {
				user: user.to_string(),
				units: units.to_string(),
				human_units: human(*units),
			})
			.collect(),
	}
}

impl Report {
	/// Everything that can be known from the file alone.
	pub fn of_plan(plan: &Plan, mode: Mode, anchors: &[AnchorHolder]) -> Self {
		let anchor = plan.anchor(anchors);
		Self {
			mode,
			ok: !anchor.is_broken(),
			file_digest: plan.digest.clone(),
			anchor: match &anchor {
				Anchor::NotApplicable => "n/a",
				Anchor::Ok => "OK",
				Anchor::Broken(_) => "BROKEN",
			}
			.into(),
			anchor_detail: match anchor {
				Anchor::Broken(why) => Some(why),
				_ => None,
			},
			source: plan.header.source.clone(),
			exported_at: plan.header.exported_at.clone(),
			tigerbeetle_cluster_id: plan.header.tigerbeetle_cluster_id.clone(),
			allocations: plan.totals.iter().map(line).collect(),
			notes: plan.notes.clone(),
			warnings: plan.warnings.clone(),
			checks: Vec::new(),
		}
	}

	/// Add the after-write checks; the report is `ok` only if every one is.
	pub fn with_checks(mut self, checks: Vec<Check>) -> Self {
		self.ok = self.ok && checks.iter().all(|c| c.ok);
		self.checks = checks;
		self
	}

	pub fn render(&self) -> String {
		let mut out = String::new();
		// Writing to a String cannot fail.
		let _ = writeln!(
			out,
			"window-import {:?}: source {} exported {} (TigerBeetle cluster {})",
			self.mode, self.source, self.exported_at, self.tigerbeetle_cluster_id
		);
		for a in &self.allocations {
			let _ = writeln!(
				out,
				"{:<24} scope {:<7} user {} + book {} + fee {} = {} (outstanding {}) {}",
				a.service,
				a.scope,
				a.human_user_shares,
				a.human_book_shares,
				a.human_fee_shares,
				a.human_held,
				a.human_outstanding,
				if a.balanced { "balanced" } else { "NOT BALANCED" },
			);
			for h in &a.holders {
				let _ = writeln!(out, "    {}  {}", h.user, h.human_units);
			}
		}
		let _ = writeln!(out, "file sha256: {}", self.file_digest);
		let _ = writeln!(out, "anchor {}{}", self.anchor, self.anchor_detail.as_ref().map_or_else(String::new, |d| format!(": {d}")));
		for note in &self.notes {
			let _ = writeln!(out, "note: {note}");
		}
		for warning in &self.warnings {
			let _ = writeln!(out, "warning: {warning}");
		}
		for c in &self.checks {
			let _ = writeln!(
				out,
				"{} {} {}: expected {}, ledger says {}",
				if c.ok { "ok      " } else { "MISMATCH" },
				c.check,
				c.subject,
				c.expected,
				c.actual
			);
		}
		out
	}
}

#[cfg(test)]
mod tests {
	use super::human;

	#[test]
	fn units_read_in_whole_notation() {
		assert_eq!(human(13_000_000_000_000_000_000_000), "13000");
		assert_eq!(human(3_250_000_000_000_000_000_000), "3250");
		assert_eq!(human(1_500_000_000_000_000_000), "1.5");
		assert_eq!(human(1), "0.000000000000000001");
		assert_eq!(human(0), "0");
	}
}
