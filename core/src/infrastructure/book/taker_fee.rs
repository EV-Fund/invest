//! Taker-fee rises: scheduled with a notice period, never written onto `book_policies` by
//! the operator's request itself.
//!
//! This is the fee-policy path (`fee_policy_changes.rs`) for the one term a book has that
//! can cost a trader more: the same roster of holders (`notice_roster`), the same notice
//! recorded per holder in the transaction that schedules the change, the same "dearer
//! terms bind only once every holder has been told or acknowledged", the same promoter
//! (the fee sweeper) and the same cancel that withdraws undelivered notices. Its own tables,
//! because a taker fee has none of a fund's management/performance terms.
//!
//! Every transaction opens with the book's advisory lock — the one `place` takes — so a
//! change, a promotion and a placement agree on which fee is in force.

use invest_domain::{
	balance::ServiceId,
	book::{BookPolicy, TakerFeeMove},
	error::DomainError,
};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use super::{lock_book, repo_err, upsert_policy};
use crate::{
	infrastructure::fee_policy_changes::notice_roster,
	ports::book::{BookPolicyRecord, PendingTakerFeeRise, TakerFeeChange},
};

/// Postgres' unique-violation SQLSTATE — how `book_taker_fee_changes_single_pending_idx` answers.
const UNIQUE_VIOLATION: &str = "23505";

fn bps(value: u32) -> Result<i32, DomainError> {
	i32::try_from(value).map_err(|_| DomainError::Validation("taker fee out of range".into()))
}

pub(super) async fn pending_on(conn: &mut PgConnection, service: &ServiceId) -> Result<Option<PendingTakerFeeRise>, DomainError> {
	let row = sqlx::query(
		"SELECT id, taker_fee_bps, EXTRACT(EPOCH FROM effective_from)::bigint AS effective_from_unix, requested_by, \
		 EXTRACT(EPOCH FROM requested_at)::bigint AS requested_at_unix, acknowledged_by \
		 FROM book_taker_fee_changes WHERE service = $1 AND state = 'scheduled'",
	)
	.bind(service.as_str())
	.fetch_optional(&mut *conn)
	.await
	.map_err(repo_err)?;
	row.map(|row| {
		Ok(PendingTakerFeeRise {
			id: row.try_get("id").map_err(repo_err)?,
			taker_fee_bps: u32::try_from(row.try_get::<i32, _>("taker_fee_bps").map_err(repo_err)?).map_err(|_| DomainError::Repository("malformed pending taker fee".into()))?,
			effective_from_unix: row.try_get("effective_from_unix").map_err(repo_err)?,
			requested_by: row.try_get("requested_by").map_err(repo_err)?,
			requested_at_unix: row.try_get("requested_at_unix").map_err(repo_err)?,
			acknowledged_by: row.try_get("acknowledged_by").map_err(repo_err)?,
		})
	})
	.transpose()
}

/// Close the pending rise, if any, and withdraw its undelivered notices in the same
/// transaction, so no notifier can later tell holders about a fee that will never bind.
/// Delivered notices stand: what was said was true when it was said.
async fn withdraw_pending(conn: &mut PgConnection, service: &ServiceId, reason: &str) -> Result<bool, DomainError> {
	let closed: Option<Uuid> = sqlx::query_scalar("UPDATE book_taker_fee_changes SET state = 'cancelled', closed_reason = $2 WHERE service = $1 AND state = 'scheduled' RETURNING id")
		.bind(service.as_str())
		.bind(reason)
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?;
	let Some(id) = closed else { return Ok(false) };
	let withdrawn = sqlx::query("UPDATE book_taker_fee_notices SET withdrawn_at = now() WHERE change_id = $1 AND delivered_at IS NULL AND withdrawn_at IS NULL")
		.bind(id)
		.execute(&mut *conn)
		.await
		.map_err(repo_err)?
		.rows_affected();
	tracing::info!(change_id = %id, service = %service, withdrawn, "book: withdrew a pending taker-fee rise and its undelivered holder notices");
	Ok(true)
}

pub(super) async fn change_policy(pool: &PgPool, service: &ServiceId, policy: &BookPolicy, request: &TakerFeeChange<'_>) -> Result<BookPolicyRecord, DomainError> {
	let mut tx = pool.begin().await.map_err(repo_err)?;
	lock_book(&mut tx, service).await?;
	let live: Option<i32> = sqlx::query_scalar("SELECT taker_fee_bps FROM book_policies WHERE service = $1")
		.bind(service.as_str())
		.fetch_optional(&mut *tx)
		.await
		.map_err(repo_err)?;
	let live = u32::try_from(live.unwrap_or(0)).map_err(|_| DomainError::Repository("malformed taker fee".into()))?;

	let mut record = match request.terms.plan(live, policy.taker_fee_bps(), request.now_unix)? {
		TakerFeeMove::Unchanged => upsert_policy(&mut tx, service, policy).await?,
		TakerFeeMove::Cut => {
			withdraw_pending(&mut tx, service, "superseded by a lower taker fee").await?;
			upsert_policy(&mut tx, service, policy).await?
		}
		TakerFeeMove::Rise { effective_from_unix } => {
			if pending_on(&mut tx, service).await?.is_some() {
				return Err(DomainError::Conflict(
					"a taker-fee rise is already pending for this product — cancel it before scheduling another".into(),
				));
			}
			let roster = notice_roster(&mut tx, service).await?;
			// The notices cannot be delivered yet (no channel — see `fee_policy_changes.rs`), so
			// a rise over holders binds only on somebody's word, and that somebody is the
			// requester, at the moment they ask.
			if !roster.is_empty() && !request.acknowledged_undelivered_notices {
				return Err(DomainError::Precondition(format!(
					"{} holder(s) are owed notice of this rise and invest cannot deliver it yet — acknowledge the undelivered notices to schedule it",
					roster.len()
				)));
			}
			// The rest of the terms bind now; the fee in force does not move.
			let record = upsert_policy(&mut tx, service, &policy.with_taker_fee_bps(live)?).await?;
			let acknowledged = !roster.is_empty();
			let inserted = sqlx::query(
				"INSERT INTO book_taker_fee_changes (id, service, from_bps, taker_fee_bps, state, effective_from, requested_by, requested_at, \
				   acknowledged_by, acknowledged_at, acknowledged_users) \
				 VALUES ($1, $2, $3, $4, 'scheduled', to_timestamp($5), $6, to_timestamp($7), $8, to_timestamp($9), $10)",
			)
			.bind(request.id)
			.bind(service.as_str())
			.bind(bps(live)?)
			.bind(bps(policy.taker_fee_bps())?)
			.bind(effective_from_unix as f64)
			.bind(request.by)
			.bind(request.now_unix as f64)
			.bind(acknowledged.then_some(request.by))
			.bind(acknowledged.then_some(request.now_unix as f64))
			.bind(acknowledged.then_some(&roster))
			.execute(&mut *tx)
			.await;
			if let Err(sqlx::Error::Database(err)) = &inserted
				&& err.code().as_deref() == Some(UNIQUE_VIOLATION)
			{
				return Err(DomainError::Conflict(
					"a taker-fee rise is already pending for this product — cancel it before scheduling another".into(),
				));
			}
			inserted.map_err(repo_err)?;
			if !roster.is_empty() {
				// TODO(concierge scope notifications): record these undelivered and let the notifier
				// emit them; until then each is given up on as it is recorded, like a fee-policy notice.
				sqlx::query("INSERT INTO book_taker_fee_notices (change_id, user_id, given_up_at) SELECT $1, holder, now() FROM UNNEST($2::uuid[]) AS holder ON CONFLICT DO NOTHING")
					.bind(request.id)
					.bind(&roster)
					.execute(&mut *tx)
					.await
					.map_err(repo_err)?;
			}
			tracing::warn!(
				change_id = %request.id,
				service = %service,
				from_bps = live,
				to_bps = policy.taker_fee_bps(),
				effective_from = effective_from_unix,
				holders = roster.len(),
				requested_by = request.by,
				"book: scheduled a taker-fee rise"
			);
			record
		}
	};
	record.pending_rise = pending_on(&mut tx, service).await?;
	tx.commit().await.map_err(repo_err)?;
	Ok(record)
}

pub(super) async fn cancel(pool: &PgPool, service: &ServiceId, by: &str) -> Result<bool, DomainError> {
	let mut tx = pool.begin().await.map_err(repo_err)?;
	lock_book(&mut tx, service).await?;
	let closed = withdraw_pending(&mut tx, service, &format!("withdrawn by {by}")).await?;
	tx.commit().await.map_err(repo_err)?;
	Ok(closed)
}

pub(super) async fn due(pool: &PgPool, now_unix: i64) -> Result<Vec<Uuid>, DomainError> {
	sqlx::query_scalar("SELECT id FROM book_taker_fee_changes WHERE state = 'scheduled' AND effective_from <= to_timestamp($1) ORDER BY effective_from")
		.bind(now_unix as f64)
		.fetch_all(pool)
		.await
		.map_err(repo_err)
}

pub(super) async fn promote(pool: &PgPool, id: Uuid, now_unix: i64) -> Result<bool, DomainError> {
	let mut tx = pool.begin().await.map_err(repo_err)?;
	// An unlocked probe for the product, so the book's lock is taken before the change's —
	// the order `change_policy` uses. Nothing below trusts the probe.
	let probe: Option<String> = sqlx::query_scalar("SELECT service FROM book_taker_fee_changes WHERE id = $1")
		.bind(id)
		.fetch_optional(&mut *tx)
		.await
		.map_err(repo_err)?;
	let Some(probe) = probe else { return Ok(false) };
	let service = ServiceId::parse(&probe)?;
	lock_book(&mut tx, &service).await?;
	let row = sqlx::query(
		"SELECT taker_fee_bps, state, EXTRACT(EPOCH FROM effective_from)::bigint AS effective_from_unix, acknowledged_users \
		 FROM book_taker_fee_changes WHERE id = $1 FOR UPDATE",
	)
	.bind(id)
	.fetch_one(&mut *tx)
	.await
	.map_err(repo_err)?;
	let state: String = row.try_get("state").map_err(repo_err)?;
	let effective_from: i64 = row.try_get("effective_from_unix").map_err(repo_err)?;
	if state != "scheduled" || effective_from > now_unix {
		return Ok(false);
	}
	// Counted over the holders of RIGHT NOW, as the fee-policy promotion does: a recipient
	// who has since sold out has no fee to be warned about.
	let undelivered: Vec<Uuid> = sqlx::query_scalar(
		"SELECT n.user_id FROM book_taker_fee_notices n \
		 JOIN fund_positions p ON p.user_id = n.user_id AND p.service = $2 AND p.units <> '0' \
		 WHERE n.change_id = $1 AND n.delivered_at IS NULL AND n.withdrawn_at IS NULL ORDER BY n.user_id",
	)
	.bind(id)
	.bind(service.as_str())
	.fetch_all(&mut *tx)
	.await
	.map_err(repo_err)?;
	let covered: Vec<Uuid> = row.try_get::<Option<Vec<Uuid>>, _>("acknowledged_users").map_err(repo_err)?.unwrap_or_default();
	let unacknowledged = undelivered.iter().filter(|user| !covered.contains(user)).count();
	if unacknowledged > 0 {
		return Err(DomainError::Conflict(format!(
			"{unacknowledged} holder notice(s) for this taker-fee rise are undelivered and not covered by an acknowledgement — a dearer fee binds once every holder has been told or acknowledged"
		)));
	}
	let new_fee: i32 = row.try_get("taker_fee_bps").map_err(repo_err)?;
	let written = sqlx::query("UPDATE book_policies SET taker_fee_bps = $2, updated_at = now() WHERE service = $1")
		.bind(service.as_str())
		.bind(new_fee)
		.execute(&mut *tx)
		.await
		.map_err(repo_err)?
		.rows_affected();
	if written == 0 {
		return Err(DomainError::Repository(format!("taker-fee rise {id} has no policy row of '{service}' to bind onto")));
	}
	sqlx::query("UPDATE book_taker_fee_changes SET state = 'applied', applied_at = to_timestamp($2) WHERE id = $1")
		.bind(id)
		.bind(now_unix as f64)
		.execute(&mut *tx)
		.await
		.map_err(repo_err)?;
	tx.commit().await.map_err(repo_err)?;
	tracing::warn!(change_id = %id, service = %service, taker_fee_bps = new_fee, "book: bound a scheduled taker-fee rise");
	Ok(true)
}
