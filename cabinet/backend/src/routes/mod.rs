pub mod admin;
pub mod auth;
pub mod book;
pub mod book_ws;
pub mod funds;
pub mod system;
#[cfg(test)]
pub(crate) mod test_support;
pub mod ws;

use std::time::Duration;

use axum::{
	Router,
	body::Bytes,
	http::{HeaderMap, HeaderValue, StatusCode, header},
	middleware,
	response::Response,
	routing::{get, post},
};
use invest_domain::authz::Permission;
use serde_json::Value;
use subtle::ConstantTimeEq;
use tonic::Status;
use tower_http::{
	timeout::TimeoutLayer,
	trace::{DefaultOnFailure, TraceLayer},
};

use crate::{cookies::Cookies, error::ApiError, now_secs, session::Session, state::AppState};

/// Where concierge returns the browser: registered for the client `invest` exactly so,
/// without `/api` (concierge `runner/migrations/0031_scopes.up.sql`). The ingress has to
/// route it here, past the cabinet's Next app — see the README.
pub const CALLBACK_PATH: &str = "/cabinet/auth/callback";

/// Outer per-request deadline: a handler still awaiting an upstream past this is aborted
/// and answers 504, so a wedged plane never holds a browser connection open. Looser than
/// the per-RPC timeout so an upstream stall normally surfaces as a gRPC error first.
const REQUEST_DEADLINE: Duration = Duration::from_secs(15);

/// Mount every endpoint. The JSON surface keeps banking's cabinet paths under
/// `/cabinet/api` (the frontend's `/api/*` behind `basePath: "/cabinet"`).
///
/// The book socket is merged in AFTER [`REQUEST_DEADLINE`]: a deadline is right for a
/// request that must finish and wrong for a socket that must not.
pub fn router(state: AppState) -> Router {
	let websockets = Router::new().route("/cabinet/api/book/ws", get(book_ws::upgrade)).with_state(state.clone());

	requests(state)
		.merge(websockets)
		.layer(TraceLayer::new_for_http().on_failure(DefaultOnFailure::new().level(tracing::Level::WARN)))
}

fn requests(state: AppState) -> Router {
	let api = Router::new()
		// The kubelet's probe target: no session, no upstream. `/health` is the deep check
		// and must not be a probe — see `system::live`.
		.route("/health", get(system::health))
		.route("/health/live", get(system::live))
		.route("/auth/login", get(auth::login))
		.route("/auth/session", get(auth::session))
		.route("/auth/logout", post(auth::logout))
		.route("/allocations", get(funds::list_allocations))
		.route("/allocations/detail", get(funds::get_allocation))
		.route("/funds/nav", get(funds::fund_nav))
		.route("/funds/nav/history", get(funds::fund_nav_history))
		.route("/funds/positions", get(funds::list_positions))
		.route("/funds/fee-policy", get(funds::fee_policy))
		.route("/funds/accrued-fees", get(funds::accrued_fees))
		.route("/funds/redemptions", get(funds::list_redemptions))
		.route("/funds/redemptions/cancel", post(funds::cancel_redemption))
		.route("/funds/subscribe", post(funds::subscribe))
		.route("/funds/redeem", post(funds::redeem))
		// The book — holders trading an allocation's units with each other. The live feed
		// is `/book/ws`, mounted outside the deadline.
		.route("/book", get(book::get_book))
		.route("/book/trades", get(book::list_trades))
		.route("/book/candles", get(book::list_candles))
		.route("/book/policy", get(book::get_policy))
		.route("/book/orders", get(book::list_open_orders).post(book::place_order))
		.route("/book/orders/cancel", post(book::cancel_order))
		.route("/book/orders/history", get(book::list_order_history))
		.route("/book/fills", get(book::list_fills))
		// The allocation console — gated here on an `invest:*` permission per route and
		// re-checked by invest-core against its own projection of the caller.
		.route("/admin/allocations", get(admin::list_allocations))
		.route("/admin/allocations/register", post(admin::register_allocation))
		.route("/admin/allocations/update", post(admin::update_allocation))
		.route("/admin/allocations/state", post(admin::set_allocation_state))
		.route("/admin/allocations/cap", post(admin::set_allocation_unit_cap))
		.route("/admin/allocations/access", post(admin::set_allocation_access))
		.route("/admin/allocations/grants", get(admin::list_allocation_access_grants))
		.route("/admin/allocations/grants/grant", post(admin::grant_allocation_access))
		.route("/admin/allocations/grants/revoke", post(admin::revoke_allocation_access))
		.route("/admin/allocations/issue", post(admin::issue_units))
		.route("/admin/allocations/retire", post(admin::retire_units))
		.route("/admin/allocations/backing", post(admin::set_allocation_backing))
		.route("/admin/allocations/holders", get(admin::list_unit_holders))
		.route("/admin/allocations/treasury", get(admin::allocation_treasury))
		.route("/admin/allocations/book", post(admin::set_book_policy))
		.route("/admin/fees/policies", get(admin::list_fee_policies))
		.route("/admin/fees/policy", post(admin::schedule_fee_policy))
		.route("/admin/fees/policy/cancel", post(admin::cancel_fee_policy_change))
		.route("/admin/fees/policy/acknowledge-notices", post(admin::acknowledge_undelivered_notices))
		.route("/admin/fees/changes", get(admin::list_fee_policy_changes))
		.route("/admin/fees/shares", get(admin::fee_shares))
		.route("/admin/fees/settle", post(admin::settle_fee_shares))
		.route("/admin/fees/assessments", get(admin::fund_fee_assessments))
		.route("/admin/valuation/queue", get(admin::redemption_queue))
		.route("/admin/valuation/post", post(admin::post_valuation))
		.route("/admin/valuation/settle", post(admin::settle_redemption))
		.route("/admin/valuation/fail", post(admin::fail_redemption))
		.route("/admin/revenue", get(admin::fund_revenue));

	Router::new()
		.nest("/cabinet/api", api)
		.route(CALLBACK_PATH, get(auth::callback))
		.with_state(state)
		.layer(middleware::map_response(no_store))
		.layer(TimeoutLayer::with_status_code(StatusCode::GATEWAY_TIMEOUT, REQUEST_DEADLINE))
}

/// Positions, orders and the session's principal pass through here: no shared cache, and
/// no browser cache that outlives a sign-out, may keep them.
async fn no_store(mut response: Response) -> Response {
	response.headers_mut().entry(header::CACHE_CONTROL).or_insert(HeaderValue::from_static("no-store"));
	response
}

/// The request's session, its access token rotated if it was about to lapse.
pub async fn require_session(state: &AppState, jar: &Cookies) -> Result<Session, ApiError> {
	let cookie = jar.get(&state.cookies.session).ok_or(ApiError::Unauthenticated)?;
	Ok(state.sessions.resolve(cookie, now_secs()).await?)
}

/// The signed-in user's `invest` access token — the credential every invest-core call
/// carries.
pub async fn require_token(state: &AppState, jar: &Cookies) -> Result<String, ApiError> {
	Ok(require_session(state, jar).await?.access_token)
}

/// The console gate: the caller's seat must hold `permission` as concierge resolves it now
/// (`GetMe`, cached for a minute). Defense in depth — invest-core re-checks every act —
/// but it keeps an investor out before any privileged call and answers a screen at the
/// BFF. Answers the access token, so a gated handler resolves the session once.
pub async fn require_permission(state: &AppState, jar: &Cookies, permission: Permission) -> Result<String, ApiError> {
	let session = require_session(state, jar).await?;
	let me = state.sessions.me(&session, false).await?;
	if !me.permissions.iter().any(|held| held == permission.as_str()) {
		return Err(ApiError::Grpc(Status::permission_denied(format!("requires {}", permission.as_str()))));
	}
	Ok(session.access_token)
}

/// The gate of every mutating route: the request comes from the cabinet's own origin, and
/// the `x-ev-csrf` header equals the readable CSRF cookie.
///
/// `Origin` is required, not merely checked when present: a browser sends it on every
/// POST, so its absence means the request did not come from a page at all. The cookie is
/// `__Host-` in production, so a sibling subdomain cannot plant one to match.
pub fn verify_csrf(state: &AppState, jar: &Cookies, headers: &HeaderMap) -> bool {
	if !origin_allowed(state, headers) {
		return false;
	}
	let cookie = jar.get(&state.cookies.csrf);
	let header = headers.get("x-ev-csrf").and_then(|v| v.to_str().ok());
	matches!((cookie, header), (Some(c), Some(h)) if !c.is_empty() && ct_str_eq(c, h))
}

/// Whether the request's `Origin` is this cabinet's public origin.
pub fn origin_allowed(state: &AppState, headers: &HeaderMap) -> bool {
	headers
		.get(header::ORIGIN)
		.and_then(|value| value.to_str().ok())
		.is_some_and(|origin| origin == state.config.cabinet_origin())
}

/// Parse a request body leniently (a malformed or empty body becomes `{}`), so a missing
/// field is reported by name rather than as a parse error.
pub fn parse_body(body: &Bytes) -> Value {
	serde_json::from_slice(body).unwrap_or_else(|_| Value::Object(Default::default()))
}
/// A required string field: `None` when missing OR empty.
pub fn required(v: &Value, key: &str) -> Option<String> {
	v.get(key).and_then(|x| x.as_str()).map(str::to_string).filter(|s| !s.is_empty())
}
/// A REQUIRED whole-number field that fits a `u32`: `None` when the field is missing, or
/// is not a non-negative integer inside the `u32` range.
///
/// Deliberately not a defaulting read. `serde_json`'s `as_u64` answers `None` for `-1`,
/// for `2.5` and for a quoted `"2"` alike, so a caller that unwraps it to zero cannot tell
/// "the operator asked for zero" from "this layer could not read what the operator asked
/// for" — and zero is a meaningful value on every ladder that crosses the BFF. `as u32`
/// would truncate 2^32 into a plausible 0, hence `u32::try_from`.
pub fn required_u32(v: &Value, key: &str) -> Option<u32> {
	v.get(key).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok())
}
/// An editable string field: missing ⇒ `""` (full-replace semantics; empty clears).
pub fn editable(v: &Value, key: &str) -> String {
	v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}
/// Constant-time string equality after a length check, which reveals only the length.
/// Not `==`: a short-circuiting compare of a secret leaks it byte by byte through timing.
pub(crate) fn ct_str_eq(a: &str, b: &str) -> bool {
	a.len() == b.len() && a.as_bytes().ct_eq(b.as_bytes()).into()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ct_str_eq_matches_plain_equality() {
		assert!(ct_str_eq("a3f9c0d1-token", "a3f9c0d1-token"));
		assert!(!ct_str_eq("a3f9c0d1-token", "a3f9c0d1-toked"));
		assert!(!ct_str_eq("short", "longer-value"));
		assert!(ct_str_eq("", ""));
	}
}
