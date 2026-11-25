use openidconnect::{
    AdditionalClaims, ClientId, CsrfToken, IdToken, IssuerUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier,
    core::{
        CoreGenderClaim, CoreIdTokenVerifier, CoreJsonWebKeySet, CoreJweContentEncryptionAlgorithm,
        CoreJwsSigningAlgorithm,
    },
    reqwest::{self, Url},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;
use workspace_core::config::SsoConfig;

const ISSUER: &str = "https://auth.alxprgs.tech";
const DISCOVERY_URL: &str = "https://auth.alxprgs.tech/.well-known/openid-configuration";
const AUTHORIZATION_URL: &str = "https://auth.alxprgs.tech/oauth/authorize";
const TOKEN_URL: &str = "https://auth.alxprgs.tech/oauth/token";
const USERINFO_URL: &str = "https://auth.alxprgs.tech/oauth/userinfo";
const JWKS_URL: &str = "https://auth.alxprgs.tech/.well-known/jwks.json";
const REVOCATION_URL: &str = "https://auth.alxprgs.tech/oauth/revoke";

const MAX_RESPONSE_BYTES: usize = 65_536;

#[derive(Debug, Error)]
pub enum SsoError {
    #[error("invalid SSO configuration")]
    Configuration,

    #[error("SSO transport failure")]
    Transport,

    #[error("SSO returned HTTP {0}")]
    HttpStatus(u16),

    #[error("SSO response exceeds the permitted size")]
    ResponseTooLarge,

    #[error("invalid SSO response")]
    InvalidResponse,

    #[error("SSO metadata does not match the expected provider")]
    MetadataMismatch,

    #[error("SSO signing keys are unavailable")]
    MissingKeys,

    #[error("ID token verification failed")]
    InvalidToken,
}

#[derive(Debug, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    userinfo_endpoint: String,
    jwks_uri: String,
    revocation_endpoint: String,
    response_types_supported: Vec<String>,
    response_modes_supported: Vec<String>,
    grant_types_supported: Vec<String>,
    id_token_signing_alg_values_supported: Vec<String>,
    code_challenge_methods_supported: Vec<String>,
    token_endpoint_auth_methods_supported: Vec<String>,
    scopes_supported: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct SsoClaims {
    token_use: String,
}

impl AdditionalClaims for SsoClaims {}

type SsoIdToken =
    IdToken<SsoClaims, CoreGenderClaim, CoreJweContentEncryptionAlgorithm, CoreJwsSigningAlgorithm>;

pub struct AuthorizationRequest {
    pub url: Url,
    pub state: CsrfToken,
    pub nonce: Nonce,
    pub verifier: PkceCodeVerifier,
}

#[derive(Debug, Serialize)]
pub struct VerifiedIdentity {
    pub issuer: String,
    pub subject: String,
}

pub struct SsoClient {
    config: SsoConfig,
    jwks: CoreJsonWebKeySet,
}

pub fn validate_config(config: &SsoConfig) -> Result<(), SsoError> {
    if config.issuer != ISSUER
        || config.client_id.is_empty()
        || config.client_id.len() > 128
        || !config
            .client_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(SsoError::Configuration);
    }

    let redirect = Url::parse(&config.redirect_uri).map_err(|_| SsoError::Configuration)?;

    if redirect.scheme() != "https"
        || redirect.host_str().is_none()
        || !redirect.username().is_empty()
        || redirect.password().is_some()
        || redirect.query().is_some()
        || redirect.fragment().is_some()
        || redirect.path() != "/auth/callback"
    {
        return Err(SsoError::Configuration);
    }

    Ok(())
}

fn contains(values: &[String], expected: &str) -> bool {
    values.iter().any(|value| value == expected)
}

fn validate_discovery(discovery: &Discovery) -> Result<(), SsoError> {
    if discovery.issuer != ISSUER
        || discovery.authorization_endpoint != AUTHORIZATION_URL
        || discovery.token_endpoint != TOKEN_URL
        || discovery.userinfo_endpoint != USERINFO_URL
        || discovery.jwks_uri != JWKS_URL
        || discovery.revocation_endpoint != REVOCATION_URL
        || !contains(&discovery.response_types_supported, "code")
        || !contains(&discovery.response_modes_supported, "query")
        || !contains(&discovery.grant_types_supported, "authorization_code")
        || !contains(&discovery.grant_types_supported, "refresh_token")
        || !contains(&discovery.id_token_signing_alg_values_supported, "RS256")
        || !contains(&discovery.code_challenge_methods_supported, "S256")
        || !contains(
            &discovery.token_endpoint_auth_methods_supported,
            "client_secret_basic",
        )
        || !["openid", "profile", "email"]
            .iter()
            .all(|scope| contains(&discovery.scopes_supported, scope))
    {
        return Err(SsoError::MetadataMismatch);
    }

    Ok(())
}

async fn get_json<T: DeserializeOwned>(http: &reqwest::Client, url: &str) -> Result<T, SsoError> {
    let mut response = http
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|_| SsoError::Transport)?;

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

pub fn start_authorization(config: &SsoConfig) -> Result<AuthorizationRequest, SsoError> {
    validate_config(config)?;

    let state = CsrfToken::new_random();
    let nonce = Nonce::new_random();

    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();

    let mut url = Url::parse(AUTHORIZATION_URL).map_err(|_| SsoError::Configuration)?;

    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("response_mode", "query")
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", &config.redirect_uri)
        .append_pair("scope", "openid profile email")
        .append_pair("state", state.secret())
        .append_pair("nonce", nonce.secret())
        .append_pair("code_challenge", challenge.as_str())
        .append_pair("code_challenge_method", "S256");

    Ok(AuthorizationRequest {
        url,
        state,
        nonce,
        verifier,
    })
}

impl SsoClient {
    pub async fn discover(config: &SsoConfig) -> Result<Self, SsoError> {
        validate_config(config)?;

        let http = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| SsoError::Transport)?;

        let metadata: Discovery = get_json(&http, DISCOVERY_URL).await?;

        validate_discovery(&metadata)?;

        let jwks: CoreJsonWebKeySet = get_json(&http, JWKS_URL).await?;

        if jwks.keys().is_empty() {
            return Err(SsoError::MissingKeys);
        }

        Ok(Self {
            config: config.clone(),
            jwks,
        })
    }

    pub fn signing_key_count(&self) -> usize {
        self.jwks.keys().len()
    }

    pub fn verify_id_token(
        &self,
        raw_token: &str,
        expected_nonce: &str,
    ) -> Result<VerifiedIdentity, SsoError> {
        if raw_token.len() > 16_384 || expected_nonce.is_empty() {
            return Err(SsoError::InvalidToken);
        }

        let token: SsoIdToken = raw_token.parse().map_err(|_| SsoError::InvalidToken)?;

        let issuer =
            IssuerUrl::new(self.config.issuer.clone()).map_err(|_| SsoError::Configuration)?;

        let verifier = CoreIdTokenVerifier::new_public_client(
            ClientId::new(self.config.client_id.clone()),
            issuer,
            self.jwks.clone(),
        )
        .set_allowed_algs([CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256]);

        let nonce = Nonce::new(expected_nonce.to_owned());

        let claims = token
            .claims(&verifier, &nonce)
            .map_err(|_| SsoError::InvalidToken)?;

        if claims.additional_claims().token_use != "id_token" || claims.audiences().len() != 1 {
            return Err(SsoError::InvalidToken);
        }

        let subject = claims.subject().as_str();

        if Uuid::parse_str(subject).map_or(true, |uuid| uuid.is_nil()) {
            return Err(SsoError::InvalidToken);
        }

        Ok(VerifiedIdentity {
            issuer: self.config.issuer.clone(),
            subject: subject.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config() -> SsoConfig {
        SsoConfig {
            issuer: ISSUER.to_owned(),
            client_id: "test-client".to_owned(),
            redirect_uri: "https://workspace.example.com/auth/callback".to_owned(),
        }
    }

    #[test]
    fn accepts_valid_config() {
        assert!(validate_config(&config()).is_ok());
    }

    #[test]
    fn rejects_insecure_callback() {
        let mut config = config();
        config.redirect_uri = "http://workspace.example.com/auth/callback".to_owned();

        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn rejects_unexpected_issuer() {
        let mut config = config();
        config.issuer = "https://other.example.com".to_owned();

        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn authorization_uses_pkce_s256() {
        let request = start_authorization(&config()).unwrap();

        let query: HashMap<_, _> = request.url.query_pairs().into_owned().collect();

        assert_eq!(query["response_type"], "code");
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["scope"], "openid profile email");
        assert_eq!(query["state"], *request.state.secret());
        assert_eq!(query["nonce"], *request.nonce.secret());

        let expected = PkceCodeChallenge::from_code_verifier_sha256(&request.verifier);

        assert_eq!(query["code_challenge"], expected.as_str());

        assert!(!query.contains_key("code_verifier"));
        assert!(!query.contains_key("client_secret"));
    }

    #[test]
    fn authorization_secrets_are_unique() {
        let first = start_authorization(&config()).unwrap();
        let second = start_authorization(&config()).unwrap();

        assert_ne!(first.state.secret(), second.state.secret());
        assert_ne!(first.nonce.secret(), second.nonce.secret());
        assert_ne!(first.verifier.secret(), second.verifier.secret());
    }

    #[test]
    fn rejects_malformed_id_token() {
        let client = SsoClient {
            config: config(),
            jwks: serde_json::from_str(r#"{"keys":[]}"#).unwrap(),
        };

        assert!(matches!(
            client.verify_id_token("not-a-token", "test-nonce"),
            Err(SsoError::InvalidToken)
        ));
    }
}
