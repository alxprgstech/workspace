-- Stage 009: per-operation OAuth scopes.
--
-- Clients may now request the documented scope set. Existing grants are left
-- untouched on purpose: a token issued with the legacy read-only scope keeps
-- exactly that scope and is never upgraded without a fresh user consent.

UPDATE mcp_oauth_clients
SET allowed_scope = 'workspace:read workspace:write execution:run files:read files:write artifacts:read artifacts:publish'
WHERE allowed_scope = 'workspace.read';

PRAGMA user_version = 6;
