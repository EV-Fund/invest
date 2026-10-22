//! Application layer — use cases, split CQRS-style.
//!
//! - **Command handlers** (write side) build an aggregate and hand it to a single
//!   repository method; that method is its own atomic unit (see [`crate::ports`]) —
//!   it opens the Postgres transaction, mutates the aggregate, and drains its
//!   `EmitsEvents` into the event log in that same transaction. Units move in
//!   TigerBeetle **after** the Postgres commit (Write-Last), via the outbox relay.
//! - **Query handlers** (read side) depend only on the narrow read ports and read
//!   Postgres projections; authoritative balances come from TigerBeetle, and cash from
//!   banking through the money port.
//!
//! Both depend on the ports in [`crate::ports`], never on concrete adapters.
//!
//! [`allocations`] is the pure control-plane registry of investable products — it moves
//! nothing, and its `require_subscribable` gate is what every deal runs before one does.
//! [`issuance`] is the operator's in-kind mint — units to an investor with no cash leg —
//! run through the same registry, NAV and cap gates as a subscription will be.
//! [`ownership`] is the read of who holds an allocation, summed from the ledger, behind
//! the cap table. [`funds`] is the price — NAV marks, the computed price of the reserved
//! allocations — a holder's position at it, and dealing with the fund: subscriptions and
//! redemptions, whose cash is banking's. [`book`] is holders dealing with each other, and
//! [`fees`] the management and performance fee, charged in units and settled into cash.
//! [`treasury`] lines every allocation's cash, supply, price and largest holders up for
//! the console, read live from banking and the ledger.

pub mod allocations;
pub mod book;
pub mod fees;
pub mod funds;
pub mod import;
pub mod issuance;
pub mod ownership;
pub mod treasury;
pub mod window_import;
