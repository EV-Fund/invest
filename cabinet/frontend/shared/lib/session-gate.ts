// The proxy's `/login` decision, without the proxy.
//
// `proxy.ts` imports `next/server` and so cannot be loaded by a test runner; where `/login`
// sends the browser is decided here from plain values instead.
//
// Nothing here keeps a guest out: every page renders for anyone, and what a caller may see
// on it is the route gate's (`application/layout/route-gate.tsx`), from the session's
// permissions. `/login` is no page of its own — it is the page to return to, with the
// sign-in dialog open over it (`?login`), or that page outright for a session.
// Relative imports only: a test runner resolves no `@/` alias.

import { zonePathname } from "../config/base-path.ts";

export interface SessionGateInput {
  /** basePath-relative, as the proxy sees it. */
  pathname: string;
  /** The query string as the request carried it, `?` included, or "". */
  search: string;
  /** The locale segment of the path, or null when there is none. */
  locale: string | null;
  /** Whether the request carries a usable session cookie. */
  hasSession: boolean;
}

/** Where to send the browser instead of rendering — a router path and its query — or null to render. */
export type SessionRedirect = { pathname: string; search: string } | null;

/** The query flag that opens the sign-in dialog over whatever page carries it. */
export const SIGN_IN_PARAM = "login";

export function decideSession({ pathname, search, locale, hasSession }: SessionGateInput): SessionRedirect {
  if (zonePathname(pathname) !== "/login") return null;
  const target = new URL(returnTarget(new URLSearchParams(search).get("returnTo")), "http://zone");
  if (!hasSession) target.searchParams.set(SIGN_IN_PARAM, "");
  const path = target.pathname === "/" ? "" : target.pathname;
  return { pathname: `/${locale ?? "en"}${path}`, search: target.search };
}

/** A zone-relative path to return to: same-origin only, and never a second `/login`. */
function returnTarget(raw: string | null): string {
  if (!raw || !raw.startsWith("/") || raw.startsWith("//") || raw.includes("\\")) return "/";
  const zoneRelative = zonePathname(raw);
  return zoneRelative.startsWith("/login") ? "/" : zoneRelative;
}
