//! The money port's gRPC adapter against a stub `scope.v1.ScopeLedgerService` — banking's
//! own server stubs (`evbanking_contracts`, the pinned rev), served in process. No TLS: the
//! adapter is built over a plain channel (`ScopeLedgerClient::with_channel`); what is under
//! test is the wire shape — the bearer every call carries, the amounts parsed off the
//! answer, and how banking's statuses reach the port. Needs no database and no ledger.

use evbanking_contracts::scope::v1::{
	self as pb,
	scope_ledger_service_server::{ScopeLedgerService, ScopeLedgerServiceServer},
};
use invest_core::{
	infrastructure::money::grpc::ScopeLedgerClient,
	ports::money::{AccountLabel, MoneyKey, MoneyLedger, MoneyLedgerError, Refusal, ReserveRequest, ScopeId, SubjectRef},
};
use invest_domain::{balance::ServiceId, money::Usdt};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status, transport::Server};

const TOKEN: &str = "service-token-for-invest";

/// banking as it stands at the pin: balances and accounts served, the saga RPCs not yet.
struct Banking;

fn authorized<T>(request: &Request<T>) -> Result<(), Status> {
	match request.metadata().get("authorization").and_then(|v| v.to_str().ok()) {
		Some(value) if value == format!("Bearer {TOKEN}") => Ok(()),
		_ => Err(Status::unauthenticated("no service token")),
	}
}

fn not_yet<T>(rpc: &str) -> Result<Response<T>, Status> {
	Err(Status::unimplemented(format!("{rpc} is part 2")))
}

#[tonic::async_trait]
impl ScopeLedgerService for Banking {
	async fn open_account(&self, request: Request<pb::OpenAccountRequest>) -> Result<Response<pb::ScopeAccount>, Status> {
		authorized(&request)?;
		let req = request.into_inner();
		if req.label == "alloc:other" {
			return Err(Status::already_exists("label owned by another client"));
		}
		Ok(Response::new(pb::ScopeAccount {
			scope: req.scope,
			label: req.label,
			balance: "0".into(),
			opened_at: 1_760_000_000,
			owner_client: "invest".into(),
		}))
	}

	async fn reserve(&self, request: Request<pb::ReserveRequest>) -> Result<Response<pb::Hold>, Status> {
		authorized(&request)?;
		not_yet("Reserve")
	}

	async fn capture(&self, _: Request<pb::CaptureRequest>) -> Result<Response<pb::Hold>, Status> {
		not_yet("Capture")
	}

	async fn void(&self, _: Request<pb::VoidRequest>) -> Result<Response<pb::Hold>, Status> {
		not_yet("Void")
	}

	async fn charge(&self, _: Request<pb::ChargeRequest>) -> Result<Response<pb::ScopeCharge>, Status> {
		not_yet("Charge")
	}

	async fn payout(&self, _: Request<pb::PayoutRequest>) -> Result<Response<pb::ScopePayout>, Status> {
		not_yet("Payout")
	}

	async fn transfer(&self, request: Request<pb::TransferRequest>) -> Result<Response<pb::ScopeTransfer>, Status> {
		authorized(&request)?;
		Err(Status::failed_precondition("approval_required"))
	}

	async fn request_payout(&self, _: Request<pb::RequestPayoutRequest>) -> Result<Response<pb::PendingOperation>, Status> {
		not_yet("RequestPayout")
	}

	async fn request_transfer(&self, _: Request<pb::RequestTransferRequest>) -> Result<Response<pb::PendingOperation>, Status> {
		not_yet("RequestTransfer")
	}

	async fn get_pending_operation(&self, _: Request<pb::GetPendingOperationRequest>) -> Result<Response<pb::PendingOperation>, Status> {
		not_yet("GetPendingOperation")
	}

	async fn get_balances(&self, request: Request<pb::GetBalancesRequest>) -> Result<Response<pb::GetBalancesResponse>, Status> {
		authorized(&request)?;
		let req = request.into_inner();
		let mut accounts = Vec::new();
		for label in req.labels {
			let balance = match label.as_str() {
				"alloc:fee" => "1250.5",
				"alloc:broken" => "a lot",
				_ => return Err(Status::not_found("not_found")),
			};
			accounts.push(pb::ScopeAccount {
				scope: req.scope.clone(),
				label,
				balance: balance.into(),
				opened_at: 1_760_000_000,
				owner_client: "invest".into(),
			});
		}
		Ok(Response::new(pb::GetBalancesResponse { accounts }))
	}

	async fn get_hold(&self, _: Request<pb::GetHoldRequest>) -> Result<Response<pb::Hold>, Status> {
		not_yet("GetHold")
	}
}

/// banking on an ephemeral port, and a client to it carrying `token`.
async fn banking(token: &str) -> ScopeLedgerClient {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	tokio::spawn(
		Server::builder()
			.add_service(ScopeLedgerServiceServer::new(Banking))
			.serve_with_incoming(TcpListenerStream::new(listener)),
	);
	let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}")).unwrap().connect_lazy();
	ScopeLedgerClient::with_channel(channel, token).unwrap()
}

fn label(raw: &str) -> AccountLabel {
	AccountLabel::parse(raw).unwrap()
}

#[tokio::test]
async fn balances_carry_the_service_token_and_parse_banking_s_decimals() {
	let client = banking(TOKEN).await;
	let accounts = client.balances(&ScopeId::invest(), &[AccountLabel::allocation(&ServiceId::fee()).unwrap()]).await.unwrap();
	assert_eq!(accounts.len(), 1);
	assert_eq!(accounts[0].label, label("alloc:fee"));
	assert_eq!(accounts[0].balance, Usdt::parse_decimal("1250.5").unwrap());

	// An account invest does not own reads as NOT_FOUND, which the NAV read takes as no cash.
	assert!(matches!(client.balances(&ScopeId::invest(), &[label("alloc:unknown")]).await, Err(MoneyLedgerError::NotFound)));
	// An amount the contract does not allow is banking's bug, not a number to price on.
	assert!(matches!(client.balances(&ScopeId::invest(), &[label("alloc:broken")]).await, Err(MoneyLedgerError::Protocol(_))));
}

#[tokio::test]
async fn a_wrong_token_is_unavailable_not_a_refusal() {
	let client = banking("someone-else").await;
	let err = client.balances(&ScopeId::invest(), &[label("alloc:fee")]).await.unwrap_err();
	assert!(matches!(err, MoneyLedgerError::Unavailable(_)), "{err:?}");
}

#[tokio::test]
async fn an_rpc_banking_does_not_serve_yet_is_unimplemented_and_a_refusal_is_its_reason() {
	let client = banking(TOKEN).await;
	let reserve = ReserveRequest {
		scope: ScopeId::invest(),
		auth_subject: "subject-1".into(),
		amount: Usdt::parse_decimal("10").unwrap(),
		ttl_seconds: None,
		key: MoneyKey::parse("sub-1").unwrap(),
		subject_ref: SubjectRef::new("subscription", uuid::Uuid::new_v4()).unwrap(),
	};
	assert!(matches!(client.reserve(&reserve).await, Err(MoneyLedgerError::Unimplemented { rpc: "Reserve" })));

	let sweep = invest_core::ports::money::FeeSweepRequest {
		scope: ScopeId::invest(),
		from: label("alloc:service_arb"),
		amount: Usdt::parse_decimal("5").unwrap(),
		key: MoneyKey::parse("sweep-1").unwrap(),
		subject_ref: SubjectRef::new("fee_settlement", uuid::Uuid::new_v4()).unwrap(),
	};
	assert!(matches!(client.sweep_fees(&sweep).await, Err(MoneyLedgerError::Refused(Refusal::ApprovalRequired))));

	let opened = client
		.open_account(&ScopeId::invest(), &label("alloc:service_arb"), &MoneyKey::parse("open-1").unwrap())
		.await
		.unwrap();
	assert_eq!((opened.label, opened.balance), (label("alloc:service_arb"), Usdt::ZERO));
	let taken = client.open_account(&ScopeId::invest(), &label("alloc:other"), &MoneyKey::parse("open-2").unwrap()).await;
	assert!(matches!(taken, Err(MoneyLedgerError::IdempotencyConflict(_))));
}
