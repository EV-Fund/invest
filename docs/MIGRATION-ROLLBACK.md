# Rolling back past a migration

invest-core and the cabinet BFF apply their migrations at boot with `sqlx::migrate!().run(pool)`
and the default migrator (`core/src/infrastructure/db.rs`, `cabinet/backend/src/db.rs`). The default
refuses a database that records a version the binary does not know: sqlx returns
`MigrateError::VersionMissing`, and the pod exits at boot. So **an image rollback alone never works
once a newer migration has run**, whatever the migration's own comment says. ev-fund has no automatic
rollback (EV-Main/gitops#124), so the pods crash-loop until a person acts.

The comments in `core/migrations/0009_*.sql`, `0010_*.sql` and `cabinet/backend/migrations/0003_*.sql`
say "roll back the image only; the old binary runs on it". That is true of the SQL (the old code reads
and writes the new schema correctly) and false of the boot. Merged migrations are immutable, so this
file carries the correction.

## Before a release with migrations

1. Take or confirm a database backup.
2. After the rollout, save the new ledger rows with every column (`checksum` included):
   `SELECT * FROM public._sqlx_migrations WHERE version >= <first new>;` and the same for
   `cabinet._sqlx_migrations`. A roll-forward after a rollback needs them back.

## v0.3.0 → v0.2.0 (core 0009, 0010; BFF 0003)

Do these before the image goes back to v0.2.0.

**BFF** (schema `cabinet`, full undo; both objects hold only minutes-old data):

```sql
DROP TABLE cabinet.reauth_proofs;
ALTER TABLE cabinet.sign_ins DROP COLUMN reauth_for;
DELETE FROM cabinet._sqlx_migrations WHERE version = 3;
```

**Core** (keep the schema; the v0.2.0 SQL runs on it):

```sql
DELETE FROM public._sqlx_migrations WHERE version IN (9, 10);
UPDATE fee_policy_notices SET given_up_at = now(), last_error = 'image rolled back'
 WHERE delivered_at IS NULL AND given_up_at IS NULL AND withdrawn_at IS NULL;
```

- Do not revert 0009 itself: its old CHECK needs a `consilium_id` on every owner row, and owner
  changes closed before they were proposed have none. A full undo is the backup.
- v0.2.0 has no approvals round: withdraw any open `invest:fee_policy` proposal in concierge by
  hand, or cancel the change in the invest admin (cancel works in v0.2.0).
- v0.2.0 has no notice sender: the `UPDATE` above gives the pending notices up, so an
  administrator's change can be acknowledged and an owner change withdrawn.

The cabinet frontend image has no migrations and rolls back on its own.

## Rolling forward again

INSERT the saved ledger rows 9 and 10 back into `public._sqlx_migrations` first. Otherwise 0009 runs a
second time and fails: its columns exist and the old constraint is gone. The BFF's 0003 runs again
cleanly after the full undo above.
