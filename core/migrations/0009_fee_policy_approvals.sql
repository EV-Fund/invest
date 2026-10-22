-- Fee-policy changes the owners decide through concierge's approvals (invest#37).
--
-- A change that leaves the house envelope is recorded `awaiting_consilium` FIRST and proposed
-- to concierge as `invest:fee_policy` right after, so a crash between the two leaves a row the
-- approvals round proposes again (the proposal is idempotent by `fee_policy_change:<id>`),
-- never a proposal with nothing behind it. `consilium_id` keeps its name and now holds the
-- concierge approval id; until the proposal is open it is NULL, which the old CHECK forbade.
--
--   - `approval_state`: concierge's state of the proposal as the round last read it — what the
--     operator sees (open, passed and in its veto delay, …). NULL until proposed.
--   - `approval_holders`: how many holders the proposal told the owners would be notified. It
--     is one of the approved fields, so the round needs it to recompute the payload hash.
--
-- Compatible with the running binary: it never writes an owner change (it refuses them), reads
-- neither new column, and every row it writes (an administrator's: no approval) passes the new
-- CHECK. An image rollback leaves this schema in place, and the old binary runs on it.
--
-- Rollback: roll back the image only; this schema stays. Do not revert it by hand. The old
-- CHECK needs a `consilium_id` on EVERY owner row, and an owner change that closed before it
-- was proposed (`rejected` or `cancelled`, `consilium_id` NULL) is history that must not be
-- deleted to satisfy it. Undoing the schema is a restore from the backup taken before the
-- rollout, not a migration.

SET lock_timeout = '3s';
SET statement_timeout = '30s';

ALTER TABLE fee_policy_changes
    ADD COLUMN approval_state TEXT CHECK (approval_state IN ('open', 'passed', 'executed', 'execution_failed', 'rejected', 'vetoed', 'expired', 'cancelled', 'void')),
    ADD COLUMN approval_holders INTEGER CHECK (approval_holders >= 0),
    DROP CONSTRAINT fee_policy_change_consilium_matches_requirement,
    ADD CONSTRAINT fee_policy_change_approval_matches_requirement CHECK (
        (requirement = 'admin' AND consilium_id IS NULL AND approval_state IS NULL AND approval_holders IS NULL)
        OR (requirement = 'owner_consilium' AND (consilium_id IS NOT NULL OR state IN ('awaiting_consilium', 'rejected', 'cancelled')))
    ),
    ADD CONSTRAINT fee_policy_change_approval_state_names_one CHECK (approval_state IS NULL OR consilium_id IS NOT NULL);

COMMENT ON COLUMN fee_policy_changes.consilium_id IS 'The concierge approval (invest:fee_policy) an owner change waits on; NULL until it is proposed. Rows imported from banking keep their consilium id. No foreign key: the approval lives in concierge.';
COMMENT ON COLUMN fee_policy_changes.approval_state IS 'concierge''s state of that approval as invest last read it; NULL until proposed.';
COMMENT ON COLUMN fee_policy_changes.approval_holders IS 'The holder count the proposal named (holders_notified). Hashed with the other fields, so the executor needs it to re-check the approval.';
