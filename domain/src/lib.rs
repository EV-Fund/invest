//! invest's domain crate.
//!
//! The single source of truth for invest's domain types. `invest-core` depends on it,
//! and so may a wasm frontend (it stays wasm-safe). It never depends on the server or
//! any adapter.
//!
//! It carries the cross-cutting [`error::DomainError`], re-exports the `ev`
//! architecture building blocks, and holds invest's bounded contexts: `allocations`
//! (the registry of investable products), `issuance` (an operator minting units in kind,
//! to an investor), `balance` (the unit chart of accounts), `money` (the 18-dp
//! fixed-point amounts), `users` (a holder's id) and `authz` (the `invest:*` permissions).
//!
//! Moved from banking 9ffafce3 (`domain`), cut down to units: cash, wallets, payments
//! and the consilium stay in banking or move to concierge.

/// `as_str` / `parse` for a closed string vocabulary — an enum stored in a column and
/// sent on the wire, whose strings `invest_contracts` pins on the other side. Spelled once
/// so every such enum answers an unknown value the same way: an error naming the
/// vocabulary, never a silent default (a corrupt row must not read as a legal state).
macro_rules! wire_vocabulary {
	($ty:ty, $what:literal, { $($variant:ident => $wire:literal),+ $(,)? }) => {
		impl $ty {
			/// The stored/wire discriminant.
			pub fn as_str(self) -> &'static str {
				match self {
					$(Self::$variant => $wire,)+
				}
			}

			/// Parse the stored/wire form; an unrecognized value is an error.
			pub fn parse(raw: &str) -> Result<Self, $crate::error::DomainError> {
				match raw {
					$($wire => Ok(Self::$variant),)+
					other => Err($crate::error::DomainError::Validation(format!(concat!("unknown ", $what, ": {}"), other))),
				}
			}
		}
	};
}

pub mod error;

pub mod allocations;
pub mod authz;
pub mod balance;
pub mod issuance;
pub mod money;
pub mod users;

/// Re-export of the `architecture` feature of the external `ev` crate — the
/// shared DDD tactical building blocks (`Id`, `Entity`, `AggregateRoot`,
/// `Repository`, `Gateway`, …) — so consumers reach them via
/// `invest_domain::architecture::…` without depending on `ev` directly.
pub use ev::architecture;
