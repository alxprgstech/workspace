//! Hard project quotas.
//!
//! The filesystem enforces the limits; this module only asks a strictly
//! limited, administrator-installed helper to apply them. The helper accepts
//! exactly four operations with validated numeric/identifier arguments, so the
//! server never gains a general root shell.

use serde::Serialize;
use uuid::Uuid;

use crate::RuntimeError;
use crate::host::Host;

/// Trusted quota helper installed by the administrator.
pub const QUOTA_HELPER: &str = "/usr/sbin/alxprgs-quota";

/// Lowest project identifier reserved for Workspace data.
pub const FIRST_PROJECT_ID: u32 = 1010;

/// First project identifier handed out to a workspace.
pub const FIRST_WORKSPACE_PROJECT_ID: u32 = 1020;

/// Highest project identifier the helper accepts.
pub const MAX_PROJECT_ID: u32 = 999_999;

/// Upper bound for a single quota, matching the largest supported filesystem.
pub const MAX_BYTES: u64 = 1 << 40;

/// Upper bound for the inode quota.
pub const MAX_FILES: u64 = 10_000_000;

/// Quota as reported by the filesystem.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct QuotaUsage {
    pub project_id: u32,
    pub used_bytes: u64,
    pub used_files: u64,
    pub soft_bytes: u64,
    pub hard_bytes: u64,
    pub soft_files: u64,
    pub hard_files: u64,
}

impl QuotaUsage {
    /// Whether the workspace is at or beyond its hard limits.
    pub fn exceeded(&self) -> bool {
        (self.hard_bytes > 0 && self.used_bytes >= self.hard_bytes)
            || (self.hard_files > 0 && self.used_files >= self.hard_files)
    }
}

fn validate_project_id(project_id: u32) -> Result<(), RuntimeError> {
    if !(FIRST_PROJECT_ID..=MAX_PROJECT_ID).contains(&project_id) {
        return Err(RuntimeError::Quota(format!(
            "project id {project_id} is outside the reserved range"
        )));
    }

    Ok(())
}

fn validate_workspace_id(workspace_id: &str) -> Result<(), RuntimeError> {
    let parsed = Uuid::parse_str(workspace_id)
        .map_err(|_| RuntimeError::Quota("invalid workspace identifier".into()))?;

    if parsed.to_string() != workspace_id {
        return Err(RuntimeError::Quota(
            "workspace identifier is not canonical".into(),
        ));
    }

    Ok(())
}

async fn run_helper(
    host: &Host,
    operation: &str,
    arguments: &[String],
) -> Result<String, RuntimeError> {
    let mut all = vec![
        "-n".to_owned(),
        QUOTA_HELPER.to_owned(),
        operation.to_owned(),
    ];

    all.extend(arguments.iter().cloned());

    host.run("/usr/bin/sudo", &all).await
}

/// Assign a project id to a workspace directory and apply its hard limits.
pub async fn assign(host: &Host, project_id: u32, workspace_id: &str) -> Result<(), RuntimeError> {
    validate_project_id(project_id)?;
    validate_workspace_id(workspace_id)?;

    run_helper(
        host,
        "assign",
        &[project_id.to_string(), workspace_id.to_owned()],
    )
    .await
    .map(|_| ())
}

/// Apply block and inode limits to a project id.
pub async fn set_limits(
    host: &Host,
    project_id: u32,
    bytes: u64,
    files: u64,
) -> Result<(), RuntimeError> {
    validate_project_id(project_id)?;

    if bytes == 0 || bytes > MAX_BYTES {
        return Err(RuntimeError::Quota("block quota is out of range".into()));
    }

    if files == 0 || files > MAX_FILES {
        return Err(RuntimeError::Quota("inode quota is out of range".into()));
    }

    run_helper(
        host,
        "set",
        &[project_id.to_string(), bytes.to_string(), files.to_string()],
    )
    .await
    .map(|_| ())
}

/// Read usage for a project id.
pub async fn show(host: &Host, project_id: u32) -> Result<Option<QuotaUsage>, RuntimeError> {
    validate_project_id(project_id)?;

    let output = run_helper(host, "show", &[project_id.to_string()]).await?;

    if output.trim() == "none" {
        return Ok(None);
    }

    let fields: Vec<&str> = output.split_whitespace().collect();

    if fields.len() != 7 {
        return Err(RuntimeError::Protocol(
            "unexpected quota report format".into(),
        ));
    }

    let value = |index: usize| -> Result<u64, RuntimeError> {
        fields[index]
            .parse()
            .map_err(|_| RuntimeError::Protocol("unexpected quota report value".into()))
    };

    Ok(Some(QuotaUsage {
        project_id,
        used_bytes: value(1)?,
        used_files: value(2)?,
        soft_bytes: value(3)?,
        hard_bytes: value(4)?,
        soft_files: value(5)?,
        hard_files: value(6)?,
    }))
}

/// Release a project id after the workspace data is gone.
pub async fn clear(host: &Host, project_id: u32) -> Result<(), RuntimeError> {
    validate_project_id(project_id)?;

    run_helper(host, "clear", &[project_id.to_string()])
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_ids_outside_the_reserved_range_are_refused() {
        assert!(validate_project_id(999).is_err());
        assert!(validate_project_id(1_000_000).is_err());
        assert!(validate_project_id(1010).is_ok());
        assert!(validate_project_id(1019).is_ok());
        assert!(validate_project_id(1020).is_ok());
    }

    #[test]
    fn workspace_ids_must_be_canonical_uuids() {
        assert!(validate_workspace_id("550e8400-e29b-41d4-a716-446655440000").is_ok());
        assert!(validate_workspace_id("550E8400-E29B-41D4-A716-446655440000").is_err());
        assert!(validate_workspace_id("../../etc").is_err());
        assert!(validate_workspace_id("550e8400").is_err());
    }

    #[test]
    fn exceeded_reports_both_limits() {
        let mut usage = QuotaUsage {
            project_id: 1000,
            used_bytes: 10,
            used_files: 10,
            soft_bytes: 100,
            hard_bytes: 100,
            soft_files: 100,
            hard_files: 100,
        };

        assert!(!usage.exceeded());

        usage.used_bytes = 100;
        assert!(usage.exceeded());

        usage.used_bytes = 0;
        usage.used_files = 100;
        assert!(usage.exceeded());
    }
}
