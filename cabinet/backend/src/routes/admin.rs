//! The allocation console — the operator surface behind `/cabinet/api/admin/*`.
//!
//! Every handler is gated first by [`require_permission`] on an `invest:*` permission as
//! concierge resolves the caller's seat, then forwards the caller's access token to
//! invest-core, which re-checks the same permission against its own projection (defense
//! in depth; a missing one surfaces as 403). Mutations verify CSRF and `Origin` first,
//! as the investor routes do, so a forged request costs no session read and no `GetMe`. The gates mirror invest-core's: `invest:allocation:manage` everywhere
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
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
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
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
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
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
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
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	let v = parse_body(&body);
	// The key is the retry contract invest-core requires, as on `/allocations/issue`: the
	// console makes one per submission and re-sends it on a timeout, so a double click
	// settles once. Not invented here — a key the BFF made up per request protects nothing.
	let (Some(service), Some(idempotency_key)) = (required(&v, "service"), required(&v, "idempotency_key")) else {
		return Err(ApiError::BadRequest("service and idempotency_key are required".into()));
	};
	let settlement = st.grpc.settle_fee_shares(&token, &service, &required(&v, "units").unwrap_or_default(), &idempotency_key).await?;
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
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
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
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
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
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
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
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
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
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
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
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
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
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
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
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
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
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
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
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
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
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
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
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_permission(&st, &jar, Permission::ValuationPost).await?;
	let v = parse_body(&body);
	let (Some(service), Some(aum)) = (required(&v, "service"), required(&v, "aum")) else {
		return Err(ApiError::BadRequest("service and aum are required".into()));
	};
	let req = iv::PostFundValuationRequest { service, aum };
	Ok(Json(st.grpc.post_valuation(&token, req).await?.into()))
}

/// `POST /cabinet/api/admin/valuation/settle` — settle a queued redemption.
pub async fn settle_redemption(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Redemption>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
	let Some(id) = required(&parse_body(&body), "redemption_id") else {
		return Err(ApiError::BadRequest("redemption_id is required".into()));
	};
	Ok(Json(st.grpc.settle_redemption(&token, &id).await?.into()))
}

/// `POST /cabinet/api/admin/valuation/fail` — fail (void + refund units) a queued redemption.
pub async fn fail_redemption(State(st): State<AppState>, jar: Cookies, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Redemption>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_permission(&st, &jar, Permission::AllocationManage).await?;
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

#[cfg(test)]
mod admin_route_tests {
	// `Status` is a large error type tonic mandates in handler signatures.
	#![allow(clippy::result_large_err)]

	use std::sync::{Arc, Mutex};

	use axum::http::StatusCode;
	use invest_contracts::invest::v1::{
		allocations_service_server::{AllocationsService, AllocationsServiceServer},
		fees_service_server::{FeesService, FeesServiceServer},
		funds_service_server::{FundsService, FundsServiceServer},
	};
	use tonic::{Code, Request as GrpcRequest, Response as GrpcResponse, Status, transport::Server};

	use super::*;
	use crate::routes::test_support::{CSRF, Harness, INVESTOR, MANAGER, Signed, USER, bearer_ok, request, serve_core};

	const SERVICE: &str = "quy-nhon";
	const HOLDER: &str = "6f1c2a3e-0b4d-4c5e-8f9a-1b2c3d4e5f60";
	const NOT_REACHED: &str = "not reached by the console tests";

	/// What the BFF forwarded, so a test asserts on the request invest-core would have seen.
	#[derive(Default)]
	struct Seen {
		register: Option<iv::RegisterAllocationRequest>,
		set_access: Option<iv::SetAllocationAccessRequest>,
		grant: Option<iv::GrantAllocationAccessRequest>,
		issue: Option<iv::IssueUnitsRequest>,
		retire: Option<iv::RetireUnitsRequest>,
		schedule: Option<iv::ScheduleFeePolicyRequest>,
		settle_fees: Option<iv::SettleFeeSharesRequest>,
		valuation: Option<iv::PostFundValuationRequest>,
		subscribe: Option<iv::SubscribeRequest>,
		calls: usize,
	}

	#[derive(Clone, Default)]
	struct Core {
		fail_with: Option<Code>,
		seen: Arc<Mutex<Seen>>,
	}

	impl Core {
		/// invest-core accepts the session's access token and nothing else.
		fn guard<T>(&self, request: &GrpcRequest<T>) -> Result<(), Status> {
			self.seen.lock().unwrap().calls += 1;
			if !bearer_ok(request) {
				return Err(Status::unauthenticated("invest-core requires the session's access token"));
			}
			match self.fail_with {
				Some(code) => Err(Status::new(code, "upstream refused")),
				None => Ok(()),
			}
		}

		fn calls(&self) -> usize {
			self.seen.lock().unwrap().calls
		}
	}

	fn allocation(service: &str, access: &str) -> iv::Allocation {
		iv::Allocation {
			service: service.into(),
			title: "Quy Nhon".into(),
			state: "open".into(),
			created_at: 1_750_000_000,
			updated_at: 1_750_000_300,
			unit_cap: "1000".into(),
			icon: "real_estate".into(),
			access: access.into(),
			caller_access: "view".into(),
			backing: "cash".into(),
			..Default::default()
		}
	}

	fn holding(kind: &str, id: &str, units: &str) -> iv::UnitHolding {
		iv::UnitHolding {
			holder: Some(iv::UnitHolderRef { kind: kind.into(), id: id.into() }),
			units: units.into(),
		}
	}

	fn issuance(req_service: String, user_id: String, units: String, cost_basis: String, source: &str) -> iv::UnitIssuance {
		iv::UnitIssuance {
			id: "7c1e".into(),
			service: req_service,
			holder_kind: "user".into(),
			holder_id: user_id,
			units,
			nav: "1.25".into(),
			cost_basis: if cost_basis.is_empty() { "16250".into() } else { cost_basis },
			state: "queued".into(),
			created_at: 1_750_000_600,
			source: source.into(),
		}
	}

	#[tonic::async_trait]
	impl AllocationsService for Core {
		async fn list_allocations(&self, request: GrpcRequest<iv::ListAllocationsRequest>) -> Result<GrpcResponse<iv::AllocationList>, Status> {
			self.guard(&request)?;
			Ok(GrpcResponse::new(iv::AllocationList {
				allocations: vec![allocation(SERVICE, "invest")],
			}))
		}

		async fn get_allocation(&self, request: GrpcRequest<iv::GetAllocationRequest>) -> Result<GrpcResponse<iv::Allocation>, Status> {
			self.guard(&request)?;
			Ok(GrpcResponse::new(iv::Allocation {
				title: "Fees".into(),
				..allocation(&request.into_inner().service, "hidden")
			}))
		}

		async fn register_allocation(&self, request: GrpcRequest<iv::RegisterAllocationRequest>) -> Result<GrpcResponse<iv::Allocation>, Status> {
			self.guard(&request)?;
			let req = request.into_inner();
			let answer = allocation(&req.service, "hidden");
			self.seen.lock().unwrap().register = Some(req);
			Ok(GrpcResponse::new(answer))
		}

		async fn update_allocation(&self, _: GrpcRequest<iv::UpdateAllocationRequest>) -> Result<GrpcResponse<iv::Allocation>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn set_allocation_state(&self, _: GrpcRequest<iv::SetAllocationStateRequest>) -> Result<GrpcResponse<iv::Allocation>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn set_allocation_unit_cap(&self, _: GrpcRequest<iv::SetAllocationUnitCapRequest>) -> Result<GrpcResponse<iv::Allocation>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn set_allocation_access(&self, request: GrpcRequest<iv::SetAllocationAccessRequest>) -> Result<GrpcResponse<iv::Allocation>, Status> {
			self.guard(&request)?;
			let req = request.into_inner();
			let answer = allocation(&req.service, &req.access);
			self.seen.lock().unwrap().set_access = Some(req);
			Ok(GrpcResponse::new(answer))
		}

		async fn set_allocation_backing(&self, _: GrpcRequest<iv::SetAllocationBackingRequest>) -> Result<GrpcResponse<iv::Allocation>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn grant_allocation_access(&self, request: GrpcRequest<iv::GrantAllocationAccessRequest>) -> Result<GrpcResponse<iv::AllocationAccessGrant>, Status> {
			self.guard(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().grant = Some(req.clone());
			Ok(GrpcResponse::new(iv::AllocationAccessGrant {
				service: req.service,
				user_id: req.user_id,
				level: req.level,
				granted_by: USER.to_string(),
				granted_at: 1_750_000_400,
			}))
		}

		async fn revoke_allocation_access(&self, _: GrpcRequest<iv::RevokeAllocationAccessRequest>) -> Result<GrpcResponse<iv::RevokeAllocationAccessResponse>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn list_allocation_access_grants(&self, request: GrpcRequest<iv::ListAllocationAccessGrantsRequest>) -> Result<GrpcResponse<iv::AllocationAccessGrantList>, Status> {
			self.guard(&request)?;
			Ok(GrpcResponse::new(iv::AllocationAccessGrantList {
				grants: vec![iv::AllocationAccessGrant {
					service: request.into_inner().service,
					user_id: HOLDER.into(),
					level: "invest".into(),
					granted_by: USER.to_string(),
					granted_at: 1_750_000_400,
				}],
			}))
		}

		async fn issue_units(&self, request: GrpcRequest<iv::IssueUnitsRequest>) -> Result<GrpcResponse<iv::UnitIssuance>, Status> {
			self.guard(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().issue = Some(req.clone());
			Ok(GrpcResponse::new(issuance(req.service, req.user_id, req.units, req.cost_basis, "mint")))
		}

		async fn retire_units(&self, request: GrpcRequest<iv::RetireUnitsRequest>) -> Result<GrpcResponse<iv::UnitIssuance>, Status> {
			self.guard(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().retire = Some(req.clone());
			Ok(GrpcResponse::new(issuance(req.service, req.user_id, req.units, req.cost_basis, "retire")))
		}

		/// A person and the `fee` allocation, each a line — the shape the holders screen
		/// renders without summing anything.
		async fn list_unit_holders(&self, request: GrpcRequest<iv::ListUnitHoldersRequest>) -> Result<GrpcResponse<iv::UnitHolders>, Status> {
			self.guard(&request)?;
			Ok(GrpcResponse::new(iv::UnitHolders {
				service: request.into_inner().service,
				units_outstanding: "13250".into(),
				queued_units: "100".into(),
				holders: vec![holding("user", HOLDER, "13000"), holding("allocation", "fee", "250")],
			}))
		}
	}

	#[tonic::async_trait]
	impl FeesService for Core {
		async fn get_fee_policy(&self, _: GrpcRequest<iv::GetFeePolicyRequest>) -> Result<GrpcResponse<iv::FeePolicy>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn list_fee_policies(&self, request: GrpcRequest<iv::ListFeePoliciesRequest>) -> Result<GrpcResponse<iv::FeePolicyList>, Status> {
			self.guard(&request)?;
			Ok(GrpcResponse::new(iv::FeePolicyList {
				policies: vec![iv::FeePolicy {
					service: SERVICE.into(),
					configured: true,
					management_bps: 200,
					performance_bps: 2_000,
					hurdle_bps: 500,
					basis: "invested_capital".into(),
					crystallization: "annual".into(),
					..Default::default()
				}],
			}))
		}

		async fn schedule_fee_policy(&self, request: GrpcRequest<iv::ScheduleFeePolicyRequest>) -> Result<GrpcResponse<iv::FeePolicyChange>, Status> {
			self.guard(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().schedule = Some(req.clone());
			Ok(GrpcResponse::new(iv::FeePolicyChange {
				id: "change-1".into(),
				service: req.service,
				state: "scheduled".into(),
				management_bps: req.management_bps,
				performance_bps: req.performance_bps,
				hurdle_bps: req.hurdle_bps,
				effective_from: req.effective_from.max(1_750_086_400),
				reason: req.reason,
				..Default::default()
			}))
		}

		async fn cancel_fee_policy_change(&self, _: GrpcRequest<iv::CancelFeePolicyChangeRequest>) -> Result<GrpcResponse<iv::FeePolicyChange>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn acknowledge_undelivered_notices(&self, _: GrpcRequest<iv::AcknowledgeUndeliveredNoticesRequest>) -> Result<GrpcResponse<iv::FeePolicyChange>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn list_fee_policy_changes(&self, _: GrpcRequest<iv::ListFeePolicyChangesRequest>) -> Result<GrpcResponse<iv::FeePolicyChangeList>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn list_fee_assessments(&self, _: GrpcRequest<iv::ListFeeAssessmentsRequest>) -> Result<GrpcResponse<iv::FeeAssessmentList>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn get_accrued_fees(&self, _: GrpcRequest<iv::GetAccruedFeesRequest>) -> Result<GrpcResponse<iv::AccruedFees>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn list_fund_fee_assessments(&self, _: GrpcRequest<iv::ListFundFeeAssessmentsRequest>) -> Result<GrpcResponse<iv::FeeAssessmentList>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn get_fee_shares(&self, _: GrpcRequest<iv::GetFeeSharesRequest>) -> Result<GrpcResponse<iv::FeeShares>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn settle_fee_shares(&self, request: GrpcRequest<iv::SettleFeeSharesRequest>) -> Result<GrpcResponse<iv::FeeSettlement>, Status> {
			self.guard(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().settle_fees = Some(req.clone());
			Ok(GrpcResponse::new(iv::FeeSettlement {
				service: req.service,
				units: "12.500000".into(),
				nav: "110.00".into(),
				cash: "1375.00".into(),
			}))
		}
	}

	#[tonic::async_trait]
	impl FundsService for Core {
		async fn subscribe(&self, request: GrpcRequest<iv::SubscribeRequest>) -> Result<GrpcResponse<iv::Subscription>, Status> {
			self.guard(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().subscribe = Some(req.clone());
			Ok(GrpcResponse::new(iv::Subscription {
				id: "sub-1".into(),
				service: req.service,
				cash: req.amount,
				state: "pending".into(),
				..Default::default()
			}))
		}

		async fn list_subscriptions(&self, _: GrpcRequest<iv::ListSubscriptionsRequest>) -> Result<GrpcResponse<iv::SubscriptionList>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn redeem(&self, _: GrpcRequest<iv::RedeemRequest>) -> Result<GrpcResponse<iv::Redemption>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn cancel_redemption(&self, _: GrpcRequest<iv::CancelRedemptionRequest>) -> Result<GrpcResponse<iv::Redemption>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn list_redemptions(&self, _: GrpcRequest<iv::ListRedemptionsRequest>) -> Result<GrpcResponse<iv::RedemptionList>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn settle_redemption(&self, _: GrpcRequest<iv::SettleRedemptionRequest>) -> Result<GrpcResponse<iv::Redemption>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn fail_redemption(&self, _: GrpcRequest<iv::FailRedemptionRequest>) -> Result<GrpcResponse<iv::Redemption>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn list_redemption_queue(&self, _: GrpcRequest<iv::ListRedemptionQueueRequest>) -> Result<GrpcResponse<iv::RedemptionQueue>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn get_position(&self, _: GrpcRequest<iv::GetPositionRequest>) -> Result<GrpcResponse<iv::Position>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn list_positions(&self, _: GrpcRequest<iv::ListPositionsRequest>) -> Result<GrpcResponse<iv::PositionList>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		/// The reserved `fee` allocation's price: its cash (`aum`) over its units.
		async fn get_fund_nav(&self, request: GrpcRequest<iv::GetFundNavRequest>) -> Result<GrpcResponse<iv::FundNav>, Status> {
			self.guard(&request)?;
			Ok(GrpcResponse::new(iv::FundNav {
				service: request.into_inner().service,
				nav: "1.10".into(),
				aum: "14575.00".into(),
				units_outstanding: "13250".into(),
				..Default::default()
			}))
		}

		async fn list_fund_nav_history(&self, _: GrpcRequest<iv::ListFundNavHistoryRequest>) -> Result<GrpcResponse<iv::FundNavHistory>, Status> {
			Err(Status::unimplemented(NOT_REACHED))
		}

		async fn post_fund_valuation(&self, request: GrpcRequest<iv::PostFundValuationRequest>) -> Result<GrpcResponse<iv::FundNav>, Status> {
			self.guard(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().valuation = Some(req.clone());
			Ok(GrpcResponse::new(iv::FundNav {
				service: req.service,
				nav: "1.25".into(),
				aum: req.aum,
				posted_at: 1_750_000_700,
				..Default::default()
			}))
		}
	}

	// ── harness ─────────────────────────────────────────────────────────────────

	struct Fixture {
		h: Harness,
		signed: Signed,
		core: Core,
	}

	async fn fixture(core: Core, permissions: &[&str]) -> Option<Fixture> {
		let addr = serve_core(
			Server::builder()
				.add_service(AllocationsServiceServer::new(core.clone()))
				.add_service(FeesServiceServer::new(core.clone()))
				.add_service(FundsServiceServer::new(core.clone())),
		)
		.await;
		let h = Harness::new(Some(addr)).await?;
		let signed = h.sign_in(permissions).await;
		Some(Fixture { h, signed, core })
	}

	impl Fixture {
		async fn send(&self, method: &str, uri: &str, body: Option<&str>, csrf: bool) -> (StatusCode, Value) {
			let (status, body, _) = self.h.send(request(method, uri, Some(&self.signed), body, csrf)).await;
			(status, body)
		}

		fn seen(&self) -> std::sync::MutexGuard<'_, Seen> {
			self.core.seen.lock().unwrap()
		}
	}

	const SCHEDULE: &str = r#"{"service":"quy-nhon","management_bps":200,"performance_bps":2000,"hurdle_bps":500,"basis":"invested_capital","crystallization":"annual"}"#;

	// ── the gates ───────────────────────────────────────────────────────────────

	#[tokio::test]
	async fn no_session_reaches_nothing() {
		let Some(f) = fixture(Core::default(), MANAGER).await else { return };
		for (method, uri, body) in [
			("GET", "/cabinet/api/admin/allocations", None),
			("GET", "/cabinet/api/admin/fees/policies", None),
			("GET", "/cabinet/api/admin/revenue", None),
			("POST", "/cabinet/api/admin/fees/policy", Some(SCHEDULE)),
			("POST", "/cabinet/api/admin/valuation/post", Some(r#"{"service":"quy-nhon","aum":"1"}"#)),
		] {
			// The CSRF pair is present so the mutations pass that gate, which runs first, and
			// are refused on the session — the thing this test is about.
			let mut req = request(method, uri, None, body, true);
			req.headers_mut().insert(axum::http::header::COOKIE, format!("invest_csrf={CSRF}").parse().unwrap());
			let (status, _, _) = f.h.send(req).await;
			assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
		}
		assert_eq!(f.core.calls(), 0, "nothing reached invest-core");
	}

	/// An investor's session is perfectly valid — the permission concierge resolves for the
	/// seat is what keeps them out, before invest-core is asked.
	#[tokio::test]
	async fn an_investor_is_refused_the_console() {
		let Some(f) = fixture(Core::default(), INVESTOR).await else { return };
		for (method, uri, body) in [
			("GET", "/cabinet/api/admin/allocations", None),
			("GET", "/cabinet/api/admin/fees/policies", None),
			("GET", "/cabinet/api/admin/revenue", None),
			("POST", "/cabinet/api/admin/fees/policy", Some(SCHEDULE)),
			("POST", "/cabinet/api/admin/valuation/post", Some(r#"{"service":"quy-nhon","aum":"1"}"#)),
		] {
			let (status, body) = f.send(method, uri, body, true).await;
			assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
			assert!(body["error"].as_str().unwrap().starts_with("requires invest:"), "{body}");
		}
		assert_eq!(f.core.calls(), 0);
	}

	/// The two console permissions are distinct, as in invest-core: posting a valuation is
	/// `invest:valuation:post`, everything else `invest:allocation:manage`.
	#[tokio::test]
	async fn a_valuation_poster_posts_but_does_not_manage() {
		let Some(f) = fixture(Core::default(), &["invest:valuation:post"]).await else { return };
		let (status, body) = f.send("POST", "/cabinet/api/admin/valuation/post", Some(r#"{"service":"quy-nhon","aum":"16250"}"#), true).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!((body["aum"].as_str(), body["posted_at"].as_str()), (Some("16250"), Some("1750000700")));
		assert_eq!(f.seen().valuation.as_ref().map(|r| (r.service.as_str(), r.aum.as_str())), Some((SERVICE, "16250")));
		let (status, _) = f.send("GET", "/cabinet/api/admin/allocations", None, false).await;
		assert_eq!(status, StatusCode::FORBIDDEN);
	}

	#[tokio::test]
	async fn a_seat_without_permissions_holds_nothing() {
		let Some(f) = fixture(Core::default(), &[]).await else { return };
		let (status, _) = f.send("GET", "/cabinet/api/admin/allocations", None, false).await;
		assert_eq!(status, StatusCode::FORBIDDEN);
	}

	/// `GetMe` is asked once a minute per session, not once per request.
	#[tokio::test]
	async fn the_permissions_are_cached_for_the_console() {
		let Some(f) = fixture(Core::default(), MANAGER).await else { return };
		for _ in 0..3 {
			assert_eq!(f.send("GET", "/cabinet/api/admin/allocations", None, false).await.0, StatusCode::OK);
		}
		assert_eq!(f.h.concierge.me_calls(), 1);
	}

	#[tokio::test]
	async fn a_mutation_without_the_csrf_header_is_refused() {
		let Some(f) = fixture(Core::default(), MANAGER).await else { return };
		let (status, body) = f.send("POST", "/cabinet/api/admin/fees/policy", Some(SCHEDULE), false).await;
		assert_eq!((status, body["error"].as_str()), (StatusCode::FORBIDDEN, Some("csrf")));
		assert!(f.seen().schedule.is_none());
	}

	/// A forged mutation is refused on CSRF before the session or the permission is read —
	/// no `GetMe` is spent on it.
	#[tokio::test]
	async fn csrf_is_checked_before_the_console_gate() {
		let Some(f) = fixture(Core::default(), INVESTOR).await else { return };
		let (status, body) = f.send("POST", "/cabinet/api/admin/fees/policy", Some(SCHEDULE), false).await;
		assert_eq!((status, body["error"].as_str()), (StatusCode::FORBIDDEN, Some("csrf")));
		assert_eq!(f.h.concierge.me_calls(), 0);
	}

	// ── fees ────────────────────────────────────────────────────────────────────

	#[tokio::test]
	async fn pricing_a_fund_forwards_the_whole_schedule() {
		let Some(f) = fixture(Core::default(), MANAGER).await else { return };
		let body = r#"{"service":"quy-nhon","management_bps":250,"performance_bps":2000,"hurdle_bps":0,"basis":"invested_capital","crystallization":"annual","effective_from":1760000000,"reason":"repricing"}"#;
		let (status, response) = f.send("POST", "/cabinet/api/admin/fees/policy", Some(body), true).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(response["state"], "scheduled");
		let forwarded = f.seen().schedule.clone().expect("invest-core saw the schedule");
		assert_eq!(
			(forwarded.management_bps, forwarded.performance_bps, forwarded.hurdle_bps),
			(250, 2_000, 0),
			"an explicit zero is a rate"
		);
		assert_eq!((forwarded.effective_from, forwarded.reason.as_str()), (1_760_000_000, "repricing"));
	}

	/// A rate this layer cannot read is refused, never coerced to zero — zero is a price.
	#[tokio::test]
	async fn a_missing_or_malformed_rate_is_refused_rather_than_read_as_zero() {
		let Some(f) = fixture(Core::default(), MANAGER).await else { return };
		for body in [
			r#"{"service":"quy-nhon","performance_bps":2000,"hurdle_bps":500,"basis":"invested_capital","crystallization":"annual"}"#,
			r#"{"service":"quy-nhon","management_bps":"200","performance_bps":2000,"hurdle_bps":500,"basis":"invested_capital","crystallization":"annual"}"#,
			r#"{"service":"quy-nhon","management_bps":2.5,"performance_bps":2000,"hurdle_bps":500,"basis":"invested_capital","crystallization":"annual"}"#,
			r#"{"service":"quy-nhon","management_bps":-1,"performance_bps":2000,"hurdle_bps":500,"basis":"invested_capital","crystallization":"annual"}"#,
			r#"{"service":"quy-nhon","management_bps":200,"performance_bps":2000,"hurdle_bps":500,"basis":"invested_capital","crystallization":"annual","effective_from":"soon"}"#,
		] {
			assert_eq!(f.send("POST", "/cabinet/api/admin/fees/policy", Some(body), true).await.0, StatusCode::BAD_REQUEST, "{body}");
		}
		assert!(f.seen().schedule.is_none());
	}

	#[tokio::test]
	async fn settling_without_an_amount_asks_for_the_whole_balance() {
		let Some(f) = fixture(Core::default(), MANAGER).await else { return };
		let (status, body) = f
			.send("POST", "/cabinet/api/admin/fees/settle", Some(r#"{"service":"quy-nhon","idempotency_key":"settle-1"}"#), true)
			.await;
		assert_eq!((status, body["cash"].as_str()), (StatusCode::OK, Some("1375.00")));
		assert_eq!(f.seen().settle_fees.as_ref().map(|r| (r.units.as_str(), r.idempotency_key.as_str())), Some(("", "settle-1")));
	}

	/// The settlement's key is invest-core's retry contract; one the console did not send is
	/// a 400 here, not a key the BFF makes up.
	#[tokio::test]
	async fn a_settlement_without_its_key_is_refused() {
		let Some(f) = fixture(Core::default(), MANAGER).await else { return };
		let (status, _) = f.send("POST", "/cabinet/api/admin/fees/settle", Some(r#"{"service":"quy-nhon"}"#), true).await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
		assert!(f.seen().settle_fees.is_none());
	}

	// ── allocations ─────────────────────────────────────────────────────────────

	#[tokio::test]
	async fn registering_forwards_the_scope_the_cash_lives_in() {
		let Some(f) = fixture(Core::default(), MANAGER).await else { return };
		let (status, _) = f
			.send(
				"POST",
				"/cabinet/api/admin/allocations/register",
				Some(r#"{"service":"car-wash","title":"Car wash","scope":"sa"}"#),
				true,
			)
			.await;
		assert_eq!(status, StatusCode::OK);
		let (status, _) = f
			.send("POST", "/cabinet/api/admin/allocations/register", Some(r#"{"service":"villa","title":"Villa"}"#), true)
			.await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(
			f.seen().register.as_ref().map(|r| (r.service.as_str(), r.scope.as_str())),
			Some(("villa", "")),
			"absent is invest's own"
		);
	}

	#[tokio::test]
	async fn an_access_level_outside_the_vocabulary_is_refused_locally() {
		let Some(f) = fixture(Core::default(), MANAGER).await else { return };
		let (status, body) = f
			.send("POST", "/cabinet/api/admin/allocations/access", Some(r#"{"service":"quy-nhon","access":"public"}"#), true)
			.await;
		assert_eq!(status, StatusCode::BAD_REQUEST);
		assert!(body["error"].as_str().unwrap().contains("hidden, view, invest"), "{body}");
		let (status, _) = f
			.send(
				"POST",
				"/cabinet/api/admin/allocations/grants/grant",
				Some(&format!(r#"{{"service":"quy-nhon","user_id":"{HOLDER}","level":"hidden"}}"#)),
				true,
			)
			.await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "a grant only ever raises");
		assert!(f.seen().set_access.is_none() && f.seen().grant.is_none());

		let (status, body) = f
			.send("POST", "/cabinet/api/admin/allocations/access", Some(r#"{"service":"quy-nhon","access":"view"}"#), true)
			.await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!((body["access"].as_str(), body["caller_access"].as_str()), (Some("view"), Some("view")));
	}

	/// The grants list names the investor by id only: an `invest` token cannot ask concierge
	/// about anyone else, so `email` is `null` rather than guessed.
	#[tokio::test]
	async fn the_grant_list_keeps_every_column_and_names_nobody_by_email() {
		let Some(f) = fixture(Core::default(), MANAGER).await else { return };
		let (status, body) = f.send("GET", "/cabinet/api/admin/allocations/grants?service=quy-nhon", None, false).await;
		assert_eq!(status, StatusCode::OK);
		let grant = &body["grants"][0];
		assert_eq!(
			(grant["user_id"].as_str(), grant["level"].as_str(), grant["granted_at"].as_str()),
			(Some(HOLDER), Some("invest"), Some("1750000400"))
		);
		assert!(grant["email"].is_null());
	}

	#[tokio::test]
	async fn issuing_and_retiring_forward_the_person_and_relay_the_row() {
		let Some(f) = fixture(Core::default(), MANAGER).await else { return };
		let issue = format!(r#"{{"service":"quy-nhon","user_id":"{HOLDER}","units":"13000","idempotency_key":"k-1"}}"#);
		let (status, body) = f.send("POST", "/cabinet/api/admin/allocations/issue", Some(&issue), true).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(
			(body["source"].as_str(), body["holder_id"].as_str(), body["cost_basis"].as_str()),
			(Some("mint"), Some(HOLDER), Some("16250"))
		);

		let retire = format!(r#"{{"service":"quy-nhon","user_id":"{HOLDER}","units":"10","idempotency_key":"k-2","cost_basis":"12.5"}}"#);
		let (status, body) = f.send("POST", "/cabinet/api/admin/allocations/retire", Some(&retire), true).await;
		assert_eq!((status, body["source"].as_str()), (StatusCode::OK, Some("retire")));
		let seen = f.seen();
		assert_eq!(seen.issue.as_ref().map(|r| r.idempotency_key.as_str()), Some("k-1"));
		let retired = seen.retire.as_ref().expect("invest-core saw the burn");
		assert!(!retired.force, "force is the operator's explicit override, never a default");
		assert_eq!(retired.cost_basis, "12.5");
	}

	#[tokio::test]
	async fn a_malformed_mint_is_refused_before_invest_core() {
		let Some(f) = fixture(Core::default(), MANAGER).await else { return };
		for body in [
			r#"{"service":"quy-nhon","units":"1","idempotency_key":"k"}"#,
			r#"{"service":"quy-nhon","user_id":"u","units":"1"}"#,
		] {
			assert_eq!(
				f.send("POST", "/cabinet/api/admin/allocations/issue", Some(body), true).await.0,
				StatusCode::BAD_REQUEST,
				"{body}"
			);
		}
		assert!(f.seen().issue.is_none());
	}

	#[tokio::test]
	async fn the_holders_route_relays_the_cap_table_line_by_line() {
		let Some(f) = fixture(Core::default(), MANAGER).await else { return };
		let (status, body) = f.send("GET", "/cabinet/api/admin/allocations/holders?service=quy-nhon", None, false).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!((body["units_outstanding"].as_str(), body["queued_units"].as_str()), (Some("13250"), Some("100")));
		assert_eq!(body["holders"][1]["holder"], serde_json::json!({ "kind": "allocation", "id": "fee" }));
	}

	/// banking answered Revenue with one RPC; here it is the `fee` allocation read three
	/// ways, and the answer keeps the treasury shape the screen reads.
	#[tokio::test]
	async fn revenue_is_the_fee_allocation_in_the_treasury_shape() {
		let Some(f) = fixture(Core::default(), MANAGER).await else { return };
		let (status, body) = f.send("GET", "/cabinet/api/admin/revenue", None, false).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(
			(body["service"].as_str(), body["title"].as_str(), body["access"].as_str()),
			(Some("fee"), Some("Fees"), Some("hidden"))
		);
		assert_eq!(body["claim"], serde_json::json!({ "posted": "14575.00", "available": "14575.00", "reserved": "0" }));
		assert_eq!(
			(body["nav"].as_str(), body["nav_posted_at"].as_str(), body["units_outstanding"].as_str()),
			(Some("1.10"), Some("0"), Some("13250"))
		);
		assert_eq!(body["holders"].as_array().map(Vec::len), Some(2));
		assert!(body.get("banking_treasury").is_none(), "banking's own figure is not invest's to show");
	}

	#[tokio::test]
	async fn a_failing_read_maps_the_code_but_hides_the_detail() {
		let core = Core {
			fail_with: Some(Code::Unavailable),
			..Core::default()
		};
		let Some(f) = fixture(core, MANAGER).await else { return };
		let (status, body) = f.send("GET", "/cabinet/api/admin/allocations", None, false).await;
		assert_eq!((status, body["error"].as_str()), (StatusCode::SERVICE_UNAVAILABLE, Some("allocations unavailable")));
	}

	/// The investor's own mutation carries the session's token and the body as given.
	#[tokio::test]
	async fn subscribing_forwards_the_fund_and_the_amount() {
		let Some(f) = fixture(Core::default(), INVESTOR).await else { return };
		let (status, body) = f.send("POST", "/cabinet/api/funds/subscribe", Some(r#"{"service":"quy-nhon","amount":"250"}"#), true).await;
		assert_eq!((status, body["state"].as_str()), (StatusCode::OK, Some("pending")));
		assert_eq!(f.seen().subscribe.as_ref().map(|r| (r.service.as_str(), r.amount.as_str())), Some((SERVICE, "250")));
	}
}
