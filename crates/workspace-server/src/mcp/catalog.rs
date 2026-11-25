//! MCP tool catalog: names, JSON Schemas, annotations and required scopes.
//!
//! The catalog is the single source of truth for `tools/list`. A tool is only
//! advertised when its implementation exists, and a state-changing tool is
//! never annotated as read-only.

use rmcp::model::{Tool, ToolAnnotations};
use serde_json::{Value, json};
use std::sync::Arc;

/// One advertised tool.
pub struct Definition {
    pub tool: Tool,
    pub scope: &'static str,
}

fn schema(value: Value) -> Arc<serde_json::Map<String, Value>> {
    match value {
        Value::Object(map) => Arc::new(map),
        _ => panic!("tool schema must be an object"),
    }
}

fn object(properties: Value, required: &[&str]) -> Arc<serde_json::Map<String, Value>> {
    schema(json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    }))
}

fn no_arguments() -> Arc<serde_json::Map<String, Value>> {
    object(json!({}), &[])
}

fn tool(
    name: &'static str,
    description: &'static str,
    schema: Arc<serde_json::Map<String, Value>>,
    scope: &'static str,
    read_only: bool,
) -> Definition {
    let mut tool = Tool::new(name, description, schema);

    tool.annotations = Some(
        ToolAnnotations::new()
            .read_only(read_only)
            .destructive(!read_only && name.contains("delete"))
            .idempotent(read_only)
            .open_world(false),
    );

    Definition { tool, scope }
}

/// All tools this build implements.
pub fn definitions() -> Vec<Definition> {
    let workspace_id = json!({
        "type": "string",
        "description": "Workspace identifier returned by workspace_create.",
        "pattern": "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
    });

    let job_id = json!({
        "type": "string",
        "description": "Job identifier returned by exec.",
        "pattern": "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
    });

    vec![
        tool(
            "server_info",
            "Read-only build and runtime information for this Workspace instance.",
            no_arguments(),
            "workspace:read",
            true,
        ),
        tool(
            "capabilities_list",
            "Read-only catalog of workspace capabilities and service modules with their real state.",
            no_arguments(),
            "workspace:read",
            true,
        ),
        tool(
            "workspace_create",
            "Create an isolated workspace with the requested capability modules.",
            object(
                json!({
                    "name": {"type": "string", "description": "Human-readable workspace name."},
                    "execution_mode": {
                        "type": "string",
                        "enum": ["ephemeral", "persistent"],
                        "description": "ephemeral (default) creates a container per job; persistent keeps one container until the idle timeout."
                    },
                    "capabilities": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Capability identifiers from capabilities_list. base is always included."
                    },
                    "ttl_seconds": {"type": "integer", "minimum": 60},
                    "cpu_millis": {"type": "integer", "minimum": 100},
                    "memory_mib": {"type": "integer", "minimum": 64}
                }),
                &["name"],
            ),
            "workspace:write",
            false,
        ),
        tool(
            "workspace_list",
            "List the workspaces owned by the calling connection.",
            no_arguments(),
            "workspace:read",
            true,
        ),
        tool(
            "workspace_info",
            "Read one workspace: state, limits, quota usage and remaining TTL.",
            object(
                json!({"workspace_id": workspace_id.clone()}),
                &["workspace_id"],
            ),
            "workspace:read",
            true,
        ),
        tool(
            "workspace_update",
            "Change a workspace name, TTL, pinning or execution mode.",
            object(
                json!({
                    "workspace_id": workspace_id.clone(),
                    "name": {"type": "string"},
                    "ttl_seconds": {"type": "integer", "minimum": 60},
                    "pinned": {"type": "boolean"},
                    "execution_mode": {"type": "string", "enum": ["ephemeral", "persistent"]}
                }),
                &["workspace_id"],
            ),
            "workspace:write",
            false,
        ),
        tool(
            "workspace_stop",
            "Stop a workspace container while keeping its files.",
            object(
                json!({"workspace_id": workspace_id.clone()}),
                &["workspace_id"],
            ),
            "workspace:write",
            false,
        ),
        tool(
            "workspace_delete",
            "Delete a workspace and its files. Published artifacts are kept.",
            object(
                json!({"workspace_id": workspace_id.clone()}),
                &["workspace_id"],
            ),
            "workspace:delete",
            false,
        ),
        tool(
            "exec",
            "Run a command inside a workspace container. Returns a job identifier immediately; poll job_status and job_logs.",
            object(
                json!({
                    "workspace_id": workspace_id,
                    "argv": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "description": "Program and arguments. No shell interpretation is applied."
                    },
                    "cwd": {"type": "string", "description": "Workspace-relative working directory."},
                    "env": {
                        "type": "object",
                        "additionalProperties": {"type": "string"},
                        "description": "Extra environment variables for this job."
                    },
                    "timeout_seconds": {"type": "integer", "minimum": 1},
                    "idempotency_key": {"type": "string", "maxLength": 128}
                }),
                &["workspace_id", "argv"],
            ),
            "execution:run",
            false,
        ),
        tool(
            "job_status",
            "Read the state, exit code and duration of a job.",
            object(json!({"job_id": job_id.clone()}), &["job_id"]),
            "workspace:read",
            true,
        ),
        tool(
            "job_logs",
            "Read bounded, paginated stdout or stderr of a job.",
            object(
                json!({
                    "job_id": job_id.clone(),
                    "stream": {"type": "string", "enum": ["stdout", "stderr"]},
                    "offset": {"type": "integer", "minimum": 0},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 262144}
                }),
                &["job_id"],
            ),
            "workspace:read",
            true,
        ),
        tool(
            "job_cancel",
            "Cancel a queued or running job.",
            object(json!({"job_id": job_id}), &["job_id"]),
            "execution:run",
            false,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_closed_schema_and_annotations() {
        for definition in definitions() {
            assert_eq!(
                definition.tool.input_schema["type"], "object",
                "{}",
                definition.tool.name
            );
            assert_eq!(
                definition.tool.input_schema["additionalProperties"], false,
                "{}",
                definition.tool.name
            );
            assert!(
                definition.tool.annotations.is_some(),
                "{}",
                definition.tool.name
            );
            assert!(!definition.scope.is_empty());
        }
    }

    #[test]
    fn mutating_tools_are_not_marked_read_only() {
        for definition in definitions() {
            let name = definition.tool.name.as_ref();

            let read_only = definition
                .tool
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.read_only_hint)
                .unwrap_or(false);

            if name.starts_with("workspace_")
                && matches!(
                    name,
                    "workspace_create" | "workspace_update" | "workspace_stop" | "workspace_delete"
                )
            {
                assert!(!read_only, "{name}");
            }

            if name == "exec" || name == "job_cancel" {
                assert!(!read_only, "{name}");
            }
        }
    }

    #[test]
    fn tool_names_are_unique() {
        let mut names: Vec<String> = definitions()
            .into_iter()
            .map(|definition| definition.tool.name.to_string())
            .collect();

        let count = names.len();

        names.sort();
        names.dedup();

        assert_eq!(names.len(), count);
    }
}
