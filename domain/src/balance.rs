//! `balance` bounded context — the unit ledger, and where units sit.
//!
//! invest's TigerBeetle cluster carries **one** ledger, [`Ledger::Share`]: fund units,
//! the service currency. Cash is not here — it is banking's, on the scope accounts
//! `(<scope>, alloc:<slug>)` invest reads through its money port (plan §3.3, §6.2). This
//! context is the **pure, wasm-safe** model of the unit chart of accounts: no I/O, no
//! `tigerbeetle` types.
//!
//! Every unit has a holder. A product's supply, `SharesOutstanding`, is by construction
//! the sum of the holdings in it — people's `UserShares` (plus their units resting in a
//! sell, `BookShares`) and the `fee` allocation's `FeeShares` — and the `fee` allocation
//! is itself held by people through its own units (issue #245). A management or
//! performance fee moves units between holders, `Dr FeeShares / Cr UserShares`; supply,
//! and so NAV per unit, does not move.
//!
//! **The numbers are banking's.** The ledger id (3), the account codes (60–65) and the
//! transfer codes are the ones banking's Share ledger used, kept as they were so the
//! units can move over in the final data wave with their history readable under the same
//! codes. A code banking used for cash is reserved here and never reused
//! ([`TransferCode::BANKING_CASH`]).

use ev::architecture::Id;
use serde::{Deserialize, Serialize};

use crate::{error::DomainError, users::UserId};

/// A service's stable identity — an allocation's slug (e.g. `"service_arb"`). Slug-shaped
/// so it is safe in a logical account key and an authorization comparison.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ServiceId(String);

impl ServiceId {
	pub fn parse(raw: &str) -> Result<Self, DomainError> {
		let value = raw.trim();
		if value.is_empty() || value.len() > 64 {
			return Err(DomainError::Validation("service id must be 1..64 chars".into()));
		}
		if !value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
			return Err(DomainError::Validation("service id must be alphanumeric/-/_".into()));
		}
		Ok(Self(value.to_owned()))
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}

	/// The **fee allocation** — the allocations' earned money as a product of its own:
	/// the fee class of every product (the units a 2-and-20 charge moves) plus the cash
	/// those units settle into. Hidden, never listed, and held by people through units
	/// like any other allocation (issue #245).
	pub fn fee() -> Self {
		Self(RESERVED_FEE.to_owned())
	}

	/// The **fund allocation** — the platform's own capital as a product of its own:
	/// seed cash arrives as a subscription into it, and the people who put it in hold
	/// its units. Hidden, never listed (issue #245).
	pub fn fund() -> Self {
		Self(RESERVED_FUND.to_owned())
	}

	/// Whether this slug is one the platform reserves for itself ([`Self::fee`],
	/// [`Self::fund`]). An operator cannot register a product under a reserved slug —
	/// the rows exist from migration `0001` — and only a reserved allocation may hold
	/// units of another product ([`crate::issuance::UnitHolder::Allocation`]).
	pub fn is_reserved(&self) -> bool {
		self.0 == RESERVED_FEE || self.0 == RESERVED_FUND
	}
}

const RESERVED_FEE: &str = "fee";
const RESERVED_FUND: &str = "fund";

impl core::fmt::Display for ServiceId {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		f.write_str(&self.0)
	}
}

/// Identity of one operator valuation mark (a `fund_valuations` row). Caller-minted,
/// like every id.
pub type ValuationId = Id<ValuationTag>;
/// Phantom tag making [`ValuationId`] a distinct, incompatible identity type.
pub struct ValuationTag;

/// A ledger in TigerBeetle (`ledger`). invest's cluster holds units only, so this has one
/// variant; the id is banking's Share ledger id, kept for the data wave.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ledger {
	/// Fund units (shares) — the **service currency**. Units float; their cash value is
	/// `units × NAV`, priced off this ledger, not held on it.
	Share,
}

impl Ledger {
	pub const fn id(self) -> u32 {
		match self {
			Self::Share => 3,
		}
	}
}

/// Account type (TigerBeetle `code`).
///
/// Codes are immutable on the accounts that carry them and part of TigerBeetle's history,
/// so a retired code is **reserved forever, never reused**. `63` named banking's company
/// stake (#245) and is reserved here for the same reason it is there. `65` was banking's
/// cash escrow for a resting buy — cash invest's ledger does not hold, so no key derives
/// it; it stays in the enum so the number is never handed to another kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountCode {
	UserShares,
	SharesOutstanding,
	FeeShares,
	/// Retired (#245) — reserved, never reuse. The company's in-kind stake.
	RetiredCompanyStake,
	BookShares,
	/// banking's cash escrow (`book_cash:<user>`). Reserved: invest's ledger has no cash.
	BookCash,
}

impl AccountCode {
	pub const fn code(self) -> u16 {
		match self {
			Self::UserShares => 60,
			Self::SharesOutstanding => 61,
			Self::FeeShares => 62,
			Self::RetiredCompanyStake => 63,
			Self::BookShares => 64,
			Self::BookCash => 65,
		}
	}

	/// The inverse of [`Self::code`]: the kind a `tb_accounts` row's number names, or
	/// `None` for a number no kind has ever carried here.
	pub const fn from_code(code: u16) -> Option<Self> {
		Some(match code {
			60 => Self::UserShares,
			61 => Self::SharesOutstanding,
			62 => Self::FeeShares,
			63 => Self::RetiredCompanyStake,
			64 => Self::BookShares,
			65 => Self::BookCash,
			_ => return None,
		})
	}

	/// Whether no live key derives this kind: a row of it, if one ever arrives with the
	/// data wave, names an account that exists and holds zero.
	pub const fn is_retired(self) -> bool {
		matches!(self, Self::RetiredCompanyStake | Self::BookCash)
	}
}

/// Which side an account's balance is normal on — drives the non-negative flag and
/// how a posted/pending balance is computed from `debits`/`credits`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Normal {
	/// A holding: balance = `debits − credits`; guard with `CreditsMustNotExceedDebits`.
	Debit,
	/// The supply contra: balance = `credits − debits`; guard with
	/// `DebitsMustNotExceedCredits`.
	Credit,
}

/// Transfer type (TigerBeetle `code`) — classifies a unit movement for audit and
/// reconciliation. Never load-bearing for correctness, only for forensics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferCode {
	/// A subscription's unit mint, paired with a cash capture in banking.
	ShareMint,
	/// A redemption's unit burn, paired with a cash payout in banking.
	ShareBurn,
	/// A fee charge: units from a holder into the product's fee class.
	FeeClawback,
	/// The fee class bought back at NAV: its units burnt as the cash settles.
	FeeSettle,
	/// An in-kind unit issuance: a mint with no cash leg. Its own code rather than
	/// [`Self::ShareMint`] because the two explain `SharesOutstanding` growth in opposite
	/// ways — a `ShareMint` always has cash behind it, an issuance never does — and
	/// reconciliation reading the Share ledger alone must be able to tell them apart.
	UnitIssue,
	/// A sell order committing its units into `BookShares`.
	BookLock,
	/// The unspent part of a sell order handed back.
	BookRelease,
	/// One trade's units leg (`Dr UserShares(buyer) / Cr BookShares(seller)`).
	BookFill,
	/// In-kind units retired: `Dr SharesOutstanding / Cr <holder shares>`. The mirror of
	/// [`Self::UnitIssue`], and its own code for the same reason.
	UnitRetire,
}

impl TransferCode {
	/// Codes banking uses on its cash ledger. A unit transfer never carries one: the
	/// numbering is shared with banking's history, and a code meaning cash there must not
	/// mean units here.
	pub const BANKING_CASH: [u16; 16] = [2, 3, 4, 10, 11, 20, 21, 22, 23, 30, 40, 41, 46, 51, 55, 56];
	/// Codes carried by transfers whose kinds #245 retired in banking: `1`, `52`, `54`.
	pub const RETIRED: [u16; 3] = [1, 52, 54];

	pub const fn code(self) -> u16 {
		match self {
			Self::ShareMint => 42,
			Self::ShareBurn => 43,
			Self::FeeClawback => 44,
			Self::FeeSettle => 45,
			Self::UnitIssue => 47,
			Self::BookLock => 48,
			Self::BookRelease => 49,
			Self::BookFill => 50,
			Self::UnitRetire => 53,
		}
	}
}

/// The logical identity of a ledger account — resolved to a concrete `u128`
/// TigerBeetle id (minted once, stored in the `tb_accounts` map) by the adapter.
/// Carries everything the adapter needs to *create* the account correctly the first
/// time: `ledger`, `code`, and the non-negative flag (all immutable in TB once set).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LedgerAccountKey {
	/// A user's unit holding in a fund (debit-normal). One per `(service, user)`. Its
	/// `credits_must_not_exceed_debits` flag is what makes a burn (a credit, including a
	/// *pending* reserve) that exceeds the user's minted units rejected atomically by
	/// TigerBeetle — the over-redeem backstop.
	UserShares(ServiceId, UserId),
	/// A fund's total units in circulation (credit-normal). One per service. By
	/// construction:
	///
	/// `SharesOutstanding(svc) == Σ_user UserShares(svc, user) + Σ_user BookShares(svc, user)
	/// + FeeShares(svc)`
	///
	/// Every term is a person's holding or the `fee` allocation's, whose holders are
	/// people, so the supply always adds up to owners.
	SharesOutstanding(ServiceId),
	/// The product's fee class: the units the `fee` allocation holds in a fund
	/// (debit-normal). One per service — a holder of units exactly like a user, which is
	/// the point: a management or performance fee is charged by moving units *between
	/// holders* (`Dr FeeShares / Cr UserShares`), never by moving cash and never by
	/// minting. It is
	/// [`UnitHolder::Allocation(fee)`](crate::issuance::UnitHolder::Allocation)'s
	/// `shares_key` in every product.
	FeeShares(ServiceId),
	/// A user's units committed to resting sell orders on a fund's book (debit-normal).
	/// One per `(service, user)`. The user still owns these units — they count in the
	/// supply invariant and in the position view as "units in orders" — but they cannot
	/// be redeemed or sold twice while they sit here.
	BookShares(ServiceId, UserId),
}

impl LedgerAccountKey {
	/// Stable string key for the `tb_accounts` id-map (and the idempotent create). The
	/// same strings banking wrote, so a migrated map row reads back unchanged.
	pub fn logical_key(&self) -> String {
		match self {
			Self::UserShares(service, user) => format!("shares:{service}:{user}"),
			Self::SharesOutstanding(service) => format!("shares_outstanding:{service}"),
			Self::FeeShares(service) => format!("shares_fee:{service}"),
			Self::BookShares(service, user) => format!("book_shares:{service}:{user}"),
		}
	}

	pub fn ledger(&self) -> Ledger {
		match self {
			Self::UserShares(..) | Self::SharesOutstanding(_) | Self::FeeShares(_) | Self::BookShares(..) => Ledger::Share,
		}
	}

	pub fn account_code(&self) -> AccountCode {
		match self {
			Self::UserShares(..) => AccountCode::UserShares,
			Self::SharesOutstanding(_) => AccountCode::SharesOutstanding,
			Self::FeeShares(_) => AccountCode::FeeShares,
			Self::BookShares(..) => AccountCode::BookShares,
		}
	}

	/// Every unit holding — the book's unit escrow included — is debit-normal; the
	/// units-outstanding contra is credit-normal.
	pub fn normal(&self) -> Normal {
		match self {
			Self::UserShares(..) | Self::FeeShares(_) | Self::BookShares(..) => Normal::Debit,
			Self::SharesOutstanding(_) => Normal::Credit,
		}
	}

	/// The inverse of [`Self::logical_key`]: read a `tb_accounts` row's key back into
	/// the account it names. Total over every string `logical_key` can produce, and an
	/// error for anything else, so a foreign or corrupt row is reported rather than
	/// silently attributed to an account it is not.
	pub fn parse_logical_key(raw: &str) -> Result<Self, DomainError> {
		fn service_and_user(rest: &str) -> Result<(ServiceId, UserId), DomainError> {
			// Neither a service slug nor a user id contains ':', so the one ':' in the
			// remainder is the split point.
			let (service, user) = rest.split_once(':').ok_or_else(|| DomainError::Validation(format!("malformed holding key: {rest}")))?;
			Ok((ServiceId::parse(service)?, user_id(user)?))
		}
		let (prefix, rest) = raw.split_once(':').ok_or_else(|| DomainError::Validation(format!("unknown logical key: {raw}")))?;
		match prefix {
			"shares" => service_and_user(rest).map(|(service, user)| Self::UserShares(service, user)),
			"shares_outstanding" => ServiceId::parse(rest).map(Self::SharesOutstanding),
			"shares_fee" => ServiceId::parse(rest).map(Self::FeeShares),
			"book_shares" => service_and_user(rest).map(|(service, user)| Self::BookShares(service, user)),
			_ => Err(DomainError::Validation(format!("unknown logical key: {raw}"))),
		}
	}
}

fn user_id(raw: &str) -> Result<UserId, DomainError> {
	uuid::Uuid::parse_str(raw)
		.map(Id::from_raw)
		.map_err(|_| DomainError::Validation(format!("invalid user id in logical key: {raw}")))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn service_id_validates() {
		assert_eq!(ServiceId::parse(" trading ").unwrap().as_str(), "trading");
		assert!(ServiceId::parse("").is_err());
		assert!(ServiceId::parse("bad space").is_err());
		assert!(ServiceId::parse("real-estate_1").is_ok());
	}

	#[test]
	fn the_reserved_slugs_parse_like_any_other() {
		// `fee` and `fund` are ordinary slugs on the wire and in the registry — what sets
		// them apart is the predicate, not the parser.
		assert_eq!(ServiceId::parse("fee").unwrap(), ServiceId::fee());
		assert_eq!(ServiceId::parse("fund").unwrap(), ServiceId::fund());
		assert!(ServiceId::fee().is_reserved());
		assert!(ServiceId::fund().is_reserved());
		assert!(!ServiceId::parse("trading").unwrap().is_reserved());
		assert!(!ServiceId::parse("fees").unwrap().is_reserved(), "reserved is exact, not a prefix");
	}

	#[test]
	fn share_keys_live_on_the_share_ledger_with_the_right_sides() {
		let uid = UserId::from_raw(uuid::Uuid::nil());
		let svc = ServiceId::parse("trading").unwrap();
		let user_shares = LedgerAccountKey::UserShares(svc.clone(), uid);
		let outstanding = LedgerAccountKey::SharesOutstanding(svc.clone());
		assert_eq!(user_shares.ledger(), Ledger::Share);
		assert_eq!(outstanding.ledger(), Ledger::Share);
		assert_eq!(Ledger::Share.id(), 3, "banking's Share ledger id, kept for the data wave");
		// The user's holding is debit-normal (the over-redeem backstop); supply is credit-normal.
		assert_eq!(user_shares.normal(), Normal::Debit);
		assert_eq!(outstanding.normal(), Normal::Credit);
		assert_eq!(user_shares.logical_key(), "shares:trading:00000000-0000-0000-0000-000000000000");
		assert_eq!(outstanding.logical_key(), "shares_outstanding:trading");
		assert_eq!(
			LedgerAccountKey::UserShares(ServiceId::fee(), uid).logical_key(),
			"shares:fee:00000000-0000-0000-0000-000000000000"
		);
	}

	#[test]
	fn the_fee_class_is_a_unit_holder_like_any_other() {
		let fee = LedgerAccountKey::FeeShares(ServiceId::parse("trading").unwrap());
		// Same ledger and same side as a user's holding — a clawback is a plain
		// holder-to-holder unit transfer, so supply (and therefore NAV) never moves.
		assert_eq!(fee.ledger(), Ledger::Share);
		assert_eq!(fee.normal(), Normal::Debit);
		assert_eq!(fee.account_code(), AccountCode::FeeShares);
		assert_eq!(fee.logical_key(), "shares_fee:trading");
	}

	#[test]
	fn the_book_escrow_sits_beside_the_holding_it_locks() {
		let uid = UserId::from_raw(uuid::Uuid::nil());
		let svc = ServiceId::parse("service_arb").unwrap();
		let units = LedgerAccountKey::BookShares(svc.clone(), uid);
		assert_eq!(units.ledger(), Ledger::Share);
		assert_eq!(units.normal(), Normal::Debit);
		assert_eq!(units.account_code(), AccountCode::BookShares);
		assert_eq!(units.logical_key(), "book_shares:service_arb:00000000-0000-0000-0000-000000000000");
		assert_ne!(units.logical_key(), LedgerAccountKey::UserShares(svc, uid).logical_key());
	}

	#[test]
	fn every_logical_key_parses_back_to_the_account_it_names() {
		let uid = UserId::new();
		let svc = ServiceId::parse("service_arb").unwrap();
		let keys = [
			LedgerAccountKey::UserShares(svc.clone(), uid),
			LedgerAccountKey::UserShares(ServiceId::fee(), uid),
			LedgerAccountKey::SharesOutstanding(svc.clone()),
			LedgerAccountKey::FeeShares(svc.clone()),
			LedgerAccountKey::BookShares(svc, uid),
		];
		for key in keys {
			let raw = key.logical_key();
			assert_eq!(LedgerAccountKey::parse_logical_key(&raw).unwrap(), key, "{raw} does not round-trip");
		}
		// A row the chart of accounts does not know is an error, never a guess — banking's
		// cash keys included: invest's map never holds one.
		for foreign in [
			"",
			"fees",
			"shares:service_arb",
			"shares:service_arb:not-a-uuid",
			"user:00000000-0000-0000-0000-000000000000",
			"service:fee",
			"book_cash:00000000-0000-0000-0000-000000000000",
			"shares_company:service_arb",
		] {
			assert!(LedgerAccountKey::parse_logical_key(foreign).is_err(), "{foreign:?} parsed");
		}
	}

	#[test]
	fn reserved_codes_are_never_derived_by_a_live_key() {
		let uid = UserId::from_raw(uuid::Uuid::nil());
		let svc = ServiceId::parse("trading").unwrap();
		let live = [
			LedgerAccountKey::UserShares(svc.clone(), uid),
			LedgerAccountKey::SharesOutstanding(svc.clone()),
			LedgerAccountKey::FeeShares(svc.clone()),
			LedgerAccountKey::BookShares(svc, uid),
		];
		for key in live {
			assert!(!key.account_code().is_retired(), "{key:?} derives a reserved kind");
		}
		assert!(AccountCode::RetiredCompanyStake.is_retired());
		assert!(AccountCode::BookCash.is_retired());
	}

	#[test]
	// The reserved codes are IN the sets on purpose: a newcomer that picks one collides here.
	fn every_account_and_transfer_code_is_unique() {
		let account_codes = [
			AccountCode::UserShares,
			AccountCode::SharesOutstanding,
			AccountCode::FeeShares,
			AccountCode::RetiredCompanyStake,
			AccountCode::BookShares,
			AccountCode::BookCash,
		]
		.map(AccountCode::code);
		let mut deduped = account_codes.to_vec();
		deduped.sort_unstable();
		deduped.dedup();
		assert_eq!(deduped.len(), account_codes.len(), "an account code is reused");
		for code in account_codes {
			assert_eq!(AccountCode::from_code(code).map(AccountCode::code), Some(code), "code {code} does not read back as itself");
		}
		assert_eq!(AccountCode::from_code(20), None, "banking's cash kinds are not ours");

		let live_transfer_codes = [
			TransferCode::ShareMint,
			TransferCode::ShareBurn,
			TransferCode::FeeClawback,
			TransferCode::FeeSettle,
			TransferCode::UnitIssue,
			TransferCode::BookLock,
			TransferCode::BookRelease,
			TransferCode::BookFill,
			TransferCode::UnitRetire,
		]
		.map(TransferCode::code);
		let transfer_codes: Vec<u16> = live_transfer_codes.iter().copied().chain(TransferCode::RETIRED).chain(TransferCode::BANKING_CASH).collect();
		let mut deduped = transfer_codes.clone();
		deduped.sort_unstable();
		deduped.dedup();
		assert_eq!(deduped.len(), transfer_codes.len(), "a transfer code is reused");
	}
}
