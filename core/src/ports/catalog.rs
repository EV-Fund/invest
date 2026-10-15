//! Publishing invest's permission catalog to concierge — the port.
//!
//! concierge resolves a seat into concrete `invest:*` permissions only against the catalog
//! invest has published; before the first publication an `invest:admin` grant holds
//! nothing. invest-core publishes it at boot (`infrastructure::concierge::catalog`).

use async_trait::async_trait;
use invest_domain::authz::PermissionCatalog;

/// The catalog version: unix seconds, compared by concierge — an older version, or the same
/// one with other content, is refused. A constant rather than the boot time, so a
/// restarting older replica can never supersede a newer catalog; bump it with every change
/// of [`PermissionCatalog::invest`] (a test pins the two together).
pub const CATALOG_VERSION: u64 = 1_792_022_400;

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
	/// Not reachable or too slow: the same call later may succeed.
	#[error("concierge is unavailable: {0}")]
	Unavailable(String),
	/// concierge answered and will answer the same until someone changes something — a
	/// wrong credential, a catalog it refuses, a newer version already stored.
	#[error("concierge refused the catalog: {0}")]
	Refused(String),
}

#[async_trait]
pub trait CatalogPublisher: Send + Sync {
	async fn publish(&self, catalog: &PermissionCatalog, version: u64) -> Result<(), PublishError>;
}
