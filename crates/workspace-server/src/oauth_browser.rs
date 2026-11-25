use super::*;
use serde::{Deserialize, Serialize};

const REQUEST_TTL: i64 = 600;

#[derive(Deserialize, Serialize)]
pub(crate) struct AuthorizationRequest {
    pub client_id: String,
    pub redirect_uri: String,
    pub response_type: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub state: String,
    pub resource: String,
    pub scope: Option<String>,
}

pub(crate) struct PendingAuthorization {
    pub request: AuthorizationRequest,
    pub client_name: String,
}

/// Effective scopes of one authorization request.
pub(crate) fn requested_scopes(request: &AuthorizationRequest) -> Vec<String> {
    match request.scope.as_deref() {
        Some(scope) => parse_scopes(scope),
        None => parse_scopes(&default_scope()),
    }
}

fn request_aad(handle: &str, client_id: &str) -> Vec<u8> {
    format!("workspace:mcp-request:v1:{handle}:{client_id}").into_bytes()
}

fn validate(request: &AuthorizationRequest, resource: &str) -> Result<(), OAuthError> {
    if !valid_token(&request.client_id, "mcp_client_")
        || !valid_redirect(&request.redirect_uri)
        || request.response_type != "code"
        || request.code_challenge_method != "S256"
        || !valid_challenge(&request.code_challenge)
        || request.resource != resource
        || request
            .scope
            .as_deref()
            .is_some_and(|scope| parse_scopes(scope).is_empty())
        || request.state.is_empty()
        || request.state.len() > 512
        || request.state.chars().any(char::is_control)
    {
        return Err(OAuthError::InvalidRequest);
    }

    Ok(())
}

impl OAuthStore {
    pub(crate) fn begin_authorization(
        &self,
        request: &AuthorizationRequest,
        key: &[u8; 32],
    ) -> Result<String, OAuthError> {
        validate(request, &self.resource)?;

        let mut connection = storage::open(&self.data_dir)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let allowed_scope: Option<String> = tx
            .query_row(
                "SELECT allowed_scope FROM mcp_oauth_clients
                 WHERE client_id = ?1 AND redirect_uri = ?2 AND is_enabled = 1",
                params![request.client_id, request.redirect_uri],
                |row| row.get(0),
            )
            .optional()?;

        let Some(allowed_scope) = allowed_scope else {
            return Err(OAuthError::InvalidClient);
        };

        if !scope_subset(&requested_scopes(request), &parse_scopes(&allowed_scope)) {
            return Err(OAuthError::InvalidRequest);
        }

        tx.execute(
            "DELETE FROM mcp_oauth_requests
             WHERE expires_at_unix <= unixepoch()",
            [],
        )?;

        tx.execute(
            "DELETE FROM mcp_oauth_grants
             WHERE expires_at_unix <= unixepoch()",
            [],
        )?;

        tx.execute(
            "DELETE FROM mcp_access_tokens
             WHERE expires_at_unix <= unixepoch()",
            [],
        )?;

        let count: i64 = tx.query_row("SELECT count(*) FROM mcp_oauth_requests", [], |row| {
            row.get(0)
        })?;

        let client_count: i64 = tx.query_row(
            "SELECT count(*) FROM mcp_oauth_requests
             WHERE client_id = ?1",
            params![request.client_id],
            |row| row.get(0),
        )?;

        if count >= 1024 || client_count >= 64 {
            return Err(OAuthError::InvalidRequest);
        }

        let handle = crypto::random_handle()?;
        let payload = serde_json::to_vec(request).map_err(|_| OAuthError::InvalidRequest)?;

        let encrypted = crypto::SecretCipher::new(key)?
            .seal(&payload, &request_aad(&handle, &request.client_id))?;

        let current_time = now(&tx)?;

        tx.execute(
            "INSERT INTO mcp_oauth_requests
             (handle_hash, client_id, encrypted_payload,
              created_at_unix, expires_at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &crypto::hash_handle(&handle)[..],
                request.client_id,
                encrypted,
                current_time,
                current_time + REQUEST_TTL,
            ],
        )?;

        tx.commit()?;

        Ok(handle)
    }

    fn pending_on(
        &self,
        connection: &Connection,
        handle: &str,
        key: &[u8; 32],
    ) -> Result<PendingAuthorization, OAuthError> {
        if !valid_token(handle, "") {
            return Err(OAuthError::InvalidGrant);
        }

        let record = connection
            .query_row(
                "SELECT r.client_id, r.encrypted_payload,
                    c.name, c.redirect_uri
             FROM mcp_oauth_requests r
             JOIN mcp_oauth_clients c ON c.client_id = r.client_id
             WHERE r.handle_hash = ?1
               AND r.expires_at_unix > unixepoch()
               AND c.is_enabled = 1",
                params![&crypto::hash_handle(handle)[..]],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;

        let Some((client_id, encrypted, client_name, redirect_uri)) = record else {
            return Err(OAuthError::InvalidGrant);
        };

        let plaintext =
            crypto::SecretCipher::new(key)?.open(&encrypted, &request_aad(handle, &client_id))?;

        let request: AuthorizationRequest =
            serde_json::from_slice(&plaintext).map_err(|_| OAuthError::InvalidRequest)?;

        validate(&request, &self.resource)?;

        if request.client_id != client_id || request.redirect_uri != redirect_uri {
            return Err(OAuthError::InvalidRequest);
        }

        let allowed_scope: String = connection.query_row(
            "SELECT allowed_scope FROM mcp_oauth_clients WHERE client_id = ?1",
            params![client_id],
            |row| row.get(0),
        )?;

        if !scope_subset(&requested_scopes(&request), &parse_scopes(&allowed_scope)) {
            return Err(OAuthError::InvalidRequest);
        }

        Ok(PendingAuthorization {
            request,
            client_name,
        })
    }

    pub(crate) fn pending_authorization(
        &self,
        handle: &str,
        key: &[u8; 32],
    ) -> Result<PendingAuthorization, OAuthError> {
        let connection = storage::open(&self.data_dir)?;
        self.pending_on(&connection, handle, key)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn complete_authorization(
        &self,
        handle: &str,
        key: &[u8; 32],
        user_id: &str,
        session_handle: &str,
        csrf: &str,
        sso_client_id: &str,
        approved: bool,
    ) -> Result<(AuthorizationRequest, Option<String>), OAuthError> {
        let mut connection = storage::open(&self.data_dir)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let pending = self.pending_on(&tx, handle, key)?;

        // Recheck the browser session inside the same transaction.
        let session_exists: bool = tx.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM web_sessions s
                JOIN users u ON u.id = s.user_id
                WHERE s.handle_hash = ?1
                  AND s.csrf_hash = ?2
                  AND u.id = ?3
                  AND s.client_id = ?4
                  AND u.issuer = ?5
                  AND u.is_enabled = 1
                  AND s.revoked_at_unix IS NULL
                  AND s.refresh_started_at_unix IS NULL
                  AND s.expires_at_unix > unixepoch()
                  AND s.access_expires_at_unix > unixepoch()
            )",
            params![
                &crypto::hash_handle(session_handle)[..],
                &crypto::hash_handle(csrf)[..],
                user_id,
                sso_client_id,
                self.user_issuer,
            ],
            |row| row.get(0),
        )?;

        if !session_exists {
            return Err(OAuthError::AccessDenied);
        }

        let changed = tx.execute(
            "DELETE FROM mcp_oauth_requests
             WHERE handle_hash = ?1
               AND expires_at_unix > unixepoch()",
            params![&crypto::hash_handle(handle)[..]],
        )?;

        if changed != 1 {
            return Err(OAuthError::InvalidGrant);
        }

        let request = pending.request;

        let code = if approved {
            Some(self.issue_code_on(
                &tx,
                &request.client_id,
                &request.redirect_uri,
                &request.code_challenge,
                user_id,
                &requested_scopes(&request).join(" "),
            )?)
        } else {
            None
        };

        tx.commit()?;

        Ok((request, code))
    }
}
