//! Publishing invest's permission and scope catalogs to concierge — the ports.
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

/// The scope catalog's version: unix seconds, same comparison as [`CATALOG_VERSION`] and
/// kept apart from it because concierge stores the two catalogs separately. Bump it with
/// every change of [`ScopeCatalog::invest`] (a test pins the two together).
pub const SCOPE_CATALOG_VERSION: u64 = 1_792_195_200;

/// One notification topic invest emits on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopicSpec {
	pub topic: &'static str,
	pub label: &'static str,
	pub description: &'static str,
	pub default_subscribed: bool,
}

/// What invest publishes to `ScopeCatalogService`: its notification topics and approval
/// kinds. Published whole — whatever is left out is withdrawn.
///
/// No approval kinds yet: invest does not open a proposal today (a fee change that needs
/// the owners is refused), and concierge treats a kind it does not know in `invest:*` as a
/// platform-level one, so declaring none costs nothing. The kinds are added together with
/// the code that proposes them (`invest:fee_policy`, `invest:valuation_override`,
/// `invest:holder_grant`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopeCatalog {
	pub topics: Vec<TopicSpec>,
}

impl ScopeCatalog {
	pub fn invest() -> Self {
		Self {
			topics: vec![TopicSpec {
				topic: "invest:fund:quy-nhon",
				label: "Quy Nhon fund",
				description: "News and notices about the Quy Nhon fund.",
				// Following it was an explicit choice before the topic moved here from
				// `fund:quy-nhon`; concierge carried those subscriptions across.
				default_subscribed: false,
			}],
		}
	}
}

#[async_trait]
pub trait ScopeCatalogPublisher: Send + Sync {
	async fn publish(&self, catalog: &ScopeCatalog, version: u64) -> Result<(), PublishError>;
}
