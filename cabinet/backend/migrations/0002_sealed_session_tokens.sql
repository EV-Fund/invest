-- Session tokens sealed at rest (security review F2): concierge's access and refresh tokens
-- are stored as ChaCha20-Poly1305 ciphertext under a key derived from the session cookie
-- (`src/session/seal.rs`), never in clear. `nonce ‖ ciphertext ‖ tag`.
--
-- Every open session ends: the clear-text rows of 0001 cannot be sealed without their
-- cookies, which the server never kept. People sign in again. No production session exists
-- yet (the BFF is not deployed), so this is a dev-database concern only.
--
-- Rollout: a pod of the previous release reading the new table fails its session reads
-- until it is replaced — sign-in is unavailable for the length of the rollout.
-- Rollback: `DROP SCHEMA cabinet CASCADE`; the previous release re-creates it on boot.

SET lock_timeout = '3s';
SET statement_timeout = '30s';

DELETE FROM cabinet.sessions;

ALTER TABLE cabinet.sessions
    DROP COLUMN access_token,
    DROP COLUMN refresh_token,
    ADD COLUMN access_sealed BYTEA NOT NULL CHECK (octet_length(access_sealed) > 28),
    ADD COLUMN refresh_sealed BYTEA NOT NULL CHECK (octet_length(refresh_sealed) > 28);
