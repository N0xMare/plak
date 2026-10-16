//! `mcp-fake-server [--style initialize|discover] [--tools standard|hundred]`
//! serves the scripted MCP server over stdio; the `exit` tool exits with
//! status 3 mid-call.

#![forbid(unsafe_code)]

use std::sync::Arc;

use plakous_mcp_fake_server::{FakeServer, Style, tool};
use rmcp::ServiceExt as _;
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut style = Style::Initialize;
    let mut hundred = false;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match (argument.as_str(), arguments.next().as_deref()) {
            ("--style", Some("initialize")) => style = Style::Initialize,
            ("--style", Some("discover")) => style = Style::Discover,
            ("--tools", Some("standard")) => hundred = false,
            ("--tools", Some("hundred")) => hundred = true,
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
    eprintln!("mcp-fake-server ready");
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
