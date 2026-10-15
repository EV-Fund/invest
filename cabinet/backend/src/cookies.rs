//! The cookies the BFF mints and reads. Names are `__Host-`-prefixed in production (HTTPS)
//! and bare in local dev, matching the frontend's `shared/config/cookies.ts` and
//! `shared/lib/csrf-client.ts`.
//!
//! `__Host-` pins a cookie to this exact host with `Path=/` and `Secure`: a sibling
//! subdomain of evinvest.ltd cannot set or shadow it, which is what makes the CSRF
//! double-submit sound without binding the token to the session row.

use std::convert::Infallible;

use axum::{
	extract::FromRequestParts,
	http::{HeaderMap, HeaderValue, header, request::Parts},
};

#[derive(Clone, Debug)]
pub struct CookieNames {
	/// The session id — HttpOnly, the only credential the browser holds.
	pub session: String,
	/// The readable double-submit token; mutating routes require it echoed in `x-ev-csrf`.
	pub csrf: String,
	/// The sign-in in flight: the `state` the callback must come back with. HttpOnly,
	/// ten minutes, cleared by the callback whatever it decides.
	pub sign_in: String,
	secure: bool,
}

impl CookieNames {
	pub fn new(secure: bool) -> Self {
		let prefix = if secure { "__Host-" } else { "" };
		Self {
			session: format!("{prefix}invest_session"),
			csrf: format!("{prefix}invest_csrf"),
			sign_in: format!("{prefix}invest_sign_in"),
			secure,
		}
	}

	/// A `Set-Cookie` value. `SameSite=Lax`, not `Strict`: the callback is a top-level
	/// navigation from evinvest.ltd, and a link into the cabinet from the landing must
	/// arrive signed in. Cross-site POSTs are what Lax withholds, and the CSRF pair and the
	/// `Origin` check cover the rest.
	pub fn set(&self, name: &str, value: &str, max_age_secs: i64, http_only: bool) -> HeaderValue {
		let mut cookie = format!("{name}={value}; Path=/; Max-Age={}; SameSite=Lax", max_age_secs.max(0));
		if http_only {
			cookie.push_str("; HttpOnly");
		}
		if self.secure {
			cookie.push_str("; Secure");
		}
		// Names are ours and values are hex or base64url the BFF made, so this cannot fail;
		// if it ever did, a cookie that sets nothing beats a panic mid-response.
		HeaderValue::from_str(&cookie).unwrap_or_else(|_| {
			tracing::error!(cookie = name, "a cookie value is not a valid header value");
			HeaderValue::from_static("invalid=; Max-Age=0")
		})
	}

	pub fn clear(&self, name: &str) -> HeaderValue {
		self.set(name, "", 0, true)
	}
}

/// The request's cookies, parsed once. Values are taken as sent: the ones the BFF reads are
/// all its own and never percent-encoded.
#[derive(Clone, Debug, Default)]
pub struct Cookies(Vec<(String, String)>);

impl Cookies {
	pub fn from_headers(headers: &HeaderMap) -> Self {
		let pairs = headers
			.get_all(header::COOKIE)
			.iter()
			.filter_map(|value| value.to_str().ok())
			.flat_map(|line| line.split(';'))
			.filter_map(|pair| pair.trim().split_once('='))
			.map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
			.collect();
		Self(pairs)
	}

	/// The first cookie of that name with a non-empty value. First, because a browser sends
	/// the most specific path first, and every cookie here has `Path=/`.
	pub fn get(&self, name: &str) -> Option<&str> {
		self.0.iter().find(|(n, v)| n == name && !v.is_empty()).map(|(_, v)| v.as_str())
	}
}

impl<S: Send + Sync> FromRequestParts<S> for Cookies {
	type Rejection = Infallible;

	async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
		Ok(Self::from_headers(&parts.headers))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn production_cookies_are_host_locked_and_secure() {
		let names = CookieNames::new(true);
		assert_eq!(names.session, "__Host-invest_session");
		assert_eq!(names.csrf, "__Host-invest_csrf");
		let set = names.set(&names.session, "abc", 60, true);
		assert_eq!(set.to_str().unwrap(), "__Host-invest_session=abc; Path=/; Max-Age=60; SameSite=Lax; HttpOnly; Secure");
	}

	#[test]
	fn dev_cookies_drop_the_prefix_and_secure() {
		let names = CookieNames::new(false);
		let set = names.set(&names.csrf, "t", 60, false);
		assert_eq!(set.to_str().unwrap(), "invest_csrf=t; Path=/; Max-Age=60; SameSite=Lax");
	}

	#[test]
	fn the_cookie_header_is_read_by_name() {
		let mut headers = HeaderMap::new();
		headers.insert(header::COOKIE, HeaderValue::from_static("a=1; invest_session=s1; empty=; invest_csrf=c1"));
		let jar = Cookies::from_headers(&headers);
		assert_eq!(jar.get("invest_session"), Some("s1"));
		assert_eq!(jar.get("invest_csrf"), Some("c1"));
		assert_eq!(jar.get("empty"), None);
		assert_eq!(jar.get("missing"), None);
	}
}
