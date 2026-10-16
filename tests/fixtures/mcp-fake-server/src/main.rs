//! `mcp-fake-server [--style initialize|discover]
//! [--tools standard|hundred|unlowerable] [--expect-env NAME=VALUE]
//! [--call-log PATH] [--list-log PATH] [--http]`
//! serves the scripted MCP server over stdio; the `exit` tool exits with
//! status 3 mid-call. `unlowerable` adds `tangled`, whose `allOf` schema no
//! portable schema can carry. `--expect-env` exits with status 4 before
//! serving unless the environment holds exactly that value. With `--http` it
//! serves streamable HTTP on a loopback port instead, prints
//! `listening on <url>` on stdout, and requires
//! `Authorization: Bearer $MCP_FAKE_BEARER` when that variable is set.
//! `--call-log` appends each called tool's name to PATH before it runs;
//! `--list-log` appends one `list` line to PATH per `tools/list` request.

#![forbid(unsafe_code)]

use std::sync::Arc;

use plakous_mcp_fake_server::http::{HttpGate, serve_http};
use plakous_mcp_fake_server::{FakeServer, Style, tool};
use rmcp::ServiceExt as _;
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut style = Style::Initialize;
    let mut hundred = false;
    let mut http = false;
    let mut unlowerable = false;
    let mut call_log = None;
    let mut list_log = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--http" {
            http = true;
            continue;
        }
        match (argument.as_str(), arguments.next().as_deref()) {
            ("--style", Some("initialize")) => style = Style::Initialize,
            ("--style", Some("discover")) => style = Style::Discover,
            ("--tools", Some("standard")) => hundred = false,
            ("--tools", Some("hundred")) => hundred = true,
            ("--tools", Some("unlowerable")) => unlowerable = true,
            ("--call-log", Some(path)) => call_log = Some(std::path::PathBuf::from(path)),
            ("--list-log", Some(path)) => list_log = Some(std::path::PathBuf::from(path)),
            ("--expect-env", Some(expected)) => {
                let (name, value) = expected
                    .split_once('=')
                    .ok_or("--expect-env takes NAME=VALUE")?;
                if std::env::var(name).ok().as_deref() != Some(value) {
                    std::process::exit(4);
                }
            }
            _ => return Err(format!("unknown argument {argument}").into()),
        }
    }
    let mut server = FakeServer::new(style, Arc::new(|| std::process::exit(3)));
    if hundred {
        server = server.with_tools(
            (0..100)
                .map(|index| tool(&format!("t{index}"), "numbered", json!({"type": "object"})))
                .collect(),
        );
    }
    if unlowerable {
        let mut tools = plakous_mcp_fake_server::standard_tools();
        tools.push(tool(
            "tangled",
            "Has a schema no portable schema carries.",
            json!({"allOf": [{"type": "object"}, {"type": "object"}]}),
        ));
        server = server.with_tools(tools);
    }
    if let Some(path) = call_log {
        server = server.with_call_log(path);
    }
    if let Some(path) = list_log {
        server = server.with_list_log(path);
    }
    if http {
        let gate = HttpGate {
            bearer: std::env::var("MCP_FAKE_BEARER").ok(),
            header: None,
        };
        let fixture = serve_http(server, gate).await?;
        println!("listening on {}", fixture.url());
        std::future::pending::<()>().await;
        return Ok(());
    }
    eprintln!("mcp-fake-server ready");
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
