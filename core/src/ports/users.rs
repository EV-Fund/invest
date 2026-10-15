//! The people invest knows — read port.
//!
//! invest owns no account: concierge does, and invest keeps a projection of what it needs
//! to decide — whether a person may hold units now, which `invest:*` permissions their
//! seat carries, and the `auth_subject` banking names their compartment by. The writer of
//! that projection is the pull of concierge's lifecycle and scope streams
//! (`infrastructure::concierge`); while it is not configured, or has not heard of a person
//! yet, nobody is known, so a mint to them is refused and they hold no permission — fail
//! closed.

use async_trait::async_trait;
use invest_domain::{authz::Permission, error::DomainError, users::UserId};

#[async_trait]
pub trait UserDirectory: Send + Sync {
	/// What invest knows of `user`, or `None` when nothing has been projected for them.
	async fn standing(&self, user: UserId) -> Result<Option<UserStanding>, DomainError>;

	/// The subject banking names `user`'s compartments by, or `None` while the projection
	/// has not learned it — no money of theirs can be moved until it has.
	async fn auth_subject(&self, user: UserId) -> Result<Option<String>, DomainError>;
}

/// One person as the projection holds them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserStanding {
	/// Not suspended in concierge: units may be minted to them, and their seat counts.
	pub active: bool,
	/// The `invest:*` permissions concierge resolved for their seat. `None` — concierge has
	/// not said — holds nothing, the same as an empty set.
	pub permissions: Option<Vec<String>>,
}

impl UserStanding {
	/// Whether this person may act under `permission` right now. A suspended account holds
	/// nothing whatever its seat says.
	pub fn may(&self, permission: Permission) -> bool {
		self.active && self.permissions.as_ref().is_some_and(|held| held.iter().any(|p| p == permission.as_str()))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn standing(active: bool, permissions: Option<&[&str]>) -> UserStanding {
		UserStanding {
			active,
			permissions: permissions.map(|p| p.iter().map(|s| (*s).to_owned()).collect()),
		}
	}

	#[test]
	fn a_permission_is_held_only_when_named_and_the_account_is_active() {
		let manage = Permission::AllocationManage;
		assert!(standing(true, Some(&["invest:allocation:manage"])).may(manage));
		assert!(!standing(false, Some(&["invest:allocation:manage"])).may(manage), "a suspended seat holds nothing");
		assert!(!standing(true, None).may(manage), "concierge has not said");
		assert!(!standing(true, Some(&[])).may(manage));
		assert!(!standing(true, Some(&["invest:valuation:post"])).may(manage));
		assert!(!standing(true, Some(&["invest:allocation:*"])).may(manage), "patterns are expanded by concierge, never here");
	}
}
