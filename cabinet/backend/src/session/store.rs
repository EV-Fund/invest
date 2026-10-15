//! The `cabinet` schema's two tables, as runtime queries (`migrations/` holds the DDL).
//! Every timestamp is unix seconds, the unit concierge states token expiries in.

use color_eyre::eyre::{Context, Result};
use sqlx::{PgPool, Row};
use uuid::Uuid;

pub struct SignInRow {
	pub verifier: String,
	pub return_to: String,
}

pub struct SessionRow {
	pub user_id: Uuid,
	pub access_sealed: Vec<u8>,
	pub access_expires_at: i64,
	pub refresh_sealed: Vec<u8>,
	pub expires_at: i64,
}

pub struct NewTokens<'a> {
	pub access_sealed: &'a [u8],
	pub access_expires_at: i64,
	pub refresh_sealed: &'a [u8],
	pub expires_at: i64,
}

pub async fn insert_sign_in(pool: &PgPool, state_hash: &[u8], verifier: &str, return_to: &str, expires_at: i64) -> Result<()> {
	sqlx::query("INSERT INTO cabinet.sign_ins (state_hash, verifier, return_to, expires_at) VALUES ($1, $2, $3, $4)")
		.bind(state_hash)
		.bind(verifier)
		.bind(return_to)
		.bind(expires_at)
		.execute(pool)
		.await
		.wrap_err("record a sign-in")?;
	Ok(())
}

/// Redeem a sign-in: deleted as it is read, so a replayed callback finds nothing — across
/// replicas, since the row is the only copy. `None` for a state never issued, already
/// redeemed, or past `now`.
pub async fn take_sign_in(pool: &PgPool, state_hash: &[u8], now: i64) -> Result<Option<SignInRow>> {
	let row = sqlx::query("DELETE FROM cabinet.sign_ins WHERE state_hash = $1 RETURNING verifier, return_to, expires_at")
		.bind(state_hash)
		.fetch_optional(pool)
		.await
		.wrap_err("redeem a sign-in")?;
	let Some(row) = row else { return Ok(None) };
	if row.try_get::<i64, _>("expires_at")? <= now {
		return Ok(None);
	}
	Ok(Some(SignInRow {
		verifier: row.try_get("verifier")?,
		return_to: row.try_get("return_to")?,
	}))
}

/// Drop what can no longer be used: sign-ins and sessions past their deadline. Run on each
/// new sign-in, so the tables stay as small as the live set without a sweeper.
pub async fn prune(pool: &PgPool, now: i64) -> Result<u64> {
	let sign_ins = sqlx::query("DELETE FROM cabinet.sign_ins WHERE expires_at <= $1")
		.bind(now)
		.execute(pool)
		.await
		.wrap_err("prune sign-ins")?
		.rows_affected();
	let sessions = sqlx::query("DELETE FROM cabinet.sessions WHERE expires_at <= $1")
		.bind(now)
		.execute(pool)
		.await
		.wrap_err("prune sessions")?
		.rows_affected();
	Ok(sign_ins + sessions)
}

pub async fn insert_session(pool: &PgPool, id_hash: &[u8], user_id: Uuid, tokens: &NewTokens<'_>, now: i64) -> Result<()> {
	sqlx::query(
		"INSERT INTO cabinet.sessions (id_hash, user_id, access_sealed, access_expires_at, refresh_sealed, expires_at, created_at)
		 VALUES ($1, $2, $3, $4, $5, $6, $7)",
	)
	.bind(id_hash)
	.bind(user_id)
	.bind(tokens.access_sealed)
	.bind(tokens.access_expires_at)
	.bind(tokens.refresh_sealed)
	.bind(tokens.expires_at)
	.bind(now)
	.execute(pool)
	.await
	.wrap_err("open a session")?;
	Ok(())
}

pub async fn get_session(pool: &PgPool, id_hash: &[u8]) -> Result<Option<SessionRow>> {
	let row = sqlx::query("SELECT user_id, access_sealed, access_expires_at, refresh_sealed, expires_at FROM cabinet.sessions WHERE id_hash = $1")
		.bind(id_hash)
		.fetch_optional(pool)
		.await
		.wrap_err("read a session")?;
	row.map(|row| {
		Ok(SessionRow {
			user_id: row.try_get("user_id")?,
			access_sealed: row.try_get("access_sealed")?,
			access_expires_at: row.try_get("access_expires_at")?,
			refresh_sealed: row.try_get("refresh_sealed")?,
			expires_at: row.try_get("expires_at")?,
		})
	})
	.transpose()
}

/// Take the lease on rotating a session whose access token is still the one `seen`: one
/// conditional UPDATE, so of two replicas exactly one asks concierge. A lease past its
/// deadline is taken over — its holder died mid-call.
pub async fn claim_rotation(pool: &PgPool, id_hash: &[u8], seen_access_expires_at: i64, now: i64, lease_until: i64) -> Result<bool> {
	let claimed = sqlx::query(
		"UPDATE cabinet.sessions SET rotating_until = $4
		 WHERE id_hash = $1 AND access_expires_at = $2 AND (rotating_until IS NULL OR rotating_until <= $3)",
	)
	.bind(id_hash)
	.bind(seen_access_expires_at)
	.bind(now)
	.bind(lease_until)
	.execute(pool)
	.await
	.wrap_err("claim a session rotation")?
	.rows_affected();
	Ok(claimed == 1)
}

/// Write the rotated pair, only over the pair the rotation started from. `false` when a
/// taken-over lease already wrote another. The session's deadline only ever shortens:
/// concierge slides the refresh token's expiry on every rotation, but the browser's cookie
/// dies at the deadline the callback gave it, and a row past that would be an orphan
/// holding a live refresh token (security review F2).
pub async fn set_tokens(pool: &PgPool, id_hash: &[u8], seen_access_expires_at: i64, tokens: &NewTokens<'_>) -> Result<bool> {
	let written = sqlx::query(
		"UPDATE cabinet.sessions
		 SET access_sealed = $3, access_expires_at = $4, refresh_sealed = $5, expires_at = LEAST(expires_at, $6), rotating_until = NULL
		 WHERE id_hash = $1 AND access_expires_at = $2",
	)
	.bind(id_hash)
	.bind(seen_access_expires_at)
	.bind(tokens.access_sealed)
	.bind(tokens.access_expires_at)
	.bind(tokens.refresh_sealed)
	.bind(tokens.expires_at)
	.execute(pool)
	.await
	.wrap_err("store a rotated session")?
	.rows_affected();
	Ok(written == 1)
}

pub async fn release_rotation(pool: &PgPool, id_hash: &[u8], seen_access_expires_at: i64) -> Result<()> {
	sqlx::query("UPDATE cabinet.sessions SET rotating_until = NULL WHERE id_hash = $1 AND access_expires_at = $2")
		.bind(id_hash)
		.bind(seen_access_expires_at)
		.execute(pool)
		.await
		.wrap_err("release a session rotation")?;
	Ok(())
}

/// Close a session concierge refused to rotate — unless another rotation already replaced
/// the pair the refusal was about.
pub async fn delete_refused(pool: &PgPool, id_hash: &[u8], seen_access_expires_at: i64) -> Result<()> {
	sqlx::query("DELETE FROM cabinet.sessions WHERE id_hash = $1 AND access_expires_at = $2")
		.bind(id_hash)
		.bind(seen_access_expires_at)
		.execute(pool)
		.await
		.wrap_err("close a refused session")?;
	Ok(())
}

/// Close a session; whose it was, or `None` when there was none.
pub async fn delete_session(pool: &PgPool, id_hash: &[u8]) -> Result<Option<Uuid>> {
	sqlx::query_scalar("DELETE FROM cabinet.sessions WHERE id_hash = $1 RETURNING user_id")
		.bind(id_hash)
		.fetch_optional(pool)
		.await
		.wrap_err("close a session")
}
