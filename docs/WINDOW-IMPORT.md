# Window import — banking's unit book into invest (schema version 1)

Step 3 of the migration window (org restructure plan §6.5): `invest-core window-import`
reads the file banking's `window-export` wrote (`docs/WINDOW-EXPORT.md` in banking, the
record types in `domain::window_export`) and loads it into invest's Postgres and its own
TigerBeetle. The file is parsed with the **same structs that wrote it**: `banking_domain`
is banking's `domain` crate, pinned by git rev in the workspace `Cargo.toml` (the head of
banking#552 until that is merged — then point the rev at `main`).

This is a different command from `invest-core import` (invest's own file format, see
`MIGRATION-IMPORT.md`); the two do not share a file, a table or an id namespace. Run one.

```
invest-core window-import --file export.jsonl \
  --anchor-holder <concierge_uuid>:13000 --anchor-holder <concierge_uuid>:3250     # dry-run (default)
invest-core window-import --file export.jsonl --anchor-holder … --anchor-holder … \
  --apply --expect-digest <sha256-hex>                                             # writes
```

The report prints the file's **SHA-256** (lower-case hex of the file's bytes, the value banking's
`close-share-accounts` checks too) and the `service_arb` **anchor** verdict (`OK`, `BROKEN` or
`n/a` when `service_arb` is not in the file). `--apply` is refused without `--expect-digest`, and
refused before any write when the file's digest differs from it.

The anchor: `--anchor-holder <concierge_uuid>:<units>` twice (valeratrades 13 000, inintel44
3 250). `service_arb` must hold exactly those holders with exactly those units, no book or fee
units, supply equal to their sum. No flags, a wrong amount or an extra holder is `BROKEN`; a
broken anchor makes the dry-run exit 1 and `--apply` refuse.

| Exit | Meaning |
| ---- | ------- |
| 0 | clean (dry-run: the file is importable; apply: the ledger read back equal to the file) |
| 1 | the anchor is BROKEN (dry-run), or apply wrote and the read-back of the ledger disagrees with the file |
| 2 | the file was refused (every defect listed with its line) or the run failed |

stdout carries a text report (Σ of units per allocation, every holder, notes, warnings, and
after an apply every check) and then the same as JSON.

## What is refused

Nothing is written unless the whole file is sound. The file is refused when:

- `schema_version` is not `1` (checked first, on line 1, before any typed parse);
- the header says `live: true` (an `--allow-live` rehearsal), `operations_read_only: false`
  or a `source` other than `banking`;
- the trailer is missing or not last, says `clean: false` or `outbox_undispatched > 0`, or its
  `counts` differ from the lines present (a lost or doubled line);
- a line does not parse, or a number is not a decimal digit string that fits 128 bits;
- an allocation's `user_shares + book_shares + fee_shares` is not its `shares_outstanding`
  (recomputed from the holdings, then compared with the trailer's `allocations[]`);
- a holder has no `concierge_user_id` (invest knows people by their concierge id only), or two
  banking users map to one concierge user;
- `book_shares` are non-zero: units resting in sell orders. Orders are not exported; cancel
  them in banking (plan step 1) and re-export;
- fee-class units sit in a product outside scope `invest`, or in `fee` / `fund` themselves
  (D24(3));
- the reserved `fee` / `fund` allocations carry other ids than migration `0001`'s;
- the `ownership` view of `fee` / `fund` disagrees with the holdings;
- a `position.units` differs from the ledger holding (the projection and the ledger disagree:
  fix banking first);
- a sum overflows 128 bits.

`--apply` additionally refuses, before the first write, when: the digest differs from
`--expect-digest`; the header's `tigerbeetle_cluster_id` equals this process's
`TIGERBEETLE_CLUSTER_ID` (the file would then not come from banking's ledger); the anchor is
BROKEN; or invest's ledger already holds units of any product in the file that are neither
nothing nor exactly the file's state (supply and every holding). A run that crashed halfway
through posting units therefore needs a look by hand, not a blind re-run.

## What `--apply` writes

One Postgres transaction: `allocations` (not the reserved two, which exist from `0001`),
`fee_policies`, `fee_policy_changes`, `book_policies`, `fund_valuations`, `fund_positions`,
`fee_assessments`, `fee_settlements` — plus one row in `window_import` (migration `0008`),
the SHA-256 of the file and its progress. Plain INSERTs: a row that is already there fails
the transaction and nothing is written.

Then the units, through the `Ledger` port: `Dr <holder> / Cr SharesOutstanding(service)`
(transfer code `UnitIssue`) under ids derived from the account's logical key. Then the
read-back: every holder's units, the supply and any units already in the ledger that the file
does not name are compared with the file; a difference exits 1 and leaves `window_import` at
`rows_written`, so every re-run verifies again. Only a matching read-back marks `complete`.

Idempotence: the same file again writes no row and posts no transfer (it re-reads the ledger
and reports `already_applied`). A different file is refused — one window, one export.

## Decisions the importer takes (and where the file is silent)

- **Scope.** The export has none: `service_arb` lands in scope `sa`, everything else in
  `invest` (`application::window_import::scope_of`, plan §6.5 step 4).
- **Fee terms of a non-`invest` product** (`fee_policy`, `fee_policy_change` of
  `service_arb`) are not imported — such a product takes no fee terms (D24(3)). The report
  says how many records were left out. Its fee **history** (`fee_assessment`,
  `fee_settlement`) is imported.
- **Ids.** Every banking user id in `position`, `fee_assessment`,
  `fee_policy_change.notices_waived_users` and every holder is replaced by the concierge id
  from the file's `user` records. `consilium_id` is kept as a plain reference (no foreign
  key; the consilium itself stays out, S2-6).
- **`book_policy.allow_unbacked_trading`** is not in schema 1: it starts `false`.
- **The `users` projection** is concierge's to write. A holder not in it yet is a warning:
  until the stream delivers them they see no holding.

## Not imported

Subscriptions, redemptions, book orders and trades, `unit_issuances`, per-investor
`allocation_access_grants`, the consilium, owners / holder-grant / seed capital (S2-6). They
are not in schema 1.

## Test anchor

`service_arb` holders — valeratrades 13 000 and inintel44 3 250, Σ 16 250 — are in the dry-run
report and in the apply's `holding_after` checks; `core/tests/window_import.rs` pins them on a
fixture. These are living clients' units: **never run `--apply` against production without
the dry-run's report in hand and the window open.**
