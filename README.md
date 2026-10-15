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
| `contracts/` | `invest_contracts` | `invest.v1` protos (`AllocationsService`, `FundsService`) → tonic stubs, plus the `allocation` facade |
| `core/` | `invest-core` | The server: gRPC, use cases, Postgres, the TigerBeetle unit ledger, the outbox relay, the money port |
| `core/migrations/` | — | sqlx migrations, applied on boot |
| `cabinet/frontend/` | — | The investor cabinet (separate PR) |

## Run

Everything runs inside the dev shell (`nix develop`); `nix run .#help` lists the apps.

```bash
nix run .#dev           # shared Postgres + invest's TigerBeetle (:3034) + invest-core (:50071)
nix run .#db            # the shared Postgres + the `invest` database
nix run .#tb            # invest's TigerBeetle on :3034 (data under .tb/)
nix run .#invest-core   # invest-core alone
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
| 3034 | invest's TigerBeetle in dev (banking's is 3033) |
| 5432 | the shared ev_invest Postgres, database `invest` |

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
| `SENTRY_DSN` | unset | required in production |
| `APP_ENV` | — | `development` / `production` |

## Not in this release (PR 1b)

Subscriptions and redemptions as sagas through banking's scope ledger, the fee plane, the
order book. Their tables are already in `0001`. The banking tests that drove them — and so
were not moved yet — are: from `allocation_registry`, `a_subscription_past_the_unit_cap_is_refused_before_any_money_moves`
and `narrowing_the_cap_below_the_issued_supply_stops_issuance_without_trapping_anyone`; from
`balance_allocations`, every deposit, withdrawal, subscription and redemption test; from
`ownership_fee`, the fee settlement, withdrawal-fee, redemption and cooldown tests. banking's
`ownership_contract` pins banking's own migration 0047 over its production history and has no
counterpart here.
