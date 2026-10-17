-- The data-wave importer's own bookkeeping (`invest-core import`, docs/MIGRATION-IMPORT.md).
--
-- One row per imported record, keyed by what the exporter calls the record (`kind`,
-- `natural_id`). It is what makes a re-run a no-op, and what turns a record that changed
-- between two exports into a hard error instead of a silent second write.
--
-- Additive: a new table nothing else reads. Rollback: DROP TABLE import_ledger (the imported
-- rows themselves stay; only the idempotency memory goes).

SET lock_timeout = '3s';
SET statement_timeout = '30s';

CREATE TABLE import_ledger (
    kind         TEXT NOT NULL CHECK (kind IN ('allocation', 'policy', 'valuation', 'fee_event', 'share_holding', 'ownership')),
    natural_id   TEXT NOT NULL CHECK (char_length(natural_id) BETWEEN 1 AND 200),
    content_hash TEXT NOT NULL CHECK (char_length(content_hash) = 32),
    imported_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, natural_id)
);
COMMENT ON TABLE import_ledger IS 'One row per record the data-wave importer applied. content_hash is the canonical-JSON digest of the record; the same natural_id with another hash is refused.';
