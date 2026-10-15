import type { NextConfig } from "next";

import { BASE_PATH } from "./shared/config/base-path";

// This file deliberately reads its own env directly instead of routing
// through ./config (backed by @evinvest/settings) or shared/config/security.ts
// (which imports ./config for IN_PRODUCTION). Next's next-config-ts
// transpiler bundles this file's static local imports into a single
// next.config.compiled.js, so importing either — even lazily via a dynamic
// import(), which the transpiler doesn't follow and which has no loader for
// bare .ts paths at runtime — drags in @evinvest/settings, an ESM-only
// package (its exports map has no "require" condition) that breaks
// `next build` with ERR_PACKAGE_PATH_NOT_EXPORTED. Same class of problem as
// the Sentry note below, same resolution: forgo full integration here.
function requiredEnv(name: string): string {
  // eslint-disable-next-line no-restricted-properties -- see the note above: ./config cannot be imported here
  const value = process.env[name];
  if (!value) throw new Error(`missing required env var ${name}`);
  return value;
}
// The invest BFF's origin (`http://invest-cabinet-backend:50072`). Every BFF route lives under
// `/cabinet`, so a request keeps its whole path there. Baked into routes-manifest.json at
// build time, so it must resolve synchronously here.
const BACKEND = requiredEnv("CABINET_BACKEND_URL").replace(/\/+$/, "");
// eslint-disable-next-line no-restricted-properties -- see the note above: ./config cannot be imported here
const IN_PRODUCTION = process.env.NODE_ENV === "production";

const nextConfig: NextConfig = {
  reactStrictMode: true,
  // Self-contained production server (.next/standalone) — the container runs
  // `node server.js` without an `npm install`.
  output: "standalone",
  // invest.evinvest.ltd/cabinet: the landing owns the rest of the host, so every page,
  // asset, rewrite and header lives under the prefix (shared/config/base-path.ts).
  basePath: BASE_PATH,
  experimental: {
    // No `rootParams` flag: since Next 16.4 `next/root-params` is on by default and
    // the option is gone from the config type. `currentLocale()` still depends on
    // it — it reads the root layout's `[locale]` from components Next hands no props.
    // The `forbidden.tsx` / `unauthorized.tsx` file conventions. Required, not
    // cosmetic: without it next-app-loader never resolves either file, so
    // `forbidden.tsx` sat in the tree doing nothing and a `forbidden()` /
    // `unauthorized()` call fell through to the 500 boundary.
    authInterrupts: true,
    // Enables `app/global-not-found.tsx` — the 404 for URLs that matched no route
    // at all. Required for the same reason: the root layout is
    // `app/[locale]/layout.tsx`, so a URL that resolved no `[locale]` has no layout
    // to render a nested `not-found.tsx` inside, and Next serves its built-in black
    // 404 instead.
    globalNotFound: true,
  },
  async rewrites() {
    // basePath-relative sources: externally /cabinet/api/* and /cabinet/auth/*. The second
    // is the sign-in callback, `/cabinet/auth/callback` without `/api`, because that is the
    // redirect URI concierge registered for the `invest` client, byte for byte. In
    // production the ingress sends both prefixes to the BFF before they reach Next; these
    // rewrites are the same routing for `next dev` and a pod reached directly.
    return [
      { source: "/api/:path*", destination: `${BACKEND}${BASE_PATH}/api/:path*` },
      { source: "/auth/:path*", destination: `${BACKEND}${BASE_PATH}/auth/:path*` },
    ];
  },
  // Request-invariant hardening on every response, mirroring
  // shared/config/security.ts's staticSecurityHeaders() (kept in sync by
  // hand — see the module comment above for why that helper can't be
  // imported here). The nonce-bearing CSP itself is set per-request in
  // proxy.ts, which imports shared/config/security.ts directly (a normal
  // ESM runtime context, not this file's CJS-transpiled one).
  async headers() {
    const headers = [
      { key: "X-Frame-Options", value: "DENY" },
      { key: "X-Content-Type-Options", value: "nosniff" },
      { key: "Referrer-Policy", value: "strict-origin-when-cross-origin" },
      // Wallet money actions run in a popup on evinvest.ltd that posts its result back to
      // this page; `same-origin` would sever the opener link and every action would end
      // as `closed` (@evinvest/wallet README). This is the strictest value that works.
      { key: "Cross-Origin-Opener-Policy", value: "same-origin-allow-popups" },
    ];
    if (IN_PRODUCTION) {
      headers.push({
        key: "Strict-Transport-Security",
        value: "max-age=63072000; includeSubDomains; preload",
      });
    }
    return [{ source: "/:path*", headers }];
  },
};

// No build-time Sentry wrapper here: @evinvest/error-monitoring's `./next` export
// is ESM-only, but Next loads next.config.ts as CJS, so `withSentry` can't be
// imported in this file. Runtime error capture is wired instead in
// `instrumentation.ts` (server) and `ErrorMonitoringProvider` (browser); only
// build-time source-map upload (which needs SENTRY_* secrets) is forgone.
export default nextConfig;
