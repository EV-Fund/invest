// What each cabinet section needs the caller's session to hold — the one table the route
// gate and the console's nav read. Cosmetic: the BFF is the boundary, this only keeps a
// page from firing requests that could only answer 401 or 403.

import type { SessionInfo } from "@/shared/contracts/admin";

/**
 * The permissions invest-core gates on (`invest_domain::authz`), as concierge resolves a
 * seat into `SessionInfo.permissions`. The investor's own surfaces need none: invest-core
 * admits any signed-in person it knows and decides per product from their access level.
 */
export const PERMISSION = {
  /** Register, open, size and admit to a product; issue units; fees; the redemption queue; revenue. */
  allocationManage: "invest:allocation:manage",
  /** Post a product's AUM, from which its NAV is derived. */
  valuationPost: "invest:valuation:post",
} as const;

/** A section's requirement: a session, one of a set of permissions, or nothing at all. */
export type Requirement = { kind: "signedIn" } | { kind: "anyOf"; permissions: readonly string[] } | null;

const SIGNED_IN: Requirement = { kind: "signedIn" };

// First match wins, so a narrower prefix precedes the section it sits in. The valuation
// screen is the one an `invest:valuation:post` seat alone can use: posting a mark is its
// act, while the redemption queue beside it is `invest:allocation:manage`'s.
const SECTIONS: readonly (readonly [prefix: `/${string}`, requirement: Exclude<Requirement, null>])[] = [
  ["/admin/valuation", { kind: "anyOf", permissions: [PERMISSION.allocationManage, PERMISSION.valuationPost] }],
  ["/admin", { kind: "anyOf", permissions: [PERMISSION.allocationManage] }],
  ["/invest", SIGNED_IN],
];

/**
 * The requirement of a zone-relative page. `/wallet` and `/account` claim none: the
 * platform elements they mount hold their own evinvest.ltd session and wall a guest off
 * themselves.
 */
export function requirementOf(path: string): Requirement {
  if (path === "/") return SIGNED_IN;
  const match = SECTIONS.find(([prefix]) => path === prefix || path.startsWith(`${prefix}/`));
  return match ? match[1] : null;
}

/** Whether `session` meets `requirement`. */
export function meets(requirement: Requirement, session: SessionInfo): boolean {
  if (requirement === null) return true;
  if (!session.authenticated) return false;
  if (requirement.kind === "signedIn") return true;
  return requirement.permissions.some((permission) => session.permissions.includes(permission));
}
