-- The writer of the `users` projection (PR 1c): invest-core pulls concierge's lifecycle and
-- scope streams with its relying-party credential (evconcierge_projection).
--
-- The engine's own bookkeeping — cursors, applied event ids, per-key sequences, parked
-- positions — lives in `concierge_projection_*`, which the crate creates and versions at
-- boot apart from this history (`db::migrate`). What this step adds is the one fact the
-- projection row needs that the engine cannot keep for it: the lifecycle sequence the
-- row's `status` is as of. A MEMBER_JOINED is a snapshot written on the scope stream, and
-- the scope stream may deliver it after a later SUSPENDED already landed from the
-- lifecycle stream; the row only takes a snapshot's status when it is at least as new as
-- what it holds, so a stale "active" can never undo a suspension.
--
-- Nullable, no backfill: a row written before this release (none in production — S2-7)
-- has no sequence and takes the next snapshot whatever it says.
--
-- Rollback: drop the column; the previous release neither reads nor writes it.

SET lock_timeout = '3s';
SET statement_timeout = '30s';

ALTER TABLE users ADD COLUMN lifecycle_sequence BIGINT CHECK (lifecycle_sequence >= 0);
COMMENT ON COLUMN users.lifecycle_sequence IS 'The concierge lifecycle sequence `status` is as of. NULL — no lifecycle fact seen yet; the next one applies.';
