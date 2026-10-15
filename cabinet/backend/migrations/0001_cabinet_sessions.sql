-- The cabinet BFF's sign-in state and sessions (S2-13), in its own schema of the `invest`
-- database. The schema and this history's table (`cabinet._sqlx_migrations`) are created
-- by the BFF's migrator before this runs (`src/db.rs`); invest-core's tables and history
-- in `public` are not touched.
--
-- Every timestamp is unix seconds, the unit concierge states token expiries in.
--
-- Rollback: `DROP SCHEMA cabinet CASCADE` — every session ends and every sign-in in
-- flight fails; people sign in again. Nothing else reads these tables.

SET lock_timeout = '3s';
SET statement_timeout = '30s';

-- A sign-in between `/cabinet/api/auth/login` and the callback: the PKCE verifier and the
-- page to return to, keyed by the hash of the `state` the browser carries. Deleted as it
-- is redeemed, so a callback is honoured once.
CREATE TABLE cabinet.sign_ins (
    state_hash BYTEA PRIMARY KEY CHECK (octet_length(state_hash) = 32),
    verifier   TEXT NOT NULL CHECK (length(verifier) BETWEEN 43 AND 128),
    return_to  TEXT NOT NULL CHECK (return_to LIKE '/cabinet%' AND length(return_to) <= 2048),
    expires_at BIGINT NOT NULL
);
CREATE INDEX sign_ins_expires_at ON cabinet.sign_ins (expires_at);

-- A signed-in browser. The row is named by the SHA-256 of the session cookie, never the
-- cookie; concierge's tokens for the relying party `invest` stay here and never reach the
-- browser.
CREATE TABLE cabinet.sessions (
    id_hash           BYTEA PRIMARY KEY CHECK (octet_length(id_hash) = 32),
    -- concierge's user id — the `sub` of the access token.
    user_id           UUID NOT NULL,
    access_token      TEXT NOT NULL,
    access_expires_at BIGINT NOT NULL,
    refresh_token     TEXT NOT NULL,
    -- The refresh token's expiry: the session ends with it.
    expires_at        BIGINT NOT NULL,
    -- A replica's lease on rotating the pair; NULL when nobody is.
    rotating_until    BIGINT,
    created_at        BIGINT NOT NULL
);
CREATE INDEX sessions_expires_at ON cabinet.sessions (expires_at);
