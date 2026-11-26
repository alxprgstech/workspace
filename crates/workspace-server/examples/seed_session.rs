//! Test-only helper: create a real Workspace web session for a synthetic
//! identity.
//!
//! This is an example binary, not part of the shipped server. It is used by the
//! update acceptance lab to sign a browser in without an external SSO round
//! trip. It still goes through the production session code, so the cookie and
//! CSRF checks in the running server are exercised for real.
//!
//! ```text
//! cargo run -p workspace-server --example seed_session -- <config.toml> <subject> admin
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use workspace_core::config::Config;
use workspace_server::{
    auth_store::AuthStore,
    secrets::AuthSecrets,
    sso::{LoginResult, SessionTokens, VerifiedIdentity},
    users,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let config_path = arguments.next().ok_or("config path is required")?;
    let subject = arguments.next().ok_or("subject UUID is required")?;
    let role = arguments.next().unwrap_or_else(|| "admin".to_owned());

    if !matches!(role.as_str(), "admin" | "user") {
        return Err("role must be admin or user".into());
    }

    let config = Config::load(&config_path)?;
    let sso = config.sso.as_ref().ok_or("SSO must be configured")?;

    workspace_server::storage::initialize(&config.storage.data_dir)?;
    users::grant_access(&config.storage.data_dir, sso, &subject, &role)?;

    let secrets = AuthSecrets::load(&config.storage.data_dir)?;
    let store = AuthStore::new(&config.storage.data_dir, secrets.encryption_key())?;

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

    let login = LoginResult {
        identity: VerifiedIdentity {
            issuer: sso.issuer.clone(),
            subject: subject.clone(),
            expires_at_unix: now + 3600,
        },
        username: Some(format!("Lab {role}")),
        email: None,
        email_verified: false,
        tokens: SessionTokens {
            access_token: "lab-access-token".into(),
            refresh_token: "lab-refresh-token".into(),
            id_token: "lab-id-token".into(),
            access_expires_at_unix: now + 3600,
        },
    };

    let session = store
        .create_session(sso, &login)?
        .ok_or("the user is disabled")?;

    // Only the opaque handles are printed. They are session material for the
    // isolated lab, never production credentials.
    println!(
        "{}",
        serde_json::json!({
            "handle": session.handle,
            "csrf": session.csrf_token,
            "expires_at_unix": session.expires_at_unix,
        })
    );

    Ok(())
}
