//! The cabinet's sign-in and its server-side sessions (S2-13).
//!
//! ```text
//! GET  /cabinet/api/auth/login     state + PKCE verifier → a `cabinet.sign_ins` row (keyed by
//!                                  the state's hash) and the state in an HttpOnly cookie
//!                                  → 302 concierge /api/auth/authorize?client_id=invest&…
//! GET  /cabinet/auth/callback      cookie state = URL state (constant time), the row taken
//!                                  once, ExchangeCode(code, verifier) → a session row, the
//!                                  session + CSRF cookies → 303 return_to
//! *    /cabinet/api/*              the session read by the hash of its cookie; its access
//!                                  token rotated when it is about to lapse
//! POST /cabinet/api/auth/logout    CSRF + Origin; the session closed, the cookies cleared
//! ```
//!
//! The browser holds a random session id and nothing else: the database holds its hash and
//! concierge's tokens, which never leave the server, sealed under a key derived from the
//! cookie ([`seal`]) — a database dump alone opens no session. The access token lives
//! fifteen minutes (concierge's access TTL, 900 s).

pub mod seal;
pub mod store;

use std::{
	collections::HashMap,
	sync::{Arc, Mutex, PoisonError},
	time::{Duration, Instant},
};

use color_eyre::eyre::{Report, eyre};
use evconcierge_client::{Authorize, ClientTokens, ConciergeError, Me, PkceVerifier, random_state};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use self::seal::{TokenKey, Which};
use crate::concierge::IdentityPlane;

/// How long a sign-in may take at concierge, Google and GitHub round trips included.
pub const SIGN_IN_TTL_SECS: i64 = 600;

/// An access token this close to its expiry is rotated before it is used: invest-core must
/// never be handed one that lapses in flight.
pub const REFRESH_AHEAD_SECS: i64 = 30;

/// A replica's lease on a rotation. Longer than a call to concierge (5 s), so it lapses
/// only when its holder is gone.
const ROTATION_LEASE_SECS: i64 = 15;

/// How long a request waits for another replica's rotation before answering 503.
const ROTATION_WAIT: Duration = Duration::from_secs(6);
const ROTATION_POLL: Duration = Duration::from_millis(100);

/// How long `GetMe` is trusted for the console gates: a grant revoked at concierge closes
/// the console at most this much later. invest-core re-checks every act against its own
/// projection, so this bounds a screen, not an action.
pub const ME_TTL: Duration = Duration::from_secs(60);

/// Names a session without being its cookie: the SHA-256 of the cookie's value. A database
/// leak yields hashes, not ids a browser could present.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SessionKey([u8; 32]);

impl SessionKey {
	/// `None` for a value that cannot be a session id, which then costs no query.
	pub fn of_cookie(value: &str) -> Option<Self> {
		if value.len() != 64 || !value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
			return None;
		}
		Some(Self(digest(b"invest-cabinet/session/v1/", value)))
	}

	pub(crate) fn as_bytes(&self) -> &[u8] {
		&self.0
	}
}

fn digest(domain: &[u8], value: &str) -> [u8; 32] {
	let mut h = Sha256::new();
	h.update(domain);
	h.update(value.as_bytes());
	h.finalize().into()
}

fn state_hash(state: &str) -> [u8; 32] {
	digest(b"invest-cabinet/sign-in/v1/", state)
}

/// 256 random bits as 64 lowercase hex characters: session ids and CSRF tokens. Inside the
/// frontend's `isSessionId` alphabet and length.
pub fn random_token() -> Result<String, Report> {
	let mut raw = [0u8; 32];
	getrandom::fill(&mut raw).map_err(|e| eyre!("the OS random source failed: {e}"))?;
	Ok(raw.iter().map(|b| format!("{b:02x}")).collect())
}

/// A live session: whose, and an access token good for at least [`REFRESH_AHEAD_SECS`].
pub struct Session {
	pub key: SessionKey,
	pub user_id: Uuid,
	pub access_token: String,
	pub access_expires_at: i64,
}

impl std::fmt::Debug for Session {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Session")
			.field("user_id", &self.user_id)
			.field("access_expires_at", &self.access_expires_at)
			.finish_non_exhaustive()
	}
}

/// A new session: the cookie values, shown to the browser once, and when the session ends.
pub struct Opened {
	pub cookie: String,
	pub csrf: String,
	pub expires_at: i64,
}

/// What the login hands the browser: the state for its cookie, and where to go.
pub struct Begun {
	pub state: String,
	pub authorize_url: String,
}

/// A redeemed sign-in: the verifier to present with the code, and where the user was.
pub struct Pending {
	pub verifier: PkceVerifier,
	pub return_to: String,
}

/// Why a request carries no usable session.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
	/// No such session, or one past its deadline: sign in.
	#[error("no session")]
	Missing,
	/// concierge refused the session's tokens; the session is closed.
	#[error("the session was refused by concierge")]
	Rejected,
	/// The tokens need concierge and it cannot be reached. Nothing is closed.
	#[error("concierge is unavailable")]
	Unavailable,
	#[error(transparent)]
	Internal(#[from] Report),
}

/// Where the sign-in endpoints meet concierge and the `cabinet` tables.
pub struct Sessions {
	pool: PgPool,
	concierge: Arc<dyn IdentityPlane>,
	/// One rotation per session at a time in this process; across replicas the row's lease.
	rotations: Mutex<HashMap<SessionKey, Arc<tokio::sync::Mutex<()>>>>,
	me: Mutex<HashMap<SessionKey, (Instant, Me)>>,
}

impl Sessions {
	pub fn new(pool: PgPool, concierge: Arc<dyn IdentityPlane>) -> Self {
		Self {
			pool,
			concierge,
			rotations: Mutex::default(),
			me: Mutex::default(),
		}
	}

	pub fn pool(&self) -> &PgPool {
		&self.pool
	}

	pub fn concierge(&self) -> &dyn IdentityPlane {
		self.concierge.as_ref()
	}

	/// A fresh state and PKCE verifier, recorded for the callback, and the authorize URL.
	pub async fn begin(&self, authorize: SignInTarget<'_>, return_to: &str, now: i64) -> Result<Begun, Report> {
		let verifier = PkceVerifier::generate()?;
		let state = random_state()?;
		store::insert_sign_in(&self.pool, &state_hash(&state), verifier.as_str(), return_to, now + SIGN_IN_TTL_SECS).await?;
		let authorize_url = Authorize {
			concierge_origin: authorize.concierge_origin,
			client_id: authorize.client_id,
			redirect_uri: authorize.redirect_uri,
			state: &state,
			verifier: &verifier,
			prompt: None,
		}
		.url();
		Ok(Begun { state, authorize_url })
	}

	/// Redeem the sign-in `state` names, once. The caller has already compared it with the
	/// browser's cookie; this is the half that holds across replicas and replays.
	pub async fn redeem(&self, state: &str, now: i64) -> Result<Option<Pending>, Report> {
		Ok(store::take_sign_in(&self.pool, &state_hash(state), now).await?.map(|row| Pending {
			verifier: PkceVerifier::from_stored(row.verifier),
			return_to: row.return_to,
		}))
	}

	/// Open a session for the pair concierge just issued. Whatever is past its deadline —
	/// sessions and abandoned sign-ins — goes on the way: here, behind a code concierge
	/// issued, and not in the login, which anyone can call (security review F1).
	pub async fn open(&self, tokens: &ClientTokens, now: i64) -> Result<Opened, Report> {
		let pruned = store::prune(&self.pool, now).await?;
		if pruned > 0 {
			tracing::debug!(pruned, "dropped expired sign-ins and sessions");
		}
		let cookie = random_token()?;
		let key = SessionKey::of_cookie(&cookie).ok_or_else(|| eyre!("a fresh session id is not one"))?;
		let (access, refresh) = seal_pair(&TokenKey::of_cookie(&cookie)?, &key, tokens)?;
		store::insert_session(&self.pool, key.as_bytes(), tokens.user_id, &new_tokens(&access, &refresh, tokens), now).await?;
		Ok(Opened {
			cookie,
			csrf: random_token()?,
			expires_at: tokens.refresh_expires_at,
		})
	}

	/// The session a cookie names, its access token rotated first when it is about to lapse.
	///
	/// A refresh token is presented to concierge once — twice, and concierge revokes the
	/// family — so rotations are serialised: within this process by a lock per session,
	/// across replicas by a lease on the row that only one conditional UPDATE can take. The
	/// rotated pair is written only over the pair the rotation started from. If an answer is
	/// lost after concierge rotated, the next rotation presents a spent token and the
	/// session closes: it fails closed, never open.
	pub async fn resolve(&self, cookie: &str, now: i64) -> Result<Session, SessionError> {
		let key = SessionKey::of_cookie(cookie).ok_or(SessionError::Missing)?;
		let tokens = TokenKey::of_cookie(cookie)?;
		let row = self.live_row(&key, now).await?;
		if row.access_expires_at > now + REFRESH_AHEAD_SECS {
			return self.live(key, &tokens, row).await;
		}
		let flight = {
			let mut map = self.rotations.lock().unwrap_or_else(PoisonError::into_inner);
			map.entry(key).or_default().clone()
		};
		let turn = flight.lock().await;
		let result = self.rotate(key, &tokens, now).await;
		drop(turn);
		{
			let mut map = self.rotations.lock().unwrap_or_else(PoisonError::into_inner);
			// The map's clone and this one: nobody else is in line.
			if Arc::strong_count(&flight) == 2 {
				map.remove(&key);
			}
		}
		result
	}

	async fn live_row(&self, key: &SessionKey, now: i64) -> Result<store::SessionRow, SessionError> {
		let row = store::get_session(&self.pool, key.as_bytes()).await?.ok_or(SessionError::Missing)?;
		if row.expires_at <= now {
			store::delete_session(&self.pool, key.as_bytes()).await?;
			return Err(SessionError::Missing);
		}
		Ok(row)
	}

	/// The session of a live row, its access token opened. A row that does not open under
	/// the cookie that names it was altered, or written by something else: it is closed,
	/// never served.
	async fn live(&self, key: SessionKey, tokens: &TokenKey, row: store::SessionRow) -> Result<Session, SessionError> {
		match tokens.open(Which::Access, key.as_bytes(), &row.access_sealed) {
			Ok(access_token) => Ok(Session {
				key,
				user_id: row.user_id,
				access_token,
				access_expires_at: row.access_expires_at,
			}),
			Err(err) => Err(self.unreadable(&key, err).await),
		}
	}

	async fn unreadable(&self, key: &SessionKey, err: Report) -> SessionError {
		tracing::error!(error = ?err, "a session row does not open under its own cookie; closed");
		match self.close(key).await {
			Ok(_) => SessionError::Missing,
			Err(close) => SessionError::Internal(close),
		}
	}

	async fn rotate(&self, key: SessionKey, tokens: &TokenKey, now: i64) -> Result<Session, SessionError> {
		let waited = Instant::now();
		let row = loop {
			let row = self.live_row(&key, now).await?;
			if row.access_expires_at > now + REFRESH_AHEAD_SECS {
				// Rotated while this request waited: here, or by another replica.
				return self.live(key, tokens, row).await;
			}
			if store::claim_rotation(&self.pool, key.as_bytes(), row.access_expires_at, now, now + ROTATION_LEASE_SECS).await? {
				break row;
			}
			if waited.elapsed() >= ROTATION_WAIT {
				tracing::warn!(user_id = %row.user_id, "another replica holds the session's rotation");
				return Err(SessionError::Unavailable);
			}
			tokio::time::sleep(ROTATION_POLL).await;
		};
		let seen = row.access_expires_at;
		let refresh_token = match tokens.open(Which::Refresh, key.as_bytes(), &row.refresh_sealed) {
			Ok(token) => token,
			Err(err) => return Err(self.unreadable(&key, err).await),
		};
		match self.concierge.refresh(&refresh_token).await {
			Ok(rotated) => {
				if rotated.user_id != row.user_id {
					store::delete_refused(&self.pool, key.as_bytes(), seen).await?;
					return Err(SessionError::Internal(eyre!("concierge rotated a session into another user")));
				}
				let (access, refresh) = seal_pair(tokens, &key, &rotated)?;
				if !store::set_tokens(&self.pool, key.as_bytes(), seen, &new_tokens(&access, &refresh, &rotated)).await? {
					// The lease was taken over while concierge answered: that holder's pair,
					// from the same refresh token, is the one concierge keeps.
					tracing::warn!(user_id = %row.user_id, "a session was rotated twice; keeping the row's pair");
					return Err(SessionError::Unavailable);
				}
				Ok(Session {
					key,
					user_id: row.user_id,
					access_token: rotated.access_token,
					access_expires_at: rotated.access_expires_at,
				})
			}
			Err(ConciergeError::Refused { code }) => {
				store::delete_refused(&self.pool, key.as_bytes(), seen).await?;
				self.forget_me(&key);
				tracing::info!(user_id = %row.user_id, ?code, "concierge refused a session refresh; session closed");
				Err(SessionError::Rejected)
			}
			Err(err) => {
				store::release_rotation(&self.pool, key.as_bytes(), seen).await?;
				if err.is_transient() {
					tracing::warn!(user_id = %row.user_id, error = %err, "concierge unavailable for a session refresh");
					Err(SessionError::Unavailable)
				} else {
					Err(SessionError::Internal(Report::new(err).wrap_err("rotating a session")))
				}
			}
		}
	}

	/// `GetMe` for a resolved session — cached for [`ME_TTL`] unless `fresh`. A refusal is
	/// concierge saying the token is dead (signed out on evinvest.ltd, held, disabled): the
	/// session closes here too. Asked once more on a timeout: one slow answer should not be
	/// a 503 for the user.
	pub async fn me(&self, session: &Session, fresh: bool) -> Result<Me, SessionError> {
		if !fresh && let Some(me) = self.cached_me(&session.key) {
			return Ok(me);
		}
		let answer = match self.concierge.me(&session.access_token).await {
			Err(err) if err.is_transient() => self.concierge.me(&session.access_token).await,
			answer => answer,
		};
		match answer {
			Ok(me) if me.user_id == session.user_id => {
				self.remember_me(session.key, me.clone());
				Ok(me)
			}
			Ok(_) => Err(SessionError::Internal(eyre!("GetMe answered another user than the session's"))),
			Err(ConciergeError::Refused { code }) => {
				tracing::info!(user_id = %session.user_id, ?code, "GetMe refused; session closed");
				self.close(&session.key).await?;
				Err(SessionError::Rejected)
			}
			Err(err) if err.is_transient() => {
				tracing::warn!(error = %err, "concierge unavailable for GetMe");
				Err(SessionError::Unavailable)
			}
			Err(err) => Err(SessionError::Internal(Report::new(err).wrap_err("GetMe"))),
		}
	}

	/// Close a session; whose it was, or `None` when there was none.
	pub async fn close(&self, key: &SessionKey) -> Result<Option<Uuid>, Report> {
		self.forget_me(key);
		store::delete_session(&self.pool, key.as_bytes()).await
	}

	fn cached_me(&self, key: &SessionKey) -> Option<Me> {
		let cache = self.me.lock().unwrap_or_else(PoisonError::into_inner);
		cache.get(key).filter(|(at, _)| at.elapsed() < ME_TTL).map(|(_, me)| me.clone())
	}

	fn remember_me(&self, key: SessionKey, me: Me) {
		let mut cache = self.me.lock().unwrap_or_else(PoisonError::into_inner);
		// Bounded without a sweeper: stale entries go whenever the map has grown.
		if cache.len() >= 1024 {
			cache.retain(|_, (at, _)| at.elapsed() < ME_TTL);
		}
		cache.insert(key, (Instant::now(), me));
	}

	fn forget_me(&self, key: &SessionKey) {
		self.me.lock().unwrap_or_else(PoisonError::into_inner).remove(key);
	}
}

/// The fixed half of an authorize request: who signs in, and where concierge returns.
pub struct SignInTarget<'a> {
	pub concierge_origin: &'a str,
	pub client_id: &'a str,
	pub redirect_uri: &'a str,
}

fn seal_pair(key: &TokenKey, session: &SessionKey, tokens: &ClientTokens) -> Result<(Vec<u8>, Vec<u8>), Report> {
	Ok((
		key.seal(Which::Access, session.as_bytes(), &tokens.access_token)?,
		key.seal(Which::Refresh, session.as_bytes(), &tokens.refresh_token)?,
	))
}

fn new_tokens<'a>(access: &'a [u8], refresh: &'a [u8], tokens: &ClientTokens) -> store::NewTokens<'a> {
	store::NewTokens {
		access_sealed: access,
		access_expires_at: tokens.access_expires_at,
		refresh_sealed: refresh,
		expires_at: tokens.refresh_expires_at,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn only_a_session_shaped_cookie_names_a_session() {
		let id = random_token().unwrap();
		assert_eq!(id.len(), 64);
		assert!(SessionKey::of_cookie(&id).is_some());
		assert!(SessionKey::of_cookie(&id.to_uppercase()).is_none(), "ids are lowercase hex");
		assert!(SessionKey::of_cookie("short").is_none());
		assert!(SessionKey::of_cookie(&format!("{}g", &id[..63])).is_none());
	}

	#[test]
	fn the_key_is_not_the_cookie() {
		let id = random_token().unwrap();
		let key = SessionKey::of_cookie(&id).unwrap();
		assert_ne!(key.as_bytes(), id.as_bytes());
		assert_eq!(key, SessionKey::of_cookie(&id).unwrap(), "the same cookie names the same row");
	}
}
