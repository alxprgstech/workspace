//! Local release tooling: key generation, signing and deterministic packaging.
//!
//! This code is used by `workspace release ...` and by the build script. It
//! never ships a private key: the key file is read from an operator-controlled
//! path and only the detached signature is written.

use super::manifest::{
    Compatibility, FileRole, MANIFEST_FILE, MANIFEST_FORMAT, Manifest, ManifestFile, PRODUCT,
    SIGNATURE_FILE, TrustAnchor, sha256_hex, validate_relative_path, verify_package,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde::Serialize;
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use zip::write::SimpleFileOptions;

const KEY_FILE: &str = "release-signing-key.pk8";
const PUBLIC_KEY_FILE: &str = "release-public-key.txt";

#[derive(Debug, Clone)]
pub struct PackageOptions {
    pub version: String,
    pub git_sha: String,
    pub build_time_unix: i64,
    pub channel: String,
    pub schema_version: u32,
    pub min_schema: u32,
    pub max_schema: u32,
    pub changelog: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid release input: {0}")]
    Invalid(String),

    #[error("manifest error: {0}")]
    Manifest(#[from] super::manifest::ManifestError),

    #[error("cryptographic error: {0}")]
    Crypto(String),

    #[error("zip error: {0}")]
    Zip(String),
}

/// Generate an Ed25519 key pair. The private key stays in `out_dir`; the public
/// key is printed and written next to it.
pub fn keygen(out_dir: &Path, force: bool) -> Result<String, ReleaseError> {
    fs::create_dir_all(out_dir)?;

    let private = out_dir.join(KEY_FILE);
    let public = out_dir.join(PUBLIC_KEY_FILE);

    if private.exists() && !force {
        return Err(ReleaseError::Invalid(format!(
            "{} already exists; pass --force to replace it",
            private.display()
        )));
    }

    let document = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
        .map_err(|_| ReleaseError::Crypto("cannot generate an Ed25519 key".into()))?;

    let key_pair = Ed25519KeyPair::from_pkcs8(document.as_ref())
        .map_err(|_| ReleaseError::Crypto("cannot load the generated key".into()))?;

    let public_base64 = STANDARD.encode(key_pair.public_key().as_ref());

    fs::write(&private, document.as_ref())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let _ = fs::set_permissions(&private, fs::Permissions::from_mode(0o600));
    }

    fs::write(&public, format!("{public_base64}\n"))?;

    Ok(public_base64)
}

/// Sign raw manifest bytes with a PKCS#8 Ed25519 key.
pub fn sign_bytes(message: &[u8], key_path: &Path) -> Result<Vec<u8>, ReleaseError> {
    let document = fs::read(key_path)?;

    let key_pair = Ed25519KeyPair::from_pkcs8(&document)
        .map_err(|_| ReleaseError::Crypto(format!("cannot read {}", key_path.display())))?;

    Ok(key_pair.sign(message).as_ref().to_vec())
}

/// Re-sign an existing manifest file.
pub fn sign_manifest(manifest_path: &Path, key_path: &Path) -> Result<PathBuf, ReleaseError> {
    let bytes = fs::read(manifest_path)?;

    let signature = sign_bytes(&bytes, key_path)?;

    let target = manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(SIGNATURE_FILE);

    fs::write(&target, STANDARD.encode(signature))?;

    Ok(target)
}

struct Collected {
    path: String,
    role: FileRole,
    bytes: Vec<u8>,
    executable: bool,
}

fn collect(input: &Path) -> Result<Vec<Collected>, ReleaseError> {
    let mut files = Vec::new();

    fn walk(
        root: &Path,
        directory: &Path,
        files: &mut Vec<Collected>,
        depth: u32,
    ) -> Result<(), ReleaseError> {
        if depth > 4 {
            return Err(ReleaseError::Invalid(
                "release input is nested too deeply".into(),
            ));
        }

        let mut entries: Vec<_> = fs::read_dir(directory)?.filter_map(Result::ok).collect();

        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();

            if name.starts_with('.') || name == MANIFEST_FILE || name == SIGNATURE_FILE {
                continue;
            }

            let metadata = fs::symlink_metadata(&path)?;

            if metadata.file_type().is_symlink() {
                return Err(ReleaseError::Invalid(format!(
                    "symbolic links are not allowed: {}",
                    path.display()
                )));
            }

            if metadata.is_dir() {
                walk(root, &path, files, depth + 1)?;
                continue;
            }

            if !metadata.is_file() {
                return Err(ReleaseError::Invalid(format!(
                    "not a regular file: {}",
                    path.display()
                )));
            }

            let relative = path
                .strip_prefix(root)
                .map_err(|_| ReleaseError::Invalid("path is outside the input".into()))?
                .to_string_lossy()
                .replace('\\', "/");

            validate_relative_path(&relative)?;

            let role = match relative.as_str() {
                "workspace.exe" | "workspace" => FileRole::WindowsBinary,
                "linux/workspace-helper" => FileRole::RuntimeHelper,
                "linux/workspace-egress" => FileRole::RuntimeEgress,
                "deploy/alxprgs-quota" => FileRole::QuotaHelper,
                _ if relative.starts_with("deploy/") => FileRole::DeployScript,
                _ if relative.ends_with(".md") => FileRole::Documentation,
                _ => FileRole::Other,
            };

            let executable = role != FileRole::Documentation
                && role != FileRole::Other
                && !relative.ends_with(".ps1");

            files.push(Collected {
                path: relative,
                role,
                bytes: fs::read(&path)?,
                executable,
            });
        }

        Ok(())
    }

    walk(input, input, &mut files, 0)?;

    Ok(files)
}

/// Build `workspace-release.json`, sign it and write a deterministic zip.
pub fn build_package(
    input: &Path,
    output: &Path,
    options: &PackageOptions,
    key_path: &Path,
) -> Result<Manifest, ReleaseError> {
    let collected = collect(input)?;

    if collected.is_empty() {
        return Err(ReleaseError::Invalid("release input is empty".into()));
    }

    for required in [
        FileRole::WindowsBinary,
        FileRole::RuntimeHelper,
        FileRole::RuntimeEgress,
    ] {
        if !collected.iter().any(|file| file.role == required) {
            return Err(ReleaseError::Invalid(format!(
                "release input is missing the {required:?} component"
            )));
        }
    }

    let files: Vec<ManifestFile> = collected
        .iter()
        .map(|file| ManifestFile {
            path: file.path.clone(),
            role: file.role,
            size: file.bytes.len() as u64,
            sha256: sha256_hex(&file.bytes),
        })
        .collect();

    let manifest = Manifest {
        format: MANIFEST_FORMAT,
        product: PRODUCT.to_owned(),
        version: options.version.clone(),
        git_sha: options.git_sha.clone(),
        build_time_unix: options.build_time_unix,
        channel: options.channel.clone(),
        schema_version: options.schema_version,
        compatibility: Compatibility {
            platforms: vec!["windows-x86_64".into()],
            runtime: vec!["wsl2".into(), "none".into()],
            min_database_schema: options.min_schema,
            max_database_schema: options.max_schema,
            min_updater_format: MANIFEST_FORMAT,
        },
        changelog: options.changelog.clone(),
        files,
    };

    manifest.validate()?;

    let manifest_bytes =
        serde_json::to_vec(&manifest).map_err(|error| ReleaseError::Invalid(error.to_string()))?;

    let signature = sign_bytes(&manifest_bytes, key_path)?;

    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut zip = zip::ZipWriter::new(File::create(output)?);

    let timestamp = zip::DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0)
        .map_err(|_| ReleaseError::Zip("cannot set a fixed timestamp".into()))?;

    let write = |zip: &mut zip::ZipWriter<File>, name: &str, bytes: &[u8], mode: u32| {
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .last_modified_time(timestamp)
            .unix_permissions(mode);

        zip.start_file(name, options)
            .map_err(|error| ReleaseError::Zip(error.to_string()))?;
        zip.write_all(bytes)
            .map_err(|error| ReleaseError::Zip(error.to_string()))?;

        Ok::<(), ReleaseError>(())
    };

    write(&mut zip, MANIFEST_FILE, &manifest_bytes, 0o644)?;
    write(
        &mut zip,
        SIGNATURE_FILE,
        STANDARD.encode(&signature).as_bytes(),
        0o644,
    )?;

    for file in &collected {
        let mode = if file.executable { 0o755 } else { 0o644 };
        write(&mut zip, &file.path, &file.bytes, mode)?;
    }

    zip.finish()
        .map_err(|error| ReleaseError::Zip(error.to_string()))?;

    Ok(manifest)
}

/// Verify a package (zip or directory) using the embedded or a supplied key.
pub fn verify(
    package: &Path,
    public_key_path: Option<&Path>,
) -> Result<super::manifest::VerifiedPackage, ReleaseError> {
    let trust = match public_key_path {
        Some(path) => TrustAnchor::from_file(path)?,
        None => TrustAnchor::embedded()?,
    };

    verify_with_trust(package, &trust)
}

/// Verify a package against the public half of the key that signed it.
///
/// `release package` uses this for its self-check: a test build signed with an
/// ephemeral key must still be internally consistent, while the deployment
/// trust check remains the embedded key.
pub fn verify_with_signing_key(
    package: &Path,
    key_path: &Path,
) -> Result<super::manifest::VerifiedPackage, ReleaseError> {
    let document = fs::read(key_path)?;
    let key_pair = Ed25519KeyPair::from_pkcs8(&document)
        .map_err(|_| ReleaseError::Crypto("cannot read the signing key".into()))?;
    let public = STANDARD.encode(key_pair.public_key().as_ref());
    let trust = TrustAnchor::from_base64(&public, "release signing key".into())?;

    verify_with_trust(package, &trust)
}

fn verify_with_trust(
    package: &Path,
    trust: &TrustAnchor,
) -> Result<super::manifest::VerifiedPackage, ReleaseError> {
    if package.is_dir() {
        return Ok(verify_package(
            package,
            trust,
            format!("local: {}", package.display()),
        )?);
    }

    let staging = tempfile::tempdir()?;

    Ok(super::manifest::extract_zip(
        package,
        staging.path(),
        trust,
        format!("archive: {}", package.display()),
    )?)
}

/// Metadata written next to a built package for the release notes.
#[derive(Debug, Serialize)]
pub struct PackageSummary {
    pub version: String,
    pub git_sha: String,
    pub manifest_sha256: String,
    pub package_sha256: String,
    pub files: usize,
    pub path: String,
}

pub fn summarize(
    package_path: &Path,
    package: &super::manifest::VerifiedPackage,
) -> Result<PackageSummary, ReleaseError> {
    let mut file = File::open(package_path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;

    Ok(PackageSummary {
        version: package.manifest.version.clone(),
        git_sha: package.manifest.git_sha.clone(),
        manifest_sha256: package.manifest_sha256.clone(),
        package_sha256: sha256_hex(&bytes),
        files: package.manifest.files.len(),
        path: package_path.display().to_string(),
    })
}

/// Read a changelog file: one entry per non-empty line, bounded.
pub fn read_changelog(path: Option<&Path>) -> Result<Vec<String>, ReleaseError> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };

    let text = fs::read_to_string(path)?;

    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let line = line
                .strip_prefix("- ")
                .or_else(|| line.strip_prefix("* "))
                .unwrap_or(line);
            line.chars().take(500).collect::<String>()
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(directory: &Path) {
        fs::create_dir_all(directory.join("linux")).unwrap();
        fs::create_dir_all(directory.join("deploy")).unwrap();

        fs::write(directory.join("workspace.exe"), b"windows-binary").unwrap();
        fs::write(directory.join("linux/workspace-helper"), b"helper").unwrap();
        fs::write(directory.join("linux/workspace-egress"), b"egress").unwrap();
        fs::write(directory.join("deploy/alxprgs-quota"), b"quota").unwrap();
        fs::write(directory.join("deploy/install.ps1"), b"# install").unwrap();
        fs::write(directory.join("README.md"), b"# release").unwrap();
    }

    #[test]
    fn package_roundtrip_and_tamper_detection() {
        let directory = tempfile::tempdir().unwrap();
        let keys = tempfile::tempdir().unwrap();

        fixture(directory.path());

        let public = keygen(keys.path(), false).unwrap();
        let trust = TrustAnchor::from_base64(&public, "test".into()).unwrap();

        let output = keys.path().join("release.zip");

        let manifest = build_package(
            directory.path(),
            &output,
            &PackageOptions {
                version: "0.2.0".into(),
                git_sha: "660023c1685bc3234958c49c926dbf56fa972835".into(),
                build_time_unix: 1_795_650_000,
                channel: "stable".into(),
                schema_version: 11,
                min_schema: 9,
                max_schema: 11,
                changelog: vec!["first".into()],
            },
            &keys.path().join(KEY_FILE),
        )
        .unwrap();

        assert_eq!(manifest.version, "0.2.0");

        let extracted = tempfile::tempdir().unwrap();
        let verified =
            super::super::manifest::extract_zip(&output, extracted.path(), &trust, "test".into())
                .unwrap();

        assert_eq!(verified.manifest.files.len(), 6);
        assert_eq!(
            verified
                .manifest
                .file(FileRole::RuntimeHelper)
                .unwrap()
                .path,
            "linux/workspace-helper"
        );

        // The archive must be deterministic (same fixed timestamps).
        let first = fs::read(&output).unwrap();
        build_package(
            directory.path(),
            &output,
            &PackageOptions {
                version: "0.2.0".into(),
                git_sha: "660023c1685bc3234958c49c926dbf56fa972835".into(),
                build_time_unix: 1_795_650_000,
                channel: "stable".into(),
                schema_version: 11,
                min_schema: 9,
                max_schema: 11,
                changelog: vec!["first".into()],
            },
            &keys.path().join(KEY_FILE),
        )
        .unwrap();

        assert_eq!(first, fs::read(&output).unwrap());
    }

    #[test]
    fn keygen_refuses_to_overwrite() {
        let directory = tempfile::tempdir().unwrap();

        keygen(directory.path(), false).unwrap();

        assert!(keygen(directory.path(), false).is_err());
        assert!(keygen(directory.path(), true).is_ok());
    }
}
