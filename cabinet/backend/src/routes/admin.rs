//! The allocation console — the operator surface behind `/cabinet/api/admin/*`.
//!
//! Every handler is gated first by [`require_permission`] on an `invest:*` permission as
//! concierge resolves the caller's seat, then forwards the caller's access token to
//! invest-core, which re-checks the same permission against its own projection (defense
//! in depth; a missing one surfaces as 403). Mutations verify CSRF and `Origin` right
//! after the gate. The gates mirror invest-core's: `invest:allocation:manage` everywhere
//! but posting a valuation, which is `invest:valuation:post`.

use axum::{
	Json,
	body::Bytes,
	extract::{Query, State},
	http::HeaderMap,
};
use invest_contracts::{
	allocation::{access as wire_access, backing as wire_backing, icon as wire_icon},
	invest::v1 as iv,
};
use invest_domain::authz::Permission;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
	cookies::Cookies,
	dto,
	error::ApiError,
	routes::{editable, parse_body, require_permission, required, required_u32, verify_csrf},
	state::AppState,
};

/// The reserved allocation the platform's fee units and fee cash belong to (#245).
const FEE_ALLOCATION: &str = "fee";

#[derive(Deserialize)]
pub struct FeeServiceQuery {
	service: Option<String>,
}

fn bool_field(v: &Value, key: &str) -> bool {
	v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// A REQUIRED basis-point rate: `None` when the field is missing, or is not a whole
/// non-negative number that fits a `u32`.
///
/// Deliberately not a defaulting missing-is-zero read, which is right for an optional
/// knob and wrong for a price. A rate that arrives as `"200"`, as `2.5`, or not at all is
/// a client that does not know what it is asking for, and coercing it to zero would set
/// the fund to charge NOTHING while answering "saved". invest-core caps the value at
/// 10000 bps; refusing a malformed one is this layer's half.
fn rate_field(v: &Value, key: &str) -> Option<u32> {
	required_u32(v, key)
}

/// `GET /cabinet/api/admin/fees/policies` — every fund's fee terms, for the fees table.
pub async fn list_fee_policies(State(st): State<AppState>, jar: Cookies) -> Result<Json<dto::FeePolicyList>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	let list = st.grpc.fee_policies(&token).await.map_err(|s| ApiError::read(s, "fee policies unavailable"))?;
	Ok(Json(list.into()))
}

/// `POST /cabinet/api/admin/fees/policy` — propose a change of a fund's terms.
///
/// Every rate is required rather than patch-style optional. A fee schedule is read as a
/// whole — "2 and 20 over a 5% hurdle, annual" is one statement — and letting an operator
/// change the management rate while leaving an unseen crystallization period in place is
/// how a fund ends up charging terms nobody chose. `effective_from` (unix seconds, omitted
/// or 0 = as soon as the notice allows) and `reason` (required by invest-core when the change
/// needs the owners) travel as given; invest-core decides the requirement and the moment.
pub async fn schedule_fee_policy(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::FeePolicyChange>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(basis), Some(crystallization)) = (required(&v, "service"), required(&v, "basis"), required(&v, "crystallization")) else {
		return Err(ApiError::BadRequest("service, basis and crystallization are required".into()));
	};
	let (Some(management_bps), Some(performance_bps), Some(hurdle_bps)) = (rate_field(&v, "management_bps"), rate_field(&v, "performance_bps"), rate_field(&v, "hurdle_bps")) else {
		return Err(ApiError::BadRequest(
			"management_bps, performance_bps and hurdle_bps are required, each a whole number of basis points".into(),
		));
	};
	// Optional, but when present it must be a whole number of unix seconds: a moment this
	// layer cannot read must not become "now" while the operator believes it is next month.
	let effective_from = match v.get("effective_from") {
		None | Some(Value::Null) => 0,
		Some(raw) => raw
			.as_i64()
			.filter(|secs| *secs >= 0)
			.ok_or_else(|| ApiError::BadRequest("effective_from must be a whole number of unix seconds".into()))?,
	};
	let req = iv::ScheduleFeePolicyRequest {
		service,
		management_bps,
		performance_bps,
		hurdle_bps,
		basis,
		crystallization,
		effective_from,
		reason: editable(&v, "reason"),
	};
	let change = st.grpc.schedule_fee_policy(&token, req).await?;
	Ok(Json(change.into()))
}

/// `POST /cabinet/api/admin/fees/policy/cancel` — withdraw a pending change of terms.
pub async fn cancel_fee_policy_change(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::FeePolicyChange>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(change_id)) = (required(&v, "service"), required(&v, "change_id")) else {
		return Err(ApiError::BadRequest("service and change_id are required".into()));
	};
	Ok(Json(st.grpc.cancel_fee_policy_change(&token, &service, &change_id).await?.into()))
}

/// `POST /cabinet/api/admin/fees/policy/acknowledge-notices` — take responsibility for the holders
/// of a scheduled change who could not be told, so a tightening binds over them. invest-core
/// decides who may (the requester or an owner) and whether there is anything to acknowledge.
pub async fn acknowledge_undelivered_notices(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::FeePolicyChange>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(change_id)) = (required(&v, "service"), required(&v, "change_id")) else {
		return Err(ApiError::BadRequest("service and change_id are required".into()));
	};
	Ok(Json(st.grpc.acknowledge_undelivered_notices(&token, &service, &change_id).await?.into()))
}

/// `GET /cabinet/api/admin/fees/changes?service=` — a fund's whole history of terms, newest first.
pub async fn list_fee_policy_changes(State(st): State<AppState>, jar: Cookies, Query(q): Query<FeeServiceQuery>) -> Result<Json<dto::FeePolicyChangeList>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	let Some(service) = q.service.filter(|s| !s.trim().is_empty()) else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let list = st
		.grpc
		.fee_policy_changes(&token, &service)
		.await
		.map_err(|s| ApiError::read(s, "fee policy history unavailable"))?;
	Ok(Json(list.into()))
}

/// `GET /cabinet/api/admin/fees/shares?service=` — uncollected fee units in one fund, and their value.
pub async fn fee_shares(State(st): State<AppState>, jar: Cookies, Query(q): Query<FeeServiceQuery>) -> Result<Json<dto::FeeShares>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	let Some(service) = q.service.filter(|s| !s.trim().is_empty()) else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let shares = st.grpc.fee_shares(&token, &service).await.map_err(|s| ApiError::read(s, "fee shares unavailable"))?;
	Ok(Json(shares.into()))
}

/// `POST /cabinet/api/admin/fees/settle` — convert a fund's accumulated fee units into cash.
///
/// Omitting `units` settles the whole balance, which is the ordinary end-of-period call.
/// Refused rather than queued when the fund's claim cannot cover it on top of its queued
/// redemptions — the manager is paid last.
pub async fn settle_fee_shares(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::FeeSettlement>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let Some(service) = required(&v, "service") else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let settlement = st.grpc.settle_fee_shares(&token, &service, &required(&v, "units").unwrap_or_default()).await?;
	Ok(Json(settlement.into()))
}

/// `GET /cabinet/api/admin/fees/assessments?service=` — every charge this fund has made.
pub async fn fund_fee_assessments(State(st): State<AppState>, jar: Cookies, Query(q): Query<FeeServiceQuery>) -> Result<Json<dto::FeeAssessmentList>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	let Some(service) = q.service.filter(|s| !s.trim().is_empty()) else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let list = st
		.grpc
		.fund_fee_assessments(&token, &service)
		.await
		.map_err(|s| ApiError::read(s, "fee assessments unavailable"))?;
	Ok(Json(list.into()))
}

/// `GET /cabinet/api/admin/valuation/queue` — the cross-user redemption queue awaiting settle.
pub async fn redemption_queue(State(st): State<AppState>, jar: Cookies) -> Result<Json<dto::RedemptionQueue>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	let queue = st.grpc.redemption_queue(&token).await.map_err(|s| ApiError::read(s, "redemption queue unavailable"))?;
	Ok(Json(queue.into()))
}

/// `GET /cabinet/api/admin/allocations` — the full catalog, drafts and closed included.
pub async fn list_allocations(State(st): State<AppState>, jar: Cookies) -> Result<Json<dto::AllocationList>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	let list = st.grpc.list_allocations(&token, true).await.map_err(|s| ApiError::read(s, "allocations unavailable"))?;
	Ok(Json(list.into()))
}

/// The `icon` field of a register/update body, validated against the wire contract.
///
/// Three distinct answers, and keeping them distinct is the point:
/// - key absent (or `null`) ⇒ `None`, "the caller is not talking about the icon". On
///   `/update` that reaches invest-core as an unset `optional` field and the stored pick
///   survives. Curl, a script, or a browser tab holding a bundle from before this field
///   existed all land here — and none of them silently restyle a live product.
/// - present and empty ⇒ `Some("")`, an explicit reset to `fund`.
/// - present and outside the vocabulary ⇒ 400. An operator who mistypes an icon must be
///   told, not handed back a product quietly wearing the wrong picture. invest-core refuses
///   it too; this just answers in the shape the admin console can render.
///
/// So `title` still 400s when omitted while `icon` does not, and that asymmetry is now
/// principled rather than accidental: neither field lets a caller destroy stored state
/// by staying silent. `title` has no wire value meaning "unchanged", so silence there is
/// a client bug and is refused; `icon` has one, so silence is honoured.
fn allocation_icon(v: &Value) -> Result<Option<String>, ApiError> {
	let Some(raw) = v.get("icon").filter(|value| !value.is_null()) else {
		return Ok(None);
	};
	let unknown = |icon: &str| ApiError::BadRequest(format!("unknown allocation icon '{icon}' — expected one of {}", wire_icon::ALL.join(", ")));
	match raw.as_str() {
		Some("") => Ok(Some(String::new())),
		Some(icon) if wire_icon::is_known(icon) => Ok(Some(icon.to_owned())),
		Some(icon) => Err(unknown(icon)),
		// A non-string `icon` was previously read as absent and silently defaulted; it is
		// as much of a client bug as a misspelled one.
		None => Err(unknown(&raw.to_string())),
	}
}

/// `POST /cabinet/api/admin/allocations/register` — register a new investable product (`draft`).
/// This is the only way a fund comes into existence.
pub async fn register_allocation(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Allocation>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(title)) = (required(&v, "service"), required(&v, "title")) else {
		return Err(ApiError::BadRequest("service and title are required".into()));
	};
	let req = iv::RegisterAllocationRequest {
		service,
		title,
		summary: required(&v, "summary").unwrap_or_default(),
		// A registration creates the row, so there is no earlier pick that an absent
		// field could destroy: "" is the wire's "chose nothing" and invest-core lands it on
		// the default. Only `/update` needs presence.
		icon: allocation_icon(&v)?.unwrap_or_default(),
		// Empty is invest's own scope (invest-core decides); the console names one only for
		// a product whose cash lives in another scope's compartments, as the panel's do.
		scope: required(&v, "scope").unwrap_or_default(),
	};
	Ok(Json(st.grpc.register_allocation(&token, req).await?.into()))
}

/// `POST /cabinet/api/admin/allocations/update` — edit an allocation's presentation fields.
pub async fn update_allocation(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Allocation>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(title)) = (required(&v, "service"), required(&v, "title")) else {
		return Err(ApiError::BadRequest("service and title are required".into()));
	};
	let req = iv::UpdateAllocationRequest {
		service,
		title,
		summary: required(&v, "summary").unwrap_or_default(),
		// `None` travels as an unset `optional` field, which invest-core reads as "leave the
		// stored icon alone" — the whole reason this route can be called by a client that
		// predates the field without quietly wiping an operator's choice.
		icon: allocation_icon(&v)?,
	};
	Ok(Json(st.grpc.update_allocation(&token, req).await?.into()))
}

/// `POST /cabinet/api/admin/allocations/state` — open or close an allocation. Closing stops new
/// subscriptions only; redemptions keep working so no investor is trapped.
pub async fn set_allocation_state(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Allocation>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(state)) = (required(&v, "service"), required(&v, "state")) else {
		return Err(ApiError::BadRequest("service and state are required".into()));
	};
	let req = iv::SetAllocationStateRequest { service, state };
	Ok(Json(st.grpc.set_allocation_state(&token, req).await?.into()))
}

/// `POST /cabinet/api/admin/allocations/cap` — resize an allocation's authorised unit supply.
/// Separate from `/update` because this one gates money: invest-core refuses a subscription
/// that would carry the issued supply past it.
pub async fn set_allocation_unit_cap(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Allocation>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(unit_cap)) = (required(&v, "service"), required(&v, "unit_cap")) else {
		return Err(ApiError::BadRequest("service and unit_cap are required".into()));
	};
	let req = iv::SetAllocationUnitCapRequest { service, unit_cap };
	Ok(Json(st.grpc.set_allocation_unit_cap(&token, req).await?.into()))
}

/// An access level a body named, checked against the wire vocabulary before invest-core
/// sees it. `grantable` narrows to what a per-investor grant may carry (`view` |
/// `invest`): `hidden` is a real level for the product's default and not for a grant,
/// which only ever raises. invest-core refuses both cases too; this answers in the shape
/// the console can render.
fn allocation_access_level(v: &Value, key: &str, grantable: bool) -> Result<String, ApiError> {
	let expected: &[&str] = if grantable { &wire_access::GRANTABLE } else { &wire_access::ALL };
	let Some(level) = required(v, key) else {
		return Err(ApiError::BadRequest(format!("{key} is required — one of {}", expected.join(", "))));
	};
	let known = if grantable { wire_access::is_grantable(&level) } else { wire_access::is_known(&level) };
	if !known {
		return Err(ApiError::BadRequest(format!("unknown access level '{level}' — expected one of {}", expected.join(", "))));
	}
	Ok(level)
}

/// `POST /cabinet/api/admin/allocations/access` — set a product's default access level
/// (`hidden` | `view` | `invest`). Orthogonal to `/state`: that says whether the product
/// deals, this says with whom by default. Per-investor grants are untouched.
pub async fn set_allocation_access(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Allocation>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let Some(service) = required(&v, "service") else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let access = allocation_access_level(&v, "access", false)?;
	let req = iv::SetAllocationAccessRequest { service, access };
	Ok(Json(st.grpc.set_allocation_access(&token, req).await?.into()))
}

/// `POST /cabinet/api/admin/allocations/backing` — declare what stands behind a product's units
/// (`cash` | `in_kind`). Orthogonal to `/state` and `/access`: this decides only whether
/// `Redeem` may pay cash out for the units. invest-core flips a product to `in_kind` on its
/// own at the first mint; this is how an operator says the fund now holds cash for the
/// units — or corrects a product back. Idempotent. The vocabulary is checked here, like
/// an access level, so a typo is refused one hop away.
pub async fn set_allocation_backing(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Allocation>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let Some(service) = required(&v, "service") else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let Some(backing) = required(&v, "backing") else {
		return Err(ApiError::BadRequest(format!("backing is required — one of {}", wire_backing::ALL.join(", "))));
	};
	if !wire_backing::is_known(&backing) {
		return Err(ApiError::BadRequest(format!("unknown backing '{backing}' — expected one of {}", wire_backing::ALL.join(", "))));
	}
	Ok(Json(st.grpc.set_allocation_backing(&token, &service, &backing).await?.into()))
}

/// `GET /cabinet/api/admin/allocations/grants?service=` — every investor raised above the
/// product's default, with who granted it and when, each carrying the investor's email.
///
/// `email` stays `null`: an `invest` token opens concierge's `GetMe` and nothing else, so
/// this BFF cannot name another person (see `dto::AllocationAccessGrant`).
pub async fn list_allocation_access_grants(State(st): State<AppState>, jar: Cookies, Query(q): Query<FeeServiceQuery>) -> Result<Json<dto::AllocationAccessGrantList>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	let Some(service) = q.service.filter(|s| !s.trim().is_empty()) else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let list = st
		.grpc
		.list_allocation_access_grants(&token, &service)
		.await
		.map_err(|s| ApiError::read(s, "allocation access grants unavailable"))?;
	Ok(Json(list.into()))
}

/// `POST /cabinet/api/admin/allocations/grants/grant` — raise one investor to `view` | `invest`
/// on a product. A repeat for the same investor overwrites the level. `user_id` is the
/// person's concierge user id.
pub async fn grant_allocation_access(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::AllocationAccessGrant>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(user_id)) = (required(&v, "service"), required(&v, "user_id")) else {
		return Err(ApiError::BadRequest("service and user_id are required".into()));
	};
	let level = allocation_access_level(&v, "level", true)?;
	let req = iv::GrantAllocationAccessRequest { service, user_id, level };
	Ok(Json(st.grpc.grant_allocation_access(&token, req).await?.into()))
}

/// `POST /cabinet/api/admin/allocations/grants/revoke` — take an investor's grant back; they
/// fall to the product's default. Idempotent: revoking a grant that does not stand is
/// still `ok`. Units they already hold are untouched and stay redeemable.
pub async fn revoke_allocation_access(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<Value>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(user_id)) = (required(&v, "service"), required(&v, "user_id")) else {
		return Err(ApiError::BadRequest("service and user_id are required".into()));
	};
	st.grpc.revoke_allocation_access(&token, iv::RevokeAllocationAccessRequest { service, user_id }).await?;
	Ok(Json(json!({ "ok": true })))
}

/// `POST /cabinet/api/admin/allocations/issue` — mint units in kind to a person, with no cash
/// leg. Body: `service`, `units`, `idempotency_key`, `user_id` (the id the console
/// carries — the concierge user id); `cost_basis` is optional and defaults to `units × NAV`.
/// Only a person: the company is not a holder (#245), and the reserved allocations are
/// seated through owners' approval, which invest does not serve yet (S2-6). The key is the retry contract — the
/// console generates one per form submission and re-sends the same one on a timeout, so
/// a double click lands one mint.
pub async fn issue_units(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::UnitIssuance>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(user_id), Some(units), Some(idempotency_key)) = (required(&v, "service"), required(&v, "user_id"), required(&v, "units"), required(&v, "idempotency_key"))
	else {
		// invest-core refuses a request naming nobody too; answering here is one hop sooner.
		return Err(ApiError::BadRequest("service, user_id, units and idempotency_key are required".into()));
	};
	let req = iv::IssueUnitsRequest {
		service,
		user_id,
		units,
		cost_basis: editable(&v, "cost_basis"),
		idempotency_key,
	};
	Ok(Json(st.grpc.issue_units(&token, req).await?.into()))
}

/// `GET /cabinet/api/admin/allocations/holders?service=` — the product's cap table: the settled
/// supply and every holder of it (people and the `fee` allocation), largest first.
pub async fn list_unit_holders(State(st): State<AppState>, jar: Cookies, Query(q): Query<FeeServiceQuery>) -> Result<Json<dto::UnitHolders>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	let Some(service) = q.service.filter(|s| !s.trim().is_empty()) else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let holders = st.grpc.list_unit_holders(&token, &service).await.map_err(|s| ApiError::read(s, "unit holders unavailable"))?;
	Ok(Json(holders.into()))
}

/// `POST /cabinet/api/admin/allocations/retire` — burn a holder's units with no cash leg, the
/// reverse of `/allocations/issue`: the units leave the holder's account and the supply
/// shrinks by the same amount. Body: `service`, `user_id`, `units`, `idempotency_key`;
/// `cost_basis` is optional and defaults to `units × NAV`; `force` (default `false`) is the operator's
/// explicit override to burn units out of a live product — without it invest-core answers
/// 412 unless the allocation is `closed`. Answers the same `UnitIssuance` shape as a
/// mint, with `source: "retire"` and positive `units`. The key is the same retry
/// contract, in the same per-product key space as `/allocations/issue`.
pub async fn retire_units(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::UnitIssuance>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(user_id), Some(units), Some(idempotency_key)) = (required(&v, "service"), required(&v, "user_id"), required(&v, "units"), required(&v, "idempotency_key"))
	else {
		// Same reason as the mint: a burn of nobody's units is a form never filled in.
		return Err(ApiError::BadRequest("service, user_id, units and idempotency_key are required".into()));
	};
	let req = iv::RetireUnitsRequest {
		service,
		user_id,
		units,
		cost_basis: editable(&v, "cost_basis"),
		idempotency_key,
		force: bool_field(&v, "force"),
	};
	Ok(Json(st.grpc.retire_units(&token, req).await?.into()))
}

/// `POST /cabinet/api/admin/allocations/book` — replace a product's trading terms: whether its
/// secondary book is open, the taker fee, the price tick, the lot size and how far past
/// the best quote a market order may fill. Closing the book stops new orders; resting
/// ones stay and may still be cancelled.
///
/// Read as a whole, like the fee schedule: `book_open` and `taker_fee_bps` must be
/// present and well-typed, because a missing flag coerced to `false` would close a book
/// the operator meant to leave open, and a missing fee read as zero would price the book
/// at nothing while the console answers "saved". `price_tick` and `lot_size` may be left
/// out (invest-core's defaults, `0.01` and `0.0001`); `market_slippage_bps` may be left out
/// and is then zero, but if it is sent it has to be a whole number of bps.
/// `allow_unbacked_trading` may be left out and is then `false` — the operator's
/// acknowledgement that the book trades units the fund holds no cash for is given, never
/// presumed; invest-core refuses to open an `in_kind` product's book without it.
pub async fn set_book_policy(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::BookPolicy>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let Some(service) = required(&v, "service") else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let Some(book_open) = v.get("book_open").and_then(Value::as_bool) else {
		return Err(ApiError::BadRequest("book_open is required and must be a boolean".into()));
	};
	let Some(taker_fee_bps) = required_u32(&v, "taker_fee_bps") else {
		return Err(ApiError::BadRequest("taker_fee_bps is required and must be a whole number of basis points".into()));
	};
	let market_slippage_bps = match v.get("market_slippage_bps") {
		None => 0,
		Some(_) => required_u32(&v, "market_slippage_bps").ok_or_else(|| ApiError::BadRequest("market_slippage_bps must be a whole number of basis points".into()))?,
	};
	let req = iv::SetBookPolicyRequest {
		service,
		book_open,
		taker_fee_bps,
		price_tick: editable(&v, "price_tick"),
		lot_size: editable(&v, "lot_size"),
		market_slippage_bps,
		allow_unbacked_trading: bool_field(&v, "allow_unbacked_trading"),
	};
	Ok(Json(st.grpc.set_book_policy(&token, req).await?.into()))
}

/// `POST /cabinet/api/admin/valuation/post` — post a fund NAV, inside the move guard. There is no
/// `override` in the body (banking#232): a mark the guard refuses needs the owners'
/// approval, which moves to concierge (S2-6) and is not served here yet.
pub async fn post_valuation(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::FundNav>, ApiError> {
	let token = require_permission(&st, &jar, Permission::ValuationPost).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let v = parse_body(&body);
	let (Some(service), Some(aum)) = (required(&v, "service"), required(&v, "aum")) else {
		return Err(ApiError::BadRequest("service and aum are required".into()));
	};
	let req = iv::PostFundValuationRequest { service, aum };
	Ok(Json(st.grpc.post_valuation(&token, req).await?.into()))
}

/// `POST /cabinet/api/admin/valuation/settle` — settle a queued redemption.
pub async fn settle_redemption(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Redemption>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let Some(id) = required(&parse_body(&body), "redemption_id") else {
		return Err(ApiError::BadRequest("redemption_id is required".into()));
	};
	Ok(Json(st.grpc.settle_redemption(&token, &id).await?.into()))
}

/// `POST /cabinet/api/admin/valuation/fail` — fail (void + refund units) a queued redemption.
pub async fn fail_redemption(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Redemption>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let Some(id) = required(&parse_body(&body), "redemption_id") else {
		return Err(ApiError::BadRequest("redemption_id is required".into()));
	};
	Ok(Json(st.grpc.fail_redemption(&token, &id).await?.into()))
}

/// `GET /cabinet/api/admin/revenue` — what the platform has earned: the reserved `fee`
/// allocation in the treasury's shape — cash, supply, price and holders (#245). banking
/// answered it with one RPC; invest-core has none, so it is read as the three reads it is
/// made of, side by side. Nothing pays it out from here.
pub async fn fund_revenue(State(st): State<AppState>, jar: Cookies) -> Result<Json<dto::AllocationTreasury>, ApiError> {
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	let unavailable = |s| ApiError::read(s, "fund revenue unavailable");
	let (allocation, nav, holders) = tokio::try_join!(
		st.grpc.get_allocation(&token, FEE_ALLOCATION),
		st.grpc.fund_nav(&token, FEE_ALLOCATION),
		st.grpc.list_unit_holders(&token, FEE_ALLOCATION),
	)
	.map_err(unavailable)?;
	Ok(Json(dto::AllocationTreasury::revenue(allocation, nav, holders)))
}
