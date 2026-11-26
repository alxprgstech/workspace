//! Release manifest, authenticity and package verification.
//!
//! A release is a plain directory (or a zip of it) that contains
//! `workspace-release.json`, a detached Ed25519 signature
//! `workspace-release.sig` and the listed files. The signature covers the exact
//! manifest bytes; the manifest pins every file's size and SHA-256. Nothing in
//! the package is executed before both checks pass.

use base64::{Engine, engine::general_purpose::STANDARD};
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    str::FromStr,
};

pub const MANIFEST_FILE: &str = "workspace-release.json";
pub const SIGNATURE_FILE: &str = "workspace-release.sig";
pub const MANIFEST_FORMAT: u32 = 1;
pub const PRODUCT: &str = "ALXPRGS Workspace";
pub const PLATFORM_WINDOWS_X64: &str = "windows-x86_64";

const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_FILES: usize = 64;
const MAX_CHANGELOG_ENTRIES: usize = 40;
const MAX_CHANGELOG_LINE: usize = 500;

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("cannot read the release package: {0}")]
    Io(#[from] io::Error),

    #[error("invalid release manifest: {0}")]
    Invalid(String),

    #[error("the release signature is missing or invalid")]
    Signature,

    #[error("the release package is not signed by the trusted key")]
    Untrusted,

    #[error("release file {path:?} failed verification: {reason}")]
    File { path: String, reason: String },

    #[error("unsupported release package: {0}")]
    Unsupported(String),

    #[error("release download failed: {0}")]
    Download(String),
}

/// One file pinned by the manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManifestFile {
    pub path: String,
    pub role: FileRole,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum FileRole {
    WindowsBinary,
    RuntimeHelper,
    RuntimeEgress,
    QuotaHelper,
    DeployScript,
    Documentation,
    Other,
}

impl FileRole {
    pub fn is_runtime_binary(self) -> bool {
        matches!(
            self,
            Self::RuntimeHelper | Self::RuntimeEgress | Self::QuotaHelper
        )
    }
}

/// Compatibility declared by the release.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Compatibility {
    /// Target platforms, e.g. `["windows-x86_64"]`.
    pub platforms: Vec<String>,
    /// Runtime backends the release is tested with, e.g. `["wsl2", "none"]`.
    pub runtime: Vec<String>,
    /// Oldest database schema this release can migrate.
    pub min_database_schema: u32,
    /// Newest database schema the previous installation may already have.
    pub max_database_schema: u32,
    /// Manifest format the updater must understand.
    pub min_updater_format: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    pub product: String,
    pub version: String,
    pub git_sha: String,
    pub build_time_unix: i64,
    pub channel: String,
    pub schema_version: u32,
    pub compatibility: Compatibility,
    pub changelog: Vec<String>,
    pub files: Vec<ManifestFile>,
}

impl Manifest {
    /// Parse and structurally validate. The signature is checked separately,
    /// before the bytes are interpreted.
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(ManifestError::Invalid("manifest is too large".into()));
        }

        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|error| ManifestError::Invalid(error.to_string()))?;

        manifest.validate()?;

        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        let invalid = |message: &str| ManifestError::Invalid(message.to_owned());

        if self.format != MANIFEST_FORMAT {
            return Err(invalid("unsupported manifest format"));
        }

        if self.product != PRODUCT {
            return Err(invalid("unexpected product"));
        }

        ReleaseVersion::from_str(&self.version)
            .map_err(|_| invalid("version must be a plain semantic version"))?;

        if self.git_sha.is_empty()
            || self.git_sha.len() > 80
            || !self
                .git_sha
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-' || byte.is_ascii_lowercase())
        {
            return Err(invalid("git_sha is not a commit identifier"));
        }

        if self.channel.is_empty() || self.channel.len() > 32 {
            return Err(invalid("invalid release channel"));
        }

        if self.schema_version == 0 || self.schema_version > 1000 {
            return Err(invalid("invalid database schema version"));
        }

        let compatibility = &self.compatibility;

        if compatibility.platforms.is_empty() {
            return Err(invalid("compatibility.platforms is empty"));
        }

        if !compatibility
            .platforms
            .iter()
            .all(|value| value == PLATFORM_WINDOWS_X64 || value == "linux-x86_64")
        {
            return Err(invalid("unknown platform in compatibility"));
        }

        if compatibility.runtime.is_empty()
            || !compatibility
                .runtime
                .iter()
                .all(|value| matches!(value.as_str(), "wsl2" | "rootless_docker" | "none"))
        {
            return Err(invalid("invalid runtime compatibility"));
        }

        if compatibility.min_database_schema == 0
            || compatibility.min_database_schema > compatibility.max_database_schema
            || compatibility.max_database_schema > self.schema_version
        {
            return Err(invalid("invalid database schema compatibility range"));
        }

        if compatibility.min_updater_format == 0
            || compatibility.min_updater_format > MANIFEST_FORMAT
        {
            return Err(invalid("the release needs a newer updater"));
        }

        if self.changelog.len() > MAX_CHANGELOG_ENTRIES {
            return Err(invalid("changelog is too long"));
        }

        if self
            .changelog
            .iter()
            .any(|line| line.len() > MAX_CHANGELOG_LINE || line.contains('\0'))
        {
            return Err(invalid("changelog entry is invalid"));
        }

        if self.files.is_empty() || self.files.len() > MAX_FILES {
            return Err(invalid("invalid file list"));
        }

        let mut paths = BTreeSet::new();
        let mut roles = BTreeSet::new();
        let mut total = 0u64;

        for file in &self.files {
            validate_relative_path(&file.path)?;

            if !paths.insert(file.path.clone()) {
                return Err(invalid("duplicate file path"));
            }

            // Only the executable components are singletons. A release may
            // carry several deploy scripts or documents.
            if matches!(
                file.role,
                FileRole::WindowsBinary
                    | FileRole::RuntimeHelper
                    | FileRole::RuntimeEgress
                    | FileRole::QuotaHelper
            ) && !roles.insert(file.role)
            {
                return Err(invalid("duplicate singleton file role"));
            }

            if file.size == 0 || file.size > MAX_FILE_BYTES {
                return Err(invalid("invalid file size"));
            }

            total = total.saturating_add(file.size);

            if total > MAX_TOTAL_BYTES {
                return Err(invalid("release package is too large"));
            }

            if file.sha256.len() != 64
                || !file
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(invalid("invalid SHA-256 digest"));
            }
        }

        for role in [
            FileRole::WindowsBinary,
            FileRole::RuntimeHelper,
            FileRole::RuntimeEgress,
        ] {
            if !self.files.iter().any(|file| file.role == role) {
                return Err(invalid("a required release component is missing"));
            }
        }

        Ok(())
    }

    pub fn file(&self, role: FileRole) -> Option<&ManifestFile> {
        self.files.iter().find(|file| file.role == role)
    }

    pub fn file_by_path(&self, path: &str) -> Option<&ManifestFile> {
        self.files.iter().find(|file| file.path == path)
    }

    /// Release version as a comparable value.
    pub fn release_version(&self) -> Result<ReleaseVersion, ManifestError> {
        ReleaseVersion::from_str(&self.version)
            .map_err(|_| ManifestError::Invalid("invalid version".into()))
    }
}

/// `major.minor.patch` only: release identifiers must not be ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReleaseVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl FromStr for ReleaseVersion {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut parts = value.split('.');

        let major = parts.next().ok_or(())?.parse().map_err(|_| ())?;
        let minor = parts.next().ok_or(())?.parse().map_err(|_| ())?;
        let patch = parts.next().ok_or(())?.parse().map_err(|_| ())?;

        if parts.next().is_some() {
            return Err(());
        }

        Ok(Self {
            major,
            minor,
            patch,
        })
    }
}

impl std::fmt::Display for ReleaseVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Reject absolute paths, drive prefixes, backslashes and `..` components.
pub fn validate_relative_path(path: &str) -> Result<(), ManifestError> {
    let invalid = |message: &str| ManifestError::Invalid(message.to_owned());

    if path.is_empty() || path.len() > 512 {
        return Err(invalid("empty or overlong path"));
    }

    if path.contains('\\') || path.contains('\0') || path.starts_with('/') {
        return Err(invalid("release paths must use forward slashes"));
    }

    let candidate = Path::new(path);

    if candidate.is_absolute() {
        return Err(invalid("absolute paths are not allowed"));
    }

    for component in candidate.components() {
        match component {
            Component::Normal(_) => {}
            _ => return Err(invalid("path traversal is not allowed")),
        }
    }

    Ok(())
}

/// Ed25519 trust anchor.
#[derive(Clone)]
pub struct TrustAnchor {
    key: [u8; 32],
    source: String,
}

impl TrustAnchor {
    pub fn embedded() -> Result<Self, ManifestError> {
        Self::from_base64(
            include_str!("../../../../deploy/release-public-key.txt").trim(),
            "built-in release key".into(),
        )
    }

    pub fn from_file(path: &Path) -> Result<Self, ManifestError> {
        let text = fs::read_to_string(path)?;

        Self::from_base64(text.trim(), format!("key file {}", path.display()))
    }

    pub fn from_base64(encoded: &str, source: String) -> Result<Self, ManifestError> {
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| ManifestError::Untrusted)?;

        let key: [u8; 32] = bytes.try_into().map_err(|_| ManifestError::Untrusted)?;

        if key == [0; 32] {
            return Err(ManifestError::Untrusted);
        }

        Ok(Self { key, source })
    }

    pub fn key_base64(&self) -> String {
        STANDARD.encode(self.key)
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn verify(&self, message: &[u8], signature: &[u8]) -> Result<(), ManifestError> {
        UnparsedPublicKey::new(&ED25519, &self.key)
            .verify(message, signature)
            .map_err(|_| ManifestError::Untrusted)
    }
}

/// A package whose signature, manifest and file hashes have been verified.
#[derive(Debug, Clone)]
pub struct VerifiedPackage {
    pub root: PathBuf,
    pub manifest: Manifest,
    pub manifest_bytes: Vec<u8>,
    pub manifest_sha256: String,
    pub source: String,
}

impl VerifiedPackage {
    pub fn file_path(&self, role: FileRole) -> Option<PathBuf> {
        self.manifest
            .file(role)
            .map(|file| self.root.join(&file.path))
    }

    pub fn component_bytes(&self, role: FileRole) -> Result<Vec<u8>, ManifestError> {
        let path = self
            .file_path(role)
            .ok_or_else(|| ManifestError::Invalid("missing component".into()))?;

        Ok(fs::read(path)?)
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);

    hex(digest.as_ref())
}

pub fn sha256_file(path: &Path) -> Result<String, ManifestError> {
    let mut file = File::open(path)?;
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = file.read(&mut buffer)?;

        if read == 0 {
            break;
        }

        context.update(&buffer[..read]);
    }

    Ok(hex(context.finish().as_ref()))
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);

    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }

    output
}

/// Verify a package directory.
pub fn verify_package(
    root: &Path,
    trust: &TrustAnchor,
    source: String,
) -> Result<VerifiedPackage, ManifestError> {
    let manifest_path = root.join(MANIFEST_FILE);
    let signature_path = root.join(SIGNATURE_FILE);

    let manifest_bytes = fs::read(&manifest_path)?;

    if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(ManifestError::Invalid("manifest is too large".into()));
    }

    let signature_text =
        fs::read_to_string(&signature_path).map_err(|_| ManifestError::Signature)?;

    let signature = STANDARD
        .decode(signature_text.trim())
        .map_err(|_| ManifestError::Signature)?;

    if signature.len() != 64 {
        return Err(ManifestError::Signature);
    }

    trust.verify(&manifest_bytes, &signature)?;

    let manifest = Manifest::parse(&manifest_bytes)?;

    for file in &manifest.files {
        let path = root.join(&file.path);

        let metadata = fs::symlink_metadata(&path).map_err(|error| ManifestError::File {
            path: file.path.clone(),
            reason: error.to_string(),
        })?;

        if !metadata.file_type().is_file() {
            return Err(ManifestError::File {
                path: file.path.clone(),
                reason: "not a regular file".into(),
            });
        }

        if metadata.len() != file.size {
            return Err(ManifestError::File {
                path: file.path.clone(),
                reason: format!(
                    "size mismatch: manifest {}, package {}",
                    file.size,
                    metadata.len()
                ),
            });
        }

        let actual = sha256_file(&path)?;

        if actual != file.sha256 {
            return Err(ManifestError::File {
                path: file.path.clone(),
                reason: "SHA-256 mismatch".into(),
            });
        }
    }

    Ok(VerifiedPackage {
        root: root.to_path_buf(),
        manifest_bytes: manifest_bytes.clone(),
        manifest_sha256: sha256_hex(&manifest_bytes),
        manifest,
        source,
    })
}

/// Extract a release zip into `target` and verify it.
///
/// Entry names are validated by the manifest rules before anything is written;
/// absolute paths, `..`, backslashes, links and oversized entries are rejected.
pub fn extract_zip(
    archive_path: &Path,
    target: &Path,
    trust: &TrustAnchor,
    source: String,
) -> Result<VerifiedPackage, ManifestError> {
    fs::create_dir_all(target)?;

    let file = File::open(archive_path)?;

    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| ManifestError::Unsupported(format!("not a zip archive: {error}")))?;

    if archive.len() > MAX_FILES + 8 {
        return Err(ManifestError::Invalid(
            "archive has too many entries".into(),
        ));
    }

    let mut entries = Vec::new();
    let mut total = 0u64;

    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| ManifestError::Unsupported(error.to_string()))?;

        let name = entry
            .name()
            .map_err(|error| ManifestError::Unsupported(error.to_string()))?
            .into_owned();

        if name.ends_with('/') {
            continue;
        }

        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(ManifestError::Invalid(
                "symbolic links are not allowed in a release package".into(),
            ));
        }

        validate_relative_path(&name)?;

        total = total.saturating_add(entry.size());

        if entry.size() > MAX_FILE_BYTES || total > MAX_TOTAL_BYTES {
            return Err(ManifestError::Invalid(
                "archive exceeds the size limits".into(),
            ));
        }

        entries.push((index, name));
    }

    // Accept both a flat archive and one wrapped in a single directory. The
    // decision uses every entry, so the result cannot depend on file order.
    let prefix = match entries.first().map(|(_, name)| name.split('/').next()) {
        Some(Some(candidate))
            if entries.iter().all(|(_, name)| {
                name.strip_prefix(candidate)
                    .and_then(|rest| rest.strip_prefix('/'))
                    .is_some_and(|rest| !rest.is_empty())
            }) =>
        {
            Some(candidate.to_owned())
        }
        _ => None,
    };

    for (index, name) in entries {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| ManifestError::Unsupported(error.to_string()))?;

        let relative = match prefix.as_deref() {
            Some(prefix) => name
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix('/'))
                .ok_or_else(|| ManifestError::Invalid("inconsistent archive prefix".into()))?,
            None => name.as_str(),
        };

        let destination = target.join(relative);

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut output = File::create(&destination)?;

        let copied = io::copy(&mut entry.by_ref().take(MAX_FILE_BYTES + 1), &mut output)?;

        if copied > MAX_FILE_BYTES {
            return Err(ManifestError::Invalid(
                "archive entry exceeds the size limit".into(),
            ));
        }
    }

    verify_package(target, trust, source)
}

/// Resolve the manifest + signature into the JSON used by the UI.
pub fn describe(package: &VerifiedPackage) -> serde_json::Value {
    serde_json::json!({
        "version": package.manifest.version,
        "git_sha": package.manifest.git_sha,
        "build_time_unix": package.manifest.build_time_unix,
        "channel": package.manifest.channel,
        "schema_version": package.manifest.schema_version,
        "compatibility": package.manifest.compatibility,
        "changelog": package.manifest.changelog,
        "manifest_sha256": package.manifest_sha256,
        "source": package.source,
    })
}

/// Parse `<scheme>://<host>:<port>` into a socket address for local health
/// checks. Only loopback servers are ever addressed.
#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::KeyPair;

    fn manifest_json(version: &str, files: &str) -> String {
        format!(
            r#"{{
              "format": 1,
              "product": "ALXPRGS Workspace",
              "version": "{version}",
              "git_sha": "660023c1685bc3234958c49c926dbf56fa972835",
              "build_time_unix": 1795650000,
              "channel": "stable",
              "schema_version": 11,
              "compatibility": {{
                "platforms": ["windows-x86_64"],
                "runtime": ["wsl2", "none"],
                "min_database_schema": 9,
                "max_database_schema": 11,
                "min_updater_format": 1
              }},
              "changelog": ["test"],
              "files": [{files}]
            }}"#
        )
    }

    fn file(name: &str, role: &str, sha: &str, size: u64) -> String {
        format!(r#"{{"path":"{name}","role":"{role}","size":{size},"sha256":"{sha}"}}"#)
    }

    #[test]
    fn parses_a_valid_manifest() {
        let sha = sha256_hex(b"x");
        let serde_json = manifest_json(
            "0.2.0",
            &[
                file("workspace.exe", "windows_binary", &sha, 1),
                file("linux/workspace-helper", "runtime_helper", &sha, 1),
                file("linux/workspace-egress", "runtime_egress", &sha, 1),
            ]
            .join(","),
        );

        let manifest = Manifest::parse(serde_json.as_bytes()).unwrap();

        assert_eq!(manifest.release_version().unwrap().to_string(), "0.2.0");
        assert_eq!(
            manifest.file(FileRole::RuntimeHelper).unwrap().path,
            "linux/workspace-helper"
        );
    }

    #[test]
    fn rejects_traversal_and_duplicate_roles() {
        let sha = sha256_hex(b"x");

        let traversal = manifest_json(
            "0.2.0",
            &[
                file("../workspace.exe", "windows_binary", &sha, 1),
                file("linux/workspace-helper", "runtime_helper", &sha, 1),
                file("linux/workspace-egress", "runtime_egress", &sha, 1),
            ]
            .join(","),
        );

        assert!(Manifest::parse(traversal.as_bytes()).is_err());

        let duplicate = manifest_json(
            "0.2.0",
            &[
                file("workspace.exe", "windows_binary", &sha, 1),
                file("other.exe", "windows_binary", &sha, 1),
                file("linux/workspace-helper", "runtime_helper", &sha, 1),
                file("linux/workspace-egress", "runtime_egress", &sha, 1),
            ]
            .join(","),
        );

        assert!(Manifest::parse(duplicate.as_bytes()).is_err());
    }

    #[test]
    fn rejects_incompatible_schema_range() {
        let sha = sha256_hex(b"x");
        let json = manifest_json(
            "0.2.0",
            &[
                file("workspace.exe", "windows_binary", &sha, 1),
                file("linux/workspace-helper", "runtime_helper", &sha, 1),
                file("linux/workspace-egress", "runtime_egress", &sha, 1),
            ]
            .join(","),
        )
        .replace("\"max_database_schema\": 11", "\"max_database_schema\": 12");

        assert!(Manifest::parse(json.as_bytes()).is_err());
    }

    #[test]
    fn versions_compare_numerically() {
        let older: ReleaseVersion = "0.2.9".parse().unwrap();
        let newer: ReleaseVersion = "0.10.0".parse().unwrap();

        assert!(newer > older);
        assert!("0.2".parse::<ReleaseVersion>().is_err());
        assert!("0.2.0-beta".parse::<ReleaseVersion>().is_err());
    }

    fn generated_key() -> (Vec<u8>, TrustAnchor) {
        let document =
            ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .unwrap();
        let key_pair = ring::signature::Ed25519KeyPair::from_pkcs8(document.as_ref()).unwrap();
        let public = STANDARD.encode(key_pair.public_key().as_ref());
        let trust = TrustAnchor::from_base64(&public, "test".into()).unwrap();

        (document.as_ref().to_vec(), trust)
    }

    fn sign_for_test(message: &[u8], document: &[u8]) -> Vec<u8> {
        let key_pair = ring::signature::Ed25519KeyPair::from_pkcs8(document).unwrap();

        key_pair.sign(message).as_ref().to_vec()
    }

    #[test]
    fn trust_anchor_rejects_a_different_key() {
        let (document, trust) = generated_key();
        let (_other_document, other) = generated_key();

        let message = b"manifest";
        let signature = sign_for_test(message, &document);

        trust.verify(message, &signature).unwrap();
        assert!(other.verify(message, &signature).is_err());
    }

    #[test]
    fn package_verification_detects_tampering() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();

        fs::write(root.join("workspace.exe"), b"binary").unwrap();
        fs::write(root.join("linux-workspace-helper"), b"helper").unwrap();
        fs::write(root.join("linux-workspace-egress"), b"egress").unwrap();

        let mut manifest = manifest_json(
            "0.2.0",
            &[
                file("workspace.exe", "windows_binary", &sha256_hex(b"binary"), 6),
                file(
                    "linux-workspace-helper",
                    "runtime_helper",
                    &sha256_hex(b"helper"),
                    6,
                ),
                file(
                    "linux-workspace-egress",
                    "runtime_egress",
                    &sha256_hex(b"egress"),
                    6,
                ),
            ]
            .join(","),
        );

        let (document, trust) = generated_key();
        let bytes = manifest.clone().into_bytes();
        let signature = sign_for_test(&bytes, &document);

        fs::write(root.join(MANIFEST_FILE), &bytes).unwrap();
        fs::write(root.join(SIGNATURE_FILE), STANDARD.encode(signature)).unwrap();

        assert!(verify_package(root, &trust, "test".into()).is_ok());

        // A modified file must fail even though the manifest is untouched.
        fs::write(root.join("workspace.exe"), b"evil!!").unwrap();
        assert!(verify_package(root, &trust, "test".into()).is_err());

        // Restoring the file but changing the manifest must fail the signature.
        fs::write(root.join("workspace.exe"), b"binary").unwrap();
        manifest = manifest.replace("\"version\": \"0.2.0\"", "\"version\": \"9.9.9\"");
        fs::write(root.join(MANIFEST_FILE), manifest).unwrap();
        assert!(verify_package(root, &trust, "test".into()).is_err());
    }
}
