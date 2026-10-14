import { isLocale, type Locale } from "@evinvest/i18n";

// The mount prefix. The cabinet is served at invest.evinvest.ltd/cabinet (a Traefik
// PathPrefix route) beside the invest landing, which owns the rest of the host.
//
// `basePath` in next.config.ts carries the prefix for everything the router builds:
// `<Link>`, `router.push`, `/_next/*`, rewrites, headers and the proxy matcher. Pages sit
// under it with the locale first: `/cabinet/ru/invest`. What the router does NOT prefix —
// `fetch`, a plain `<a href>`, `window.location` — goes through the helpers below, picked
// by whether the URL is handed to the router or to the browser.
export const BASE_PATH = "/cabinet";

/**
 * A BFF route, for `fetch`. Locale-free on purpose: the API answers in data, not prose,
 * and the one endpoint whose payload is language-dependent takes the locale as a parameter.
 */
export const apiPath = (path: `/${string}`): string => `${BASE_PATH}${path}`;

/** A page path for the router (`<Link>`, `router.push`), which adds `basePath` itself. */
export const cabinetPath = (locale: Locale, path: `/${string}`): string =>
  path === "/" ? `/${locale}` : `/${locale}${path}`;

/** The same page as a document URL, for a plain `<a href>` or `window.location`. */
export const documentPath = (locale: Locale, path: `/${string}`): string => `${BASE_PATH}${cabinetPath(locale, path)}`;

/**
 * The router path a request should have had, when its first segment is not a locale we
 * serve — or `null` when it already has one. Someone typing `/cabinet/invest` wants their
 * cabinet in a language we have, not a 404 from the root layout's `notFound()`, which has
 * no boundary above it and would render Next's built-in page instead of ours.
 */
export const localeRepairedPath = (pathname: string, locale: Locale): string | null => {
  const [, first] = pathname.split("/");
  if (isLocale(first)) return null;
  return pathname === "/" || pathname === "" ? `/${locale}` : `/${locale}${pathname}`;
};

/**
 * The page path the app reasons in — `/`, `/invest`, `/admin/fees` — from either a router
 * path (`/ru/invest`, what `usePathname()` returns) or a document path
 * (`/cabinet/ru/invest`, what `window.location` holds). A missing or unknown locale
 * segment is left alone rather than thrown on: the nav highlighting a wrong row is
 * recoverable, a hook that throws while rendering the shell is not.
 */
export const zonePathname = (pathname: string): `/${string}` => {
  const routed = pathname === BASE_PATH ? "/" : pathname.startsWith(`${BASE_PATH}/`) ? pathname.slice(BASE_PATH.length) : pathname;
  const [, first, ...rest] = routed.split("/");
  const path = isLocale(first) ? `/${rest.join("/")}` : routed;
  return (path === "" ? "/" : path) as `/${string}`;
};

/**
 * Requests under the prefix that are NOT pages: Next's build output and the BFF. None of
 * them may be locale-redirected or gated behind the session. The proxy matcher excludes
 * them too; this is the same rule in code, so correctness does not hang on a regex in a
 * config object that nothing tests.
 */
const NON_PAGE = /^\/(?:_next\/static\/|_next\/image|api\/|favicon\.ico$)/;

/** @see NON_PAGE — `pathname` is basePath-relative, as the proxy sees it. */
export const isNonPagePath = (pathname: string): boolean => NON_PAGE.test(pathname);

/**
 * The same cabinet page in another locale — path, query and hash intact — as a document
 * URL for `window.location`.
 */
export const relocalise = (
  locale: Locale,
  url: { pathname: string; search: string; hash: string },
): string => documentPath(locale, zonePathname(url.pathname)) + url.search + url.hash;
