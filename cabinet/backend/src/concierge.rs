//! What the BFF asks concierge, as a port: the three calls of a relying party's session.
//! The adapter is concierge's own `evconcierge_client`; tests put an in-memory fake in its
//! place, so the sign-in and rotation paths run without a concierge.

use async_trait::async_trait;
use evconcierge_client::{ClientTokens, ConciergeClient, ConciergeError, Me, PkceVerifier};

#[async_trait]
pub trait IdentityPlane: Send + Sync {
	/// Redeem the code the browser brought back. Single-use; concierge checks the verifier
	/// against the challenge the authorize request carried.
	async fn exchange_code(&self, code: &str, redirect_uri: &str, verifier: &PkceVerifier) -> Result<ClientTokens, ConciergeError>;

	/// Rotate the pair. `Refused` ends the session: signed out on evinvest.ltd, suspended,
	/// or a rotated-out token presented (which concierge reads as theft and revokes).
	async fn refresh(&self, refresh_token: &str) -> Result<ClientTokens, ConciergeError>;

	/// Who the access token belongs to, and the `invest:*` permissions their seat holds now.
	async fn me(&self, access_token: &str) -> Result<Me, ConciergeError>;
}

#[async_trait]
impl IdentityPlane for ConciergeClient {
	async fn exchange_code(&self, code: &str, redirect_uri: &str, verifier: &PkceVerifier) -> Result<ClientTokens, ConciergeError> {
		ConciergeClient::exchange_code(self, code, redirect_uri, verifier).await
	}

	async fn refresh(&self, refresh_token: &str) -> Result<ClientTokens, ConciergeError> {
		self.refresh_client_token(refresh_token).await
	}

	async fn me(&self, access_token: &str) -> Result<Me, ConciergeError> {
		self.get_me(access_token).await
	}
}
