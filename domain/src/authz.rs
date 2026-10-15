//! `authz` — the `invest:*` permissions invest-core gates on.
//!
//! The names are invest's catalog (plan §2: banking's `bank:allocation:manage` and
//! `bank:valuation:post` move to the `invest` namespace). concierge resolves a seat to a
//! concrete set of these and invest only asks whether one is in it — wildcards and
//! aliases are expanded on concierge's side, never here.

/// One permission invest-core checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Permission {
	/// Register, open, close, size and admit investors to a product; mint and retire its
	/// units in kind; read its cap table.
	AllocationManage,
	/// Post a product's AUM, from which its NAV is derived.
	ValuationPost,
}

impl Permission {
	/// Every permission invest defines — what its catalog in concierge publishes.
	pub const ALL: [Self; 2] = [Self::AllocationManage, Self::ValuationPost];

	pub const fn as_str(self) -> &'static str {
		match self {
			Self::AllocationManage => "invest:allocation:manage",
			Self::ValuationPost => "invest:valuation:post",
		}
	}

	/// Whether the `invest:operator` seat carries it. The operator runs the day-to-day of a
	/// product — posting its marks — but neither opens, closes nor admits to it. A new
	/// permission has to answer this before it compiles.
	const fn for_operator(self) -> bool {
		match self {
			Self::AllocationManage => false,
			Self::ValuationPost => true,
		}
	}
}

/// The scope's admin seat: everything invest defines. Granted only by an executed
/// `concierge:scope_admin_grant` (concierge docs/SCOPES-AND-APPROVALS.md §5.6); concierge
/// grants that approval as exactly this name, so without the alias in the catalog the grant
/// would hold nothing.
pub const ADMIN_ALIAS: &str = "invest:admin";
/// The operator seat an admin may hand out and take back (`GrantPermission`).
pub const OPERATOR_ALIAS: &str = "invest:operator";

/// One alias of the catalog: a name a seat is granted, the permissions it stands for, and
/// the aliases its holders may grant in turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogAlias {
	pub name: &'static str,
	pub members: Vec<&'static str>,
	pub delegates: Vec<&'static str>,
}

/// invest's permission catalog as concierge's `AuthService.PublishCatalog` takes it: the
/// permissions of the `invest` namespace and the two seat aliases. concierge expands the
/// aliases; invest only ever asks for a concrete [`Permission`].
///
/// No permission for "an investor may use invest" (`invest:self:invest`): concierge gives a
/// relying party's member nothing they were not granted (`iam::resolve`), so such a
/// permission would be held by no investor. invest-core admits an investor by the `users`
/// projection — known and active — not by a permission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionCatalog {
	pub permissions: Vec<&'static str>,
	pub aliases: Vec<CatalogAlias>,
}

impl PermissionCatalog {
	pub fn invest() -> Self {
		let all: Vec<&'static str> = Permission::ALL.iter().map(|p| p.as_str()).collect();
		let operator = Permission::ALL.iter().filter(|p| p.for_operator()).map(|p| p.as_str()).collect();
		Self {
			aliases: vec![
				CatalogAlias {
					name: ADMIN_ALIAS,
					members: all.clone(),
					// Never the admin alias itself: concierge refuses a catalog that does
					// (the roster of a scope's approvals is the owners' to fill).
					delegates: vec![OPERATOR_ALIAS],
				},
				CatalogAlias {
					name: OPERATOR_ALIAS,
					members: operator,
					delegates: Vec::new(),
				},
			],
			permissions: all,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_catalog_holds_every_permission_and_follows_concierges_rules() {
		let catalog = PermissionCatalog::invest();
		assert_eq!(catalog.permissions, ["invest:allocation:manage", "invest:valuation:post"]);
		for alias in &catalog.aliases {
			assert_eq!(alias.name.split(':').count(), 2, "{}: an alias is <namespace>:<name>", alias.name);
			assert!(alias.name.starts_with("invest:"), "{}", alias.name);
			for member in &alias.members {
				assert!(catalog.permissions.contains(member), "{} names {member}, which the catalog does not define", alias.name);
			}
			assert!(!alias.delegates.contains(&ADMIN_ALIAS), "{} delegates the admin alias", alias.name);
			for delegate in &alias.delegates {
				let target = catalog.aliases.iter().find(|a| a.name == *delegate).expect("a delegate is an alias of the catalog");
				assert!(target.delegates.is_empty(), "{delegate} is delegated and delegates in turn");
			}
		}
		let admin = catalog.aliases.iter().find(|a| a.name == ADMIN_ALIAS).unwrap();
		assert_eq!(admin.members, catalog.permissions, "the scope admin holds everything");
		let operator = catalog.aliases.iter().find(|a| a.name == OPERATOR_ALIAS).unwrap();
		assert_eq!(operator.members, ["invest:valuation:post"]);
	}

	#[test]
	fn permissions_live_in_the_invest_namespace() {
		for permission in Permission::ALL {
			let name = permission.as_str();
			assert!(name.starts_with("invest:"), "{name}");
			assert_eq!(name.split(':').count(), 3, "{name}: a permission is <namespace>:<resource>:<action>");
		}
	}
}
