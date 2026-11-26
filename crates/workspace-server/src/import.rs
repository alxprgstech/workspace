//! Controlled external downloads for `file_import` and `url_import`.
//!
//! Two different policies share one hardened downloader:
//!
//! * `file_import` fetches a URL that ChatGPT placed into the tool arguments
//!   (the documented `openai/fileParams` mechanism). Any public HTTPS host is
//!   allowed, because the host mints a short-lived signed URL.
//! * `url_import` only fetches hosts from the administrator allowlist.
//!
//! Both reject credentials in URLs, non-HTTPS schemes, loopback/private/
//! link-local/metadata addresses (including after redirects), pin the resolved
//! address to defeat DNS rebinding, cap the size and the number of redirects.

use serde::Serialize;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

use crate::files::{Files, FilesError};
use workspace_runtime::Host;

const MAX_REDIRECTS: usize = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

/// Result of a controlled download.
#[derive(Debug, Clone, Serialize)]
pub struct Downloaded {
    pub staging: String,
    pub bytes: u64,
    pub file_name: Option<String>,
}

/// SSRF and download policy.
#[derive(Debug, Clone, Default)]
pub struct Downloader {
    allowlist: Vec<String>,
}

impl Downloader {
    pub fn new(allowlist: Vec<String>) -> Self {
        Self {
            allowlist: allowlist
                .into_iter()
                .map(|entry| entry.trim().to_ascii_lowercase())
                .filter(|entry| !entry.is_empty())
                .collect(),
        }
    }

    /// Download `url` into the dedicated staging area.
    ///
    /// `require_allowlist` selects the stricter policy used by `url_import`.
    pub async fn fetch(
        &self,
        host: &Host,
        files: &Files,
        url: &str,
        require_allowlist: bool,
    ) -> Result<Downloaded, FilesError> {
        let mut current = ParsedUrl::parse(url)?;

        let mut redirects = 0;

        loop {
            if require_allowlist && !self.allowed(&current.host) {
                return Err(FilesError::Invalid(
                    "the URL host is not on the administrator allowlist",
                ));
            }

            let addresses = resolve_public(&current.host, current.port).await?;
            let client = build_client(&current.host, addresses.first().copied())?;

            let response = client
                .get(current.original.clone())
                .send()
                .await
                .map_err(|_| FilesError::Invalid("the download failed"))?;

            if response.status().is_redirection() {
                redirects += 1;

                if redirects > MAX_REDIRECTS {
                    return Err(FilesError::Invalid("too many redirects"));
                }

                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or(FilesError::Invalid("redirect without a location"))?;

                current = ParsedUrl::resolve(&current, location)?;

                continue;
            }

            if !response.status().is_success() {
                return Err(FilesError::Invalid("the download source returned an error"));
            }

            let declared = response.content_length();
            let limit = files.max_import_bytes();

            if declared.is_some_and(|length| length > limit) {
                return Err(FilesError::TooLarge);
            }

            let staging = files.staging_name();
            let path = files.staging_path(&staging)?;

            let mut command = host
                .stream_stdin_command("/usr/bin/tee", &[path])
                .map_err(|error| FilesError::Storage(error.to_string()))?;

            let mut child = command
                .spawn()
                .map_err(|error| FilesError::Storage(error.to_string()))?;

            let Some(mut stdin) = child.stdin.take() else {
                return Err(FilesError::Storage("cannot open the staging pipe".into()));
            };

            let mut stream = response.bytes_stream();
            let mut total = 0u64;

            use futures_util::StreamExt;

            while let Some(chunk) = stream.next().await {
                let Ok(chunk) = chunk else {
                    let _ = child.kill().await;
                    let _ = files.discard_staged(&staging).await;

                    return Err(FilesError::Storage("the download was interrupted".into()));
                };

                total += chunk.len() as u64;

                if total > limit {
                    let _ = child.kill().await;
                    let _ = files.discard_staged(&staging).await;

                    return Err(FilesError::TooLarge);
                }

                if stdin.write_all(&chunk).await.is_err() {
                    break;
                }
            }

            drop(stdin);

            let status = child
                .wait()
                .await
                .map_err(|error| FilesError::Storage(error.to_string()))?;

            if !status.success() {
                let _ = files.discard_staged(&staging).await;

                return Err(FilesError::Storage(
                    "the downloaded file could not be stored".into(),
                ));
            }

            return Ok(Downloaded {
                staging,
                bytes: total,
                file_name: current.file_name,
            });
        }
    }

    fn allowed(&self, host: &str) -> bool {
        self.allowlist
            .iter()
            .any(|entry| host == entry || host.ends_with(&format!(".{entry}")) || entry == "*")
    }
}

fn build_client(host: &str, address: Option<IpAddr>) -> Result<reqwest::Client, FilesError> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        // An environment proxy would silently bypass the validated, pinned
        // address: the request would be resolved again by the proxy. The
        // download policy is a direct connection, so the proxy is disabled
        // explicitly instead of depending on the host environment.
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent("ALXPRGS-Workspace/0.1");

    if let Some(address) = address {
        // Pin the validated address so DNS cannot be rebound between the check
        // and the connection.
        builder = builder.resolve(host, std::net::SocketAddr::new(address, 0));
    }

    builder
        .build()
        .map_err(|_| FilesError::Invalid("cannot initialise the HTTPS client"))
}

/// Resolve a host and reject every non-public address.
async fn resolve_public(host: &str, port: u16) -> Result<Vec<IpAddr>, FilesError> {
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| FilesError::Invalid("the URL host cannot be resolved"))?
        .map(|socket| socket.ip())
        .collect::<Vec<_>>();

    if addresses.is_empty() {
        return Err(FilesError::Invalid("the URL host has no addresses"));
    }

    for address in &addresses {
        if is_forbidden(*address) {
            return Err(FilesError::Invalid(
                "the URL resolves to a non-public address",
            ));
        }
    }

    Ok(addresses)
}

/// Reject addresses a workspace must never reach.
pub fn is_forbidden(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ipv4) => is_forbidden_v4(ipv4),
        IpAddr::V6(ipv6) => {
            if let Some(mapped) = ipv6.to_ipv4_mapped() {
                return is_forbidden_v4(mapped);
            }

            ipv6.is_loopback()
                || ipv6.is_unspecified()
                || ipv6.is_multicast()
                || (ipv6.segments()[0] & 0xfe00) == 0xfc00 // unique local
                || (ipv6.segments()[0] & 0xffc0) == 0xfe80 // link local
        }
    }
}

fn is_forbidden_v4(address: Ipv4Addr) -> bool {
    address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_unspecified()
        || address.is_multicast()
        // 100.64.0.0/10 carrier-grade NAT
        || (address.octets()[0] == 100 && (64..128).contains(&address.octets()[1]))
        // 192.0.0.0/24 IETF protocol assignments
        || (address.octets()[0] == 192 && address.octets()[1] == 0 && address.octets()[2] == 0)
        // 198.18.0.0/15 benchmarking
        || (address.octets()[0] == 198 && matches!(address.octets()[1], 18 | 19))
}

/// Minimal absolute URL representation.
struct ParsedUrl {
    original: String,
    host: String,
    port: u16,
    file_name: Option<String>,
}

impl ParsedUrl {
    fn parse(raw: &str) -> Result<Self, FilesError> {
        let url =
            reqwest::Url::parse(raw).map_err(|_| FilesError::Invalid("invalid download URL"))?;

        if url.scheme() != "https" {
            return Err(FilesError::Invalid("download URL must use HTTPS"));
        }

        if !url.username().is_empty() || url.password().is_some() {
            return Err(FilesError::Invalid(
                "download URL must not contain credentials",
            ));
        }

        let host = url
            .host_str()
            .ok_or(FilesError::Invalid("download URL has no host"))?
            .to_ascii_lowercase();

        let port = url.port_or_known_default().unwrap_or(443);

        let file_name = url
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .filter(|name| !name.is_empty())
            .map(|name| name.to_owned());

        Ok(Self {
            original: url.to_string(),
            host,
            port,
            file_name,
        })
    }

    fn resolve(base: &ParsedUrl, location: &str) -> Result<Self, FilesError> {
        let resolved = reqwest::Url::parse(&base.original)
            .and_then(|url| url.join(location))
            .map_err(|_| FilesError::Invalid("invalid redirect location"))?;

        Self::parse(resolved.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_and_special_addresses_are_forbidden() {
        for address in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::1",
        ] {
            assert!(
                is_forbidden(address.parse().unwrap()),
                "{address} must be forbidden"
            );
        }

        for address in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(
                !is_forbidden(address.parse().unwrap()),
                "{address} must be allowed"
            );
        }
    }

    #[test]
    fn urls_require_https_without_credentials() {
        assert!(ParsedUrl::parse("http://example.com/a.zip").is_err());
        assert!(ParsedUrl::parse("https://user:pass@example.com/a.zip").is_err());
        assert!(ParsedUrl::parse("https://example.com/a.zip").is_ok());
    }

    #[test]
    fn allowlist_matches_hosts_and_subdomains_only() {
        let downloader = Downloader::new(vec!["example.com".into()]);

        assert!(downloader.allowed("example.com"));
        assert!(downloader.allowed("files.example.com"));
        assert!(!downloader.allowed("example.com.evil.test"));
        assert!(!downloader.allowed("evil.test"));

        let empty = Downloader::new(vec![]);
        assert!(!empty.allowed("example.com"));
    }

    #[test]
    fn file_names_are_taken_from_the_url() {
        let url = ParsedUrl::parse("https://example.com/dir/source.zip").unwrap();

        assert_eq!(url.file_name.as_deref(), Some("source.zip"));
        assert_eq!(url.host, "example.com");
    }

    #[test]
    fn redirects_cannot_downgrade_the_policy() {
        let base = ParsedUrl::parse("https://example.com/a.zip").unwrap();

        // A redirect to plain HTTP or to credentials is refused exactly like a
        // top-level URL: the policy applies to every hop.
        assert!(ParsedUrl::resolve(&base, "http://example.com/b.zip").is_err());
        assert!(ParsedUrl::resolve(&base, "https://user:pass@example.com/b.zip").is_err());

        let resolved = ParsedUrl::resolve(&base, "https://cdn.example.com/b.zip").unwrap();

        assert_eq!(resolved.host, "cdn.example.com");
    }

    #[test]
    fn ipv4_mapped_ipv6_uses_the_ipv4_policy() {
        // ::ffff:127.0.0.1 must be refused exactly like 127.0.0.1.
        for mapped in [
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
        ] {
            assert!(is_forbidden(mapped.parse().unwrap()), "{mapped}");
        }
    }
}
