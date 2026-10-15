//! Postgres adapter for the [`FeePolicyChanges`] port — the history of a product's fee terms
//! and the ONLY writer of `fee_policies`.
//!
//! Three transactions here are load-bearing:
//!
//! - [`FeePolicyChanges::schedule`] writes the change already `scheduled` and one notice
//!   per holder in the same transaction, so a change never exists without the record of who
//!   was owed word of it. invest has no channel to deliver a notice yet (concierge's scope
//!   notifications are a follow-up), so each is recorded as GIVEN UP the moment it is owed:
//!   a dearer change then binds over its holders only once an operator acknowledges them,
//!   and a cheaper one binds by itself. A change that needs the owners is refused — their
//!   approval is concierge's, and invest is not its client yet.
//! - [`FeePolicyChanges::cancel`] closes the change and, in the same transaction, withdraws
//!   every holder notice not delivered yet, so no notifier can later tell holders about
//!   terms that will never bind.
//! - [`FeePolicyChanges::promote`] settles every holder's management accrual at the OLD
//!   rate as of `effective_from` (`carry_accrual` reads `fee_policies` on this same
//!   connection, which is why it runs strictly BEFORE the upsert), then writes the new
//!   terms. `docs/FEES.md` § "The elapsed clock": nobody re-prices time that has passed.
//!   It refuses a dearer change while a holder's notice is undelivered: the notice period is
//!   only notice if the notices arrived — unless an operator has taken responsibility for
//!   exactly those holders ([`FeePolicyChanges::acknowledge_undelivered_notices`], which a
//!   later call extends by the holders given up on since).
//!
//! Runtime queries throughout (`sqlx::query*`), so `cargo build` needs no database; the
//! integration suite in `tests/fee_policy_changes.rs` executes every one of them.

use async_trait::async_trait;
use invest_domain::{
	balance::ServiceId,
	error::DomainError,
	fees::{self, ChangeRequirement, FeePolicy, FeePolicyChangeId, FeePolicyChangeState},
	users::UserId,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow};
use uuid::Uuid;

use crate::{
	infrastructure::{
		fee_accrual::carry_accrual,
		fees::{policy_from_row, repo_err},
	},
	ports::fees::{FeePolicyChange, FeePolicyChanges, NewFeePolicyChange, NoticeWaiver},
};

/// Postgres' unique-violation SQLSTATE — how `fee_policy_changes_single_pending_idx` answers.
const UNIQUE_VIOLATION: &str = "23505";

macro_rules! change_columns {
	() => {
		"id, service, version, management_bps, performance_bps, hurdle_bps, basis, crystallization, state, requirement, \
		 EXTRACT(EPOCH FROM effective_from)::bigint AS effective_from_unix, requested_by, reason, \
		 EXTRACT(EPOCH FROM requested_at)::bigint AS requested_at_unix, EXTRACT(EPOCH FROM scheduled_at)::bigint AS scheduled_at_unix, \
		 EXTRACT(EPOCH FROM applied_at)::bigint AS applied_at_unix, \
		 notices_waived_by, EXTRACT(EPOCH FROM notices_waived_at)::bigint AS notices_waived_at_unix, notices_waived_users"
	};
}

pub struct PgFeePolicyChanges {
	pool: PgPool,
}

impl PgFeePolicyChanges {
	pub fn new(pool: PgPool) -> Self {
		Self { pool }
	}
}

/// Take the ONE lock every transaction over a product's terms opens with: its row in
/// `allocations`, `FOR UPDATE`. Scheduling, carrying and promoting a change all read the
/// live terms and write something derived from them (the requirement, the notice's "from",
/// the accrual carried at the old rate); without a common lock, a schedule computing its
/// requirement while a promotion commits reads the terms of a minute ago and records a
/// requirement the new terms would not have allowed. `NotFound` for a slug the registry does
/// not know — terms for a product that does not exist are always a typo.
async fn lock_product(conn: &mut PgConnection, service: &ServiceId) -> Result<(), DomainError> {
	sqlx::query_scalar::<_, String>("SELECT service FROM allocations WHERE service = $1 FOR UPDATE")
		.bind(service.as_str())
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?
		.map(|_| ())
		.ok_or_else(|| DomainError::NotFound {
			entity: "allocation",
			id: service.to_string(),
		})
}

/// How many investors hold units of the product right now — who the notice period and the
/// notices are for.
pub(crate) async fn holder_count(conn: &mut PgConnection, service: &ServiceId) -> Result<u32, DomainError> {
	let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fund_positions WHERE service = $1 AND units <> '0'")
		.bind(service.as_str())
		.fetch_one(&mut *conn)
		.await
		.map_err(repo_err)?;
	Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

/// One notice of a change that has not reached its holder, and whether it has been given
/// up on.
struct UndeliveredNotice {
	user_id: UserId,
	given_up: bool,
}

/// Counted by DELIVERY: a notice is owed until it is stamped delivered. Counted over the
/// holders of RIGHT NOW: a recipient who has since redeemed every unit has
/// no terms to be warned about, and must not hold the change for those who stayed. A notice
/// WITHDRAWN with its change is owed to nobody and is not counted either. The ONE rule
/// behind the figures on the wire, the tightening gate and the acknowledgement's list
/// (which keeps only the given-up half, and only the holders no acknowledgement covers yet).
async fn undelivered_notices(conn: &mut PgConnection, id: FeePolicyChangeId, service: &ServiceId) -> Result<Vec<UndeliveredNotice>, DomainError> {
	let rows: Vec<(Uuid, bool)> = sqlx::query_as(
		"SELECT n.user_id, n.given_up_at IS NOT NULL FROM fee_policy_notices n \
		 JOIN fund_positions p ON p.user_id = n.user_id AND p.service = $2 AND p.units <> '0' \
		 WHERE n.change_id = $1 AND n.delivered_at IS NULL AND n.withdrawn_at IS NULL ORDER BY n.user_id",
	)
	.bind(id.raw())
	.bind(service.as_str())
	.fetch_all(&mut *conn)
	.await
	.map_err(repo_err)?;
	Ok(rows
		.into_iter()
		.map(|(user_id, given_up)| UndeliveredNotice {
			user_id: UserId::from_raw(user_id),
			given_up,
		})
		.collect())
}

fn count(n: usize) -> u32 {
	u32::try_from(n).unwrap_or(u32::MAX)
}

fn change_from_row(row: &PgRow) -> Result<FeePolicyChange, DomainError> {
	let (service, policy) = policy_from_row(row)?;
	Ok(FeePolicyChange {
		id: FeePolicyChangeId::from_raw(row.try_get("id").map_err(repo_err)?),
		service,
		version: u32::try_from(row.try_get::<i32, _>("version").map_err(repo_err)?).map_err(|_| DomainError::Repository("negative change version".into()))?,
		policy,
		state: FeePolicyChangeState::parse(row.try_get::<String, _>("state").map_err(repo_err)?.as_str())?,
		requirement: ChangeRequirement::parse(row.try_get::<String, _>("requirement").map_err(repo_err)?.as_str())?,
		effective_from_unix: row.try_get("effective_from_unix").map_err(repo_err)?,
		requested_by: row.try_get("requested_by").map_err(repo_err)?,
		requested_at_unix: row.try_get("requested_at_unix").map_err(repo_err)?,
		reason: row.try_get("reason").map_err(repo_err)?,
		scheduled_at_unix: row.try_get("scheduled_at_unix").map_err(repo_err)?,
		applied_at_unix: row.try_get("applied_at_unix").map_err(repo_err)?,
		// Filled in by `hydrate`, which has the connection this row was read on.
		undelivered_notices: 0,
		notices_given_up: 0,
		notices_unacknowledged: 0,
		notices_waiver: waiver_from_row(row)?,
	})
}

/// The three waiver columns are whole or absent — the schema says so — so a name without
/// the rest is a row this binary does not understand, not a half-acknowledgement.
fn waiver_from_row(row: &PgRow) -> Result<Option<NoticeWaiver>, DomainError> {
	let by: Option<String> = row.try_get("notices_waived_by").map_err(repo_err)?;
	let at_unix: Option<i64> = row.try_get("notices_waived_at_unix").map_err(repo_err)?;
	let users: Option<Vec<Uuid>> = row.try_get("notices_waived_users").map_err(repo_err)?;
	match (by, at_unix, users) {
		(None, None, None) => Ok(None),
		(Some(by), Some(at_unix), Some(users)) => Ok(Some(NoticeWaiver {
			by,
			at_unix,
			users: users.into_iter().map(UserId::from_raw).collect(),
		})),
		_ => Err(DomainError::Repository("fee policy change carries a partial notice waiver".into())),
	}
}

/// A row as the wire shows it: with the undelivered-notice figures, which only a scheduled
/// change has — every other state has nothing left to wait for, and the count is spared.
async fn hydrate(conn: &mut PgConnection, row: &PgRow) -> Result<FeePolicyChange, DomainError> {
	let mut change = change_from_row(row)?;
	if change.state == FeePolicyChangeState::Scheduled {
		let undelivered = undelivered_notices(conn, change.id, &change.service).await?;
		let given_up = undelivered.iter().filter(|notice| notice.given_up);
		change.undelivered_notices = count(undelivered.len());
		change.notices_given_up = count(given_up.clone().count());
		let covered: &[UserId] = change.notices_waiver.as_ref().map_or(&[], |waiver| waiver.users.as_slice());
		change.notices_unacknowledged = count(given_up.filter(|notice| !covered.contains(&notice.user_id)).count());
	}
	Ok(change)
}

async fn find_on(conn: &mut PgConnection, id: FeePolicyChangeId) -> Result<Option<FeePolicyChange>, DomainError> {
	let row = sqlx::query(concat!("SELECT ", change_columns!(), " FROM fee_policy_changes WHERE id = $1"))
		.bind(id.raw())
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?;
	match row {
		Some(row) => hydrate(conn, &row).await.map(Some),
		None => Ok(None),
	}
}

/// Load a change `FOR UPDATE` — the opening move of every transition on it.
async fn locked(conn: &mut PgConnection, id: FeePolicyChangeId) -> Result<FeePolicyChange, DomainError> {
	let row = sqlx::query(concat!("SELECT ", change_columns!(), " FROM fee_policy_changes WHERE id = $1 FOR UPDATE"))
		.bind(id.raw())
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?
		.ok_or_else(|| DomainError::NotFound {
			entity: "fee policy change",
			id: id.to_string(),
		})?;
	hydrate(conn, &row).await
}

/// The terms in force for a product right now, or `None` for one that charges nothing —
/// read on the caller's connection so a notice written in a transaction states the row that
/// transaction sees.
async fn current_terms(conn: &mut PgConnection, service: &ServiceId) -> Result<Option<FeePolicy>, DomainError> {
	let row = sqlx::query("SELECT service, management_bps, performance_bps, hurdle_bps, basis, crystallization FROM fee_policies WHERE service = $1")
		.bind(service.as_str())
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?;
	row.map(|row| policy_from_row(&row).map(|(_, policy)| policy)).transpose()
}

/// The holders a notice is owed to: every position with units, the roster the notice period
/// is decided on ([`holder_count`]).
async fn notice_roster(conn: &mut PgConnection, service: &ServiceId) -> Result<Vec<Uuid>, DomainError> {
	sqlx::query_scalar("SELECT user_id FROM fund_positions WHERE service = $1 AND units <> '0' ORDER BY user_id")
		.bind(service.as_str())
		.fetch_all(&mut *conn)
		.await
		.map_err(repo_err)
}

/// Record one notice per holder, on the caller's transaction; a retried scheduling records
/// each once. invest has no channel to deliver them yet, so each is given up on as it is
/// recorded — said on the row, not assumed: the promotion gate and the acknowledgement read
/// exactly what a notifier will one day write.
// TODO(concierge scope notifications): record the notice undelivered and have the notifier
// emit it through concierge's EmitScopeNotification, stamping `delivered_at`/`given_up_at`.
async fn record_notices(conn: &mut PgConnection, change: FeePolicyChangeId, holders: &[Uuid]) -> Result<(), DomainError> {
	if holders.is_empty() {
		return Ok(());
	}
	sqlx::query("INSERT INTO fee_policy_notices (change_id, user_id, given_up_at) SELECT $1, holder, now() FROM UNNEST($2::uuid[]) AS holder ON CONFLICT DO NOTHING")
		.bind(change.raw())
		.bind(holders)
		.execute(&mut *conn)
		.await
		.map_err(repo_err)?;
	Ok(())
}

fn bps(value: u32) -> i32 {
	// The constructor caps every rate far below `i32::MAX`; the fallback only guards the cast.
	i32::try_from(value).unwrap_or(i32::MAX)
}

#[async_trait]
impl FeePolicyChanges for PgFeePolicyChanges {
	async fn schedule(&self, change: &NewFeePolicyChange) -> Result<FeePolicyChange, DomainError> {
		// TODO(concierge approvals): a change that needs the owners is proposed as an
		// `invest:fee_policy` approval in concierge and scheduled when it is carried —
		// banking's `schedule_approved`. Until invest is concierge's approvals client, it is
		// refused here rather than scheduled on one administrator's word.
		if change.requirement == ChangeRequirement::OwnerConsilium {
			return Err(DomainError::Precondition(
				"this change tightens the terms beyond the house envelope and needs the owners' approval, which invest cannot ask for yet".into(),
			));
		}
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		lock_product(&mut tx, &change.service).await?;
		let from = current_terms(&mut tx, &change.service).await?;
		// The application decided the requirement against the terms it read BEFORE this lock.
		// A promotion may have committed in between (an INSERT waiting on the pending index
		// is exactly that window), so the decision is re-taken under the lock and any
		// disagreement is a refusal: the caller re-reads and re-submits, rather than this path
		// recording a requirement the live terms would not have allowed.
		if fees::requirement_for(from.as_ref(), &change.policy) != change.requirement {
			return Err(DomainError::Conflict("the live terms changed while the request was being recorded — re-submit".into()));
		}
		let has_holders = holder_count(&mut tx, &change.service).await? > 0;
		let version: i32 = sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) + 1 FROM fee_policy_changes WHERE service = $1")
			.bind(change.service.as_str())
			.fetch_one(&mut *tx)
			.await
			.map_err(repo_err)?;
		let effective_from = fees::earliest_effective_from(change.now_unix, change.requested_effective_from_unix, has_holders);

		let inserted = sqlx::query(
			"INSERT INTO fee_policy_changes (id, service, version, management_bps, performance_bps, hurdle_bps, basis, crystallization, \
			   state, requirement, effective_from, requested_by, requested_at, reason, scheduled_at) \
			 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, to_timestamp($11), $12, to_timestamp($13), $14, to_timestamp($13))",
		)
		.bind(change.id.raw())
		.bind(change.service.as_str())
		.bind(version)
		.bind(bps(change.policy.management_bps()))
		.bind(bps(change.policy.performance_bps()))
		.bind(bps(change.policy.hurdle_bps()))
		.bind(change.policy.basis().as_str())
		.bind(change.policy.crystallization().as_str())
		.bind(FeePolicyChangeState::Scheduled.as_str())
		.bind(change.requirement.as_str())
		.bind(effective_from as f64)
		.bind(&change.requested_by)
		.bind(change.now_unix as f64)
		.bind(&change.reason)
		.execute(&mut *tx)
		.await;
		if let Err(sqlx::Error::Database(err)) = &inserted
			&& err.code().as_deref() == Some(UNIQUE_VIOLATION)
		{
			// The pending index (or, on a concurrent request, the version key) spoke: one change
			// on its way per product, so the second is a refusal, not a retry.
			return Err(DomainError::Conflict(
				"a fee-policy change is already pending for this product — cancel it before scheduling another".into(),
			));
		}
		inserted.map_err(repo_err)?;

		let roster = notice_roster(&mut tx, &change.service).await?;
		record_notices(&mut tx, change.id, &roster).await?;
		// Read AFTER the notices are recorded: the row handed back states how many holders
		// are still to be told.
		let stored = find_on(&mut tx, change.id)
			.await?
			.ok_or_else(|| DomainError::Repository("fee policy change vanished inside its own transaction".into()))?;
		tx.commit().await.map_err(repo_err)?;
		Ok(stored)
	}

	async fn cancel(&self, service: &ServiceId, id: FeePolicyChangeId, by: &str, _now_unix: i64) -> Result<FeePolicyChange, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let change = locked(&mut tx, id).await?;
		if &change.service != service {
			return Err(DomainError::NotFound {
				entity: "fee policy change",
				id: id.to_string(),
			});
		}
		match change.state {
			FeePolicyChangeState::Cancelled | FeePolicyChangeState::Rejected => return Ok(change),
			FeePolicyChangeState::Scheduled | FeePolicyChangeState::AwaitingConsilium => {}
			state @ (FeePolicyChangeState::Active | FeePolicyChangeState::Superseded) => {
				return Err(DomainError::Conflict(format!("the fee-policy change is {} and can no longer be cancelled", state.as_str())));
			}
		}
		sqlx::query("UPDATE fee_policy_changes SET state = 'cancelled', closed_reason = $2 WHERE id = $1")
			.bind(id.raw())
			.bind(format!("withdrawn by {by}"))
			.execute(&mut *tx)
			.await
			.map_err(repo_err)?;
		// The notices go with the change, in the same transaction: a notifier catching up
		// later must not tell a holder that terms which will never bind "change on <date>"
		// (#319). Delivered ones stand — what was said was true when it was said.
		let withdrawn = sqlx::query("UPDATE fee_policy_notices SET withdrawn_at = now() WHERE change_id = $1 AND delivered_at IS NULL AND withdrawn_at IS NULL")
			.bind(id.raw())
			.execute(&mut *tx)
			.await
			.map_err(repo_err)?
			.rows_affected();
		if withdrawn > 0 {
			tracing::info!(change_id = %id, service = %service, withdrawn, "fee policy: withdrew undelivered holder notices with the cancelled change");
		}
		let stored = find_on(&mut tx, id)
			.await?
			.ok_or_else(|| DomainError::Repository("fee policy change vanished under lock".into()))?;
		tx.commit().await.map_err(repo_err)?;
		Ok(stored)
	}

	async fn acknowledge_undelivered_notices(&self, service: &ServiceId, id: FeePolicyChangeId, by: &str, now_unix: i64) -> Result<FeePolicyChange, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		// The product lock first, as every transition on a change takes it: the list written
		// here is judged against the holders of the moment, and a promotion must not read a
		// half-written acknowledgement.
		lock_product(&mut tx, service).await?;
		let change = locked(&mut tx, id).await?;
		if &change.service != service {
			return Err(DomainError::NotFound {
				entity: "fee policy change",
				id: id.to_string(),
			});
		}
		if change.state != FeePolicyChangeState::Scheduled {
			// A closed change waits on nobody, so its record — if it has one — has nothing
			// left to gain: a repeat returns it, as it did while the change was open.
			if change.notices_waiver.is_some() {
				return Ok(change);
			}
			return Err(DomainError::Conflict(format!(
				"the fee-policy change is {}; only a scheduled change has holder notices to acknowledge",
				change.state.as_str()
			)));
		}
		// Only a tightening waits on the holders' notice — `promote` binds cheaper terms over
		// an untold holder by itself. An acknowledgement on a loosening would be a line in the
		// history saying somebody waived a protection nobody was owed.
		let current = current_terms(&mut tx, service).await?;
		if !change.policy.tightens_from(current.as_ref().unwrap_or(&FeePolicy::NONE)) {
			return Err(DomainError::Conflict(
				"the terms only get cheaper for the holders — a loosening binds over an undelivered notice by itself and needs no acknowledgement".into(),
			));
		}
		// Only the notices GIVEN UP on: a notice still being tried may yet arrive, and a
		// holder it reaches was told — waiving their notice before the trying is over would
		// take responsibility for holders nobody has failed to reach. A holder still in the queue at this moment holds the change until their
		// notice is delivered or given up on — and a later acknowledgement then ADDS them:
		// the record grows by the holders given up on since, never shrinks, and the caller
		// who extends it takes responsibility for the whole list (it is their name and
		// moment the row then carries). A repeat that would add nobody leaves the record
		// untouched — it says who took responsibility, and a retry must not rewrite that.
		let undelivered = undelivered_notices(&mut tx, change.id, service).await?;
		let covered: &[UserId] = change.notices_waiver.as_ref().map_or(&[], |waiver| waiver.users.as_slice());
		let added: Vec<Uuid> = undelivered
			.iter()
			.filter(|notice| notice.given_up && !covered.contains(&notice.user_id))
			.map(|notice| notice.user_id.raw())
			.collect();
		if added.is_empty() {
			if change.notices_waiver.is_some() {
				return Ok(change);
			}
			if undelivered.is_empty() {
				// Not a no-op: the operator saw a figure that is no longer true (the notice was
				// delivered, or the holder left), and the change binds by itself on the next tick.
				return Err(DomainError::Conflict(
					"every holder notice for this change has been delivered — there is nothing to acknowledge".into(),
				));
			}
			return Err(DomainError::Conflict(format!(
				"{} holder notice(s) for this change are still being delivered and none has been given up on yet — there is nothing to acknowledge until delivery is given up",
				undelivered.len()
			)));
		}
		let users: Vec<Uuid> = covered.iter().map(|user| user.raw()).chain(added).collect();
		sqlx::query("UPDATE fee_policy_changes SET notices_waived_by = $2, notices_waived_at = to_timestamp($3), notices_waived_users = $4 WHERE id = $1")
			.bind(id.raw())
			.bind(by)
			.bind(now_unix as f64)
			.bind(&users)
			.execute(&mut *tx)
			.await
			.map_err(repo_err)?;
		let stored = find_on(&mut tx, id)
			.await?
			.ok_or_else(|| DomainError::Repository("fee policy change vanished under lock".into()))?;
		tx.commit().await.map_err(repo_err)?;
		Ok(stored)
	}

	async fn find(&self, id: FeePolicyChangeId) -> Result<Option<FeePolicyChange>, DomainError> {
		let mut conn = self.pool.acquire().await.map_err(repo_err)?;
		find_on(&mut conn, id).await
	}

	async fn holder_count(&self, service: &ServiceId) -> Result<u32, DomainError> {
		let mut conn = self.pool.acquire().await.map_err(repo_err)?;
		holder_count(&mut conn, service).await
	}

	async fn pending(&self, service: &ServiceId) -> Result<Option<FeePolicyChange>, DomainError> {
		let mut conn = self.pool.acquire().await.map_err(repo_err)?;
		let row = sqlx::query(concat!(
			"SELECT ",
			change_columns!(),
			" FROM fee_policy_changes WHERE service = $1 AND state IN ('awaiting_consilium', 'scheduled')"
		))
		.bind(service.as_str())
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?;
		match row {
			Some(row) => hydrate(&mut conn, &row).await.map(Some),
			None => Ok(None),
		}
	}

	async fn list(&self, service: &ServiceId) -> Result<Vec<FeePolicyChange>, DomainError> {
		let mut conn = self.pool.acquire().await.map_err(repo_err)?;
		let rows = sqlx::query(concat!("SELECT ", change_columns!(), " FROM fee_policy_changes WHERE service = $1 ORDER BY version DESC"))
			.bind(service.as_str())
			.fetch_all(&mut *conn)
			.await
			.map_err(repo_err)?;
		let mut changes = Vec::with_capacity(rows.len());
		for row in &rows {
			changes.push(hydrate(&mut conn, row).await?);
		}
		Ok(changes)
	}

	async fn due(&self, now_unix: i64) -> Result<Vec<FeePolicyChangeId>, DomainError> {
		let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM fee_policy_changes WHERE state = 'scheduled' AND effective_from <= to_timestamp($1) ORDER BY effective_from")
			.bind(now_unix)
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?;
		Ok(ids.into_iter().map(FeePolicyChangeId::from_raw).collect())
	}

	async fn promote(&self, id: FeePolicyChangeId, now_unix: i64) -> Result<bool, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		// An unlocked probe for the product, so the PRODUCT lock is taken before the change's —
		// the order `schedule` uses. Nothing below trusts the probe.
		let Some(probe) = find_on(&mut tx, id).await? else {
			return Ok(false);
		};
		lock_product(&mut tx, &probe.service).await?;
		let change = locked(&mut tx, id).await?;
		if change.state != FeePolicyChangeState::Scheduled || change.effective_from_unix > now_unix {
			return Ok(false);
		}
		// A notice that has not reached its holder — still undelivered, or given up on — is a
		// holder the notice period exists to warn who was never warned. Terms that get DEARER
		// for them do not bind over them: the change stays `scheduled`, a notice delivered
		// later lets the next tick promote, a notice given up on needs the operator, and the
		// sweeper's failure streak turns the refusal into an error.
		// The operator's move is the acknowledgement: it names the holders whose notice had
		// been given up on when it was given (and, on a later call, since), and the terms
		// bind over THOSE — a holder it does not name (one whose notice was still being tried at
		// the time, or one who had redeemed and has since bought back in) still holds the change,
		// so an acknowledgement never widens by itself: only another acknowledgement does.
		// Terms that only get cheaper bind regardless, on the record: a holder the identity
		// plane cannot reach would otherwise pin a
		// product's terms forever — the loosening, and the lowering of a legacy rate above
		// today's ceiling, included. No terms at all are measured as `FeePolicy::NONE`: a
		// first positive rate tightens.
		let undelivered = undelivered_notices(&mut tx, change.id, &change.service).await?;
		if !undelivered.is_empty() {
			let total = undelivered.len();
			let given_up = undelivered.iter().filter(|notice| notice.given_up).count();
			let current = current_terms(&mut tx, &change.service).await?;
			if change.policy.tightens_from(current.as_ref().unwrap_or(&FeePolicy::NONE)) {
				let Some(waiver) = &change.notices_waiver else {
					return Err(DomainError::Conflict(format!(
						"{total} holder notice(s) for this change are undelivered, {given_up} of them given up on — dearer terms bind once every holder has been told"
					)));
				};
				let unacknowledged = undelivered.iter().filter(|notice| !waiver.users.contains(&notice.user_id)).count();
				if unacknowledged > 0 {
					return Err(DomainError::Conflict(format!(
						"{total} holder notice(s) for this change are undelivered, {unacknowledged} of them to holders the acknowledgement by {} does not cover — dearer terms bind once every holder has been told or acknowledged",
						waiver.by
					)));
				}
				// WARN on purpose: terms got dearer for holders who were never told, on one
				// person's word. The line names them by id; the row keeps the same list.
				tracing::warn!(
					change_id = %change.id,
					service = %change.service,
					undelivered = total,
					given_up,
					acknowledged_by = %waiver.by,
					acknowledged_at = waiver.at_unix,
					holders = ?waiver.users,
					"fee policy: binding dearer terms over holders whose notice was not delivered, on the operator's acknowledgement"
				);
			} else {
				tracing::warn!(
					change_id = %change.id,
					service = %change.service,
					undelivered = total,
					given_up,
					"fee policy: binding cheaper terms over holders whose notice was not delivered"
				);
			}
		}

		// Every holder's row, locked — the same lock a charge and a settle take — so no
		// assessment can read the new terms against a clock the old terms still own.
		let positions =
			sqlx::query("SELECT user_id, EXTRACT(EPOCH FROM fees_accrued_at)::bigint AS accrued_at_unix FROM fund_positions WHERE service = $1 AND units <> '0' ORDER BY user_id FOR UPDATE")
				.bind(change.service.as_str())
				.fetch_all(&mut *tx)
				.await
				.map_err(repo_err)?;
		for row in &positions {
			let user: Uuid = row.try_get("user_id").map_err(repo_err)?;
			let accrued_at: i64 = row.try_get("accrued_at_unix").map_err(repo_err)?;
			// A clock already past `effective_from` (a sweep or a top-up landed in the minute
			// between the moment and this promotion) has nothing left to settle at the old
			// rate; carrying it would only move the clock backwards.
			if accrued_at < change.effective_from_unix {
				carry_accrual(&mut tx, user, change.service.as_str(), change.effective_from_unix).await?;
			}
		}

		// STRICTLY AFTER the carries above: `carry_accrual` prices the elapsed window off the
		// row this upsert replaces.
		sqlx::query(
			"INSERT INTO fee_policies (service, management_bps, performance_bps, hurdle_bps, basis, crystallization, updated_by, updated_at, version, effective_from) \
			 VALUES ($1, $2, $3, $4, $5, $6, $7, now(), $8, to_timestamp($9)) \
			 ON CONFLICT (service) DO UPDATE SET management_bps = EXCLUDED.management_bps, performance_bps = EXCLUDED.performance_bps, \
			   hurdle_bps = EXCLUDED.hurdle_bps, basis = EXCLUDED.basis, crystallization = EXCLUDED.crystallization, \
			   updated_by = EXCLUDED.updated_by, updated_at = now(), version = EXCLUDED.version, effective_from = EXCLUDED.effective_from",
		)
		.bind(change.service.as_str())
		.bind(bps(change.policy.management_bps()))
		.bind(bps(change.policy.performance_bps()))
		.bind(bps(change.policy.hurdle_bps()))
		.bind(change.policy.basis().as_str())
		.bind(change.policy.crystallization().as_str())
		.bind(&change.requested_by)
		.bind(i32::try_from(change.version).unwrap_or(i32::MAX))
		.bind(change.effective_from_unix as f64)
		.execute(&mut *tx)
		.await
		.map_err(repo_err)?;

		sqlx::query("UPDATE fee_policy_changes SET state = 'superseded' WHERE service = $1 AND state = 'active'")
			.bind(change.service.as_str())
			.execute(&mut *tx)
			.await
			.map_err(repo_err)?;
		sqlx::query("UPDATE fee_policy_changes SET state = 'active', applied_at = to_timestamp($2) WHERE id = $1")
			.bind(id.raw())
			.bind(now_unix as f64)
			.execute(&mut *tx)
			.await
			.map_err(repo_err)?;
		tx.commit().await.map_err(repo_err)?;
		Ok(true)
	}
}
