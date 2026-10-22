//! Integration tests for the owners' approval of a fee-policy change through concierge
//! (`invest:fee_policy`, invest#37) — real Postgres **and** TigerBeetle, and concierge's
//! approvals in memory (`InMemoryApprovals`). They run when `DATABASE_URL` is set and a
//! TigerBeetle replica is reachable, skip otherwise, and fail instead of skipping under `CI`.
//!
//! What is pinned: a change beyond the house envelope is recorded `awaiting_consilium` and
//! proposed in its own request with the requester's proof, or refused before anything is
//! written when there is none; a change that request could not propose is closed, never
//! proposed later; a passed proposal is re-read, re-hashed against the STORED change,
//! scheduled with its notice period starting then, and reported; a proposal whose hash is not
//! the change's is refused; one that ends any other way closes the change `rejected`; and a
//! cancel withdraws the proposal, now or on a later round.

use invest_core::{
	application::{allocations as allocations_app, fee_approvals, fees as fee_app, funds as funds_app},
	ports::{
		AllocationRegistry,
		approvals::{ApprovalOutcome, ApprovalState, ApprovalValue, UserProof},
		catalog::KindLevel,
		fees::{ApprovedTerms, FeePolicies, FeePolicyChange, FeePolicyChanges, NewFeePolicyChange, PositionAccruals},
		money::ScopeId,
	},
};
use invest_domain::{
	allocations::{AllocationAccess, AllocationIcon},
	balance::ServiceId,
	error::DomainError,
	fees::{ChangeRequirement, CrystallizationPeriod, FeePolicy, FeePolicyChangeId, FeePolicyChangeState, MIN_NOTICE_SECS, ManagementBasis},
	money::Usdt,
	users::UserId,
};
use uuid::Uuid;

mod common;

/// Slack for the second or two between a test's `now()` and the one the write path stamps.
const CLOCK_SLACK: i64 = 30;

type Harness = common::Engine;

/// The engine, with the owner changes earlier tests of this binary left waiting closed: each
/// test gets a fresh in-memory concierge, which knows none of their approvals, and a round
/// would otherwise count them.
async fn harness() -> Option<Harness> {
	let h = common::engine("fee-policy approvals test").await?;
	sqlx::query("UPDATE fee_policy_changes SET state = 'rejected', closed_reason = 'left by an earlier test' WHERE state = 'awaiting_consilium'")
		.execute(&h.pool)
		.await
		.unwrap();
	sqlx::query("UPDATE fee_policy_changes SET approval_state = 'cancelled' WHERE state = 'cancelled' AND approval_state IN ('open', 'passed')")
		.execute(&h.pool)
		.await
		.unwrap();
	Some(h)
}

fn policy_ports(h: &Harness) -> fee_app::FeePolicyPorts<'_> {
	h.fee_policy_ports()
}

fn approval_ports(h: &Harness) -> fee_approvals::FeeApprovalPorts<'_> {
	fee_approvals::FeeApprovalPorts {
		approvals: &h.approvals,
		changes: &h.changes,
		policies: &h.policies,
	}
}

fn now() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// The house terms with the management rate raised past the envelope — the owners' to carry.
fn dearer() -> FeePolicy {
	FeePolicy::new(300, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual).unwrap()
}

const REASON: &str = "the new mandate costs more to run";

/// The requester's freshly confirmed token, as the BFF forwards it. Opaque here: concierge is
/// the one that verifies it.
const PROOF: &str = "requester-invest-token";

async fn open_fund(h: &Harness) -> ServiceId {
	let service = ServiceId::parse(&format!("fpa-{}", Uuid::new_v4())).unwrap();
	allocations_app::register(&h.allocations, service.clone(), &ScopeId::invest(), "EV Trading", "Systematic crypto", AllocationIcon::default())
		.await
		.unwrap();
	allocations_app::open(&h.allocations, h.money.as_ref(), &service).await.unwrap();
	h.allocations.set_access(&service, AllocationAccess::Invest).await.unwrap();
	// The house terms, installed on a fund with no holders: scheduled and promoted at once.
	let installed = schedule(h, UserId::new(), &service, FeePolicy::HOUSE).await.unwrap();
	assert!(h.changes.promote(installed.id, now()).await.unwrap());
	service
}

async fn holder(h: &Harness, service: &ServiceId) -> UserId {
	let user = common::active_user(&h.users).await;
	let amount = Usdt::parse_decimal("1000").unwrap();
	h.money.fund(&ScopeId::invest(), &common::auth_subject(user), amount);
	funds_app::subscribe(&h.fund_ports(), &h.subscriptions, &h.users, user, service.clone(), amount, now())
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert!(h.accruals.find(user, service).await.unwrap().is_some(), "the subscription's position projection landed");
	user
}

async fn schedule(h: &Harness, requester: UserId, service: &ServiceId, policy: FeePolicy) -> Result<FeePolicyChange, DomainError> {
	schedule_with(h, requester, service, policy, UserProof::new(PROOF.to_owned())).await
}

async fn schedule_with(h: &Harness, requester: UserId, service: &ServiceId, policy: FeePolicy, requester_proof: Option<UserProof>) -> Result<FeePolicyChange, DomainError> {
	fee_app::schedule_policy(
		&policy_ports(h),
		requester,
		fee_app::PolicyChangeRequest {
			service: service.clone(),
			policy,
			requested_effective_from_unix: 0,
			reason: REASON.to_owned(),
			requester_proof,
		},
		now(),
	)
	.await
}

fn is_reauth_required(err: &DomainError) -> bool {
	matches!(err, DomainError::Precondition(why) if fee_approvals::is_reauth_required(why))
}

/// A fund nobody holds, and an owner change proposed on it.
async fn proposed(h: &Harness) -> (ServiceId, FeePolicyChange, String) {
	let (service, change, approval, _) = proposed_by(h).await;
	(service, change, approval)
}

/// [`proposed`], with the requester.
async fn proposed_by(h: &Harness) -> (ServiceId, FeePolicyChange, String, UserId) {
	let service = open_fund(h).await;
	let requester = common::active_user(&h.users).await;
	let change = schedule(h, requester, &service, dearer()).await.unwrap();
	let approval = change.approval_id.expect("the change carries its proposal").to_string();
	(service, change, approval, requester)
}

async fn change_of(h: &Harness, change: &FeePolicyChange) -> FeePolicyChange {
	h.changes.find(change.id).await.unwrap().expect("the change row exists")
}

async fn closed_reason(h: &Harness, change: &FeePolicyChange) -> Option<String> {
	sqlx::query_scalar("SELECT closed_reason FROM fee_policy_changes WHERE id = $1")
		.bind(change.id.raw())
		.fetch_one(&h.pool)
		.await
		.unwrap()
}

async fn notice_count(h: &Harness, change: &FeePolicyChange) -> i64 {
	sqlx::query_scalar("SELECT COUNT(*) FROM fee_policy_notices WHERE change_id = $1")
		.bind(change.id.raw())
		.fetch_one(&h.pool)
		.await
		.unwrap()
}

async fn round(h: &Harness) -> fee_approvals::ApprovalRound {
	fee_approvals::round(&approval_ports(h), now()).await.unwrap()
}

#[tokio::test]
async fn a_change_beyond_the_envelope_is_proposed_to_the_owners_with_its_terms() {
	let Some(h) = harness().await else { return };
	let service = open_fund(&h).await;
	let requester = common::active_user(&h.users).await;

	let change = schedule(&h, requester, &service, dearer()).await.unwrap();

	assert_eq!(change.state, FeePolicyChangeState::AwaitingConsilium);
	assert_eq!(change.requirement, ChangeRequirement::OwnerConsilium);
	assert_eq!(change.scheduled_at_unix, None, "the notice clock starts when the owners carry it");
	assert_eq!(change.approval_state, Some(ApprovalState::Open));
	assert_eq!(change.approval_holders, Some(0));
	assert_eq!(notice_count(&h, &change).await, 0, "nobody is told about terms the owners have not carried");
	let proposals = h.approvals.proposals();
	assert_eq!(proposals.len(), 1);
	let (view, proposal) = &proposals[0];
	assert_eq!(Some(view.id.clone()), change.approval_id.map(|id| id.to_string()));
	assert_eq!(proposal.kind, "invest:fee_policy");
	assert_eq!(proposal.scope, "invest");
	assert_eq!(proposal.subject_ref, format!("fee_policy_change:{}", change.id));
	assert_eq!(proposal.dedupe_key, proposal.subject_ref);
	assert_eq!(proposal.reason, REASON);
	assert_eq!(proposal.requested_by, common::auth_subject(requester), "concierge mails the asker the outcome");
	assert_eq!(h.approvals.proofs(), vec![PROOF.to_owned()], "the requester's token rode with the proposal");
	let value = |key: &str| proposal.fields.iter().find(|f| f.key == key).map(|f| f.value.clone());
	assert_eq!(value("allocation"), Some(ApprovalValue::Text(service.to_string())));
	assert_eq!(value("current_management"), Some(ApprovalValue::Bps(200)));
	assert_eq!(value("proposed_management"), Some(ApprovalValue::Bps(300)));
	assert_eq!(value("raises_ceiling"), Some(ApprovalValue::Flag(true)));
	assert_eq!(value("holders_notified"), Some(ApprovalValue::Count(0)));
	assert_eq!(value("effective_from"), Some(ApprovalValue::Timestamp(change.effective_from_unix)));
	assert_eq!(view.payload_hash, fee_approvals::expected_hash(&change, Some(&FeePolicy::HOUSE)).unwrap());
	// One pending change per product holds for an awaiting one too.
	let err = schedule(&h, requester, &service, FeePolicy::HOUSE).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
}

/// D24(1): without the requester's proof concierge could not keep them off the roster, so an
/// owner change without one is refused before anything is written or proposed. An
/// administrator's change needs none.
#[tokio::test]
async fn an_owner_change_without_the_requesters_proof_is_refused_before_it_is_written() {
	let Some(h) = harness().await else { return };
	let service = open_fund(&h).await;

	let err = schedule_with(&h, common::active_user(&h.users).await, &service, dearer(), None).await.unwrap_err();

	assert!(is_reauth_required(&err), "got {err:?}");
	assert!(err.to_string().contains("reauth_required: "), "{err}");
	assert!(h.changes.pending(&service).await.unwrap().is_none(), "nothing was recorded");
	assert_eq!(h.changes.list(&service).await.unwrap().len(), 1, "only the installed terms are on record");
	assert!(h.approvals.proposals().is_empty() && h.approvals.proofs().is_empty(), "nothing was proposed");
	let cheaper = FeePolicy::new(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual).unwrap();
	assert_eq!(schedule_with(&h, UserId::new(), &service, cheaper, None).await.unwrap().state, FeePolicyChangeState::Scheduled);
}

/// concierge takes a proposal only in the name its proof's `sub` carries — the requester's
/// `auth_subject`, which invest learns from concierge. A requester it has not learned yet is
/// refused before anything is written, proof or not.
#[tokio::test]
async fn an_owner_change_by_a_requester_invest_does_not_know_yet_is_refused_before_it_is_written() {
	let Some(h) = harness().await else { return };
	let service = open_fund(&h).await;

	let err = schedule(&h, UserId::new(), &service, dearer()).await.unwrap_err();

	assert!(matches!(err, DomainError::Precondition(ref why) if why.contains("not known to invest")), "got {err:?}");
	assert!(!is_reauth_required(&err), "a fresh sign-in would not help");
	assert!(h.changes.pending(&service).await.unwrap().is_none(), "nothing was recorded");
	assert!(h.approvals.proposals().is_empty() && h.approvals.proofs().is_empty(), "nothing was proposed");
}

/// concierge did not take the proof (too old, spent, someone else's): the change is closed,
/// its pending slot is free, and the operator is told to confirm their sign-in again.
#[tokio::test]
async fn a_proof_concierge_refuses_closes_the_change_and_asks_for_a_fresh_sign_in() {
	let Some(h) = harness().await else { return };
	let service = open_fund(&h).await;
	h.approvals.refuse_proofs(Some("the sign-in is 300 s old"));

	let err = schedule(&h, common::active_user(&h.users).await, &service, dearer()).await.unwrap_err();

	assert!(is_reauth_required(&err), "got {err:?}");
	let closed = h.changes.list(&service).await.unwrap().into_iter().next().unwrap();
	assert_eq!((closed.state, closed.approval_id), (FeePolicyChangeState::Rejected, None));
	assert!(closed_reason(&h, &closed).await.is_some_and(|why| why.contains("sign-in")));
	assert!(h.changes.pending(&service).await.unwrap().is_none(), "the slot is free for the next try");

	h.approvals.refuse_proofs(None);
	let again = schedule(&h, common::active_user(&h.users).await, &service, dearer()).await.unwrap();
	assert!(again.approval_id.is_some(), "a fresh proof proposes it");
}

/// The proof is one token for one request and is not kept, so a change its request could not
/// propose is closed at once — never left for a round to propose without the proof.
#[tokio::test]
async fn a_change_concierge_could_not_take_is_closed_not_proposed_later() {
	let Some(h) = harness().await else { return };
	let service = open_fund(&h).await;

	h.approvals.outage(true);
	let err = schedule(&h, common::active_user(&h.users).await, &service, dearer()).await.unwrap_err();
	assert!(
		matches!(err, DomainError::Precondition(ref why) if why.contains("submit it again") && !fee_approvals::is_reauth_required(why)),
		"got {err:?}"
	);
	let closed = h.changes.list(&service).await.unwrap().into_iter().next().unwrap();
	assert_eq!(closed.state, FeePolicyChangeState::Rejected);
	assert!(h.changes.pending(&service).await.unwrap().is_none());

	h.approvals.outage(false);
	assert_eq!(round(&h).await, fee_approvals::ApprovalRound::default(), "the round proposes nothing");
	assert!(h.approvals.proposals().is_empty());
}

/// A change whose request died between writing it and proposing it: a young one is left to
/// its request, an old one is closed — and neither is proposed by the round.
#[tokio::test]
async fn a_change_its_request_never_proposed_is_closed_by_the_round() {
	let Some(h) = harness().await else { return };
	let unproposed = |service: &ServiceId, at: i64| NewFeePolicyChange {
		id: FeePolicyChangeId::new(),
		service: service.clone(),
		policy: dearer(),
		requirement: ChangeRequirement::OwnerConsilium,
		requested_effective_from_unix: 0,
		requested_by: UserId::new().to_string(),
		reason: REASON.to_owned(),
		now_unix: at,
	};
	let (old_fund, young_fund) = (open_fund(&h).await, open_fund(&h).await);
	let old = h
		.changes
		.schedule(&unproposed(&old_fund, now() - fee_approvals::UNPROPOSED_AFTER_SECS - CLOCK_SLACK))
		.await
		.unwrap();
	let young = h.changes.schedule(&unproposed(&young_fund, now())).await.unwrap();

	let swept = round(&h).await;

	assert_eq!(swept, fee_approvals::ApprovalRound { rejected: 1, ..Default::default() });
	assert_eq!(change_of(&h, &old).await.state, FeePolicyChangeState::Rejected);
	assert!(closed_reason(&h, &old).await.is_some_and(|why| why.contains("never put to the owners")));
	assert_eq!(change_of(&h, &young).await.state, FeePolicyChangeState::AwaitingConsilium, "its request may still propose it");
	assert!(h.approvals.proposals().is_empty() && h.approvals.proofs().is_empty(), "the round proposes nothing");
}

#[tokio::test]
async fn a_passed_proposal_is_re_hashed_scheduled_and_reported() {
	let Some(h) = harness().await else { return };
	let (_, change, approval) = proposed(&h).await;

	h.approvals.set_state(&approval, ApprovalState::Passed, "");
	let before = now();
	let first = round(&h).await;

	assert_eq!((first.scheduled, first.reported), (1, 1));
	let scheduled = change_of(&h, &change).await;
	assert_eq!(scheduled.state, FeePolicyChangeState::Scheduled);
	let scheduled_at = scheduled.scheduled_at_unix.expect("stamped when the owners carried it");
	assert!(scheduled_at >= before - CLOCK_SLACK);
	assert!(scheduled.effective_from_unix >= scheduled_at, "nothing binds before the owners' decision");
	assert_eq!(notice_count(&h, &change).await, 0, "a fund nobody holds owes nobody a notice");
	assert_eq!(scheduled.approval_state, Some(ApprovalState::Executed));
	assert_eq!(h.approvals.reports(), vec![(approval.clone(), ApprovalOutcome::Executed, String::new())]);

	// Served again (a lost report): reported again, never scheduled again.
	h.approvals.set_state(&approval, ApprovalState::Passed, "");
	h.approvals.unreport(&approval);
	let second = round(&h).await;
	assert_eq!((second.scheduled, second.reported), (0, 1));
	assert_eq!(change_of(&h, &change).await.scheduled_at_unix, Some(scheduled_at));
}

/// An owner change on a fund people hold is proposed like any other; the proposal names how
/// many holders are to be told. Its notices and the period are `tests/fee_policy_notices.rs`.
#[tokio::test]
async fn an_owner_change_on_a_held_fund_is_proposed_with_its_holders() {
	let Some(h) = harness().await else { return };
	let service = open_fund(&h).await;
	holder(&h, &service).await;
	let requester = common::active_user(&h.users).await;

	let change = schedule(&h, requester, &service, dearer()).await.unwrap();

	assert_eq!(change.state, FeePolicyChangeState::AwaitingConsilium);
	assert_eq!(change.approval_holders, Some(1));
	let proposals = h.approvals.proposals();
	assert_eq!(proposals.len(), 1, "proposed in its own request");
	let (_, proposal) = &proposals[0];
	let holders = proposal.fields.iter().find(|f| f.key == "holders_notified").map(|f| f.value.clone());
	assert_eq!(holders, Some(ApprovalValue::Count(1)), "the owners see how many holders are to be told");
}

/// A holder who joins while the owners vote is still owed the notice: the change is
/// scheduled with a notice for them, it does not bind over them until they are told, and
/// once that notice is given up on nobody may waive it — the requester included. The
/// operator withdraws the change instead.
#[tokio::test]
async fn a_holder_who_joins_while_the_owners_vote_keeps_the_owner_change_from_binding() {
	let Some(h) = harness().await else { return };
	let (service, change, approval, requester) = proposed_by(&h).await;
	holder(&h, &service).await;

	h.approvals.set_state(&approval, ApprovalState::Passed, "");
	assert_eq!(round(&h).await.scheduled, 1, "the proposal named no holder, and the stored change still says so");
	let scheduled = change_of(&h, &change).await;
	assert_eq!(scheduled.state, FeePolicyChangeState::Scheduled);
	assert!(
		scheduled.effective_from_unix >= scheduled.scheduled_at_unix.unwrap() + MIN_NOTICE_SECS,
		"the holder gets the notice period"
	);
	assert_eq!(notice_count(&h, &change).await, 1);
	sqlx::query("UPDATE fee_policy_notices SET given_up_at = now() WHERE change_id = $1")
		.bind(change.id.raw())
		.execute(&h.pool)
		.await
		.unwrap();

	let err = fee_app::acknowledge_undelivered_notices(&h.changes, &service, change.id, requester, now()).await.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(ref why) if why.contains("owners decided")), "got {err:?}");
	sqlx::query("UPDATE fee_policy_changes SET effective_from = now() - interval '1 hour' WHERE id = $1")
		.bind(change.id.raw())
		.execute(&h.pool)
		.await
		.unwrap();
	assert!(h.changes.promote(change.id, now()).await.is_err(), "dearer terms do not bind over an untold holder");
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
}

/// Two runners (two replicas, or a round overlapping the next) carry out the same passed
/// proposal. The one that loses the product lock finds the change already scheduled under
/// this approval and reports EXECUTED too — never EXECUTION_FAILED over a change that landed.
#[tokio::test]
async fn two_rounds_carrying_out_one_approval_both_report_it_executed() {
	let Some(h) = harness().await else { return };
	let (_, change, approval) = proposed(&h).await;
	h.approvals.set_state(&approval, ApprovalState::Passed, "");
	// Later than the proposal's provisional moment, so scheduling rewrites `effective_from`.
	let later = now() + 10;

	let (one, two) = (approval_ports(&h), approval_ports(&h));
	let (first, second) = tokio::join!(fee_approvals::round(&one, later), fee_approvals::round(&two, later));
	let (first, second) = (first.unwrap(), second.unwrap());

	assert!(first.scheduled + second.scheduled >= 1);
	let scheduled = change_of(&h, &change).await;
	assert_eq!(scheduled.state, FeePolicyChangeState::Scheduled);
	assert_eq!(scheduled.approval_state, Some(ApprovalState::Executed));
	let reports = h.approvals.reports();
	assert!(!reports.is_empty());
	assert!(reports.iter().all(|(id, outcome, _)| id == &approval && *outcome == ApprovalOutcome::Executed), "{reports:?}");
}

/// The store half of the same race, in a fixed order: the second scheduling under the same
/// approval arrives after the first rewrote `effective_from`, and is answered with the
/// scheduled change rather than a conflict.
#[tokio::test]
async fn scheduling_an_approved_change_twice_returns_the_scheduled_change() {
	let Some(h) = harness().await else { return };
	let (_, change, _) = proposed(&h).await;
	let terms = ApprovedTerms {
		approval_id: change.approval_id.unwrap(),
		policy: change.policy,
		current: Some(FeePolicy::HOUSE),
		effective_from_unix: change.effective_from_unix,
		holders: change.approval_holders,
	};

	let first = h.changes.schedule_approved(change.id, &terms, now() + 10).await.unwrap();
	assert_ne!(first.effective_from_unix, change.effective_from_unix, "scheduling fixed the moment anew");
	let second = h.changes.schedule_approved(change.id, &terms, now() + 20).await.unwrap();

	assert_eq!(second.state, FeePolicyChangeState::Scheduled);
	assert_eq!((second.scheduled_at_unix, second.effective_from_unix), (first.scheduled_at_unix, first.effective_from_unix));
	// Terms the owners did not pass are still refused.
	let other = ApprovedTerms { policy: FeePolicy::HOUSE, ..terms };
	assert!(matches!(h.changes.schedule_approved(change.id, &other, now()).await, Err(DomainError::Conflict(_))));
}

/// While the owners' veto window runs, the round notes the passed state and the moment the
/// window ends, so the operator can be told until when a veto is possible; the change still
/// awaits. A later read without a moment keeps the noted one.
#[tokio::test]
async fn a_passed_proposal_in_its_veto_window_shows_until_when() {
	let Some(h) = harness().await else { return };
	let (_, change, approval) = proposed(&h).await;
	let until = now() + 86_400;
	h.approvals.set_state(&approval, ApprovalState::Passed, "");
	h.approvals.set_executable_at(&approval, Some(until));
	h.approvals.hold_back(&approval, true);

	assert_eq!(round(&h).await.scheduled, 0);

	let waiting = change_of(&h, &change).await;
	assert_eq!(waiting.state, FeePolicyChangeState::AwaitingConsilium);
	assert_eq!((waiting.approval_state, waiting.approval_executable_at_unix), (Some(ApprovalState::Passed), Some(until)));
	h.changes.note_approval_state(change.id, ApprovalState::Passed, None).await.unwrap();
	assert_eq!(change_of(&h, &change).await.approval_executable_at_unix, Some(until));
}

/// A round that read the proposal before another reported its outcome writes what it read
/// late. The noted state never moves back: not from an ended state, not from passed to open.
#[tokio::test]
async fn a_stale_approval_state_never_overwrites_a_later_one() {
	let Some(h) = harness().await else { return };
	let (_, change, _) = proposed(&h).await;

	h.changes.note_approval_state(change.id, ApprovalState::Passed, None).await.unwrap();
	h.changes.note_approval_state(change.id, ApprovalState::Open, None).await.unwrap();
	assert_eq!(change_of(&h, &change).await.approval_state, Some(ApprovalState::Passed));

	h.changes.note_approval_state(change.id, ApprovalState::Executed, None).await.unwrap();
	for stale in [ApprovalState::Passed, ApprovalState::Open, ApprovalState::Void] {
		h.changes.note_approval_state(change.id, stale, None).await.unwrap();
	}
	assert_eq!(change_of(&h, &change).await.approval_state, Some(ApprovalState::Executed));
}

#[tokio::test]
async fn a_passed_proposal_whose_hash_is_not_the_changes_is_refused() {
	let Some(h) = harness().await else { return };
	let (service, change, approval) = proposed(&h).await;

	h.approvals.set_state(&approval, ApprovalState::Passed, "");
	h.approvals.tamper_hash(&approval);
	let round = round(&h).await;

	assert_eq!((round.scheduled, round.rejected), (0, 1));
	let refused = change_of(&h, &change).await;
	assert_eq!(refused.state, FeePolicyChangeState::Rejected);
	assert!(closed_reason(&h, &change).await.is_some_and(|why| why.contains("do not match")));
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE), "the terms did not move");
	let reports = h.approvals.reports();
	assert_eq!(reports.len(), 1);
	assert_eq!((reports[0].0.as_str(), reports[0].1), (approval.as_str(), ApprovalOutcome::Void));
}

/// A pass at SCOPE level is the scope admins', not the owners': the change is rejected and
/// the proposal reported void, as for a hash mismatch. An unreadable level is refused too.
#[tokio::test]
async fn a_pass_not_decided_by_the_platform_owners_is_refused() {
	let Some(h) = harness().await else { return };
	for level in [Some(KindLevel::Scope), None] {
		let (service, change, approval) = proposed(&h).await;
		h.approvals.set_state(&approval, ApprovalState::Passed, "");
		h.approvals.set_level(&approval, level);

		let round = round(&h).await;

		assert_eq!((round.scheduled, round.rejected), (0, 1), "{level:?}");
		assert_eq!(change_of(&h, &change).await.state, FeePolicyChangeState::Rejected);
		assert!(closed_reason(&h, &change).await.is_some_and(|why| why.contains("not decided by the platform owners")));
		assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE), "the terms did not move");
		assert!(h.approvals.reports().iter().any(|(id, outcome, _)| id == &approval && *outcome == ApprovalOutcome::Void));
	}
}

#[tokio::test]
async fn a_proposal_that_does_not_pass_rejects_the_change() {
	let Some(h) = harness().await else { return };
	for (state, note, words) in [
		(ApprovalState::Rejected, "", "rejected"),
		(ApprovalState::Vetoed, "not this quarter", "vetoed the change: not this quarter"),
		(ApprovalState::Expired, "", "expired"),
	] {
		let (_, change, approval) = proposed(&h).await;
		h.approvals.set_state(&approval, state, note);

		assert_eq!(round(&h).await.rejected, 1, "{state:?}");
		let closed = change_of(&h, &change).await;
		assert_eq!(closed.state, FeePolicyChangeState::Rejected, "{state:?}");
		assert_eq!(closed.approval_state, Some(state));
		assert!(closed_reason(&h, &change).await.is_some_and(|why| why.contains(words)), "{state:?}");
		assert_eq!(notice_count(&h, &change).await, 0);
	}
	assert!(h.approvals.reports().is_empty(), "a decision that did not pass has no outcome to report");
}

#[tokio::test]
async fn an_open_proposal_follows_concierge_and_a_refused_one_closes_at_once() {
	let Some(h) = harness().await else { return };
	let (_, change, approval) = proposed(&h).await;
	h.approvals.set_state(&approval, ApprovalState::Passed, "");
	// Passed but inside its veto delay: concierge does not serve it yet, the state shows.
	h.approvals.hold_back(&approval, true);
	round(&h).await;
	let waiting = change_of(&h, &change).await;
	assert_eq!((waiting.state, waiting.approval_state), (FeePolicyChangeState::AwaitingConsilium, Some(ApprovalState::Passed)));

	let service = open_fund(&h).await;
	h.approvals.refuse_proposals(Some("FailedPrecondition: the platform pauses after its owners changed"));
	let err = schedule(&h, common::active_user(&h.users).await, &service, dearer()).await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref why) if why.contains("pauses")), "got {err:?}");
	let refused = h.changes.list(&service).await.unwrap().into_iter().next().unwrap();
	assert_eq!(refused.state, FeePolicyChangeState::Rejected, "the history keeps the refused change");
	assert!(h.changes.pending(&service).await.unwrap().is_none(), "and it holds no pending slot");
}

#[tokio::test]
async fn cancelling_a_proposed_change_withdraws_its_proposal() {
	let Some(h) = harness().await else { return };
	let (service, change, approval, requester) = proposed_by(&h).await;

	let cancelled = fee_app::cancel_change(&policy_ports(&h), &service, change.id, requester, now()).await.unwrap();

	assert_eq!(cancelled.state, FeePolicyChangeState::Cancelled);
	assert_eq!(h.approvals.withdrawals(), vec![approval.clone()]);
	assert_eq!(change_of(&h, &change).await.approval_state, Some(ApprovalState::Cancelled));
	assert_eq!(round(&h).await, fee_approvals::ApprovalRound::default(), "nothing is left to withdraw");
}

/// Banking let only the proposer (or an owner) withdraw a consilium change. Any other
/// operator is refused, and the proposal stays open in concierge.
#[tokio::test]
async fn only_the_requester_may_withdraw_a_change_the_owners_decide() {
	let Some(h) = harness().await else { return };
	let (service, change, approval) = proposed(&h).await;
	let other = common::active_user(&h.users).await;

	let err = fee_app::cancel_change(&policy_ports(&h), &service, change.id, other, now()).await.unwrap_err();

	assert!(matches!(err, DomainError::Forbidden(ref why) if why.contains("only by the one who requested it")), "got {err:?}");
	let kept = change_of(&h, &change).await;
	assert_eq!(kept.state, FeePolicyChangeState::AwaitingConsilium);
	assert_eq!(kept.approval_id.map(|id| id.to_string()), Some(approval));
	assert!(h.approvals.withdrawals().is_empty());
}

#[tokio::test]
async fn a_withdrawal_concierge_missed_is_made_by_the_round_and_a_late_pass_is_reported_void() {
	let Some(h) = harness().await else { return };
	let (service, change, approval, requester) = proposed_by(&h).await;

	h.approvals.outage(true);
	let cancelled = fee_app::cancel_change(&policy_ports(&h), &service, change.id, requester, now()).await.unwrap();
	assert_eq!(cancelled.state, FeePolicyChangeState::Cancelled, "the operator's word ends it here at once");
	assert!(h.approvals.withdrawals().is_empty());
	h.approvals.outage(false);
	assert_eq!(round(&h).await.withdrawn, 1);
	assert_eq!(h.approvals.withdrawals(), vec![approval]);

	// Past its delay a passed proposal cannot be withdrawn; the pull serves it and the round
	// reports it void rather than scheduling a change the operator cancelled.
	let (service, change, approval, requester) = proposed_by(&h).await;
	h.approvals.set_state(&approval, ApprovalState::Passed, "");
	h.approvals.outage(true);
	fee_app::cancel_change(&policy_ports(&h), &service, change.id, requester, now()).await.unwrap();
	h.approvals.outage(false);
	round(&h).await;
	assert_eq!(change_of(&h, &change).await.state, FeePolicyChangeState::Cancelled);
	assert!(h.approvals.reports().iter().any(|(id, outcome, _)| id == &approval && *outcome == ApprovalOutcome::Void));
	assert_eq!(change_of(&h, &change).await.approval_state, Some(ApprovalState::Void));
}
