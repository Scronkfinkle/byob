//! Connectors: MCP servers a plugin declares (or the user pastes), adapted into
//! [`byob_tools::Tool`]s.
//!
//! The MCP client itself lives in `byob-mcp`; this module only needs the small
//! [`Connector`] trait, so the adapter is testable with a fake and the client can be
//! swapped. [`ConnectorTool::from_mcp`] and [`ConnectorResult::from_mcp`] turn MCP's
//! `tools/list` entries and `tools/call` results (raw JSON) into these types.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use byob_tools::{Access, FileChange, Tool, ToolContext, ToolOutput, ToolSpec};
use serde_json::{Map, Value, json};

use crate::PluginRef;
use crate::manifest::{McpServerConfig, ParseOptions, parse_manifest, parse_server, plugin_id_ok, setting_key_ok};
use crate::names::{ToolNames, namespaced};
use crate::skills::slug;

/// Most tools taken from one server (a huge server would drown the model's context).
pub const MAX_TOOLS_PER_SERVER: usize = 128;
/// Most text returned to the model from one call.
pub const MAX_RESULT_CHARS: usize = 100_000;
/// Biggest image saved from one result.
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_DESCRIPTION_CHARS: usize = 1000;

/// A running MCP server, as far as tools are concerned. Implemented by `byob-mcp`'s client.
pub trait Connector: Send + Sync {
    /// The server's tools (`tools/list`, all pages).
    fn list_tools(&self) -> Result<Vec<ConnectorTool>, String>;
    /// Call one tool (`tools/call`). Long calls should poll `ctx.cancelled()`.
    /// `Err` is a transport/protocol failure; a tool-level failure is
    /// `Ok(ConnectorResult { is_error: true, .. })`.
    fn call_tool(&self, name: &str, args: &Value, ctx: &ToolContext<'_>) -> Result<ConnectorResult, String>;
}

/// One tool of an MCP server.
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectorTool {
    pub name: String,
    /// `annotations.title`, else `title`.
    pub title: Option<String>,
    pub description: Option<String>,
    /// JSON Schema for the arguments (`inputSchema`).
    pub input_schema: Value,
    pub read_only_hint: Option<bool>,
    pub destructive_hint: Option<bool>,
}

impl ConnectorTool {
    /// From one entry of MCP `tools/list` → `tools`. `None` without a usable name.
    pub fn from_mcp(v: &Value) -> Option<Self> {
        let name = v.get("name")?.as_str()?.trim();
        if name.is_empty() {
            return None;
        }
        let ann = v.get("annotations");
        let text = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string).filter(|s| !s.trim().is_empty());
        Some(Self {
            name: name.to_string(),
            title: text(ann.and_then(|a| a.get("title"))).or_else(|| text(v.get("title"))),
            description: text(v.get("description")),
            input_schema: v.get("inputSchema").cloned().unwrap_or(Value::Null),
            read_only_hint: ann.and_then(|a| a.get("readOnlyHint")).and_then(Value::as_bool),
            destructive_hint: ann.and_then(|a| a.get("destructiveHint")).and_then(Value::as_bool),
        })
    }

    /// What people see: the title, else the name.
    pub fn label(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.name)
    }

    /// Only reads: says so (`readOnlyHint`), or gives no hints and its name only reads
    /// (`get_issue`, `searchDocs`). MCP's default for a tool without hints is "may change
    /// things".
    pub fn read_only(&self) -> bool {
        self.read_only_hint == Some(true) || (self.read_only_hint.is_none() && self.destructive_hint.is_none() && name_reads_only(&self.name))
    }

    /// Should the user approve each call? Hob's rule: everything that isn't read-only,
    /// unless it says it isn't destructive and nothing untrusted entered the task.
    pub fn needs_approval(&self, untrusted: bool) -> bool {
        !self.read_only() && (self.destructive_hint != Some(false) || untrusted)
    }
}

const READ_VERBS: [&str; 13] = ["get", "list", "read", "search", "find", "fetch", "lookup", "describe", "ask", "show", "view", "count", "browse"];
const CHANGE_WORDS: &[&str] = &[
    "create", "add", "update", "set", "delete", "remove", "send", "post", "write", "edit", "move", "reset", "clear", "run", "exec", "archive", "cancel",
    "close", "merge", "publish", "upload", "replace", "drop", "kill", "purge", "apply", "insert", "modify", "rename", "revoke", "grant", "pay", "buy", "order",
    "book", "transfer",
];

/// `get_issue`, `listFiles`, `search` read; `get_or_create`, `query` don't count.
/// ("query" isn't a read verb: database servers run any SQL through it.)
pub fn name_reads_only(name: &str) -> bool {
    let Some(rest) = READ_VERBS.iter().find_map(|v| {
        let rest = name.strip_prefix(v)?;
        let next = rest.chars().next();
        (next.is_none() || next.is_some_and(|c| matches!(c, '_' | '-' | '.') || c.is_ascii_uppercase())).then_some(rest)
    }) else {
        return false;
    };
    // a change word anywhere not glued to a digit (hob: `(?:^|[_\-.]|[a-z])word`, /i)
    let lower = rest.to_ascii_lowercase();
    !CHANGE_WORDS.iter().any(|w| {
        lower.match_indices(w).any(|(i, _)| i == 0 || lower.as_bytes().get(i - 1).is_some_and(|&c| matches!(c, b'_' | b'-' | b'.') || c.is_ascii_alphabetic()))
    })
}

/// One piece of a tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectorContent {
    Text(String),
    /// Base64 image data.
    Image {
        data: String,
        mime_type: String,
    },
    Resource {
        uri: String,
        text: Option<String>,
    },
    /// Something we don't show (audio, resource links…): its type.
    Other(String),
}

/// A `tools/call` result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectorResult {
    pub content: Vec<ConnectorContent>,
    pub is_error: bool,
}

impl ConnectorResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self { content: vec![ConnectorContent::Text(text.into())], is_error: false }
    }

    /// From an MCP `CallToolResult` JSON object. Unknown shapes are tolerated.
    pub fn from_mcp(v: &Value) -> Self {
        let s = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);
        let content = v
            .get("content")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|c| match c.get("type").and_then(Value::as_str) {
                        Some("text") => ConnectorContent::Text(s(c.get("text")).unwrap_or_default()),
                        Some("image") => {
                            ConnectorContent::Image { data: s(c.get("data")).unwrap_or_default(), mime_type: s(c.get("mimeType")).unwrap_or_default() }
                        }
                        Some("resource") => {
                            let r = c.get("resource");
                            ConnectorContent::Resource { uri: s(r.and_then(|r| r.get("uri"))).unwrap_or_default(), text: s(r.and_then(|r| r.get("text"))) }
                        }
                        other => ConnectorContent::Other(other.unwrap_or("unknown").to_string()),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self { content, is_error: v.get("isError").and_then(Value::as_bool).unwrap_or(false) }
    }
}

/// One MCP server tool as a byob tool.
pub struct ConnectorToolAdapter {
    name: String,
    plugin: PluginRef,
    /// The server is a local program (stdio), not a URL.
    local: bool,
    tool: ConnectorTool,
    connector: Arc<dyn Connector>,
}

impl ConnectorToolAdapter {
    pub fn new(name: String, plugin: PluginRef, local: bool, tool: ConnectorTool, connector: Arc<dyn Connector>) -> Self {
        Self { name, plugin, local, tool, connector }
    }

    /// The MCP tool behind this one (hints for the approval gate: [`ConnectorTool::needs_approval`]).
    pub fn connector_tool(&self) -> &ConnectorTool {
        &self.tool
    }

    pub fn plugin(&self) -> &PluginRef {
        &self.plugin
    }
}

impl Tool for ConnectorToolAdapter {
    fn spec(&self) -> ToolSpec {
        let schema = match &self.tool.input_schema {
            Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("object") => Value::Object(o.clone()),
            _ => json!({"type": "object", "properties": {}}),
        };
        let description = self.tool.description.as_deref().unwrap_or(self.tool.label()).chars().take(MAX_DESCRIPTION_CHARS).collect();
        ToolSpec { name: self.name.clone(), description, parameters: schema }
    }

    fn title(&self, _args: &Value) -> String {
        format!("Used {}: {}", self.plugin.name, self.tool.label())
    }

    /// A local server is unsandboxed code on this computer (Execute); an online one only
    /// reaches the network. Read-only hints are on [`Self::connector_tool`] for a finer gate.
    fn access(&self, _args: &Value) -> Access {
        if self.local { Access::Execute } else { Access::Network }
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        if ctx.cancelled() {
            return ToolOutput::err("Stopped by the user.");
        }
        let args = if args.is_object() { args.clone() } else { Value::Object(Map::new()) };
        match self.connector.call_tool(&self.tool.name, &args, ctx) {
            Ok(r) => output(&r, ctx.workspace, &slug(&self.plugin.id)),
            Err(e) => ToolOutput::err(format!("{} couldn't do that: {e}", self.plugin.name)),
        }
    }
}

/// MCP result → text for the model; images become files in `dir`.
fn output(r: &ConnectorResult, dir: &Path, label: &str) -> ToolOutput {
    let mut texts = Vec::new();
    let mut changes = Vec::new();
    for c in &r.content {
        match c {
            ConnectorContent::Text(t) => texts.push(t.clone()),
            ConnectorContent::Image { data, mime_type } => match save_image(dir, label, changes.len() + 1, data, mime_type) {
                Ok(path) => {
                    let shown = byob_tools::sandbox::slash(path.strip_prefix(dir).unwrap_or(&path));
                    texts.push(format!("[image saved as {shown}]"));
                    changes.push(FileChange::Created(path));
                }
                Err(e) => texts.push(format!("[image not saved: {e}]")),
            },
            ConnectorContent::Resource { uri, text } => texts.push(text.clone().unwrap_or_else(|| format!("[resource {uri}]"))),
            ConnectorContent::Other(_) => {}
        }
    }
    let mut text = texts.join("\n");
    if text.is_empty() {
        text = "(no result)".into();
    }
    if text.chars().count() > MAX_RESULT_CHARS {
        text = text.chars().take(MAX_RESULT_CHARS).collect::<String>() + "\n…(cut: the result was too long)";
    }
    ToolOutput { text, failed: r.is_error, changes }
}

fn save_image(dir: &Path, label: &str, n: usize, data: &str, mime: &str) -> Result<PathBuf, String> {
    let ext = match mime {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    };
    if data.len() / 4 * 3 > MAX_IMAGE_BYTES {
        return Err("too big".into());
    }
    let bytes = base64_decode(data).ok_or("not valid base64")?;
    let label = if label.is_empty() { "image" } else { label };
    let mut i = 0u32;
    let path = loop {
        let suffix = if i == 0 { String::new() } else { format!("-{i}") };
        let p = dir.join(format!("{label}-{n}{suffix}.{ext}"));
        if !p.exists() {
            break p;
        }
        i = i.checked_add(1).ok_or("no free file name")?;
    };
    fs::write(&path, bytes).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Standard or URL-safe base64, padding and whitespace optional.
pub(crate) fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return None,
        };
        buf = (buf << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// List a server's tools and adapt them, named `<plugin>__<tool>` (made unique through
/// `names`). `local`: the server is a program on this computer
/// ([`McpServerConfig::is_local`]).
pub fn connector_tools(
    plugin: &PluginRef,
    local: bool,
    connector: Arc<dyn Connector>,
    names: &mut ToolNames,
) -> Result<Vec<Arc<ConnectorToolAdapter>>, String> {
    let listed = connector.list_tools()?;
    if listed.len() > MAX_TOOLS_PER_SERVER {
        log::warn!("plugin {}: server offers {} tools; using the first {MAX_TOOLS_PER_SERVER}", plugin.id, listed.len());
    }
    Ok(listed
        .into_iter()
        .take(MAX_TOOLS_PER_SERVER)
        .map(|t| {
            let preferred = namespaced(&plugin.id, &t.name);
            let name = names.claim(&preferred, &preferred);
            Arc::new(ConnectorToolAdapter::new(name, plugin.clone(), local, t, Arc::clone(&connector)))
        })
        .collect())
}

/// A connector the user added by pasting a link, a command line, or a Claude / Cursor /
/// VS Code style `{"mcpServers": …}` config. Turn it into a plugin with [`Self::manifest_json`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectorDraft {
    /// Plugin id (a slug of the name).
    pub id: String,
    pub name: String,
    pub servers: BTreeMap<String, McpServerConfig>,
    /// Keys and values typed alongside (headers, env); store them as secret settings,
    /// never in the plugin folder.
    pub secrets: BTreeMap<String, String>,
    /// Labels for `secrets` keys that came from headers.
    pub labels: BTreeMap<String, String>,
}

/// `"https://mcp.linear.app/mcp"` → `"Linear"`.
pub fn name_from_url(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host_port = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = host_port.rsplit_once('@').map_or(host_port, |(_, h)| h);
    let host = if host_port.starts_with('[') { host_port } else { host_port.split(':').next().unwrap_or("") };
    if host.is_empty() {
        return "App".into();
    }
    if host == "localhost" || host.contains(':') || host.starts_with('[') || host.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return "Local app".into();
    }
    let parts: Vec<&str> = host.split('.').filter(|p| !matches!(*p, "www" | "mcp" | "api" | "app" | "server" | "remote")).collect();
    let n = if parts.len() > 1 { parts[parts.len() - 2] } else { parts.first().copied().unwrap_or("App") };
    let mut c = n.chars();
    c.next().map_or_else(|| "App".into(), |f| f.to_uppercase().chain(c).collect())
}

/// `npx -y '@a/b' My\ Folder` → `["npx", "-y", "@a/b", "My Folder"]`.
pub fn parse_command_line(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' && chars.peek().is_some() {
                cur.extend(chars.next());
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                has = true;
            }
            '\\' if chars.peek().is_some() => {
                cur.extend(chars.next());
                has = true;
            }
            c if c.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                has = false;
            }
            c => cur.push(c),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Parse what the user pasted into "Add connector".
/// `env`: `KEY=value` lines for commands, `Header: value` lines for links.
pub fn parse_connector(name: &str, input: &str, env: &str) -> Result<ConnectorDraft, String> {
    let text = input.trim();
    let mut name = name.trim().to_string();
    let lines: Vec<&str> = env.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let mut secrets = BTreeMap::new();
    let mut labels = BTreeMap::new();
    let lower = text.to_ascii_lowercase();
    let servers = if lower.starts_with("http://") || lower.starts_with("https://") {
        if name.is_empty() {
            name = name_from_url(text);
        }
        let mut headers = BTreeMap::new();
        for l in &lines {
            let Some(i) = l.find([':', '=']).filter(|&i| i > 0) else { continue };
            let header = l[..i].trim();
            let key: String =
                format!("HEADER_{}", header.to_ascii_uppercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect::<String>())
                    .chars()
                    .take(64)
                    .collect();
            headers.insert(header.to_string(), format!("${{{key}}}"));
            secrets.insert(key.clone(), l[i + 1..].trim().to_string());
            labels.insert(key, header.to_string());
        }
        let raw = json!({"url": text, "headers": headers});
        BTreeMap::from([(server_key(&name), parse_server(&raw, "link").map_err(|_| "That link doesn't look right.".to_string())?)])
    } else if text.starts_with('{') {
        let j: Value = serde_json::from_str(text).map_err(|_| "That config isn't valid JSON.".to_string())?;
        let map = j.get("mcpServers").or_else(|| j.get("servers")).unwrap_or(&j);
        let map = map.as_object().ok_or("No servers found in that config.")?;
        let servers = map.iter().map(|(k, v)| Ok((k.clone(), parse_server(v, &format!("mcpServers.{k}"))?))).collect::<Result<BTreeMap<_, _>, String>>()?;
        if name.is_empty() {
            name = map.keys().next().cloned().ok_or("No servers found in that config.")?;
        }
        servers
    } else {
        let mut words = parse_command_line(text);
        if words.is_empty() {
            return Err("Enter the command that starts the connector.".into());
        }
        let command = words.remove(0);
        for l in &lines {
            if let Some((k, v)) = l.split_once('=').filter(|(k, _)| !k.trim().is_empty()) {
                secrets.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        if name.is_empty() {
            let last = words.last().unwrap_or(&command);
            name = Path::new(last).file_name().map_or_else(|| last.clone(), |n| n.to_string_lossy().into_owned());
        }
        BTreeMap::from([(server_key(&name), McpServerConfig::Stdio { command, args: words, env: BTreeMap::new() })])
    };
    if servers.is_empty() {
        return Err("No servers found in that config.".into());
    }
    if let Some(k) = secrets.keys().find(|k| !setting_key_ok(k)) {
        return Err(format!("“{k}” must be UPPER_CASE, like API_KEY."));
    }
    let id = slug(&name);
    if !plugin_id_ok(&id) {
        return Err("Give the connection a name with letters or numbers.".into());
    }
    Ok(ConnectorDraft { id, name, servers, secrets, labels })
}

fn server_key(name: &str) -> String {
    let s = slug(name);
    if s.is_empty() { "app".into() } else { s }
}

/// An `mcpServers` entry in hob's JSON form.
pub fn server_json(c: &McpServerConfig) -> Value {
    match c {
        McpServerConfig::Stdio { command, args, env } => {
            let mut o = json!({"command": command, "args": args});
            if !env.is_empty() {
                o["env"] = json!(env);
            }
            o
        }
        McpServerConfig::Url { url, sse, headers, oauth } => {
            let mut o = json!({"url": url});
            if *sse {
                o["type"] = json!("sse");
            }
            if !headers.is_empty() {
                o["headers"] = json!(headers);
            }
            if let Some(a) = oauth {
                o["oauth"] = json!({"clientId": a.client_id});
                if let Some(s) = &a.client_secret {
                    o["oauth"]["clientSecret"] = json!(s);
                }
            }
            o
        }
    }
}

impl ConnectorDraft {
    /// Online-only: every server is a URL.
    pub fn online(&self) -> bool {
        self.servers.values().all(|c| !c.is_local())
    }

    /// The `byob-plugin.json` to write for this connector (already validated).
    pub fn manifest_json(&self) -> Value {
        let description = if self.online() {
            let hosts: Vec<String> = self
                .servers
                .values()
                .filter_map(|c| match c {
                    McpServerConfig::Url { url, .. } => url.split_once("://").and_then(|(_, r)| r.split('/').next()).map(str::to_string),
                    McpServerConfig::Stdio { .. } => None,
                })
                .collect();
            format!("Online connector: {}", hosts.join(", "))
        } else {
            "Connector (MCP).".into()
        };
        let servers: Map<String, Value> = self.servers.iter().map(|(k, c)| (k.clone(), server_json(c))).collect();
        let mut m = json!({
            "id": self.id, "name": self.name, "version": "1.0.0", "description": description,
            "icon": if self.online() { "globe" } else { "plug" }, "mcpServers": servers,
        });
        if !self.secrets.is_empty() {
            let settings: Vec<Value> = self.secrets.keys().map(|k| json!({"key": k, "label": self.labels.get(k).unwrap_or(k), "secret": true})).collect();
            m["settings"] = json!(settings);
        }
        m
    }

    /// Check the generated manifest parses (it always should).
    pub fn check(&self) -> Result<(), String> {
        parse_manifest(&self.manifest_json(), ParseOptions::default()).map(|_| ()).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    struct Fake {
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl Connector for Fake {
        fn list_tools(&self) -> Result<Vec<ConnectorTool>, String> {
            let listed = json!([
                {"name": "get-issue", "description": "Read an issue", "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}}},
                {"name": "create.issue", "annotations": {"title": "Create issue", "destructiveHint": true}},
                {"name": "snapshot", "inputSchema": "nonsense"},
                {"name": ""},
                {"nope": 1},
            ]);
            Ok(listed.as_array().map(|a| a.iter().filter_map(ConnectorTool::from_mcp).collect()).unwrap_or_default())
        }
        fn call_tool(&self, name: &str, args: &Value, _ctx: &ToolContext<'_>) -> Result<ConnectorResult, String> {
            self.calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((name.to_string(), args.clone()));
            match name {
                "get-issue" => Ok(ConnectorResult::from_mcp(
                    &json!({"content": [{"type": "text", "text": "Issue 1"}, {"type": "resource", "resource": {"uri": "x://1"}}, {"type": "audio"}]}),
                )),
                "snapshot" => Ok(ConnectorResult::from_mcp(&json!({"content": [{"type": "image", "data": "aGk=", "mimeType": "image/png"}]}))),
                _ => Err("connection closed".into()),
            }
        }
    }

    fn plugin() -> PluginRef {
        PluginRef { id: "linear".into(), name: "Linear".into(), dir: PathBuf::from("/p"), network: false, env: BTreeMap::new() }
    }

    #[test]
    fn adapts_server_tools() {
        let fake = Arc::new(Fake { calls: Mutex::new(Vec::new()) });
        let mut names = ToolNames::new(["linear__get-issue"]);
        let tools = connector_tools(&plugin(), false, fake.clone(), &mut names).unwrap();
        let specs: Vec<String> = tools.iter().map(|t| t.spec().name).collect();
        assert_eq!(specs, ["linear__get-issue_2", "linear__create_issue", "linear__snapshot"]);
        assert_eq!(tools[2].spec().parameters, json!({"type": "object", "properties": {}}));
        assert_eq!(tools[1].spec().description, "Create issue");
        assert_eq!(tools[1].title(&json!({})), "Used Linear: Create issue");
        assert_eq!(tools[0].access(&json!({})), Access::Network);
        assert!(tools[0].connector_tool().read_only() && !tools[0].connector_tool().needs_approval(false));
        assert!(tools[1].connector_tool().needs_approval(false));

        let ws = std::env::temp_dir().join(format!("byob-plugins-conn-{}", std::process::id()));
        let _ = fs::remove_dir_all(&ws);
        fs::create_dir_all(&ws).unwrap();
        let cancel = AtomicBool::new(false);
        let ctx = ToolContext::new(&ws, &cancel);
        let out = tools[0].run(&json!("not an object"), &ctx);
        assert_eq!(out.text, "Issue 1\n[resource x://1]");
        assert!(!out.failed);
        assert_eq!(fake.calls.lock().unwrap()[0], ("get-issue".to_string(), json!({})));
        let out = tools[1].run(&json!({"title": "x"}), &ctx);
        assert!(out.failed && out.text == "Linear couldn't do that: connection closed");
        let out = tools[2].run(&json!({}), &ctx);
        assert_eq!(out.text, "[image saved as linear-1.png]");
        assert_eq!(fs::read(ws.join("linear-1.png")).unwrap(), b"hi");
        assert_eq!(out.changes, [FileChange::Created(ws.join("linear-1.png"))]);
        let again = tools[2].run(&json!({}), &ctx);
        assert_eq!(again.text, "[image saved as linear-1-1.png]");
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn local_servers_execute() {
        let fake = Arc::new(Fake { calls: Mutex::new(Vec::new()) });
        let tools = connector_tools(&plugin(), true, fake, &mut ToolNames::default()).unwrap();
        assert_eq!(tools[0].access(&json!({})), Access::Execute);
    }

    #[test]
    fn read_only_names() {
        for n in ["get_issue", "listFiles", "search", "fetch-page", "get_order_status"] {
            assert_eq!(name_reads_only(n), n != "get_order_status", "{n}");
        }
        for n in ["query", "getaway", "get_or_create", "list_and_delete", "create_issue"] {
            assert!(!name_reads_only(n), "{n}");
        }
    }

    #[test]
    fn base64() {
        assert_eq!(base64_decode("aGVsbG8gd29ybGQ=").unwrap(), b"hello world");
        assert_eq!(base64_decode("aGVsbG8\ngd29ybGQ").unwrap(), b"hello world");
        assert!(base64_decode("!!").is_none());
    }

    #[test]
    fn pasted_configs() {
        let d = parse_connector("", r#"{"mcpServers": {"memory": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-memory"]}, "docs": {"url": "https://example.com/mcp"}}}"#, "").unwrap();
        assert_eq!(d.id, "docs");
        assert_eq!(d.servers.len(), 2);
        assert!(!d.online());
        d.check().unwrap();

        let d = parse_connector("", r#"{"servers": {"gh": {"type": "http", "url": "https://api.githubcopilot.com/mcp/"}}}"#, "").unwrap();
        assert!(d.online());

        let d = parse_connector("", "https://mcp.linear.app/mcp", "Authorization: Bearer abc\n").unwrap();
        assert_eq!((d.id.as_str(), d.name.as_str()), ("linear", "Linear"));
        assert_eq!(d.secrets["HEADER_AUTHORIZATION"], "Bearer abc");
        let m = d.manifest_json();
        assert_eq!(m["mcpServers"]["linear"]["headers"]["Authorization"], "${HEADER_AUTHORIZATION}");
        assert_eq!(m["settings"][0]["label"], "Authorization");
        assert_eq!(m["description"], "Online connector: mcp.linear.app");
        d.check().unwrap();

        let d = parse_connector("", "npx -y '@scope/my server' ~/My\\ Notes", "API_KEY=abc").unwrap();
        assert_eq!(d.name, "My Notes");
        match &d.servers["my-notes"] {
            McpServerConfig::Stdio { command, args, .. } => {
                assert_eq!((command.as_str(), args.as_slice()), ("npx", &["-y".to_string(), "@scope/my server".into(), "~/My Notes".into()][..]))
            }
            other => panic!("{other:?}"),
        }
        d.check().unwrap();

        assert_eq!(parse_connector("", "{nope", "").unwrap_err(), "That config isn't valid JSON.");
        assert_eq!(parse_connector("", "{}", "").unwrap_err(), "No servers found in that config.");
        assert_eq!(parse_connector("", "  ", "").unwrap_err(), "Enter the command that starts the connector.");
        assert!(parse_connector("", r#"{"mcpServers": {"x": {"url": "ftp://x"}}}"#, "").unwrap_err().contains("must start with https://"));
        assert!(parse_connector("", "server", "lower=1").unwrap_err().contains("UPPER_CASE"));
    }

    #[test]
    fn url_names() {
        assert_eq!(name_from_url("https://mcp.linear.app/mcp"), "Linear");
        assert_eq!(name_from_url("http://localhost:3000/mcp"), "Local app");
        assert_eq!(name_from_url("http://127.0.0.1/mcp"), "Local app");
        assert_eq!(name_from_url("https://notion.com"), "Notion");
    }
}
