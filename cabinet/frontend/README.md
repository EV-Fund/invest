# invest cabinet — frontend

The investor cabinet at **`invest.evinvest.ltd/cabinet`**: Next.js (App Router) with
`basePath: "/cabinet"`, `@evinvest/uikit` for every control and the app shell, and
`@evinvest/i18n` in five locales. The landing owns the rest of the host
(EV-Fund/landing); a Traefik `PathPrefix(/cabinet)` route sends this prefix here.

Imported from the banking cabinet (EV-Main/banking `cabinet/frontend` at
`4f18800b`) and cut down to what invest owns (ORG-RESTRUCTURE-PLAN §6, D3, D4, D8,
D11). Money stays banking's: no withdrawals, payments, treasury, outbox or users
screens here — those live in the central admin on evinvest.ltd.

## Routes

All under `/cabinet/{locale}`; a request without a locale is redirected into one.

| Path | What |
|---|---|
| `/` | Portfolio — positions across allocations, performance |
| `/invest`, `/invest/[service]` | Allocations: the catalog, one product, subscribe and redeem |
| `/invest/[service]/trade` | The order book terminal for one product |
| `/wallet` | Banking's wallet element, `scope="invest"` (`@evinvest/wallet`) |
| `/account` | Concierge's account element (`@evinvest/concierge`) |
| `/admin/allocations`, `/admin/allocations/[service]` | Allocation registry; panel access per allocation |
| `/admin/valuation`, `/admin/fees`, `/admin/revenue` | Valuation and redemptions, fee terms, fund revenue |
| `/login` | Not a page: redirects to the `returnTo` page with `?login`, which opens sign-in |

The console (`/admin/*`) is shown when the session's `user.isAdmin` is true — the
BFF derives it from the caller's `<scope>:admin` / `<scope>:operator` seats, since
an admin of any scope that owns an allocation manages it here (§2). The investor
pages need `invest:self:invest`. Both are cosmetic gates
(`shared/config/route-permissions.ts`); the BFF is the boundary.

## Sign-in and session

- **Guest:** every page renders; the route gate shows a sign-in wall in the dimmed
  shell. "Sign in" sets `?login`, which mounts concierge's `<mfe-concierge-sign-in>`
  (`features/auth`), loaded from the registry at
  `https://evinvest.ltd/api/auth/mfe/registry.json`.
- **After sign-in** (`ev:signed-in`) the page navigates to the BFF's RP login,
  `/cabinet/api/auth/login?return_to=<this page>`. The BFF runs concierge's
  `authorize` → `code` → callback for the RP client `invest` and sets the session
  cookie. An OAuth round trip (Google, GitHub) returns to the same RP login.
- **Session:** the BFF's `invest_session` cookie (`__Host-` in production) is the
  only credential this app forwards; `GET /cabinet/api/auth/session` returns the
  principal (`SessionInfo` in `shared/contracts/admin.ts`) and refreshes the tokens
  behind it. CSRF: double-submit, cookie `invest_csrf`, header `x-ev-csrf`.
- **Sign-out** from the account element: `POST /cabinet/api/auth/logout`, then
  concierge's confirm page ends the evinvest.ltd session and returns to
  `https://invest.evinvest.ltd/` — so the `invest` scope's `return_paths` in
  concierge must list `/`.

## What the invest BFF must serve

This app is the frontend only; the Rust BFF is not written yet. Everything below
`/cabinet/api/*` is rewritten to `CABINET_BACKEND_URL` and must be answered there,
in the shapes the banking cabinet BFF uses today:

| Path | Origin today | Notes |
|---|---|---|
| `/api/auth/login`, `/api/auth/callback`, `/api/auth/logout`, `/api/auth/session` | new | RP session for client `invest` |
| `/api/funds/*`, `/api/allocations*` | banking BFF | positions, NAV, fee policy, subscriptions, redemptions |
| `/api/book/*`, `/api/book/ws` | banking BFF | order book, candles, orders, fills; websocket |
| `/api/admin/allocations/*`, `/api/admin/valuation/*`, `/api/admin/fees/*`, `/api/admin/revenue` | banking BFF | the allocation console |
| `/api/wallet` | banking BFF | read only: the balance available to deal with |
| `/api/kyc/status`, `/api/kyc/start` | concierge (site-root) | relayed with the RP session's credential |
| `/api/users` | banking BFF | the profile the KYC gate reads |
| `/api/admin/users` | banking BFF | the user picker in issue/grant forms |
| `/api/admin/tenants/{ns}/grants` | banking BFF → concierge | panel access per allocation |
| `/api/consilium/holder-grant`, `/api/owners*` | banking BFF | to concierge approvals (§4.4) |
| `/api/platform` | banking BFF | maintenance / read-only / announcement banner |

## Generated files

`shared/contracts/gen/**` is generated — **do not edit it by hand**. It was copied
as is from banking `cabinet/frontend/shared/contracts/gen` at `4f18800b`, where
`@hey-api/openapi-ts` emits it from banking's `contracts/openapi.json`
(`openapi-ts.config.ts`). Until invest publishes its own OpenAPI, regenerate in
banking and copy the output; `npm run gen:api` here expects the spec at
`../../contracts/openapi.json`, which this repo does not have yet.

`messages/en/common.json` and `i18n-slices.json` are generated from the code
(`npm run i18n:extract`, `npm run i18n:slices`).

## Develop

```sh
npm ci
cp .env.example .env.local     # CABINET_BACKEND_URL is required
npm run dev                    # http://localhost:3000/cabinet
npm run check && npm run lint && npm run i18n:check
npm run build
```

Without a BFF every page still renders (as a guest, behind the sign-in wall);
`/cabinet/api/*` answers 500 from the failed rewrite.

## i18n

Five locales (`en` `ru` `vi` `fr` `de`) through `@evinvest/i18n`, under the same
translation policy as the public site: English is canonical, and a translation
whose English source has since changed is refused and falls back to English.

English is written where it renders: every call is `t("key", "English sentence")`
with both arguments literal, and `messages/en/common.json` is generated from the
code by `npm run i18n:extract` — do not edit it by hand. A wire value that picks
its words (a state, a role) goes through a table of literal `t()` calls looked up
by the value (`shared/lib/wire-words.ts`), never a key assembled at runtime: the
extractor refuses one, since it has no English to extract.

`npm run i18n:check` is two gates. `evinvest-i18n-check` fails when the committed
English catalogue no longer matches the code, or on drift. The cabinet's own
`scripts/i18n-check.mts` then holds every locale at 100 % coverage and refuses a
translation identical to its English. The second checker
runs under `tsx` rather than node's `--experimental-strip-types` (which the unit
tests use) because it imports `shared/config/i18n.ts`, and that module loads the
catalogues through the `@/messages/...` tsconfig path alias, which bare node
does not resolve.

**The locale is the first segment under the prefix** — `/cabinet/ru/invest`. A
request without one (`/cabinet`, an old link) is redirected by `proxy.ts` to the
locale in the `ev_locale` cookie — the same cookie the landing on this host
writes — else `Accept-Language`, else English. `currentLocale()`
(`shared/config/locale.ts`) reads the segment through `next/root-params`.

Catalogues live in `messages/<locale>/common.json`. English is a plain map; every
other locale records the English each entry was translated from, which is what
makes drift detectable:

```jsonc
{ "nav.wallet": { "en": "Wallet", "t": "Кошелёк" } }
```

Only the navigation is translated so far. The banking vocabulary in it —
*Treasury*, *Valuation & redemptions* — wants a native review before any locale
is offered to a real investor.

## Observability

Wired through the shared `@evinvest/*` libraries; every integration no-ops until
its env var is set, so local dev needs no configuration.

- **Analytics** (`@evinvest/analytics`) — `PostHogProvider` in
  [`application/providers.tsx`](./application/providers.tsx); capture from client
  components with `useCapture()` / `useAnalytics()`. The key and host come from
  `config.public.posthogKey` / `posthogHost` (`NEXT_PUBLIC_POSTHOG_KEY` / `_HOST`
  through `config.ts`) and are passed to the provider as props — its own env
  fallback reads `process.env[name]` dynamically, which Next never inlines into
  the browser bundle. Both are build-time values (`NEXT_PUBLIC_*`); the host also
  feeds the CSP `connect-src`. What the shared
  seam lacks lives in [`shared/analytics`](./shared/analytics): the activation
  funnel's event names (`login_view`, `session_created`, `kyc_started`,
  `kyc_completed`, `first_deposit`, `first_subscription` — a contract with the
  site and the PostHog funnel), the `once`/`mark` idempotency marks
  (sessionStorage per tab, localStorage per "first ever"), and `identity` — the
  one hand-rolled vendor call, `posthog.identify(userId)` on the first
  authenticated render, which waits for the provider's lazy `init` before it
  fires. That exception ends when `@evinvest/analytics` grows `identify()`.
- **Error monitoring** (`@evinvest/error-monitoring`) — `ErrorMonitoringProvider`
  (browser) in providers; server/runtime init + request-error capture in
  [`instrumentation.ts`](./instrumentation.ts). No build-time integration — see the
  note at the end of [`next.config.ts`](./next.config.ts). Reads
  `NEXT_PUBLIC_SENTRY_DSN` (browser, required in production) / `SENTRY_DSN` (server).
- **Experiments** (`@evinvest/experiments`) — the A/B registry lives in
  [`application/experiments.ts`](./application/experiments.ts); sticky variant
  assignment runs in [`proxy.ts`](./proxy.ts). Read a variant in a Server
  Component with `getVariant`, render with `ExperimentTracker` (bridge `onEvent`
  to `useCapture`). Empty until the first experiment is declared.

See [`.env.example`](./.env.example) for the full env surface.

## Data

Every browser→BFF read goes through the cache in `shared/lib/resource.ts`. A view
does **not** call an entity client from a `useEffect`; it declares what it reads
and the cache decides whether that costs a request.

```ts
const wallet = useResource(walletResource);          // no argument
const nav = useResource(fundNavResource, service);   // keyed per fund
```

Why it exists: every screen used to own its own `useEffect(() => fetchX().then(setX))`,
so leaving a page threw its answer away. Moving between two screens of the same
account re-fetched the same balance and preceded each arrival with a skeleton —
which reads as loading a *different* account. Nothing was wrong with any one of
those reads; the problem was that none of them was shared.

Why not Next's `fetch` extension: `next: { revalidate, tags }` is a **server**
extension, applied to fetches Next issues while rendering. This cabinet reads
little on the server — `/cabinet/api/*` is a rewrite straight to the invest BFF
(`next.config.ts`), there are no route handlers, and most reads are browser calls
carrying the user's session cookie (a few pages seed their first HTML through
`shared/api/server/bff.ts`). So the cache is client-side, but keeps Next's
vocabulary — `revalidate` in seconds, `tags`, and a `revalidateTag()` — because
the semantics are the same.

Rules:

- **Declare each read once**, in `entities/<x>/model/<x>-resource.ts`, with the
  window it stays fresh for and the tags its mutations move. Views import from
  `model/`, never from `api/`.
- **A mutation names what it moved**, it does not refetch. A subscription names
  `wallet · positions · fees`, so the figures on *every* open surface follow — no call
  site has to know which screens are mounted. A write whose response IS the new
  state (`saveProfile`, `postValuation`) publishes it rather than invalidating.
- **`isLoading` is the only state that earns a skeleton.** It is true only when
  there is nothing to show and nothing has failed. Cached data renders on the
  first frame and refreshes behind itself; see the `Settled` rule below, which
  this pairs with exactly.
- **A failed refresh never blanks a figure.** The stale value stays and the error
  is reported beside it. Only a read that has *never* succeeded should surface as
  an error state.
- **`persist: true` is for non-personal data only** — the fund catalog and NAVs.
  A balance, a position, a profile or an operation stays in memory, which dies
  with the page. Sign-out clears both halves.
- **Warm ahead of the click.** `application/prefetch.ts` maps each route to the
  reads it makes; the rail and the tab bar warm the target on pointer or keyboard
  intent, and the shell warms the shared reads at idle. That is what makes the
  *first* visit to a screen skeleton-free too.

## Motion

Everything animated in the cabinet comes from `shared/ui/motion`. Import the
primitives from the slice root; do **not** import `motion/react` in a view. The
point of the slice is that curves, durations and travel distances are decided
once — in `shared/ui/motion/tokens.ts` — rather than per screen, exactly as
colour is decided once in the uikit's variables.

| Primitive | Use for |
|---|---|
| `Settled` | a skeleton handing over to the content it stood in for |
| `Panel` + `PanelPresence` | a popup/drawer/result card mounting and unmounting |
| `PanelSwap` | the content inside an already-open panel changing record |
| `Reveal` | a single block arriving on mount |
| `Stagger` + `StaggerItem` | a screen, list or grid arriving in sequence |
| `AnimatedNumber` | a figure travelling to its new value instead of being replaced |

Rules:

- **This is a money surface.** Motion exists to say *what changed* — never to
  decorate, and never in a way that delays a figure landing on screen. The
  cabinet's `DUR.base` is deliberately about half the landing's.
- **`opacity` and `transform` only**, always once, always collapsing to a plain
  fade under `prefers-reduced-motion` (every primitive handles this; hand-written
  motion must call `useReducedMotion()` itself).
- **A screen arrives as its sections, and the sections animate in place.** Put
  `Stagger` on the container the screen already has — the grid, the flex column
  — and give each section `as` so `StaggerItem` renders the element that was
  already there. Never wrap a section in a new `div`: the cabinet's sections are
  grid and flex items carrying their own placement (`xl:col-start-1`,
  `lg:order-3`, `flex-1`), and a wrapper takes that placement for itself and
  changes the layout. `SECTION_STAGGER` is the step for cards, `STAGGER` for
  rows. A component passed to `as` has to spread its unnamed props — `ref` and
  `style` — onto its DOM node, which is how motion reaches it; the uikit
  primitives do.
- **Two entrances never compose on one element.** A skeleton handover, or a
  nested `Reveal`, that happens while the section around it is still arriving
  keeps its fade and drops its travel — otherwise the content sets off again as
  the card is settling and the card stutters. Sections publish this through
  `shared/ui/motion/entrance`; `Settled`, `Reveal` and `StaggerItem` all read
  it, so it is automatic and there is nothing to pass down.
- **Chrome does not animate on navigation.** The rail, the tab bar and the
  system banner stay put; the entrance belongs to page content. The mobile app
  bar is the exception, and only because it *is* the page's title.
- **A figure lands without an animation frame.** `AnimatedNumber` counts on
  `requestAnimationFrame`, which a hidden tab never gets — so the count alone
  would leave a page opened in the background reading `$0.00` until it was
  brought to the front (#346). Its driver (`shared/ui/motion/number-driver.ts`)
  writes the final figure by whichever comes first: the count completing, the
  page going hidden, or a timer just past `DUR.slow`. Hand-written motion that
  gates a figure on a frame needs the same guarantee.
- **A figure that arrived in the server's HTML does not count in.** `AnimatedNumber`
  starts from 0 only for a figure the browser draws; one hydrated from a server read
  (`useSeededResource`) has been on screen since the HTML landed, and counting it
  up would take it away from the reader. Same for a screen's entrance: a page that
  renders server reads passes `entrance={false}` for them, and no arrival plays in a
  streaming fallback, which is never hydrated (`shared/lib/use-hydrated`).
- **`Settled` does not animate when no skeleton was shown.** Data already present
  on the first render cuts straight in — fading it would invent a delay the data
  never had. It decides by adjusting state *during render*, not in an effect: an
  effect would paint the content opaque once and only then start the fade, and
  that flash is worse than the cut it replaced.
- **A panel that swaps records is two motions, not one.** Key `Panel` on whether
  the panel is open — never on which record it shows, or every row click plays a
  full exit and enter — and put `PanelSwap` inside for the record change. The
  allocation side panel (`views/admin/allocations`) is the reference.
- **Alerts and result cards that replace one another share one
  `PanelPresence`.** A submit that fails should swap review → error in place
  rather than emptying the column and refilling it. See `views/invest/deal-panels`.
- **Nav markers are the kit's.** The rail and the tab bar are uikit `ShellNav` and
  `BottomTabBar`; their markers move as the kit decides, not per app.
- Note the eslint guard on arbitrary Tailwind values applies here too — motion
  values live in props and tokens, not in class strings.
