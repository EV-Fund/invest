// The investor's funds surface — `/api/funds/*` — as the invest BFF emits it
// (`cabinet/backend/src/dto.rs`, the `funds` and `fees` sections). Hand-written against
// those structs: snake_case, every 64-bit value and timestamp a string, every field present.
// A field added to a DTO there is added here in the same change. The shapes the console
// shares with this surface (`FundNav`, `Redemption`, `FeePolicy`) live in `./admin`.

import type { Redemption } from "./admin";

/** Decimal as the BFF renders it — never do arithmetic on it raw (`shared/lib/money.ts`). */
type Decimal = string;
/** Unix seconds, as a string. */
type Timestamp = string;

/** The caller's holding in one fund. */
export interface Position {
  service: string;
  /** Units held free in the holding. */
  units: Decimal;
  /** Current NAV per unit, USDT. */
  nav: Decimal;
  /** `units × nav`, USDT. */
  value: Decimal;
  /** Net cash invested at average cost, USDT. */
  cost_basis: Decimal;
  /** `value − cost_basis`, signed. */
  pnl: Decimal;
  /** When the NAV was marked; `"0"` for the seed NAV. */
  nav_as_of: Timestamp;
}

export interface PositionList {
  positions: Position[];
}

/** One posted valuation mark. */
export interface NavMark {
  nav: Decimal;
  aum: Decimal;
  posted_at: Timestamp;
}

/** The caller's holding valued at one instant. */
export interface ParticipationPoint {
  at: Timestamp;
  value: Decimal;
}

/**
 * The performance chart's two series, oldest first (`GET /api/funds/nav/history`).
 * `truncated`: the window held more marks than the cap and the oldest were dropped.
 */
export interface FundNavHistory {
  allocation: string;
  marks: NavMark[];
  participation: ParticipationPoint[];
  truncated: boolean;
}

/**
 * A subscription as the saga records it (`invest_domain::subscriptions::SubscriptionState`):
 * `reserving` and `reserved` are in flight — banking holds the cash, the relay mints and
 * captures — `settled` and `failed` are terminal. A wire value this build does not name is
 * kept as a string rather than refused.
 */
export type SubscriptionState = "reserving" | "reserved" | "settled" | "failed";

export interface Subscription {
  id: string;
  service: string;
  cash: Decimal;
  nav: Decimal;
  units: Decimal;
  state: SubscriptionState | (string & {});
  /** A machine code — banking's refusal (`mandate_required`, …) or invest's own; `""` unless `failed`. */
  failure_reason: string;
}

/** What a subscription means to the reader: units in hand, on their way, or none. */
export type SubscriptionPhase = "settled" | "pending" | "failed";

/** An unknown state reads as pending: nothing says it failed, and nothing says it minted. */
export function subscriptionPhase(state: Subscription["state"]): SubscriptionPhase {
  if (state === "settled") return "settled";
  if (state === "failed") return "failed";
  return "pending";
}

export interface RedemptionList {
  redemptions: Redemption[];
}

/** What the caller's holding owes right now without being charged. */
export interface AccruedFees {
  service: string;
  configured: boolean;
  management: Decimal;
  performance: Decimal;
  debt: Decimal;
  total: Decimal;
  high_water_mark: Decimal;
}
