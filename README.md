# invest

EV invest: allocations, the units ledger, the investor cabinet.

invest is the allocation engine of EV (plan §6): the registry of investable products, the
units investors hold in them — kept in invest's own TigerBeetle cluster — the in-kind
issuance of units, who holds what, and the price of a unit (NAV). Cash is not here: it is
banking's, on scope accounts invest reads and moves through banking's
`scope.v1.ScopeLedgerService`.

The code was moved from banking at `9ffafce3` (first commit of this repository), not
rewritten.

## Layout

| Path | Crate | What |
| ---- | ----- | ---- |
| `domain/` | `invest_domain` | Pure, wasm-safe domain: allocations, issuance, the unit chart of accounts, money types, permissions |
| `contracts/` | `invest_contracts` | `invest.v1` protos (`AllocationsService`, `FundsService`, `BookService`, `FeesService`) → tonic stubs, plus the `allocation` and `book` facades |
| `core/` | `invest-core` | The server: gRPC, use cases, Postgres, the TigerBeetle unit ledger, the outbox relay and its money sagas, the fee sweeper, the money port |
| `core/migrations/` | — | sqlx migrations, applied on boot |
| `cabinet/backend/` | `invest-cabinet-backend` | The cabinet's BFF: sign-in as concierge's relying party `invest`, server-side sessions, `/cabinet/api/*` → invest-core ([README](cabinet/backend/README.md)) |
| `cabinet/frontend/` | — | The investor cabinet at `invest.evinvest.ltd/cabinet` (Next.js): routes, sign-in and the BFF contract in its [README](cabinet/frontend/README.md) |

## Run

Everything runs inside the dev shell (`nix develop`); `nix run .#help` lists the apps.

```bash
nix run .#dev           # shared Postgres + invest's TigerBeetle (:3034) + invest-core (:50071)
nix run .#db            # the shared Postgres + the `invest` database
nix run .#tb            # invest's TigerBeetle on :3034 (data under .tb/)
nix run .#invest-core   # invest-core alone
nix run .#cabinet-backend  # the cabinet BFF (:50072) alone; needs .#db
nix run .#cabinet-frontend # the cabinet, next dev on :3000/cabinet, against the BFF
nix run .#check-rust    # clippy (warnings denied) + tests against a throwaway Postgres + TigerBeetle
nix run .#check-frontend   # the cabinet's tsc + eslint + i18n gates
```

Tests by hand, against your own services:

```bash
nix run .#db & nix run .#tb &
DATABASE_URL=postgres://postgres@127.0.0.1:5432/invest_test \
TIGERBEETLE_ADDRESS=127.0.0.1:3034 CI=true \
  nix develop -c cargo test --workspace
```

`CI=true` turns a missing Postgres or TigerBeetle into a failure instead of a skip. The suites
create `invest_test_template` and one `invest_test_<suite>` database next to the base one;
drop them when done.

## Develop

The cabinet end to end — invest-core, the BFF and the frontend — in three terminals:

```bash
nix run .#dev               # 1: shared Postgres + TigerBeetle (:3034) + invest-core (:50071)
nix run .#cabinet-backend   # 2: the BFF on :50072; migrates the `cabinet` schema
nix run .#cabinet-frontend  # 3: http://localhost:3000/cabinet (npm ci on first run)
```

What each needs from outside this repo:

- **Signing in** needs concierge (`~/projects/ev/concierge`, gRPC :55670) with the client
  `invest` registered, and its secret in the BFF's env: `CONCIERGE_CLIENT_SECRET` =
  concierge's `RP_CLIENT_SECRET_INVEST` (`cabinet/backend/.env`). Without concierge the BFF
  still starts, every page renders as a guest, and `/cabinet/api/health/live` answers.
- **Cash** — a subscription's hold, a buy's escrow — needs banking's scope ledger
  (`SCOPE_LEDGER_ADDR` for invest-core, `core/.env`). Unset, invest-core starts and refuses
  every cash leg.
- **People and permissions** reach invest-core only through concierge's streams
  (`CONCIERGE_GRPC_ADDR` + secret in `core/.env`); without them nobody holds an `invest:*`
  permission and the console stays hidden.

`next dev` serves `http://localhost:3000`, which is the BFF's dev `CABINET_PUBLIC_ORIGIN`
— the only `Origin` its mutations accept — and both run without `__Host-`/Secure cookies
(`AUTH_COOKIE_SECURE=false`). The frontend's own notes: [cabinet/frontend](cabinet/frontend/README.md).

## Ports

| Port | What |
| ---- | ---- |
| 50071 | invest-core gRPC (`invest.v1.*`, `grpc.health.v1`) |
| 50072 | the cabinet BFF's HTTP (`/cabinet/api/*`, `/cabinet/auth/callback`) |
| 50073 | the cabinet's Next server in its image (`next dev` keeps :3000) |
| 3034 | invest's TigerBeetle in dev (banking's is 3033) |
| 5432 | the shared ev_invest Postgres, database `invest` (the BFF owns its `cabinet` schema) |

## Configuration

invest-core reads the environment only; `core/.env.example` documents every variable, and
`invest-core --print-required-vars [profile]` prints the ones a deployment must set.

| Variable | Default | |
| -------- | ------- | - |
| `DATABASE_URL` | — | Postgres; migrations run on boot |
| `GRPC_ADDR` | `127.0.0.1:50071` | gRPC listener |
| `TIGERBEETLE_ADDRESS`, `TIGERBEETLE_CLUSTER_ID` | — | invest's own cluster |
| `AUTH_ISSUER` | `https://auth.concierge.ev` | issuer of accepted access tokens |
| `AUTH_AUDIENCE` | `invest` | concierge's relying-party client for invest |
| `AUTH_JWKS_GRPC_ENDPOINT` | — | concierge gRPC, for its JWKS |
| `SCOPE_LEDGER_ADDR` | unset | banking's scope ledger (`https://…:50054`); unset ⇒ cash reads are unavailable |
| `SCOPE_LEDGER_TLS_CA_PEM_FILE`, `SCOPE_LEDGER_TLS_CERT_PEM_FILE`, `SCOPE_LEDGER_TLS_KEY_PEM_FILE`, `SCOPE_LEDGER_TLS_DOMAIN` | unset | mTLS to banking |
| `SCOPE_LEDGER_SERVICE_TOKEN` | unset | secret; `typ=service`, `sub = invest` |
| `CONCIERGE_GRPC_ADDR` | unset | concierge's gRPC (`http://…:55670`, or its TLS listener `https://…:55672`), pulled for the `users` projection; unset ⇒ the projection does not run and nobody is known to invest |
| `CONCIERGE_CLIENT_ID` | `invest` | the relying-party client invest reads the streams as |
| `CONCIERGE_CLIENT_SECRET` | unset | secret; concierge's `RP_CLIENT_SECRET_INVEST`; required with the address |
| `CONCIERGE_TLS_CA_PEM_FILE`, `CONCIERGE_TLS_DOMAIN` | unset | the CA (required for `https://`) and the name of concierge's TLS listener |
| `SENTRY_DSN` | unset | required in production |
| `APP_ENV` | — | `development` / `production` |

## People

invest has no accounts. Who a person is, whether they are suspended and which `invest:*`
permissions their seat carries come from concierge: invest-core pulls concierge's
`UserEvents.PullUserLifecycle` and `ScopeEvents.PullScopeEvents` with the relying-party
credential of the client `invest` and keeps the `users` projection
(`core/src/infrastructure/concierge.rs`). The pull engine is concierge's
`evconcierge_projection` crate: cursors, dedupe, per-user order, and an event of a kind this
build cannot name is parked by position and replayed on the next start, never skipped. One
invest-core pulls at a time, under a session advisory lock.

| Event | `users` |
| ----- | ------- |
| `MEMBER_JOINED` (scope `invest`, or one that names `invest` an observer) | the row, `auth_subject`, `status` (`disabled` ⇒ `suspended`) |
| lifecycle `SUSPENDED` / `REINSTATED` | `status` |
| `GRANTS_CHANGED` in scope `invest` | `permissions`, replaced whole |

A suspended person holds no permission and is minted nothing; banking refuses their cash.
Not kept yet: other scopes' grants (`sa:*`), scope state, the token revoke floor.

## Money flows

Every flow with a cash leg is a saga the relay carries across the two ledgers — the units
on invest's TigerBeetle, the cash through banking's scope ledger — each step idempotent by
a key derived from the aggregate (`core/src/infrastructure/relay/sagas.rs`):

| Flow | Cash in banking | Units in invest |
| ---- | --------------- | --------------- |
| Subscribe | `Reserve` on the subscriber's compartment (synchronous, TTL 1 h), then `Capture` into `(<scope>, alloc:<slug>)` | pending mint, posted after the capture; voided with the hold when banking refuses |
| Redeem | `Payout` from `(<scope>, alloc:<slug>)` at settle | pending burn at request, posted after the payout; voided when banking refuses |
| Book buy | `Reserve` before the order is written; each fill `Capture`s to the seller, the release `Void`s the rest | sells escrow units; a fill is pending until its cash moved |
| Fees | the taker fee and a fee-class settlement go `alloc:<slug>` → `(<scope>, fee)`, which banking forwards to `(invest, alloc:fee)` from every scope | a charge moves units into the product's fee class; a settlement burns them |

Opening a product (`SetAllocationState open`) opens its `alloc:<slug>` account in banking
first. banking serves only `OpenAccount`, `GetBalances` and `Transfer` today; a saga step
it does not serve yet retries from the same outbox row.

Not here yet: the owners' approval of a fee change beyond the house envelope (refused until
invest is a client of concierge's approvals), delivering holder notices of a fee change
(recorded as given up; a dearer change binds once its requester acknowledges), and
`RequestPayout` for a redemption past banking's automatic ceilings (fails the redemption).
banking's `ownership_contract` suite pins banking's own migration 0047 over its production
history and has no counterpart here.
