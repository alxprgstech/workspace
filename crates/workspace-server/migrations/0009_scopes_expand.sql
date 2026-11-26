-- Stage 013: expand the client allowlist to every documented scope.
--
-- Tokens are only honoured while their grant stays inside the client's
-- allowed_scope. Clients registered before workspace:delete and files:import
-- existed would otherwise be unable to use those tools even after a fresh
-- consent, because the containment check would reject the new grant. Grants
-- themselves are not touched: the user still has to approve the new scopes.

UPDATE mcp_oauth_clients
SET allowed_scope = 'workspace:read workspace:write workspace:delete execution:run files:read files:write files:import artifacts:read artifacts:publish'
WHERE allowed_scope IN (
    'workspace.read',
    'workspace:read workspace:write execution:run files:read files:write artifacts:read artifacts:publish'
);

PRAGMA user_version = 9;
