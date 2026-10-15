// The central admin on evinvest.ltd (D3, ORG-RESTRUCTURE-PLAN §7): users, approvals (the
// consilium moved into concierge) and every money screen — treasury, payments, withdrawals.
// Another app on another origin, so links to it are plain anchors, never the router.
export const PLATFORM_ADMIN_URL = "https://evinvest.ltd/admin";

/** Scopes and their grants — who administers a scope, who may open its panel. */
export const PLATFORM_SCOPES_URL = `${PLATFORM_ADMIN_URL}/scopes`;

/** The owners' approvals (concierge's, D4/D23) — where a consilium is voted and followed. */
export const PLATFORM_APPROVALS_URL = `${PLATFORM_ADMIN_URL}/approvals`;
