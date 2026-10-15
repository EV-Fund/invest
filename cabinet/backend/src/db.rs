//! The BFF's Postgres: the `cabinet` schema of the `invest` database, nothing else.

use color_eyre::eyre::{Context, Result};
use sqlx::{PgPool, migrate::Migrator, postgres::PgPoolOptions};

/// The schema the BFF owns. invest-core's tables are next to it and never read from here.
pub const SCHEMA: &str = "cabinet";

pub async fn connect(database_url: &str, max_connections: u32) -> Result<PgPool> {
	PgPoolOptions::new()
		.max_connections(max_connections)
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
