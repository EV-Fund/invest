//! Claims of an access token the BFF reads for itself: the `token_version` it was issued with,
//! for the session's revocation check, and the `auth_time` of the sign-in behind it, to tell a
//! freshly confirmed sign-in apart.
//!
//! Read, not verified: the token came from concierge's token endpoint over the BFF's own
//! authenticated call and is stored sealed, so nothing here decides whether to *trust* it —
//! invest-core verifies the signature on every call it is presented with. This only asks
//! "which revoke generation is this token from", to set against what `GetMe` says the
//! account is at now. A hand-rolled base64url decode keeps a dependency out for one claim.

use serde_json::Value;

/// The token's `token_version` claim: `Some(0)` for a JWT that carries none (concierge's
/// own reading of a missing claim), `None` for a string that is not a JWT at all — then
/// there is nothing to compare, and the check stands down.
pub fn token_version(access_token: &str) -> Option<u64> {
	let claims = claims(access_token)?;
	match claims.get("token_version") {
		None => Some(0),
		Some(v) => v.as_u64(),
	}
}

/// When the person last proved who they are at concierge (unix seconds): the token's
/// `auth_time` claim. `None` when the token carries none — a concierge that does not state it
/// cannot show the sign-in was fresh, so nothing is taken as fresh.
pub fn auth_time(access_token: &str) -> Option<i64> {
	claims(access_token)?.get("auth_time")?.as_i64()
}

fn claims(access_token: &str) -> Option<serde_json::Map<String, Value>> {
	let payload = access_token.split('.').nth(1)?;
	match serde_json::from_slice(&base64url(payload)?).ok()? {
		Value::Object(claims) => Some(claims),
		_ => None,
	}
}

fn base64url(input: &str) -> Option<Vec<u8>> {
	let mut out = Vec::with_capacity(input.len() * 3 / 4);
	let (mut bits, mut have) = (0u32, 0u32);
	for byte in input.trim_end_matches('=').bytes() {
		let sextet = match byte {
			b'A'..=b'Z' => byte - b'A',
			b'a'..=b'z' => byte - b'a' + 26,
			b'0'..=b'9' => byte - b'0' + 52,
			b'-' => 62,
			b'_' => 63,
			_ => return None,
		};
		bits = (bits << 6) | u32::from(sextet);
		have += 6;
		if have >= 8 {
			have -= 8;
			out.push((bits >> have) as u8);
			bits &= (1 << have) - 1;
		}
	}
	Some(out)
}

#[cfg(test)]
mod tests {
	use super::*;

	// {"sub":"u","token_version":2}, {"sub":"u"}
	const V2: &str = "h.eyJzdWIiOiJ1IiwidG9rZW5fdmVyc2lvbiI6Mn0.s";
	const NONE: &str = "h.eyJzdWIiOiJ1In0.s";

	#[test]
	fn the_claim_is_read_from_the_payload() {
		assert_eq!(token_version(V2), Some(2));
		assert_eq!(token_version(NONE), Some(0), "a missing claim is version 0, as concierge reads it");
	}

	#[test]
	fn the_sign_in_moment_is_read_only_when_stated() {
		// {"sub":"u","auth_time":1760000000}
		assert_eq!(auth_time("h.eyJzdWIiOiJ1IiwiYXV0aF90aW1lIjoxNzYwMDAwMDAwfQ.s"), Some(1_760_000_000));
		assert_eq!(auth_time(NONE), None, "no claim, no fresh sign-in");
		assert_eq!(auth_time("not-a-jwt"), None);
	}

	#[test]
	fn a_string_that_is_not_a_jwt_has_no_version() {
		assert_eq!(token_version("invest-access-token"), None);
		assert_eq!(token_version("a.!!!.c"), None);
		assert_eq!(token_version("a.e30.c"), Some(0), "an empty claim set is still a JWT");
		assert_eq!(token_version(""), None);
	}
}
