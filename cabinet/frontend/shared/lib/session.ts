"use client";

// The browser's view of the invest RP session, and the one call that keeps it alive.
//
// The session cookie is the invest BFF's (an RP session opened after concierge's
// `authorize` round trip); the BFF holds the short-lived tokens behind it and refreshes
// them when this endpoint is read. A tab left idle still looks signed in to `proxy.ts`
// while its tokens have lapsed, so two callers keep the page working without a reload:
// the api client (heals a 401 in place) and the session keeper (reads this before the
// tokens lapse). See `shared/lib/api-client.ts` and `application/layout/session-keeper.tsx`.

import { apiPath } from "@/shared/config/base-path";
import type { SessionInfo } from "@/shared/contracts/admin";

/// A fetch/parse failure — same shape as a server-resolved "not signed in", but
/// identity-distinguishable so consumers don't force a re-login on a network blip.
export const SESSION_UNAVAILABLE: SessionInfo = Object.freeze({ authenticated: false, permissions: [] });

const ENDPOINT = apiPath("/api/auth/session");

/// How long a confirmed session is presumed fresh. Under concierge's access TTL (300s
/// deployed) minus the refresh skew, so a page that reads on this cadence never has the
/// BFF present a lapsed token.
export const STALE_MS = 4 * 60_000;

let cached: SessionInfo | null = null;
let inflight: Promise<SessionInfo | null> | null = null;
/// Bumped on every REFRESH ATTEMPT (not just the successful ones) so an unreachable
/// endpoint — local dev may run without a BFF — is retried on the same slow
/// cadence instead of before every request.
let attemptedAt = 0;
/// Bumped on every BFF answer, so a caller can tell "the cookie was re-set while my
/// request was in flight" from "nothing has happened since".
let generation = 0;
const listeners = new Set<(session: SessionInfo) => void>();

/** The last session the BFF resolved, if any consumer has fetched it yet. */
export function cachedSession(): SessionInfo | null {
  return cached;
}

/** How many times the BFF has answered — a cheap "did the session change under me". */
export function sessionGeneration(): number {
  return generation;
}

/** Subscribe to BFF answers (a keeper rotation, a 401 heal, a sign-out). */
export function onSessionChange(listener: (session: SessionInfo) => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** The cached session, fetching once if nothing has yet. Never rejects. */
export function readSession(): Promise<SessionInfo> {
  if (cached) return Promise.resolve(cached);
  return refreshSession().then((session) => session ?? SESSION_UNAVAILABLE);
}

/**
 * Round-trip the BFF session endpoint, which refreshes the tokens behind the session.
 *
 * Single-flight: concurrent callers (several 401s at once, a keeper tick landing on a
 * page load) coalesce into ONE refresh. Resolves to the principal, to
 * `{ authenticated: false }` when the BFF says the session is genuinely gone, or to
 * `null` when the answer is UNKNOWN (offline, 5xx, no BFF in dev) — the distinction
 * callers need before bouncing anyone to /login.
 */
export function refreshSession(): Promise<SessionInfo | null> {
  inflight ??= (async () => {
    attemptedAt = Date.now();
    try {
      const res = await fetch(ENDPOINT, { headers: { accept: "application/json" }, cache: "no-store" });
      // A 5xx means the session's fate is unknown (the BFF keeps the session on a
      // store blip rather than signing anyone out) — report unknown, never signed-out.
      if (!res.ok) return null;
      const session = (await res.json()) as SessionInfo;
      cached = session;
      attemptedAt = Date.now();
      generation += 1;
      for (const listener of listeners) listener(session);
      return session;
    } catch {
      return null;
    } finally {
      inflight = null;
    }
  })();
  return inflight;
}

/**
 * Rotate the access cookie if the session hasn't been confirmed for STALE_MS — the
 * pre-flight both the api client and the keeper use. A no-op (and never a round-trip)
 * on a page that is already talking to the BFF regularly.
 */
export async function refreshIfStale(): Promise<void> {
  if (Date.now() - attemptedAt < STALE_MS) return;
  await refreshSession();
}

/** Test seam: drop all module state so each case starts from a cold page load. */
export function resetSessionForTests(): void {
  cached = null;
  inflight = null;
  attemptedAt = 0;
  generation = 0;
  listeners.clear();
}
