//! [`Approvals`] over concierge's `ApprovalExecutionService` — invest as the proposer and
//! executor of its `invest:*` kinds.
//!
//! It rides the credential the streams are pulled with (`x-ev-client-id` + the client
//! secret, concierge `docs/SCOPES-AND-APPROVALS.md` §2), which concierge maps to the client
//! `invest`: namespace `invest`, scope `invest`. So invest opens, reads, reports and
//! withdraws only its own proposals, in its own scope.

use std::time::Duration;

use async_trait::async_trait;
use evconcierge_client::proto::{
	Approval, ApprovalFieldValue, ApprovalLevel as WireLevel, ApprovalOutcome as WireOutcome, ApprovalState as WireState, GetProposalRequest, ProposeRequest, PullApprovedRequest,
	ReportOutcomeRequest, WithdrawProposalRequest, approval_execution_service_client::ApprovalExecutionServiceClient, approval_field_value::Value,
};
use tonic::{
	Code, Request, Status,
	metadata::{AsciiMetadataValue, MetadataValue},
	transport::Channel,
};

use super::ConciergeStreamsConfig;
use crate::ports::{
	approvals::{ApprovalField, ApprovalOutcome, ApprovalState, ApprovalValue, ApprovalView, Approvals, ApprovalsError, Proposal, UserProof},
	catalog::KindLevel,
};

/// A call that has not answered by then counts as concierge being unavailable; the round
/// retries.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// The metadata key naming the relying party; its secret rides in `authorization`.
const CLIENT_ID_HEADER: &str = "x-ev-client-id";

/// Where the requester's own token rides on `Propose` (D24(1)), raw, without `Bearer`.
const USER_TOKEN_HEADER: &str = "x-ev-user-token";

/// How concierge starts the message of a FAILED_PRECONDITION about the requester's proof.
const REAUTH_REQUIRED: &str = "reauth_required:";

/// How concierge starts the message of an UNAUTHENTICATED about the requester's token (an
/// unknown or ended session) — unlike one about invest's own credential.
const INVALID_USER_TOKEN: &str = "invalid user token";

pub struct ConciergeApprovals {
	client: ApprovalExecutionServiceClient<Channel>,
	client_id: AsciiMetadataValue,
	bearer: AsciiMetadataValue,
}

impl ConciergeApprovals {
	/// Over the streams' channel and credential. Lazy, like them.
	pub fn connect(config: &ConciergeStreamsConfig) -> color_eyre::Result<Self> {
		let client_id = MetadataValue::try_from(config.client_id.as_str()).map_err(|_| color_eyre::eyre::eyre!("the concierge client id is not a valid header value"))?;
		let mut bearer =
			MetadataValue::try_from(format!("Bearer {}", config.client_secret)).map_err(|_| color_eyre::eyre::eyre!("the concierge client secret is not a valid header value"))?;
		// Keeps the secret out of any Debug print of the request's metadata.
		bearer.set_sensitive(true);
		Ok(Self {
			client: ApprovalExecutionServiceClient::new(super::channel(config)?),
			client_id,
			bearer,
		})
	}

	fn request<T>(&self, message: T) -> Request<T> {
		let mut request = Request::new(message);
		request.metadata_mut().insert(CLIENT_ID_HEADER, self.client_id.clone());
		request.metadata_mut().insert("authorization", self.bearer.clone());
		request.set_timeout(CALL_TIMEOUT);
		request
	}
}

/// Which failures a retry of the same call can cure. A refusal of the request itself is for
/// good; RESOURCE_EXHAUSTED is concierge's cap on live proposals in one scope.
/// UNAUTHENTICATED and UNIMPLEMENTED are configuration (a wrong secret, a concierge without
/// approvals): retried, because the fix is a deploy, not another request. A refused proof of
/// the requester is apart from both: only the person can cure it, by confirming again.
fn classify(rpc: &str, status: &Status) -> ApprovalsError {
	let message = format!("{rpc}: {:?}: {}", status.code(), status.message());
	if status.code() == Code::FailedPrecondition && status.message().starts_with(REAUTH_REQUIRED) {
		return ApprovalsError::ReauthRequired(message);
	}
	if status.code() == Code::Unauthenticated && status.message().starts_with(INVALID_USER_TOKEN) {
		return ApprovalsError::Refused(message);
	}
	match status.code() {
		Code::ResourceExhausted => ApprovalsError::Throttled(message),
		Code::InvalidArgument | Code::NotFound | Code::AlreadyExists | Code::PermissionDenied | Code::FailedPrecondition | Code::OutOfRange => ApprovalsError::Refused(message),
		Code::Ok
		| Code::Cancelled
		| Code::Unknown
		| Code::DeadlineExceeded
		| Code::Aborted
		| Code::Unimplemented
		| Code::Internal
		| Code::Unavailable
		| Code::DataLoss
		| Code::Unauthenticated => ApprovalsError::Unavailable(message),
	}
}

fn field(field: &ApprovalField) -> ApprovalFieldValue {
	let value = match &field.value {
		ApprovalValue::Text(text) => Value::Text(text.clone()),
		ApprovalValue::Bps(bps) => Value::Bps(*bps),
		ApprovalValue::Flag(flag) => Value::Flag(*flag),
		ApprovalValue::Timestamp(at) => Value::Timestamp(*at),
		ApprovalValue::Count(count) => Value::Count(*count),
	};
	ApprovalFieldValue {
		key: field.key.to_owned(),
		value: Some(value),
	}
}

fn view(approval: Approval) -> Result<ApprovalView, ApprovalsError> {
	let state = match WireState::try_from(approval.state) {
		Ok(WireState::Open) => ApprovalState::Open,
		Ok(WireState::Passed) => ApprovalState::Passed,
		Ok(WireState::Executed) => ApprovalState::Executed,
		Ok(WireState::ExecutionFailed) => ApprovalState::ExecutionFailed,
		Ok(WireState::Rejected) => ApprovalState::Rejected,
		Ok(WireState::Vetoed) => ApprovalState::Vetoed,
		Ok(WireState::Expired) => ApprovalState::Expired,
		Ok(WireState::Cancelled) => ApprovalState::Cancelled,
		Ok(WireState::Void) => ApprovalState::Void,
		// A state this build cannot name is not one it may act on; the round retries later.
		Ok(WireState::Unspecified) | Err(_) =>
			return Err(ApprovalsError::Unavailable(format!(
				"approval {} is in a state invest cannot read: {}",
				approval.id, approval.state
			))),
	};
	let note = match state {
		ApprovalState::Vetoed => approval.veto_reason,
		ApprovalState::Void if !approval.void_reason.is_empty() => approval.void_reason,
		ApprovalState::Void | ApprovalState::Executed | ApprovalState::ExecutionFailed => approval.outcome_detail,
		ApprovalState::Open | ApprovalState::Passed | ApprovalState::Rejected | ApprovalState::Expired | ApprovalState::Cancelled => String::new(),
	};
	// An unreadable level is kept as unknown, not guessed: the executor refuses to act on it.
	let level = match WireLevel::try_from(approval.level) {
		Ok(WireLevel::Platform) => Some(KindLevel::Platform),
		Ok(WireLevel::Scope) => Some(KindLevel::Scope),
		Ok(WireLevel::Unspecified) | Err(_) => None,
	};
	Ok(ApprovalView {
		id: approval.id,
		kind: approval.kind,
		scope: approval.scope,
		subject_ref: approval.subject_ref,
		state,
		level,
		// concierge sends 0 until the proposal passes.
		executable_at_unix: (approval.executable_at != 0).then_some(approval.executable_at),
		payload_hash: approval.payload_hash,
		note,
	})
}

#[async_trait]
impl Approvals for ConciergeApprovals {
	async fn propose(&self, proposal: &Proposal, proof: &UserProof) -> Result<ApprovalView, ApprovalsError> {
		let mut request = self.request(ProposeRequest {
			kind: proposal.kind.to_owned(),
			scope: proposal.scope.to_owned(),
			subject_ref: proposal.subject_ref.clone(),
			requested_by: proposal.requested_by.clone(),
			reason: proposal.reason.clone(),
			fields: proposal.fields.iter().map(field).collect(),
			dedupe_key: proposal.dedupe_key.clone(),
		});
		// A token that is no header value cannot be presented; concierge would refuse it the same.
		let mut token = MetadataValue::try_from(proof.token()).map_err(|_| ApprovalsError::ReauthRequired("the requester's token is not a valid header value".into()))?;
		token.set_sensitive(true);
		request.metadata_mut().insert(USER_TOKEN_HEADER, token);
		let approval = self.client.clone().propose(request).await.map_err(|status| classify("Propose", &status))?.into_inner();
		view(approval)
	}

	async fn get(&self, approval_id: &str) -> Result<ApprovalView, ApprovalsError> {
		let request = self.request(GetProposalRequest {
			approval_id: approval_id.to_owned(),
		});
		let approval = self.client.clone().get_proposal(request).await.map_err(|status| classify("GetProposal", &status))?.into_inner();
		view(approval)
	}

	async fn pull_approved(&self, limit: u32) -> Result<Vec<ApprovalView>, ApprovalsError> {
		let request = self.request(PullApprovedRequest { limit });
		let approved = self.client.clone().pull_approved(request).await.map_err(|status| classify("PullApproved", &status))?.into_inner();
		approved.approvals.into_iter().map(view).collect()
	}

	async fn report(&self, approval_id: &str, outcome: ApprovalOutcome, detail: &str) -> Result<(), ApprovalsError> {
		let outcome = match outcome {
			ApprovalOutcome::Executed => WireOutcome::Executed,
			ApprovalOutcome::Failed => WireOutcome::Failed,
			ApprovalOutcome::Void => WireOutcome::Void,
		};
		let request = self.request(ReportOutcomeRequest {
			approval_id: approval_id.to_owned(),
			outcome: outcome as i32,
			detail: detail.to_owned(),
		});
		self.client.clone().report_outcome(request).await.map_err(|status| classify("ReportOutcome", &status))?;
		Ok(())
	}

	async fn withdraw(&self, approval_id: &str, reason: &str) -> Result<ApprovalView, ApprovalsError> {
		let request = self.request(WithdrawProposalRequest {
			approval_id: approval_id.to_owned(),
			reason: reason.to_owned(),
		});
		let approval = self
			.client
			.clone()
			.withdraw_proposal(request)
			.await
			.map_err(|status| classify("WithdrawProposal", &status))?
			.into_inner();
		view(approval)
	}
}

/// No concierge configured: nothing can be proposed. A change the owners decide is refused
/// at once (closed `rejected`, FAILED_PRECONDITION to the operator) rather than left waiting
/// for an approval nobody can open. The approvals round does not run without concierge.
pub struct UnconfiguredApprovals;

const UNCONFIGURED: &str = "no concierge is configured (CONCIERGE_GRPC_ADDR unset), so the owners cannot be asked";

#[async_trait]
impl Approvals for UnconfiguredApprovals {
	async fn propose(&self, _: &Proposal, _: &UserProof) -> Result<ApprovalView, ApprovalsError> {
		Err(ApprovalsError::Refused(UNCONFIGURED.into()))
	}

	async fn get(&self, _: &str) -> Result<ApprovalView, ApprovalsError> {
		Err(ApprovalsError::Unavailable(UNCONFIGURED.into()))
	}

	async fn pull_approved(&self, _: u32) -> Result<Vec<ApprovalView>, ApprovalsError> {
		Err(ApprovalsError::Unavailable(UNCONFIGURED.into()))
	}

	async fn report(&self, _: &str, _: ApprovalOutcome, _: &str) -> Result<(), ApprovalsError> {
		Err(ApprovalsError::Unavailable(UNCONFIGURED.into()))
	}

	async fn withdraw(&self, _: &str, _: &str) -> Result<ApprovalView, ApprovalsError> {
		Err(ApprovalsError::Unavailable(UNCONFIGURED.into()))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The executor acts only on `Some(Platform)`, so a level this build cannot name must
	/// map to `None`, never to a guess.
	#[test]
	fn the_level_is_read_from_the_wire_and_an_unknown_one_stays_unknown() {
		for (wire, level) in [
			(WireLevel::Platform as i32, Some(KindLevel::Platform)),
			(WireLevel::Scope as i32, Some(KindLevel::Scope)),
			(WireLevel::Unspecified as i32, None),
			(99, None),
		] {
			let approval = Approval {
				id: "a".into(),
				state: WireState::Passed as i32,
				level: wire,
				..Approval::default()
			};
			assert_eq!(view(approval).unwrap().level, level, "wire level {wire}");
		}
	}

	/// Only concierge's refusal of the requester's proof asks the person to sign in again;
	/// any other FAILED_PRECONDITION stays a refusal of the request.
	#[test]
	fn a_refused_proof_is_told_apart_from_a_refused_request() {
		let reauth = classify("Propose", &Status::failed_precondition("reauth_required: the sign-in is 300 s old"));
		assert!(matches!(reauth, ApprovalsError::ReauthRequired(ref why) if why.contains("300 s old")), "{reauth:?}");
		let refused = classify("Propose", &Status::failed_precondition("the platform pauses after its owners changed"));
		assert!(matches!(refused, ApprovalsError::Refused(_)), "{refused:?}");
		let elsewhere = classify("Propose", &Status::invalid_argument("reauth_required: not this code"));
		assert!(matches!(elsewhere, ApprovalsError::Refused(_)), "{elsewhere:?}");
	}

	/// The requester's token refused is a refusal of this request; invest's own credential
	/// refused stays a configuration fault a deploy fixes.
	#[test]
	fn an_ended_user_session_is_a_refusal_not_an_outage() {
		let user = classify("Propose", &Status::unauthenticated("invalid user token"));
		assert!(matches!(user, ApprovalsError::Refused(_)), "{user:?}");
		let ours = classify("Propose", &Status::unauthenticated("unknown client"));
		assert!(matches!(ours, ApprovalsError::Unavailable(_)), "{ours:?}");
	}
}
