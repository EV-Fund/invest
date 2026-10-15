# invest-cabinet-backend

The invest cabinet's **BFF**: the only server the browser at
`invest.evinvest.ltd/cabinet` talks to. It signs people in as concierge's relying party
`invest`, keeps the session server-side, and forwards each request to invest-core over
gRPC with the user's access token. It holds no money and no units.

Cut from banking's `cabinet/backend` at `9ffafce3` (the commit before this crate's first
change is the verbatim copy): the invest groups of routes, their DTOs and tests, the book
socket. What is not invest's — wallet, owners, consilium, KYC, platform, users — stayed in
banking or moves to concierge and the central admin (plan §6).

## Sign-in and session (S2-13)

| Step | Where | What |
| ---- | ----- | ---- |
| Login | `GET /cabinet/api/auth/login?return_to=/cabinet/…` | A state and a PKCE verifier are recorded in `cabinet.sign_ins` (by the state's hash); the state goes to the browser in an HttpOnly cookie; 302 to `https://evinvest.ltd/api/auth/authorize?client_id=invest&…`. `return_to` outside `/cabinet` is a 400. Silent when the evinvest.ltd session exists. |
| Callback | `GET /cabinet/auth/callback?code&state` | The URL's state must equal the cookie's (constant time) and its record must not be redeemed; then `ExchangeCode(code, verifier)`; a session row; the session and CSRF cookies; 303 to `return_to`. |
| Requests | `/cabinet/api/*` | The session is read by the SHA-256 of its cookie; an access token within 30 s of expiry is rotated first (`RefreshClientToken`), one rotation per session across replicas (a lease on the row). |
| Session | `GET /cabinet/api/auth/session` | The principal (`SessionInfo`, camelCase) or a guest — always 200 unless the answer is unknown (503). Rotates and re-reads `GetMe`. |
| Logout | `POST /cabinet/api/auth/logout` | CSRF + `Origin`; the session row deleted, the cookies cleared. concierge's confirm page ends the evinvest.ltd session. |

The callback is **`/cabinet/auth/callback`, without `/api`**: that is the redirect URI
concierge registered for the client (`runner/migrations/0031_scopes.up.sql`), and the
redirect URI must match byte for byte. The ingress (or the frontend's rewrites) must send
`/cabinet/auth/*` here, not to the Next app.

Cookies (`__Host-` prefixed and `Secure` in production, bare in dev):

| Cookie | HttpOnly | What |
| ------ | -------- | ---- |
| `invest_session` | yes | 256 random bits, hex. The database holds its hash and concierge's tokens; the tokens never reach the browser. `SameSite=Lax`, lives as long as concierge's refresh token. |
| `invest_csrf` | no | The double-submit token, echoed in `x-ev-csrf` on every mutation. |
| `invest_sign_in` | yes | The state of a sign-in in flight; ten minutes, cleared by the callback. |

Every mutating route (and the book socket's handshake) also requires
`Origin == CABINET_PUBLIC_ORIGIN`; a request without `Origin` is refused.

The console gates (`/cabinet/api/admin/*`) read the caller's `invest:*` permissions from
concierge's `GetMe`, cached a minute per session; invest-core re-checks every act against
its own projection. `SessionInfo.user.isAdmin` is `invest:allocation:manage` or
`invest:valuation:post`; `role` is `admin` with the first, `operator` with only the second,
`investor` otherwise.

## Routes

`/cabinet/api` + the frontend's `/api/*` path, in banking's shapes:

| Group | Routes | invest-core |
| ----- | ------ | ----------- |
| auth | `auth/login`, `auth/session`, `auth/logout`; `/cabinet/auth/callback` | — (concierge) |
| catalog | `allocations`, `allocations/detail` | `AllocationsService` |
| funds | `funds/nav`, `funds/nav/history`, `funds/positions`, `funds/fee-policy`, `funds/accrued-fees`, `funds/redemptions`, `funds/redemptions/cancel`, `funds/subscribe`, `funds/redeem` | `FundsService`, `FeesService` |
| book | `book`, `book/trades`, `book/candles`, `book/policy`, `book/orders` (GET, POST), `book/orders/cancel`, `book/orders/history`, `book/fills`, `book/ws` | `BookService` |
| console | `admin/allocations{,/register,/update,/state,/cap,/access,/backing,/holders,/book,/issue,/retire,/grants,/grants/grant,/grants/revoke}`, `admin/fees/{policies,policy,policy/cancel,policy/acknowledge-notices,changes,shares,settle,assessments}`, `admin/valuation/{queue,post,settle,fail}`, `admin/revenue` | all four |
| health | `health/live` (the probe), `health` (deep: invest-core + Postgres) | `grpc.health.v1` |

Changed against banking: `admin/revenue` is assembled from `GetAllocation`, `GetFundNav`
and `ListUnitHolders` of the `fee` allocation (invest-core has no `GetFundRevenue`);
e-mails beside user ids (`grants[].email`, `notices_waived_by_email`, the redemption
queue's `email`) are always empty — an `invest` token opens concierge's `GetMe` and nothing
else; `funds/subscribe` answers the saga's `state` and `failure_reason`;
`admin/allocations/register` forwards an optional `scope`. Not served:
`admin/valuation/override` and the consilium (owners' approval moves to concierge, S2-6).

## Run

```bash
nix run .#db                     # the shared Postgres + the `invest` database
nix run .#cabinet-backend        # :50072; migrates the `cabinet` schema on boot
```

The frontend's `CABINET_BACKEND_URL` is this server **with `/cabinet`**
(`http://127.0.0.1:50072/cabinet`): Next rewrites `/cabinet/api/:path*` to
`${CABINET_BACKEND_URL}/api/:path*`, and every route here lives under `/cabinet`.

Tests (Postgres required; concierge and invest-core are fakes):

```bash
DATABASE_URL=postgres://postgres@127.0.0.1:5432/invest_test CI=true \
  nix develop -c cargo test -p invest-cabinet-backend
```

The suite provisions `<database>_cabinet` next to `DATABASE_URL`'s database and recreates
it on every run.

## Configuration

`.env.example` documents every variable; `invest-cabinet-backend --print-required-vars
[profile]` prints what a deployment must set.

| Variable | Default | |
| -------- | ------- | - |
| `CABINET_BACKEND_BIND` | `127.0.0.1:50072` | HTTP listener |
| `DATABASE_URL` | — | the `invest` database; the BFF owns the `cabinet` schema (needs `CREATE` on the database for the first boot) |
| `DB_MAX_CONNECTIONS` | `5` | |
| `INVEST_CORE_GRPC_ADDR` | — | `http://…:50071` |
| `CONCIERGE_GRPC_ADDR` | — | `http://…:55670` |
| `CONCIERGE_CLIENT_ID` | `invest` | |
| `CONCIERGE_CLIENT_SECRET` | — | secret; concierge's `RP_CLIENT_SECRET_INVEST` |
| `CONCIERGE_PUBLIC_ORIGIN` | `https://evinvest.ltd` | where the browser authorizes |
| `CABINET_PUBLIC_ORIGIN` | — | `https://invest.evinvest.ltd`; the redirect URI's origin and the only accepted `Origin` |
| `AUTH_COOKIE_SECURE` | from `APP_ENV` | `__Host-` + `Secure` cookies; must match the frontend's |
| `APP_ENV` | — | `development` / `production` |
| `SENTRY_DSN` | — | required in production |

## Layout

| Module | Role |
| ------ | ---- |
| `config.rs` | the `ev::settings!` env surface — the deploy contract, pinned by tests |
| `session/` | sign-in state, sessions, rotation (`mod.rs`); their SQL (`store.rs`) |
| `concierge.rs` | the port to concierge (`ExchangeCode`, `RefreshClientToken`, `GetMe`); the adapter is `evconcierge_client` |
| `state.rs` | `AppState` and `Grpc`, the typed calls into invest-core |
| `cookies.rs` | cookie names, `Set-Cookie` values, the request's cookies |
| `dto.rs` | browser-facing JSON (snake_case; 64-bit values as strings) |
| `error.rs` | gRPC status → HTTP status + `{ "error": … }` |
| `routes/` | `auth`, `funds`, `book`, `book_ws` over `ws`, `admin`, `system` |
| `migrations/` | the `cabinet` schema; history in `cabinet._sqlx_migrations`, apart from invest-core's |
