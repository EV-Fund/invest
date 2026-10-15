//! Publishing invest's permission catalog at boot: the [`CatalogPublisher`] adapter over
//! concierge's own `evconcierge_client`, and [`publish_at_boot`], the retry around it.
//!
//! The scope catalog (`ScopeCatalogService.PublishScopeCatalog`: approval kinds and
//! notification topics) is not published yet. concierge needs it only before invest opens
//! an `invest:*` proposal or emits on an `invest:*` topic, and invest does neither today —
//! a fee change that needs the owners is refused, holder notices are recorded as given up.
//! TODO: publish it with the first `Propose` or `EmitScopeNotification` (`invest:fee_policy`,
//! `invest:valuation_override`, `invest:holder_grant`, `invest:fund:quy-nhon`).

use std::time::Duration;

use async_trait::async_trait;
use evconcierge_client::{Catalog, ConciergeClient, ConciergeError, proto::CatalogAlias};
use invest_domain::authz::PermissionCatalog;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::ConciergeStreamsConfig;
use crate::ports::catalog::{CatalogPublisher, PublishError};

pub struct ClientCatalogPublisher {
	client: ConciergeClient,
}

impl ClientCatalogPublisher {
	/// Over the same address and credential the streams are pulled with. Lazy, like them.
	pub fn connect(config: &ConciergeStreamsConfig) -> color_eyre::Result<Self> {
		let client = ConciergeClient::from_channel(super::channel(config)?, config.client_id.clone(), config.client_secret.clone())
			.map_err(|err| color_eyre::eyre::eyre!("concierge catalog client: {err}"))?;
		Ok(Self { client })
	}
}

#[async_trait]
impl CatalogPublisher for ClientCatalogPublisher {
	async fn publish(&self, catalog: &PermissionCatalog, version: u64) -> Result<(), PublishError> {
		let catalog = Catalog {
			version,
			permissions: catalog.permissions.iter().map(|p| (*p).to_owned()).collect(),
			aliases: catalog
				.aliases
				.iter()
				.map(|alias| CatalogAlias {
					name: alias.name.to_owned(),
					members: alias.members.iter().map(|m| (*m).to_owned()).collect(),
					delegates: alias.delegates.iter().map(|d| (*d).to_owned()).collect(),
				})
				.collect(),
		};
		self.client.publish_catalog(catalog).await.map_err(|err: ConciergeError| {
			if err.is_transient() {
				PublishError::Unavailable(err.to_string())
			} else {
				PublishError::Refused(err.to_string())
			}
		})
	}
}

/// The first wait after a failed attempt; doubled per failure up to [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// What [`publish_at_boot`] ended with.
#[derive(Debug, Eq, PartialEq)]
pub enum Published {
	Done,
	/// concierge refused it; retrying the same catalog cannot change the answer.
	Refused,
	/// Shutdown came first.
	Cancelled,
}

/// Publish `catalog` at `version`, retrying while concierge is unreachable — with backoff,
/// for as long as it takes: until it lands, an `invest:admin` grant holds nothing. A
/// refusal is logged and not retried. Publishing the same catalog again is a no-op on
/// concierge's side, so every boot of every replica does it.
pub async fn publish_at_boot(publisher: &dyn CatalogPublisher, catalog: &PermissionCatalog, version: u64, shutdown: &CancellationToken) -> Published {
	let mut backoff = FIRST_BACKOFF;
	loop {
		let attempt = tokio::select! {
			biased;
			() = shutdown.cancelled() => return Published::Cancelled,
			attempt = publisher.publish(catalog, version) => attempt,
		};
		match attempt {
			Ok(()) => {
				info!(version, "concierge: published the invest permission catalog");
				return Published::Done;
			}
			Err(PublishError::Refused(reason)) => {
				error!(
					version,
					"concierge refused the invest permission catalog — invest:* seats resolve against the last one it stored (needs intervention): {reason}"
				);
				return Published::Refused;
			}
			Err(PublishError::Unavailable(reason)) => {
				warn!(version, retry_in = ?backoff, "concierge: publishing the permission catalog failed, retrying: {reason}");
				tokio::select! {
					() = shutdown.cancelled() => return Published::Cancelled,
					() = tokio::time::sleep(backoff) => {},
				}
				backoff = (backoff * 2).min(MAX_BACKOFF);
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use invest_domain::authz::CatalogAlias;

	use super::*;
	use crate::{infrastructure::concierge::fake::InMemoryCatalogPublisher, ports::catalog::CATALOG_VERSION};

	fn unavailable() -> Result<(), PublishError> {
		Err(PublishError::Unavailable("connection refused".into()))
	}

	#[test]
	fn the_published_catalog_is_pinned_to_its_version() {
		// Changing the catalog without a newer CATALOG_VERSION is refused by concierge
		// (same version, other content): change both together, and this pin with them.
		let pinned = PermissionCatalog {
			permissions: vec!["invest:allocation:manage", "invest:valuation:post"],
			aliases: vec![
				CatalogAlias {
					name: "invest:admin",
					members: vec!["invest:allocation:manage", "invest:valuation:post"],
					delegates: vec!["invest:operator"],
				},
				CatalogAlias {
					name: "invest:operator",
					members: vec!["invest:valuation:post"],
					delegates: vec![],
				},
			],
		};
		assert_eq!((CATALOG_VERSION, PermissionCatalog::invest()), (1_792_022_400, pinned));
	}

	#[tokio::test(start_paused = true)]
	async fn an_unreachable_concierge_is_retried_with_backoff_until_it_lands() {
		let publisher = InMemoryCatalogPublisher::new();
		publisher.script([unavailable(), unavailable(), unavailable()]);
		let started = tokio::time::Instant::now();

		let outcome = publish_at_boot(&publisher, &PermissionCatalog::invest(), CATALOG_VERSION, &CancellationToken::new()).await;

		assert_eq!(outcome, Published::Done);
		assert_eq!(publisher.attempts(), 4);
		assert_eq!(started.elapsed(), Duration::from_secs(1 + 2 + 4), "doubling from one second");
		assert_eq!(publisher.published(), vec![(PermissionCatalog::invest(), CATALOG_VERSION)]);
	}

	#[tokio::test(start_paused = true)]
	async fn a_refusal_is_not_retried() {
		let publisher = InMemoryCatalogPublisher::new();
		publisher.script([unavailable(), Err(PublishError::Refused("version older than the stored one".into()))]);

		let outcome = publish_at_boot(&publisher, &PermissionCatalog::invest(), CATALOG_VERSION, &CancellationToken::new()).await;

		assert_eq!(outcome, Published::Refused);
		assert_eq!(publisher.attempts(), 2);
		assert!(publisher.published().is_empty());
	}

	#[tokio::test(start_paused = true)]
	async fn the_backoff_caps_and_shutdown_ends_the_wait() {
		let publisher = InMemoryCatalogPublisher::new();
		publisher.script(std::iter::repeat_with(unavailable).take(100));
		let shutdown = CancellationToken::new();
		let catalog = PermissionCatalog::invest();
		let started = tokio::time::Instant::now();
		// 1+2+…+256 s, then 300 s per attempt: an hour in is well past the cap.
		let (outcome, ()) = tokio::join!(publish_at_boot(&publisher, &catalog, CATALOG_VERSION, &shutdown), async {
			tokio::time::sleep(Duration::from_secs(3600)).await;
			shutdown.cancel();
		});

		assert_eq!(outcome, Published::Cancelled);
		assert_eq!(started.elapsed(), Duration::from_secs(3600), "shutdown is not held behind a backoff");
		// 9 doubling waits (511 s), then (3600 - 511) / 300 = 10 capped ones, + the first call.
		assert_eq!(publisher.attempts(), 9 + 10 + 1);
	}
}
