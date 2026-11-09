//! [`Connector`] backed by the `yah-mcp` client: how plugin `mcpServers` become live tools.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use yah_mcp::{CallResult, Content, HttpConfig, McpClient, McpTool, StdioConfig};
use yah_sandbox::{Policy, Sandbox};
use yah_tools::ToolContext;

use crate::connectors::{Connector, ConnectorContent, ConnectorResult, ConnectorTool};
use crate::manifest::McpServerConfig;

/// Default deadline for one `tools/call`.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);

pub struct McpConnector {
    client: McpClient,
    call_timeout: Duration,
}

impl McpConnector {
    /// Start (stdio) or open a session with (URL) the server. `cwd` is the working folder
    /// for stdio servers, normally the plugin's folder.
    pub fn connect(config: &McpServerConfig, cwd: Option<&Path>) -> Result<Self, String> {
        Self::connect_in(config, cwd, None)
    }

    /// [`connect`](Self::connect), starting stdio servers through `sandbox` when given.
    pub fn connect_in(config: &McpServerConfig, cwd: Option<&Path>, sandbox: Option<&McpSandbox>) -> Result<Self, String> {
        let client = match config {
            McpServerConfig::Stdio { command, args, env } => {
                let mut c = StdioConfig::new(command.clone());
                c.args.clone_from(args);
                c.env.clone_from(env);
                c.cwd = cwd.map(Path::to_path_buf);
                if let Some(sb) = sandbox {
                    sb.wrap(&mut c);
                }
                McpClient::connect_stdio(&c)
            }
            McpServerConfig::Url { sse: true, url, .. } => {
                return Err(format!("{url}: the legacy HTTP+SSE MCP transport isn't supported yet"));
            }
            McpServerConfig::Url { url, headers, .. } => {
                let mut c = HttpConfig::new(url.clone());
                c.headers = headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                McpClient::connect_http(&c)
            }
        }
        .map_err(|e| e.to_string())?;
        Ok(Self { client, call_timeout: CALL_TIMEOUT })
    }

    pub fn client(&self) -> &McpClient {
        &self.client
    }
}

/// How local (stdio) MCP servers are confined: they keep network access and may read
/// anything, but may only write to the workspace, their own folder (the `cwd`), the temp
/// folder, package-manager caches (`~/.npm`, `~/.cache`, `~/.local/share/uv`) and folders
/// named in their arguments (e.g. the filesystem server's roots). The app's API key is
/// removed from their environment.
#[derive(Debug, Clone)]
pub struct McpSandbox {
    pub sandbox: Sandbox,
    pub workspace: PathBuf,
}

impl McpSandbox {
    pub fn policy(&self, cfg: &StdioConfig) -> Policy {
        let mut p = Policy { read: vec![PathBuf::from("/")], ..Default::default() }
            .network(true)
            .allow_unsandboxed(true)
            .remove_env("YAH_API_KEY")
            .allow_write(&self.workspace)
            .allow_write(std::env::temp_dir());
        p.write.extend(yah_sandbox::system_write_paths());
        if let Some(dir) = &cfg.cwd {
            p = p.allow_write(dir);
        }
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            for d in [".npm", ".cache", ".local/share/uv"] {
                p = p.allow_write(home.join(d));
            }
        }
        let home = std::env::var_os("HOME").and_then(|h| PathBuf::from(h).canonicalize().ok());
        for a in &cfg.args {
            let Ok(path) = Path::new(a).canonicalize() else { continue };
            // never "/" or the whole home folder (or anything above it)
            if Path::new(a).is_absolute() && path.is_dir() && path.parent().is_some() && !home.as_ref().is_some_and(|h| h.starts_with(&path)) {
                p = p.allow_write(path);
            }
        }
        p
    }

    /// Rewrite `cfg` to start the server through the sandbox helper. Leaves it unchanged
    /// (logged) if the helper path isn't UTF-8.
    pub fn wrap(&self, cfg: &mut StdioConfig) {
        let policy = self.policy(cfg);
        let argv: Option<Vec<String>> = self.sandbox.argv(&policy, &cfg.command, &cfg.args).into_iter().map(|a| a.into_string().ok()).collect();
        match (self.sandbox.helper().to_str(), argv) {
            (Some(helper), Some(argv)) => {
                cfg.command = helper.to_owned();
                cfg.args = argv;
            }
            _ => log::warn!("MCP server {}: not sandboxed (the app's path isn't UTF-8)", cfg.command),
        }
    }
}

impl Connector for McpConnector {
    fn list_tools(&self) -> Result<Vec<ConnectorTool>, String> {
        Ok(self.client.list_tools().map_err(|e| e.to_string())?.into_iter().map(tool).collect())
    }

    fn call_tool(&self, name: &str, args: &Value, ctx: &ToolContext<'_>) -> Result<ConnectorResult, String> {
        let result = self.client.call_tool_cancellable(name, args.clone(), self.call_timeout, ctx.cancel_flag()).map_err(|e| e.to_string())?;
        Ok(result_of(result))
    }
}

fn tool(t: McpTool) -> ConnectorTool {
    ConnectorTool {
        title: t.annotations.title.or(t.title),
        description: Some(t.description).filter(|d| !d.is_empty()),
        input_schema: t.input_schema,
        read_only_hint: t.annotations.read_only_hint,
        destructive_hint: t.annotations.destructive_hint,
        name: t.name,
    }
}

fn result_of(r: CallResult) -> ConnectorResult {
    let content = r
        .content
        .into_iter()
        .map(|c| match c {
            Content::Text(t) => ConnectorContent::Text(t),
            Content::Image { mime_type, data } => ConnectorContent::Image { data, mime_type },
            Content::Resource { uri, text, .. } => ConnectorContent::Resource { uri, text },
            Content::Audio { .. } => ConnectorContent::Other("audio".into()),
            Content::ResourceLink { uri, .. } => ConnectorContent::Other(format!("resource link {uri}")),
            Content::Other { kind } => ConnectorContent::Other(kind),
        })
        .collect::<Vec<_>>();
    // A server that only sent structured output (or nothing) still gets its text seen.
    let content = if content.is_empty() { vec![ConnectorContent::Text(r.text)] } else { content };
    ConnectorResult { content, is_error: r.is_error }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yah_mcp::ToolAnnotations;

    #[test]
    fn maps_tool_metadata() {
        let t = McpTool {
            name: "search".into(),
            title: Some("Search".into()),
            description: String::new(),
            input_schema: serde_json::json!({"type":"object"}),
            annotations: ToolAnnotations { read_only_hint: Some(true), ..Default::default() },
        };
        let c = tool(t);
        assert_eq!((c.name.as_str(), c.title.as_deref(), c.description, c.read_only_hint), ("search", Some("Search"), None, Some(true)));
    }

    #[test]
    fn empty_content_falls_back_to_text() {
        let r = CallResult { text: "(no result)".into(), is_error: true, content: Vec::new(), structured: None };
        assert_eq!(result_of(r), ConnectorResult { content: vec![ConnectorContent::Text("(no result)".into())], is_error: true });
    }

    #[test]
    fn sandbox_wraps_stdio_servers() {
        let ws = std::env::temp_dir();
        let sb = McpSandbox { sandbox: Sandbox::new("/opt/yah"), workspace: ws.clone() };
        let mut cfg = StdioConfig::new("npx").args(["-y", "server", "/"]).cwd("/plugins/x");
        let policy = sb.policy(&cfg);
        assert!(policy.network && policy.remove_env.contains(&"YAH_API_KEY".to_owned()));
        assert!(policy.write.contains(&ws.canonicalize().unwrap()) && policy.write.contains(&PathBuf::from("/plugins/x")));
        assert!(!policy.write.contains(&PathBuf::from("/")), "an argument of / must not open the whole disk");
        sb.wrap(&mut cfg);
        assert_eq!(cfg.command, "/opt/yah");
        assert_eq!(cfg.args[0], yah_sandbox::HELPER_ARG);
        assert_eq!(&cfg.args[2..], ["--", "npx", "-y", "server", "/"]);
    }

    #[test]
    fn legacy_sse_is_refused_cleanly() {
        let cfg = McpServerConfig::Url { url: "http://x/sse".into(), sse: true, headers: Default::default(), oauth: None };
        assert!(McpConnector::connect(&cfg, None).is_err());
    }
}
