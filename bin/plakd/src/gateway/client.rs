//! A blocking gateway client for the CLI. One HTTP/1.1 exchange, then close.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use plakous::gateway::protocol::call;
use plakous::gateway::protocol::refusal::RefusalBody;
use plakous::gateway::protocol::{Endpoint, KEY_HEADER, SUPPORTED_VERSIONS};

use super::{GatewayAddress, GatewayUnavailable};
use crate::CliResult;

pub(crate) const VERSIONS_PATH: &str = plakous::gateway::protocol::VERSIONS_PATH;

pub(crate) struct Call {
    pub status: u16,
    pub error: Option<String>,
    pub body: Vec<u8>,
}

pub(crate) fn exchange(
    address: &GatewayAddress,
    key: Option<&str>,
    method: &str,
    path: &str,
    body: &[u8],
) -> CliResult<Call> {
    exchange_with_timeout(address, key, method, path, body, Duration::from_secs(30))
}

fn exchange_with_timeout(
    address: &GatewayAddress,
    key: Option<&str>,
    method: &str,
    path: &str,
    body: &[u8],
    timeout: Duration,
) -> CliResult<Call> {
    match address {
        GatewayAddress::Unix(socket) => {
            let stream = UnixStream::connect(socket)
                .map_err(|_| GatewayUnavailable::Unreachable(address).failure())?;
            let _ = stream.set_read_timeout(Some(timeout));
            let _ = stream.set_write_timeout(Some(timeout));
            write_and_read(stream, key, method, path, body, address)
        }
        GatewayAddress::Tcp(addr) => {
            let stream = TcpStream::connect_timeout(addr, timeout.min(Duration::from_secs(10)))
                .map_err(|_| GatewayUnavailable::Unreachable(address).failure())?;
            let _ = stream.set_read_timeout(Some(timeout));
            let _ = stream.set_write_timeout(Some(timeout));
            write_and_read(stream, key, method, path, body, address)
        }
    }
}

pub(crate) fn admin<T: serde::de::DeserializeOwned>(
    address: &GatewayAddress,
    key: Option<&str>,
    value: &impl serde::Serialize,
) -> CliResult<T> {
    request(address, key, Endpoint::Admin, value)
}

pub(crate) fn request<T: serde::de::DeserializeOwned>(
    address: &GatewayAddress,
    key: Option<&str>,
    endpoint: Endpoint,
    request: &impl serde::Serialize,
) -> CliResult<T> {
    let body = call::encode(request).map_err(|error| error.to_string())?;
    let version = supported_version(&versions(address, key)?)?;
    let call = exchange(address, key, "POST", &endpoint.path(version), &body)?;
    Ok(decode_call(call)?)
}

pub(crate) fn supported_version(
    reply: &plakous::gateway::protocol::VersionsReply,
) -> Result<u16, String> {
    let served = &reply.versions;
    plakous::gateway::protocol::negotiate(SUPPORTED_VERSIONS, served)
        .ok_or_else(|| format!("credential gateway protocol mismatch: serves {served:?}"))
}

pub(crate) fn versions(
    address: &GatewayAddress,
    key: Option<&str>,
) -> CliResult<plakous::gateway::protocol::VersionsReply> {
    let call = exchange_with_timeout(
        address,
        key,
        "GET",
        VERSIONS_PATH,
        &[],
        Duration::from_secs(10),
    )?;
    Ok(decode_call(call)?)
}

pub(crate) fn decode_call<T: serde::de::DeserializeOwned>(call: Call) -> Result<T, String> {
    if let Some(code) = call.error.as_deref() {
        let detail = serde_json::from_slice::<RefusalBody>(&call.body)
            .map(|body| body.gateway_error.code().to_owned())
            .unwrap_or_else(|_| code.to_owned());
        return Err(format!("gateway refused the call ({detail})"));
    }
    if call.status != 200 {
        return Err(format!("gateway returned HTTP {}", call.status));
    }
    call::decode(&call.body).map_err(|error| error.to_string())
}

const MAX_RESPONSE_HEAD_BYTES: usize = 64 * 1024;
/// Bound on the raw HTTP response (head + call body + chunked framing overhead).
const MAX_RAW_RESPONSE_BYTES: usize = MAX_RESPONSE_HEAD_BYTES + call::MAX_CALL_BYTES + 16 * 1024;

fn write_and_read<S: Read + Write>(
    mut stream: S,
    key: Option<&str>,
    method: &str,
    path: &str,
    body: &[u8],
    address: &GatewayAddress,
) -> CliResult<Call> {
    let head = request_head(key, method, path, body.len())?;
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body))
        .map_err(|_| GatewayUnavailable::Unreachable(address).failure())?;
    let mut raw = Vec::new();
    let max_read = u64::try_from(MAX_RAW_RESPONSE_BYTES.saturating_add(1)).unwrap_or(u64::MAX);
    stream
        .take(max_read)
        .read_to_end(&mut raw)
        .map_err(|_| GatewayUnavailable::Unreachable(address).failure())?;
    if raw.len() > MAX_RAW_RESPONSE_BYTES {
        return Err("gateway response exceeds size limit".to_owned().into());
    }
    if raw.is_empty() {
        return Err(GatewayUnavailable::Unreachable(address).failure());
    }
    Ok(parse_response(&raw)?)
}

pub(crate) fn request_head(
    key: Option<&str>,
    method: &str,
    path: &str,
    length: usize,
) -> Result<zeroize::Zeroizing<String>, String> {
    let mut head = zeroize::Zeroizing::new(format!(
        "{method} {path} HTTP/1.1\r\nhost: plak-gateway\r\ncontent-length: {length}\r\nconnection: close\r\n",
    ));
    if let Some(key) = key {
        if key.bytes().any(|byte| matches!(byte, b'\r' | b'\n' | 0)) {
            return Err("caller key is not a header value".to_owned());
        }
        head.push_str(KEY_HEADER);
        head.push_str(": ");
        head.push_str(key);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    Ok(head)
}

pub(crate) fn parse_response(raw: &[u8]) -> Result<Call, String> {
    let split = raw
        .windows(4)
        .position(|mark| mark == b"\r\n\r\n")
        .ok_or_else(|| "gateway response has no body".to_owned())?;
    if split > MAX_RESPONSE_HEAD_BYTES {
        return Err("gateway response head exceeds size limit".to_owned());
    }
    let head = std::str::from_utf8(&raw[..split])
        .map_err(|_| "gateway response head is not utf-8".to_owned())?;
    let rest = &raw[split + 4..];
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| "gateway response has no status".to_owned())?;
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|status| status.parse().ok())
        .ok_or_else(|| "gateway status is malformed".to_owned())?;
    let mut error = None;
    let mut chunked = false;
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case(plakous::gateway::protocol::ERROR_HEADER) {
            error = Some(value.trim().to_owned());
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().ok();
        }
    }
    let body = if chunked {
        decode_chunks(rest)?
    } else if let Some(length) = content_length {
        if length > call::MAX_CALL_BYTES {
            return Err("gateway response body exceeds size limit".to_owned());
        }
        rest.get(..length)
            .ok_or_else(|| "gateway body is shorter than content-length".to_owned())?
            .to_vec()
    } else {
        if rest.len() > call::MAX_CALL_BYTES {
            return Err("gateway response body exceeds size limit".to_owned());
        }
        rest.to_vec()
    };
    Ok(Call {
        status,
        error,
        body,
    })
}

/// HTTP/1.1 chunked body. The relay server does not set a content length.
fn decode_chunks(mut rest: &[u8]) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let line_end = rest
            .windows(2)
            .position(|mark| mark == b"\r\n")
            .ok_or_else(|| "chunk size line is truncated".to_owned())?;
        let line = std::str::from_utf8(&rest[..line_end])
            .map_err(|_| "chunk size line is not utf-8".to_owned())?;
        let size_text = line.split(';').next().unwrap_or(line).trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| "chunk size is malformed".to_owned())?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(body);
        }
        let Some(chunk_end) = size.checked_add(2) else {
            return Err("chunk size overflows".to_owned());
        };
        if body
            .len()
            .checked_add(size)
            .is_none_or(|total| total > call::MAX_CALL_BYTES)
        {
            return Err("gateway response body exceeds size limit".to_owned());
        }
        if rest.len() < chunk_end || &rest[size..chunk_end] != b"\r\n" {
            return Err("chunk is truncated".to_owned());
        }
        body.extend_from_slice(&rest[..size]);
        rest = &rest[chunk_end..];
    }
}
