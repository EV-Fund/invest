//! The money port over banking's `scope.v1.ScopeLedgerService` (`evbanking_contracts`, the
//! rev pinned in the workspace manifest).
//!
//! banking proves invest twice, and both are configured here: the client certificate
//! (mTLS — banking maps its fingerprint to the client id `invest`) and a `typ=service`
//! token whose `sub` must be that same client, sent as `authorization: Bearer …`. Until
//! banking's `MintServiceToken` lands the token is provisioned out of band
//! (`SCOPE_LEDGER_SERVICE_TOKEN`).
//!
//! Refusals arrive as (code, message) with the machine reason as the whole message; the
//! adapter turns the status into a [`MoneyLedgerError`] and nothing else — the caller
//! decides what a refusal means for its saga.

use std::time::Duration;

use async_trait::async_trait;
use evbanking_contracts::scope::v1::{self as pb, money_event::Kind, money_events_client::MoneyEventsClient, scope_ledger_service_client::ScopeLedgerServiceClient};
use invest_domain::{architecture::Gateway, money::Usdt};
use tonic::{
	Code, Request, Status,
	metadata::MetadataValue,
	transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity},
};

use crate::ports::money::{
	AccountLabel, CaptureRequest, CaptureTo, FeeSweepRequest, Hold, HoldId, HoldState, MoneyEvent, MoneyEventKind, MoneyEventPage, MoneyEventSource, MoneyKey, MoneyLedger, MoneyLedgerError,
	OperationId, Payout, PayoutLine, PayoutReason, PayoutRequest, PendingOperation, PendingOperationState, Refusal, RequestPayoutRequest, ReserveRequest, ScopeAccount, ScopeId, Transfer,
	VoidRequest,
};

/// Deadline on one call. A saga step that times out is retried with the same key, so a
/// short bound costs nothing but a retry, and a hung banking cannot hold a relay pass.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Where banking's scope ledger listens and how invest proves itself to it. Every PEM is
/// the file's contents, read at boot.
pub struct ScopeLedgerConfig {
	/// `https://host:port` — banking's `SCOPE_LEDGER_BIND` (50054 in banking's layout).
	pub addr: String,
	/// The CA banking's server certificate chains to: a cluster-internal CA no public root
	/// vouches for, so it is pinned rather than trusted from the system store.
	pub ca_pem: String,
	/// invest's client certificate and key — the identity banking maps to `invest`.
	pub client_cert_pem: String,
	pub client_key_pem: String,
	/// The name to verify banking's certificate against, when it differs from `addr`'s host.
	pub tls_domain: Option<String>,
	/// The `typ=service` token for the client `invest`.
	pub service_token: String,
}

pub struct ScopeLedgerClient {
	client: ScopeLedgerServiceClient<Channel>,
	/// banking serves `MoneyEvents` on the scope ledger's listener, behind the same mTLS
	/// identity and token.
	events: MoneyEventsClient<Channel>,
	authorization: MetadataValue<tonic::metadata::Ascii>,
}

impl ScopeLedgerClient {
	/// A client over mTLS. Lazy: nothing is dialled until the first call, so a banking that
	/// is down at boot is a retried call later, not a failed boot.
	pub fn connect(config: &ScopeLedgerConfig) -> color_eyre::Result<Self> {
		let mut tls = ClientTlsConfig::new()
			.ca_certificate(Certificate::from_pem(&config.ca_pem))
			.identity(Identity::from_pem(&config.client_cert_pem, &config.client_key_pem));
		if let Some(domain) = &config.tls_domain {
			tls = tls.domain_name(domain.clone());
		}
		let channel = Endpoint::from_shared(config.addr.clone())?.timeout(CALL_TIMEOUT).tls_config(tls)?.connect_lazy();
		Self::with_channel(channel, &config.service_token)
	}

	/// A client over an already built channel — the seam the tests use to reach a stub
	/// server without TLS.
	pub fn with_channel(channel: Channel, service_token: &str) -> color_eyre::Result<Self> {
		let authorization = MetadataValue::try_from(format!("Bearer {service_token}")).map_err(|_| color_eyre::eyre::eyre!("SCOPE_LEDGER_SERVICE_TOKEN is not a valid header value"))?;
		Ok(Self {
			client: ScopeLedgerServiceClient::new(channel.clone()),
			events: MoneyEventsClient::new(channel),
			authorization,
		})
	}

	fn request<T>(&self, message: T) -> Request<T> {
		let mut request = Request::new(message);
		request.metadata_mut().insert("authorization", self.authorization.clone());
		request
	}
}

impl Gateway for ScopeLedgerClient {}

#[async_trait]
impl MoneyLedger for ScopeLedgerClient {
	async fn open_account(&self, scope: &ScopeId, label: &AccountLabel, key: &MoneyKey) -> Result<ScopeAccount, MoneyLedgerError> {
		let request = self.request(pb::OpenAccountRequest {
			scope: scope.as_str().to_owned(),
			label: label.as_str().to_owned(),
			idempotency_key: key.as_str().to_owned(),
		});
		let account = self.client.clone().open_account(request).await.map_err(|status| from_status("OpenAccount", status))?.into_inner();
		account_from_proto(account)
	}

	async fn balances(&self, scope: &ScopeId, labels: &[AccountLabel]) -> Result<Vec<ScopeAccount>, MoneyLedgerError> {
		let request = self.request(pb::GetBalancesRequest {
			scope: scope.as_str().to_owned(),
			labels: labels.iter().map(|label| label.as_str().to_owned()).collect(),
		});
		let response = self.client.clone().get_balances(request).await.map_err(|status| from_status("GetBalances", status))?.into_inner();
		response.accounts.into_iter().map(account_from_proto).collect()
	}

	async fn reserve(&self, request: &ReserveRequest) -> Result<Hold, MoneyLedgerError> {
		let message = pb::ReserveRequest {
			scope: request.scope.as_str().to_owned(),
			auth_subject: request.auth_subject.clone(),
			amount: request.amount.to_decimal_string(),
			ttl_seconds: request.ttl_seconds.unwrap_or(0),
			idempotency_key: request.key.as_str().to_owned(),
			subject_ref: request.subject_ref.as_str().to_owned(),
		};
		let hold = self
			.client
			.clone()
			.reserve(self.request(message))
			.await
			.map_err(|status| from_status("Reserve", status))?
			.into_inner();
		hold_from_proto(hold)
	}

	async fn capture(&self, request: &CaptureRequest) -> Result<Hold, MoneyLedgerError> {
		let to = match &request.to {
			CaptureTo::Account(label) => pb::account_ref::Account::ScopeLabel(label.as_str().to_owned()),
			CaptureTo::User(auth_subject) => pb::account_ref::Account::AuthSubject(auth_subject.clone()),
		};
		let message = pb::CaptureRequest {
			scope: request.scope.as_str().to_owned(),
			hold_id: request.hold_id.0.clone(),
			to: Some(pb::AccountRef { account: Some(to) }),
			amount: request.amount.map(Usdt::to_decimal_string).unwrap_or_default(),
			idempotency_key: request.key.as_str().to_owned(),
			subject_ref: request.subject_ref.as_str().to_owned(),
		};
		let hold = self
			.client
			.clone()
			.capture(self.request(message))
			.await
			.map_err(|status| from_status("Capture", status))?
			.into_inner();
		hold_from_proto(hold)
	}

	async fn void(&self, request: &VoidRequest) -> Result<Hold, MoneyLedgerError> {
		let message = pb::VoidRequest {
			scope: request.scope.as_str().to_owned(),
			hold_id: request.hold_id.0.clone(),
			idempotency_key: request.key.as_str().to_owned(),
			subject_ref: request.subject_ref.as_str().to_owned(),
		};
		let hold = self.client.clone().void(self.request(message)).await.map_err(|status| from_status("Void", status))?.into_inner();
		hold_from_proto(hold)
	}

	async fn hold(&self, scope: &ScopeId, hold_id: &HoldId) -> Result<Hold, MoneyLedgerError> {
		let message = pb::GetHoldRequest {
			scope: scope.as_str().to_owned(),
			hold_id: hold_id.0.clone(),
		};
		let hold = self
			.client
			.clone()
			.get_hold(self.request(message))
			.await
			.map_err(|status| from_status("GetHold", status))?
			.into_inner();
		hold_from_proto(hold)
	}

	async fn payout(&self, request: &PayoutRequest) -> Result<Payout, MoneyLedgerError> {
		let reason = match request.reason {
			PayoutReason::Redemption => pb::PayoutReason::Redemption,
			PayoutReason::Book => pb::PayoutReason::Book,
			PayoutReason::FeeRebate => pb::PayoutReason::FeeRebate,
		};
		let message = pb::PayoutRequest {
			scope: request.scope.as_str().to_owned(),
			from_label: request.from.as_str().to_owned(),
			lines: request.lines.iter().map(line_to_proto).collect(),
			idempotency_key: request.key.as_str().to_owned(),
			subject_ref: request.subject_ref.as_str().to_owned(),
			memo: String::new(),
			reason: reason as i32,
		};
		let payout = self
			.client
			.clone()
			.payout(self.request(message))
			.await
			.map_err(|status| from_status("Payout", status))?
			.into_inner();
		Ok(Payout {
			id: payout.payout_id,
			total: amount(&payout.total)?,
		})
	}

	async fn sweep_fees(&self, request: &FeeSweepRequest) -> Result<Transfer, MoneyLedgerError> {
		let message = pb::TransferRequest {
			scope: request.scope.as_str().to_owned(),
			from_label: request.from.as_str().to_owned(),
			to_label: FEE_COLLECTOR.to_owned(),
			amount: request.amount.to_decimal_string(),
			idempotency_key: request.key.as_str().to_owned(),
			subject_ref: request.subject_ref.as_str().to_owned(),
			memo: String::new(),
			reason: pb::TransferReason::FeeSweep as i32,
		};
		let transfer = self
			.client
			.clone()
			.transfer(self.request(message))
			.await
			.map_err(|status| from_status("Transfer", status))?
			.into_inner();
		Ok(Transfer {
			id: transfer.transfer_id,
			amount: amount(&transfer.amount)?,
		})
	}

	async fn request_payout(&self, request: &RequestPayoutRequest) -> Result<PendingOperation, MoneyLedgerError> {
		let message = pb::RequestPayoutRequest {
			scope: request.scope.as_str().to_owned(),
			from_label: request.from.as_str().to_owned(),
			lines: request.lines.iter().map(line_to_proto).collect(),
			distribution: false,
			idempotency_key: request.key.as_str().to_owned(),
			subject_ref: request.subject_ref.as_str().to_owned(),
			memo: request.memo.clone(),
		};
		let operation = self
			.client
			.clone()
			.request_payout(self.request(message))
			.await
			.map_err(|status| match status.code() {
				// banking's cap on a client's operations awaiting approval in one scope: not
				// transient on a relay's clock — it frees only as the owners decide — so the
				// saga reads it as a refusal rather than retrying ahead of every later event.
				Code::ResourceExhausted => MoneyLedgerError::Refused(Refusal::Other(APPROVAL_QUEUE_FULL.to_owned())),
				_ => from_status("RequestPayout", status),
			})?
			.into_inner();
		operation_from_proto(operation)
	}

	async fn pending_operation(&self, scope: &ScopeId, operation_id: &OperationId) -> Result<PendingOperation, MoneyLedgerError> {
		let message = pb::GetPendingOperationRequest {
			scope: scope.as_str().to_owned(),
			operation_id: operation_id.0.clone(),
		};
		let operation = self
			.client
			.clone()
			.get_pending_operation(self.request(message))
			.await
			.map_err(|status| from_status("GetPendingOperation", status))?
			.into_inner();
		operation_from_proto(operation)
	}
}

#[async_trait]
impl MoneyEventSource for ScopeLedgerClient {
	async fn pull(&self, after_position: i64, limit: u32) -> Result<MoneyEventPage, MoneyLedgerError> {
		let message = pb::PullRequest {
			// Every scope invest holds a grant in: one cursor over `invest` and `sa` alike.
			scope: String::new(),
			after_position,
			limit,
		};
		let page = self.events.clone().pull(self.request(message)).await.map_err(|status| from_status("Pull", status))?.into_inner();
		Ok(MoneyEventPage {
			events: page.events.into_iter().map(event_from_proto).collect::<Result<_, _>>()?,
			next_position: page.next_position,
		})
	}
}

/// What [`ScopeLedgerClient::request_payout`] calls banking's RESOURCE_EXHAUSTED: too many
/// of invest's operations already wait on an approval in the scope.
pub const APPROVAL_QUEUE_FULL: &str = "approval_queue_full";

fn line_to_proto(line: &PayoutLine) -> pb::PayoutLine {
	pb::PayoutLine {
		auth_subject: line.auth_subject.clone(),
		amount: line.amount.to_decimal_string(),
	}
}

fn operation_from_proto(operation: pb::PendingOperation) -> Result<PendingOperation, MoneyLedgerError> {
	let state = match pb::PendingOperationState::try_from(operation.state) {
		Ok(pb::PendingOperationState::AwaitingApproval) => PendingOperationState::AwaitingApproval,
		Ok(pb::PendingOperationState::Executed) => PendingOperationState::Executed,
		Ok(pb::PendingOperationState::Rejected) => PendingOperationState::Rejected,
		Ok(pb::PendingOperationState::Failed) => PendingOperationState::Failed,
		Ok(pb::PendingOperationState::Unspecified) | Err(_) => return Err(MoneyLedgerError::Protocol(format!("operation {} carries state {}", operation.operation_id, operation.state))),
	};
	if operation.operation_id.is_empty() {
		return Err(MoneyLedgerError::Protocol("an operation without an id".into()));
	}
	Ok(PendingOperation {
		id: OperationId(operation.operation_id),
		state,
		subject_ref: operation.subject_ref,
		failure_reason: operation.failure_reason,
	})
}

/// An event the consumer can key a dedupe on: banking's `event_id` is the unit of delivery
/// and positions start at 1, so an empty id (every such event would collapse into one
/// "duplicate") or a position below 1 is a broken answer, not an event.
fn event_from_proto(event: pb::MoneyEvent) -> Result<MoneyEvent, MoneyLedgerError> {
	if event.event_id.is_empty() || event.position < 1 {
		return Err(MoneyLedgerError::Protocol(format!("Pull: an event with id {:?} at position {}", event.event_id, event.position)));
	}
	let kind = match Kind::try_from(event.kind) {
		Ok(Kind::AccountOpened) => MoneyEventKind::AccountOpened,
		Ok(Kind::CompartmentIn) => MoneyEventKind::CompartmentIn,
		Ok(Kind::CompartmentOut) => MoneyEventKind::CompartmentOut,
		Ok(Kind::HoldPlaced) => MoneyEventKind::HoldPlaced,
		Ok(Kind::HoldCaptured) => MoneyEventKind::HoldCaptured,
		Ok(Kind::HoldVoided) => MoneyEventKind::HoldVoided,
		Ok(Kind::HoldExpired) => MoneyEventKind::HoldExpired,
		Ok(Kind::Charged) => MoneyEventKind::Charged,
		Ok(Kind::PaidOut) => MoneyEventKind::PaidOut,
		Ok(Kind::Transferred) => MoneyEventKind::Transferred,
		Ok(Kind::InvoiceCreated) => MoneyEventKind::InvoiceCreated,
		Ok(Kind::InvoicePaid) => MoneyEventKind::InvoicePaid,
		Ok(Kind::InvoiceCancelled) => MoneyEventKind::InvoiceCancelled,
		Ok(Kind::InvoiceExpired) => MoneyEventKind::InvoiceExpired,
		Ok(Kind::MandateGranted) => MoneyEventKind::MandateGranted,
		Ok(Kind::MandateRevoked) => MoneyEventKind::MandateRevoked,
		Ok(Kind::OperationRequested) => MoneyEventKind::OperationRequested,
		Ok(Kind::OperationRejected) => MoneyEventKind::OperationRejected,
		Ok(Kind::OperationFailed) => MoneyEventKind::OperationFailed,
		// UNSPECIFIED names nothing either: parked like a kind from a newer banking.
		Ok(Kind::Unspecified) | Err(_) => MoneyEventKind::Unknown(event.kind),
	};
	Ok(MoneyEvent {
		event_id: event.event_id,
		position: event.position,
		scope: event.scope,
		kind,
		ref_id: event.ref_id,
		subject_ref: event.subject_ref,
		failure_reason: event.failure_reason,
	})
}

/// The scope's fee collector — banking's own account in every scope, the only credit a
/// `FEE_SWEEP` may name.
const FEE_COLLECTOR: &str = "fee";

/// A status as the port names it. The reason of a refusal is the whole message
/// (`scope/v1/ledger.proto`, REFUSALS); anything a retry with the same key may cure is
/// `Unavailable`, since every mutating call is idempotent by that key.
fn from_status(rpc: &'static str, status: Status) -> MoneyLedgerError {
	match status.code() {
		Code::FailedPrecondition | Code::PermissionDenied => MoneyLedgerError::Refused(Refusal::parse(status.message())),
		Code::NotFound => MoneyLedgerError::NotFound,
		Code::AlreadyExists => MoneyLedgerError::IdempotencyConflict(status.message().to_owned()),
		Code::InvalidArgument => MoneyLedgerError::Invalid(format!("{rpc}: {}", status.message())),
		Code::Unimplemented => MoneyLedgerError::Unimplemented { rpc },
		Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled | Code::ResourceExhausted | Code::Aborted | Code::Internal | Code::Unknown | Code::Unauthenticated =>
			MoneyLedgerError::Unavailable(format!("{rpc}: {:?}: {}", status.code(), status.message())),
		Code::Ok | Code::OutOfRange | Code::DataLoss => MoneyLedgerError::Protocol(format!("{rpc}: {:?}: {}", status.code(), status.message())),
	}
}

fn amount(raw: &str) -> Result<Usdt, MoneyLedgerError> {
	Usdt::parse_decimal(raw).map_err(|err| MoneyLedgerError::Protocol(format!("amount '{raw}': {err}")))
}

fn account_from_proto(account: pb::ScopeAccount) -> Result<ScopeAccount, MoneyLedgerError> {
	Ok(ScopeAccount {
		scope: ScopeId::parse(&account.scope).map_err(|err| MoneyLedgerError::Protocol(err.to_string()))?,
		label: AccountLabel::parse(&account.label).map_err(|err| MoneyLedgerError::Protocol(err.to_string()))?,
		balance: amount(&account.balance)?,
		opened_at: account.opened_at,
	})
}

fn hold_from_proto(hold: pb::Hold) -> Result<Hold, MoneyLedgerError> {
	let state = match pb::HoldState::try_from(hold.state) {
		Ok(pb::HoldState::Active) => HoldState::Active,
		Ok(pb::HoldState::Captured) => HoldState::Captured,
		Ok(pb::HoldState::Voided) => HoldState::Voided,
		Ok(pb::HoldState::Expired) => HoldState::Expired,
		Ok(pb::HoldState::Unspecified) | Err(_) => return Err(MoneyLedgerError::Protocol(format!("hold {} carries state {}", hold.hold_id, hold.state))),
	};
	Ok(Hold {
		id: HoldId(hold.hold_id),
		scope: ScopeId::parse(&hold.scope).map_err(|err| MoneyLedgerError::Protocol(err.to_string()))?,
		auth_subject: hold.auth_subject,
		amount: amount(&hold.amount)?,
		captured: amount(&hold.captured)?,
		remaining: amount(&hold.remaining)?,
		state,
		subject_ref: hold.subject_ref,
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_refusal_is_its_reason_and_unimplemented_is_not_a_refusal() {
		let refused = from_status("Reserve", Status::failed_precondition("mandate_required"));
		assert!(matches!(refused, MoneyLedgerError::Refused(Refusal::MandateRequired)), "{refused:?}");
		let denied = from_status("Transfer", Status::permission_denied("not_granted"));
		assert!(matches!(denied, MoneyLedgerError::Refused(Refusal::NotGranted)), "{denied:?}");
		let not_yet = from_status("Reserve", Status::unimplemented("Reserve is part 2"));
		assert!(matches!(not_yet, MoneyLedgerError::Unimplemented { rpc: "Reserve" }), "{not_yet:?}");
		assert!(matches!(from_status("GetBalances", Status::not_found("x")), MoneyLedgerError::NotFound));
		assert!(matches!(from_status("OpenAccount", Status::already_exists("x")), MoneyLedgerError::IdempotencyConflict(_)));
		assert!(matches!(from_status("Payout", Status::unavailable("down")), MoneyLedgerError::Unavailable(_)));
	}

	#[test]
	fn an_event_kind_this_build_cannot_name_is_unknown_not_ignored() {
		let kind_of = |kind: i32| {
			event_from_proto(pb::MoneyEvent {
				event_id: "e".into(),
				position: 1,
				kind,
				..Default::default()
			})
			.unwrap()
			.kind
		};
		assert_eq!(kind_of(Kind::HoldVoided as i32), MoneyEventKind::HoldVoided);
		assert_eq!(kind_of(Kind::OperationFailed as i32), MoneyEventKind::OperationFailed);
		assert_eq!(kind_of(99), MoneyEventKind::Unknown(99));
		assert_eq!(kind_of(0), MoneyEventKind::Unknown(0));
	}

	#[test]
	fn an_event_without_an_id_or_a_position_is_a_protocol_violation() {
		let event = |event_id: &str, position: i64| pb::MoneyEvent {
			event_id: event_id.into(),
			position,
			kind: Kind::HoldVoided as i32,
			..Default::default()
		};
		assert!(event_from_proto(event("e", 1)).is_ok());
		assert!(matches!(event_from_proto(event("", 1)), Err(MoneyLedgerError::Protocol(_))));
		assert!(matches!(event_from_proto(event("e", 0)), Err(MoneyLedgerError::Protocol(_))));
		assert!(matches!(event_from_proto(event("e", -4)), Err(MoneyLedgerError::Protocol(_))));
	}

	#[test]
	fn an_operation_s_state_must_be_named() {
		let operation = |state: i32| pb::PendingOperation {
			operation_id: "op".into(),
			state,
			..Default::default()
		};
		assert_eq!(
			operation_from_proto(operation(pb::PendingOperationState::Rejected as i32)).unwrap().state,
			PendingOperationState::Rejected
		);
		assert!(matches!(operation_from_proto(operation(0)), Err(MoneyLedgerError::Protocol(_))));
	}

	#[test]
	fn an_unparsable_amount_from_banking_is_a_protocol_violation() {
		assert!(matches!(amount("1.5"), Ok(v) if v == Usdt::parse_decimal("1.5").unwrap()));
		assert!(matches!(amount("-1"), Err(MoneyLedgerError::Protocol(_))));
	}
}
