-- A sign-in the operator confirms again for one owner-level act (D24(1)): the person who asks
-- for a PLATFORM-level change must not vote on it, and concierge keeps them off the roster
-- only when invest-core relays their own token from a sign-in confirmed in the last two
-- minutes. The cabinet starts such a sign-in with `max_age=0` and keeps the token it brings
-- back here, apart from the session's own pair, for one use.
--
--   - `sign_ins.reauth_for`: the hash of the session a confirmation is for; NULL for an
--     ordinary sign-in. The callback stores the proof only for that session's browser.
--   - `reauth_proofs`: the confirmed token, sealed under the session cookie like the session's
--     tokens (`src/session/seal.rs`), one per session, deleted as it is used and with its
--     session. `expires_at` is unix seconds, a little before concierge stops taking it.
--
-- Compatible with the running binary: it never writes `reauth_for` and never reads either new
-- object. A confirmation that a pod of the previous release redeems is taken as an ordinary
-- sign-in: that browser gets a new session, and the operator confirms again.
--
-- Rollback: roll back the image only; the previous release runs on this schema. To undo the
-- schema too, `DROP TABLE cabinet.reauth_proofs; ALTER TABLE cabinet.sign_ins DROP COLUMN
-- reauth_for;` — both hold only minutes-old data; a confirmation in flight then fails, and the
-- operator confirms again.

SET lock_timeout = '3s';
SET statement_timeout = '30s';

ALTER TABLE cabinet.sign_ins
    ADD COLUMN reauth_for BYTEA CHECK (reauth_for IS NULL OR octet_length(reauth_for) = 32);

CREATE TABLE cabinet.reauth_proofs (
    session_hash BYTEA PRIMARY KEY REFERENCES cabinet.sessions (id_hash) ON DELETE CASCADE,
    proof_sealed BYTEA NOT NULL CHECK (octet_length(proof_sealed) > 28),
    expires_at   BIGINT NOT NULL
);
CREATE INDEX reauth_proofs_expires_at ON cabinet.reauth_proofs (expires_at);
