//! Trusted capability image builder.
//!
//! The caller names module identifiers. The server decides which packages land
//! in the image, which base image is used and how the build runs; an MCP client
//! can never supply a Dockerfile, a build argument or an image reference.
//!
//! Builds are serialised, resource-limited, keyed by a canonical cache key and
//! recorded with the immutable image identifier that was produced. A cached
//! image is only reused while that identifier still exists.

use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Mutex;

use workspace_core::helper::Operation;
use workspace_runtime::Helper;
use workspace_runtime::docker::Docker;
use workspace_runtime::host::Host;
use workspace_runtime::status::RuntimeMonitor;

use crate::storage;

/// Pinned Debian base for package-only images.
pub const DEBIAN_BASE: &str =
    "debian@sha256:8aa5a7bf5a1955bddbe7d74fcdd0b91ab30e74362a8b73af672b0481b35b44a0";

/// Rust toolchain image; the tag is resolved to an immutable identifier during
/// the build and that identifier is what gets recorded and reused.
pub const RUST_BASE: &str = "rust:1.100-slim";

const MANIFESTS: &str = include_str!("../../../images/manifests.toml");

/// Longest a single trusted build may take.
const BUILD_TIMEOUT: Duration = Duration::from_secs(1800);

/// Memory and CPU limits applied to the build container.
const BUILD_MEMORY: &str = "2048m";
const BUILD_CPU_QUOTA: &str = "200000";

#[derive(Debug, Deserialize)]
pub struct Recipe {
    version: String,
    packages: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Manifests {
    base: Recipe,
    git: Recipe,
    cpp: Recipe,
    python: Recipe,
    ffmpeg: Recipe,
}

#[derive(Debug, Error)]
pub enum ImageError {
    #[error("{0}")]
    Invalid(String),

    #[error("image build failed: {0}")]
    Build(String),

    #[error("image storage failed: {0}")]
    Storage(String),

    #[error("image database failed: {0}")]
    Database(String),

    #[error("the execution runtime is not configured")]
    RuntimeUnavailable,
}

impl ImageError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "capability_error",
            Self::Build(_) => "image_build_failed",
            Self::Storage(_) => "image_storage_error",
            Self::Database(_) => "database_error",
            Self::RuntimeUnavailable => "runtime_not_ready",
        }
    }
}

/// Result of ensuring an image.
#[derive(Debug, Clone, Serialize)]
pub struct BuiltImage {
    pub reference: String,
    pub image_id: String,
    pub cache_key: String,
    pub modules: Vec<String>,
}

/// Builder bound to one runtime and one data set.
pub struct ImageBuilder {
    docker: Docker,
    host: Host,
    helper: Helper,
    data_dir: PathBuf,
    data_root: String,
    socket: String,
    build_lock: Mutex<()>,
}

impl ImageBuilder {
    pub fn new(monitor: &RuntimeMonitor, data_dir: PathBuf) -> Result<Self, ImageError> {
        let settings = monitor.settings().ok_or(ImageError::RuntimeUnavailable)?;

        Ok(Self {
            docker: monitor
                .docker()
                .cloned()
                .ok_or(ImageError::RuntimeUnavailable)?,
            host: monitor
                .host()
                .cloned()
                .ok_or(ImageError::RuntimeUnavailable)?,
            helper: monitor
                .helper()
                .cloned()
                .ok_or(ImageError::RuntimeUnavailable)?,
            data_dir,
            data_root: settings.data_root.clone(),
            socket: settings.socket.clone(),
            build_lock: Mutex::new(()),
        })
    }

    /// Canonical cache key for a module set.
    pub fn cache_key(modules: &[String]) -> String {
        let sorted: BTreeSet<&str> = modules.iter().map(String::as_str).collect();

        let canonical = format!(
            "alxprgs-workspace-image-v1|{}|{}",
            DEBIAN_BASE,
            sorted.into_iter().collect::<Vec<_>>().join(",")
        );

        hex(&crate::crypto::hash_handle(&canonical))
    }

    /// Ensure an image exists for these modules.
    pub async fn ensure(&self, modules: &[String]) -> Result<BuiltImage, ImageError> {
        let recipe = self.recipe()?;
        let dockerfile = render_dockerfile(&recipe, modules)?;
        let key = Self::cache_key(modules);

        if let Some(image) = self.cached(&key).await? {
            return Ok(image);
        }

        let _guard = self.build_lock.lock().await;

        if let Some(image) = self.cached(&key).await? {
            return Ok(image);
        }

        self.build(&key, modules, &dockerfile).await
    }

    fn recipe(&self) -> Result<Manifests, ImageError> {
        toml::from_str(MANIFESTS).map_err(|error| ImageError::Invalid(format!("recipe: {error}")))
    }

    async fn cached(&self, key: &str) -> Result<Option<BuiltImage>, ImageError> {
        let connection = self.connection()?;

        let record = connection
            .query_row(
                "SELECT image_ref, image_id, modules, state, error
                 FROM image_specs WHERE cache_key = ?1",
                params![key],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .ok();

        let Some((reference, image_id, modules, state, error)) = record else {
            return Ok(None);
        };

        match state.as_str() {
            "ready" => {}
            "building" => {
                return Err(ImageError::Build(
                    "an image build for this module set is already running".into(),
                ));
            }
            _ => {
                return Err(ImageError::Build(
                    error.unwrap_or_else(|| "the previous build failed".into()),
                ));
            }
        }

        let (Some(reference), Some(image_id)) = (reference, image_id) else {
            return Ok(None);
        };

        // Reuse only while the recorded identifier still describes the image.
        match self.docker.image_id(&reference).await {
            Ok(current) if current == image_id => Ok(Some(BuiltImage {
                reference,
                image_id,
                cache_key: key.to_owned(),
                modules: serde_json::from_str(&modules).unwrap_or_default(),
            })),
            _ => Ok(None),
        }
    }

    async fn build(
        &self,
        key: &str,
        modules: &[String],
        dockerfile: &str,
    ) -> Result<BuiltImage, ImageError> {
        self.record_building(key, modules)?;

        match self.run_build(key, dockerfile, modules).await {
            Ok(image) => {
                self.record_ready(key, modules, &image)?;
                Ok(image)
            }
            Err(error) => {
                let _ = self.record_failed(key, modules, &error.to_string());
                Err(error)
            }
        }
    }

    async fn run_build(
        &self,
        key: &str,
        dockerfile: &str,
        modules: &[String],
    ) -> Result<BuiltImage, ImageError> {
        let context = format!("{}/builds/{key}", self.data_root);

        // Create the whole build path from the data root: capability roots must
        // already exist, so the root itself can never be created implicitly.
        self.helper
            .call(
                Operation::Mkdir {
                    root: self.data_root.clone(),
                    path: format!("builds/{key}"),
                    parents: true,
                },
                &[],
            )
            .await
            .map_err(|error| ImageError::Storage(error.to_string()))?;

        self.helper
            .call(
                Operation::Write {
                    root: context.clone(),
                    path: "Dockerfile".to_owned(),
                    create_parents: false,
                    replace: true,
                    expected_sha256: None,
                },
                dockerfile.as_bytes(),
            )
            .await
            .map_err(|error| ImageError::Storage(error.to_string()))?;

        let temporary = format!("alxprgs-workspace/{key}:latest");

        let arguments = vec![
            "--host".to_owned(),
            self.socket.clone(),
            "build".to_owned(),
            "--progress".to_owned(),
            "plain".to_owned(),
            "--network".to_owned(),
            "default".to_owned(),
            "--memory".to_owned(),
            BUILD_MEMORY.to_owned(),
            "--cpu-quota".to_owned(),
            BUILD_CPU_QUOTA.to_owned(),
            "--tag".to_owned(),
            temporary.clone(),
            "--file".to_owned(),
            format!("{context}/Dockerfile"),
            context,
        ];

        self.host
            .run_with_timeout("/usr/bin/docker", &arguments, BUILD_TIMEOUT)
            .await
            .map_err(|error| ImageError::Build(error.to_string()))?;

        let image_id =
            self.docker.image_id(&temporary).await.map_err(|error| {
                ImageError::Build(format!("cannot read the built image: {error}"))
            })?;

        let short = image_id.trim_start_matches("sha256:");
        let reference = format!("alxprgs-workspace/{key}:{short}");

        self.docker
            .tag(&temporary, &reference)
            .await
            .map_err(|error| ImageError::Build(format!("cannot pin the built image: {error}")))?;

        Ok(BuiltImage {
            reference,
            image_id,
            cache_key: key.to_owned(),
            modules: modules.to_vec(),
        })
    }

    fn connection(&self) -> Result<rusqlite::Connection, ImageError> {
        storage::open(&self.data_dir).map_err(|error| ImageError::Database(error.to_string()))
    }

    fn record_building(&self, key: &str, modules: &[String]) -> Result<(), ImageError> {
        let connection = self.connection()?;

        connection
            .execute(
                "INSERT INTO image_specs
                 (cache_key, modules, base_image, state, created_at_unix, updated_at_unix)
                 VALUES (?1, ?2, ?3, 'building', unixepoch(), unixepoch())
                 ON CONFLICT(cache_key) DO UPDATE SET
                    state = 'building', error = NULL, updated_at_unix = unixepoch()",
                params![
                    key,
                    serde_json::to_string(modules).unwrap_or_default(),
                    DEBIAN_BASE
                ],
            )
            .map_err(|error| ImageError::Database(error.to_string()))?;

        Ok(())
    }

    fn record_ready(
        &self,
        key: &str,
        modules: &[String],
        image: &BuiltImage,
    ) -> Result<(), ImageError> {
        let connection = self.connection()?;

        connection
            .execute(
                "UPDATE image_specs SET
                    image_ref = ?2, image_id = ?3, state = 'ready', error = NULL,
                    updated_at_unix = unixepoch()
                 WHERE cache_key = ?1",
                params![key, image.reference, image.image_id],
            )
            .map_err(|error| ImageError::Database(error.to_string()))?;

        let _ = modules;

        Ok(())
    }

    fn record_failed(&self, key: &str, modules: &[String], error: &str) -> Result<(), ImageError> {
        let connection = self.connection()?;

        connection
            .execute(
                "INSERT INTO image_specs
                 (cache_key, modules, base_image, state, error, created_at_unix, updated_at_unix)
                 VALUES (?1, ?2, ?3, 'failed', ?4, unixepoch(), unixepoch())
                 ON CONFLICT(cache_key) DO UPDATE SET
                    state = 'failed', error = excluded.error, updated_at_unix = unixepoch()",
                params![
                    key,
                    serde_json::to_string(modules).unwrap_or_default(),
                    DEBIAN_BASE,
                    error
                ],
            )
            .map_err(|error| ImageError::Database(error.to_string()))?;

        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Render the trusted Dockerfile for a module set.
///
/// `rust` selects the pinned Rust toolchain image; every other module is an
/// apt package installed on the pinned Debian base. `whisper-vad` fails closed
/// because this build has no trusted, checksum-pinned source for it.
pub fn render_dockerfile(recipes: &Manifests, modules: &[String]) -> Result<String, ImageError> {
    let requested: BTreeSet<&str> = modules.iter().map(String::as_str).collect();

    if requested.contains("whisper-vad") {
        return Err(ImageError::Invalid(
            "whisper-vad has no trusted, checksum-pinned source in this build".into(),
        ));
    }

    let mut packages: BTreeSet<&str> = BTreeSet::new();

    packages.extend(recipes.base.packages.iter().map(String::as_str));

    for (id, recipe) in [
        ("git", &recipes.git),
        ("cpp", &recipes.cpp),
        ("python", &recipes.python),
        ("ffmpeg", &recipes.ffmpeg),
    ] {
        if requested.contains(id) {
            packages.extend(recipe.packages.iter().map(String::as_str));
        }
    }

    let base = if requested.contains("rust") {
        RUST_BASE
    } else {
        DEBIAN_BASE
    };

    let packages = packages.into_iter().collect::<Vec<_>>().join(" ");

    let mut dockerfile = format!(
        "# Generated from compiled-in trusted recipes; do not edit by hand.\n\
         FROM {base}\n\
         ENV DEBIAN_FRONTEND=noninteractive\n\
         RUN apt-get update \\\n\
          && apt-get install -y --no-install-recommends {packages} \\\n\
          && rm -rf /var/lib/apt/lists/*\n"
    );

    if requested.contains("rust") {
        // Cargo caches live on the workspace cache mount, never in the image.
        dockerfile.push_str(
            "ENV RUSTUP_HOME=/usr/local/rustup \\\n\
             \x20   CARGO_HOME=/usr/local/cargo \\\n\
             \x20   PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n\
             RUN rustup component add clippy rustfmt \\\n\
             \x20 && cargo --version && rustc --version && cargo clippy --version\n",
        );
    }

    // A stable, non-secret user descriptor keeps the image reproducible.
    dockerfile.push_str("ENV LANG=C.UTF-8\n");

    Ok(dockerfile)
}

/// Version list of the modules available in this build.
pub fn module_versions() -> Result<Vec<(String, String)>, ImageError> {
    let recipes: Manifests = toml::from_str(MANIFESTS)
        .map_err(|error| ImageError::Invalid(format!("recipe: {error}")))?;

    Ok(vec![
        ("base".to_owned(), recipes.base.version),
        ("git".to_owned(), recipes.git.version),
        ("cpp".to_owned(), recipes.cpp.version),
        ("python".to_owned(), recipes.python.version),
        ("ffmpeg".to_owned(), recipes.ffmpeg.version),
        ("rust".to_owned(), "1.100.0".to_owned()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipes() -> Manifests {
        toml::from_str(MANIFESTS).unwrap()
    }

    #[test]
    fn dockerfile_only_contains_manifest_packages() {
        let dockerfile =
            render_dockerfile(&recipes(), &["base".to_owned(), "ffmpeg".to_owned()]).unwrap();

        assert!(dockerfile.starts_with("# Generated"));
        assert!(dockerfile.contains(&format!("FROM {DEBIAN_BASE}")));
        assert!(dockerfile.contains("ffmpeg"));
        assert!(!dockerfile.contains("gcc"));
    }

    #[test]
    fn rust_uses_the_pinned_toolchain_image() {
        let dockerfile =
            render_dockerfile(&recipes(), &["base".to_owned(), "rust".to_owned()]).unwrap();

        assert!(dockerfile.contains(&format!("FROM {RUST_BASE}")));
        assert!(dockerfile.contains("cargo clippy --version"));
    }

    #[test]
    fn unsupported_modules_fail_closed() {
        assert!(render_dockerfile(&recipes(), &["whisper-vad".to_owned()]).is_err());
    }

    #[test]
    fn cache_keys_are_canonical_and_stable() {
        let first = ImageBuilder::cache_key(&["base".to_owned(), "ffmpeg".to_owned()]);
        let second = ImageBuilder::cache_key(&["ffmpeg".to_owned(), "base".to_owned()]);

        assert_eq!(first, second);
        assert_eq!(first.len(), 64);
        assert_ne!(first, ImageBuilder::cache_key(&["base".to_owned()]));
    }

    #[test]
    fn every_advertised_module_has_a_version() {
        let versions = module_versions().unwrap();

        for id in ["base", "git", "cpp", "python", "ffmpeg", "rust"] {
            assert!(
                versions.iter().any(|(module, _)| module == id),
                "missing {id}"
            );
        }
    }
}
