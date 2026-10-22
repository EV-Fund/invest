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

#[async_trait]
impl HolderNotifier for ConciergeNotifier {
	async fn notify(&self, notice: &HolderNotice) -> Result<NoticeOutcome, NotifyError> {
		let request = EmitRequest {
			user_id: notice.user.to_string(),
			topic: notice.topic.to_owned(),
			kind: notice.kind.to_owned(),
			title: notice.title.clone(),
			body: notice.body.clone(),
			// TODO(evconcierge_client 0.4): send `notice.link` with `link_scope`. The published
			// 0.3 has no `link_scope`, and without it concierge joins the path onto the
			// platform's cabinet, where `/invest/<fund>` does not exist. No link beats a
			// broken one; the body names the invest cabinet.
			link: String::new(),
			dedupe_key: notice.dedupe_key.clone(),
			occurred_at: notice.occurred_at_unix,
		};
		match self.client.emit_scope_notification(request).await {
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
