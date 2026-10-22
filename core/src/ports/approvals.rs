//! concierge's approvals, as invest proposes and executes them — the port.
//!
//! invest is the only proposer and executor of its `invest:*` kinds (concierge
//! `docs/SCOPES-AND-APPROVALS.md` §5): it opens a proposal with `Propose`, concierge's
//! roster decides, and invest pulls what passed, carries it out, and reports the outcome.
//! Every call is idempotent on concierge's side (`dedupe_key` on a proposal, the same
//! outcome twice is a no-op), so a retried round costs nothing.

use async_trait::async_trait;
use sha2::{Digest, Sha256};

/// One typed value of a proposal, as concierge renders it to the voters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalValue {
	Text(String),
	Bps(i64),
	Flag(bool),
	/// Unix seconds.
	Timestamp(i64),
	Count(u64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalField {
	pub key: &'static str,
	pub value: ApprovalValue,
}

/// A proposal as invest opens it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Proposal {
	pub kind: &'static str,
	pub scope: &'static str,
	/// `<type>:<uuid>`: what invest acts on when the proposal passes.
	pub subject_ref: String,
	/// The asker's `auth_subject`, or empty. Information only: without the person's own
	/// token concierge cannot prove who asked, so it keeps nobody off the roster.
	pub requested_by: String,
	pub reason: String,
	pub fields: Vec<ApprovalField>,
	/// Makes a retried `Propose` return the proposal the first one opened.
	pub dedupe_key: String,
}

/// Where a proposal stands in concierge (§5.4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalState {
	Open,
	/// The threshold was met. Vetoable until `executable_at`; then invest pulls it.
	Passed,
	Executed,
	ExecutionFailed,
	Rejected,
	Vetoed,
	Expired,
	/// Withdrawn by the proposer.
	Cancelled,
	Void,
}

impl ApprovalState {
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Open => "open",
			Self::Passed => "passed",
			Self::Executed => "executed",
			Self::ExecutionFailed => "execution_failed",
			Self::Rejected => "rejected",
			Self::Vetoed => "vetoed",
			Self::Expired => "expired",
			Self::Cancelled => "cancelled",
			Self::Void => "void",
		}
	}

	pub fn parse(raw: &str) -> Option<Self> {
		Some(match raw {
			"open" => Self::Open,
			"passed" => Self::Passed,
			"executed" => Self::Executed,
			"execution_failed" => Self::ExecutionFailed,
			"rejected" => Self::Rejected,
			"vetoed" => Self::Vetoed,
			"expired" => Self::Expired,
			"cancelled" => Self::Cancelled,
			"void" => Self::Void,
			_ => return None,
		})
	}

	/// Whether the owners may still carry it, or have and invest has not acted yet.
	pub const fn is_live(self) -> bool {
		matches!(self, Self::Open | Self::Passed)
	}
}

/// A proposal as concierge answers it to its proposer: no voter is named.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalView {
	pub id: String,
	pub kind: String,
	pub scope: String,
	pub subject_ref: String,
	pub state: ApprovalState,
	pub payload_hash: String,
	/// The veto reason, the void reason or the reported outcome detail, whichever the state
	/// has; empty otherwise.
	pub note: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalOutcome {
	Executed,
	/// Tried and failed. Terminal in concierge: a retry is a new proposal.
	Failed,
	/// No longer applies; nothing was attempted.
	Void,
}

#[derive(Debug, thiserror::Error)]
pub enum ApprovalsError {
	/// concierge answered and will answer the same to the same request.
	#[error("concierge refused: {0}")]
	Refused(String),
	/// The namespace's cap on live proposals in one scope.
	#[error("concierge is at its limit: {0}")]
	Throttled(String),
	/// Not reachable, too slow, or configured wrong: the same call later may succeed.
	#[error("concierge is unavailable: {0}")]
	Unavailable(String),
}

#[async_trait]
pub trait Approvals: Send + Sync {
	async fn propose(&self, proposal: &Proposal) -> Result<ApprovalView, ApprovalsError>;
	async fn get(&self, approval_id: &str) -> Result<ApprovalView, ApprovalsError>;
	/// The passed proposals of invest's namespace that are due, oldest first. A pull claims
	/// nothing: a proposal is served again until its outcome is reported.
	async fn pull_approved(&self, limit: u32) -> Result<Vec<ApprovalView>, ApprovalsError>;
	async fn report(&self, approval_id: &str, outcome: ApprovalOutcome, detail: &str) -> Result<(), ApprovalsError>;
	async fn withdraw(&self, approval_id: &str, reason: &str) -> Result<ApprovalView, ApprovalsError>;
}

/// concierge's `payload_hash` (§5.4): SHA-256, lowercase hex, of the kind, the scope, the
/// subject and one line per field sorted by key. invest computes it over the change it
/// STORED and refuses to act on a passed proposal whose hash differs.
pub fn payload_hash(kind: &str, scope: &str, subject_ref: &str, fields: &[ApprovalField]) -> String {
	let mut lines: Vec<String> = fields
		.iter()
		.map(|field| {
			let (tag, value) = match &field.value {
				ApprovalValue::Text(text) => ("TEXT", text.clone()),
				ApprovalValue::Bps(bps) => ("BPS", bps.to_string()),
				ApprovalValue::Flag(flag) => ("FLAG", flag.to_string()),
				ApprovalValue::Timestamp(at) => ("TIMESTAMP", at.to_string()),
				ApprovalValue::Count(count) => ("COUNT", count.to_string()),
			};
			format!("{}\t{tag}\t{value}\n", field.key)
		})
		.collect();
	lines.sort();
	let preimage = format!("ev-approval-v1\n{kind}\n{scope}\n{subject_ref}\n{}", lines.concat());
	Sha256::digest(preimage.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_payload_hash_is_concierges_preimage_with_the_lines_sorted() {
		// Pinned against `shasum -a 256` over the preimage concierge's
		// `domain::approvals::payload_preimage` builds for these fields; the fields are given
		// out of order on purpose.
		let fields = [
			ApprovalField {
				key: "raises_ceiling",
				value: ApprovalValue::Flag(true),
			},
			ApprovalField {
				key: "proposed_management",
				value: ApprovalValue::Bps(300),
			},
			ApprovalField {
				key: "allocation",
				value: ApprovalValue::Text("trading".into()),
			},
			ApprovalField {
				key: "holders_notified",
				value: ApprovalValue::Count(3),
			},
		];
		assert_eq!(
			payload_hash("invest:fee_policy", "invest", "fee_policy_change:00000000-0000-0000-0000-000000000233", &fields),
			"5d48acc57b66ef2484a1a761f25cd106d0287c7fddbf376cc52b23877a5c184d"
		);
	}

	#[test]
	fn approval_states_round_trip_through_their_stored_strings() {
		for state in [
			ApprovalState::Open,
			ApprovalState::Passed,
			ApprovalState::Executed,
			ApprovalState::ExecutionFailed,
			ApprovalState::Rejected,
			ApprovalState::Vetoed,
			ApprovalState::Expired,
			ApprovalState::Cancelled,
			ApprovalState::Void,
		] {
			assert_eq!(ApprovalState::parse(state.as_str()), Some(state));
			assert_eq!(state.is_live(), matches!(state, ApprovalState::Open | ApprovalState::Passed));
		}
		assert_eq!(ApprovalState::parse("approved"), None);
	}
}
