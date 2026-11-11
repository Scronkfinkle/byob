//! The model's plugin-authoring tools: `make_tool`, `edit_plugin`, `write_plugin_file`,
//! `plugin_status`, `keep_plugin`, `remove_plugin` and `save_skill`.
//!
//! They do their file work on the turn thread, then ask the engine to reload the chat's
//! plugins ([`crate::plugins::PluginOp`]); the engine swaps the running turn's tools, so a
//! tool made in one round is callable in the next. Changes to this chat's scratch plugins
//! are never asked about; changes to a workspace's or the global plugins ask the user (an
//! approval card), and keeping a plugin everywhere can only be allowed by a person.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;

use byob_plugins::authoring::{RUNNER_MARK, ToolDecl, WantedSetting, derive_schema, last_lines, tool_script, upsert_tool, write_manifest};
use byob_plugins::manifest::{path_inside, plugin_id_ok, tool_name_ok};
use byob_plugins::skills::{NewSkill, save_skill};
use byob_protocol::{PluginAction, PluginCard, PluginInfo, PluginOrigin, PluginScope, PluginTest};
use byob_sandbox::Sandbox;
use byob_tools::{Access, Question, Tool, ToolContext, ToolOutput, ToolSpec, parse_args};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Input;
use crate::extensions::MADE_MARKER;
use crate::plugins::{PluginOp, PluginReply, request};

/// The plugin `make_tool` uses when none is named.
pub const SCRATCH: &str = "scratch";
/// Most bytes of a plugin file written or edited at once.
const MAX_FILE_BYTES: usize = 200_000;
/// Most characters of a test run's output shown.
const MAX_TEST_OUTPUT: usize = 4000;
/// Lines of a failed test run's output shown (the end of a traceback).
const TRACEBACK_LINES: usize = 40;
/// Most files listed when keeping a plugin.
const MAX_LISTED: usize = 40;

/// What the authoring tools share.
pub(crate) struct AuthorEnv {
    pub inbox: Sender<Input>,
    pub python: Vec<OsString>,
    pub sandbox: Option<Sandbox>,
    /// Standalone skills kept everywhere (`<config>/skills`).
    pub global_skills: Option<PathBuf>,
}

impl AuthorEnv {
    fn ask(&self, ctx: &ToolContext<'_>, op: PluginOp) -> Result<PluginReply, String> {
        let chat = ctx.chat().ok_or("plugins need a chat")?;
        request(&self.inbox, chat, op, ctx.cancel_flag())
    }

    fn card(&self, ctx: &ToolContext<'_>, tool: &str, card: PluginCard) {
        if let Some(chat) = ctx.chat() {
            let _ = self.inbox.send(Input::StepView { chat: chat.to_owned(), tool: tool.to_owned(), view: byob_protocol::View::of(&card) });
        }
    }
}

/// Register the authoring tools.
pub(crate) fn register(tools: &mut byob_tools::Registry, env: &Arc<AuthorEnv>) {
    tools.register(Arc::new(MakeTool(Arc::clone(env))));
    tools.register(Arc::new(EditPlugin(Arc::clone(env))));
    tools.register(Arc::new(WritePluginFile(Arc::clone(env))));
    tools.register(Arc::new(PluginStatus(Arc::clone(env))));
    tools.register(Arc::new(KeepPlugin(Arc::clone(env))));
    tools.register(Arc::new(RemovePlugin(Arc::clone(env))));
    tools.register(Arc::new(SaveSkill(Arc::clone(env))));
}

/// Ask the user before changing something outside this chat.
fn confirm(ctx: &ToolContext<'_>, title: String, text: String, warning: Option<String>, always: bool) -> Result<(), String> {
    let a = ctx.ask(&Question { title, text, warning, access: Access::Write, always_ask: always });
    if a.allowed() {
        Ok(())
    } else if a.by_user() {
        Err("the user said no".into())
    } else {
        Err("not allowed here (nobody could confirm it)".into())
    }
}

/// Where a change to plugin `info` lands, for a question.
fn where_(info: &PluginInfo) -> &'static str {
    info.scope.label()
}

/// A plugin the chat can see, or a model-readable error listing the ones it has.
fn find<'a>(reply: &'a PluginReply, id: &str) -> Result<&'a PluginInfo, String> {
    reply.find(id).ok_or_else(|| {
        let have: Vec<&str> = reply.infos.iter().filter(|p| p.shadowed_by.is_none()).map(|p| p.id.as_str()).collect();
        format!("there's no plugin {id:?} here. Plugins: {}.", if have.is_empty() { "none".to_owned() } else { have.join(", ") })
    })
}

/// Plugin `id` the chat uses, or, if it doesn't load (a broken manifest only shows up as a
/// load error), its folder, so the authoring tools can still read, fix or remove it.
fn locate(reply: &PluginReply, id: &str) -> Result<PluginInfo, String> {
    if let Some(info) = reply.find(id) {
        return Ok(info.clone());
    }
    broken(reply, id).ok_or_else(|| find(reply, id).err().unwrap_or_default())
}

/// The folder of plugin `id` if it failed to load, in one of the places plugins are written
/// (this chat's, a workspace's `.byob/plugins`, or the global folder; never byob's own).
fn broken(reply: &PluginReply, id: &str) -> Option<PluginInfo> {
    reply.errors.iter().find_map(|e| {
        let dir = Path::new(&e.path);
        let parent = dir.parent()?;
        let ours = parent == reply.scratch_dir.as_path() || reply.global_dir.as_deref() == Some(parent) || parent.ends_with(".byob/plugins");
        if dir.file_name()?.to_str()? != id || !ours {
            return None;
        }
        let mut info = new_info(id, dir);
        info.scope = e.scope;
        info.origin = match e.scope {
            PluginScope::Chat => PluginOrigin::Agent,
            PluginScope::Workspace => PluginOrigin::Repo,
            _ => PluginOrigin::User,
        };
        info.problems = vec![e.message.clone()];
        Some(info)
    })
}

fn writable(info: &PluginInfo) -> Result<(), String> {
    if info.origin == PluginOrigin::Bundled {
        return Err(format!("{} ships with byob and can't be changed", info.id));
    }
    Ok(())
}

/// The loader's problems with the plugin in `dir` after a reload, verbatim.
fn problems(reply: &PluginReply, id: &str, dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = reply.find(id).filter(|p| Path::new(&p.dir) == dir).map(|p| p.problems.clone()).unwrap_or_default();
    let shown = dir.display().to_string();
    out.extend(reply.errors.iter().filter(|e| e.path == shown).map(|e| e.message.clone()));
    out
}

fn tools_of(reply: &PluginReply, id: &str, dir: &Path) -> Vec<String> {
    reply.find(id).filter(|p| Path::new(&p.dir) == dir).map(|p| p.tools.clone()).unwrap_or_default()
}

fn diff(before: &[String], after: &[String]) -> (Vec<String>, Vec<String>) {
    let added = after.iter().filter(|t| !before.contains(t)).cloned().collect();
    let removed = before.iter().filter(|t| !after.contains(t)).cloned().collect();
    (added, removed)
}

fn missing_settings(reply: &PluginReply, id: &str) -> Vec<String> {
    reply.find(id).map(|p| p.settings.iter().filter(|s| s.required && !s.set).map(|s| s.label.clone()).collect()).unwrap_or_default()
}

/// "Problems: …" lines for the model (empty if none).
fn problem_text(list: &[String]) -> String {
    if list.is_empty() { String::new() } else { format!("\nProblems the loader reports:\n- {}", list.join("\n- ")) }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or("")
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("\n…(cut)");
    out
}

/// A one-line summary of a JSON Schema's parameters for the model.
fn params_line(params: &Value) -> String {
    let required: Vec<&str> = params.get("required").and_then(Value::as_array).map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let Some(props) = params.get("properties").and_then(Value::as_object) else { return "none".into() };
    if props.is_empty() {
        return "none".into();
    }
    props
        .iter()
        .map(|(k, v)| {
            let ty = v.get("type").and_then(Value::as_str).unwrap_or("any");
            match (required.contains(&k.as_str()), v.get("default")) {
                (true, _) => format!("{k} ({ty}, required)"),
                (false, Some(d)) => format!("{k} ({ty}, default {d})"),
                (false, None) => format!("{k} ({ty}, optional)"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// ---- make_tool --------------------------------------------------------------------------

struct MakeTool(Arc<AuthorEnv>);

#[derive(Deserialize)]
struct MakeArgs {
    name: String,
    #[serde(default)]
    description: Option<String>,
    code: String,
    #[serde(default)]
    try_args: Option<Value>,
    #[serde(default)]
    network: bool,
    #[serde(default)]
    settings: Vec<Value>,
    #[serde(default)]
    plugin: Option<String>,
}

impl Tool for MakeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "make_tool".into(),
            description: "Make yourself a new tool from Python code: define `def run(arg: type = default, ...)` returning a string or JSON-able value. \
The parameters' type hints become the tool's schema. The tool is callable from your next step. Pass try_args to test it at once; \
call again with the same name to fix it."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Tool name: letters, digits and _, e.g. get_weather"},
                    "description": {"type": "string", "description": "What the tool does, for you later (default: the docstring)"},
                    "code": {"type": "string", "description": "Python source defining def run(...). Standard library only unless packages are installed. Settings arrive in os.environ."},
                    "try_args": {"type": "object", "description": "Arguments for a test run right away, e.g. {\"city\": \"Paris\"}"},
                    "network": {"type": "boolean", "description": "true if the code needs the internet"},
                    "settings": {"type": "array", "items": {"type": "string"}, "description": "Environment variables the user must fill in (API keys), e.g. [\"WEATHER_API_KEY\"]"},
                    "plugin": {"type": "string", "description": "Plugin id to put it in (lowercase, dashes); default: this chat's scratch plugin"}
                },
                "required": ["name", "code"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Made the tool {}", str_arg(args, "name"))
    }

    fn pending_title(&self, args: &Value) -> String {
        format!("Make the tool {}", str_arg(args, "name"))
    }

    /// Writing the chat's own scratch plugin needs no approval; running the test does.
    fn access(&self, args: &Value) -> Access {
        if args.get("try_args").is_some_and(|a| !a.is_null()) { Access::Execute } else { Access::Read }
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        match self.make(args, ctx) {
            Ok(out) => out,
            Err(e) => ToolOutput::err(e),
        }
    }
}

impl MakeTool {
    fn make(&self, args: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, String> {
        let a: MakeArgs = parse_args(args)?;
        let name = a.name.trim().to_owned();
        if !tool_name_ok(&name) {
            return Err(format!("{name:?} can't be a tool name: start with a letter, then letters, digits and _ (at most 64)"));
        }
        if a.code.trim().is_empty() {
            return Err("write the code: a Python def run(...)".into());
        }
        let settings = a.settings.iter().map(WantedSetting::parse).collect::<Result<Vec<_>, _>>()?;
        let id = a.plugin.as_deref().map_or(SCRATCH, str::trim).to_owned();
        if !plugin_id_ok(&id) {
            return Err(format!("plugin {id:?}: use lowercase letters, digits and dashes, like \"weather-tools\""));
        }
        let derived = derive_schema(&a.code, &self.0.python, self.0.sandbox.as_ref())?;
        let description = a.description.map(|d| d.trim().to_owned()).filter(|d| !d.is_empty()).or_else(|| Some(derived.doc.clone()).filter(|d| !d.is_empty()));
        let description = description.unwrap_or_else(|| format!("{} (a tool made in a chat)", name.replace('_', " ")));
        let before = self.0.ask(ctx, PluginOp::Status)?;
        let (dir, scope) = match before.find(&id) {
            Some(info) => {
                writable(info)?;
                if info.scope != PluginScope::Chat {
                    confirm(ctx, format!("Change the plugin {id} ({})", where_(info)), format!("Add or replace its tool {name}."), None, false)?;
                }
                (PathBuf::from(&info.dir), info.scope)
            }
            None => (before.scratch_dir.join(&id), PluginScope::Chat),
        };
        let old_tools = tools_of(&before, &id, &dir);
        let script = format!("tools/{name}.py");
        fs::create_dir_all(dir.join("tools")).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
        fs::write(dir.join(&script), tool_script(&a.code)).map_err(|e| format!("couldn't write {script}: {e}"))?;
        let decl = ToolDecl { name: name.clone(), description, params: derived.params.clone(), run: script, network: a.network, settings };
        let replaced = upsert_tool(&dir, &id, &decl)?;
        let after = self.0.ask(ctx, PluginOp::Reload)?;
        let found = problems(&after, &id, &dir);
        let new_tools = tools_of(&after, &id, &dir);
        let (added, removed) = diff(&old_tools, &new_tools);
        let missing = missing_settings(&after, &id);
        let mut text = format!(
            "{} tool `{name}` in plugin `{id}` ({}). Parameters: {}.",
            if replaced { "Replaced the" } else { "Made the" },
            scope.label(),
            params_line(&derived.params)
        );
        let mut out = ToolOutput::default();
        let mut test = None;
        match after.tool_named(&dir, &name) {
            Some(model_name) => {
                text.push_str(&format!(" Call it as `{model_name}` from your next step."));
                if let Some(t) = a.try_args.filter(|t| !t.is_null()) {
                    if !t.is_object() {
                        return Err("try_args must be an object of arguments, like {\"city\": \"Paris\"}".into());
                    }
                    match after.tools.get(&model_name) {
                        Some(tool) => {
                            let r = tool.run(&t, ctx);
                            let shown = if r.failed { last_lines(&r.text, TRACEBACK_LINES) } else { clip(&r.text, MAX_TEST_OUTPUT) };
                            if r.failed {
                                text.push_str(&format!(
                                    "\n\nTest {model_name}({t}) FAILED:\n{shown}\n\nFix the code and call make_tool again with the same name."
                                ));
                                out.failed = true;
                            } else {
                                text.push_str(&format!("\n\nTest {model_name}({t}) worked:\n{shown}"));
                            }
                            out.changes = r.changes;
                            test = Some(PluginTest { ok: !r.failed, output: shown });
                        }
                        None => text.push_str("\n\n(The test didn't run: the tool isn't loaded.)"),
                    }
                }
            }
            None => {
                let off = after.find(&id).is_some_and(|p| !p.enabled);
                text.push_str(if off {
                    "\nThe plugin is switched off for this chat, so the tool isn't offered. Ask the user to switch it on."
                } else {
                    "\nThe tool isn't loaded (see the problems below)."
                });
                out.failed = true;
            }
        }
        if !missing.is_empty() {
            text.push_str(&format!(
                "\n\nIt needs settings only the user can fill in: {}. Ask the user to fill them in on the plugin's card; never ask for secrets in the chat.",
                missing.join(", ")
            ));
        }
        text.push_str(&problem_text(&found));
        if scope == PluginScope::Chat {
            text.push_str(&format!("\nIt lives in this chat only. If the user wants it later too, use keep_plugin (plugin \"{id}\")."));
        }
        let card =
            PluginCard { plugin: id, scope, action: PluginAction::Made, tools: new_tools, added, removed, test, problems: found, missing_settings: missing };
        self.0.card(ctx, "make_tool", card);
        out.text = text;
        Ok(out)
    }
}

// ---- edit_plugin / write_plugin_file ----------------------------------------------------

struct EditPlugin(Arc<AuthorEnv>);

#[derive(Deserialize)]
struct EditArgs {
    plugin: String,
    file: String,
    find: String,
    #[serde(default)]
    replace: String,
}

/// The file `rel` of a plugin, checked to stay inside it.
fn plugin_file(dir: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim().trim_start_matches("./");
    if rel.is_empty() || !path_inside(rel) {
        return Err(format!("{rel:?} must be a path inside the plugin folder, like tools/weather.py"));
    }
    Ok(dir.join(rel))
}

/// Reload after an edit and tell the model (and the card) what the loader says.
fn after_edit(env: &AuthorEnv, ctx: &ToolContext<'_>, tool: &str, info_before: &PluginInfo, before: &PluginReply, what: String) -> Result<ToolOutput, String> {
    let dir = PathBuf::from(&info_before.dir);
    let after = env.ask(ctx, PluginOp::Reload)?;
    let found = problems(&after, &info_before.id, &dir);
    let old = tools_of(before, &info_before.id, &dir);
    let new = tools_of(&after, &info_before.id, &dir);
    let (added, removed) = diff(&old, &new);
    let loads = after.find(&info_before.id).is_some_and(|p| Path::new(&p.dir) == dir);
    let mut text = what;
    if loads {
        text.push_str(&format!(" The plugin reloaded; its tools: {}.", if new.is_empty() { "none".to_owned() } else { new.join(", ") }));
    } else {
        text.push_str(" The plugin no longer loads.");
    }
    text.push_str(&problem_text(&found));
    let card = PluginCard {
        plugin: info_before.id.clone(),
        scope: info_before.scope,
        action: PluginAction::Edited,
        tools: new,
        added,
        removed,
        test: None,
        problems: found.clone(),
        missing_settings: missing_settings(&after, &info_before.id),
    };
    env.card(ctx, tool, card);
    Ok(if loads && found.is_empty() { ToolOutput::ok(text) } else { ToolOutput::err(text) })
}

impl Tool for EditPlugin {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_plugin".into(),
            description:
                "Replace one exact snippet in a file of a plugin (its manifest, a tool script, a skill), then reload it and see the loader's problems.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "plugin": {"type": "string", "description": "Plugin id, e.g. scratch"},
                    "file": {"type": "string", "description": "Path inside the plugin, e.g. tools/weather.py or byob-plugin.json"},
                    "find": {"type": "string", "description": "Exact text to replace; must occur once"},
                    "replace": {"type": "string", "description": "New text"}
                },
                "required": ["plugin", "file", "find", "replace"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Edited {} in plugin {}", str_arg(args, "file"), str_arg(args, "plugin"))
    }

    fn pending_title(&self, args: &Value) -> String {
        format!("Edit {} in plugin {}", str_arg(args, "file"), str_arg(args, "plugin"))
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let run = || -> Result<ToolOutput, String> {
            let a: EditArgs = parse_args(args)?;
            let before = self.0.ask(ctx, PluginOp::Status)?;
            let info = locate(&before, a.plugin.trim())?;
            writable(&info)?;
            let file = plugin_file(Path::new(&info.dir), &a.file)?;
            if a.find.is_empty() {
                return Err("`find` can't be empty; to write a whole file use write_plugin_file".into());
            }
            let meta = fs::metadata(&file).map_err(|_| format!("{} doesn't exist in plugin {}", a.file, info.id))?;
            if meta.len() > MAX_FILE_BYTES as u64 {
                return Err(format!("{} is too big to edit here", a.file));
            }
            let text = fs::read_to_string(&file).map_err(|e| format!("{}: {e}", a.file))?;
            match text.matches(&a.find).count() {
                0 => {
                    return Err(format!("the text to find isn't in {}. Read it with plugin_status (plugin and file) and copy the snippet exactly.", a.file));
                }
                1 => {}
                n => return Err(format!("the text to find occurs {n} times in {}; include more surrounding lines so it is unique", a.file)),
            }
            if info.scope != PluginScope::Chat {
                confirm(
                    ctx,
                    format!("Edit {} of plugin {} ({})", a.file, info.id, where_(&info)),
                    format!("Replace:\n{}\nwith:\n{}", clip(&a.find, 600), clip(&a.replace, 600)),
                    None,
                    false,
                )?;
            }
            let new = text.replacen(&a.find, &a.replace, 1);
            fs::write(&file, new).map_err(|e| format!("couldn't write {}: {e}", a.file))?;
            after_edit(&self.0, ctx, "edit_plugin", &info, &before, format!("Edited {} of plugin {}.", a.file, info.id))
        };
        run().unwrap_or_else(ToolOutput::err)
    }
}

struct WritePluginFile(Arc<AuthorEnv>);

#[derive(Deserialize)]
struct WriteArgs {
    plugin: String,
    path: String,
    content: String,
}

impl Tool for WritePluginFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_plugin_file".into(),
            description: "Create or overwrite a file in a plugin (byob-plugin.json, tools/x.py, skills/<name>/SKILL.md, agents/<name>.md), then reload it and see the loader's problems. A new plugin id starts a new plugin in this chat.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "plugin": {"type": "string", "description": "Plugin id (lowercase letters, digits, dashes)"},
                    "path": {"type": "string", "description": "Path inside the plugin, e.g. skills/report/SKILL.md"},
                    "content": {"type": "string", "description": "The whole file"}
                },
                "required": ["plugin", "path", "content"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Wrote {} in plugin {}", str_arg(args, "path"), str_arg(args, "plugin"))
    }

    fn pending_title(&self, args: &Value) -> String {
        format!("Write {} in plugin {}", str_arg(args, "path"), str_arg(args, "plugin"))
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let run = || -> Result<ToolOutput, String> {
            let a: WriteArgs = parse_args(args)?;
            let id = a.plugin.trim();
            if !plugin_id_ok(id) {
                return Err(format!("plugin {id:?}: use lowercase letters, digits and dashes, like \"weather-tools\""));
            }
            if a.content.len() > MAX_FILE_BYTES {
                return Err(format!("that file is too big (at most {} KB)", MAX_FILE_BYTES / 1000));
            }
            let before = self.0.ask(ctx, PluginOp::Status)?;
            // A broken plugin of that id gets fixed in place, not shadowed by a new one.
            let info = match before.find(id).cloned().or_else(|| broken(&before, id)) {
                Some(info) => info,
                None => new_info(id, &before.scratch_dir.join(id)),
            };
            writable(&info)?;
            let dir = PathBuf::from(&info.dir);
            let file = plugin_file(&dir, &a.path)?;
            let rel = a.path.trim().trim_start_matches("./").to_owned();
            if info.scope != PluginScope::Chat {
                confirm(ctx, format!("Write {rel} in plugin {} ({})", info.id, where_(&info)), clip(&a.content, 1200), None, false)?;
            }
            let mut content = a.content.clone();
            let mut note = String::new();
            if rel.starts_with("tools/") && rel.ends_with(".py") && content.contains("def run(") && !content.contains(RUNNER_MARK) {
                content = tool_script(&content);
                note.push_str(" (byob's runner footer was added so run() gets its arguments)");
            }
            if let Some(parent) = file.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("couldn't create {}: {e}", parent.display()))?;
            }
            if rel != byob_plugins::MANIFEST && !dir.join(byob_plugins::MANIFEST).exists() {
                write_manifest(&dir, &byob_plugins::authoring::new_manifest(id, id, "Made in a chat"))?;
                note.push_str(&format!(" A new plugin {id} was started in this chat with a minimal byob-plugin.json."));
            }
            fs::write(&file, content).map_err(|e| format!("couldn't write {rel}: {e}"))?;
            after_edit(&self.0, ctx, "write_plugin_file", &info, &before, format!("Wrote {rel} in plugin {id}{note}."))
        };
        run().unwrap_or_else(ToolOutput::err)
    }
}

/// How a plugin about to be created in the chat's scratch folder is described.
fn new_info(id: &str, dir: &Path) -> PluginInfo {
    PluginInfo {
        id: id.to_owned(),
        name: id.to_owned(),
        version: "0.1.0".into(),
        description: String::new(),
        icon: String::new(),
        scope: PluginScope::Chat,
        origin: PluginOrigin::Agent,
        dir: dir.display().to_string(),
        enabled: true,
        enabled_from: byob_types::config::Level::Default,
        trusted: true,
        declined: false,
        shadowed_by: None,
        tools: Vec::new(),
        skills: Vec::new(),
        agents: Vec::new(),
        commands: Vec::new(),
        servers: Vec::new(),
        backend: None,
        views: Vec::new(),
        settings: Vec::new(),
        network: false,
        problems: Vec::new(),
        last_error: None,
        fingerprint: String::new(),
    }
}

// ---- plugin_status ----------------------------------------------------------------------

struct PluginStatus(Arc<AuthorEnv>);

/// Files of a plugin, relative, with sizes (for status and keep).
fn file_list(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), String::new(), 0usize)];
    while let Some((d, prefix, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "__pycache__" {
                continue;
            }
            let rel = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            match e.metadata() {
                Ok(m) if m.is_dir() && depth < 4 => stack.push((e.path(), rel, depth + 1)),
                Ok(m) if m.is_file() => out.push(format!("{rel} ({} bytes)", m.len())),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

fn describe(p: &PluginInfo, detail: bool) -> String {
    let state = match (p.shadowed_by, p.enabled, p.trusted) {
        (Some(s), _, _) => format!("not used: the copy in {} wins", s.label()),
        (None, false, _) => format!("switched off ({})", p.enabled_from.label()),
        (None, true, false) if p.declined => "its code is off: the user declined it".into(),
        (None, true, false) => "its code waits for the user's OK (it came with the workspace)".into(),
        (None, true, true) => "on".into(),
    };
    let origin = match p.origin {
        PluginOrigin::Bundled => "ships with byob",
        PluginOrigin::User => "the user's",
        PluginOrigin::Agent => "made by byob",
        PluginOrigin::Repo => "came with the workspace",
    };
    let list = |v: &[String]| if v.is_empty() { "none".to_owned() } else { v.join(", ") };
    let mut s = format!("- {} ({}; {}; {origin}; {state})\n  tools: {}", p.id, p.scope.label(), p.version, list(&p.tools));
    if !p.skills.is_empty() {
        s.push_str(&format!("; skills: {}", list(&p.skills)));
    }
    if !p.agents.is_empty() {
        s.push_str(&format!("; subagents: {}", list(&p.agents)));
    }
    for srv in &p.servers {
        s.push_str(&format!("\n  MCP server {}: {:?}{}", srv.name, srv.state, srv.error.as_ref().map(|e| format!(" ({e})")).unwrap_or_default()));
    }
    if !p.settings.is_empty() {
        let set: Vec<String> = p.settings.iter().map(|x| format!("{} {}", x.key, if x.set { "set" } else { "MISSING" })).collect();
        s.push_str(&format!("\n  settings: {}", set.join(", ")));
    }
    if p.network {
        s.push_str("\n  may use the internet");
    }
    for pr in &p.problems {
        s.push_str(&format!("\n  problem: {pr}"));
    }
    if let Some(e) = &p.last_error {
        s.push_str(&format!("\n  last error: {}", clip(e, 600)));
    }
    if let Some(b) = &p.backend {
        let starts = if b.starts > 1 { format!(", started {} times", b.starts) } else { String::new() };
        s.push_str(&format!("\n  backend: {}{starts}", b.state));
        if let Some(e) = &b.error {
            s.push_str(&format!("\n  backend error: {}", clip(e, 1500)));
        }
        for f in &b.faults {
            s.push_str(&format!("\n  backend fault: {}", clip(f, 800)));
        }
        let trouble = matches!(b.state.as_str(), "crashed" | "failed") || !b.faults.is_empty();
        let lines = if detail {
            25
        } else if trouble {
            8
        } else {
            0
        };
        let tail = byob_plugins::backend::last_lines(&b.stderr, lines);
        if lines > 0 && !tail.is_empty() {
            s.push_str(&format!("\n  backend stderr (last lines):\n    {}", tail.replace('\n', "\n    ")));
        }
    }
    if detail {
        s.push_str(&format!("\n  folder: {}\n  files: {}", p.dir, list(&file_list(Path::new(&p.dir)).into_iter().take(MAX_LISTED).collect::<Vec<_>>())));
    }
    s
}

impl Tool for PluginStatus {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plugin_status".into(),
            description: "Show your plugins (or one): where each lives, whether it's on, its tools, skills and subagents, missing settings, load problems and the last error. With plugin and file: show that file.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "plugin": {"type": "string", "description": "One plugin id (default: all)"},
                    "file": {"type": "string", "description": "A file of that plugin to show, e.g. tools/weather.py"}
                }
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        match str_arg(args, "plugin") {
            "" => "Checked the plugins".into(),
            p => format!("Checked the plugin {p}"),
        }
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let reply = match self.0.ask(ctx, PluginOp::Status) {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(e),
        };
        let asked = str_arg(args, "plugin").trim();
        let mut text = String::new();
        if asked.is_empty() {
            if reply.infos.is_empty() {
                text.push_str("No plugins yet. make_tool makes one in this chat.");
            }
            for p in &reply.infos {
                text.push_str(&describe(p, false));
                text.push('\n');
            }
        } else {
            let copies: Vec<&PluginInfo> = reply.infos.iter().filter(|p| p.id == asked).collect();
            if copies.is_empty() {
                let Some(b) = broken(&reply, asked) else { return ToolOutput::err(find(&reply, asked).err().unwrap_or_default()) };
                let file = str_arg(args, "file").trim();
                if file.is_empty() {
                    let files = plugin_files(Path::new(&b.dir));
                    return ToolOutput::err(format!(
                        "plugin {asked} ({}) doesn't load: {}\nIts files: {}. Read one with plugin_status (plugin, file), fix it with edit_plugin or write_plugin_file.",
                        b.dir,
                        b.problems.join("; "),
                        if files.is_empty() { "none".to_owned() } else { files.join(", ") }
                    ));
                }
                return read_plugin_file(&b, file);
            }
            let file = str_arg(args, "file").trim();
            if !file.is_empty() {
                let Some(p) = copies.iter().find(|p| p.shadowed_by.is_none()).or(copies.first()) else { return ToolOutput::err("no such plugin") };
                return read_plugin_file(p, file);
            }
            for p in copies {
                text.push_str(&describe(p, true));
                text.push('\n');
            }
        }
        for e in &reply.errors {
            text.push_str(&format!("- couldn't load {} ({}): {}\n", e.path, e.scope.label(), e.message));
        }
        ToolOutput::ok(text.trim_end().to_owned())
    }
}

/// `file` of plugin `p`, for plugin_status.
fn read_plugin_file(p: &PluginInfo, file: &str) -> ToolOutput {
    let path = match plugin_file(Path::new(&p.dir), file) {
        Ok(path) => path,
        Err(e) => return ToolOutput::err(e),
    };
    match fs::read(&path) {
        Ok(bytes) if bytes.len() > MAX_FILE_BYTES => ToolOutput::err(format!("{file} is too big to show")),
        Ok(bytes) => ToolOutput::ok(format!("{file} of plugin {}:\n{}", p.id, String::from_utf8_lossy(&bytes))),
        Err(e) => ToolOutput::err(format!("{file}: {e}")),
    }
}

/// Files in a plugin folder (relative, `/`-separated), for telling the model what's there.
fn plugin_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else { continue };
        for e in entries.flatten().take(200) {
            let path = e.path();
            // Not following symlinks: a looped link can't keep this going.
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(dir) {
                out.push(rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"));
            }
            if out.len() >= 50 {
                out.sort();
                return out;
            }
        }
    }
    out.sort();
    out
}

// ---- keep_plugin / remove_plugin ---------------------------------------------------------

/// Copy plugin folder `src` to `<target_root>/<id>` (renaming it to `id`), marking it as
/// byob's (`marker`), and delete `src` when `moving`. Returns the files copied.
pub(crate) fn keep_files(src: &Path, target_root: &Path, id: &str, marker: bool, moving: bool) -> Result<Vec<String>, String> {
    if !plugin_id_ok(id) {
        return Err(format!("{id:?} isn't a plugin id (lowercase letters, digits, dashes)"));
    }
    let dst = target_root.join(id);
    if dst.exists() {
        return Err(format!("there's already a plugin folder {}; pick another id (new_id) or remove that one first", dst.display()));
    }
    let files = file_list(src);
    copy_dir(src, &dst, 0).map_err(|e| {
        let _ = fs::remove_dir_all(&dst);
        format!("couldn't copy the plugin: {e}")
    })?;
    let mut manifest = byob_plugins::authoring::read_manifest_json(&dst, id)?;
    if manifest.get("id").and_then(Value::as_str) != Some(id) {
        if let Some(o) = manifest.as_object_mut() {
            o.insert("id".into(), Value::String(id.to_owned()));
            if o.get("name").and_then(Value::as_str) == Some(SCRATCH) {
                o.insert("name".into(), Value::String(id.to_owned()));
            }
        }
        write_manifest(&dst, &manifest)?;
    }
    if marker {
        let _ = fs::write(dst.join(MADE_MARKER), "made by byob\n");
    }
    if moving && let Err(e) = fs::remove_dir_all(src) {
        log::warn!("removing {} after keeping it: {e}", src.display());
    }
    Ok(files)
}

fn copy_dir(src: &Path, dst: &Path, depth: usize) -> std::io::Result<()> {
    if depth > 8 {
        return Ok(());
    }
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)?.flatten() {
        let name = e.file_name();
        if name == "__pycache__" || name.to_string_lossy() == MADE_MARKER {
            continue;
        }
        let ty = e.file_type()?;
        if ty.is_dir() {
            copy_dir(&e.path(), &dst.join(&name), depth + 1)?;
        } else if ty.is_file() {
            fs::copy(e.path(), dst.join(&name))?;
        }
    }
    Ok(())
}

struct KeepPlugin(Arc<AuthorEnv>);

#[derive(Deserialize)]
struct KeepArgs {
    plugin: String,
    scope: String,
    #[serde(default)]
    new_id: Option<String>,
}

impl Tool for KeepPlugin {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "keep_plugin".into(),
            description: "Keep one of this chat's plugins for later chats: move it into the workspace (.byob/plugins) or everywhere. The user is asked first. Only when the user wants to keep it.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "plugin": {"type": "string", "description": "Plugin id, e.g. scratch"},
                    "scope": {"type": "string", "enum": ["workspace", "global"], "description": "workspace: chats in this workspace; global: every chat"},
                    "new_id": {"type": "string", "description": "A descriptive id to keep it under (recommended for the scratch plugin), e.g. weather"}
                },
                "required": ["plugin", "scope"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Kept the plugin {}", str_arg(args, "plugin"))
    }

    fn pending_title(&self, args: &Value) -> String {
        format!("Keep the plugin {} ({})", str_arg(args, "plugin"), str_arg(args, "scope"))
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let run = || -> Result<ToolOutput, String> {
            let a: KeepArgs = parse_args(args)?;
            let scope = match a.scope.trim() {
                "workspace" => PluginScope::Workspace,
                "global" | "everywhere" => PluginScope::Global,
                other => return Err(format!("scope {other:?}: use \"workspace\" or \"global\"")),
            };
            let before = self.0.ask(ctx, PluginOp::Status)?;
            let info = find(&before, a.plugin.trim())?.clone();
            writable(&info)?;
            if info.scope == scope {
                return Ok(ToolOutput::ok(format!("{} is already kept {}.", info.id, scope.label())));
            }
            let id = a.new_id.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&info.id).to_owned();
            let root = match scope {
                PluginScope::Workspace => ctx.workspace.join(".byob").join("plugins"),
                _ => before.global_dir.clone().ok_or("there's no global plugins folder in this run")?,
            };
            if root.join(&id).exists() {
                return Err(format!("{} already has a plugin {id}; pass another new_id", scope.label()));
            }
            let src = PathBuf::from(&info.dir);
            let files = file_list(&src);
            let mut text = format!("Tools: {}\nFiles:\n", if info.tools.is_empty() { "none".to_owned() } else { info.tools.join(", ") });
            for f in files.iter().take(MAX_LISTED) {
                text.push_str(&format!("  {f}\n"));
            }
            if files.len() > MAX_LISTED {
                text.push_str(&format!("  … and {} more\n", files.len() - MAX_LISTED));
            }
            let (title, warning) = match scope {
                PluginScope::Global => {
                    (format!("Keep the plugin {} for every chat as {id}", info.id), Some("Every chat will be able to use its tools.".to_owned()))
                }
                _ => (format!("Keep the plugin {} in this workspace as {id}", info.id), None),
            };
            confirm(ctx, title, text.trim_end().to_owned(), warning, scope == PluginScope::Global)?;
            keep_files(&src, &root, &id, scope == PluginScope::Global, info.scope == PluginScope::Chat)?;
            let op = if scope == PluginScope::Workspace { PluginOp::Made { id: id.clone() } } else { PluginOp::Reload };
            let after = self.0.ask(ctx, op)?;
            let dir = root.join(&id);
            let tools = tools_of(&after, &id, &dir);
            let card = PluginCard {
                plugin: id.clone(),
                scope,
                action: PluginAction::Kept,
                tools: tools.clone(),
                added: Vec::new(),
                removed: Vec::new(),
                test: None,
                problems: problems(&after, &id, &dir),
                missing_settings: missing_settings(&after, &id),
            };
            self.0.card(ctx, "keep_plugin", card);
            Ok(ToolOutput::ok(format!(
                "Kept {} as `{id}` {} ({}). Its tools: {}.",
                info.id,
                scope.label(),
                dir.display(),
                if tools.is_empty() { "none".to_owned() } else { tools.join(", ") }
            )))
        };
        run().unwrap_or_else(ToolOutput::err)
    }
}

struct RemovePlugin(Arc<AuthorEnv>);

impl Tool for RemovePlugin {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "remove_plugin".into(),
            description: "Delete a plugin (the copy this chat uses). This chat's own plugins go at once; others ask the user.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"plugin": {"type": "string", "description": "Plugin id"}},
                "required": ["plugin"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Removed the plugin {}", str_arg(args, "plugin"))
    }

    fn pending_title(&self, args: &Value) -> String {
        format!("Remove the plugin {}", str_arg(args, "plugin"))
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let run = || -> Result<ToolOutput, String> {
            let before = self.0.ask(ctx, PluginOp::Status)?;
            let info = locate(&before, str_arg(args, "plugin").trim())?;
            writable(&info)?;
            if info.scope != PluginScope::Chat {
                confirm(ctx, format!("Delete the plugin {} ({})", info.id, where_(&info)), format!("Deletes {}", info.dir), None, false)?;
            }
            fs::remove_dir_all(&info.dir).map_err(|e| format!("couldn't delete {}: {e}", info.dir))?;
            let op = if info.scope == PluginScope::Workspace { PluginOp::Forget { id: info.id.clone() } } else { PluginOp::Reload };
            let after = self.0.ask(ctx, op)?;
            let card = PluginCard {
                plugin: info.id.clone(),
                scope: info.scope,
                action: PluginAction::Removed,
                tools: Vec::new(),
                added: Vec::new(),
                removed: info.tools.clone(),
                test: None,
                problems: Vec::new(),
                missing_settings: Vec::new(),
            };
            self.0.card(ctx, "remove_plugin", card);
            let left = after.find(&info.id).map(|p| format!(" The copy in {} is used now.", p.scope.label())).unwrap_or_default();
            Ok(ToolOutput::ok(format!("Removed the plugin {} ({}).{left}", info.id, info.scope.label())))
        };
        run().unwrap_or_else(ToolOutput::err)
    }
}

// ---- save_skill -------------------------------------------------------------------------

struct SaveSkill(Arc<AuthorEnv>);

fn skill_scope(args: &Value) -> &str {
    match str_arg(args, "scope") {
        "chat" => "chat",
        "workspace" => "workspace",
        _ => "global",
    }
}

impl Tool for SaveSkill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "save_skill".into(),
            description: "Save how to do a task as a skill (steps, plus the code that worked as script files) for later: in this chat, this workspace, or every chat (default). Only when the user asks.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Short name, lowercase words with dashes, e.g. weekly-report"},
                    "description": {"type": "string", "description": "One sentence: when to use this skill"},
                    "instructions": {"type": "string", "description": "Clear step-by-step instructions in Markdown. Say which script to run and what to pass it."},
                    "files": {"type": "object", "description": "Scripts and data to keep with the skill: file name → contents. Use code that already worked in this chat.", "additionalProperties": {"type": "string"}},
                    "packages": {"type": "array", "items": {"type": "string"}, "description": "PyPI packages the scripts need"},
                    "network": {"type": "boolean", "description": "true only if the scripts need the internet"},
                    "replace": {"type": "boolean", "description": "true to update an existing skill with this name"},
                    "scope": {"type": "string", "enum": ["chat", "workspace", "global"], "description": "Where to keep it (default global: every chat)"}
                },
                "required": ["name", "description", "instructions"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Saved the “{}” skill", byob_plugins::skills::skill_name(str_arg(args, "name")))
    }

    /// Saving for later chats changes later behaviour: it asks (by the approvals); a skill
    /// for this chat only doesn't.
    fn access(&self, args: &Value) -> Access {
        if skill_scope(args) == "chat" { Access::Read } else { Access::Write }
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let run = || -> Result<ToolOutput, String> {
            let s: NewSkill = parse_args(args)?;
            let root = match skill_scope(args) {
                "chat" => {
                    let reply = self.0.ask(ctx, PluginOp::Status)?;
                    let dir = match reply.find(SCRATCH).filter(|p| p.scope == PluginScope::Chat) {
                        Some(p) => PathBuf::from(&p.dir),
                        None => {
                            let dir = reply.scratch_dir.join(SCRATCH);
                            if !dir.join(byob_plugins::MANIFEST).exists() {
                                write_manifest(&dir, &byob_plugins::authoring::new_manifest(SCRATCH, SCRATCH, "Made in a chat"))?;
                            }
                            dir
                        }
                    };
                    dir.join("skills")
                }
                "workspace" => ctx.workspace.join(".byob").join("skills"),
                _ => self.0.global_skills.clone().ok_or("there's no global skills folder in this run; use scope workspace")?,
            };
            let name = save_skill(&root, &s)?;
            self.0.ask(ctx, PluginOp::Reload)?;
            Ok(ToolOutput::ok(format!("Saved. The skill {name} is in your skills list from the next message (use_skill \"{name}\").")))
        };
        run().unwrap_or_else(ToolOutput::err)
    }
}
