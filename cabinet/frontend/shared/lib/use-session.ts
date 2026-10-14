"use client";

// Browser-side session hook: reads the invest BFF's principal (`/cabinet/api/auth/session`;
// the response never returns a token) so client components can read the session's
// permissions the route gate and the admin nav decide on. Server-side authorization is
// still authoritative; this only shapes the UI.
//
// The fetch, its module-level cache and the single-flight live in `shared/lib/session.ts`
// — shared with the api client and the session keeper, so every consumer on a page
// (rail, route gate, admin views) and every cookie rotation are ONE request. Only
// resolved responses are cached; a fetch failure yields the SESSION_UNAVAILABLE sentinel
// and retries on the next mount, so a transient blip never sticks (or bounces anyone to
// /login). Consumers also re-render when the keeper's rotation returns a different
// principal — a sign-out in another tab reaches this one without a reload.

import { useEffect, useState } from "react";

import type { SessionInfo } from "@/shared/contracts/admin";
import { SESSION_UNAVAILABLE, cachedSession, onSessionChange, readSession } from "@/shared/lib/session";

export { SESSION_UNAVAILABLE };

export function useSession(): SessionInfo | null {
  const [session, setSession] = useState<SessionInfo | null>(cachedSession);
  useEffect(() => {
    let active = true;
    void readSession().then((s) => active && setSession(s));
    const stop = onSessionChange((s) => active && setSession(s));
    return () => {
      active = false;
      stop();
    };
  }, []);
  return session;
}
