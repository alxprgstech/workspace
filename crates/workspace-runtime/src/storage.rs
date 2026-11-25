//! Dedicated storage layout, filesystem facts and WSL configuration.
//!
//! Only server-generated identifiers are ever turned into paths, and every
//! identifier is parsed back as a canonical UUID before it is used.

use serde::Serialize;
use uuid::Uuid;

use crate::RuntimeError;
use crate::host::Host;

/// Directory layout under `runtime.data_root`.
#[derive(Debug, Clone)]
pub struct Layout {
    root: String,
}

impl Layout {
    pub fn new(data_root: &str) -> Self {
        Self {
            root: data_root.trim_end_matches('/').to_owned(),
        }
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    /// Parse and canonicalise a workspace identifier.
    pub fn workspace_id(id: &str) -> Result<Uuid, RuntimeError> {
        let parsed = Uuid::parse_str(id)
            .map_err(|_| RuntimeError::Storage("invalid workspace identifier".into()))?;

        if parsed.to_string() != id {
            return Err(RuntimeError::Storage(
                "workspace identifier is not canonical".into(),
            ));
        }

        Ok(parsed)
    }

    pub fn workspace_dir(&self, id: &str) -> Result<String, RuntimeError> {
        Self::workspace_id(id)?;

        Ok(format!("{}/workspaces/{id}", self.root))
    }

    pub fn files_dir(&self, id: &str) -> Result<String, RuntimeError> {
        Ok(format!("{}/files", self.workspace_dir(id)?))
    }

    pub fn cache_dir(&self, id: &str) -> Result<String, RuntimeError> {
        Ok(format!("{}/cache", self.workspace_dir(id)?))
    }

    pub fn workspaces_root(&self) -> String {
        format!("{}/workspaces", self.root)
    }

    pub fn artifacts_root(&self) -> String {
        format!("{}/artifacts", self.root)
    }

    pub fn staging_root(&self) -> String {
        format!("{}/staging", self.root)
    }

    pub fn logs_root(&self) -> String {
        format!("{}/logs", self.root)
    }
}

/// Mount facts for the dedicated data filesystem.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FilesystemStatus {
    pub mount_point: String,
    pub fs_type: String,
    pub options: Vec<String>,
    pub quota_enabled: bool,
    pub size_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
}

impl FilesystemStatus {
    /// Parse `findmnt -n -M <dir> -o FSTYPE,OPTIONS` output.
    pub fn parse_mount(mount_point: &str, raw: &str) -> Result<Self, RuntimeError> {
        let mut fields = raw.split_whitespace();

        let fs_type = fields
            .next()
            .ok_or_else(|| RuntimeError::Protocol("mount has no filesystem type".into()))?;

        let options: Vec<String> = fields
            .next()
            .unwrap_or_default()
            .split(',')
            .filter(|option| !option.is_empty())
            .map(str::to_owned)
            .collect();

        Ok(Self {
            mount_point: mount_point.to_owned(),
            fs_type: fs_type.to_owned(),
            quota_enabled: options.iter().any(|option| option == "prjquota"),
            options,
            size_bytes: 0,
            used_bytes: 0,
            available_bytes: 0,
        })
    }

    /// Fill in the `df -B1 --output=size,used,avail` numbers.
    pub fn with_usage(mut self, raw: &str) -> Result<Self, RuntimeError> {
        let line = raw
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .nth(1)
            .ok_or_else(|| RuntimeError::Protocol("df returned no usage line".into()))?;

        let fields: Vec<&str> = line.split_whitespace().collect();

        if fields.len() < 3 {
            return Err(RuntimeError::Protocol("unexpected df output".into()));
        }

        let parse = |value: &str| -> u64 { value.parse().unwrap_or(0) };

        self.size_bytes = parse(fields[0]);
        self.used_bytes = parse(fields[1]);
        self.available_bytes = parse(fields[2]);

        Ok(self)
    }
}

/// Read the dedicated filesystem facts.
pub async fn filesystem(host: &Host, data_root: &str) -> Result<FilesystemStatus, RuntimeError> {
    let mount = host
        .run(
            "/usr/bin/findmnt",
            &[
                "-n".to_owned(),
                "-M".to_owned(),
                data_root.to_owned(),
                "-o".to_owned(),
                "FSTYPE,OPTIONS".to_owned(),
            ],
        )
        .await?;

    let status = FilesystemStatus::parse_mount(data_root, &mount)?;

    let usage = host
        .run(
            "/usr/bin/df",
            &[
                "-B1".to_owned(),
                "--output=size,used,avail".to_owned(),
                data_root.to_owned(),
            ],
        )
        .await?;

    status.with_usage(&usage)
}

/// Relevant parts of `/etc/wsl.conf`.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct WslConfiguration {
    pub automount_enabled: Option<bool>,
    pub interop_enabled: Option<bool>,
    pub append_windows_path: Option<bool>,
}

impl WslConfiguration {
    /// Parse the INI subset WSL uses.
    pub fn parse(raw: &str) -> Self {
        let mut configuration = Self::default();
        let mut section = String::new();

        for line in raw.lines() {
            let line = line.trim();

            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }

            if line.starts_with('[') && line.ends_with(']') {
                section = line[1..line.len() - 1].trim().to_ascii_lowercase();
                continue;
            }

            let Some((key, value)) = line.split_once('=') else {
                continue;
            };

            let key = key.trim().to_ascii_lowercase();
            let value = value.trim().to_ascii_lowercase();
            let parsed = match value.as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };

            match (section.as_str(), key.as_str()) {
                ("automount", "enabled") => configuration.automount_enabled = parsed,
                ("interop", "enabled") => configuration.interop_enabled = parsed,
                ("interop", "appendwindowspath") => {
                    configuration.append_windows_path = parsed;
                }
                _ => {}
            }
        }

        configuration
    }
}

/// Kernel release of the dedicated environment.
pub async fn kernel_release(host: &Host) -> Result<String, RuntimeError> {
    host.run("/usr/bin/uname", &["-r".to_owned()]).await
}

/// Read `/etc/wsl.conf` from the dedicated distribution.
pub async fn wsl_configuration(host: &Host) -> Result<WslConfiguration, RuntimeError> {
    let raw = host
        .run("/usr/bin/cat", &["/etc/wsl.conf".to_owned()])
        .await?;

    Ok(WslConfiguration::parse(&raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_paths_require_canonical_uuid() {
        let layout = Layout::new("/srv/alxprgs-workspace/data/");

        assert_eq!(
            layout
                .workspace_dir("550e8400-e29b-41d4-a716-446655440000")
                .unwrap(),
            "/srv/alxprgs-workspace/data/workspaces/550e8400-e29b-41d4-a716-446655440000"
        );

        assert!(layout.workspace_dir("../../etc").is_err());
        assert!(
            layout
                .workspace_dir("550E8400-E29B-41D4-A716-446655440000")
                .is_err()
        );
    }

    #[test]
    fn parses_mount_and_usage() {
        let status = FilesystemStatus::parse_mount(
            "/srv/alxprgs-workspace/data",
            "ext4 rw,nosuid,nodev,relatime,prjquota",
        )
        .unwrap();

        assert_eq!(status.fs_type, "ext4");
        assert!(status.quota_enabled);

        let status = status
            .with_usage("Size Used Avail\n27917287424 2202009 25165824000\n")
            .unwrap();

        assert_eq!(status.size_bytes, 27_917_287_424);
        assert_eq!(status.used_bytes, 2_202_009);
    }

    #[test]
    fn missing_quota_option_is_detected() {
        let status = FilesystemStatus::parse_mount("/data", "ext4 rw,nosuid,nodev").unwrap();

        assert!(!status.quota_enabled);
    }

    #[test]
    fn parses_wsl_configuration() {
        let configuration = WslConfiguration::parse(
            "[automount]\nenabled = false\n[interop]\nenabled=false\nappendWindowsPath = false\n",
        );

        assert_eq!(configuration.automount_enabled, Some(false));
        assert_eq!(configuration.interop_enabled, Some(false));
        assert_eq!(configuration.append_windows_path, Some(false));
    }

    #[test]
    fn empty_wsl_configuration_is_unknown_not_disabled() {
        assert_eq!(WslConfiguration::parse(""), WslConfiguration::default());
    }
}
