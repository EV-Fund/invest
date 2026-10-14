"use client";

// What each screen reads, so the reading can start before the screen does.
//
// The cache in `shared/lib/resource.ts` removes the skeleton on the SECOND visit to a
// screen — the value is already there. This closes the first visit too. `next/link` would
// prefetch the route's RSC payload on hover, but the cabinet turns that off (see
// `shared/ui/cabinet-link.tsx`: a dynamic route with no loading boundary has nothing to
// carry); this is the same idea applied where it pays — the data the screen will ask for,
// driven off the same signal. By the time the click lands the answer
// is usually in the cache, and the screen paints its figures on the first frame.
//
// Two triggers, both cheap and both cancel-free (a warm that turns out to be unnecessary
// costs one GET that a later screen would have made anyway):
//
//   · intent — pointer or keyboard focus on a nav row warms that row's screen;
//   · idle — once the shell is up, warm the handful of reads shared by most screens, at
//     idle priority so they never race the reads the current screen is making for itself.

import { adminAllocationsResource, fundRevenueResource, redemptionQueueResource } from "@/entities/admin/model/admin-resource";
import { bookPolicyResource, bookSnapshotResource, bookTradesResource, openOrdersResource } from "@/entities/book/model/book-resource";
import { allocationsResource, fundNavResource, positionsResource, redemptionsResource } from "@/entities/fund/model/fund-resource";
import { walletResource } from "@/entities/wallet/model/wallet-resource";

// Zone-relative paths, exactly as the rail writes them (basePath is not part of these).
// Ordered longest-prefix-first so `/invest/<service>` isn't answered by `/invest`.
const ROUTES: ReadonlyArray<{ prefix: string; warm: (path: string) => void }> = [
  { prefix: "/admin/revenue", warm: () => fundRevenueResource.prefetch() },
  { prefix: "/admin/allocations", warm: () => adminAllocationsResource.prefetch() },
  {
    prefix: "/admin/valuation",
    warm: () => {
      adminAllocationsResource.prefetch();
      redemptionQueueResource.prefetch();
    },
  },
  {
    prefix: "/invest/",
    warm: (path) => {
      allocationsResource.prefetch();
      positionsResource.prefetch();
      redemptionsResource.prefetch();
      // `/invest/<service>` — the fund's own page opens on its NAV, and on whether its
      // book is open (the "Trade" control reads the policy).
      const [slug, surface] = path.slice("/invest/".length).split("/");
      const service = decodeURIComponent(slug ?? "");
      if (!service) return;
      fundNavResource.prefetch(service);
      bookPolicyResource.prefetch(service);
      if (surface === "trade") {
        // The terminal: the book paints from the cache before the socket's first frame
        // lands, and the form needs the balance to say what is available.
        bookSnapshotResource.prefetch(service);
        bookTradesResource.prefetch(service);
        openOrdersResource.prefetch(service);
        walletResource.prefetch();
      }
    },
  },
  {
    prefix: "/invest",
    warm: () => {
      positionsResource.prefetch();
      redemptionsResource.prefetch();
      walletResource.prefetch();
      allocationsResource.prefetch();
    },
  },
  {
    prefix: "/",
    warm: () => {
      positionsResource.prefetch();
      allocationsResource.prefetch();
    },
  },
];

// "/" is Home, not "every path": it is matched exactly, never as a prefix. Every other entry
// matches its own path or anything nested under it.
function matches(path: string, prefix: string): boolean {
  if (prefix === "/") return path === "/";
  return path === prefix || path.startsWith(prefix.endsWith("/") ? prefix : `${prefix}/`);
}

/** Warm the reads the screen at `href` will make. Safe to call repeatedly — fresh entries are left alone. */
export function prefetchRoute(href: string): void {
  const path = href.split("?")[0] ?? href;
  ROUTES.find((route) => matches(path, route.prefix))?.warm(path);
}

/**
 * Handlers to spread onto a nav link so intent warms its screen. `onFocus` is not optional
 * garnish: a keyboard user tabbing the rail has to get the same head start a pointer does.
 */
export function prefetchOn(href: string) {
  const warm = () => prefetchRoute(href);
  return { onMouseEnter: warm, onFocus: warm, onTouchStart: warm };
}

/**
 * Warm what nearly every screen in the cabinet reads: the balance, the positions, and the
 * fund catalog the rail itself lists. Called once when the signed-in shell mounts.
 *
 * At idle, so the current screen's own reads go first — nothing here is needed to paint the
 * page the user actually asked for.
 */
export function warmShell(): () => void {
  const warm = () => {
    walletResource.prefetch();
    positionsResource.prefetch();
    allocationsResource.prefetch();
  };
  if (typeof window === "undefined") return () => undefined;
  if (typeof requestIdleCallback === "function") {
    const handle = requestIdleCallback(warm, { timeout: 2_000 });
    return () => cancelIdleCallback(handle);
  }
  // Safari has no requestIdleCallback; a short timer is close enough for a warm-up.
  const timer = setTimeout(warm, 500);
  return () => clearTimeout(timer);
}
