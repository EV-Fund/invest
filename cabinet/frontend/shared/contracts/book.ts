// The secondary market in an allocation's units — invest-core's central limit order book,
// bridged by the BFF under `/api/book/*`. Hand-written against the BFF's DTOs
// (`cabinet/backend/src/dto.rs`, the `book` section); the socket frame is the BFF's own
// envelope around them.
//
// Every amount, price, size, timestamp and revision crosses as a STRING (the BFF renders
// 64-bit values that way) — never do arithmetic on them raw.

/**
 * One order. `user_id` is set only on the caller's own orders. `price` is the limit — for a
 * market order, the one invest-core derived from the quote at placement. `avg_fill_price`
 * is `""` until the first fill; `reject_reason` is set only on `rejected`, `cancel_reason`
 * only on `cancelled`.
 */
export interface Order {
  id: string;
  service: string;
  user_id: string;
  side: OrderSide | (string & {});
  kind: OrderKind | (string & {});
  tif: OrderTif | (string & {});
  price: string;
  size: string;
  filled: string;
  remaining: string;
  avg_fill_price: string;
  fee_paid: string;
  /** `open` | `partially_filled` | `filled` | `cancelled` | `rejected`. */
  state: string;
  reject_reason: string;
  cancel_reason: OrderCancelReason | "" | (string & {});
  client_order_id: string;
  created_at: string;
  updated_at: string;
}

export interface OrderList {
  orders: Order[];
}

/**
 * One fill. On the public tape `user_side`, `order_id` and `fee` are `""`; on the caller's
 * own tape they are the caller's side, their order and what they paid as taker (`""` when
 * they were the maker). A counterparty is never disclosed on either.
 */
export interface Trade {
  id: string;
  service: string;
  price: string;
  size: string;
  /** The side that crossed the spread. */
  taker_side: OrderSide | (string & {});
  executed_at: string;
  user_side: OrderSide | "" | (string & {});
  order_id: string;
  fee: string;
}

export interface TradeList {
  trades: Trade[];
}

/** One aggregated price level of one side. */
export interface BookLevel {
  price: string;
  size: string;
  orders: number;
}

/**
 * The book as of `revision`: `bids` best (highest) first, `asks` best (lowest) first.
 * `last_price`/`last_side` are `""` before the first trade; `mid`/`spread` unless both sides
 * have a level; `change_24h` when there is no trade a day back. `nav` is the fund's
 * accounting mark, carried so a ticker can show both prices side by side.
 */
export interface BookSnapshot {
  service: string;
  revision: string;
  bids: BookLevel[];
  asks: BookLevel[];
  last_price: string;
  last_side: OrderSide | "" | (string & {});
  mid: string;
  spread: string;
  nav: string;
  volume_24h: string;
  /** Signed decimal percent, e.g. `-2.5`. */
  change_24h: string;
  as_of: string;
}

/** One OHLCV bucket; `time` is its start. Empty buckets are omitted — a chart carries the
 *  last close forward itself. */
export interface Candle {
  time: string;
  open: string;
  high: string;
  low: string;
  close: string;
  volume: string;
}

export interface CandleList {
  candles: Candle[];
  service: string;
  resolution: CandleResolution | (string & {});
}

/** One allocation's trading terms. `updated_at` is `"0"` when the product has no policy row
 *  — the defaults, and a closed book. */
export interface BookPolicy {
  service: string;
  book_open: boolean;
  taker_fee_bps: number;
  price_tick: string;
  lot_size: string;
  market_slippage_bps: number;
  /** The operator acknowledged that the units trade unbacked by fund cash; the terminal
   *  tells buyers the book is their only exit. */
  allow_unbacked_trading: boolean;
  updated_at: string;
}

/** `buy` takes units for cash, `sell` the reverse. The wire's own words. */
export type OrderSide = "buy" | "sell";

/** A `limit` rests at its price; a `market` crosses whatever is there, capped by the
 *  policy's slippage. */
export type OrderKind = "limit" | "market";

/** `gtc` rests until filled or cancelled, `ioc` fills what it can and cancels the rest,
 *  `alo` (post-only) is refused rather than crossing the spread. */
export type OrderTif = "gtc" | "ioc" | "alo";

/** Why a `cancelled` order is cancelled — `""` on every other state. `user` is the owner's
 *  own cancel; the other two are the hub cancelling what an IOC limit / a market order
 *  could not fill at once, which is how a cancelled order can still carry `filled > 0`. */
export type OrderCancelReason = "user" | "ioc_remainder" | "market_remainder";

/** The candle buckets the hub aggregates; the wire's own vocabulary. */
export type CandleResolution = "1m" | "5m" | "15m" | "1h" | "4h" | "1d";

/** The place-order body, exactly as `POST /api/book/orders` reads it. `price` is absent
 *  for a market order (the hub derives the limit from the quote); `tif` is absent for one
 *  too, since a market order is by nature immediate-or-cancel. */
export interface PlaceOrderBody {
  service: string;
  side: OrderSide;
  kind: OrderKind;
  tif?: OrderTif;
  price?: string;
  size: string;
  /** 1..64 chars, unique per service — the retry key. A resend of the same intent carries
   *  the same id and lands one order (a reuse answers 409). */
  client_order_id: string;
}

/** The admin body for `POST /api/admin/allocations/book`. The three optional fields are
 *  sent only when the operator set them: absent means "keep the hub's value". */
export interface SetBookPolicyBody {
  service: string;
  book_open: boolean;
  taker_fee_bps: number;
  price_tick?: string;
  lot_size?: string;
  market_slippage_bps?: number;
  /** The operator's acknowledgement that units held `in_kind` may trade for cash the fund
   *  does not hold. Always sent: the hub refuses to open such a book — and refuses orders
   *  on it — while this is `false`, and a left-out field reads as `false` on the BFF. */
  allow_unbacked_trading: boolean;
}

// ── The realtime frame ─────────────────────────────────────────────────────────

/**
 * Everything `/api/book/ws` is allowed to say.
 *
 * Unlike the governance socket this one IS a delivery: the frame carries the same snapshot
 * `GET /api/book` answers and the latest public trades, so the book on screen is rendered
 * from it directly. Nothing personal rides on it — no balance and no other user's orders.
 * The caller's own orders are still a REST read: `orders_revision` only says WHEN to
 * re-issue it.
 */
export interface BookFrame {
  type: "book";
  /** `null` only if invest-core sent a frame without one, which its contract does not allow. */
  snapshot: BookSnapshot | null;
  /** Newest first. */
  trades: Trade[];
  /** The book revision at which the caller's own orders last changed; "0" = never. */
  orders_revision: string;
}

export interface HeartbeatFrame {
  type: "heartbeat";
  at?: string;
}

export type BookSocketFrame = BookFrame | HeartbeatFrame;
