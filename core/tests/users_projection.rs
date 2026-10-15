//! The `users` projection's writer against real Postgres, with concierge played by the
//! in-memory streams (`infrastructure::concierge::fake`). The pull engine is concierge's
//! `evconcierge_projection` and is tested there; what is pinned here is invest's side — what
//! each event does to a row, that a replay or a stale event changes nothing, that the cursor
//! survives a restart, that an unreadable kind is parked and replayed rather than skipped,
//! and that what the projection says is what the gates act on.
//!
//! Every test resets the engine's bookkeeping and holds [`SERIAL`]: the cursors live in this
//! binary's one database, and two tests pulling different fakes through them would each see
//! the other's positions.

use std::{
	sync::LazyLock,
	time::{Duration, Instant},
};

use evconcierge_projection::{Member, ProjectionConfig, ScopeEvent, ScopeEventBody, UserEvent, UserEventKind};
use invest_core::{
	application::issuance as issuance_app,
	infrastructure::{
		concierge::{ConciergeProjection, fake::InMemoryConcierge},
		users::PgUsers,
	},
	ports::{AllocationRegistry, money::ScopeId, users::UserDirectory},
};
use invest_domain::{
	allocations::{Allocation, AllocationIcon, AllocationId},
	authz::Permission,
	balance::ServiceId,
	error::DomainError,
	issuance::UnitHolder,
	users::UserId,
};
use sqlx::PgPool;
use tokio::sync::MutexGuard;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod common;

static SERIAL: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

const MANAGE: &str = "invest:allocation:manage";

struct Harness {
	pool: PgPool,
	users: PgUsers,
	concierge: InMemoryConcierge,
	_serial: MutexGuard<'static, ()>,
}

async fn harness() -> Option<Harness> {
	let serial = SERIAL.lock().await;
	let pool = common::pool().await?;
	sqlx::query("TRUNCATE concierge_projection_cursor, concierge_projection_applied, concierge_projection_sequence, concierge_projection_parked")
		.execute(&pool)
		.await
		.expect("reset the projection's bookkeeping");
	Some(Harness {
		users: PgUsers::new(pool.clone()),
		pool,
		concierge: InMemoryConcierge::new(),
		_serial: serial,
	})
}

impl Harness {
	/// A fresh engine over the same database and streams — what a restart builds.
	fn projection(&self) -> ConciergeProjection<InMemoryConcierge> {
		ConciergeProjection::new(self.pool.clone(), self.concierge.clone(), config())
	}

	async fn cycle(&self) -> evconcierge_projection::Cycle {
		self.projection().cycle().await.expect("a projection cycle")
	}

	async fn cursor(&self, stream: &str) -> Option<i64> {
		sqlx::query_scalar("SELECT position FROM concierge_projection_cursor WHERE stream = $1")
			.bind(stream)
			.fetch_optional(&self.pool)
			.await
			.expect("read a cursor")
	}

	async fn parked(&self) -> Vec<(String, i64)> {
		sqlx::query_as("SELECT stream, position FROM concierge_projection_parked ORDER BY stream, position")
			.fetch_all(&self.pool)
			.await
			.expect("read the parked events")
	}

	async fn may(&self, person: &Person, permission: Permission) -> bool {
		self.users.standing(person.user).await.unwrap().is_some_and(|standing| standing.may(permission))
	}

	async fn active(&self, person: &Person) -> Option<bool> {
		self.users.standing(person.user).await.unwrap().map(|standing| standing.active)
	}

	/// A person visible to scope `invest`, active as of lifecycle `lifecycle_sequence`.
	fn joins(&self, person: &Person, lifecycle_sequence: u64) {
		self.concierge.push_scope(member_joined("invest", person, "active", lifecycle_sequence, 1));
	}
}

fn config() -> ProjectionConfig {
	ProjectionConfig {
		poll_interval: Duration::from_millis(50),
		..ProjectionConfig::default()
	}
}

struct Person {
	user: UserId,
	subject: String,
}

fn person() -> Person {
	Person {
		user: UserId::new(),
		subject: format!("subj-{}", Uuid::new_v4()),
	}
}

fn event_id() -> String {
	Uuid::new_v4().to_string()
}

fn member_joined(scope: &str, person: &Person, status: &str, lifecycle_sequence: u64, sequence: u64) -> ScopeEvent {
	ScopeEvent {
		event_id: event_id(),
		position: 0,
		scope: scope.to_owned(),
		occurred_at: 1_800_000_000,
		sequence,
		body: ScopeEventBody::MemberJoined {
			user_id: person.user.to_string(),
			auth_subject: person.subject.clone(),
			member: Member {
				email: "p@example.com".to_owned(),
				email_verified: true,
				username: String::new(),
				preferred_name: String::new(),
				status: status.to_owned(),
				kyc_level: 0,
				token_version: 1,
				lifecycle_sequence,
			},
		},
	}
}

fn grants(scope: &str, person: &Person, permissions: &[&str], sequence: u64) -> ScopeEvent {
	ScopeEvent {
		event_id: event_id(),
		position: 0,
		scope: scope.to_owned(),
		occurred_at: 1_800_000_000,
		sequence,
		body: ScopeEventBody::GrantsChanged {
			user_id: person.user.to_string(),
			auth_subject: person.subject.clone(),
			permissions: permissions.iter().map(|p| (*p).to_owned()).collect(),
		},
	}
}

fn lifecycle(kind: UserEventKind, person: &Person, sequence: u64) -> UserEvent {
	UserEvent {
		event_id: event_id(),
		kind,
		user_id: person.user.to_string(),
		auth_subject: person.subject.clone(),
		sequence,
		occurred_at: 1_800_000_000,
		kyc_level: 0,
		email: "p@example.com".to_owned(),
		email_verified: true,
		token_version: 1,
		role: "investor".to_owned(),
	}
}

#[tokio::test]
async fn a_member_their_grants_and_their_lifecycle_land_on_one_row() {
	let Some(h) = harness().await else { return };
	let p = person();
	h.joins(&p, 1);
	h.concierge.push_scope(grants("invest", &p, &[MANAGE], 2));
	h.concierge.push_user(lifecycle(UserEventKind::Suspended, &p, 2));

	let cycle = h.cycle().await;
	assert_eq!((cycle.applied, cycle.parked), (3, 0), "{cycle:?}");
	assert_eq!(h.active(&p).await, Some(false), "suspended by the lifecycle stream");
	assert!(!h.may(&p, Permission::AllocationManage).await, "a suspended seat holds nothing, whatever it was granted");
	assert_eq!(
		h.users.auth_subject(p.user).await.unwrap().as_deref(),
		Some(p.subject.as_str()),
		"banking's subject is learned from the stream"
	);

	h.concierge.push_user(lifecycle(UserEventKind::Reinstated, &p, 3));
	h.cycle().await;
	assert_eq!(h.active(&p).await, Some(true));
	assert!(h.may(&p, Permission::AllocationManage).await);
	assert!(!h.may(&p, Permission::ValuationPost).await, "only what was granted");
}

#[tokio::test]
async fn a_disabled_member_is_seeded_suspended_and_a_neutral_kind_keeps_the_status() {
	let Some(h) = harness().await else { return };
	let p = person();
	h.concierge.push_scope(member_joined("invest", &p, "disabled", 4, 1));
	h.concierge.push_user(lifecycle(UserEventKind::KycChanged, &p, 5));

	h.cycle().await;
	assert_eq!(h.active(&p).await, Some(false), "a KYC change says nothing of the status");
}

#[tokio::test]
async fn a_replay_and_a_stale_event_change_nothing() {
	let Some(h) = harness().await else { return };
	let p = person();
	h.joins(&p, 1);
	let granted = grants("invest", &p, &[MANAGE], 3);
	h.concierge.push_scope(granted.clone());
	// Redelivered: the same event_id again.
	h.concierge.push_scope(granted);
	// Older than what landed: a grant set the seat no longer has.
	h.concierge.push_scope(grants("invest", &p, &[], 2));

	let cycle = h.cycle().await;
	assert_eq!((cycle.applied, cycle.skipped), (2, 2), "{cycle:?}");
	assert!(h.may(&p, Permission::AllocationManage).await, "the stale empty set did not revoke");

	// Everything pulled again from position 0, as after a cursor lost to a restore.
	sqlx::query("DELETE FROM concierge_projection_cursor").execute(&h.pool).await.unwrap();
	let again = h.cycle().await;
	assert_eq!(again.applied, 0, "{again:?}");
	assert!(h.may(&p, Permission::AllocationManage).await);
}

#[tokio::test]
async fn a_late_member_snapshot_does_not_undo_a_suspension() {
	let Some(h) = harness().await else { return };
	let p = person();
	// Visible first through `sa`, suspended at lifecycle 5 …
	h.concierge.push_scope(member_joined("sa", &p, "active", 4, 1));
	h.concierge.push_user(lifecycle(UserEventKind::Suspended, &p, 5));
	h.cycle().await;
	assert_eq!(h.active(&p).await, Some(false));

	// … then joins `invest` with a snapshot written before the suspension, read only now.
	h.concierge.push_scope(member_joined("invest", &p, "active", 4, 1));
	h.cycle().await;
	assert_eq!(h.active(&p).await, Some(false), "a snapshot older than the row's status must not reinstate");
}

#[tokio::test]
async fn grants_of_another_scope_are_not_invest_permissions() {
	let Some(h) = harness().await else { return };
	let p = person();
	h.concierge.push_scope(member_joined("sa", &p, "active", 1, 1));
	h.concierge.push_scope(grants("sa", &p, &["sa:admin", MANAGE], 2));

	h.cycle().await;
	assert_eq!(h.active(&p).await, Some(true), "a member of an observed scope is a person invest knows");
	assert!(!h.may(&p, Permission::AllocationManage).await, "an `invest:*` name inside another scope's set is not a grant");
}

#[tokio::test]
async fn the_cursor_survives_a_restart() {
	let Some(h) = harness().await else { return };
	let p = person();
	h.joins(&p, 1);
	let head = h.concierge.push_user(lifecycle(UserEventKind::Suspended, &p, 2));
	h.cycle().await;
	assert_eq!(h.cursor("scope").await, Some(1));
	assert_eq!(h.cursor("user_lifecycle").await, Some(head));

	// A new engine — a restarted invest-core — pulls from the stored cursor: only the new row.
	h.concierge.push_user(lifecycle(UserEventKind::Reinstated, &p, 3));
	let cycle = h.cycle().await;
	assert_eq!((cycle.applied, cycle.skipped), (1, 0), "{cycle:?}");
	assert_eq!(h.cursor("user_lifecycle").await, Some(head + 1));
	assert_eq!(h.active(&p).await, Some(true));
}

#[tokio::test]
async fn a_kind_this_build_cannot_name_is_parked_and_replayed_after_an_upgrade() {
	let Some(h) = harness().await else { return };
	let (a, b) = (person(), person());
	h.joins(&a, 1);
	h.joins(&b, 1);
	let unknown = h.concierge.push_user_unknown(&event_id(), &a.subject, 99);
	h.concierge.push_user(lifecycle(UserEventKind::Suspended, &b, 2));
	// A's next event queues behind the parked one, so the sequence guard never runs past it.
	let behind = h.concierge.push_user(lifecycle(UserEventKind::Reinstated, &a, 3));
	let scope_unknown = h.concierge.push_scope_unknown(&event_id(), "invest", 42);

	let cycle = h.cycle().await;
	assert_eq!(h.active(&b).await, Some(false), "one unreadable row does not hold back another user's suspension");
	assert_eq!(
		h.parked().await,
		vec![("scope".to_owned(), scope_unknown), ("user_lifecycle".to_owned(), unknown), ("user_lifecycle".to_owned(), behind)]
	);
	assert_eq!(cycle.parked, 3, "{cycle:?}");
	assert_eq!(h.cursor("user_lifecycle").await, Some(behind), "the cursor moved past what it parked");
	assert_eq!(h.active(&a).await, Some(true), "nothing of A applied out of order");

	// A build that can read the kind: the same row, re-pulled by position.
	let mut suspended = lifecycle(UserEventKind::Suspended, &a, 2);
	suspended.event_id = sqlx::query_scalar("SELECT event_id FROM concierge_projection_parked WHERE stream = 'user_lifecycle' AND position = $1")
		.bind(unknown)
		.fetch_one(&h.pool)
		.await
		.unwrap();
	h.concierge.upgrade_user(unknown, suspended);
	let replayed = h.projection().replay_parked().await.unwrap();
	assert_eq!(replayed, 2, "the upgraded row and the one queued behind it");
	assert_eq!(h.active(&a).await, Some(true), "applied in order: suspended at 2, reinstated at 3");
	assert_eq!(
		h.parked().await,
		vec![("scope".to_owned(), scope_unknown)],
		"the scope row this build still cannot read stays parked"
	);
}

#[tokio::test]
async fn a_revoked_grant_is_refused_at_the_next_check() {
	let Some(h) = harness().await else { return };
	let p = person();
	h.joins(&p, 1);
	h.concierge.push_scope(grants("invest", &p, &[MANAGE, "invest:valuation:post"], 2));
	h.cycle().await;
	assert!(h.may(&p, Permission::AllocationManage).await);
	assert!(h.may(&p, Permission::ValuationPost).await);

	h.concierge.push_scope(grants("invest", &p, &["invest:valuation:post"], 3));
	h.cycle().await;
	assert!(!h.may(&p, Permission::AllocationManage).await, "revoked");
	assert!(h.may(&p, Permission::ValuationPost).await, "the set is replaced, the rest kept");

	h.concierge.push_scope(grants("invest", &p, &[], 4));
	h.cycle().await;
	assert!(!h.may(&p, Permission::ValuationPost).await, "an empty set holds nothing");
}

#[tokio::test]
async fn a_suspension_from_concierge_refuses_a_mint_until_reinstated() {
	let Some(h) = harness().await else { return };
	let Some(engine) = common::engine("users-projection test").await else { return };
	let p = person();
	h.joins(&p, 1);
	h.concierge.push_user(lifecycle(UserEventKind::Suspended, &p, 2));
	h.cycle().await;

	let service = ServiceId::parse(&format!("svc-{}", Uuid::new_v4())).unwrap();
	let mut allocation = Allocation::register(AllocationId::new(), service.clone(), "Projection", "", AllocationIcon::default()).unwrap();
	engine.allocations.register(&mut allocation, &ScopeId::invest()).await.unwrap();
	let mint = |key: &str| common::mint_request(&service, UnitHolder::User(p.user), "10", None, key);
	let now = 1_800_000_000;

	let err = issuance_app::issue_units(&engine.fund_ports(), &engine.issuances, &engine.users, mint("suspended"), now)
		.await
		.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(ref m) if m.contains("not active")), "got {err:?}");

	h.concierge.push_user(lifecycle(UserEventKind::Reinstated, &p, 3));
	h.cycle().await;
	issuance_app::issue_units(&engine.fund_ports(), &engine.issuances, &engine.users, mint("reinstated"), now)
		.await
		.unwrap();
	common::drain_to_quiescence(&engine.relay, &engine.pool).await;
}

#[tokio::test]
async fn run_pulls_as_the_singleton_and_stops_on_shutdown() {
	let Some(h) = harness().await else { return };
	let p = person();
	h.joins(&p, 1);
	h.concierge.push_scope(grants("invest", &p, &[MANAGE], 2));

	// Held elsewhere, the lock keeps `run` from pulling anything.
	let held = h.projection().try_acquire_lock().await.unwrap().expect("the lock is free");
	let shutdown = CancellationToken::new();
	let (applied, ()) = tokio::join!(
		async {
			let blocked_until = Instant::now() + Duration::from_millis(300);
			while Instant::now() < blocked_until {
				assert!(!h.may(&p, Permission::AllocationManage).await, "pulled without the lock");
				tokio::task::yield_now().await;
			}
			held.close().await.unwrap();
			let deadline = Instant::now() + Duration::from_secs(10);
			while !h.may(&p, Permission::AllocationManage).await {
				assert!(Instant::now() < deadline, "run never applied the grant after the lock was released");
				tokio::time::sleep(Duration::from_millis(20)).await;
			}
			assert!(h.projection().try_acquire_lock().await.unwrap().is_none(), "run holds the lock while it pulls");
			shutdown.cancel();
			true
		},
		h.projection().run(shutdown.clone()),
	);
	assert!(applied);
}

#[tokio::test]
async fn a_concierge_outage_is_an_error_the_next_cycle_recovers_from() {
	let Some(h) = harness().await else { return };
	let p = person();
	h.joins(&p, 1);
	h.concierge.outage(true);
	assert!(h.projection().cycle().await.is_err());
	assert_eq!(h.active(&p).await, None, "nothing applied");

	h.concierge.outage(false);
	h.cycle().await;
	assert_eq!(h.active(&p).await, Some(true));
}
