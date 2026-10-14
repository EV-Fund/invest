// Browser → BFF admin-console client. Thin typed fetchers over the `/api/admin/*`
// routes; the shapes match the BFF DTOs (`@/shared/contracts/admin`). Mutations carry
// the CSRF double-submit header. No tokens are seen here — the BFF holds them and the
// owning plane re-checks the operator's role.

import { getJson, postJson } from "@/shared/lib/api-client";
import type {
  AcknowledgeUndeliveredNoticesRequest,
  Allocation,
  AllocationAccessGrant,
  AllocationAccessGrantList,
  AllocationAccessLevel,
  AllocationBacking,
  AllocationGrantLevel,
  AllocationIcon,
  AllocationList,
  AllocationTreasury,
  AdminUserList,
  CancelFeePolicyChangeRequest,
  FeeAssessmentList,
  FeePolicyChange,
  FeePolicyChangeList,
  FeePolicyList,
  FeeSettlement,
  FeeShares,
  ScheduleFeePolicyRequest,
  FundNav,
  Redemption,
  RedemptionQueue,
  RetireUnitsBody,
  UnitHolders,
  UnitIssuance,
} from "@/shared/contracts/admin";
import type { BookPolicy, SetBookPolicyBody } from "@/shared/contracts/book";
import type { Consilium } from "@/shared/contracts/governance";

// ── users ─────────────────────────────────────────────────────────────────────
export interface UserFilters {
  query?: string;
  role?: string;
  status?: string;
  limit?: number;
  offset?: number;
}

export function fetchUsers(filters: UserFilters = {}): Promise<AdminUserList> {
  const params = new URLSearchParams();
  if (filters.query) params.set("query", filters.query);
  if (filters.role) params.set("role", filters.role);
  if (filters.status) params.set("status", filters.status);
  if (filters.limit) params.set("limit", String(filters.limit));
  if (filters.offset) params.set("offset", String(filters.offset));
  const qs = params.toString();
  return getJson(`/api/admin/users${qs ? `?${qs}` : ""}`);
}

// ── allocations (the registry — the only way a fund comes into existence) ────────
// The admin listing includes drafts and closed products; the investor-facing
// `/api/allocations` returns the open set only.
export const fetchAllocations = (): Promise<AllocationList> => getJson("/api/admin/allocations");

/** The register/update body. `icon` is required here even though it is optional on the
 *  read shape. The hub no longer needs it to be: `icon` carries presence on the wire, so
 *  an omitted field means "leave it alone" rather than "reset to `fund`". It stays
 *  required because this console always knows the icon it is editing, and a request that
 *  states every field it is writing is one you can read back later and know what it did.
 *  Send the current value even when only the title changed. */
export interface AllocationWrite {
  service: string;
  title: string;
  summary: string;
  icon: AllocationIcon;
}

export const registerAllocation = (body: AllocationWrite): Promise<Allocation> => postJson("/api/admin/allocations/register", body);

export const updateAllocation = (body: AllocationWrite): Promise<Allocation> => postJson("/api/admin/allocations/update", body);

export const setAllocationState = (service: string, state: "open" | "closed"): Promise<Allocation> => postJson("/api/admin/allocations/state", { service, state });

// Its own route rather than a field on `/update`: the cap gates money, so the hub raises
// its own audit event for it and refuses a subscription that would mint past it.
export const setAllocationUnitCap = (service: string, unitCap: string): Promise<Allocation> => postJson("/api/admin/allocations/cap", { service, unit_cap: unitCap });

// A product's default access — "closed by default": every registration lands on `view`
// (listed, locked) until an operator opens it up. Its own route for the same reason the
// unit cap has one: the hub raises its own audit event for an access change.
export const setAllocationAccess = (service: string, access: AllocationAccessLevel): Promise<Allocation> => postJson("/api/admin/allocations/access", { service, access });

// One investor raised above the product's default — `hidden` is refused here (grantable
// only, not the full vocabulary): a grant that lowered a holder below the default would
// make "revoke" ambiguous.
export const fetchAllocationAccessGrants = (service: string): Promise<AllocationAccessGrantList> =>
  getJson(`/api/admin/allocations/grants?service=${encodeURIComponent(service)}`);

export const grantAllocationAccess = (service: string, userId: string, level: AllocationGrantLevel): Promise<AllocationAccessGrant> =>
  postJson("/api/admin/allocations/grants/grant", { service, user_id: userId, level });

// Drops the investor back to the product's own default — not to `hidden`, which is why
// this is "revoke" and not "hide".
export const revokeAllocationAccess = (service: string, userId: string): Promise<Record<string, never>> =>
  postJson("/api/admin/allocations/grants/revoke", { service, user_id: userId });

/** The in-kind issue body, exactly as the BFF reads it: always a person (`user_id` — the
 *  company is not a holder and the reserved allocations are seated by the owners'
 *  consilium, #245), `cost_basis` present only when the operator typed one (absent means
 *  `units × NAV` hub-side — an empty string is NOT the same as absent). `issueUnitsBody`
 *  in `views/admin/allocations/lib/issuance.ts` is the one place that builds it. */
export interface IssueUnitsBody {
  service: string;
  /** Who receives the units — the id the console carries. */
  user_id: string;
  /** Decimal units, > 0. */
  units: string;
  /** Decimal USDT the holder is deemed to have paid; omitted = `units × NAV`. */
  cost_basis?: string;
  /** 1..64 chars, unique per service. The retry contract: one key per submission, the
   *  same key on a retry of that submission, so a double click lands one mint. */
  idempotency_key: string;
}

export const issueUnits = (body: IssueUnitsBody): Promise<UnitIssuance> => postJson("/api/admin/allocations/issue", body);

// The mirror of a mint: burn units out of one person so the supply shrinks by them.
// Answers the same shape with `source: "retire"`; the key shares the mint's per-product
// key space. Allowed on a `closed` product, or on a live one only with the operator's
// explicit `force`.
export const retireUnits = (body: RetireUnitsBody): Promise<UnitIssuance> => postJson("/api/admin/allocations/retire", body);

// What stands behind the units. Its own route for the same reason the cap and the access
// level have one: it decides whether `Redeem` pays out or is refused, so the hub raises
// its own audit event for the flip.
export const setAllocationBacking = (service: string, backing: AllocationBacking): Promise<Allocation> => postJson("/api/admin/allocations/backing", { service, backing });

export const fetchUnitHolders = (service: string): Promise<UnitHolders> => getJson(`/api/admin/allocations/holders?service=${encodeURIComponent(service)}`);

// The product's secondary-market terms. Reading them is the investor route
// (`GET /api/book/policy`, `entities/book`); only the write is an operator's. The
// optional fields are sent only when set — absent means "keep the hub's value" — and
// `views/admin/allocations/lib/book-policy.ts` is the one place that builds the body.
export const setBookPolicy = (body: SetBookPolicyBody): Promise<BookPolicy> => postJson("/api/admin/allocations/book", body);

// ── valuation + redemptions ─────────────────────────────────────────────────────
export const fetchRedemptionQueue = (): Promise<RedemptionQueue> => getJson("/api/admin/valuation/queue");

export const postValuation = (body: { service: string; aum: string }): Promise<FundNav> => postJson("/api/admin/valuation/post", body);

// A mark the NAV-move guard refuses is not posted with a flag — it is put to the owners.
// The answer is the consilium the room and the emailed invitations then show.
export const proposeValuationOverride = (body: { service: string; aum: string }): Promise<Consilium> => postJson("/api/admin/valuation/override", body);

export const settleRedemption = (redemptionId: string): Promise<Redemption> => postJson("/api/admin/valuation/settle", { redemption_id: redemptionId });

export const failRedemption = (redemptionId: string): Promise<Redemption> => postJson("/api/admin/valuation/fail", { redemption_id: redemptionId });

// ── revenue (the reserved `fee` allocation) ──────────────────────────────────────
// What the platform has earned, in the treasury's own shape (#245): the `fee`
// allocation's claim, supply, price and holders. Paying it OUT is a payment order
// (`entities/payment`) authorised by the owners' consilium, or a holder's redemption.
export const fetchFundRevenue = (): Promise<AllocationTreasury> => getJson("/api/admin/revenue");

// ── fees ────────────────────────────────────────────────────────────────────────
export const fetchFeePolicies = (): Promise<FeePolicyList> => getJson("/api/admin/fees/policies");

export const fetchFeeShares = (service: string): Promise<FeeShares> => getJson(`/api/admin/fees/shares?service=${encodeURIComponent(service)}`);

export const fetchFeeAssessments = (service: string): Promise<FeeAssessmentList> => getJson(`/api/admin/fees/assessments?service=${encodeURIComponent(service)}`);

/** Propose a change of a fund's terms. The hub decides who must agree and when the change
 *  binds; the answer says both (`state`, `requirement`, `effective_from`). A second request
 *  while one is pending is refused until the first is cancelled. */
export const scheduleFeePolicy = (body: ScheduleFeePolicyRequest): Promise<FeePolicyChange> => postJson("/api/admin/fees/policy", body);

export const cancelFeePolicyChange = (body: CancelFeePolicyChangeRequest): Promise<FeePolicyChange> => postJson("/api/admin/fees/policy/cancel", body);

/** Take responsibility for the holders of a scheduled change who could not be told, so a
 *  tightening binds over them. The hub decides who may (the requester or an owner — 403
 *  otherwise) and whether there is anything left to acknowledge (409 when not). */
export const acknowledgeUndeliveredNotices = (body: AcknowledgeUndeliveredNoticesRequest): Promise<FeePolicyChange> => postJson("/api/admin/fees/policy/acknowledge-notices", body);

/** A fund's whole history of terms, newest version first. */
export const fetchFeePolicyChanges = (service: string): Promise<FeePolicyChangeList> => getJson(`/api/admin/fees/changes?service=${encodeURIComponent(service)}`);

// Empty `units` settles the whole accumulated balance — the ordinary end-of-period call.
export const settleFeeShares = (body: { service: string; units: string }): Promise<FeeSettlement> => postJson("/api/admin/fees/settle", body);

