//! The owners' approval of a fee-policy change, through concierge (`invest:fee_policy`).
//!
//! A change that leaves the house envelope ([`invest_domain::fees::requirement_for`]) is the owners' to
//! carry. It is recorded `awaiting_consilium` first and proposed right after ([`propose`]);
//! the request that wrote it proposes it in the same breath, and [`round`] proposes again
//! whatever a lost request left behind. The proposal is idempotent by its
//! `fee_policy_change:<id>`, so a second attempt returns the first one's proposal.
//!
//! [`round`] runs every [`ROUND_INTERVAL`] (`infrastructure::fee_approvals`). It carries out
//! the proposals that passed: re-reads each by its id, re-computes the payload hash from the
//! STORED change and refuses on a mismatch, schedules the change (the notice period starts
//! now, as it did under banking's consilium) and reports the outcome. It closes a change
//! whose proposal ended any other way as `rejected`, and withdraws the proposal of a change
//! the operator cancelled when the cancel could not reach concierge.
//!
//! Every step is idempotent, so two replicas, a crash between scheduling and reporting, or
//! a proposal served twice cost nothing: a change already scheduled by its approval is
//! reported executed again, never scheduled again. A step concierge does not answer is left
//! for the next round.

use std::time::Duration;

use invest_domain::{
	error::DomainError,
	fees::{FeePolicy, FeePolicyChangeId, FeePolicyChangeState},
	users::UserId,
};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::ports::{
	approvals::{ApprovalField, ApprovalOutcome, ApprovalState, ApprovalValue, ApprovalView, Approvals, ApprovalsError, Proposal, payload_hash},
	catalog::fee_policy_fields as field,
	fees::{ApprovedTerms, FeePolicies, FeePolicyChange, FeePolicyChanges},
	users::UserDirectory,
};

pub const FEE_POLICY_KIND: &str = "invest:fee_policy";
/// invest proposes in its own scope only: concierge refuses a relying party anything else.
pub const APPROVAL_SCOPE: &str = "invest";
const SUBJECT_TYPE: &str = "fee_policy_change";

/// How often [`round`] runs. concierge's own executor runs every 30 s; a passed change is
/// scheduled within one more round.
pub const ROUND_INTERVAL: Duration = Duration::from_secs(30);

/// Proposals pulled, and changes watched, per round.
const BATCH: u32 = 50;

/// What the change asks the owners for, as concierge's `subject_ref` and `dedupe_key`.
pub fn subject_ref(id: FeePolicyChangeId) -> String {
	format!("{SUBJECT_TYPE}:{id}")
}

fn parse_subject_ref(raw: &str) -> Option<FeePolicyChangeId> {
	let id = raw.strip_prefix(SUBJECT_TYPE)?.strip_prefix(':')?;
	Uuid::parse_str(id).ok().map(FeePolicyChangeId::from_raw)
}

/// The fields of the proposal, from the STORED change and the terms in force. Only an owner
/// change carries `approval_holders`; anything else has nothing to propose.
pub fn proposal_fields(change: &FeePolicyChange, current: Option<&FeePolicy>) -> Result<Vec<ApprovalField>, DomainError> {
	let holders = change
		.approval_holders
		.ok_or_else(|| DomainError::Precondition(format!("fee-policy change {} names no holder count, so it was never proposed by invest", change.id)))?;
	let from = current.copied().unwrap_or(FeePolicy::NONE);
	let to = change.policy;
	let text = |value: &str| ApprovalValue::Text(value.to_owned());
	let bps = |value: u32| ApprovalValue::Bps(i64::from(value));
	Ok(vec![
		ApprovalField {
			key: field::ALLOCATION,
			value: text(change.service.as_str()),
		},
		ApprovalField {
			key: field::CURRENT_MANAGEMENT,
			value: bps(from.management_bps()),
		},
		ApprovalField {
			key: field::PROPOSED_MANAGEMENT,
			value: bps(to.management_bps()),
		},
		ApprovalField {
			key: field::CURRENT_PERFORMANCE,
			value: bps(from.performance_bps()),
		},
		ApprovalField {
			key: field::PROPOSED_PERFORMANCE,
			value: bps(to.performance_bps()),
		},
		ApprovalField {
			key: field::CURRENT_HURDLE,
			value: bps(from.hurdle_bps()),
		},
		ApprovalField {
			key: field::PROPOSED_HURDLE,
			value: bps(to.hurdle_bps()),
		},
		ApprovalField {
			key: field::CURRENT_BASIS,
			value: text(from.basis().as_str()),
		},
		ApprovalField {
			key: field::PROPOSED_BASIS,
			value: text(to.basis().as_str()),
		},
		ApprovalField {
			key: field::CURRENT_CRYSTALLIZATION,
			value: text(from.crystallization().as_str()),
		},
		ApprovalField {
			key: field::PROPOSED_CRYSTALLIZATION,
			value: text(to.crystallization().as_str()),
		},
		ApprovalField {
			key: field::RAISES_CEILING,
			value: ApprovalValue::Flag(!to.within_house_envelope()),
		},
		// Provisional: the notice period runs from the moment the owners carry it.
		ApprovalField {
			key: field::EFFECTIVE_FROM,
			value: ApprovalValue::Timestamp(change.effective_from_unix),
		},
		ApprovalField {
			key: field::HOLDERS_NOTIFIED,
			value: ApprovalValue::Count(u64::from(holders)),
		},
	])
}

/// The payload hash concierge must hold for `change`.
pub fn expected_hash(change: &FeePolicyChange, current: Option<&FeePolicy>) -> Result<String, DomainError> {
	let fields = proposal_fields(change, current)?;
	Ok(payload_hash(FEE_POLICY_KIND, APPROVAL_SCOPE, &subject_ref(change.id), &fields))
}

/// The handles proposing and executing borrow.
pub struct FeeApprovalPorts<'a> {
	pub approvals: &'a dyn Approvals,
	pub changes: &'a dyn FeePolicyChanges,
	pub policies: &'a dyn FeePolicies,
	pub users: &'a dyn UserDirectory,
}

/// What came of [`propose`].
pub enum Proposed {
	/// concierge holds the proposal; the change carries its id.
	Opened(FeePolicyChange),
	/// concierge refused it, and will refuse it again: the change is closed `rejected`.
	Refused { change: FeePolicyChange, why: String },
	/// concierge could not be asked now. The change waits unproposed; [`round`] retries.
	Later(FeePolicyChange),
}

/// Propose `change` to the owners. `change` is an owner change still `awaiting_consilium`.
pub async fn propose(ports: &FeeApprovalPorts<'_>, change: FeePolicyChange) -> Result<Proposed, DomainError> {
	let current = ports.policies.find(&change.service).await?;
	let fields = proposal_fields(&change, current.as_ref())?;
	let subject = subject_ref(change.id);
	let ours = payload_hash(FEE_POLICY_KIND, APPROVAL_SCOPE, &subject, &fields);
	let proposal = Proposal {
		kind: FEE_POLICY_KIND,
		scope: APPROVAL_SCOPE,
		dedupe_key: subject.clone(),
		subject_ref: subject,
		requested_by: requested_by(ports.users, &change.requested_by).await,
		reason: change.reason.clone(),
		fields,
	};
	match ports.approvals.propose(&proposal).await {
		Ok(opened) if opened.payload_hash != ours => {
			// The two planes disagree on what is being approved: nothing that passes under
			// this proposal could ever be executed.
			error!(change_id = %change.id, approval_id = %opened.id, ours = %ours, theirs = %opened.payload_hash, "fee approvals: the opened proposal's payload hash is not the change's");
			if let Err(err) = ports.approvals.withdraw(&opened.id, "the proposal does not match the fee-policy change").await {
				warn!(change_id = %change.id, approval_id = %opened.id, "fee approvals: could not withdraw the mismatched proposal: {err}");
			}
			let why = "concierge and invest disagree about what is being approved".to_owned();
			let change = ports.changes.close_unapproved(change.id, None, &why).await?;
			Ok(Proposed::Refused { change, why })
		}
		Ok(opened) => {
			let id = approval_uuid(&opened)?;
			let change = ports.changes.record_proposal(change.id, id, opened.state).await?;
			info!(change_id = %change.id, approval_id = %id, state = opened.state.as_str(), "fee approvals: proposed to the owners");
			Ok(Proposed::Opened(change))
		}
		Err(ApprovalsError::Refused(why)) => {
			error!(change_id = %change.id, "fee approvals: concierge refused the proposal: {why}");
			let why = format!("concierge refused the proposal: {why}");
			let change = ports.changes.close_unapproved(change.id, None, &why).await?;
			Ok(Proposed::Refused { change, why })
		}
		Err(err @ (ApprovalsError::Throttled(_) | ApprovalsError::Unavailable(_))) => {
			warn!(change_id = %change.id, "fee approvals: could not propose now, the next round retries: {err}");
			Ok(Proposed::Later(change))
		}
	}
}

/// The asker's `auth_subject`, so concierge mails them the outcome; empty while the
/// projection has not learned it. Information only (see [`Proposal::requested_by`]).
async fn requested_by(users: &dyn UserDirectory, requested_by: &str) -> String {
	let Ok(user) = Uuid::parse_str(requested_by).map(UserId::from_raw) else {
		return String::new();
	};
	match users.auth_subject(user).await {
		Ok(subject) => subject.unwrap_or_default(),
		Err(err) => {
			warn!(%user, "fee approvals: could not read the requester's auth_subject; proposing without it: {err}");
			String::new()
		}
	}
}

fn approval_uuid(view: &ApprovalView) -> Result<Uuid, DomainError> {
	Uuid::parse_str(&view.id).map_err(|_| DomainError::Repository(format!("concierge named an approval by a non-UUID id {:?}", view.id)))
}

/// Withdraw the proposal of a change the operator has just cancelled. Best effort: a
/// withdrawal concierge does not take now is retried by [`round`].
pub async fn withdraw_cancelled(ports: &FeeApprovalPorts<'_>, change: &FeePolicyChange) -> Result<(), DomainError> {
	let Some(approval) = change.approval_id else { return Ok(()) };
	if change.state != FeePolicyChangeState::Cancelled || change.approval_state.is_some_and(|state| !state.is_live()) {
		return Ok(());
	}
	match ports.approvals.withdraw(&approval.to_string(), "the operator withdrew the fee-policy change").await {
		Ok(view) => ports.changes.note_approval_state(change.id, view.state).await,
		Err(err) => {
			warn!(change_id = %change.id, approval_id = %approval, "fee approvals: could not withdraw the proposal of a cancelled change, the next round retries: {err}");
			Ok(())
		}
	}
}

/// One round's outcome, as counts — for the tests and the log.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ApprovalRound {
	pub proposed: usize,
	pub scheduled: usize,
	/// Changes closed `rejected` because their proposal ended otherwise, or did not match.
	pub rejected: usize,
	/// Outcomes concierge recorded.
	pub reported: usize,
	pub withdrawn: usize,
}

/// One pass. Safe beside another replica's and from a test: every step is idempotent.
pub async fn round(ports: &FeeApprovalPorts<'_>, now: i64) -> Result<ApprovalRound, DomainError> {
	let mut round = ApprovalRound::default();
	execute_approved(ports, &mut round, now).await?;
	watch(ports, &mut round).await?;
	Ok(round)
}

async fn execute_approved(ports: &FeeApprovalPorts<'_>, round: &mut ApprovalRound, now: i64) -> Result<(), DomainError> {
	let approved = match ports.approvals.pull_approved(BATCH).await {
		Ok(approved) => approved,
		Err(err) => {
			warn!("fee approvals: could not pull the passed proposals, will retry: {err}");
			return Ok(());
		}
	};
	for pulled in approved {
		// Another `invest:*` kind is carried out by its own flow.
		if pulled.kind != FEE_POLICY_KIND {
			continue;
		}
		if let Err(err) = carry_out(ports, &pulled, round, now).await {
			error!(approval_id = %pulled.id, subject_ref = %pulled.subject_ref, "fee approvals: carrying out a passed change failed, the next pull retries: {err}");
		}
	}
	Ok(())
}

/// concierge takes an outcome detail as one line of at most this many bytes, and refuses
/// the whole report otherwise.
const DETAIL_BYTES: usize = 200;

fn outcome_detail(detail: &str) -> String {
	let mut line = String::with_capacity(detail.len().min(DETAIL_BYTES));
	for c in detail.chars().map(|c| if c.is_control() { ' ' } else { c }) {
		if line.len() + c.len_utf8() > DETAIL_BYTES {
			break;
		}
		line.push(c);
	}
	line
}

/// Tell concierge how a passed proposal came out. Unreported, it is served again on the
/// next pull and reported then: the change's own state keeps it from being scheduled twice.
async fn report(
	ports: &FeeApprovalPorts<'_>,
	round: &mut ApprovalRound,
	change: Option<FeePolicyChangeId>,
	approval_id: &str,
	outcome: ApprovalOutcome,
	detail: &str,
) -> Result<(), DomainError> {
	match ports.approvals.report(approval_id, outcome, &outcome_detail(detail)).await {
		Ok(()) => {
			round.reported += 1;
			if let Some(change) = change {
				let state = match outcome {
					ApprovalOutcome::Executed => ApprovalState::Executed,
					ApprovalOutcome::Failed => ApprovalState::ExecutionFailed,
					ApprovalOutcome::Void => ApprovalState::Void,
				};
				ports.changes.note_approval_state(change, state).await?;
			}
		}
		Err(ApprovalsError::Refused(why)) => {
			error!(%approval_id, ?outcome, "fee approvals: concierge refused the outcome, so it holds none for this proposal: {why}");
		}
		Err(err @ (ApprovalsError::Unavailable(_) | ApprovalsError::Throttled(_))) => {
			warn!(%approval_id, ?outcome, "fee approvals: could not report the outcome, the next pull retries: {err}");
		}
	}
	Ok(())
}

async fn carry_out(ports: &FeeApprovalPorts<'_>, pulled: &ApprovalView, round: &mut ApprovalRound, now: i64) -> Result<(), DomainError> {
	let found = match parse_subject_ref(&pulled.subject_ref) {
		Some(id) => ports.changes.find(id).await?,
		None => None,
	};
	let Some(change) = found else {
		error!(approval_id = %pulled.id, subject_ref = %pulled.subject_ref, "fee approvals: a passed proposal names no fee-policy change invest has");
		return report(ports, round, None, &pulled.id, ApprovalOutcome::Void, "no such fee-policy change").await;
	};
	// Before anything else, whatever state the change is in: a proposal that is not the one
	// recorded for the change never reports the change's outcome.
	if change.approval_id.map(|id| id.to_string()).as_deref() != Some(pulled.id.as_str()) {
		error!(change_id = %change.id, approval_id = %pulled.id, "fee approvals: a passed proposal is not the one this change waits on");
		return report(ports, round, None, &pulled.id, ApprovalOutcome::Void, "another approval decides this fee-policy change").await;
	}
	match change.state {
		FeePolicyChangeState::AwaitingConsilium => {}
		// Scheduled by this approval before, and not reported: say it again, never act again.
		FeePolicyChangeState::Scheduled | FeePolicyChangeState::Active | FeePolicyChangeState::Superseded => {
			return report(ports, round, Some(change.id), &pulled.id, ApprovalOutcome::Executed, "").await;
		}
		FeePolicyChangeState::Rejected | FeePolicyChangeState::Cancelled => {
			info!(change_id = %change.id, state = change.state.as_str(), "fee approvals: the owners passed a change that already ended; reporting it void");
			return report(
				ports,
				round,
				Some(change.id),
				&pulled.id,
				ApprovalOutcome::Void,
				"the fee-policy change ended before the owners' decision was carried out",
			)
			.await;
		}
	}
	// A pulled proposal can still go VOID before it is acted on (the owners changed):
	// re-read it by its id right before acting, and act only on PASSED.
	let approval = match ports.approvals.get(&pulled.id).await {
		Ok(approval) => approval,
		Err(err) => {
			warn!(change_id = %change.id, approval_id = %pulled.id, "fee approvals: could not re-read the proposal, will retry: {err}");
			return Ok(());
		}
	};
	if approval.state != ApprovalState::Passed || approval.id != pulled.id {
		info!(change_id = %change.id, approval_id = %pulled.id, state = approval.state.as_str(), "fee approvals: the proposal is no longer passed; not executing");
		return Ok(());
	}
	let current = ports.policies.find(&change.service).await?;
	let matches = approval.kind == FEE_POLICY_KIND
		&& approval.scope == APPROVAL_SCOPE
		&& approval.subject_ref == subject_ref(change.id)
		&& expected_hash(&change, current.as_ref()).is_ok_and(|ours| ours == approval.payload_hash);
	if !matches {
		error!(change_id = %change.id, approval_id = %approval.id, "fee approvals: the passed proposal does not match the stored change; refusing to schedule it");
		let why = "the approved terms do not match the stored fee-policy change";
		ports.changes.close_unapproved(change.id, None, why).await?;
		round.rejected += 1;
		return report(ports, round, Some(change.id), &approval.id, ApprovalOutcome::Void, why).await;
	}
	let terms = ApprovedTerms {
		approval_id: approval_uuid(&approval)?,
		policy: change.policy,
		current,
		effective_from_unix: change.effective_from_unix,
		holders: change.approval_holders,
	};
	match ports.changes.schedule_approved(change.id, &terms, now).await {
		Ok(scheduled) => {
			round.scheduled += 1;
			// WARN on purpose, as the operator's own scheduling: the terms investors are
			// charged on are about to change.
			warn!(change_id = %scheduled.id, service = %scheduled.service, approval_id = %approval.id, effective_from = scheduled.effective_from_unix, "fee approvals: the owners carried a fee-policy change; scheduled");
			report(ports, round, Some(change.id), &approval.id, ApprovalOutcome::Executed, "").await
		}
		Err(err @ DomainError::Repository(_)) => Err(err),
		Err(err) => {
			// The row moved under the approval (cancelled meanwhile, edited): concierge is
			// told it failed, and the change does not wait on a decision that cannot run.
			let why = format!("could not schedule the approved change: {err}");
			ports.changes.close_unapproved(change.id, Some(ApprovalState::ExecutionFailed), &why).await?;
			round.rejected += 1;
			report(ports, round, Some(change.id), &approval.id, ApprovalOutcome::Failed, &why).await
		}
	}
}

/// Whether the round goes on to the next change.
enum Flow {
	Next,
	/// concierge cannot be reached: the others would fail the same way.
	Stop,
}

/// The changes still waiting on concierge: propose the unproposed, follow the open ones to
/// their end, and withdraw what the operator cancelled while concierge was away.
async fn watch(ports: &FeeApprovalPorts<'_>, round: &mut ApprovalRound) -> Result<(), DomainError> {
	for change in ports.changes.watched(BATCH).await? {
		let flow = match (change.state, change.approval_id) {
			(FeePolicyChangeState::AwaitingConsilium, None) => match propose(ports, change).await? {
				Proposed::Opened(_) => {
					round.proposed += 1;
					Flow::Next
				}
				Proposed::Refused { .. } => {
					round.rejected += 1;
					Flow::Next
				}
				Proposed::Later(_) => Flow::Stop,
			},
			(FeePolicyChangeState::AwaitingConsilium, Some(approval)) => follow(ports, round, &change, approval).await?,
			(FeePolicyChangeState::Cancelled, Some(approval)) => withdraw(ports, round, &change, approval).await?,
			// `watched` lists none of these; one that turns up is left alone.
			(
				state
				@ (FeePolicyChangeState::Scheduled | FeePolicyChangeState::Active | FeePolicyChangeState::Superseded | FeePolicyChangeState::Rejected | FeePolicyChangeState::Cancelled),
				_,
			) => {
				error!(change_id = %change.id, state = state.as_str(), "fee approvals: a watched change is in a state the round does not handle");
				Flow::Next
			}
		};
		if let Flow::Stop = flow {
			break;
		}
	}
	Ok(())
}

async fn follow(ports: &FeeApprovalPorts<'_>, round: &mut ApprovalRound, change: &FeePolicyChange, approval: Uuid) -> Result<Flow, DomainError> {
	let view = match ports.approvals.get(&approval.to_string()).await {
		Ok(view) => view,
		Err(ApprovalsError::Refused(why)) => {
			// A change imported from banking waits on a consilium concierge never knew; it
			// must not hold the product's one pending slot for ever.
			error!(change_id = %change.id, approval_id = %approval, "fee approvals: concierge does not know the approval a change waits on: {why}");
			ports
				.changes
				.close_unapproved(change.id, None, "concierge does not know the approval this change waited on")
				.await?;
			round.rejected += 1;
			return Ok(Flow::Next);
		}
		Err(err) => {
			warn!(change_id = %change.id, "fee approvals: could not read the proposal, will retry: {err}");
			return Ok(Flow::Stop);
		}
	};
	let closed = |why: String| (Some(view.state), why);
	let (state, why) = match view.state {
		ApprovalState::Open | ApprovalState::Passed => {
			if change.approval_state != Some(view.state) {
				ports.changes.note_approval_state(change.id, view.state).await?;
			}
			return Ok(Flow::Next);
		}
		ApprovalState::Executed => {
			// invest reports EXECUTED only after the change is scheduled, in that order; a
			// change still awaiting under an executed proposal is a fault to read, not to fix.
			error!(change_id = %change.id, approval_id = %approval, "fee approvals: concierge says the proposal was executed, but the change was never scheduled");
			return Ok(Flow::Next);
		}
		ApprovalState::Rejected => closed("the owners rejected the change".to_owned()),
		ApprovalState::Vetoed => closed(with_note("an owner vetoed the change", &view.note)),
		ApprovalState::Expired => closed("the owners did not decide before the proposal expired".to_owned()),
		ApprovalState::Cancelled => closed("the proposal was withdrawn in concierge".to_owned()),
		ApprovalState::Void => closed(with_note("concierge voided the proposal", &view.note)),
		ApprovalState::ExecutionFailed => closed(with_note("the approved change could not be carried out", &view.note)),
	};
	ports.changes.close_unapproved(change.id, state, &why).await?;
	round.rejected += 1;
	info!(change_id = %change.id, approval_id = %approval, state = view.state.as_str(), "fee approvals: the proposal ended without passing; the change is rejected");
	Ok(Flow::Next)
}

fn with_note(what: &str, note: &str) -> String {
	if note.is_empty() { what.to_owned() } else { format!("{what}: {note}") }
}

async fn withdraw(ports: &FeeApprovalPorts<'_>, round: &mut ApprovalRound, change: &FeePolicyChange, approval: Uuid) -> Result<Flow, DomainError> {
	let id = approval.to_string();
	match ports.approvals.withdraw(&id, "the operator withdrew the fee-policy change").await {
		Ok(view) => {
			ports.changes.note_approval_state(change.id, view.state).await?;
			round.withdrawn += 1;
			Ok(Flow::Next)
		}
		// Past its delay a passed proposal is the proposer's to report, not to withdraw: the
		// pull serves it and `carry_out` reports it void. Whatever it is now is noted, so the
		// round stops asking once it has ended.
		Err(ApprovalsError::Refused(why)) => match ports.approvals.get(&id).await {
			Ok(view) => {
				info!(change_id = %change.id, approval_id = %approval, state = view.state.as_str(), "fee approvals: concierge would not withdraw the proposal of a cancelled change: {why}");
				ports.changes.note_approval_state(change.id, view.state).await?;
				Ok(Flow::Next)
			}
			Err(err) => {
				error!(change_id = %change.id, approval_id = %approval, "fee approvals: the proposal of a cancelled change can be neither withdrawn ({why}) nor read: {err}");
				Ok(Flow::Next)
			}
		},
		Err(err) => {
			warn!(change_id = %change.id, "fee approvals: could not withdraw the proposal of a cancelled change, will retry: {err}");
			Ok(Flow::Stop)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_subject_ref_round_trips_and_names_nothing_else() {
		let id = FeePolicyChangeId::from_raw(Uuid::from_u128(0x233));
		assert_eq!(subject_ref(id), "fee_policy_change:00000000-0000-0000-0000-000000000233");
		assert_eq!(parse_subject_ref(&subject_ref(id)), Some(id));
		assert_eq!(parse_subject_ref("payment:00000000-0000-0000-0000-000000000233"), None);
		assert_eq!(parse_subject_ref("fee_policy_change00000000-0000-0000-0000-000000000233"), None);
		assert_eq!(parse_subject_ref("fee_policy_change:nope"), None);
	}

	#[test]
	fn an_outcome_detail_is_one_line_concierge_takes() {
		assert_eq!(outcome_detail("conflict:\nthe row moved"), "conflict: the row moved");
		let long = "é".repeat(150);
		let cut = outcome_detail(&long);
		assert!(cut.len() <= DETAIL_BYTES && long.starts_with(&cut));
		assert_eq!(cut.chars().count(), 100);
	}
}
