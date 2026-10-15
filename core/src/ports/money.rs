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

impl From<MoneyLedgerError> for DomainError {
	/// For a read on a request path: a refusal is the state standing in the way, an
	/// unknown account is not found; everything else is infrastructure and never reaches
	/// a client verbatim.
	fn from(err: MoneyLedgerError) -> Self {
		match err {
			MoneyLedgerError::Refused(refusal) => DomainError::Precondition(refusal.reason().to_owned()),
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
	fn unimplemented_is_infrastructure_never_a_refusal() {
		let err: DomainError = MoneyLedgerError::Unimplemented { rpc: "Reserve" }.into();
		assert!(matches!(err, DomainError::Repository(_)), "{err:?}");
		let err: DomainError = MoneyLedgerError::Refused(Refusal::ScopeFrozen).into();
		assert!(matches!(err, DomainError::Precondition(ref r) if r == "scope_frozen"), "{err:?}");
	}
}
