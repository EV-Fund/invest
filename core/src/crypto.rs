//! The process-level rustls crypto provider, chosen once at start-up.
//!
//! The dependency graph can enable two rustls providers: `ring` (through tonic) and
//! `aws-lc-rs` (through reqwest, which concierge's auth crate pulls in). rustls refuses to
//! guess between them — banking's first TLS listener panicked on its first handshake with
//! "Could not automatically determine the process-level CryptoProvider" — so `main`
//! installs one before any TLS client (the scope ledger's mTLS) is built.

use rustls::crypto::CryptoProvider;

/// Install `ring` as the process-level provider, unless one is installed already. Ring
/// because tonic's `tls-ring` already makes it the provider every outbound TLS seam uses.
/// Idempotent, so tests that share a process may each call it.
pub fn install_crypto_provider() {
	if CryptoProvider::get_default().is_some() {
		return;
	}
	// `Err` only means another thread installed one between the check and here — a provider
	// is in place either way, which is all this function promises.
	if rustls::crypto::ring::default_provider().install_default().is_err() {
		tracing::debug!("rustls crypto provider was installed concurrently; keeping that one");
	}
}
