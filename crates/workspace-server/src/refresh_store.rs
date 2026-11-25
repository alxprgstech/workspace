use super::*;

const REFRESH_WINDOW_SECONDS: i64 = 30;
const REFRESH_TIMEOUT_SECONDS: i64 = 120;

pub enum RefreshDecision {
    Missing,
    NotNeeded,
    Pending,
    Acquired(Box<RefreshLease>),
}

pub struct RefreshLease {
    config: SsoConfig,
    handle_hash: [u8; 32],
    lease_id: [u8; 16],
    user_id: String,
    subject: String,
    generation: i64,
    expires_at: i64,
    refresh_token: String,
    previous_id_token: String,
}

impl RefreshLease {
    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    pub fn previous_id_token(&self) -> &str {
        &self.previous_id_token
    }

    pub fn subject(&self) -> &str {
        &self.subject
    }
}

struct RefreshRecord {
    user_id: String,
    subject: String,
    generation: i64,
    started_at: Option<i64>,
    expires_at: i64,
    access_expires_at: i64,
    encrypted: Vec<u8>,
}

pub(super) fn generation_aad(
    config: &SsoConfig,
    handle_hash: &[u8; 32],
    user_id: &str,
    expires_at: i64,
    generation: i64,
) -> Vec<u8> {
    let mut aad = session_aad(config, handle_hash, user_id, expires_at);

    // Preserve compatibility with existing sessions.
    if generation != 0 {
        aad.extend_from_slice(b"\0generation\0");
        aad.extend_from_slice(&generation.to_be_bytes());
    }

    aad
}

impl AuthStore {
    pub fn begin_refresh(
        &self,
        config: &SsoConfig,
        handle: &str,
        csrf_token: Option<&str>,
    ) -> Result<RefreshDecision, AuthStoreError> {
        sso::validate_config(config).map_err(|_| AuthStoreError::InvalidFlow)?;

        if !valid_handle(handle) || csrf_token.is_some_and(|token| !valid_handle(token)) {
            return Ok(RefreshDecision::Missing);
        }

        let handle_hash = hash_handle(handle);
        let csrf_hash = csrf_token.map(hash_handle);

        let mut connection = storage::open(&self.data_dir)?;

        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let now: i64 = tx.query_row("SELECT unixepoch()", [], |row| row.get(0))?;

        let record = tx
            .query_row(
                "SELECT
                u.id, u.subject,
                s.generation,
                s.refresh_started_at_unix,
                s.expires_at_unix,
                s.access_expires_at_unix,
                s.encrypted_tokens
             FROM web_sessions s
             JOIN users u ON u.id = s.user_id
             WHERE s.handle_hash = ?1
               AND s.client_id = ?2
               AND u.issuer = ?3
               AND u.is_enabled = 1
               AND s.revoked_at_unix IS NULL
               AND s.expires_at_unix > ?4
               AND (?5 IS NULL OR s.csrf_hash = ?5)",
                params![
                    &handle_hash[..],
                    &config.client_id,
                    &config.issuer,
                    now,
                    csrf_hash.as_ref().map(|hash| &hash[..]),
                ],
                |row| {
                    Ok(RefreshRecord {
                        user_id: row.get(0)?,
                        subject: row.get(1)?,
                        generation: row.get(2)?,
                        started_at: row.get(3)?,
                        expires_at: row.get(4)?,
                        access_expires_at: row.get(5)?,
                        encrypted: row.get(6)?,
                    })
                },
            )
            .optional()?;

        let Some(record) = record else {
            return Ok(RefreshDecision::Missing);
        };

        if let Some(started_at) = record.started_at {
            if started_at <= now - REFRESH_TIMEOUT_SECONDS {
                // The previous exchange may have consumed
                // the old token. Never acquire it again.
                tx.execute(
                    "DELETE FROM web_sessions
                     WHERE handle_hash = ?1",
                    params![&handle_hash[..]],
                )?;

                tx.commit()?;

                return Ok(RefreshDecision::Missing);
            }

            return Ok(RefreshDecision::Pending);
        }

        if record.access_expires_at >= record.expires_at
            || record.access_expires_at > now + REFRESH_WINDOW_SECONDS
        {
            return Ok(RefreshDecision::NotNeeded);
        }

        let plaintext = match self.cipher.open(
            &record.encrypted,
            &generation_aad(
                config,
                &handle_hash,
                &record.user_id,
                record.expires_at,
                record.generation,
            ),
        ) {
            Ok(plaintext) => plaintext,
            Err(_) => {
                // A corrupted session can never be refreshed again.
                tx.execute(
                    "DELETE FROM web_sessions
                     WHERE handle_hash = ?1",
                    params![&handle_hash[..]],
                )?;

                tx.commit()?;

                return Ok(RefreshDecision::Missing);
            }
        };

        let Ok(tokens) = serde_json::from_slice::<SessionTokens>(&plaintext) else {
            tx.execute(
                "DELETE FROM web_sessions
                 WHERE handle_hash = ?1",
                params![&handle_hash[..]],
            )?;

            tx.commit()?;

            return Ok(RefreshDecision::Missing);
        };

        if tokens.access_expires_at_unix != record.access_expires_at
            || tokens.refresh_token.is_empty()
        {
            tx.execute(
                "DELETE FROM web_sessions
                 WHERE handle_hash = ?1",
                params![&handle_hash[..]],
            )?;

            tx.commit()?;

            return Ok(RefreshDecision::Missing);
        }

        let lease_id = random_lease_id().map_err(|_| AuthStoreError::InvalidFlow)?;

        let changed = tx.execute(
            "UPDATE web_sessions
             SET refresh_started_at_unix = ?1,
                 refresh_lease_id = ?4
             WHERE handle_hash = ?2
               AND generation = ?3
               AND refresh_started_at_unix IS NULL
               AND refresh_lease_id IS NULL",
            params![now, &handle_hash[..], record.generation, &lease_id[..]],
        )?;

        if changed != 1 {
            return Err(AuthStoreError::InvalidFlow);
        }

        tx.commit()?;

        Ok(RefreshDecision::Acquired(Box::new(RefreshLease {
            config: config.clone(),
            handle_hash,
            lease_id,
            user_id: record.user_id,
            subject: record.subject,
            generation: record.generation,
            expires_at: record.expires_at,
            refresh_token: tokens.refresh_token,
            previous_id_token: tokens.id_token,
        })))
    }

    pub fn finish_refresh(
        &self,
        lease: &RefreshLease,
        refreshed: RefreshedSession,
    ) -> Result<bool, AuthStoreError> {
        let RefreshedSession {
            mut tokens,
            profile,
        } = refreshed;

        let mut connection = storage::open(&self.data_dir)?;

        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let now: i64 = tx.query_row("SELECT unixepoch()", [], |row| row.get(0))?;

        tokens.access_expires_at_unix = tokens.access_expires_at_unix.min(lease.expires_at);

        if tokens.access_expires_at_unix <= now
            || tokens.access_token.is_empty()
            || tokens.id_token.is_empty()
            || tokens.refresh_token.is_empty()
            || tokens.refresh_token == lease.refresh_token
        {
            return Err(AuthStoreError::InvalidPayload);
        }

        let generation = lease
            .generation
            .checked_add(1)
            .ok_or(AuthStoreError::InvalidPayload)?;

        let plaintext = serde_json::to_vec(&tokens).map_err(|_| AuthStoreError::InvalidPayload)?;

        let encrypted = self.cipher.seal(
            &plaintext,
            &generation_aad(
                &lease.config,
                &lease.handle_hash,
                &lease.user_id,
                lease.expires_at,
                generation,
            ),
        )?;

        let changed = tx.execute(
            "UPDATE web_sessions
             SET encrypted_tokens = ?1,
                 access_expires_at_unix = ?2,
                 generation = ?3,
                 refresh_started_at_unix = NULL,
                 refresh_lease_id = NULL
             WHERE handle_hash = ?4
               AND generation = ?5
               AND refresh_lease_id = ?6
               AND user_id = ?7
               AND client_id = ?8
               AND expires_at_unix = ?9
               AND expires_at_unix > ?10
               AND refresh_started_at_unix IS NOT NULL
               AND revoked_at_unix IS NULL
               AND user_id IN (
                   SELECT id FROM users
                   WHERE issuer = ?11
                     AND subject = ?12
                     AND is_enabled = 1
               )",
            params![
                encrypted,
                tokens.access_expires_at_unix,
                generation,
                &lease.handle_hash[..],
                lease.generation,
                &lease.lease_id[..],
                &lease.user_id,
                &lease.config.client_id,
                lease.expires_at,
                now,
                &lease.config.issuer,
                &lease.subject,
            ],
        )?;

        if changed != 1 {
            tx.commit()?;

            return Ok(false);
        }

        // The SSO profile may change, the local role never does.
        tx.execute(
            "UPDATE users
             SET username = COALESCE(?1, username),
                 email = COALESCE(?2, email),
                 email_verified = COALESCE(?3, email_verified),
                 updated_at_unix = unixepoch()
             WHERE issuer = ?4
               AND subject = ?5
               AND is_enabled = 1",
            params![
                profile.username,
                profile.email,
                profile.email_verified,
                &lease.config.issuer,
                &lease.subject,
            ],
        )?;

        tx.commit()?;

        Ok(true)
    }

    pub fn abort_refresh(&self, lease: &RefreshLease) -> Result<bool, AuthStoreError> {
        let connection = storage::open(&self.data_dir)?;

        // Do not clear the marker and reuse the old token.
        // An uncertain exchange requires a new login.
        let changed = connection.execute(
            "DELETE FROM web_sessions
             WHERE handle_hash = ?1
               AND refresh_lease_id = ?2
               AND user_id = ?3
               AND client_id = ?4",
            params![
                &lease.handle_hash[..],
                &lease.lease_id[..],
                &lease.user_id,
                &lease.config.client_id,
            ],
        )?;

        Ok(changed == 1)
    }
}

impl AuthStore {
    /// Only valid when no refresh request has been submitted.
    pub fn release_unsubmitted_refresh(
        &self,
        lease: &RefreshLease,
    ) -> Result<bool, AuthStoreError> {
        let connection = storage::open(&self.data_dir)?;

        let changed = connection.execute(
            "UPDATE web_sessions
             SET refresh_started_at_unix = NULL,
                 refresh_lease_id = NULL
             WHERE handle_hash = ?1
               AND refresh_lease_id = ?2
               AND user_id = ?3
               AND client_id = ?4
               AND revoked_at_unix IS NULL",
            params![
                &lease.handle_hash[..],
                &lease.lease_id[..],
                &lease.user_id,
                &lease.config.client_id,
            ],
        )?;

        Ok(changed == 1)
    }
}
