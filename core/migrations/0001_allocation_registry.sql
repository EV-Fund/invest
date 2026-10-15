-- invest's control plane, from scratch (S2-5).
--
-- A snapshot of the allocation-side tables banking reached at 9ffafce3 (its migrations
-- 0005, 0010, 0021–0023, 0027, 0033, 0034, 0036–0042, 0045, 0047), written as the final
-- schema rather than replayed as history. No data comes with it: the units, the rows and
-- the TigerBeetle accounts move over in the final wave (plan §6.5).
--
-- Differences from banking's copy, each because invest has no history to keep:
--   - `users` is invest's own projection of the people concierge knows, keyed by the
--     concierge user id (the `sub` of an `invest` access token), not banking's account.
--   - `tb_accounts` has no `network`: invest's ledger holds units, and units have no rail.
--   - The `unit_issuances` CHECKs banking added NOT VALID (over pre-#245 rows) are plain
--     CHECKs here.
--   - `fee_policy_changes.consilium_id` keeps its column and its CHECK but loses the
--     foreign key: the consilium stays out of invest (S2-6), and the owners' approval it
--     names arrives from concierge.
--
-- The subscription, redemption, fee and book tables land now, unused by this release,
-- so the code that writes them (PR 1b) needs no schema of its own and reads the same
-- snapshot.
--
-- Rollback: a fresh database — drop it. Nothing here alters existing data.

SET lock_timeout = '3s';
SET statement_timeout = '30s';

-- ── people ──────────────────────────────────────────────────────────────────

CREATE TABLE users (
    id           UUID PRIMARY KEY,
    status       TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'suspended')),
    permissions  TEXT[],
    auth_subject TEXT UNIQUE CHECK (length(auth_subject) BETWEEN 1 AND 256),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
COMMENT ON TABLE users IS 'The people concierge told invest about, keyed by their concierge user id. Written by the projection of concierge''s user stream; read to admit a holder and to gate a caller.';
COMMENT ON COLUMN users.permissions IS 'The invest:* permissions concierge resolved for the seat. NULL — concierge has not said — holds nothing.';
COMMENT ON COLUMN users.auth_subject IS 'The subject banking names the person''s compartment by (scope.v1). NULL until the projection learns it.';

-- ── event log, outbox, ledger plumbing ─────────────────────────────────────

CREATE TABLE event_log (
    seq          BIGSERIAL PRIMARY KEY,
    event_id     UUID NOT NULL UNIQUE,
    aggregate    TEXT NOT NULL,
    aggregate_id UUID NOT NULL,
    kind         TEXT NOT NULL,
    payload      JSONB NOT NULL,
    occurred_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX event_log_aggregate_idx ON event_log (aggregate, aggregate_id);

CREATE TABLE outbox (
    seq            BIGSERIAL PRIMARY KEY,
    event_id       UUID NOT NULL UNIQUE,
    aggregate      TEXT NOT NULL,
    aggregate_id   UUID NOT NULL,
    kind           TEXT NOT NULL,
    payload        JSONB NOT NULL,
    occurred_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    dispatched_at  TIMESTAMPTZ,
    attempts       INTEGER NOT NULL DEFAULT 0,
    last_error     TEXT,
    parked_at      TIMESTAMPTZ,
    compensated_at TIMESTAMPTZ
);
CREATE INDEX outbox_undispatched_idx ON outbox (seq) WHERE dispatched_at IS NULL AND parked_at IS NULL;
CREATE INDEX outbox_parked_idx ON outbox (parked_at) WHERE parked_at IS NOT NULL;

CREATE TABLE saga_steps (
    event_id       UUID NOT NULL,
    leg            INTEGER NOT NULL,
    role           TEXT NOT NULL,
    tb_transfer_id BYTEA NOT NULL UNIQUE,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (event_id, leg)
);

CREATE TABLE tb_accounts (
    logical_key   TEXT PRIMARY KEY,
    tb_account_id BYTEA NOT NULL UNIQUE,
    ledger        INTEGER NOT NULL,
    code          INTEGER NOT NULL,
    flags         INTEGER NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ── the registry ───────────────────────────────────────────────────────────

CREATE TABLE allocations (
    id         UUID PRIMARY KEY,
    service    TEXT NOT NULL UNIQUE CHECK (service ~ '^[A-Za-z0-9_-]{1,64}$'),
    title      TEXT NOT NULL CHECK (length(title) BETWEEN 1 AND 120),
    summary    TEXT NOT NULL DEFAULT '' CHECK (length(summary) <= 280),
    state      TEXT NOT NULL CHECK (state IN ('draft', 'open', 'closed')),
    unit_cap   TEXT NOT NULL DEFAULT '100000000000000000000000000' CHECK (unit_cap ~ '^[1-9][0-9]*$'),
    icon       TEXT NOT NULL DEFAULT 'fund'
               CHECK (icon IN ('fund', 'real_estate', 'trading', 'yield', 'venture', 'treasury', 'commodity', 'credit', 'index', 'arbitrage')),
    access     TEXT NOT NULL DEFAULT 'view' CHECK (access IN ('hidden', 'view', 'invest')),
    backing    TEXT NOT NULL DEFAULT 'cash' CHECK (backing IN ('cash', 'in_kind')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX allocations_open_idx ON allocations (service) WHERE state = 'open';
COMMENT ON COLUMN allocations.unit_cap IS 'Authorised unit supply in 18-dp base units. Subscribe refuses to mint past it; redemptions are unaffected.';
COMMENT ON COLUMN allocations.icon IS 'Catalog glyph the client maps onto one of its SVGs. Presentation only — gates neither money nor lifecycle.';
COMMENT ON COLUMN allocations.access IS 'Default access level — hidden | view | invest. Who sees the product and who may subscribe, before per-user grants. Orthogonal to state; never gates redemptions.';
COMMENT ON COLUMN allocations.backing IS 'cash | in_kind — whether the fund''s cash stands behind the units. Redeem is refused on in_kind (holders exit through the book). Flipped to in_kind by the first in-kind mint; back to cash only by SetAllocationBacking.';

-- The platform's own allocations (#245): `fee` holds every product's fee class, `fund`
-- the platform's capital, both held by people through units and hidden from every
-- catalog. Their ids are banking's — uuid5(NAMESPACE_OID, 'evbanking:allocation:<slug>')
-- — so the data wave lands on the same rows (`invest_domain::allocations::FEE_ALLOCATION_ID`).
INSERT INTO allocations (id, service, title, summary, state, access, backing)
VALUES
    ('680240f5-1c41-58e2-987c-e52dbe2faa01', 'fee',  'Fee allocation',  'The platform''s earned fees, held by its people.', 'open', 'hidden', 'cash'),
    ('7533b905-9200-5ffb-b053-844c9c8da99a', 'fund', 'Fund allocation', 'The platform''s own capital, held by its people.', 'open', 'hidden', 'cash')
ON CONFLICT (service) DO NOTHING;

CREATE TABLE allocation_access_grants (
    service    TEXT NOT NULL REFERENCES allocations (service),
    user_id    UUID NOT NULL REFERENCES users (id),
    level      TEXT NOT NULL CHECK (level IN ('view', 'invest')),
    granted_by UUID NOT NULL REFERENCES users (id),
    granted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (service, user_id)
);
COMMENT ON TABLE allocation_access_grants IS 'Per-investor exceptions to allocations.access: the investor holds max(default, level). Audit facts live in event_log under the allocation aggregate.';

-- ── price and positions ────────────────────────────────────────────────────

CREATE TABLE fund_valuations (
    id                UUID PRIMARY KEY,
    service           TEXT NOT NULL,
    aum               TEXT NOT NULL CHECK (aum ~ '^[0-9]+$'),
    units_outstanding TEXT NOT NULL CHECK (units_outstanding ~ '^[1-9][0-9]*$'),
    nav               TEXT NOT NULL CHECK (nav ~ '^[0-9]+$'),
    posted_by         TEXT NOT NULL,
    posted_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX fund_valuations_service_idx ON fund_valuations (service, posted_at DESC);

CREATE TABLE fund_positions (
    user_id         UUID NOT NULL,
    service         TEXT NOT NULL,
    cost_basis      TEXT NOT NULL DEFAULT '0' CHECK (cost_basis ~ '^[0-9]+$'),
    high_water_mark TEXT NOT NULL DEFAULT '0' CHECK (high_water_mark ~ '^[0-9]+$'),
    units           TEXT NOT NULL DEFAULT '0' CHECK (units ~ '^[0-9]+$'),
    fee_debt        TEXT NOT NULL DEFAULT '0' CHECK (fee_debt ~ '^[0-9]+$'),
    fees_accrued_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    crystallized_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, service)
);
CREATE INDEX fund_positions_fee_due_idx ON fund_positions (fees_accrued_at) WHERE units <> '0';

-- ── in-kind issuance ───────────────────────────────────────────────────────

CREATE TABLE unit_issuances (
    id              UUID PRIMARY KEY,
    service         TEXT NOT NULL REFERENCES allocations (service),
    holder_kind     TEXT NOT NULL CHECK (holder_kind IN ('user', 'allocation')),
    holder_id       UUID REFERENCES users (id),
    holder_service  TEXT REFERENCES allocations (service),
    source          TEXT NOT NULL DEFAULT 'mint' CHECK (source IN ('mint', 'retire')),
    units           TEXT NOT NULL CHECK (units ~ '^[1-9][0-9]*$'),
    nav             TEXT NOT NULL CHECK (nav ~ '^[1-9][0-9]*$'),
    cost_basis      TEXT NOT NULL CHECK (cost_basis ~ '^[0-9]+$'),
    idempotency_key TEXT NOT NULL CHECK (char_length(idempotency_key) BETWEEN 1 AND 64),
    state           TEXT NOT NULL DEFAULT 'queued' CHECK (state IN ('queued', 'applied')),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    applied_at      TIMESTAMPTZ,
    UNIQUE (service, idempotency_key),
    CONSTRAINT unit_issuances_user_holder_names_a_user CHECK ((holder_kind = 'user') = (holder_id IS NOT NULL)),
    CONSTRAINT unit_issuances_allocation_holder_names_a_service CHECK ((holder_kind = 'allocation') = (holder_service IS NOT NULL)),
    CONSTRAINT unit_issuances_applied_is_stamped CHECK ((state = 'applied') = (applied_at IS NOT NULL))
);
CREATE INDEX unit_issuances_service_idx ON unit_issuances (service, created_at DESC);
COMMENT ON TABLE unit_issuances IS 'In-kind unit mints and retirements (no cash leg) by an invest:allocation:manage holder. The units live in TigerBeetle; this row is the record, the retry key and the queued/applied state.';
COMMENT ON COLUMN unit_issuances.holder_service IS 'The allocation holding the units when holder_kind = allocation (a reserved slug: fee | fund); NULL otherwise.';
COMMENT ON COLUMN unit_issuances.source IS 'mint | retire — minted in kind (Dr holder shares / Cr shares_outstanding) or retired (Dr shares_outstanding / Cr holder shares). units is always the magnitude.';

-- ── subscriptions and redemptions (PR 1b) ──────────────────────────────────

CREATE TABLE subscriptions (
    id         UUID PRIMARY KEY,
    user_id    UUID NOT NULL,
    service    TEXT NOT NULL,
    cash       TEXT NOT NULL CHECK (cash ~ '^[1-9][0-9]*$'),
    nav        TEXT NOT NULL CHECK (nav ~ '^[1-9][0-9]*$'),
    units      TEXT NOT NULL CHECK (units ~ '^[1-9][0-9]*$'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX subscriptions_user_idx ON subscriptions (user_id);

CREATE TABLE redemptions (
    id         UUID PRIMARY KEY,
    user_id    UUID NOT NULL,
    service    TEXT NOT NULL,
    units      TEXT NOT NULL CHECK (units ~ '^[1-9][0-9]*$'),
    nav        TEXT CHECK (nav ~ '^[1-9][0-9]*$'),
    cash       TEXT CHECK (cash ~ '^[1-9][0-9]*$'),
    state      TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX redemptions_user_idx ON redemptions (user_id);

-- ── fees (PR 1b) ───────────────────────────────────────────────────────────

CREATE TABLE fee_policies (
    service         TEXT PRIMARY KEY REFERENCES allocations (service) ON DELETE CASCADE,
    management_bps  INTEGER NOT NULL CHECK (management_bps BETWEEN 0 AND 10000),
    performance_bps INTEGER NOT NULL CHECK (performance_bps BETWEEN 0 AND 10000),
    hurdle_bps      INTEGER NOT NULL DEFAULT 0 CHECK (hurdle_bps BETWEEN 0 AND 10000),
    basis           TEXT NOT NULL CHECK (basis IN ('invested_capital', 'market_value')),
    crystallization TEXT NOT NULL CHECK (crystallization IN ('monthly', 'quarterly', 'semi_annual', 'annual')),
    updated_by      TEXT NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    version         INTEGER NOT NULL DEFAULT 1 CHECK (version >= 1),
    effective_from  TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT fee_policies_management_ceiling CHECK (management_bps <= 500),
    CONSTRAINT fee_policies_performance_ceiling CHECK (performance_bps <= 5000)
);

CREATE TABLE fee_assessments (
    id              UUID PRIMARY KEY,
    user_id         UUID NOT NULL,
    service         TEXT NOT NULL,
    trigger_kind    TEXT NOT NULL CHECK (trigger_kind IN ('period', 'redemption')),
    nav             TEXT NOT NULL CHECK (nav ~ '^[1-9][0-9]*$'),
    management      TEXT NOT NULL CHECK (management ~ '^[0-9]+$'),
    performance     TEXT NOT NULL CHECK (performance ~ '^[0-9]+$'),
    debt_opening    TEXT NOT NULL CHECK (debt_opening ~ '^[0-9]+$'),
    charged_units   TEXT NOT NULL CHECK (charged_units ~ '^[0-9]+$'),
    charged_cash    TEXT NOT NULL CHECK (charged_cash ~ '^[0-9]+$'),
    debt_carried    TEXT NOT NULL CHECK (debt_carried ~ '^[0-9]+$'),
    high_water_mark TEXT NOT NULL CHECK (high_water_mark ~ '^[0-9]+$'),
    assessed_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX fee_assessments_service_idx ON fee_assessments (service, assessed_at DESC);
CREATE INDEX fee_assessments_user_idx ON fee_assessments (user_id, assessed_at DESC);
COMMENT ON COLUMN fee_assessments.charged_units IS 'Units clawed back by this charge, in 18-dp base units. ''0'' when the whole charge deferred into fund_positions.fee_debt because every unit was escrowed or reserved.';

CREATE TABLE fee_settlements (
    id         UUID PRIMARY KEY,
    service    TEXT NOT NULL,
    units      TEXT NOT NULL CHECK (units ~ '^[1-9][0-9]*$'),
    nav        TEXT NOT NULL CHECK (nav ~ '^[1-9][0-9]*$'),
    cash       TEXT NOT NULL CHECK (cash ~ '^[1-9][0-9]*$'),
    settled_by TEXT NOT NULL,
    settled_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX fee_settlements_service_idx ON fee_settlements (service, settled_at DESC);

CREATE TABLE fee_policy_changes (
    id                   UUID PRIMARY KEY,
    service              TEXT NOT NULL REFERENCES allocations (service) ON DELETE CASCADE,
    version              INTEGER NOT NULL CHECK (version >= 1),
    management_bps       INTEGER NOT NULL CHECK (management_bps BETWEEN 0 AND 10000),
    performance_bps      INTEGER NOT NULL CHECK (performance_bps BETWEEN 0 AND 10000),
    hurdle_bps           INTEGER NOT NULL CHECK (hurdle_bps BETWEEN 0 AND 10000),
    basis                TEXT NOT NULL CHECK (basis IN ('invested_capital', 'market_value')),
    crystallization      TEXT NOT NULL CHECK (crystallization IN ('monthly', 'quarterly', 'semi_annual', 'annual')),
    state                TEXT NOT NULL CHECK (state IN ('awaiting_consilium', 'scheduled', 'active', 'superseded', 'rejected', 'cancelled')),
    requirement          TEXT NOT NULL CHECK (requirement IN ('admin', 'owner_consilium')),
    effective_from       TIMESTAMPTZ NOT NULL,
    requested_by         TEXT NOT NULL,
    requested_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    reason               TEXT NOT NULL DEFAULT '',
    consilium_id         UUID,
    scheduled_at         TIMESTAMPTZ,
    applied_at           TIMESTAMPTZ,
    closed_reason        TEXT,
    notices_waived_by    TEXT,
    notices_waived_at    TIMESTAMPTZ,
    notices_waived_users UUID[],
    UNIQUE (service, version),
    CONSTRAINT fee_policy_change_applied_is_stamped CHECK ((state IN ('active', 'superseded')) = (applied_at IS NOT NULL)),
    CONSTRAINT fee_policy_change_awaiting_is_unstamped CHECK (state <> 'awaiting_consilium' OR (scheduled_at IS NULL AND applied_at IS NULL)),
    CONSTRAINT fee_policy_change_closed_states_why CHECK ((state IN ('rejected', 'cancelled')) = (closed_reason IS NOT NULL)),
    CONSTRAINT fee_policy_change_consilium_matches_requirement CHECK ((requirement = 'owner_consilium') = (consilium_id IS NOT NULL)),
    CONSTRAINT fee_policy_change_notice_waiver_follows_scheduling CHECK (notices_waived_at IS NULL OR scheduled_at IS NOT NULL),
    CONSTRAINT fee_policy_change_notice_waiver_is_whole CHECK (num_nonnulls(notices_waived_by, notices_waived_at, notices_waived_users) IN (0, 3)),
    CONSTRAINT fee_policy_change_notice_waiver_names_someone CHECK (notices_waived_users IS NULL OR cardinality(notices_waived_users) > 0),
    CONSTRAINT fee_policy_change_owners_are_told_why CHECK (requirement <> 'owner_consilium' OR length(reason) > 0),
    CONSTRAINT fee_policy_change_scheduled_is_stamped CHECK (state NOT IN ('scheduled', 'active', 'superseded') OR scheduled_at IS NOT NULL),
    CONSTRAINT fee_policy_changes_management_ceiling CHECK (state IN ('superseded', 'rejected', 'cancelled') OR management_bps <= 500),
    CONSTRAINT fee_policy_changes_performance_ceiling CHECK (state IN ('superseded', 'rejected', 'cancelled') OR performance_bps <= 5000)
);
CREATE INDEX fee_policy_changes_consilium_idx ON fee_policy_changes (consilium_id) WHERE consilium_id IS NOT NULL;
CREATE INDEX fee_policy_changes_due_idx ON fee_policy_changes (effective_from) WHERE state = 'scheduled';
CREATE INDEX fee_policy_changes_history_idx ON fee_policy_changes (service, version DESC);
CREATE UNIQUE INDEX fee_policy_changes_single_active_idx ON fee_policy_changes (service) WHERE state = 'active';
CREATE UNIQUE INDEX fee_policy_changes_single_pending_idx ON fee_policy_changes (service) WHERE state IN ('awaiting_consilium', 'scheduled');
COMMENT ON COLUMN fee_policy_changes.consilium_id IS 'The owners'' approval a tightening change waits on. No foreign key: the approval lives in concierge, not in this database.';

-- ── the book (PR 1b) ───────────────────────────────────────────────────────

CREATE TABLE book_policies (
    service                TEXT PRIMARY KEY REFERENCES allocations (service),
    book_open              BOOLEAN NOT NULL DEFAULT false,
    taker_fee_bps          INTEGER NOT NULL DEFAULT 0 CHECK (taker_fee_bps BETWEEN 0 AND 10000),
    price_tick             TEXT NOT NULL DEFAULT '10000000000000000' CHECK (price_tick ~ '^[1-9][0-9]*$'),
    lot_size               TEXT NOT NULL DEFAULT '100000000000000' CHECK (lot_size ~ '^[1-9][0-9]*$'),
    market_slippage_bps    INTEGER NOT NULL DEFAULT 500 CHECK (market_slippage_bps BETWEEN 0 AND 10000),
    allow_unbacked_trading BOOLEAN NOT NULL DEFAULT false,
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);
COMMENT ON TABLE book_policies IS 'Per-allocation trading terms, set by invest:allocation:manage. No row = closed book. Amounts are 18-dp base-unit digit strings.';

CREATE TABLE book_revisions (
    service    TEXT PRIMARY KEY REFERENCES allocations (service),
    revision   BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE book_orders (
    id              UUID PRIMARY KEY,
    seq             BIGINT GENERATED ALWAYS AS IDENTITY,
    service         TEXT NOT NULL REFERENCES allocations (service),
    user_id         UUID NOT NULL REFERENCES users (id),
    client_order_id TEXT NOT NULL CHECK (char_length(client_order_id) BETWEEN 1 AND 64),
    side            TEXT NOT NULL CHECK (side IN ('buy', 'sell')),
    kind            TEXT NOT NULL CHECK (kind IN ('limit', 'market')),
    tif             TEXT NOT NULL CHECK (tif IN ('gtc', 'ioc', 'alo')),
    price           TEXT NOT NULL CHECK (price ~ '^[1-9][0-9]*$'),
    size            TEXT NOT NULL CHECK (size ~ '^[1-9][0-9]*$'),
    filled          TEXT NOT NULL DEFAULT '0' CHECK (filled ~ '^[0-9]+$'),
    notional_filled TEXT NOT NULL DEFAULT '0' CHECK (notional_filled ~ '^[0-9]+$'),
    fee_paid        TEXT NOT NULL DEFAULT '0' CHECK (fee_paid ~ '^[0-9]+$'),
    reserved        TEXT NOT NULL CHECK (reserved ~ '^[1-9][0-9]*$'),
    state           TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'partially_filled', 'filled', 'cancelled', 'rejected')),
    reject_reason   TEXT,
    cancel_reason   TEXT CHECK (cancel_reason IN ('user', 'ioc_remainder', 'market_remainder')),
    revision        BIGINT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, client_order_id),
    CHECK ((state = 'rejected') = (reject_reason IS NOT NULL)),
    CHECK ((state = 'cancelled') = (cancel_reason IS NOT NULL))
);
CREATE INDEX book_orders_resting_idx ON book_orders (service, side, (price::numeric), seq) WHERE state IN ('open', 'partially_filled');
CREATE INDEX book_orders_user_idx ON book_orders (user_id, seq DESC);

CREATE TABLE book_trades (
    id            UUID PRIMARY KEY,
    seq           BIGINT GENERATED ALWAYS AS IDENTITY,
    service       TEXT NOT NULL REFERENCES allocations (service),
    buyer_id      UUID NOT NULL REFERENCES users (id),
    seller_id     UUID NOT NULL REFERENCES users (id),
    buy_order_id  UUID NOT NULL REFERENCES book_orders (id),
    sell_order_id UUID NOT NULL REFERENCES book_orders (id),
    taker_side    TEXT NOT NULL CHECK (taker_side IN ('buy', 'sell')),
    price         TEXT NOT NULL CHECK (price ~ '^[1-9][0-9]*$'),
    size          TEXT NOT NULL CHECK (size ~ '^[1-9][0-9]*$'),
    notional      TEXT NOT NULL CHECK (notional ~ '^[0-9]+$'),
    fee           TEXT NOT NULL CHECK (fee ~ '^[0-9]+$'),
    executed_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (buyer_id <> seller_id)
);
CREATE INDEX book_trades_buyer_idx ON book_trades (buyer_id, seq DESC);
CREATE INDEX book_trades_seller_idx ON book_trades (seller_id, seq DESC);
CREATE INDEX book_trades_tape_idx ON book_trades (service, executed_at DESC, seq DESC);
