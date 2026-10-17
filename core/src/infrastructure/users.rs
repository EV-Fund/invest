//! Postgres adapter for the [`UserDirectory`] port — the `users` projection.
//!
//! A row is a person concierge told invest about, keyed by their concierge user id. The
//! reader answers the use cases. The writers are concierge's streams, through
//! [`infrastructure::concierge`](super::concierge): [`PgUsers::seed_member`],
//! [`PgUsers::apply_lifecycle`] and [`PgUsers::replace_permissions`]. [`PgUsers::record`]
//! sets a row outright, for tests that need a person without a stream.
//!
//! `token_version` is the revocation threshold: it only ever rises ([`PgUsers::token_version`]
//! reads it for the auth layer), so a stale snapshot cannot lift a revocation.
//!
//! `status` moves only forward in concierge's per-user lifecycle sequence
//! (`lifecycle_sequence`): the two streams are read one after the other, so a MEMBER_JOINED
//! snapshot can arrive after a later SUSPENDED and must not undo it.

use async_trait::async_trait;
use invest_domain::{error::DomainError, users::UserId};
use sqlx::PgPool;

use crate::ports::users::{UserDirectory, UserStanding};

pub struct PgUsers {
	pool: PgPool,
}

impl PgUsers {
	pub fn new(pool: PgPool) -> Self {
		Self { pool }
	}

	/// Upsert what concierge says of `user`: whether they are active, their `invest:*`
	/// permissions (`None` — not said), and the `auth_subject` banking names their
	/// compartment by (`None` — not known yet; a later `Some` fills it, a later `None`
	/// never erases it).
	pub async fn record(&self, user: UserId, standing: &UserStanding, auth_subject: Option<&str>) -> Result<(), DomainError> {
		sqlx::query(
			"INSERT INTO users (id, status, permissions, auth_subject) VALUES ($1, $2, $3, $4) \
			 ON CONFLICT (id) DO UPDATE SET status = EXCLUDED.status, permissions = EXCLUDED.permissions, \
			 auth_subject = COALESCE(EXCLUDED.auth_subject, users.auth_subject), updated_at = now()",
		)
		.bind(user.raw())
		.bind(status_word(standing.active))
		.bind(standing.permissions.as_deref())
		.bind(auth_subject)
		.execute(&self.pool)
		.await
		.map_err(repo_err)?;
		Ok(())
	}
}

impl PgUsers {
	/// Seed or refresh `user` from a MEMBER_JOINED snapshot as of `lifecycle_sequence`. The
	/// status is taken only when the snapshot is at least as new as the row's; the
	/// permissions are never touched (they arrive on GRANTS_CHANGED, per scope). The
	/// `token_version` threshold takes the snapshot's value only if it is higher.
	pub async fn seed_member(&self, user: UserId, auth_subject: &str, active: bool, lifecycle_sequence: u64, token_version: u64) -> Result<(), DomainError> {
		let sequence = bigint(user, "lifecycle sequence", lifecycle_sequence)?;
		let token_version = bigint(user, "token version", token_version)?;
		sqlx::query(
			"INSERT INTO users (id, status, auth_subject, lifecycle_sequence, token_version) VALUES ($1, $2, $3, $4, $5) \
			 ON CONFLICT (id) DO UPDATE SET \
			   status = CASE WHEN users.lifecycle_sequence IS NULL OR users.lifecycle_sequence <= EXCLUDED.lifecycle_sequence \
			                 THEN EXCLUDED.status ELSE users.status END, \
			   lifecycle_sequence = GREATEST(users.lifecycle_sequence, EXCLUDED.lifecycle_sequence), \
			   token_version = GREATEST(users.token_version, EXCLUDED.token_version), \
			   auth_subject = COALESCE(users.auth_subject, EXCLUDED.auth_subject), updated_at = now()",
		)
		.bind(user.raw())
		.bind(status_word(active))
		.bind(auth_subject)
		.bind(sequence)
		.bind(token_version)
		.execute(&self.pool)
		.await
		.map_err(|err| repo_err_for(user, "seed", err))?;
		Ok(())
	}

	/// Apply a lifecycle event at `sequence`: `active` is what the event says of the status
	/// (`None` — it says nothing, the status stays). Returns `false` when `user` has no row:
	/// their MEMBER_JOINED has not been applied, and a lifecycle fact alone does not make a
	/// person invest knows. Every lifecycle kind is a snapshot of the user, so each one may
	/// raise the `token_version` threshold (SESSIONS_REVOKED is the one that does).
	pub async fn apply_lifecycle(&self, user: UserId, auth_subject: &str, active: Option<bool>, sequence: u64, token_version: u64) -> Result<bool, DomainError> {
		let sequence = bigint(user, "lifecycle sequence", sequence)?;
		let token_version = bigint(user, "token version", token_version)?;
		let updated = sqlx::query(
			"UPDATE users SET \
			   status = CASE WHEN $2::text IS NOT NULL AND (lifecycle_sequence IS NULL OR lifecycle_sequence < $4) \
			                 THEN $2 ELSE status END, \
			   lifecycle_sequence = GREATEST(lifecycle_sequence, $4), \
			   token_version = GREATEST(token_version, $5), \
			   auth_subject = COALESCE(auth_subject, $3), updated_at = now() \
			 WHERE id = $1",
		)
		.bind(user.raw())
		.bind(active.map(status_word))
		.bind(auth_subject)
		.bind(sequence)
		.bind(token_version)
		.execute(&self.pool)
		.await
		.map_err(|err| repo_err_for(user, "apply lifecycle to", err))?;
		Ok(updated.rows_affected() > 0)
	}

	/// Replace `user`'s `invest:*` permissions with `permissions`, the whole set concierge
	/// now resolves for their seat (empty — holds nothing). A person not seeded yet gets a
	/// row that is `suspended` until their MEMBER_JOINED states otherwise: a grant alone
	/// does not say the account is active.
	pub async fn replace_permissions(&self, user: UserId, auth_subject: &str, permissions: &[String]) -> Result<(), DomainError> {
		sqlx::query(
			"INSERT INTO users (id, status, permissions, auth_subject) VALUES ($1, 'suspended', $2, $3) \
			 ON CONFLICT (id) DO UPDATE SET permissions = EXCLUDED.permissions, \
			   auth_subject = COALESCE(users.auth_subject, EXCLUDED.auth_subject), updated_at = now()",
		)
		.bind(user.raw())
		.bind(permissions)
		.bind(auth_subject)
		.execute(&self.pool)
		.await
		.map_err(|err| repo_err_for(user, "replace the permissions of", err))?;
		Ok(())
	}
}

fn status_word(active: bool) -> &'static str {
	if active { "active" } else { "suspended" }
}

/// concierge's sequences and token versions are `u64`; the columns are `BIGINT`. One past
/// `i64::MAX` is not a value any real history reaches, so it is a corrupt event, not
/// something to clamp.
fn bigint(user: UserId, what: &str, value: u64) -> Result<i64, DomainError> {
	i64::try_from(value).map_err(|_| DomainError::Repository(format!("{what} {value} for {user} does not fit BIGINT")))
}

fn repo_err_for(user: UserId, action: &str, err: sqlx::Error) -> DomainError {
	DomainError::Repository(format!("{action} user {user}: {err}"))
}

#[async_trait]
impl UserDirectory for PgUsers {
	async fn standing(&self, user: UserId) -> Result<Option<UserStanding>, DomainError> {
		let row = sqlx::query_as::<_, (String, Option<Vec<String>>)>("SELECT status, permissions FROM users WHERE id = $1")
			.bind(user.raw())
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?;
		row.map(|(status, permissions)| {
			let active = match status.as_str() {
				"active" => true,
				"suspended" => false,
				other => return Err(DomainError::Repository(format!("users.status '{other}' for {user} is outside the vocabulary"))),
			};
			Ok(UserStanding { active, permissions })
		})
		.transpose()
	}

	async fn token_version(&self, user: UserId) -> Result<Option<u64>, DomainError> {
		let floor: Option<Option<i64>> = sqlx::query_scalar("SELECT token_version FROM users WHERE id = $1")
			.bind(user.raw())
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?;
		// The CHECK keeps the column non-negative, so the conversion cannot fail; if it ever
		// did, a threshold that cannot be read must refuse, not accept.
		floor
			.flatten()
			.map(|v| u64::try_from(v).map_err(|_| DomainError::Repository(format!("users.token_version {v} for {user} is negative"))))
			.transpose()
	}

	async fn auth_subject(&self, user: UserId) -> Result<Option<String>, DomainError> {
		let subject: Option<Option<String>> = sqlx::query_scalar("SELECT auth_subject FROM users WHERE id = $1")
			.bind(user.raw())
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?;
		Ok(subject.flatten())
	}
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}
