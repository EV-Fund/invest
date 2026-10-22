//! [`HolderNotifier`] over concierge's `ScopeInboxService.EmitScopeNotification`, with the
//! credential the streams are pulled with. concierge files the notice in invest's inbox and
//! mails it, and answers per user whether it did (concierge `docs/SCOPES-AND-APPROVALS.md`
//! §6): `delivered` is false for anyone who is not an active member of invest's scope, and
//! for a member who switched the topic, or both channels, off. A scope cannot publish a
//! topic its members cannot switch off; that stays concierge's own.

use async_trait::async_trait;
use evconcierge_client::{ConciergeClient, ConciergeError, proto::EmitRequest};

use super::ConciergeStreamsConfig;
use crate::ports::notices::{HolderNotice, HolderNotifier, NoticeOutcome, NotifyError};

pub struct ConciergeNotifier {
	client: ConciergeClient,
}

impl ConciergeNotifier {
	/// Over the streams' channel and credential. Lazy, like them.
	pub fn connect(config: &ConciergeStreamsConfig) -> color_eyre::Result<Self> {
		let client = ConciergeClient::from_channel(super::channel(config)?, config.client_id.clone(), config.client_secret.clone())
			.map_err(|err| color_eyre::eyre::eyre!("concierge notice client: {err}"))?;
		Ok(Self { client })
	}
}

/// What concierge's `delivered = false` means, in the words the notice row keeps.
const NOT_DELIVERED: &str = "concierge did not deliver it: the holder is not an active member of invest's scope, or has switched fee-term notices (or both channels) off";

/// The notice as concierge's wire carries it. The link opens in the cabinet `link_scope`
/// names; with concierge's `NOTICE_LINK_BASES` set for it, the stored link is absolute.
fn emit_request(notice: &HolderNotice) -> EmitRequest {
	EmitRequest {
		user_id: notice.user.to_string(),
		topic: notice.topic.to_owned(),
		kind: notice.kind.to_owned(),
		title: notice.title.clone(),
		body: notice.body.clone(),
		link: notice.link.clone(),
		link_scope: notice.link_scope.to_owned(),
		dedupe_key: notice.dedupe_key.clone(),
		occurred_at: notice.occurred_at_unix,
	}
}

#[async_trait]
impl HolderNotifier for ConciergeNotifier {
	async fn notify(&self, notice: &HolderNotice) -> Result<NoticeOutcome, NotifyError> {
		match self.client.emit_scope_notification(emit_request(notice)).await {
			Ok(response) if response.delivered => Ok(NoticeOutcome::Delivered),
			Ok(_) => Ok(NoticeOutcome::NotDelivered(NOT_DELIVERED.to_owned())),
			Err(err @ ConciergeError::Unavailable(_)) => Err(NotifyError::Unavailable(err.to_string())),
			Err(
				err @ (ConciergeError::Refused { .. }
				| ConciergeError::Rejected { .. }
				| ConciergeError::InvalidResponse(_)
				| ConciergeError::Config(_)
				| ConciergeError::Randomness { .. }
				| ConciergeError::Failed { .. }),
			) => Err(NotifyError::Refused(err.to_string())),
		}
	}
}

/// The notifier of an invest-core with no concierge configured: nothing can be delivered, so
/// every notice is given up on at once — what invest did before it had a channel — and an
/// administrator's change waits for its requester's acknowledgement.
pub struct UnconfiguredNotifier;

#[async_trait]
impl HolderNotifier for UnconfiguredNotifier {
	async fn notify(&self, _notice: &HolderNotice) -> Result<NoticeOutcome, NotifyError> {
		Ok(NoticeOutcome::NotDelivered("no notice channel is configured (CONCIERGE_GRPC_ADDR unset)".to_owned()))
	}
}

#[cfg(test)]
mod tests {
	use invest_domain::users::UserId;

	use super::*;

	#[test]
	fn the_notice_links_into_the_invest_cabinet() {
		let user = UserId::new();
		let notice = HolderNotice {
			user,
			topic: "invest:fee-terms",
			kind: "fee_terms_change",
			title: "New fee terms for EV Trading".to_owned(),
			body: "The fee terms of EV Trading change.".to_owned(),
			link: "/invest/ev-trading".to_owned(),
			link_scope: "invest",
			dedupe_key: "fee_policy_notice:x".to_owned(),
			occurred_at_unix: 1_792_677_600,
		};

		let wire = emit_request(&notice);

		assert_eq!((wire.link.as_str(), wire.link_scope.as_str()), ("/invest/ev-trading", "invest"));
		assert_eq!(wire.user_id, user.to_string());
		assert_eq!(
			(wire.topic.as_str(), wire.kind.as_str(), wire.dedupe_key.as_str()),
			("invest:fee-terms", "fee_terms_change", "fee_policy_notice:x")
		);
		assert_eq!((wire.title, wire.body, wire.occurred_at), (notice.title, notice.body, notice.occurred_at_unix));
	}
}
