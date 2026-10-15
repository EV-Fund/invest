//! `users` — who holds units.
//!
//! invest keeps no account of its own for a person: the identity plane (concierge) owns
//! the account, and invest knows a holder by the concierge user id its tokens carry as
//! `sub`. Keying units by that id directly, rather than by a local surrogate mapped to
//! it, removes the mirror banking needed (`users.concierge_user_id`) and the second
//! lookup every operator command paid to resolve a console's id into a ledger one.
//!
//! Pure and wasm-safe: ids only, no I/O.

use ev::architecture::Id;

/// A holder's id — the concierge user id, the `sub` of an access token for the `invest`
/// relying party.
pub type UserId = Id<UserTag>;
/// Phantom tag making [`UserId`] a distinct, incompatible identity type.
pub struct UserTag;
