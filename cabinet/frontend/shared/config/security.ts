// Security response headers for the cabinet host document.
//
// The cabinet renders authenticated, money-moving surfaces and runs the
// platform MFE code, so the host document ships a baseline of hardening headers. The
// CSP is the load-bearing one: the platform elements are loaded as runtime ESM <script>
// from registry-provided scriptUrls (@evinvest/mfe), so script-src cannot be a trivial
// 'self' — it carries the elements' bundle directories (plus the observability endpoints
// the browser talks to), otherwise the CSP breaks them.
//
// Split of responsibilities: the request-invariant headers below are emitted
// statically from next.config.ts; the CSP is emitted per-request from proxy.ts
// because it carries a per-request nonce (so Next's own inline bootstrap scripts
// stay allowed without 'unsafe-inline').

import { CONCIERGE_MFE_PREFIX, CONCIERGE_ORIGIN } from "@evinvest/concierge";
import { WALLET_MFE_PREFIX, WALLET_ORIGIN } from "@evinvest/wallet";

import { config } from "../../config.ts";

const IN_PRODUCTION = config.isProduction;
// `next dev` only (never build/start, never the test runner where NODE_ENV is
// unset): React + Turbopack use eval() in development for HMR and callstack
// reconstruction, so the strict no-eval CSP would break the dev server.
const IN_DEVELOPMENT = config.isDevelopment;

// The platform's element bundles: the concierge sign-in dialog and account page, the
// banking wallet. Directories rather than the bare origin (D20, @evinvest/mfe README), so
// nothing else evinvest.ltd serves can run as a script here; the registry readers refuse a
// bundle outside the same directories. MFE_ALLOWED_ORIGINS adds a staging stand's origins.
function mfeSources(): string[] {
  return [CONCIERGE_MFE_PREFIX, WALLET_MFE_PREFIX, ...splitList(config.mfeAllowedOrigins)];
}

// Origins the browser opens network connections to (XHR/fetch/WebSocket), so
// connect-src lets them through. Same-origin /api/* is covered by 'self'; the
// observability endpoints are env-driven and contribute nothing when unset.
function connectOrigins(): string[] {
  // The registries, the session read and the elements' own API calls all go to the
  // platform origin — the elements run in this document, so their fetches are ours.
  const origins = new Set<string>([CONCIERGE_ORIGIN, WALLET_ORIGIN, ...splitList(config.mfeAllowedOrigins)]);
  const posthog = absoluteOrigin(config.public.posthogHost);
  if (posthog) origins.add(posthog);
  const sentry = absoluteOrigin(config.public.sentryDsn);
  if (sentry) origins.add(sentry);
  return [...origins];
}

function absoluteOrigin(value: string | undefined): string | null {
  if (!value) return null;
  try {
    return new URL(value).origin;
  } catch {
    return null; // relative URL → same-origin, already covered by 'self'.
  }
}

function splitList(value: string | undefined): string[] {
  return (value ?? "").split(/[\s,]+/).filter(Boolean);
}

/**
 * The `ws://` or `wss://` form of the origin this document was served from, for
 * `connect-src` — or null when the host is unknown or unusable.
 *
 * This exists because `'self'` does not reliably cover a websocket. CSP Level 3 made
 * `'self'` match a same-origin `wss:` handshake and current browsers implement that, but
 * the behaviour was absent for years and the failure mode is the worst kind: the socket is
 * blocked before it opens, with no error the page can catch — it simply never connects, and
 * the order book quietly falls back to polling forever on the browsers that got it wrong.
 * Naming the origin explicitly costs one token and removes the question.
 *
 * The host comes from the request rather than an env var because only the request knows
 * the public origin the reader asked for. `x-forwarded-*` win over `host` because behind
 * the ingress `host` is the internal upstream name, and a CSP naming that origin would
 * allow a socket nobody opens while blocking the one the browser does.
 *
 * A host is emitted only if it parses as one. The header is attacker-controllable, and a
 * malformed or injected value must fall out of the allow-list rather than into it.
 */
export function websocketOrigin(host: string | null | undefined, forwardedProto?: string | null): string | null {
  if (!host) return null;
  // The first entry of a comma-joined forwarded chain is the client-facing one.
  const proto = (forwardedProto ?? "").split(",")[0]?.trim().toLowerCase();
  const scheme = proto === "http" ? "ws" : proto === "https" ? "wss" : IN_PRODUCTION ? "wss" : "ws";
  try {
    const url = new URL(`${scheme}://${host.split(",")[0]!.trim()}`);
    // `new URL` tolerates a path, a userinfo section and a query; an origin must have none
    // of them, and CSP would silently take the whole string as a source expression.
    return url.host && url.pathname === "/" && !url.username && !url.search ? url.origin : null;
  } catch {
    return null;
  }
}

/** Cloudflare Turnstile's script and frame, which the sign-in element's forms carry. */
const TURNSTILE_ORIGIN = "https://challenges.cloudflare.com";

/**
 * The Content-Security-Policy for the host document, bound to a per-request nonce.
 *
 * `socketOrigin` is the value of {@link websocketOrigin} for this request. It is optional
 * so that a caller with no request in hand (the tests, any static evaluation) still gets a
 * complete policy — one that is stricter, never looser.
 */
export function contentSecurityPolicy(nonce: string, socketOrigin?: string | null): string {
  const script = ["'self'", `'nonce-${nonce}'`, TURNSTILE_ORIGIN, ...mfeSources()];
  if (IN_DEVELOPMENT) script.push("'unsafe-eval'");
  // The order book's stream is same-origin (`/cabinet/api/book/ws`, rewritten to the
  // BFF), so this adds a scheme, not a new host to trust.
  const connect = ["'self'", ...connectOrigins(), ...(socketOrigin ? [socketOrigin] : [])];
  return [
    `default-src 'self'`,
    `base-uri 'self'`,
    `object-src 'none'`,
    `frame-ancestors 'none'`,
    `form-action 'self'`,
    `script-src ${script.join(" ")}`,
    // The concierge sign-in element runs in this document, so its Turnstile challenge
    // renders in Cloudflare's frame here.
    `frame-src ${TURNSTILE_ORIGIN}`,
    // styled-components / Tailwind inject runtime <style>; 'unsafe-inline' here is
    // style-only and does not weaken the XSS-relevant script-src. The element bundles
    // bring their stylesheets from their own directories.
    `style-src 'self' 'unsafe-inline' ${mfeSources().join(" ")}`,
    `img-src 'self' data: blob:`,
    `font-src 'self'`,
    // Sentry Replay (mounted through `@evinvest/error-monitoring`'s provider on every
    // cabinet page) compresses its recording in a worker it builds from a `blob:` URL —
    // and it starts recording in buffer mode whenever the error sample rate is above
    // zero, DSN or not, so this is a production fact and not a dev-overlay one. Stated
    // here rather than loosened in `script-src`: without a `worker-src` the browser
    // falls back to `script-src`, and `blob:` there would admit scripts, not just workers.
    `worker-src 'self' blob:`,
    `connect-src ${connect.join(" ")}`,
    ...(IN_PRODUCTION ? [`upgrade-insecure-requests`] : []),
  ].join("; ");
}

/**
 * The per-request CSP nonce, read back from the header proxy.ts set, so Server
 * Components that render their own inline scripts (e.g. next-themes) can stamp it
 * and stay allowed under script-src. `null` outside a request (e.g. static
 * prerender), where there is no inline script to protect. `next/headers` is
 * imported lazily so next.config.ts can import this module at config-eval time.
 */
export async function requestNonce(): Promise<string | null> {
  const { headers } = await import("next/headers");
  const csp = (await headers()).get("content-security-policy");
  return csp?.match(/'nonce-([^']+)'/)?.[1] ?? null;
}

/**
 * Request-invariant security headers emitted statically by next.config.ts.
 * X-Frame-Options is the belt to the CSP's frame-ancestors braces (and covers
 * routes the proxy matcher skips); HSTS is production-only (it requires HTTPS).
 */
export function staticSecurityHeaders(): Array<{ key: string; value: string }> {
  const headers = [
    { key: "X-Frame-Options", value: "DENY" },
    { key: "X-Content-Type-Options", value: "nosniff" },
    { key: "Referrer-Policy", value: "strict-origin-when-cross-origin" },
    { key: "Cross-Origin-Opener-Policy", value: "same-origin-allow-popups" },
  ];
  if (IN_PRODUCTION) {
    headers.push({ key: "Strict-Transport-Security", value: "max-age=63072000; includeSubDomains; preload" });
  }
  return headers;
}
