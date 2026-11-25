//! Tool payloads served by the MCP gateway.
//!
//! Every payload is derived from the same [`RuntimeStatus`] the HTTP API and
//! `doctor` use, so the surfaces cannot disagree.

use serde_json::{Value, json};
use workspace_core::capabilities;
use workspace_runtime::status::RuntimeStatus;

/// Build and runtime information for this instance.
pub fn server_information(schema_version: u32, runtime: &RuntimeStatus) -> Value {
    json!({
        "product": workspace_core::PRODUCT_NAME,
        "version": workspace_core::VERSION,
        "api_version": "v1",
        "platform": std::env::consts::OS,
        "architecture": std::env::consts::ARCH,
        "schema_version": schema_version,
        // `runtime_ready` keeps its historic meaning: untrusted execution is
        // possible right now, proven by the practical acceptance.
        "runtime_ready": runtime.execution_ready,
        "infrastructure_ready": runtime.infrastructure_ready,
        "execution_ready": runtime.execution_ready,
        "runtime": runtime,
    })
}

/// Catalog of workspace capabilities and service modules.
pub fn capabilities_catalog(schema_version: u32, runtime: &RuntimeStatus) -> Value {
    let modules: Vec<Value> = capabilities::MODULES
        .iter()
        .map(|module| {
            json!({
                "id": module.id,
                "name": module.name,
                "description": module.description,
                "version": module.version,
                "tools": module.tools,
                "depends_on": module.depends_on,
                "status": if module.available { "available" } else { "not_installed" },
            })
        })
        .collect();

    let docker = if runtime.execution_ready {
        "ready"
    } else if runtime.infrastructure_ready {
        "partial"
    } else {
        "not_ready"
    };

    json!({
        "schema_version": schema_version,
        "services": [
            {
                "id": "http_api",
                "status": "ready",
                "description": "HTTP API for health, readiness and server information.",
            },
            {
                "id": "sso_web_session",
                "status": "ready",
                "description": "ALXPRGS SSO sign-in with server-side encrypted sessions and refresh rotation.",
            },
            {
                "id": "mcp_gateway",
                "status": "ready",
                "description": "Local OAuth authorization, token issuance, refresh rotation, revocation and authenticated MCP transport.",
            },
            {
                "id": "docker_runtime",
                "status": docker,
                "description": "Dedicated rootless Docker runtime with hard quotas and proven isolation.",
            },
            {
                "id": "workspace_manager",
                "status": "in_progress",
                "description": "Workspace lifecycle management is being completed.",
            },
            {
                "id": "job_execution",
                "status": "in_progress",
                "description": "Job execution inside isolated containers is being completed.",
            },
        ],
        "modules": modules,
        "runtime_blocking": runtime.blocking,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unconfigured() -> RuntimeStatus {
        RuntimeStatus::unconfigured("execution runtime is not configured")
    }

    #[test]
    fn server_information_never_claims_readiness_without_acceptance() {
        let info = server_information(4, &unconfigured());

        assert_eq!(info["product"], workspace_core::PRODUCT_NAME);
        assert_eq!(info["schema_version"], 4);
        assert_eq!(info["runtime_ready"], false);
        assert_eq!(info["execution_ready"], false);
        assert_eq!(info["infrastructure_ready"], false);
    }

    #[test]
    fn catalog_reports_the_real_module_state() {
        let catalog = capabilities_catalog(4, &unconfigured());
        let modules = catalog["modules"].as_array().unwrap();

        let status = |id: &str| {
            modules
                .iter()
                .find(|entry| entry["id"] == id)
                .map(|entry| entry["status"].as_str().unwrap().to_owned())
                .unwrap_or_else(|| panic!("missing module {id}"))
        };

        for id in ["base", "rust", "cpp", "git", "python", "ffmpeg"] {
            assert_eq!(status(id), "available", "{id}");
        }

        // No trusted, checksum-pinned source exists for this module yet.
        assert_eq!(status("whisper-vad"), "not_installed");

        assert_eq!(catalog["services"][3]["id"], "docker_runtime");
        assert_eq!(catalog["services"][3]["status"], "not_ready");
    }
}
