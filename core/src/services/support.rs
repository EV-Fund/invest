//! Cross-cutting helpers shared by every service: the auth gates
//! (`caller_id`/`require_permission`), the domain→status error mapper, id parsers, and
//! the small wire-shape utilities. Each service module owns its own proto mappers.
//!
//! `Result<_, Status>` is tonic's mandated handler signature; `Status` is a large
//! type we don't control, so the large-err lint does not apply in this module.
#![allow(clippy::result_large_err)]

use std::time::{SystemTime, UNIX_EPOCH};

use evconcierge_auth::claims_of;
use invest_domain::{authz::Permission, error::DomainError, users::UserId};
use tonic::{Request, Status};
use uuid::Uuid;

use crate::AppState;

/// The authenticated caller's concierge user id (the access-token `sub`). The auth layer
/// admits access tokens of the `invest` audience only, so `sub` is a person.
pub(super) fn caller_id<T>(request: &Request<T>) -> Result<UserId, Status> {
	let claims = claims_of(request).ok_or_else(|| Status::unauthenticated("missing claims"))?;
	if !claims.is_access() {
		return Err(Status::permission_denied("access token required"));
	}
	Uuid::parse_str(&claims.sub)
		.map(UserId::from_raw)
		.map_err(|_| Status::unauthenticated("subject is not a user id"))
}

/// Gate an RPC on an `invest:*` permission, read from the caller's projected seat (see
/// [`crate::ports::users`]). Every path fails closed: a caller concierge has not told
/// invest about, or whose permissions it has not stated, holds nothing; a suspended one
/// is refused outright; an unreadable projection is `UNAVAILABLE`.
pub(super) async fn require_permission<T>(state: &AppState, request: &Request<T>, permission: Permission) -> Result<(), Status> {
	if holds_permission(state, request, permission).await? {
		Ok(())
	} else {
		Err(Status::permission_denied(format!("requires {}", permission.as_str())))
	}
}

/// [`require_permission`] as a question rather than a gate: the same resolution and the
/// same fail-closed answers for a suspended account or an unreadable projection — only
/// the final "no" comes back as `Ok(false)` instead of `PERMISSION_DENIED`. For handlers
/// that serve everyone and merely *widen* for a permission holder.
///
/// The token's `token_version` is not compared here: the projection does not carry
/// concierge's revoke floor yet, so a revoked token keeps its permissions for the rest of
/// its TTL (minutes). banking compares it on every money RPC; invest will once the user
/// stream brings the floor.
pub(super) async fn holds_permission<T>(state: &AppState, request: &Request<T>, permission: Permission) -> Result<bool, Status> {
	let caller = caller_id(request)?;
	let Some(standing) = state.users.standing(caller).await.map_err(|_| Status::unavailable("internal error"))? else {
		return Ok(false);
	};
	if !standing.active {
		return Err(Status::permission_denied("account is suspended"));
	}
	Ok(standing.may(permission))
}

/// A user an operator's request names — a concierge user id. A malformed one is
/// `INVALID_ARGUMENT`: it is a field of the request, not the caller's own subject.
pub(super) fn parse_target_user(raw: &str) -> Result<UserId, Status> {
	Uuid::parse_str(raw).map(UserId::from_raw).map_err(|_| Status::invalid_argument("invalid user id"))
}

/// Treat an empty proto string field as an absent optional.
pub(super) fn optional(raw: &str) -> Option<&str> {
	if raw.is_empty() { None } else { Some(raw) }
}

/// The wall clock, for the staleness and history windows. A clock before the epoch is a
/// host misconfiguration; reading it as 0 makes every mark look fresh-or-future rather
/// than failing every request.
pub(super) fn unix_now() -> i64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or_default()
}

/// Map a domain error to a gRPC status without leaking control-plane internals.
pub(super) fn map_err(err: DomainError) -> Status {
	match err {
		DomainError::NotFound { .. } => Status::not_found(err.to_string()),
		DomainError::Validation(_) => Status::invalid_argument(err.to_string()),
		DomainError::Forbidden(_) => Status::permission_denied(err.to_string()),
		DomainError::Precondition(_) => Status::failed_precondition(err.to_string()),
		DomainError::Conflict(_) => Status::already_exists(err.to_string()),
		DomainError::Repository(detail) => {
			tracing::error!("repository failure behind a gRPC call: {detail}");
			Status::unavailable("internal error")
		}
	}
}

#[cfg(test)]
mod tests {
	use tonic::Code;

	use super::*;

	#[test]
	fn infrastructure_detail_never_reaches_the_caller() {
		let status = map_err(DomainError::Repository("connection refused to 10.42.0.1:5432".into()));
		assert_eq!(status.code(), Code::Unavailable);
		assert!(!status.message().contains("10.42"), "{}", status.message());
	}

	#[test]
	fn an_operator_naming_a_malformed_user_is_told_so() {
		assert_eq!(parse_target_user("not-a-uuid").unwrap_err().code(), Code::InvalidArgument);
		assert!(parse_target_user("00000000-0000-0000-0000-000000000001").is_ok());
	}
}
