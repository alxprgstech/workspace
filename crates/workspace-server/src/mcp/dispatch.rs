//! MCP tool dispatch.
//!
//! Every call is checked against the scope declared in the catalog and against
//! ownership of the named workspace. Arguments are parsed strictly: unknown or
//! mistyped fields are rejected instead of being ignored.

use serde_json::{Map, Value, json};

use crate::jobs::{ExecRequest, LogPage};
use crate::workspaces::{CreateRequest, UpdateRequest};

use super::GatewayServices;
use super::catalog;

/// A structured tool failure.
#[derive(Debug)]
pub struct ToolError {
    pub code: &'static str,
    pub message: String,
}

impl ToolError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_arguments", message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new("not_available", message)
    }

    /// JSON payload returned to the client.
    pub fn to_json(&self) -> Value {
        json!({ "error": self.code, "message": self.message })
    }
}

/// Caller context derived from the verified OAuth grant.
pub struct Caller<'a> {
    pub user_id: &'a str,
    pub scopes: &'a [String],
}

fn authenticated(caller: &Caller<'_>) -> Result<(), ToolError> {
    if caller.user_id.is_empty() {
        return Err(ToolError::new(
            "unauthenticated",
            "this tool requires a locally authorized user",
        ));
    }

    Ok(())
}

/// Dispatch one tool call.
pub async fn call(
    services: &GatewayServices,
    caller: &Caller<'_>,
    name: &str,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolError> {
    if !matches!(name, "server_info" | "capabilities_list") {
        authenticated(caller)?;
    }

    let definition = catalog::definitions()
        .into_iter()
        .find(|definition| definition.tool.name == name)
        .ok_or_else(|| ToolError::new("unknown_tool", format!("unknown tool: {name}")))?;

    if !caller.scopes.iter().any(|scope| scope == definition.scope) {
        return Err(ToolError::new(
            "insufficient_scope",
            format!(
                "{name} requires the {} scope; re-authorize the connection to grant it",
                definition.scope
            ),
        ));
    }

    match name {
        "server_info" => Ok(super::tools::server_information(
            services.schema_version,
            &services.runtime.status().await,
        )),
        "capabilities_list" => Ok(super::tools::capabilities_catalog(
            services.schema_version,
            &services.runtime.status().await,
        )),
        "workspace_create" => workspace_create(services, caller, arguments).await,
        "workspace_list" => workspace_list(services, caller),
        "workspace_info" => workspace_info(services, caller, arguments).await,
        "workspace_update" => workspace_update(services, caller, arguments).await,
        "workspace_stop" => workspace_stop(services, caller, arguments).await,
        "workspace_delete" => workspace_delete(services, caller, arguments).await,
        "exec" => exec(services, caller, arguments).await,
        "job_status" => job_status(services, caller, arguments),
        "job_logs" => job_logs(services, caller, arguments),
        "job_cancel" => job_cancel(services, caller, arguments).await,
        "files_list" => files_list(services, caller, arguments).await,
        "file_read" => file_read(services, caller, arguments).await,
        "file_write" => file_write(services, caller, arguments).await,
        "file_patch" => file_patch(services, caller, arguments).await,
        "artifact_publish" => artifact_publish(services, caller, arguments).await,
        "artifact_list" => artifact_list(services, caller),
        "artifact_download" => artifact_download(services, caller, arguments),
        other => Err(ToolError::new(
            "unknown_tool",
            format!("unknown tool: {other}"),
        )),
    }
}

fn files(services: &GatewayServices) -> Result<&crate::files::Files, ToolError> {
    services
        .files
        .as_deref()
        .ok_or_else(|| ToolError::unavailable("the file service is not available"))
}

fn artifacts(services: &GatewayServices) -> Result<&crate::artifacts::Artifacts, ToolError> {
    services
        .artifacts
        .as_deref()
        .ok_or_else(|| ToolError::unavailable("the artifact service is not available"))
}

fn files_error(error: crate::files::FilesError) -> ToolError {
    ToolError::new(error.code(), error.to_string())
}

fn artifact_error(error: crate::artifacts::ArtifactError) -> ToolError {
    ToolError::new(error.code(), error.to_string())
}

fn manager(services: &GatewayServices) -> Result<&crate::workspaces::Manager, ToolError> {
    services
        .workspaces
        .as_deref()
        .ok_or_else(|| ToolError::unavailable("workspace management is not available"))
}

fn executor(services: &GatewayServices) -> Result<&crate::jobs::Executor, ToolError> {
    services
        .jobs
        .as_deref()
        .ok_or_else(|| ToolError::unavailable("job execution is not available"))
}

fn string_argument<'a>(
    arguments: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a str, ToolError> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::invalid(format!("{name} must be a string")))
}

fn optional_string(
    arguments: &Map<String, Value>,
    name: &str,
) -> Result<Option<String>, ToolError> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ToolError::invalid(format!("{name} must be a string"))),
    }
}

fn optional_u64(arguments: &Map<String, Value>, name: &str) -> Result<Option<u64>, ToolError> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| ToolError::invalid(format!("{name} must be a non-negative integer"))),
    }
}

fn optional_bool(arguments: &Map<String, Value>, name: &str) -> Result<Option<bool>, ToolError> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| ToolError::invalid(format!("{name} must be a boolean"))),
    }
}

fn string_array(arguments: &Map<String, Value>, name: &str) -> Result<Vec<String>, ToolError> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(entries)) => entries
            .iter()
            .map(|entry| {
                entry
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| ToolError::invalid(format!("{name} must contain strings")))
            })
            .collect(),
        Some(_) => Err(ToolError::invalid(format!("{name} must be an array"))),
    }
}

fn environment(arguments: &Map<String, Value>) -> Result<Vec<(String, String)>, ToolError> {
    match arguments.get("env") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(entries)) => entries
            .iter()
            .map(|(key, value)| {
                value
                    .as_str()
                    .map(|value| (key.clone(), value.to_owned()))
                    .ok_or_else(|| ToolError::invalid("env values must be strings"))
            })
            .collect(),
        Some(_) => Err(ToolError::invalid("env must be an object")),
    }
}

fn workspace_error(error: crate::workspaces::WorkspaceError) -> ToolError {
    ToolError::new(error.code(), error.to_string())
}

fn job_error(error: crate::jobs::JobError) -> ToolError {
    ToolError::new(error.code(), error.to_string())
}

async fn workspace_create(
    services: &GatewayServices,
    caller: &Caller<'_>,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolError> {
    let name = string_argument(arguments, "name")?;

    let request = CreateRequest {
        name: name.to_owned(),
        execution_mode: optional_string(arguments, "execution_mode")?,
        capabilities: string_array(arguments, "capabilities")?,
        ttl_seconds: optional_u64(arguments, "ttl_seconds")?,
        cpu_millis: optional_u64(arguments, "cpu_millis")?.map(|value| value as u32),
        memory_mib: optional_u64(arguments, "memory_mib")?,
        persistent: optional_bool(arguments, "persistent")?,
    };

    let workspace = manager(services)?
        .create(caller.user_id, &request)
        .await
        .map_err(workspace_error)?;

    Ok(json!(workspace))
}

fn workspace_list(services: &GatewayServices, caller: &Caller<'_>) -> Result<Value, ToolError> {
    let workspaces = manager(services)?
        .list(caller.user_id)
        .map_err(workspace_error)?;

    Ok(json!({ "workspaces": workspaces }))
}

async fn workspace_info(
    services: &GatewayServices,
    caller: &Caller<'_>,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolError> {
    let id = string_argument(arguments, "workspace_id")?;
    let manager = manager(services)?;
    let workspace = manager
        .get_owned(caller.user_id, id)
        .await
        .map_err(workspace_error)?;

    let usage = manager.usage(&workspace).await.ok();

    Ok(json!({
        "workspace": workspace,
        "usage": usage,
        "ttl_remaining_seconds": workspace.ttl_remaining_seconds(now()),
    }))
}

async fn workspace_update(
    services: &GatewayServices,
    caller: &Caller<'_>,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolError> {
    let id = string_argument(arguments, "workspace_id")?;

    let request = UpdateRequest {
        name: optional_string(arguments, "name")?,
        ttl_seconds: optional_u64(arguments, "ttl_seconds")?,
        pinned: optional_bool(arguments, "pinned")?,
        execution_mode: optional_string(arguments, "execution_mode")?,
    };

    let workspace = manager(services)?
        .update(caller.user_id, id, &request)
        .await
        .map_err(workspace_error)?;

    Ok(json!(workspace))
}

async fn workspace_stop(
    services: &GatewayServices,
    caller: &Caller<'_>,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolError> {
    let id = string_argument(arguments, "workspace_id")?;

    let workspace = manager(services)?
        .stop(caller.user_id, id)
        .await
        .map_err(workspace_error)?;

    Ok(json!(workspace))
}

async fn workspace_delete(
    services: &GatewayServices,
    caller: &Caller<'_>,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolError> {
    let id = string_argument(arguments, "workspace_id")?;

    manager(services)?
        .delete(caller.user_id, id)
        .await
        .map_err(workspace_error)?;

    Ok(json!({ "deleted": true, "workspace_id": id }))
}

async fn exec(
    services: &GatewayServices,
    caller: &Caller<'_>,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolError> {
    let workspace_id = string_argument(arguments, "workspace_id")?;
    let argv = string_array(arguments, "argv")?;

    if argv.is_empty() {
        return Err(ToolError::invalid("argv must contain the program to run"));
    }

    let request = ExecRequest {
        argv,
        cwd: optional_string(arguments, "cwd")?.unwrap_or_default(),
        env: environment(arguments)?,
        timeout_seconds: optional_u64(arguments, "timeout_seconds")?,
        idempotency_key: optional_string(arguments, "idempotency_key")?,
    };

    let job = executor(services)?
        .submit(caller.user_id, workspace_id, request)
        .await
        .map_err(job_error)?;

    Ok(json!(job))
}

fn job_status(
    services: &GatewayServices,
    caller: &Caller<'_>,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolError> {
    let id = string_argument(arguments, "job_id")?;

    let job = executor(services)?
        .owned_job(caller.user_id, id)
        .map_err(job_error)?;

    Ok(json!({
        "job": job,
        "duration_ms": job.duration_ms(),
    }))
}

fn job_logs(
    services: &GatewayServices,
    caller: &Caller<'_>,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolError> {
    let id = string_argument(arguments, "job_id")?;
    let stream = optional_string(arguments, "stream")?.unwrap_or_else(|| "stdout".to_owned());
    let offset = optional_u64(arguments, "offset")?.unwrap_or(0);
    let limit = optional_u64(arguments, "limit")?.unwrap_or(64 * 1024);

    let page: LogPage = executor(services)?
        .logs(caller.user_id, id, &stream, offset, limit)
        .map_err(job_error)?;

    Ok(json!(page))
}

async fn job_cancel(
    services: &GatewayServices,
    caller: &Caller<'_>,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolError> {
    let id = string_argument(arguments, "job_id")?;

    let job = executor(services)?
        .cancel(caller.user_id, id)
        .await
        .map_err(job_error)?;

    Ok(json!(job))
}

async fn files_list(
    services: &GatewayServices,
    caller: &Caller<'_>,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolError> {
    let workspace_id = string_argument(arguments, "workspace_id")?;

    let listing = files(services)?
        .list(
            caller.user_id,
            workspace_id,
            &optional_string(arguments, "path")?.unwrap_or_default(),
            optional_u64(arguments, "depth")?.unwrap_or(2) as u32,
            optional_u64(arguments, "limit")?.unwrap_or(500) as u32,
        )
        .await
        .map_err(files_error)?;

    Ok(json!(listing))
}

async fn file_read(
    services: &GatewayServices,
    caller: &Caller<'_>,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolError> {
    let workspace_id = string_argument(arguments, "workspace_id")?;
    let path = string_argument(arguments, "path")?;

    let chunk = files(services)?
        .read(
            caller.user_id,
            workspace_id,
            path,
            optional_u64(arguments, "offset")?.unwrap_or(0),
            optional_u64(arguments, "length")?.unwrap_or(64 * 1024),
        )
        .await
        .map_err(files_error)?;

    Ok(json!(chunk))
}

async fn file_write(
    services: &GatewayServices,
    caller: &Caller<'_>,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolError> {
    let workspace_id = string_argument(arguments, "workspace_id")?;
    let path = string_argument(arguments, "path")?;
    let content = string_argument(arguments, "content")?;

    let create = optional_bool(arguments, "create")?.unwrap_or(false);

    let outcome = files(services)?
        .write(
            caller.user_id,
            workspace_id,
            path,
            content.as_bytes(),
            optional_string(arguments, "expected_sha256")?.as_deref(),
            create,
            !create,
        )
        .await
        .map_err(files_error)?;

    Ok(json!(outcome))
}

async fn file_patch(
    services: &GatewayServices,
    caller: &Caller<'_>,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolError> {
    let workspace_id = string_argument(arguments, "workspace_id")?;
    let path = string_argument(arguments, "path")?;
    let patch = string_argument(arguments, "patch")?;

    let outcome = files(services)?
        .patch(
            caller.user_id,
            workspace_id,
            path,
            patch,
            optional_string(arguments, "expected_sha256")?.as_deref(),
        )
        .await
        .map_err(files_error)?;

    Ok(json!(outcome))
}

async fn artifact_publish(
    services: &GatewayServices,
    caller: &Caller<'_>,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolError> {
    let workspace_id = string_argument(arguments, "workspace_id")?;
    let path = string_argument(arguments, "path")?;

    let artifact = artifacts(services)?
        .publish(
            caller.user_id,
            workspace_id,
            path,
            optional_string(arguments, "name")?.as_deref(),
        )
        .await
        .map_err(artifact_error)?;

    Ok(json!(artifact))
}

fn artifact_list(services: &GatewayServices, caller: &Caller<'_>) -> Result<Value, ToolError> {
    let artifacts = artifacts(services)?
        .list(caller.user_id)
        .map_err(artifact_error)?;

    Ok(json!({ "artifacts": artifacts }))
}

fn artifact_download(
    services: &GatewayServices,
    caller: &Caller<'_>,
    arguments: &Map<String, Value>,
) -> Result<Value, ToolError> {
    let artifact_id = string_argument(arguments, "artifact_id")?;

    let link = artifacts(services)?
        .download_link(
            caller.user_id,
            artifact_id,
            optional_u64(arguments, "ttl_seconds")?,
        )
        .map_err(artifact_error)?;

    Ok(json!(link))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_arguments_are_rejected() {
        let arguments = Map::new();

        assert_eq!(
            string_argument(&arguments, "workspace_id")
                .unwrap_err()
                .code,
            "invalid_arguments"
        );
    }

    #[test]
    fn wrong_types_are_rejected() {
        let mut arguments = Map::new();
        arguments.insert("ttl_seconds".into(), json!("long"));

        assert!(optional_u64(&arguments, "ttl_seconds").is_err());
    }

    #[test]
    fn environment_objects_become_pairs() {
        let mut arguments = Map::new();
        arguments.insert("env".into(), json!({"A": "1", "B": "2"}));

        let mut pairs = environment(&arguments).unwrap();
        pairs.sort();

        assert_eq!(
            pairs,
            vec![
                ("A".to_owned(), "1".to_owned()),
                ("B".to_owned(), "2".to_owned())
            ]
        );
    }

    #[test]
    fn scope_check_is_central() {
        // The catalog is the only place that maps a tool to a scope.
        assert!(catalog::definitions().iter().any(|definition| {
            definition.tool.name == "workspace_delete" && definition.scope == "workspace:delete"
        }));
    }
}
