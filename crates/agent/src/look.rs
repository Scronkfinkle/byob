//! Windows' looks (`byob_protocol::look`): each client reports its own; the `ui_look` tool
//! (named by the bundled `appearance` plugin) shows the model the look of the window showing
//! its chat and changes it there, then reports what the window actually changed.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use byob_protocol::ServerMsg;
use byob_protocol::look::{CAP_LOOK, LookChanges, LookReport};
use byob_tools::{Access, Tool, ToolContext, ToolOutput, ToolSpec};
use serde_json::{Value, json};

use crate::{ClientId, Engine, Input};

/// The tool's name.
pub const TOOL: &str = "ui_look";

/// How long the tool waits for a window to answer.
const TIMEOUT: Duration = Duration::from_secs(10);

/// What the engine answers the tool: the look before, and after the change (if one was asked).
pub(crate) type Answer = Result<(LookReport, Option<LookReport>), String>;

/// A change sent to windows, waiting for the first to answer.
pub(crate) struct Waiting {
    before: LookReport,
    reply: Sender<Answer>,
}

const NO_WINDOW: &str = "no byob window is showing this chat right now (or it's too old to report its look), so there's no look to read or change";

impl Engine {
    fn look_clients(&self, chat: &str) -> Vec<ClientId> {
        self.clients.iter().filter(|(_, c)| c.attached && c.chat.as_deref() == Some(chat) && c.caps.iter().any(|c| c == CAP_LOOK)).map(|(id, _)| *id).collect()
    }

    /// `ClientMsg::ReportLook`: keep it; answer the tool that asked for the change.
    pub(crate) fn report_look(&mut self, client: ClientId, report: LookReport) {
        let report = report.sanitized();
        self.look_seq += 1;
        let seq = self.look_seq;
        if let Some(c) = self.clients.get_mut(&client) {
            c.look = Some((seq, report.clone()));
        }
        if let Some(w) = report.answered.and_then(|n| self.look_waits.remove(&n)) {
            let _ = w.reply.send(Ok((w.before, Some(report))));
        }
    }

    /// `Input::Look`: the look of the window showing `chat` that reported last, and with
    /// `changes`, send them to every window showing it.
    pub(crate) fn tool_look(&mut self, chat: &str, changes: Option<LookChanges>, reply: Sender<Answer>) {
        let ids = self.look_clients(chat);
        let latest = ids.iter().filter_map(|id| self.clients.get(id)?.look.clone()).max_by_key(|(seq, _)| *seq).map(|(_, r)| r);
        let Some(before) = latest else {
            let _ = reply.send(Err(NO_WINDOW.into()));
            return;
        };
        let Some(changes) = changes else {
            let _ = reply.send(Ok((before, None)));
            return;
        };
        self.look_seq += 1;
        let request = self.look_seq;
        // Forget requests nobody answered (their tool gave up long ago).
        while self.look_waits.len() > 16 {
            self.look_waits.pop_first();
        }
        self.look_waits.insert(request, Waiting { before, reply });
        for id in ids {
            self.send_to(id, ServerMsg::SetLook { request, changes: changes.clone() });
        }
    }
}

fn request(inbox: &Sender<Input>, chat: &str, changes: Option<LookChanges>, cancel: &AtomicBool) -> Answer {
    let (reply, answer) = channel();
    inbox.send(Input::Look { chat: chat.to_owned(), changes, reply }).map_err(|_| "byob is shutting down".to_owned())?;
    let started = Instant::now();
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("stopped".into());
        }
        match answer.recv_timeout(Duration::from_millis(50)) {
            Ok(r) => return r,
            Err(RecvTimeoutError::Timeout) if started.elapsed() < TIMEOUT => {}
            Err(_) => return Err("the window didn't answer".into()),
        }
    }
}

fn shown(v: &Value) -> String {
    match v {
        Value::String(s) => format!("{s:?}"),
        Value::Null => "(unset)".into(),
        v => v.to_string(),
    }
}

/// The look for the model.
fn describe(r: &LookReport) -> String {
    let mut out = format!(
        "The look of the byob window showing this chat (each device keeps its own).\nSettings: {}\nColour roles (as colour.<role>): {}\n\n",
        names(r),
        roles(r).join(", ")
    );
    for f in &r.fields {
        let _ = writeln!(out, "- {}: {} — {}", f.key, shown(&f.value), f.describe);
    }
    if !r.themes.is_empty() {
        out.push_str("\nThemes (for dark_theme / light_theme):\n");
        for t in &r.themes {
            let _ = writeln!(out, "- {}: {} ({})", t.id, t.name, if t.dark { "dark" } else { "light" });
        }
    }
    out
}

/// The window's setting names (the colours as one `colour.<role>`), for a model that guessed.
fn names(have: &LookReport) -> String {
    let mut names: Vec<&str> = have.fields.iter().map(|f| f.key.as_str()).filter(|k| !k.starts_with("colour.")).collect();
    if have.fields.iter().any(|f| f.key.starts_with("colour.")) {
        names.push("colour.<role>");
    }
    names.join(", ")
}

/// The colour roles the window has (`app_bg`, `text`, …).
fn roles(have: &LookReport) -> Vec<&str> {
    have.fields.iter().filter_map(|f| f.key.strip_prefix("colour.")).collect()
}

/// Names models reach for → the setting they mean (checked against the window's settings
/// before use). Colours by role, then other settings.
const COLOUR_ALIASES: &[(&[&str], &str)] = &[
    (&["bg", "background", "window", "window_bg", "app_background", "canvas", "base"], "app_bg"),
    (&["fg", "foreground", "body", "body_text", "text_colour", "main_text"], "text"),
    (&["muted", "dim", "secondary", "subtle", "text_muted", "subtext", "muted_text"], "text_dim"),
    (&["faint", "text_faint_colour"], "text_faint"),
    (&["surface", "panel", "panels", "header", "header_bg", "title_bar", "titlebar", "toolbar", "composer", "chrome"], "panel_bg"),
    (&["sidebar", "chat_list", "nav"], "sidebar_bg"),
    (&["input", "field", "input_bg", "textbox"], "field_bg"),
    (&["reply", "replies", "reply_bg", "assistant", "assistant_bg", "bot", "bot_bubble"], "assistant_bubble"),
    (&["user", "user_bg", "your_messages", "you", "me"], "user_bubble"),
    (&["code_background", "code_block", "pre"], "code_bg"),
    (&["code_fg", "code_foreground", "code"], "code_text"),
    (&["primary", "highlight", "brand", "theme_colour"], "accent"),
    (&["error", "errors", "red"], "danger"),
    (&["outline", "borders", "stroke"], "border"),
];
const SETTING_ALIASES: &[(&[&str], &str)] = &[
    (&["font_size", "text_scale", "font_scale", "size"], "text_size"),
    (&["radius", "rounding", "corner_radius", "roundness", "border_radius"], "corners"),
    (&["scale", "ui_scale"], "zoom"),
    (&["spacing"], "density"),
    (&["padding", "text_padding", "inset", "bubble_inset", "message_padding", "inner_padding", "bubble_margin"], "bubble_padding"),
    (&["line_height", "leading", "line_gap"], "line_spacing"),
    (&["width", "max_width", "content_width"], "column_width"),
    (&["layout", "alignment"], "message_layout"),
    (&["contrast"], "high_contrast"),
    (&["animations"], "reduce_motion"),
    (&["user_bubble", "user_messages", "my_messages"], "your_messages"),
    (&["assistant_bubble", "assistant_messages", "reply_style"], "replies"),
    (&["accent_colour", "accent_color", "primary"], "accent"),
    (&["dark_mode", "theme_mode", "appearance", "scheme", "color_scheme", "colour_scheme"], "mode"),
];

/// `Color-Scheme`, `colors.bg` → `colour_scheme`, `colour.bg`.
fn tidy(key: &str) -> String {
    let k = key.trim().to_lowercase().replace(['-', ' '], "_");
    for p in ["colors.", "colours.", "color.", "color_", "colour_"] {
        if let Some(rest) = k.strip_prefix(p) {
            return format!("colour.{rest}");
        }
    }
    k
}

/// The setting `key` means for `value`, if it can be told: exact, tidied, a bare colour
/// role, or a common name for one. A `#rrggbb` value goes to a colour unless the setting
/// takes colours itself (`replies: "#fff"` is the replies' colour, not their style).
/// `None` when it's a guess too far (see [`suggest`]).
fn resolve(key: &str, value: &Value, have: &LookReport) -> Option<String> {
    let has = |k: &str| have.get(k).is_some();
    let takes_colour = |k: &str| have.fields.iter().any(|f| f.key == k && (k.starts_with("colour.") || f.describe.contains('#')));
    let hex = value.as_str().is_some_and(|v| v.trim().starts_with('#'));
    let k = tidy(key);
    let (colour, bare) = match k.strip_prefix("colour.") {
        Some(role) => (true, role.to_owned()),
        None => (false, k.clone()),
    };
    let as_colour = |role: &str| has(&format!("colour.{role}")).then(|| format!("colour.{role}"));
    let colour_for = || as_colour(&bare).or_else(|| COLOUR_ALIASES.iter().find(|(from, _)| from.contains(&bare.as_str())).and_then(|(_, to)| as_colour(to)));
    for exact in [key, k.as_str()] {
        if has(exact) && (!hex || takes_colour(exact)) {
            return Some(exact.to_owned());
        }
    }
    if hex && let Some(c) = colour_for() {
        return Some(c);
    }
    if has(&k) {
        return Some(k);
    }
    // "theme" means the one in use now.
    if k == "theme" {
        return match have.get("mode").and_then(Value::as_str) {
            Some("dark") => Some("dark_theme".into()),
            Some("light") => Some("light_theme".into()),
            _ => None,
        };
    }
    if !colour
        && let Some((_, to)) = SETTING_ALIASES.iter().find(|(from, _)| from.contains(&bare.as_str()))
        && has(to)
    {
        return Some((*to).to_owned());
    }
    colour_for()
}

/// The setting `key` most likely meant, by the words it shares with names and descriptions.
fn suggest(key: &str, have: &LookReport) -> Option<String> {
    let words: Vec<String> = tidy(key).split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2 && *w != "colour").map(str::to_owned).collect();
    let mut best: Option<(usize, &str)> = None;
    for f in &have.fields {
        let text = format!("{} {}", f.key, f.describe).to_lowercase();
        let n = words.iter().filter(|w| text.contains(w.as_str())).count();
        if n > 0 && best.is_none_or(|(b, _)| n > b) {
            best = Some((n, f.key.as_str()));
        }
    }
    best.map(|(_, k)| k.to_owned())
}

/// `set`, with nested objects flattened: `{"colour": {"bg": …}}` → `colour.bg`.
fn flatten(set: &serde_json::Map<String, Value>) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for (k, v) in set {
        match v {
            Value::Object(inner) => out.extend(inner.iter().map(|(ik, iv)| (format!("{k}.{ik}"), iv.clone()))),
            v => out.push((k.clone(), v.clone())),
        }
    }
    out
}

/// What a `set` turned into: the changes to send, how guessed names were read, and what
/// was left out and why.
#[derive(Debug, Default, PartialEq)]
struct Checked {
    set: LookChanges,
    read_as: Vec<String>,
    skipped: Vec<String>,
}

/// The `set` argument, checked against what the window has (`None`: only read it; an
/// error when nothing is left to change).
fn changes(args: &Value, have: &LookReport) -> Result<Option<Checked>, String> {
    let Some(set) = args.get("set") else { return Ok(None) };
    let map = set.as_object().ok_or("set: an object of setting → new value")?;
    if map.is_empty() {
        return Ok(None);
    }
    let mut c = Checked::default();
    let mut unknown = false;
    for (k, v) in flatten(map) {
        if matches!(v, Value::Object(_) | Value::Array(_)) {
            c.skipped.push(format!("{k}: one value (a string, number or true/false)"));
            continue;
        }
        match resolve(&k, &v, have) {
            Some(real) => {
                if real != k {
                    c.read_as.push(format!("{k} as {real}"));
                }
                c.set.insert(real, v);
            }
            None => {
                unknown = true;
                match suggest(&k, have) {
                    Some(s) => c.skipped.push(format!("there's no setting {k:?}; did you mean {s}?")),
                    None => c.skipped.push(format!("there's no setting {k:?}")),
                }
            }
        }
    }
    if unknown {
        c.skipped.push(format!("the settings are: {}; colour roles: {}", names(have), roles(have).join(", ")));
    }
    if c.set.is_empty() {
        return Err(c.skipped.join("; "));
    }
    Ok(Some(c))
}

/// The model's tool: read the window's look, or change it.
pub(crate) struct UiLook {
    pub inbox: Sender<Input>,
}

impl Tool for UiLook {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TOOL.into(),
            description: "Read or change how the byob window showing this chat looks: dark or light, theme, accent colour, corner roundness, text size, zoom, density, message bubbles, fonts, any single colour. Without arguments: every setting with its value and what it may be. Only this device's window(s) change.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "set": {"type": "object", "description": "Setting → new value, e.g. {\"accent\": \"orange\", \"corners\": 12}. Call without arguments first for the settings there are.", "additionalProperties": true}
                }
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        match args.get("set").and_then(Value::as_object).filter(|m| !m.is_empty()) {
            Some(m) => {
                let keys: Vec<&str> = m.keys().map(String::as_str).take(4).collect();
                let more = if m.len() > 4 { ", …" } else { "" };
                format!("Look: changed {}{more}", keys.join(", "))
            }
            None => "Read the look".into(),
        }
    }

    fn pending_title(&self, args: &Value) -> String {
        if args.get("set").and_then(Value::as_object).is_some_and(|m| !m.is_empty()) { "Change the look".into() } else { "Read the look".into() }
    }

    fn preview(&self, _args: &Value, _workspace: &Path) -> Option<byob_tools::Preview> {
        None
    }

    /// Only how this window looks changes, visibly and at once, and Settings → Appearance
    /// changes it back: nothing to approve.
    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let Some(chat) = ctx.chat() else { return ToolOutput::err(NO_WINDOW) };
        let now = match request(&self.inbox, chat, None, ctx.cancel_flag()) {
            Ok((r, _)) => r,
            Err(e) => return ToolOutput::err(e),
        };
        let Checked { set, read_as, skipped } = match changes(args, &now) {
            Ok(Some(c)) => c,
            Ok(None) => return ToolOutput::ok(describe(&now)),
            Err(e) => return ToolOutput::err(e),
        };
        let asked: Vec<String> = set.keys().cloned().collect();
        let (before, after) = match request(&self.inbox, chat, Some(set), ctx.cancel_flag()) {
            Ok((b, Some(a))) => (b, a),
            Ok((_, None)) => return ToolOutput::err("the window didn't answer"),
            Err(e) => return ToolOutput::err(e),
        };
        // What was asked for, setting by setting; colours that follow from it (an accent
        // changes links and selections too) only as a count.
        let changed = after.changes_since(&before);
        let mut out = String::from("In the window now:\n");
        if !read_as.is_empty() {
            let _ = writeln!(out, "(Read {}.)", read_as.join(", "));
        }
        for k in &asked {
            match changed.iter().find(|(c, _, _)| c == k) {
                Some((_, old, new)) => {
                    let _ = writeln!(out, "- {k}: {} → {}", shown(old), shown(new));
                }
                None if after.problems.iter().any(|p| p.starts_with(&format!("{k}:"))) => {}
                None => {
                    let _ = writeln!(out, "- {k}: already {}", after.get(k).map_or_else(|| "(unset)".to_owned(), shown));
                }
            }
        }
        let follow = changed.iter().filter(|(k, _, _)| !asked.contains(k)).count();
        if follow > 0 {
            let _ = writeln!(out, "({follow} colours that follow from these changed with them.)");
        }
        let problems: Vec<&String> = after.problems.iter().chain(&skipped).collect();
        if !problems.is_empty() {
            out.push_str("\nNot applied:\n");
            for p in &problems {
                let _ = writeln!(out, "- {p}");
            }
        }
        if changed.is_empty() && !problems.is_empty() { ToolOutput::err(out) } else { ToolOutput::ok(out) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use byob_protocol::look::LookField;

    fn report() -> LookReport {
        LookReport {
            fields: vec![
                LookField { key: "accent".into(), value: json!("theme"), describe: "theme, blue, orange or #rrggbb".into() },
                LookField { key: "colour.app_bg".into(), value: json!("#101010"), describe: "Background".into() },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn changes_are_checked_against_the_windows_settings() {
        assert_eq!(changes(&json!({}), &report()), Ok(None));
        assert_eq!(changes(&json!({"set": {}}), &report()), Ok(None));
        assert_eq!(changes(&json!({"set": {"accent": "orange"}}), &report()).unwrap().unwrap().set["accent"], "orange");
        let e = changes(&json!({"set": {"sparkle": true}}), &report()).unwrap_err();
        assert!(e.contains("no setting \"sparkle\"") && e.contains("the settings are: accent, colour.<role>"), "{e}");
        assert!(changes(&json!({"set": {"accent": ["a"]}}), &report()).is_err());
        // What it knows goes ahead; what it doesn't is said.
        let c = changes(&json!({"set": {"accent": "teal", "bubbles": "outlined"}}), &report()).unwrap().unwrap();
        assert_eq!((c.set.len(), c.skipped.len()), (1, 2));
        assert!(changes(&json!({"set": {"colour.app_bg": "#202020"}}), &report()).unwrap().is_some());
        let text = describe(&report());
        assert!(text.contains("- accent: \"theme\" — theme, blue") && text.contains("Colour roles (as colour.<role>): app_bg"), "{text}");
    }

    fn field(key: &str, value: Value, describe: &str) -> LookField {
        LookField { key: key.into(), value, describe: describe.into() }
    }

    /// Regression: a Windows 98 and a pink theme took a dozen calls each, the model guessing
    /// `bg`, `colour.muted`, `colour.header`, a nested `colour` object, `theme`, and a colour
    /// for `replies` (a style). Names it plainly meant are read as the real ones, and said.
    #[test]
    fn the_names_models_guess_are_read_as_the_settings_they_mean() {
        let r = LookReport {
            fields: vec![
                field("mode", json!("light"), "dark, light, or system"),
                field("light_theme", json!("gruvbox-light"), "the theme used when light"),
                field("dark_theme", json!("gruvbox-dark"), "the theme used when dark"),
                field("accent", json!("theme"), "theme, a name, or #rrggbb"),
                field("replies", json!("filled"), "how replies' bubbles look: filled, outlined or minimal"),
                field("your_messages", json!("filled"), "how your messages' bubbles look: filled, outlined or minimal"),
                field("corners", json!(8), "corner radius 0 to 16"),
                field("colour.app_bg", json!("#ffffff"), "Window background (Surfaces)"),
                field("colour.panel_bg", json!("#eeeeee"), "Header and panels (Surfaces)"),
                field("colour.text", json!("#111111"), "Body text (Text)"),
                field("colour.text_dim", json!("#555555"), "Secondary text (Text)"),
                field("colour.assistant_bubble", json!("#f0f0f0"), "Replies (Messages)"),
                field("colour.border", json!("#cccccc"), "Borders (Surfaces)"),
            ],
            ..Default::default()
        };
        let c = changes(
            &json!({"set": {"colour": {"background": "#fdf3f6", "muted": "#a87886", "replies": "#fffafc"}, "bg": "#c0c0c0", "fg": "#000000",
                "colour.header": "#000080", "theme": "rose-pine-dawn", "replies": "outlined", "accent": "#e07a9a", "radius": 0, "Border": "#808080",
                "bubbles": true, "colour.reply.bg": "#ffffff"}}),
            &r,
        )
        .unwrap()
        .unwrap();
        let got = |k: &str| c.set.get(k).cloned().unwrap_or(Value::Null);
        assert_eq!(got("colour.text_dim"), "#a87886");
        assert_eq!(got("colour.assistant_bubble"), "#fffafc", "a colour for replies is their colour");
        assert_eq!(got("colour.text"), "#000000");
        assert_eq!(got("colour.panel_bg"), "#000080");
        assert_eq!(got("colour.border"), "#808080");
        assert_eq!(got("light_theme"), "rose-pine-dawn", "the theme in use");
        assert_eq!(got("replies"), "outlined", "a style for replies is their style");
        assert_eq!(got("accent"), "#e07a9a");
        assert_eq!(got("corners"), 0);
        assert!(c.set.contains_key("colour.app_bg"), "bg and colour.background both mean the window background");
        assert!(c.read_as.iter().any(|n| n == "bg as colour.app_bg"), "{:?}", c.read_as);
        let skipped = c.skipped.join("\n");
        assert!(skipped.contains("\"bubbles\"; did you mean"), "{skipped}");
        assert!(skipped.contains("\"colour.reply.bg\""), "{skipped}");
        assert!(skipped.contains("colour roles: app_bg, panel_bg"), "{skipped}");
    }
}
