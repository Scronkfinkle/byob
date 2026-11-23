//! Workspaces and layered settings in the UI: the New workspace form (with a folder
//! browser that lists the engine's folders, so it works from a browser too), a workspace's
//! settings window, a chat's settings popover, and the global defaults in Settings.
//!
//! Every settings editor shows, per field, what applies and where it comes from
//! ("from workspace"), and a Reset that makes the level inherit again. The engine resolves
//! and validates; this only edits one level's [`Layer`] and sends it.

use byob_protocol::profiles::{Profile, ProfileList};
use byob_protocol::workspace::{Approvals, EnvKind, Field, Layer, Level, ToolMode};
use byob_protocol::{ClientMsg, ConfigScope, ConfigView, EnvSupport, FolderList, NewFolder, ServerMsg, WorkspaceSummary};
use egui::{RichText, vec2};

use crate::envpanel::EnvPanel;
use crate::theme::{self, Tokens};

/// Settings a chat can change (and the order they're shown in).
pub const CHAT_FIELDS: &[Field] = &[
    Field::Endpoint,
    Field::Env,
    Field::Approvals,
    Field::ToolMode,
    Field::Compaction,
    Field::Subagents,
    Field::SubagentModel,
    Field::ToolCalls,
    Field::ModelImages,
    Field::Instructions,
];
/// How the chat answers: a name, what it means, and the line put first in the chat's
/// instructions (empty: as it comes). Kept in the instructions, so any engine honours it;
/// the editor shows the person's own text without it.
pub const STYLES: [(&str, &str, &str); 4] = [
    ("Normal", "As it comes", ""),
    ("Short", "Brief answers, no extra detail", "Answer style: short. Keep replies brief and to the point; skip background unless asked."),
    ("Explain more", "Says the why as it goes", "Answer style: explain more. Explain your reasoning and the why behind each step, in plain words."),
    (
        "Teach me",
        "Walks you through it so you learn",
        "Answer style: teach me. Teach as you go: explain ideas simply, check understanding, and suggest small steps to try.",
    ),
];

/// Which style instructions start with, and the person's own text after it.
pub fn split_style(text: &str) -> (usize, &str) {
    for (i, (_, _, line)) in STYLES.iter().enumerate().skip(1) {
        if let Some(rest) = text.strip_prefix(line)
            && (rest.is_empty() || rest.starts_with('\n'))
        {
            return (i, rest.trim_start_matches('\n'));
        }
    }
    (0, text)
}

/// Instructions from a style and the person's own text (`None` when both are empty).
pub fn join_style(style: usize, own: &str) -> Option<String> {
    let line = STYLES.get(style).map_or("", |s| s.2);
    let own = own.trim();
    match (line.is_empty(), own.is_empty()) {
        (true, true) => None,
        (true, false) => Some(own.to_owned()),
        (false, true) => Some(line.to_owned()),
        (false, false) => Some(format!("{line}\n\n{own}")),
    }
}

/// A field's name as people say it (the types crate keeps the technical one).
pub fn plain_label(f: Field) -> &'static str {
    match f {
        Field::Endpoint => "Model",
        Field::Approvals => "Mode",
        Field::Env => "Commands run on",
        Field::Image => "Container image",
        Field::ToolMode => "Tool calling",
        Field::Compaction => "Shorten long chats",
        Field::Subagents => "Helpers",
        Field::SubagentModel => "Helpers' model",
        Field::ToolCalls => "Steps per reply",
        Field::ModelImages => "Images it sees",
        Field::Instructions => "Instructions",
    }
}

/// Settings of a workspace.
pub const WORKSPACE_FIELDS: &[Field] = &[
    Field::Env,
    Field::Image,
    Field::Endpoint,
    Field::Approvals,
    Field::ToolMode,
    Field::Compaction,
    Field::Subagents,
    Field::SubagentModel,
    Field::ToolCalls,
    Field::ModelImages,
    Field::Instructions,
];
/// The choices for tool calls per turn; 0 is no limit.
const TOOL_CALL_LIMITS: [u32; 5] = [10, 30, 100, 300, 0];

fn tool_calls_label(n: u32) -> String {
    if n == 0 { "No limit".into() } else { format!("{n}") }
}

/// The choices for images sent to the model: JPEG qualities, and 0 (as they are).
const IMAGE_QUALITIES: [u8; 5] = [0, 95, 85, 70, 50];

fn image_quality_label(q: u8) -> String {
    match q {
        0 => "As they are".into(),
        85 => "Smaller (JPEG 85%)".into(),
        q => format!("JPEG {q}%"),
    }
}

/// What a select shows while it follows the level above: "Default · Ask before changes".
fn inherit(value: impl std::fmt::Display) -> String {
    format!("Default · {value}")
}

/// "On" or "Off".
fn on_off(on: bool) -> &'static str {
    if on { "On" } else { "Off" }
}

/// How full the context gets before a chat is shortened: the choices.
const COMPACT_AT: [u8; 5] = [50, 60, 70, 80, 90];

fn level_of(scope: &ConfigScope) -> Level {
    match scope {
        ConfigScope::Global => Level::Global,
        ConfigScope::Workspace { .. } => Level::Workspace,
        ConfigScope::Chat => Level::Chat,
    }
}

/// "inherited from workspace", "default".
fn inherited(from: Level) -> String {
    match from {
        Level::Default => "default".into(),
        Level::Flag => "set on the command line".into(),
        other => format!("from {}", other.label()),
    }
}

fn short_url(url: &str) -> String {
    crate::connect::short_url(url)
}

/// One level's settings, edited in place: each change is sent at once (text fields when
/// they lose focus), and the engine answers with the new [`ConfigView`].
/// The instructions field's id at `level` (tests find its rect by it).
fn instructions_id(level: Level) -> egui::Id {
    egui::Id::unique(("instructions-edit", format!("{level:?}")))
}

/// Text's first non-empty line without Markdown heading or list marks, at most `max` chars.
fn headline(text: &str, max: usize) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    let line = line.trim_start_matches(['#', '-', '*', '>', ' ']);
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        out.push('…');
    }
    out
}

pub struct LayerEditor {
    pub scope: ConfigScope,
    view: Option<ConfigView>,
    draft: Layer,
    asked: bool,
    /// Editing this level's own endpoint.
    endpoint_open: bool,
    url: String,
    model: String,
    key: String,
    instructions: String,
    image: String,
    /// The answer style at the start of this level's instructions ([`STYLES`]).
    style: usize,
    /// The label column's width when several groups are drawn one under another (the chat
    /// settings sheet), so their controls line up.
    pub label_w: f32,
    /// The engine's model profiles (`None`: an engine without them; the endpoint is typed).
    pub profiles: Option<ProfileList>,
    /// Leave out "default" / "from global settings" beside each control (its select
    /// already says "Default · …"); Reset still shows where this level sets one.
    pub quiet: bool,
}

impl LayerEditor {
    pub fn new(scope: ConfigScope) -> Self {
        Self {
            scope,
            view: None,
            draft: Layer::default(),
            asked: false,
            endpoint_open: false,
            url: String::new(),
            model: String::new(),
            key: String::new(),
            instructions: String::new(),
            image: String::new(),
            style: 0,
            label_w: 0.0,
            profiles: None,
            quiet: false,
        }
    }

    /// The engine's answer; ignored unless it is for this scope.
    pub fn received(&mut self, v: &ConfigView) -> bool {
        if v.scope != self.scope {
            return false;
        }
        self.draft = v.own.clone();
        self.endpoint_open = self.profiles.is_none() && (v.own.base_url.is_some() || v.own.model.is_some());
        self.url = v.own.base_url.clone().unwrap_or_else(|| v.effective.base_url.value.clone());
        self.model = v.own.model.clone().unwrap_or_else(|| v.effective.model.value.clone());
        self.key.clear();
        let (style, own) = split_style(v.own.instructions.as_deref().unwrap_or_default());
        self.style = style;
        self.instructions = own.to_owned();
        self.image = v.own.image.clone().unwrap_or_default();
        self.view = Some(v.clone());
        true
    }

    /// Ask the engine again (the chat in view changed, or the window reopened).
    pub fn refresh(&mut self) {
        self.asked = false;
        self.view = None;
    }

    pub fn view(&self) -> Option<&ConfigView> {
        self.view.as_ref()
    }

    fn send(&self, api_key: Option<String>) -> ClientMsg {
        ClientMsg::SetConfig { scope: self.scope.clone(), layer: self.draft.clone(), api_key }
    }

    /// Draw `fields`; returns a message for the engine.
    pub fn show(&mut self, ui: &mut egui::Ui, t: &Tokens, fields: &[Field]) -> Option<ClientMsg> {
        if !self.asked {
            self.asked = true;
            return Some(ClientMsg::GetConfig { scope: self.scope.clone() });
        }
        let Some(view) = self.view.clone() else {
            theme::spinner(ui, t, None);
            return None;
        };
        let level = level_of(&self.scope);
        let mut out = None;
        let avail = ui.available_width();
        let id = egui::Id::unique(("layer", format!("{:?}", self.scope), format!("{:?}", fields.first())));
        // The settings' rhythm (crate::form): the same label column and row gaps as the app settings.
        let grid = crate::form::grid(id, t, avail);
        let grid = if avail < 420.0 { grid.min_col_width(70.0) } else { grid };
        let grid = if self.label_w > 0.0 { grid.min_col_width(self.label_w) } else { grid };
        grid.show(ui, |ui| {
            for &f in fields {
                if f == Field::Image && view.effective.env.value != EnvKind::Container || f == Field::Instructions {
                    continue;
                }
                ui.label(RichText::new(plain_label(f)).color(t.text));
                ui.vertical(|ui| {
                    if let Some(m) = self.field(ui, t, &view, level, f) {
                        out = Some(m);
                    }
                });
                ui.end_row();
            }
        });
        // Instructions are long text: under the rows, the form's whole width.
        if fields.contains(&Field::Instructions) {
            ui.add_space(t.pad(10.0));
            // A project's instructions are its goal: what it's for, which every chat gets.
            let label = if level == Level::Workspace { "Goal and instructions" } else { Field::Instructions.label() };
            ui.label(RichText::new(label).color(t.text));
            ui.add_space(t.pad(4.0));
            if let Some(m) = self.field(ui, t, &view, level, Field::Instructions) {
                out = Some(m);
            }
        }
        out
    }

    /// "How it answers": the styles as a menu; returns the message that saves a change.
    pub fn answer_style(&mut self, ui: &mut egui::Ui, t: &Tokens) -> Option<ClientMsg> {
        self.view.as_ref()?;
        let mut picked = None;
        let id = egui::Id::unique(("layer", format!("{:?}", self.scope), "style"));
        let grid = crate::form::grid(id, t, ui.available_width());
        let grid = if self.label_w > 0.0 { grid.min_col_width(self.label_w) } else { grid };
        grid.show(ui, |ui| {
            ui.label(RichText::new("How it answers").color(t.text));
            let now = STYLES.get(self.style).map_or("Normal", |s| s.0);
            crate::form::combo(("answer-style", format!("{:?}", self.scope))).selected_text(now).width(200.0).show_ui(ui, |ui| {
                for (i, (name, what, _)) in STYLES.iter().enumerate() {
                    let mut job = egui::text::LayoutJob::default();
                    let body = egui::TextStyle::Body.resolve(ui.style());
                    let small = egui::TextStyle::Small.resolve(ui.style());
                    job.append(name, 0.0, egui::TextFormat::simple(body, t.text_strong));
                    job.append(&format!("   {what}"), 0.0, egui::TextFormat::simple(small, t.text_dim));
                    if ui.selectable_label(self.style == i, job).clicked() {
                        picked = Some(i);
                    }
                }
            });
            ui.end_row();
        });
        let i = picked.filter(|i| *i != self.style)?;
        self.style = i;
        self.draft.instructions = join_style(i, &self.instructions);
        Some(self.send(None))
    }

    /// Draw one field's control (and where its value comes from), for pages that lay out
    /// their own rows (the settings screen); returns a message for the engine.
    pub fn control(&mut self, ui: &mut egui::Ui, t: &Tokens, f: Field) -> Option<ClientMsg> {
        if !self.asked {
            self.asked = true;
            return Some(ClientMsg::GetConfig { scope: self.scope.clone() });
        }
        let Some(view) = self.view.clone() else {
            theme::spinner(ui, t, None);
            return None;
        };
        let level = level_of(&self.scope);
        self.field(ui, t, &view, level, f)
    }

    /// Where `f`'s value comes from, and a Reset when this level sets it.
    fn source(&mut self, ui: &mut egui::Ui, t: &Tokens, view: &ConfigView, level: Level, f: Field) -> Option<ClientMsg> {
        let e = &view.effective;
        let from = match f {
            Field::Endpoint => e.base_url.from.max(e.model.from),
            Field::ToolMode => e.tool_mode.from,
            Field::Approvals => e.approvals.from,
            Field::Compaction => e.auto_compact.from.max(e.compact_percent.from),
            Field::Subagents => e.subagents.from,
            Field::SubagentModel => e.subagent_profile.as_ref().map_or(Level::Default, |s| s.from),
            Field::ModelImages => e.image_quality.from,
            Field::ToolCalls => e.max_tool_calls.from,
            Field::Instructions => level,
            Field::Env => e.env.from,
            Field::Image => Level::Workspace,
        };
        if view.own.has(f) {
            if from == Level::Flag {
                ui.label(RichText::new("overridden on the command line").small().color(t.warning));
            }
            let reset =
                crate::form::button(ui, t, "Reset", true).on_hover_text(format!("Use the inherited value again (stop setting it per {})", level.label()));
            if reset.clicked() {
                self.draft.reset(f);
                self.endpoint_open = self.draft.base_url.is_some();
                return Some(self.send(if f == Field::Endpoint { Some(String::new()) } else { None }));
            }
        } else if !self.quiet && f != Field::Instructions && f != Field::Image && !(level == Level::Global && from == Level::Default) {
            ui.label(RichText::new(inherited(from)).small().color(t.text_faint));
        }
        None
    }

    fn field(&mut self, ui: &mut egui::Ui, t: &Tokens, view: &ConfigView, level: Level, f: Field) -> Option<ClientMsg> {
        let e = &view.effective;
        if f == Field::Instructions {
            return self.instructions(ui, t, view, level);
        }
        let mut out = None;
        ui.horizontal_wrapped(|ui| {
            match f {
                Field::Endpoint => out = self.endpoint(ui, t, view),
                Field::SubagentModel => out = self.subagent_model(ui, t, view),
                Field::ToolMode => {
                    let mut v = self.draft.tool_mode;
                    let shown = |m: Option<ToolMode>| m.map_or_else(|| inherit(e.tool_mode.value.label()), |m| m.label().to_owned());
                    crate::form::combo(("tool_mode", level)).selected_text(shown(v)).show_ui(ui, |ui| {
                        ui.selectable_value(&mut v, None, shown(None));
                        for m in ToolMode::ALL {
                            ui.selectable_value(&mut v, Some(m), m.label());
                        }
                    });
                    if v != self.draft.tool_mode {
                        self.draft.tool_mode = v;
                        out = Some(self.send(None));
                    }
                }
                Field::Approvals => {
                    let mut v = self.draft.approvals;
                    let shown = |a: Option<Approvals>| a.map_or_else(|| inherit(e.approvals.value.label()), |a| a.label().to_owned());
                    crate::form::combo(("approvals", level)).selected_text(shown(v)).show_ui(ui, |ui| {
                        ui.selectable_value(&mut v, None, shown(None));
                        for a in Approvals::ALL {
                            ui.selectable_value(&mut v, Some(a), a.label());
                        }
                    });
                    if v != self.draft.approvals {
                        self.draft.approvals = v;
                        out = Some(self.send(None));
                    }
                }
                Field::Compaction => {
                    // One select for both: off, or on once the context is this full.
                    let now = (self.draft.auto_compact, self.draft.compact_percent);
                    let label = |v: (Option<bool>, Option<u8>)| match v {
                        (None, None) if e.auto_compact.value => inherit(format!("On at {}%", e.compact_percent.value)),
                        (None, None) => inherit("Off"),
                        (Some(false), _) => "Off".to_owned(),
                        (_, p) => format!("On at {}%", p.unwrap_or(e.compact_percent.value)),
                    };
                    let mut v = now;
                    crate::form::combo(("compact", level))
                        .selected_text(label(v))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut v, (None, None), label((None, None)));
                            for at in COMPACT_AT {
                                ui.selectable_value(&mut v, (Some(true), Some(at)), format!("On at {at}%"));
                            }
                            ui.selectable_value(&mut v, (Some(false), None), "Off");
                        })
                        .response
                        .on_hover_text("Summarise older messages once a request fills this much of the model's context");
                    if v != now {
                        (self.draft.auto_compact, self.draft.compact_percent) = v;
                        out = Some(self.send(None));
                    }
                }
                Field::Subagents => {
                    let mut v = self.draft.subagents;
                    let shown = |s: Option<bool>| match s {
                        None => inherit(on_off(e.subagents.value)),
                        Some(true) => "On".into(),
                        Some(false) => "Off".into(),
                    };
                    crate::form::combo(("subagents", level))
                        .selected_text(shown(v))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut v, None, shown(None));
                            ui.selectable_value(&mut v, Some(true), "On");
                            ui.selectable_value(&mut v, Some(false), "Off");
                        })
                        .response
                        .on_hover_text("Whether the model may hand tasks to subagents (the delegate tool)");
                    if v != self.draft.subagents {
                        self.draft.subagents = v;
                        out = Some(self.send(None));
                    }
                }
                Field::ToolCalls => {
                    let mut v = self.draft.max_tool_calls;
                    let shown = |n: Option<u32>| n.map_or_else(|| inherit(tool_calls_label(e.max_tool_calls.value)), tool_calls_label);
                    crate::form::combo(("tool_calls", level))
                        .selected_text(shown(v))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut v, None, shown(None));
                            for n in TOOL_CALL_LIMITS {
                                ui.selectable_value(&mut v, Some(n), tool_calls_label(n));
                            }
                        })
                        .response
                        .on_hover_text("How many tools the model may call in one reply before it has to answer; long jobs (many files, many images) need more");
                    if v != self.draft.max_tool_calls {
                        self.draft.max_tool_calls = v;
                        out = Some(self.send(None));
                    }
                }
                Field::ModelImages => {
                    let mut v = self.draft.image_quality;
                    let shown = |q: Option<u8>| q.map_or_else(|| inherit(image_quality_label(e.image_quality.value)), image_quality_label);
                    crate::form::combo(("image_quality", level))
                        .selected_text(shown(v))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut v, None, shown(None));
                            for q in IMAGE_QUALITIES {
                                ui.selectable_value(&mut v, Some(q), image_quality_label(q));
                            }
                        })
                        .response
                        .on_hover_text(
                            "Images the model looks at (attachments, and images it reads) are re-encoded as JPEG when that makes them smaller: \
                             smaller requests, nearly the same picture. Transparency becomes white.",
                        );
                    if v != self.draft.image_quality {
                        self.draft.image_quality = v;
                        out = Some(self.send(None));
                    }
                }
                // Drawn by `instructions` (a block, not a row of controls).
                Field::Instructions => {}
                Field::Env => {
                    let chat = level == Level::Chat;
                    let mut v = self.draft.env;
                    let shown = |k: Option<EnvKind>| match k {
                        None if chat => inherit(e.env.value.label()),
                        None => "Auto".to_owned(),
                        Some(k) => k.label().to_owned(),
                    };
                    crate::form::combo(("env", level))
                        .selected_text(shown(v))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut v, None, shown(None));
                            for k in EnvKind::ALL.into_iter().filter(|k| if chat { k.per_chat() } else { *k != EnvKind::Auto }) {
                                ui.selectable_value(&mut v, Some(k), k.label());
                            }
                        })
                        .response
                        .on_hover_text(env_help(v.unwrap_or(e.env.value)));
                    if v != self.draft.env {
                        self.draft.env = v;
                        out = Some(self.send(None));
                    }
                }
                Field::Image => {
                    let mut custom = self.draft.image.is_some();
                    ui.vertical(|ui| {
                        let before = custom;
                        ui.radio_value(&mut custom, false, "The folder's devcontainer.json, else the default image");
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut custom, true, "Image");
                            let r = ui.add_enabled(custom, egui::TextEdit::singleline(&mut self.image).hint_text("python:3-slim").desired_width(180.0));
                            if r.lost_focus() && custom && !self.image.trim().is_empty() && Some(self.image.trim()) != self.draft.image.as_deref() {
                                self.draft.image = Some(self.image.trim().to_owned());
                                out = Some(self.send(None));
                            }
                        });
                        if before != custom && !custom {
                            self.draft.image = None;
                            out = Some(self.send(None));
                        }
                        if custom && !before && !self.image.trim().is_empty() {
                            self.draft.image = Some(self.image.trim().to_owned());
                            out = Some(self.send(None));
                        }
                    });
                }
            }
            // Where it comes from (or Reset), right of the control.
            if let Some(m) = self.source(ui, t, view, level, f) {
                out = Some(m);
            }
        });
        out
    }

    /// The instructions: what the levels above add (folded, to read in full), then this
    /// level's own text in a field as wide as there is room for, which grows with its text
    /// (and then scrolls), and Reset under it.
    fn instructions(&mut self, ui: &mut egui::Ui, t: &Tokens, view: &ConfigView, level: Level) -> Option<ClientMsg> {
        let mut out = None;
        // Clear of a scroll bar on the right.
        let w = (ui.available_width() - t.pad(10.0)).max(120.0);
        ui.vertical(|ui| {
            ui.set_width(w);
            for (from, text) in view.effective.instructions.iter().filter(|(l, _)| *l != level) {
                let lines = text.lines().filter(|l| !l.trim().is_empty()).count();
                let more = if lines > 1 { format!(" (+{} more lines)", lines - 1) } else { String::new() };
                let title = format!("From {}: {}{more}", from.label(), headline(text, 60));
                egui::CollapsingHeader::new(RichText::new(title).small().color(t.text_dim))
                    .id_salt(("inherited-instructions", format!("{level:?}{from:?}")))
                    .default_open(false)
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .fill(t.code_bg)
                            .stroke(egui::Stroke::new(1.0, t.code_border))
                            .corner_radius(t.round(4.0))
                            .inner_margin(t.pad(8.0))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                egui::ScrollArea::vertical().id_salt(("inherited-instructions-text", format!("{level:?}{from:?}"))).max_height(220.0).show(
                                    ui,
                                    |ui| {
                                        ui.add(egui::Label::new(RichText::new(text.as_str()).small().color(t.text_dim)).wrap());
                                    },
                                );
                            });
                    });
            }
            let hint = match level {
                Level::Global => "What every chat should know",
                Level::Workspace => "What every chat in this project should know",
                _ => "What this chat should know",
            };
            // Tall enough for its lines (wrapped ones scroll), 4 to 18 of them.
            let row_h = ui.text_style_height(&egui::TextStyle::Body);
            #[allow(clippy::cast_precision_loss)] // a line count
            let rows = (self.instructions.lines().count() + 1).clamp(4, 18) as f32;
            let r = egui::ScrollArea::vertical()
                .id_salt(("instructions", format!("{level:?}")))
                .min_scrolled_height(row_h * rows + t.pad(8.0))
                .max_height(row_h * 18.0 + t.pad(8.0))
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.instructions)
                            .id(instructions_id(level))
                            .desired_rows(4)
                            .desired_width(f32::INFINITY)
                            .hint_text(hint),
                    )
                })
                .inner;
            let text = join_style(self.style, &self.instructions);
            if r.lost_focus() && text != self.draft.instructions {
                self.draft.instructions = text;
                out = Some(self.send(None));
            }
            if let Some(m) = self.source(ui, t, view, level, Field::Instructions) {
                out = Some(m);
            }
        });
        out
    }

    /// The endpoint: what applies, or this level's own URL, model and key.
    fn endpoint(&mut self, ui: &mut egui::Ui, t: &Tokens, view: &ConfigView) -> Option<ClientMsg> {
        match self.profiles.clone() {
            Some(list) => self.profile_picker(ui, t, view, &list),
            None => self.typed_endpoint(ui, t, view),
        }
    }

    /// Pick one of the saved models, inherit, or type one ("Custom…", saved as a model).
    fn profile_picker(&mut self, ui: &mut egui::Ui, t: &Tokens, view: &ConfigView, list: &ProfileList) -> Option<ClientMsg> {
        let e = &view.effective;
        let own = view.own.profile.clone();
        let named = |p: &Profile| crate::models::menu_label(p);
        // What a level inheriting the endpoint gets, by its profile's name if it is one.
        // Several profiles may share an endpoint: the default one first.
        let main = list.roles.get(byob_protocol::profiles::ROLE_MAIN).and_then(|id| list.get(id));
        let inherited = main
            .into_iter()
            .chain(&list.items)
            .find(|v| v.profile.points_at(&e.base_url.value, &e.model.value))
            .map_or_else(|| e.model.value.clone(), |v| v.profile.label().to_owned());
        // A server whose model is literally called "default" reads as its own default.
        let inherited = if inherited.eq_ignore_ascii_case("default") { "the server's model".to_owned() } else { inherited };
        let typed = own.is_none() && (view.own.base_url.is_some() || view.own.model.is_some());
        let current = match &own {
            Some(id) => list.get(id).map_or_else(|| "A deleted model".to_owned(), |v| named(&v.profile)),
            None if typed => format!("{} · {}", e.model.value, short_url(&e.base_url.value)),
            None => inherit(&inherited),
        };
        let mut out = None;
        ui.vertical(|ui| {
            let mut picked: Option<Option<String>> = None;
            let mut custom = false;
            let width = ui.available_width().min(crate::form::SELECT_W);
            crate::form::combo(("profile", format!("{:?}", self.scope))).selected_text(current).width(width).show_ui(ui, |ui| {
                if ui.selectable_label(own.is_none() && !typed, inherit(&inherited)).clicked() {
                    picked = Some(None);
                }
                for v in &list.items {
                    let r = ui.selectable_label(own.as_deref() == Some(v.profile.id.as_str()), named(&v.profile));
                    let r = if v.profile.description.is_empty() { r } else { r.on_hover_text(&v.profile.description) };
                    if r.clicked() {
                        picked = Some(Some(v.profile.id.clone()));
                    }
                }
                ui.separator();
                if ui
                    .selectable_label(self.endpoint_open, "Custom…")
                    .on_hover_text("Type a server and model; it is saved as a model you can pick again")
                    .clicked()
                {
                    custom = true;
                }
            });
            if custom {
                self.endpoint_open = true;
                self.url.clone_from(&e.base_url.value);
                self.model.clear();
            }
            if let Some(p) = picked {
                self.endpoint_open = false;
                if p != own {
                    out = Some(ClientMsg::SetLevelProfile { scope: self.scope.clone(), profile: p });
                }
            }
            if own.as_ref().is_some_and(|id| list.get(id).is_none()) {
                ui.label(RichText::new("The model picked here was deleted, so it inherits one.").small().color(t.warning));
            } else if typed {
                ui.label(RichText::new("Typed here before models could be saved.").small().color(t.text_faint));
            }
            if self.endpoint_open {
                let w = ui.available_width().min(340.0);
                ui.add(egui::TextEdit::singleline(&mut self.url).hint_text("http://127.0.0.1:8080/v1").desired_width(w));
                ui.add(egui::TextEdit::singleline(&mut self.model).hint_text("model, as the server names it").desired_width(w));
                ui.add(egui::TextEdit::singleline(&mut self.key).password(true).hint_text("API key, if the server needs one").desired_width(w));
                ui.horizontal(|ui| {
                    let ok = !self.url.trim().is_empty() && !self.model.trim().is_empty();
                    if ui.add_enabled(ok, egui::Button::new("Save and use")).on_hover_text("Saved under Settings → Models too").clicked() {
                        let model = self.model.trim().to_owned();
                        let profile = Profile { name: model.clone(), base_url: self.url.trim().to_owned(), model, ..Profile::default() };
                        let api_key = Some(std::mem::take(&mut self.key)).map(|k| k.trim().to_owned()).filter(|k| !k.is_empty());
                        out = Some(ClientMsg::CreateProfile { profile, api_key, assign: Some(self.scope.clone()), make_main: false });
                        self.endpoint_open = false;
                    }
                    if ui.small_button("Cancel").clicked() {
                        self.endpoint_open = false;
                    }
                });
                ui.label(RichText::new("The key stays on the machine byob runs on and is only sent to this server.").small().color(t.text_faint));
            }
        });
        out
    }

    /// The saved model this level's subagents run on (unless the call or the agent picks one).
    fn subagent_model(&mut self, ui: &mut egui::Ui, t: &Tokens, view: &ConfigView) -> Option<ClientMsg> {
        const SAME: &str = "Same as the chat";
        let Some(list) = self.profiles.clone().filter(|l| !l.items.is_empty()) else {
            // Nothing to choose from: the same box as the others, quiet, saying why.
            crate::form::static_select(ui, t, SAME, crate::form::SELECT_W.min(ui.available_width()))
                .on_hover_text("Save models under Settings → Models to run helpers on another one");
            return None;
        };
        let name = |id: &str| list.get(id).map_or_else(|| "A deleted model".to_owned(), |v| v.profile.label().to_owned());
        let own = self.draft.subagent_profile.clone();
        let inherited = match (&own, &view.effective.subagent_profile) {
            (None, Some(s)) => inherit(name(&s.value)),
            (None, None) => inherit(SAME),
            (Some(_), _) => "Default".to_owned(),
        };
        let mut v = own.clone();
        crate::form::combo(("subagent_model", format!("{:?}", self.scope)))
            .selected_text(own.as_deref().map_or_else(|| inherited.clone(), name))
            .width(ui.available_width().min(crate::form::SELECT_W))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut v, None, inherited.clone());
                for p in &list.items {
                    let r = ui.selectable_value(&mut v, Some(p.profile.id.clone()), crate::models::menu_label(&p.profile));
                    if !p.profile.description.is_empty() {
                        r.on_hover_text(&p.profile.description);
                    }
                }
            })
            .response
            .on_hover_text("The model delegated tasks run on, unless the delegate call or the agent's file names another");
        if own.as_ref().is_some_and(|id| list.get(id).is_none()) {
            ui.label(RichText::new("The model picked here was deleted, so it inherits one.").small().color(t.warning));
        }
        if v != own {
            self.draft.subagent_profile = v;
            return Some(self.send(None));
        }
        None
    }

    /// An engine without model profiles: type this level's endpoint.
    fn typed_endpoint(&mut self, ui: &mut egui::Ui, t: &Tokens, view: &ConfigView) -> Option<ClientMsg> {
        let e = &view.effective;
        if !self.endpoint_open {
            ui.label(RichText::new(format!("{} · {}", e.model.value, short_url(&e.base_url.value))).color(t.text_dim));
            if ui.small_button("Use another…").on_hover_text("Pick an endpoint and model for this level only").clicked() {
                self.endpoint_open = true;
            }
            return None;
        }
        let mut out = None;
        ui.vertical(|ui| {
            let w = ui.available_width().min(340.0);
            ui.add(egui::TextEdit::singleline(&mut self.url).hint_text("http://127.0.0.1:8080/v1").desired_width(w));
            ui.add(egui::TextEdit::singleline(&mut self.model).hint_text("model").desired_width(w));
            let hint = if view.has_key { "API key saved (leave empty to keep it)" } else { "API key, if the server needs one" };
            ui.add(egui::TextEdit::singleline(&mut self.key).password(true).hint_text(hint).desired_width(w));
            ui.horizontal(|ui| {
                let ok = !self.url.trim().is_empty() && !self.model.trim().is_empty();
                if ui.add_enabled(ok, egui::Button::new("Apply")).clicked() {
                    self.draft.base_url = Some(self.url.trim().to_owned());
                    self.draft.model = Some(self.model.trim().to_owned());
                    let key = Some(std::mem::take(&mut self.key)).filter(|k| !k.trim().is_empty());
                    out = Some(self.send(key));
                }
                if !view.own.has(Field::Endpoint) && ui.small_button("Cancel").clicked() {
                    self.endpoint_open = false;
                }
            });
            ui.label(RichText::new("The key stays on the machine byob runs on and is only sent to this server.").small().color(t.text_faint));
        });
        out
    }
}

/// One line on what an environment means.
pub fn env_help(k: EnvKind) -> &'static str {
    match k {
        EnvKind::Auto => "On this machine, in byob's sandbox; byob asks once before loading the folder's project environment or using its devcontainer.json.",
        EnvKind::Host => "Always on this machine, in byob's sandbox; never in a container.",
        EnvKind::Nix => "On this machine, with the folder's Nix dev shell (flake.nix / shell.nix) loaded without asking. install_packages uses Nix.",
        EnvKind::Container => "In this workspace's own Docker/Podman container, with the folder mounted at the same path. No questions on the first command.",
    }
}

/// A folder name like the engine makes (`My Data!` → `my-data`), for the preview.
fn slug(name: &str) -> String {
    let mut s = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
    }
    let s = s.trim_end_matches('-').chars().take(48).collect::<String>();
    if s.is_empty() { "project".into() } else { s }
}

/// `path` as [`short_home`] writes it, kept to about `max` characters by dropping folders
/// from the front: the end (the project's own folder) is the part people look for.
/// `/very/long/…/projects/alpha` → `…/projects/alpha`.
pub fn short_path(path: &str, max: usize) -> String {
    let short = short_home(path);
    if short.chars().count() <= max {
        return short;
    }
    let parts: Vec<&str> = short.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut len = 1;
    for part in parts.iter().rev() {
        let n = part.chars().count() + 1;
        if !kept.is_empty() && len + n > max {
            break;
        }
        kept.push(part);
        len += n;
    }
    kept.reverse();
    let tail = kept.join("/");
    // One folder longer than everything: its end.
    if tail.chars().count() > max {
        let skip = tail.chars().count() - max.saturating_sub(1);
        return format!("…{}", tail.chars().skip(skip).collect::<String>());
    }
    format!("…/{tail}")
}

/// The last part of a folder path (`/home/me/Projects/Alpha/` → `Alpha`): a new
/// workspace's name until the user types one.
/// `path` with the home folder written `~`, as people read it. The browser UI can't ask
/// where home is, so the usual home folders (`/Users/<name>`, `/home/<name>`,
/// `C:\Users\<name>`) are recognised by their shape too.
pub fn short_home(path: &str) -> String {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_default();
    let home = home.trim_end_matches(['/', '\\']);
    if let Some(rest) = path.strip_prefix(home).filter(|r| !home.is_empty() && (r.is_empty() || r.starts_with(['/', '\\']))) {
        return format!("~{rest}");
    }
    let parts: Vec<&str> = path.split(['/', '\\']).collect();
    let user_root = match parts.as_slice() {
        ["", "Users" | "home", _, ..] => Some(3),
        [drive, "Users", _, ..] if drive.ends_with(':') => Some(3),
        _ => None,
    };
    match user_root.filter(|&n| parts.len() >= n) {
        Some(n) if parts.len() == n => "~".to_owned(),
        Some(n) => format!("~/{}", parts[n..].join("/")),
        None => path.to_owned(),
    }
}

pub fn folder_name(path: &str) -> String {
    path.trim().trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or_default().to_owned()
}

/// Where a new project's own folder comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The first folder the person added: chats work there.
    Folder,
    /// None added: byob makes one for it in its projects folder.
    Empty,
}

/// Longest goal the form takes (it becomes the project's instructions).
const MAX_GOAL: usize = 4000;

/// "New project": its name, the instructions every chat in it gets (its
/// goal: the instructions every chat in it gets), and the folders it works in, if any. With
/// none, byob keeps its files in a folder of its own. Where commands run waits under
/// Advanced (Auto unless changed).
pub struct NewWorkspaceForm {
    pub name: String,
    /// The user typed the name: adding a folder doesn't rename it.
    name_edited: bool,
    pub goal: String,
    /// Folders it works in; the first is where its chats run.
    pub folders: Vec<String>,
    browser: Option<FolderList>,
    browsing: bool,
    pub env: EnvKind,
    /// The Advanced section is open.
    pub advanced: bool,
    use_image: bool,
    image: String,
    check: Option<(bool, String)>,
    checking: bool,
    creating: bool,
    error: Option<String>,
    /// Put the keyboard in the name field on the next frame.
    focus_name: bool,
    /// Start adding a folder on the next frame (started from "From a folder").
    pick_now: bool,
    /// Enter was pressed in the name (the buttons, drawn after, act on it).
    enter: bool,
    #[cfg(not(target_arch = "wasm32"))]
    picked: (std::sync::mpsc::Sender<Option<String>>, std::sync::mpsc::Receiver<Option<String>>),
}

impl Default for NewWorkspaceForm {
    fn default() -> Self {
        Self {
            name: String::new(),
            name_edited: false,
            goal: String::new(),
            folders: Vec::new(),
            browser: None,
            browsing: false,
            env: EnvKind::Auto,
            advanced: false,
            use_image: false,
            image: String::new(),
            check: None,
            checking: false,
            creating: false,
            error: None,
            focus_name: true,
            pick_now: false,
            enter: false,
            #[cfg(not(target_arch = "wasm32"))]
            picked: std::sync::mpsc::channel(),
        }
    }
}

/// What the form wants after a frame.
#[derive(Default)]
pub struct FormOutput {
    pub msgs: Vec<ClientMsg>,
    pub close: bool,
}

impl NewWorkspaceForm {
    /// Start with an environment chosen (scripted runs): another than Auto opens Advanced.
    pub fn with_env(env: EnvKind) -> Self {
        Self { env, advanced: env != EnvKind::Auto, ..Self::default() }
    }

    /// Start on a folder (dropped on the window, or scripted): named after it.
    pub fn with_folder(path: &str) -> Self {
        let mut f = Self::default();
        f.add_folder(path);
        f
    }

    /// Start one way: from a folder (the picker opens straight away on the desktop), or
    /// with the keyboard in the name.
    pub fn with_start(start: crate::projects::Start) -> Self {
        match start {
            crate::projects::Start::Link => Self { pick_now: true, focus_name: false, ..Self::default() },
            crate::projects::Start::Choose | crate::projects::Start::Empty => Self::default(),
        }
    }

    /// Where its own folder comes from.
    pub fn source(&self) -> Source {
        if self.folders.is_empty() { Source::Empty } else { Source::Folder }
    }

    /// Nothing typed or added yet: closing loses nothing.
    pub fn untouched(&self) -> bool {
        self.name.trim().is_empty() && self.goal.trim().is_empty() && self.folders.is_empty()
    }

    /// Add a folder (picked, browsed, dropped). The first names the project until a name is
    /// typed; a repeat or an empty path is ignored; at most [`MAX_LINKED_FOLDERS`] besides
    /// the first.
    pub fn add_folder(&mut self, path: &str) {
        let path = path.trim();
        if path.is_empty() || self.folders.iter().any(|f| f == path) || self.folders.len() > byob_protocol::workspace::MAX_LINKED_FOLDERS {
            return;
        }
        if self.folders.is_empty() && !self.name_edited {
            self.name = folder_name(path);
        }
        self.folders.push(path.to_owned());
    }

    fn apply(&mut self, msg: &ServerMsg) -> bool {
        match msg {
            ServerMsg::Folders(list) => {
                self.browser = Some(list.clone());
                self.browsing = false;
            }
            ServerMsg::ContainersChecked { ok, message } => {
                self.check = Some((*ok, message.clone()));
                self.checking = false;
            }
            ServerMsg::WorkspaceCreated { error, .. } => {
                self.creating = false;
                self.error.clone_from(error);
            }
            _ => return false,
        }
        true
    }

    /// The layer the new project starts with: its goal is its instructions.
    fn layer(&self) -> Layer {
        Layer {
            instructions: Some(self.goal.trim().to_owned()).filter(|g| !g.is_empty()),
            env: Some(self.env).filter(|e| *e != EnvKind::Auto),
            image: Some(self.image.trim().to_owned()).filter(|i| self.env == EnvKind::Container && self.use_image && !i.is_empty()),
            ..Layer::default()
        }
    }

    /// Create was pressed: the message for the engine. The first folder is its own (chats
    /// run there); the others are linked. No folder: byob makes one.
    pub fn create_msg(&mut self) -> ClientMsg {
        self.creating = true;
        self.error = None;
        let folder = match self.folders.first() {
            Some(f) => NewFolder::Existing { path: f.clone() },
            None => NewFolder::Create,
        };
        let folders = self.folders.iter().skip(1).cloned().collect();
        ClientMsg::CreateWorkspace { name: self.name.trim().to_owned(), folder, config: self.layer(), folders }
    }

    /// Why Create can't be pressed yet, if so.
    fn problem(&self, support: &EnvSupport) -> Option<String> {
        if self.name.trim().is_empty() {
            return Some("Give it a name.".into());
        }
        if self.env == EnvKind::Container {
            if support.runtime.is_none() {
                return Some("Neither Docker nor Podman is installed on the machine byob runs on.".into());
            }
            if self.use_image && !byob_types::config::valid_image(self.image.trim()) {
                return Some("The image should look like python:3-slim.".into());
            }
        }
        None
    }

    /// The whole form: its fields, then its buttons (tests, and anywhere it isn't split).
    pub fn show(&mut self, ui: &mut egui::Ui, t: &Tokens, root: &str, support: &EnvSupport) -> FormOutput {
        let mut out = self.body(ui, t, root, support);
        let actions = self.actions(ui, t, support);
        out.msgs.extend(actions.msgs);
        out.close |= actions.close;
        out
    }

    /// Its fields (they scroll when the window is short; the buttons stay in view). One
    /// rhythm: label, 6 points, field, its help under it, 20 points to the next group.
    pub fn body(&mut self, ui: &mut egui::Ui, t: &Tokens, root: &str, support: &EnvSupport) -> FormOutput {
        let mut out = FormOutput::default();
        #[cfg(not(target_arch = "wasm32"))]
        while let Ok(p) = self.picked.1.try_recv() {
            if let Some(p) = p {
                self.add_folder(&p);
                self.browser = None;
            }
        }
        if std::mem::take(&mut self.pick_now) {
            self.begin_add(ui.ctx(), native_dialog(), &mut out);
        }
        let field_w = ui.available_width().min(520.0);
        ui.spacing_mut().item_spacing.y = 0.0;
        // Its name: the field says what it is, a folder at its start.
        let name_id = egui::Id::unique("new-project-name");
        let name = form_field_margin(ui, t, name_id, egui::Margin { left: 40, right: 12, top: 11, bottom: 11 }, |frame| {
            egui::TextEdit::singleline(&mut self.name).id(name_id).hint_text("Project name").char_limit(80).desired_width(field_w).frame(frame)
        });
        crate::icons::folder(
            ui.painter(),
            egui::Rect::from_center_size(egui::pos2(name.rect.left() + 20.0, name.rect.center().y), egui::vec2(16.0, 16.0)),
            t.text_dim,
        );
        if std::mem::take(&mut self.focus_name) {
            name.request_focus();
        }
        if name.changed() {
            self.name_edited = !self.name.trim().is_empty();
        }
        self.enter |= name.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        ui.add_space(20.0);
        // What every chat in it should know: added to each one's instructions.
        form_label(ui, t, "Instructions", true);
        let goal_id = egui::Id::unique("new-project-goal");
        form_field(ui, t, goal_id, |frame| {
            egui::TextEdit::multiline(&mut self.goal)
                .id(goal_id)
                .hint_text("What every chat in this project should know")
                .char_limit(MAX_GOAL)
                .desired_rows(3)
                .desired_width(field_w)
                .frame(frame)
        });
        ui.add_space(20.0);
        // The folders it works in, if any.
        form_label(ui, t, "Folders", true);
        self.folder_list(ui, t, root, field_w, &mut out);
        ui.add_space(16.0);
        // Where commands run, folded: most people never change it.
        if advanced_row(ui, t, self.advanced, self.env.label()).clicked() {
            self.advanced = !self.advanced;
        }
        if self.advanced {
            ui.add_space(8.0);
            ui.spacing_mut().item_spacing.y = 4.0;
            self.advanced_rows(ui, t, support, field_w, &mut out);
        }
        out
    }

    /// Why it can't be made yet (if so), then Cancel and Create project.
    pub fn actions(&mut self, ui: &mut egui::Ui, t: &Tokens, support: &EnvSupport) -> FormOutput {
        let mut out = FormOutput::default();
        let enter = std::mem::take(&mut self.enter);
        ui.add_space(20.0);
        let problem = self.problem(support);
        if let Some(e) = &self.error {
            ui.add(egui::Label::new(RichText::new(e).size(t.text_size(12.0)).color(t.danger)).wrap());
            ui.add_space(8.0);
        }
        // The action on the right, Cancel beside it (as desktop dialogs do).
        let h = (ui.text_style_height(&egui::TextStyle::Button) + 18.0).max(36.0).ceil();
        ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), h + 4.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let can = problem.is_none() && !self.creating;
            let create = dialog_button(ui, t, "Create project", true, can, h);
            let create = match &problem {
                Some(p) => create.on_hover_text(p.as_str()),
                None => create.on_hover_text("Creates it and opens a new chat in it"),
            };
            if can && (create.clicked() || enter) {
                out.msgs.push(self.create_msg());
            }
            if dialog_button(ui, t, "Cancel", false, true, h).clicked() {
                out.close = true;
            }
            if self.creating {
                theme::spinner(ui, t, None);
            }
        });
        // Room under the buttons: the scroll area around the form clips at its content's edge.
        ui.add_space(4.0);
        out
    }

    /// The folders it works in: what they're for, each one (the first is where its chats
    /// run) with a way to take it off, then one Add folder button (the system's dialog; the
    /// engine's folder list where there's none, as in the browser).
    fn folder_list(&mut self, ui: &mut egui::Ui, t: &Tokens, root: &str, width: f32, out: &mut FormOutput) {
        let root_shown = if root.is_empty() { "~/byob-workspaces".to_owned() } else { root.trim_end_matches('/').to_owned() };
        let leaf = if self.name.trim().is_empty() { "<name>".to_owned() } else { slug(&self.name) };
        let mut remove = None;
        for (i, f) in self.folders.iter().enumerate() {
            if folder_row(ui, t, f, width, i == 0, true) {
                remove = Some(i);
            }
            ui.add_space(6.0);
        }
        if let Some(i) = remove {
            self.folders.remove(i);
        }
        let mut add = false;
        if self.folders.is_empty() {
            // None yet: one quiet box that says what to do, with its button.
            add = empty_folders(ui, t, width).on_hover_text(format!("Without a folder, byob keeps its files in {root_shown}/{leaf}")).clicked();
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if !self.folders.is_empty() {
                add = small_button(ui, t, crate::icons::folder_plus, "Add another").on_hover_text("Its files stay where they are").clicked();
            }
            if add {
                self.begin_add(ui.ctx(), native_dialog(), out);
            }
            if self.browsing {
                theme::spinner(ui, t, None);
            }
        });
        if let Some(list) = self.browser.clone()
            && let Some(go) = browser(ui, t, &list, width)
        {
            match go {
                Some(path) => {
                    self.browsing = true;
                    out.msgs.push(ClientMsg::ListFolders { path: Some(path) });
                }
                None => {
                    if list.error.is_none() {
                        self.add_folder(&list.path);
                    }
                    self.browser = None;
                }
            }
        }
    }

    /// Add a folder: the system's dialog where there is one (`native`), else the engine's
    /// folder list (the browser, a machine without a dialog).
    #[cfg_attr(target_arch = "wasm32", allow(unused_variables))]
    fn begin_add(&mut self, ctx: &egui::Context, native: bool, out: &mut FormOutput) {
        #[cfg(not(target_arch = "wasm32"))]
        if native {
            pick_folder(ctx, self.picked.0.clone());
            return;
        }
        self.browsing = true;
        out.msgs.push(ClientMsg::ListFolders { path: self.folders.last().cloned() });
    }

    /// Where commands run: one aligned row per choice (radio, icon, name), what it means,
    /// and the container's options.
    fn advanced_rows(&mut self, ui: &mut egui::Ui, t: &Tokens, support: &EnvSupport, field_w: f32, out: &mut FormOutput) {
        ui.label(RichText::new("Where commands run").color(t.text));
        ui.add_space(2.0);
        ui.spacing_mut().item_spacing.y = 0.0;
        for k in EnvKind::ALL {
            if env_row(ui, t, field_w.min(320.0), k, self.env == k).clicked() && self.env != k {
                self.env = k;
                if k == EnvKind::Container && support.runtime.is_some() && self.check.is_none() && !self.checking {
                    self.checking = true;
                    out.msgs.push(ClientMsg::CheckContainers);
                }
            }
        }
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.add_space(4.0);
        ui.add(egui::Label::new(RichText::new(env_help(self.env)).small().color(t.text_dim)).wrap());
        match self.env {
            EnvKind::Nix if !support.nix => {
                ui.label(RichText::new("Nix isn't installed on the machine byob runs on; commands would run without a dev shell.").small().color(t.warning));
            }
            EnvKind::Container => self.container_rows(ui, t, support, field_w, out),
            _ => {}
        }
    }

    fn container_rows(&mut self, ui: &mut egui::Ui, t: &Tokens, support: &EnvSupport, field_w: f32, out: &mut FormOutput) {
        let Some(runtime) = &support.runtime else {
            ui.label(RichText::new("Neither Docker nor Podman is installed on the machine byob runs on.").small().color(t.danger));
            return;
        };
        ui.radio_value(&mut self.use_image, false, "The folder's .devcontainer/devcontainer.json (else python:3-slim)");
        ui.horizontal(|ui| {
            ui.radio_value(&mut self.use_image, true, "Image");
            ui.add_enabled(
                self.use_image,
                egui::TextEdit::singleline(&mut self.image).hint_text("python:3-slim").desired_width((field_w - 90.0).clamp(120.0, 260.0)),
            );
        });
        ui.horizontal_wrapped(|ui| {
            match &self.check {
                _ if self.checking => {
                    theme::spinner(ui, t, None);
                    ui.label(RichText::new(format!("Checking {runtime}…")).small().color(t.text_dim));
                }
                Some((true, m)) => {
                    ui.label(RichText::new(format!("● {m}")).small().color(t.success));
                }
                Some((false, m)) => {
                    ui.label(RichText::new(m).small().color(t.danger));
                }
                None => {}
            }
            if !self.checking && ui.small_button(format!("Check {runtime}")).clicked() {
                self.checking = true;
                out.msgs.push(ClientMsg::CheckContainers);
            }
        });
    }
}

/// One "where commands run" choice: a radio dot, the environment's icon and its name, all
/// on one line (the whole row is the click target).
fn env_row(ui: &mut egui::Ui, t: &Tokens, width: f32, kind: EnvKind, on: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 26.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if resp.hovered() {
            p.rect_filled(rect, t.round(5.0), t.hover);
        }
        let dot = egui::pos2(rect.left() + 6.0 + 7.0, rect.center().y);
        p.circle(dot, 7.0, t.field_bg, egui::Stroke::new(1.0, if on { t.accent } else { t.field_border }));
        if on {
            p.circle_filled(dot, 3.5, t.accent);
        }
        let icon = egui::Rect::from_center_size(egui::pos2(dot.x + 7.0 + 10.0 + 7.0, rect.center().y), vec2(14.0, 14.0));
        crate::icons::env(p, icon, kind, crate::icons::env_color(t, kind));
        let color = if on { t.text_strong } else { t.text };
        let label = p.layout_no_wrap(kind.label().to_owned(), egui::TextStyle::Body.resolve(ui.style()), color);
        p.galley(egui::pos2(icon.right() + 8.0, rect.center().y - label.size().y / 2.0), label, color);
    }
    resp.on_hover_text(env_help(kind))
}

/// A form field's label: 13 points, strong; "optional" faint beside it.
fn form_label(ui: &mut egui::Ui, t: &Tokens, text: &str, optional: bool) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(RichText::new(text).size(t.text_size(13.0)).color(t.text_strong));
        if optional {
            ui.label(RichText::new("optional").size(t.text_size(12.0)).color(t.text_faint));
        }
    });
    ui.add_space(6.0);
}

/// No folders yet: an outlined box, "Add a folder on this computer" and an Add pill. The
/// whole box is the button.
fn empty_folders(ui: &mut egui::Ui, t: &Tokens, width: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 104.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let edge = if resp.hovered() { t.text_faint } else { t.field_border };
        p.rect_stroke(rect, t.round(crate::form::CONTROL_R + 2.0), egui::Stroke::new(1.0, edge), egui::StrokeKind::Inside);
        let body = egui::TextStyle::Body.resolve(ui.style());
        let g = p.layout_no_wrap("Add a folder on this computer".to_owned(), body.clone(), t.text_dim);
        p.galley(egui::pos2(rect.center().x - g.size().x / 2.0, rect.top() + 24.0), g, t.text_dim);
        let label = p.layout_no_wrap("Add".to_owned(), body, t.text);
        let pill = egui::Rect::from_center_size(egui::pos2(rect.center().x, rect.top() + 68.0), egui::vec2(label.size().x + 50.0, 32.0));
        p.rect_filled(pill, pill.height() / 2.0, if resp.hovered() { crate::form::soft_hover(t) } else { crate::form::soft(t) });
        crate::icons::folder_plus(p, egui::Rect::from_center_size(egui::pos2(pill.left() + 20.0, pill.center().y), egui::vec2(15.0, 15.0)), t.text);
        p.galley(egui::pos2(pill.left() + 32.0, pill.center().y - label.size().y / 2.0), label, t.text);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A text field in the form's style: the field colours, a 1-point border and 8-point corners;
/// focused, an accent border and a soft accent ring around it (no heavy outline).
pub(crate) fn form_field<'t>(ui: &mut egui::Ui, t: &Tokens, id: egui::Id, edit: impl FnOnce(egui::Frame) -> egui::TextEdit<'t>) -> egui::Response {
    form_field_margin(ui, t, id, egui::Margin::symmetric(12, 9), edit)
}

/// [`form_field`] with its own inner margins (room for an icon inside it).
pub(crate) fn form_field_margin<'t>(
    ui: &mut egui::Ui,
    t: &Tokens,
    id: egui::Id,
    margin: egui::Margin,
    edit: impl FnOnce(egui::Frame) -> egui::TextEdit<'t>,
) -> egui::Response {
    let focused = ui.memory(|m| m.has_focus(id));
    let radius = t.round(crate::form::CONTROL_R);
    let frame = egui::Frame::new()
        .fill(t.field_bg)
        .stroke(egui::Stroke::new(1.0, if focused { t.accent } else { t.field_border }))
        .corner_radius(radius)
        .inner_margin(margin);
    let r = ui.add(edit(frame));
    if focused {
        ui.painter().rect_stroke(r.rect.expand(2.0), radius + 2.0, egui::Stroke::new(3.0, t.accent.gamma_multiply(0.22)), egui::StrokeKind::Outside);
    }
    r
}

/// One folder of the project: its icon, its path (shortened from the front; in full on
/// hover), "Chats start here" on the first, and × to take it off. Returns whether × was
/// clicked.
fn folder_row(ui: &mut egui::Ui, t: &Tokens, path: &str, width: f32, first: bool, removable: bool) -> bool {
    let h = 40.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, h), egui::Sense::hover());
    let radius = t.round(crate::form::CONTROL_R);
    ui.painter().rect(rect, radius, t.field_bg, egui::Stroke::new(1.0, t.field_border), egui::StrokeKind::Inside);
    crate::icons::folder(ui.painter(), egui::Rect::from_center_size(egui::pos2(rect.left() + 20.0, rect.center().y), egui::vec2(16.0, 16.0)), t.text_dim);
    let x_rect = if removable {
        egui::Rect::from_center_size(egui::pos2(rect.right() - 20.0, rect.center().y), egui::vec2(26.0, 26.0))
    } else {
        egui::Rect::from_min_max(egui::pos2(rect.right() - 4.0, rect.top()), rect.max)
    };
    let x = ui.interact(x_rect, egui::Id::unique(("folder-row-x", path)), if removable { egui::Sense::click() } else { egui::Sense::hover() });
    if removable {
        if x.hovered() {
            ui.painter().rect_filled(x_rect, t.round(6.0), t.hover);
        }
        crate::icons::cross(ui.painter(), x_rect.shrink(7.0), if x.hovered() { t.text_strong } else { t.text_dim });
    }
    let small = egui::FontId::proportional(t.text_size(12.0));
    let note = first.then(|| ui.painter().layout_no_wrap("Chats start here".to_owned(), small, t.text_faint));
    let note_w = note.as_ref().map_or(0.0, |g| g.size().x + 12.0);
    if let Some(g) = note {
        let gy = rect.center().y - g.size().y / 2.0;
        ui.painter().galley(egui::pos2(x_rect.left() - 8.0 - g.size().x, gy), g, t.text_faint);
    }
    // The folder's own name first (what people recognise), where it is after it, dimmer;
    // whatever doesn't fit goes from the end of the location, never from the name.
    let left = rect.left() + 38.0;
    let room = (x_rect.left() - 8.0 - note_w - left).max(20.0);
    let name = folder_name(path);
    let parent = path.trim_end_matches(['/', '\\']).strip_suffix(name.as_str()).unwrap_or_default().trim_end_matches(['/', '\\']);
    let mut job = egui::text::LayoutJob::default();
    job.append(&name, 0.0, egui::TextFormat::simple(egui::TextStyle::Body.resolve(ui.style()), t.text_strong));
    if !parent.is_empty() {
        job.append(&format!("  {}", short_path(parent, 40)), 0.0, egui::TextFormat::simple(egui::FontId::proportional(t.text_size(12.0)), t.text_faint));
    }
    job.wrap = egui::text::TextWrapping::truncate_at_width(room);
    let g = ui.painter().layout_job(job);
    let gy = rect.center().y - g.size().y / 2.0;
    ui.painter().galley(egui::pos2(left, gy), g, t.text);
    let tip = ui.interact(
        egui::Rect::from_min_max(rect.min, egui::pos2(x_rect.left() - 4.0, rect.max.y)),
        egui::Id::unique(("folder-row", path)),
        egui::Sense::hover(),
    );
    tip.on_hover_text(path);
    removable && x.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Take it off this project (the folder stays)").clicked()
}

/// A small outlined button with an icon (Add folder…).
fn small_button(ui: &mut egui::Ui, t: &Tokens, icon: crate::icons::Paint, label: &str) -> egui::Response {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let g = ui.painter().layout_no_wrap(label.to_owned(), font, t.text);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(g.size().x + 44.0, 34.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if resp.is_pointer_button_down_on() {
            t.pressed
        } else if resp.hovered() {
            t.hover
        } else {
            egui::Color32::TRANSPARENT
        };
        ui.painter().rect(rect, t.round(crate::form::CONTROL_R), fill, egui::Stroke::new(1.0, t.field_border), egui::StrokeKind::Inside);
        icon(ui.painter(), egui::Rect::from_center_size(egui::pos2(rect.left() + 20.0, rect.center().y), egui::vec2(15.0, 15.0)), t.text_dim);
        let gy = rect.center().y - g.size().y / 2.0;
        ui.painter().galley(egui::pos2(rect.left() + 34.0, gy), g, t.text);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// "Advanced", folded: a caret, the word, and what's set there now, faint on the right.
fn advanced_row(ui: &mut egui::Ui, t: &Tokens, open: bool, env: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        if resp.hovered() {
            ui.painter().rect_filled(rect, t.round(6.0), t.hover);
        }
        let color = if resp.hovered() { t.text_strong } else { t.text_dim };
        crate::icons::chevron(ui.painter(), egui::Rect::from_center_size(egui::pos2(rect.left() + 10.0, rect.center().y), egui::vec2(12.0, 12.0)), open, color);
        let g = ui.painter().layout_no_wrap("Advanced".to_owned(), egui::TextStyle::Body.resolve(ui.style()), color);
        let gy = rect.center().y - g.size().y / 2.0;
        ui.painter().galley(egui::pos2(rect.left() + 24.0, gy), g, color);
        let v = ui.painter().layout_no_wrap(format!("Commands run: {env}"), egui::FontId::proportional(t.text_size(12.0)), t.text_faint);
        let vy = rect.center().y - v.size().y / 2.0;
        ui.painter().galley(egui::pos2(rect.right() - 8.0 - v.size().x, vy), v, t.text_faint);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Where its commands run")
}

/// A dialog button: the main action in the accent (faded while it can't be pressed), the
/// other quiet with an outline. Same height and corners for both.
pub(crate) fn dialog_button(ui: &mut egui::Ui, t: &Tokens, label: &str, primary: bool, enabled: bool, h: f32) -> egui::Response {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let (text, fill, stroke) = match (primary, enabled) {
        (true, true) => (t.on_accent, t.accent, egui::Stroke::NONE),
        (true, false) => (t.on_accent.gamma_multiply(0.75), t.accent.gamma_multiply(0.38), egui::Stroke::NONE),
        (false, _) => (t.text, egui::Color32::TRANSPARENT, egui::Stroke::new(1.0, t.field_border)),
    };
    let g = ui.painter().layout_no_wrap(label.to_owned(), font, text);
    let w = (g.size().x + 32.0).max(88.0);
    let sense = if enabled { egui::Sense::click() } else { egui::Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), sense);
    if ui.is_rect_visible(rect) {
        let fill = match (primary, enabled, resp.hovered(), resp.is_pointer_button_down_on()) {
            (true, true, true, _) => t.accent_hover,
            (false, _, _, true) => t.pressed,
            (false, _, true, _) => t.hover,
            _ => fill,
        };
        ui.painter().rect(rect, t.round(crate::form::CONTROL_R), fill, stroke, egui::StrokeKind::Inside);
        ui.painter().galley(rect.center() - g.size() / 2.0, g, text);
    }
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// The engine's folders: up, the subfolders, Use this folder. Returns `Some(Some(path))`
/// to list another folder, `Some(None)` to close.
fn browser(ui: &mut egui::Ui, t: &Tokens, list: &FolderList, width: f32) -> Option<Option<String>> {
    let mut go = None;
    egui::Frame::new().stroke(egui::Stroke::new(1.0, t.border)).corner_radius(t.round(6.0)).inner_margin(6.0).show(ui, |ui| {
        ui.set_width(width);
        ui.horizontal(|ui| {
            if let Some(parent) = &list.parent
                && ui.small_button("↑ Up").clicked()
            {
                go = Some(Some(parent.clone()));
            }
            ui.add(egui::Label::new(RichText::new(&list.path).small().monospace().color(t.text_dim)).truncate());
        });
        if let Some(e) = &list.error {
            ui.label(RichText::new(e).small().color(t.danger));
        }
        egui::ScrollArea::vertical().max_height(160.0).id_salt("folders").show(ui, |ui| {
            if list.dirs.is_empty() && list.error.is_none() {
                ui.label(RichText::new("No subfolders").small().color(t.text_faint));
            }
            for d in &list.dirs {
                if ui.add(egui::Button::new(RichText::new(format!("{d}/")).color(t.text)).frame(false)).clicked() {
                    let sep = if list.path.ends_with('/') { "" } else { "/" };
                    go = Some(Some(format!("{}{sep}{d}", list.path)));
                }
            }
            if list.truncated {
                ui.label(RichText::new("(more not shown)").small().color(t.text_faint));
            }
        });
        if ui.button("Use this folder").clicked() {
            go = Some(None);
        }
    });
    go
}

/// Whether the system has a folder dialog to show (never in the browser).
fn native_dialog() -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    return crate::export::dialog_unavailable_reason().is_none();
    #[cfg(target_arch = "wasm32")]
    false
}

/// The desktop's folder dialog, on a thread (rfd talks to the desktop portal).
#[cfg(not(target_arch = "wasm32"))]
fn pick_folder(ctx: &egui::Context, tx: std::sync::mpsc::Sender<Option<String>>) {
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new().name("byob-folder-picker".into()).spawn(move || {
        let picked = pollster::block_on(rfd::AsyncFileDialog::new().set_title("Project folder").pick_folder());
        let _ = tx.send(picked.map(|f| f.path().display().to_string()));
        ctx.request_repaint();
    });
}

/// One workspace's settings window: name, folder, its settings, its environment's state,
/// and Remove.
/// The parts of a project's settings, one at a time beside its nav.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectTab {
    General,
    Folders,
    Chats,
    Environment,
    Plugins,
}

impl ProjectTab {
    const ALL: [ProjectTab; 5] = [ProjectTab::General, ProjectTab::Folders, ProjectTab::Chats, ProjectTab::Environment, ProjectTab::Plugins];

    /// By its name, any case ("chats").
    pub fn find(name: &str) -> Option<ProjectTab> {
        Self::ALL.into_iter().find(|t| t.label().eq_ignore_ascii_case(name))
    }

    fn label(self) -> &'static str {
        match self {
            ProjectTab::General => "General",
            ProjectTab::Folders => "Folders",
            ProjectTab::Chats => "Chats",
            ProjectTab::Environment => "Environment",
            ProjectTab::Plugins => "Plugins",
        }
    }

    fn icon(self) -> crate::icons::Paint {
        match self {
            ProjectTab::General => crate::icons::gear,
            ProjectTab::Folders => crate::icons::folder,
            ProjectTab::Chats => crate::icons::bubble,
            ProjectTab::Environment => crate::icons::terminal,
            ProjectTab::Plugins => crate::icons::puzzle_piece,
        }
    }

    /// What the part is for, under its title.
    fn blurb(self) -> &'static str {
        match self {
            ProjectTab::General => "Its name, and what every chat in it should know.",
            ProjectTab::Folders => "Its chats can read and edit files in these. They start in the first one.",
            ProjectTab::Chats => "How its chats work. Default follows your settings for all chats.",
            ProjectTab::Environment => "Where its commands run, and what's installed for it.",
            ProjectTab::Plugins => "From its .byob/plugins folder. Switches here apply to its chats.",
        }
    }
}

pub struct WorkspaceWindow {
    pub id: String,
    pub tab: ProjectTab,
    name: String,
    /// The name field was filled from the project (once: after that it's what's typed,
    /// empty too).
    named: bool,
    editor: LayerEditor,
    env: EnvPanel,
    removing: bool,
    clean: bool,
    /// A folder path typed to link (the browser UI, or a machine without a folder dialog).
    link_path: String,
    /// The path field is out (Add folder → Paste a path).
    typing_path: bool,
    #[cfg(not(target_arch = "wasm32"))]
    picked: (std::sync::mpsc::Sender<Option<String>>, std::sync::mpsc::Receiver<Option<String>>),
}

impl WorkspaceWindow {
    pub fn new(id: &str, remove: bool) -> Self {
        let mut editor = LayerEditor::new(ConfigScope::Workspace { id: id.to_owned() });
        editor.quiet = true;
        Self {
            id: id.to_owned(),
            tab: ProjectTab::General,
            name: String::new(),
            named: false,
            editor,
            env: EnvPanel::default(),
            removing: remove,
            clean: false,
            link_path: String::new(),
            typing_path: false,
            #[cfg(not(target_arch = "wasm32"))]
            picked: std::sync::mpsc::channel(),
        }
    }

    /// Its folders: its own (where chats run), then the ones it links, each with a way to
    /// take it off; then a way to add one. Changes go to the engine as the whole list.
    fn folders(&mut self, ui: &mut egui::Ui, t: &Tokens, ws: &WorkspaceSummary, out: &mut FormOutput) {
        let mut next: Option<Vec<String>> = None;
        #[cfg(not(target_arch = "wasm32"))]
        while let Ok(p) = self.picked.1.try_recv() {
            if let Some(p) = p {
                let mut list = ws.folders.clone();
                if p != ws.folder && !list.contains(&p) {
                    list.push(p);
                    next = Some(list);
                }
            }
        }
        // A path being typed: a field on top, Add and Cancel beside it.
        if self.typing_path {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let field_id = egui::Id::unique(("project-link-path", ws.id.as_str()));
                let room = (ui.available_width() - 170.0).max(160.0);
                let field = form_field(ui, t, field_id, |frame| {
                    egui::TextEdit::singleline(&mut self.link_path)
                        .id(field_id)
                        .hint_text("/path/to/a/folder")
                        .char_limit(4096)
                        .desired_width(room)
                        .frame(frame)
                });
                let typed = self.link_path.trim().to_owned();
                let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (crate::form::button(ui, t, "Add", !typed.is_empty()).clicked() || enter) && !typed.is_empty() {
                    let mut list = ws.folders.clone();
                    if typed != ws.folder && !list.contains(&typed) {
                        list.push(typed);
                        next = Some(list);
                    }
                    self.link_path.clear();
                    self.typing_path = false;
                }
                if crate::form::button(ui, t, "Cancel", true).clicked() {
                    self.link_path.clear();
                    self.typing_path = false;
                }
            });
            ui.add_space(12.0);
        }
        // Each folder a row of the card: its name, where it is, and either where chats
        // start or a way to take it off.
        crate::form::card(t).show(ui, |ui| {
            let width = ui.available_width();
            folder_line(ui, t, &ws.folder, width, true, false);
            for (i, f) in ws.folders.iter().enumerate() {
                if folder_line(ui, t, f, width, false, true) {
                    let mut list = ws.folders.clone();
                    list.remove(i);
                    next = Some(list);
                }
            }
        });
        if let Some(folders) = next {
            out.msgs.push(ClientMsg::SetWorkspaceFolders { id: ws.id.clone(), folders });
        }
    }

    /// The nav on the left (one entry per part), the part in view on the right. Returns
    /// messages and whether to close.
    fn show(&mut self, ui: &mut egui::Ui, t: &Tokens, ws: Option<&WorkspaceSummary>, plugins: &mut crate::plugins::PluginsUi, height: f32) -> FormOutput {
        let mut out = FormOutput::default();
        let Some(ws) = ws else {
            ui.label(RichText::new("This project was removed.").color(t.text_dim));
            out.close = crate::form::button(ui, t, "Close", true).clicked();
            return out;
        };
        if !std::mem::replace(&mut self.named, true) {
            self.name.clone_from(&ws.name);
        }
        // A narrow nav: its items fill it, a short gap to the hairline, then the part.
        let nav_w = 156.0;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.allocate_ui_with_layout(egui::vec2(nav_w, height), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_min_size(egui::vec2(nav_w, height));
                ui.spacing_mut().item_spacing.y = 2.0;
                for tab in ProjectTab::ALL {
                    if project_nav_item(ui, t, tab, self.tab == tab, nav_w - 4.0).clicked() {
                        self.tab = tab;
                    }
                }
            });
            // A hairline between the nav and the part in view.
            let x = ui.cursor().left();
            ui.painter().vline(x, ui.cursor().top()..=ui.cursor().top() + height, egui::Stroke::new(1.0, t.separator.gamma_multiply(0.7)));
            ui.add_space(20.0);
            let w = ui.available_width();
            ui.allocate_ui_with_layout(egui::vec2(w, height), egui::Layout::top_down(egui::Align::Min), |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(("project-part", ws.id.as_str(), self.tab.label()))
                    .max_height(height)
                    .min_scrolled_height(height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(w - 10.0);
                        // The part's title and what it's for; its main action (if any) on the right.
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(RichText::new(self.tab.label()).size(t.text_size(16.0)).family(theme::bold_or_regular(ui.ctx())).color(t.text_strong));
                                ui.add_space(2.0);
                                ui.label(RichText::new(self.tab.blurb()).size(t.text_size(13.0)).color(t.text_dim));
                            });
                            if self.tab == ProjectTab::Folders {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| self.add_folder_menu(ui, t));
                            }
                        });
                        ui.add_space(16.0);
                        crate::form::outlined_controls(ui, t);
                        match self.tab {
                            ProjectTab::General => self.general(ui, t, ws, &mut out),
                            ProjectTab::Folders => self.folders(ui, t, ws, &mut out),
                            ProjectTab::Chats => self.chats(ui, t, &mut out),
                            ProjectTab::Environment => {
                                self.env_kind(ui, t, &mut out);
                                ui.add_space(16.0);
                                crate::form::card(t).show(ui, |ui| {
                                    ui.add_space(12.0);
                                    ui.set_width(ui.available_width());
                                    if let Some(m) = self.env.show(ui, t, &ws.id) {
                                        out.msgs.push(m);
                                    }
                                    ui.add_space(12.0);
                                });
                            }
                            ProjectTab::Plugins => {
                                crate::form::card(t).show(ui, |ui| {
                                    ui.add_space(8.0);
                                    ui.set_width(ui.available_width());
                                    plugins.panel(ui, t, &crate::plugins::Filter::Workspace(ws.id.clone()));
                                    ui.add_space(8.0);
                                });
                            }
                        }
                        ui.add_space(8.0);
                    });
            });
        });
        out
    }

    /// "Add folder" with its menu: choose one on this computer, or paste a path.
    fn add_folder_menu(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        let b = small_button(ui, t, crate::icons::folder_plus, "Add folder").on_hover_text("Its files stay where they are");
        crate::menu::below(&b, t, 220.0).show(|ui| {
            crate::menu::tighten(ui);
            #[cfg(not(target_arch = "wasm32"))]
            if crate::export::dialog_unavailable_reason().is_none() && crate::menu::item(ui, t, Some(crate::icons::folder), "Choose a folder…", None).clicked()
            {
                pick_folder(ui.ctx(), self.picked.0.clone());
                ui.close();
            }
            if crate::menu::item(ui, t, Some(crate::icons::pencil), "Paste a path…", None).clicked() {
                self.typing_path = true;
                ui.close();
            }
        });
    }

    /// Name and instructions, then Remove project (asks first) in its own card.
    fn general(&mut self, ui: &mut egui::Ui, t: &Tokens, ws: &WorkspaceSummary, out: &mut FormOutput) {
        use crate::form::{RowStyle, card, setting_row};
        let id = |what: &str| egui::Id::unique(("project-settings", ws.id.as_str(), what.to_owned()));
        let first = RowStyle { first: true, ..RowStyle::default() };
        card(t).show(ui, |ui| {
            setting_row(ui, t, id("name"), "Name", "How it shows in the sidebar.", first, |ui| {
                let changed = self.name.trim() != ws.name && !self.name.trim().is_empty();
                if changed && crate::form::button(ui, t, "Save", true).clicked() {
                    out.msgs.push(ClientMsg::RenameWorkspace { id: ws.id.clone(), name: self.name.trim().to_owned() });
                }
                let field_id = id("name-field");
                let r = form_field(ui, t, field_id, |frame| {
                    egui::TextEdit::singleline(&mut self.name).id(field_id).char_limit(80).desired_width(180.0).frame(frame)
                });
                if changed && r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    out.msgs.push(ClientMsg::RenameWorkspace { id: ws.id.clone(), name: self.name.trim().to_owned() });
                }
            });
            let wide = RowStyle { wide: true, ..RowStyle::default() };
            setting_row(ui, t, id("instructions"), "Instructions", "Added to every chat in this project.", wide, |ui| {
                if let Some(m) = self.editor.control(ui, t, Field::Instructions) {
                    out.msgs.push(m);
                }
            });
        });
        ui.add_space(16.0);
        card(t).show(ui, |ui| {
            let desc = "Its chats leave byob. The folders and their files stay.";
            setting_row(ui, t, id("remove"), "Remove project", desc, first, |ui| {
                if self.removing {
                    if crate::form::danger_button(ui, t, "Remove").clicked() {
                        out.msgs.push(ClientMsg::RemoveWorkspace { id: ws.id.clone(), clean: self.clean });
                        out.close = true;
                    }
                    if crate::form::button(ui, t, "Cancel", true).clicked() {
                        self.removing = false;
                    }
                } else if crate::form::danger_button(ui, t, "Remove…").clicked() {
                    self.removing = true;
                }
            });
            if self.removing {
                ui.horizontal(|ui| {
                    crate::form::switch(ui, t, &mut self.clean);
                    ui.label(RichText::new("Also delete its installed packages and container").size(t.text_size(13.0)).color(t.text_dim));
                });
                ui.add_space(14.0);
            }
        });
    }

    /// Its chats' settings, one row each.
    fn chats(&mut self, ui: &mut egui::Ui, t: &Tokens, out: &mut FormOutput) {
        let fields: Vec<Field> = WORKSPACE_FIELDS.iter().copied().filter(|f| !matches!(f, Field::Instructions | Field::Env | Field::Image)).collect();
        self.rows(ui, t, &fields, out);
    }

    /// Where its commands run (and the container image, for a container).
    fn env_kind(&mut self, ui: &mut egui::Ui, t: &Tokens, out: &mut FormOutput) {
        let before = self.editor.view().map(|v| v.own.env);
        let container = self.editor.view().is_some_and(|v| v.effective.env.value == EnvKind::Container);
        let fields: Vec<Field> = if container { vec![Field::Env, Field::Image] } else { vec![Field::Env] };
        self.rows(ui, t, &fields, out);
        if before.is_some() && self.editor.view().map(|v| v.own.env) != before {
            self.env.refresh();
        }
    }

    /// A card of setting rows, one per field.
    fn rows(&mut self, ui: &mut egui::Ui, t: &Tokens, fields: &[Field], out: &mut FormOutput) {
        use crate::form::{RowStyle, card, setting_row};
        let id = self.id.clone();
        card(t).show(ui, |ui| {
            for (i, &f) in fields.iter().enumerate() {
                let style = RowStyle { first: i == 0, ..RowStyle::default() };
                setting_row(ui, t, egui::Id::unique(("project-row", id.as_str(), plain_label(f))), plain_label(f), field_help(f), style, |ui| {
                    if let Some(m) = self.editor.control(ui, t, f) {
                        out.msgs.push(m);
                    }
                });
            }
        });
    }
}

/// One folder as a row of a card: its icon, its name, where it is (dimmer); on the right,
/// a "Chats start here" chip for the first, a remove button (on hover) for the others; a
/// soft line above each but the first. Returns whether it was taken off.
fn folder_line(ui: &mut egui::Ui, t: &Tokens, path: &str, width: f32, first: bool, removable: bool) -> bool {
    let h = 52.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, h), egui::Sense::hover());
    let p = ui.painter();
    if !first {
        p.hline(rect.x_range(), rect.top(), egui::Stroke::new(1.0, t.separator.gamma_multiply(0.7)));
    }
    crate::icons::folder(p, egui::Rect::from_center_size(egui::pos2(rect.left() + 9.0, rect.center().y), egui::vec2(16.0, 16.0)), t.text_dim);
    let x_rect = egui::Rect::from_center_size(egui::pos2(rect.right() - 14.0, rect.center().y), egui::vec2(28.0, 28.0));
    let small = egui::FontId::proportional(t.text_size(12.0));
    let note = first.then(|| p.layout_no_wrap("Chats start here".to_owned(), small.clone(), t.text_dim));
    let right_room = note.as_ref().map_or(40.0, |g| g.size().x + 16.0);
    if let Some(g) = note {
        let chip =
            egui::Rect::from_min_size(egui::pos2(rect.right() - g.size().x - 16.0, rect.center().y - g.size().y / 2.0 - 3.0), g.size() + egui::vec2(16.0, 6.0));
        p.rect_filled(chip, chip.height() / 2.0, crate::form::soft(t));
        p.galley(chip.min + egui::vec2(8.0, 3.0), g, t.text_dim);
    }
    let left = rect.left() + 30.0;
    let room = (rect.right() - right_room - 12.0 - left).max(40.0);
    let name = folder_name(path);
    let parent = path.trim_end_matches(['/', '\\']).strip_suffix(name.as_str()).unwrap_or_default().trim_end_matches(['/', '\\']);
    let mut job = egui::text::LayoutJob::default();
    job.append(&name, 0.0, egui::TextFormat::simple(egui::TextStyle::Body.resolve(ui.style()), t.text_strong));
    if !parent.is_empty() {
        job.append(&format!("  {}", short_path(parent, 48)), 0.0, egui::TextFormat::simple(small, t.text_faint));
    }
    job.wrap = egui::text::TextWrapping::truncate_at_width(room);
    let g = p.layout_job(job);
    p.galley(egui::pos2(left, rect.center().y - g.size().y / 2.0), g, t.text);
    resp.on_hover_text(path);
    if !removable {
        return false;
    }
    let row_over = ui.rect_contains_pointer(rect);
    let x = ui.interact(x_rect, egui::Id::unique(("folder-line-x", path)), egui::Sense::click());
    if row_over || x.hovered() {
        if x.hovered() {
            ui.painter().rect_filled(x_rect, t.round(crate::form::CONTROL_R), crate::form::soft_hover(t));
        }
        crate::icons::cross(ui.painter(), x_rect.shrink(8.0), if x.hovered() { t.text_strong } else { t.text_dim });
    }
    x.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Take it off this project (the folder stays)").clicked()
}

/// One entry of the project settings' nav: icon and name; the one in view on a soft grey.
fn project_nav_item(ui: &mut egui::Ui, t: &Tokens, tab: ProjectTab, on: bool, width: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 34.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if on {
            t.selection
        } else if resp.hovered() {
            crate::form::row_hover(t)
        } else {
            egui::Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, t.round(crate::form::CONTROL_R), fill);
        let color = if on || resp.hovered() { t.text_strong } else { t.text_dim };
        (tab.icon())(ui.painter(), egui::Rect::from_center_size(egui::pos2(rect.left() + 16.0, rect.center().y), egui::vec2(15.0, 15.0)), color);
        let g = ui.painter().layout_no_wrap(tab.label().to_owned(), egui::TextStyle::Body.resolve(ui.style()), color);
        ui.painter().galley(egui::pos2(rect.left() + 32.0, rect.center().y - g.size().y / 2.0), g, color);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// What a project's chat setting does, under its name.
fn field_help(f: Field) -> &'static str {
    match f {
        Field::Env => "Where its commands run: this computer, a container or a dev shell.",
        Field::Image => "The container its commands run in.",
        Field::Endpoint => "The model its chats use.",
        Field::Approvals => "What it may do without asking you first.",
        Field::ToolMode => "How the model calls tools.",
        Field::Compaction => "Summarise older messages when a chat gets long.",
        Field::Subagents => "Let a chat hand side tasks to helpers.",
        Field::SubagentModel => "The model the helpers use.",
        Field::ToolCalls => "How many steps one reply can take.",
        Field::ModelImages => "How pictures are sent to the model.",
        Field::Instructions => "Added to every chat in this project.",
    }
}

/// Everything workspace-related the app shows besides the chat list.
pub struct WorkspaceUi {
    pub root: String,
    pub support: EnvSupport,
    pub form: Option<NewWorkspaceForm>,
    pub window: Option<WorkspaceWindow>,
    /// The chat settings popover, when open.
    pub chat: Option<LayerEditor>,
    /// Settings → Defaults for every chat.
    pub defaults: LayerEditor,
    /// The engine's model profiles, for the editors' model pickers.
    profiles: Option<ProfileList>,
    /// Counts openings, so a window opened again starts at its usual place (a window keeps
    /// where it was dragged only while it stays open).
    opens: u64,
}

impl Default for WorkspaceUi {
    fn default() -> Self {
        Self {
            root: String::new(),
            support: EnvSupport::default(),
            form: None,
            window: None,
            chat: None,
            defaults: LayerEditor::new(ConfigScope::Global),
            profiles: None,
            opens: 0,
        }
    }
}

impl WorkspaceUi {
    /// The engine's profiles changed: every editor's model picker shows them.
    pub fn set_profiles(&mut self, list: &ProfileList) {
        self.profiles = Some(list.clone());
        self.defaults.profiles = Some(list.clone());
        if let Some(w) = &mut self.window {
            w.editor.profiles = Some(list.clone());
        }
        if let Some(c) = &mut self.chat {
            c.profiles = Some(list.clone());
        }
    }

    /// The profiles, if the engine has them.
    pub fn profiles(&self) -> Option<&ProfileList> {
        self.profiles.as_ref()
    }

    /// Apply a message meant for these views; false if it isn't one.
    pub fn apply(&mut self, msg: &ServerMsg) -> bool {
        match msg {
            ServerMsg::Workspaces { root, support, .. } => {
                self.root.clone_from(root);
                self.support = support.clone();
                // The list also changes after a workspace's settings did: let the open views
                // show their sources again. (Not a reply to anything, so it isn't consumed.)
                return false;
            }
            ServerMsg::Config(view) => {
                let mut used = self.defaults.received(view);
                if let Some(w) = &mut self.window {
                    used |= w.editor.received(view);
                }
                if let Some(c) = &mut self.chat {
                    used |= c.received(view);
                }
                let _ = used;
            }
            ServerMsg::WorkspaceEnv(env) => {
                if let Some(w) = &mut self.window {
                    w.env.received(env.clone());
                }
            }
            ServerMsg::WorkspaceCreated { id, .. } => {
                if let Some(f) = &mut self.form {
                    f.apply(msg);
                }
                if id.is_some() {
                    self.form = None;
                }
            }
            ServerMsg::Folders(_) | ServerMsg::ContainersChecked { .. } => {
                if let Some(f) = &mut self.form {
                    f.apply(msg);
                }
            }
            _ => return false,
        }
        true
    }

    /// The chat in view changed: an open chat popover shows the new one.
    pub fn chat_changed(&mut self) {
        if let Some(c) = &mut self.chat {
            c.refresh();
        }
    }

    pub fn open_new(&mut self, env: EnvKind) {
        self.form = Some(NewWorkspaceForm::with_env(env));
        self.opens += 1;
    }

    /// The New project dialog, started one way (from the Projects page).
    pub fn open_start(&mut self, start: crate::projects::Start) {
        self.form = Some(NewWorkspaceForm::with_start(start));
        self.opens += 1;
    }

    /// The New workspace form on `path` (a folder dropped on the window).
    pub fn open_with_folder(&mut self, path: &str) {
        self.form = Some(NewWorkspaceForm::with_folder(path));
        self.opens += 1;
    }

    pub fn open_settings(&mut self, id: &str, remove: bool) {
        let mut w = WorkspaceWindow::new(id, remove);
        w.editor.profiles.clone_from(&self.profiles);
        self.window = Some(w);
        self.opens += 1;
    }

    /// The chat settings window's id while it is open (tests find its rect by it).
    #[cfg(test)]
    pub(crate) fn chat_window_id(&self) -> egui::Id {
        egui::Id::unique(("chat-settings", self.opens))
    }

    /// The project settings dialog's id while it is open (tests find its rect by it).
    #[cfg(test)]
    pub(crate) fn settings_window_id(&self) -> egui::Id {
        egui::Id::unique(("workspace-settings", self.opens))
    }

    pub fn toggle_chat(&mut self) {
        self.chat = match self.chat.take() {
            Some(_) => None,
            None => {
                let mut e = LayerEditor::new(ConfigScope::Chat);
                e.profiles.clone_from(&self.profiles);
                Some(e)
            }
        };
        self.opens += 1;
    }

    /// Draw the windows that are open; returns messages for the engine.
    pub fn windows(
        &mut self,
        ctx: &egui::Context,
        t: &Tokens,
        workspaces: &[WorkspaceSummary],
        chat_title: &str,
        anchor: egui::Pos2,
        plugins: &mut crate::plugins::PluginsUi,
    ) -> Vec<ClientMsg> {
        let mut msgs = Vec::new();
        let screen = ctx.content_rect();
        let max_w = (screen.width() - 24.0).clamp(240.0, 520.0);
        let max_h = (screen.height() - 80.0).max(200.0);
        let opens = self.opens;
        if let Some(form) = &mut self.form {
            let mut open = true;
            let (root, support) = (self.root.clone(), self.support.clone());
            let mut close = false;
            // Centred over the window, the rest dimmed: one thing to decide. A click outside
            // closes it only while nothing is chosen or typed (no work lost to a stray click).
            let w = max_w.min(480.0);
            // 10-point corners, 24 points of room inside: crisp, with the fields' 6-point
            // corners sitting within them.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // a few points
            let corner = t.round(crate::form::CARD_R + 2.0).round().clamp(0.0, 255.0) as u8;
            let dialog = crate::menu::frame(t).inner_margin(egui::Margin::same(24)).corner_radius(egui::CornerRadius::same(corner));
            let modal = egui::Modal::new(egui::Id::unique(("new-workspace", opens))).frame(dialog).show(ctx, |ui| {
                ui.set_width(w);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Create project").strong().size(t.text_size(20.0)).color(t.text_strong));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let size = crate::icons::button_height(ui);
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
                        let x = crate::sidebar::window_button(ui, t, rect, egui::Id::unique(("new-workspace-close", opens)), crate::icons::cross, true);
                        if x.on_hover_text("Close (Esc)").clicked() {
                            open = false;
                        }
                    });
                });
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    open = false;
                }
                ui.add_space(20.0);
                // As tall as what it shows; the fields scroll when that outgrows the window,
                // the buttons stay in view under them.
                let button_room = 70.0;
                egui::ScrollArea::vertical().max_height((max_h - 60.0 - button_room).max(120.0)).auto_shrink([false, true]).show(ui, |ui| {
                    let o = form.body(ui, t, &root, &support);
                    msgs.extend(o.msgs);
                    close = o.close;
                });
                let o = form.actions(ui, t, &support);
                msgs.extend(o.msgs);
                close |= o.close;
            });
            if modal.backdrop_response.clicked() && form.untouched() {
                open = false;
            }
            if !open || close {
                self.form = None;
            }
        }
        if let Some(w) = &mut self.window {
            let mut open = true;
            let mut close = false;
            let ws = workspaces.iter().find(|x| x.id == w.id);
            // Centred like New project, the rest dimmed: its name and what it holds on top,
            // then its settings in cards, scrolling under the title.
            // Wider than the other dialogs: its rows keep their controls beside their names.
            let w_dialog = (screen.width() - 48.0).clamp(240.0, 860.0);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // a few points
            let corner = t.round(crate::form::CARD_R + 2.0).round().clamp(0.0, 255.0) as u8;
            // 12 points on the left (the nav's items reach close to the edge), 24 elsewhere; the
            // title and subtitle are indented back to 24.
            let dialog =
                crate::menu::frame(t).inner_margin(egui::Margin { left: 12, right: 24, top: 24, bottom: 24 }).corner_radius(egui::CornerRadius::same(corner));
            let modal = egui::Modal::new(egui::Id::unique(("workspace-settings", opens))).frame(dialog).show(ctx, |ui| {
                ui.set_width(w_dialog);
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    let name = ws.map_or("Project", |x| x.name.as_str());
                    // Close on the right first, so a long name is cut short before it rather
                    // than pushing the dialog out of the window.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let size = crate::icons::button_height(ui);
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
                        let x = crate::sidebar::window_button(ui, t, rect, egui::Id::unique(("workspace-settings-close", opens)), crate::icons::cross, true);
                        if x.on_hover_text("Close (Esc)").clicked() {
                            open = false;
                        }
                        ui.add_space(8.0);
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add(egui::Label::new(RichText::new(name).strong().size(t.text_size(20.0)).color(t.text_strong)).truncate());
                        });
                    });
                });
                if let Some(x) = ws {
                    ui.add_space(2.0);
                    let chats = crate::sidebar::chats_label(x.chats);
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        ui.label(RichText::new(format!("Project settings · {chats}")).size(t.text_size(13.0)).color(t.text_dim));
                    });
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) && !ui.memory(|m| m.focused().is_some()) {
                    open = false;
                }
                ui.add_space(8.0);
                // One height for every part, so the dialog doesn't jump between them.
                let room = (max_h - 120.0).clamp(240.0, 600.0);
                ui.add_space(8.0);
                let o = w.show(ui, t, ws, plugins, room);
                msgs.extend(o.msgs);
                close = o.close;
            });
            if modal.backdrop_response.clicked() && !w.removing {
                open = false;
            }
            if !open || close {
                self.window = None;
            }
        }
        if let Some(editor) = &mut self.chat {
            // A sheet under the settings button (right edges lined up): what most people
            // change first, where it works next, and the rest folded under Advanced.
            let w = max_w.min(400.0);
            let x = (anchor.x - w).clamp(8.0, (screen.right() - w - 8.0).max(8.0));
            let mut close = ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !ctx.memory(|m| m.focused().is_some());
            let id = egui::Id::unique(("chat-settings", opens));
            let advanced_id = egui::Id::unique("chat-settings-advanced");
            let mut advanced = ctx.data(|d| d.get_temp::<bool>(advanced_id)).unwrap_or(false);
            egui::Area::new(id).order(egui::Order::Foreground).fixed_pos(egui::pos2(x, anchor.y)).constrain(true).show(ctx, |ui| {
                crate::menu::frame(t).inner_margin(egui::Margin::symmetric(16, 14)).show(ui, |ui| {
                    // The frame adds its margins (16 each side) and its 1-point border.
                    ui.set_width(w - 34.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Chat settings").strong().color(t.text_strong));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if crate::icons::icon_button(ui, t, crate::icons::cross, None, false).on_hover_text("Close (Esc)").clicked() {
                                close = true;
                            }
                        });
                    });
                    let ws = editor.view().and_then(|v| v.workspace.as_ref()).map(|w| w.name.clone());
                    let line = match ws {
                        Some(ws) => format!("{chat_title} · in {ws} · changes apply to this chat only"),
                        None => format!("{chat_title} · changes apply to this chat only"),
                    };
                    ui.add(egui::Label::new(RichText::new(line).small().color(t.text_dim)).truncate());
                    ui.add_space(8.0);
                    egui::ScrollArea::vertical()
                        .max_height(max_h - 90.0)
                        // Without this a floating sheet scrolls after a few rows; shorter content still shrinks it.
                        .min_scrolled_height((max_h - 90.0).min(560.0))
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            // One label column for every group, as wide as the longest label.
                            let font = egui::TextStyle::Body.resolve(ui.style());
                            editor.label_w =
                                ["How it answers", "Commands run on", "Shorten long chats", "Container image", "Steps per reply", "Images it sees"]
                                    .iter()
                                    .map(|l| ui.painter().layout_no_wrap((*l).to_owned(), font.clone(), t.text).size().x)
                                    .fold(0.0, f32::max);
                            let section = |ui: &mut egui::Ui, title: &str| {
                                ui.add_space(6.0);
                                ui.label(RichText::new(title).small().color(t.text_faint));
                                ui.add_space(2.0);
                            };
                            section(ui, "Basics");
                            if let Some(m) = editor.show(ui, t, &[Field::Endpoint, Field::Approvals]) {
                                msgs.push(m);
                            }
                            if let Some(m) = editor.answer_style(ui, t) {
                                msgs.push(m);
                            }
                            if let Some(m) = editor.show(ui, t, &[Field::Instructions]) {
                                msgs.push(m);
                            }
                            section(ui, "Where it works");
                            if let Some(m) = editor.show(ui, t, &[Field::Env, Field::Image]) {
                                msgs.push(m);
                            }
                            ui.add_space(8.0);
                            // Advanced: folded until asked for.
                            let label = if advanced { "Hide advanced" } else { "Advanced: tools, memory, helpers, plugins" };
                            let r = ui
                                .add(egui::Label::new(RichText::new(label).color(t.text_dim)).sense(egui::Sense::click()))
                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                            if r.clicked() {
                                advanced = !advanced;
                            }
                            if advanced {
                                ui.add_space(4.0);
                                let fields = [Field::ToolMode, Field::Compaction, Field::Subagents, Field::SubagentModel, Field::ToolCalls, Field::ModelImages];
                                if let Some(m) = editor.show(ui, t, &fields) {
                                    msgs.push(m);
                                }
                                ui.add_space(6.0);
                                plugins.chat_switches(ui, t);
                            }
                        });
                });
            });
            ctx.data_mut(|d| d.insert_temp(advanced_id, advanced));
            if close {
                self.chat = None;
            }
        }
        msgs
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use byob_types::config::{Forced, Keys, Levels, resolve};

    use super::*;

    #[test]
    fn answer_styles_sit_first_in_the_instructions_and_come_off_again() {
        assert_eq!(join_style(0, "  be kind "), Some("be kind".into()));
        assert_eq!(join_style(0, ""), None);
        let short = join_style(1, "").unwrap();
        assert_eq!(split_style(&short), (1, ""));
        let both = join_style(3, "Use metric units.").unwrap();
        assert_eq!(split_style(&both), (3, "Use metric units."));
        // Text that only mentions a style isn't one.
        assert_eq!(split_style("Answer style: short please").0, 0);
    }

    #[test]
    fn home_folders_read_as_tilde() {
        assert_eq!(short_home("/Users/ana/byob-workspaces"), "~/byob-workspaces");
        // Long paths keep their end.
        assert_eq!(short_path("/srv/a/b/c/projects/alpha", 20), "…/b/c/projects/alpha", "as many folders as fit in 20");
        assert_eq!(short_path("/srv/x", 20), "/srv/x");
        assert!(short_path(&format!("/srv/{}", "y".repeat(80)), 20).chars().count() <= 20, "one huge folder is cut too");
        assert_eq!(short_path("/srv/ünïcödé/földer", 12), "…/földer");
        assert_eq!(short_home("/home/ana/work/x"), "~/work/x");
        assert_eq!(short_home(r"C:\Users\ana\byob-workspaces"), "~/byob-workspaces");
        assert_eq!(short_home("/Users/ana"), "~");
        assert_eq!(short_home("/srv/data"), "/srv/data");
        assert_eq!(short_home("relative/path"), "relative/path");
    }

    pub(crate) fn view(scope: ConfigScope, own: Layer, ws: Option<&Layer>) -> ConfigView {
        let global = Layer { base_url: Some("http://g/v1".into()), model: Some("gm".into()), ..Layer::default() };
        let chat = (scope == ConfigScope::Chat).then_some(&own);
        let workspace = if matches!(scope, ConfigScope::Workspace { .. }) { Some(&own) } else { ws };
        let effective = resolve(Levels { global: Some(&global), workspace, chat }, Keys::default(), Forced::default());
        ConfigView { scope, own, effective, has_key: false, title: "t".into(), workspace: None }
    }

    fn render(f: impl FnMut(&mut egui::Ui)) {
        let ctx = egui::Context::default();
        let mut f = f;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| f(ui));
        out.textures_delta.clear();
    }

    #[test]
    fn editors_ask_once_then_show_their_scope_only() {
        let mut e = LayerEditor::new(ConfigScope::Chat);
        let mut first = None;
        render(|ui| first = e.show(ui, &Tokens::dark(), CHAT_FIELDS));
        assert_eq!(first, Some(ClientMsg::GetConfig { scope: ConfigScope::Chat }));
        let ws = Layer { env: Some(EnvKind::Container), approvals: Some(Approvals::AllowEdits), ..Layer::default() };
        assert!(!e.received(&view(ConfigScope::Global, Layer::default(), None)), "another scope's answer is ignored");
        let own = Layer { model: Some("small".into()), ..Layer::default() };
        assert!(e.received(&view(ConfigScope::Chat, own, Some(&ws))));
        assert!(e.endpoint_open && e.model == "small" && e.url == "http://g/v1");
        let v = e.view().unwrap();
        assert_eq!((v.effective.env.from, v.effective.approvals.from), (Level::Workspace, Level::Workspace));
        let mut sent = Some(ClientMsg::Cancel);
        render(|ui| sent = e.show(ui, &Tokens::light(), CHAT_FIELDS));
        assert_eq!(sent, None, "drawing sends nothing by itself");
    }

    #[test]
    fn instructions_fit_the_form_and_inherited_ones_fold_to_a_headline() {
        assert_eq!(headline("\n# House rules\n\n- one", 60), "House rules");
        assert_eq!(headline("- a list item first", 6), "a list…");
        assert_eq!(headline("", 10), "");
        let global = "# House rules\n\nYou are careful.\n\n- Short paragraphs.\n- Metric units.";
        let own = Layer { instructions: Some(format!("# Project rules\n\n{}", "A long line that has to wrap. ".repeat(20))), ..Layer::default() };
        let id = ConfigScope::Workspace { id: "w".into() };
        let mut v = view(id.clone(), own, None);
        v.effective.instructions.insert(0, (Level::Global, global.to_owned()));
        for width in [520.0, 720.0] {
            let mut e = LayerEditor::new(id.clone());
            e.asked = true;
            assert!(e.received(&v));
            let ctx = egui::Context::default();
            let mut used = egui::Rect::NOTHING;
            for _ in 0..2 {
                let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                    let r = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(width, 2000.0));
                    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r));
                    assert_eq!(e.show(&mut child, &Tokens::dark(), WORKSPACE_FIELDS), None);
                    used = child.min_rect();
                });
                out.textures_delta.clear();
            }
            let field = ctx.read_response(instructions_id(Level::Workspace)).map(|r| r.rect).unwrap();
            // Under the rows, the form's whole width (clear of a scroll bar), no more: it used to
            // sit in the rows' control column, at most 420 pt wide whatever the room.
            assert!(field.left() < 1.0 && field.right() > width - 20.0 && field.right() <= width, "{width}: {field:?}");
            assert!(used.width() <= width + 0.5, "{width}: {used:?}");
        }
    }

    #[test]
    fn the_new_project_form_checks_before_creating() {
        let support = EnvSupport { nix: false, installer: None, runtime: Some("docker".into()) };
        let mut f = NewWorkspaceForm::default();
        // A name is all it needs: no folder means byob keeps its files.
        assert!(f.problem(&support).is_some_and(|p| p.contains("name")));
        assert_eq!(f.source(), Source::Empty);
        f.name = "Data".into();
        f.name_edited = true;
        assert!(f.problem(&support).is_none());
        f.env = EnvKind::Container;
        f.use_image = true;
        f.image = "bad image!".into();
        assert!(f.problem(&support).is_some_and(|p| p.contains("image")));
        f.image = "debian:stable-slim".into();
        assert_eq!(f.layer(), Layer { env: Some(EnvKind::Container), image: Some("debian:stable-slim".into()), ..Layer::default() });
        assert!(f.problem(&EnvSupport::default()).is_some_and(|p| p.contains("Docker")));
        // The goal is its instructions (trimmed; blank is none).
        f.goal = "  Ship the app by June.  ".into();
        assert_eq!(f.layer().instructions.as_deref(), Some("Ship the app by June."));
        f.goal = "   ".into();
        assert_eq!(f.layer().instructions, None);
        // Answers from the engine land in the form.
        f.apply(&ServerMsg::Folders(FolderList { path: "/srv".into(), parent: Some("/".into()), dirs: vec!["data".into()], truncated: false, error: None }));
        assert!(f.browser.is_some() && f.folders.is_empty(), "listing a folder doesn't add it");
        f.apply(&ServerMsg::ContainersChecked { ok: false, message: "Docker isn't running".into() });
        assert_eq!(f.check, Some((false, "Docker isn't running".to_owned())));
        assert_eq!(slug("My Data!"), "my-data");
        let mut sent = Vec::new();
        render(|ui| sent = f.show(ui, &Tokens::dark(), "/home/me/byob-workspaces", &support).msgs);
        assert!(sent.is_empty());
    }

    #[test]
    fn a_new_project_takes_a_name_a_goal_and_any_folders() {
        assert_eq!(folder_name("/home/me/Projects/Alpha/"), "Alpha");
        assert_eq!(folder_name(r"C:\Users\me\Beta"), "Beta");
        assert_eq!(folder_name("  "), "");
        let support = EnvSupport::default();
        // No folder: byob makes one; the goal travels as its instructions.
        let mut f = NewWorkspaceForm { name: "Notes".into(), goal: "Plan the trip".into(), ..NewWorkspaceForm::default() };
        assert_eq!(
            f.create_msg(),
            ClientMsg::CreateWorkspace {
                name: "Notes".into(),
                folder: NewFolder::Create,
                config: Layer { instructions: Some("Plan the trip".into()), ..Layer::default() },
                folders: Vec::new()
            }
        );
        // A folder picked (or dropped): named after it; chats run there.
        let mut f = NewWorkspaceForm::with_folder("/home/me/Projects/Alpha");
        assert_eq!((f.source(), f.name.as_str(), f.env, f.advanced), (Source::Folder, "Alpha", EnvKind::Auto, false));
        assert!(f.problem(&support).is_none());
        // More folders: linked; a repeat or a blank is ignored; the name stays the first's.
        f.add_folder("/srv/api");
        f.add_folder("/srv/api");
        f.add_folder("  ");
        assert_eq!(f.folders, ["/home/me/Projects/Alpha", "/srv/api"]);
        assert_eq!(f.name, "Alpha");
        assert_eq!(
            f.create_msg(),
            ClientMsg::CreateWorkspace {
                name: "Alpha".into(),
                folder: NewFolder::Existing { path: "/home/me/Projects/Alpha".into() },
                config: Layer::default(),
                folders: vec!["/srv/api".into()]
            }
        );
        // A typed name stays when the first folder changes.
        let mut f = NewWorkspaceForm { name: "Mine".into(), name_edited: true, ..NewWorkspaceForm::default() };
        f.add_folder("/srv/gamma");
        assert_eq!(f.name, "Mine");
        // No more than the engine takes.
        let mut f = NewWorkspaceForm::default();
        for i in 0..40 {
            f.add_folder(&format!("/srv/f{i}"));
        }
        assert_eq!(f.folders.len(), byob_protocol::workspace::MAX_LINKED_FOLDERS + 1);
        // Untouched until something is typed or added.
        assert!(NewWorkspaceForm::default().untouched());
        assert!(!NewWorkspaceForm { goal: "x".into(), ..NewWorkspaceForm::default() }.untouched());
        // Container (scripted) opens Advanced.
        assert!(NewWorkspaceForm::with_env(EnvKind::Container).advanced);
        // Every state draws, narrow and wide, without sending anything.
        for folders in [vec![], vec!["/srv/a".to_owned()], vec!["/srv/a".to_owned(), "/srv/b".to_owned()]] {
            let mut f = NewWorkspaceForm { folders: folders.clone(), advanced: true, goal: "A goal\nover lines".into(), ..NewWorkspaceForm::default() };
            let mut sent = vec![ClientMsg::Cancel];
            render(|ui| sent = f.show(ui, &Tokens::light(), "", &support).msgs);
            assert!(sent.is_empty(), "{folders:?}: {sent:?}");
        }
    }

    #[test]
    fn created_workspaces_close_the_form_and_errors_stay() {
        let mut w = WorkspaceUi::default();
        w.open_new(EnvKind::Container);
        assert!(w.apply(&ServerMsg::WorkspaceCreated { id: None, error: Some("already a project".into()) }));
        assert_eq!(w.form.as_ref().and_then(|f| f.error.as_deref()), Some("already a project"));
        w.apply(&ServerMsg::WorkspaceCreated { id: Some("w1".into()), error: None });
        assert!(w.form.is_none());
        w.toggle_chat();
        assert!(w.chat.is_some());
        w.toggle_chat();
        assert!(w.chat.is_none());
    }

    /// The project's name can be emptied to type another: it used to come back the moment
    /// the field was empty, so deleting it all and typing put the new text after the old.
    #[test]
    fn a_projects_name_field_can_be_emptied() {
        let ws = WorkspaceSummary { id: "w".into(), name: "Old".into(), folder: "/w".into(), ..Default::default() };
        let mut w = WorkspaceWindow::new("w", false);
        let mut plugins = crate::plugins::PluginsUi::default();
        render(|ui| drop(w.show(ui, &Tokens::dark(), Some(&ws), &mut plugins, 400.0)));
        assert_eq!(w.name, "Old");
        w.name.clear();
        render(|ui| drop(w.show(ui, &Tokens::dark(), Some(&ws), &mut plugins, 400.0)));
        assert_eq!(w.name, "", "an emptied field stays empty");
    }

    /// "From a folder" where there's no folder dialog (the browser) lists the engine's
    /// folders straight away; it used to open the form as if New project was chosen.
    #[test]
    fn from_a_folder_without_a_dialog_lists_the_folders() {
        let mut f = NewWorkspaceForm::with_start(crate::projects::Start::Link);
        assert!(f.pick_now);
        let mut out = FormOutput::default();
        render(|ui| f.begin_add(ui.ctx(), false, &mut out));
        assert_eq!(out.msgs, [ClientMsg::ListFolders { path: None }]);
        assert!(f.browsing);
    }
}
