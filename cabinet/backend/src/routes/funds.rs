//! The investor's own routes: the catalog, prices, positions, subscriptions and
//! redemptions — behind `/cabinet/api/allocations*` and `/cabinet/api/funds/*`.

use axum::{
	Json,
	body::Bytes,
	extract::{Query, State},
	http::HeaderMap,
};
use invest_contracts::invest::v1 as iv;
use serde::Deserialize;

use crate::{
	cookies::Cookies,
	dto,
	error::ApiError,
	routes::{parse_body, require_token, required, verify_csrf},
	state::AppState,
};

#[derive(Deserialize)]
pub struct ServiceQuery {
	service: Option<String>,
}

/// `?allocation=&from=&to=` — a history window over one allocation, unix seconds.
#[derive(Deserialize)]
pub struct HistoryQuery {
	allocation: Option<String>,
	from: Option<i64>,
	to: Option<i64>,
}

// ── allocations (the catalog of investable products) ─────────────────────────

/// `GET /api/allocations` — the investor-facing catalog: the `open` allocations the caller
/// may at least view. Each row carries `access` (the product's default) and `caller_access`
/// (what THIS investor holds) — the subscribe control keys off the latter. The unlisted
/// half (draft/closed/hidden) is admin-only and lives on the admin surface.
pub async fn list_allocations(State(st): State<AppState>, jar: Cookies) -> Result<Json<dto::AllocationList>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let list = st.grpc.list_allocations(&token, false).await.map_err(|s| ApiError::read(s, "allocations unavailable"))?;
	Ok(Json(list.into()))
}

/// `GET /api/allocations/detail?service=` — one allocation, in any state. An investor
/// holding units of a closed product still has to render it. A product hidden from this
/// investor is a 404, exactly as an unknown service is.
pub async fn get_allocation(State(st): State<AppState>, jar: Cookies, Query(q): Query<ServiceQuery>) -> Result<Json<dto::Allocation>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let allocation = st
		.grpc
		.get_allocation(&token, &q.service.unwrap_or_default())
		.await
		.map_err(|s| ApiError::read(s, "allocation unavailable"))?;
	Ok(Json(allocation.into()))
}

// ── funds ─────────────────────────────────────────────

/// `GET /api/funds/nav?service=` — the current NAV (price per share) of a fund.
pub async fn fund_nav(State(st): State<AppState>, jar: Cookies, Query(q): Query<ServiceQuery>) -> Result<Json<dto::FundNav>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let nav = st
		.grpc
		.fund_nav(&token, &q.service.unwrap_or_default())
		.await
		.map_err(|s| ApiError::read(s, "fund nav unavailable"))?;
	Ok(Json(nav.into()))
}

/// `GET /api/funds/nav/history?allocation=&from=&to=` — the valuation log of one
/// allocation over `[from, to]` (unix seconds; absent = all-time / now) plus the caller's
/// own participation through it: the two series of the dashboard's performance chart.
/// Keyed by the allocation, not the service, so a hidden allocation can serve its holders
/// the same way later. A seed-only fund answers empty `marks`, not an error.
pub async fn fund_nav_history(State(st): State<AppState>, jar: Cookies, Query(q): Query<HistoryQuery>) -> Result<Json<dto::FundNavHistory>, ApiError> {
	let allocation = q.allocation.filter(|a| !a.is_empty()).ok_or_else(|| ApiError::BadRequest("allocation is required".into()))?;
	let token = require_token(&st, &jar).await?;
	let req = iv::ListFundNavHistoryRequest {
		allocation,
		from: q.from.unwrap_or(0),
		to: q.to.unwrap_or(0),
	};
	let history = st.grpc.fund_nav_history(&token, req).await.map_err(|s| ApiError::read(s, "fund history unavailable"))?;
	Ok(Json(history.into()))
}

/// `GET /api/funds/fee-policy?service=` — what this fund charges. Not gated on holding a
/// position: the terms are part of deciding whether to buy in at all.
pub async fn fee_policy(State(st): State<AppState>, jar: Cookies, Query(q): Query<ServiceQuery>) -> Result<Json<dto::FeePolicy>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let policy = st
		.grpc
		.fee_policy(&token, &q.service.unwrap_or_default())
		.await
		.map_err(|s| ApiError::read(s, "fee policy unavailable"))?;
	Ok(Json(policy.into()))
}

/// `GET /api/funds/accrued-fees?service=` — what the caller's holding owes right now,
/// uncharged. The disclosure behind showing a position's value net of fees.
pub async fn accrued_fees(State(st): State<AppState>, jar: Cookies, Query(q): Query<ServiceQuery>) -> Result<Json<dto::AccruedFees>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let accrued = st
		.grpc
		.accrued_fees(&token, &q.service.unwrap_or_default())
		.await
		.map_err(|s| ApiError::read(s, "accrued fees unavailable"))?;
	Ok(Json(accrued.into()))
}

/// `GET /api/funds/positions` — the caller's fund positions.
pub async fn list_positions(State(st): State<AppState>, jar: Cookies) -> Result<Json<dto::PositionList>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let list = st.grpc.list_positions(&token).await.map_err(|s| ApiError::read(s, "positions unavailable"))?;
	Ok(Json(list.into()))
}

/// `GET /api/funds/redemptions` — the caller's redemptions, newest first.
pub async fn list_redemptions(State(st): State<AppState>, jar: Cookies) -> Result<Json<dto::RedemptionList>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let list = st.grpc.list_redemptions(&token).await.map_err(|s| ApiError::read(s, "redemptions unavailable"))?;
	Ok(Json(list.into()))
}

/// `POST /api/funds/subscribe` — CSRF-checked: subscribe free balance into a fund (mint units).
pub async fn subscribe(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Subscription>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let v = parse_body(&body);
	let (Some(service), Some(amount)) = (required(&v, "service"), required(&v, "amount")) else {
		return Err(ApiError::BadRequest("service and amount are required".into()));
	};
	let req = iv::SubscribeRequest { service, amount };
	Ok(Json(st.grpc.subscribe(&token, req).await?.into()))
}

/// `POST /api/funds/redeem` — CSRF-checked: redeem units back to cash (accept-and-queue).
pub async fn redeem(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Redemption>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let v = parse_body(&body);
	let (Some(service), Some(units)) = (required(&v, "service"), required(&v, "units")) else {
		return Err(ApiError::BadRequest("service and units are required".into()));
	};
	let req = iv::RedeemRequest { service, units };
	Ok(Json(st.grpc.redeem(&token, req).await?.into()))
}

/// `POST /api/funds/redemptions/cancel` — CSRF-checked: cancel a still-queued redemption.
pub async fn cancel_redemption(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Redemption>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let Some(redemption_id) = required(&parse_body(&body), "redemption_id") else {
		return Err(ApiError::BadRequest("redemption_id is required".into()));
	};
	Ok(Json(st.grpc.cancel_redemption(&token, &redemption_id).await?.into()))
}
