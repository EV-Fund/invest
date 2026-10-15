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
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::AllocationManage => "invest:allocation:manage",
			Self::ValuationPost => "invest:valuation:post",
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn permissions_live_in_the_invest_namespace() {
		for permission in [Permission::AllocationManage, Permission::ValuationPost] {
			let name = permission.as_str();
			assert!(name.starts_with("invest:"), "{name}");
			assert_eq!(name.split(':').count(), 3, "{name}: a permission is <namespace>:<resource>:<action>");
		}
	}
}
