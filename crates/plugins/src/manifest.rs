//! `hob-plugin.json` (alias `yah-plugin.json`): the plugin manifest.
//!
//! Parsing mirrors hob's `parseManifest`: the same required fields, defaults and limits,
//! and error messages written for the plugin's author (e.g.
//! `hob-plugin.json: tools[0].run must be a path inside the plugin folder`). Unknown fields
//! are ignored so newer manifests still load.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use serde::Serialize;
use serde_json::{Map, Value};

/// The manifest file name hob uses. yah reads it unchanged.
pub const MANIFEST: &str = "hob-plugin.json";
/// yah's own name for the same file; read when there is no `hob-plugin.json`.
pub const MANIFEST_ALIAS: &str = "yah-plugin.json";

/// Prompt length caps (characters), as in hob: built-in plugins carry longer how-tos.
const PROMPT_MAX: usize = 600;
const PROMPT_MAX_BUILTIN: usize = 1500;

/// A checked plugin manifest with defaults filled in.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Manifest {
    /// `[a-z0-9][a-z0-9-]{0,47}`.
    pub id: String,
    pub name: String,
    /// `0.0.0` when left out.
    pub version: String,
    pub description: String,
    pub author: Option<String>,
    /// A lucide icon name; `puzzle` when left out.
    pub icon: String,
    pub permissions: Permissions,
    pub starters: Vec<Starter>,
    pub tools: Vec<ManifestTool>,
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    pub settings: Vec<Setting>,
    /// A line added to the system prompt while the plugin is on.
    pub prompt: Option<String>,
    /// Node `process.platform` names (`darwin`, `linux`, `win32`); everywhere when `None`.
    pub platforms: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Permissions {
    /// Python tools (and skills' scripts) may use the internet.
    pub network: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Starter {
    pub title: String,
    pub prompt: String,
    /// The starter asks the user for a file first.
    pub file: bool,
}

/// A tool a plugin declares.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ManifestTool {
    /// `[A-Za-z][A-Za-z0-9_]{0,63}`.
    pub name: String,
    pub description: String,
    /// JSON Schema object for the arguments.
    pub params: Value,
    pub kind: ToolKind,
    /// Step title with `{arg}` placeholders.
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ToolKind {
    /// A Python script, relative to the plugin folder.
    Script(String),
    /// One of the app's own tools, by name (built-in plugins only).
    Native(String),
}

/// How to reach one MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum McpServerConfig {
    /// A program on this computer, spoken to over stdio.
    Stdio { command: String, args: Vec<String>, env: BTreeMap<String, String> },
    /// A server at a URL (Streamable HTTP, or legacy HTTP+SSE).
    Url { url: String, sse: bool, headers: BTreeMap<String, String>, oauth: Option<OAuthClient> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OAuthClient {
    pub client_id: String,
    pub client_secret: Option<String>,
}

impl McpServerConfig {
    /// Stdio servers run unsandboxed local code; URL servers only talk to the network.
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Stdio { .. })
    }

    /// Fill `${KEY}` placeholders in args, env values, the URL and headers from the
    /// plugin's settings. Unknown keys are left as they are.
    pub fn with_settings(&self, values: &BTreeMap<String, String>) -> Self {
        let fill = |s: &String| fill_placeholders(s, values);
        let fill_map = |m: &BTreeMap<String, String>| m.iter().map(|(k, v)| (k.clone(), fill(v))).collect();
        match self {
            Self::Stdio { command, args, env } => Self::Stdio { command: fill(command), args: args.iter().map(fill).collect(), env: fill_map(env) },
            Self::Url { url, sse, headers, oauth } => Self::Url { url: fill(url), sse: *sse, headers: fill_map(headers), oauth: oauth.clone() },
        }
    }
}

/// `"Bearer ${API_KEY}"` + `{API_KEY: "abc"}` → `"Bearer abc"`.
pub fn fill_placeholders(s: &str, values: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let key = &after[..end];
                match values.get(key) {
                    Some(v) => out.push_str(v),
                    None => {
                        out.push_str("${");
                        out.push_str(key);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub enum SettingKind {
    #[default]
    Text,
    Number,
    Boolean,
    Directory,
    File,
}

impl SettingKind {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "text" => Self::Text,
            "number" => Self::Number,
            "boolean" => Self::Boolean,
            "directory" => Self::Directory,
            "file" => Self::File,
            _ => return None,
        })
    }
}

/// A value the user fills in on the plugin's card; passed to tools and servers as an
/// environment variable named `key`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Setting {
    /// `[A-Z][A-Z0-9_]{0,63}`.
    pub key: String,
    pub label: String,
    pub secret: bool,
    pub help: Option<String>,
    pub kind: SettingKind,
    pub required: bool,
    pub multiple: bool,
    /// Lists are joined with newlines, as hob does.
    pub default: Option<String>,
}

/// Why a manifest was rejected; the message is meant for the plugin's author.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{file}: {message}")]
pub struct ManifestError {
    pub file: String,
    pub message: String,
}

/// Options for [`parse_manifest`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ParseOptions<'a> {
    /// Ships with the app: may use `native` tools and a longer prompt.
    pub builtin: bool,
    /// File name used in error messages; [`MANIFEST`] when `None`.
    pub file: Option<&'a str>,
}

/// `[a-z0-9][a-z0-9-]{0,47}`
pub fn plugin_id_ok(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty() && b.len() <= 48 && is_lower_alnum(b[0]) && b.iter().all(|&c| is_lower_alnum(c) || c == b'-')
}

/// `[A-Za-z][A-Za-z0-9_]{0,63}`
pub fn tool_name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && b.len() <= 64 && b[0].is_ascii_alphabetic() && b.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'_')
}

/// `[A-Z][A-Z0-9_]{0,63}`
pub fn setting_key_ok(key: &str) -> bool {
    let b = key.as_bytes();
    !b.is_empty() && b.len() <= 64 && b[0].is_ascii_uppercase() && b.iter().all(|&c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
}

fn is_lower_alnum(c: u8) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit()
}

/// A relative path that stays inside its folder (`tools/x.py`, not `/x` or `../x`).
pub fn path_inside(p: &str) -> bool {
    let mut depth: usize = 0;
    for c in Path::new(p).components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => return false,
            },
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    // Windows-style absolute paths on other systems.
    !(p.starts_with('\\') || p.as_bytes().get(1) == Some(&b':'))
}

/// Check a parsed `hob-plugin.json` and fill in defaults.
pub fn parse_manifest(raw: &Value, opts: ParseOptions<'_>) -> Result<Manifest, ManifestError> {
    let file = opts.file.unwrap_or(MANIFEST);
    parse(raw, opts.builtin).map_err(|message| ManifestError { file: file.to_string(), message })
}

/// Parse manifest text (JSON).
pub fn parse_manifest_str(text: &str, opts: ParseOptions<'_>) -> Result<Manifest, ManifestError> {
    let file = opts.file.unwrap_or(MANIFEST);
    let raw: Value = serde_json::from_str(text).map_err(|e| ManifestError { file: file.to_string(), message: format!("isn't valid JSON: {e}") })?;
    parse_manifest(&raw, opts)
}

type R<T> = Result<T, String>;

/// Required text: present, a string, not blank. Trimmed.
fn req_str(v: Option<&Value>, field: &str) -> R<String> {
    match v {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        _ => Err(format!("{field} must be text")),
    }
}

/// Optional text: absent → `None`; present must be a string.
fn opt_str(v: Option<&Value>, field: &str) -> R<Option<String>> {
    match v {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
        Some(_) => Err(format!("{field} must be text")),
    }
}

fn list<'a>(v: Option<&'a Value>, field: &str) -> R<&'a [Value]> {
    match v {
        None => Ok(&[]),
        Some(Value::Array(a)) => Ok(a),
        Some(_) => Err(format!("{field} must be a list")),
    }
}

fn obj<'a>(v: &'a Value, field: &str) -> R<&'a Map<String, Value>> {
    v.as_object().ok_or_else(|| format!("{field} must be an object"))
}

fn str_map(v: Option<&Value>, field: &str) -> R<BTreeMap<String, String>> {
    let Some(v) = v else { return Ok(BTreeMap::new()) };
    let bad = || format!("{field} must map names to text");
    let m = v.as_object().ok_or_else(bad)?;
    m.iter().map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())).ok_or_else(bad)).collect()
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn parse(raw: &Value, builtin: bool) -> R<Manifest> {
    let r = raw.as_object().ok_or("must be a JSON object")?;
    let id = req_str(r.get("id"), "id")?;
    if !plugin_id_ok(&id) {
        return Err("id must be lowercase letters, digits and dashes, like \"trip-planner\"".into());
    }
    let tools = list(r.get("tools"), "tools")?.iter().enumerate().map(|(i, t)| parse_tool(t, i, builtin)).collect::<R<Vec<_>>>()?;
    let mcp_servers = parse_servers(r.get("mcpServers"))?;
    let settings = list(r.get("settings"), "settings")?.iter().enumerate().map(|(i, s)| parse_setting(s, i)).collect::<R<Vec<_>>>()?;
    let starters = list(r.get("starters"), "starters")?
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let f = format!("starters[{i}]");
            let s = obj(s, &f)?;
            Ok(Starter {
                title: req_str(s.get("title"), &format!("{f}.title"))?,
                prompt: req_str(s.get("prompt"), &format!("{f}.prompt"))?,
                file: s.get("file") == Some(&Value::Bool(true)),
            })
        })
        .collect::<R<Vec<_>>>()?;
    let network = r.get("permissions").and_then(Value::as_object).and_then(|p| p.get("network")) == Some(&Value::Bool(true));
    let prompt = match r.get("prompt") {
        None => None,
        p => Some(truncate_chars(&req_str(p, "prompt")?, if builtin { PROMPT_MAX_BUILTIN } else { PROMPT_MAX })),
    };
    let platforms = r.get("platforms").and_then(Value::as_array).map(|a| a.iter().map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string)).collect());
    let author = match r.get("author") {
        None => None,
        a => Some(req_str(a, "author")?),
    };
    Ok(Manifest {
        id,
        name: req_str(r.get("name"), "name")?,
        version: opt_str(r.get("version"), "version")?.filter(|v| !v.is_empty()).unwrap_or_else(|| "0.0.0".into()),
        description: opt_str(r.get("description"), "description")?.unwrap_or_default(),
        author,
        icon: opt_str(r.get("icon"), "icon")?.filter(|v| !v.is_empty()).unwrap_or_else(|| "puzzle".into()),
        permissions: Permissions { network },
        starters,
        tools,
        mcp_servers,
        settings,
        prompt,
        platforms,
    })
}

fn parse_tool(t: &Value, i: usize, builtin: bool) -> R<ManifestTool> {
    let f = format!("tools[{i}]");
    let t = obj(t, &f)?;
    let native = match t.get("native") {
        None => None,
        n => Some(req_str(n, &format!("{f}.native"))?),
    };
    if native.is_some() && !builtin {
        return Err(format!("{f}.native is only for plugins that ship with the app; use \"run\" with a Python script"));
    }
    // native tools bring their own name, description and parameters
    let name = match (&native, t.get("name")) {
        (Some(n), None) => n.clone(),
        (_, v) => req_str(v, &format!("{f}.name"))?,
    };
    if !tool_name_ok(&name) {
        return Err(format!("{f}.name must be letters, digits or _, starting with a letter"));
    }
    let params = match t.get("params") {
        None => serde_json::json!({ "type": "object", "properties": {} }),
        Some(p @ Value::Object(_)) => p.clone(),
        Some(_) => return Err(format!("{f}.params must be a JSON schema object")),
    };
    let run = match t.get("run") {
        None => None,
        v => {
            let p = req_str(v, &format!("{f}.run"))?;
            if !path_inside(&p) {
                return Err(format!("{f}.run must be a path inside the plugin folder"));
            }
            Some(p)
        }
    };
    let kind = match (run, native) {
        (Some(r), _) => ToolKind::Script(r),
        (None, Some(n)) => ToolKind::Native(n),
        (None, None) => return Err(format!("{f}.run must name the Python script that runs this tool")),
    };
    let description = match (&kind, t.get("description")) {
        (ToolKind::Native(_), None) => String::new(),
        (_, d) => req_str(d, &format!("{f}.description"))?,
    };
    let title = match t.get("title") {
        None => None,
        v => Some(req_str(v, &format!("{f}.title"))?),
    };
    Ok(ManifestTool { name, description, params, kind, title })
}

fn parse_servers(v: Option<&Value>) -> R<BTreeMap<String, McpServerConfig>> {
    let Some(v) = v else { return Ok(BTreeMap::new()) };
    let m = v.as_object().ok_or("mcpServers must be an object of servers")?;
    m.iter().map(|(k, v)| Ok((k.clone(), parse_server(v, &format!("mcpServers.{k}"))?))).collect()
}

/// One entry of an `mcpServers` object (manifest or pasted Claude/Cursor config).
pub(crate) fn parse_server(v: &Value, f: &str) -> R<McpServerConfig> {
    let v = obj(v, f)?;
    let ty = v.get("type").and_then(Value::as_str);
    if v.get("url").is_some() || matches!(ty, Some("http" | "sse" | "streamable-http")) {
        let url = req_str(v.get("url"), &format!("{f}.url"))?;
        let lower = url.to_ascii_lowercase();
        let rest = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://"));
        if !rest.is_some_and(|r| !r.is_empty() && !r.starts_with('/')) {
            return Err(format!("{f}.url must start with https://"));
        }
        let headers = str_map(v.get("headers"), &format!("{f}.headers"))?;
        let oauth = match v.get("oauth") {
            None => None,
            Some(o) => {
                let o = obj(o, &format!("{f}.oauth"))?;
                Some(OAuthClient {
                    client_id: req_str(o.get("clientId"), &format!("{f}.oauth.clientId"))?,
                    client_secret: match o.get("clientSecret") {
                        None => None,
                        s => Some(req_str(s, &format!("{f}.oauth.clientSecret"))?),
                    },
                })
            }
        };
        return Ok(McpServerConfig::Url { url, sse: ty == Some("sse"), headers, oauth });
    }
    let args = list(v.get("args"), &format!("{f}.args"))?
        .iter()
        .enumerate()
        .map(|(i, a)| a.as_str().map(|s| s.trim().to_string()).ok_or_else(|| format!("{f}.args[{i}] must be text")))
        .collect::<R<Vec<_>>>()?;
    let env = str_map(v.get("env"), &format!("{f}.env"))?;
    Ok(McpServerConfig::Stdio { command: req_str(v.get("command"), &format!("{f}.command"))?, args, env })
}

fn parse_setting(s: &Value, i: usize) -> R<Setting> {
    let f = format!("settings[{i}]");
    let s = obj(s, &f)?;
    let key = req_str(s.get("key"), &format!("{f}.key"))?;
    if !setting_key_ok(&key) {
        return Err(format!("{f}.key must be UPPER_CASE, like \"API_KEY\""));
    }
    let kind = match s.get("type") {
        None => SettingKind::Text,
        t => {
            let t = req_str(t, &format!("{f}.type"))?;
            SettingKind::parse(&t).ok_or_else(|| format!("{f}.type must be one of text, number, boolean, directory, file"))?
        }
    };
    let help = match s.get("help") {
        None => None,
        h => Some(req_str(h, &format!("{f}.help"))?),
    };
    let default = s.get("default").map(|d| match d {
        Value::Array(a) => a.iter().map(value_text).collect::<Vec<_>>().join("\n"),
        other => value_text(other),
    });
    Ok(Setting {
        key,
        label: req_str(s.get("label"), &format!("{f}.label"))?,
        secret: truthy(s.get("secret")),
        help,
        kind,
        required: truthy(s.get("required")),
        multiple: truthy(s.get("multiple")),
        default,
    })
}

/// JS `String(v)` for the values settings defaults take.
fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_user(v: Value) -> Result<Manifest, String> {
        parse_manifest(&v, ParseOptions::default()).map_err(|e| e.to_string())
    }

    #[test]
    fn minimal_manifest_gets_defaults() {
        let m = parse_user(json!({"id": "recipe-helper", "name": "Recipe Helper", "extra": {"future": true}})).unwrap();
        assert_eq!(m.version, "0.0.0");
        assert_eq!(m.icon, "puzzle");
        assert!(!m.permissions.network);
        assert!(m.tools.is_empty() && m.mcp_servers.is_empty() && m.settings.is_empty());
    }

    #[test]
    fn author_facing_errors() {
        let cases = [
            (json!([]), "hob-plugin.json: must be a JSON object"),
            (json!({"name": "x"}), "hob-plugin.json: id must be text"),
            (json!({"id": "Bad_Id", "name": "x"}), "hob-plugin.json: id must be lowercase letters, digits and dashes, like \"trip-planner\""),
            (json!({"id": "a", "name": "x", "tools": {}}), "hob-plugin.json: tools must be a list"),
            (
                json!({"id": "a", "name": "x", "tools": [{"name": "t", "description": "d", "run": "../escape.py"}]}),
                "hob-plugin.json: tools[0].run must be a path inside the plugin folder",
            ),
            (
                json!({"id": "a", "name": "x", "tools": [{"name": "t", "description": "d", "run": "/abs.py"}]}),
                "hob-plugin.json: tools[0].run must be a path inside the plugin folder",
            ),
            (
                json!({"id": "a", "name": "x", "tools": [{"name": "t", "description": "d"}]}),
                "hob-plugin.json: tools[0].run must name the Python script that runs this tool",
            ),
            (
                json!({"id": "a", "name": "x", "tools": [{"name": "1t", "description": "d", "run": "t.py"}]}),
                "hob-plugin.json: tools[0].name must be letters, digits or _, starting with a letter",
            ),
            (
                json!({"id": "a", "name": "x", "tools": [{"native": "run_python"}]}),
                "hob-plugin.json: tools[0].native is only for plugins that ship with the app; use \"run\" with a Python script",
            ),
            (
                json!({"id": "a", "name": "x", "settings": [{"key": "api_key", "label": "k"}]}),
                "hob-plugin.json: settings[0].key must be UPPER_CASE, like \"API_KEY\"",
            ),
            (
                json!({"id": "a", "name": "x", "settings": [{"key": "K", "label": "k", "type": "color"}]}),
                "hob-plugin.json: settings[0].type must be one of text, number, boolean, directory, file",
            ),
            (json!({"id": "a", "name": "x", "mcpServers": {"s": {"url": "ftp://x"}}}), "hob-plugin.json: mcpServers.s.url must start with https://"),
            (
                json!({"id": "a", "name": "x", "mcpServers": {"s": {"command": "x", "env": {"A": 1}}}}),
                "hob-plugin.json: mcpServers.s.env must map names to text",
            ),
            (json!({"id": "a", "name": "x", "mcpServers": {"s": {}}}), "hob-plugin.json: mcpServers.s.command must be text"),
        ];
        for (raw, want) in cases {
            assert_eq!(parse_user(raw.clone()).unwrap_err(), want, "for {raw}");
        }
    }

    #[test]
    fn python_tool_and_settings() {
        let m = parse_user(json!({
            "id": "text-stats", "name": "Text stats", "permissions": {"network": true},
            "tools": [{"name": "count_words", "description": "Count words.", "params": {"type": "object", "properties": {"file": {"type": "string"}}}, "run": "tools/count.py", "title": "Counted {file}"}],
            "settings": [{"key": "API_KEY", "label": "API key", "secret": true, "type": "directory", "multiple": true, "default": ["/a", "/b"]}],
            "starters": [{"title": "Go", "prompt": "/go", "file": true}],
            "prompt": "x".repeat(700),
        }))
        .unwrap();
        assert!(m.permissions.network);
        assert_eq!(m.tools[0].kind, ToolKind::Script("tools/count.py".into()));
        assert_eq!(m.tools[0].title.as_deref(), Some("Counted {file}"));
        let s = &m.settings[0];
        assert!(s.secret && s.multiple && !s.required);
        assert_eq!(s.kind, SettingKind::Directory);
        assert_eq!(s.default.as_deref(), Some("/a\n/b"));
        assert!(m.starters[0].file);
        assert_eq!(m.prompt.map(|p| p.chars().count()), Some(600));
    }

    #[test]
    fn native_tools_for_builtins() {
        let raw = json!({"id": "code", "name": "Code", "tools": [{"native": "run_python"}]});
        let m = parse_manifest(&raw, ParseOptions { builtin: true, file: None }).unwrap();
        assert_eq!(m.tools[0].name, "run_python");
        assert_eq!(m.tools[0].kind, ToolKind::Native("run_python".into()));
    }

    #[test]
    fn server_configs() {
        let m = parse_user(json!({"id": "a", "name": "x", "mcpServers": {
            "mem": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-memory"], "env": {"K": "${API_KEY}"}},
            "lin": {"url": "https://mcp.linear.app/mcp", "headers": {"Authorization": "Bearer ${API_KEY}"}, "oauth": {"clientId": "c"}},
            "old": {"url": "https://example.com/sse", "type": "sse"},
        }}))
        .unwrap();
        assert!(m.mcp_servers["mem"].is_local());
        assert!(!m.mcp_servers["lin"].is_local());
        assert!(matches!(&m.mcp_servers["old"], McpServerConfig::Url { sse: true, .. }));
        let vals = BTreeMap::from([("API_KEY".to_string(), "abc".to_string())]);
        match m.mcp_servers["lin"].with_settings(&vals) {
            McpServerConfig::Url { headers, oauth, .. } => {
                assert_eq!(headers["Authorization"], "Bearer abc");
                assert_eq!(oauth.map(|o| o.client_id).as_deref(), Some("c"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn placeholders() {
        let vals = BTreeMap::from([("A".to_string(), "1".to_string())]);
        assert_eq!(fill_placeholders("${A}-${B}-${A", &vals), "1-${B}-${A");
        assert_eq!(fill_placeholders("ünï${A}cødé", &vals), "ünï1cødé");
    }

    #[test]
    fn inside_paths() {
        assert!(path_inside("tools/x.py") && path_inside("./a/../b.py"));
        assert!(!path_inside("../x") && !path_inside("a/../../x") && !path_inside("/x") && !path_inside("C:\\x") && !path_inside("\\x"));
    }
}
