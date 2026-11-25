//! Validation of MCP access tokens (stage 007 foundation).
//!
//! MCP credentials are deliberately independent from web sessions: they are
//! self-contained RS256 tokens issued for the MCP resource, validated against
//! a JWKS snapshot. Nothing here is shared with the browser session store.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use serde::Deserialize;
use thiserror::Error;

/// Clock skew tolerated for `exp`/`nbf`, in seconds.
pub const LEEWAY_SECONDS: i64 = 60;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TokenError {
    #[error("malformed token")]
    Malformed,
    #[error("unsupported token algorithm")]
    UnsupportedAlgorithm,
    #[error("unknown signing key")]
    UnknownKey,
    #[error("invalid token signature")]
    InvalidSignature,
    #[error("unexpected token issuer")]
    WrongIssuer,
    #[error("unexpected token audience")]
    WrongAudience,
    #[error("unexpected token resource")]
    WrongResource,
    #[error("token expired")]
    Expired,
    #[error("token not yet valid")]
    NotYetValid,
    #[error("required scope missing")]
    MissingScope,
}

/// What an MCP access token must prove to reach the gateway.
#[derive(Debug, Clone)]
pub struct McpExpectations {
    pub issuer: String,
    pub audience: String,
    pub resource: String,
    pub required_scopes: Vec<String>,
}

/// A verified, minimal view of the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCaller {
    pub subject: String,
    pub scopes: Vec<String>,
}

/// One RSA signing key from a JWKS document.
#[derive(Debug, Clone)]
pub struct Jwk {
    pub kid: Option<String>,
    pub modulus: Vec<u8>,
    pub exponent: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct RawJwks {
    keys: Vec<RawJwk>,
}

#[derive(Debug, Deserialize)]
struct RawJwk {
    kid: Option<String>,
    kty: String,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
}

/// Parse a JWKS document, keeping only usable RSA signing keys.
///
/// Non-RSA entries are dropped rather than trusted; an empty result means the
/// document cannot verify anything and callers fail closed.
pub fn parse_jwks(document: &str) -> Result<Vec<Jwk>, TokenError> {
    let raw: RawJwks = serde_json::from_str(document).map_err(|_| TokenError::Malformed)?;

    let mut keys = Vec::new();

    for key in raw.keys {
        if key.kty != "RSA" {
            continue;
        }

        let (Some(modulus), Some(exponent)) = (key.n, key.e) else {
            continue;
        };

        let (Ok(modulus), Ok(exponent)) = (
            URL_SAFE_NO_PAD.decode(modulus),
            URL_SAFE_NO_PAD.decode(exponent),
        ) else {
            continue;
        };

        if modulus.is_empty() || exponent.is_empty() {
            continue;
        }

        keys.push(Jwk {
            kid: key.kid,
            modulus,
            exponent,
        });
    }

    Ok(keys)
}

#[derive(Debug, Deserialize)]
struct Header {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
    #[serde(default)]
    typ: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Claims {
    iss: String,
    #[serde(default)]
    aud: Option<Audience>,
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    sub: Option<String>,
    exp: i64,
    #[serde(default)]
    nbf: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    scp: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn contains(&self, expected: &str) -> bool {
        match self {
            Self::One(value) => value == expected,
            Self::Many(values) => values.iter().any(|value| value == expected),
        }
    }
}

/// Verify an RS256 access token against a JWKS snapshot.
///
/// `now` is passed explicitly so expiry behavior is testable and does not
/// depend on the wall clock.
pub fn verify_access_token(
    token: &str,
    keys: &[Jwk],
    expectations: &McpExpectations,
    now: i64,
) -> Result<McpCaller, TokenError> {
    let mut parts = token.split('.');

    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(TokenError::Malformed);
    };

    if header.is_empty() || payload.is_empty() || signature.is_empty() {
        return Err(TokenError::Malformed);
    }

    let header: Header = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(header)
            .map_err(|_| TokenError::Malformed)?,
    )
    .map_err(|_| TokenError::Malformed)?;

    if header.alg != "RS256" {
        return Err(TokenError::UnsupportedAlgorithm);
    }

    if header
        .typ
        .as_deref()
        .is_some_and(|typ| typ != "JWT" && typ != "at+jwt")
    {
        return Err(TokenError::Malformed);
    }

    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| TokenError::Malformed)?;

    let signed = token
        .rsplit_once('.')
        .map(|(signed, _)| signed)
        .ok_or(TokenError::Malformed)?;

    let key = match header.kid.as_deref() {
        Some(kid) => keys
            .iter()
            .find(|key| key.kid.as_deref() == Some(kid))
            .ok_or(TokenError::UnknownKey)?,
        // Without a kid only an unambiguous single-key JWKS is usable.
        None => match keys {
            [key] => key,
            _ => return Err(TokenError::UnknownKey),
        },
    };

    RsaPublicKeyComponents {
        n: &key.modulus,
        e: &key.exponent,
    }
    .verify(&RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &signature)
    .map_err(|_| TokenError::InvalidSignature)?;

    let claims: Claims = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| TokenError::Malformed)?,
    )
    .map_err(|_| TokenError::Malformed)?;

    if claims.iss != expectations.issuer {
        return Err(TokenError::WrongIssuer);
    }

    let audience_ok = claims
        .aud
        .as_ref()
        .is_some_and(|audience| audience.contains(&expectations.audience));

    if !audience_ok {
        return Err(TokenError::WrongAudience);
    }

    // RFC 8707: when the token names its resource it must name ours.
    if claims
        .resource
        .as_deref()
        .is_some_and(|resource| resource != expectations.resource)
    {
        return Err(TokenError::WrongResource);
    }

    if claims.exp <= now - LEEWAY_SECONDS {
        return Err(TokenError::Expired);
    }

    if claims.nbf.is_some_and(|nbf| nbf > now + LEEWAY_SECONDS) {
        return Err(TokenError::NotYetValid);
    }

    let mut scopes: Vec<String> = claims
        .scope
        .as_deref()
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect();

    if let Some(list) = claims.scp {
        for scope in list {
            let scope = scope.trim().to_owned();

            if !scope.is_empty() && !scopes.contains(&scope) {
                scopes.push(scope);
            }
        }
    }

    // Normalise names so a legacy read-only token keeps exactly read access
    // and can never be mistaken for a wider grant.
    let mut canonical = crate::oauth_store::parse_scopes(&scopes.join(" "));
    canonical.sort();
    canonical.dedup();

    let required: Vec<String> = expectations
        .required_scopes
        .iter()
        .flat_map(|scope| crate::oauth_store::parse_scopes(scope))
        .collect();

    for scope in &required {
        if !canonical.contains(scope) {
            return Err(TokenError::MissingScope);
        }
    }

    let subject = claims.sub.ok_or(TokenError::Malformed)?;

    if subject.is_empty() {
        return Err(TokenError::Malformed);
    }

    Ok(McpCaller {
        subject,
        scopes: canonical,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::{
        RsaPrivateKey,
        pkcs1v15::SigningKey,
        sha2::Sha256,
        signature::{SignatureEncoding, Signer},
        traits::PublicKeyParts,
    };
    use serde_json::{Value, json};

    const ISSUER: &str = "https://auth.alxprgs.tech";
    const AUDIENCE: &str = "https://workspace.example.com/chatgpt/mcp";
    const RESOURCE: &str = "https://workspace.example.com/chatgpt/mcp";
    const SUBJECT: &str = "550e8400-e29b-41d4-a716-446655440000";
    const NOW: i64 = 1_800_000_000;

    fn expectations() -> McpExpectations {
        McpExpectations {
            issuer: ISSUER.to_owned(),
            audience: AUDIENCE.to_owned(),
            resource: RESOURCE.to_owned(),
            required_scopes: vec!["workspace.read".to_owned()],
        }
    }

    struct Fixture {
        signing: SigningKey<Sha256>,
        keys: Vec<Jwk>,
    }

    fn fixture() -> Fixture {
        let private = RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap();

        let jwks = json!({
            "keys": [{
                "kty": "RSA",
                "kid": "mcp-key",
                "use": "sig",
                "alg": "RS256",
                "n": URL_SAFE_NO_PAD.encode(private.n().to_bytes_be()),
                "e": URL_SAFE_NO_PAD.encode(private.e().to_bytes_be()),
            }]
        });

        Fixture {
            signing: SigningKey::<Sha256>::new(private),
            keys: parse_jwks(&jwks.to_string()).unwrap(),
        }
    }

    fn sign(signing: &SigningKey<Sha256>, claims: &Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT","kid":"mcp-key"}"#);

        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());

        let signed = format!("{header}.{payload}");

        let signature = signing.sign(signed.as_bytes());

        format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
    }

    fn claims() -> Value {
        json!({
            "iss": ISSUER,
            "aud": AUDIENCE,
            "sub": SUBJECT,
            "resource": RESOURCE,
            "exp": NOW + 600,
            "scope": "workspace.read workspace.write",
        })
    }

    #[test]
    fn accepts_a_valid_token() {
        let fixture = fixture();

        let token = sign(&fixture.signing, &claims());

        let caller = verify_access_token(&token, &fixture.keys, &expectations(), NOW).unwrap();

        assert_eq!(caller.subject, SUBJECT);
        // Legacy names are normalised: `workspace.read` grants read access and
        // an unknown `workspace.write` is not silently treated as write access.
        assert_eq!(caller.scopes, vec!["workspace:read".to_owned()]);
    }

    #[test]
    fn rejects_expired_tokens_beyond_leeway() {
        let fixture = fixture();

        let mut claims = claims();
        claims["exp"] = json!(NOW - LEEWAY_SECONDS - 1);

        let token = sign(&fixture.signing, &claims);

        assert_eq!(
            verify_access_token(&token, &fixture.keys, &expectations(), NOW),
            Err(TokenError::Expired)
        );

        let mut within = claims.clone();
        within["exp"] = json!(NOW - LEEWAY_SECONDS + 1);

        let token = sign(&fixture.signing, &within);

        assert!(verify_access_token(&token, &fixture.keys, &expectations(), NOW).is_ok());
    }

    #[test]
    fn rejects_not_yet_valid_tokens() {
        let fixture = fixture();

        let mut claims = claims();
        claims["nbf"] = json!(NOW + LEEWAY_SECONDS + 5);

        let token = sign(&fixture.signing, &claims);

        assert_eq!(
            verify_access_token(&token, &fixture.keys, &expectations(), NOW),
            Err(TokenError::NotYetValid)
        );
    }

    #[test]
    fn rejects_wrong_issuer_audience_and_resource() {
        let fixture = fixture();

        for (field, value, expected) in [
            (
                "iss",
                json!("https://evil.example.com"),
                TokenError::WrongIssuer,
            ),
            ("aud", json!("another-resource"), TokenError::WrongAudience),
            (
                "resource",
                json!("https://evil.example.com/chatgpt/mcp"),
                TokenError::WrongResource,
            ),
        ] {
            let mut claims = claims();
            claims[field] = value;

            let token = sign(&fixture.signing, &claims);

            assert_eq!(
                verify_access_token(&token, &fixture.keys, &expectations(), NOW),
                Err(expected),
                "{field}"
            );
        }
    }

    #[test]
    fn accepts_multiple_audiences() {
        let fixture = fixture();

        let mut claims = claims();
        claims["aud"] = json!([AUDIENCE, "https://other.example.com"]);

        let token = sign(&fixture.signing, &claims);

        assert!(verify_access_token(&token, &fixture.keys, &expectations(), NOW).is_ok());
    }

    #[test]
    fn rejects_missing_scope() {
        let fixture = fixture();

        let mut claims = claims();
        claims["scope"] = json!("workspace.write");

        let token = sign(&fixture.signing, &claims);

        assert_eq!(
            verify_access_token(&token, &fixture.keys, &expectations(), NOW),
            Err(TokenError::MissingScope)
        );
    }

    #[test]
    fn rejects_tampered_payload_and_wrong_key() {
        let fixture = fixture();
        let other = self::fixture();

        let token = sign(&fixture.signing, &claims());

        // Signed by a different key that shares the same kid.
        let other_token = sign(&other.signing, &claims());

        assert_eq!(
            verify_access_token(&other_token, &fixture.keys, &expectations(), NOW),
            Err(TokenError::InvalidSignature)
        );

        // Payload swapped after signing.
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged = URL_SAFE_NO_PAD.encode(json!({ "exp": NOW + 600 }).to_string());

        parts[1] = &forged;

        let forged = parts.join(".");

        assert_eq!(
            verify_access_token(&forged, &fixture.keys, &expectations(), NOW),
            Err(TokenError::InvalidSignature)
        );
    }

    #[test]
    fn rejects_unknown_key_algorithm_and_malformed_tokens() {
        let fixture = fixture();

        // `alg: none` with a signature-shaped tail must be refused as an
        // unsupported algorithm, never accepted as "unsigned".
        let unsigned = format!(
            "{}.{}.{}",
            URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT"}"#),
            URL_SAFE_NO_PAD.encode(claims().to_string()),
            URL_SAFE_NO_PAD.encode([0_u8; 256])
        );

        assert_eq!(
            verify_access_token(&unsigned, &fixture.keys, &expectations(), NOW),
            Err(TokenError::UnsupportedAlgorithm)
        );

        // A token with no signature at all is malformed.
        let empty_signature = format!(
            "{}.{}.",
            URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT"}"#),
            URL_SAFE_NO_PAD.encode(claims().to_string())
        );

        assert_eq!(
            verify_access_token(&empty_signature, &fixture.keys, &expectations(), NOW),
            Err(TokenError::Malformed)
        );

        let unknown_kid = format!(
            "{}.{}.{}",
            URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT","kid":"other"}"#),
            URL_SAFE_NO_PAD.encode(claims().to_string()),
            URL_SAFE_NO_PAD.encode([0_u8; 256])
        );

        assert_eq!(
            verify_access_token(&unknown_kid, &fixture.keys, &expectations(), NOW),
            Err(TokenError::UnknownKey)
        );

        for token in ["", "a.b", "a.b.c.d", "not-a-token"] {
            assert_eq!(
                verify_access_token(token, &fixture.keys, &expectations(), NOW),
                Err(TokenError::Malformed),
                "{token}"
            );
        }
    }

    #[test]
    fn parses_only_usable_rsa_keys() {
        let jwks = json!({
            "keys": [
                {"kty": "EC", "kid": "ec"},
                {"kty": "RSA", "kid": "broken", "n": "!!", "e": "AQAB"},
                {"kty": "RSA", "kid": "ok", "n": "AQAB", "e": "AQAB"}
            ]
        });

        let keys = parse_jwks(&jwks.to_string()).unwrap();

        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].kid.as_deref(), Some("ok"));

        assert!(matches!(parse_jwks("nonsense"), Err(TokenError::Malformed)));
    }
}
