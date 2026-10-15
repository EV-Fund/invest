//! Postgres adapter for the [`UserDirectory`] port — the `users` projection.
//!
//! A row is a person concierge told invest about, keyed by their concierge user id. The
//! reader answers the use cases. The writers are concierge's streams, through
//! [`infrastructure::concierge`](super::concierge): [`PgUsers::seed_member`],
//! [`PgUsers::apply_lifecycle`] and [`PgUsers::replace_permissions`]. [`PgUsers::record`]
//! sets a row outright, for tests that need a person without a stream.
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
	/// permissions are never touched (they arrive on GRANTS_CHANGED, per scope).
	pub async fn seed_member(&self, user: UserId, auth_subject: &str, active: bool, lifecycle_sequence: u64) -> Result<(), DomainError> {
		let sequence = sequence_column(user, lifecycle_sequence)?;
		sqlx::query(
			"INSERT INTO users (id, status, auth_subject, lifecycle_sequence) VALUES ($1, $2, $3, $4) \
			 ON CONFLICT (id) DO UPDATE SET \
			   status = CASE WHEN users.lifecycle_sequence IS NULL OR users.lifecycle_sequence <= EXCLUDED.lifecycle_sequence \
			                 THEN EXCLUDED.status ELSE users.status END, \
			   lifecycle_sequence = GREATEST(users.lifecycle_sequence, EXCLUDED.lifecycle_sequence), \
			   auth_subject = COALESCE(users.auth_subject, EXCLUDED.auth_subject), updated_at = now()",
		)
		.bind(user.raw())
		.bind(status_word(active))
		.bind(auth_subject)
		.bind(sequence)
		.execute(&self.pool)
		.await
		.map_err(|err| repo_err_for(user, "seed", err))?;
		Ok(())
	}

	/// Apply a lifecycle event at `sequence`: `active` is what the event says of the status
	/// (`None` — it says nothing, the status stays). Returns `false` when `user` has no row:
	/// their MEMBER_JOINED has not been applied, and a lifecycle fact alone does not make a
	/// person invest knows.
	pub async fn apply_lifecycle(&self, user: UserId, auth_subject: &str, active: Option<bool>, sequence: u64) -> Result<bool, DomainError> {
		let sequence = sequence_column(user, sequence)?;
		let updated = sqlx::query(
			"UPDATE users SET \
			   status = CASE WHEN $2::text IS NOT NULL AND (lifecycle_sequence IS NULL OR lifecycle_sequence < $4) \
			                 THEN $2 ELSE status END, \
			   lifecycle_sequence = GREATEST(lifecycle_sequence, $4), \
			   auth_subject = COALESCE(auth_subject, $3), updated_at = now() \
			 WHERE id = $1",
		)
		.bind(user.raw())
		.bind(active.map(status_word))
		.bind(auth_subject)
		.bind(sequence)
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

/// concierge's sequences are `u64`; the column is `BIGINT`. One past `i64::MAX` is not a
/// sequence any real history reaches, so it is a corrupt event, not a value to clamp.
fn sequence_column(user: UserId, sequence: u64) -> Result<i64, DomainError> {
	i64::try_from(sequence).map_err(|_| DomainError::Repository(format!("lifecycle sequence {sequence} for {user} does not fit BIGINT")))
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
