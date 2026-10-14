// What each cabinet section needs the caller's session to hold — the one table the route
// gate reads. Cosmetic: the BFF is the boundary, this only keeps a page from firing
// requests that could only answer 401 or 403.

import type { SessionInfo } from "@/shared/contracts/admin";

/**
 * The investor's own surfaces. `bank:self:invest` became `invest:self:invest` when the
 * allocations moved out of banking (ORG-RESTRUCTURE-PLAN §2).
 */
export const INVEST_SELF = "invest:self:invest";

/**
 * A section's requirement: a permission the session lists, the allocation console, or
 * nothing at all.
 *
 * The console has no single permission: an admin or operator of ANY scope that owns an
 * allocation manages it here (§2, D11), so which seats qualify is the BFF's to derive.
 * It reports the answer as `user.isAdmin`.
 */
export type Requirement = { kind: "permission"; permission: string } | { kind: "console" } | null;

const SECTIONS: readonly (readonly [prefix: `/${string}`, requirement: Exclude<Requirement, null>])[] = [
  ["/admin", { kind: "console" }],
  ["/invest", { kind: "permission", permission: INVEST_SELF }],
];

/**
 * The requirement of a zone-relative page. `/wallet` and `/account` claim none: the
 * platform elements they mount hold their own evinvest.ltd session and wall a guest off
 * themselves.
 */
export function requirementOf(path: string): Requirement {
  if (path === "/") return { kind: "permission", permission: INVEST_SELF };
  const match = SECTIONS.find(([prefix]) => path === prefix || path.startsWith(`${prefix}/`));
  return match ? match[1] : null;
}

/** Whether `session` meets `requirement`. */
export function meets(requirement: Requirement, session: SessionInfo): boolean {
  if (requirement === null) return true;
  if (requirement.kind === "console") return session.user?.isAdmin ?? false;
  return session.permissions.includes(requirement.permission);
}
