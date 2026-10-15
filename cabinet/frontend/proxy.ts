import { type NextRequest, NextResponse } from "next/server";

import { createAbMiddleware } from "@evinvest/experiments/next";
import { isLocale, negotiate, type Locale } from "@evinvest/i18n";

import { experiments } from "@/application/experiments";
import { config as appConfig } from "@/config";
import { isNonPagePath, localeRepairedPath } from "@/shared/config/base-path";
import { COOKIES, isSessionId } from "@/shared/config/cookies";
import { contentSecurityPolicy, websocketOrigin } from "@/shared/config/security";
import { decideSession } from "@/shared/lib/session-gate";

const CSP_HEADER = "content-security-policy";

// A/B assignment boundary (Next 16 "proxy", formerly middleware; Node runtime).
// Assigns a sticky `ab_<key>` cookie per experiment in the registry on first
// visit. A no-op while `experiments` is empty.
const ab = createAbMiddleware(experiments);

// Every page renders for a guest; what it shows them is the route gate's
// (`application/layout/route-gate.tsx`), and the BFF still verifies the session on every
// API call. This module imports `next/server` and so cannot be unit-tested at all — what it
// decides about `/login` lives in `shared/lib/session-gate.ts`.
//
// `req.nextUrl.pathname` is basePath-relative here, and a redirect built from a clone of
// `req.nextUrl` gets `/cabinet` put back on by Next.

/** The locale segment of `/{locale}/…`, or null when there is none. */
function localeOf(pathname: string): Locale | null {
  const first = pathname.split("/")[1];
  return isLocale(first) ? first : null;
}

export async function proxy(req: NextRequest) {
  const { pathname, search } = req.nextUrl;

  // Assets and the BFF are not pages: no locale redirect, no session gate, no cookie
  // writes. The matcher below excludes them as well; correctness lives here, where it
  // can be tested, and the matcher only keeps this proxy off the asset hot path.
  if (isNonPagePath(pathname)) return NextResponse.next();

  const signedIn = isSessionId(req.cookies.get(COOKIES.session)?.value);
  const locale = localeOf(pathname);

  // Per-request nonce: written onto the forwarded request headers so Next applies
  // it to its own inline bootstrap scripts (keeping script-src free of
  // 'unsafe-inline'), and echoed on the response so the browser enforces the CSP.
  const nonce = crypto.randomUUID().replaceAll("-", "");
  // The order book socket is same-origin, but `'self'` has not always covered a websocket
  // handshake and a blocked one fails silently — so the origin is named explicitly,
  // derived from the request because only it knows the public host.
  const csp = contentSecurityPolicy(
    nonce,
    websocketOrigin(
      req.headers.get("x-forwarded-host") ?? req.headers.get("host"),
      req.headers.get("x-forwarded-proto"),
    ),
  );
  req.headers.set(CSP_HEADER, csp);

  // No usable locale in the path: an old link, a bookmark, the landing's "Open the
  // cabinet" link to the bare `/cabinet`. Send them to a real URL rather than 404ing —
  // the cookie is the reader's last choice, Accept-Language the first guess, English the
  // floor.
  if (!locale) {
    const remembered = req.cookies.get(COOKIES.locale)?.value;
    const chosen = isLocale(remembered) ? remembered : negotiate(req.headers.get("accept-language"));
    const repaired = localeRepairedPath(pathname, chosen);
    if (repaired) {
      const url = req.nextUrl.clone();
      url.pathname = repaired;
      return withCsp(NextResponse.redirect(url), csp);
    }
  }

  const redirect = decideSession({ pathname, search, locale, hasSession: signedIn });
  if (redirect) {
    const url = req.nextUrl.clone();
    url.pathname = redirect.pathname;
    url.search = redirect.search;
    return withCsp(NextResponse.redirect(url), csp);
  }

  return withCsp(withLocale(req, ab(req)), csp);
}

// Assigns a sticky locale on first visit — the same shape as the A/B cookie.
//
// `negotiate` reads Accept-Language, which is a *suggestion*: the header is simply the
// best first guess for a signed-in human, and the cookie makes their correction stick.
//
// Deliberately never overwritten once set from a header. A reader who chose English on a
// Russian-configured laptop must not be flipped back on the next navigation.
function withLocale(req: NextRequest, res: NextResponse): NextResponse {
  // The URL is authoritative; this only remembers it, so an unprefixed entry later
  // resolves to the language the reader was last reading rather than re-guessing.
  const fromUrl = localeOf(req.nextUrl.pathname);
  if (!fromUrl && req.cookies.get(COOKIES.locale)?.value) return res;
  res.cookies.set(COOKIES.locale, fromUrl ?? negotiate(req.headers.get("accept-language")), {
    path: "/",
    sameSite: "lax",
    secure: appConfig.authCookieSecure,
    // Readable by the client switcher. It carries no authority, so HttpOnly
    // would buy nothing and cost the switcher a round trip.
    httpOnly: false,
    maxAge: 60 * 60 * 24 * 365,
  });
  return res;
}

function withCsp(res: NextResponse, csp: string): NextResponse {
  res.headers.set(CSP_HEADER, csp);
  return res;
}

export const config = {
  // basePath-relative. "/" is listed explicitly: the negative lookahead alone never
  // matches the bare index. `isNonPagePath` is the same rule in code; keep the two in step.
  matcher: ["/", "/((?!api/|auth/|_next/static|_next/image|favicon.ico).*)"],
};
