//! Connect to an MCP server, list its tools, and optionally call one.
//!
//! ```text
//! cargo run -p yah-mcp --example mcp_probe -- http://127.0.0.1:8000/mcp [TOOL [JSON_ARGS]]
//! cargo run -p yah-mcp --example mcp_probe -- --stdio "python3 server.py" [TOOL [JSON_ARGS]]
//! ```

use std::time::Duration;

use yah_mcp::{HttpConfig, McpClient, StdioConfig};

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let target = args.next().ok_or("usage: mcp_probe URL | --stdio \"CMD ARGS\" [TOOL [JSON]]")?;
    let client = if target == "--stdio" {
        let line = args.next().ok_or("--stdio needs a command")?;
        let mut words = line.split_whitespace();
        let command = words.next().ok_or("empty command")?;
        McpClient::connect_stdio(&StdioConfig::new(command).args(words))
    } else {
        McpClient::connect_http(&HttpConfig::new(target))
    }
    .map_err(|e| e.to_string())?;
    println!("server: {:?}", client.server_info());
    for tool in client.list_tools().map_err(|e| e.to_string())? {
        println!("tool {}: {} {}", tool.name, tool.description, tool.input_schema);
    }
    if let Some(tool) = args.next() {
        let json = args.next().unwrap_or_else(|| "{}".into());
        let input = serde_json::from_str(&json).map_err(|e| e.to_string())?;
        match client.call_tool(&tool, input, Duration::from_secs(30)) {
            Ok(out) => println!("call {tool}: is_error={} text={:?}", out.is_error, out.text),
            Err(e) => println!("call {tool} failed: {e}"),
        }
    }
    client.close();
    Ok(())
}
