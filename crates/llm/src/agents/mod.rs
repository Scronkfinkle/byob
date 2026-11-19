//! Agent CLIs as inference endpoints: `cli://claude` runs Claude Code, `cli://codex` runs
//! Codex, each signed in with the user's own subscription, so no API key is needed.
//!
//! byob stays the agent. The CLI is only the brain: it gets the conversation as text and
//! answers with text, with its own tools turned off wherever the CLI allows it. byob's own
//! tools are offered through prompt-based tool calling ([`crate::toolprompt`]), so approvals,
//! diffs, undo and byob's file rules apply the same whatever answers.
//!
//! Every agent is one [`Provider`] in its own file; [`provider`] finds one by id. Adding
//! another agent is a new file plus a line in [`PROVIDERS`].

mod claude;
mod codex;
pub mod proc;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::{ChatMessage, ChatOutcome, LlmError, StopWhen, StreamEvent, ToolSpec, Usage, toolprompt};

/// Longest a single agent turn may run.
const RUN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// How long a detection result is reused before the CLI is asked again.
const DETECT_TTL: Duration = Duration::from_secs(30);
/// The context window budgeted for every agent CLI.
pub const CONTEXT_TOKENS: u64 = 200_000;

/// One piece of an agent's reply, the same shape for every CLI.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    Text(String),
    Reasoning(String),
    /// A tool the CLI ran itself (only CLIs that can't turn their tools off do).
    Tool {
        name: String,
        summary: String,
    },
    Done {
        usage: Option<Usage>,
    },
    Error {
        kind: ErrorKind,
        message: String,
    },
}

/// Why an agent run failed, in terms the UI can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    NotInstalled,
    NotLoggedIn,
    RateLimited,
    Quota,
    Timeout,
    Other,
}

/// What [`Provider::detect`] found on this computer.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Detection {
    pub found: bool,
    pub binary: Option<PathBuf>,
    pub version: Option<String>,
    pub logged_in: bool,
    /// The plan, when the CLI says (`max`, `pro`, …).
    pub plan: Option<String>,
    pub auth_method: Option<String>,
    /// The Terminal command that signs in.
    pub login_command: &'static str,
    /// What this agent can't be stopped from doing, in one line (empty when nothing).
    pub limits: &'static str,
}

/// One agent turn.
pub struct RunRequest<'a> {
    pub binary: &'a Path,
    /// byob's instructions, tool prompt included.
    pub system: &'a str,
    /// The conversation so far, as text ([`transcript`]).
    pub prompt: &'a str,
    /// The same with its images, for a CLI that takes them ([`Provider::takes_images`]);
    /// empty when there are none, and then `prompt` is the whole conversation.
    pub parts: &'a [Part],
    pub model: &'a str,
    pub effort: Option<&'a str>,
    /// An empty folder the CLI runs in, so it auto-reads no guide files.
    pub cwd: &'a Path,
}

/// Plan limits, when the CLI reports them (Claude: the 5-hour and weekly windows).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PlanUsage {
    /// `(window, share used 0..=1, resets at, unix seconds)`.
    pub windows: Vec<(String, f64, Option<u64>)>,
}

/// An agent CLI.
pub trait Provider: Sync {
    /// The id in `cli://<id>`.
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    /// The binary's name, and the environment variable that can point at it.
    fn binary_name(&self) -> &'static str;
    fn binary_env(&self) -> &'static str;
    /// Is it installed and signed in? Spawns the CLI: use [`detect`], which caches.
    fn detect_uncached(&self, binary: Option<PathBuf>) -> Detection;
    /// Models to offer.
    fn models(&self, binary: &Path) -> Vec<String>;
    /// Can it be sent images (in [`RunRequest::parts`])? Otherwise the prompt says they
    /// were left out.
    fn takes_images(&self) -> bool {
        false
    }
    /// Run one turn, handing every event to `on_event`.
    fn run(&self, req: &RunRequest<'_>, stop: &dyn Fn() -> bool, on_event: &mut dyn FnMut(AgentEvent));
    /// Plan usage from the latest run, if the CLI reports it.
    fn usage(&self) -> Option<PlanUsage> {
        None
    }
}

/// Every agent byob knows, in the order they're offered.
static PROVIDERS: &[&dyn Provider] = &[&claude::Claude, &codex::Codex];

/// The agent for `cli://<id>`.
pub fn provider(id: &str) -> Option<&'static dyn Provider> {
    PROVIDERS.iter().copied().find(|p| p.id() == id)
}

/// Every agent, for settings screens.
pub fn providers() -> impl Iterator<Item = &'static dyn Provider> {
    PROVIDERS.iter().copied()
}

/// Folders agent CLIs are usually installed in, tried after `$<NAME>_BIN` and before PATH.
pub fn install_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs = Vec::new();
    if let Some(h) = &home {
        for d in [".local/bin", ".claude/local", ".npm-global/bin", ".bun/bin", ".cargo/bin", ".volta/bin"] {
            dirs.push(h.join(d));
        }
    }
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    dirs
}

/// Where `p`'s binary is: `$<ENV>` if it names a file, else the first install folder or PATH
/// entry that has it.
pub fn find_binary(p: &dyn Provider) -> Option<PathBuf> {
    if let Some(v) = std::env::var_os(p.binary_env()) {
        let path = PathBuf::from(v);
        return path.is_file().then_some(path);
    }
    let path_dirs = std::env::var_os("PATH").map(|v| std::env::split_paths(&v).collect::<Vec<_>>()).unwrap_or_default();
    install_dirs().into_iter().chain(path_dirs).map(|d| d.join(p.binary_name())).find(|f| f.is_file())
}

/// [`Provider::detect_uncached`], reused for [`DETECT_TTL`].
pub fn detect(p: &dyn Provider) -> Detection {
    static CACHE: OnceLock<Mutex<Vec<(&'static str, Instant, Detection)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(Vec::new()));
    if let Some((_, _, d)) = cache.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(id, at, _)| *id == p.id() && at.elapsed() < DETECT_TTL) {
        return d.clone();
    }
    let d = p.detect_uncached(find_binary(p));
    let mut c = cache.lock().unwrap_or_else(PoisonError::into_inner);
    c.retain(|(id, _, _)| *id != p.id());
    c.push((p.id(), Instant::now(), d.clone()));
    d
}

/// An empty folder agents run in (so no `CLAUDE.md`/`AGENTS.md` gets read in).
fn sandbox_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("byob-agent-cwd");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// The error a failed agent run means for byob, worded so the engine's connection advice
/// fits (401 → sign in, 429 → limits) and the message says what to run.
fn llm_error(name: &str, login: &str, kind: ErrorKind, message: &str) -> LlmError {
    let message = message.trim();
    match kind {
        ErrorKind::NotInstalled => LlmError::Stream(format!("{name} isn't installed on this computer (or byob can't find it): {message}")),
        ErrorKind::NotLoggedIn => LlmError::Http { status: 401, message: format!("{name} isn't signed in. Run `{login}` in Terminal. {message}") },
        ErrorKind::RateLimited | ErrorKind::Quota => LlmError::Http { status: 429, message: format!("{name}: {message}") },
        ErrorKind::Timeout => LlmError::Stream(format!("{name} took too long and was stopped")),
        ErrorKind::Other => LlmError::Stream(format!("{name}: {message}")),
    }
}

/// The models of a signed-in agent, or why there are none.
pub fn list_models(id: &str) -> Result<Vec<String>, LlmError> {
    let p = provider(id).ok_or_else(|| LlmError::Stream(format!("there's no agent called {id:?}")))?;
    let d = detect(p);
    let binary = match (&d.binary, d.found) {
        (Some(b), true) => b.clone(),
        _ => {
            return Err(llm_error(
                p.name(),
                d.login_command,
                ErrorKind::NotInstalled,
                &format!("looked for `{}` in the usual folders and PATH", p.binary_name()),
            ));
        }
    };
    if !d.logged_in {
        return Err(llm_error(p.name(), d.login_command, ErrorKind::NotLoggedIn, ""));
    }
    Ok(p.models(&binary))
}

/// The conversation as one text prompt, for CLIs that take a single message. byob's
/// system messages are left out (they go to [`system_text`]); tool calls are written in the
/// `<function_call>` form the tool prompt teaches, so the model sees its own earlier calls.
pub fn transcript(messages: &[ChatMessage]) -> String {
    transcript_parts(messages, false)
        .into_iter()
        .filter_map(|p| match p {
            Part::Text(t) => Some(t),
            Part::Image(_) => None,
        })
        .collect()
}

/// Most images sent with one request (the latest; earlier ones are mentioned instead).
pub const MAX_IMAGES: usize = 20;

/// A piece of a prompt: text, or an image (a `data:` or http(s) URL) where it was attached.
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    Image(String),
}

/// [`transcript`] with each user message's images after its text when `images` is set (the
/// latest [`MAX_IMAGES`]), or a note that they were left out.
pub fn transcript_parts(messages: &[ChatMessage], images: bool) -> Vec<Part> {
    let total: usize = messages.iter().filter(|m| m.role == "user").map(|m| m.images.len()).sum();
    let mut skip = if images { total.saturating_sub(MAX_IMAGES) } else { total };
    let mut parts = Vec::new();
    let mut out = String::new();
    for m in messages {
        let text = m.content.as_deref().unwrap_or_default().trim();
        match m.role.as_str() {
            "system" => continue,
            "user" => {
                out.push_str("<user>\n");
                out.push_str(text);
                let left_out = m.images.len().min(skip);
                skip -= left_out;
                if left_out > 0 {
                    let why = if images { "earlier image(s) left out" } else { "image(s) attached; this model can't see them here" };
                    out.push_str(&format!("\n[{left_out} {why}]"));
                }
                for url in m.images.iter().skip(left_out) {
                    out.push('\n');
                    parts.push(Part::Text(std::mem::take(&mut out)));
                    parts.push(Part::Image(url.clone()));
                }
                out.push_str("\n</user>\n\n");
            }
            "assistant" => {
                out.push_str("<assistant>\n");
                out.push_str(text);
                for c in &m.tool_calls {
                    let args: serde_json::Value =
                        serde_json::from_str(&c.function.arguments).unwrap_or(serde_json::Value::String(c.function.arguments.clone()));
                    let call = serde_json::json!({"name": c.function.name, "arguments": args});
                    out.push_str(&format!("\n<function_call>{call}</function_call>"));
                }
                out.push_str("\n</assistant>\n\n");
            }
            "tool" => {
                out.push_str("<tool_result>\n");
                out.push_str(text);
                out.push_str("\n</tool_result>\n\n");
            }
            _ => {}
        }
    }
    out.push_str("Write the next <assistant> reply. Write only the reply itself, without the <assistant> tags.");
    parts.push(Part::Text(out));
    parts
}

/// byob's system messages, joined.
pub fn system_text(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .filter(|m| m.role == "system")
        .filter_map(|m| m.content.as_deref())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Run one chat turn on agent `id`: what [`crate::Client::stream_chat`] does for `cli://`
/// URLs. When byob asks for native tools the tool prompt is added here and calls are parsed
/// back out of the reply, since no CLI hands its tool calls to byob.
pub(crate) fn stream(
    id: &str,
    config: &byob_types::InferenceConfig,
    messages: &[ChatMessage],
    tools: &[ToolSpec],
    stop_when: &StopWhen<'_>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> Result<ChatOutcome, LlmError> {
    let p = provider(id).ok_or_else(|| LlmError::Stream(format!("there's no agent called {id:?}")))?;
    let d = detect(p);
    let Some(binary) = d.binary.clone().filter(|_| d.found) else {
        return Err(llm_error(p.name(), d.login_command, ErrorKind::NotInstalled, &format!("looked for `{}` in the usual folders and PATH", p.binary_name())));
    };
    let mut system = system_text(messages);
    if !tools.is_empty() {
        system.push_str("\n\n");
        system.push_str(&toolprompt::tool_prompt(tools));
    }
    let prompt = transcript(messages);
    let has_images = messages.iter().any(|m| m.role == "user" && !m.images.is_empty());
    let parts = if has_images && p.takes_images() { transcript_parts(messages, true) } else { Vec::new() };
    let effort = config.params.effort.map(|e| e.as_str());
    let cwd = sandbox_dir();
    let req = RunRequest { binary: &binary, system: &system, prompt: &prompt, parts: &parts, model: config.model.trim(), effort, cwd: &cwd };
    log::info!("agent {id}: model {:?}, system {} chars, prompt {} chars", req.model, system.len(), prompt.len());
    let mut out = ChatOutcome::default();
    let mut failure: Option<(ErrorKind, String)> = None;
    let started = Instant::now();
    p.run(&req, &|| stop_when.now(), &mut |ev| match ev {
        AgentEvent::Text(t) => {
            out.content.push_str(&t);
            on_event(StreamEvent::Content(t));
        }
        AgentEvent::Reasoning(t) => {
            out.reasoning.push_str(&t);
            on_event(StreamEvent::Reasoning(t));
        }
        AgentEvent::Tool { name, summary } => log::warn!("agent {id} ran its own tool {name}: {summary}"),
        AgentEvent::Done { usage } => out.usage = usage,
        AgentEvent::Error { kind, message } => failure = Some((kind, message)),
    });
    if stop_when.now() {
        out.cancelled = true;
        return Ok(out);
    }
    if let Some((kind, message)) = failure {
        log::warn!("agent {id}: {kind:?}: {message}");
        return Err(llm_error(p.name(), d.login_command, kind, &message));
    }
    if let Some(u) = &out.usage {
        log::info!("agent {id}: {} prompt + {} reply tokens in {:.1}s", u.prompt_tokens, u.completion_tokens, started.elapsed().as_secs_f32());
    }
    if !tools.is_empty() {
        let parsed = toolprompt::parse_tool_calls(&out.content, tools);
        if !parsed.calls.is_empty() {
            out.content = parsed.text;
            out.tool_calls = parsed.calls;
        }
    }
    out.finish_reason = Some(if out.tool_calls.is_empty() { "stop" } else { "tool_calls" }.into());
    Ok(out)
}

/// The run timeout every provider uses.
pub(crate) fn run_timeout() -> Duration {
    RUN_TIMEOUT
}

/// Sort an error message from a CLI into an [`ErrorKind`].
pub(crate) fn classify(message: &str) -> ErrorKind {
    let m = message.to_ascii_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| m.contains(w));
    if any(&["not logged in", "log in", "login", "/login", "not signed in", "sign in", "unauthorized", "invalid api key", "401", "authentication"]) {
        ErrorKind::NotLoggedIn
    } else if any(&["usage limit", "quota", "out of credits", "insufficient", "billing"]) {
        ErrorKind::Quota
    } else if any(&["rate limit", "rate_limit", "too many requests", "429", "overloaded"]) {
        ErrorKind::RateLimited
    } else {
        ErrorKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_keeps_turns_tool_calls_and_results_but_not_system() {
        let call = ChatMessage::assistant_tool_calls(
            Some("Looking.".into()),
            &[crate::ToolCall { id: "c1".into(), name: "read_file".into(), arguments: r#"{"path":"a.txt"}"#.into() }],
        );
        let messages = vec![ChatMessage::system("be nice"), ChatMessage::user("read a.txt"), call, ChatMessage::tool_result("c1", "hello")];
        let t = transcript(&messages);
        assert!(!t.contains("be nice"));
        assert!(t.contains("<user>\nread a.txt\n</user>"));
        assert!(t.contains(r#"<function_call>{"arguments":{"path":"a.txt"},"name":"read_file"}</function_call>"#), "{t}");
        assert!(t.contains("<tool_result>\nhello\n</tool_result>"));
        assert_eq!(system_text(&messages), "be nice");
    }

    /// Regression: images attached in a chat on Claude Code were dropped ("this model can't
    /// see them here"). For a CLI that takes them they come after their message's text, the
    /// latest MAX_IMAGES of them.
    #[test]
    fn images_go_where_they_were_attached() {
        let messages = vec![
            ChatMessage::user_with_images("what's this?", vec!["data:image/png;base64,AAAA".into()]),
            ChatMessage::assistant("A dot."),
            ChatMessage::user("thanks"),
        ];
        let parts = transcript_parts(&messages, true);
        assert!(
            matches!(&parts[..], [Part::Text(a), Part::Image(u), Part::Text(b)]
            if a.ends_with("<user>\nwhat's this?\n") && u == "data:image/png;base64,AAAA" && b.starts_with("\n</user>") && b.contains("thanks")),
            "{parts:?}"
        );
        // Without images, the note as before.
        assert!(transcript(&messages).contains("[1 image(s) attached; this model can't see them here]"));
        // Only the latest are sent.
        let many: Vec<ChatMessage> = (0..MAX_IMAGES + 3).map(|i| ChatMessage::user_with_images(format!("{i}"), vec![format!("https://x/{i}.png")])).collect();
        let parts = transcript_parts(&many, true);
        let sent: Vec<&String> = parts.iter().filter_map(|p| if let Part::Image(u) = p { Some(u) } else { None }).collect();
        assert_eq!((sent.len(), sent[0].as_str()), (MAX_IMAGES, "https://x/3.png"));
        let text: String = parts.iter().filter_map(|p| if let Part::Text(t) = p { Some(t.as_str()) } else { None }).collect();
        assert_eq!(text.matches("[1 earlier image(s) left out]").count(), 3);
    }

    /// The probe reports vision from this, and the engine strips images from requests to a
    /// model without it: Claude Code takes them, Codex doesn't (yet).
    #[test]
    fn claude_code_takes_images() {
        assert!(provider("claude").is_some_and(|p| p.takes_images()));
        assert!(provider("codex").is_some_and(|p| !p.takes_images()));
    }

    #[test]
    fn errors_are_sorted_by_what_the_user_can_do() {
        assert_eq!(classify("Invalid API key · Please run /login"), ErrorKind::NotLoggedIn);
        assert_eq!(classify("You've hit your usage limit"), ErrorKind::Quota);
        assert_eq!(classify("429 Too Many Requests"), ErrorKind::RateLimited);
        assert_eq!(classify("model_not_found"), ErrorKind::Other);
        assert!(matches!(llm_error("Claude Code", "claude auth login", ErrorKind::NotLoggedIn, ""), LlmError::Http { status: 401, .. }));
        assert!(matches!(llm_error("Codex", "codex login", ErrorKind::Quota, "x"), LlmError::Http { status: 429, .. }));
    }

    /// Live: needs the CLI installed and signed in. `cargo test -p byob-llm live_ -- --ignored --nocapture`
    fn live_chat(url: &str, model: &str) {
        use std::sync::atomic::AtomicBool;
        let client = crate::Client::new(byob_types::InferenceConfig { base_url: url.into(), model: model.into(), ..Default::default() }).unwrap();
        println!("models: {:?}", client.list_models().unwrap());
        let tools = vec![crate::ToolSpec {
            name: "read_file".into(),
            description: "Read a text file in the workspace.".into(),
            parameters: serde_json::json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        }];
        let cancel = AtomicBool::new(false);
        let mut streamed = String::new();
        // 1. Plain answer, streamed.
        let out = client.stream_chat(&[ChatMessage::system("You are terse."), ChatMessage::user("Reply with exactly: ok")], &[], &cancel, |e| {
            if let StreamEvent::Content(t) = e {
                streamed.push_str(&t);
            }
        });
        let out = out.unwrap();
        println!("plain: {:?} usage {:?}", out.content, out.usage);
        assert!(out.content.to_lowercase().contains("ok") && streamed == out.content && out.usage.is_some());
        // 2. Needs a file: byob's tool must come back as a call, not be run by the CLI.
        let out = client
            .stream_chat(
                &[ChatMessage::system("You work in the user's folder."), ChatMessage::user("What does notes.txt say? Read it.")],
                &tools,
                &cancel,
                |_| {},
            )
            .unwrap();
        println!("tool: calls {:?} text {:?}", out.tool_calls, out.content);
        assert_eq!(out.tool_calls.first().map(|c| c.name.as_str()), Some("read_file"));
        // 3. Cancel mid-answer returns at once, marked cancelled.
        let cancel = AtomicBool::new(false);
        let start = Instant::now();
        let out = client
            .stream_chat(&[ChatMessage::user("Count from 1 to 300, one number per line.")], &[], &cancel, |_| {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed)
            })
            .unwrap();
        println!("cancel: cancelled={} after {:?}, {} chars", out.cancelled, start.elapsed(), out.content.len());
        assert!(out.cancelled);
    }

    #[test]
    #[ignore = "live: runs the signed-in Claude Code CLI"]
    fn live_claude() {
        live_chat("cli://claude", "haiku");
    }

    /// Live: Claude Code sees an attached image (a red square).
    #[test]
    #[ignore = "live: runs the signed-in Claude Code CLI"]
    fn live_claude_sees_images() {
        use std::sync::atomic::AtomicBool;
        const RED: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAABAAAAAQCAIAAACQkWg2AAAAFklEQVR42mP4z8BAEmIY1TCqYfhqAACQ+f8B8u7oVwAAAABJRU5ErkJggg==";
        let client = crate::Client::new(byob_types::InferenceConfig { base_url: "cli://claude".into(), model: "haiku".into(), ..Default::default() }).unwrap();
        let messages =
            [ChatMessage::system("You are terse."), ChatMessage::user_with_images("What single color fills this image? One word.", vec![RED.into()])];
        let out = client.stream_chat(&messages, &[], &AtomicBool::new(false), |_| {}).unwrap();
        println!("{:?}", out.content);
        assert!(out.content.to_lowercase().contains("red"), "{:?}", out.content);
    }

    #[test]
    #[ignore = "live: runs the signed-in Codex CLI"]
    fn live_codex() {
        live_chat("cli://codex", "gpt-5.6-luna");
    }

    #[test]
    fn a_missing_binary_is_not_installed() {
        // SAFETY of the env var: only this test reads BYOB_TEST_NOPE_BIN.
        struct Nope;
        impl Provider for Nope {
            fn id(&self) -> &'static str {
                "nope"
            }
            fn name(&self) -> &'static str {
                "Nope"
            }
            fn binary_name(&self) -> &'static str {
                "byob-no-such-agent-binary"
            }
            fn binary_env(&self) -> &'static str {
                "BYOB_TEST_NOPE_BIN"
            }
            fn detect_uncached(&self, binary: Option<PathBuf>) -> Detection {
                Detection { found: binary.is_some(), binary, ..Default::default() }
            }
            fn models(&self, _: &Path) -> Vec<String> {
                Vec::new()
            }
            fn run(&self, _: &RunRequest<'_>, _: &dyn Fn() -> bool, _: &mut dyn FnMut(AgentEvent)) {}
        }
        assert_eq!(find_binary(&Nope), None);
        assert!(!detect(&Nope).found);
    }

    #[test]
    fn every_provider_has_a_unique_url_safe_id() {
        let ids: Vec<_> = providers().map(|p| p.id()).collect();
        for id in &ids {
            assert_eq!(byob_types::cli_agent(&format!("cli://{id}")), Some(*id));
        }
        let mut sorted = ids.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len());
    }
}
