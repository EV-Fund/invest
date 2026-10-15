//! `funds` context — a holder's units at the fund's price, and the price itself.
//!
//! Position and NAV reads act on the caller's own `sub`; posting a valuation is gated on
//! `invest:valuation:post`. Subscribe and redeem arrive with the money sagas (PR 1b).
//!
//! `Result<_, Status>` is tonic's mandated handler signature; `Status` is a large
//! type we don't control, so the large-err lint does not apply in this module.
#![allow(clippy::result_large_err)]

use evconcierge_auth::claims_of;
use invest_contracts::invest::v1::{self as pb, funds_service_server::FundsService};
use invest_domain::{authz::Permission, balance::ServiceId, money::Usdt};
use tonic::{Request, Response, Status};

use crate::{
	AppState,
	application::funds as funds_app,
	services::support::{caller_id, holds_permission, map_err, require_permission, unix_now},
};

#[derive(Clone)]
pub struct FundsSvc {
	pub state: AppState,
}

impl FundsSvc {
	pub fn new(state: AppState) -> Self {
		Self { state }
	}
}

#[tonic::async_trait]
impl FundsService for FundsSvc {
	async fn get_position(&self, request: Request<pb::GetPositionRequest>) -> Result<Response<pb::Position>, Status> {
		let user = caller_id(&request)?;
		let service = ServiceId::parse(&request.get_ref().service).map_err(map_err)?;
		let view = funds_app::get_position(&self.state.fund_ports(), self.state.positions.as_ref(), user, service)
			.await
			.map_err(map_err)?;
		Ok(Response::new(position_to_proto(&view)))
	}

	async fn list_positions(&self, request: Request<pb::ListPositionsRequest>) -> Result<Response<pb::PositionList>, Status> {
		let user = caller_id(&request)?;
		let views = funds_app::list_positions(&self.state.fund_ports(), self.state.positions.as_ref(), user).await.map_err(map_err)?;
		Ok(Response::new(pb::PositionList {
			positions: views.iter().map(position_to_proto).collect(),
		}))
	}

	async fn get_fund_nav(&self, request: Request<pb::GetFundNavRequest>) -> Result<Response<pb::FundNav>, Status> {
		// Any authenticated user may read a fund's price — of a product they may see. A
		// product hidden from THIS caller is NOT_FOUND, exactly as `GetAllocation` answers,
		// unless they hold AllocationManage: a question, not a gate, since the handler
		// serves everyone and merely widens for a manager.
		let caller = caller_id(&request)?;
		let unrestricted = holds_permission(&self.state, &request, Permission::AllocationManage).await?;
		let service = ServiceId::parse(&request.get_ref().service).map_err(map_err)?;
		let view = funds_app::fund_nav_view(&self.state.fund_ports(), service, caller, unrestricted, unix_now())
			.await
			.map_err(map_err)?;
		Ok(Response::new(fund_nav_to_proto(&view)))
	}

	async fn list_fund_nav_history(&self, request: Request<pb::ListFundNavHistoryRequest>) -> Result<Response<pb::FundNavHistory>, Status> {
		// Gated exactly as `get_fund_nav`: the log of a price the caller may not see is
		// NOT_FOUND. The request names the ALLOCATION; the registry key is the service id.
		let caller = caller_id(&request)?;
		let unrestricted = holds_permission(&self.state, &request, Permission::AllocationManage).await?;
		let req = request.get_ref();
		let service = ServiceId::parse(&req.allocation).map_err(map_err)?;
		let view = funds_app::fund_nav_history(
			&self.state.fund_ports(),
			self.state.positions.as_ref(),
			service,
			caller,
			unrestricted,
			req.from,
			req.to,
			unix_now(),
		)
		.await
		.map_err(map_err)?;
		Ok(Response::new(pb::FundNavHistory {
			allocation: view.service.to_string(),
			marks: view
				.marks
				.iter()
				.map(|m| pb::NavMark {
					nav: m.nav.to_decimal_string(),
					aum: m.aum.to_decimal_string(),
					posted_at: m.posted_at_unix,
				})
				.collect(),
			participation: view
				.participation
				.iter()
				.map(|p| pb::ParticipationPoint {
					at: p.at_unix,
					value: p.value.to_decimal_string(),
				})
				.collect(),
			truncated: view.truncated,
		}))
	}

	async fn post_fund_valuation(&self, request: Request<pb::PostFundValuationRequest>) -> Result<Response<pb::FundNav>, Status> {
		require_permission(&self.state, &request, Permission::ValuationPost).await?;
		let caller = caller_id(&request)?;
		// The token's `sub`, as recorded — the subject a redeem cooldown compares against
		// (PR 1b), so the two spellings agree by construction.
		let posted_by = claims_of(&request).ok_or_else(|| Status::unauthenticated("missing claims"))?.sub.clone();
		let req = request.into_inner();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		let aum = Usdt::parse_decimal(&req.aum).map_err(map_err)?;
		funds_app::post_fund_valuation(
			self.state.allocations.as_ref(),
			self.state.nav.as_ref(),
			self.state.ledger.as_ref(),
			service.clone(),
			aum,
			&posted_by,
			unix_now(),
		)
		.await
		.map_err(map_err)?;
		// Answer by re-reading the view rather than mapping the mark by hand: the response
		// carries the allocation's supply headroom too, and one construction path is what
		// keeps this route and `GetFundNav` from drifting apart field by field. Unrestricted:
		// ValuationPost already admitted the caller to a product in any state, and the mark
		// they just wrote must not vanish behind their own (possibly `hidden`) access level.
		let view = funds_app::fund_nav_view(&self.state.fund_ports(), service, caller, true, unix_now()).await.map_err(map_err)?;
		Ok(Response::new(fund_nav_to_proto(&view)))
	}
}

fn position_to_proto(view: &funds_app::PositionView) -> pb::Position {
	pb::Position {
		service: view.service.to_string(),
		units: view.units.to_decimal_string(),
		nav: view.nav.to_decimal_string(),
		value: view.value.to_decimal_string(),
		cost_basis: view.cost_basis.to_decimal_string(),
		pnl: signed_diff(view.value, view.cost_basis),
		nav_as_of: view.nav_as_of,
		units_in_orders: view.units_in_orders.to_decimal_string(),
	}
}

/// The single place a `FundNav` is built — `GetFundNav` and `PostFundValuation` answer with
/// the same shape, and two hand-rolled constructions of one message is how a field ends up
/// populated on one route and empty on the other.
fn fund_nav_to_proto(view: &funds_app::FundNavView) -> pb::FundNav {
	pb::FundNav {
		service: view.service.to_string(),
		nav: view.nav.to_decimal_string(),
		aum: view.aum.map(|a| a.to_decimal_string()).unwrap_or_default(),
		units_outstanding: view.units_outstanding.to_decimal_string(),
		posted_at: view.posted_at,
		stale: view.stale,
		unit_cap: view.unit_cap.to_decimal_string(),
		remaining_capacity: view.remaining_capacity.to_decimal_string(),
	}
}

/// `value − cost_basis` as a signed decimal string (P&L; negative on a loss). `Usdt` is
/// unsigned, so the sign is applied here at the wire boundary.
fn signed_diff(value: Usdt, cost_basis: Usdt) -> String {
	if value >= cost_basis {
		value.checked_sub(cost_basis).unwrap_or(Usdt::ZERO).to_decimal_string()
	} else {
		format!("-{}", cost_basis.checked_sub(value).unwrap_or(Usdt::ZERO).to_decimal_string())
	}
}
