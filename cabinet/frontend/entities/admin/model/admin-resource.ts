"use client";

// The allocation console's reads, cached the same way the investor screens' are, so moving
// between the registry, valuation, fees and revenue doesn't re-fetch each one from scratch.
//
// Shorter windows than the investor side: these are operational screens, and an operator
// looking at a queue is looking at it *now*. Cached data still paints first — the refresh
// happens behind it — so the effect is a screen that opens on last-known state and settles,
// rather than one that opens empty.
//
// Mutations stay where they are, in the views: each admin action already owns a deliberate
// refetch-and-reconcile. Those flows call `refresh()` or `invalidate()` on the resource instead
// of re-fetching by hand, which keeps every other open surface in step for free.

import {
  fetchAllocationAccessGrants,
  fetchAllocations as fetchAdminAllocations,
  fetchFundRevenue,
  fetchFeeAssessments,
  fetchFeePolicies,
  fetchFeePolicyChanges,
  fetchFeeShares,
  fetchRedemptionQueue,
  fetchUnitHolders,
  fetchUsers,
} from "@/entities/admin/api/admin-client";
import type { AdminUserSummary } from "@/shared/contracts/admin";
import { TAG } from "@/shared/lib/cache-tags";
import { isPendingChange } from "@/shared/lib/fee-terms";
import { defineResource } from "@/shared/lib/resource";

const OPERATIONAL = 10;

// A pending change moves without anyone on the screen acting — the sweeper promotes it at
// its moment, the owners carry or reject it from their mailboxes — and none of the clock's
// other triggers fires while the tab just sits there open (`shared/lib/resource.ts`,
// `poll`). Polled only while one is on its way, so a fund whose terms are settled costs
// nothing; the card and the history row are read by the two resources below, both on it.
const PENDING_CHANGE_POLL = { startMs: 15_000, maxMs: 60_000 };

// The `fee` allocation — what the platform earned, in the treasury's shape (#245).
// Operational cadence like the queues: a settling fee or an executed payment out of the
// claim moves the figure, and an owner looking at it is looking at it now.
export const fundRevenueResource = defineResource({
  name: "admin.fundRevenue",
  fetch: fetchFundRevenue,
  revalidate: OPERATIONAL,
  tags: [TAG.adminRevenue],
});

// Fee terms change only when an operator rewrites them — registry cadence, not
// operational — so the table opens on what it last showed.
export const feePoliciesResource = defineResource({
  name: "admin.feePolicies",
  fetch: fetchFeePolicies,
  revalidate: 300,
  tags: [TAG.adminFees],
  poll: { ...PENDING_CHANGE_POLL, while: (list) => (list?.policies ?? []).some((p) => isPendingChange(p.pending?.state)) },
});

// The accumulated units move with every sweep, and their value moves with every mark, so
// this is operational: an operator deciding whether to settle is looking at it now.
export const feeSharesResource = defineResource({
  name: "admin.feeShares",
  fetch: fetchFeeShares,
  key: (service) => service,
  revalidate: OPERATIONAL,
  tags: [TAG.adminFees],
  enabled: (service) => service.trim().length > 0,
});

export const feeAssessmentsResource = defineResource({
  name: "admin.feeAssessments",
  fetch: fetchFeeAssessments,
  key: (service) => service,
  revalidate: OPERATIONAL,
  tags: [TAG.adminFees],
  enabled: (service) => service.trim().length > 0,
});

// The history moves when an operator schedules or cancels a change — and, once a day at
// most, when the sweeper promotes one — so it is registry cadence like the policies. It
// shares their tag: every mutation on this screen names `adminFees` and both re-read.
export const feePolicyChangesResource = defineResource({
  name: "admin.feePolicyChanges",
  fetch: fetchFeePolicyChanges,
  key: (service) => service,
  revalidate: 300,
  tags: [TAG.adminFees],
  enabled: (service) => service.trim().length > 0,
  poll: { ...PENDING_CHANGE_POLL, while: (list) => (list?.changes ?? []).some((c) => isPendingChange(c.state)) },
});

export const redemptionQueueResource = defineResource({
  name: "admin.redemptionQueue",
  fetch: fetchRedemptionQueue,
  revalidate: OPERATIONAL,
  tags: [TAG.adminQueue],
});

// The operator listing — drafts and closed products included, unlike the investor-facing
// `/api/allocations` the rail reads. Two different questions, two different cache entries.
export const adminAllocationsResource = defineResource({
  name: "admin.allocations",
  fetch: fetchAdminAllocations,
  revalidate: 30,
  tags: [TAG.adminAllocations],
});

// Who is raised above a product's default access, per service — operational cadence: the
// grants panel opens on a specific product and an operator granting or revoking there is
// looking at the roster *now*.
export const allocationAccessGrantsResource = defineResource({
  name: "admin.allocationGrants",
  fetch: fetchAllocationAccessGrants,
  key: (service) => service,
  revalidate: OPERATIONAL,
  tags: [TAG.adminAllocationGrants],
  enabled: (service) => service.trim().length > 0,
});

// The cap table — the settled supply and every holder of it, per service. Operational: the issuance panel
// opens on one product, and an operator who has just queued a mint is watching for it to
// land — the table moves when the relay posts, not when the POST answers.
export const unitHoldersResource = defineResource({
  name: "admin.unitHolders",
  fetch: fetchUnitHolders,
  key: (service) => service,
  revalidate: OPERATIONAL,
  tags: [TAG.adminUnitHolders],
  enabled: (service) => service.trim().length > 0,
});

// Keyed on the whole filter set: a search is a different question, not a refinement of the
// last one, and the hub applies `query`/`role`/`status` server-side.
// Every account at once, for pickers that fuzzy-match client-side: the hub's query only
// narrows by substring. Pages at the plane's 200-row ceiling.
export const userDirectoryResource = defineResource({
  name: "admin.userDirectory",
  fetch: async () => {
    const users: AdminUserSummary[] = [];
    for (;;) {
      const page = await fetchUsers({ limit: 200, offset: users.length });
      users.push(...page.users);
      if (page.users.length === 0 || users.length >= Number(page.total)) return users;
    }
  },
  revalidate: OPERATIONAL,
  tags: [TAG.adminUsers],
});

