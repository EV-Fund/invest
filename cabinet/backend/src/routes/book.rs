//! The book routes — the secondary market in an allocation's units, behind `/api/book/*`.
//!
//! Every handler forwards the user's access token to invest-core. Reads answer with a fixed
//! generic message on failure, mutations relay invest-core's client-safe wording — the
//! same split as the fund routes. The string vocabularies a browser may send (`side`,
//! `kind`, `tif`, `resolution`) are checked here against `invest_contracts::book`, so a
//! client bug is refused one hop away with the list of words that would have worked.
//!
//! The live feed is [`super::book_ws`].

use axum::{
	Json,
	body::Bytes,
	extract::{Query, State},
	http::HeaderMap,
};
use invest_contracts::{
	book::{kind as wire_kind, resolution as wire_resolution, side as wire_side, tif as wire_tif},
	invest::v1 as iv,
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
	cookies::Cookies,
	dto,
	error::ApiError,
	routes::{editable, parse_body, require_token, required, verify_csrf},
	state::AppState,
};

/// `?service=&depth=` — the book's identity and how many levels a side to render.
#[derive(Deserialize)]
pub struct BookQuery {
	pub service: Option<String>,
	pub depth: Option<u32>,
}

/// `?service=&limit=` — a tape or a history page. `service` is optional on the caller's
/// own lists (absent = every allocation) and required on the public tape.
#[derive(Deserialize)]
pub struct PageQuery {
	service: Option<String>,
	limit: Option<u32>,
}

#[derive(Deserialize)]
pub struct ServiceQuery {
	service: Option<String>,
}

#[derive(Deserialize)]
pub struct CandlesQuery {
	service: Option<String>,
	resolution: Option<String>,
	from: Option<i64>,
	to: Option<i64>,
}

/// The one allocation a per-book read names. A missing or blank service is a client error
/// decided here — never a call to invest-core with an empty id.
pub fn required_service(service: Option<String>) -> Result<String, ApiError> {
	service.filter(|s| !s.trim().is_empty()).ok_or_else(|| ApiError::BadRequest("service is required".into()))
}

/// A word the contract pins: present, and one of `expected`. The refusal names the
/// vocabulary, so a client that sent `BUY` or `fok` learns what would have worked.
fn vocabulary(v: &Value, key: &str, expected: &[&str]) -> Result<String, ApiError> {
	let Some(word) = required(v, key) else {
		return Err(ApiError::BadRequest(format!("{key} is required — one of {}", expected.join(", "))));
	};
	if !expected.contains(&word.as_str()) {
		return Err(ApiError::BadRequest(format!("unknown {key} '{word}' — expected one of {}", expected.join(", "))));
	}
	Ok(word)
}

// ── the book (readable by anyone who may view the allocation) ────────────────

/// `GET /api/book?service=&depth=` — the aggregated book: the top `depth` levels a side,
/// the last trade, the 24h figures and NAV for the ticker. A hidden allocation is a 404,
/// as on `/api/allocations/detail`. An absent `depth` is invest-core's default.
pub async fn get_book(State(st): State<AppState>, jar: Cookies, Query(q): Query<BookQuery>) -> Result<Json<dto::BookSnapshot>, ApiError> {
	let service = required_service(q.service)?;
	let token = require_token(&st, &jar).await?;
	let book = st
		.grpc
		.get_book(&token, &service, q.depth.unwrap_or(0))
		.await
		.map_err(|s| ApiError::read(s, "book unavailable"))?;
	Ok(Json(book.into()))
}

/// `GET /api/book/trades?service=&limit=` — the public tape, newest first: price, size,
/// the taker's side and the time. No order ids and no parties.
pub async fn list_trades(State(st): State<AppState>, jar: Cookies, Query(q): Query<PageQuery>) -> Result<Json<dto::TradeList>, ApiError> {
	let service = required_service(q.service)?;
	let token = require_token(&st, &jar).await?;
	let tape = st
		.grpc
		.list_trades(&token, &service, q.limit.unwrap_or(0))
		.await
		.map_err(|s| ApiError::read(s, "trades unavailable"))?;
	Ok(Json(tape.into()))
}

/// `GET /api/book/candles?service=&resolution=&from=&to=` — OHLCV buckets over
/// `[from, to)` in unix seconds; an absent `to` is now. Buckets with no trade are omitted.
pub async fn list_candles(State(st): State<AppState>, jar: Cookies, Query(q): Query<CandlesQuery>) -> Result<Json<dto::CandleList>, ApiError> {
	let service = required_service(q.service)?;
	let resolution = match q.resolution.as_deref().filter(|r| !r.is_empty()) {
		Some(r) if wire_resolution::is_known(r) => r.to_string(),
		Some(r) => return Err(ApiError::BadRequest(format!("unknown resolution '{r}' — expected one of {}", wire_resolution::ALL.join(", ")))),
		None => return Err(ApiError::BadRequest(format!("resolution is required — one of {}", wire_resolution::ALL.join(", ")))),
	};
	let token = require_token(&st, &jar).await?;
	let req = iv::ListCandlesRequest {
		service,
		resolution,
		from: q.from.unwrap_or(0),
		to: q.to.unwrap_or(0),
	};
	let candles = st.grpc.list_candles(&token, req).await.map_err(|s| ApiError::read(s, "candles unavailable"))?;
	Ok(Json(candles.into()))
}

/// `GET /api/book/policy?service=` — the allocation's trading terms: whether the book is
/// open, the taker fee, tick, lot and market slippage. Part of deciding whether to trade
/// at all, so not gated on holding a position.
pub async fn get_policy(State(st): State<AppState>, jar: Cookies, Query(q): Query<ServiceQuery>) -> Result<Json<dto::BookPolicy>, ApiError> {
	let service = required_service(q.service)?;
	let token = require_token(&st, &jar).await?;
	let policy = st.grpc.get_book_policy(&token, &service).await.map_err(|s| ApiError::read(s, "book policy unavailable"))?;
	Ok(Json(policy.into()))
}

// ── the caller's orders ──────────────────────────────────────────────────────

/// `POST /api/book/orders` — CSRF-checked: place an order. Body
/// `{service, side, kind, tif?, price?, size, client_order_id}`. `tif` may be left out on
/// a market order (always `ioc`); a limit order has to say. `price` is empty for a market
/// order. Idempotent by `client_order_id`: a retry returns the order the id already names.
pub async fn place_order(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Order>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let Some(service) = required(&v, "service") else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let side = vocabulary(&v, "side", &wire_side::ALL)?;
	let kind = vocabulary(&v, "kind", &wire_kind::ALL)?;
	let tif = match required(&v, "tif") {
		Some(tif) if wire_tif::is_known(&tif) => tif,
		Some(tif) => return Err(ApiError::BadRequest(format!("unknown tif '{tif}' — expected one of {}", wire_tif::ALL.join(", ")))),
		None => String::new(),
	};
	let (Some(size), Some(client_order_id)) = (required(&v, "size"), required(&v, "client_order_id")) else {
		return Err(ApiError::BadRequest("size and client_order_id are required".into()));
	};
	let token = require_token(&st, &jar).await?;
	let req = iv::PlaceOrderRequest {
		service,
		side,
		kind,
		tif,
		price: editable(&v, "price"),
		size,
		client_order_id,
	};
	Ok(Json(st.grpc.place_order(&token, req).await?.into()))
}

/// `POST /api/book/orders/cancel` — CSRF-checked: take the caller's resting order off the
/// book; its unspent escrow is released. Never refused for a frozen caller.
pub async fn cancel_order(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Order>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let Some(order_id) = required(&parse_body(&body), "order_id") else {
		return Err(ApiError::BadRequest("order_id is required".into()));
	};
	let token = require_token(&st, &jar).await?;
	Ok(Json(st.grpc.cancel_order(&token, &order_id).await?.into()))
}

/// `GET /api/book/orders?service=` — the caller's resting orders, oldest first. An absent
/// `service` is every allocation.
pub async fn list_open_orders(State(st): State<AppState>, jar: Cookies, Query(q): Query<ServiceQuery>) -> Result<Json<dto::OrderList>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let orders = st
		.grpc
		.list_open_orders(&token, &q.service.unwrap_or_default())
		.await
		.map_err(|s| ApiError::read(s, "orders unavailable"))?;
	Ok(Json(orders.into()))
}

/// `GET /api/book/orders/history?service=&limit=` — the caller's orders in every state,
/// newest first. An absent `service` is every allocation; an absent `limit` invest-core's page.
pub async fn list_order_history(State(st): State<AppState>, jar: Cookies, Query(q): Query<PageQuery>) -> Result<Json<dto::OrderList>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let orders = st
		.grpc
		.list_order_history(&token, &q.service.unwrap_or_default(), q.limit.unwrap_or(0))
		.await
		.map_err(|s| ApiError::read(s, "order history unavailable"))?;
	Ok(Json(orders.into()))
}

/// `GET /api/book/fills?service=&limit=` — the caller's own fills, newest first, each with
/// their side, their order and the fee they paid as taker.
pub async fn list_fills(State(st): State<AppState>, jar: Cookies, Query(q): Query<PageQuery>) -> Result<Json<dto::TradeList>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let fills = st
		.grpc
		.list_user_trades(&token, &q.service.unwrap_or_default(), q.limit.unwrap_or(0))
		.await
		.map_err(|s| ApiError::read(s, "fills unavailable"))?;
	Ok(Json(fills.into()))
}
