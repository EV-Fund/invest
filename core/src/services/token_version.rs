//! Revocation in the auth layer: an access token issued before a "revoke all" is refused.
//!
//! concierge's verifier is stateless — it checks signature, issuer, audience and expiry, and
//! leaves `token_version` to "where the authoritative value is reachable". For invest-core
//! that is the `users` projection, which keeps the highest `token_version` concierge has
//! stated per person. This wrapper compares the two on every call, before any handler runs,
//! so a revoked token stops working on the next request instead of at its expiry.
//!
//! The rule is fail-closed where it can be and permissive only where there is nothing to
//! compare: a token below the threshold is `UNAUTHENTICATED`; an unreadable projection is
//! `UNAVAILABLE` (retryable, never a pass); no threshold stored yet means the token stands.

use std::sync::Arc;

use evconcierge_auth::{AuthError, Authenticate, Claims};
use invest_domain::users::UserId;
use tracing::warn;
use uuid::Uuid;

use crate::ports::users::UserDirectory;

/// `inner`'s verdict, then the revocation threshold on top of it.
#[derive(Clone)]
pub struct TokenVersionGuard<A> {
	inner: A,
	users: Arc<dyn UserDirectory>,
}

impl<A> TokenVersionGuard<A> {
	pub fn new(inner: A, users: Arc<dyn UserDirectory>) -> Self {
		Self { inner, users }
	}
}

impl<A: Authenticate> Authenticate for TokenVersionGuard<A> {
	async fn authenticate(&self, token: String) -> Result<Claims, AuthError> {
		let claims = self.inner.authenticate(token).await?;
		// The threshold is keyed by the person; a `sub` that is not one has no row to compare
		// with and no handler would serve it (`support::caller_id`), so it is refused here
		// rather than waved through unchecked.
		let Ok(subject) = Uuid::parse_str(&claims.sub) else {
			return Err(AuthError::InvalidToken);
		};
		let threshold = self.users.token_version(UserId::from_raw(subject)).await.map_err(|err| {
			warn!(error = %err, "token_version threshold unreadable; request refused as unavailable");
			AuthError::Unavailable
		})?;
		match threshold {
			Some(threshold) if claims.token_version < threshold => Err(AuthError::InvalidToken),
			_ => Ok(claims),
		}
	}
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use async_trait::async_trait;
	use evconcierge_auth::TokenType;
	use invest_domain::error::DomainError;
	use tonic::Code;

	use super::*;
	use crate::ports::users::UserStanding;

	/// A verifier that accepts every token as `claims`.
	#[derive(Clone)]
	struct Accepts(Claims);

	impl Authenticate for Accepts {
		async fn authenticate(&self, _token: String) -> Result<Claims, AuthError> {
			Ok(self.0.clone())
		}
	}

	/// A verifier that refuses every token.
	#[derive(Clone)]
	struct Refuses;

	impl Authenticate for Refuses {
		async fn authenticate(&self, _token: String) -> Result<Claims, AuthError> {
			Err(AuthError::InvalidToken)
		}
	}

	/// Thresholds by user; `None` as the whole map is an unreadable projection.
	struct Thresholds(Option<HashMap<UserId, u64>>);

	#[async_trait]
	impl UserDirectory for Thresholds {
		async fn standing(&self, _user: UserId) -> Result<Option<UserStanding>, DomainError> {
			unreachable!("the guard reads only the threshold")
		}

		async fn token_version(&self, user: UserId) -> Result<Option<u64>, DomainError> {
			match &self.0 {
				Some(map) => Ok(map.get(&user).copied()),
				None => Err(DomainError::Repository("projection down".into())),
			}
		}

		async fn auth_subject(&self, _user: UserId) -> Result<Option<String>, DomainError> {
			unreachable!("the guard reads only the threshold")
		}
	}

	fn claims(user: UserId, token_version: u64) -> Claims {
		Claims {
			sub: user.to_string(),
			iss: "https://auth.concierge.ev".into(),
			aud: "invest".into(),
			exp: u64::MAX,
			iat: 0,
			typ: TokenType::Access,
			jti: None,
			token_version,
		}
	}

	fn guard(claims: Claims, thresholds: Option<HashMap<UserId, u64>>) -> TokenVersionGuard<Accepts> {
		TokenVersionGuard::new(Accepts(claims), Arc::new(Thresholds(thresholds)))
	}

	async fn verdict(g: &TokenVersionGuard<impl Authenticate>) -> Result<Claims, AuthError> {
		g.authenticate("t".into()).await
	}

	#[tokio::test]
	async fn a_token_issued_before_a_revoke_is_refused() {
		let user = UserId::new();
		let g = guard(claims(user, 3), Some(HashMap::from([(user, 4)])));
		let err = verdict(&g).await.unwrap_err();
		assert!(matches!(err, AuthError::InvalidToken));
		assert_eq!(tonic::Status::from(&err).code(), Code::Unauthenticated);
	}

	#[tokio::test]
	async fn a_token_at_or_above_the_threshold_is_accepted() {
		let user = UserId::new();
		assert!(verdict(&guard(claims(user, 4), Some(HashMap::from([(user, 4)])))).await.is_ok(), "at the threshold");
		assert!(verdict(&guard(claims(user, 9), Some(HashMap::from([(user, 4)])))).await.is_ok(), "above it");
	}

	#[tokio::test]
	async fn no_stored_threshold_accepts_the_token() {
		let user = UserId::new();
		assert!(verdict(&guard(claims(user, 0), Some(HashMap::new()))).await.is_ok(), "no row");
		assert!(
			verdict(&guard(claims(user, 0), Some(HashMap::from([(UserId::new(), 7)])))).await.is_ok(),
			"another user's threshold is not this user's"
		);
	}

	#[tokio::test]
	async fn a_token_without_the_claim_reads_as_version_zero_and_is_refused_above_it() {
		// concierge's `Claims` deserialises a missing claim as 0.
		let user = UserId::new();
		assert!(verdict(&guard(claims(user, 0), Some(HashMap::from([(user, 1)])))).await.is_err());
	}

	#[tokio::test]
	async fn an_unreadable_threshold_is_unavailable_not_a_pass() {
		let err = verdict(&guard(claims(UserId::new(), 9), None)).await.unwrap_err();
		assert!(matches!(err, AuthError::Unavailable));
		assert_eq!(tonic::Status::from(&err).code(), Code::Unavailable);
	}

	#[tokio::test]
	async fn the_inner_verdict_stands_and_a_subject_that_is_not_a_user_is_refused() {
		let thresholds: Arc<dyn UserDirectory> = Arc::new(Thresholds(Some(HashMap::new())));
		assert!(matches!(
			TokenVersionGuard::new(Refuses, thresholds.clone()).authenticate("t".into()).await,
			Err(AuthError::InvalidToken)
		));

		let mut odd = claims(UserId::new(), 1);
		odd.sub = "not-a-uuid".into();
		assert!(matches!(
			TokenVersionGuard::new(Accepts(odd), thresholds).authenticate("t".into()).await,
			Err(AuthError::InvalidToken)
		));
	}
}
