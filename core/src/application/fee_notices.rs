//! Telling a fund's holders that its fee terms change — the notice the notice period exists
//! for.
//!
//! Every holder of a scheduled change is owed one notice, recorded in the same transaction
//! as the change (`fee_policy_notices`). Only people hold a position: a reserved allocation
//! that holds a product (`fee` holds its fee class) holds it on the ledger's `FeeShares`
//! account, never in `fund_positions`, so it is owed no notice and gets none.
//!
//! [`round`] is the sender, run by `infrastructure::fee_notices` every [`ROUND_INTERVAL`] as
//! a singleton. It sends each due notice through concierge's scope inbox on [`TOPIC`] and:
//!
//! - stamps it delivered when concierge says it reached the holder;
//! - gives it up at once when concierge says it will not deliver it. [`TOPIC`] is required,
//!   so a member cannot switch it off: that answer means the holder is not an active member
//!   of invest's scope (or concierge dropped the flag, and they switched it off);
//! - otherwise tries again after [`retry_delay`], and gives it up after [`MAX_ATTEMPTS`].
//!
//! Every attempt of one notice carries the same `dedupe_key`, so a retry after a lost answer,
//! or after a restart, never sends a second message. concierge answers a repeated key with
//! "not delivered", the same as for a holder it cannot reach, so a retry after an attempt
//! that may have landed is given up rather than marked delivered: the holders' side of the
//! doubt (a dearer change then waits, or is withdrawn).
//!
//! **The notice period is not moved by delivery.** It runs from the moment the change is
//! scheduled (for an owner change, when the owners pass it): `effective_from` is at least 24
//! hours after that, and delivery never shortens it. A change that makes the terms dearer
//! binds only once every current holder's notice is delivered — or, for an administrator's
//! change, once its requester acknowledged the ones given up on. The notice of an owner
//! change cannot be waived: a holder nobody could reach keeps it from binding, and the
//! requester withdraws it.

use std::time::Duration;

use invest_domain::{
	error::DomainError,
	fees::{CrystallizationPeriod, FeePolicy, ManagementBasis},
};
use tracing::{error, info, warn};

use crate::ports::{
	catalog::FEE_TERMS_TOPIC,
	notices::{FeeNoticeOutbox, HolderNotice, HolderNotifier, NoticeClaim, NoticeOutcome, NoticeSubject, NotifyError, PendingNotice},
};

/// The topic invest publishes in its scope catalog for these notices.
pub const TOPIC: &str = FEE_TERMS_TOPIC;
/// The event kind within the topic.
pub const KIND: &str = "fee_terms_change";
/// The cabinet the notice links into: invest's own, the only scope its client may name.
pub const LINK_SCOPE: &str = "invest";

/// How often [`round`] runs.
pub const ROUND_INTERVAL: Duration = Duration::from_secs(15);

/// Notices sent per round, one at a time: each holds its change for the length of a call.
const BATCH: u32 = 100;

/// Attempts before a notice concierge could not take is given up on. With [`retry_delay`]
/// that is about three hours of trying — well inside the 24-hour notice period, so the
/// operator learns in time that a holder could not be told.
pub const MAX_ATTEMPTS: u32 = 10;

const FIRST_RETRY_SECS: i64 = 30;
const MAX_RETRY_SECS: i64 = 3600;

/// The wait before the next attempt, after `attempts` failed ones: 30 s, doubled per failure,
/// at most an hour.
pub fn retry_delay(attempts: u32) -> i64 {
	let doublings = attempts.saturating_sub(1).min(16);
	(FIRST_RETRY_SECS << doublings).min(MAX_RETRY_SECS)
}

/// The two ports a round borrows.
pub struct NoticePorts<'a> {
	pub outbox: &'a dyn FeeNoticeOutbox,
	pub notifier: &'a dyn HolderNotifier,
}

/// What one round did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NoticeRound {
	pub delivered: u32,
	pub retried: u32,
	pub given_up: u32,
}

/// Send every notice that is due now. A database failure ends the round; the next one starts
/// again from the same rows.
pub async fn round(ports: &NoticePorts<'_>, now_unix: i64) -> Result<NoticeRound, DomainError> {
	let mut done = NoticeRound::default();
	for pending in ports.outbox.due(now_unix, BATCH).await? {
		// Delivered, given up on or withdrawn since it was listed: nothing left to send.
		let Some(claim) = ports.outbox.claim(pending).await? else { continue };
		match attempt(ports.notifier, pending, claim, now_unix).await? {
			Verdict::Delivered => done.delivered += 1,
			Verdict::Retried => done.retried += 1,
			Verdict::GivenUp => done.given_up += 1,
		}
	}
	Ok(done)
}

enum Verdict {
	Delivered,
	Retried,
	GivenUp,
}

async fn attempt(notifier: &dyn HolderNotifier, pending: PendingNotice, claim: Box<dyn NoticeClaim>, now_unix: i64) -> Result<Verdict, DomainError> {
	let earlier = claim.attempts();
	let Some(notice) = notice_for(pending, claim.subject(), now_unix) else {
		let reason = "the change carries no record of the terms it replaces, so the notice cannot state them";
		warn!(change_id = %pending.change, user = %pending.user, "fee notice: given up — {reason}");
		claim.give_up(now_unix, reason).await?;
		return Ok(Verdict::GivenUp);
	};
	match notifier.notify(&notice).await {
		Ok(NoticeOutcome::Delivered) => {
			claim.delivered(now_unix).await?;
			info!(change_id = %pending.change, user = %pending.user, "fee notice: delivered");
			Ok(Verdict::Delivered)
		}
		Ok(NoticeOutcome::NotDelivered(why)) => {
			let reason = if earlier == 0 {
				why
			} else {
				// concierge answers a repeated dedupe key as "not delivered" too, so after an
				// attempt that may have landed this answer cannot tell the two apart.
				format!("{why} (after {earlier} earlier attempt(s), any of which may have reached the holder)")
			};
			warn!(change_id = %pending.change, user = %pending.user, "fee notice: given up — {reason}");
			claim.give_up(now_unix, &reason).await?;
			Ok(Verdict::GivenUp)
		}
		Err(err) => {
			let attempts = earlier.saturating_add(1);
			if let NotifyError::Refused(_) = &err {
				error!(change_id = %pending.change, user = %pending.user, attempts, "fee notice: the channel refused it (needs intervention): {err}");
			}
			if attempts >= MAX_ATTEMPTS {
				let reason = format!("gave up after {attempts} attempts: {err}");
				warn!(change_id = %pending.change, user = %pending.user, "fee notice: given up — {reason}");
				claim.give_up(now_unix, &reason).await?;
				return Ok(Verdict::GivenUp);
			}
			let delay = retry_delay(attempts);
			if let NotifyError::Unavailable(_) = &err {
				warn!(change_id = %pending.change, user = %pending.user, attempts, retry_in_secs = delay, "fee notice: not sent, will retry: {err}");
			}
			claim.retry_at(now_unix.saturating_add(delay), &err.to_string()).await?;
			Ok(Verdict::Retried)
		}
	}
}

/// The notice as the holder reads it, or `None` when the change recorded no "from".
pub fn notice_for(pending: PendingNotice, subject: &NoticeSubject, now_unix: i64) -> Option<HolderNotice> {
	let from = subject.from?;
	let to = &subject.to;
	let fund = subject.fund_title.as_str();
	let mut body = format!("The fee terms of {fund} change. The new terms apply from {}.", utc_moment(subject.effective_from_unix));
	if subject.effective_from_unix > now_unix {
		body.push_str(" Until then, your current terms stay in force.");
	}
	body.push_str("\n\n");
	for line in [
		term("Management fee", |p| format!("{} a year", percent(p.management_bps())), &from, to),
		term("The management fee is charged on", |p| basis(p.basis()).to_owned(), &from, to),
		term("Performance fee", |p| format!("{} of the gain", percent(p.performance_bps())), &from, to),
		term("Hurdle (the yearly gain before a performance fee is due)", |p| hurdle(p.hurdle_bps()), &from, to),
		term("The performance fee is settled", |p| crystallization(p.crystallization()).to_owned(), &from, to),
	] {
		body.push_str(&line);
		body.push('\n');
	}
	body.push_str("\nYou can see the fund and its terms in your invest cabinet.");
	Some(HolderNotice {
		user: pending.user,
		topic: TOPIC,
		kind: KIND,
		title: format!("New fee terms for {fund}"),
		body,
		link: format!("/invest/{}", subject.service),
		link_scope: LINK_SCOPE,
		// One message per holder and change, whatever the number of attempts.
		dedupe_key: format!("fee_policy_notice:{}", pending.change),
		occurred_at_unix: subject.scheduled_at_unix,
	})
}

fn term(name: &str, show: impl Fn(&FeePolicy) -> String, from: &FeePolicy, to: &FeePolicy) -> String {
	let (old, new) = (show(from), show(to));
	if old == new {
		format!("{name}: {new} (no change)")
	} else {
		format!("{name}: {old} → {new}")
	}
}

/// Basis points as a percentage without trailing zeros: 200 → "2%", 250 → "2.5%".
fn percent(bps: u32) -> String {
	let (whole, cents) = (bps / 100, bps % 100);
	match cents {
		0 => format!("{whole}%"),
		c if c % 10 == 0 => format!("{whole}.{}%", c / 10),
		c => format!("{whole}.{c:02}%"),
	}
}

fn hurdle(bps: u32) -> String {
	if bps == 0 { "none".to_owned() } else { format!("{} a year", percent(bps)) }
}

fn basis(basis: ManagementBasis) -> &'static str {
	match basis {
		ManagementBasis::InvestedCapital => "the money you invested",
		ManagementBasis::MarketValue => "the current value of your units",
	}
}

fn crystallization(period: CrystallizationPeriod) -> &'static str {
	match period {
		CrystallizationPeriod::Monthly => "every month",
		CrystallizationPeriod::Quarterly => "every quarter",
		CrystallizationPeriod::SemiAnnual => "every half year",
		CrystallizationPeriod::Annual => "once a year",
	}
}

/// `unix` as "23 October 2026, 14:00 UTC". The proleptic Gregorian calendar from days since
/// 1970 (H. Hinnant's `civil_from_days`), so no date library is needed for one line.
fn utc_moment(unix: i64) -> String {
	const MONTHS: [&str; 12] = [
		"January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December",
	];
	let (days, secs) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
	let z = days + 719_468;
	let era = z.div_euclid(146_097);
	let doe = z.rem_euclid(146_097);
	let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let day = doy - (153 * mp + 2) / 5 + 1;
	let month = if mp < 10 { mp + 3 } else { mp - 9 };
	let year = yoe + era * 400 + i64::from(month <= 2);
	// `month` is 1..=12 by construction.
	let name = MONTHS[usize::try_from(month - 1).unwrap_or(0)];
	format!("{day} {name} {year}, {:02}:{:02} UTC", secs / 3_600, secs % 3_600 / 60)
}

#[cfg(test)]
mod tests {
	use invest_domain::{balance::ServiceId, fees::FeePolicyChangeId, users::UserId};

	use super::*;

	fn subject(from: Option<FeePolicy>, to: FeePolicy) -> NoticeSubject {
		NoticeSubject {
			service: ServiceId::parse("ev-trading").unwrap(),
			fund_title: "EV Trading".to_owned(),
			from,
			to,
			// 2026-10-23 14:00 UTC.
			effective_from_unix: 1_792_764_000,
			scheduled_at_unix: 1_792_677_600,
		}
	}

	#[test]
	fn the_notice_states_the_fund_both_terms_the_date_and_the_link() {
		let pending = PendingNotice {
			change: FeePolicyChangeId::new(),
			user: UserId::new(),
		};
		let to = FeePolicy::new(300, 2_500, 500, ManagementBasis::MarketValue, CrystallizationPeriod::Quarterly).unwrap();
		let notice = notice_for(pending, &subject(Some(FeePolicy::HOUSE), to), 1_792_677_600).unwrap();

		assert_eq!(notice.title, "New fee terms for EV Trading");
		assert_eq!(
			notice.body,
			"The fee terms of EV Trading change. The new terms apply from 23 October 2026, 14:00 UTC. Until then, your current terms stay in force.\n\n\
			 Management fee: 2% a year → 3% a year\n\
			 The management fee is charged on: the money you invested → the current value of your units\n\
			 Performance fee: 20% of the gain → 25% of the gain\n\
			 Hurdle (the yearly gain before a performance fee is due): none → 5% a year\n\
			 The performance fee is settled: once a year → every quarter\n\
			 \nYou can see the fund and its terms in your invest cabinet."
		);
		assert_eq!((notice.link.as_str(), notice.link_scope), ("/invest/ev-trading", "invest"));
		assert_eq!((notice.topic, notice.kind), ("invest:fee-terms", "fee_terms_change"));
		assert_eq!(notice.dedupe_key, format!("fee_policy_notice:{}", pending.change));
		assert_eq!((notice.user, notice.occurred_at_unix), (pending.user, 1_792_677_600));
		// concierge's limits on an emit (`emit_fields`).
		assert!(notice.title.chars().count() <= 200 && notice.body.chars().count() <= 2_000 && notice.dedupe_key.len() <= 128 && notice.kind.len() <= 64);
	}

	#[test]
	fn an_unchanged_term_says_so_and_a_past_date_promises_nothing() {
		let pending = PendingNotice {
			change: FeePolicyChangeId::new(),
			user: UserId::new(),
		};
		let cheaper = FeePolicy::new(150, 2_000, 0, ManagementBasis::InvestedCapital, CrystallizationPeriod::Annual).unwrap();
		let notice = notice_for(pending, &subject(Some(FeePolicy::HOUSE), cheaper), 1_792_764_001).unwrap();
		assert!(notice.body.contains("Management fee: 2% a year → 1.5% a year\n"), "{}", notice.body);
		assert!(notice.body.contains("Performance fee: 20% of the gain (no change)\n"), "{}", notice.body);
		assert!(!notice.body.contains("Until then"), "the terms already apply: {}", notice.body);
	}

	#[test]
	fn a_change_with_no_recorded_from_has_no_notice_text() {
		let pending = PendingNotice {
			change: FeePolicyChangeId::new(),
			user: UserId::new(),
		};
		assert_eq!(notice_for(pending, &subject(None, FeePolicy::HOUSE), 0), None);
	}

	#[test]
	fn rates_and_dates_read_plainly() {
		assert_eq!([percent(0), percent(200), percent(250), percent(125), percent(5)], ["0%", "2%", "2.5%", "1.25%", "0.05%"]);
		assert_eq!(utc_moment(0), "1 January 1970, 00:00 UTC");
		assert_eq!(utc_moment(951_782_400), "29 February 2000, 00:00 UTC");
		assert_eq!(utc_moment(1_792_764_000), "23 October 2026, 14:00 UTC");
	}

	#[test]
	fn the_retry_delay_doubles_from_thirty_seconds_up_to_an_hour() {
		assert_eq!((1..=MAX_ATTEMPTS).map(retry_delay).collect::<Vec<_>>(), [30, 60, 120, 240, 480, 960, 1_920, 3_600, 3_600, 3_600]);
	}
}
