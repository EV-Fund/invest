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
/// every change of [`ScopeCatalog::invest`] (a test pins the two together). concierge also
/// refuses a version more than a day ahead of its clock, so the bump is never a future date.
pub const SCOPE_CATALOG_VERSION: u64 = 1_792_627_200;

/// One notification topic invest emits on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopicSpec {
	pub topic: &'static str,
	pub label: &'static str,
	pub description: &'static str,
	pub default_subscribed: bool,
}

/// Who decides a kind (concierge §5.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KindLevel {
	/// The persisted owners, more than half.
	Platform,
	/// The scope's admins, under the scope's policy.
	Scope,
}

/// A field type concierge renders and hashes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FieldType {
	Text,
	Bps,
	Flag,
	/// Unix seconds.
	Timestamp,
	Count,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldSpec {
	pub key: &'static str,
	pub label: &'static str,
	pub field_type: FieldType,
	pub required: bool,
}

/// One approval kind invest proposes and executes. A PLATFORM kind escalates nowhere, so
/// none of invest's kinds declares an escalation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KindSpec {
	pub name: &'static str,
	pub label: &'static str,
	pub description: &'static str,
	pub level: KindLevel,
	pub fields: Vec<FieldSpec>,
}

/// The field keys of `invest:fee_policy`, shared by the catalog and the proposer.
pub mod fee_policy_fields {
	pub const ALLOCATION: &str = "allocation";
	pub const CURRENT_MANAGEMENT: &str = "current_management";
	pub const PROPOSED_MANAGEMENT: &str = "proposed_management";
	pub const CURRENT_PERFORMANCE: &str = "current_performance";
	pub const PROPOSED_PERFORMANCE: &str = "proposed_performance";
	pub const CURRENT_HURDLE: &str = "current_hurdle";
	pub const PROPOSED_HURDLE: &str = "proposed_hurdle";
	pub const CURRENT_BASIS: &str = "current_basis";
	pub const PROPOSED_BASIS: &str = "proposed_basis";
	pub const CURRENT_CRYSTALLIZATION: &str = "current_crystallization";
	pub const PROPOSED_CRYSTALLIZATION: &str = "proposed_crystallization";
	/// The proposed terms leave the house envelope. The name is concierge's floor field
	/// (`invest:fee_policy` escalates on it at scope level); at PLATFORM it only informs.
	pub const RAISES_CEILING: &str = "raises_ceiling";
	pub const EFFECTIVE_FROM: &str = "effective_from";
	pub const HOLDERS_NOTIFIED: &str = "holders_notified";
}

/// `invest:fee_policy` at PLATFORM: the owners decide a fee change beyond the house envelope,
/// as banking's consilium did (owner's decision, 2026-10-22).
fn fee_policy_kind() -> KindSpec {
	use fee_policy_fields as f;
	let field = |key, label, field_type| FieldSpec {
		key,
		label,
		field_type,
		required: true,
	};
	KindSpec {
		name: "invest:fee_policy",
		label: "Fee-policy change",
		description: "Change a fund's fee terms beyond the house envelope (at most 2% management and 20% performance, on invested capital, crystallized yearly), or lower its hurdle. When the owners pass it, the holders are told and the notice period starts.",
		level: KindLevel::Platform,
		fields: vec![
			field(f::ALLOCATION, "Fund", FieldType::Text),
			field(f::CURRENT_MANAGEMENT, "Management fee now", FieldType::Bps),
			field(f::PROPOSED_MANAGEMENT, "Management fee proposed", FieldType::Bps),
			field(f::CURRENT_PERFORMANCE, "Performance fee now", FieldType::Bps),
			field(f::PROPOSED_PERFORMANCE, "Performance fee proposed", FieldType::Bps),
			field(f::CURRENT_HURDLE, "Hurdle now", FieldType::Bps),
			field(f::PROPOSED_HURDLE, "Hurdle proposed", FieldType::Bps),
			field(f::CURRENT_BASIS, "Management fee basis now", FieldType::Text),
			field(f::PROPOSED_BASIS, "Management fee basis proposed", FieldType::Text),
			field(f::CURRENT_CRYSTALLIZATION, "Crystallization now", FieldType::Text),
			field(f::PROPOSED_CRYSTALLIZATION, "Crystallization proposed", FieldType::Text),
			field(f::RAISES_CEILING, "Leaves the house envelope", FieldType::Flag),
			field(f::EFFECTIVE_FROM, "Binds no earlier than", FieldType::Timestamp),
			field(f::HOLDERS_NOTIFIED, "Holders to be notified", FieldType::Count),
		],
	}
}

/// What invest publishes to `ScopeCatalogService`: its notification topics and approval
/// kinds. Published whole — whatever is left out is withdrawn.
///
/// A kind is added together with the code that proposes it: concierge refuses a proposal
/// of a kind the catalog in force does not declare. `invest:valuation_override` and
/// `invest:holder_grant` are not proposed yet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopeCatalog {
	pub topics: Vec<TopicSpec>,
	pub kinds: Vec<KindSpec>,
}

impl ScopeCatalog {
	pub fn invest() -> Self {
		Self {
			kinds: vec![fee_policy_kind()],
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
