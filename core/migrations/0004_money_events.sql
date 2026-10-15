-- banking's money events (S2-18): invest-core pulls `scope.v1.MoneyEvents` and acts on what
-- banking did on its own — a hold voided or expired under a resting buy, the owners'
-- verdict on a payout past the automatic ceilings.
--
-- A redemption whose payout banking answers `approval_required` is asked for instead
-- (`RequestPayout`) and waits `awaiting_approval` on the operation banking stored, its burn
-- still pending; the operation's verdict completes or fails it.
--
-- The consumer's bookkeeping: one cursor (the money plane's position, global and gappy),
-- the events it acted on (dedupe by banking's event id, written in the transaction that
-- acts, with the cursor), and the events it parked (a kind this build cannot name; banking
-- asks consumers to park those and move on).
--
-- No backfill: no row of these tables exists anywhere yet (S2-7: no production invest
-- database), so the plain index and the swapped CHECK cost nothing.
--
-- Rollback: drop the tables, the index and the column, and restore the old CHECK — only
-- with no redemption `awaiting_approval` (the previous release cannot read that state).

SET lock_timeout = '3s';
SET statement_timeout = '30s';

ALTER TABLE redemptions
    DROP CONSTRAINT redemptions_state_is_known,
    ADD CONSTRAINT redemptions_state_is_known CHECK (state IN ('queued', 'settling', 'awaiting_approval', 'completed', 'failed', 'cancelled')),
    ADD COLUMN payout_operation_id TEXT,
    ADD CONSTRAINT redemptions_awaiting_names_its_operation CHECK (state <> 'awaiting_approval' OR payout_operation_id IS NOT NULL);
CREATE UNIQUE INDEX redemptions_payout_operation_idx ON redemptions (payout_operation_id) WHERE payout_operation_id IS NOT NULL;
COMMENT ON COLUMN redemptions.payout_operation_id IS 'banking''s operation for a payout past the automatic ceilings (scope.v1 RequestPayout); set once, when the redemption went awaiting_approval.';
COMMENT ON COLUMN redemptions.failure_reason IS 'A machine reason: banking''s refusal of the payout (insufficient_funds, user_frozen, …), its verdict on a requested one (approval_rejected, approval_expired, proposal_refused, …), or units_unavailable. NULL for an operator''s fail and every other state.';

CREATE TABLE money_events_cursor (
    singleton  BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    position   BIGINT NOT NULL CHECK (position >= 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
COMMENT ON TABLE money_events_cursor IS 'The last MoneyEvents position the consumer is done with (PullResponse.next_position). One row.';

CREATE TABLE money_events_applied (
    event_id   TEXT PRIMARY KEY,
    position   BIGINT NOT NULL,
    kind       TEXT NOT NULL,
    ref_id     TEXT NOT NULL,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
COMMENT ON TABLE money_events_applied IS 'Money events the consumer acted on, by banking''s event id — a redelivery finds its id here and does nothing.';

CREATE TABLE money_events_parked (
    event_id  TEXT PRIMARY KEY,
    position  BIGINT NOT NULL,
    kind      INTEGER NOT NULL,
    scope     TEXT NOT NULL,
    reason    TEXT NOT NULL,
    parked_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
COMMENT ON TABLE money_events_parked IS 'Money events the consumer could not read (a kind this build cannot name). Re-pulled by position on every start and applied once the build knows them.';
