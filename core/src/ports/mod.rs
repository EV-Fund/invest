//! Driven ports — the outbound interfaces the application depends on, implemented by
//! `infrastructure`. The hexagonal "domain/port" layer over the generic DDD building
//! blocks in [`invest_domain::architecture`].
//!
//! Persistence ports are use-case-shaped and each method is internally atomic — the
//! aggregate's drained events are written to the event log (and, when they move units,
//! the outbox) in the same transaction as the state change, so callers never juggle a
//! transaction across the port boundary: [`AllocationRegistry`] (the registry of
//! investable products), [`UnitIssuanceRepository`] (an operator's in-kind mint),
//! [`NavMarks`] (the posted AUM marks) and [`FundPositionReader`] (the cost-basis
//! projection).
//!
//! Two gateways ([`Gateway`](invest_domain::architecture::Gateway)) can never join a
//! Postgres transaction, so they are written last, after the commit: [`Ledger`] — the
//! units, in invest's own TigerBeetle cluster — and [`MoneyLedger`] — the cash, in
//! banking.
//!
//! [`UserDirectory`] is the projection of the people concierge knows: who may hold, and
//! which `invest:*` permissions they carry; [`catalog::CatalogPublisher`] tells concierge
//! which permissions those are.
//!
//! The dealing ports follow the same rule: [`SubscriptionRepository`] and
//! [`RedemptionRepository`] record the money sagas, [`BookStore`] the order book, and the
//! [`fees`] seams the fee plane — each write atomic with its outbox row, every unit and
//! cash movement made after the commit.

pub mod allocations;
pub mod book;
pub mod catalog;
pub mod fees;
pub mod issuance;
pub mod ledger;
pub mod money;
pub mod nav;
pub mod positions;
pub mod redemptions;
pub mod subscriptions;
pub mod users;

pub use allocations::{AllocationAccessGrant, AllocationRecord, AllocationRegistry};
pub use issuance::{IssueOutcome, UnitIssuanceRecord, UnitIssuanceRepository};
pub use ledger::{CompletionKind, Ledger, LedgerBalance, LedgerError, LedgerTransfer, PendingCompletion};
pub use money::{MoneyEventSource, MoneyLedger, MoneyLedgerError};
pub use nav::{NavMarks, Valuation};
pub use positions::{FundPosition, FundPositionReader, UnitFlow};
pub use redemptions::{QueuedRedemption, RedemptionRecord, RedemptionRepository};
pub use subscriptions::{SubscriptionRecord, SubscriptionRepository};
pub use users::{UserDirectory, UserStanding};
