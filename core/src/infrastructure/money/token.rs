//! The `typ=service` bearer invest sends to banking's scope listener, minted on demand.
//!
//! banking issues it over the same mTLS channel (`scope.v1.ScopeAuthService/MintServiceToken`,
//! an empty request, the client proven by its certificate alone) and it lives a few minutes,
//! so [`TokenSource`] caches one and mints again when less than [`REFRESH_MARGIN_SECS`]
//! remain. Callers that find the cache stale queue on one lock: exactly one mint is in
//! flight, and the others read its result (single-flight).
//!
//! Only the mint is retried. A mint changes nothing in banking's books, so asking again is
//! safe; the money calls that carry the token (Charge, Transfer, Payout …) are never retried
//! here — they are retried by their sagas, under their idempotency keys.
//!
//! The token is a credential: it is never formatted into an error, a log line or `Debug`.

use std::{fmt, sync::Arc, time::Duration};

use async_trait::async_trait;
use evbanking_contracts::scope::v1::{MintServiceTokenRequest, scope_auth_service_client::ScopeAuthServiceClient};
use tokio::{sync::Mutex, time::Instant};
use tonic::{Code, Request, Status, metadata::MetadataValue, transport::Channel};

/// Mint again when the cached token has less than this left. banking's TTL is 300 s, so the
/// margin also absorbs a clock that differs by seconds between the two hosts.
pub const REFRESH_MARGIN_SECS: i64 = 60;

/// Attempts at one mint, and the pause before the second (doubled for each further one).
const MINT_ATTEMPTS: u32 = 3;
const MINT_BACKOFF: Duration = Duration::from_millis(250);

/// After a mint has failed for good, callers within this window fail at once instead of
/// each queueing for its own series of attempts against a banking that is down.
const FAILURE_COOLDOWN: Duration = Duration::from_secs(1);

/// Deadline on one mint call, so a hung banking cannot hold the lock (and every caller).
const MINT_TIMEOUT: Duration = Duration::from_secs(5);

/// A token and the unix second it stops being valid.
#[derive(Clone)]
pub struct MintedToken {
	pub token: String,
	pub expires_at: i64,
}

impl fmt::Debug for MintedToken {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("MintedToken").field("token", &"<redacted>").field("expires_at", &self.expires_at).finish()
	}
}

/// Where tokens come from: banking's `MintServiceToken` in production, a fake in tests.
#[async_trait]
pub trait TokenMinter: Send + Sync {
	async fn mint(&self) -> Result<MintedToken, Status>;
}

/// The unix clock, a port so a test can walk a token to its expiry.
pub trait UnixClock: Send + Sync {
	fn now_unix(&self) -> i64;
}

pub struct HostClock;

impl UnixClock for HostClock {
	fn now_unix(&self) -> i64 {
		crate::infrastructure::now_unix()
	}
}

/// Why no bearer could be had. The text carries the status code and banking's message,
/// never the token.
#[derive(Debug, thiserror::Error)]
#[error("service token unavailable: {0}")]
pub struct TokenError(String);

enum State {
	Empty,
	Held(MintedToken),
}

struct Cache {
	state: State,
	/// When the last mint gave up, and why.
	failed: Option<(Instant, String)>,
}

pub struct TokenSource {
	minter: Arc<dyn TokenMinter>,
	clock: Arc<dyn UnixClock>,
	cache: Mutex<Cache>,
}

impl TokenSource {
	pub fn new(minter: Arc<dyn TokenMinter>, clock: Arc<dyn UnixClock>) -> Self {
		Self {
			minter,
			clock,
			cache: Mutex::new(Cache { state: State::Empty, failed: None }),
		}
	}

	/// The production source: banking's `ScopeAuthService` on the scope ledger's channel.
	pub fn over_channel(channel: Channel) -> Self {
		Self::new(
			Arc::new(GrpcMinter {
				client: ScopeAuthServiceClient::new(channel),
			}),
			Arc::new(HostClock),
		)
	}

	/// The `authorization` header value for one call: the cached token while it has at
	/// least [`REFRESH_MARGIN_SECS`] left, a fresh one otherwise.
	pub async fn bearer(&self) -> Result<MetadataValue<tonic::metadata::Ascii>, TokenError> {
		let mut cache = self.cache.lock().await;
		let now = self.clock.now_unix();
		if let State::Held(held) = &cache.state
			&& held.expires_at - now >= REFRESH_MARGIN_SECS
		{
			return header(&held.token);
		}
		if let Some((at, reason)) = &cache.failed
			&& at.elapsed() < FAILURE_COOLDOWN
		{
			// A token inside its margin still opens the door until it really expires.
			if let State::Held(held) = &cache.state
				&& held.expires_at > now
			{
				return header(&held.token);
			}
			return Err(TokenError(reason.clone()));
		}
		match self.mint_with_retry().await {
			Ok(minted) => {
				let value = header(&minted.token)?;
				cache.state = State::Held(minted);
				cache.failed = None;
				Ok(value)
			}
			Err(reason) => {
				tracing::warn!(%reason, "scope ledger service token mint failed");
				cache.failed = Some((Instant::now(), reason.clone()));
				if let State::Held(held) = &cache.state
					&& held.expires_at > now
				{
					return header(&held.token);
				}
				Err(TokenError(reason))
			}
		}
	}

	async fn mint_with_retry(&self) -> Result<MintedToken, String> {
		let mut backoff = MINT_BACKOFF;
		let mut attempt = 1;
		loop {
			let failure = match self.minter.mint().await {
				Ok(minted) if minted.token.is_empty() => return Err("MintServiceToken answered an empty token".into()),
				Ok(minted) => return Ok(minted),
				Err(status) => status,
			};
			// UNAUTHENTICATED is "this certificate maps to no client": asking again cannot help.
			let transient = matches!(
				failure.code(),
				Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled | Code::ResourceExhausted | Code::Aborted | Code::Internal | Code::Unknown
			);
			if !transient || attempt >= MINT_ATTEMPTS {
				return Err(format!("MintServiceToken: {:?}: {}", failure.code(), failure.message()));
			}
			tokio::time::sleep(backoff).await;
			backoff *= 2;
			attempt += 1;
		}
	}
}

fn header(token: &str) -> Result<MetadataValue<tonic::metadata::Ascii>, TokenError> {
	// The error must not echo the token: it is a credential, and a malformed one is
	// banking's bug to report, not ours to print.
	MetadataValue::try_from(format!("Bearer {token}")).map_err(|_| TokenError("MintServiceToken answered a token that is not a valid header value".into()))
}

struct GrpcMinter {
	client: ScopeAuthServiceClient<Channel>,
}

#[async_trait]
impl TokenMinter for GrpcMinter {
	async fn mint(&self) -> Result<MintedToken, Status> {
		let mut request = Request::new(MintServiceTokenRequest {});
		request.set_timeout(MINT_TIMEOUT);
		let response = self.client.clone().mint_service_token(request).await?.into_inner();
		Ok(MintedToken {
			token: response.token,
			expires_at: response.expires_at,
		})
	}
}

#[cfg(test)]
mod tests {
	use std::{
		collections::VecDeque,
		sync::{
			Mutex as StdMutex,
			atomic::{AtomicI64, AtomicU32, Ordering},
		},
	};

	use super::*;

	struct ManualClock(AtomicI64);

	impl UnixClock for ManualClock {
		fn now_unix(&self) -> i64 {
			self.0.load(Ordering::SeqCst)
		}
	}

	/// Answers from a script, one entry per mint call; the last entry repeats.
	struct ScriptedMinter {
		calls: AtomicU32,
		script: StdMutex<VecDeque<Result<MintedToken, Status>>>,
		/// How long a mint takes, so concurrent callers really overlap.
		latency: Duration,
	}

	impl ScriptedMinter {
		fn new(script: Vec<Result<MintedToken, Status>>, latency: Duration) -> Arc<Self> {
			Arc::new(Self {
				calls: AtomicU32::new(0),
				script: StdMutex::new(script.into()),
				latency,
			})
		}

		fn calls(&self) -> u32 {
			self.calls.load(Ordering::SeqCst)
		}
	}

	#[async_trait]
	impl TokenMinter for ScriptedMinter {
		async fn mint(&self) -> Result<MintedToken, Status> {
			self.calls.fetch_add(1, Ordering::SeqCst);
			tokio::time::sleep(self.latency).await;
			let mut script = self.script.lock().unwrap();
			if script.len() > 1 {
				script.pop_front().unwrap()
			} else {
				script.front().cloned().unwrap_or_else(|| Err(Status::internal("empty script")))
			}
		}
	}

	fn token(name: &str, expires_at: i64) -> Result<MintedToken, Status> {
		Ok(MintedToken { token: name.into(), expires_at })
	}

	fn source(minter: &Arc<ScriptedMinter>, now: i64) -> (Arc<TokenSource>, Arc<ManualClock>) {
		let clock = Arc::new(ManualClock(AtomicI64::new(now)));
		(Arc::new(TokenSource::new(minter.clone(), clock.clone())), clock)
	}

	async fn bearer(source: &TokenSource) -> String {
		source.bearer().await.unwrap().to_str().unwrap().to_owned()
	}

	#[tokio::test(start_paused = true)]
	async fn a_token_is_reused_until_the_margin_and_then_minted_again() {
		let minter = ScriptedMinter::new(vec![token("t1", 1_300), token("t2", 1_600)], Duration::ZERO);
		let (source, clock) = source(&minter, 1_000);

		assert_eq!(bearer(&source).await, "Bearer t1");
		assert_eq!(bearer(&source).await, "Bearer t1");
		assert_eq!(minter.calls(), 1);

		// Exactly the margin left: not yet less than it.
		clock.0.store(1_240, Ordering::SeqCst);
		assert_eq!(bearer(&source).await, "Bearer t1");
		assert_eq!(minter.calls(), 1);

		// 59 s left: inside it.
		clock.0.store(1_241, Ordering::SeqCst);
		assert_eq!(bearer(&source).await, "Bearer t2");
		assert_eq!(minter.calls(), 2);
	}

	#[tokio::test(start_paused = true)]
	async fn concurrent_callers_share_one_mint() {
		let minter = ScriptedMinter::new(vec![token("t1", 1_300)], Duration::from_millis(100));
		let (source, _) = source(&minter, 1_000);

		let mut tasks = tokio::task::JoinSet::new();
		for _ in 0..16 {
			let source = source.clone();
			tasks.spawn(async move { bearer(&source).await });
		}
		while let Some(done) = tasks.join_next().await {
			assert_eq!(done.unwrap(), "Bearer t1");
		}
		assert_eq!(minter.calls(), 1);
	}

	#[tokio::test(start_paused = true)]
	async fn a_transient_mint_failure_is_retried_with_backoff() {
		let minter = ScriptedMinter::new(vec![Err(Status::unavailable("down")), Err(Status::deadline_exceeded("slow")), token("t1", 1_300)], Duration::ZERO);
		let (source, _) = source(&minter, 1_000);
		let started = Instant::now();
		assert_eq!(bearer(&source).await, "Bearer t1");
		assert_eq!(minter.calls(), 3);
		// 250 ms, then 500 ms.
		assert!(started.elapsed() >= Duration::from_millis(750));
	}

	#[tokio::test(start_paused = true)]
	async fn a_mint_that_keeps_failing_is_an_error_not_a_panic_and_callers_fail_fast_after_it() {
		let minter = ScriptedMinter::new(vec![Err(Status::unavailable("down"))], Duration::ZERO);
		let (source, _) = source(&minter, 1_000);

		let err = source.bearer().await.unwrap_err();
		assert!(err.to_string().contains("Unavailable"), "{err}");
		assert_eq!(minter.calls(), MINT_ATTEMPTS);

		// Inside the cooldown nobody mints again.
		assert!(source.bearer().await.is_err());
		assert_eq!(minter.calls(), MINT_ATTEMPTS);

		// After it, one more series.
		tokio::time::advance(FAILURE_COOLDOWN + Duration::from_millis(1)).await;
		assert!(source.bearer().await.is_err());
		assert_eq!(minter.calls(), 2 * MINT_ATTEMPTS);
	}

	#[tokio::test(start_paused = true)]
	async fn a_certificate_banking_does_not_know_is_not_retried() {
		let minter = ScriptedMinter::new(vec![Err(Status::unauthenticated("no client"))], Duration::ZERO);
		let (source, _) = source(&minter, 1_000);
		assert!(source.bearer().await.is_err());
		assert_eq!(minter.calls(), 1);
	}

	#[tokio::test(start_paused = true)]
	async fn a_token_inside_its_margin_still_serves_while_banking_is_down() {
		let minter = ScriptedMinter::new(vec![token("t1", 1_300), Err(Status::unavailable("down"))], Duration::ZERO);
		let (source, clock) = source(&minter, 1_000);
		assert_eq!(bearer(&source).await, "Bearer t1");

		clock.0.store(1_280, Ordering::SeqCst);
		assert_eq!(bearer(&source).await, "Bearer t1");

		// Past its expiry nothing is served.
		clock.0.store(1_300, Ordering::SeqCst);
		tokio::time::advance(FAILURE_COOLDOWN + Duration::from_millis(1)).await;
		assert!(source.bearer().await.is_err());
	}

	#[tokio::test(start_paused = true)]
	async fn an_empty_or_malformed_token_is_refused_and_never_echoed() {
		let minter = ScriptedMinter::new(vec![token("", 1_300)], Duration::ZERO);
		let (empty, _) = source(&minter, 1_000);
		assert!(empty.bearer().await.is_err());

		let minter = ScriptedMinter::new(vec![token("secret\nvalue", 1_300)], Duration::ZERO);
		let (malformed, _) = source(&minter, 1_000);
		let err = malformed.bearer().await.unwrap_err().to_string();
		assert!(!err.contains("secret"), "{err}");
	}

	#[test]
	fn a_minted_token_does_not_print() {
		let minted = MintedToken {
			token: "hunter2".into(),
			expires_at: 5,
		};
		assert!(!format!("{minted:?}").contains("hunter2"));
	}
}
