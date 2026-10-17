-- A taker-fee RISE on a product's book is scheduled, not applied: it waits out a notice
-- period, the holders are owed a notice each, and the requester acknowledges the notices
-- that cannot be delivered. The same shape as `fee_policy_changes` / `fee_policy_notices`
-- (fund fee terms), kept in its own tables because those rows carry management and
-- performance terms a taker fee does not have. Additive: `book_policies` is untouched, the
-- live fee stays in `book_policies.taker_fee_bps` until a rise is promoted onto it.

CREATE TABLE book_taker_fee_changes (
    id                 UUID PRIMARY KEY,
    service            TEXT NOT NULL REFERENCES allocations (service) ON DELETE CASCADE,
    from_bps           INTEGER NOT NULL CHECK (from_bps BETWEEN 0 AND 10000),
    taker_fee_bps      INTEGER NOT NULL CHECK (taker_fee_bps BETWEEN 0 AND 10000),
    state              TEXT NOT NULL CHECK (state IN ('scheduled', 'applied', 'cancelled')),
    effective_from     TIMESTAMPTZ NOT NULL,
    requested_by       TEXT NOT NULL,
    requested_at       TIMESTAMPTZ NOT NULL,
    applied_at         TIMESTAMPTZ,
    closed_reason      TEXT,
    acknowledged_by    TEXT,
    acknowledged_at    TIMESTAMPTZ,
    acknowledged_users UUID[],
    CONSTRAINT book_taker_fee_changes_is_a_rise CHECK (taker_fee_bps > from_bps),
    CONSTRAINT book_taker_fee_changes_applied_is_stamped CHECK ((state = 'applied') = (applied_at IS NOT NULL)),
    CONSTRAINT book_taker_fee_changes_cancelled_says_why CHECK ((state = 'cancelled') = (closed_reason IS NOT NULL)),
    CONSTRAINT book_taker_fee_changes_acknowledgement_is_whole CHECK (num_nonnulls(acknowledged_by, acknowledged_at, acknowledged_users) IN (0, 3))
);
CREATE UNIQUE INDEX book_taker_fee_changes_single_pending_idx ON book_taker_fee_changes (service) WHERE state = 'scheduled';
CREATE INDEX book_taker_fee_changes_due_idx ON book_taker_fee_changes (effective_from) WHERE state = 'scheduled';
CREATE INDEX book_taker_fee_changes_history_idx ON book_taker_fee_changes (service, requested_at DESC);
COMMENT ON TABLE book_taker_fee_changes IS 'Taker-fee rises of a product''s book, scheduled with a notice period. One pending per product; a promotion writes the fee onto book_policies. Cuts are immediate and leave no row here.';

CREATE TABLE book_taker_fee_notices (
    change_id    UUID NOT NULL REFERENCES book_taker_fee_changes (id) ON DELETE CASCADE,
    user_id      UUID NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    delivered_at TIMESTAMPTZ,
    given_up_at  TIMESTAMPTZ,
    withdrawn_at TIMESTAMPTZ,
    PRIMARY KEY (change_id, user_id),
    CONSTRAINT book_taker_fee_notices_one_outcome CHECK (num_nonnulls(delivered_at, given_up_at) <= 1)
);
COMMENT ON TABLE book_taker_fee_notices IS 'One notice per holder of a scheduled taker-fee rise. Undelivered = delivered_at IS NULL AND withdrawn_at IS NULL; given up = given_up_at IS NOT NULL.';
