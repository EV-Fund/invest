//! The writer of the `users` projection: concierge's lifecycle and scope streams, pulled
//! with invest's relying-party credential.
//!
//! The pull engine is concierge's own `evconcierge_projection`: cursors, dedupe by
//! `event_id`, per-key order by `sequence`, parking of a kind this build cannot name (by
//! position, replayed on the next start) and of a lifecycle event whose MEMBER_JOINED has
//! not landed. Its bookkeeping lives in `concierge_projection_*` ([`db::migrate`]). This
//! module is what invest does with each event — [`UsersHandler`] — and the singleton that
//! runs the engine — [`ConciergeProjection`]. [`catalog`] publishes, over the same
//! credential, the permission catalog those grants resolve against.
//!
//! What a row takes from the streams:
//!
//! | Event | `users` |
//! | ----- | ------- |
//! | MEMBER_JOINED (any visible scope) | the row: `auth_subject`, `status` (`active`, or `suspended` for concierge's `disabled`) |
//! | lifecycle SUSPENDED / REINSTATED | `status` |
//! | GRANTS_CHANGED in scope `invest` | `permissions` — the whole `invest:*` set, replaced |
//!
//! Other scopes' grants (`sa:*` for the allocations invest runs there) and SCOPE_CHANGED are
//! read and not kept: nothing in invest decides on them yet. A scope's freeze is enforced by
//! banking, which refuses its debits.
//!
//! [`db::migrate`]: super::db::migrate

use std::time::Duration;

use async_trait::async_trait;
use evconcierge_projection::{
	Cycle, EventSource, GrpcSource, Handler, PgProjectionStore, Projection, ProjectionConfig, ProjectionError, ScopeEvent, ScopeEventBody, UserEvent, UserEventKind,
};
use invest_domain::{error::DomainError, users::UserId};
use sqlx::{PgPool, Postgres, pool::PoolConnection};
use tokio_util::sync::CancellationToken;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};
use tracing::{debug, error, warn};
use uuid::Uuid;

use super::{singleton, users::PgUsers};
use crate::ports::money::ScopeId;

pub mod catalog;
pub mod fake;

/// Fixed key for the session-level `pg_advisory_lock` the projection holds while it pulls.
/// The engine's sequence guard is read-then-write per event, so two pullers could each pass
/// it with a different event of one user and land the older one last. ASCII "EVINUSR_";
/// only stability matters, as for the relay's key.
const PROJECTION_LOCK_KEY: i64 = 0x4556_494e_5553_525f_u64 as i64;

/// Deadline on one pull, and on dialling concierge.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Where concierge listens and how invest proves itself to it (concierge
/// docs/SCOPES-AND-APPROVALS.md §2: `x-ev-client-id` + `authorization: Bearer <secret>`).
pub struct ConciergeStreamsConfig {
	pub addr: String,
	pub client_id: String,
	pub client_secret: String,
	/// The CA of concierge's TLS listener (PEM), for an `https://` address.
	pub ca_pem: Option<String>,
	pub tls_domain: Option<String>,
}

/// The stream source over concierge's gRPC. Lazy: a concierge that is down at boot is a
/// retried pull later, not a failed boot.
pub fn connect(config: &ConciergeStreamsConfig) -> color_eyre::Result<GrpcSource> {
	GrpcSource::from_channel(channel(config)?, &config.client_id, &config.client_secret).map_err(|err| color_eyre::eyre::eyre!("concierge stream client: {err}"))
}

/// A lazy channel to concierge, over TLS when the address is `https://`. Shared by the
/// stream source and the catalog publisher.
fn channel(config: &ConciergeStreamsConfig) -> color_eyre::Result<Channel> {
	let mut endpoint = Endpoint::from_shared(config.addr.clone())?.connect_timeout(CALL_TIMEOUT).timeout(CALL_TIMEOUT);
	if let Some(ca_pem) = &config.ca_pem {
		let mut tls = ClientTlsConfig::new().ca_certificate(Certificate::from_pem(ca_pem));
		if let Some(domain) = &config.tls_domain {
			tls = tls.domain_name(domain.clone());
		}
		endpoint = endpoint.tls_config(tls)?;
	}
	Ok(endpoint.connect_lazy())
}

/// What invest does with each event: snapshots into `users`, keyed by the concierge user id
/// (the `sub` of an `invest` token), carrying the `auth_subject` banking names compartments by.
pub struct UsersHandler {
	users: PgUsers,
	/// The scope whose grants are invest's permissions.
	scope: ScopeId,
}

impl UsersHandler {
	pub fn new(pool: PgPool) -> Self {
		Self {
			users: PgUsers::new(pool),
			scope: ScopeId::invest(),
		}
	}
}

#[async_trait]
impl Handler for UsersHandler {
	type Error = DomainError;

	async fn on_user_event(&self, event: &UserEvent) -> Result<(), DomainError> {
		let Some(user) = user_id(&event.user_id, &event.event_id) else {
			return Ok(());
		};
		let active = match event.kind {
			UserEventKind::Suspended => Some(false),
			UserEventKind::Reinstated => Some(true),
			// Snapshots of facts invest does not keep (KYC level, role, token floor, the money
			// plane's seat set). CREATED says nothing a MEMBER_JOINED did not already seed.
			UserEventKind::Created | UserEventKind::KycChanged | UserEventKind::SessionsRevoked | UserEventKind::RoleChanged | UserEventKind::PermissionsChanged => None,
		};
		if !self.users.apply_lifecycle(user, &event.auth_subject, active, event.sequence).await? {
			// The engine hands a lifecycle event over only after the user's MEMBER_JOINED, which
			// writes the row; its absence means the projection tables were reset without the
			// engine's bookkeeping (the crate README's "Rebuilding").
			warn!(event_id = %event.event_id, %user, "concierge projection: lifecycle event for a user with no row — projection and bookkeeping disagree (needs intervention)");
		}
		Ok(())
	}

	async fn on_scope_event(&self, event: &ScopeEvent) -> Result<(), DomainError> {
		match &event.body {
			ScopeEventBody::MemberJoined { user_id: raw, auth_subject, member } => {
				let Some(user) = user_id(raw, &event.event_id) else {
					return Ok(());
				};
				let active = member_active(&member.status, &event.event_id);
				self.users.seed_member(user, auth_subject, active, member.lifecycle_sequence).await
			}
			ScopeEventBody::GrantsChanged {
				user_id: raw,
				auth_subject,
				permissions,
			} => {
				if event.scope != self.scope.as_str() {
					debug!(event_id = %event.event_id, scope = %event.scope, "concierge projection: grants of another scope, not kept");
					return Ok(());
				}
				let Some(user) = user_id(raw, &event.event_id) else {
					return Ok(());
				};
				self.users.replace_permissions(user, auth_subject, &own_namespace(permissions, &event.event_id)).await
			}
			ScopeEventBody::ScopeChanged(scope) => {
				debug!(event_id = %event.event_id, scope = %scope.id, state = ?scope.state, "concierge projection: scope change, not kept");
				Ok(())
			}
		}
	}
}

/// The concierge user id an event names. One that is not a UUID can never match a token's
/// `sub`, so there is no row it could apply to: logged and skipped, never retried — a retry
/// would hold every later event behind it for ever.
fn user_id(raw: &str, event_id: &str) -> Option<UserId> {
	match Uuid::parse_str(raw) {
		Ok(id) => Some(UserId::from_raw(id)),
		Err(err) => {
			error!(%event_id, user_id = %raw, "concierge projection: event names a user id that is not a UUID ({err}); skipped (needs intervention)");
			None
		}
	}
}

/// concierge's account status (`domain::users::UserStatus`): `active` or `disabled`. A word
/// this build does not know is read as not active — the safe reading of a status one cannot
/// name.
fn member_active(status: &str, event_id: &str) -> bool {
	match status {
		"active" => true,
		"disabled" => false,
		other => {
			warn!(%event_id, status = %other, "concierge projection: unknown account status, read as suspended");
			false
		}
	}
}

/// The `invest:*` part of a grant set. concierge sends only the scope's namespace; anything
/// else would be a permission invest must not honour, so it is dropped loudly.
fn own_namespace(permissions: &[String], event_id: &str) -> Vec<String> {
	let (own, foreign): (Vec<String>, Vec<String>) = permissions.iter().cloned().partition(|p| p.starts_with("invest:"));
	if !foreign.is_empty() {
		warn!(%event_id, ?foreign, "concierge projection: grants outside the invest namespace in scope invest, dropped");
	}
	own
}

/// The projection as a background worker: the engine over invest's Postgres, run by one
/// invest-core at a time under [`PROJECTION_LOCK_KEY`].
pub struct ConciergeProjection<Src> {
	pool: PgPool,
	engine: Projection<PgProjectionStore, UsersHandler, Src>,
}

impl<Src: EventSource> ConciergeProjection<Src> {
	/// Over a pool whose schema [`db::migrate`](super::db::migrate) has brought up to date —
	/// both invest's migrations and the engine's tables.
	pub fn new(pool: PgPool, source: Src, config: ProjectionConfig) -> Self {
		let engine = Projection::new(PgProjectionStore::new(pool.clone()), UsersHandler::new(pool.clone()), source, config);
		Self { pool, engine }
	}

	/// One pass over both streams, unfenced — the tests own the sole puller by construction;
	/// [`run`](Self::run) always holds the lock.
	pub async fn cycle(&self) -> Result<Cycle, ProjectionError> {
		let mut cycle = Cycle::default();
		self.engine.cycle(&mut cycle).await?;
		Ok(cycle)
	}

	/// Re-pull what is parked and apply what this build can now read. [`run`](Self::run) does
	/// this on every acquisition of the lock.
	pub async fn replay_parked(&self) -> Result<u64, ProjectionError> {
		self.engine.replay_parked().await
	}

	/// Run until `shutdown` is cancelled. Take the singleton lock on a dedicated connection
	/// (a second invest-core blocks here, owning nothing), then run the engine — it replays
	/// what is parked and cycles with its own backoff — while the lock connection is probed.
	/// A lost lock or a failed acquisition re-enters the acquisition with backoff and never
	/// returns: the composition root tears every branch down when one ends, and a database
	/// blip must not take the gRPC server with it.
	///
	/// The engine's future is dropped mid-cycle on shutdown or a lost lock. That is safe by
	/// its contract: a cursor moves only after its events are done, so a cut-off event is
	/// pulled again and re-applied as a snapshot.
	pub async fn run(self, shutdown: CancellationToken) {
		singleton::run_under_lock("concierge projection", PROJECTION_LOCK_KEY, &self.pool, shutdown, || self.engine.run()).await;
	}

	/// Block on a dedicated pooled connection until the session advisory lock is held; the
	/// connection's lifetime is the lock's.
	pub async fn acquire_lock(&self) -> Result<PoolConnection<Postgres>, sqlx::Error> {
		singleton::acquire(&self.pool, PROJECTION_LOCK_KEY).await
	}

	/// Take the lock if it is free, else `None` — for tests that observe the singleton.
	pub async fn try_acquire_lock(&self) -> Result<Option<PoolConnection<Postgres>>, sqlx::Error> {
		singleton::try_acquire(&self.pool, PROJECTION_LOCK_KEY).await
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn only_concierges_active_status_is_active() {
		assert!(member_active("active", "e"));
		assert!(!member_active("disabled", "e"));
		assert!(!member_active("frozen", "e"), "a status this build cannot name is not active");
		assert!(!member_active("", "e"));
	}

	#[test]
	fn grants_outside_the_invest_namespace_are_dropped() {
		let granted = vec!["invest:allocation:manage".to_owned(), "sa:admin".to_owned(), "investor:x".to_owned()];
		assert_eq!(own_namespace(&granted, "e"), vec!["invest:allocation:manage".to_owned()]);
	}

	#[test]
	fn a_user_id_that_is_not_a_uuid_names_nobody() {
		assert!(user_id("not-a-uuid", "e").is_none());
		let id = Uuid::new_v4();
		assert_eq!(user_id(&id.to_string(), "e").map(|u| u.raw()), Some(id));
	}
}
