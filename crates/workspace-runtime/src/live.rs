use serde_json::Value;
use std::{path::PathBuf, time::Duration};
use tokio::{process::Command, time::timeout};

const DISTRO: &str = "ALXPRGS-Workspace";
const SOCKET: &str = "unix:///run/user/1000/docker.sock";
const DATA: &str = "/srv/alxprgs-workspace/data";

#[derive(Debug)]
pub struct LiveReport {
    pub engine: String,
    pub cgroup_version: String,
    pub storage: String,
}

async fn wsl(args: &[&str]) -> Result<String, String> {
    if !cfg!(windows) {
        return Err("WSL backend requires Windows".into());
    }

    let root = std::env::var_os("SystemRoot").ok_or("SystemRoot is missing")?;

    let binary = PathBuf::from(root).join("System32").join("wsl.exe");

    let mut command = Command::new(binary);

    command
        .args([
            "-d",
            DISTRO,
            "-u",
            "workspace",
            "--cd",
            "/",
            "--exec",
            "/usr/bin/env",
            "-i",
            "HOME=/home/workspace",
            "PATH=/usr/bin:/bin",
            "XDG_RUNTIME_DIR=/run/user/1000",
            "/usr/bin/timeout",
            "5s",
        ])
        .args(args)
        .kill_on_drop(true);

    let output = timeout(Duration::from_secs(10), command.output())
        .await
        .map_err(|_| "WSL command timed out")?
        .map_err(|error| error.to_string())?;

    if !output.status.success() {
        return Err(format!("WSL command failed: {}", output.status));
    }

    String::from_utf8(output.stdout)
        .map(|text| text.trim().to_owned())
        .map_err(|_| "Invalid UTF-8 from WSL".into())
}

fn validate_engine(info: &Value) -> Result<(), String> {
    let rootless = info["SecurityOptions"].as_array().is_some_and(|options| {
        options.iter().any(|option| {
            option
                .as_str()
                .is_some_and(|value| value.split(',').any(|part| part == "name=rootless"))
        })
    });

    if !rootless {
        return Err("Rootless Docker is not proven".into());
    }

    for (key, expected) in [
        ("Name", "alxprgs-workspace"),
        ("OSType", "linux"),
        ("CgroupDriver", "systemd"),
        ("CgroupVersion", "2"),
        ("DockerRootDir", "/home/workspace/.local/share/docker"),
    ] {
        if info[key].as_str() != Some(expected) {
            return Err(format!("Unexpected Docker property: {key}"));
        }
    }

    Ok(())
}

fn validate_storage(output: &str) -> Result<(), String> {
    let mut parts = output.split_whitespace();

    if parts.next() != Some("ext4") {
        return Err("Workspace storage is not ext4".into());
    }

    let options = parts.next().unwrap_or_default();

    for required in ["rw", "prjquota", "nosuid", "nodev"] {
        if !options.split(',').any(|option| option == required) {
            return Err(format!("Missing storage option: {required}"));
        }
    }

    Ok(())
}

pub async fn probe_wsl() -> Result<LiveReport, String> {
    timeout(Duration::from_secs(25), async {
        let kernel = wsl(&["/usr/bin/uname", "-r"]).await?;

        if !kernel.to_ascii_lowercase().contains("wsl2") {
            return Err("WSL2 kernel is not proven".into());
        }

        let mut result = None;

        for _ in 0..5 {
            match wsl(&[
                "/usr/bin/docker",
                "--host",
                SOCKET,
                "info",
                "--format",
                "{{json .}}",
            ])
            .await
            {
                Ok(output) => {
                    result = Some(output);
                    break;
                }
                Err(_) => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }

        let raw = result.ok_or("Dedicated Docker Engine is unavailable")?;

        let info: Value = serde_json::from_str(&raw).map_err(|_| "Invalid Docker information")?;

        validate_engine(&info)?;

        let storage = wsl(&["/usr/bin/findmnt", "-n", "-M", DATA, "-o", "FSTYPE,OPTIONS"]).await?;

        validate_storage(&storage)?;

        let version = info["ServerVersion"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or("Docker version is missing")?;

        Ok(LiveReport {
            engine: version.to_owned(),
            cgroup_version: "2".into(),
            storage,
        })
    })
    .await
    .map_err(|_| "Runtime preflight timed out".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_engine() -> Value {
        json!({
            "SecurityOptions": ["name=rootless", "name=seccomp"],
            "Name": "alxprgs-workspace",
            "OSType": "linux",
            "CgroupDriver": "systemd",
            "CgroupVersion": "2",
            "DockerRootDir": "/home/workspace/.local/share/docker"
        })
    }

    #[test]
    fn accepts_dedicated_engine() {
        assert!(validate_engine(&valid_engine()).is_ok());
    }

    #[test]
    fn rejects_rootful_engine() {
        let mut info = valid_engine();
        info["SecurityOptions"] = json!(["name=seccomp"]);
        assert!(validate_engine(&info).is_err());
    }

    #[test]
    fn rejects_other_engine() {
        let mut info = valid_engine();
        info["Name"] = json!("docker-desktop");
        assert!(validate_engine(&info).is_err());
    }

    #[test]
    fn rejects_wrong_cgroup() {
        let mut info = valid_engine();
        info["CgroupVersion"] = json!("1");
        assert!(validate_engine(&info).is_err());
    }

    #[test]
    fn rejects_missing_quota() {
        assert!(validate_storage("ext4 rw,nosuid,nodev").is_err());
    }

    #[test]
    fn accepts_quota_storage() {
        assert!(validate_storage("ext4 rw,nosuid,nodev,relatime,prjquota").is_ok());
    }
}
