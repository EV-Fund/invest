//! Sealing a session's tokens at rest (security review F2).
//!
//! The key is derived from the session cookie itself — HKDF-SHA256 over its 256 random bits
//! — so the database alone opens nothing: a leaked dump or backup holds ciphertext, and the
//! one thing that opens a row is the browser that owns it. Every read and every rotation of
//! a session happens inside a request carrying that cookie, so the server never needs the
//! key at any other time, and there is no key to provision or rotate.
//!
//! ChaCha20-Poly1305 from `ring`, which the TLS stack already links. A token is sealed with
//! a fresh random nonce and bound by associated data to which token it is and to the row's
//! id hash, so neither opens as the other nor on another row.

use color_eyre::eyre::{Report, eyre};
use ring::{
	aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey},
	hkdf::{HKDF_SHA256, Salt},
};

const INFO: &[u8] = b"invest-cabinet/tokens/v1";

/// Which of a session's two tokens.
#[derive(Clone, Copy, Debug)]
pub enum Which {
	Access,
	Refresh,
}

impl Which {
	fn label(self) -> &'static [u8] {
		match self {
			Self::Access => b"access/",
			Self::Refresh => b"refresh/",
		}
	}
}

pub struct TokenKey(LessSafeKey);

impl TokenKey {
	/// The key of the session this cookie names.
	pub fn of_cookie(cookie: &str) -> Result<Self, Report> {
		let prk = Salt::new(HKDF_SHA256, INFO).extract(cookie.as_bytes());
		let okm = prk.expand(&[INFO], &CHACHA20_POLY1305).map_err(|_| eyre!("HKDF refused to expand a token key"))?;
		Ok(Self(LessSafeKey::new(UnboundKey::from(okm))))
	}

	/// `nonce ‖ ciphertext ‖ tag`.
	pub fn seal(&self, which: Which, id_hash: &[u8], token: &str) -> Result<Vec<u8>, Report> {
		let mut nonce = [0u8; NONCE_LEN];
		getrandom::fill(&mut nonce).map_err(|e| eyre!("the OS random source failed: {e}"))?;
		let mut sealed = token.as_bytes().to_vec();
		self.0
			.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(aad(which, id_hash)), &mut sealed)
			.map_err(|_| eyre!("sealing a session token failed"))?;
		Ok([nonce.as_slice(), &sealed].concat())
	}

	/// The token, or an error for anything sealed under another cookie, for another row or
	/// token, or altered since.
	pub fn open(&self, which: Which, id_hash: &[u8], blob: &[u8]) -> Result<String, Report> {
		let (nonce, sealed) = blob.split_at_checked(NONCE_LEN).ok_or_else(|| eyre!("a sealed token is shorter than its nonce"))?;
		let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| eyre!("a sealed token's nonce is malformed"))?;
		let mut buf = sealed.to_vec();
		let plain = self
			.0
			.open_in_place(nonce, Aad::from(aad(which, id_hash)), &mut buf)
			.map_err(|_| eyre!("a session token does not open under this session's key"))?;
		String::from_utf8(plain.to_vec()).map_err(|_| eyre!("a sealed token is not UTF-8"))
	}
}

fn aad(which: Which, id_hash: &[u8]) -> Vec<u8> {
	[INFO, b"/", which.label(), id_hash].concat()
}

#[cfg(test)]
mod tests {
	use super::*;

	const ROW: &[u8] = &[7u8; 32];

	#[test]
	fn a_token_opens_under_its_own_cookie_row_and_kind_only() {
		let key = TokenKey::of_cookie("a".repeat(64).as_str()).unwrap();
		let sealed = key.seal(Which::Access, ROW, "the-access-token").unwrap();
		assert!(!sealed.windows(16).any(|w| w == b"the-access-token"), "nothing in clear");
		assert_eq!(key.open(Which::Access, ROW, &sealed).unwrap(), "the-access-token");

		let other = TokenKey::of_cookie("b".repeat(64).as_str()).unwrap();
		assert!(other.open(Which::Access, ROW, &sealed).is_err(), "another cookie");
		assert!(key.open(Which::Refresh, ROW, &sealed).is_err(), "the other token");
		assert!(key.open(Which::Access, &[8u8; 32], &sealed).is_err(), "another row");
		let mut tampered = sealed.clone();
		*tampered.last_mut().unwrap() ^= 1;
		assert!(key.open(Which::Access, ROW, &tampered).is_err(), "altered");
		assert!(key.open(Which::Access, ROW, &sealed[..5]).is_err(), "truncated");
	}

	#[test]
	fn the_same_token_seals_differently_each_time() {
		let key = TokenKey::of_cookie("c".repeat(64).as_str()).unwrap();
		assert_ne!(key.seal(Which::Refresh, ROW, "t").unwrap(), key.seal(Which::Refresh, ROW, "t").unwrap());
	}
}
