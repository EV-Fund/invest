use std::{sync::Arc, time::Duration};

use invest_contracts::invest::v1 as iv;
use tonic::{
	Request, Status,
	transport::{Channel, Endpoint},
};
use tonic_health::pb::{HealthCheckRequest, health_check_response::ServingStatus, health_client::HealthClient};

use crate::{config::AppConfig, cookies::CookieNames, session::Sessions};

/// Cap on establishing a TCP connection to invest-core: a black-holed or half-open replica
/// must fail fast rather than wedge the awaiting request task.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Per-RPC deadline. tonic has no default request timeout, so without this a wedged
/// invest-core would keep the browser-facing task alive indefinitely. The router's
/// `TimeoutLayer` is a slightly looser outer bound, so a stall surfaces here first.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Shared application state, cheaply cloneable per request.
#[derive(Clone)]
pub struct AppState {
	pub config: Arc<AppConfig>,
	pub grpc: Grpc,
	pub sessions: Arc<Sessions>,
	pub cookies: Arc<CookieNames>,
}

/// gRPC egress to invest-core. The channel is lazily connected and cheap to clone, so a
/// typed client is built per call from it. Every call carries the signed-in user's
/// `invest` access token: invest-core verifies it locally against concierge's JWKS and
/// gates on its own projection of the user.
#[derive(Clone)]
pub struct Grpc {
	core: Channel,
}

impl Grpc {
	pub fn connect_lazy(core_addr: &str) -> color_eyre::Result<Self> {
		Ok(Self {
			core: endpoint(core_addr)?.connect_lazy(),
		})
	}

	fn funds(&self) -> iv::funds_service_client::FundsServiceClient<Channel> {
		iv::funds_service_client::FundsServiceClient::new(self.core.clone())
	}

	fn fees(&self) -> iv::fees_service_client::FeesServiceClient<Channel> {
		iv::fees_service_client::FeesServiceClient::new(self.core.clone())
	}

	fn allocations(&self) -> iv::allocations_service_client::AllocationsServiceClient<Channel> {
		iv::allocations_service_client::AllocationsServiceClient::new(self.core.clone())
	}

	fn book(&self) -> iv::book_service_client::BookServiceClient<Channel> {
		iv::book_service_client::BookServiceClient::new(self.core.clone())
	}

	/// invest-core's standard `grpc.health.v1` — outside its auth layer, so no token.
	pub async fn check(&self) -> Result<bool, Status> {
		let answer = HealthClient::new(self.core.clone()).check(HealthCheckRequest { service: String::new() }).await?.into_inner();
		Ok(answer.status == ServingStatus::Serving as i32)
	}

	pub async fn subscribe(&self, token: &str, req: iv::SubscribeRequest) -> Result<iv::Subscription, Status> {
		Ok(self.funds().subscribe(bearer(token, req)?).await?.into_inner())
	}

	pub async fn redeem(&self, token: &str, req: iv::RedeemRequest) -> Result<iv::Redemption, Status> {
		Ok(self.funds().redeem(bearer(token, req)?).await?.into_inner())
	}

	pub async fn cancel_redemption(&self, token: &str, redemption_id: &str) -> Result<iv::Redemption, Status> {
		let req = iv::CancelRedemptionRequest {
			redemption_id: redemption_id.to_string(),
		};
		Ok(self.funds().cancel_redemption(bearer(token, req)?).await?.into_inner())
	}

	pub async fn list_positions(&self, token: &str) -> Result<iv::PositionList, Status> {
		Ok(self.funds().list_positions(bearer(token, iv::ListPositionsRequest {})?).await?.into_inner())
	}

	pub async fn list_redemptions(&self, token: &str) -> Result<iv::RedemptionList, Status> {
		Ok(self.funds().list_redemptions(bearer(token, iv::ListRedemptionsRequest {})?).await?.into_inner())
	}

	pub async fn list_allocations(&self, token: &str, include_unlisted: bool) -> Result<iv::AllocationList, Status> {
		let req = iv::ListAllocationsRequest { include_unlisted };
		Ok(self.allocations().list_allocations(bearer(token, req)?).await?.into_inner())
	}

	pub async fn get_allocation(&self, token: &str, service: &str) -> Result<iv::Allocation, Status> {
		let req = iv::GetAllocationRequest { service: service.to_string() };
		Ok(self.allocations().get_allocation(bearer(token, req)?).await?.into_inner())
	}

	pub async fn register_allocation(&self, token: &str, req: iv::RegisterAllocationRequest) -> Result<iv::Allocation, Status> {
		Ok(self.allocations().register_allocation(bearer(token, req)?).await?.into_inner())
	}

	pub async fn update_allocation(&self, token: &str, req: iv::UpdateAllocationRequest) -> Result<iv::Allocation, Status> {
		Ok(self.allocations().update_allocation(bearer(token, req)?).await?.into_inner())
	}

	pub async fn set_allocation_state(&self, token: &str, req: iv::SetAllocationStateRequest) -> Result<iv::Allocation, Status> {
		Ok(self.allocations().set_allocation_state(bearer(token, req)?).await?.into_inner())
	}

	pub async fn set_allocation_unit_cap(&self, token: &str, req: iv::SetAllocationUnitCapRequest) -> Result<iv::Allocation, Status> {
		Ok(self.allocations().set_allocation_unit_cap(bearer(token, req)?).await?.into_inner())
	}

	pub async fn set_allocation_access(&self, token: &str, req: iv::SetAllocationAccessRequest) -> Result<iv::Allocation, Status> {
		Ok(self.allocations().set_allocation_access(bearer(token, req)?).await?.into_inner())
	}

	pub async fn grant_allocation_access(&self, token: &str, req: iv::GrantAllocationAccessRequest) -> Result<iv::AllocationAccessGrant, Status> {
		Ok(self.allocations().grant_allocation_access(bearer(token, req)?).await?.into_inner())
	}

	pub async fn revoke_allocation_access(&self, token: &str, req: iv::RevokeAllocationAccessRequest) -> Result<(), Status> {
		self.allocations().revoke_allocation_access(bearer(token, req)?).await?;
		Ok(())
	}

	pub async fn list_allocation_access_grants(&self, token: &str, service: &str) -> Result<iv::AllocationAccessGrantList, Status> {
		let req = iv::ListAllocationAccessGrantsRequest { service: service.to_string() };
		Ok(self.allocations().list_allocation_access_grants(bearer(token, req)?).await?.into_inner())
	}

	pub async fn issue_units(&self, token: &str, req: iv::IssueUnitsRequest) -> Result<iv::UnitIssuance, Status> {
		Ok(self.allocations().issue_units(bearer(token, req)?).await?.into_inner())
	}

	pub async fn retire_units(&self, token: &str, req: iv::RetireUnitsRequest) -> Result<iv::UnitIssuance, Status> {
		Ok(self.allocations().retire_units(bearer(token, req)?).await?.into_inner())
	}

	pub async fn set_allocation_backing(&self, token: &str, service: &str, backing: &str) -> Result<iv::Allocation, Status> {
		let req = iv::SetAllocationBackingRequest {
			service: service.to_string(),
			backing: backing.to_string(),
		};
		Ok(self.allocations().set_allocation_backing(bearer(token, req)?).await?.into_inner())
	}

	pub async fn list_unit_holders(&self, token: &str, service: &str) -> Result<iv::UnitHolders, Status> {
		let req = iv::ListUnitHoldersRequest { service: service.to_string() };
		Ok(self.allocations().list_unit_holders(bearer(token, req)?).await?.into_inner())
	}

	/// Every allocation's line — or the one `service` names (empty = all) — with at most
	/// `holder_limit` holders each (0 = invest-core's default).
	pub async fn allocation_treasury(&self, token: &str, service: &str, holder_limit: u32) -> Result<iv::AllocationTreasuryList, Status> {
		let req = iv::ListAllocationTreasuryRequest {
			service: service.to_string(),
			holder_limit,
		};
		Ok(self.allocations().list_allocation_treasury(bearer(token, req)?).await?.into_inner())
	}

	pub async fn fund_nav(&self, token: &str, service: &str) -> Result<iv::FundNav, Status> {
		let req = iv::GetFundNavRequest { service: service.to_string() };
		Ok(self.funds().get_fund_nav(bearer(token, req)?).await?.into_inner())
	}

	/// The marks of one allocation over a window plus the caller's participation — the
	/// hub derives the second series from the caller's `sub`, so the token is the user's.
	pub async fn fund_nav_history(&self, token: &str, req: iv::ListFundNavHistoryRequest) -> Result<iv::FundNavHistory, Status> {
		Ok(self.funds().list_fund_nav_history(bearer(token, req)?).await?.into_inner())
	}

	/// A fund's fee terms. Readable by any authenticated caller — an investor is entitled
	/// to know what they are paying before they subscribe, not only after.
	pub async fn fee_policy(&self, token: &str, service: &str) -> Result<iv::FeePolicy, Status> {
		let req = iv::GetFeePolicyRequest { service: service.to_string() };
		Ok(self.fees().get_fee_policy(bearer(token, req)?).await?.into_inner())
	}

	/// What the caller's holding owes right now, without charging it — the figure a
	/// position's value has to be shown net of.
	pub async fn accrued_fees(&self, token: &str, service: &str) -> Result<iv::AccruedFees, Status> {
		let req = iv::GetAccruedFeesRequest { service: service.to_string() };
		Ok(self.fees().get_accrued_fees(bearer(token, req)?).await?.into_inner())
	}

	/// Every configured policy, for the operator's fees table.
	pub async fn fee_policies(&self, token: &str) -> Result<iv::FeePolicyList, Status> {
		Ok(self.fees().list_fee_policies(bearer(token, iv::ListFeePoliciesRequest {})?).await?.into_inner())
	}

	/// Propose a change of a fund's terms. Never an in-place write: the hub records a
	/// versioned change that binds after the holders' notice period — and after the owners'
	/// quorum when it tightens the terms beyond the house envelope.
	pub async fn schedule_fee_policy(&self, token: &str, req: iv::ScheduleFeePolicyRequest) -> Result<iv::FeePolicyChange, Status> {
		Ok(self.fees().schedule_fee_policy(bearer(token, req)?).await?.into_inner())
	}

	/// Withdraw a pending change of terms.
	pub async fn cancel_fee_policy_change(&self, token: &str, service: &str, change_id: &str) -> Result<iv::FeePolicyChange, Status> {
		let req = iv::CancelFeePolicyChangeRequest {
			service: service.to_string(),
			change_id: change_id.to_string(),
		};
		Ok(self.fees().cancel_fee_policy_change(bearer(token, req)?).await?.into_inner())
	}

	/// Take responsibility for the holders of a scheduled change who could not be told, so a
	/// tightening binds over them. The hub decides who may: the requester or an owner.
	pub async fn acknowledge_undelivered_notices(&self, token: &str, service: &str, change_id: &str) -> Result<iv::FeePolicyChange, Status> {
		let req = iv::AcknowledgeUndeliveredNoticesRequest {
			service: service.to_string(),
			change_id: change_id.to_string(),
		};
		Ok(self.fees().acknowledge_undelivered_notices(bearer(token, req)?).await?.into_inner())
	}

	/// A fund's whole history of terms, newest version first.
	pub async fn fee_policy_changes(&self, token: &str, service: &str) -> Result<iv::FeePolicyChangeList, Status> {
		let req = iv::ListFeePolicyChangesRequest { service: service.to_string() };
		Ok(self.fees().list_fee_policy_changes(bearer(token, req)?).await?.into_inner())
	}

	/// The manager's uncollected fee units in one fund, and what they are worth now.
	pub async fn fee_shares(&self, token: &str, service: &str) -> Result<iv::FeeShares, Status> {
		let req = iv::GetFeeSharesRequest { service: service.to_string() };
		Ok(self.fees().get_fee_shares(bearer(token, req)?).await?.into_inner())
	}

	/// Convert accumulated fee units into cash. Empty `units` settles the whole balance;
	/// `idempotency_key` names the settlement, so a retry moves no cash twice.
	pub async fn settle_fee_shares(&self, token: &str, service: &str, units: &str, idempotency_key: &str) -> Result<iv::FeeSettlement, Status> {
		let req = iv::SettleFeeSharesRequest {
			service: service.to_string(),
			units: units.to_string(),
			idempotency_key: idempotency_key.to_string(),
		};
		Ok(self.fees().settle_fee_shares(bearer(token, req)?).await?.into_inner())
	}

	/// Every charge this fund has made, across all its holders — the audit trail behind
	/// the units the settlement above is about to convert.
	pub async fn fund_fee_assessments(&self, token: &str, service: &str) -> Result<iv::FeeAssessmentList, Status> {
		let req = iv::ListFundFeeAssessmentsRequest { service: service.to_string() };
		Ok(self.fees().list_fund_fee_assessments(bearer(token, req)?).await?.into_inner())
	}

	pub async fn place_order(&self, token: &str, req: iv::PlaceOrderRequest) -> Result<iv::Order, Status> {
		Ok(self.book().place_order(bearer(token, req)?).await?.into_inner())
	}

	pub async fn cancel_order(&self, token: &str, order_id: &str) -> Result<iv::Order, Status> {
		let req = iv::CancelOrderRequest { order_id: order_id.to_string() };
		Ok(self.book().cancel_order(bearer(token, req)?).await?.into_inner())
	}

	/// `service` empty = every allocation.
	pub async fn list_open_orders(&self, token: &str, service: &str) -> Result<iv::OrderList, Status> {
		let req = iv::ListOpenOrdersRequest { service: service.to_string() };
		Ok(self.book().list_open_orders(bearer(token, req)?).await?.into_inner())
	}

	/// `service` empty = every allocation; `limit` 0 = the hub's default page.
	pub async fn list_order_history(&self, token: &str, service: &str, limit: u32) -> Result<iv::OrderList, Status> {
		let req = iv::ListOrderHistoryRequest {
			service: service.to_string(),
			limit,
		};
		Ok(self.book().list_order_history(bearer(token, req)?).await?.into_inner())
	}

	/// `service` empty = every allocation; `limit` 0 = the hub's default page.
	pub async fn list_user_trades(&self, token: &str, service: &str, limit: u32) -> Result<iv::TradeList, Status> {
		let req = iv::ListUserTradesRequest {
			service: service.to_string(),
			limit,
		};
		Ok(self.book().list_user_trades(bearer(token, req)?).await?.into_inner())
	}

	/// `depth` 0 = the hub's default number of levels a side.
	pub async fn get_book(&self, token: &str, service: &str, depth: u32) -> Result<iv::BookSnapshot, Status> {
		let req = iv::GetBookRequest {
			service: service.to_string(),
			depth,
		};
		Ok(self.book().get_book(bearer(token, req)?).await?.into_inner())
	}

	/// The public tape; `limit` 0 = the hub's default page.
	pub async fn list_trades(&self, token: &str, service: &str, limit: u32) -> Result<iv::TradeList, Status> {
		let req = iv::ListTradesRequest {
			service: service.to_string(),
			limit,
		};
		Ok(self.book().list_trades(bearer(token, req)?).await?.into_inner())
	}

	pub async fn list_candles(&self, token: &str, req: iv::ListCandlesRequest) -> Result<iv::CandleList, Status> {
		Ok(self.book().list_candles(bearer(token, req)?).await?.into_inner())
	}

	/// The live book feed for one allocation: one frame now, then one per change. The
	/// per-RPC timeout bounds only the response headers, not the body, so the stream
	/// outlives [`REQUEST_TIMEOUT`] by design.
	pub async fn watch_book(&self, token: &str, service: &str, depth: u32) -> Result<tonic::Streaming<iv::BookEvent>, Status> {
		let req = iv::WatchBookRequest {
			service: service.to_string(),
			depth,
		};
		Ok(self.book().watch_book(bearer(token, req)?).await?.into_inner())
	}

	pub async fn get_book_policy(&self, token: &str, service: &str) -> Result<iv::BookPolicy, Status> {
		let req = iv::GetBookPolicyRequest { service: service.to_string() };
		Ok(self.book().get_book_policy(bearer(token, req)?).await?.into_inner())
	}

	pub async fn set_book_policy(&self, token: &str, req: iv::SetBookPolicyRequest) -> Result<iv::BookPolicy, Status> {
		Ok(self.book().set_book_policy(bearer(token, req)?).await?.into_inner())
	}

	pub async fn redemption_queue(&self, token: &str) -> Result<iv::RedemptionQueue, Status> {
		Ok(self.funds().list_redemption_queue(bearer(token, iv::ListRedemptionQueueRequest {})?).await?.into_inner())
	}

	pub async fn post_valuation(&self, token: &str, req: iv::PostFundValuationRequest) -> Result<iv::FundNav, Status> {
		Ok(self.funds().post_fund_valuation(bearer(token, req)?).await?.into_inner())
	}

	pub async fn settle_redemption(&self, token: &str, redemption_id: &str) -> Result<iv::Redemption, Status> {
		let req = iv::SettleRedemptionRequest {
			redemption_id: redemption_id.to_string(),
		};
		Ok(self.funds().settle_redemption(bearer(token, req)?).await?.into_inner())
	}

	pub async fn fail_redemption(&self, token: &str, redemption_id: &str) -> Result<iv::Redemption, Status> {
		let req = iv::FailRedemptionRequest {
			redemption_id: redemption_id.to_string(),
		};
		Ok(self.funds().fail_redemption(bearer(token, req)?).await?.into_inner())
	}
}

/// A lazily-connected `Endpoint` with explicit connect and per-RPC deadlines.
fn endpoint(addr: &str) -> color_eyre::Result<Endpoint> {
	Ok(Endpoint::from_shared(addr.to_string())?.connect_timeout(CONNECT_TIMEOUT).timeout(REQUEST_TIMEOUT))
}

/// Attach the user's access token as `authorization: Bearer …` — what invest-core's auth
/// layer reads.
// `tonic::Status` is a large error type; boxing it at every call site buys nothing here.
#[allow(clippy::result_large_err)]
fn bearer<T>(token: &str, msg: T) -> Result<Request<T>, Status> {
	let mut req = Request::new(msg);
	let value = format!("Bearer {token}").parse().map_err(|_| Status::unauthenticated("invalid bearer token"))?;
	req.metadata_mut().insert("authorization", value);
	Ok(req)
}

#[cfg(test)]
mod tests {
	use std::{net::TcpListener, time::Instant};

	use super::*;

	/// A half-open upstream — a port whose TCP connection completes but never speaks HTTP/2
	/// — is the worst case for a missing timeout: the connect succeeds, then the RPC awaits
	/// a response that never comes. A bound listener we never `accept()` is exactly that.
	#[tokio::test]
	async fn upstream_rpc_fails_fast_against_a_half_open_plane() {
		let listener = TcpListener::bind("127.0.0.1:0").expect("bind black-hole listener");
		let addr = listener.local_addr().unwrap();
		let grpc = Grpc::connect_lazy(&format!("http://{addr}")).expect("build the lazy channel");

		let started = Instant::now();
		let guard = tokio::time::timeout(REQUEST_TIMEOUT + Duration::from_secs(5), grpc.check()).await;

		let result = guard.expect("the call must return within the deadline, not hang");
		assert!(result.is_err(), "a half-open plane must surface an error, not a hung await");
		assert!(started.elapsed() < REQUEST_TIMEOUT + Duration::from_secs(2), "the per-RPC timeout must bound the call");
		drop(listener);
	}
}
