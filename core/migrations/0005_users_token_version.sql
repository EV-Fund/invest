-- The revocation threshold of the `users` projection: concierge's per-user `token_version`,
-- the value a "revoke all" bumps. invest-core refuses an access token whose `token_version`
-- claim is below it (grpc auth layer), so a revoked token stops working on the next call
-- instead of living out its TTL.
--
-- The lifecycle and scope streams already carry it (UserEvent.token_version and
-- Member.token_version in evconcierge_projection); this step only gives it a column. The
-- writer keeps the greatest value seen, so a stale snapshot can never lower the threshold.
--
-- Nullable, no backfill, no data rewrite: NULL means "no threshold known" and every token
-- is accepted, exactly as before this release. The first MEMBER_JOINED or lifecycle event
-- for a user fills it.
--
-- Numbered 0005: 0004 is taken by money_events.
--
-- Rollback: drop the column; the previous release neither reads nor writes it.

SET lock_timeout = '3s';
SET statement_timeout = '30s';

ALTER TABLE users ADD COLUMN token_version BIGINT CHECK (token_version >= 0);
COMMENT ON COLUMN users.token_version IS 'concierge''s token_version for the user: access tokens with a lower `token_version` claim are refused. NULL — no threshold seen yet; tokens are accepted.';
