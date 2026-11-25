use super::*;
use openidconnect::ClientSecret;
use std::{
    collections::BTreeSet,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    id_token: String,
    token_type: String,
    expires_in: i64,
    scope: String,
}

#[derive(Deserialize)]
struct UserInfo {
    sub: String,
    preferred_username: Option<String>,
    email: Option<String>,
    email_verified: Option<bool>,
}

#[derive(Serialize, Deserialize)]
pub struct SessionTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: String,
    pub access_expires_at_unix: i64,
}

pub struct LoginResult {
    pub identity: VerifiedIdentity,
    pub username: Option<String>,
    pub email: Option<String>,
    pub email_verified: bool,
    pub tokens: SessionTokens,
}

pub(super) fn unix_now() -> Result<i64, SsoError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|time| i64::try_from(time.as_secs()).ok())
        .ok_or(SsoError::InvalidResponse)
}

fn validate_token_response(response: &TokenResponse) -> Result<(), SsoError> {
    let scopes: BTreeSet<_> = response.scope.split_ascii_whitespace().collect();

    let expected = BTreeSet::from(["openid", "profile", "email"]);

    if !response.token_type.eq_ignore_ascii_case("Bearer")
        || !(1..=3600).contains(&response.expires_in)
        || scopes != expected
        || [
            &response.access_token,
            &response.refresh_token,
            &response.id_token,
        ]
        .iter()
        .any(|token| {
            token.is_empty()
                || token.len() > 16_384
                || !token.bytes().all(|byte| byte.is_ascii_graphic())
        })
    {
        return Err(SsoError::InvalidResponse);
    }

    Ok(())
}

async fn read_response<T: DeserializeOwned>(
    mut response: reqwest::Response,
) -> Result<T, SsoError> {
    if response.status() != reqwest::StatusCode::OK {
        return Err(SsoError::HttpStatus(response.status().as_u16()));
    }

    let mut body = Vec::new();

    while let Some(chunk) = response.chunk().await.map_err(|_| SsoError::Transport)? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(SsoError::ResponseTooLarge);
        }

        body.extend_from_slice(&chunk);
    }

    serde_json::from_slice(&body).map_err(|_| SsoError::InvalidResponse)
}

impl SsoClient {
    pub async fn exchange_code(
        &self,
        client_secret: &ClientSecret,
        code: &str,
        verifier: &str,
        expected_nonce: &str,
    ) -> Result<LoginResult, SsoError> {
        if code.is_empty()
            || code.len() > 128
            || !(43..=128).contains(&verifier.len())
            || !verifier
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte))
            || expected_nonce.is_empty()
        {
            return Err(SsoError::InvalidResponse);
        }

        let started_at = unix_now()?;

        let http = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| SsoError::Transport)?;

        let response = http
            .post(TOKEN_URL)
            .basic_auth(&self.config.client_id, Some(client_secret.secret()))
            .header("Accept", "application/json")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("code_verifier", verifier),
                ("redirect_uri", &self.config.redirect_uri),
            ])
            .send()
            .await
            .map_err(|_| SsoError::Transport)?;

        let response: TokenResponse = read_response(response).await?;

        validate_token_response(&response)?;

        let identity = self.verify_login_token(
            &response.id_token,
            expected_nonce,
            Some(&response.access_token),
        )?;

        let profile = http
            .get(USERINFO_URL)
            .bearer_auth(&response.access_token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|_| SsoError::Transport)?;

        let profile: UserInfo = read_response(profile).await?;

        if profile.sub != identity.subject
            || profile
                .preferred_username
                .as_ref()
                .is_some_and(|value| value.len() > 512)
            || profile
                .email
                .as_ref()
                .is_some_and(|value| value.len() > 512)
        {
            return Err(SsoError::InvalidToken);
        }

        let expires_at = (started_at + response.expires_in).min(identity.expires_at_unix);

        if expires_at <= unix_now()? {
            return Err(SsoError::InvalidToken);
        }

        Ok(LoginResult {
            identity,
            username: profile.preferred_username,
            email: profile.email,
            email_verified: profile.email_verified.unwrap_or(false),
            tokens: SessionTokens {
                access_token: response.access_token,
                refresh_token: response.refresh_token,
                id_token: response.id_token,
                access_expires_at_unix: expires_at,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response() -> TokenResponse {
        TokenResponse {
            access_token: "test-access".to_owned(),
            refresh_token: "test-refresh".to_owned(),
            id_token: "test-id".to_owned(),
            token_type: "Bearer".to_owned(),
            expires_in: 300,
            scope: "openid profile email".to_owned(),
        }
    }

    #[test]
    fn accepts_expected_token_response() {
        assert!(validate_token_response(&response()).is_ok());
    }

    #[test]
    fn rejects_unexpected_scope() {
        let mut token = response();
        token.scope = "openid profile email admin".to_owned();

        assert!(validate_token_response(&token).is_err());
    }

    #[test]
    fn rejects_invalid_token_lifetime() {
        let mut token = response();
        token.expires_in = 0;

        assert!(validate_token_response(&token).is_err());
    }

    #[test]
    fn rejects_missing_refresh_token() {
        let mut token = response();
        token.refresh_token.clear();

        assert!(validate_token_response(&token).is_err());
    }
}
