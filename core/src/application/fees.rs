//! Fee use cases — assessing "2 and 20" against a holding, and turning the units it
//! collects into cash.
//!
//! Every write here is the same shape: read the policy, read the holding's *live* unit
//! balances from TigerBeetle, ask [`invest_domain::fees::assess`] what is owed, and persist only
//! if something is actually owed. Nothing is charged when the fund has no policy, when
//! the policy is all zeros, when the investor has no position, or when the charge floors
//! to nothing — and in the last case the clocks are deliberately left alone so the
//! accrual simply continues into the next sweep. A charge that is owed but cannot be
//! collected right now (the units are escrowed by a resting sell or reserved by a queued
//! redemption) is NOT one of those cases: it is recorded, carried whole as debt, and the
//! clock moves — see [`invest_domain::fees::FeeCharge::is_empty`].
//!
//! **Only invest's own scope pays fees to the `fee` allocation (D24(3)).** A charge moves
//! units into the product's fee class, which the `fee` allocation — and so its holders —
//! owns; a product whose cash lives in another scope (`sa`) earns for banking's treasury
//! instead, which units cannot reach. Such a product is never assessed, and its fee class
//! is never settled ([`fee_bearing`]).
//!
//! **The NAV a fee is charged at is the dealing NAV**, staleness guard included. That is
//! a safety property, not an accident: if an operator stops posting marks, fees stop
//! accruing rather than accruing against a price nobody has confirmed. A fee is the one
//! charge the operator sets, prices, and collects, so it gets the same guard as an
//! investor's own dealing.

use std::collections::{HashMap, HashSet};

use invest_domain::{
	balance::{LedgerAccountKey, ServiceId},
	error::DomainError,
	fees::{self, ChangeRequirement, FeeAssessment, FeeAssessmentId, FeeCharge, FeePolicy, FeePolicyChangeId, FeeSettlement, FeeSettlementId, PositionSnapshot, Trigger},
	issuance::IdempotencyKey,
	money::{Nav, Shares, Usdt},
	users::UserId,
};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::{
	application::{
		allocations as allocations_app,
		fee_approvals::{self, FeeApprovalPorts, Proposed},
		funds::{account_cash, dealing_nav, refuse_recent_poster},
	},
	ports::{
		allocations::AllocationRegistry,
		approvals::Approvals,
		fees::{AssessmentRecord, FeeAssessments, FeePolicies, FeePolicyChange, FeePolicyChanges, FeeSettlements, NewFeePolicyChange, PolicyRecord, PositionAccruals, SettlementRecord},
		ledger::Ledger,
		money::MoneyLedger,
		nav::NavMarks,
		redemptions::RedemptionRepository,
		users::UserDirectory,
	},
};

/// A holding's fee position for display: what has accrued since the last charge, and
/// what is still owed from earlier ones. `total` is what would be taken if the fee were
/// assessed right now, which is what a "value net of fees" figure must subtract.
pub struct AccruedFees {
	pub management: Usdt,
	pub performance: Usdt,
	pub debt: Usdt,
	pub total: Usdt,
	/// The investor's own high-water mark — the price their performance fee is measured
	/// from. Worth showing: it is the single number that explains why two investors in
	/// the same fund owe different fees.
	pub high_water_mark: Nav,
}

/// Assess one holding and charge it if anything is owed — collecting what the free
/// holding can give up and carrying the rest as debt.
///
/// Returns `None` — not an error — for every "nothing to do" case, because the sweeper
/// walks thousands of positions and a fund without a policy is normal, not exceptional.
// The fee plane's three seams, the two ledgers and the marks, plus a four-field request; a
// struct would serve the sweeper's one call site.
#[allow(clippy::too_many_arguments)]
pub async fn assess_position(
	allocations: &dyn AllocationRegistry,
	policies: &dyn FeePolicies,
	accruals: &dyn PositionAccruals,
	assessments: &dyn FeeAssessments,
	ledger: &dyn Ledger,
	money: &dyn MoneyLedger,
	nav: &dyn NavMarks,
	relay: &Notify,
	user: UserId,
	service: ServiceId,
	trigger: Trigger,
	now_unix: i64,
) -> Result<Option<FeeAssessment>, DomainError> {
	if !fee_bearing(allocations, &service).await? {
		return Ok(None);
	}
	let Some((policy, snapshot, price)) = load_assessment_inputs(policies, accruals, ledger, money, nav, user, &service, now_unix).await? else {
		return Ok(None);
	};
	let charge = fees::assess(&policy, &snapshot, price, trigger, now_unix)?;
	if charge.is_empty() {
		return Ok(None);
	}
	let mut assessment = FeeAssessment::record(FeeAssessmentId::new(), user, service, price, trigger, charge)?;
	// Conditional on the accrual clock still being the one we assessed against. Losing
	// that race is a non-event like every other "nothing to do" here: the winner charged
	// this window, and re-charging it would bill the investor twice for the same days.
	if !assessments.charge(&mut assessment, snapshot.accrued_at_unix, now_unix).await? {
		return Ok(None);
	}
	relay.notify_one();
	Ok(Some(assessment))
}

/// What a holding owes right now, without charging it — the disclosure behind a
/// position's net-of-fees value. A fund with no policy owes nothing.
// The fee plane's two read seams, the two ledgers and the marks, plus a three-field request.
#[allow(clippy::too_many_arguments)]
pub async fn accrued_fees(
	allocations: &dyn AllocationRegistry,
	policies: &dyn FeePolicies,
	accruals: &dyn PositionAccruals,
	ledger: &dyn Ledger,
	money: &dyn MoneyLedger,
	nav: &dyn NavMarks,
	user: UserId,
	service: &ServiceId,
	now_unix: i64,
) -> Result<Option<AccruedFees>, DomainError> {
	if !fee_bearing(allocations, service).await? {
		return Ok(None);
	}
	let Some((policy, snapshot, price)) = load_assessment_inputs(policies, accruals, ledger, money, nav, user, service, now_unix).await? else {
		return Ok(None);
	};
	let charge = fees::assess(&policy, &snapshot, price, Trigger::Period, now_unix)?;
	Ok(Some(disclose(&charge, snapshot.high_water_mark)))
}

/// Whether `service` pays fees to the `fee` allocation: only a product whose cash lives in
/// invest's own scope does. In any other scope the fee is banking's (it sweeps `(<scope>,
/// fee)` into `(main, treasury)`), and a unit clawback would hand the `fee` holders a claim
/// on a product that owes them nothing.
async fn fee_bearing(allocations: &dyn AllocationRegistry, service: &ServiceId) -> Result<bool, DomainError> {
	Ok(allocations.scope(service).await?.is_invest())
}

/// The accrued view of a charge: the *accrued* performance figure, never the
/// crystallized one, so a position reads the same on both sides of a period boundary.
fn disclose(charge: &FeeCharge, high_water_mark: Nav) -> AccruedFees {
	let total = charge
		.management
		.checked_add(charge.performance_accrued)
		.and_then(|sum| sum.checked_add(charge.debt_opening))
		.unwrap_or(Usdt::ZERO);
	AccruedFees {
		management: charge.management,
		performance: charge.performance_accrued,
		debt: charge.debt_opening,
		total,
		high_water_mark,
	}
}

/// The three reads every assessment needs. `None` whenever there is nothing to assess:
/// no policy, a policy that charges nothing, or no position.
///
/// Both unit figures come from TigerBeetle, not the projection's copy: the projection
/// lags the relay, and a cap computed from a stale figure could try to claw back units
/// the holder no longer has (which TigerBeetle's non-negative flag would then park).
///
/// The fee is owed on the whole position — the holding's **posted** balance plus what a
/// resting sell has moved into the book's escrow. Posted, not available, because units a
/// queued redemption has reserved are still the holder's until the burn lands. The
/// clawback, by contrast, may only take the holding's **available** balance: the escrow
/// belongs to the order until the book releases it, and the reserve to the redemption,
/// so the charge defers into debt instead of racing either.
// Every port an assessment reads; a struct would serve these two callers only.
#[allow(clippy::too_many_arguments)]
async fn load_assessment_inputs(
	policies: &dyn FeePolicies,
	accruals: &dyn PositionAccruals,
	ledger: &dyn Ledger,
	money: &dyn MoneyLedger,
	nav: &dyn NavMarks,
	user: UserId,
	service: &ServiceId,
	now_unix: i64,
) -> Result<Option<(FeePolicy, PositionSnapshot, Nav)>, DomainError> {
	let Some(policy) = policies.find(service).await? else {
		return Ok(None);
	};
	if policy.is_zero() {
		return Ok(None);
	}
	let Some(accrual) = accruals.find(user, service).await? else {
		return Ok(None);
	};
	let price = dealing_nav(nav, ledger, money, service, now_unix).await?;
	let holding = ledger.balance(&LedgerAccountKey::UserShares(service.clone(), user)).await?;
	let escrowed = ledger.balance(&LedgerAccountKey::BookShares(service.clone(), user)).await?.posted;
	let units = Shares::from_base_units(holding.posted)
		.checked_add(Shares::from_base_units(escrowed))
		.ok_or_else(|| DomainError::Validation("position units overflow".into()))?;
	let snapshot = PositionSnapshot {
		units,
		collectable: Shares::from_base_units(holding.available()),
		cost_basis: accrual.cost_basis,
		high_water_mark: accrual.high_water_mark,
		debt: accrual.debt,
		accrued_at_unix: accrual.accrued_at_unix,
		crystallized_at_unix: accrual.crystallized_at_unix,
	};
	Ok(Some((policy, snapshot, price)))
}

/// The driven ports a change of terms borrows.
pub struct FeePolicyPorts<'a> {
	pub policies: &'a dyn FeePolicies,
	pub changes: &'a dyn FeePolicyChanges,
	pub allocations: &'a dyn AllocationRegistry,
	/// Where a change the owners decide is proposed, and its proposal withdrawn.
	pub approvals: &'a dyn Approvals,
	/// The asker's `auth_subject`, which a proposal names.
	pub users: &'a dyn UserDirectory,
}

impl FeePolicyPorts<'_> {
	fn approval_ports(&self) -> FeeApprovalPorts<'_> {
		FeeApprovalPorts {
			approvals: self.approvals,
			changes: self.changes,
			policies: self.policies,
			users: self.users,
		}
	}
}

/// A change of terms as the operator asks for it.
pub struct PolicyChangeRequest {
	pub service: ServiceId,
	pub policy: FeePolicy,
	/// `0` = as soon as the notice period allows.
	pub requested_effective_from_unix: i64,
	/// Why, in the requester's words. Required when the owners must approve.
	pub reason: String,
}

/// Propose a change of a fund's fee terms (operator).
///
/// The requirement is decided by the pure rule in [`fees::requirement_for`]: the owners
/// exactly when the change tightens the terms beyond the house envelope, one
/// `AllocationManage` holder otherwise. An administrator's change is scheduled at once and
/// a notice is recorded for every holder; the terms bind no earlier than the notice period
/// while anyone holds units, and a dearer change binds over a holder only once they were
/// told or an operator acknowledged them.
///
/// A change the owners decide is recorded `awaiting_consilium` and proposed to them as
/// concierge's `invest:fee_policy` ([`fee_approvals`]); the notice period starts when they
/// pass it. concierge refusing the proposal closes the change `rejected` and answers
/// [`DomainError::Precondition`] with concierge's reason. concierge out of reach leaves the
/// change waiting, and the approvals round proposes it later.
pub async fn schedule_policy(ports: &FeePolicyPorts<'_>, requester: UserId, request: PolicyChangeRequest, now: i64) -> Result<FeePolicyChange, DomainError> {
	// The registry is the same gate subscribe runs through: terms for an unregistered
	// product are always a typo.
	allocations_app::get(ports.allocations, &request.service).await?;
	// A reserved allocation charges no fee: a charge moves units into the product's fee
	// class, which the `fee` allocation holds — and `fee` cannot hold itself (the holder
	// graph is reserved → product, one hop), so the units would have no holder.
	if request.service.is_reserved() {
		return Err(DomainError::Validation(format!("'{}' is a reserved allocation and charges no fee", request.service)));
	}
	// Terms that are never charged would only mislead holders (and owe them a notice).
	if !request.policy.is_zero() && !fee_bearing(ports.allocations, &request.service).await? {
		return Err(DomainError::Precondition(format!(
			"'{}' lives outside invest's scope: its fees go to banking, not to the fee allocation, so it takes no fee terms",
			request.service
		)));
	}
	if request.requested_effective_from_unix > now.saturating_add(fees::MAX_EFFECTIVE_FROM_HORIZON_SECS) {
		return Err(DomainError::Validation("effective_from may be at most 366 days ahead".into()));
	}
	let current = ports.policies.find(&request.service).await?;
	let requirement = fees::requirement_for(current.as_ref(), &request.policy);
	fees::validate_reason(&request.reason, requirement == ChangeRequirement::OwnerConsilium)?;
	let change = NewFeePolicyChange {
		id: FeePolicyChangeId::new(),
		service: request.service.clone(),
		policy: request.policy,
		requirement,
		requested_effective_from_unix: request.requested_effective_from_unix,
		requested_by: requester.to_string(),
		reason: request.reason.clone(),
		now_unix: now,
	};
	let stored = ports.changes.schedule(&change).await?;
	match requirement {
		ChangeRequirement::Admin => Ok(stored),
		ChangeRequirement::OwnerConsilium => match fee_approvals::propose(&ports.approval_ports(), stored).await? {
			Proposed::Opened(change) | Proposed::Later(change) => Ok(change),
			Proposed::Refused { why, .. } => Err(DomainError::Precondition(why)),
		},
	}
}

/// Withdraw a pending change (operator). The change is closed here first, so the operator's
/// word holds at once; the proposal of an owner change is then withdrawn in concierge, or by
/// the approvals round when concierge does not answer now.
pub async fn cancel_change(ports: &FeePolicyPorts<'_>, service: &ServiceId, id: FeePolicyChangeId, by: UserId, now: i64) -> Result<FeePolicyChange, DomainError> {
	ports.changes.find(id).await?.filter(|change| &change.service == service).ok_or_else(|| DomainError::NotFound {
		entity: "fee policy change",
		id: id.to_string(),
	})?;
	let cancelled = ports.changes.cancel(service, id, &by.to_string(), now).await?;
	fee_approvals::withdraw_cancelled(&ports.approval_ports(), &cancelled).await?;
	// Re-read: the withdrawal notes concierge's state on the row.
	Ok(ports.changes.find(id).await?.unwrap_or(cancelled))
}

/// Take responsibility for the holders of a scheduled tightening who could not be told
/// (operator): the terms then bind over exactly the holders whose notice was given up on
/// at this moment, on the record — who, when, and whom for. The move is refused to anyone
/// but the one who asked for the change: the holders' notice is the protection the notice
/// period exists for, and waiving it must not be one `AllocationManage` holder away from a
/// change somebody else proposed. Nothing to acknowledge (a loosening, a change not
/// scheduled, no notice given up on yet) is a conflict rather than a no-op — see
/// [`FeePolicyChanges::acknowledge_undelivered_notices`].
// TODO(concierge approvals): banking also let any fund owner acknowledge; invest learns who
// the owners are from concierge, not yet wired.
pub async fn acknowledge_undelivered_notices(changes: &dyn FeePolicyChanges, service: &ServiceId, id: FeePolicyChangeId, by: UserId, now: i64) -> Result<FeePolicyChange, DomainError> {
	let change = changes.find(id).await?.filter(|change| &change.service == service).ok_or_else(|| DomainError::NotFound {
		entity: "fee policy change",
		id: id.to_string(),
	})?;
	if change.requested_by != by.to_string() {
		return Err(DomainError::Forbidden("the holders' notice may be waived only by the one who requested the change".into()));
	}
	changes.acknowledge_undelivered_notices(service, id, &by.to_string(), now).await
}

/// Who is reading a product's terms. A product hidden from `caller` answers as unregistered
/// — `NotFound`, exactly as `GetAllocation` and `GetFundNav` answer — unless `unrestricted`
/// (an `AllocationManage` holder, decided at the boundary): the terms are part of what the
/// product IS, and a product a caller cannot see has no terms to show them either.
pub struct PolicyReader<'a> {
	pub allocations: &'a dyn AllocationRegistry,
	pub caller: UserId,
	pub unrestricted: bool,
}

impl PolicyReader<'_> {
	async fn require_visible(&self, service: &ServiceId) -> Result<(), DomainError> {
		allocations_app::get_for(self.allocations, service, self.caller, self.unrestricted).await.map(|_| ())
	}

	/// The products whose terms this reader may list: every one for an unrestricted
	/// reader, else the catalog as they see it — the same set `ListAllocations` shows them.
	async fn visible_set(&self) -> Result<Option<HashSet<ServiceId>>, DomainError> {
		if self.unrestricted {
			return Ok(None);
		}
		let catalog = allocations_app::list_for(self.allocations, self.caller, false).await?;
		Ok(Some(catalog.into_iter().map(|record| record.allocation.service().clone()).collect()))
	}
}

/// A product's whole history of terms, newest version first.
pub async fn list_changes(changes: &dyn FeePolicyChanges, reader: &PolicyReader<'_>, service: &ServiceId) -> Result<Vec<FeePolicyChange>, DomainError> {
	reader.require_visible(service).await?;
	changes.list(service).await
}

/// The live terms of a product together with the change on its way, if any — what every
/// reader of a policy is shown, holder or not: the terms they are on, and the terms coming.
pub struct PolicyView {
	pub current: Option<PolicyRecord>,
	pub pending: Option<FeePolicyChange>,
}

pub async fn policy_view(policies: &dyn FeePolicies, changes: &dyn FeePolicyChanges, reader: &PolicyReader<'_>, service: &ServiceId) -> Result<PolicyView, DomainError> {
	reader.require_visible(service).await?;
	Ok(PolicyView {
		current: policies.current(service).await?,
		pending: changes.pending(service).await?,
	})
}

/// One fund's terms, or `None` when the product charges nothing.
pub async fn get_policy(policies: &dyn FeePolicies, service: &ServiceId) -> Result<Option<FeePolicy>, DomainError> {
	policies.find(service).await
}

/// Every configured policy the reader may see, each with its pending change.
pub async fn list_policies(policies: &dyn FeePolicies, changes: &dyn FeePolicyChanges, reader: &PolicyReader<'_>) -> Result<Vec<(ServiceId, PolicyView)>, DomainError> {
	let visible = reader.visible_set().await?;
	let mut views = Vec::new();
	for (service, current) in policies.list().await? {
		if visible.as_ref().is_some_and(|visible| !visible.contains(&service)) {
			continue;
		}
		let pending = changes.pending(&service).await?;
		views.push((service, PolicyView { current: Some(current), pending }));
	}
	Ok(views)
}

/// How many consecutive failed promotions of one change turn the per-tick warning into an
/// error: ten minutes of the same change refusing to land is no longer a transient.
pub const PROMOTION_FAILURES_BEFORE_ERROR: u32 = 10;

/// Promote every scheduled change whose moment has come. Driven by the fee sweeper; returns
/// how many were promoted. A failure on one product warns and moves on — the change stays
/// `scheduled` and the next tick retries it — so one product's broken position cannot hold
/// every other product's terms hostage. `failures` is the sweeper's memory of consecutive
/// failures per change: past [`PROMOTION_FAILURES_BEFORE_ERROR`] the line is an error, with
/// the database's own words, because a change that will never land is an operator's problem
/// and a warning per minute is how such a problem hides in a log.
pub async fn promote_due(changes: &dyn FeePolicyChanges, now: i64, failures: &mut HashMap<FeePolicyChangeId, u32>) -> Result<usize, DomainError> {
	let mut promoted = 0usize;
	for id in changes.due(now).await? {
		match changes.promote(id, now).await {
			Ok(true) => {
				promoted += 1;
				failures.remove(&id);
			}
			Ok(false) => {
				failures.remove(&id);
			}
			Err(err) => {
				let streak = failures.entry(id).or_insert(0);
				*streak = streak.saturating_add(1);
				if *streak >= PROMOTION_FAILURES_BEFORE_ERROR {
					tracing::error!(change_id = %id, consecutive_failures = *streak, "fee policy: a scheduled change keeps failing to promote: {err}");
				} else {
					tracing::warn!(change_id = %id, consecutive_failures = *streak, "fee policy: could not promote a scheduled change (will retry): {err}");
				}
			}
		}
	}
	Ok(promoted)
}

/// One investor's fee statement, newest first.
pub async fn list_assessments(assessments: &dyn FeeAssessments, user: UserId) -> Result<Vec<AssessmentRecord>, DomainError> {
	assessments.list_by_user(user).await
}

/// Every charge against one fund (operator).
pub async fn list_fund_assessments(assessments: &dyn FeeAssessments, service: &ServiceId) -> Result<Vec<AssessmentRecord>, DomainError> {
	assessments.list_by_service(service).await
}

/// The manager's uncollected fee units in a fund, and what they are worth right now.
pub async fn fee_shares(ledger: &dyn Ledger, money: &dyn MoneyLedger, nav: &dyn NavMarks, service: &ServiceId, now_unix: i64) -> Result<(Shares, Usdt), DomainError> {
	let units = Shares::from_base_units(ledger.balance(&LedgerAccountKey::FeeShares(service.clone())).await?.available());
	let price = dealing_nav(nav, ledger, money, service, now_unix).await?;
	Ok((units, price.value(units)?))
}

/// Convert accumulated fee units into the `fee` allocation's cash (operator) — the
/// product buying its fee class back from the `fee` allocation at the day's NAV: the cash
/// is swept from `(invest, alloc:<product>)` into `(invest, fee)`, which banking forwards to
/// `(invest, alloc:fee)` (D24(3)). A product in another scope has no fee units to settle
/// ([`DomainError::Precondition`]): its fees are banking's, and burning units against a sweep
/// into `(<scope>, fee)` would pay the `fee` holders nothing. `units` defaults to the whole
/// accumulated balance.
///
/// This is the **only** operation in the fee plane that moves cash, and it runs once per
/// period for a whole fund rather than once per investor — which is the entire point of
/// collecting in units. It is Read-First gated on the product's account covering the payout
/// and **refuses** when short rather than queueing: unlike an investor's redemption,
/// nobody is waiting on this, and a fee that cannot be paid today is simply left
/// accumulating as units at no cost — the `fee` allocation's holders keep their claim
/// on the product through the units, whose value its NAV already carries.
///
/// # The queue is reserved before the manager is paid
///
/// Queuing a redemption reserves the investor's *units* (a pending burn) but **no cash** —
/// there is none to reserve, because the redemption is priced at settle. So the product's
/// account carries no trace of what the queue will shortly cost, and a settlement gated only
/// on `available` would happily hand the manager money the queue is about to need, leaving
/// investors unpayable while the fee revenue sits collected.
///
/// This is the ordinary fund-accounting holdback: accrued-but-unpaid fees are a liability
/// of the fund, and the manager draws only what is left once the fund's obligations are
/// covered. So the queue is priced at the same dealing NAV and reserved first. It is an
/// estimate — the queue settles at whatever mark is posted then, not this one — but it is
/// the only honest estimate available, and erring toward the investor is the direction
/// every rounding decision in this plane already leans.
///
/// # The settler is bound by the redeem cooldown
///
/// The settlement prices the fee class at the product's dealing NAV and turns it into cash
/// on the `fee` allocation — cash the `fee` holders are then paid out of. An administrator who marks
/// the product up and settles at once has converted their own mark into the fee holders'
/// (very possibly their own) cash, which is the move the redeem cooldown exists to stop one
/// step later. So the same cooldown binds the settler (#245, M-1): within
/// [`VALUATION_REDEEM_COOLDOWN_SECS`] of their own mark, the settlement is refused.
// Four ports, the relay and a four-field request; a struct would serve this one call site.
#[allow(clippy::too_many_arguments)]
pub async fn settle_fee_shares(
	settlements: &dyn FeeSettlements,
	allocations: &dyn AllocationRegistry,
	ledger: &dyn Ledger,
	money: &dyn MoneyLedger,
	nav: &dyn NavMarks,
	redemptions: &dyn RedemptionRepository,
	relay: &Notify,
	service: ServiceId,
	units: Option<Shares>,
	idempotency_key: &IdempotencyKey,
	settler: UserId,
	now_unix: i64,
) -> Result<FeeSettlement, DomainError> {
	// The key names the settlement: a retried request asks for the one it already made,
	// and gets it back without a second sweep.
	let id = settlement_id(&service, idempotency_key);
	if let Some(existing) = settlements.find(id).await? {
		if units.is_some_and(|asked| asked != existing.units()) {
			return Err(DomainError::Conflict(format!(
				"idempotency key '{}' already settled {} fee units of '{service}'",
				idempotency_key.as_str(),
				existing.units()
			)));
		}
		return Ok(existing);
	}
	let scope = allocations.scope(&service).await?;
	if !scope.is_invest() {
		return Err(DomainError::Precondition(format!(
			"'{service}' lives in scope '{scope}': its fees go to banking's treasury, so there is nothing to settle into the fee allocation"
		)));
	}
	let held = Shares::from_base_units(ledger.balance(&LedgerAccountKey::FeeShares(service.clone())).await?.available());
	let units = units.unwrap_or(held);
	if units.is_zero() {
		return Err(DomainError::Validation("no fee units to settle".into()));
	}
	if units > held {
		return Err(DomainError::Validation("cannot settle more fee units than the fund has accumulated".into()));
	}
	refuse_recent_poster(nav, ledger, &service, settler, now_unix).await?;
	let price = dealing_nav(nav, ledger, money, &service, now_unix).await?;
	let mut settlement = FeeSettlement::record(id, service.clone(), units, price)?;
	let fund = account_cash(money, &scope, &service).await?;
	let reserved = queued_redemption_cash(redemptions, &service, price).await?;
	let required = settlement
		.cash()
		.checked_add(reserved)
		.ok_or_else(|| DomainError::Repository("fee settlement plus the queue overflows".into()))?;
	if fund < required {
		return Err(DomainError::Validation(if reserved.is_zero() {
			"the fund's account cannot cover this fee settlement — top it up or settle fewer units".into()
		} else {
			format!(
				"the fund's account cannot cover this fee settlement on top of {reserved} USDT of queued redemptions — \
				 settle the queue first, top the fund up, or settle fewer units"
			)
		}));
	}
	let stored = settlements.settle(&mut settlement, &settler.to_string()).await?;
	relay.notify_one();
	Ok(stored)
}

/// The settlement an idempotency key names in a product: a name-based UUID, so the same
/// key always names the same settlement and two products never share one.
fn settlement_id(service: &ServiceId, key: &IdempotencyKey) -> FeeSettlementId {
	// Fixed forever: changing it would let a retry across the change settle twice.
	const NAMESPACE: Uuid = Uuid::from_u128(0x6665_655f_7365_7474_6c65_6d65_6e74_0001);
	FeeSettlementId::from_raw(Uuid::new_v5(&NAMESPACE, format!("{service}:{}", key.as_str()).as_bytes()))
}

/// What the queued redemptions for one fund would cost at `price`.
///
/// The queue read is cross-fund (it backs the operator's single "clear the queue" screen),
/// so it is filtered here rather than growing a second query for one caller.
async fn queued_redemption_cash(redemptions: &dyn RedemptionRepository, service: &ServiceId, price: Nav) -> Result<Usdt, DomainError> {
	let mut total = Usdt::ZERO;
	for queued in redemptions.list_queued().await? {
		if &queued.service != service {
			continue;
		}
		let cash = price.value(queued.units)?;
		total = total.checked_add(cash).ok_or_else(|| DomainError::Repository("queued redemption cash overflows".into()))?;
	}
	Ok(total)
}

/// Settlement history for one fund.
pub async fn list_settlements(settlements: &dyn FeeSettlements, service: &ServiceId) -> Result<Vec<SettlementRecord>, DomainError> {
	settlements.list_by_service(service).await
}
