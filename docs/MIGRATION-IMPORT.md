# Data-wave import (plan §6.5, step 3)

`invest-core import` loads banking's export into invest: the registry rows into Postgres and
the units into invest's own TigerBeetle. banking's exporter is built to this file. Schema
version **1**.

```
invest-core import --file <path.jsonl> [--dry-run] [--anchor <slug>=<user-uuid>:<units>]...
```

| Exit | Meaning |
| ---- | ------- |
| 0 | imported (or dry-run clean); every reconciliation check is ok or unverified |
| 1 | the reconciliation found a mismatch |
| 2 | the file was refused (parse/validation error, conflict) or the run failed |

stdout carries a table and then the same report as JSON (`mode`, `ok`, `allocations`,
`checks`). Statuses: `ok`, `mismatch` (exit 1), `unverified` (the file cannot support the
check; printed, not failed).

## Modes

- `--dry-run`: parses, validates and reconciles **the file against itself**. Reads no
  configuration, opens no connection, writes nothing. Run it on any machine first.
- Real run: needs the invest-core environment (`DATABASE_URL`, `TIGERBEETLE_ADDRESS`,
  `TIGERBEETLE_CLUSTER_ID`, plus the variables the config requires) and migration `0007`
  applied (start the new release once). It refuses to write a file whose own reconciliation
  fails; otherwise it writes, then reads the ledger back and reconciles again.

## Idempotence

Table `import_ledger(kind, natural_id, content_hash, imported_at)`. A re-run imports
nothing it already imported. The same `(kind, natural_id)` with another `content_hash`
(canonical JSON of the line, key order irrelevant) is a **hard error**, nothing is written:
fix the export or decide by hand. Postgres rows commit in one transaction with their ledger
rows. Units are posted as `Dr <holder> / Cr SharesOutstanding` (transfer code 47, the in-kind
issuance leg) under ids derived from the `natural_id`, so a crashed run re-posts harmlessly.
A row that already exists in invest but was not imported through this ledger fails the
transaction (no upsert). The two reserved allocations `fee` and `fund` exist from migration
`0001`; their records must carry banking's ids and are not rewritten.

## File

UTF-8, JSON Lines, no blank lines. Unknown fields, kinds, schema versions and malformed
lines are errors reported with their line number; all errors are collected, none skipped.

Line 1, the header:

```json
{"schema":1,"exported_at":"2026-10-16T12:00:00Z","source":"banking","source_sha":"<banking commit>"}
```

Then one record per line: `{"kind": …, "natural_id": …, <fields>}`. Rules for all fields:
UUIDs lowercase hyphenated; timestamps RFC 3339 **UTC with `Z`**; amounts and units are
**integer strings of 18-dp base units** (no sign, no leading zeros, fit 128 bits); every
other value obeys the CHECK of the column it lands in (see `core/migrations/0001`, `0002`).
`natural_id` is derived, and the importer rejects any other value:

| kind | extra discriminator | `natural_id` | lands in |
| ---- | ------------------- | ------------ | -------- |
| `allocation` | – | `<service>` | `allocations` |
| `policy` | `"policy":"fee"` | `fee:<service>` | `fee_policies` |
| `policy` | `"policy":"book"` | `book:<service>` | `book_policies` |
| `valuation` | – | `<id>` | `fund_valuations` |
| `fee_event` | `"event":"assessment"` | `assessment:<id>` | `fee_assessments` |
| `fee_event` | `"event":"settlement"` | `settlement:<id>` | `fee_settlements` |
| `share_holding` | – | the ledger logical key, e.g. `shares:<service>:<user>` | TigerBeetle |
| `ownership` | – | `<fee\|fund>:<user>` | checked only |

Fields (all required unless marked):

- `allocation`: `id`, `service`, `title`, `summary` (optional, default `""`), `state`
  (`draft|open|closed`), `unit_cap`, `icon`, `access`, `backing`, `scope`.
- `policy` fee: `service`, `management_bps` (≤500), `performance_bps` (≤5000), `hurdle_bps`,
  `basis`, `crystallization`, `updated_by`, `updated_at`, `version` (≥1), `effective_from`.
  Only for products of scope `invest`: a fee policy of another scope is refused.
  `policy` book: `service`, `book_open`, `taker_fee_bps`, `price_tick`, `lot_size`,
  `market_slippage_bps`, `allow_unbacked_trading`.
- `valuation`: `id`, `service`, `aum`, `units_outstanding` (>0), `nav`, `posted_by`,
  `posted_at`; optional `cash` — not a column: the allocation's cash in banking at the mark.
  The `fee` valuation should carry it, or its NAV check is `unverified`.
- `fee_event` assessment: `id`, `user_id`, `service`, `trigger_kind`, `nav`, `management`,
  `performance`, `debt_opening`, `charged_units`, `charged_cash`, `debt_carried`,
  `high_water_mark`, `assessed_at`. Settlement: `id`, `service`, `units`, `nav`, `cash`,
  `settled_by`, `settled_at`. History of non-invest products (service_arb under the old
  model) is allowed; only fee **units** are not.
- `share_holding`: `service`, `account_code`, `units`, and the holder:

  | `account_code` | account | holder fields |
  | -------------- | ------- | ------------- |
  | 60 | UserShares | `holder_user` |
  | 61 | SharesOutstanding | none |
  | 62 | FeeShares | `holder_allocation: "fee"` |
  | 64 | BookShares | `holder_user`, `units` must be `"0"` (orders are cancelled in step 1) |

  63 and 65 are retired and refused. Every service with holdings needs its 61 record.
  The `fee` and `fund` allocations are services like any other: their holders are 60 rows of
  service `fee` / `fund`.
- `ownership`: `allocation` (`fee|fund`), `holder_user`, `units` (>0). Must match the 60 rows
  of that allocation exactly (both directions); it writes nothing.

The importer writes **no** `fund_positions` (cost basis, high-water mark, fee debt),
`subscriptions`, `redemptions`, `book_*`, `fee_policy_changes` or `unit_issuances`: they are not
in schema 1. Add a kind (bump `schema`) before relying on them.

## Reconciliation

Per allocation, Σ units per account code in the file vs the ledger after the import (a dry
run uses the file for both sides, proving the file is internally sound). Checks:

- `supply_equals_holders_{file,after}`: `SharesOutstanding == ΣUserShares + ΣFeeShares + ΣBookShares`.
- `units_file_vs_after`: per allocation and account code. Units already sitting in invest's
  ledger on a file account show up here.
- `fee_shares_only_for_invest` (#17): a `FeeShares` record of a product whose scope is not
  `invest` is a **parse error** (banking burns them before the export); after the import every
  non-`invest` product the file or the database knows must have 0 fee units.
- `fee_nav_recomputed` (#17): NAV of `fee` = (`cash` of its latest valuation + Σ fee class of
  each invest product × that product's latest valuation NAV) / units held in `fee`; must equal
  the file's latest `fee` valuation NAV exactly. Missing mark or cash → `unverified`.
- `ownership_matches_holdings`, and the anchors below.
- `--anchor service_arb=<user-uuid>:13000` (repeatable, decimal units): that holder's
  `UserShares` equals the amount in the file **and** after the import. Names are never baked
  in; the runbook passes them.
