//! Integration tests for changing a fund's fee terms (#233) — real Postgres **and**
//! TigerBeetle (no mocks of either). They run when `DATABASE_URL` is set and a TigerBeetle
//! replica is reachable (`nix run .#db` + `.#tb`), skip otherwise, and fail instead of
//! skipping under `CI`.
//!
//! What is pinned here is the LIFE of a change rather than the arithmetic of a fee (that is
//! `tests/fee_policy.rs`): the notice period binds exactly while someone holds units; every
//! holder is owed one notice when a change becomes scheduled; dearer terms bind over a
//! holder only once they were told or the requester acknowledged them; one change is on its
//! way per product at a time; and a promotion settles the elapsed window at the OLD rate
//! before the new one binds.
//!
//! Moved from banking 9ffafce3 (`piggybank/core/tests/fee_policy_changes.rs`), cut to what
//! invest does. Two things are not here yet, and their tests stayed in banking: the
//! owners' approval of a change beyond the house envelope (concierge's approvals; until
//! invest is their client such a change is refused, which is pinned below), and delivering
//! the holders' notices (concierge's scope notifications; until then a notice is recorded
//! given up on). A notifier's outcomes are stood in for by writing `fee_policy_notices`
//! directly — the columns it will stamp are the ones the promotion gate reads. banking's
//! legacy-row tests are not here either: invest's ceilings are validated CHECKs (0001), so
//! an active row above them cannot exist to be lowered.

use invest_core::{
	application::{allocations as allocations_app, fees as fee_app, funds as funds_app},
	infrastructure::fee_policy_changes::PgFeePolicyChanges,
	ports::{
		AllocationRegistry,
		fees::{FeePolicies, FeePolicyChange, FeePolicyChanges, NewFeePolicyChange, PositionAccruals},
		money::ScopeId,
	},
};
use invest_domain::{
	allocations::{AllocationAccess, AllocationIcon},
	balance::{LedgerAccountKey, ServiceId},
	error::DomainError,
	fees::{ChangeRequirement, CrystallizationPeriod, FeePolicy, FeePolicyChangeId, FeePolicyChangeState, MAX_EFFECTIVE_FROM_HORIZON_SECS, MIN_NOTICE_SECS, ManagementBasis},
	money::{Shares, Usdt},
	users::UserId,
};
use sqlx::PgPool;
use uuid::Uuid;

mod common;

const YEAR: i64 = 365 * 24 * 60 * 60;
/// Slack for the second or two between a test's `now()` and the one the write path stamps.
const CLOCK_SLACK: i64 = 30;

type Harness = common::Engine;

async fn harness() -> Option<Harness> {
	common::engine("fee-policy-change test").await
}

fn policy_ports(h: &Harness) -> fee_app::FeePolicyPorts<'_> {
	fee_app::FeePolicyPorts {
		policies: &h.policies,
		changes: &h.changes,
		allocations: &h.allocations,
	}
}

/// The terms as an `AllocationManage` holder reads them: every product, hidden or not.
fn manager(h: &Harness) -> fee_app::PolicyReader<'_> {
	fee_app::PolicyReader {
		allocations: &h.allocations,
		caller: UserId::new(),
		unrestricted: true,
	}
}

/// The terms as one investor reads them: only the products they can see.
fn reader(h: &Harness, caller: UserId) -> fee_app::PolicyReader<'_> {
	fee_app::PolicyReader {
		allocations: &h.allocations,
		caller,
		unrestricted: false,
	}
}

fn now() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn policy(management: u32, performance: u32, hurdle: u32, basis: ManagementBasis, period: CrystallizationPeriod) -> FeePolicy {
	FeePolicy::new(management, performance, hurdle, basis, period).unwrap()
}

/// Below the house terms, so a rise back to them tightens without needing the owners.
fn cheaper() -> FeePolicy {
	policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual)
}

/// The house terms with the management rate raised past the envelope — the change that
/// needs the owners.
fn dearer() -> FeePolicy {
	policy(300, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual)
}

fn unique_service() -> ServiceId {
	ServiceId::parse(&format!("fpc-{}", Uuid::new_v4())).unwrap()
}

/// A registered, open product with NO policy yet — every test decides its own terms.
async fn open_fund(h: &Harness, service: &ServiceId) {
	allocations_app::register(&h.allocations, service.clone(), &ScopeId::invest(), "EV Trading", "Systematic crypto", AllocationIcon::default())
		.await
		.unwrap();
	allocations_app::open(&h.allocations, h.money.as_ref(), service).await.unwrap();
	h.allocations.set_access(service, AllocationAccess::Invest).await.unwrap();
}

async fn schedule(h: &Harness, requester: UserId, service: &ServiceId, policy: FeePolicy, effective_from: i64, reason: &str) -> Result<FeePolicyChange, DomainError> {
	fee_app::schedule_policy(
		&policy_ports(h),
		requester,
		fee_app::PolicyChangeRequest {
			service: service.clone(),
			policy,
			requested_effective_from_unix: effective_from,
			reason: reason.to_owned(),
		},
		now(),
	)
	.await
}

/// Install terms on a fund with no holders: scheduled and promoted in one breath.
async fn install(h: &Harness, service: &ServiceId, policy: FeePolicy) {
	let change = schedule(h, UserId::new(), service, policy, 0, "").await.unwrap();
	assert!(h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(service).await.unwrap(), Some(policy));
}

async fn person(h: &Harness) -> UserId {
	common::active_user(&h.users).await
}

/// A holder of `amount` USDT of the product, subscribed through banking and projected.
async fn holder(h: &Harness, service: &ServiceId, amount: &str) -> UserId {
	let user = person(h).await;
	h.money.fund(&ScopeId::invest(), &common::auth_subject(user), usdt(amount));
	funds_app::subscribe(&h.fund_ports(), &h.subscriptions, &h.users, user, service.clone(), usdt(amount), now())
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert!(
		h.accruals.find(user, service).await.unwrap().is_some_and(|accrual| !accrual.cost_basis.is_zero()),
		"the subscription's position projection landed"
	);
	user
}

async fn backdate(h: &Harness, user: UserId, service: &ServiceId, secs: i64) {
	sqlx::query(
		"UPDATE fund_positions SET fees_accrued_at = now() - make_interval(secs => $3), \
		 crystallized_at = now() - make_interval(secs => $3) WHERE user_id = $1 AND service = $2",
	)
	.bind(user.raw())
	.bind(service.as_str())
	.bind(secs as f64)
	.execute(&h.pool)
	.await
	.unwrap();
}

/// Stand in for the notice period having run: pull a scheduled change's moment into the past.
async fn let_the_notice_run(h: &Harness, change: &FeePolicyChange) {
	sqlx::query("UPDATE fee_policy_changes SET effective_from = now() - interval '1 hour' WHERE id = $1")
		.bind(change.id.raw())
		.execute(&h.pool)
		.await
		.unwrap();
}

/// Stand in for a notifier having reached `user` after all.
async fn deliver_notice(h: &Harness, change: &FeePolicyChange, user: UserId) {
	sqlx::query("UPDATE fee_policy_notices SET delivered_at = now(), given_up_at = NULL WHERE change_id = $1 AND user_id = $2")
		.bind(change.id.raw())
		.bind(user.raw())
		.execute(&h.pool)
		.await
		.unwrap();
}

/// Stand in for a notifier still trying to reach `user` — neither delivered nor given up.
async fn still_trying(h: &Harness, change: &FeePolicyChange, user: UserId) {
	sqlx::query("UPDATE fee_policy_notices SET given_up_at = NULL WHERE change_id = $1 AND user_id = $2")
		.bind(change.id.raw())
		.bind(user.raw())
		.execute(&h.pool)
		.await
		.unwrap();
}

/// Stand in for a notifier giving up on `user`.
async fn give_up(h: &Harness, change: &FeePolicyChange, user: UserId) {
	sqlx::query("UPDATE fee_policy_notices SET given_up_at = now() WHERE change_id = $1 AND user_id = $2")
		.bind(change.id.raw())
		.bind(user.raw())
		.execute(&h.pool)
		.await
		.unwrap();
}

/// One notice row as the table holds it.
#[derive(Debug)]
struct Notice {
	user: Uuid,
	delivered: bool,
	given_up: bool,
	withdrawn: bool,
}

/// Every notice of a change, by holder id.
async fn notices(h: &Harness, change: &FeePolicyChange) -> Vec<Notice> {
	let rows: Vec<(Uuid, bool, bool, bool)> =
		sqlx::query_as("SELECT user_id, delivered_at IS NOT NULL, given_up_at IS NOT NULL, withdrawn_at IS NOT NULL FROM fee_policy_notices WHERE change_id = $1 ORDER BY user_id")
			.bind(change.id.raw())
			.fetch_all(&h.pool)
			.await
			.unwrap();
	rows.into_iter()
		.map(|(user, delivered, given_up, withdrawn)| Notice {
			user,
			delivered,
			given_up,
			withdrawn,
		})
		.collect()
}

async fn change_of(h: &Harness, change: &FeePolicyChange) -> FeePolicyChange {
	h.changes.find(change.id).await.unwrap().expect("the change row exists")
}

async fn fee_debt_of(h: &Harness, user: UserId, service: &ServiceId) -> Usdt {
	h.accruals.find(user, service).await.unwrap().expect("a position").debt
}

fn assert_close(actual: Usdt, expected: Usdt, what: &str) {
	let epsilon = usdt("0.05");
	let diff = if actual > expected {
		actual.checked_sub(expected).unwrap()
	} else {
		expected.checked_sub(actual).unwrap()
	};
	assert!(diff <= epsilon, "{what}: expected ~{expected}, got {actual}");
}

/// The acknowledgement as the operator gives it: through the use case, so the
/// requester-only rule is exercised on every call.
async fn acknowledge(h: &Harness, service: &ServiceId, change: &FeePolicyChange, by: UserId) -> Result<FeePolicyChange, DomainError> {
	fee_app::acknowledge_undelivered_notices(&h.changes, service, change.id, by, now()).await
}

/// The waiver columns as the row carries them — what the history screen reads.
async fn waiver_row(h: &Harness, change: &FeePolicyChange) -> (Option<String>, Option<i64>, Option<Vec<Uuid>>) {
	sqlx::query_as("SELECT notices_waived_by, EXTRACT(EPOCH FROM notices_waived_at)::bigint, notices_waived_users FROM fee_policy_changes WHERE id = $1")
		.bind(change.id.raw())
		.fetch_one(&h.pool)
		.await
		.unwrap()
}

/// The constraint a refused write names, or a panic naming what was accepted instead.
fn refused_by(result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>, what: &str) -> String {
	match result {
		Err(sqlx::Error::Database(err)) => {
			assert_eq!(err.code().as_deref(), Some("23514"), "{what}: expected a CHECK violation, got {err}");
			err.constraint().unwrap_or_default().to_owned()
		}
		Err(other) => panic!("{what}: expected a CHECK violation, got {other}"),
		Ok(_) => panic!("{what}: the schema accepted it"),
	}
}

/// How many backends of this database are queued on the product lock right now — the one
/// observable fact that says a transaction over the terms is WAITING rather than running.
async fn backends_waiting_on_the_product_lock(pool: &PgPool) -> i64 {
	sqlx::query_scalar(
		"SELECT COUNT(*) FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' \
		 AND query LIKE '%allocations%FOR UPDATE%'",
	)
	.fetch_one(pool)
	.await
	.unwrap()
}

/// Poll until `n` backends are queued on the product lock, or give up loudly.
async fn wait_for_lock_waiters(pool: &PgPool, n: i64) {
	for _ in 0..200 {
		if backends_waiting_on_the_product_lock(pool).await >= n {
			return;
		}
		tokio::time::sleep(std::time::Duration::from_millis(50)).await;
	}
	panic!("expected {n} transaction(s) to queue on the product lock; none did within 10s");
}

/// The statement every transaction over a product's terms opens with, exactly as
/// `lock_product` sends it and `pg_stat_activity` echoes it back.
const PRODUCT_LOCK_STATEMENT: &str = "SELECT service FROM allocations WHERE service = $1 FOR UPDATE";

/// A promotion in flight, as the lock sees it: a transaction holding the product's row
/// with nothing committed yet. The caller decides what it changes, then commits it.
async fn promotion_in_flight(h: &Harness, service: &ServiceId) -> sqlx::Transaction<'static, sqlx::Postgres> {
	let mut tx = h.pool.begin().await.unwrap();
	sqlx::query_scalar::<_, String>(PRODUCT_LOCK_STATEMENT).bind(service.as_str()).fetch_one(&mut *tx).await.unwrap();
	tx
}

/// What the application would hand to `schedule` after judging `next` against the terms
/// it read BEFORE any lock — the same decision `schedule_policy` takes, taken here in the
/// open so the test can hold it stale.
async fn judged_request(h: &Harness, service: &ServiceId, next: FeePolicy) -> NewFeePolicyChange {
	let current = h.policies.find(service).await.unwrap();
	NewFeePolicyChange {
		id: FeePolicyChangeId::new(),
		service: service.clone(),
		policy: next,
		requirement: invest_domain::fees::requirement_for(current.as_ref(), &next),
		requested_effective_from_unix: 0,
		requested_by: UserId::new().to_string(),
		reason: String::new(),
		now_unix: now(),
	}
}

/// `schedule` on its own task, so the test can watch it from outside: it must not be able
/// to finish while another transaction holds the product.
fn spawn_schedule(h: &Harness, change: NewFeePolicyChange) -> tokio::task::JoinHandle<Result<FeePolicyChange, DomainError>> {
	let changes = PgFeePolicyChanges::new(h.pool.clone());
	tokio::spawn(async move { changes.schedule(&change).await })
}

/// Block until a backend of THIS database is queued on the product lock, or fail. The
/// proof that a racing `schedule` waits rather than proceeds is the waiter itself in
/// `pg_stat_activity` — not a guess at how long the race takes. Scoped to the current
/// database: a sibling suite on the same server takes the same lock in its own.
async fn wait_for_the_lock_waiter(pool: &PgPool) {
	for _ in 0..500 {
		let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' AND query = $1)")
			.bind(PRODUCT_LOCK_STATEMENT)
			.fetch_one(pool)
			.await
			.unwrap();
		if waiting {
			return;
		}
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
	}
	panic!("no transaction queued on the product lock within 10s");
}

/// The blocked `schedule` once the promotion has committed — bounded, so a lock that is
/// never released fails the test instead of hanging the suite.
async fn released(handle: tokio::task::JoinHandle<Result<FeePolicyChange, DomainError>>) -> Result<FeePolicyChange, DomainError> {
	tokio::time::timeout(std::time::Duration::from_secs(10), handle)
		.await
		.expect("schedule is released once the promotion commits")
		.expect("schedule did not panic")
}

#[tokio::test]
async fn a_change_on_a_fund_with_no_holders_binds_at_once_and_notifies_nobody() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;

	// From nothing to the house terms: a tightening, but inside the envelope — an
	// administrator's call, and with nobody holding units there is nobody to give notice to.
	let before = now();
	let change = schedule(&h, UserId::new(), &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	assert_eq!(change.requirement, ChangeRequirement::Admin);
	assert_eq!(change.state, FeePolicyChangeState::Scheduled);
	assert!(
		change.effective_from_unix >= before && change.effective_from_unix <= now() + CLOCK_SLACK,
		"binds at once: {}",
		change.effective_from_unix
	);
	assert!(notices(&h, &change).await.is_empty(), "no holders, no notices");
	// Not live until promoted: the policy port still says the product charges nothing.
	assert_eq!(h.policies.find(&service).await.unwrap(), None);
	assert_eq!(h.changes.pending(&service).await.unwrap().map(|pending| pending.id), Some(change.id));

	assert!(
		fee_app::promote_due(&h.changes, now(), &mut std::collections::HashMap::new()).await.unwrap() >= 1,
		"the sweeper's promotion picks it up"
	);
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
	let current = h.policies.current(&service).await.unwrap().expect("a live row");
	assert_eq!(current.version, 1);
	assert_eq!(current.effective_from_unix, change.effective_from_unix);
	let applied = change_of(&h, &change).await;
	assert_eq!(applied.state, FeePolicyChangeState::Active);
	assert!(applied.applied_at_unix.is_some());
	assert!(h.changes.pending(&service).await.unwrap().is_none());
	// A second promotion of the same change is a no-op, not a second version.
	assert!(!h.changes.promote(change.id, now()).await.unwrap());
}

#[tokio::test]
async fn a_change_over_a_held_fund_waits_out_the_notice_and_records_a_notice_per_holder() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	let first = holder(&h, &service, "1000").await;
	let second = holder(&h, &service, "500").await;

	// A loosening is an administrator's call, but the holders still get their 24 hours.
	let cheaper = cheaper();
	let before = now();
	let change = schedule(&h, UserId::new(), &service, cheaper, 0, "").await.unwrap();
	assert_eq!(change.requirement, ChangeRequirement::Admin);
	assert_eq!(change.state, FeePolicyChangeState::Scheduled);
	assert!(
		change.effective_from_unix >= before + MIN_NOTICE_SECS,
		"never earlier than the notice period: {}",
		change.effective_from_unix
	);
	assert!(change.effective_from_unix <= now() + MIN_NOTICE_SECS + CLOCK_SLACK);

	// One notice per holder, in the same transaction — given up on as it is recorded: invest
	// has no channel to deliver one yet, and says so on the row rather than pretending.
	let recorded = notices(&h, &change).await;
	let mut expected = vec![first.raw(), second.raw()];
	expected.sort();
	assert_eq!(recorded.iter().map(|notice| notice.user).collect::<Vec<_>>(), expected);
	assert!(recorded.iter().all(|notice| notice.given_up && !notice.delivered && !notice.withdrawn), "{recorded:?}");
	assert_eq!((change.undelivered_notices, change.notices_given_up), (2, 2));

	// Not due yet: the old terms stay live and the sweeper leaves the change alone.
	assert!(!h.changes.due(now()).await.unwrap().contains(&change.id));
	assert!(!h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
	// Once the moment has come, it is promoted and becomes version 2 — cheaper terms bind
	// over holders who could not be told, on the record.
	assert!(h.changes.due(change.effective_from_unix + 1).await.unwrap().contains(&change.id));
	assert!(h.changes.promote(change.id, change.effective_from_unix + 1).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(cheaper));
	assert_eq!(h.policies.current(&service).await.unwrap().unwrap().version, 2);
	let history = h.changes.list(&service).await.unwrap();
	assert_eq!(
		history.iter().map(|row| (row.version, row.state)).collect::<Vec<_>>(),
		vec![(2, FeePolicyChangeState::Active), (1, FeePolicyChangeState::Superseded)]
	);

	// A later moment the operator asks for is honoured as it is.
	let later = now() + 3 * MIN_NOTICE_SECS;
	let deferred = schedule(&h, UserId::new(), &service, FeePolicy::HOUSE, later, "").await.unwrap();
	assert_eq!(deferred.effective_from_unix, later);
}

#[tokio::test]
async fn a_tightening_does_not_bind_over_a_holder_who_was_never_told() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, cheaper()).await;
	let investor = holder(&h, &service, "1000").await;
	let change = schedule(&h, UserId::new(), &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	assert_eq!(change.requirement, ChangeRequirement::Admin);
	assert_eq!(notices(&h, &change).await.len(), 1);
	let_the_notice_run(&h, &change).await;

	// Nobody could tell the holder: dearer terms do not bind over them.
	let refused = h.changes.promote(change.id, now()).await.unwrap_err();
	assert!(matches!(refused, DomainError::Conflict(_)), "{refused:?}");
	assert!(
		refused.to_string().contains("1 holder notice(s) for this change are undelivered, 1 of them given up on"),
		"{refused}"
	);
	assert_eq!(change_of(&h, &change).await.state, FeePolicyChangeState::Scheduled, "the change waits; nothing was written");
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(cheaper()), "the old terms stay live");
	assert!(h.changes.due(now()).await.unwrap().contains(&change.id), "still due: the sweeper keeps retrying, and escalating");
	let mut failures = std::collections::HashMap::new();
	fee_app::promote_due(&h.changes, now(), &mut failures).await.unwrap();
	assert_eq!(failures.get(&change.id), Some(&1), "the sweeper counts the refusal towards its error streak");

	// Reached after all: the terms bind on the next tick, with nobody's help.
	deliver_notice(&h, &change, investor).await;
	assert!(h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
	assert_eq!(change_of(&h, &change).await.state, FeePolicyChangeState::Active);
}

#[tokio::test]
async fn cancelling_a_change_withdraws_the_notices_not_delivered() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	let told = holder(&h, &service, "1000").await;
	let untold = holder(&h, &service, "500").await;
	let change = schedule(&h, UserId::new(), &service, cheaper(), 0, "").await.unwrap();
	deliver_notice(&h, &change, told).await;
	assert_eq!(change_of(&h, &change).await.undelivered_notices, 1);

	let cancelled = fee_app::cancel_change(&h.changes, &service, change.id, UserId::new(), now()).await.unwrap();
	assert_eq!(cancelled.state, FeePolicyChangeState::Cancelled);
	assert_eq!(cancelled.undelivered_notices, 0, "a closed change is owed to nobody");
	let rows = notices(&h, &change).await;
	let row = |user: UserId| rows.iter().find(|notice| notice.user == user.raw()).expect("a notice per holder");
	assert!(row(told).delivered && !row(told).withdrawn, "what was said stands");
	assert!(row(untold).withdrawn && !row(untold).delivered, "what was not said never will be");
}

#[tokio::test]
async fn cheaper_terms_bind_over_an_unreachable_holder_and_a_holder_who_left_holds_nothing() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	let unreachable = holder(&h, &service, "1000").await;

	// A loosening over a holder nobody could reach: nobody is worse off, and a product
	// whose one holder cannot be reached must still be able to lower its terms.
	let loosening = schedule(&h, UserId::new(), &service, cheaper(), 0, "").await.unwrap();
	let_the_notice_run(&h, &loosening).await;
	assert!(h.changes.promote(loosening.id, now()).await.unwrap(), "cheaper terms bind, on the record");
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(cheaper()));

	// A tightening back to the house terms over the same holder: refused.
	let tightening = schedule(&h, UserId::new(), &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	let_the_notice_run(&h, &tightening).await;
	let refused = h.changes.promote(tightening.id, now()).await.unwrap_err();
	assert!(matches!(refused, DomainError::Conflict(_)), "{refused:?}");
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(cheaper()));

	// The holder redeems every unit: a notice that never reached them is no longer about
	// anyone's terms, and the change binds for whoever comes next.
	sqlx::query("UPDATE fund_positions SET units = '0' WHERE user_id = $1 AND service = $2")
		.bind(unreachable.raw())
		.bind(service.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	assert!(h.changes.promote(tightening.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
}

/// A position concierge has not told invest about (no `users` row) is still somebody whose
/// terms are about to change: the notice period is decided on every position with units,
/// and the notice is owed to them like anyone.
#[tokio::test]
async fn a_holder_invest_does_not_know_still_gets_the_notice_period() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	let stranger = Uuid::new_v4();
	sqlx::query("INSERT INTO fund_positions (user_id, service, cost_basis, units, high_water_mark) VALUES ($1, $2, '1000000000', '1000000000000000000000', '1000000000')")
		.bind(stranger)
		.bind(service.as_str())
		.execute(&h.pool)
		.await
		.unwrap();

	let before = now();
	let change = schedule(&h, UserId::new(), &service, cheaper(), 0, "").await.unwrap();
	assert_eq!(change.state, FeePolicyChangeState::Scheduled);
	assert!(change.effective_from_unix >= before + MIN_NOTICE_SECS, "the floor is decided on every position with units");
	assert_eq!(notices(&h, &change).await.iter().map(|notice| notice.user).collect::<Vec<_>>(), vec![stranger]);
	assert!(!h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
}

/// A tightening beyond the house envelope is the owners' to carry, and invest cannot ask
/// them yet (concierge's approvals): it is refused — before anything is written, whether
/// it comes through the use case or straight at the store — rather than scheduled on one
/// administrator's word.
#[tokio::test]
async fn a_tightening_beyond_the_envelope_is_refused_until_invest_can_ask_the_owners() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	holder(&h, &service, "1000").await;

	// A change the owners would be asked to approve says why; one with no reason is
	// refused as such first.
	let err = schedule(&h, UserId::new(), &service, dearer(), 0, "  ").await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "got {err:?}");
	let err = schedule(&h, UserId::new(), &service, dearer(), 0, "the new mandate costs more to run").await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref m) if m.contains("owners")), "got {err:?}");
	let direct = NewFeePolicyChange {
		id: FeePolicyChangeId::new(),
		service: service.clone(),
		policy: dearer(),
		requirement: ChangeRequirement::OwnerConsilium,
		requested_effective_from_unix: 0,
		requested_by: UserId::new().to_string(),
		reason: "the new mandate costs more to run".into(),
		now_unix: now(),
	};
	let err = h.changes.schedule(&direct).await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref m) if m.contains("owners")), "got {err:?}");
	assert!(h.changes.pending(&service).await.unwrap().is_none(), "nothing was recorded");
	assert_eq!(h.changes.list(&service).await.unwrap().len(), 1, "only the installed terms are on record");
}

#[tokio::test]
async fn no_change_is_scheduled_beyond_the_horizon() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	holder(&h, &service, "100").await;
	let err = schedule(&h, UserId::new(), &service, cheaper(), now() + MAX_EFFECTIVE_FROM_HORIZON_SECS + 60, "")
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "got {err:?}");
	assert!(h.changes.pending(&service).await.unwrap().is_none());
	assert!(schedule(&h, UserId::new(), &service, cheaper(), now() + MAX_EFFECTIVE_FROM_HORIZON_SECS - 60, "").await.is_ok());
}

#[tokio::test]
async fn an_acknowledged_notice_lets_a_tightening_bind_and_the_history_says_who_and_for_whom() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, cheaper()).await;
	let unreachable = holder(&h, &service, "1000").await;
	let requester = UserId::new();
	let change = schedule(&h, requester, &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	assert_eq!((change.undelivered_notices, change.notices_given_up), (1, 1), "one holder, given up on as recorded");
	assert_eq!(change.notices_waiver, None);
	let_the_notice_run(&h, &change).await;

	let refused = h.changes.promote(change.id, now()).await.unwrap_err();
	assert!(matches!(refused, DomainError::Conflict(_)), "{refused:?}");
	assert!(refused.to_string().contains("1 of them given up on"), "{refused}");

	let before = now();
	let acknowledged = acknowledge(&h, &service, &change, requester).await.unwrap();
	let waiver = acknowledged.notices_waiver.clone().expect("the acknowledgement is on the change");
	assert_eq!(waiver.by, requester.to_string());
	assert!(waiver.at_unix >= before && waiver.at_unix <= now() + CLOCK_SLACK, "{}", waiver.at_unix);
	assert_eq!(waiver.users, vec![unreachable], "exactly the holder whose notice was undelivered");
	assert_eq!(acknowledged.state, FeePolicyChangeState::Scheduled, "an acknowledgement is not a promotion");
	assert_eq!(
		(acknowledged.undelivered_notices, acknowledged.notices_given_up),
		(1, 1),
		"the figures stay honest: the holder is still untold"
	);
	assert_eq!(waiver_row(&h, &change).await, (Some(requester.to_string()), Some(waiver.at_unix), Some(vec![unreachable.raw()])));

	assert!(h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
	let active = change_of(&h, &change).await;
	assert_eq!(active.state, FeePolicyChangeState::Active);
	assert_eq!(active.notices_waiver, Some(waiver));
	assert_eq!((active.undelivered_notices, active.notices_given_up), (0, 0), "an active change waits on nobody");
}

#[tokio::test]
async fn an_acknowledgement_covers_the_holders_of_its_moment_and_no_one_else() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, cheaper()).await;
	let untold = holder(&h, &service, "1000").await;
	let away = holder(&h, &service, "500").await;
	let requester = UserId::new();
	let change = schedule(&h, requester, &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	assert_eq!(change.undelivered_notices, 2);
	let_the_notice_run(&h, &change).await;

	// `away` holds nothing at the moment of the acknowledgement, so it names `untold` alone.
	sqlx::query("UPDATE fund_positions SET units = '0' WHERE user_id = $1 AND service = $2")
		.bind(away.raw())
		.bind(service.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	let acknowledged = acknowledge(&h, &service, &change, requester).await.unwrap();
	assert_eq!(acknowledged.notices_waiver.as_ref().map(|waiver| waiver.users.clone()), Some(vec![untold]));

	// They buy back in before the promotion: untold, and not on the record — the change
	// waits again.
	sqlx::query("UPDATE fund_positions SET units = '500' WHERE user_id = $1 AND service = $2")
		.bind(away.raw())
		.bind(service.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	let refused = h.changes.promote(change.id, now()).await.unwrap_err();
	assert!(matches!(refused, DomainError::Conflict(_)), "{refused:?}");
	assert!(refused.to_string().contains("1 of them to holders the acknowledgement by"), "{refused}");
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(cheaper()));

	// Their notice reaches them after all: the change binds.
	deliver_notice(&h, &change, away).await;
	assert!(h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
}

#[tokio::test]
async fn there_is_nothing_to_acknowledge_on_a_loosening_a_told_holder_or_a_closed_change() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	let investor = holder(&h, &service, "1000").await;

	// A loosening binds over an untold holder by itself: no protection to waive.
	let requester = UserId::new();
	let loosening = schedule(&h, requester, &service, cheaper(), 0, "").await.unwrap();
	let err = acknowledge(&h, &service, &loosening, requester).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "{err:?}");
	assert!(err.to_string().contains("only get cheaper"), "{err}");
	assert_eq!(waiver_row(&h, &loosening).await, (None, None, None));
	let_the_notice_run(&h, &loosening).await;
	assert!(h.changes.promote(loosening.id, now()).await.unwrap());

	// A tightening whose notice is still being tried: nobody has failed to be reached.
	let change = schedule(&h, requester, &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	still_trying(&h, &change, investor).await;
	let err = acknowledge(&h, &service, &change, requester).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "{err:?}");
	assert!(err.to_string().contains("none has been given up on yet"), "{err}");

	// Every notice delivered: the figure the operator acted on is no longer true.
	deliver_notice(&h, &change, investor).await;
	let err = acknowledge(&h, &service, &change, requester).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "{err:?}");
	assert!(err.to_string().contains("nothing to acknowledge"), "{err}");
	assert_eq!(waiver_row(&h, &change).await, (None, None, None));

	// Promoted: nothing waits on anybody.
	let_the_notice_run(&h, &change).await;
	assert!(h.changes.promote(change.id, now()).await.unwrap());
	let err = acknowledge(&h, &service, &change, requester).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "{err:?}");
	assert!(err.to_string().contains("active"), "{err}");

	// A change of another product is not found under this one.
	let other = unique_service();
	open_fund(&h, &other).await;
	let elsewhere = schedule(&h, requester, &other, cheaper(), 0, "").await.unwrap();
	let err = acknowledge(&h, &service, &elsewhere, requester).await.unwrap_err();
	assert!(matches!(err, DomainError::NotFound { .. }), "{err:?}");
}

#[tokio::test]
async fn a_later_acknowledgement_adds_the_holders_given_up_on_since_and_stands_as_the_latest() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, cheaper()).await;
	let first_lost = holder(&h, &service, "1000").await;
	let second_lost = holder(&h, &service, "500").await;
	let requester = UserId::new();
	let change = schedule(&h, requester, &service, FeePolicy::HOUSE, 0, "").await.unwrap();
	// The second holder's notice is still being tried when the first acknowledgement is given.
	still_trying(&h, &change, second_lost).await;
	assert_eq!(change_of(&h, &change).await.notices_unacknowledged, 1, "one holder to offer the acknowledgement over");
	let first = acknowledge(&h, &service, &change, requester).await.unwrap();
	let waiver = first.notices_waiver.clone().expect("acknowledged");
	assert_eq!(waiver.users, vec![first_lost]);
	assert_eq!((first.undelivered_notices, first.notices_given_up, first.notices_unacknowledged), (2, 1, 0));

	// Given up on AFTER the acknowledgement: the record does not stretch to cover them.
	give_up(&h, &change, second_lost).await;
	let_the_notice_run(&h, &change).await;
	let refused = h.changes.promote(change.id, now()).await.unwrap_err();
	assert!(refused.to_string().contains("1 of them to holders the acknowledgement by"), "{refused}");
	let waiting = change_of(&h, &change).await;
	assert_eq!((waiting.undelivered_notices, waiting.notices_given_up, waiting.notices_unacknowledged), (2, 2, 1));

	// A later acknowledgement adds them, and stands as the latest record.
	tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
	let second = acknowledge(&h, &service, &change, requester).await.unwrap();
	let extended = second.notices_waiver.clone().expect("still acknowledged");
	assert_eq!(extended.users, vec![first_lost, second_lost], "the earlier record first, the addition after");
	assert!(extended.at_unix > waiver.at_unix, "{} <= {}", extended.at_unix, waiver.at_unix);
	assert_eq!(second.notices_unacknowledged, 0);

	// A repeat with nobody new to add leaves the latest record as it is.
	tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
	assert_eq!(acknowledge(&h, &service, &change, requester).await.unwrap().notices_waiver, Some(extended.clone()));

	assert!(h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
	assert_eq!(change_of(&h, &change).await.notices_waiver, Some(extended));
}

#[tokio::test]
async fn an_acknowledgement_is_the_requesters_and_the_schema_holds_it_whole() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, cheaper()).await;
	let unreachable = holder(&h, &service, "1000").await;
	let requester = UserId::new();
	let change = schedule(&h, requester, &service, FeePolicy::HOUSE, 0, "").await.unwrap();

	// Another administrator — `AllocationManage` alone — may not waive the holders' notice
	// on a change somebody else asked for.
	let err = acknowledge(&h, &service, &change, UserId::new()).await.unwrap_err();
	assert!(matches!(err, DomainError::Forbidden(_)), "{err:?}");
	assert_eq!(waiver_row(&h, &change).await, (None, None, None));

	let waiver = acknowledge(&h, &service, &change, requester).await.unwrap().notices_waiver.expect("acknowledged");
	assert_eq!(waiver.users, vec![unreachable]);

	let partial = sqlx::query("UPDATE fee_policy_changes SET notices_waived_users = NULL WHERE id = $1")
		.bind(change.id.raw())
		.execute(&h.pool)
		.await;
	assert!(refused_by(partial, "a partial waiver").contains("fee_policy_change_notice_waiver_is_whole"));
	let empty = sqlx::query("UPDATE fee_policy_changes SET notices_waived_users = '{}' WHERE id = $1")
		.bind(change.id.raw())
		.execute(&h.pool)
		.await;
	assert!(refused_by(empty, "a waiver covering nobody").contains("fee_policy_change_notice_waiver_names_someone"));
}

#[tokio::test]
async fn the_terms_of_a_hidden_product_are_kept_from_an_investor_who_cannot_see_it() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	h.allocations.set_access(&service, AllocationAccess::Hidden).await.unwrap();
	let outsider = person(&h).await;
	let listed = |views: Vec<(ServiceId, fee_app::PolicyView)>| views.into_iter().any(|(listed, _)| listed == service);

	// A direct read answers as `GetAllocation` does for a product hidden from the caller:
	// as if it were not registered. The catalog of terms simply omits it.
	let Err(view) = fee_app::policy_view(&h.policies, &h.changes, &reader(&h, outsider), &service).await else {
		panic!("the terms of a hidden product were shown to an outsider")
	};
	assert!(matches!(view, DomainError::NotFound { .. }), "{view:?}");
	let history = fee_app::list_changes(&h.changes, &reader(&h, outsider), &service).await.unwrap_err();
	assert!(matches!(history, DomainError::NotFound { .. }), "{history:?}");
	assert!(!listed(fee_app::list_policies(&h.policies, &h.changes, &reader(&h, outsider)).await.unwrap()));

	// A manager reads every product's terms, hidden or not.
	let view = fee_app::policy_view(&h.policies, &h.changes, &manager(&h), &service).await.unwrap();
	assert_eq!(view.current.map(|current| current.policy), Some(FeePolicy::HOUSE));
	assert_eq!(fee_app::list_changes(&h.changes, &manager(&h), &service).await.unwrap().len(), 1);
	assert!(listed(fee_app::list_policies(&h.policies, &h.changes, &manager(&h)).await.unwrap()));

	// Raised to `view` by name, the same investor reads the same terms as anyone listed.
	h.allocations.grant_access(&service, outsider, AllocationAccess::View, outsider).await.unwrap();
	let view = fee_app::policy_view(&h.policies, &h.changes, &reader(&h, outsider), &service).await.unwrap();
	assert_eq!(view.current.map(|current| current.policy), Some(FeePolicy::HOUSE));
	assert_eq!(fee_app::list_changes(&h.changes, &reader(&h, outsider), &service).await.unwrap().len(), 1);
	assert!(listed(fee_app::list_policies(&h.policies, &h.changes, &reader(&h, outsider)).await.unwrap()));
}

#[tokio::test]
async fn one_change_is_on_its_way_per_product_until_it_is_cancelled() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	holder(&h, &service, "100").await;

	let first = schedule(
		&h,
		UserId::new(),
		&service,
		policy(150, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual),
		0,
		"",
	)
	.await
	.unwrap();
	let err = schedule(
		&h,
		UserId::new(),
		&service,
		policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual),
		0,
		"",
	)
	.await
	.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
	assert!(err.to_string().contains("cancel it"), "the refusal must say what to do: {err}");

	let admin = UserId::new();
	let cancelled = fee_app::cancel_change(&h.changes, &service, first.id, admin, now()).await.unwrap();
	assert_eq!(cancelled.state, FeePolicyChangeState::Cancelled);
	// Idempotent, and never promoted.
	assert_eq!(
		fee_app::cancel_change(&h.changes, &service, first.id, admin, now()).await.unwrap().state,
		FeePolicyChangeState::Cancelled
	);
	let_the_notice_run(&h, &first).await;
	assert!(!h.changes.promote(first.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE));
	// The slot is free again, and a change filed under another product's slug is not this
	// product's to cancel.
	let second = schedule(
		&h,
		UserId::new(),
		&service,
		policy(100, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual),
		0,
		"",
	)
	.await
	.unwrap();
	assert!(matches!(
		fee_app::cancel_change(&h.changes, &unique_service(), second.id, admin, now()).await.unwrap_err(),
		DomainError::NotFound { .. }
	));
	// A live version cannot be cancelled: it is not on its way anywhere.
	let active = h.changes.list(&service).await.unwrap().into_iter().find(|row| row.state == FeePolicyChangeState::Active).unwrap();
	assert!(matches!(
		fee_app::cancel_change(&h.changes, &service, active.id, admin, now()).await.unwrap_err(),
		DomainError::Conflict(_)
	));
}

#[tokio::test]
async fn promotion_settles_the_elapsed_window_at_the_old_rate_before_the_new_one_binds() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	let investor = holder(&h, &service, "1000").await;
	// A year on the house 2%, never swept.
	backdate(&h, investor, &service, YEAR).await;
	assert_eq!(fee_debt_of(&h, investor, &service).await, Usdt::ZERO);

	// Halve the management rate. The change binds after the notice; stand in for the notice
	// having run.
	let halved = cheaper();
	let change = schedule(&h, UserId::new(), &service, halved, 0, "").await.unwrap();
	let_the_notice_run(&h, &change).await;
	assert!(h.changes.promote(change.id, now()).await.unwrap());
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(halved));

	// The year that had already passed was priced at the OLD 2% — 20 USDT on the 1000
	// invested — and carried into the position's debt, with the clock restarted at the
	// moment the new terms bound. Nobody re-prices time that has passed.
	assert_close(fee_debt_of(&h, investor, &service).await, usdt("20"), "a year at the old rate, carried");
	let accrual = h.accruals.find(investor, &service).await.unwrap().unwrap();
	let effective_from = change_of(&h, &change).await.effective_from_unix;
	assert_eq!(accrual.accrued_at_unix, effective_from, "the elapsed clock restarts where the new terms begin");

	// The next assessment collects the carried 20 plus an hour at the NEW 1% — a hair over
	// 20, nowhere near the 30 a year at 1% plus a re-priced year would be, and nowhere near
	// the 40 of two years at 2%.
	let charge = common::assess_period(&h, investor, &service, now()).await.unwrap().expect("the carried debt is collectable");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_close(charge.charge().due, usdt("20"), "carried debt plus an hour at the new rate");
	assert_close(charge.charge().debt_opening, usdt("20"), "the carried year is the opening debt");
	assert_eq!(fee_debt_of(&h, investor, &service).await, Usdt::ZERO, "a 1000-unit holding covers the charge whole");
	let taken = Shares::from_base_units(h.ledger.balance(&LedgerAccountKey::FeeShares(service.clone())).await.unwrap().posted);
	assert_eq!(taken, charge.charge().charged_units);
}

#[tokio::test]
async fn a_requirement_decided_against_stale_terms_is_refused_under_the_lock() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;

	// What a request judged before a promotion landed would carry: an administrator's
	// requirement for a change the live terms make the owners'.
	let stale = NewFeePolicyChange {
		id: FeePolicyChangeId::new(),
		service: service.clone(),
		policy: dearer(),
		requirement: ChangeRequirement::Admin,
		requested_effective_from_unix: 0,
		requested_by: UserId::new().to_string(),
		reason: String::new(),
		now_unix: now(),
	};
	let err = h.changes.schedule(&stale).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
	assert!(err.to_string().contains("re-submit"), "{err}");
	assert!(h.changes.pending(&service).await.unwrap().is_none(), "nothing was recorded");
	// The same terms with the requirement the live terms call for are not stale.
	assert_eq!(
		fee_app::policy_view(&h.policies, &h.changes, &manager(&h), &service).await.unwrap().current.map(|c| c.policy),
		Some(FeePolicy::HOUSE)
	);
}

/// banking#250: the schedule-vs-promote race with two REAL transactions in a forced order.
///
/// An operator's request is judged against the terms it read; a promotion may land between
/// that read and the request's own transaction. The product lock is what turns that into a
/// refusal rather than a change recorded against terms that no longer hold. Here a third
/// transaction holds the product lock, the promotion queues behind it FIRST and the
/// scheduling SECOND, and the lock is released — so the scheduling is guaranteed to run
/// after the promotion has committed, on the terms the promotion wrote.
#[tokio::test]
async fn a_scheduling_queued_behind_a_promotion_is_judged_on_the_promoted_terms() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;

	// The change about to be promoted adds a hurdle — a loosening, an administrator's call,
	// and with no holders it may bind at once.
	let hurdled = policy(200, 2_000, 800, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	let promoting = schedule(&h, UserId::new(), &service, hurdled, 0, "").await.unwrap();
	assert_eq!(promoting.state, FeePolicyChangeState::Scheduled);

	// The request judged BEFORE that promotion: back to the house terms, which against the
	// live (hurdle-less) terms changes nothing and is an administrator's call. Against the
	// promoted terms it LOWERS a hurdle — the owners' call.
	let judged_stale = NewFeePolicyChange {
		id: FeePolicyChangeId::new(),
		service: service.clone(),
		policy: FeePolicy::HOUSE,
		requirement: ChangeRequirement::Admin,
		requested_effective_from_unix: 0,
		requested_by: UserId::new().to_string(),
		reason: String::new(),
		now_unix: now(),
	};
	assert_eq!(invest_domain::fees::requirement_for(Some(&FeePolicy::HOUSE), &judged_stale.policy), ChangeRequirement::Admin);
	assert_eq!(invest_domain::fees::requirement_for(Some(&hurdled), &judged_stale.policy), ChangeRequirement::OwnerConsilium);

	// A third transaction holds the product lock, so the two under test can be queued in a
	// known order behind it.
	let mut gate = h.pool.begin().await.unwrap();
	sqlx::query("SELECT service FROM allocations WHERE service = $1 FOR UPDATE")
		.bind(service.as_str())
		.fetch_one(&mut *gate)
		.await
		.unwrap();

	let promotion = {
		let changes = PgFeePolicyChanges::new(h.pool.clone());
		let id = promoting.id;
		tokio::spawn(async move { changes.promote(id, now()).await })
	};
	wait_for_lock_waiters(&h.pool, 1).await;
	let stale_id = judged_stale.id;
	let scheduling = {
		let changes = PgFeePolicyChanges::new(h.pool.clone());
		tokio::spawn(async move { changes.schedule(&judged_stale).await })
	};
	wait_for_lock_waiters(&h.pool, 2).await;
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE), "nothing has moved while the gate is held");

	gate.commit().await.unwrap();

	assert!(promotion.await.unwrap().unwrap(), "the promotion, first in the queue, lands");
	let err = scheduling.await.unwrap().unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
	assert!(err.to_string().contains("re-submit"), "the refusal names the stale judgement, not the pending slot: {err}");

	assert_eq!(h.policies.find(&service).await.unwrap(), Some(hurdled), "the promoted terms are the live terms");
	assert!(h.changes.find(stale_id).await.unwrap().is_none(), "the stale request left no row behind");
	assert!(h.changes.pending(&service).await.unwrap().is_none());
	assert_eq!(
		h.changes.list(&service).await.unwrap().iter().map(|row| (row.version, row.state)).collect::<Vec<_>>(),
		vec![(2, FeePolicyChangeState::Active), (1, FeePolicyChangeState::Superseded)]
	);
}

#[tokio::test]
async fn the_schema_holds_every_new_row_to_the_ceilings_and_only_closed_history_above_them() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;

	// The live row: neither ceiling can be crossed by any write path, SQL included.
	let live = |management: i32, performance: i32| {
		sqlx::query(
			"INSERT INTO fee_policies (service, management_bps, performance_bps, hurdle_bps, basis, crystallization, updated_by) VALUES ($1, $2, $3, 0, 'invested_capital', 'annual', 'itest')",
		)
		.bind(service.as_str())
		.bind(management)
		.bind(performance)
	};
	assert_eq!(
		refused_by(live(501, 2_000).execute(&h.pool).await, "management 501 on the live row"),
		"fee_policies_management_ceiling"
	);
	assert_eq!(
		refused_by(live(200, 5_001).execute(&h.pool).await, "performance 5001 on the live row"),
		"fee_policies_performance_ceiling"
	);
	live(500, 5_000).execute(&h.pool).await.expect("the ceilings themselves are legal terms");

	// The history: a row that can still bind is held to the ceilings; a closed one is not.
	let history = |state: &'static str, management: i32, performance: i32, hurdle: i32| {
		sqlx::query(
			"INSERT INTO fee_policy_changes (id, service, version, management_bps, performance_bps, hurdle_bps, basis, crystallization, state, requirement, \
			   effective_from, requested_by, scheduled_at, closed_reason) \
			 SELECT gen_random_uuid(), $1, COALESCE(MAX(version), 0) + 1, $3, $4, $5, 'invested_capital', 'annual', $2, 'admin', now(), 'itest', \
			   CASE WHEN $2 = 'scheduled' THEN now() END, CASE WHEN $2 = 'cancelled' THEN 'itest' END \
			 FROM fee_policy_changes WHERE service = $1",
		)
		.bind(service.as_str())
		.bind(state)
		.bind(management)
		.bind(performance)
		.bind(hurdle)
	};
	assert_eq!(
		refused_by(history("scheduled", 501, 2_000, 0).execute(&h.pool).await, "management 501 scheduled"),
		"fee_policy_changes_management_ceiling"
	);
	assert_eq!(
		refused_by(history("scheduled", 200, 5_001, 0).execute(&h.pool).await, "performance 5001 scheduled"),
		"fee_policy_changes_performance_ceiling"
	);
	assert!(
		refused_by(history("scheduled", 200, 2_000, 10_001).execute(&h.pool).await, "hurdle 10001").contains("hurdle_bps"),
		"a hurdle above 100% is refused by the column check"
	);
	history("cancelled", 10_000, 10_000, 10_000)
		.execute(&h.pool)
		.await
		.expect("closed history is not held to today's ceilings");
	history("scheduled", 500, 5_000, 10_000).execute(&h.pool).await.expect("the ceilings themselves are legal terms");
}

#[tokio::test]
async fn the_horizon_is_inclusive_at_366_days_and_refuses_the_next_second() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	// The moment is passed in explicitly, so the boundary is exact and not a race with the
	// wall clock.
	let at = now();
	let request = |effective_from: i64| fee_app::PolicyChangeRequest {
		service: service.clone(),
		policy: FeePolicy::HOUSE,
		requested_effective_from_unix: effective_from,
		reason: String::new(),
	};

	let err = fee_app::schedule_policy(&policy_ports(&h), UserId::new(), request(at + MAX_EFFECTIVE_FROM_HORIZON_SECS + 1), at)
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "got {err:?}");
	assert!(err.to_string().contains("366 days"), "{err}");
	assert!(h.changes.pending(&service).await.unwrap().is_none());

	let change = fee_app::schedule_policy(&policy_ports(&h), UserId::new(), request(at + MAX_EFFECTIVE_FROM_HORIZON_SECS), at)
		.await
		.expect("exactly 366 days ahead is the last legal moment");
	assert_eq!(change.effective_from_unix, at + MAX_EFFECTIVE_FROM_HORIZON_SECS);
}

#[tokio::test]
async fn a_schedule_racing_a_promotion_waits_on_the_lock_and_is_refused_once_the_terms_moved() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;

	// Against the house terms, halving the management rate is a plain loosening: one
	// administrator, no quorum.
	let cheaper = cheaper();
	let request = judged_request(&h, &service, cheaper).await;
	assert_eq!(request.requirement, ChangeRequirement::Admin);

	// A promotion takes the product before the request reaches the lock ...
	let mut promotion = promotion_in_flight(&h, &service).await;
	let schedule = spawn_schedule(&h, request);
	// ... and the request queues behind it rather than reading the terms of a minute ago.
	wait_for_the_lock_waiter(&h.pool).await;
	assert!(!schedule.is_finished(), "schedule finished while another transaction held the product");

	// The promotion grants the holders a 10% hurdle — terms the queued request never saw.
	// Against THEM the same loosening also takes the hurdle away, which is the owners' call.
	sqlx::query("UPDATE fee_policies SET hurdle_bps = 1000 WHERE service = $1")
		.bind(service.as_str())
		.execute(&mut *promotion)
		.await
		.unwrap();
	promotion.commit().await.unwrap();

	let err = released(schedule).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
	assert!(err.to_string().contains("re-submit"), "{err}");
	assert!(h.changes.pending(&service).await.unwrap().is_none(), "the stale request left no row");
	assert_eq!(h.changes.list(&service).await.unwrap().len(), 1, "only the installed terms are on record");
	let with_hurdle = policy(200, 2_000, 1000, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual);
	assert_eq!(
		fee_app::policy_view(&h.policies, &h.changes, &manager(&h), &service).await.unwrap().current.map(|c| c.policy),
		Some(with_hurdle),
		"the terms the promotion committed are the live ones"
	);
	// Re-submitted against the live terms, the same request is the owners' to decide.
	assert_eq!(judged_request(&h, &service, cheaper).await.requirement, ChangeRequirement::OwnerConsilium);
}

#[tokio::test]
async fn a_schedule_racing_a_promotion_waits_on_the_lock_and_proceeds_once_the_terms_stayed() {
	let Some(h) = harness().await else { return };
	let service = unique_service();
	open_fund(&h, &service).await;
	install(&h, &service, FeePolicy::HOUSE).await;
	let cheaper = cheaper();
	let request = judged_request(&h, &service, cheaper).await;
	let id = request.id;

	let promotion = promotion_in_flight(&h, &service).await;
	let schedule = spawn_schedule(&h, request);
	wait_for_the_lock_waiter(&h.pool).await;
	assert!(!schedule.is_finished(), "schedule finished while another transaction held the product");

	// The lock is a queue, not a refusal: a transaction that changed nothing lets the
	// request through exactly as it was judged.
	promotion.commit().await.unwrap();
	let scheduled = released(schedule).await.expect("the terms the request was judged against are still the live ones");
	assert_eq!(scheduled.id, id);
	assert_eq!(scheduled.state, FeePolicyChangeState::Scheduled);
	assert_eq!(scheduled.requirement, ChangeRequirement::Admin);
	assert_eq!(scheduled.policy, cheaper);
	assert_eq!(h.changes.pending(&service).await.unwrap().map(|pending| pending.id), Some(id));
	assert_eq!(h.policies.find(&service).await.unwrap(), Some(FeePolicy::HOUSE), "nothing binds before the promotion");
}
