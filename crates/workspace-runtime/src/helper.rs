//! Client for the trusted Linux helper.
//!
//! One process per request: the server frames a structured request, writes it
//! to the helper's stdin, reads one framed response and reaps the process. The
//! helper never sees a shell and never receives a command name from a caller.

use serde::Serialize;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;
use workspace_core::helper::{self, Operation, Request, Response};

use crate::{Host, RuntimeError, RuntimeSettings};

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Identity reported by the helper.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HelperInfo {
    pub version: String,
    pub protocol: u32,
    pub uid: Option<u32>,
}

/// Helper client bound to one installation.
#[derive(Debug, Clone)]
pub struct Helper {
    host: Host,
    path: String,
    allow_prefix: String,
    timeout: Duration,
}

impl Helper {
    pub fn new(host: Host, settings: &RuntimeSettings) -> Self {
        Self {
            host,
            path: settings.helper_path.clone(),
            allow_prefix: settings.data_root.clone(),
            timeout: settings.command_timeout,
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Execute one helper operation.
    pub async fn call(
        &self,
        operation: Operation,
        body: &[u8],
    ) -> Result<(Value, Vec<u8>), RuntimeError> {
        if body.len() as u64 > helper::MAX_BODY_BYTES {
            return Err(RuntimeError::Protocol(
                "helper request body exceeds protocol limits".into(),
            ));
        }

        let request = Request {
            protocol: helper::PROTOCOL_VERSION,
            request_id: next_request_id(),
            body_len: body.len() as u64,
            operation,
        };

        let encoded = serde_json::to_vec(&request)
            .map_err(|error| RuntimeError::Protocol(format!("cannot encode request: {error}")))?;

        let arguments = vec!["--allow-prefix".to_owned(), self.allow_prefix.clone()];

        let mut child = self.host.helper_command(&self.path, &arguments)?.spawn()?;

        let result = tokio::time::timeout(
            self.timeout,
            exchange(&mut child, &encoded, body, &request.request_id),
        )
        .await
        .map_err(|_| RuntimeError::Timeout(self.timeout.as_secs()))?;

        // Never leave a helper process behind, even on a protocol failure.
        let _ = child.start_kill();
        let _ = child.wait().await;

        result
    }

    /// Verify that the helper is installed, executable and speaks this protocol.
    pub async fn ping(&self) -> Result<HelperInfo, RuntimeError> {
        let (value, _) = self.call(Operation::Ping, &[]).await?;

        let version = value["version"]
            .as_str()
            .ok_or_else(|| RuntimeError::Protocol("helper did not report a version".into()))?;

        let protocol = value["protocol"]
            .as_u64()
            .ok_or_else(|| RuntimeError::Protocol("helper did not report a protocol".into()))?;

        Ok(HelperInfo {
            version: version.to_owned(),
            protocol: protocol as u32,
            uid: value["uid"].as_u64().map(|uid| uid as u32),
        })
    }
}

async fn exchange(
    child: &mut Child,
    header: &[u8],
    body: &[u8],
    request_id: &str,
) -> Result<(Value, Vec<u8>), RuntimeError> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| RuntimeError::Transport("helper stdin is unavailable".into()))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| RuntimeError::Transport("helper stdout is unavailable".into()))?;

    let stderr = child.stderr.take().map(|stderr| {
        tokio::spawn(async move {
            let mut stderr = stderr;
            let mut collected = Vec::new();
            let mut buffer = [0u8; 4096];

            while collected.len() < 8192 {
                match stderr.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => collected.extend_from_slice(&buffer[..read]),
                }
            }

            String::from_utf8_lossy(&collected).into_owned()
        })
    });

    let mut frame = Vec::with_capacity(4 + header.len() + body.len());
    frame.extend_from_slice(&(header.len() as u32).to_be_bytes());
    frame.extend_from_slice(header);
    frame.extend_from_slice(body);

    stdin
        .write_all(&frame)
        .await
        .map_err(|error| RuntimeError::Transport(format!("cannot write to helper: {error}")))?;

    // Close stdin so the helper can never wait for more input.
    drop(stdin);

    let read = read_response(&mut stdout).await;

    let status = child
        .wait()
        .await
        .map_err(|error| RuntimeError::Transport(error.to_string()))?;

    let stderr = match stderr {
        Some(task) => task.await.unwrap_or_default(),
        None => String::new(),
    };

    let (header, body) = read?;

    let response: Response = serde_json::from_slice(&header)
        .map_err(|error| RuntimeError::Protocol(format!("invalid helper response: {error}")))?;

    if response.request_id != request_id {
        return Err(RuntimeError::Protocol(
            "helper response does not match the request".into(),
        ));
    }

    if response.protocol != helper::PROTOCOL_VERSION {
        return Err(RuntimeError::Protocol(
            "helper protocol version mismatch".into(),
        ));
    }

    if response.body_len != body.len() as u64 {
        return Err(RuntimeError::Protocol(
            "helper response body is inconsistent".into(),
        ));
    }

    match response.result {
        Ok(value) => Ok((value, body)),
        Err(error) => {
            let message = if stderr.trim().is_empty() {
                error.message
            } else {
                format!("{} ({})", error.message, stderr.trim())
            };

            Err(RuntimeError::Helper {
                code: error.code,
                message,
            })
        }
    }
    .map_err(|error| match &error {
        // A missing binary or a non-zero exit with no protocol answer is a
        // transport problem, not a helper-level rejection.
        RuntimeError::Protocol(message) if !status.success() => RuntimeError::Transport(format!(
            "helper exited with {}: {}{message}",
            status.code().unwrap_or(-1),
            stderr.trim()
        )),
        _ => error,
    })
}

async fn read_response(
    stdout: &mut tokio::process::ChildStdout,
) -> Result<(Vec<u8>, Vec<u8>), RuntimeError> {
    let mut length = [0u8; 4];

    stdout
        .read_exact(&mut length)
        .await
        .map_err(|error| RuntimeError::Transport(format!("helper did not answer: {error}")))?;

    let header_len = u32::from_be_bytes(length);

    if header_len > helper::MAX_HEADER_BYTES {
        return Err(RuntimeError::Protocol(
            "helper response header is too large".into(),
        ));
    }

    let mut header = vec![0u8; header_len as usize];
    stdout.read_exact(&mut header).await.map_err(|error| {
        RuntimeError::Transport(format!("truncated helper response header: {error}"))
    })?;

    let body_len = serde_json::from_slice::<Value>(&header)
        .ok()
        .and_then(|value| value.get("body_len").and_then(Value::as_u64))
        .unwrap_or(0);

    if body_len > helper::MAX_BODY_BYTES {
        return Err(RuntimeError::Protocol(
            "helper response body is too large".into(),
        ));
    }

    let mut body = vec![0u8; body_len as usize];

    if body_len > 0 {
        stdout.read_exact(&mut body).await.map_err(|error| {
            RuntimeError::Transport(format!("truncated helper response body: {error}"))
        })?;
    }

    Ok((header, body))
}

fn next_request_id() -> String {
    let counter = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();

    format!("{nanos:x}-{counter:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_unique() {
        let first = next_request_id();
        let second = next_request_id();

        assert_ne!(first, second);
        assert!(!first.is_empty());
    }
}
