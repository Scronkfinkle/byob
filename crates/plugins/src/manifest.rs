//! `byob-plugin.json`: the plugin manifest.
//!
//! Parsing mirrors hob's `parseManifest`: the same required fields, defaults and limits,
//! and error messages written for the plugin's author (e.g.
//! `byob-plugin.json: tools[0].run must be a path inside the plugin folder`). Unknown fields
//! are ignored so newer manifests still load.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use serde::Serialize;
use serde_json::{Map, Value};

/// The manifest file name.
pub const MANIFEST: &str = "byob-plugin.json";
/// The manifest's old name (from hob, which byob started as a rewrite of). Not read: a
/// folder with only this file gets a warning to rename it.
pub const OLD_MANIFEST: &str = "hob-plugin.json";

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
    /// A long-lived process the engine talks to in JSON lines (`backend`), see
    /// `docs/plugins-backend.md`.
    pub backend: Option<BackendDecl>,
    /// Python packages its scripts and backend need (`python.requirements`, pip specs).
    pub requirements: Vec<String>,
    /// Live views its backend draws at render sites (`views`).
    pub views: Vec<ManifestView>,
    /// Can't be switched off (`locked`; honoured for bundled plugins and their forks).
    pub locked: bool,
    /// Artifact content types it adds (`artifact_types`, also `content_types`).
    pub artifact_types: Vec<ManifestArtifactType>,
    /// Which renderer draws a content type on clients (`renderers`).
    pub renderers: Vec<ManifestRenderer>,
    /// A section of the system prompt (`system_prompt`): a file of the plugin, or text an
    /// in-tree builder makes.
    pub system_prompt: Option<PromptSource>,
    /// Places in the turn pipeline it sits in (`hooks`), see [`ManifestHook`].
    pub hooks: Vec<ManifestHook>,
    /// Workspace environment providers it adds (`environments`), see [`ManifestEnv`].
    pub environments: Vec<ManifestEnv>,
}

/// The turn pipeline's hook points, in the order a turn reaches them.
pub const HOOK_EVENTS: [&str; 5] = ["before_prompt", "before_request", "before_tool_call", "after_tool_call", "on_turn_end"];
/// Most hooks one plugin declares.
pub const MAX_HOOKS: usize = 16;
/// A backend hook's default and longest timeout (milliseconds).
pub const HOOK_TIMEOUT_MS: u64 = 2000;
pub const MAX_HOOK_TIMEOUT_MS: u64 = 60_000;
/// Hooks run by priority, lowest first; this when left out.
pub const DEFAULT_HOOK_PRIORITY: i64 = 100;

/// A place in the turn pipeline (`hooks[]`): `{"event": "before_tool_call", "priority": 50,
/// "backend": true, "timeout_ms": 2000, "fail": "closed", "tools": ["delete_file"]}`, or
/// `{"event": "before_request", "native": "compaction"}` (in-tree code by id).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestHook {
    /// One of [`HOOK_EVENTS`].
    pub event: String,
    /// Lower runs first ([`DEFAULT_HOOK_PRIORITY`] when left out).
    pub priority: i64,
    /// In-tree code by id (`None`: the plugin's backend answers a `hook` request).
    pub native: Option<String>,
    /// How long a backend may take (ms).
    pub timeout_ms: u64,
    /// `"fail": "closed"`: a backend that faults or times out stops the stage (a tool call is
    /// denied, a request isn't made); the default (open) carries on as if it said continue.
    pub fail_closed: bool,
    /// `before_tool_call` / `after_tool_call` only for these tools (empty: every tool).
    pub tools: Vec<String>,
}

/// Most environment providers one plugin declares.
pub const MAX_ENVIRONMENTS: usize = 8;

/// A workspace environment provider (`environments[]`): in-tree (`{"native": "nix"}`), or a
/// command that prints the environment as JSON when the workspace has one of `detect`:
/// `{"id": "conda", "label": "conda environment", "detect": ["environment.yml"], "inputs":
/// ["environment.yml"], "command": "python3", "args": ["conda_env.py"]}`. Loading it is asked
/// once per workspace, like the built-in ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestEnv {
    /// `[a-z0-9_-]{1,32}` (the native id when native).
    pub id: String,
    pub native: Option<String>,
    /// For messages: "conda environment".
    pub label: String,
    /// Files that make a workspace use it (the first one found is named).
    pub detect: Vec<String>,
    /// Files whose contents key its cache (`detect` too).
    pub inputs: Vec<String>,
    /// The program (on `PATH`, or a file of the plugin) and its arguments (a file of the
    /// plugin is passed as its full path). It runs in the workspace, sandboxed, with network.
    pub command: Option<String>,
    pub args: Vec<String>,
}

/// Most artifact types or renderers one plugin declares.
pub const MAX_TYPES: usize = 16;

/// An artifact type a plugin adds: `{"type": "text/html", "native": "html_app", "guidance":
/// "guidance.md"}`, or a declarative text type `{"type": "text/csv", "suffixes": [".csv"],
/// "label": "table", "describe": ".csv (a table)", "pick": "a table → .csv"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestArtifactType {
    /// The content type, e.g. `text/html`.
    pub content_type: String,
    /// An in-tree type (hooks, defaults) by id; the fields below override its defaults.
    pub native: Option<String>,
    pub suffixes: Option<Vec<String>>,
    pub label: Option<String>,
    pub describe: Option<String>,
    pub pick: Option<String>,
    /// A file of the plugin with the type's system-prompt guidance.
    pub guidance: Option<String>,
    /// `"binary": true`: its versions aren't text (`create_artifact` / `update_artifact` /
    /// `read_artifact` don't take them; the plugin's own tools write them).
    pub binary: bool,
}

/// `{"type": "text/html", "renderer": "web" | "text" | "native:<id>" | "view:<view id>"}`
/// (`view:` names one of the plugin's views with `"site": "artifact"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestRenderer {
    pub content_type: String,
    pub renderer: String,
}

/// Where a plugin's system-prompt section comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PromptSource {
    /// A file of the plugin (`"system_prompt": "prompt.md"`).
    File(String),
    /// An in-tree builder by id (`"system_prompt": {"native": "artifacts"}`).
    Native(String),
}

/// Render sites a view may declare.
pub const VIEW_SITES: [&str; 5] = ["pane", "settings", "status", "card", "artifact"];
/// Most views one plugin declares.
pub const MAX_VIEWS: usize = 16;

/// A view the plugin's backend draws (`views[]`): `{"site": "pane", "id": "todo",
/// "title": "To do", "icon": "list"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestView {
    /// `[a-z0-9_-]{1,48}`, unique in the plugin.
    pub id: String,
    /// One of [`VIEW_SITES`].
    pub site: String,
    /// The plugin's name when left out.
    pub title: String,
    pub icon: String,
}

/// Most `python.requirements` entries.
pub const MAX_REQUIREMENTS: usize = 64;
/// Engine events a backend may subscribe to (`backend.events`, or `subscribe` at run time).
pub const BACKEND_EVENTS: [&str; 5] = ["chat_opened", "turn_started", "turn_finished", "step_done", "artifact_updated"];

/// How to start a plugin's backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackendDecl {
    pub run: BackendRun,
    /// Extra arguments after the script (or the command).
    pub args: Vec<String>,
    /// Engine events it gets from the start (`subscribe` adds more at run time).
    pub events: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum BackendRun {
    /// A Python script in the plugin folder, run with the plugin's interpreter.
    Python(String),
    /// Any program (on `PATH`, or relative to the plugin folder), run in the plugin folder.
    Command(String),
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
    /// A native tool this build may not have (e.g. `install_packages` without a package
    /// manager): missing isn't a problem.
    pub optional: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ToolKind {
    /// A Python script, relative to the plugin folder.
    Script(String),
    /// One of the app's own (in-tree) tools, by name. A name this build doesn't have is a
    /// problem of the plugin.
    Native(String),
    /// Served by the plugin's long-lived backend (`"run": "backend"`).
    Backend,
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

/// Check a parsed `byob-plugin.json` and fill in defaults.
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
    let tools = list(r.get("tools"), "tools")?.iter().enumerate().map(|(i, t)| parse_tool(t, i)).collect::<R<Vec<_>>>()?;
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
    let backend = parse_backend(r.get("backend"))?;
    if backend.is_none()
        && let Some(i) = tools.iter().position(|t| t.kind == ToolKind::Backend)
    {
        return Err(format!("tools[{i}].run is \"backend\" but the plugin has no \"backend\""));
    }
    let requirements = parse_requirements(r.get("python"))?;
    let hooks = parse_hooks(r.get("hooks"), backend.is_some())?;
    let name = req_str(r.get("name"), "name")?;
    let views = parse_views(r.get("views"), &name)?;
    if backend.is_none() && !views.is_empty() {
        return Err("views are drawn by the plugin's backend: add \"backend\": \"backend.py\"".into());
    }
    let renderers = parse_renderers(r.get("renderers"))?;
    for (i, d) in renderers.iter().enumerate() {
        if let Some(v) = d.renderer.strip_prefix("view:")
            && !views.iter().any(|x| x.id == v && x.site == "artifact")
        {
            return Err(format!("renderers[{i}].renderer: {:?} needs a view {{\"site\": \"artifact\", \"id\": \"{v}\"}} in \"views\"", d.renderer));
        }
    }
    Ok(Manifest {
        id,
        name,
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
        backend,
        requirements,
        views,
        locked: r.get("locked") == Some(&Value::Bool(true)),
        artifact_types: parse_artifact_types(r.get("artifact_types").or_else(|| r.get("content_types")))?,
        renderers,
        system_prompt: parse_prompt_source(r.get("system_prompt"))?,
        hooks,
        environments: parse_environments(r.get("environments"))?,
    })
}

/// `"hooks": [{"event", "priority"?, "native"? | "backend": true, "timeout_ms"?, "fail"?, "tools"?}]`.
fn parse_hooks(v: Option<&Value>, has_backend: bool) -> R<Vec<ManifestHook>> {
    let items = list(v, "hooks")?;
    if items.len() > MAX_HOOKS {
        return Err(format!("hooks: at most {MAX_HOOKS}"));
    }
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let f = format!("hooks[{i}]");
        let o = obj(item, &f)?;
        let event = req_str(o.get("event"), &format!("{f}.event"))?;
        if !HOOK_EVENTS.contains(&event.as_str()) {
            return Err(format!("{f}.event must be one of {}", HOOK_EVENTS.join(", ")));
        }
        let priority = match o.get("priority") {
            None | Some(Value::Null) => DEFAULT_HOOK_PRIORITY,
            Some(p) => p.as_i64().filter(|p| p.abs() <= 1_000_000).ok_or_else(|| format!("{f}.priority must be a whole number"))?,
        };
        let native = opt_str(o.get("native"), &format!("{f}.native"))?;
        if let Some(n) = &native
            && !native_id_ok(n)
        {
            return Err(format!("{f}.native must be an id like \"compaction\""));
        }
        if native.is_none() && !has_backend {
            return Err(format!("{f} is answered by the plugin's backend: add \"backend\": \"backend.py\" (or name in-tree code with \"native\")"));
        }
        let timeout_ms = match o.get("timeout_ms") {
            None | Some(Value::Null) => HOOK_TIMEOUT_MS,
            Some(t) => {
                t.as_u64().filter(|t| (1..=MAX_HOOK_TIMEOUT_MS).contains(t)).ok_or_else(|| format!("{f}.timeout_ms must be 1 to {MAX_HOOK_TIMEOUT_MS}"))?
            }
        };
        let fail_closed = match opt_str(o.get("fail"), &format!("{f}.fail"))?.as_deref() {
            None | Some("open") => false,
            Some("closed") => true,
            Some(_) => return Err(format!("{f}.fail must be \"open\" or \"closed\"")),
        };
        let tools = list(o.get("tools"), &format!("{f}.tools"))?
            .iter()
            .enumerate()
            .map(|(j, t)| t.as_str().map(str::trim).filter(|t| tool_name_ok(t)).map(str::to_owned).ok_or_else(|| format!("{f}.tools[{j}] must be a tool name")))
            .collect::<R<Vec<_>>>()?;
        if !tools.is_empty() && !matches!(event.as_str(), "before_tool_call" | "after_tool_call") {
            return Err(format!("{f}.tools only applies to before_tool_call and after_tool_call"));
        }
        out.push(ManifestHook { event, priority, native, timeout_ms, fail_closed, tools });
    }
    Ok(out)
}

/// `"environments": [{"native": "nix"}, {"id", "label", "detect", "inputs"?, "command", "args"?}]`.
fn parse_environments(v: Option<&Value>) -> R<Vec<ManifestEnv>> {
    let items = list(v, "environments")?;
    if items.len() > MAX_ENVIRONMENTS {
        return Err(format!("environments: at most {MAX_ENVIRONMENTS}"));
    }
    let mut out: Vec<ManifestEnv> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let f = format!("environments[{i}]");
        let o = obj(item, &f)?;
        let native = opt_str(o.get("native"), &format!("{f}.native"))?;
        let id = match (&native, o.get("id")) {
            (Some(n), None) => n.clone(),
            (_, v) => req_str(v, &format!("{f}.id"))?,
        };
        let id_ok = (1..=32).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'));
        if !id_ok || native.as_deref().is_some_and(|n| !native_id_ok(n)) {
            return Err(format!("{f}.id must be lowercase letters, digits, _ or -, like \"conda\""));
        }
        if out.iter().any(|e| e.id == id) {
            return Err(format!("{f}.id {id:?} is used twice"));
        }
        let files = |key: &str| -> R<Vec<String>> {
            list(o.get(key), &format!("{f}.{key}"))?
                .iter()
                .enumerate()
                .map(|(j, p)| match p.as_str().map(str::trim) {
                    Some(p) if !p.is_empty() && path_inside(p) => Ok(p.to_owned()),
                    _ => Err(format!("{f}.{key}[{j}] must be a file name inside the workspace, like \"environment.yml\"")),
                })
                .collect()
        };
        let detect = files("detect")?;
        let inputs = files("inputs")?;
        let command = opt_str(o.get("command"), &format!("{f}.command"))?.filter(|c| !c.is_empty());
        let args = list(o.get("args"), &format!("{f}.args"))?
            .iter()
            .enumerate()
            .map(|(j, a)| a.as_str().map(str::to_owned).ok_or_else(|| format!("{f}.args[{j}] must be text")))
            .collect::<R<Vec<_>>>()?;
        if native.is_none() && (detect.is_empty() || command.is_none()) {
            return Err(format!("{f} needs \"native\" (built into byob) or \"detect\" (the workspace files) and \"command\" (prints the environment as JSON)"));
        }
        let label =
            truncate_chars(&opt_str(o.get("label"), &format!("{f}.label"))?.filter(|l| !l.is_empty()).unwrap_or_else(|| format!("{id} environment")), 60);
        out.push(ManifestEnv { id, native, label, detect, inputs, command, args });
    }
    Ok(out)
}

/// `type/subtype`, lowercase (what `byob_protocol::artifact::valid_content_type` takes).
fn content_type_ok(t: &str) -> bool {
    let ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'+' | b'_' | b'-'));
    t.len() <= 128 && t.split_once('/').is_some_and(|(a, b)| ok(a) && (ok(b) || b == "*"))
}

fn native_id_ok(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.'))
}

fn parse_artifact_types(v: Option<&Value>) -> R<Vec<ManifestArtifactType>> {
    let items = list(v, "artifact_types")?;
    if items.len() > MAX_TYPES {
        return Err(format!("artifact_types: at most {MAX_TYPES}"));
    }
    let mut out: Vec<ManifestArtifactType> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let f = format!("artifact_types[{i}]");
        let o = obj(item, &f)?;
        let content_type = req_str(o.get("type"), &format!("{f}.type"))?.to_ascii_lowercase();
        if !content_type_ok(&content_type) || content_type.ends_with("/*") {
            return Err(format!("{f}.type must be a content type like \"text/csv\""));
        }
        if out.iter().any(|t| t.content_type == content_type) {
            return Err(format!("{f}.type {content_type:?} is declared twice"));
        }
        let native = opt_str(o.get("native"), &format!("{f}.native"))?;
        if let Some(n) = &native
            && !native_id_ok(n)
        {
            return Err(format!("{f}.native must be an id like \"html_app\""));
        }
        let suffixes = match o.get("suffixes") {
            None => None,
            v => Some(
                list(v, &format!("{f}.suffixes"))?
                    .iter()
                    .enumerate()
                    .map(|(j, s)| match s.as_str().map(str::trim) {
                        Some(s) if s.starts_with('.') && s.len() > 1 && s.len() <= 32 && !s.contains(['/', '\\']) => Ok(s.to_ascii_lowercase()),
                        _ => Err(format!("{f}.suffixes[{j}] must be a file name ending like \".csv\"")),
                    })
                    .collect::<R<Vec<_>>>()?,
            ),
        };
        if native.is_none() && suffixes.as_ref().is_none_or(Vec::is_empty) {
            return Err(format!("{f} needs \"native\" (a type built into byob) or \"suffixes\" (the file names it covers)"));
        }
        let guidance = opt_str(o.get("guidance"), &format!("{f}.guidance"))?;
        if let Some(g) = &guidance
            && (g.is_empty() || !path_inside(g))
        {
            return Err(format!("{f}.guidance must be a file inside the plugin folder, like \"guidance.md\""));
        }
        let short = |key: &str| -> R<Option<String>> { Ok(opt_str(o.get(key), &format!("{f}.{key}"))?.map(|s| truncate_chars(&s, 400))) };
        let binary = match o.get("binary") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(format!("{f}.binary must be true or false")),
        };
        out.push(ManifestArtifactType {
            content_type,
            native,
            suffixes,
            label: short("label")?,
            describe: short("describe")?,
            pick: short("pick")?,
            guidance,
            binary,
        });
    }
    Ok(out)
}

fn parse_renderers(v: Option<&Value>) -> R<Vec<ManifestRenderer>> {
    let items = list(v, "renderers")?;
    if items.len() > MAX_TYPES {
        return Err(format!("renderers: at most {MAX_TYPES}"));
    }
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let f = format!("renderers[{i}]");
            let o = obj(item, &f)?;
            let content_type = req_str(o.get("type"), &format!("{f}.type"))?.to_ascii_lowercase();
            if !content_type_ok(&content_type) {
                return Err(format!("{f}.type must be a content type like \"text/csv\" (or \"image/*\")"));
            }
            let renderer = req_str(o.get("renderer"), &format!("{f}.renderer"))?;
            let ok = matches!(renderer.as_str(), "web" | "text")
                || renderer.strip_prefix("native:").is_some_and(native_id_ok)
                || renderer.strip_prefix("view:").is_some_and(byob_view_id_ok);
            if !ok {
                return Err(format!("{f}.renderer must be \"web\", \"text\", \"native:<id>\" or \"view:<view id>\""));
            }
            Ok(ManifestRenderer { content_type, renderer })
        })
        .collect()
}

fn parse_prompt_source(v: Option<&Value>) -> R<Option<PromptSource>> {
    let f = "system_prompt";
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() || !path_inside(s) {
                return Err(format!("{f} must be a file inside the plugin folder, like \"prompt.md\""));
            }
            Ok(Some(PromptSource::File(s.to_owned())))
        }
        Some(v) => {
            let o = obj(v, f)?;
            let n = req_str(o.get("native"), &format!("{f}.native"))?;
            if !native_id_ok(&n) {
                return Err(format!("{f}.native must be an id like \"artifacts\""));
            }
            Ok(Some(PromptSource::Native(n)))
        }
    }
}

/// A view id: `[a-z0-9_-]{1,48}`.
fn byob_view_id_ok(id: &str) -> bool {
    (1..=48).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// `"views": [{"site": "pane", "id": "todo", "title": "To do", "icon": "list"}]`.
fn parse_views(v: Option<&Value>, plugin_name: &str) -> R<Vec<ManifestView>> {
    let items = list(v, "views")?;
    if items.len() > MAX_VIEWS {
        return Err(format!("views: at most {MAX_VIEWS}"));
    }
    let mut out: Vec<ManifestView> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let f = format!("views[{i}]");
        let o = obj(item, &f)?;
        let site = req_str(o.get("site"), &format!("{f}.site"))?;
        if !VIEW_SITES.contains(&site.as_str()) {
            return Err(format!("{f}.site must be one of {}", VIEW_SITES.join(", ")));
        }
        let id = match o.get("id") {
            None => site.clone(),
            v => req_str(v, &format!("{f}.id"))?,
        };
        if !byob_view_id_ok(&id) {
            return Err(format!("{f}.id must be lowercase letters, digits, _ or -, like \"todo\""));
        }
        if out.iter().any(|x| x.id == id) {
            return Err(format!("{f}.id {id:?} is used twice"));
        }
        let title = truncate_chars(&opt_str(o.get("title"), &format!("{f}.title"))?.filter(|t| !t.is_empty()).unwrap_or_else(|| plugin_name.to_owned()), 80);
        let icon = opt_str(o.get("icon"), &format!("{f}.icon"))?.unwrap_or_default();
        out.push(ManifestView { id, site, title, icon });
    }
    Ok(out)
}

/// `"backend": "backend.py"`, `{"run": "backend.py", "args": [], "events": []}` or
/// `{"command": "node", "args": ["server.js"]}`.
fn parse_backend(v: Option<&Value>) -> R<Option<BackendDecl>> {
    let f = "backend";
    let script = |p: String| -> R<BackendRun> {
        if !path_inside(&p) || p.is_empty() {
            return Err(format!("{f}.run must be a path inside the plugin folder, like \"backend.py\""));
        }
        Ok(BackendRun::Python(p))
    };
    let o = match v {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(s)) => return Ok(Some(BackendDecl { run: script(s.trim().to_string())?, args: Vec::new(), events: Vec::new() })),
        Some(v) => obj(v, f)?,
    };
    let run = match (o.get("run"), o.get("command")) {
        (Some(r), None) => script(req_str(Some(r), &format!("{f}.run"))?)?,
        (None, Some(c)) => BackendRun::Command(req_str(Some(c), &format!("{f}.command"))?),
        _ => return Err(format!("{f} needs \"run\" (a Python script) or \"command\" (a program), one of them")),
    };
    let strings = |key: &str| -> R<Vec<String>> {
        list(o.get(key), &format!("{f}.{key}"))?
            .iter()
            .enumerate()
            .map(|(i, a)| a.as_str().map(str::to_string).ok_or_else(|| format!("{f}.{key}[{i}] must be text")))
            .collect()
    };
    let args = strings("args")?;
    let events = strings("events")?;
    if let Some(e) = events.iter().find(|e| !BACKEND_EVENTS.contains(&e.as_str())) {
        return Err(format!("{f}.events: {e:?} isn't an engine event; use {}", BACKEND_EVENTS.join(", ")));
    }
    Ok(Some(BackendDecl { run, args, events }))
}

/// `"python": {"requirements": ["requests>=2", "./wheels/x.whl"]}`.
fn parse_requirements(v: Option<&Value>) -> R<Vec<String>> {
    let Some(v) = v else { return Ok(Vec::new()) };
    let o = obj(v, "python")?;
    let reqs = list(o.get("requirements"), "python.requirements")?;
    if reqs.len() > MAX_REQUIREMENTS {
        return Err(format!("python.requirements: at most {MAX_REQUIREMENTS} packages"));
    }
    reqs.iter()
        .enumerate()
        .map(|(i, r)| {
            let r = req_str(Some(r), &format!("python.requirements[{i}]"))?;
            // one pip spec each, never options (--index-url …)
            if r.starts_with('-') || r.contains(['\n', '\r']) || r.len() > 512 {
                return Err(format!("python.requirements[{i}] must be one package spec like \"requests>=2.31\""));
            }
            Ok(r)
        })
        .collect()
}

fn parse_tool(t: &Value, i: usize) -> R<ManifestTool> {
    let f = format!("tools[{i}]");
    let t = obj(t, &f)?;
    let native = match t.get("native") {
        None => None,
        n => Some(req_str(n, &format!("{f}.native"))?),
    };
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
        Some(Value::String(s)) if s.trim() == "backend" => Some(String::new()),
        v => {
            let p = req_str(v, &format!("{f}.run"))?;
            if !path_inside(&p) {
                return Err(format!("{f}.run must be a path inside the plugin folder"));
            }
            Some(p)
        }
    };
    let kind = match (run, native) {
        (Some(r), _) if r.is_empty() => ToolKind::Backend,
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
    let optional = t.get("optional") == Some(&Value::Bool(true));
    Ok(ManifestTool { name, description, params, kind, title, optional })
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
            (json!([]), "byob-plugin.json: must be a JSON object"),
            (json!({"name": "x"}), "byob-plugin.json: id must be text"),
            (json!({"id": "Bad_Id", "name": "x"}), "byob-plugin.json: id must be lowercase letters, digits and dashes, like \"trip-planner\""),
            (json!({"id": "a", "name": "x", "tools": {}}), "byob-plugin.json: tools must be a list"),
            (
                json!({"id": "a", "name": "x", "tools": [{"name": "t", "description": "d", "run": "../escape.py"}]}),
                "byob-plugin.json: tools[0].run must be a path inside the plugin folder",
            ),
            (
                json!({"id": "a", "name": "x", "tools": [{"name": "t", "description": "d", "run": "/abs.py"}]}),
                "byob-plugin.json: tools[0].run must be a path inside the plugin folder",
            ),
            (
                json!({"id": "a", "name": "x", "tools": [{"name": "t", "description": "d"}]}),
                "byob-plugin.json: tools[0].run must name the Python script that runs this tool",
            ),
            (
                json!({"id": "a", "name": "x", "tools": [{"name": "1t", "description": "d", "run": "t.py"}]}),
                "byob-plugin.json: tools[0].name must be letters, digits or _, starting with a letter",
            ),
            (
                json!({"id": "a", "name": "x", "artifact_types": [{"type": "csv"}]}),
                "byob-plugin.json: artifact_types[0].type must be a content type like \"text/csv\"",
            ),
            (
                json!({"id": "a", "name": "x", "artifact_types": [{"type": "text/csv"}]}),
                "byob-plugin.json: artifact_types[0] needs \"native\" (a type built into byob) or \"suffixes\" (the file names it covers)",
            ),
            (
                json!({"id": "a", "name": "x", "renderers": [{"type": "text/csv", "renderer": "wasm"}]}),
                "byob-plugin.json: renderers[0].renderer must be \"web\", \"text\", \"native:<id>\" or \"view:<view id>\"",
            ),
            (
                json!({"id": "a", "name": "x", "system_prompt": "../p.md"}),
                "byob-plugin.json: system_prompt must be a file inside the plugin folder, like \"prompt.md\"",
            ),
            (
                json!({"id": "a", "name": "x", "settings": [{"key": "api_key", "label": "k"}]}),
                "byob-plugin.json: settings[0].key must be UPPER_CASE, like \"API_KEY\"",
            ),
            (
                json!({"id": "a", "name": "x", "settings": [{"key": "K", "label": "k", "type": "color"}]}),
                "byob-plugin.json: settings[0].type must be one of text, number, boolean, directory, file",
            ),
            (json!({"id": "a", "name": "x", "mcpServers": {"s": {"url": "ftp://x"}}}), "byob-plugin.json: mcpServers.s.url must start with https://"),
            (
                json!({"id": "a", "name": "x", "mcpServers": {"s": {"command": "x", "env": {"A": 1}}}}),
                "byob-plugin.json: mcpServers.s.env must map names to text",
            ),
            (json!({"id": "a", "name": "x", "mcpServers": {"s": {}}}), "byob-plugin.json: mcpServers.s.command must be text"),
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
    fn backends_and_requirements() {
        let m = parse_user(json!({"id": "a", "name": "x", "backend": "backend.py", "tools": [{"name": "t", "description": "d", "run": "backend"}],
            "python": {"requirements": ["requests>=2", "./wheels/w.whl"]}}))
        .unwrap();
        assert_eq!(m.backend.unwrap().run, BackendRun::Python("backend.py".into()));
        assert_eq!(m.tools[0].kind, ToolKind::Backend);
        assert_eq!(m.requirements, ["requests>=2", "./wheels/w.whl"]);
        let m = parse_user(json!({"id": "a", "name": "x", "backend": {"command": "node", "args": ["s.js"], "events": ["turn_finished"]}})).unwrap();
        let b = m.backend.unwrap();
        assert_eq!((b.run, b.args, b.events), (BackendRun::Command("node".into()), vec!["s.js".to_string()], vec!["turn_finished".to_string()]));
        let cases = [
            (
                json!({"id": "a", "name": "x", "tools": [{"name": "t", "description": "d", "run": "backend"}]}),
                "byob-plugin.json: tools[0].run is \"backend\" but the plugin has no \"backend\"",
            ),
            (
                json!({"id": "a", "name": "x", "backend": "../b.py"}),
                "byob-plugin.json: backend.run must be a path inside the plugin folder, like \"backend.py\"",
            ),
            (
                json!({"id": "a", "name": "x", "backend": {"run": "b.py", "command": "node"}}),
                "byob-plugin.json: backend needs \"run\" (a Python script) or \"command\" (a program), one of them",
            ),
            (
                json!({"id": "a", "name": "x", "backend": {"run": "b.py", "events": ["tick"]}}),
                "byob-plugin.json: backend.events: \"tick\" isn't an engine event; use chat_opened, turn_started, turn_finished, step_done, artifact_updated",
            ),
            (
                json!({"id": "a", "name": "x", "python": {"requirements": ["--index-url=http://x"]}}),
                "byob-plugin.json: python.requirements[0] must be one package spec like \"requests>=2.31\"",
            ),
        ];
        for (raw, want) in cases {
            assert_eq!(parse_user(raw.clone()).unwrap_err(), want, "for {raw}");
        }
    }

    #[test]
    fn views_at_render_sites() {
        let m = parse_user(json!({"id": "a", "name": "Todo", "backend": "b.py",
            "views": [{"site": "pane", "id": "list", "title": "To do", "icon": "list"}, {"site": "status"}]}))
        .unwrap();
        assert_eq!(m.views[0], ManifestView { id: "list".into(), site: "pane".into(), title: "To do".into(), icon: "list".into() });
        assert_eq!((m.views[1].id.as_str(), m.views[1].title.as_str()), ("status", "Todo"), "id defaults to the site, title to the name");
        let cases = [
            (
                json!({"id": "a", "name": "x", "views": [{"site": "pane"}]}),
                "byob-plugin.json: views are drawn by the plugin's backend: add \"backend\": \"backend.py\"",
            ),
            (
                json!({"id": "a", "name": "x", "backend": "b.py", "views": [{"site": "sidebar"}]}),
                "byob-plugin.json: views[0].site must be one of pane, settings, status, card, artifact",
            ),
            (
                json!({"id": "a", "name": "x", "backend": "b.py", "views": [{"site": "pane", "id": "My View"}]}),
                "byob-plugin.json: views[0].id must be lowercase letters, digits, _ or -, like \"todo\"",
            ),
            (
                json!({"id": "a", "name": "x", "backend": "b.py", "views": [{"site": "pane"}, {"site": "pane"}]}),
                "byob-plugin.json: views[1].id \"pane\" is used twice",
            ),
        ];
        for (raw, want) in cases {
            assert_eq!(parse_user(raw.clone()).unwrap_err(), want, "for {raw}");
        }
    }

    #[test]
    fn native_bindings() {
        // Any plugin may name in-tree parts (a fork of a bundled plugin keeps them); the
        // engine resolves the ids and reports unknown ones as problems.
        let raw = json!({"id": "code", "name": "Code", "locked": true,
            "tools": [{"native": "run_python"}, {"native": "install_packages", "optional": true}],
            "artifact_types": [{"type": "text/html", "native": "html_app", "guidance": "guidance.md"},
                               {"type": "Text/CSV", "suffixes": [".CSV"], "label": "table", "pick": "a table → .csv"}],
            "renderers": [{"type": "text/html", "renderer": "web"}, {"type": "image/*", "renderer": "native:image"}],
            "system_prompt": {"native": "artifacts"}});
        let m = parse_user(raw).unwrap();
        assert_eq!(m.tools[0].name, "run_python");
        assert_eq!(m.tools[0].kind, ToolKind::Native("run_python".into()));
        assert!(!m.tools[0].optional && m.tools[1].optional && m.locked);
        assert_eq!(m.artifact_types[0].native.as_deref(), Some("html_app"));
        assert_eq!(m.artifact_types[0].guidance.as_deref(), Some("guidance.md"));
        assert_eq!((m.artifact_types[1].content_type.as_str(), m.artifact_types[1].suffixes.clone()), ("text/csv", Some(vec![".csv".to_string()])));
        assert_eq!(m.renderers[1], ManifestRenderer { content_type: "image/*".into(), renderer: "native:image".into() });
        assert_eq!(m.system_prompt, Some(PromptSource::Native("artifacts".into())));
        let m = parse_user(json!({"id": "a", "name": "x", "content_types": [{"type": "text/x", "native": "text"}], "system_prompt": "prompt.md"})).unwrap();
        assert_eq!(m.artifact_types[0].native.as_deref(), Some("text"));
        assert_eq!(m.system_prompt, Some(PromptSource::File("prompt.md".into())));
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

    #[test]
    fn hooks_and_environments() {
        let parse = |extra: serde_json::Value| {
            let mut m = serde_json::json!({"id": "p", "name": "P"});
            for (k, v) in extra.as_object().unwrap() {
                m[k] = v.clone();
            }
            parse_manifest(&m, ParseOptions::default())
        };
        let m = parse(serde_json::json!({"backend": "b.py", "hooks": [
            {"event": "before_tool_call", "priority": -5, "tools": ["delete_file"], "fail": "closed", "timeout_ms": 500},
            {"event": "before_request", "native": "compaction"}
        ]}))
        .unwrap();
        assert_eq!(
            m.hooks[0],
            ManifestHook {
                event: "before_tool_call".into(),
                priority: -5,
                native: None,
                timeout_ms: 500,
                fail_closed: true,
                tools: vec!["delete_file".into()]
            }
        );
        assert_eq!((m.hooks[1].priority, m.hooks[1].timeout_ms, m.hooks[1].fail_closed), (DEFAULT_HOOK_PRIORITY, HOOK_TIMEOUT_MS, false));
        let err = |v| parse(v).unwrap_err().message;
        assert!(err(serde_json::json!({"hooks": [{"event": "before_lunch", "native": "x"}]})).contains("event must be one of"));
        assert!(err(serde_json::json!({"hooks": [{"event": "on_turn_end"}]})).contains("add \"backend\""));
        assert!(err(serde_json::json!({"hooks": [{"event": "before_prompt", "native": "x", "tools": ["a"]}]})).contains("tools only applies"));
        assert!(err(serde_json::json!({"backend": "b.py", "hooks": [{"event": "on_turn_end", "timeout_ms": 0}]})).contains("timeout_ms"));
        assert!(err(serde_json::json!({"backend": "b.py", "hooks": [{"event": "on_turn_end", "fail": "maybe"}]})).contains("\"open\" or \"closed\""));
        let m = parse(serde_json::json!({"environments": [
            {"native": "nix"},
            {"id": "conda", "detect": ["environment.yml"], "command": "python3", "args": ["env.py"]}
        ]}))
        .unwrap();
        assert_eq!((m.environments[0].id.as_str(), m.environments[0].native.as_deref()), ("nix", Some("nix")));
        assert_eq!((m.environments[1].label.as_str(), m.environments[1].command.as_deref()), ("conda environment", Some("python3")));
        assert!(err(serde_json::json!({"environments": [{"id": "x", "detect": ["a"]}]})).contains("needs \"native\""));
        assert!(err(serde_json::json!({"environments": [{"id": "x", "detect": ["../a"], "command": "c"}]})).contains("inside the workspace"));
    }
}
