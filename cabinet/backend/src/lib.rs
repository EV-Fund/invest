//! The invest cabinet's BFF (S2-13): the browser's only server at
//! `invest.evinvest.ltd/cabinet/api/*`. It signs people in as concierge's relying party
//! `invest`, keeps their session server-side (the `cabinet` schema of the `invest`
//! database), and forwards each request to invest-core over gRPC with the user's access
//! token. It holds no money and no units: invest-core and banking do.

pub mod concierge;
pub mod config;
pub mod cookies;
pub mod crypto;
pub mod db;
pub mod dto;
pub mod error;
pub mod routes;
pub mod session;
pub mod state;

/// The wall clock in unix seconds — the unit concierge states token expiries in. A clock
/// before the epoch is a host misconfiguration; reading it as 0 makes every session look
/// expired, which fails closed.
pub fn now_secs() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
		.unwrap_or(0)
}
