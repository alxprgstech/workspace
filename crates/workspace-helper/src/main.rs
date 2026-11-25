//! One-shot filesystem helper for the dedicated Workspace runtime.
//!
//! The server invokes this binary inside the dedicated Linux environment and
//! talks to it over stdin/stdout using the framed protocol from
//! `workspace_core::helper`. The helper never invokes a shell: it receives
//! structured requests and resolves every path through a capability directory
//! (`cap-std`), so a path can never escape the workspace root, not even through
//! a symbolic link.
//!
//! Usage:
//!
//! ```text
//! workspace-helper --allow-prefix /srv/alxprgs-workspace/data
//! ```
//!
//! Every root named in a request must be equal to or below one of the allowed
//! prefixes.

#![forbid(unsafe_code)]

mod ops;
mod safe;

use std::io::{self, Read, Write};
use std::process::ExitCode;
use workspace_core::helper::{self, HelperError, Request, Response};

fn main() -> ExitCode {
    let prefixes = match parse_prefixes(std::env::args().skip(1)) {
        Ok(prefixes) => prefixes,
        Err(message) => {
            eprintln!("workspace-helper: {message}");
            return ExitCode::from(2);
        }
    };

    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    if let Err(error) = serve(&prefixes, &mut stdin, &mut stdout) {
        eprintln!("workspace-helper: {error}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

/// Parse `--allow-prefix <path>` arguments. At least one prefix is required.
fn parse_prefixes(arguments: impl Iterator<Item = String>) -> Result<Vec<String>, String> {
    let mut prefixes = Vec::new();
    let mut arguments = arguments.peekable();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--allow-prefix" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--allow-prefix requires a value".to_owned())?;

                if !value.starts_with('/') {
                    return Err("allowed prefixes must be absolute paths".to_owned());
                }

                prefixes.push(value);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    if prefixes.is_empty() {
        return Err("at least one --allow-prefix is required".to_owned());
    }

    Ok(prefixes)
}

/// Serve exactly one request and one response, then exit.
fn serve(prefixes: &[String], reader: &mut impl Read, writer: &mut impl Write) -> io::Result<()> {
    let (header, body) = helper::read_frame(reader)?;

    let request: Request = match serde_json::from_slice(&header) {
        Ok(request) => request,
        Err(error) => {
            let response = Response {
                protocol: helper::PROTOCOL_VERSION,
                request_id: String::new(),
                body_len: 0,
                result: Err(HelperError::new(
                    helper::code::INVALID_REQUEST,
                    format!("malformed request: {error}"),
                )),
            };

            return write_response(writer, &response, &[]);
        }
    };

    if request.protocol != helper::PROTOCOL_VERSION {
        let response = Response {
            protocol: helper::PROTOCOL_VERSION,
            request_id: request.request_id,
            body_len: 0,
            result: Err(HelperError::new(
                helper::code::VERSION_MISMATCH,
                "unsupported protocol version",
            )),
        };

        return write_response(writer, &response, &[]);
    }

    let request_id = request.request_id.clone();

    let (response, response_body) = match ops::execute(prefixes, &request, &body) {
        Ok(outcome) => (
            Response {
                protocol: helper::PROTOCOL_VERSION,
                request_id,
                body_len: outcome.body.len() as u64,
                result: Ok(outcome.value),
            },
            outcome.body,
        ),
        Err(error) => (
            Response {
                protocol: helper::PROTOCOL_VERSION,
                request_id,
                body_len: 0,
                result: Err(error),
            },
            Vec::new(),
        ),
    };

    write_response(writer, &response, &response_body)
}

fn write_response(writer: &mut impl Write, response: &Response, body: &[u8]) -> io::Result<()> {
    let encoded = serde_json::to_vec(response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

    helper::write_frame(writer, &encoded, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn run(arguments: &[&str], request: &serde_json::Value) -> Response {
        let prefixes = parse_prefixes(arguments.iter().map(|value| (*value).to_owned())).unwrap();

        let encoded = serde_json::to_vec(request).unwrap();

        let mut input = Vec::new();
        helper::write_frame(&mut input, &encoded, b"").unwrap();

        let mut output = Vec::new();
        serve(&prefixes, &mut Cursor::new(input), &mut output).unwrap();

        let (header, _body) = helper::read_frame(&mut output.as_slice()).unwrap();
        serde_json::from_slice(&header).unwrap()
    }

    #[test]
    fn requires_at_least_one_prefix() {
        assert!(parse_prefixes(std::iter::empty()).is_err());
        assert!(parse_prefixes(["relative".to_owned()].into_iter()).is_err());
        assert!(parse_prefixes(["--allow-prefix".to_owned()].into_iter()).is_err());
        assert!(
            parse_prefixes(["--allow-prefix".to_owned(), "/data".to_owned()].into_iter()).is_ok()
        );
    }

    #[test]
    fn rejects_unknown_protocol_version() {
        let response = run(
            &["--allow-prefix", "/tmp"],
            &serde_json::json!({
                "protocol": 999,
                "request_id": "r1",
                "body_len": 0,
                "op": "ping"
            }),
        );

        assert!(response.result.is_err());
        assert_eq!(
            response.result.unwrap_err().code,
            helper::code::VERSION_MISMATCH
        );
    }

    #[test]
    fn rejects_malformed_request() {
        let prefixes = vec!["/tmp".to_owned()];
        let mut input = Vec::new();
        helper::write_frame(&mut input, b"{not json", b"").unwrap();

        let mut output = Vec::new();
        serve(&prefixes, &mut Cursor::new(input), &mut output).unwrap();

        let (header, _) = helper::read_frame(&mut output.as_slice()).unwrap();
        let response: Response = serde_json::from_slice(&header).unwrap();

        assert_eq!(
            response.result.unwrap_err().code,
            helper::code::INVALID_REQUEST
        );
    }
}
