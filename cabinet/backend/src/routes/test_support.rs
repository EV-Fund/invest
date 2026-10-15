//! What every route test stands up: a real Postgres for the session store (one database per
//! test binary, migrated once), a fake concierge behind the [`IdentityPlane`] port, and a
//! stub invest-core the calling module serves over real gRPC.
//!
//! A missing `DATABASE_URL` skips a suite locally and fails it under `CI=true` — a green
//! run that touched no database is what CI exists to catch.

// Stubs answer `tonic::Status`, a large error type tonic mandates.
#![allow(clippy::result_large_err)]

use std::{
	collections::HashMap,
	net::SocketAddr,
	sync::{Arc, Mutex},
	time::Duration,
};

use async_trait::async_trait;
use axum::{
	Router,
	body::Body,
	http::{Request, StatusCode, header},
};
use evconcierge_client::{ClientTokens, ConciergeError, Me, PkceVerifier};
use serde_json::Value;
use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use tokio::{net::TcpListener, sync::OnceCell};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Code;
use tower::ServiceExt;
use uuid::Uuid;

use crate::{
	concierge::IdentityPlane,
	config::AppConfig,
	cookies::CookieNames,
	db, now_secs,
	routes::router,
	session::Sessions,
	state::{AppState, Grpc},
};

/// The cabinet's origin in every test, as production configures its own.
pub const ORIGIN: &str = "http://invest.test";
/// The access token a seeded session holds; the stub invest-core accepts nothing else.
pub const ACCESS_TOKEN: &str = "invest-access-token";
pub const CSRF: &str = "csrf-token-value";
/// The signed-in user of a seeded session.
pub const USER: Uuid = Uuid::from_u128(0x0a1b_2c3d_4e5f_4a6b_8c7d_9e0f_1a2b_3c4d);
pub const INVESTOR: &[&str] = &["invest:self:invest"];
pub const MANAGER: &[&str] = &["invest:self:invest", "invest:allocation:manage", "invest:valuation:post"];

fn env(core: &str, database_url: &str) -> impl Fn(&str) -> Option<String> {
	let (core, database_url) = (core.to_owned(), database_url.to_owned());
	move |var: &str| -> Option<String> {
		Some(match var {
			"DATABASE_URL" => database_url.clone(),
			"INVEST_CORE_GRPC_ADDR" => core.clone(),
			"CONCIERGE_GRPC_ADDR" => "http://127.0.0.1:1".into(),
			"CONCIERGE_CLIENT_SECRET" => "test-secret".into(),
			"CONCIERGE_PUBLIC_ORIGIN" => "https://concierge.test".into(),
			"CABINET_PUBLIC_ORIGIN" => ORIGIN.into(),
			"APP_ENV" => "development".into(),
			_ => return None,
		})
	}
}

fn state(core: &str, pool: PgPool, concierge: Arc<FakeConcierge>, database_url: &str) -> AppState {
	let config = AppConfig::from_source(env(core, database_url)).expect("the test env loads");
	AppState {
		cookies: Arc::new(CookieNames::new(config.cookie_secure())),
		sessions: Arc::new(Sessions::new(pool, concierge)),
		grpc: Grpc::connect_lazy(core).expect("build the lazy channel"),
		config: Arc::new(config),
	}
}

/// State whose every dependency is a black hole and dialled only when a handler asks: the
/// probe tests' proof that the probe never asks.
pub fn unreachable_state() -> AppState {
	const BLACK_HOLE: &str = "http://127.0.0.1:1";
	const NO_DATABASE: &str = "postgres://postgres@127.0.0.1:1/none";
	let pool = PgPoolOptions::new()
		.acquire_timeout(Duration::from_secs(1))
		.connect_lazy(NO_DATABASE)
		.expect("a lazy pool over a well-formed URL");
	state(BLACK_HOLE, pool, Arc::new(FakeConcierge::default()), NO_DATABASE)
}

// ── the fake concierge ──────────────────────────────────────────────────────────

/// How the fake answers `RefreshClientToken`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Refresh {
	#[default]
	Rotate,
	Refuse,
	Unavailable,
}

#[derive(Default)]
struct Fake {
	permissions: Vec<String>,
	refresh: Refresh,
	me_refused: bool,
	/// The challenge the authorize URL carried; an exchange whose verifier does not answer
	/// it is refused, as concierge does.
	challenge: Option<String>,
	exchanges: usize,
	refreshes: usize,
	me_calls: usize,
}

/// concierge as the BFF sees it through [`IdentityPlane`]. Every token it issues belongs to
/// [`USER`]; access tokens are numbered so a rotation is visible.
#[derive(Default)]
pub struct FakeConcierge(Mutex<Fake>);

impl FakeConcierge {
	fn with<T>(&self, f: impl FnOnce(&mut Fake) -> T) -> T {
		f(&mut self.0.lock().expect("the fake's lock is never poisoned"))
	}

	pub fn grant(&self, permissions: &[&str]) {
		self.with(|f| f.permissions = permissions.iter().map(|p| (*p).to_owned()).collect());
	}

	pub fn on_refresh(&self, refresh: Refresh) {
		self.with(|f| f.refresh = refresh);
	}

	pub fn refuse_me(&self) {
		self.with(|f| f.me_refused = true);
	}

	pub fn expect_challenge(&self, challenge: &str) {
		self.with(|f| f.challenge = Some(challenge.to_owned()));
	}

	pub fn exchanges(&self) -> usize {
		self.with(|f| f.exchanges)
	}

	pub fn refreshes(&self) -> usize {
		self.with(|f| f.refreshes)
	}

	pub fn me_calls(&self) -> usize {
		self.with(|f| f.me_calls)
	}
}

fn issued(access_token: String) -> ClientTokens {
	ClientTokens {
		user_id: USER,
		access_token,
		access_expires_at: now_secs() + 300,
		refresh_token: format!("refresh-{}", Uuid::new_v4()),
		refresh_expires_at: now_secs() + 86_400,
	}
}

#[async_trait]
impl IdentityPlane for FakeConcierge {
	async fn exchange_code(&self, code: &str, _redirect_uri: &str, verifier: &PkceVerifier) -> Result<ClientTokens, ConciergeError> {
		let n = self.with(|f| {
			f.exchanges += 1;
			match &f.challenge {
				Some(expected) if *expected != verifier.challenge() => Err(ConciergeError::Rejected {
					code: Code::InvalidArgument,
					message: "code_verifier does not match".into(),
				}),
				_ => Ok(f.exchanges),
			}
		})?;
		if code == "spent" {
			return Err(ConciergeError::Refused { code: Code::Unauthenticated });
		}
		Ok(issued(format!("{ACCESS_TOKEN}-exchanged-{n}")))
	}

	async fn refresh(&self, _refresh_token: &str) -> Result<ClientTokens, ConciergeError> {
		let (refresh, n) = self.with(|f| {
			f.refreshes += 1;
			(f.refresh, f.refreshes)
		});
		match refresh {
			Refresh::Rotate => Ok(issued(format!("{ACCESS_TOKEN}-rotated-{n}"))),
			Refresh::Refuse => Err(ConciergeError::Refused { code: Code::Unauthenticated }),
			Refresh::Unavailable => Err(ConciergeError::Unavailable("concierge is down".into())),
		}
	}

	async fn me(&self, _access_token: &str) -> Result<Me, ConciergeError> {
		self.with(|f| {
			f.me_calls += 1;
			if f.me_refused {
				return Err(ConciergeError::Refused { code: Code::Unauthenticated });
			}
			Ok(Me {
				user_id: USER,
				email: "investor@example.test".into(),
				email_verified: true,
				username: String::new(),
				preferred_name: String::new(),
				status: "active".into(),
				token_version: 0,
				permissions: f.permissions.clone(),
			})
		})
	}
}

// ── Postgres ────────────────────────────────────────────────────────────────────

fn ci() -> bool {
	std::env::var("CI").is_ok_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
}

/// This binary's database, provisioned once: `<DATABASE_URL's database>_cabinet`, dropped
/// and re-created at the start of each run, then migrated. `None` (a skip) when
/// `DATABASE_URL` is unset locally; a panic under CI.
pub async fn database_url() -> Option<String> {
	let Some(base) = std::env::var("DATABASE_URL").ok().filter(|s| !s.is_empty()) else {
		assert!(!ci(), "DATABASE_URL is required in CI — run `nix run .#db` and export it (or unset CI to skip locally)");
		eprintln!("DATABASE_URL unset — skipping a cabinet suite");
		return None;
	};
	// A tokio `OnceCell` can await inside `get_or_init` and is runtime-agnostic: each
	// `#[tokio::test]` runs on its own runtime, and they all share this cell. Only the URL
	// is shared — a pool belongs to the runtime it was opened on.
	static URL: OnceCell<String> = OnceCell::const_new();
	Some(URL.get_or_init(|| provision(base)).await.clone())
}

async fn provision(base: String) -> String {
	let (server, name) = base.rsplit_once('/').expect("DATABASE_URL names a database");
	let name = name.split('?').next().unwrap_or(name);
	assert!(
		!name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
		"DATABASE_URL must name a plain lowercase database (got {name:?}): it is spliced into DDL"
	);
	let clone = format!("{name}_cabinet");
	let mut admin = PgConnection::connect(&format!("{server}/postgres")).await.expect("connect to the server's postgres database");
	sqlx::query(AssertSqlSafe(format!("DROP DATABASE IF EXISTS {clone} WITH (FORCE)")))
		.execute(&mut admin)
		.await
		.expect("drop the previous run's database");
	sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {clone}")))
		.execute(&mut admin)
		.await
		.expect("create this run's database");
	let url = format!("{server}/{clone}");
	let pool = PgPoolOptions::new().max_connections(1).connect(&url).await.expect("connect to this run's database");
	db::migrate(&pool).await.expect("apply the cabinet migrations");
	pool.close().await;
	url
}

// ── the harness ─────────────────────────────────────────────────────────────────

/// Serve a stub invest-core on an ephemeral port, handing tonic the listener already held
/// so a sibling test cannot be given the same port.
pub async fn serve_core(router: tonic::transport::server::Router) -> SocketAddr {
	let listener = TcpListener::bind("127.0.0.1:0").await.expect("claim an ephemeral port");
	let addr = listener.local_addr().expect("the listener has an address");
	tokio::spawn(async move {
		router.serve_with_incoming(TcpListenerStream::new(listener)).await.expect("the stub invest-core serves");
	});
	addr
}

pub struct Harness {
	pub app: Router,
	pub state: AppState,
	pub concierge: Arc<FakeConcierge>,
}

impl Harness {
	/// The real router over a real `AppState`: Postgres, the fake concierge, and invest-core
	/// at `core` (a stub, or a black hole for suites that never reach it). `None` when the
	/// suite is skipped.
	pub async fn new(core: Option<SocketAddr>) -> Option<Self> {
		let url = database_url().await?;
		let pool = PgPoolOptions::new().max_connections(4).connect(&url).await.expect("connect to the test database");
		let concierge = Arc::new(FakeConcierge::default());
		let core = core.map_or_else(|| "http://127.0.0.1:1".to_owned(), |addr| format!("http://{addr}"));
		let state = state(&core, pool, concierge.clone(), &url);
		Some(Self {
			app: router(state.clone()),
			state,
			concierge,
		})
	}

	/// A session for [`USER`] holding `permissions`, opened as the callback would. Its
	/// cookie header carries the session and the CSRF cookie [`CSRF`].
	pub async fn sign_in(&self, permissions: &[&str]) -> Signed {
		self.sign_in_with(permissions, now_secs() + 300).await
	}

	/// [`Self::sign_in`] with the access token expiring at `access_expires_at`.
	pub async fn sign_in_with(&self, permissions: &[&str], access_expires_at: i64) -> Signed {
		self.concierge.grant(permissions);
		let tokens = ClientTokens {
			user_id: USER,
			access_token: ACCESS_TOKEN.into(),
			access_expires_at,
			refresh_token: format!("refresh-{}", Uuid::new_v4()),
			refresh_expires_at: now_secs() + 86_400,
		};
		let opened = self.state.sessions.open(&tokens, now_secs()).await.expect("open a session");
		Signed {
			cookie: format!("invest_session={}; invest_csrf={CSRF}", opened.cookie),
			session: opened.cookie,
		}
	}

	/// Send a request through the router; the status, the JSON body (`Null` when there is
	/// none) and the response headers.
	pub async fn send(&self, request: Request<Body>) -> (StatusCode, Value, axum::http::HeaderMap) {
		let response = self.app.clone().oneshot(request).await.expect("the router responds");
		let status = response.status();
		let headers = response.headers().clone();
		let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.expect("read the body");
		(status, serde_json::from_slice(&bytes).unwrap_or(Value::Null), headers)
	}

	/// How many session rows the store holds for the given cookie (0 or 1).
	pub async fn session_rows(&self, cookie: &str) -> i64 {
		let key = crate::session::SessionKey::of_cookie(cookie).expect("a session-shaped cookie");
		sqlx::query_scalar("SELECT count(*) FROM cabinet.sessions WHERE id_hash = $1")
			.bind(key.as_bytes())
			.fetch_one(self.state.sessions.pool())
			.await
			.expect("count the session's rows")
	}
}

pub struct Signed {
	/// The `Cookie` header a browser with this session sends.
	pub cookie: String,
	/// The session cookie's value alone.
	pub session: String,
}

/// A request as the cabinet's page sends it: the session cookie, its `Origin`, and — for a
/// mutation — the CSRF header matching the cookie.
pub fn request(method: &str, uri: &str, signed: Option<&Signed>, body: Option<&str>, csrf: bool) -> Request<Body> {
	let mut builder = Request::builder().method(method).uri(uri).header(header::ORIGIN, ORIGIN);
	if let Some(signed) = signed {
		builder = builder.header(header::COOKIE, &signed.cookie);
	}
	if csrf {
		builder = builder.header("x-ev-csrf", CSRF);
	}
	match body {
		Some(json) => builder.header(header::CONTENT_TYPE, "application/json").body(Body::from(json.to_owned())),
		None => builder.body(Body::empty()),
	}
	.expect("build the request")
}

/// The `authorization` bearer the stub invest-core accepts — a seeded session's token.
pub fn bearer_ok<T>(request: &tonic::Request<T>) -> bool {
	request.metadata().get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {ACCESS_TOKEN}"))
}

/// Every `Set-Cookie` of a response, by name.
pub fn set_cookies(headers: &axum::http::HeaderMap) -> HashMap<String, String> {
	headers
		.get_all(header::SET_COOKIE)
		.iter()
		.filter_map(|v| v.to_str().ok())
		.filter_map(|c| c.split(';').next()?.split_once('=').map(|(n, v)| (n.to_owned(), v.to_owned())))
		.collect()
}
