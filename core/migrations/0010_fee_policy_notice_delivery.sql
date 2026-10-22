-- invest delivers the holders' notice of a fee-policy change through concierge
-- (`ScopeInboxService.EmitScopeNotification`, topic `invest:fee-terms`).
--
-- A notice is now recorded undelivered, and the notice sender (`infrastructure::fee_notices`)
-- stamps `delivered_at` when concierge says it reached the holder, or `given_up_at` when
-- concierge says it cannot or the retries are spent. `fee_policy_notices` is that sender's
-- outbox, written in the same transaction as the change it is owed for.
--
--   - `attempts`: how many times the sender acted on the notice.
--   - `next_attempt_at`: when a notice that failed for a transient reason is tried again;
--     NULL means "now".
--   - `last_error`: why the last attempt failed, or why the notice was given up on.
--   - `from_*` on the change: the terms in force when the change was scheduled. The notice
--     states "from" and "to", and by the time it is sent the live row may already be the
--     new terms. All five are set together, or none (a row the previous binary wrote).
--
-- Compatible with the running binary: it writes every notice already given up on, which the
-- sender never picks up, and it neither reads nor writes the new columns; each has a default
-- or is nullable. No index on the pending notices: the table holds one row per holder of a
-- scheduled change, and a non-concurrent index build would lock it.
--
-- Rollback: roll back the image only; this schema stays and the old binary runs on it. The
-- old binary has no sender, so a notice the new one recorded undelivered stays undelivered:
-- a dearer change waits on it, and its operator cannot acknowledge it. After an image
-- rollback, give those notices up by hand, so an administrator's change can be acknowledged
-- and an owner change withdrawn:
--   UPDATE fee_policy_notices SET given_up_at = now(), last_error = 'image rolled back'
--    WHERE delivered_at IS NULL AND given_up_at IS NULL AND withdrawn_at IS NULL;

SET lock_timeout = '3s';
SET statement_timeout = '30s';

ALTER TABLE fee_policy_notices
    ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    ADD COLUMN next_attempt_at TIMESTAMPTZ,
    ADD COLUMN last_error TEXT;

ALTER TABLE fee_policy_changes
    ADD COLUMN from_management_bps INTEGER CHECK (from_management_bps >= 0),
    ADD COLUMN from_performance_bps INTEGER CHECK (from_performance_bps >= 0),
    ADD COLUMN from_hurdle_bps INTEGER CHECK (from_hurdle_bps >= 0),
    ADD COLUMN from_basis TEXT CHECK (from_basis IN ('invested_capital', 'market_value')),
    ADD COLUMN from_crystallization TEXT CHECK (from_crystallization IN ('monthly', 'quarterly', 'semi_annual', 'annual')),
    ADD CONSTRAINT fee_policy_change_from_is_whole CHECK (
        num_nonnulls(from_management_bps, from_performance_bps, from_hurdle_bps, from_basis, from_crystallization) IN (0, 5)
    );

COMMENT ON COLUMN fee_policy_notices.attempts IS 'How many times the notice sender acted on this notice.';
COMMENT ON COLUMN fee_policy_notices.next_attempt_at IS 'When the sender tries a notice that failed for a transient reason again; NULL = now.';
COMMENT ON COLUMN fee_policy_notices.last_error IS 'Why the last attempt failed, or why the notice was given up on.';
COMMENT ON COLUMN fee_policy_changes.from_management_bps IS 'The terms in force when the change was scheduled (all five from_* columns, or none) — the "from" of the holders'' notice.';
