//! The holders' notices of a fee-policy change: the Postgres outbox ([`PgFeeNotices`]) and
//! the sender that drains it ([`FeeNoticeSender`]).
//!
//! `fee_policy_notices` is the outbox: `FeePolicyChanges` writes one row per holder in the
//! same transaction that schedules the change, and withdraws the undelivered ones in the
//! transaction that cancels it. The sender is the only writer of `delivered_at`,
//! `given_up_at` and the retry columns.
//!
//! **One sender.** It runs under a session advisory lock ([`NOTICE_LOCK_KEY`]), like the money-event
//! consumer: two senders would both send one notice, and the second would read concierge's
//! "already sent" as "not delivered".
//!
//! **A claim holds the change.** [`PgFeeNotices::claim`] opens a transaction that takes the
//! change `FOR SHARE` and the notice `FOR UPDATE`, and keeps them through the call to
//! concierge (bounded by its timeout). A cancel takes the change `FOR UPDATE` before it
//! withdraws the notices, so it waits for the attempt — and an attempt that starts after a
//! cancel finds the notice withdrawn. Lock order is the cancel's: change, then notice.

use std::sync::Arc;

use async_trait::async_trait;
use invest_domain::{
	balance::ServiceId,
	error::DomainError,
	fees::{CrystallizationPeriod, FeePolicy, FeePolicyChangeId, ManagementBasis},
	users::UserId,
};
use sqlx::{PgPool, Postgres, Row, Transaction};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::error;
use uuid::Uuid;

use crate::{
	application::fee_notices::{self, NoticePorts, NoticeRound, ROUND_INTERVAL},
	infrastructure::{now_unix, singleton},
	ports::notices::{FeeNoticeOutbox, HolderNotifier, NoticeClaim, NoticeSubject, PendingNotice},
};

/// Fixed key for the sender's session advisory lock ("EVINNTC_").
const NOTICE_LOCK_KEY: i64 = 0x4556_494e_4e54_435f_u64 as i64;

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

pub struct PgFeeNotices {
	pool: PgPool,
}

impl PgFeeNotices {
	pub fn new(pool: PgPool) -> Self {
		Self { pool }
	}
}

#[async_trait]
impl FeeNoticeOutbox for PgFeeNotices {
	async fn due(&self, now_unix: i64, limit: u32) -> Result<Vec<PendingNotice>, DomainError> {
		// Only a change still on its way or in force: a superseded one's terms no longer
		// apply, and telling a holder about them now would only mislead.
		let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
			"SELECT n.change_id, n.user_id FROM fee_policy_notices n JOIN fee_policy_changes c ON c.id = n.change_id \
			 WHERE n.delivered_at IS NULL AND n.given_up_at IS NULL AND n.withdrawn_at IS NULL \
			 AND (n.next_attempt_at IS NULL OR n.next_attempt_at <= to_timestamp($1)) AND c.state IN ('scheduled', 'active') \
			 ORDER BY n.created_at, n.change_id, n.user_id LIMIT $2",
		)
		.bind(now_unix as f64)
		.bind(i64::from(limit))
		.fetch_all(&self.pool)
		.await
		.map_err(repo_err)?;
		Ok(rows
			.into_iter()
			.map(|(change, user)| PendingNotice {
				change: FeePolicyChangeId::from_raw(change),
				user: UserId::from_raw(user),
			})
			.collect())
	}

	async fn claim(&self, notice: PendingNotice) -> Result<Option<Box<dyn NoticeClaim>>, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let Some(change) = sqlx::query(
			"SELECT c.service, c.management_bps, c.performance_bps, c.hurdle_bps, c.basis, c.crystallization, \
			   c.from_management_bps, c.from_performance_bps, c.from_hurdle_bps, c.from_basis, c.from_crystallization, \
			   EXTRACT(EPOCH FROM c.effective_from)::bigint AS effective_from_unix, EXTRACT(EPOCH FROM c.scheduled_at)::bigint AS scheduled_at_unix, a.title \
			 FROM fee_policy_changes c JOIN allocations a ON a.service = c.service \
			 WHERE c.id = $1 AND c.state IN ('scheduled', 'active') FOR SHARE OF c",
		)
		.bind(notice.change.raw())
		.fetch_optional(&mut *tx)
		.await
		.map_err(repo_err)?
		else {
			return Ok(None);
		};
		let attempts: Option<i32> = sqlx::query_scalar(
			"SELECT attempts FROM fee_policy_notices WHERE change_id = $1 AND user_id = $2 \
			 AND delivered_at IS NULL AND given_up_at IS NULL AND withdrawn_at IS NULL FOR UPDATE",
		)
		.bind(notice.change.raw())
		.bind(notice.user.raw())
		.fetch_optional(&mut *tx)
		.await
		.map_err(repo_err)?;
		let Some(attempts) = attempts else {
			return Ok(None);
		};
		let subject = NoticeSubject {
			service: ServiceId::parse(change.try_get::<String, _>("service").map_err(repo_err)?.as_str())?,
			fund_title: change.try_get("title").map_err(repo_err)?,
			from: from_terms(&change)?,
			to: FeePolicy::from_stored(
				bps(&change, "management_bps")?,
				bps(&change, "performance_bps")?,
				bps(&change, "hurdle_bps")?,
				ManagementBasis::parse(change.try_get::<String, _>("basis").map_err(repo_err)?.as_str())?,
				CrystallizationPeriod::parse(change.try_get::<String, _>("crystallization").map_err(repo_err)?.as_str())?,
			)?,
			effective_from_unix: change.try_get("effective_from_unix").map_err(repo_err)?,
			// A scheduled change always has its moment; 0 lets concierge stamp its own.
			scheduled_at_unix: change.try_get::<Option<i64>, _>("scheduled_at_unix").map_err(repo_err)?.unwrap_or(0),
		};
		Ok(Some(Box::new(PgNoticeClaim {
			tx,
			notice,
			subject,
			attempts: u32::try_from(attempts).map_err(|_| DomainError::Repository("negative notice attempt count".into()))?,
		})))
	}
}

fn bps(row: &sqlx::postgres::PgRow, column: &str) -> Result<u32, DomainError> {
	u32::try_from(row.try_get::<i32, _>(column).map_err(repo_err)?).map_err(|_| DomainError::Repository(format!("negative {column} on a fee policy change")))
}

/// The "from" the change recorded when it was scheduled; `None` when it recorded none (the
/// schema keeps the five columns whole).
fn from_terms(row: &sqlx::postgres::PgRow) -> Result<Option<FeePolicy>, DomainError> {
	let Some(basis) = row.try_get::<Option<String>, _>("from_basis").map_err(repo_err)? else {
		return Ok(None);
	};
	let rate = |column: &str| -> Result<u32, DomainError> {
		let value: Option<i32> = row.try_get(column).map_err(repo_err)?;
		value
			.and_then(|value| u32::try_from(value).ok())
			.ok_or_else(|| DomainError::Repository(format!("fee policy change carries a partial \"from\" ({column})")))
	};
	let crystallization: Option<String> = row.try_get("from_crystallization").map_err(repo_err)?;
	let crystallization = crystallization.ok_or_else(|| DomainError::Repository("fee policy change carries a partial \"from\" (from_crystallization)".into()))?;
	FeePolicy::from_stored(
		rate("from_management_bps")?,
		rate("from_performance_bps")?,
		rate("from_hurdle_bps")?,
		ManagementBasis::parse(&basis)?,
		CrystallizationPeriod::parse(&crystallization)?,
	)
	.map(Some)
}

struct PgNoticeClaim {
	tx: Transaction<'static, Postgres>,
	notice: PendingNotice,
	subject: NoticeSubject,
	attempts: u32,
}

impl PgNoticeClaim {
	async fn finish(mut self, sql: &'static str, at_unix: i64, text: Option<&str>) -> Result<(), DomainError> {
		sqlx::query(sql)
			.bind(self.notice.change.raw())
			.bind(self.notice.user.raw())
			.bind(at_unix as f64)
			.bind(text)
			.execute(&mut *self.tx)
			.await
			.map_err(repo_err)?;
		self.tx.commit().await.map_err(repo_err)
	}
}

#[async_trait]
impl NoticeClaim for PgNoticeClaim {
	fn subject(&self) -> &NoticeSubject {
		&self.subject
	}

	fn attempts(&self) -> u32 {
		self.attempts
	}

	async fn delivered(self: Box<Self>, now_unix: i64) -> Result<(), DomainError> {
		self.finish(
			"UPDATE fee_policy_notices SET delivered_at = to_timestamp($3), attempts = attempts + 1, next_attempt_at = NULL, last_error = $4 \
			 WHERE change_id = $1 AND user_id = $2",
			now_unix,
			None,
		)
		.await
	}

	async fn retry_at(self: Box<Self>, at_unix: i64, error: &str) -> Result<(), DomainError> {
		self.finish(
			"UPDATE fee_policy_notices SET attempts = attempts + 1, next_attempt_at = to_timestamp($3), last_error = $4 WHERE change_id = $1 AND user_id = $2",
			at_unix,
			Some(error),
		)
		.await
	}

	async fn give_up(self: Box<Self>, now_unix: i64, reason: &str) -> Result<(), DomainError> {
		self.finish(
			"UPDATE fee_policy_notices SET given_up_at = to_timestamp($3), attempts = attempts + 1, next_attempt_at = NULL, last_error = $4 \
			 WHERE change_id = $1 AND user_id = $2",
			now_unix,
			Some(reason),
		)
		.await
	}
}

/// [`fee_notices::round`] every [`ROUND_INTERVAL`], as the one sender.
pub struct FeeNoticeSender {
	pool: PgPool,
	outbox: PgFeeNotices,
	notifier: Arc<dyn HolderNotifier>,
}

impl FeeNoticeSender {
	/// `pool` holds the lock connection and the claim's: give it two connections of its own,
	/// apart from request traffic.
	pub fn new(pool: PgPool, notifier: Arc<dyn HolderNotifier>) -> Self {
		Self {
			outbox: PgFeeNotices::new(pool.clone()),
			pool,
			notifier,
		}
	}

	pub async fn run(&self, shutdown: CancellationToken) {
		singleton::run_under_lock("fee notices", NOTICE_LOCK_KEY, &self.pool, shutdown, || self.work()).await;
	}

	async fn work(&self) {
		let mut ticks = tokio::time::interval(ROUND_INTERVAL);
		ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
		loop {
			ticks.tick().await;
			// The database is away: the next round starts again from the same rows.
			if let Err(err) = self.round(now_unix()).await {
				error!("fee notices: the round failed, will retry: {err}");
			}
		}
	}

	/// One round, outside the lock — for tests that drive the sender step by step.
	pub async fn round(&self, now_unix: i64) -> Result<NoticeRound, DomainError> {
		let ports = NoticePorts {
			outbox: &self.outbox,
			notifier: self.notifier.as_ref(),
		};
		fee_notices::round(&ports, now_unix).await
	}
}
