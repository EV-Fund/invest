//! The money port — banking's ledger, as invest sees it (S2-4, plan §3.3–3.4, §6.3).
//!
//! invest holds units; cash is banking's. An allocation's cash sits on a scope account
//! `(<scope>, alloc:<slug>)` that invest opens and owns, and a user's money for invest on
//! their compartment in the scope. [`MoneyLedger`] is everything invest asks of that:
//! open an account, read balances (the NAV input), and the saga steps the money flows run
//! (plan §6.3) — hold a user's cash, capture it into an allocation, void it, pay out, sweep
//! fees. A [`Gateway`]: like TigerBeetle it owns its own atomicity and can never join a
//! Postgres transaction, so every call is made after the control-plane commit and is
//! idempotent by a key the caller stored before the first try.
//!
//! The adapter speaks `scope.v1.ScopeLedgerService` over mTLS with a service token; a
//! fake answers the same contract in memory for tests. An RPC banking does not serve yet
//! (`Charge`, as of the pinned contracts) answers UNIMPLEMENTED, which the adapter reports
//! as [`MoneyLedgerError::Unimplemented`] ("not yet"), never as a refusal.
//!
//! Amounts cross as [`Usdt`]; users as their concierge `auth_subject`, the name banking
//! keys compartments by.

use async_trait::async_trait;
use invest_domain::{architecture::Gateway, balance::ServiceId, error::DomainError, money::Usdt};
use thiserror::Error;

#[async_trait]
pub trait MoneyLedger: Gateway {
	/// Open `(scope, label)` as an account invest owns. A repeat returns the account; a
	/// label another client owns is [`MoneyLedgerError::IdempotencyConflict`].
	async fn open_account(&self, scope: &ScopeId, label: &AccountLabel, key: &MoneyKey) -> Result<ScopeAccount, MoneyLedgerError>;

	/// Balances of accounts invest owns in `scope`, in the order asked, each label once. A
	/// label that is unknown or not invest's is [`MoneyLedgerError::NotFound`].
	async fn balances(&self, scope: &ScopeId, labels: &[AccountLabel]) -> Result<Vec<ScopeAccount>, MoneyLedgerError>;

	/// Hold `amount` of a user's compartment in the scope, under their mandate for invest.
	async fn reserve(&self, request: &ReserveRequest) -> Result<Hold, MoneyLedgerError>;

	/// Move (part of) a hold into an account invest owns, or into another user's
	/// compartment (a book fill). `amount: None` takes everything still held.
	async fn capture(&self, request: &CaptureRequest) -> Result<Hold, MoneyLedgerError>;

	/// Release whatever a hold still holds back to the user.
	async fn void(&self, request: &VoidRequest) -> Result<Hold, MoneyLedgerError>;

	/// One hold invest placed, as it stands — a saga's recovery read after a timeout.
	async fn hold(&self, scope: &ScopeId, hold_id: &HoldId) -> Result<Hold, MoneyLedgerError>;

	/// Pay out of an account invest owns into users' compartments, for an automatic reason
	/// and within banking's automatic ceilings. A batch is atomic.
	async fn payout(&self, request: &PayoutRequest) -> Result<Payout, MoneyLedgerError>;

	/// Sweep fees from an account invest owns into the scope's `fee` collector — the one
	/// automatic transfer between accounts (`FEE_SWEEP`).
	async fn sweep_fees(&self, request: &FeeSweepRequest) -> Result<Transfer, MoneyLedgerError>;

	/// Ask for a payout past the automatic ceilings: banking stores it, opens the owners'
	/// approval itself and executes it once approved. The answer is the stored operation,
	/// normally still awaiting approval.
	async fn request_payout(&self, request: &RequestPayoutRequest) -> Result<PendingOperation, MoneyLedgerError>;

	/// One operation invest requested, as it stands now — the authoritative verdict a money
	/// event only announces.
	async fn pending_operation(&self, scope: &ScopeId, operation_id: &OperationId) -> Result<PendingOperation, MoneyLedgerError>;
}

/// banking's money-event log (`scope.v1.MoneyEvents`) as invest reads it: what happened to
/// the holds it placed, the payouts it made and the operations it requested — including
/// what banking did on its own (a hold expired, a revoked mandate voiding it, the owners
/// deciding). Pulled, never pushed; a [`Gateway`] like the ledger it reports on.
#[async_trait]
pub trait MoneyEventSource: Gateway {
	/// Events after `after_position`, oldest first, at most `limit`, of every scope invest
	/// holds a grant in. `next_position` is the cursor to store — it may run past the last
	/// event when banking filtered the rest out.
	async fn pull(&self, after_position: i64, limit: u32) -> Result<MoneyEventPage, MoneyLedgerError>;
}

/// A scope: concierge's tenant name, `[a-z][a-z0-9_]{0,31}`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ScopeId(String);

impl ScopeId {
	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		let mut bytes = raw.bytes();
		let well_formed = raw.len() <= 32 && bytes.next().is_some_and(|b| b.is_ascii_lowercase()) && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
		if !well_formed {
			return Err(DomainError::Validation(format!("'{raw}' is not a scope id ([a-z][a-z0-9_]{{0,31}})")));
		}
		Ok(Self(raw.to_owned()))
	}

	/// invest's own scope — where the reserved allocations' cash lives (plan §3.3).
	pub fn invest() -> Self {
		Self("invest".to_owned())
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl core::fmt::Display for ScopeId {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		f.write_str(&self.0)
	}
}

/// A scope account's label, `[a-z0-9_:.-]{1,64}`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AccountLabel(String);

impl AccountLabel {
	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		if raw.is_empty() || raw.len() > 64 || !raw.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b':' | b'.' | b'-')) {
			return Err(DomainError::Validation(format!("'{raw}' is not an account label ([a-z0-9_:.-]{{1,64}})")));
		}
		Ok(Self(raw.to_owned()))
	}

	/// The account an allocation's cash sits on: `alloc:<slug>` (plan §3.3). A slug with an
	/// uppercase letter has no such account — banking's labels are lowercase.
	pub fn allocation(service: &ServiceId) -> Result<Self, DomainError> {
		Self::parse(&format!("alloc:{service}"))
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl core::fmt::Display for AccountLabel {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		f.write_str(&self.0)
	}
}

/// The idempotency key of one mutating call, 1..128 chars. Unique per (invest, scope,
/// RPC) on banking's side; a repeat with the same terms returns the original result.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MoneyKey(String);

impl MoneyKey {
	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		if raw.is_empty() || raw.chars().count() > 128 {
			return Err(DomainError::Validation("a money idempotency key is 1..128 chars".into()));
		}
		Ok(Self(raw.to_owned()))
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}
}

/// invest's reference for the business object behind a call: `<type>:<lowercase uuid>`
/// (`subscription:4f0c…`). banking echoes it in results and money events.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SubjectRef(String);

impl SubjectRef {
	pub fn new(kind: &str, id: uuid::Uuid) -> Result<Self, DomainError> {
		let mut bytes = kind.bytes();
		let kind_ok = bytes.next().is_some_and(|b| b.is_ascii_lowercase()) && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
		if !kind_ok {
			return Err(DomainError::Validation(format!("'{kind}' is not a subject type ([a-z][a-z0-9_]*)")));
		}
		Ok(Self(format!("{kind}:{}", id.hyphenated())))
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}
}

/// A hold's id, as banking minted it. Opaque.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct HoldId(pub String);

/// An operation banking stored for an approval (`RequestPayout`), as banking minted it — a
/// lowercase UUID, kept opaque.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OperationId(pub String);

/// A scope account invest owns, as banking reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopeAccount {
	pub scope: ScopeId,
	pub label: AccountLabel,
	pub balance: Usdt,
	/// Unix seconds.
	pub opened_at: i64,
}

#[derive(Clone, Debug)]
pub struct ReserveRequest {
	pub scope: ScopeId,
	pub auth_subject: String,
	pub amount: Usdt,
	/// Seconds until the rest is released automatically; `None` holds until captured or
	/// voided.
	pub ttl_seconds: Option<u32>,
	pub key: MoneyKey,
	pub subject_ref: SubjectRef,
}

/// Where a capture credits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CaptureTo {
	/// An account invest owns in the hold's scope.
	Account(AccountLabel),
	/// Another user's compartment (a book fill's cash leg), by `auth_subject`.
	User(String),
}

#[derive(Clone, Debug)]
pub struct CaptureRequest {
	pub scope: ScopeId,
	pub hold_id: HoldId,
	pub to: CaptureTo,
	/// `None` — everything still held.
	pub amount: Option<Usdt>,
	pub key: MoneyKey,
	pub subject_ref: SubjectRef,
}

#[derive(Clone, Debug)]
pub struct VoidRequest {
	pub scope: ScopeId,
	pub hold_id: HoldId,
	pub key: MoneyKey,
	pub subject_ref: SubjectRef,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HoldState {
	/// Holding `remaining`.
	Active,
	/// `remaining` reached zero by captures.
	Captured,
	/// Released by a void (or the user revoking the mandate).
	Voided,
	/// Released by its TTL.
	Expired,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Hold {
	pub id: HoldId,
	pub scope: ScopeId,
	pub auth_subject: String,
	pub amount: Usdt,
	pub captured: Usdt,
	pub remaining: Usdt,
	pub state: HoldState,
	pub subject_ref: String,
}

/// Why money leaves an allocation for users without an approval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PayoutReason {
	Redemption,
	Book,
	FeeRebate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutLine {
	pub auth_subject: String,
	pub amount: Usdt,
}

#[derive(Clone, Debug)]
pub struct PayoutRequest {
	pub scope: ScopeId,
	pub from: AccountLabel,
	pub lines: Vec<PayoutLine>,
	pub reason: PayoutReason,
	pub key: MoneyKey,
	pub subject_ref: SubjectRef,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Payout {
	pub id: String,
	pub total: Usdt,
}

/// A payout the owners decide — the same lines as a [`PayoutRequest`], with no automatic
/// reason to claim.
#[derive(Clone, Debug)]
pub struct RequestPayoutRequest {
	pub scope: ScopeId,
	pub from: AccountLabel,
	pub lines: Vec<PayoutLine>,
	pub key: MoneyKey,
	pub subject_ref: SubjectRef,
	/// Shown on the approval and in the users' wallet history, at most 140 chars.
	pub memo: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingOperationState {
	/// Stored; the approval is open (or about to be).
	AwaitingApproval,
	/// Approved and executed, once; the cash has moved.
	Executed,
	/// The approvers said no, or the approval lapsed or was never opened.
	Rejected,
	/// Approved, but banking could not execute it.
	Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingOperation {
	pub id: OperationId,
	pub state: PendingOperationState,
	/// invest's own ref, as requested.
	pub subject_ref: String,
	/// banking's machine reason for `Rejected` (`approval_rejected`, `approval_expired`,
	/// `proposal_refused`, …) or `Failed` (`insufficient_funds`, `approval_mismatch`, …).
	pub failure_reason: String,
}

#[derive(Clone, Debug)]
pub struct FeeSweepRequest {
	pub scope: ScopeId,
	pub from: AccountLabel,
	pub amount: Usdt,
	pub key: MoneyKey,
	pub subject_ref: SubjectRef,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transfer {
	pub id: String,
	pub amount: Usdt,
}

/// What banking's money-event log says happened (`scope.v1.MoneyEvent.Kind`). The kinds
/// invest has no use for are named all the same, so that a kind this build does NOT know is
/// [`MoneyEventKind::Unknown`] — parked, never mistaken for one it can ignore.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoneyEventKind {
	AccountOpened,
	CompartmentIn,
	CompartmentOut,
	HoldPlaced,
	HoldCaptured,
	HoldVoided,
	HoldExpired,
	Charged,
	PaidOut,
	Transferred,
	InvoiceCreated,
	InvoicePaid,
	InvoiceCancelled,
	InvoiceExpired,
	MandateGranted,
	MandateRevoked,
	OperationRequested,
	OperationRejected,
	OperationFailed,
	/// A wire value this build cannot name, kept as sent.
	Unknown(i32),
}

/// One money event, with the fields invest reads. Amounts and balances stay with banking:
/// the consumer acts on what happened to a hold or an operation, never on a figure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MoneyEvent {
	/// The unit of at-least-once delivery; dedupe by it.
	pub event_id: String,
	pub position: i64,
	pub scope: String,
	pub kind: MoneyEventKind,
	/// The aggregate: a hold id, a payout id, an operation id, …
	pub ref_id: String,
	/// invest's own ref of the call that caused it (`book_order:<uuid>`, `redemption:<uuid>`);
	/// empty when the user or banking itself did.
	pub subject_ref: String,
	pub failure_reason: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MoneyEventPage {
	pub events: Vec<MoneyEvent>,
	pub next_position: i64,
}

/// The machine reasons banking refuses with (`scope/v1/ledger.proto`, REFUSALS) — the
/// whole status message of a FAILED_PRECONDITION or PERMISSION_DENIED.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Refusal {
	InsufficientFunds,
	MandateRequired,
	ApprovalRequired,
	HoldNotActive,
	ScopeFrozen,
	UserFrozen,
	NotAccountOwner,
	NotGranted,
	/// A reason this build does not know — kept verbatim rather than guessed at.
	Other(String),
}

impl Refusal {
	pub fn parse(reason: &str) -> Self {
		match reason {
			"insufficient_funds" => Self::InsufficientFunds,
			"mandate_required" => Self::MandateRequired,
			"approval_required" => Self::ApprovalRequired,
			"hold_not_active" => Self::HoldNotActive,
			"scope_frozen" => Self::ScopeFrozen,
			"user_frozen" => Self::UserFrozen,
			"not_account_owner" => Self::NotAccountOwner,
			"not_granted" => Self::NotGranted,
			other => Self::Other(other.to_owned()),
		}
	}

	pub fn reason(&self) -> &str {
		match self {
			Self::InsufficientFunds => "insufficient_funds",
			Self::MandateRequired => "mandate_required",
			Self::ApprovalRequired => "approval_required",
			Self::HoldNotActive => "hold_not_active",
			Self::ScopeFrozen => "scope_frozen",
			Self::UserFrozen => "user_frozen",
			Self::NotAccountOwner => "not_account_owner",
			Self::NotGranted => "not_granted",
			Self::Other(reason) => reason,
		}
	}
}

/// What a saga must tell apart: a refusal it reacts to (void, ask for approval), "not
/// yet" from a banking that does not serve the RPC, and a transient failure it retries
/// with the same key.
#[derive(Debug, Error)]
pub enum MoneyLedgerError {
	/// banking refused the operation for a reason the protocol names.
	#[error("money ledger refused: {}", .0.reason())]
	Refused(Refusal),
	/// The account, hold or operation is unknown — or not invest's, which banking answers
	/// the same way.
	#[error("money ledger: not found")]
	NotFound,
	/// The key was used before for different terms, or the label is another client's.
	#[error("money ledger: idempotency key or label already taken: {0}")]
	IdempotencyConflict(String),
	/// banking found the request malformed — a bug on invest's side.
	#[error("money ledger: invalid request: {0}")]
	Invalid(String),
	/// banking does not serve this RPC yet (UNIMPLEMENTED). Not a refusal: retrying later,
	/// once it does, is correct.
	#[error("money ledger: {rpc} is not implemented by banking yet")]
	Unimplemented { rpc: &'static str },
	/// Transient — unreachable, timed out, or no ledger configured. Retry with the same key.
	#[error("money ledger unavailable: {0}")]
	Unavailable(String),
	/// banking answered something the contract does not allow (an unparsable amount).
	#[error("money ledger protocol violation: {0}")]
	Protocol(String),
}

/// The `failure_reason` a subscription or redemption records when a money step did not go
/// through — a short machine code the cabinet can branch on, never prose. banking's refusal
/// reason is kept as banking spelled it (`user_frozen`, `mandate_required`,
/// `hold_not_active`, `insufficient_funds`, `scope_frozen`, …); anything else is one of a
/// fixed few. The detail behind a non-refusal goes to the log, not to the holder.
pub fn failure_reason(err: &MoneyLedgerError) -> String {
	match err {
		MoneyLedgerError::Refused(refusal) => machine_reason(refusal.reason(), "refused"),
		MoneyLedgerError::NotFound => "not_found".to_owned(),
		MoneyLedgerError::IdempotencyConflict(_) => "conflict".to_owned(),
		MoneyLedgerError::Invalid(_) => "invalid_request".to_owned(),
		MoneyLedgerError::Unimplemented { .. } | MoneyLedgerError::Unavailable(_) => "money_ledger_unavailable".to_owned(),
		MoneyLedgerError::Protocol(_) => "protocol_error".to_owned(),
	}
}

/// `raw` when it reads as a machine reason (`[a-z][a-z0-9_]{0,63}`, the shape every reason
/// banking names has), else `fallback` — a reason the holder sees is never free text from
/// the wire.
pub fn machine_reason(raw: &str, fallback: &str) -> String {
	let mut bytes = raw.bytes();
	let machine = raw.len() <= 64 && bytes.next().is_some_and(|b| b.is_ascii_lowercase()) && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
	if machine { raw.to_owned() } else { fallback.to_owned() }
}

impl From<MoneyLedgerError> for DomainError {
	/// For a read on a request path: a refusal is the state standing in the way, an
	/// unknown account is not found; everything else is infrastructure and never reaches
	/// a client verbatim.
	fn from(err: MoneyLedgerError) -> Self {
		match err {
			// A reason banking sends that is not a machine code never reaches a client verbatim.
			MoneyLedgerError::Refused(refusal) => DomainError::Precondition(machine_reason(refusal.reason(), "refused")),
			MoneyLedgerError::NotFound => DomainError::NotFound {
				entity: "scope account",
				id: "money ledger".into(),
			},
			MoneyLedgerError::IdempotencyConflict(detail) => DomainError::Conflict(detail),
			err @ (MoneyLedgerError::Invalid(_) | MoneyLedgerError::Unimplemented { .. } | MoneyLedgerError::Unavailable(_) | MoneyLedgerError::Protocol(_)) =>
				DomainError::Repository(err.to_string()),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn scope_ids_and_labels_follow_banking_s_grammar() {
		assert!(ScopeId::parse("invest").is_ok());
		assert!(ScopeId::parse("sa").is_ok());
		for bad in ["", "Invest", "1sa", "in-vest", "a234567890123456789012345678901234"] {
			assert!(ScopeId::parse(bad).is_err(), "{bad:?}");
		}
		assert_eq!(AccountLabel::allocation(&ServiceId::fee()).unwrap().as_str(), "alloc:fee");
		assert_eq!(AccountLabel::allocation(&ServiceId::parse("service_arb").unwrap()).unwrap().as_str(), "alloc:service_arb");
		assert!(AccountLabel::allocation(&ServiceId::parse("Trading").unwrap()).is_err(), "labels are lowercase");
		assert!(AccountLabel::parse("").is_err());
		assert!(AccountLabel::parse("a b").is_err());
	}

	#[test]
	fn a_subject_ref_is_a_type_and_a_lowercase_uuid() {
		let id = uuid::Uuid::from_u128(0x4f0c_aaaa_bbbb_cccc_dddd_eeee_ffff_0000);
		let subject = SubjectRef::new("subscription", id).unwrap();
		assert_eq!(subject.as_str(), "subscription:4f0caaaa-bbbb-cccc-dddd-eeeeffff0000");
		assert!(SubjectRef::new("Subscription", id).is_err());
		assert!(SubjectRef::new("", id).is_err());
	}

	#[test]
	fn refusal_reasons_round_trip_and_an_unknown_one_is_kept() {
		for reason in [
			"insufficient_funds", "mandate_required", "approval_required", "hold_not_active", "scope_frozen", "user_frozen", "not_account_owner", "not_granted",
		] {
			let refusal = Refusal::parse(reason);
			assert!(!matches!(refusal, Refusal::Other(_)), "{reason}");
			assert_eq!(refusal.reason(), reason);
		}
		assert_eq!(Refusal::parse("over_budget"), Refusal::Other("over_budget".into()));
	}

	#[test]
	fn a_failure_reason_is_a_machine_code_never_prose() {
		for (err, reason) in [
			(MoneyLedgerError::Refused(Refusal::UserFrozen), "user_frozen"),
			(MoneyLedgerError::Refused(Refusal::MandateRequired), "mandate_required"),
			(MoneyLedgerError::Refused(Refusal::HoldNotActive), "hold_not_active"),
			(MoneyLedgerError::Refused(Refusal::InsufficientFunds), "insufficient_funds"),
			(MoneyLedgerError::Refused(Refusal::ScopeFrozen), "scope_frozen"),
			(MoneyLedgerError::Refused(Refusal::ApprovalRequired), "approval_required"),
			(MoneyLedgerError::Refused(Refusal::Other("scope_closed".into())), "scope_closed"),
			(MoneyLedgerError::Refused(Refusal::Other("Not a <reason>".into())), "refused"),
			(MoneyLedgerError::NotFound, "not_found"),
			(MoneyLedgerError::Unavailable("connection reset by 10.0.0.7".into()), "money_ledger_unavailable"),
			(MoneyLedgerError::Unimplemented { rpc: "Payout" }, "money_ledger_unavailable"),
			(MoneyLedgerError::Protocol("amount 'x'".into()), "protocol_error"),
		] {
			assert_eq!(failure_reason(&err), reason, "{err:?}");
		}
		assert_eq!(machine_reason("approval_expired", "x"), "approval_expired");
		assert_eq!(machine_reason("", "approval_rejected"), "approval_rejected");
	}

	#[test]
	fn unimplemented_is_infrastructure_never_a_refusal() {
		let err: DomainError = MoneyLedgerError::Unimplemented { rpc: "Reserve" }.into();
		assert!(matches!(err, DomainError::Repository(_)), "{err:?}");
		let err: DomainError = MoneyLedgerError::Refused(Refusal::ScopeFrozen).into();
		assert!(matches!(err, DomainError::Precondition(ref r) if r == "scope_frozen"), "{err:?}");
	}
}
