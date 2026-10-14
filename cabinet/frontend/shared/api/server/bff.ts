import "server-only";

import { cookies } from "next/headers";

import { config } from "@/config";
import { COOKIES, isSessionId } from "@/shared/config/cookies";

// Server-render reads from the BFF — the server half of `shared/lib/api-client.ts`.
//
// A server component calls this to put real figures into the first HTML instead of a
// skeleton. It forwards only the session cookie (the one credential the invest BFF reads;
// the BFF keeps the tokens behind it server-side) and never throws: any failure — no cookie, a
// 401, a timeout, a body of the wrong shape — is "no data", and the screen falls back to
// the browser read it has always made, which can still heal a session the server could
// not. A server read is an optimisation, never a new way for a screen to fail.

/** What a server read hands to a client resource as its `seed`. */
export interface ServerRead<T> {
  data: T;
  /** Epoch ms when the BFF answered — the resource's freshness clock starts here. */
  fetchedAt: number;
}

/** Long enough for a healthy BFF; short enough that a stuck one still lets the page settle. */
const READ_TIMEOUT_MS = 3_000;

/**
 * GET a BFF path (`/api/…`) with the caller's session cookie.
 *
 * `accept` narrows the parsed body; a body it rejects is reported as no data rather than
 * cast into a type it does not have.
 */
export async function bffRead<T>(path: `/api/${string}`, accept: (body: unknown) => body is T): Promise<ServerRead<T> | null> {
  const session = (await cookies()).get(COOKIES.session)?.value;
  if (!isSessionId(session)) return null;
  try {
    const res = await fetch(`${config.backendUrl.replace(/\/+$/, "")}${path}`, {
      headers: { accept: "application/json", cookie: `${COOKIES.session}=${session}` },
      cache: "no-store",
      signal: AbortSignal.timeout(READ_TIMEOUT_MS),
    });
    if (!res.ok) return null;
    const body: unknown = await res.json();
    return accept(body) ? { data: body, fetchedAt: Date.now() } : null;
  } catch {
    return null;
  }
}
