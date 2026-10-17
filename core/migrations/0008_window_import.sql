-- The migration window's import (`invest-core window-import --apply`, docs/WINDOW-IMPORT.md).
--
-- One row, ever: which export file the window's rows came from and how far the import got.
-- `rows_written` — the registry rows are in (they commit together with this row, in one
-- transaction) and the units may still be missing from the ledger, which cannot join the
-- transaction; `complete` — the units were posted and the ledger read back equal to the file.
-- A re-run of the same file reads this row and writes nothing it has written; a different
-- file is refused, so a second export is never layered over the first.
--
-- Additive: a new table nothing else reads. Rollback: DROP TABLE window_import (the imported
-- rows stay; only the memory of which file they came from goes).

SET lock_timeout = '3s';
SET statement_timeout = '30s';

CREATE TABLE window_import (
    singleton              BOOLEAN PRIMARY KEY DEFAULT true CHECK (singleton),
    file_digest            TEXT NOT NULL CHECK (char_length(file_digest) = 32),
    source                 TEXT NOT NULL,
    exported_at            TEXT NOT NULL,
    tigerbeetle_cluster_id TEXT NOT NULL,
    state                  TEXT NOT NULL CHECK (state IN ('rows_written', 'complete')),
    started_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at           TIMESTAMPTZ,
    CONSTRAINT window_import_complete_is_stamped CHECK ((state = 'complete') = (completed_at IS NOT NULL))
);
COMMENT ON TABLE window_import IS 'The single export file the window import came from (digest of its bytes) and its progress. At most one row: a second, different export is refused.';
