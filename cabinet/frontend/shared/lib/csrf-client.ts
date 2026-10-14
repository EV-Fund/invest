// Browser-side CSRF double-submit helper: read the readable `invest_csrf` cookie the
// invest BFF mints and build the header for mutating BFF requests. Standalone (no server
// imports, so no `COOKIES` and its config) so every slice can depend on it.

const CSRF_HEADER = "x-ev-csrf";

function readCookie(name: string): string | null {
  const prefix = `${name}=`;
  const found = document.cookie.split("; ").find((entry) => entry.startsWith(prefix));
  if (!found) return null;
  try {
    return decodeURIComponent(found.slice(prefix.length));
  } catch {
    // A malformed `%` sequence (a cookie another app on the parent domain wrote) would throw
    // out of every mutating request; no token is the failure the server already answers.
    return null;
  }
}

export function csrfHeader(): Record<string, string> {
  // Prefer the `__Host-` (host-locked) cookie so a subdomain-injected plain
  // `invest_csrf` can't shadow it; fall back to the dev (unprefixed) name.
  const token = readCookie("__Host-invest_csrf") ?? readCookie("invest_csrf");
  return token ? { [CSRF_HEADER]: token } : {};
}
