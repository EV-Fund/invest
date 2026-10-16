//! `funds` context — a holder's units at the fund's price, and the price itself.
//!
//! Subscribe, redeem, position and NAV reads act on the caller's own `sub`; posting a
//! valuation is gated on `invest:valuation:post`, and the redemption queue — settle, fail,
//! list — on `invest:allocation:manage`.
//!
//! `Result<_, Status>` is tonic's mandated handler signature; `Status` is a large
//! type we don't control, so the large-err lint does not apply in this module.
#![allow(clippy::result_large_err)]

use evconcierge_auth::claims_of;
use invest_contracts::invest::v1::{self as pb, funds_service_server::FundsService};
use invest_domain::{
	authz::Permission,
	balance::ServiceId,
	money::{Shares, Usdt},
	redemptions::Redemption,
};
use tonic::{Request, Response, Status};

use crate::{
	AppState,
	application::funds as funds_app,
	ports::{RedemptionRecord, SubscriptionRecord},
	services::support::{active_caller, caller_id, holds_permission, map_err, parse_redemption_id, require_permission, unix_now},
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
	async fn subscribe(&self, request: Request<pb::SubscribeRequest>) -> Result<Response<pb::Subscription>, Status> {
		let user = active_caller(&self.state, &request).await?;
		let req = request.into_inner();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		let amount = Usdt::parse_decimal(&req.amount).map_err(map_err)?;
		let record = funds_app::subscribe(
			&self.state.fund_ports(),
			self.state.subscriptions.as_ref(),
			self.state.users.as_ref(),
			user,
			service,
			amount,
			unix_now(),
		)
		.await
		.map_err(map_err)?;
		Ok(Response::new(subscription_to_proto(&record)))
	}

	async fn list_subscriptions(&self, request: Request<pb::ListSubscriptionsRequest>) -> Result<Response<pb::SubscriptionList>, Status> {
		let user = caller_id(&request)?;
		let records = funds_app::list_subscriptions(self.state.subscriptions.as_ref(), user).await.map_err(map_err)?;
		Ok(Response::new(pb::SubscriptionList {
			subscriptions: records.iter().map(subscription_to_proto).collect(),
		}))
	}

	async fn redeem(&self, request: Request<pb::RedeemRequest>) -> Result<Response<pb::Redemption>, Status> {
		let user = active_caller(&self.state, &request).await?;
		let req = request.into_inner();
		let service = ServiceId::parse(&req.service).map_err(map_err)?;
		let units = Shares::parse_decimal(&req.units).map_err(map_err)?;
		let redemption = funds_app::request_redemption(
			&self.state.fund_ports(),
			self.state.redemptions.as_ref(),
			self.state.users.as_ref(),
			user,
			service,
			units,
			unix_now(),
		)
		.await
		.map_err(map_err)?;
		Ok(Response::new(redemption_to_proto(&redemption, None)))
	}

	async fn cancel_redemption(&self, request: Request<pb::CancelRedemptionRequest>) -> Result<Response<pb::Redemption>, Status> {
		let user = caller_id(&request)?;
		let id = parse_redemption_id(&request.get_ref().redemption_id)?;
		let redemption = funds_app::cancel_redemption(self.state.redemptions.as_ref(), &self.state.relay_notify, id, user)
			.await
			.map_err(map_err)?;
		Ok(Response::new(redemption_to_proto(&redemption, None)))
	}

	async fn list_redemptions(&self, request: Request<pb::ListRedemptionsRequest>) -> Result<Response<pb::RedemptionList>, Status> {
		let user = caller_id(&request)?;
		let records = funds_app::list_redemptions(self.state.redemptions.as_ref(), user).await.map_err(map_err)?;
		Ok(Response::new(pb::RedemptionList {
			redemptions: records.iter().map(redemption_record_to_proto).collect(),
		}))
	}

	async fn settle_redemption(&self, request: Request<pb::SettleRedemptionRequest>) -> Result<Response<pb::Redemption>, Status> {
		require_permission(&self.state, &request, Permission::AllocationManage).await?;
		let id = parse_redemption_id(&request.get_ref().redemption_id)?;
		let redemption = funds_app::settle_redemption(&self.state.fund_ports(), self.state.redemptions.as_ref(), id, unix_now())
			.await
			.map_err(map_err)?;
		Ok(Response::new(redemption_to_proto(&redemption, None)))
	}

	async fn fail_redemption(&self, request: Request<pb::FailRedemptionRequest>) -> Result<Response<pb::Redemption>, Status> {
		require_permission(&self.state, &request, Permission::AllocationManage).await?;
		let id = parse_redemption_id(&request.get_ref().redemption_id)?;
		let redemption = funds_app::fail_redemption(self.state.redemptions.as_ref(), &self.state.relay_notify, id).await.map_err(map_err)?;
		Ok(Response::new(redemption_to_proto(&redemption, None)))
	}

	async fn resolve_awaiting_redemption(&self, request: Request<pb::ResolveAwaitingRedemptionRequest>) -> Result<Response<pb::Redemption>, Status> {
		require_permission(&self.state, &request, Permission::AllocationManage).await?;
		let req = request.get_ref();
		let id = parse_redemption_id(&req.redemption_id)?;
		// An unknown or unspecified outcome is refused rather than defaulted: this moves money.
		if pb::AwaitingRedemptionOutcome::try_from(req.outcome) != Ok(pb::AwaitingRedemptionOutcome::Fail) {
			return Err(Status::invalid_argument("outcome must be AWAITING_REDEMPTION_OUTCOME_FAIL"));
		}
		let record = funds_app::resolve_awaiting_redemption(
			self.state.redemptions.as_ref(),
			self.state.money.as_ref(),
			&self.state.relay_notify,
			id,
			req.confirm_banking_not_found,
		)
		.await
		.map_err(map_err)?;
		Ok(Response::new(redemption_record_to_proto(&record)))
	}

	async fn list_redemption_queue(&self, request: Request<pb::ListRedemptionQueueRequest>) -> Result<Response<pb::RedemptionQueue>, Status> {
		require_permission(&self.state, &request, Permission::AllocationManage).await?;
		let queued = self.state.redemptions.list_queued().await.map_err(map_err)?;
		Ok(Response::new(pb::RedemptionQueue {
			items: queued
				.into_iter()
				.map(|q| pb::RedemptionQueueItem {
					redemption_id: q.id.to_string(),
					user_id: q.user_id.to_string(),
					service: q.service.to_string(),
					units: q.units.to_decimal_string(),
					created_at: q.created_at,
				})
				.collect(),
		}))
	}

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
		// The token's `sub`, as recorded — the subject the redeem cooldown compares against,
		// so the two spellings agree by construction.
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

fn subscription_to_proto(record: &SubscriptionRecord) -> pb::Subscription {
	let subscription = &record.subscription;
	pb::Subscription {
		id: subscription.id().to_string(),
		service: subscription.service().to_string(),
		cash: subscription.cash().to_decimal_string(),
		nav: subscription.nav().to_decimal_string(),
		units: subscription.units().to_decimal_string(),
		state: subscription.state().as_str().to_owned(),
		failure_reason: record.failure_reason.clone().unwrap_or_default(),
		created_at: record.created_at,
	}
}

fn redemption_record_to_proto(record: &RedemptionRecord) -> pb::Redemption {
	redemption_to_proto(&record.redemption, record.failure_reason.as_deref())
}

fn redemption_to_proto(redemption: &Redemption, failure_reason: Option<&str>) -> pb::Redemption {
	pb::Redemption {
		id: redemption.id().to_string(),
		service: redemption.service().to_string(),
		units: redemption.units().to_decimal_string(),
		nav: redemption.nav().map(|n| n.to_decimal_string()).unwrap_or_default(),
		cash: redemption.cash().map(|c| c.to_decimal_string()).unwrap_or_default(),
		state: redemption.state().as_str().to_owned(),
		failure_reason: failure_reason.unwrap_or_default().to_owned(),
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
