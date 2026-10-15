//! Postgres adapter for the [`UserDirectory`] port — the `users` projection.
//!
//! A row is a person concierge told invest about, keyed by their concierge user id. The
//! reader answers the use cases; [`PgUsers::record`] is the projection's one writer —
//! concierge's user stream once its consumer lands, and the tests until then.

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
		.bind(if standing.active { "active" } else { "suspended" })
		.bind(standing.permissions.as_deref())
		.bind(auth_subject)
		.execute(&self.pool)
		.await
		.map_err(repo_err)?;
		Ok(())
	}
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
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}
