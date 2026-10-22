//! The holders' notice of a fee-policy change — the outbox it waits in, and the channel it
//! leaves by.
//!
//! [`FeeNoticeOutbox`] is `fee_policy_notices`, written in the same transaction as the change
//! the notices are owed for. [`HolderNotifier`] is concierge's scope inbox: it delivers to a
//! member of invest's scope in the cabinet and by mail, and answers whether it did.

use async_trait::async_trait;
use invest_domain::{
	balance::ServiceId,
	error::DomainError,
	fees::{FeePolicy, FeePolicyChangeId},
	users::UserId,
};

/// One notice the sender may act on now: not delivered, not given up on, not withdrawn, of
/// a change whose terms are still on their way or in force.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingNotice {
	pub change: FeePolicyChangeId,
	pub user: UserId,
}

/// What the notice is about, read under the claim's lock.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoticeSubject {
	pub service: ServiceId,
	/// The fund's display title.
	pub fund_title: String,
	/// The terms in force when the change was scheduled. `None` on a change the previous
	/// binary scheduled, which recorded no "from" — it also recorded no undelivered notice.
	pub from: Option<FeePolicy>,
	pub to: FeePolicy,
	pub effective_from_unix: i64,
	pub scheduled_at_unix: i64,
}

/// A notice held for one attempt. While it is held, the change cannot be cancelled, promoted
/// or acknowledged, so a holder is never told about terms that were withdrawn in the
/// meantime. Dropping it without a verdict leaves the notice as it was.
#[async_trait]
pub trait NoticeClaim: Send {
	fn subject(&self) -> &NoticeSubject;
	/// How many times the sender has acted on the notice before this attempt.
	fn attempts(&self) -> u32;
	async fn delivered(self: Box<Self>, now_unix: i64) -> Result<(), DomainError>;
	async fn retry_at(self: Box<Self>, at_unix: i64, error: &str) -> Result<(), DomainError>;
	async fn give_up(self: Box<Self>, now_unix: i64, reason: &str) -> Result<(), DomainError>;
}

#[async_trait]
pub trait FeeNoticeOutbox: Send + Sync {
	/// Notices whose next attempt is due at `now_unix`, oldest first.
	async fn due(&self, now_unix: i64, limit: u32) -> Result<Vec<PendingNotice>, DomainError>;

	/// Hold one notice for an attempt; `None` when it is no longer pending (delivered, given
	/// up on or withdrawn since [`FeeNoticeOutbox::due`] listed it).
	async fn claim(&self, notice: PendingNotice) -> Result<Option<Box<dyn NoticeClaim>>, DomainError>;
}

/// One notice as it is sent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HolderNotice {
	/// The concierge user id the notice is for.
	pub user: UserId,
	pub topic: &'static str,
	pub kind: &'static str,
	pub title: String,
	pub body: String,
	/// A cabinet path, opened in the cabinet of `link_scope`.
	pub link: String,
	pub link_scope: &'static str,
	/// The same for every attempt of one notice, so a retry is never a second message.
	pub dedupe_key: String,
	pub occurred_at_unix: i64,
}

/// What the channel answered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NoticeOutcome {
	Delivered,
	/// The channel will not deliver it, and says why: the holder is not reachable there, or
	/// switched off a notice that is not required.
	NotDelivered(String),
}

#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
	/// Not reachable or too slow; the same call later may succeed.
	#[error("the notice channel is unavailable: {0}")]
	Unavailable(String),
	/// The channel refused the request itself (a credential, a topic its catalog does not
	/// publish yet). Retried as well: the cure is a deploy or a catalog, not this notice.
	#[error("the notice channel refused the notice: {0}")]
	Refused(String),
}

#[async_trait]
pub trait HolderNotifier: Send + Sync {
	async fn notify(&self, notice: &HolderNotice) -> Result<NoticeOutcome, NotifyError>;
}
