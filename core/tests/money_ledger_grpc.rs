//! The money port's gRPC adapter against a stub `scope.v1.ScopeLedgerService` — banking's
//! own server stubs (`evbanking_contracts`, the pinned rev), served in process. No TLS: the
//! adapter is built over a plain channel (`ScopeLedgerClient::with_parts`); what is under
//! test is the wire shape — the bearer every call carries, the amounts parsed off the
//! answer, and how banking's statuses reach the port. Needs no database and no ledger.

use std::sync::{
	Arc,
	atomic::{AtomicU32, Ordering},
};

use evbanking_contracts::scope::v1::{
	self as pb,
	scope_auth_service_server::{ScopeAuthService, ScopeAuthServiceServer},
	scope_ledger_service_server::{ScopeLedgerService, ScopeLedgerServiceServer},
};
use invest_core::{
	infrastructure::money::{
		grpc::ScopeLedgerClient,
		token::{HostClock, MintedToken, TokenMinter, TokenSource},
	},
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

	// Two pages of `book_order:` holds, as banking pages them: by hold id, the second page's
	// token being the first's reply. The prefix is checked, not applied: a request that
	// loses it fails here.
	async fn list_holds(&self, request: Request<pb::ListHoldsRequest>) -> Result<Response<pb::ListHoldsResponse>, Status> {
		authorized(&request)?;
		let req = request.into_inner();
		if req.subject_ref_prefix != "book_order:" || !req.scope.is_empty() {
			return Err(Status::invalid_argument("unexpected filter"));
		}
		let hold = |id: &str| pb::Hold {
			hold_id: id.into(),
			scope: "invest".into(),
			auth_subject: "subject-1".into(),
			amount: "10".into(),
			captured: "0".into(),
			remaining: "10".into(),
			state: pb::HoldState::Active as i32,
			expires_at: 0,
			created_at: 1_760_000_000,
			subject_ref: format!("book_order:{id}"),
			mandate_id: String::new(),
			placed_by_client: "invest".into(),
		};
		Ok(Response::new(match req.page_token.as_str() {
			"" => pb::ListHoldsResponse {
				holds: vec![hold("h-1")],
				next_page_token: "after-h-1".into(),
			},
			"after-h-1" => pb::ListHoldsResponse {
				holds: vec![hold("h-2")],
				next_page_token: String::new(),
			},
			_ => return Err(Status::invalid_argument("unknown token")),
		}))
	}
}

/// Hands out one fixed token, valid for ten minutes by the host clock.
struct FixedMinter(String);

#[tonic::async_trait]
impl TokenMinter for FixedMinter {
	async fn mint(&self) -> Result<MintedToken, Status> {
		Ok(MintedToken {
			token: self.0.clone(),
			expires_at: invest_core::infrastructure::now_unix() + 600,
		})
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
	ScopeLedgerClient::with_parts(channel, TokenSource::new(Arc::new(FixedMinter(token.into())), Arc::new(HostClock)))
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

#[tokio::test]
async fn list_holds_walks_every_page_and_carries_the_placement_time() {
	let client = banking(TOKEN).await;
	let holds = client.list_holds("book_order:").await.unwrap();
	let ids: Vec<&str> = holds.iter().map(|hold| hold.id.0.as_str()).collect();
	assert_eq!(ids, ["h-1", "h-2"]);
	assert!(holds.iter().all(|hold| hold.created_at == 1_760_000_000 && hold.scope == ScopeId::invest()));
	assert_eq!(holds[1].subject_ref, "book_order:h-2");
}

/// banking's `ScopeAuthService`: counts the mints, hands out `TOKEN` valid for `ttl` seconds,
/// or refuses while `failing`.
struct Minting {
	mints: Arc<AtomicU32>,
	ttl: i64,
	failing: bool,
}

#[tonic::async_trait]
impl ScopeAuthService for Minting {
	async fn mint_service_token(&self, _: Request<pb::MintServiceTokenRequest>) -> Result<Response<pb::MintServiceTokenResponse>, Status> {
		self.mints.fetch_add(1, Ordering::SeqCst);
		if self.failing {
			return Err(Status::unauthenticated("no client for this certificate"));
		}
		Ok(Response::new(pb::MintServiceTokenResponse {
			token: TOKEN.into(),
			expires_at: invest_core::infrastructure::now_unix() + self.ttl,
		}))
	}
}

/// banking with the mint RPC on the same listener, and a client that gets its token from it
/// over the wire. Returns the mint counter.
async fn banking_that_mints(ttl: i64, failing: bool) -> (ScopeLedgerClient, Arc<AtomicU32>) {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let mints = Arc::new(AtomicU32::new(0));
	tokio::spawn(
		Server::builder()
			.add_service(ScopeAuthServiceServer::new(Minting { mints: mints.clone(), ttl, failing }))
			.add_service(ScopeLedgerServiceServer::new(Banking))
			.serve_with_incoming(TcpListenerStream::new(listener)),
	);
	let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}")).unwrap().connect_lazy();
	(ScopeLedgerClient::with_parts(channel.clone(), TokenSource::over_channel(channel)), mints)
}

#[tokio::test]
async fn the_client_mints_its_token_over_the_wire_once_and_reuses_it() {
	let (client, mints) = banking_that_mints(300, false).await;
	for _ in 0..3 {
		client.balances(&ScopeId::invest(), &[label("alloc:fee")]).await.unwrap();
	}
	assert_eq!(mints.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_token_about_to_lapse_is_replaced_before_the_next_call() {
	// 30 s is inside the refresh margin: every call finds the cache stale and mints again,
	// and every call still passes banking's token check.
	let (client, mints) = banking_that_mints(30, false).await;
	for _ in 0..3 {
		client.balances(&ScopeId::invest(), &[label("alloc:fee")]).await.unwrap();
	}
	assert_eq!(mints.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_refused_mint_makes_the_call_unavailable_and_nothing_is_sent_or_retried() {
	let (client, mints) = banking_that_mints(300, true).await;
	let err = client.balances(&ScopeId::invest(), &[label("alloc:fee")]).await.unwrap_err();
	match &err {
		MoneyLedgerError::Unavailable(message) => assert!(message.contains("unavailable") && !message.contains(TOKEN), "{message}"),
		other => panic!("{other:?}"),
	}
	// UNAUTHENTICATED is final: one mint, no backoff loop.
	assert_eq!(mints.load(Ordering::SeqCst), 1);

	// A money call that cannot get a token is not sent either.
	let sweep = invest_core::ports::money::FeeSweepRequest {
		scope: ScopeId::invest(),
		from: label("alloc:service_arb"),
		amount: Usdt::parse_decimal("5").unwrap(),
		key: MoneyKey::parse("sweep-1").unwrap(),
		subject_ref: SubjectRef::new("fee_settlement", uuid::Uuid::new_v4()).unwrap(),
	};
	assert!(matches!(client.sweep_fees(&sweep).await, Err(MoneyLedgerError::Unavailable(_))));
}
