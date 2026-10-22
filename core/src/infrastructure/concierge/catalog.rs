//! Publishing invest's permission catalog at boot: the [`CatalogPublisher`] adapter over
//! concierge's own `evconcierge_client`, and [`publish_at_boot`], the retry around it.
//!
//! The scope catalog (`ScopeCatalogService.PublishScopeCatalog`: approval kinds and
//! notification topics) goes the same way, through [`ClientScopeCatalogPublisher`] and
//! [`publish_scope_at_boot`]; both share one retry loop.

use std::{future::Future, time::Duration};

use async_trait::async_trait;
use evconcierge_client::{
	Catalog, ConciergeClient, ConciergeError, ScopeCatalog as WireScopeCatalog,
	proto::{ApprovalFieldSpec, ApprovalFieldType, ApprovalKindSpec, ApprovalLevel as WireLevel, CatalogAlias, NotificationTopicSpec},
};
use invest_domain::authz::PermissionCatalog;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::ConciergeStreamsConfig;
use crate::ports::catalog::{CatalogPublisher, FieldType, KindLevel, KindSpec, PublishError, ScopeCatalog, ScopeCatalogPublisher};

pub struct ClientCatalogPublisher {
	client: ConciergeClient,
}

impl ClientCatalogPublisher {
	/// Over the credential the streams are pulled with, and over their address unless
	/// `catalog_addr` names another. Lazy, like them.
	///
	/// Why a separate address: `AuthService.PublishCatalog` is mounted on concierge's open
	/// listener only, while its TLS listener carries just the service seams; over the
	/// streams' TLS channel the call is `Unimplemented`. On the open port the client secret
	/// crosses the cluster unencrypted, as the cabinet BFF's code exchange already does.
	pub fn connect(config: &ConciergeStreamsConfig) -> color_eyre::Result<Self> {
		let channel = super::channel_to(config.catalog_addr.as_deref().unwrap_or(&config.addr), config)?;
		let client =
			ConciergeClient::from_channel(channel, config.client_id.clone(), config.client_secret.clone()).map_err(|err| color_eyre::eyre::eyre!("concierge catalog client: {err}"))?;
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
		self.client.publish_catalog(catalog).await.map_err(publish_error)
	}
}

fn publish_error(err: ConciergeError) -> PublishError {
	if err.is_transient() {
		PublishError::Unavailable(err.to_string())
	} else {
		PublishError::Refused(err.to_string())
	}
}

pub struct ClientScopeCatalogPublisher {
	client: ConciergeClient,
}

impl ClientScopeCatalogPublisher {
	/// Over the same credential as the permission catalog: `ScopeCatalogService` resolves
	/// the namespace from the client, so nothing new is configured.
	pub fn connect(config: &ConciergeStreamsConfig) -> color_eyre::Result<Self> {
		let client = ConciergeClient::from_channel(super::channel(config)?, config.client_id.clone(), config.client_secret.clone())
			.map_err(|err| color_eyre::eyre::eyre!("concierge scope catalog client: {err}"))?;
		Ok(Self { client })
	}
}

#[async_trait]
impl ScopeCatalogPublisher for ClientScopeCatalogPublisher {
	async fn publish(&self, catalog: &ScopeCatalog, version: u64) -> Result<(), PublishError> {
		let wire = WireScopeCatalog {
			version,
			approval_kinds: catalog.kinds.iter().map(kind_spec).collect(),
			notification_topics: catalog
				.topics
				.iter()
				.map(|t| NotificationTopicSpec {
					topic: t.topic.to_owned(),
					label: t.label.to_owned(),
					description: t.description.to_owned(),
					default_subscribed: t.default_subscribed,
				})
				.collect(),
		};
		self.client.publish_scope_catalog(wire).await.map_err(publish_error)
	}
}

fn kind_spec(spec: &KindSpec) -> ApprovalKindSpec {
	ApprovalKindSpec {
		name: spec.name.to_owned(),
		label: spec.label.to_owned(),
		description: spec.description.to_owned(),
		level: match spec.level {
			KindLevel::Platform => WireLevel::Platform,
			KindLevel::Scope => WireLevel::Scope,
		} as i32,
		fields: spec
			.fields
			.iter()
			.map(|field| ApprovalFieldSpec {
				key: field.key.to_owned(),
				label: field.label.to_owned(),
				r#type: match field.field_type {
					FieldType::Text => ApprovalFieldType::Text,
					FieldType::Bps => ApprovalFieldType::Bps,
					FieldType::Flag => ApprovalFieldType::Flag,
					FieldType::Timestamp => ApprovalFieldType::Timestamp,
					FieldType::Count => ApprovalFieldType::Count,
				} as i32,
				required: field.required,
			})
			.collect(),
		// A PLATFORM kind escalates nowhere (concierge refuses one that declares any).
		escalations: Vec::new(),
		// Filled by concierge on read; ignored on publish.
		proposer: String::new(),
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
	retry_publish("permission catalog", "invest:* seats resolve against the last one it stored", version, shutdown, || {
		publisher.publish(catalog, version)
	})
	.await
}

/// The scope catalog, the same way: until it lands concierge refuses an `invest:*` proposal
/// and an emit on an `invest:*` topic, and a refusal leaves the last stored catalog in force.
pub async fn publish_scope_at_boot(publisher: &dyn ScopeCatalogPublisher, catalog: &ScopeCatalog, version: u64, shutdown: &CancellationToken) -> Published {
	retry_publish("scope catalog", "invest topics and kinds stay as the last one it stored", version, shutdown, || {
		publisher.publish(catalog, version)
	})
	.await
}

async fn retry_publish<F, Fut>(what: &str, on_refusal: &str, version: u64, shutdown: &CancellationToken, mut attempt: F) -> Published
where
	F: FnMut() -> Fut,
	Fut: Future<Output = Result<(), PublishError>>, {
	let mut backoff = FIRST_BACKOFF;
	loop {
		let outcome = tokio::select! {
			biased;
			() = shutdown.cancelled() => return Published::Cancelled,
			outcome = attempt() => outcome,
		};
		match outcome {
			Ok(()) => {
				info!(version, "concierge: published the invest {what}");
				return Published::Done;
			}
			Err(PublishError::Refused(reason)) => {
				error!(version, "concierge refused the invest {what} — {on_refusal} (needs intervention): {reason}");
				return Published::Refused;
			}
			Err(PublishError::Unavailable(reason)) => {
				warn!(version, retry_in = ?backoff, "concierge: publishing the {what} failed, retrying: {reason}");
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
	use crate::{
		infrastructure::concierge::fake::{InMemoryCatalogPublisher, InMemoryScopeCatalogPublisher},
		ports::catalog::{CATALOG_VERSION, SCOPE_CATALOG_VERSION, TopicSpec},
	};

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

	#[test]
	fn the_published_scope_catalog_is_pinned_to_its_version() {
		// Same rule as the permission catalog: other content under the same version is
		// refused by concierge, so change the catalog, the version and this pin together.
		let catalog = ScopeCatalog::invest();
		let topics = vec![TopicSpec {
			topic: "invest:fund:quy-nhon",
			label: "Quy Nhon fund",
			description: "News and notices about the Quy Nhon fund.",
			default_subscribed: false,
		}];
		let kinds: Vec<(&str, KindLevel, Vec<&str>)> = catalog.kinds.iter().map(|k| (k.name, k.level, k.fields.iter().map(|f| f.key).collect())).collect();
		let pinned_kinds = vec![(
			"invest:fee_policy",
			KindLevel::Platform,
			vec![
				"allocation",
				"current_management",
				"proposed_management",
				"current_performance",
				"proposed_performance",
				"current_hurdle",
				"proposed_hurdle",
				"current_basis",
				"proposed_basis",
				"current_crystallization",
				"proposed_crystallization",
				"raises_ceiling",
				"effective_from",
				"holders_notified",
			],
		)];
		assert_eq!((SCOPE_CATALOG_VERSION, catalog.topics, kinds), (1_792_627_200, topics, pinned_kinds));
	}

	#[test]
	fn the_approval_kinds_are_ones_concierge_accepts() {
		// concierge's rules on kinds (`domain::scope_catalog`, `domain::floors`), checked
		// here so a bad constant fails a test and not the boot-time publication.
		let catalog = ScopeCatalog::invest();
		let key_ok = |s: &str| s.len() <= 32 && s.bytes().next().is_some_and(|b| b.is_ascii_lowercase()) && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
		let text_ok = |s: &str, max: usize| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control) && !s.contains("://") && !s.contains("www.");
		for kind in &catalog.kinds {
			let rest = kind.name.strip_prefix("invest:").unwrap_or_else(|| panic!("{} is outside the invest namespace", kind.name));
			assert!(key_ok(rest), "{}", kind.name);
			assert!(text_ok(kind.label, 80) && text_ok(kind.description, 500), "{}", kind.name);
			assert!(kind.fields.len() <= 16, "{}", kind.name);
			let mut seen = std::collections::BTreeSet::new();
			for field in &kind.fields {
				assert!(key_ok(field.key) && seen.insert(field.key), "{} field {}", kind.name, field.key);
				assert!(text_ok(field.label, 60), "{} field {}", kind.name, field.key);
			}
			// The owners decide every kind invest publishes (2026-10-22): a SCOPE kind would
			// need concierge's floor fields, a PLATFORM one needs nothing more.
			assert_eq!(kind.level, KindLevel::Platform, "{}", kind.name);
			let wire = kind_spec(kind);
			assert!(wire.escalations.is_empty() && wire.level == WireLevel::Platform as i32, "{}", kind.name);
		}
	}

	#[test]
	fn the_scope_catalog_is_one_concierge_accepts() {
		// concierge's rules on topics (SCOPES-AND-APPROVALS §6, scopes.proto), checked here
		// so a bad constant fails a test and not the boot-time publication.
		let catalog = ScopeCatalog::invest();
		let mut seen = std::collections::BTreeSet::new();
		for t in &catalog.topics {
			let rest = t.topic.strip_prefix("invest:").unwrap_or_else(|| panic!("{} is outside the invest namespace", t.topic));
			let segment_ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
			assert!(rest.split(':').all(segment_ok), "{}", t.topic);
			assert!(t.topic.len() <= 64 && t.label.len() <= 60 && t.description.len() <= 200, "{}", t.topic);
			assert!(!rest.starts_with("floor_"), "floor_* is reserved to concierge: {}", t.topic);
			assert!(seen.insert(t.topic), "duplicate topic {}", t.topic);
		}
	}

	#[tokio::test(start_paused = true)]
	async fn the_scope_catalog_is_retried_like_the_permission_catalog() {
		let publisher = InMemoryScopeCatalogPublisher::new();
		publisher.script([unavailable(), unavailable(), unavailable()]);
		let started = tokio::time::Instant::now();

		let outcome = publish_scope_at_boot(&publisher, &ScopeCatalog::invest(), SCOPE_CATALOG_VERSION, &CancellationToken::new()).await;

		assert_eq!(outcome, Published::Done);
		assert_eq!(publisher.attempts(), 4);
		assert_eq!(started.elapsed(), Duration::from_secs(1 + 2 + 4));
		assert_eq!(publisher.published(), vec![(ScopeCatalog::invest(), SCOPE_CATALOG_VERSION)]);
	}

	#[tokio::test(start_paused = true)]
	async fn a_refused_scope_catalog_is_not_retried() {
		let publisher = InMemoryScopeCatalogPublisher::new();
		publisher.script([Err(PublishError::Refused("version older than the stored one".into()))]);

		let outcome = publish_scope_at_boot(&publisher, &ScopeCatalog::invest(), SCOPE_CATALOG_VERSION, &CancellationToken::new()).await;

		assert_eq!(outcome, Published::Refused);
		assert_eq!(publisher.attempts(), 1);
		assert!(publisher.published().is_empty());
	}

	#[tokio::test(start_paused = true)]
	async fn shutdown_ends_the_scope_catalog_wait() {
		let publisher = InMemoryScopeCatalogPublisher::new();
		publisher.script(std::iter::repeat_with(unavailable).take(100));
		let shutdown = CancellationToken::new();
		let catalog = ScopeCatalog::invest();
		let (outcome, ()) = tokio::join!(publish_scope_at_boot(&publisher, &catalog, SCOPE_CATALOG_VERSION, &shutdown), async {
			tokio::time::sleep(Duration::from_secs(3600)).await;
			shutdown.cancel();
		});
		assert_eq!(outcome, Published::Cancelled);
		assert_eq!(publisher.attempts(), 9 + 10 + 1, "same doubling and cap as the permission catalog");
	}
}
