//! Integration tests for the holders' notice of a fee-policy change — real Postgres **and**
//! TigerBeetle, and concierge's scope inbox in memory (`InMemoryNotifier`). They run when
//! `DATABASE_URL` is set and a TigerBeetle replica is reachable, skip otherwise, and fail
//! instead of skipping under `CI`.
//!
//! What is pinned: a notice is recorded undelivered with the change, sent once through
//! concierge and stamped delivered; a transient failure is retried after a backoff and given
//! up on when the attempts are spent; a holder concierge will not deliver to (not a member,
//! the topic switched off) is given up on at once; a second sender, or a sender after a lost
//! answer, never sends a second message; a withdrawn notice is never sent; and an owner change
//! on a held fund is taken, binds only after the notice period and the holders' notices, and
//! cannot be waived when a notice was given up on.

use std::sync::Arc;

use invest_core::{
	application::{
		allocations as allocations_app, fee_approvals,
		fee_notices::{self, MAX_ATTEMPTS},
		fees as fee_app, funds as funds_app,
	},
	infrastructure::{
		concierge::{fake::InMemoryNotifier, notices::UnconfiguredNotifier},
		fee_notices::FeeNoticeSender,
	},
	ports::{
		AllocationRegistry,
		approvals::{ApprovalState, UserProof},
		fees::{FeePolicies, FeePolicyChange, FeePolicyChanges, PositionAccruals},
		money::ScopeId,
		notices::{HolderNotifier, NotifyError, PendingNotice},
	},
};
use invest_domain::{
	allocations::{AllocationAccess, AllocationIcon},
	balance::ServiceId,
	error::DomainError,
	fees::{ChangeRequirement, CrystallizationPeriod, FeePolicy, FeePolicyChangeState, MIN_NOTICE_SECS, ManagementBasis},
	money::Usdt,
	users::UserId,
};
use uuid::Uuid;

mod common;

struct Harness {
	engine: common::Engine,
	notifier: InMemoryNotifier,
	sender: FeeNoticeSender,
}

/// The engine and a sender over the in-memory inbox, with the notices earlier tests of this
/// binary left pending set aside: a round sends every due notice, theirs included.
async fn harness() -> Option<Harness> {
	let engine = common::engine("fee-policy notices test").await?;
	sqlx::query("UPDATE fee_policy_notices SET given_up_at = now(), last_error = 'left by an earlier test' WHERE delivered_at IS NULL AND given_up_at IS NULL AND withdrawn_at IS NULL")
		.execute(&engine.pool)
		.await
		.unwrap();
	// Each test brings a fresh in-memory concierge, which knows none of their approvals.
	sqlx::query("UPDATE fee_policy_changes SET state = 'rejected', closed_reason = 'left by an earlier test' WHERE state = 'awaiting_consilium'")
		.execute(&engine.pool)
		.await
		.unwrap();
	sqlx::query("UPDATE fee_policy_changes SET approval_state = 'cancelled' WHERE state = 'cancelled' AND approval_state IN ('open', 'passed')")
		.execute(&engine.pool)
		.await
		.unwrap();
	let notifier = InMemoryNotifier::new();
	let sender = FeeNoticeSender::new(engine.pool.clone(), Arc::new(notifier.clone()));
	Some(Harness { engine, notifier, sender })
}

fn now() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// The terms every fund here starts on: below the house envelope, so the house terms are an
/// administrator's tightening.
fn starting() -> FeePolicy {
	FeePolicy::new(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual).unwrap()
}

/// Dearer, within the envelope: one administrator's call.
fn dearer() -> FeePolicy {
	FeePolicy::HOUSE
}

/// Beyond the house envelope: the owners' to carry.
fn owners_terms() -> FeePolicy {
	FeePolicy::new(300, 2_500, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Quarterly).unwrap()
}

fn cheaper() -> FeePolicy {
	FeePolicy::new(50, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual).unwrap()
}

/// An open fund in invest's scope on the [`starting`] terms, titled `title`.
async fn open_fund(h: &Harness, title: &str) -> ServiceId {
	let e = &h.engine;
	let service = ServiceId::parse(&format!("fpn-{}", Uuid::new_v4())).unwrap();
	allocations_app::register(&e.allocations, service.clone(), &ScopeId::invest(), title, "Systematic crypto", AllocationIcon::default())
		.await
		.unwrap();
	allocations_app::open(&e.allocations, e.money.as_ref(), &service).await.unwrap();
	e.allocations.set_access(&service, AllocationAccess::Invest).await.unwrap();
	let installed = schedule(h, UserId::new(), &service, starting(), None).await.unwrap();
	assert!(e.changes.promote(installed.id, now()).await.unwrap());
	service
}

/// A holder of `service`, a member of invest's scope in concierge unless `member` is false.
async fn holder(h: &Harness, service: &ServiceId, member: bool) -> UserId {
	let e = &h.engine;
	let user = common::active_user(&e.users).await;
	let amount = Usdt::parse_decimal("1000").unwrap();
	e.money.fund(&ScopeId::invest(), &common::auth_subject(user), amount);
	funds_app::subscribe(&e.fund_ports(), &e.subscriptions, &e.users, user, service.clone(), amount, now())
		.await
		.unwrap();
	common::drain_to_quiescence(&e.relay, &e.pool).await;
	assert!(e.accruals.find(user, service).await.unwrap().is_some(), "the subscription's position projection landed");
	if member {
		h.notifier.join(user);
	}
	user
}

async fn schedule(h: &Harness, requester: UserId, service: &ServiceId, policy: FeePolicy, proof: Option<&str>) -> Result<FeePolicyChange, DomainError> {
	fee_app::schedule_policy(
		&h.engine.fee_policy_ports(),
		requester,
		fee_app::PolicyChangeRequest {
			service: service.clone(),
			policy,
			requested_effective_from_unix: 0,
			reason: "the new mandate costs more to run".to_owned(),
			requester_proof: proof.and_then(|token| UserProof::new(token.to_owned())),
		},
		now(),
	)
	.await
}

async fn change_of(h: &Harness, change: &FeePolicyChange) -> FeePolicyChange {
	h.engine.changes.find(change.id).await.unwrap().expect("the change row exists")
}

/// One notice row: delivered, given up, withdrawn, attempts, next attempt, last error.
type Row = (bool, bool, bool, i32, Option<i64>, Option<String>);

async fn notice_row(h: &Harness, change: &FeePolicyChange, user: UserId) -> Row {
	sqlx::query_as(
		"SELECT delivered_at IS NOT NULL, given_up_at IS NOT NULL, withdrawn_at IS NOT NULL, attempts, \
		 EXTRACT(EPOCH FROM next_attempt_at)::bigint, last_error FROM fee_policy_notices WHERE change_id = $1 AND user_id = $2",
	)
	.bind(change.id.raw())
	.bind(user.raw())
	.fetch_one(&h.engine.pool)
	.await
	.unwrap()
}

async fn let_the_notice_run(h: &Harness, change: &FeePolicyChange) {
	sqlx::query("UPDATE fee_policy_changes SET effective_from = now() - interval '1 hour' WHERE id = $1")
		.bind(change.id.raw())
		.execute(&h.engine.pool)
		.await
		.unwrap();
}

/// An owner change on `service`, proposed and passed by the owners, and scheduled by the
/// approvals round.
async fn owners_pass(h: &Harness, service: &ServiceId, requester: UserId) -> FeePolicyChange {
	let e = &h.engine;
	let change = schedule(h, requester, service, owners_terms(), Some("requester-invest-token")).await.unwrap();
	assert_eq!((change.requirement, change.state), (ChangeRequirement::OwnerConsilium, FeePolicyChangeState::AwaitingConsilium));
	let approval = change.approval_id.expect("proposed in its own request").to_string();
	e.approvals.set_state(&approval, ApprovalState::Passed, "");
	let ports = fee_approvals::FeeApprovalPorts {
		approvals: &e.approvals,
		changes: &e.changes,
		policies: &e.policies,
	};
	assert_eq!(fee_approvals::round(&ports, now()).await.unwrap().scheduled, 1);
	change_of(h, &change).await
}

#[tokio::test]
async fn every_holder_is_sent_one_notice_and_it_is_stamped_delivered() {
	let Some(h) = harness().await else { return };
	let service = open_fund(&h, "EV Trading").await;
	let first = holder(&h, &service, true).await;
	let second = holder(&h, &service, true).await;
	let change = schedule(&h, UserId::new(), &service, dearer(), None).await.unwrap();
	assert_eq!((change.undelivered_notices, change.notices_given_up), (2, 0), "recorded undelivered, with the change");

	let round = h.sender.round(now()).await.unwrap();

	assert_eq!((round.delivered, round.retried, round.given_up), (2, 0, 0));
	for user in [first, second] {
		let (delivered, given_up, withdrawn, attempts, next, error) = notice_row(&h, &change, user).await;
		assert!(delivered && !given_up && !withdrawn, "{user}");
		assert_eq!((attempts, next, error), (1, None, None));
	}
	let written = h.notifier.written();
	assert_eq!(written.len(), 2);
	let notice = written.iter().find(|n| n.user == first).expect("the first holder was told");
	assert_eq!((notice.topic, notice.kind, notice.link_scope), ("invest:fee-terms", "fee_terms_change", "invest"));
	assert_eq!(notice.title, "New fee terms for EV Trading");
	assert_eq!(notice.link, format!("/invest/{service}"));
	assert_eq!(notice.dedupe_key, format!("fee_policy_notice:{}", change.id));
	assert_eq!(notice.occurred_at_unix, change.scheduled_at_unix.unwrap());
	assert!(notice.body.contains("Management fee: 1% a year → 2% a year"), "the old terms and the new: {}", notice.body);
	assert!(notice.body.contains("Until then, your current terms stay in force."), "{}", notice.body);

	// Nothing is due any more: a second round, or a second sender after a restart, sends nothing.
	assert_eq!(h.sender.round(now()).await.unwrap(), Default::default());
	let restarted = FeeNoticeSender::new(h.engine.pool.clone(), Arc::new(h.notifier.clone()));
	assert_eq!(restarted.round(now()).await.unwrap(), Default::default());
	assert_eq!(h.notifier.calls().len(), 2, "one call per notice");

	// Every holder told: a dearer change binds once its moment comes, with nobody's help.
	assert_eq!(change_of(&h, &change).await.undelivered_notices, 0);
	let_the_notice_run(&h, &change).await;
	assert!(h.engine.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.engine.policies.find(&service).await.unwrap(), Some(dearer()));
}

#[tokio::test]
async fn a_transient_failure_is_retried_after_a_backoff_and_then_delivered() {
	let Some(h) = harness().await else { return };
	let service = open_fund(&h, "EV Yield").await;
	let investor = holder(&h, &service, true).await;
	let change = schedule(&h, UserId::new(), &service, dearer(), None).await.unwrap();
	h.notifier
		.fail_next([NotifyError::Unavailable("connection refused".into()), NotifyError::Refused("topic not published yet".into())]);
	let t0 = now();

	assert_eq!(h.sender.round(t0).await.unwrap().retried, 1);
	let (delivered, given_up, _, attempts, next, error) = notice_row(&h, &change, investor).await;
	assert!(!delivered && !given_up);
	assert_eq!((attempts, next), (1, Some(t0 + fee_notices::retry_delay(1))));
	assert!(error.unwrap().contains("connection refused"));
	// Not due before its moment.
	assert_eq!(h.sender.round(t0 + 29).await.unwrap(), Default::default());

	assert_eq!(h.sender.round(t0 + 30).await.unwrap().retried, 1, "a refusal is retried too: the cure is a deploy");
	let (_, _, _, attempts, next, _) = notice_row(&h, &change, investor).await;
	assert_eq!((attempts, next), (2, Some(t0 + 30 + fee_notices::retry_delay(2))));

	assert_eq!(h.sender.round(t0 + 90).await.unwrap().delivered, 1);
	let (delivered, given_up, _, attempts, next, error) = notice_row(&h, &change, investor).await;
	assert!(delivered && !given_up);
	assert_eq!((attempts, next, error), (3, None, None));
	assert_eq!(h.notifier.written().len(), 1);
}

#[tokio::test]
async fn a_notice_is_given_up_on_when_the_attempts_are_spent() {
	let Some(h) = harness().await else { return };
	let service = open_fund(&h, "EV Credit").await;
	let investor = holder(&h, &service, true).await;
	let change = schedule(&h, UserId::new(), &service, dearer(), None).await.unwrap();
	h.notifier.fail_next((0..MAX_ATTEMPTS).map(|_| NotifyError::Unavailable("timed out".into())));

	let mut at = now();
	for _ in 1..MAX_ATTEMPTS {
		assert_eq!(h.sender.round(at).await.unwrap().retried, 1);
		at += 3_600;
	}
	assert_eq!(h.sender.round(at).await.unwrap().given_up, 1);

	let (delivered, given_up, _, attempts, next, error) = notice_row(&h, &change, investor).await;
	assert!(!delivered && given_up);
	assert_eq!((attempts, next), (i32::try_from(MAX_ATTEMPTS).unwrap(), None));
	assert!(error.unwrap().starts_with("gave up after 10 attempts"));
	assert_eq!(change_of(&h, &change).await.notices_given_up, 1);
	assert!(h.notifier.written().is_empty());
}

#[tokio::test]
async fn a_non_member_is_given_up_on_at_once_and_a_holder_who_switched_it_off_is_still_told() {
	let Some(h) = harness().await else { return };
	let service = open_fund(&h, "EV Index").await;
	let stranger = holder(&h, &service, false).await;
	let quiet = holder(&h, &service, true).await;
	h.notifier.opt_out(quiet);
	let change = schedule(&h, UserId::new(), &service, dearer(), None).await.unwrap();

	let round = h.sender.round(now()).await.unwrap();

	assert_eq!((round.delivered, round.given_up), (1, 1));
	let (delivered, given_up, _, attempts, _, error) = notice_row(&h, &change, stranger).await;
	assert!(!delivered && given_up && attempts == 1);
	assert!(error.is_some(), "the row says why");
	// The topic is required: a switch-off does not keep the notice from a member.
	assert!(notice_row(&h, &change, quiet).await.0, "delivered");
	assert_eq!(h.notifier.written().iter().map(|n| n.user).collect::<Vec<_>>(), vec![quiet]);
	let change = change_of(&h, &change).await;
	assert_eq!((change.undelivered_notices, change.notices_given_up), (1, 1));
	// Dearer terms wait: on its requester's acknowledgement, or for good.
	let_the_notice_run(&h, &change).await;
	assert!(h.engine.changes.promote(change.id, now()).await.is_err());
}

#[tokio::test]
async fn a_retry_after_a_lost_answer_sends_no_second_message() {
	let Some(h) = harness().await else { return };
	let service = open_fund(&h, "EV Venture").await;
	let investor = holder(&h, &service, true).await;
	let change = schedule(&h, UserId::new(), &service, dearer(), None).await.unwrap();
	// The process died after concierge took the notice and before the row was stamped: the
	// notice is still pending, and concierge holds its dedupe key.
	let pending = PendingNotice { change: change.id, user: investor };
	let lost = {
		use invest_core::ports::notices::FeeNoticeOutbox;
		let outbox = invest_core::infrastructure::fee_notices::PgFeeNotices::new(h.engine.pool.clone());
		let claim = outbox.claim(pending).await.unwrap().expect("pending");
		let notice = fee_notices::notice_for(pending, claim.subject(), now()).unwrap();
		drop(claim);
		notice
	};
	h.notifier.notify(&lost).await.unwrap();

	let restarted = FeeNoticeSender::new(h.engine.pool.clone(), Arc::new(h.notifier.clone()));
	restarted.round(now()).await.unwrap();

	assert_eq!(h.notifier.written().len(), 1, "the holder got one message");
	// concierge answers a repeated key as "not delivered"; the sender cannot tell, so it holds
	// the holders' side of the doubt.
	let (delivered, given_up, ..) = notice_row(&h, &change, investor).await;
	assert!(!delivered && given_up);
}

#[tokio::test]
async fn a_withdrawn_notice_is_never_sent() {
	let Some(h) = harness().await else { return };
	let service = open_fund(&h, "EV Arbitrage").await;
	let investor = holder(&h, &service, true).await;
	let requester = UserId::new();
	let change = schedule(&h, requester, &service, dearer(), None).await.unwrap();
	fee_app::cancel_change(&h.engine.fee_policy_ports(), &service, change.id, requester, now()).await.unwrap();

	assert_eq!(h.sender.round(now()).await.unwrap(), Default::default());
	assert!(h.notifier.calls().is_empty());
	let (delivered, given_up, withdrawn, ..) = notice_row(&h, &change, investor).await;
	assert!(!delivered && !given_up && withdrawn);
}

#[tokio::test]
async fn without_concierge_every_notice_is_given_up_on_at_once() {
	let Some(h) = harness().await else { return };
	let service = open_fund(&h, "EV Treasury").await;
	let investor = holder(&h, &service, true).await;
	let requester = UserId::new();
	let change = schedule(&h, requester, &service, dearer(), None).await.unwrap();
	let unconfigured = FeeNoticeSender::new(h.engine.pool.clone(), Arc::new(UnconfiguredNotifier));

	assert_eq!(unconfigured.round(now()).await.unwrap().given_up, 1);
	let (_, given_up, _, _, _, error) = notice_row(&h, &change, investor).await;
	assert!(given_up && error.unwrap().contains("no notice channel"));
	// As before invest had a channel: the requester may acknowledge the holder.
	let acknowledged = fee_app::acknowledge_undelivered_notices(&h.engine.changes, &service, change.id, requester, now()).await.unwrap();
	assert!(acknowledged.notices_waiver.is_some());
}

/// The owners decide a change on a fund people hold: it is taken, the notice period starts
/// when they pass it, the holders are told, and the terms bind only once both are true.
#[tokio::test]
async fn an_owner_change_on_a_held_fund_is_taken_and_binds_after_the_notice_period() {
	let Some(h) = harness().await else { return };
	let service = open_fund(&h, "EV Real Estate").await;
	let investor = holder(&h, &service, true).await;
	let requester = common::active_user(&h.engine.users).await;

	let before = now();
	let change = owners_pass(&h, &service, requester).await;
	assert_eq!(change.state, FeePolicyChangeState::Scheduled);
	let scheduled_at = change.scheduled_at_unix.expect("stamped when the owners carried it");
	assert!(scheduled_at >= before);
	assert!(change.effective_from_unix >= scheduled_at + MIN_NOTICE_SECS, "the holders get the full notice period");
	assert_eq!(change.undelivered_notices, 1);

	assert_eq!(h.sender.round(now()).await.unwrap().delivered, 1);
	let notice = h.notifier.written().pop().expect("the holder was told");
	assert_eq!(notice.user, investor);
	assert!(notice.body.contains("The performance fee is settled: once a year → every quarter"), "{}", notice.body);

	// Told, but the notice period has not run: delivery does not shorten it.
	assert_eq!(change_of(&h, &change).await.effective_from_unix, change.effective_from_unix);
	assert!(!h.engine.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.engine.policies.find(&service).await.unwrap(), Some(starting()));

	let_the_notice_run(&h, &change).await;
	assert!(h.engine.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.engine.policies.find(&service).await.unwrap(), Some(owners_terms()));
}

/// The owners approved terms the holders were to be told about: a notice given up on is not
/// waived by anyone, the change does not bind, and the requester withdraws it.
#[tokio::test]
async fn an_undelivered_notice_of_an_owner_change_cannot_be_waived() {
	let Some(h) = harness().await else { return };
	let service = open_fund(&h, "EV Commodity").await;
	let stranger = holder(&h, &service, false).await;
	let requester = common::active_user(&h.engine.users).await;
	let change = owners_pass(&h, &service, requester).await;

	assert_eq!(h.sender.round(now()).await.unwrap().given_up, 1);
	assert!(notice_row(&h, &change, stranger).await.1, "given up on");

	let err = fee_app::acknowledge_undelivered_notices(&h.engine.changes, &service, change.id, requester, now())
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(ref why) if why.contains("owners decided")), "got {err:?}");
	let_the_notice_run(&h, &change).await;
	assert!(h.engine.changes.promote(change.id, now()).await.is_err(), "dearer terms do not bind over an untold holder");
	assert_eq!(h.engine.policies.find(&service).await.unwrap(), Some(starting()));

	let cancelled = fee_app::cancel_change(&h.engine.fee_policy_ports(), &service, change.id, requester, now()).await.unwrap();
	assert_eq!(cancelled.state, FeePolicyChangeState::Cancelled);
}

/// A loosening binds by itself, even over a holder nobody reached; the holder who can be
/// reached is still told, after the terms already apply.
#[tokio::test]
async fn a_notice_of_terms_already_in_force_is_still_sent_and_says_so() {
	let Some(h) = harness().await else { return };
	let service = open_fund(&h, "EV Fund").await;
	let investor = holder(&h, &service, true).await;
	let change = schedule(&h, UserId::new(), &service, cheaper(), None).await.unwrap();
	let_the_notice_run(&h, &change).await;
	assert!(h.engine.changes.promote(change.id, now()).await.unwrap());

	assert_eq!(h.sender.round(now()).await.unwrap().delivered, 1);
	let notice = h.notifier.written().pop().unwrap();
	assert_eq!(notice.user, investor);
	assert!(
		notice.body.contains("Management fee: 1% a year → 0.5% a year"),
		"the old terms are the ones it replaced: {}",
		notice.body
	);
	assert!(!notice.body.contains("Until then"), "{}", notice.body);
}
