//! Browser-facing JSON DTOs. These reproduce the exact wire shape banking's cabinet BFF
//! emitted (proto-loader with `keepCase` + `longs: String`): snake_case fields, with
//! 64-bit integers rendered as strings, so the cabinet's `shared/contracts` types — copied
//! from banking — stay valid. The invest.v1 messages are banking.v1's under another
//! package, so the conversions moved over unchanged.

use invest_contracts::invest::v1 as iv;
use serde::Serialize;

/// Declares one of the list DTOs: a `Vec` of an already-converted element type, the scalar
/// fields the wire shape carries alongside it, and the `From` that builds it from the proto
/// message. Every name is spelled out at the call site rather than derived from the DTO's,
/// because none of them line up reliably — the field is not always the plural of the element
/// type (`FeePolicyList.policies`), and the source message is not always `<Dto>`.
///
/// The second form names the source binding so extra fields can be given as expressions over
/// it, which is what a field needing more than a move requires (`total: r.total.to_string()`).
macro_rules! list_dto {
	($(#[$meta:meta])* $name:ident from $src:path { $field:ident: Vec<$elem:ty> $(,)? }) => {
		list_dto! { $(#[$meta])* $name from $src as l { $field: Vec<$elem> } }
	};
	(
		$(#[$meta:meta])*
		$name:ident from $src:path as $msg:ident {
			$field:ident: Vec<$elem:ty>
			$(, $(#[$xmeta:meta])* $extra:ident: $xty:ty = $value:expr)* $(,)?
		}
	) => {
		$(#[$meta])*
		#[derive(Serialize)]
		pub struct $name {
			pub $field: Vec<$elem>,
			$($(#[$xmeta])* pub $extra: $xty,)*
		}

		impl From<$src> for $name {
			fn from($msg: $src) -> Self {
				Self {
					$field: $msg.$field.into_iter().map(<$elem>::from).collect(),
					$($extra: $value,)*
				}
			}
		}
	};
}

// ── fees (operator) ───────────────────────────────────────────────

list_dto! {
	/// Every configured policy, for the operator's fees table. The element type is the same
	/// `FeePolicy` the investor read returns — the operator sees no more of a fund's terms
	/// than the people paying them do.
	FeePolicyList from iv::FeePolicyList { policies: Vec<FeePolicy> }
}

/// The manager's uncollected fee units in one fund. `value` is `units × current NAV` —
/// what a settlement would convert them to if it ran now.
#[derive(Serialize)]
pub struct FeeShares {
	pub service: String,
	pub units: String,
	pub value: String,
}

impl From<iv::FeeShares> for FeeShares {
	fn from(f: iv::FeeShares) -> Self {
		Self {
			service: f.service,
			units: f.units,
			value: f.value,
		}
	}
}

#[derive(Serialize)]
pub struct FeeSettlement {
	pub service: String,
	pub units: String,
	pub nav: String,
	pub cash: String,
}

impl From<iv::FeeSettlement> for FeeSettlement {
	fn from(s: iv::FeeSettlement) -> Self {
		Self {
			service: s.service,
			units: s.units,
			nav: s.nav,
			cash: s.cash,
		}
	}
}

/// One charge against one holding. `charged_cash` is below `management + performance`
/// exactly when the holding could not cover the whole charge and the rest went to
/// `debt_carried`.
#[derive(Serialize)]
pub struct FeeAssessment {
	pub service: String,
	pub trigger: String,
	pub nav: String,
	pub management: String,
	pub performance: String,
	pub debt_opening: String,
	pub charged_units: String,
	pub charged_cash: String,
	pub debt_carried: String,
	pub high_water_mark: String,
	pub assessed_at: String,
}

impl From<iv::FeeAssessment> for FeeAssessment {
	fn from(a: iv::FeeAssessment) -> Self {
		Self {
			service: a.service,
			trigger: a.trigger,
			nav: a.nav,
			management: a.management,
			performance: a.performance,
			debt_opening: a.debt_opening,
			charged_units: a.charged_units,
			charged_cash: a.charged_cash,
			debt_carried: a.debt_carried,
			high_water_mark: a.high_water_mark,
			assessed_at: a.assessed_at.to_string(),
		}
	}
}

list_dto! { FeeAssessmentList from iv::FeeAssessmentList { assessments: Vec<FeeAssessment> } }

// ── allocations (the registry of investable products) ─────────────

/// One catalog entry. Carries no money — units/NAV/P&L come from the funds surface
/// keyed by the same `service`.
#[derive(Serialize)]
pub struct Allocation {
	pub service: String,
	pub title: String,
	pub summary: String,
	/// `draft` | `open` | `closed`.
	pub state: String,
	pub created_at: String,
	pub updated_at: String,
	/// Authorised unit supply, decimal. Subscribe refuses a mint that would pass it.
	pub unit_cap: String,
	/// Catalog glyph the client maps onto one of its SVGs — presentation only. Always
	/// present and always one of the contract's values (`fund` when the operator picked
	/// none), so a client never has to render a missing field. A client reading a
	/// *cached* allocation from before this field existed still has to tolerate its
	/// absence.
	pub icon: String,
	/// The product's default access level: `hidden` | `view` | `invest`. What the
	/// admin console edits; an investor screen renders the card off the field below.
	pub access: String,
	/// The level the CALLER effectively holds — `max(access, their grant)`. The
	/// subscribe control keys off this, never off `state` alone: an `open` product at
	/// `view` is one the investor can see and cannot buy into. Honest for an admin too
	/// (their permission does not make them an investor), so the console must not read
	/// it as "what investors get".
	pub caller_access: String,
	/// What stands behind the units: `cash` | `in_kind`. Gates money on the way OUT: a
	/// redemption pays out of the fund's cash claim, and an `in_kind` product has none
	/// there, so invest-core refuses `Redeem` on it (412) and holders exit through the book.
	/// The client draws the redeem control off this, never off `state` alone.
	pub backing: String,
}

impl From<iv::Allocation> for Allocation {
	fn from(a: iv::Allocation) -> Self {
		Self {
			service: a.service,
			title: a.title,
			summary: a.summary,
			state: a.state,
			unit_cap: a.unit_cap,
			icon: a.icon,
			access: a.access,
			caller_access: a.caller_access,
			backing: a.backing,
			created_at: a.created_at.to_string(),
			updated_at: a.updated_at.to_string(),
		}
	}
}

list_dto! { AllocationList from iv::AllocationList { allocations: Vec<Allocation> } }

/// One investor raised above a product's default level. `user_id` and `granted_by` are
/// concierge user ids — invest knows a person by nothing else — so either can be echoed
/// straight back on a grant or revoke. `email` is always `null` here: a relying party's
/// token opens concierge's `GetMe` and nothing else, so the BFF cannot name another person
/// (banking's BFF looked them up as a first-party client). The field stays so the
/// cabinet's type is unchanged. `granted_at` crosses as a string like every other int64.
#[derive(Serialize)]
pub struct AllocationAccessGrant {
	pub service: String,
	pub user_id: String,
	pub email: Option<String>,
	/// `view` | `invest`.
	pub level: String,
	pub granted_by: String,
	pub granted_at: String,
}

impl From<iv::AllocationAccessGrant> for AllocationAccessGrant {
	fn from(g: iv::AllocationAccessGrant) -> Self {
		Self {
			service: g.service,
			user_id: g.user_id,
			email: None,
			level: g.level,
			granted_by: g.granted_by,
			granted_at: g.granted_at.to_string(),
		}
	}
}

list_dto! { AllocationAccessGrantList from iv::AllocationAccessGrantList { grants: Vec<AllocationAccessGrant> } }

/// One in-kind issuance — units moved with no cash behind them: minted (`source: mint`,
/// by an operator or an executed holder grant) or burnt out of the holder's account
/// (`source: retire`, supply shrank). `units` is always the magnitude; `source` is the
/// direction. `holder_id` is a BANKING user id for a `user` holder and the holding
/// allocation's slug for an `allocation` holder (the `fee` allocation holding a product's
/// fee class) — never resolve the latter as a user. `state` is `queued` until the relay posts the
/// leg, then `applied`; the console polls for the latter before it shows the holder
/// their units.
#[derive(Serialize)]
pub struct UnitIssuance {
	pub id: String,
	pub service: String,
	/// `user` | `allocation`.
	pub holder_kind: String,
	pub holder_id: String,
	pub units: String,
	pub nav: String,
	pub cost_basis: String,
	/// `queued` | `applied`.
	pub state: String,
	pub created_at: String,
	/// `mint` | `retire`.
	pub source: String,
}

impl From<iv::UnitIssuance> for UnitIssuance {
	fn from(i: iv::UnitIssuance) -> Self {
		Self {
			id: i.id,
			service: i.service,
			holder_kind: i.holder_kind,
			holder_id: i.holder_id,
			units: i.units,
			nav: i.nav,
			cost_basis: i.cost_basis,
			state: i.state,
			created_at: i.created_at.to_string(),
			source: i.source,
		}
	}
}

/// One holder of an allocation's units, as the cap table and the treasury name them:
/// `kind` is `user` (`id` is the concierge user id) or `allocation` (`id` is the holding
/// allocation's slug — the reserved `fee` allocation holding a product's fee class). A
/// screen resolves a `user` through the directory and renders an `allocation` by its
/// slug; it never looks the latter up as a person.
#[derive(Serialize)]
pub struct UnitHolderRef {
	/// `user` | `allocation`.
	pub kind: String,
	pub id: String,
}

impl From<iv::UnitHolderRef> for UnitHolderRef {
	fn from(h: iv::UnitHolderRef) -> Self {
		Self { kind: h.kind, id: h.id }
	}
}

/// One line of a cap table: who, and how many units (decimal). Zero holdings are not
/// listed. `holder` is never `null`: the plane always names one, and a line without a
/// holder would be the very "unit that is nobody's" #245 rules out — it is dropped
/// rather than rendered as a blank row.
#[derive(Serialize)]
pub struct UnitHolding {
	pub holder: UnitHolderRef,
	pub units: String,
}

impl UnitHolding {
	fn from_wire(h: iv::UnitHolding) -> Option<Self> {
		Some(Self {
			holder: h.holder.map(UnitHolderRef::from)?,
			units: h.units,
		})
	}
}

/// A product's cap table (#245): the settled supply and every holder of it, largest
/// first — people and the `fee` allocation alike, each a line. `queued_units` are mints
/// recorded but not yet on the ledger — the console must not pin the cap to
/// `units_outstanding` while it is non-zero.
#[derive(Serialize)]
pub struct UnitHolders {
	pub service: String,
	pub units_outstanding: String,
	pub queued_units: String,
	pub holders: Vec<UnitHolding>,
}

impl From<iv::UnitHolders> for UnitHolders {
	fn from(h: iv::UnitHolders) -> Self {
		Self {
			service: h.service,
			units_outstanding: h.units_outstanding,
			queued_units: h.queued_units,
			holders: h.holders.into_iter().filter_map(UnitHolding::from_wire).collect(),
		}
	}
}

// ── funds (the service currency) ──────────────────────────────────

#[derive(Serialize)]
pub struct Position {
	pub service: String,
	pub units: String,
	pub nav: String,
	pub value: String,
	pub cost_basis: String,
	pub pnl: String,
	pub nav_as_of: String,
}

impl From<iv::Position> for Position {
	fn from(p: iv::Position) -> Self {
		Self {
			service: p.service,
			units: p.units,
			nav: p.nav,
			value: p.value,
			cost_basis: p.cost_basis,
			pnl: p.pnl,
			nav_as_of: p.nav_as_of.to_string(),
		}
	}
}

list_dto! { PositionList from iv::PositionList { positions: Vec<Position> } }

#[derive(Serialize)]
pub struct Subscription {
	pub id: String,
	pub service: String,
	pub cash: String,
	pub nav: String,
	pub units: String,
	/// New beside banking's shape: an invest subscription is a saga across banking's cash
	/// and invest's units, so it answers `pending` and settles later — the cabinet should
	/// not read the answer as minted.
	pub state: String,
	/// Why it failed; empty otherwise.
	pub failure_reason: String,
}

impl From<iv::Subscription> for Subscription {
	fn from(s: iv::Subscription) -> Self {
		Self {
			id: s.id,
			service: s.service,
			cash: s.cash,
			nav: s.nav,
			units: s.units,
			state: s.state,
			failure_reason: s.failure_reason,
		}
	}
}

#[derive(Serialize)]
pub struct Redemption {
	pub id: String,
	pub service: String,
	pub units: String,
	pub nav: String,
	pub cash: String,
	/// `queued`, `settling`, `awaiting_approval` (past banking's automatic ceilings, before
	/// the owners), `completed`, `failed` or `cancelled`.
	pub state: String,
	/// Why it failed after settle, as a machine code; empty otherwise.
	pub failure_reason: String,
}

impl From<iv::Redemption> for Redemption {
	fn from(r: iv::Redemption) -> Self {
		Self {
			id: r.id,
			service: r.service,
			units: r.units,
			nav: r.nav,
			cash: r.cash,
			state: r.state,
			failure_reason: r.failure_reason,
		}
	}
}

list_dto! { RedemptionList from iv::RedemptionList { redemptions: Vec<Redemption> } }

#[derive(Serialize)]
pub struct FundNav {
	pub service: String,
	pub nav: String,
	pub aum: String,
	pub units_outstanding: String,
	pub posted_at: String,
	pub stale: bool,
	pub unit_cap: String,
	/// Units still issuable — already nets off in-flight mints, so a screen offering it
	/// can never offer more than Subscribe accepts.
	pub remaining_capacity: String,
}

impl From<iv::FundNav> for FundNav {
	fn from(f: iv::FundNav) -> Self {
		Self {
			service: f.service,
			nav: f.nav,
			aum: f.aum,
			units_outstanding: f.units_outstanding,
			posted_at: f.posted_at.to_string(),
			stale: f.stale,
			unit_cap: f.unit_cap,
			remaining_capacity: f.remaining_capacity,
		}
	}
}

/// One posted valuation mark. Stamps are strings like every timestamp here.
#[derive(Serialize)]
pub struct NavMark {
	pub nav: String,
	pub aum: String,
	pub posted_at: String,
}

impl From<iv::NavMark> for NavMark {
	fn from(m: iv::NavMark) -> Self {
		Self {
			nav: m.nav,
			aum: m.aum,
			posted_at: m.posted_at.to_string(),
		}
	}
}

/// The caller's holding valued at one instant (units held then × the NAV in force then).
#[derive(Serialize)]
pub struct ParticipationPoint {
	pub at: String,
	pub value: String,
}

impl From<iv::ParticipationPoint> for ParticipationPoint {
	fn from(p: iv::ParticipationPoint) -> Self {
		Self {
			at: p.at.to_string(),
			value: p.value,
		}
	}
}

/// The two series of the performance chart, oldest first. `participation` ends at the
/// window's end priced off the live holding, so its last point equals the position card;
/// `truncated` says the window held more marks than the cap and the OLDEST were dropped.
#[derive(Serialize)]
pub struct FundNavHistory {
	pub allocation: String,
	pub marks: Vec<NavMark>,
	pub participation: Vec<ParticipationPoint>,
	pub truncated: bool,
}

impl From<iv::FundNavHistory> for FundNavHistory {
	fn from(h: iv::FundNavHistory) -> Self {
		Self {
			allocation: h.allocation,
			marks: h.marks.into_iter().map(NavMark::from).collect(),
			participation: h.participation.into_iter().map(ParticipationPoint::from).collect(),
			truncated: h.truncated,
		}
	}
}

// ── fees ──────────────────────────────────────────────────────────────────────

/// A fund's fee terms as the investor should read them. `configured` distinguishes "this
/// product charges nothing" from "we could not tell you" — the client renders no fee
/// section at all in the first case rather than a row of zeros that looks like a policy.
#[derive(Serialize)]
pub struct FeePolicy {
	pub service: String,
	pub configured: bool,
	pub management_bps: u32,
	pub performance_bps: u32,
	pub hurdle_bps: u32,
	pub basis: String,
	pub crystallization: String,
	pub updated_at: String,
	/// Which version of the fund's history these terms are; `0` when unconfigured.
	pub version: u32,
	/// Unix seconds (as a string, like every timestamp here) since which they bind.
	pub effective_from: String,
	/// The change on its way — awaiting the owners or scheduled — or `null`. Shown to
	/// holders and non-holders alike: the terms coming are part of deciding to stay in.
	pub pending: Option<FeePolicyChange>,
}

impl From<iv::FeePolicy> for FeePolicy {
	fn from(p: iv::FeePolicy) -> Self {
		Self {
			service: p.service,
			configured: p.configured,
			management_bps: p.management_bps,
			performance_bps: p.performance_bps,
			hurdle_bps: p.hurdle_bps,
			basis: p.basis,
			crystallization: p.crystallization,
			updated_at: p.updated_at.to_string(),
			version: p.version,
			effective_from: p.effective_from.to_string(),
			pending: p.pending.map(FeePolicyChange::from),
		}
	}
}

/// One row of a fund's fee-policy history — the shape `ScheduleFeePolicy`,
/// `CancelFeePolicyChange` and `AcknowledgeUndeliveredNotices` answer with, and the rows of
/// `/api/admin/fees/changes`. `state` is awaiting_consilium | scheduled | active |
/// superseded | rejected | cancelled; `requirement` is admin | owner_consilium. Timestamps
/// are unix seconds as strings, `"0"` where the moment has not come.
#[derive(Serialize)]
pub struct FeePolicyChange {
	pub id: String,
	pub service: String,
	pub version: u32,
	pub state: String,
	pub management_bps: u32,
	pub performance_bps: u32,
	pub hurdle_bps: u32,
	pub basis: String,
	pub crystallization: String,
	pub effective_from: String,
	pub requirement: String,
	/// The concierge approval (`invest:fee_policy`) this change waits on or waited on;
	/// `null` for an administrator's change, for an owner change not proposed yet, and for
	/// a caller without `AllocationManage`.
	pub consilium_id: Option<String>,
	/// concierge's state of that approval as invest last read it: open | passed |
	/// executed | execution_failed | rejected | vetoed | expired | cancelled | void; `null`
	/// when there is none. `passed` while `state` is still `awaiting_consilium` means the
	/// owners carried it: any owner may still veto it until its delay ends (24 h), and
	/// invest schedules it on its next round after that.
	pub approval_state: Option<String>,
	/// How many holders the proposal told the owners would be notified; `null` when invest
	/// did not propose the change.
	pub approval_holders: Option<u32>,
	pub requested_by: String,
	pub requested_at: String,
	pub scheduled_at: String,
	pub applied_at: String,
	pub reason: String,
	/// Who took responsibility for holders who could not be told — the latest to, when the
	/// record was extended; `null` until someone did.
	pub notices_waived_by: Option<String>,
	/// How the identity plane names `notices_waived_by`. Always `null` from this BFF, which
	/// cannot look another person up (see [`AllocationAccessGrant`]); kept for the type.
	pub notices_waived_by_email: Option<String>,
	/// When; `"0"` until someone did.
	pub notices_waived_at: String,
	/// The holders (concierge user ids) the acknowledgement covers; empty until someone did.
	pub notices_waived_users: Vec<String>,
	/// Notices of this change not yet delivered to a current holder, how many of those the
	/// mailer has given up on — what the pending card says "N holders could not be told"
	/// from — and how many of the given-up ones no acknowledgement covers yet: the ones a
	/// (further) acknowledgement is offered over. Counted only while `scheduled`.
	pub undelivered_notices: u32,
	pub notices_given_up: u32,
	pub notices_unacknowledged: u32,
}

impl From<iv::FeePolicyChange> for FeePolicyChange {
	fn from(c: iv::FeePolicyChange) -> Self {
		Self {
			id: c.id,
			service: c.service,
			version: c.version,
			state: c.state,
			management_bps: c.management_bps,
			performance_bps: c.performance_bps,
			hurdle_bps: c.hurdle_bps,
			basis: c.basis,
			crystallization: c.crystallization,
			effective_from: c.effective_from.to_string(),
			requirement: c.requirement,
			consilium_id: non_empty(c.consilium_id),
			approval_state: non_empty(c.approval_state),
			approval_holders: c.approval_holders,
			requested_by: c.requested_by,
			requested_at: c.requested_at.to_string(),
			scheduled_at: c.scheduled_at.to_string(),
			applied_at: c.applied_at.to_string(),
			reason: c.reason,
			notices_waived_by: non_empty(c.notices_waived_by),
			notices_waived_by_email: None,
			notices_waived_at: c.notices_waived_at.to_string(),
			notices_waived_users: c.notices_waived_users,
			undelivered_notices: c.undelivered_notices,
			notices_given_up: c.notices_given_up,
			notices_unacknowledged: c.notices_unacknowledged,
		}
	}
}

list_dto! { FeePolicyChangeList from iv::FeePolicyChangeList { changes: Vec<FeePolicyChange> } }

/// What a holding owes right now without being charged. `high_water_mark` travels with it
/// because it is the single number that explains why two investors in the same fund at the
/// same price owe different performance fees.
#[derive(Serialize)]
pub struct AccruedFees {
	pub service: String,
	pub configured: bool,
	pub management: String,
	pub performance: String,
	pub debt: String,
	pub total: String,
	pub high_water_mark: String,
}

impl From<iv::AccruedFees> for AccruedFees {
	fn from(a: iv::AccruedFees) -> Self {
		Self {
			service: a.service,
			configured: a.configured,
			management: a.management,
			performance: a.performance,
			debt: a.debt,
			total: a.total,
			high_water_mark: a.high_water_mark,
		}
	}
}

// ── book (the secondary market in an allocation's units) ──────────
// Every amount, size and price crosses as the decimal string invest-core sent; every int64
// timestamp and uint64 revision as a string, like the rest of this wire. The string
// vocabularies (`side`, `kind`, `tif`, `state`, `resolution`) are pinned in
// `invest_contracts::book` and relayed verbatim.

/// One order. `user_id` is populated only on the caller's own orders. `price` is the
/// limit — for a market order, the one invest-core derived from the quote at placement.
/// `avg_fill_price` is empty until the first fill; `reject_reason` is set only on
/// `rejected`, `cancel_reason` only on `cancelled`.
#[derive(Serialize)]
pub struct Order {
	pub id: String,
	pub service: String,
	pub user_id: String,
	/// `buy` | `sell`.
	pub side: String,
	/// `limit` | `market`.
	pub kind: String,
	/// `gtc` | `ioc` | `alo`.
	pub tif: String,
	pub price: String,
	pub size: String,
	pub filled: String,
	pub remaining: String,
	pub avg_fill_price: String,
	pub fee_paid: String,
	/// `open` | `partially_filled` | `filled` | `cancelled` | `rejected`.
	pub state: String,
	pub reject_reason: String,
	/// `user` | `ioc_remainder` | `market_remainder`; empty unless `cancelled`.
	pub cancel_reason: String,
	pub client_order_id: String,
	pub created_at: String,
	pub updated_at: String,
}

impl From<iv::Order> for Order {
	fn from(o: iv::Order) -> Self {
		Self {
			id: o.id,
			service: o.service,
			user_id: o.user_id,
			side: o.side,
			kind: o.kind,
			tif: o.tif,
			price: o.price,
			size: o.size,
			filled: o.filled,
			remaining: o.remaining,
			avg_fill_price: o.avg_fill_price,
			fee_paid: o.fee_paid,
			state: o.state,
			reject_reason: o.reject_reason,
			cancel_reason: o.cancel_reason,
			client_order_id: o.client_order_id,
			created_at: o.created_at.to_string(),
			updated_at: o.updated_at.to_string(),
		}
	}
}

list_dto! { OrderList from iv::OrderList { orders: Vec<Order> } }

/// One fill. On the public tape `user_side`, `order_id` and `fee` are empty; on the
/// caller's own tape they are the caller's side, their order and what they paid as taker
/// (empty when they were the maker). A counterparty is never disclosed on either.
#[derive(Serialize)]
pub struct Trade {
	pub id: String,
	pub service: String,
	pub price: String,
	pub size: String,
	/// `buy` | `sell` — the side that crossed the spread.
	pub taker_side: String,
	pub executed_at: String,
	pub user_side: String,
	pub order_id: String,
	pub fee: String,
}

impl From<iv::Trade> for Trade {
	fn from(t: iv::Trade) -> Self {
		Self {
			id: t.id,
			service: t.service,
			price: t.price,
			size: t.size,
			taker_side: t.taker_side,
			executed_at: t.executed_at.to_string(),
			user_side: t.user_side,
			order_id: t.order_id,
			fee: t.fee,
		}
	}
}

list_dto! { TradeList from iv::TradeList { trades: Vec<Trade> } }

/// One aggregated price level of one side.
#[derive(Serialize)]
pub struct BookLevel {
	pub price: String,
	pub size: String,
	pub orders: u32,
}

impl From<iv::BookLevel> for BookLevel {
	fn from(l: iv::BookLevel) -> Self {
		Self {
			price: l.price,
			size: l.size,
			orders: l.orders,
		}
	}
}

/// The book as of `revision`: `bids` best (highest) first, `asks` best (lowest) first.
/// `last_price`/`last_side` are empty before the first trade; `mid`/`spread` unless both
/// sides have a level; `change_24h` when there is no trade a day back. `nav` is the fund's
/// accounting mark, carried so a ticker can show both prices side by side.
#[derive(Serialize)]
pub struct BookSnapshot {
	pub service: String,
	pub revision: String,
	pub bids: Vec<BookLevel>,
	pub asks: Vec<BookLevel>,
	pub last_price: String,
	/// `buy` | `sell` — the taker's side of the last trade.
	pub last_side: String,
	pub mid: String,
	pub spread: String,
	pub nav: String,
	pub volume_24h: String,
	/// Signed decimal percent, e.g. `-2.5`.
	pub change_24h: String,
	pub as_of: String,
}

impl From<iv::BookSnapshot> for BookSnapshot {
	fn from(s: iv::BookSnapshot) -> Self {
		Self {
			service: s.service,
			revision: s.revision.to_string(),
			bids: s.bids.into_iter().map(BookLevel::from).collect(),
			asks: s.asks.into_iter().map(BookLevel::from).collect(),
			last_price: s.last_price,
			last_side: s.last_side,
			mid: s.mid,
			spread: s.spread,
			nav: s.nav,
			volume_24h: s.volume_24h,
			change_24h: s.change_24h,
			as_of: s.as_of.to_string(),
		}
	}
}

/// One OHLCV bucket; `time` is the bucket's start. Empty buckets are omitted by the
/// invest-core — a chart carries the last close forward itself.
#[derive(Serialize)]
pub struct Candle {
	pub time: String,
	pub open: String,
	pub high: String,
	pub low: String,
	pub close: String,
	pub volume: String,
}

impl From<iv::Candle> for Candle {
	fn from(c: iv::Candle) -> Self {
		Self {
			time: c.time.to_string(),
			open: c.open,
			high: c.high,
			low: c.low,
			close: c.close,
			volume: c.volume,
		}
	}
}

list_dto! {
	CandleList from iv::CandleList as l {
		candles: Vec<Candle>,
		service: String = l.service,
		/// `1m` | `5m` | `15m` | `1h` | `4h` | `1d`.
		resolution: String = l.resolution,
	}
}

/// One allocation's trading terms. `updated_at` is `"0"` when the product has no policy
/// row — the defaults, and a closed book.
#[derive(Serialize)]
pub struct BookPolicy {
	pub service: String,
	pub book_open: bool,
	pub taker_fee_bps: u32,
	pub price_tick: String,
	pub lot_size: String,
	pub market_slippage_bps: u32,
	/// The operator acknowledged that the units trade unbacked by fund cash; the terminal
	/// tells buyers the book is their only exit.
	pub allow_unbacked_trading: bool,
	pub updated_at: String,
	/// The taker-fee rise scheduled but not yet bound; `taker_fee_bps` is the fee in force.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub pending_taker_fee_rise: Option<PendingTakerFeeRise>,
}

/// A taker-fee rise on its way. Instants are unix seconds as strings, like `updated_at`.
#[derive(Serialize)]
pub struct PendingTakerFeeRise {
	pub id: String,
	pub taker_fee_bps: u32,
	pub effective_from: String,
	pub requested_at: String,
}

impl From<iv::PendingTakerFeeRise> for PendingTakerFeeRise {
	fn from(r: iv::PendingTakerFeeRise) -> Self {
		Self {
			id: r.id,
			taker_fee_bps: r.taker_fee_bps,
			effective_from: r.effective_from.to_string(),
			requested_at: r.requested_at.to_string(),
		}
	}
}

impl From<iv::BookPolicy> for BookPolicy {
	fn from(p: iv::BookPolicy) -> Self {
		Self {
			service: p.service,
			book_open: p.book_open,
			taker_fee_bps: p.taker_fee_bps,
			price_tick: p.price_tick,
			lot_size: p.lot_size,
			market_slippage_bps: p.market_slippage_bps,
			allow_unbacked_trading: p.allow_unbacked_trading,
			updated_at: p.updated_at.to_string(),
			pending_taker_fee_rise: p.pending_taker_fee_rise.map(Into::into),
		}
	}
}

/// One frame of the live book feed. `orders_revision` is the book revision at which the
/// CALLER's own orders on this allocation last changed (`"0"` = never): the client
/// refetches `/api/book/orders` only when it moves.
#[derive(Serialize)]
pub struct BookEvent {
	/// `None` only if invest-core sent a frame without one, which the contract does not allow;
	/// kept optional rather than invented so a client sees the gap instead of an empty book.
	pub snapshot: Option<BookSnapshot>,
	/// The latest public trades, newest first.
	pub trades: Vec<Trade>,
	pub orders_revision: String,
}

impl From<iv::BookEvent> for BookEvent {
	fn from(e: iv::BookEvent) -> Self {
		Self {
			snapshot: e.snapshot.map(BookSnapshot::from),
			trades: e.trades.into_iter().map(Trade::from).collect(),
			orders_revision: e.orders_revision.to_string(),
		}
	}
}

/// An allocation's cash claim: `posted` is settled, `reserved` is spoken for, `available`
/// is the difference. The shape banking's treasury used, kept for the Revenue screen.
#[derive(Default, Serialize)]
pub struct AllocationClaim {
	pub posted: String,
	pub available: String,
	pub reserved: String,
}

/// One allocation as the Revenue screen reads it (#245): name, cash, supply, price and who
/// holds it. Banking served this as `GetFundRevenue`; invest-core has no such RPC, so the
/// route assembles it from `GetAllocation`, `GetFundNav` and `ListUnitHolders` of the
/// reserved `fee` allocation ([`Self::revenue`]). `nav_posted_at` is `"0"` while nothing
/// under it has been marked (a reserved allocation's price is computed, never posted).
#[derive(Serialize)]
pub struct AllocationTreasury {
	pub service: String,
	pub title: String,
	/// `hidden` | `view` | `invest`.
	pub access: String,
	pub claim: AllocationClaim,
	pub units_outstanding: String,
	pub nav: String,
	pub nav_posted_at: String,
	pub holders: Vec<UnitHolding>,
	/// banking's own treasury (D17). Never set here: it is banking's figure and lives in the
	/// central admin; the field stays so the cabinet's type is unchanged.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub banking_treasury: Option<AllocationClaim>,
}

impl AllocationTreasury {
	/// The reserved allocation's line. Its cash is what invest-core prices it from — the
	/// `alloc:<slug>` balance banking reports, which `GetFundNav` answers as `aum` — and
	/// none of it is reserved: invest holds no approvals over it (§3.9).
	pub fn revenue(allocation: iv::Allocation, nav: iv::FundNav, holders: iv::UnitHolders) -> Self {
		let cash = if nav.aum.is_empty() { "0".to_owned() } else { nav.aum };
		Self {
			service: allocation.service,
			title: allocation.title,
			access: allocation.access,
			claim: AllocationClaim {
				posted: cash.clone(),
				available: cash,
				reserved: "0".into(),
			},
			units_outstanding: holders.units_outstanding,
			nav: nav.nav,
			nav_posted_at: nav.posted_at.to_string(),
			holders: holders.holders.into_iter().filter_map(UnitHolding::from_wire).collect(),
			banking_treasury: None,
		}
	}
}

/// One queued redemption in the Valuation screen's queue.
#[derive(Serialize)]
pub struct RedemptionQueueItem {
	pub redemption_id: String,
	pub user_id: String,
	pub email: String,
	pub service: String,
	pub units: String,
	pub created_at: String,
}

#[derive(Serialize)]
pub struct RedemptionQueue {
	pub items: Vec<RedemptionQueueItem>,
}

impl From<iv::RedemptionQueue> for RedemptionQueue {
	fn from(q: iv::RedemptionQueue) -> Self {
		Self {
			items: q
				.items
				.into_iter()
				.map(|i| RedemptionQueueItem {
					redemption_id: i.redemption_id,
					user_id: i.user_id,
					// invest keeps no copy of identity; the console names the holder by id.
					email: String::new(),
					service: i.service,
					units: i.units,
					created_at: i.created_at.to_string(),
				})
				.collect(),
		}
	}
}

/// A string the proto leaves empty when the field does not apply.
fn non_empty(value: String) -> Option<String> {
	(!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// An issuance crosses with its `source` intact: the console tells a mint (supply
	/// grew) from a retirement (it shrank) by this field alone.
	#[test]
	fn an_issuance_carries_its_source_and_holder() {
		let issuance = UnitIssuance::from(iv::UnitIssuance {
			id: "7c1e".into(),
			service: "service_arb".into(),
			holder_kind: "user".into(),
			holder_id: "d4d6".into(),
			units: "13000".into(),
			nav: "1.25".into(),
			cost_basis: "16250".into(),
			state: "queued".into(),
			created_at: 1_700_000_000,
			source: "retire".into(),
		});
		assert_eq!(
			(issuance.holder_kind.as_str(), issuance.holder_id.as_str(), issuance.source.as_str(), issuance.state.as_str()),
			("user", "d4d6", "retire", "queued")
		);
		assert_eq!(
			(issuance.units.as_str(), issuance.cost_basis.as_str(), issuance.created_at.as_str()),
			("13000", "16250", "1700000000")
		);
	}

	/// A redemption waiting on the owners, and one they turned down, reach the cabinet as
	/// invest-core named them.
	#[test]
	fn a_redemption_carries_its_state_and_failure_reason() {
		let redemption = |state: &str, failure_reason: &str| {
			Redemption::from(iv::Redemption {
				id: "r1".into(),
				service: "service_arb".into(),
				units: "10".into(),
				nav: "1.5".into(),
				cash: "15".into(),
				state: state.into(),
				failure_reason: failure_reason.into(),
			})
		};
		let awaiting = redemption("awaiting_approval", "");
		assert_eq!((awaiting.state.as_str(), awaiting.failure_reason.as_str()), ("awaiting_approval", ""));
		let rejected = redemption("failed", "approval_rejected");
		assert_eq!((rejected.state.as_str(), rejected.failure_reason.as_str()), ("failed", "approval_rejected"));
	}

	/// The cap table crosses line by line, in invest-core's order, with nothing summed.
	#[test]
	fn the_cap_table_keeps_invest_cores_order_and_sums_nothing() {
		let holders = UnitHolders::from(iv::UnitHolders {
			service: "quy-nhon".into(),
			units_outstanding: "13250".into(),
			queued_units: "100".into(),
			holders: vec![
				iv::UnitHolding {
					holder: Some(iv::UnitHolderRef {
						kind: "user".into(),
						id: "d4d6".into(),
					}),
					units: "13000".into(),
				},
				iv::UnitHolding {
					holder: Some(iv::UnitHolderRef {
						kind: "allocation".into(),
						id: "fee".into(),
					}),
					units: "250".into(),
				},
			],
		});
		assert_eq!((holders.units_outstanding.as_str(), holders.queued_units.as_str()), ("13250", "100"));
		assert_eq!(
			holders
				.holders
				.iter()
				.map(|h| (h.holder.kind.as_str(), h.holder.id.as_str(), h.units.as_str()))
				.collect::<Vec<_>>(),
			[("user", "d4d6", "13000"), ("allocation", "fee", "250")]
		);
		let json = serde_json::to_value(&holders).unwrap();
		for retired in ["company_units", "fee_units", "investor_units"] {
			assert!(json.get(retired).is_none(), "`{retired}` is not on the wire any more");
		}
	}
}
