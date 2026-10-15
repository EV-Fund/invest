-- The money sagas (PR 1b): what invest keeps of a flow whose cash is banking's.
--
-- An allocation's cash sits on `(<scope>, alloc:<service>)` in banking (plan §3.3), and a
-- subscriber's money is held on their compartment in that same scope — so the registry
-- learns which scope a product lives in (S2-7). `fee`, `fund` and every product default to
-- `invest`; `service_arb` lives in `sa`. The UPDATE below is for a database that already
-- carries the row (the data wave, plan §6.5); a fresh one has none.
--
-- A subscription, a redemption and a buy order now cross two ledgers, so each row records
-- the saga's state and banking's hold; an order records when its escrow was in place, and a
-- trade the relay had to undo says so. No row of these tables exists in any database yet
-- (S2-7: no production invest database), so the NOT NULL columns need no backfill.
--
-- Holder notices of a fee-policy change get their own table: banking queued them in its
-- governance mail outbox, which stays in banking. invest has no channel to deliver them
-- yet, so a notice is recorded as given up the moment it is owed, and a dearer change
-- binds over its holders only once an operator acknowledges them
-- (`AcknowledgeUndeliveredNotices`). When concierge's notifications are wired, a notice
-- starts undelivered and the notifier stamps it.
--
-- Rollback: drop the columns and the table (no data outside this release writes them).

SET lock_timeout = '3s';
SET statement_timeout = '30s';

ALTER TABLE allocations
    ADD COLUMN scope TEXT NOT NULL DEFAULT 'invest' CHECK (scope ~ '^[a-z][a-z0-9_]{0,31}$'),
    ADD CONSTRAINT allocations_reserved_live_in_invest CHECK (service NOT IN ('fee', 'fund') OR scope = 'invest');
UPDATE allocations SET scope = 'sa' WHERE service = 'service_arb';
COMMENT ON COLUMN allocations.scope IS 'The concierge scope the product''s cash lives in: banking''s (<scope>, alloc:<service>) account, and the compartments subscribers pay from. Fixed at registration.';

ALTER TABLE subscriptions
    ADD COLUMN scope          TEXT NOT NULL,
    ADD COLUMN state          TEXT NOT NULL DEFAULT 'reserving' CHECK (state IN ('reserving', 'reserved', 'settled', 'failed')),
    ADD COLUMN hold_id        TEXT,
    ADD COLUMN failure_reason TEXT,
    ADD COLUMN updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD CONSTRAINT subscriptions_held_names_its_hold CHECK (state NOT IN ('reserved', 'settled') OR hold_id IS NOT NULL),
    ADD CONSTRAINT subscriptions_failed_says_why CHECK ((state = 'failed') = (failure_reason IS NOT NULL));
COMMENT ON COLUMN subscriptions.hold_id IS 'banking''s hold on the subscriber''s compartment (scope.v1 Reserve); the relay captures it into (scope, alloc:<service>).';

ALTER TABLE redemptions
    ADD COLUMN failure_reason TEXT,
    ADD CONSTRAINT redemptions_state_is_known CHECK (state IN ('queued', 'settling', 'completed', 'failed', 'cancelled'));
COMMENT ON COLUMN redemptions.failure_reason IS 'Why banking refused the payout at settle (scope.v1 refusal reason); NULL for an operator''s fail and every other state.';

ALTER TABLE book_orders
    ADD COLUMN scope       TEXT NOT NULL,
    ADD COLUMN hold_id     TEXT,
    ADD COLUMN escrowed_at TIMESTAMPTZ,
    ADD CONSTRAINT book_orders_buy_names_its_hold CHECK (side = 'sell' OR hold_id IS NOT NULL),
    ADD CONSTRAINT book_orders_buy_is_escrowed_when_written CHECK (side = 'sell' OR escrowed_at IS NOT NULL);
COMMENT ON COLUMN book_orders.hold_id IS 'A buy''s cash escrow: banking''s hold on the buyer''s compartment, captured by each fill and voided by the release.';
COMMENT ON COLUMN book_orders.escrowed_at IS 'When the order''s escrow was in place: a buy''s hold before it is written, a sell''s units once the relay posted the lock. The matcher fills only escrowed orders.';

ALTER TABLE book_trades
    ADD COLUMN busted_at   TIMESTAMPTZ,
    ADD COLUMN bust_reason TEXT,
    ADD CONSTRAINT book_trades_busted_says_why CHECK ((busted_at IS NULL) = (bust_reason IS NULL));
COMMENT ON COLUMN book_trades.busted_at IS 'The relay could not settle the trade before its units moved (banking refused the cash, an escrow was missing) and undid it: the seller''s units back, the buy off the book, its hold released.';

CREATE TABLE fee_policy_notices (
    change_id    UUID NOT NULL REFERENCES fee_policy_changes (id) ON DELETE CASCADE,
    user_id      UUID NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    delivered_at TIMESTAMPTZ,
    given_up_at  TIMESTAMPTZ,
    withdrawn_at TIMESTAMPTZ,
    PRIMARY KEY (change_id, user_id),
    CONSTRAINT fee_policy_notices_one_outcome CHECK (num_nonnulls(delivered_at, given_up_at) <= 1)
);
COMMENT ON TABLE fee_policy_notices IS 'One notice per holder of a scheduled fee-policy change. Undelivered = delivered_at IS NULL AND withdrawn_at IS NULL; given up = given_up_at IS NOT NULL.';
