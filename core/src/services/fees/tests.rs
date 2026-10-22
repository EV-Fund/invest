//! The one piece of this surface the integration suites cannot reach without a full gRPC
//! harness: what an INVESTOR is shown of a change of terms versus what an OPERATOR is
//! (banking#250). The terms, the state, the moments and the reason are public; who asked
//! is governance detail, blanked for everyone without `AllocationManage`.

use invest_domain::{
	balance::ServiceId,
	fees::{ChangeRequirement, CrystallizationPeriod, FeePolicy, FeePolicyChangeId, FeePolicyChangeState, ManagementBasis},
	users::UserId,
};
use uuid::Uuid;

use super::{Audience, change_to_proto, policy_to_proto};
use crate::{
	application::fees::PolicyView,
	ports::{
		approvals::ApprovalState,
		fees::{FeePolicyChange, NoticeWaiver, PolicyRecord},
	},
};

fn scheduled_change() -> FeePolicyChange {
	FeePolicyChange {
		id: FeePolicyChangeId::from_raw(Uuid::from_u128(0x233)),
		service: ServiceId::parse("trading").unwrap(),
		version: 2,
		policy: FeePolicy::new(300, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual).unwrap(),
		state: FeePolicyChangeState::Scheduled,
		requirement: ChangeRequirement::Admin,
		effective_from_unix: 1_700_100_000,
		requested_by: "8f0d2a8e-2b3e-4a1a-9a53-6d5a0d1d2e3f".to_owned(),
		requested_at_unix: 1_700_000_000,
		reason: "the new mandate costs more to run".to_owned(),
		scheduled_at_unix: Some(1_700_000_000),
		applied_at_unix: None,
		undelivered_notices: 0,
		notices_given_up: 0,
		notices_unacknowledged: 0,
		notices_waiver: None,
		approval_id: None,
		approval_state: None,
		approval_holders: None,
	}
}

/// A change beyond the house envelope, proposed to the owners and passed by them.
fn owner_change() -> FeePolicyChange {
	FeePolicyChange {
		state: FeePolicyChangeState::AwaitingConsilium,
		requirement: ChangeRequirement::OwnerConsilium,
		scheduled_at_unix: None,
		approval_id: Some(Uuid::from_u128(0xc0)),
		approval_state: Some(ApprovalState::Passed),
		approval_holders: Some(3),
		..scheduled_change()
	}
}

/// A scheduled change acknowledged by the requester, with a holder given up on since — the
/// figures are only checked for reaching the wire, not for adding up.
fn acknowledged_change() -> FeePolicyChange {
	FeePolicyChange {
		state: FeePolicyChangeState::Scheduled,
		scheduled_at_unix: Some(1_700_000_500),
		undelivered_notices: 2,
		notices_given_up: 1,
		notices_unacknowledged: 1,
		notices_waiver: Some(NoticeWaiver {
			by: "8f0d2a8e-2b3e-4a1a-9a53-6d5a0d1d2e3f".to_owned(),
			at_unix: 1_700_050_000,
			users: vec![UserId::from_raw(Uuid::from_u128(0xa1)), UserId::from_raw(Uuid::from_u128(0xa2))],
		}),
		..scheduled_change()
	}
}

#[test]
fn an_investor_is_shown_the_terms_but_not_who_asked() {
	let change = scheduled_change();
	let shown = change_to_proto(&change, Audience::Investor);

	assert_eq!(shown.requested_by, "", "who asked is governance detail");
	assert_eq!(shown.consilium_id, "", "an administrator's change has no approval");
	// Everything an investor is entitled to stays: the terms they will be on, when, and why.
	assert_eq!(shown.id, change.id.to_string());
	assert_eq!(shown.management_bps, 300);
	assert_eq!(shown.performance_bps, 2_000);
	assert_eq!(shown.state, "scheduled");
	assert_eq!(shown.requirement, "admin");
	assert_eq!(shown.effective_from, 1_700_100_000);
	assert_eq!(shown.requested_at, 1_700_000_000);
	assert_eq!(shown.reason, "the new mandate costs more to run");
}

#[test]
fn an_operator_is_shown_who_asked() {
	let change = scheduled_change();
	let shown = change_to_proto(&change, Audience::Operator);

	assert_eq!(shown.requested_by, "8f0d2a8e-2b3e-4a1a-9a53-6d5a0d1d2e3f");
}

#[test]
fn the_owners_approval_is_shown_to_operators_and_its_existence_to_everyone() {
	let change = owner_change();

	let operator = change_to_proto(&change, Audience::Operator);
	assert_eq!(operator.consilium_id, Uuid::from_u128(0xc0).to_string());
	assert_eq!(operator.approval_state, "passed");
	assert_eq!(operator.approval_holders, Some(3));
	assert_eq!(operator.state, "awaiting_consilium");
	assert_eq!(operator.requirement, "owner_consilium");

	let investor = change_to_proto(&change, Audience::Investor);
	assert_eq!(
		(investor.consilium_id.as_str(), investor.approval_state.as_str(), investor.approval_holders),
		("", "", None),
		"which approval, and its tally, is governance detail"
	);
	assert_eq!(investor.state, "awaiting_consilium", "that the owners are deciding is public");

	let admin = change_to_proto(&scheduled_change(), Audience::Operator);
	assert_eq!((admin.consilium_id.as_str(), admin.approval_state.as_str(), admin.approval_holders), ("", "", None));
}

#[test]
fn the_waiver_and_the_undelivered_figures_are_governance_detail() {
	let change = acknowledged_change();

	let operator = change_to_proto(&change, Audience::Operator);
	assert_eq!(operator.notices_waived_by, "8f0d2a8e-2b3e-4a1a-9a53-6d5a0d1d2e3f");
	assert_eq!(operator.notices_waived_at, 1_700_050_000);
	assert_eq!(
		operator.notices_waived_users,
		vec![UserId::from_raw(Uuid::from_u128(0xa1)).to_string(), UserId::from_raw(Uuid::from_u128(0xa2)).to_string()]
	);
	assert_eq!(operator.undelivered_notices, 2);
	assert_eq!(operator.notices_given_up, 1);
	assert_eq!(operator.notices_unacknowledged, 1);

	let investor = change_to_proto(&change, Audience::Investor);
	assert_eq!(investor.notices_waived_by, "", "who took responsibility is governance detail");
	assert!(investor.notices_waived_users.is_empty(), "other holders' ids are never shown to an investor");
	assert_eq!(investor.undelivered_notices, 0);
	assert_eq!(investor.notices_given_up, 0);
	assert_eq!(investor.notices_unacknowledged, 0);
	// That the notice was waived, and when, is a fact about the terms they are on.
	assert_eq!(investor.notices_waived_at, 1_700_050_000);

	let untouched = change_to_proto(&scheduled_change(), Audience::Operator);
	assert_eq!(untouched.notices_waived_at, 0);
	assert_eq!(untouched.notices_waived_by, "");
	assert!(untouched.notices_waived_users.is_empty());
}

#[test]
fn the_redaction_follows_the_pending_change_onto_the_policy_view() {
	// `GetFeePolicy` carries the change on its way inside the policy; the same audience rule
	// must apply there, or the blanking on `ListFeePolicyChanges` is one screen away from
	// being moot.
	let service = ServiceId::parse("trading").unwrap();
	let view = PolicyView {
		current: Some(PolicyRecord {
			policy: FeePolicy::HOUSE,
			version: 1,
			effective_from_unix: 1_690_000_000,
			updated_at_unix: 1_690_000_000,
		}),
		pending: Some(scheduled_change()),
	};

	let investor = policy_to_proto(&service, &view, Audience::Investor);
	let pending = investor.pending.expect("the change on its way is shown to everyone");
	assert_eq!(pending.requested_by, "");
	assert_eq!(investor.management_bps, 200, "the live terms are public");

	let operator = policy_to_proto(&service, &view, Audience::Operator);
	let pending = operator.pending.expect("the change on its way is shown to everyone");
	assert_ne!(pending.requested_by, "");
}
