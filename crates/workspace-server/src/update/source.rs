//! Release package acquisition.
//!
//! The source is either an explicitly chosen local directory/zip, the
//! configured local package directory, or the latest GitHub release of the
//! configured repository. Nothing here accepts a URL from an untrusted caller:
//! the repository comes from validated configuration.

use super::{
    LoadedPackage, UpdateError,
    manifest::{TrustAnchor, extract_zip, sha256_file, verify_package},
    platform::Layout,
};
use serde::Deserialize;
use std::{env, fs, path::PathBuf};
use workspace_core::config::{Config, UpdateSource};

const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

pub async fn load_package(
    config: &Config,
    layout: &Layout,
    explicit: Option<&std::path::Path>,
    trust: &TrustAnchor,
) -> Result<LoadedPackage, UpdateError> {
    let configured = config.update.package_dir.as_deref();

    if let Some(path) = explicit {
        return local(path, trust, layout);
    }

    if let Some(path) = configured {
        return local(path, trust, layout);
    }

    match config.update.source {
        UpdateSource::Local => {
            let default = layout.updates_dir().join("package");

            if default.exists() {
                local(&default, trust, layout)
            } else {
                Err(UpdateError::Config(format!(
                    "no release package found at {}; copy a verified package there or pass --package",
                    default.display()
                )))
            }
        }
        UpdateSource::Github => download(config, layout, trust).await,
    }
}

fn staging_dir(layout: &Layout) -> PathBuf {
    layout
        .updates_dir()
        .join("staging")
        .join(format!("source-{}", super::journal::random_token(6)))
}

fn local(
    path: &std::path::Path,
    trust: &TrustAnchor,
    layout: &Layout,
) -> Result<LoadedPackage, UpdateError> {
    if path.is_dir() {
        let package = verify_package(path, trust, format!("local: {}", path.display()))?;

        return Ok(LoadedPackage {
            package,
            package_sha256: None,
        });
    }

    if path.is_file() {
        let sha = sha256_file(path)?;

        let target = staging_dir(layout);

        if target.exists() {
            fs::remove_dir_all(&target)?;
        }

        let package = extract_zip(
            path,
            &target,
            trust,
            format!("local archive: {}", path.display()),
        )?;

        return Ok(LoadedPackage {
            package,
            package_sha256: Some(sha),
        });
    }

    Err(UpdateError::Manifest(
        super::manifest::ManifestError::Invalid(format!(
            "release package {} does not exist",
            path.display()
        )),
    ))
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

async fn download(
    config: &Config,
    layout: &Layout,
    trust: &TrustAnchor,
) -> Result<LoadedPackage, UpdateError> {
    let repository = config.update.repository();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .user_agent(format!(
            "alxprgs-workspace-updater/{}",
            workspace_core::VERSION
        ))
        .build()
        .map_err(|error| UpdateError::Download(error.to_string()))?;

    let token = env::var(config.update.api_token_env())
        .ok()
        .filter(|value| !value.trim().is_empty());

    let mut request = client
        .get(format!(
            "https://api.github.com/repos/{repository}/releases/latest"
        ))
        .header("Accept", "application/vnd.github+json");

    if let Some(token) = token.as_deref() {
        request = request.bearer_auth(token);
    }

    let response = request
        .send()
        .await
        .map_err(|error| UpdateError::Download(error.to_string()))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(UpdateError::Download(format!(
            "repository {repository} has no published release"
        )));
    }

    if !response.status().is_success() {
        return Err(UpdateError::Download(format!(
            "GitHub API returned HTTP {}",
            response.status()
        )));
    }

    let release: GithubRelease = response
        .json()
        .await
        .map_err(|error| UpdateError::Download(error.to_string()))?;

    let tag = release.tag_name.trim_start_matches('v');

    let asset = release
        .assets
        .iter()
        .filter(|asset| asset.name.ends_with(".zip"))
        .find(|asset| asset.name.contains("windows"))
        .or_else(|| {
            release
                .assets
                .iter()
                .find(|asset| asset.name.ends_with(".zip"))
        })
        .ok_or_else(|| {
            UpdateError::Download(format!("release {tag} has no Windows package asset"))
        })?;

    if asset.size > MAX_DOWNLOAD_BYTES {
        return Err(UpdateError::Download("release asset is too large".into()));
    }

    let mut request = client.get(&asset.browser_download_url);

    if let Some(token) = token.as_deref() {
        request = request.bearer_auth(token);
    }

    let response = request
        .send()
        .await
        .map_err(|error| UpdateError::Download(error.to_string()))?;

    if !response.status().is_success() {
        return Err(UpdateError::Download(format!(
            "release download returned HTTP {}",
            response.status()
        )));
    }

    if response
        .content_length()
        .is_some_and(|length| length > MAX_DOWNLOAD_BYTES)
    {
        return Err(UpdateError::Download("release asset is too large".into()));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|error| UpdateError::Download(error.to_string()))?;

    if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
        return Err(UpdateError::Download("release asset is too large".into()));
    }

    let staging = staging_dir(layout);

    fs::create_dir_all(&staging)?;

    let archive = staging.join("release.zip");

    fs::write(&archive, &bytes)?;

    let sha = super::manifest::sha256_hex(&bytes);

    let extracted = staging.join("package");

    let package = extract_zip(
        &archive,
        &extracted,
        trust,
        format!("github: {repository} {}", release.tag_name),
    )?;

    // The manifest is authoritative; a mismatched tag is a packaging error.
    if !release.tag_name.is_empty()
        && release.tag_name != package.manifest.version
        && release.tag_name != format!("v{}", package.manifest.version)
    {
        return Err(UpdateError::Download(format!(
            "release tag {} does not match the signed manifest version {}",
            release.tag_name, package.manifest.version
        )));
    }

    Ok(LoadedPackage {
        package,
        package_sha256: Some(sha),
    })
}
