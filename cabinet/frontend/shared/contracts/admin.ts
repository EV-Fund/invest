// Admin-console response types — the shapes the BFF's `/api/admin/*` handlers emit, and
// the session principal. Hand-written against the BFF's DTOs (`cabinet/backend/src/dto.rs`,
// `routes/auth.rs::SessionInfo`): snake_case, 64-bit values as strings — except the session,
// which is camelCase. Import these from `@/shared/contracts/admin`.

export interface SessionUser {
  userId: string;
  email: string;
  /** Whether the mailbox is proven. A KYC level above 0 needs it. */
  emailVerified: boolean;
  /** A handle beside the email; absent when the account has none. */
  username?: string;
  status: string;
  /** Derived by the BFF from the seat's `invest:*` permissions: `admin` with
   *  `invest:allocation:manage`, `operator` with only `invest:valuation:post`, else
   *  `investor`. A label, not a gate — gates read `permissions`. */
  role: "admin" | "operator" | "investor" | (string & {});
  /** `invest:allocation:manage` or `invest:valuation:post`: the console is shown. */
  isAdmin: boolean;
  /** Always `false` from the invest BFF: break-glass is banking's and concierge's. */
  roleIsBreakGlass: boolean;
}

export interface SessionInfo {
  authenticated: boolean;
  user?: SessionUser;
  /**
   * What the caller's seat holds, concrete — a guest's set when nobody is signed in. What a
   * page shows is decided from this (`shared/config/route-permissions.ts`); the BFF still
   * decides what runs.
   */
  permissions: string[];
}

/** An allocation's cash claim off one ledger balance: `posted` is settled, `reserved` is
 *  spoken for by approved payments out of it, `available` is the difference. */
export interface AllocationClaim {
  posted: string;
  available: string;
  reserved: string;
}

/** The Revenue screen's answer (`GET /api/admin/revenue`, #245): the reserved `fee`
 *  allocation's name, cash, supply, price and who holds it. The BFF assembles it from
 *  invest-core's allocation, NAV and cap table; `claim.reserved` is always `"0"` (invest
 *  holds no approvals over it) and `nav_posted_at` is `"0"` while nothing under it has been
 *  marked. banking's own treasury (D17) is never set here — it is banking's figure. */
export interface AllocationTreasury {
  service: string;
  title: string;
  access: AllocationAccessLevel;
  claim: AllocationClaim;
  units_outstanding: string;
  nav: string;
  nav_posted_at: string;
  holders: UnitHolding[];
}

// ── allocations (the registry of investable products) ───────────────────────────

/** `draft` — registered, takes no money yet. `open` — subscribe + redeem. `closed` —
 *  redeem only, so winding a product down never traps an investor. */
export type AllocationState = "draft" | "open" | "closed";

/** The picture an operator chose for a product, in the wire's snake_case.
 *
 *  A closed set mirrored from `icon::ALL` in `contracts/src/allocation.rs`; the hub's CHECK
 *  constraint and the BFF both reject anything outside it (the BFF answers 400), so this
 *  union and that list have to be edited together. `fund` is the hub's default. */
export type AllocationIcon = "fund" | "real_estate" | "trading" | "yield" | "venture" | "treasury" | "commodity" | "credit" | "index" | "arbitrage";

/** `hidden` — not in the caller's catalog at all. `view` — listed, `Subscribe` refused.
 *  `invest` — listed and open to new money (subject to `state` and the unit cap). Ranked
 *  `hidden < view < invest`, mirroring `contracts::allocation::access::ALL`. */
export type AllocationAccessLevel = "hidden" | "view" | "invest";

/** The levels a per-investor grant may carry — everything above the `hidden` floor.
 *  Mirrors `contracts::allocation::access::GRANTABLE`. */
export type AllocationGrantLevel = "view" | "invest";

/** What stands behind a product's units. `cash` — paid for into the fund's claim, and a
 *  redemption pays out of it. `in_kind` — the units stand for an asset the holders own
 *  off-platform; the fund holds no cash for them, so `Redeem` is refused (412) and holders
 *  exit through the book. The hub flips a product to `in_kind` on its first in-kind mint;
 *  only `SetAllocationBacking` takes it back. A registration lands on `cash`. */
export type AllocationBacking = "cash" | "in_kind";

// ── fees ─────────────────────────────────────────────────────────────────────

/** The five-field schedule a fund charges by — one statement, never patched a leg at a
 *  time. Rates are basis points; `basis` and `crystallization` are the money plane's closed
 *  vocabularies (`shared/lib/fee-terms.ts` has the words for them). */
export interface FeeTerms {
  management_bps: number;
  performance_bps: number;
  hurdle_bps: number;
  basis: string;
  crystallization: string;
}

/** A fund's terms in force. `configured` false means no policy row exists, which is a
 *  different fact from a policy whose rates are zero — the first can never charge. */
export interface FeePolicy extends FeeTerms {
  service: string;
  configured: boolean;
  /** Unix seconds as a string; the moment the row was last promoted. */
  updated_at: string;
  /** Which version of the fund's history these terms are; `0` when unconfigured. */
  version: number;
  /** Unix seconds as a string since which they bind; `"0"` when unconfigured. */
  effective_from: string;
  /** The change on its way — awaiting the owners or scheduled — or absent. Shown to
   *  holders and non-holders alike: the terms coming are part of deciding to stay in. */
  pending?: FeePolicyChange | null;
}

export interface FeePolicyList {
  policies: FeePolicy[];
}

/** Where a change stands. Typed as an open string like every other lifecycle on this wire
 *  (`./governance`): a member the plane grows later must render as its wire word, not
 *  break the build. The names this build knows are `admin.feeChange.state.*`. */
export type FeePolicyChangeState = "awaiting_consilium" | "scheduled" | "active" | "superseded" | "rejected" | "cancelled" | string;

/** Where the owners' decision on a change stands in concierge. The names this build knows
 *  are `admin.fees.approval.state.*`. */
export type ApprovalState = "open" | "passed" | "executed" | "execution_failed" | "rejected" | "vetoed" | "expired" | "cancelled" | "void" | string;

/** Who had to agree: one `AllocationManage` holder, or the platform owners through
 *  concierge's approvals (`invest:fee_policy`). The wire word keeps its banking-era name. */
export type FeePolicyRequirement = "admin" | "owner_consilium" | string;

/**
 * One row of a fund's fee-policy history — what `POST /api/admin/fees/policy` and
 * `/policy/cancel` answer with, and the rows of `GET /api/admin/fees/changes`.
 *
 * Timestamps are unix seconds as strings, `"0"` where the moment has not come:
 * `effective_from` is provisional while the change awaits the owners (the 24h notice is
 * counted from their approval, not from the request) and fixed once scheduled.
 */
export interface FeePolicyChange extends FeeTerms {
  id: string;
  service: string;
  version: number;
  state: FeePolicyChangeState;
  effective_from: string;
  requirement: FeePolicyRequirement;
  /** The concierge approval (`invest:fee_policy`) this change waits on or waited on; the
   *  field keeps its banking-era name. `null` for an administrator's change, for an owner
   *  change not proposed yet (concierge was out of reach; invest proposes it on a later
   *  round), and for a caller without `AllocationManage`. */
  consilium_id?: string | null;
  /** concierge's state of that approval as invest last read it. `passed` while `state` is
   *  still `awaiting_consilium` means the owners carried it, but any owner may veto it until
   *  the 24h delay ends; invest schedules it after that. `null` when there is no approval.
   *  An open string for the reason `FeePolicyChangeState` gives. */
  approval_state: ApprovalState | null;
  /** How many holders the proposal told the owners would be notified, counted when it was
   *  proposed; `null` when invest did not propose the change. */
  approval_holders: number | null;
  /** Unix seconds as a string: when the owners' veto window of a passed approval ends and
   *  invest may schedule the change. `null` until invest has read a passed proposal, on a
   *  change invest did not propose, and for a caller without `AllocationManage`. */
  approval_executable_at: string | null;
  requested_by: string;
  requested_at: string;
  scheduled_at: string;
  applied_at: string;
  reason: string;
  /**
   * The operator who took responsibility for the holders who could not be told
   * (`/policy/acknowledge-notices`) — the latest to, when a later act extended the record;
   * `null` until someone did. Operators only — blank for everyone else even once given.
   */
  notices_waived_by: string | null;
  /**
   * The same operator by the email the directory knows them by — what a reader is shown,
   * the id being a fact for the record and a tooltip. Always present on the wire; `null`
   * when nobody acknowledged, when the reader is not an operator, or when the directory
   * cannot name the id (a directory outage never fails the page).
   */
  notices_waived_by_email: string | null;
  /** Unix seconds as a string the (latest) acknowledgement was given; `"0"` until someone gave it. */
  notices_waived_at: string;
  /** The banking user ids whose notice the mailer had given up on by that moment — exactly
   *  the holders the change binds over untold. A holder given up on later is not among them
   *  until a further act adds them. Operators only. */
  notices_waived_users: string[];
  /**
   * Notices not yet delivered to a CURRENT holder, how many of those the mailer has given
   * up on, and how many of the given-up ones the acknowledgement does not cover (all of
   * them while there is none) — the holders a (further) act would name, so it is worth
   * offering exactly while this is non-zero. Counted only while `scheduled` — `0` in every
   * other state, and `0` for everyone but operators — so a non-zero figure is always about
   * a change still on its way.
   */
  undelivered_notices: number;
  notices_given_up: number;
  notices_unacknowledged: number;
}

export interface FeePolicyChangeList {
  changes: FeePolicyChange[];
}

/** `POST /api/admin/fees/policy`. `effective_from` is unix seconds as a NUMBER — the BFF
 *  reads it with `as_i64` — and `0` asks for the earliest moment the notice allows.
 *  `reason` is required by the hub exactly when the owners must approve. */
export interface ScheduleFeePolicyRequest extends FeeTerms {
  service: string;
  effective_from: number;
  reason: string;
}

/** `POST /api/admin/fees/policy/cancel`. Idempotent on an already-cancelled change. */
export interface CancelFeePolicyChangeRequest {
  service: string;
  change_id: string;
}

/** `POST /api/admin/fees/policy/acknowledge-notices`. Idempotent on a change already
 *  acknowledged (the first record stands). 409 when the change is not `scheduled` or every
 *  notice has been delivered; 403 for a caller who is neither the requester nor an owner. */
export interface AcknowledgeUndeliveredNoticesRequest {
  service: string;
  change_id: string;
}

/** Uncollected fee units in one fund. `value` is what a settlement would convert them to
 *  at the current NAV. */
export interface FeeShares {
  service: string;
  units: string;
  value: string;
}

export interface FeeSettlement {
  service: string;
  units: string;
  nav: string;
  cash: string;
}

/** One charge against one holding. `charged_cash` falls short of
 *  `management + performance` exactly when the rest went to `debt_carried`. */
export interface FeeAssessment {
  service: string;
  trigger: string;
  nav: string;
  management: string;
  performance: string;
  debt_opening: string;
  charged_units: string;
  charged_cash: string;
  debt_carried: string;
  high_water_mark: string;
  assessed_at: string;
}

export interface FeeAssessmentList {
  assessments: FeeAssessment[];
}

export interface Allocation {
  service: string;
  title: string;
  summary: string;
  state: AllocationState;
  /** Unix seconds; `"0"` on the write responses, which return the clock-free aggregate. */
  created_at: string;
  updated_at: string;
  /** Authorised unit supply, decimal. Subscribe refuses a mint that would pass it. */
  unit_cap: string;
  /**
   * Optional on READ, and deliberately so rather than as a hedge: `allocationsResource`
   * carries `persist: true`, so a returning user's first frame is rehydrated from a
   * sessionStorage object serialised before this field existed. Optionality alone is not
   * the fix — render it through `ProductIcon` (`@/shared/ui/icons/products`), which lands
   * both a missing value and one newer than this build on `fund`.
   *
   * Required on WRITE by this client's own choice, not by the hub — see
   * `AllocationWrite`. On the wire the field carries presence, so an update that omits
   * it changes nothing.
   */
  icon?: AllocationIcon;
  /**
   * access
   *
   * The product's default access level — what a caller with no grant holds. A
   * registration lands on `view`: listed, locked. Optional on READ for the same reason
   * `icon` is above — `allocationsResource` persists to sessionStorage, and a returning
   * user's first frame may rehydrate an object serialised before this field existed.
   */
  access?: AllocationAccessLevel;
  /**
   * caller_access
   *
   * The level THIS caller effectively holds — `max(access, their own grant)`. The
   * subscribe control and the "locked" badge key off this, never off `access` or `state`
   * alone: an `open` product at `view` is visible and not investable. Honest for an
   * `AllocationManage` holder too — their permission does not make them an investor. Same
   * optionality caveat as `access`.
   */
  caller_access?: AllocationAccessLevel;
  /**
   * backing
   *
   * Optional on READ for the same reason `access` is — a persisted catalog object may
   * predate the field. Absent reads as `cash`, the hub's own default for an unset product:
   * `backingOf` in `views/admin/allocations/lib/backing.ts` is the one place that folds it.
   */
  backing?: AllocationBacking;
}

export interface AllocationList {
  allocations: Allocation[];
}

/** One investor raised above a product's default access level. */
export interface AllocationAccessGrant {
  service: string;
  /**
   * The investor — the id the console carries: concierge-first, banking when no mirror
   * exists. Revoke sends it back exactly as received; the BFF owns the mapping.
   */
  user_id: string;
  /**
   * The investor's email, for display. `null` when the hub could not match `user_id` to
   * an identity (a banking-only id with no concierge mirror, or a mirror that has since
   * gone); absent on a payload serialised before the field existed. Fall back to the id.
   */
  email?: string | null;
  level: AllocationGrantLevel;
  /** The `AllocationManage` holder who granted it — console-facing id, same rule as `user_id`. */
  granted_by: string;
  /** Unix seconds. */
  granted_at: string;
}

export interface AllocationAccessGrantList {
  grants: AllocationAccessGrant[];
}

// ── in-kind issuance (units with no cash leg) ───────────────────────────────────

/** Who holds units: a person (`user`, `id` is the banking user id) or the reserved `fee`
 *  allocation (`allocation`, `id` is its slug — never a user to look up). The company is
 *  not a holder (#245). */
export type UnitHolderKind = "user" | "allocation";

/** One holder of an allocation's units, as the cap table and the treasury name them. */
export interface UnitHolderRef {
  kind: UnitHolderKind;
  id: string;
}

/** One line of a cap table: who, and how many units (decimal). A zero holding is not
 *  listed, and a line never lacks a holder — the BFF drops one rather than render it. */
export interface UnitHolding {
  holder: UnitHolderRef;
  units: string;
}

/** `queued` until the relay posts the mint, then `applied`. A `queued` row is real — the
 *  hub has accepted it — but the units are not on the ledger yet, so a holders read
 *  taken straight after the POST still shows the supply as it was. */
export type UnitIssuanceState = "queued" | "applied";

/** Where the units came from — or went: `mint` (`/allocations/issue` or an executed
 *  holder grant — the supply grew by `units`) or `retire` (`/allocations/retire` — burned
 *  out of a holder, the supply SHRANK by `units`). Always populated; a row that predates
 *  the field reads as `mint`. */
export type UnitIssuanceSource = "mint" | "retire";

/** One in-kind issuance — a mint or a retirement — as the hub recorded it. */
export interface UnitIssuance {
  id: string;
  service: string;
  holder_kind: UnitHolderKind;
  /** The banking user id for `user`; the holding allocation's slug for `allocation`. */
  holder_id: string;
  units: string;
  /** Decimal USDT per unit the mint was recorded at. */
  nav: string;
  /** Decimal USDT the holder is deemed to have paid — what P&L and the management fee
   *  are measured from. */
  cost_basis: string;
  state: UnitIssuanceState;
  /** Unix seconds. */
  created_at: string;
  source: UnitIssuanceSource;
}

/** The retirement body, exactly as the BFF reads it (`POST /api/admin/allocations/
 *  retire`). The mirror of a mint: units are burned out of ONE person — the reserved
 *  allocations are never burned from here — and the supply shrinks by them; no cash moves
 *  either way. `cost_basis` follows the mint's rule (absent = `units × NAV`, an empty
 *  string is malformed). `force` is the operator's explicit override to burn out of a
 *  live (draft or open) product — omitted, not `false`, when the product is closed and
 *  needs none. `views/admin/allocations/lib/retire.ts` is the one place that builds it. */
export interface RetireUnitsBody {
  service: string;
  /** Whose units are burnt — the id the console carries. */
  user_id: string;
  /** Decimal units, > 0, at most what the holder has available. */
  units: string;
  /** Decimal USDT of book value written off; omitted = `units × NAV`. */
  cost_basis?: string;
  /** 1..64 chars, in the same per-product key space as `/allocations/issue`. */
  idempotency_key: string;
  force?: true;
}

/** A product's cap table (#245): the settled supply and every holder of it, largest first
 *  — people and the `fee` allocation alike, each a line. Σ `holders[].units` =
 *  `units_outstanding` is the supply invariant the hub checks; it is read here, never
 *  derived. All units decimal. */
export interface UnitHolders {
  service: string;
  units_outstanding: string;
  /** In-kind mints the relay has accepted but not yet posted — not part of
   *  `units_outstanding` until they land, so a cap pinned to the settled figure alone
   *  would leave them to land above it. */
  queued_units: string;
  holders: UnitHolding[];
}

// ── valuation + redemptions ─────────────────────────────────────────────────────
export interface FundNav {
  service: string;
  nav: string;
  aum: string;
  /** The settled supply — the denominator NAV is derived against. */
  units_outstanding: string;
  posted_at: string;
  stale: boolean;
  unit_cap: string;
  /** Units still issuable. Already nets off in-flight mints, so offering this figure can
   *  never offer more than the hub will accept. */
  remaining_capacity: string;
}

export interface RedemptionQueueItem {
  redemption_id: string;
  user_id: string;
  email: string;
  service: string;
  units: string;
  created_at: string;
}

export interface RedemptionQueue {
  items: RedemptionQueueItem[];
}

/**
 * A redemption as the saga records it: `queued`, `settling` and `awaiting_approval` (past
 * banking's automatic ceilings, before the owners) are in flight; `completed`, `failed` and
 * `cancelled` are terminal. A wire value this build does not name is kept as a string.
 */
export type RedemptionState = "queued" | "settling" | "awaiting_approval" | "completed" | "failed" | "cancelled";

export interface Redemption {
  id: string;
  service: string;
  units: string;
  nav: string;
  cash: string;
  state: RedemptionState | (string & {});
  /** A machine code (`insufficient_funds`, `approval_rejected`, …); `""` unless `failed`. */
  failure_reason: string;
}
