//! Signing in as concierge's relying party `invest`, and the session endpoints the cabinet
//! reads. The flow is in [`crate::session`]; this module is its HTTP face.

use axum::{
	Json,
	extract::{Query, State},
	http::{HeaderMap, HeaderValue, StatusCode, header},
	response::{Html, IntoResponse, Response},
};
use evconcierge_client::{ConciergeError, Me};
use invest_domain::authz::Permission;
use serde::{Deserialize, Serialize};
use tonic::Code;

use crate::{
	cookies::Cookies,
	error::ApiError,
	now_secs,
	routes::{ct_str_eq, verify_csrf},
	session::{SIGN_IN_TTL_SECS, SessionError, SessionKey, SignInTarget},
	state::AppState,
};

/// Where a sign-in lands when the login names no page — the cabinet's root, which the
/// frontend sends on to the reader's locale.
const HOME: &str = "/cabinet";

/// The longest `return_to` taken. A page path with its query; anything longer is not one
/// of the cabinet's links.
const MAX_RETURN_TO: usize = 2048;

/// What a sign-in page may load and do: nothing but this origin, and never inside a frame.
const PAGE_CSP: &str = "default-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

/// The page to land on after signing in, if `raw` is a path inside the cabinet. Refused:
/// anything outside `/cabinet` (an open redirect off the sign-in), a scheme-relative
/// `//host`, a backslash a browser may read as a slash, a dot segment (`/cabinet/../x`
/// leaves the cabinet once the browser resolves it, `%2e` included), and anything that is
/// not visible ASCII.
pub(crate) fn return_target(raw: Option<&str>) -> Option<String> {
	let Some(raw) = raw.filter(|r| !r.is_empty()) else {
		return Some(HOME.to_owned());
	};
	if raw.len() > MAX_RETURN_TO || !raw.bytes().all(|b| b.is_ascii_graphic()) {
		return None;
	}
	let rest = raw.strip_prefix(HOME)?;
	if !(rest.is_empty() || rest.starts_with('/') || rest.starts_with('?')) {
		return None;
	}
	let path = raw.split(['?', '#']).next().unwrap_or(raw);
	let lowered = path.to_ascii_lowercase();
	if raw.contains('\\') || path.contains("//") || lowered.contains("..") || lowered.contains("%2e") || lowered.contains("%2f") || lowered.contains("%5c") {
		return None;
	}
	Some(raw.to_owned())
}

/// A page on this origin with fixed text: nothing from the request is echoed.
fn page(status: StatusCode, title: &str, message: &str) -> Response {
	let body = format!(
		"<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
		 <title>{title}</title></head><body><main><h1>{title}</h1><p>{message}</p><p><a href=\"{HOME}\">Back to the cabinet</a></p></main></body></html>"
	);
	let mut res = (status, Html(body)).into_response();
	res.headers_mut().insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(PAGE_CSP));
	res
}

/// The sign-in's answers carry a code or a state in their URLs: kept out of caches and out
/// of the `Referer` of whatever the page loads next.
fn private(mut res: Response) -> Response {
	let h = res.headers_mut();
	h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
	h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
	res
}

fn failed(status: StatusCode, message: &str) -> Response {
	page(status, "Sign-in failed", message)
}

fn report(err: &color_eyre::Report, what: &str) {
	tracing::error!(error = ?err, "{what}");
}

#[derive(Deserialize)]
pub struct LoginQuery {
	return_to: Option<String>,
}

/// `GET /cabinet/api/auth/login?return_to=` — start a sign-in: a state and PKCE verifier
/// recorded server-side, the state in an HttpOnly cookie, and the browser sent to
/// concierge's `authorize`. Silent when the evinvest.ltd session already exists.
pub async fn login(State(st): State<AppState>, Query(q): Query<LoginQuery>) -> Response {
	let Some(return_to) = return_target(q.return_to.as_deref()) else {
		return private(failed(StatusCode::BAD_REQUEST, "That page is not part of the cabinet."));
	};
	let redirect_uri = st.config.redirect_uri();
	let target = SignInTarget {
		concierge_origin: st.config.concierge_public_origin.trim_end_matches('/'),
		client_id: &st.config.concierge_client_id,
		redirect_uri: &redirect_uri,
	};
	let begun = match st.sessions.begin(target, &return_to, now_secs()).await {
		Ok(begun) => begun,
		Err(err) => {
			report(&err, "starting a sign-in");
			return private(failed(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong on our side. Try again."));
		}
	};
	let Ok(location) = HeaderValue::from_str(&begun.authorize_url) else {
		return private(failed(StatusCode::INTERNAL_SERVER_ERROR, "The sign-in is misconfigured."));
	};
	let cookie = st.cookies.set(&st.cookies.sign_in, &begun.state, SIGN_IN_TTL_SECS, true);
	private((StatusCode::FOUND, [(header::LOCATION, location), (header::SET_COOKIE, cookie)]).into_response())
}

#[derive(Deserialize)]
pub struct CallbackQuery {
	code: Option<String>,
	state: Option<String>,
	error: Option<String>,
}

/// `GET /cabinet/auth/callback` — finish a sign-in. The code is presented to concierge
/// only when the state in the URL is the one this browser's cookie holds and its record
/// has not been redeemed: a callback URL that leaked, or one an attacker started in their
/// own browser and fed to a victim (login CSRF), stops here.
pub async fn callback(State(st): State<AppState>, jar: Cookies, Query(q): Query<CallbackQuery>) -> Response {
	// The sign-in cookie is single-use whatever happens next.
	let clear = st.cookies.clear(&st.cookies.sign_in);
	let with_clear = |res: Response| {
		let mut res = private(res);
		res.headers_mut().append(header::SET_COOKIE, clear.clone());
		res
	};
	if let Some(error) = q.error.as_deref() {
		// Fixed pages: concierge's error word picks one, and is not shown.
		return with_clear(match error {
			"access_denied" => page(StatusCode::FORBIDDEN, "No access", "Your account has no access to EV Invest."),
			"temporarily_unavailable" => page(
				StatusCode::SERVICE_UNAVAILABLE,
				"Sign-in unavailable",
				"Signing in is unavailable right now. Try again in a minute.",
			),
			_ => failed(StatusCode::BAD_REQUEST, "The sign-in could not be completed. Try again."),
		});
	}
	let (Some(state), Some(code)) = (q.state.as_deref().filter(|s| !s.is_empty()), q.code.as_deref().filter(|c| !c.is_empty())) else {
		return with_clear(failed(StatusCode::BAD_REQUEST, "This sign-in link is incomplete. Start again."));
	};
	if !jar.get(&st.cookies.sign_in).is_some_and(|cookie| ct_str_eq(cookie, state)) {
		tracing::info!("sign-in callback without this browser's state; the code is not presented");
		return with_clear(failed(StatusCode::BAD_REQUEST, "This sign-in was started elsewhere or has expired. Start again."));
	}
	let now = now_secs();
	let pending = match st.sessions.redeem(state, now).await {
		Ok(Some(pending)) => pending,
		Ok(None) => {
			tracing::info!("sign-in callback with a state already redeemed or expired; the code is not presented");
			return with_clear(failed(StatusCode::BAD_REQUEST, "This sign-in was already used or has expired. Start again."));
		}
		Err(err) => {
			report(&err, "redeeming a sign-in");
			return with_clear(failed(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong on our side. Try again."));
		}
	};
	let tokens = match st.sessions.concierge().exchange_code(code, &st.config.redirect_uri(), &pending.verifier).await {
		Ok(tokens) => tokens,
		Err(ConciergeError::Refused { code: Code::PermissionDenied }) => return with_clear(page(StatusCode::FORBIDDEN, "No access", "Your account has no access to EV Invest.")),
		// A spent or forged code, or a verifier that does not answer the challenge.
		Err(ConciergeError::Refused { .. } | ConciergeError::Rejected { .. }) => {
			return with_clear(failed(StatusCode::BAD_REQUEST, "This sign-in has expired or was already used. Start again."));
		}
		Err(err) if err.is_transient() => {
			tracing::warn!(error = %err, "concierge unavailable at the sign-in callback");
			return with_clear(page(
				StatusCode::SERVICE_UNAVAILABLE,
				"Sign-in unavailable",
				"Signing in is unavailable right now. Try again in a minute.",
			));
		}
		Err(err) => {
			report(&color_eyre::Report::new(err), "redeeming a sign-in code");
			return with_clear(failed(StatusCode::BAD_GATEWAY, "Something went wrong. Try again."));
		}
	};
	// Signing in again from a browser with a session: the old one is closed, not orphaned.
	if let Some(old) = jar.get(&st.cookies.session).and_then(SessionKey::of_cookie)
		&& let Err(err) = st.sessions.close(&old).await
	{
		report(&err, "closing the session a new sign-in replaces");
		return with_clear(failed(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong on our side. Try again."));
	}
	let opened = match st.sessions.open(&tokens, now).await {
		Ok(opened) => opened,
		Err(err) => {
			report(&err, "opening a session");
			return with_clear(failed(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong on our side. Try again."));
		}
	};
	// `return_to` passed `return_target` before it was stored, and is ASCII.
	let Ok(location) = HeaderValue::from_str(&pending.return_to) else {
		return with_clear(failed(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong on our side. Try again."));
	};
	tracing::info!(user_id = %tokens.user_id, "signed in");
	let max_age = opened.expires_at - now;
	let mut res = with_clear((StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response());
	let h = res.headers_mut();
	h.append(header::SET_COOKIE, st.cookies.set(&st.cookies.session, &opened.cookie, max_age, true));
	h.append(header::SET_COOKIE, st.cookies.set(&st.cookies.csrf, &opened.csrf, max_age, false));
	res
}

/// `POST /cabinet/api/auth/logout` — CSRF- and Origin-checked: this browser's session is
/// closed and its cookies cleared. The evinvest.ltd session is concierge's, ended by its
/// confirm page the cabinet navigates to next.
pub async fn logout(State(st): State<AppState>, jar: Cookies, headers: HeaderMap) -> Result<Response, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	if let Some(key) = jar.get(&st.cookies.session).and_then(SessionKey::of_cookie)
		&& let Some(user_id) = st.sessions.close(&key).await.map_err(ApiError::Internal)?
	{
		tracing::info!(%user_id, "signed out");
	}
	Ok(signed_out(&st, StatusCode::NO_CONTENT.into_response()))
}

fn signed_out(st: &AppState, mut res: Response) -> Response {
	let h = res.headers_mut();
	h.append(header::SET_COOKIE, st.cookies.clear(&st.cookies.session));
	h.append(header::SET_COOKIE, st.cookies.clear(&st.cookies.csrf));
	h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
	res
}

/// The cabinet's `SessionInfo` (`shared/contracts/admin.ts`). camelCase, unlike the rest of
/// this wire, because that is the shape the frontend reads for this one endpoint.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
	authenticated: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	user: Option<SessionUser>,
	permissions: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionUser {
	user_id: String,
	email: String,
	email_verified: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	username: Option<String>,
	status: String,
	role: &'static str,
	is_admin: bool,
	/// Always false: break-glass is banking's and concierge's, and invest grants nothing on it.
	role_is_break_glass: bool,
}

impl SessionInfo {
	fn guest() -> Self {
		Self {
			authenticated: false,
			user: None,
			permissions: Vec::new(),
		}
	}

	fn of(me: Me) -> Self {
		let holds = |p: Permission| me.permissions.iter().any(|held| held == p.as_str());
		let (manage, value) = (holds(Permission::AllocationManage), holds(Permission::ValuationPost));
		// The console has no single permission (frontend `route-permissions.ts`): anyone who
		// may administer allocations or post their valuations sees it. `role` only picks the
		// rail entries: the fees screen is `admin`'s, and the fee routes need AllocationManage.
		let role = if manage {
			"admin"
		} else if value {
			"operator"
		} else {
			"investor"
		};
		Self {
			authenticated: true,
			user: Some(SessionUser {
				user_id: me.user_id.to_string(),
				email: me.email,
				email_verified: me.email_verified,
				username: Some(me.username).filter(|u| !u.is_empty()),
				status: me.status,
				role,
				is_admin: manage || value,
				role_is_break_glass: false,
			}),
			permissions: me.permissions,
		}
	}
}

/// `GET /cabinet/api/auth/session` — the principal, or a guest. Rotates the tokens behind
/// the session when they are about to lapse and asks concierge afresh, so the cabinet's
/// session keeper reading this every few minutes keeps the session alive and current.
///
/// A guest is 200, not 401: the frontend reads a non-2xx as "unknown" (offline, BFF down)
/// and must be able to tell it from "signed out". A session that is gone has its cookies
/// cleared, so the frontend's proxy stops treating the browser as signed in.
pub async fn session(State(st): State<AppState>, jar: Cookies) -> Result<Response, ApiError> {
	let Some(cookie) = jar.get(&st.cookies.session) else {
		return Ok(no_store(Json(SessionInfo::guest()).into_response()));
	};
	let resolved = st.sessions.resolve(cookie, now_secs()).await;
	let me = match resolved {
		Ok(session) => st.sessions.me(&session, true).await,
		Err(err) => Err(err),
	};
	match me {
		Ok(me) => Ok(no_store(Json(SessionInfo::of(me)).into_response())),
		Err(SessionError::Missing | SessionError::Rejected) => Ok(signed_out(&st, Json(SessionInfo::guest()).into_response())),
		Err(err) => Err(err.into()),
	}
}

fn no_store(mut res: Response) -> Response {
	res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
	res
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_page_inside_the_cabinet_is_returned_to() {
		for ok in ["/cabinet", "/cabinet/", "/cabinet/ru/invest", "/cabinet/ru/invest/quy-nhon/trade?tab=orders", "/cabinet?x=1"] {
			assert_eq!(return_target(Some(ok)).as_deref(), Some(ok), "{ok}");
		}
		assert_eq!(return_target(None).as_deref(), Some("/cabinet"));
		assert_eq!(return_target(Some("")).as_deref(), Some("/cabinet"));
	}

	#[test]
	fn anything_outside_the_cabinet_is_refused() {
		for bad in [
			"/",
			"/admin",
			"/cabinetx",
			"/cabinet-evil",
			"https://evil.test/cabinet",
			"//evil.test/cabinet",
			"/cabinet//evil.test",
			"/cabinet/../admin",
			"/cabinet/%2e%2e/admin",
			"/cabinet/%2E%2E/admin",
			"/cabinet/%2fevil",
			"/cabinet\\evil",
			"/cabinet/\u{7f}",
			"/cabinet/ru invest",
			"cabinet",
		] {
			assert_eq!(return_target(Some(bad)), None, "{bad:?}");
		}
		assert_eq!(return_target(Some(&format!("/cabinet/{}", "a".repeat(MAX_RETURN_TO)))), None);
	}
}
