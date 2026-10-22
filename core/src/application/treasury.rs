//! What each allocation holds and who holds it — the console's "held through allocations"
//! table (#36).
//!
//! A read model with no store of its own: every figure comes from the source that owns it,
//! read live. The cash is banking's (`(<scope>, alloc:<slug>)` through the money port),
//! the supply and the holders are the ledger's ([`ownership_app::allocation_ownership`]),
//! the price is [`funds_app::nav_of`] — the reader every position uses, so the table and
//! a holder's card can never disagree. Banking's own treasury is banking's figure (D17)
//! and stays out.
//!
//! The reserved `fee` and `fund` allocations are lines like any product: they are hidden
//! from the catalog, not from the people who run the platform.

use invest_domain::{
	allocations::Allocation,
	balance::ServiceId,
	error::DomainError,
	money::{Shares, Usdt},
};

use crate::{
	application::{
		funds::{self as funds_app, FundPorts, NavQuote},
		issuance::UnitHolding,
		ownership as ownership_app,
	},
	ports::money::ScopeId,
};

/// How many of the largest holders a line carries when the caller does not say. A table
/// cell shows a few and folds the rest into a count; five leaves the screen room to pick.
pub const DEFAULT_TOP_HOLDERS: usize = 5;
/// The most holders one line may carry. A line is one allocation inside a list of all of
/// them, so a caller that wants a full cap table reads `ListUnitHolders` instead.
pub const MAX_TOP_HOLDERS: usize = 1000;

/// One allocation's line of the table.
pub struct AllocationTreasuryLine {
	pub allocation: Allocation,
	pub scope: ScopeId,
	/// The balance of `(<scope>, alloc:<slug>)` in banking; zero for an account not opened.
	pub cash: Usdt,
	/// The settled supply, from the ledger.
	pub units_outstanding: Shares,
	pub price: NavQuote,
	/// Largest first, at most the limit asked for.
	pub top_holders: Vec<UnitHolding>,
	/// Every holder with units, the folded ones included.
	pub holder_count: usize,
}

/// The table: one line for `service`, or for every registered allocation ordered by slug.
///
/// The lines are read one after another, not fanned out: each costs a banking call and two
/// ledger scans, the allocations are a few dozen, and a parallel sweep of all of them
/// would take as many pool connections as there are products.
pub async fn allocation_treasury(ports: &FundPorts<'_>, service: Option<&ServiceId>, holder_limit: usize) -> Result<Vec<AllocationTreasuryLine>, DomainError> {
	if holder_limit == 0 || holder_limit > MAX_TOP_HOLDERS {
		return Err(DomainError::Validation(format!("holder_limit must be 1..={MAX_TOP_HOLDERS}")));
	}
	let allocations = match service {
		Some(service) => vec![ports.allocations.find(service).await?.ok_or_else(|| DomainError::NotFound {
			entity: "allocation",
			id: service.to_string(),
		})?],
		None => ports.allocations.list_all().await?,
	};
	let mut lines = Vec::with_capacity(allocations.len());
	for allocation in allocations {
		lines.push(line(ports, allocation, holder_limit).await?);
	}
	Ok(lines)
}

async fn line(ports: &FundPorts<'_>, allocation: Allocation, holder_limit: usize) -> Result<AllocationTreasuryLine, DomainError> {
	let service = allocation.service().clone();
	let scope = ports.allocations.scope(&service).await?;
	let (cash, price, ownership) = tokio::try_join!(
		funds_app::account_cash(ports.money, &scope, &service),
		funds_app::nav_of(ports.nav, ports.ledger, ports.money, &service),
		ownership_app::allocation_ownership(ports.ledger, service.clone()),
	)?;
	let holder_count = ownership.holders.len();
	let mut top_holders = ownership.holders;
	top_holders.truncate(holder_limit);
	Ok(AllocationTreasuryLine {
		allocation,
		scope,
		cash,
		units_outstanding: ownership.units_outstanding,
		price,
		top_holders,
		holder_count,
	})
}
