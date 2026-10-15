//! A worker that must run in one invest-core at a time, held by a session advisory lock.
//!
//! The users projection and the money-event consumer both read-then-write per event, so two
//! of either would pass the same guard before either wrote. Each takes a fixed key; this is
//! the loop they share: acquire on a dedicated connection (a second instance blocks inside
//! Postgres, owning nothing), run the work while the connection is probed, and start over
//! when the lock is lost.

use std::{future::Future, time::Duration};

use sqlx::{PgPool, Postgres, pool::PoolConnection};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

/// How often the lock-holding connection is touched. A session lock lives exactly as long as
/// its backend, so this bounds how long a worker keeps going after Postgres freed its lock.
const LOCK_PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// Block on a dedicated pooled connection until the session advisory lock `key` is held; the
/// connection's lifetime is the lock's.
pub(crate) async fn acquire(pool: &PgPool, key: i64) -> Result<PoolConnection<Postgres>, sqlx::Error> {
	let mut conn = pool.acquire().await?;
	sqlx::query("SELECT pg_advisory_lock($1)").bind(key).execute(conn.as_mut()).await?;
	Ok(conn)
}

/// Take the lock if it is free, else `None` — for tests that observe the singleton.
pub(crate) async fn try_acquire(pool: &PgPool, key: i64) -> Result<Option<PoolConnection<Postgres>>, sqlx::Error> {
	let mut conn = pool.acquire().await?;
	let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)").bind(key).fetch_one(conn.as_mut()).await?;
	Ok(acquired.then_some(conn))
}

/// Resolve with the error once the lock-holding connection stops answering.
async fn probe_until_lost(lock: &mut PoolConnection<Postgres>) -> sqlx::Error {
	loop {
		tokio::time::sleep(LOCK_PROBE_INTERVAL).await;
		if let Err(err) = sqlx::query("SELECT 1").execute(lock.as_mut()).await {
			return err;
		}
	}
}

/// Run `work` as the singleton `name` until `shutdown` is cancelled. A lost lock, a failed
/// acquisition or a `work` that returns re-enters the acquisition (with backoff after a
/// failure) and never returns: the composition root tears every branch down when one ends,
/// and a database blip must not take the gRPC server with it.
///
/// `work` is dropped mid-await on shutdown or a lost lock, so it must be safe to cut off at
/// any await — both callers commit an event's effect and cursor together.
pub(crate) async fn run_under_lock<F, Fut>(name: &str, key: i64, pool: &PgPool, shutdown: CancellationToken, work: F)
where
	F: Fn() -> Fut,
	Fut: Future<Output = ()>, {
	const MAX_BACKOFF: Duration = Duration::from_secs(30);
	let mut backoff = Duration::from_millis(500);
	loop {
		let mut lock = tokio::select! {
			biased;
			() = shutdown.cancelled() => return,
			lock = acquire(pool, key) => match lock {
				Ok(lock) => lock,
				Err(err) => {
					error!("{name}: could not acquire its lock (retrying in {backoff:?}): {err}");
					tokio::select! {
						() = shutdown.cancelled() => return,
						() = tokio::time::sleep(backoff) => {},
					}
					backoff = (backoff * 2).min(MAX_BACKOFF);
					continue;
				}
			},
		};
		backoff = Duration::from_millis(500);
		info!("{name}: acquired its lock — running as the singleton");
		let stop = tokio::select! {
			biased;
			() = shutdown.cancelled() => {
				info!("{name}: shutdown requested — stopping");
				true
			}
			err = probe_until_lost(&mut lock) => {
				error!("{name}: lost its lock connection — re-acquiring: {err}");
				false
			}
			() = work() => {
				error!("{name}: its work returned — restarting it");
				false
			}
		};
		// Close, never drop: a dropped connection goes back to the pool still holding the
		// advisory lock (a probe can fail without the backend dying), and the next
		// acquisition — this process's or a standby's — would wait on it for ever.
		if let Err(err) = lock.close().await {
			debug!("{name}: closing the lock connection: {err}");
		}
		if stop {
			return;
		}
	}
}
