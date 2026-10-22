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
| `SCOPE_LEDGER_TLS_CA_PEM_FILE`, `SCOPE_LEDGER_TLS_CERT_PEM_FILE`, `SCOPE_LEDGER_TLS_KEY_PEM_FILE`, `SCOPE_LEDGER_TLS_DOMAIN` | unset | mTLS to banking; the three files are required with the address. The `typ=service` bearer is not configured: banking mints it for the certificate (`ScopeAuthService/MintServiceToken`, ~300 s) and invest renews it before it lapses |
| `CONCIERGE_GRPC_ADDR` | unset | concierge's gRPC (`http://…:55670`, or its TLS listener `https://…:55672`), pulled for the `users` projection; unset ⇒ the projection does not run and nobody is known to invest |
| `CONCIERGE_CLIENT_ID` | `invest` | the relying-party client invest reads the streams as |
| `CONCIERGE_CLIENT_SECRET` | unset | secret; concierge's `RP_CLIENT_SECRET_INVEST`; required with the address |
| `CONCIERGE_TLS_CA_PEM_FILE`, `CONCIERGE_TLS_DOMAIN` | unset | the CA (required for `https://`) and the name of concierge's TLS listener |
| `CONCIERGE_CATALOG_GRPC_ADDR` | unset | optional; where the `invest:*` permission catalog is published (`http://…:55670`). concierge mounts `AuthService.PublishCatalog` on its open listener only, so with `CONCIERGE_GRPC_ADDR` on the TLS listener the publish is `Unimplemented`; set this to the open port. Unset ⇒ the catalog goes over `CONCIERGE_GRPC_ADDR`. The client secret crosses the cluster unencrypted on it. Streams and the scope catalog stay on `CONCIERGE_GRPC_ADDR` |
| `SENTRY_DSN` | unset | secret; optional — unset ⇒ errors are not reported (a startup WARN in production). One Sentry project (`invest`) serves the core, the BFF and the cabinet; each reports under its own `service` name |
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

At boot invest-core also publishes its permission catalog to concierge
(`AuthService.PublishCatalog`, same credential), retrying with backoff while concierge is
unreachable: concierge resolves `invest:*` grants only against it.

| Catalog entry | What |
| ------------- | ---- |
| `invest:allocation:manage` | open, close, size and admit to a product; mint and retire units; settle redemptions; fee terms |
| `invest:valuation:post` | post a product's AUM |
| alias `invest:admin` | both; the scope's admin seat, granted only by the owners' `concierge:scope_admin_grant`; may grant `invest:operator` |
| alias `invest:operator` | `invest:valuation:post` |

There is no "may use invest" permission: concierge grants a member nothing by default, so
an investor is admitted by the projection (known, active), not by a permission. The
version is `CATALOG_VERSION` (`core/src/ports/catalog.rs`) — bump it with every change.
The scope catalog (`ScopeCatalogService.PublishScopeCatalog`: approval kinds and
notification topics) is published the same way at `SCOPE_CATALOG_VERSION`. concierge refuses
a version older than the one it stores, so an older image cannot publish after a newer one.

## Fee changes the owners decide

A change of fee terms that makes them dearer and leaves the house envelope (more than 2 %
management or 20 % performance, a basis on the market value, a crystallization more often
than yearly), or one that lowers the hurdle, is the platform owners' to approve
(`core/src/application/fee_approvals.rs`). Until invest can deliver holder notices, such a
change is refused on a fund anyone holds (FAILED_PRECONDITION), and the notice of a holder
who joins while the owners vote cannot be waived: the change does not bind over them.

1. `ScheduleFeePolicy` records it `awaiting_consilium`, with no notice yet, and proposes it as
   concierge's `invest:fee_policy` (PLATFORM level), deduped by `fee_policy_change:<id>`.
   `consilium_id` on the change is the concierge approval id.
2. Every 30 s the approvals round pulls the passed proposals (`PullApproved`), reads each
   again, checks its `payload_hash` against the STORED change, schedules the change — the
   holders' notices and the notice period start now — and reports `EXECUTED`. A hash that
   does not match rejects the change and reports `VOID`.
3. A proposal that is rejected, vetoed, expired, withdrawn in concierge or voided closes the
   change as `rejected`. `CancelFeePolicyChange` withdraws the proposal.

The round runs only when `CONCIERGE_GRPC_ADDR` is set; without concierge such a change is
refused. A step concierge does not answer is retried on the next round.

## Money flows

Every flow with a cash leg is a saga the relay carries across the two ledgers — the units
on invest's TigerBeetle, the cash through banking's scope ledger — each step idempotent by
a key derived from the aggregate (`core/src/infrastructure/relay/sagas.rs`):

| Flow | Cash in banking | Units in invest |
| ---- | --------------- | --------------- |
| Subscribe | `Reserve` on the subscriber's compartment (synchronous, TTL 1 h), then `Capture` into `(<scope>, alloc:<slug>)` | pending mint, posted after the capture; voided with the hold when banking refuses |
| Redeem | `Payout` from `(<scope>, alloc:<slug>)` at settle, only once the burn is reserved; past the automatic ceilings (`approval_required`) a `RequestPayout` instead, the redemption `awaiting_approval` until banking's verdict | pending burn at request — admitted only for units free and not claimed by an undrained event — posted after the payout (or the executed approval); voided when banking refuses or the owners reject |
| Book buy | `Reserve` before the order is written; each fill `Capture`s to the seller, the release `Void`s the rest | sells escrow units, and only escrowed orders of active holders are matched; a fill is pending until its cash moved, and one that cannot settle is busted — the seller's units back, the buy's hold voided; a buy whose hold banking voids or expires on its own comes off the book `rejected` |
| Fees | the taker fee (at most 1 %) goes `alloc:<slug>` → `(<scope>, fee)`; a fee-class settlement (invest's scope only) goes `(invest, alloc:<slug>)` → `(invest, fee)`. banking empties `(invest, fee)` into `(invest, alloc:fee)` and every other scope's collector into `(main, treasury)` (D24(3)) | an invest product's charge moves units into its fee class, held by the `fee` allocation; a settlement reserves their burn before the sweep and posts or voids it after; one settlement per product at a time, idempotent by its key. A product in another scope (`sa`) is never charged in units, takes no fee terms and has nothing to settle |

Opening a product (`SetAllocationState open`) opens its `alloc:<slug>` account in banking
first. A saga step banking does not serve yet (UNIMPLEMENTED) retries from the same outbox
row.

What banking does on its own reaches invest through its money-event log
(`scope.v1.MoneyEvents`), pulled by one consumer under an advisory lock
(`core/src/infrastructure/money_events.rs`): a voided or expired hold under a resting buy,
and the owners' verdict on a requested payout (read back with `GetPendingOperation`, and
re-asked every minute for redemptions still awaiting). It runs whenever `SCOPE_LEDGER_ADDR`
is set — same listener, same credentials. `failure_reason` on a subscription or a redemption
is a machine code: banking's refusal as banking names it, its verdict on an approval, or one
of invest's own (`units_unavailable`, `money_ledger_unavailable`).

Not on banking's side yet (cf38ba57): the automatic sweep of `(<scope>, fee)` that ledger.proto promises — cash sits on the collector until banking runs it, so `(invest, alloc:fee)` receives nothing, and no scope's fee reaches the treasury.

Not here yet: delivering holder notices of a fee change (recorded as given up; a dearer
change binds once its requester acknowledges), and a reaper
for a buy's hold whose `Reserve` answer was lost (`application/book.rs`).
banking's `ownership_contract` suite pins banking's own migration 0047 over its production
history and has no counterpart here.
