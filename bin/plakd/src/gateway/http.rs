//! One HTTP/1.1 exchange on a connection the gateway already accepted.
//! The relay server core owns the long-term codec; this reader exists so the
//! listeners can authenticate and serve admin, facts and refresh without a
//! second HTTP stack.

use std::collections::BTreeMap;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

const MAX_HEAD_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

pub(crate) struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub(crate) fn json(status: u16, body: Vec<u8>, error: Option<&str>) -> Self {
        let mut headers = vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("content-length".to_owned(), body.len().to_string()),
            ("connection".to_owned(), "close".to_owned()),
        ];
        if let Some(code) = error {
            headers.push((
                plakous::gateway::protocol::ERROR_HEADER.to_owned(),
                code.to_owned(),
            ));
        }
        Self {
            status,
            headers,
            body,
        }
    }
}

pub(crate) async fn read_request<S>(stream: &mut S, max_body: u64) -> std::io::Result<Request>
where
    S: AsyncRead + Unpin,
{
    let head = read_head(stream).await?;
    let text = std::str::from_utf8(&head)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let (start, header_text) = text
        .split_once("\r\n")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "request line"))?;
    let mut parts = start.split(' ');
    let method = parts
        .next()
        .filter(|method| !method.is_empty())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "method"))?
        .to_owned();
    let target = parts
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "path"))?;
    let path = target
        .split_once('?')
        .map_or(target, |(path, _)| path)
        .to_owned();
    let mut headers = BTreeMap::new();
    for line in header_text.split("\r\n") {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "header",
            ));
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = match headers.get("content-length") {
        Some(value) => value
            .parse::<u64>()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "content-length"))?,
        None => 0,
    };
    if length > max_body {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "body limit",
        ));
    }
    let mut body = vec![0_u8; usize::try_from(length).unwrap_or(usize::MAX)];
    if !body.is_empty() {
        stream.read_exact(&mut body).await?;
    }
    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

async fn read_head<S>(stream: &mut S) -> std::io::Result<Vec<u8>>
where
    S: AsyncRead + Unpin,
{
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while head.len() < MAX_HEAD_BYTES {
        let read = stream.read(&mut byte).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "request head",
            ));
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            head.truncate(head.len() - 2);
            return Ok(head);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "request head is too large",
    ))
}

pub(crate) async fn write_response<S>(stream: &mut S, response: &Response) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let reason = reason(response.status);
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", response.status);
    for (name, value) in &response.headers {
        if value.bytes().any(|byte| matches!(byte, b'\r' | b'\n' | 0)) {
            continue;
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&response.body).await?;
    stream.flush().await
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        426 => "Upgrade Required",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    }
}
