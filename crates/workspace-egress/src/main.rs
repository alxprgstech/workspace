#![forbid(unsafe_code)]

//! Trusted allowlist egress proxy for Workspace containers.
//!
//! Workspace containers run on an internal Docker network with no default
//! route. The only reachable address is this proxy, which:
//!
//! * accepts HTTPS tunnelling only (`CONNECT host:443`); plain HTTP is denied;
//! * allows a host only when it matches the administrator allowlist;
//! * rejects IP literals and resolves every host itself, refusing loopback,
//!   private, link-local, metadata, CGNAT and other non-public addresses;
//! * connects to the address it validated, so DNS cannot be rebound between the
//!   policy check and the connection;
//! * bounds connections, request size and idle time.
//!
//! It has no administrative interface and no filesystem access beyond reading
//! the allowlist once at startup.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;

const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_CONNECTIONS: usize = 64;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(300);
const ALLOWED_PORT: u16 = 443;

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("EGRESS_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3128);

    let allowlist_path =
        std::env::var("EGRESS_ALLOWLIST").unwrap_or_else(|_| "/etc/egress/allowlist".to_owned());

    let allowlist = match std::fs::read_to_string(&allowlist_path) {
        Ok(text) => Allowlist::parse(&text),
        Err(error) => {
            eprintln!("workspace-egress: cannot read {allowlist_path}: {error}");
            std::process::exit(2);
        }
    };

    if allowlist.is_empty() {
        eprintln!("workspace-egress: the allowlist is empty; refusing to start");
        std::process::exit(2);
    }

    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("workspace-egress: cannot bind port {port}: {error}");
            std::process::exit(1);
        }
    };

    eprintln!(
        "workspace-egress: listening on {port}, {} allowed hosts",
        allowlist.len()
    );

    let allowlist = Arc::new(allowlist);
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                eprintln!("workspace-egress: accept failed: {error}");
                continue;
            }
        };

        let Ok(permit) = Arc::clone(&connections).try_acquire_owned() else {
            let mut stream = stream;
            let _ = stream
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\n\r\n")
                .await;
            continue;
        };

        let allowlist = Arc::clone(&allowlist);

        tokio::spawn(async move {
            let _permit = permit;

            if let Err(error) = handle(stream, &allowlist).await {
                eprintln!("workspace-egress: {peer}: {error}");
            }
        });
    }
}

#[derive(Debug)]
struct Allowlist {
    hosts: BTreeSet<String>,
}

impl Allowlist {
    fn parse(text: &str) -> Self {
        let hosts = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| line.trim_end_matches('.').to_ascii_lowercase())
            .filter(|line| {
                !line.is_empty()
                    && line.len() <= 253
                    && line
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            })
            .collect();

        Self { hosts }
    }

    fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }

    fn len(&self) -> usize {
        self.hosts.len()
    }

    /// Exact host or any subdomain of an allowed host.
    fn allows(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();

        self.hosts.contains(&host)
            || self
                .hosts
                .iter()
                .any(|entry| host.ends_with(&format!(".{entry}")))
    }
}

async fn handle(mut stream: TcpStream, allowlist: &Allowlist) -> Result<(), String> {
    let request = tokio::time::timeout(IO_TIMEOUT, read_head(&mut stream))
        .await
        .map_err(|_| "timed out reading the request".to_owned())??;

    let mut parts = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();

    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();

    if !version.starts_with("HTTP/1.") {
        return reject(&mut stream, "400 Bad Request", "malformed request").await;
    }

    if method != "CONNECT" {
        return reject(
            &mut stream,
            "405 Method Not Allowed",
            "only HTTPS tunnelling is allowed",
        )
        .await;
    }

    let Some((host, port)) = split_authority(target) else {
        return reject(&mut stream, "400 Bad Request", "invalid authority").await;
    };

    if port != ALLOWED_PORT {
        return reject(&mut stream, "403 Forbidden", "only port 443 is allowed").await;
    }

    if host.parse::<IpAddr>().is_ok() {
        return reject(&mut stream, "403 Forbidden", "IP literals are not allowed").await;
    }

    if !allowlist.allows(&host) {
        return reject(&mut stream, "403 Forbidden", "host is not on the allowlist").await;
    }

    // Resolve and validate every address before connecting.
    let addresses = match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(addresses) => addresses.map(|socket| socket.ip()).collect::<Vec<_>>(),
        Err(_) => return reject(&mut stream, "502 Bad Gateway", "host cannot be resolved").await,
    };

    if addresses.is_empty() {
        return reject(&mut stream, "502 Bad Gateway", "host has no addresses").await;
    }

    if addresses.iter().any(|address| is_forbidden(*address)) {
        return reject(
            &mut stream,
            "403 Forbidden",
            "host resolves to a non-public address",
        )
        .await;
    }

    let mut upstream = None;

    for address in addresses {
        let attempt =
            tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((address, port))).await;

        if let Ok(Ok(socket)) = attempt {
            upstream = Some(socket);
            break;
        }
    }

    let Some(mut upstream) = upstream else {
        return reject(&mut stream, "502 Bad Gateway", "connection failed").await;
    };

    if stream
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .is_err()
    {
        return Ok(());
    }

    match tokio::time::timeout(
        IO_TIMEOUT,
        tokio::io::copy_bidirectional(&mut stream, &mut upstream),
    )
    .await
    {
        Ok(Ok((to_upstream, to_client))) => {
            eprintln!("workspace-egress: {host}: {to_upstream} up, {to_client} down bytes");
            Ok(())
        }
        Ok(Err(error)) => Err(format!("{host}: tunnel error: {error}")),
        Err(_) => Err(format!("{host}: tunnel idle timeout")),
    }
}

async fn read_head(stream: &mut TcpStream) -> Result<String, String> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| format!("read failed: {error}"))?;

        if read == 0 {
            break;
        }

        buffer.extend_from_slice(&chunk[..read]);

        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }

        if buffer.len() > MAX_HEADER_BYTES {
            return Err("request headers are too large".to_owned());
        }
    }

    String::from_utf8(buffer).map_err(|_| "request headers are not UTF-8".to_owned())
}

async fn reject(stream: &mut TcpStream, status: &str, message: &str) -> Result<(), String> {
    eprintln!("workspace-egress: denied: {status}: {message}");

    let body = format!("{status}: {message}\n");

    let _ = stream
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await;

    Ok(())
}

fn split_authority(target: &str) -> Option<(String, u16)> {
    let (host, port) = target.rsplit_once(':')?;

    let host = host.trim_matches(['[', ']']).to_owned();

    if host.is_empty() || host.len() > 253 {
        return None;
    }

    let port: u16 = port.parse().ok()?;

    Some((host, port))
}

/// Reject any address that is not a public unicast destination.
pub fn is_forbidden(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ipv4) => forbidden_v4(ipv4),
        IpAddr::V6(ipv6) => {
            if let Some(mapped) = ipv6.to_ipv4_mapped() {
                return forbidden_v4(mapped);
            }

            ipv6.is_loopback()
                || ipv6.is_unspecified()
                || ipv6.is_multicast()
                || (ipv6.segments()[0] & 0xfe00) == 0xfc00
                || (ipv6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

fn forbidden_v4(address: Ipv4Addr) -> bool {
    address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_unspecified()
        || address.is_multicast()
        || (address.octets()[0] == 100 && (64..128).contains(&address.octets()[1]))
        || (address.octets()[0] == 192 && address.octets()[1] == 0 && address.octets()[2] == 0)
        || (address.octets()[0] == 198 && matches!(address.octets()[1], 18 | 19))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_matches_exact_hosts_and_subdomains() {
        let list = Allowlist::parse(
            "# comment\n\
             crates.io\n\
             .static.crates.io\n\
             github.com\n",
        );

        assert!(list.allows("crates.io"));
        assert!(list.allows("index.crates.io"));
        assert!(list.allows("github.com"));
        assert!(list.allows("objects.github.com"));

        assert!(!list.allows("example.com"));
        assert!(!list.allows("crates.io.evil.test"));
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn private_addresses_are_forbidden() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "172.20.10.1",
            "192.168.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
        ] {
            assert!(is_forbidden(address.parse().unwrap()), "{address}");
        }

        for address in ["1.1.1.1", "140.82.121.4", "2606:4700:4700::1111"] {
            assert!(!is_forbidden(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn authority_parsing_is_strict() {
        assert_eq!(
            split_authority("crates.io:443"),
            Some(("crates.io".to_owned(), 443))
        );
        assert_eq!(
            split_authority("[2606:4700::1111]:443"),
            Some(("2606:4700::1111".to_owned(), 443))
        );
        assert_eq!(split_authority("crates.io"), None);
        assert_eq!(split_authority(":443"), None);
        assert_eq!(split_authority("crates.io:https"), None);
    }

    #[test]
    fn empty_allowlist_is_refused() {
        assert!(Allowlist::parse("\n# only comments\n").is_empty());
    }
}
