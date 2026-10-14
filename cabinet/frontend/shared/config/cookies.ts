// Cookie names this app reads. The session and CSRF cookies are minted by the invest BFF
// (the RP session it opens after concierge's `authorize` round trip), never here; the
// proxy gates on `session` and `csrf-client.ts` reads `csrf` for the double-submit header.

import { config } from "../../config.ts";

// `__Host-` cookies require the Secure attribute, which browsers reject over plain
// http://localhost. So in dev (no Secure) the prefix is dropped; in production the
// cookies are `__Host-`-prefixed and Secure. Toggle explicitly with AUTH_COOKIE_SECURE,
// else infer from NODE_ENV — must match the BFF's setting.
const SECURE = config.authCookieSecure;
const PREFIX = SECURE ? "__Host-" : "";

export const COOKIES = {
  session: `${PREFIX}invest_session`,
  csrf: `${PREFIX}invest_csrf`,
  // The one cookie this app MINTS: the reader's locale, so an unprefixed entry resolves
  // to the language they were last reading. Same name as the landing on this host
  // writes, so the language a visitor read the landing in carries into the cabinet.
  locale: `${PREFIX}ev_locale`,
  // Minted by the browser too: the device's time zone, so a server render formats times
  // the way the browser will hydrate them. Carries no authority — see
  // `shared/lib/time-zone-name.ts`.
  timeZone: `${PREFIX}ev_tz`,
} as const;

// Next hands cookie values over percent-DECODED, so an encoded `;` or `=` would become a
// separator in an outgoing Cookie header, and a CR/LF or non-Latin-1 character makes fetch
// throw. The id's format is the BFF's; this only keeps an unusable value out of a header.
const SESSION_ID = /^[A-Za-z0-9_-]{16,256}$/;

/** Whether the session cookie holds something that can be forwarded at all. */
export function isSessionId(value: string | undefined): value is string {
  return value !== undefined && SESSION_ID.test(value);
}
