# Agent Instructions — invest

> **Self-update rule:** if anything here becomes stale — layout, tooling, conventions —
> update this file in the same change. It holds cross-cutting rules only; bring-up and the
> `nix run` apps live in [`flake.nix`](./flake.nix) and [`README.md`](./README.md).

The workspace-wide rules (`~/projects/ev/AGENTS.md`: our own libraries first, the uikit,
the design source) apply here too.

## What this repo is

The allocation engine (plan §6, `~/projects/ev/ORG-RESTRUCTURE-PLAN.md`): the registry of
investable products, the units in invest's **own** TigerBeetle cluster (D19), in-kind
issuance, ownership, NAV. Moved from banking `9ffafce3` — the first commit is the verbatim
copy; read `git log -p` against it before "fixing" something that looks odd, it may be
banking's on purpose.

| Path | What |
| ---- | ---- |
| `domain/` (`invest_domain`) | Pure, wasm-safe. No I/O, no clock, no `sqlx`/`tonic`/`tigerbeetle`. |
| `contracts/` (`invest_contracts`) | `proto/invest/v1/*.proto` → tonic stubs. The wire is banking's `banking.v1` shape under `invest.v1`. |
| `core/` (`invest-core`) | `services` (gRPC) → `application` (use cases) → `ports` → `infrastructure` (Postgres, TigerBeetle, banking). |
| `core` — `import` | `invest-core import --file …` is the one-shot data-wave importer (`application/import`, `infrastructure/import.rs`, `import_cmd.rs`); format and runbook notes in [`docs/MIGRATION-IMPORT.md`](docs/MIGRATION-IMPORT.md). Not part of `serve`. |
| `cabinet/backend/` (`invest-cabinet-backend`) | The cabinet's BFF (HTTP): concierge sign-in, sessions in the `cabinet` schema, routes → invest-core. Cut from banking's `cabinet/backend` at `9ffafce3`. |
| `cabinet/frontend/` | The cabinet (Next.js, `basePath: /cabinet`, its own npm package). Talks to the BFF alone; its wire types in `shared/contracts` are hand-written against `cabinet/backend/src/dto.rs` — change both in one PR. Gate: `nix run .#check-frontend`. |

## Invariants — what not to break

- **Units in TigerBeetle, never in Postgres.** Postgres holds the registry, the records, the
  outbox and projections; a balance is always read from the ledger. The ledger is written
  **last**, by the relay, after the control-plane commit.
- **Cash is banking's.** invest holds no cash and no cash ledger. Cash is read and moved
  through the `MoneyLedger` port (`core/src/ports/money.rs`) only. An allocation's cash
  lives on `(<scope>, alloc:<slug>)` in banking; the reserved `fee`/`fund` cash on
  `(invest, alloc:fee|fund)`.
- **Fees reach the `fee` holders from invest's own scope only (D24(3)).** A product whose
  scope is not `invest` (`sa`) is never charged in units, takes no fee terms and has no fee
  class to settle; its taker fee is swept to `(<scope>, fee)` and is banking's treasury's.
- **The numbers are banking's.** Ledger id 3, account codes 60–65, transfer codes,
  logical keys (`shares:<svc>:<user>` …), relay salts (`issue:mint` …) and the reserved
  allocation ids are kept as banking had them so the data wave moves over with its history
  readable. A code banking used — cash included — is never reused
  (`every_account_and_transfer_code_is_unique`).
- **Single relay.** The relay drains under a session `pg_advisory_lock`; do not add
  `SKIP LOCKED`. Parked rows leave the drain; a compensated row is never unparked.
- **A user is their concierge user id** (the `sub` of an `invest` access token). There is no
  local account; `users` is a projection (status, `invest:*` permissions, banking's
  `auth_subject`). No projection row ⇒ no permissions and no holding — fail closed.
- **The projection has one writer**: concierge's lifecycle and scope streams, pulled by
  `evconcierge_projection` under a session advisory lock (`infrastructure/concierge.rs`).
  Its bookkeeping (`concierge_projection_*`) is created at boot by the crate, versioned
  apart from `core/migrations/`; truncate it together with `users` or not at all.
- **Single money-event consumer.** banking's `MoneyEvents` are pulled under their own session
  advisory lock (`infrastructure/money_events.rs`); an event's effect, its dedupe row and the
  cursor commit in one transaction. A redemption's approval verdict is always read back with
  `GetPendingOperation` — the event is only the nudge.
- **A buy's hold may outlive its order only for a minute.** The hold reaper (`infrastructure/hold_reaper.rs`) voids `book_order:` holds with no order row once banking's `created_at` is older than `HOLD_REAPER_AFTER_SECS` (default 900, at least 60). Never age a hold by this process's clock.
- **Health stays outside auth.** `grpc.health.v1` is not wrapped in the auth layer.
- **The browser holds no token.** The cabinet BFF keeps concierge's tokens sealed (key
  derived from the cookie) in `cabinet.sessions`, keyed by the hash of the session cookie. Every mutating route calls
  `verify_csrf` (double-submit + `Origin`); the redirect URI is concierge's registered
  `/cabinet/auth/callback`, byte for byte.

## Database

- Migrations: `core/migrations/`, applied on boot (`db::migrate`). Author with the CLI in the
  dev shell: `sqlx migrate add --source core/migrations --sequential <name>`. A merged
  migration is immutable; fix forward.
- The cabinet BFF's own: `cabinet/backend/migrations/`, schema `cabinet`, history in
  `cabinet._sqlx_migrations` (apart from invest-core's). Same CLI, `--source
  cabinet/backend/migrations`.
- SQL is runtime `sqlx::query*` with `.bind()`, never the `query!` macros; every query must
  be executed by a test against a real database.

## Tests

- Integration suites (`core/tests/`) use real Postgres **and** TigerBeetle — no mocks of
  either. Banking's cash is the in-memory `MoneyLedger` fake
  (`infrastructure::money::fake`), never wired into a running server.
- Locally a missing `DATABASE_URL`/TigerBeetle **skips** a suite; with `CI=true` it fails.
  Run with `CI=true` and read the counts — a green run that skipped is not a run.
  `nix run .#check-rust` is the gate CI runs.

## Commits

`type(scope): description`, ≤72 chars, English, imperative. Commit from the dev shell
(`nix develop -c git commit …`): the pre-commit hook runs treefmt (nightly rustfmt) and
`git add -u`s what it formatted — check `git show --stat` afterwards.
