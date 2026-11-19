//! A blocking gateway client for the CLI. One HTTP/1.1 exchange, then close.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use plakous::gateway::protocol::call;
use plakous::gateway::protocol::refusal::RefusalBody;
use plakous::gateway::protocol::{Endpoint, KEY_HEADER, SUPPORTED_VERSIONS};

use super::address::GatewayAddress;

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
) -> Result<Call, String> {
    match address {
        GatewayAddress::Unix(socket) => {
            let stream = UnixStream::connect(socket).map_err(|error| error.to_string())?;
            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
            write_and_read(stream, key, method, path, body)
        }
        GatewayAddress::Tcp(addr) => {
            let stream = TcpStream::connect_timeout(addr, Duration::from_secs(10))
                .map_err(|error| error.to_string())?;
            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
            write_and_read(stream, key, method, path, body)
        }
    }
}

pub(crate) fn admin<T: serde::de::DeserializeOwned>(
    address: &GatewayAddress,
    key: Option<&str>,
    value: &impl serde::Serialize,
) -> Result<T, String> {
    request(address, key, Endpoint::Admin, value)
}

pub(crate) fn request<T: serde::de::DeserializeOwned>(
    address: &GatewayAddress,
    key: Option<&str>,
    endpoint: Endpoint,
    request: &impl serde::Serialize,
) -> Result<T, String> {
    let body = call::encode(request).map_err(|error| error.to_string())?;
    let served = versions(address, key)?.versions;
    let version = plakous::gateway::protocol::negotiate(SUPPORTED_VERSIONS, &served)
        .ok_or_else(|| format!("credential gateway protocol mismatch: serves {served:?}"))?;
    let call = exchange(address, key, "POST", &endpoint.path(version), &body)?;
    decode_call(call)
}

pub(crate) fn versions(
    address: &GatewayAddress,
    key: Option<&str>,
) -> Result<plakous::gateway::protocol::VersionsReply, String> {
    let call = exchange(
        address,
        key,
        "GET",
        plakous::gateway::protocol::VERSIONS_PATH,
        &[],
    )?;
    decode_call(call)
}

fn decode_call<T: serde::de::DeserializeOwned>(call: Call) -> Result<T, String> {
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

fn write_and_read<S: Read + Write>(
    mut stream: S,
    key: Option<&str>,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<Call, String> {
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nhost: plak-gateway\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
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
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body))
        .map_err(|error| error.to_string())?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|error| error.to_string())?;
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<Call, String> {
    let split = raw
        .windows(4)
        .position(|mark| mark == b"\r\n\r\n")
        .ok_or_else(|| "gateway response has no body".to_owned())?;
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
        rest.get(..length)
            .ok_or_else(|| "gateway body is shorter than content-length".to_owned())?
            .to_vec()
    } else {
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
        if rest.len() < size + 2 || &rest[size..size + 2] != b"\r\n" {
            return Err("chunk is truncated".to_owned());
        }
        body.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}
