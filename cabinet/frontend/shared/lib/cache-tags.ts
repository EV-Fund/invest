// The invalidation vocabulary shared by every cached read and every mutation that moves it.
//
// A tag names a fact about the account, not an endpoint: `positions` is "what this account
// holds in the funds", and a subscription, a redemption and a fill all move it. Naming facts
// is what lets a mutation say what it changed without knowing which screens are mounted.
//
// Kept in one file because the interesting cases cross entity boundaries: subscribing to a
// fund moves the positions AND the fees accrued on them. A per-entity constant would put the
// relationship nowhere. Cash is not among the facts: it is banking's, read only by its
// wallet element.

export const TAG = {
  /** Fund positions — units, cost basis, P&L. */
  positions: "positions",
  /** The caller's own redemption requests. */
  redemptions: "redemptions",
  /** The registry of investable funds. Operator-owned; changes rarely. */
  catalog: "catalog",
  /** Per-fund NAV. Moves only when an operator posts a valuation. */
  nav: "nav",
  /** A fund's fee terms and what the caller's holding has accrued against them. Moves
   *  when the sweeper charges, so it travels with `positions`. */
  fees: "fees",

  // ── the book (secondary market in an allocation's units) ──────────────────────
  /** A product's public book: the snapshot and the trade tape. Delivered by the socket
   *  and polled underneath it (`entities/book/model/book-socket.ts`). */
  book: "book",
  /** The caller's own orders — resting, history and fills. Moves on every fill, and a
   *  fill also moves `positions` (the escrow settles into it), so the socket names both
   *  when `orders_revision` moves. */
  orders: "book.orders",
  /** A product's trading terms: open flag, taker fee, tick, lot, slippage. Operator-set;
   *  changes rarely. */
  bookPolicy: "book.policy",

  // ── operator console ──────────────────────────────────────────────────────────
  /** The redemption queue an operator works through. */
  adminQueue: "admin.queue",
  /** The operator allocation registry — drafts and closed products included. */
  adminAllocations: "admin.allocations",
  /** Per-service investor access grants — who is raised above a product's default. */
  adminAllocationGrants: "admin.allocations.grants",
  /** Per-service cap table (holders and supply). An in-kind issuance moves this AND `nav`
   *  (the mark's `units_outstanding` is the same ledger fact), so
   *  the mutation names both rather than this one alone. */
  adminUnitHolders: "admin.allocations.holders",
  /** The `fee` allocation's treasury view: its claim, supply, price and holders. */
  adminRevenue: "admin.revenue",
  /** Fee terms, accumulated units and the charge history. Settling moves the fee units
   *  AND the revenue figure, so a settle names this and `adminRevenue` together. */
  adminFees: "admin.fees",
} as const;
