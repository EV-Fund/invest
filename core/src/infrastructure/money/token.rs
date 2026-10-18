//! The `typ=service` bearer invest sends to banking's scope listener, minted on demand.
//!
//! banking issues it over the same mTLS channel (`scope.v1.ScopeAuthService/MintServiceToken`,
//! an empty request, the client proven by its certificate alone). [`TokenSource`] caches one
//! and keeps it fresh:
//!
//! - **Fresh enough** (more than the token's margin left): returned as is.
//! - **In the refresh window** (still valid, margin not left): returned as is, at once, and
//!   one background mint replaces it. Nobody waits for a renewal while a good token is held.
//! - **Empty, expired or rejected**: callers wait on the one mint in flight.
//!
//! The mint runs as its own task and stores its result itself, so a caller that gives up
//! (a cancelled request) loses nothing: the token lands in the cache for the next one.
//! banking lets a client mint once per 5 s, so every mint start and every failure opens a
//! cooldown of that length; inside it callers get the last failure instead of another try.
//!
//! Only the mint is retried, and only for failures a repeat can cure (UNAVAILABLE,
//! DEADLINE_EXCEEDED, ABORTED). A mint changes nothing in banking's books. The money calls
//! that carry the token are never retried here — their sagas retry them under their
//! idempotency keys.
//!
//! The token is a credential: it is never formatted into an error, a log line or `Debug`,
//! and its header value is marked sensitive.

use std::{
	fmt,
	sync::{
		Arc, Mutex, MutexGuard, PoisonError,
		atomic::{AtomicU64, Ordering},
	},
	time::Duration,
};

use async_trait::async_trait;
use evbanking_contracts::scope::v1::{MintServiceTokenRequest, scope_auth_service_client::ScopeAuthServiceClient};
use tokio::{sync::watch, time::Instant};
use tonic::{Code, Request, Status, metadata::MetadataValue, transport::Channel};

type Bearer = MetadataValue<tonic::metadata::Ascii>;

/// Upper bound of a token's refresh margin; the margin is a fifth of the lifetime banking
/// actually granted, so a short TTL does not mean a mint per call.
const MAX_MARGIN_SECS: i64 = 60;
const MIN_MARGIN_SECS: i64 = 5;

/// A token that arrives with less life than this is banking's clock or TTL gone wrong, not a
/// token to use.
const MIN_LIFETIME_SECS: i64 = 5;

/// A held token with less than this left is treated as expired: it would lapse in flight.
const USABLE_FLOOR_SECS: i64 = 2;

/// banking's limit is one mint per 5 s per client. No mint starts sooner than this after the
/// last start, and after a failure none starts sooner than this after the failure.
const MIN_MINT_INTERVAL: Duration = Duration::from_secs(5);

/// Attempts at one mint series and the pause before the second (doubled for each further).
const MINT_ATTEMPTS: u32 = 3;
const MINT_BACKOFF: Duration = Duration::from_millis(250);

/// The whole series, backoffs included, so a hung banking cannot keep callers waiting.
const SERIES_TIMEOUT: Duration = Duration::from_secs(6);

/// Deadline on one mint call.
const MINT_TIMEOUT: Duration = Duration::from_secs(5);

/// How much of banking's message an error carries.
const MESSAGE_LIMIT: usize = 200;

/// A token and the unix second it stops being valid.
#[derive(Clone)]
pub struct MintedToken {
	pub(crate) token: String,
	pub(crate) expires_at: i64,
}

impl MintedToken {
	pub fn new(token: impl Into<String>, expires_at: i64) -> Self {
		Self { token: token.into(), expires_at }
	}
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

/// Why no bearer could be had. The text carries the status code and a bounded piece of
/// banking's message, never the token.
#[derive(Debug, Clone, thiserror::Error)]
#[error("service token unavailable: {0}")]
pub struct TokenError(String);

/// A bearer for one call, and the generation of the token it came from: a call banking
/// refuses with UNAUTHENTICATED hands it back to [`TokenSource::reject`].
pub struct Lease {
	pub(crate) header: Bearer,
	pub(crate) generation: u64,
}

struct Held {
	header: Bearer,
	expires_at: i64,
	margin: i64,
	generation: u64,
}

impl Held {
	fn remaining(&self, now: i64) -> i64 {
		// `expires_at` is banking's number; a hostile or broken one must not overflow.
		self.expires_at.saturating_sub(now)
	}

	fn lease(&self) -> Lease {
		Lease {
			header: self.header.clone(),
			generation: self.generation,
		}
	}
}

#[derive(Default)]
struct Cache {
	held: Option<Held>,
	inflight: bool,
	/// No mint starts before this.
	not_before: Option<Instant>,
	/// The last reason a bearer could not be had; cleared by the next good token.
	failed: Option<String>,
	generations: u64,
	epoch: u64,
}

struct Failure {
	reason: String,
	/// A code no retry cures — a certificate banking does not know, no grant, no such RPC.
	permanent: bool,
}

struct Inner {
	minter: Arc<dyn TokenMinter>,
	clock: Arc<dyn UnixClock>,
	cache: Mutex<Cache>,
	/// Bumped whenever a mint finishes, for the callers waiting on it.
	done: watch::Sender<u64>,
	permanent_failures: AtomicU64,
}

pub struct TokenSource {
	inner: Arc<Inner>,
}

impl TokenSource {
	pub fn new(minter: Arc<dyn TokenMinter>, clock: Arc<dyn UnixClock>) -> Self {
		Self {
			inner: Arc::new(Inner {
				minter,
				clock,
				cache: Mutex::new(Cache::default()),
				done: watch::channel(0).0,
				permanent_failures: AtomicU64::new(0),
			}),
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

	/// How many mints ended in a failure no retry cures; the alert-ready twin of the
	/// `scope_token_mint_permanent` error log.
	pub fn permanent_failures(&self) -> u64 {
		self.inner.permanent_failures.load(Ordering::Relaxed)
	}

	/// A bearer good for one call. Waits only when no usable token is held.
	pub async fn bearer(&self) -> Result<Lease, TokenError> {
		loop {
			// Subscribed before the cache is read: a mint finishing in between still wakes us.
			let mut finished = self.inner.done.subscribe();
			{
				let mut cache = self.inner.lock();
				let now = self.inner.clock.now_unix();
				if let Some(held) = &cache.held {
					let remaining = held.remaining(now);
					if remaining >= held.margin {
						return Ok(held.lease());
					}
					if remaining >= USABLE_FLOOR_SECS {
						let lease = held.lease();
						if cache.can_start_mint() {
							self.inner.start_mint(&mut cache);
						}
						return Ok(lease);
					}
				}
				if !cache.inflight {
					if !cache.can_start_mint() {
						return Err(TokenError(cache.failed.clone().unwrap_or_else(|| "no usable token and the next mint is not due yet".into())));
					}
					self.inner.start_mint(&mut cache);
				}
			}
			// The sender lives in `inner`, which `self` holds, so this cannot report a closed
			// channel; either way the loop re-reads the cache.
			let _ = finished.changed().await;
		}
	}

	/// banking refused a call that carried this lease's token with UNAUTHENTICATED: the token
	/// is dropped — unless a newer one has replaced it, so parallel refusals of the same old
	/// token do not throw each other's fresh ones away. The next caller mints, once the 5 s
	/// since the last mint have passed.
	pub fn reject(&self, generation: u64) {
		let mut cache = self.inner.lock();
		if cache.held.as_ref().is_some_and(|held| held.generation == generation) {
			cache.held = None;
			cache.failed = Some("the ledger refused the service token as unauthenticated".into());
			tracing::warn!("scope ledger refused the service token; it will be minted again");
		}
	}

	#[cfg(test)]
	fn failed(&self) -> Option<String> {
		self.inner.lock().failed.clone()
	}
}

impl Cache {
	fn can_start_mint(&self) -> bool {
		!self.inflight && self.not_before.is_none_or(|at| Instant::now() >= at)
	}
}

impl Inner {
	fn lock(&self) -> MutexGuard<'_, Cache> {
		// The cache holds plain data and is never left half-written across a panic point.
		self.cache.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Spawns the mint as its own task: it outlives a caller that is cancelled, and stores its
	/// own result. The task ends by itself within [`SERIES_TIMEOUT`]; there is nothing to
	/// join at shutdown.
	fn start_mint(self: &Arc<Self>, cache: &mut Cache) {
		cache.inflight = true;
		cache.not_before = Some(Instant::now() + MIN_MINT_INTERVAL);
		let inner = self.clone();
		tokio::spawn(async move { inner.run_mint().await });
	}

	async fn run_mint(&self) {
		let outcome = match tokio::time::timeout(SERIES_TIMEOUT, self.mint_series()).await {
			Ok(outcome) => outcome,
			Err(_) => Err(Failure {
				reason: "MintServiceToken: the retry series timed out".into(),
				permanent: false,
			}),
		};
		let mut cache = self.lock();
		// Read after the mint: it may have taken seconds, and the token's life counts from now.
		let now = self.clock.now_unix();
		let outcome = outcome.and_then(|minted| accept(&mut cache, minted, now));
		match outcome {
			Ok(()) => cache.failed = None,
			Err(failure) => {
				if failure.permanent {
					self.permanent_failures.fetch_add(1, Ordering::Relaxed);
					tracing::error!(alert = "scope_token_mint_permanent", reason = %failure.reason, "scope ledger service token cannot be minted; a retry will not help");
				} else {
					tracing::warn!(reason = %failure.reason, "scope ledger service token mint failed");
				}
				cache.failed = Some(failure.reason);
				cache.not_before = Some(Instant::now() + MIN_MINT_INTERVAL);
			}
		}
		cache.inflight = false;
		cache.epoch += 1;
		self.done.send_replace(cache.epoch);
	}

	async fn mint_series(&self) -> Result<MintedToken, Failure> {
		let mut backoff = MINT_BACKOFF;
		let mut attempt = 1;
		loop {
			let status = match self.minter.mint().await {
				Ok(minted) => return Ok(minted),
				Err(status) => status,
			};
			let repeatable = matches!(status.code(), Code::Unavailable | Code::DeadlineExceeded | Code::Aborted);
			if !repeatable || attempt >= MINT_ATTEMPTS {
				return Err(Failure {
					reason: format!("MintServiceToken: {:?}: {}", status.code(), bounded(status.message())),
					permanent: matches!(status.code(), Code::Unauthenticated | Code::PermissionDenied | Code::Unimplemented),
				});
			}
			tokio::time::sleep(backoff).await;
			backoff *= 2;
			attempt += 1;
		}
	}
}

/// Takes a minted token into the cache, or says why it is not one.
fn accept(cache: &mut Cache, minted: MintedToken, now: i64) -> Result<(), Failure> {
	let nonsense = |reason: String| Failure { reason, permanent: false };
	if minted.token.is_empty() {
		return Err(nonsense("MintServiceToken answered an empty token".into()));
	}
	let lifetime = minted.expires_at.saturating_sub(now);
	if lifetime < MIN_LIFETIME_SECS {
		return Err(nonsense(format!("banking clock/ttl nonsense: the token is valid for {lifetime} s")));
	}
	// The error must not echo the token: it is a credential.
	let mut header: Bearer =
		MetadataValue::try_from(format!("Bearer {}", minted.token)).map_err(|_| nonsense("MintServiceToken answered a token that is not a valid header value".into()))?;
	header.set_sensitive(true);
	cache.generations += 1;
	cache.held = Some(Held {
		header,
		expires_at: minted.expires_at,
		margin: (lifetime / 5).clamp(MIN_MARGIN_SECS, MAX_MARGIN_SECS),
		generation: cache.generations,
	});
	Ok(())
}

fn bounded(message: &str) -> String {
	message.chars().take(MESSAGE_LIMIT).collect()
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
		Ok(MintedToken::new(response.token, response.expires_at))
	}
}

#[cfg(test)]
mod tests {
	use std::{
		collections::VecDeque,
		sync::atomic::{AtomicI64, AtomicU32},
	};

	use super::*;

	struct ManualClock(AtomicI64);

	impl ManualClock {
		fn set(&self, now: i64) {
			self.0.store(now, Ordering::SeqCst);
		}
	}

	impl UnixClock for ManualClock {
		fn now_unix(&self) -> i64 {
			self.0.load(Ordering::SeqCst)
		}
	}

	/// Answers from a script, one entry per mint call; the last entry repeats.
	struct ScriptedMinter {
		calls: AtomicU32,
		script: Mutex<VecDeque<Result<MintedToken, Status>>>,
		/// How long a mint takes, so concurrent callers really overlap.
		latency: Duration,
	}

	impl ScriptedMinter {
		fn new(script: Vec<Result<MintedToken, Status>>, latency: Duration) -> Arc<Self> {
			Arc::new(Self {
				calls: AtomicU32::new(0),
				script: Mutex::new(script.into()),
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
		Ok(MintedToken::new(name, expires_at))
	}

	fn source(minter: &Arc<ScriptedMinter>, now: i64) -> (Arc<TokenSource>, Arc<ManualClock>) {
		let clock = Arc::new(ManualClock(AtomicI64::new(now)));
		(Arc::new(TokenSource::new(minter.clone(), clock.clone())), clock)
	}

	async fn bearer(source: &TokenSource) -> String {
		source.bearer().await.unwrap().header.to_str().unwrap().to_owned()
	}

	/// Lets spawned mint tasks run to their next await point or completion.
	async fn settle() {
		tokio::time::sleep(Duration::from_millis(1)).await;
	}

	async fn past_the_cooldown() {
		tokio::time::advance(MIN_MINT_INTERVAL + Duration::from_millis(1)).await;
	}

	#[tokio::test(start_paused = true)]
	async fn a_token_is_reused_until_the_margin_then_renewed_in_the_background() {
		let minter = ScriptedMinter::new(vec![token("t1", 1_300), token("t2", 1_600)], Duration::ZERO);
		let (source, clock) = source(&minter, 1_000);

		assert_eq!(bearer(&source).await, "Bearer t1");
		assert_eq!(bearer(&source).await, "Bearer t1");
		assert_eq!(minter.calls(), 1);

		// Exactly the margin (a fifth of 300 s, capped at 60) left: not yet less than it.
		clock.set(1_240);
		past_the_cooldown().await;
		assert_eq!(bearer(&source).await, "Bearer t1");
		settle().await;
		assert_eq!(minter.calls(), 1);

		// 59 s left: the caller still gets t1, and t2 is minted behind its back.
		clock.set(1_241);
		assert_eq!(bearer(&source).await, "Bearer t1");
		settle().await;
		assert_eq!(minter.calls(), 2);
		assert_eq!(bearer(&source).await, "Bearer t2");
	}

	#[tokio::test(start_paused = true)]
	async fn callers_in_the_window_do_not_wait_and_one_refresher_runs() {
		// Every mint takes four seconds.
		let minter = ScriptedMinter::new(vec![token("t1", 1_300), token("t2", 1_600)], Duration::from_secs(4));
		let (source, clock) = source(&minter, 1_000);
		assert_eq!(bearer(&source).await, "Bearer t1");

		clock.set(1_250);
		past_the_cooldown().await;
		let started = Instant::now();
		for _ in 0..8 {
			assert_eq!(bearer(&source).await, "Bearer t1");
		}
		assert_eq!(started.elapsed(), Duration::ZERO);
		settle().await;
		assert_eq!(minter.calls(), 2, "one refresher, not eight");
	}

	#[tokio::test(start_paused = true)]
	async fn concurrent_callers_on_an_empty_cache_share_one_mint() {
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
	async fn a_caller_that_gives_up_mid_mint_does_not_lose_the_token() {
		let minter = ScriptedMinter::new(vec![token("t1", 1_300)], Duration::from_secs(2));
		let (source, _) = source(&minter, 1_000);

		let waiting = {
			let source = source.clone();
			tokio::spawn(async move { source.bearer().await.map(|_| ()) })
		};
		settle().await;
		waiting.abort();
		let _ = waiting.await;

		tokio::time::sleep(Duration::from_secs(3)).await;
		assert_eq!(bearer(&source).await, "Bearer t1");
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
	async fn a_mint_that_keeps_failing_is_an_error_and_the_cooldown_holds_callers_off() {
		let minter = ScriptedMinter::new(vec![Err(Status::unavailable("down"))], Duration::ZERO);
		let (source, _) = source(&minter, 1_000);

		let err = source.bearer().await.err().unwrap();
		assert!(err.to_string().contains("Unavailable"), "{err}");
		assert_eq!(minter.calls(), MINT_ATTEMPTS);

		assert!(source.bearer().await.is_err());
		assert_eq!(minter.calls(), MINT_ATTEMPTS);

		past_the_cooldown().await;
		assert!(source.bearer().await.is_err());
		assert_eq!(minter.calls(), 2 * MINT_ATTEMPTS);
		assert_eq!(source.permanent_failures(), 0);
	}

	#[tokio::test(start_paused = true)]
	async fn only_unavailable_deadline_and_aborted_are_repeated() {
		for status in [
			Status::unauthenticated("no client"),
			Status::permission_denied("not_granted"),
			Status::unimplemented("old banking"),
			Status::resource_exhausted("minting too often"),
			Status::internal("boom"),
			Status::cancelled("gone"),
		] {
			let code = status.code();
			let minter = ScriptedMinter::new(vec![Err(status)], Duration::ZERO);
			let (source, _) = source(&minter, 1_000);
			assert!(source.bearer().await.is_err());
			assert_eq!(minter.calls(), 1, "{code:?} must not be repeated");
			assert!(source.bearer().await.is_err());
			assert_eq!(minter.calls(), 1, "{code:?} must wait out the cooldown");
			let permanent = matches!(code, Code::Unauthenticated | Code::PermissionDenied | Code::Unimplemented);
			assert_eq!(source.permanent_failures(), u64::from(permanent), "{code:?}");
		}
	}

	#[tokio::test(start_paused = true)]
	async fn a_token_in_its_window_serves_while_banking_refuses_and_an_expired_one_does_not() {
		let minter = ScriptedMinter::new(vec![token("t1", 1_300), Err(Status::resource_exhausted("minting too often"))], Duration::ZERO);
		let (source, clock) = source(&minter, 1_000);
		assert_eq!(bearer(&source).await, "Bearer t1");

		clock.set(1_280);
		past_the_cooldown().await;
		assert_eq!(bearer(&source).await, "Bearer t1");
		settle().await;
		assert_eq!(minter.calls(), 2);
		// The refusal opened a cooldown, but t1 is still good.
		assert_eq!(bearer(&source).await, "Bearer t1");
		assert_eq!(minter.calls(), 2);

		// Its life ends: nothing is served on a stale reading of the clock.
		clock.set(1_299);
		assert!(source.bearer().await.is_err());
	}

	#[tokio::test(start_paused = true)]
	async fn a_token_a_second_from_its_end_is_not_a_fallback() {
		// t1 is 1 s from its end by the clock as it stands now: unusable, so the caller waits
		// for a mint, which fails — and the caller must not get t1 for it.
		let minter = ScriptedMinter::new(vec![token("t1", 1_030), Err(Status::unavailable("down"))], Duration::ZERO);
		let (source, clock) = source(&minter, 1_000);
		assert_eq!(bearer(&source).await, "Bearer t1");
		clock.set(1_029);
		past_the_cooldown().await;
		assert!(source.bearer().await.is_err());
	}

	#[tokio::test(start_paused = true)]
	async fn a_token_that_is_already_over_or_absurd_is_refused() {
		for expires_at in [1_000, 1_004, 900, i64::MIN] {
			let minter = ScriptedMinter::new(vec![token("t1", expires_at)], Duration::ZERO);
			let (source, _) = source(&minter, 1_000);
			let err = source.bearer().await.err().unwrap().to_string();
			assert!(err.contains("clock/ttl nonsense"), "{expires_at}: {err}");
			assert!(!err.contains("t1"));
			// …and it opened the cooldown instead of being asked again.
			assert!(source.bearer().await.is_err());
			assert_eq!(minter.calls(), 1);
		}
		// Five seconds of life is the least that is taken.
		let minter = ScriptedMinter::new(vec![token("t1", 1_005)], Duration::ZERO);
		let (source, _) = source(&minter, 1_000);
		assert_eq!(bearer(&source).await, "Bearer t1");
	}

	#[tokio::test(start_paused = true)]
	async fn an_empty_or_malformed_token_obeys_the_cooldown_and_is_never_echoed() {
		let minter = ScriptedMinter::new(vec![token("", 1_300)], Duration::ZERO);
		let (empty, _) = source(&minter, 1_000);
		assert!(empty.bearer().await.is_err());
		assert!(empty.bearer().await.is_err());
		assert_eq!(minter.calls(), 1);

		let minter = ScriptedMinter::new(vec![token("secret\nvalue", 1_300)], Duration::ZERO);
		let (malformed, _) = source(&minter, 1_000);
		let err = malformed.bearer().await.err().unwrap().to_string();
		assert!(!err.contains("secret"), "{err}");
		assert!(malformed.bearer().await.is_err());
		assert_eq!(minter.calls(), 1);
	}

	#[tokio::test(start_paused = true)]
	async fn a_short_ttl_is_not_a_mint_per_call() {
		// 30 s of life: margin 6 s, so a burst of calls shares one token.
		let minter = ScriptedMinter::new(vec![token("t1", 1_030)], Duration::ZERO);
		let (source, clock) = source(&minter, 1_000);
		for now in [1_000, 1_005, 1_010, 1_023] {
			clock.set(now);
			assert_eq!(bearer(&source).await, "Bearer t1");
		}
		settle().await;
		assert_eq!(minter.calls(), 1);
	}

	#[tokio::test(start_paused = true)]
	async fn a_refused_token_is_dropped_once_and_minted_again_after_the_cooldown() {
		let minter = ScriptedMinter::new(vec![token("t1", 1_300), token("t2", 1_300)], Duration::ZERO);
		let (source, _) = source(&minter, 1_000);
		let first = source.bearer().await.unwrap();

		// Refused right after the mint: no new mint inside banking's 5 s.
		source.reject(first.generation);
		assert!(source.bearer().await.is_err());
		assert_eq!(minter.calls(), 1);

		past_the_cooldown().await;
		assert_eq!(bearer(&source).await, "Bearer t2");
		assert_eq!(minter.calls(), 2);

		// A second call refused with the OLD token must not throw t2 away.
		source.reject(first.generation);
		assert_eq!(bearer(&source).await, "Bearer t2");
		assert_eq!(minter.calls(), 2);
	}

	#[tokio::test(start_paused = true)]
	async fn a_good_mint_clears_the_last_failure() {
		let minter = ScriptedMinter::new(vec![Err(Status::unauthenticated("no client")), token("t1", 1_300)], Duration::ZERO);
		let (source, _) = source(&minter, 1_000);
		assert!(source.bearer().await.is_err());
		assert!(source.failed().is_some());
		past_the_cooldown().await;
		assert_eq!(bearer(&source).await, "Bearer t1");
		assert_eq!(source.failed(), None);
	}

	#[tokio::test(start_paused = true)]
	async fn banking_s_message_is_bounded_and_the_header_is_sensitive() {
		let minter = ScriptedMinter::new(vec![Err(Status::unavailable("x".repeat(5_000)))], Duration::ZERO);
		let (source, _) = source(&minter, 1_000);
		assert!(source.bearer().await.err().unwrap().to_string().len() < 400);

		let minter = ScriptedMinter::new(vec![token("t1", 1_300)], Duration::ZERO);
		let (source, _) = self::source(&minter, 1_000);
		assert!(source.bearer().await.unwrap().header.is_sensitive());
	}

	#[test]
	fn a_minted_token_does_not_print() {
		let minted = MintedToken::new("hunter2", 5);
		assert!(!format!("{minted:?}").contains("hunter2"));
	}
}
