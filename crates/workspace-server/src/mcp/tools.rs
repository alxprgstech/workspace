//! Tool payloads served by the MCP gateway.
//!
//! Both tools are read-only and describe only what this build actually does.

use serde_json::{Value, json};

/// Build and runtime information for this instance.
pub fn server_information(schema_version: u32) -> Value {
    json!({
        "product": workspace_core::PRODUCT_NAME,
        "version": workspace_core::VERSION,
        "api_version": "v1",
        "platform": std::env::consts::OS,
        "architecture": std::env::consts::ARCH,
        "schema_version": schema_version,
        // The execution runtime is not implemented yet; reporting it as ready
        // would make every later status claim unreliable.
        "runtime_ready": false,
    })
}

/// Catalog of capabilities with their real implementation status.
pub fn capabilities_catalog(schema_version: u32) -> Value {
    json!({
        "schema_version": schema_version,
        "capabilities": [
            {
                "id": "http_api",
                "status": "ready",
                "description": "Loopback HTTP API for health, readiness and server information.",
            },
            {
                "id": "sso_web_session",
                "status": "ready",
                "description": "ALXPRGS SSO sign-in with server-side encrypted sessions and refresh rotation.",
            },
            {
                "id": "mcp_gateway",
                "status": "ready",
                "description": "Local OAuth authorization, token issuance, refresh rotation, revocation and authenticated MCP transport are implemented.",
            },
            {
                "id": "docker_runtime",
                "status": "partial",
                "description": "Dedicated WSL2 rootless Docker and quota storage preflight are implemented; execution remains disabled.",
            },
            {
                "id": "workspace_manager",
                "status": "absent",
                "description": "Workspace lifecycle management (stage 009) is not implemented.",
            },
            {
                "id": "job_execution",
                "status": "absent",
                "description": "Job execution (stage 011) is not implemented.",
            },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_information_reports_the_build() {
        let info = server_information(3);

        assert_eq!(info["product"], workspace_core::PRODUCT_NAME);
        assert_eq!(info["version"], workspace_core::VERSION);
        assert_eq!(info["schema_version"], 3);
        assert_eq!(info["runtime_ready"], false);
        assert!(info["platform"].is_string());
    }

    #[test]
    fn catalog_never_claims_unimplemented_capabilities() {
        let catalog = capabilities_catalog(3);

        let entries = catalog["capabilities"].as_array().unwrap();

        let status = |id: &str| {
            entries
                .iter()
                .find(|entry| entry["id"] == id)
                .map(|entry| entry["status"].as_str().unwrap().to_owned())
                .unwrap_or_else(|| panic!("missing capability {id}"))
        };

        assert_eq!(status("http_api"), "ready");
        assert_eq!(status("mcp_gateway"), "ready");
        assert_eq!(status("docker_runtime"), "partial");
        assert_eq!(status("workspace_manager"), "absent");
    }
}
