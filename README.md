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
| `cabinet/frontend/` | — | The investor cabinet (separate PR) |

## Run

Everything runs inside the dev shell (`nix develop`); `nix run .#help` lists the apps.

```bash
nix run .#dev           # shared Postgres + invest's TigerBeetle (:3034) + invest-core (:50071)
nix run .#db            # the shared Postgres + the `invest` database
nix run .#tb            # invest's TigerBeetle on :3034 (data under .tb/)
nix run .#invest-core   # invest-core alone
nix run .#cabinet-backend  # the cabinet BFF (:50072) alone; needs .#db
nix run .#check-rust    # clippy (warnings denied) + tests against a throwaway Postgres + TigerBeetle
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

## Ports

| Port | What |
| ---- | ---- |
| 50071 | invest-core gRPC (`invest.v1.*`, `grpc.health.v1`) |
| 50072 | the cabinet BFF's HTTP (`/cabinet/api/*`, `/cabinet/auth/callback`) |
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
The scope catalog (approval kinds, notification topics) is published once invest opens a
proposal or emits a notice; it does neither yet.

## Money flows

Every flow with a cash leg is a saga the relay carries across the two ledgers — the units
on invest's TigerBeetle, the cash through banking's scope ledger — each step idempotent by
a key derived from the aggregate (`core/src/infrastructure/relay/sagas.rs`):

| Flow | Cash in banking | Units in invest |
| ---- | --------------- | --------------- |
| Subscribe | `Reserve` on the subscriber's compartment (synchronous, TTL 1 h), then `Capture` into `(<scope>, alloc:<slug>)` | pending mint, posted after the capture; voided with the hold when banking refuses |
| Redeem | `Payout` from `(<scope>, alloc:<slug>)` at settle, only once the burn is reserved | pending burn at request — admitted only for units free and not claimed by an undrained event — posted after the payout; voided when banking refuses |
| Book buy | `Reserve` before the order is written; each fill `Capture`s to the seller, the release `Void`s the rest | sells escrow units, and only escrowed orders of active holders are matched; a fill is pending until its cash moved, and one that cannot settle is busted — the seller's units back, the buy's hold voided |
| Fees | the taker fee (at most 1 %) and a fee-class settlement go `alloc:<slug>` → `(<scope>, fee)`, which banking forwards to `(invest, alloc:fee)` from every scope | a charge moves units into the product's fee class; a settlement reserves their burn before the sweep and posts or voids it after; one settlement per product at a time, idempotent by its key |

Opening a product (`SetAllocationState open`) opens its `alloc:<slug>` account in banking
first. banking serves only `OpenAccount`, `GetBalances` and `Transfer` today; a saga step
it does not serve yet retries from the same outbox row.

Not here yet: the owners' approval of a fee change beyond the house envelope (refused until
invest is a client of concierge's approvals), delivering holder notices of a fee change
(recorded as given up; a dearer change binds once its requester acknowledges), and
`RequestPayout` for a redemption past banking's automatic ceilings (fails the redemption).
banking's `ownership_contract` suite pins banking's own migration 0047 over its production
history and has no counterpart here.
