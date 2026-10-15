//! The BFF's Postgres: the `cabinet` schema of the `invest` database, nothing else.

use std::time::Duration;

use color_eyre::eyre::{Context, Result};
use sqlx::{PgPool, migrate::Migrator, postgres::PgPoolOptions};

/// The schema the BFF owns. invest-core's tables are next to it and never read from here.
pub const SCHEMA: &str = "cabinet";

/// How long a request waits for a free connection. Well under the router's 15 s deadline:
/// a pool drained by a flood (the anonymous login writes a row) answers 500 fast rather
/// than holding every signed-in request until it times out (security review F1).
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(3);

pub async fn connect(database_url: &str, max_connections: u32) -> Result<PgPool> {
	PgPoolOptions::new()
		.max_connections(max_connections)
		.acquire_timeout(ACQUIRE_TIMEOUT)
		.connect(database_url)
		.await
		.wrap_err("connect to the session store")
}

/// Apply the BFF's migrations (`cabinet/backend/migrations`, embedded at build time) on
/// boot. Their history lives in `cabinet._sqlx_migrations`, apart from invest-core's
/// `public._sqlx_migrations` in the same database, so neither service ever reads the
/// other's migrations as unknown or modified. A merged migration is immutable.
pub async fn migrate(pool: &PgPool) -> Result<()> {
	migrator().run(pool).await.wrap_err("apply the cabinet migrations")
}

fn migrator() -> Migrator {
	let mut migrator = sqlx::migrate!("./migrations");
	migrator.create_schema(SCHEMA);
	migrator.dangerous_set_table_name(format!("{SCHEMA}._sqlx_migrations"));
	migrator
}

#[cfg(test)]
mod tests {
	use std::time::{Duration, Instant};

	use crate::routes::test_support::database_url;

	/// A request that cannot get a connection fails fast instead of queueing for sqlx's
	/// 30 s default: a pool drained by a flood of sign-ins must not turn every signed-in
	/// request into the router's 504 (security review F1).
	#[tokio::test]
	async fn an_exhausted_pool_refuses_within_seconds() {
		let Some(url) = database_url().await else { return };
		let pool = super::connect(&url, 1).await.expect("connect");
		let _held = pool.acquire().await.expect("the one connection");
		let started = Instant::now();
		let waited = tokio::time::timeout(Duration::from_secs(8), pool.acquire()).await;
		let answer = waited.expect("acquire must give up on its own, well before the request deadline");
		assert!(answer.is_err(), "no connection was free");
		assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
	}
}
