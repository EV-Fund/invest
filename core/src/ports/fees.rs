//! Persistence ports for the fee plane — policy, per-investor accrual state, the
//! charge, and the periodic conversion of accumulated fee units into cash.
//!
//! Three seams, deliberately separate:
//!
//! - [`FeePolicies`] is plain control-plane configuration keyed by `service` — terms,
//!   not money. A service with no row charges nothing.
//! - [`PositionAccruals`] is the read side of the accrual clocks living on the
//!   `fund_positions` projection. It deliberately does **not** return a unit balance:
//!   units are authoritative in TigerBeetle, and a fee capped by a projection that lags
//!   the ledger would be a fee capped by the wrong number. The application joins this
//!   with a live TB read.
//! - [`FeeAssessments`] writes a charge. Its one method is atomic across three things
//!   that must never drift apart — the audit row, the position's new debt/mark/clocks,
//!   and the outbox event the relay turns into the unit clawback.

use std::sync::Arc;

use async_trait::async_trait;
use invest_domain::{
	architecture::Repository,
	balance::ServiceId,
	error::DomainError,
	fees::{ChangeRequirement, FeeAssessment, FeePolicy, FeePolicyChangeId, FeePolicyChangeState, FeeSettlement, FeeSettlementId, Trigger},
	money::{Nav, Shares, Usdt},
	users::UserId,
};
use uuid::Uuid;

use crate::ports::approvals::{ApprovalState, Approvals};

/// The live row of `fee_policies`: the terms in force right now, which version of the
/// product's history they are, and since when.
#[derive(Clone, Copy, Debug)]
pub struct PolicyRecord {
	pub policy: FeePolicy,
	pub version: u32,
	pub effective_from_unix: i64,
	pub updated_at_unix: i64,
}

/// The policy port is READ-ONLY. There is deliberately no `set`: the only writer of
/// `fee_policies` is the promotion of a scheduled [`FeePolicyChange`] (see
/// [`FeePolicyChanges::promote`]), which is what makes the version and the notice period
/// impossible to route around.
#[async_trait]
pub trait FeePolicies: Send + Sync {
	/// One fund's terms. `None` means the product charges no fee at all — the safe
	/// default, so a fee can never appear on a product nobody configured.
	async fn find(&self, service: &ServiceId) -> Result<Option<FeePolicy>, DomainError>;

	/// The same row with its version and timestamps — what the wire shows.
	async fn current(&self, service: &ServiceId) -> Result<Option<PolicyRecord>, DomainError>;

	/// Every configured policy, ordered by `service` — the operator console's view.
	async fn list(&self) -> Result<Vec<(ServiceId, PolicyRecord)>, DomainError>;
}

/// One row of a product's fee-policy history.
#[derive(Clone, Debug)]
pub struct FeePolicyChange {
	pub id: FeePolicyChangeId,
	pub service: ServiceId,
	pub version: u32,
	pub policy: FeePolicy,
	pub state: FeePolicyChangeState,
	pub requirement: ChangeRequirement,
	/// When the terms bind, fixed at scheduling.
	pub effective_from_unix: i64,
	pub requested_by: String,
	pub requested_at_unix: i64,
	/// Why, in the requester's words; empty when none was given on an administrator's change.
	pub reason: String,
	pub scheduled_at_unix: Option<i64>,
	pub applied_at_unix: Option<i64>,
	/// Notices of this change that have not reached a CURRENT holder, how many of those have
	/// been given up on — the figures the tightening gate is decided on — and how many
	/// of the given-up ones no acknowledgement covers yet: what a (further) acknowledgement
	/// would name, and so whether one is worth offering. Counted only while `scheduled`;
	/// zero in every other state, where there is nothing to wait for.
	pub undelivered_notices: u32,
	pub notices_given_up: u32,
	pub notices_unacknowledged: u32,
	/// The operator's acknowledgement of holders who could not be told, if given — see
	/// [`FeePolicyChanges::acknowledge_undelivered_notices`].
	pub notices_waiver: Option<NoticeWaiver>,
	/// The concierge approval an owner change waits on (the `consilium_id` column); `None`
	/// for an administrator's change, and for an owner change not proposed yet.
	pub approval_id: Option<Uuid>,
	/// concierge's state of that approval as invest last read it.
	pub approval_state: Option<ApprovalState>,
	/// The holder count the proposal named. One of the approved fields, so it is kept to
	/// re-check the approval; `None` on any change invest did not propose.
	pub approval_holders: Option<u32>,
}

/// What the owners passed, as the executor re-checked it against the stored change. The
/// store schedules the change only if the row under its lock still says exactly this.
#[derive(Clone, Debug)]
pub struct ApprovedTerms {
	pub approval_id: Uuid,
	pub policy: FeePolicy,
	/// The terms in force when the owners were asked: a change approved against other
	/// terms is not the change they approved.
	pub current: Option<FeePolicy>,
	/// The provisional `effective_from` the proposal named.
	pub effective_from_unix: i64,
	pub holders: Option<u32>,
}

/// An operator taking responsibility for holders whose notice never arrived: who, when, and
/// the ids of exactly the holders whose notice had been given up on by that
/// moment — the latest acknowledgement's, which took over the whole list. A tightening
/// then binds over THESE holders and no other.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoticeWaiver {
	pub by: String,
	pub at_unix: i64,
	pub users: Vec<UserId>,
}

/// A change as the application proposes it. The id is minted by the caller, so a retried
/// request and its first attempt name the same change.
#[derive(Clone, Debug)]
pub struct NewFeePolicyChange {
	pub id: FeePolicyChangeId,
	pub service: ServiceId,
	pub policy: FeePolicy,
	pub requirement: ChangeRequirement,
	/// The operator's requested effective moment; `0` = as soon as the notice allows.
	pub requested_effective_from_unix: i64,
	pub requested_by: String,
	/// Already validated by the application (`invest_domain::fees::validate_reason`).
	pub reason: String,
	pub now_unix: i64,
}

#[async_trait]
pub trait FeePolicyChanges: Send + Sync {
	/// Record a change. An administrator's is `scheduled` at once, its `effective_from` fixed
	/// by the notice rule, and a notice recorded for every holder. One the owners decide is
	/// `awaiting_consilium`, not proposed yet: no notice, a provisional `effective_from`, and
	/// the holder count the proposal will name. Refuses with [`DomainError::Conflict`] when
	/// another change is already pending for the product.
	async fn schedule(&self, change: &NewFeePolicyChange) -> Result<FeePolicyChange, DomainError>;

	/// Link an owner change to the concierge approval proposed for it, with the approval's
	/// state. Recorded whatever the change's state, so a proposal opened for a change that
	/// was cancelled meanwhile is still withdrawn. Idempotent on the same approval; a
	/// conflict when the change already names another.
	async fn record_proposal(&self, id: FeePolicyChangeId, approval: Uuid, state: ApprovalState) -> Result<FeePolicyChange, DomainError>;

	/// The owners carried the change: move it to `scheduled`, start the notice period now,
	/// record the holders' notices. Idempotent on a change already scheduled by this
	/// approval; a conflict when the row, or the terms in force, no longer match `terms`, or
	/// the change is no longer awaiting.
	async fn schedule_approved(&self, id: FeePolicyChangeId, terms: &ApprovedTerms, now_unix: i64) -> Result<FeePolicyChange, DomainError>;

	/// Close an owner change whose approval ended without passing, or could not be carried
	/// out, as `rejected` with `why`; `approval_state` is noted when given. A change no
	/// longer awaiting is returned as it is.
	async fn close_unapproved(&self, id: FeePolicyChangeId, approval_state: Option<ApprovalState>, why: &str) -> Result<FeePolicyChange, DomainError>;

	/// Note concierge's state of the change's approval. A change with no approval is left
	/// alone, and so is one whose noted state has ended (a stale read never overwrites
	/// `executed`) or is `passed` and would go back to `open`.
	async fn note_approval_state(&self, id: FeePolicyChangeId, state: ApprovalState) -> Result<(), DomainError>;

	/// The owner changes still waiting on concierge, oldest first: every `awaiting_consilium`
	/// one, and every cancelled one invest proposed whose approval may still be live.
	async fn watched(&self, limit: u32) -> Result<Vec<FeePolicyChange>, DomainError>;

	/// Withdraw a pending change. Idempotent on an already-cancelled change; a conflict on
	/// any other closed state. The approval of an owner change is concierge's to withdraw:
	/// the caller does that.
	async fn cancel(&self, service: &ServiceId, id: FeePolicyChangeId, by: &str, now_unix: i64) -> Result<FeePolicyChange, DomainError>;

	/// Take responsibility for the holders of a scheduled tightening whose notice has been
	/// GIVEN UP on: record who, when, and the list of those holders on the change, so
	/// that [`FeePolicyChanges::promote`] binds the tightening over them — and over nobody
	/// else. A notice still being tried is not waived: it may yet arrive, and until it is
	/// delivered or given up on its holder keeps holding the change; once given up on, a
	/// later acknowledgement ADDS them to the list, and its caller and moment replace the
	/// record's — they take responsibility for the whole list, which never shrinks. A repeat
	/// that would add nobody returns the record untouched. A conflict on a change that is
	/// not `scheduled` (unless it carries a record, which the repeat then returns), that
	/// only loosens the terms (it binds by itself), or that has no notice given up on yet
	/// — there is nothing to take responsibility for, and an acknowledgement covering
	/// nobody would be a misleading line in the history.
	async fn acknowledge_undelivered_notices(&self, service: &ServiceId, id: FeePolicyChangeId, by: &str, now_unix: i64) -> Result<FeePolicyChange, DomainError>;

	async fn find(&self, id: FeePolicyChangeId) -> Result<Option<FeePolicyChange>, DomainError>;

	/// How many investors hold units of the product right now — who a notice is owed to.
	async fn holder_count(&self, service: &ServiceId) -> Result<u32, DomainError>;

	/// The one change on its way for a product, if any.
	async fn pending(&self, service: &ServiceId) -> Result<Option<FeePolicyChange>, DomainError>;

	/// A product's whole history, newest version first.
	async fn list(&self, service: &ServiceId) -> Result<Vec<FeePolicyChange>, DomainError>;

	/// Scheduled changes whose `effective_from` has passed — the sweeper's work list.
	async fn due(&self, now_unix: i64) -> Result<Vec<FeePolicyChangeId>, DomainError>;

	/// Promote one due change into `fee_policies`, in one transaction: settle every holder's
	/// accrual at the OLD rate as of `effective_from`, then write the new terms, mark the
	/// change `active` and the previous active one `superseded`. `Ok(false)` when the change
	/// is no longer scheduled or not yet due — a sweep racing another is not a failure. A
	/// conflict while a tightening has an undelivered notice to a current holder the
	/// operator has not acknowledged.
	async fn promote(&self, id: FeePolicyChangeId, now_unix: i64) -> Result<bool, DomainError>;
}

/// The control-plane half of a holding's fee state. `units` is absent on purpose — see
/// the module note.
#[derive(Clone, Debug)]
pub struct PositionAccrual {
	pub user: UserId,
	pub service: ServiceId,
	/// Net cash invested (average cost) — the invested-capital base.
	pub cost_basis: Usdt,
	/// This investor's own high-water mark.
	pub high_water_mark: Nav,
	/// Fee assessed but not yet collected.
	pub debt: Usdt,
	/// Unix seconds of the last management accrual.
	pub accrued_at_unix: i64,
	/// Unix seconds of the last performance crystallization.
	pub crystallized_at_unix: i64,
}

#[async_trait]
pub trait PositionAccruals: Send + Sync {
	/// One holding's accrual state, or `None` if the investor has no position.
	async fn find(&self, user: UserId, service: &ServiceId) -> Result<Option<PositionAccrual>, DomainError>;

	/// Unit-holding positions in invest's own scope whose management fee has not been accrued
	/// since `accrued_before_unix`, oldest first, capped at `limit`. Other scopes' positions
	/// owe no fee to the `fee` allocation and are left out here, so they can never crowd
	/// the work queue. The sweeper's work queue:
	/// bounded per cycle so one pass can never monopolise the pool, and ordered so the
	/// longest-unaccrued holding is always served first.
	async fn due(&self, accrued_before_unix: i64, limit: i64) -> Result<Vec<PositionAccrual>, DomainError>;
}

#[async_trait]
pub trait FeeAssessments: Repository<Aggregate = FeeAssessment> {
	/// Persist one charge in a single transaction: the immutable `fee_assessments` audit
	/// row, the position's new `fee_debt` / `high_water_mark` / clocks, and the drained
	/// `Charged` event into `event_log` + `outbox`.
	///
	/// It takes the position row lock, which is the same target a redemption settle
	/// takes — so a fee assessment and a settle can never interleave and each divide a
	/// cost basis by a figure the other is about to change.
	///
	/// `expected_accrued_at_unix` is the accrual clock the caller assessed against, and
	/// the write is conditional on it still being there. `Ok(false)` means it was not:
	/// another assessor charged this holding for the same elapsed window first, so this
	/// charge is abandoned whole rather than billing the investor twice. The lock cannot
	/// substitute for this — it orders the two writes but cannot tell that the second
	/// one's inputs went stale while it waited.
	///
	/// `now_unix` is stamped onto the clocks, so the window the charge was computed for
	/// and the window recorded as charged are the same one.
	async fn charge(&self, assessment: &mut FeeAssessment, expected_accrued_at_unix: i64, now_unix: i64) -> Result<bool, DomainError>;

	/// One investor's charges, newest first — the fee statement.
	async fn list_by_user(&self, user: UserId) -> Result<Vec<AssessmentRecord>, DomainError>;

	/// Every charge against one fund, newest first — the operator's view.
	async fn list_by_service(&self, service: &ServiceId) -> Result<Vec<AssessmentRecord>, DomainError>;
}

/// An audit row: what was owed, what was actually taken, and what deferred. `due` and
/// `charged_cash` differ exactly when a holding could not cover the charge.
#[derive(Clone, Debug)]
pub struct AssessmentRecord {
	pub user: UserId,
	pub service: ServiceId,
	pub trigger: Trigger,
	pub nav: Nav,
	pub management: Usdt,
	pub performance: Usdt,
	pub debt_opening: Usdt,
	pub charged_units: Shares,
	pub charged_cash: Usdt,
	pub debt_carried: Usdt,
	pub high_water_mark: Nav,
	pub assessed_at_unix: i64,
}

#[async_trait]
pub trait FeeSettlements: Repository<Aggregate = FeeSettlement> {
	/// Record a bulk conversion of fee units into cash + its `SharesSettled` event — or,
	/// when a settlement under the same id is already recorded, return that one and record
	/// nothing. Serialized per product, and refused ([`DomainError::Validation`]) when the
	/// fee units free in the ledger, less those undrained settlements already claim, do not
	/// cover it: two settlements must never sweep the same value.
	async fn settle(&self, settlement: &mut FeeSettlement, settled_by: &str) -> Result<FeeSettlement, DomainError>;

	/// One settlement by id.
	async fn find(&self, id: FeeSettlementId) -> Result<Option<FeeSettlement>, DomainError>;

	/// Settlement history for one fund, newest first.
	async fn list_by_service(&self, service: &ServiceId) -> Result<Vec<SettlementRecord>, DomainError>;
}

/// One bulk conversion of accumulated fee units into fee revenue.
#[derive(Clone, Debug)]
pub struct SettlementRecord {
	pub service: ServiceId,
	pub units: Shares,
	pub nav: Nav,
	pub cash: Usdt,
	pub settled_by: String,
	pub settled_at_unix: i64,
}

/// The fee plane's four seams, handed around together because they always travel
/// together: an assessment needs the policy, the accrual clocks, and the charge, and
/// nothing outside the fee plane needs any of them. One field on the app state instead
/// of four, and one parameter instead of four on every handler.
#[derive(Clone)]
pub struct FeePorts {
	pub policies: Arc<dyn FeePolicies>,
	/// The history of the terms and the only path that writes `fee_policies`.
	pub changes: Arc<dyn FeePolicyChanges>,
	/// concierge's approvals, where a change the owners decide is proposed.
	pub approvals: Arc<dyn Approvals>,
	pub accruals: Arc<dyn PositionAccruals>,
	pub assessments: Arc<dyn FeeAssessments>,
	pub settlements: Arc<dyn FeeSettlements>,
}
