//! Settings → Models: the saved models (profiles), what each server said about itself,
//! adding several models of one server at once, editing one, and which is the default.
//!
//! The engine owns the profiles ([`ServerMsg::Profiles`]); this sends the profile messages
//! and shows the answers, so it works the same in a browser tab. Keys are write-only: the
//! engine only says whether a profile has one.

use std::collections::{BTreeMap, BTreeSet};

use byob_protocol::profiles::{Effort, Params, Profile, ProfileList, ProfileView, ROLE_MAIN, ROLE_SUBAGENTS, ROLE_UTILITY, ROLE_VISION};
use byob_protocol::workspace::ToolMode;
use byob_protocol::{ClientMsg, EngineState, InferenceTest, Link, PRESETS, Preset, ServerMsg};
use egui::RichText;

use crate::connect::short_url;
use crate::theme::{self, Tokens};

/// A test the list asked for.
#[derive(Debug, Clone, PartialEq)]
enum Tested {
    Running,
    Done(InferenceTest),
}

/// Settings → Models between frames.
#[derive(Default)]
pub struct ModelsUi {
    /// The engine's profiles (`None`: an engine without them, or not heard yet).
    pub list: Option<ProfileList>,
    tests: BTreeMap<String, Tested>,
    editor: Option<Editor>,
    add: Option<AddForm>,
    /// Asking before deleting this one.
    deleting: Option<String>,
    /// Scripted runs: open this profile's edit form once the list has it.
    edit_when_listed: Option<String>,
    out: Vec<ClientMsg>,
    /// Scripted runs (`--settings-section model:<id>`): open this profile's form, Advanced
    /// expanded, once the list has it.
    pub reveal: Option<String>,
}

/// Adding models: a server (found, preset, or typed), its model list, the ones to add.
#[derive(Default)]
struct AddForm {
    url: String,
    key: String,
    /// Models the server listed.
    models: Vec<String>,
    picked: BTreeSet<String>,
    /// A model id typed in (servers that list nothing).
    manual: String,
    /// Listing the models of this URL.
    listing: Option<String>,
    /// What the last listing said, if it went wrong (or found nothing).
    problem: Option<String>,
}

/// Editing one profile (or a new one).
struct Editor {
    profile: Profile,
    has_key: bool,
    key: String,
    remove_key: bool,
    models: Vec<String>,
    listing: Option<String>,
    temperature: String,
    top_p: String,
    max_tokens: String,
    max_parallel: String,
    context: String,
    compact_percent: String,
    extra: String,
    error: Option<String>,
    /// Expand Advanced on the next frame.
    open_advanced: bool,
    /// Frames left to keep the form's end in view (scripted runs).
    scroll_frames: u8,
}

fn opt_text<T: ToString>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_default()
}

/// A number field: empty is none, otherwise it must parse.
fn parse<T: std::str::FromStr>(s: &str, what: &str) -> Result<Option<T>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    s.parse().map(Some).map_err(|_| format!("{what}: {s:?} isn't a number"))
}

impl Editor {
    fn new(v: &ProfileView) -> Self {
        let p = &v.profile;
        Self {
            profile: p.clone(),
            has_key: v.has_key,
            key: String::new(),
            remove_key: false,
            models: Vec::new(),
            listing: None,
            temperature: opt_text(p.params.temperature),
            top_p: opt_text(p.params.top_p),
            max_tokens: opt_text(p.params.max_tokens),
            max_parallel: opt_text(p.max_parallel),
            context: opt_text(p.context),
            compact_percent: opt_text(p.compact_percent),
            extra: if p.params.extra.is_empty() { String::new() } else { serde_json::to_string_pretty(&p.params.extra).unwrap_or_default() },
            error: None,
            open_advanced: false,
            scroll_frames: 0,
        }
    }

    /// The profile as edited, or what's wrong with it.
    fn built(&self) -> Result<Profile, String> {
        let mut p = self.profile.clone();
        p.params = Params {
            temperature: parse(&self.temperature, "temperature")?,
            top_p: parse(&self.top_p, "top_p")?,
            max_tokens: parse(&self.max_tokens, "max tokens")?,
            effort: p.params.effort,
            extra: extra_json(&self.extra)?,
            rest: p.params.rest.clone(),
        };
        p.max_parallel = parse(&self.max_parallel, "subagents at once")?;
        p.context = parse(&self.context, "context window")?;
        p.compact_percent = parse(&self.compact_percent, "compact at")?;
        p.validated()
    }
}

/// The efforts a profile's model takes, what unset is called, and the field's tooltip; `None`
/// when it takes none (OpenAI-compatible servers). Codex has no "max" (byob would send xhigh).
pub(crate) fn effort_levels(p: &Profile) -> Option<(&'static [Effort], &'static str, &'static str)> {
    const CODEX: [Effort; 4] = [Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh];
    match byob_types::cli_agent(&p.base_url) {
        Some("claude") => Some((&Effort::ALL, "Claude Code's default", "How hard Claude thinks (claude --effort): lower is faster and uses less of your plan")),
        Some("codex") => {
            Some((&CODEX, "Codex's default", "How hard the model reasons (Codex model_reasoning_effort): lower is faster and uses less of your plan"))
        }
        Some(_) => None,
        None if p.anthropic() => Some((&Effort::ALL, "API default", "How hard Claude thinks (output_config.effort): lower is faster and cheaper")),
        None => None,
    }
}

/// The "Extra request JSON" field as an object (empty: none), or what's wrong with it.
fn extra_json(text: &str) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    if text.trim().is_empty() {
        return Ok(serde_json::Map::new());
    }
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::Object(m)) => Ok(m),
        Ok(_) => Err("the extra request JSON must be an object, like {\"top_k\": 20}".into()),
        Err(e) => Err(format!("extra request JSON: {e}")),
    }
}

/// The color of a profile's status dot, and what it means.
fn status(t: &Tokens, v: &ProfileView, test: Option<&Tested>) -> (egui::Color32, String) {
    match test {
        Some(Tested::Running) => return (t.warning, "Testing…".into()),
        Some(Tested::Done(r)) if r.ok => return (t.success, if r.detail.is_empty() { "It answers".into() } else { format!("It answers: {}", r.detail) }),
        Some(Tested::Done(r)) => {
            let title = r.problem.map_or("That didn't work", byob_protocol::Problem::title);
            return (t.danger, if r.detail.is_empty() { title.to_owned() } else { format!("{title}: {}", r.detail) });
        }
        None => {}
    }
    match &v.link {
        Link::Ok => (t.success, "Answers".into()),
        Link::Checking => (t.warning, "Checking…".into()),
        Link::Failed { problem, detail } => (t.danger, format!("{}: {detail}", problem.map_or("Not reachable", byob_protocol::Problem::title))),
        Link::Unknown if v.info.as_ref().is_some_and(|i| i.probing) => (t.warning, "Checking…".into()),
        Link::Unknown if known(v) => (t.success, "The server answered its probe".into()),
        Link::Unknown => (t.text_faint, "Not checked yet".into()),
    }
}

/// The probe found something about it.
fn known(v: &ProfileView) -> bool {
    v.info.as_ref().is_some_and(|i| !i.server.is_empty() || i.context_tokens.is_some() || i.vision.is_some())
}

fn dot(ui: &mut egui::Ui, color: egui::Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
    resp
}

fn badge(ui: &mut egui::Ui, t: &Tokens, text: &str, strong: bool) {
    let (fill, color) = if strong { (t.accent, t.on_accent) } else { (t.panel_bg, t.text_dim) };
    // Painted to its own size (a frame would stretch to the row's height).
    let galley = ui.painter().layout_no_wrap(text.to_owned(), egui::TextStyle::Small.resolve(ui.style()), color);
    let (rect, _) = ui.allocate_exact_size(galley.size() + egui::vec2(10.0, 4.0), egui::Sense::hover());
    let stroke = egui::Stroke::new(1.0, if strong { t.accent } else { t.border });
    ui.painter().rect(rect, t.round(4.0), fill, stroke, egui::StrokeKind::Inside);
    ui.painter().galley(rect.min + egui::vec2(5.0, 2.0), galley, color);
}

/// "Default", "Vision", … for the roles byob shows.
fn role_label(role: &str) -> String {
    match role {
        ROLE_MAIN => "Default".into(),
        ROLE_VISION => "Vision".into(),
        ROLE_SUBAGENTS => "Subagents".into(),
        ROLE_UTILITY => "Titles".into(),
        other => other.to_owned(),
    }
}

impl ModelsUi {
    /// A message for this section; true if it was only for it.
    pub fn apply(&mut self, msg: &ServerMsg) -> bool {
        match msg {
            ServerMsg::Profiles(l) => {
                self.list = Some((**l).clone());
                if let Some(v) = self.edit_when_listed.as_ref().and_then(|id| l.get(id)) {
                    self.editor = Some(Editor::new(v));
                    self.edit_when_listed = None;
                }
                // A profile that went away can't be edited any more.
                if let Some(e) = &self.editor
                    && !e.profile.id.is_empty()
                    && l.get(&e.profile.id).is_none()
                {
                    self.editor = None;
                }
                false
            }
            ServerMsg::ProfileTested(t) => {
                if let Some(e) = self.editor.as_mut().filter(|e| e.profile.id == t.id) {
                    e.models.clone_from(&t.test.models);
                }
                self.tests.insert(t.id.clone(), Tested::Done(t.test.clone()));
                true
            }
            _ => false,
        }
    }

    /// A connection test answer: the model list of a server being added or edited.
    pub fn tested(&mut self, t: &InferenceTest) {
        if let Some(a) = &mut self.add
            && a.listing.as_deref() == Some(t.base_url.as_str())
        {
            a.listing = None;
            a.models.clone_from(&t.models);
            a.problem = if t.models.is_empty() {
                Some(if t.problem.is_some() || !t.ok {
                    let title = t.problem.map_or("That didn't work", byob_protocol::Problem::title);
                    format!("{title}. {}", t.detail)
                } else {
                    "It answers but lists no models; type the model id below.".into()
                })
            } else {
                None
            };
            if a.models.len() == 1 {
                a.picked = a.models.iter().cloned().collect();
            }
        }
        if let Some(e) = &mut self.editor
            && e.listing.as_deref() == Some(t.base_url.as_str())
        {
            e.listing = None;
            e.models.clone_from(&t.models);
        }
    }

    pub fn take(&mut self) -> Vec<ClientMsg> {
        std::mem::take(&mut self.out)
    }

    /// The engine has profiles (else Settings shows the older connection form).
    pub fn available(&self) -> bool {
        self.list.is_some()
    }

    /// Scripted runs: open the edit form of profile `id` (now, or when the list arrives).
    pub fn start_edit(&mut self, id: &str) {
        match self.list.as_ref().and_then(|l| l.get(id)) {
            Some(v) => self.editor = Some(Editor::new(v)),
            None => self.edit_when_listed = Some(id.to_owned()),
        }
    }

    /// Scripted runs: open the Add form on `url`, listing its models.
    pub fn start_add(&mut self, url: &str) {
        let mut a = AddForm { url: url.to_owned(), ..AddForm::default() };
        a.listing = Some(url.to_owned());
        self.out.push(ClientMsg::TestInference { base_url: url.to_owned(), model: String::new(), api_key: None });
        self.add = Some(a);
    }

    /// The section's body.
    pub fn show(&mut self, ui: &mut egui::Ui, t: &Tokens, state: Option<&EngineState>) {
        let Some(list) = self.list.clone() else {
            theme::spinner(ui, t, None);
            return;
        };
        if let Some(v) = self.reveal.as_deref().and_then(|id| list.get(id)) {
            self.editor = Some(Editor { open_advanced: true, scroll_frames: 30, ..Editor::new(v) });
            self.reveal = None;
        }
        if list.items.is_empty() {
            ui.label(RichText::new("No models saved yet. Add the ones your servers run; the first becomes the default.").color(t.text_dim));
        }
        let main = list.roles.get(ROLE_MAIN).cloned();
        for (i, v) in list.items.iter().enumerate() {
            self.row(ui, t, &list, v, main.as_deref() == Some(v.profile.id.as_str()), i == 0);
            if self.editor.as_ref().is_some_and(|e| e.profile.id == v.profile.id) {
                self.edit_form(ui, t);
                ui.add_space(t.pad(crate::form::ROW_GAP));
            }
        }
        ui.add_space(t.pad(4.0));
        if self.add.is_some() {
            self.add_form(ui, t, state);
        } else if ui.button("+ Add models…").on_hover_text("From a server on this computer, a provider, or any URL").clicked() {
            self.add = Some(AddForm::default());
        }
    }

    /// The dropdown for one model `role` (Settings → Models → Roles); `none` names no model.
    pub fn role_control(&mut self, ui: &mut egui::Ui, role: &str, none: &str) {
        let Some(list) = self.list.as_ref() else { return };
        let current = list.roles.get(role).cloned();
        let name = |id: &Option<String>| id.as_ref().and_then(|i| list.get(i)).map_or_else(|| none.to_owned(), |v| v.profile.label().to_owned());
        let mut picked = current.clone();
        crate::form::combo((role, "role")).selected_text(name(&current)).width(200.0).show_ui(ui, |ui| {
            ui.selectable_value(&mut picked, None, none);
            for v in &list.items {
                ui.selectable_value(&mut picked, Some(v.profile.id.clone()), v.profile.label());
            }
        });
        if picked != current {
            self.out.push(ClientMsg::SetRole { role: role.into(), profile: picked });
        }
    }

    fn row(&mut self, ui: &mut egui::Ui, t: &Tokens, list: &ProfileList, v: &ProfileView, is_main: bool, first: bool) {
        let p = &v.profile;
        let test = self.tests.get(&p.id).cloned();
        let test = test.as_ref();
        let (color, what) = status(t, v, test);
        // A plain list, rows split by hairlines like the settings around it (no boxes).
        if !first {
            let y = ui.cursor().top();
            ui.painter().hline(ui.max_rect().x_range(), y, egui::Stroke::new(1.0, t.separator.gamma_multiply(0.7)));
        }
        egui::Frame::new().inner_margin(egui::vec2(0.0, t.pad(crate::form::ROW_PAD))).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                dot(ui, color).on_hover_text(&what);
                ui.label(RichText::new(p.label()).strong().color(t.text_strong));
                for role in list.roles_of(&p.id) {
                    badge(ui, t, &role_label(role), role == ROLE_MAIN);
                }
                if v.has_key {
                    ui.label(RichText::new("key").small().color(t.text_faint)).on_hover_text("An API key is saved for it (on the machine byob runs on)");
                }
            });
            ui.label(RichText::new(format!("{} · {}", p.model, short_url(&p.base_url))).color(t.text_dim));
            if !p.description.is_empty() {
                ui.label(RichText::new(&p.description).small().color(t.text));
            }
            let found = match &v.info {
                Some(i) if i.probing => "checking what it can do…".to_owned(),
                Some(i) if known(v) => i.summary(),
                _ => "not checked yet".to_owned(),
            };
            let mut line = found;
            if let Some(m) = p.tool_mode {
                line.push_str(&format!(" · tool calling: {}", m.label().to_lowercase()));
            }
            ui.label(RichText::new(line).small().color(t.text_faint)).on_hover_text(&what);
            if let Some(Tested::Done(r)) = test
                && !r.ok
            {
                ui.label(RichText::new(&what).small().color(t.danger));
            }
            ui.horizontal_wrapped(|ui| {
                if ui.small_button("Edit").clicked() {
                    self.editor = if self.editor.as_ref().is_some_and(|e| e.profile.id == p.id) { None } else { Some(Editor::new(v)) };
                }
                if ui.small_button("Duplicate").on_hover_text("A copy (key included) to change, e.g. with other settings").clicked() {
                    self.out.push(ClientMsg::DuplicateProfile { id: p.id.clone() });
                }
                let running = matches!(test, Some(Tested::Running));
                if ui.add_enabled(!running, egui::Button::new("Test").small()).on_hover_text("Ask the server whether it answers, and what it can do").clicked()
                {
                    self.tests.insert(p.id.clone(), Tested::Running);
                    self.out.push(ClientMsg::TestProfile { id: p.id.clone() });
                }
                if !is_main && ui.small_button("Make default").on_hover_text("The model every chat uses unless its project or itself picks another").clicked() {
                    self.out.push(ClientMsg::SetRole { role: ROLE_MAIN.into(), profile: Some(p.id.clone()) });
                }
                if self.deleting.as_deref() == Some(p.id.as_str()) {
                    ui.label(RichText::new("Delete it? Chats and projects that use it go back to the default.").small().color(t.danger));
                    if ui.small_button(RichText::new("Delete").color(t.danger)).clicked() {
                        self.out.push(ClientMsg::DeleteProfile { id: p.id.clone() });
                        self.deleting = None;
                    }
                    if ui.small_button("Cancel").clicked() {
                        self.deleting = None;
                    }
                } else if !is_main && ui.small_button("Delete…").clicked() {
                    self.deleting = Some(p.id.clone());
                }
            });
        });
    }

    fn edit_form(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        let Some(e) = &mut self.editor else { return };
        let mut save = false;
        let mut cancel = false;
        let mut list_models = None;
        egui::Frame::new().stroke(egui::Stroke::new(1.0, t.border)).corner_radius(t.round(6.0)).inner_margin(t.pad(10.0)).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            let w = (ui.available_width() - crate::form::label_w(t, ui.available_width()) - t.pad(crate::form::COL_GAP)).clamp(160.0, 420.0);
            crate::form::grid(("profile-edit", &e.profile.id), t, ui.available_width()).show(ui, |ui| {
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(&mut e.profile.name).desired_width(w));
                ui.end_row();
                ui.label("Server URL");
                ui.add(egui::TextEdit::singleline(&mut e.profile.base_url).hint_text("http://127.0.0.1:8080/v1").desired_width(w));
                ui.end_row();
                ui.label("Model");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut e.profile.model).hint_text("as the server names it").desired_width((w - 90.0).max(80.0)));
                    if e.models.is_empty() {
                        let listing = e.listing.is_some();
                        if ui
                            .add_enabled(!listing, egui::Button::new(if listing { "Listing…" } else { "List…" }))
                            .on_hover_text("Ask the server which models it has")
                            .clicked()
                        {
                            list_models = Some(e.profile.base_url.trim().to_owned());
                        }
                    } else {
                        let picked = ui
                            .menu_button("Choose…", |ui| {
                                let mut picked = None;
                                egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                                    for m in &e.models {
                                        if ui.selectable_label(*m == e.profile.model, m).clicked() {
                                            picked = Some(m.clone());
                                            ui.close();
                                        }
                                    }
                                });
                                picked
                            })
                            .inner
                            .flatten();
                        if let Some(m) = picked {
                            e.profile.model = m;
                        }
                    }
                });
                ui.end_row();
                ui.label("API key");
                ui.horizontal(|ui| {
                    let hint =
                        if e.has_key && !e.remove_key { "saved — leave blank to keep (same server only)" } else { "optional — hosted APIs need one" };
                    ui.add_enabled(!e.remove_key, egui::TextEdit::singleline(&mut e.key).password(true).hint_text(hint).desired_width((w - 90.0).max(80.0)));
                    if e.has_key {
                        ui.checkbox(&mut e.remove_key, "Remove");
                    }
                });
                ui.end_row();
                ui.label("Description");
                ui.vertical(|ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut e.profile.description)
                            .hint_text("What it's good at, e.g. fast, good at code")
                            .desired_rows(2)
                            .desired_width(w),
                    );
                    ui.label(RichText::new("The main model reads this when it picks a model for a subagent.").small().color(t.text_faint));
                });
                ui.end_row();
                ui.label("Subagents");
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut e.profile.subagents, "Offered to subagents")
                        .on_hover_text("The main model may run a delegated task on this model (it chooses by the description)");
                    ui.label(RichText::new("at most").color(t.text_dim));
                    ui.add(egui::TextEdit::singleline(&mut e.max_parallel).hint_text("any").desired_width(40.0))
                        .on_hover_text("Subagents running on this model at once (besides the overall limit and the server's free slots)");
                    ui.label(RichText::new("at once").color(t.text_dim));
                });
                ui.end_row();
                ui.label("Tool calling");
                let shown = |m: Option<ToolMode>| m.map_or_else(|| "As each chat says".to_owned(), |m| m.label().to_owned());
                crate::form::combo(("profile-tools", &e.profile.id))
                    .selected_text(shown(e.profile.tool_mode))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut e.profile.tool_mode, None, shown(None));
                        for m in ToolMode::ALL {
                            ui.selectable_value(&mut e.profile.tool_mode, Some(m), m.label());
                        }
                    })
                    .response
                    .on_hover_text("A property of the model: when set, it beats the chat's and project's tool calling setting");
                ui.end_row();
            });
            let open = std::mem::take(&mut e.open_advanced).then_some(true);
            egui::CollapsingHeader::new(RichText::new("Advanced").color(t.text)).id_salt(("profile-advanced", &e.profile.id)).open(open).show(ui, |ui| {
                let anthropic = e.profile.anthropic();
                ui.label(RichText::new("Sent with every request to this model; leave a field empty for the server's default.").small().color(t.text_faint));
                crate::form::grid(("profile-advanced-grid", &e.profile.id), t, ui.available_width()).show(ui, |ui| {
                    let field = |ui: &mut egui::Ui, label: &str, v: &mut String, hint: &str, tip: &str| {
                        ui.label(label);
                        ui.add(egui::TextEdit::singleline(v).hint_text(hint).desired_width(120.0)).on_hover_text(tip);
                        ui.end_row();
                    };
                    field(
                        ui,
                        "Context window",
                        &mut e.context,
                        "as probed",
                        "Tokens to budget chats against (and compact before), instead of what the server reported",
                    );
                    field(
                        ui,
                        "Compact at (%)",
                        &mut e.compact_percent,
                        "as each chat says",
                        "Summarize older messages when a request passes this much of the context window (10 to 95), instead of the global setting. A workspace's or chat's own setting still wins",
                    );
                    let sampling = if anthropic { "Current Claude models reject it: leave it empty" } else { "Sampling temperature" };
                    field(ui, "Temperature", &mut e.temperature, "server default", sampling);
                    field(ui, "top_p", &mut e.top_p, "server default", if anthropic { sampling } else { "Nucleus sampling" });
                    let (hint, tip) =
                        if anthropic { ("64000", "Most tokens of one reply, thinking included") } else { ("server default", "Most tokens of one reply") };
                    field(ui, "Max tokens", &mut e.max_tokens, hint, tip);
                    if let Some((levels, default, tip)) = effort_levels(&e.profile) {
                        ui.label("Effort");
                        let shown = |v: Option<Effort>| v.map_or(default, Effort::as_str);
                        crate::form::combo(("profile-effort", &e.profile.id))
                            .selected_text(shown(e.profile.params.effort))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut e.profile.params.effort, None, shown(None));
                                for &v in levels {
                                    ui.selectable_value(&mut e.profile.params.effort, Some(v), v.as_str());
                                }
                            })
                            .response
                            .on_hover_text(tip);
                        ui.end_row();
                    }
                    ui.label("Extra request JSON");
                    ui.vertical(|ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut e.extra)
                                .code_editor()
                                .hint_text("{\"chat_template_kwargs\": {\"enable_thinking\": false}}")
                                .desired_rows(2)
                                .desired_width(w),
                        )
                        .on_hover_text("A JSON object merged into every request body, last. It can't set model, messages, tools, stream or system.");
                        if let Err(err) = extra_json(&e.extra) {
                            ui.label(RichText::new(err).small().color(t.danger));
                        }
                    });
                    ui.end_row();
                });
                if anthropic && (!e.temperature.trim().is_empty() || !e.top_p.trim().is_empty()) {
                    ui.label(RichText::new("Current Claude models reject temperature and top_p: requests with them fail.").small().color(t.warning));
                }
            });
            if e.scroll_frames > 0 {
                e.scroll_frames -= 1;
                ui.scroll_to_cursor(Some(egui::Align::Max));
            }
            if let Some(err) = &e.error {
                ui.label(RichText::new(err).color(t.danger));
            }
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(RichText::new("Save").color(t.on_accent)).fill(t.accent)).clicked() {
                    save = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
        if let Some(url) = list_models.filter(|u| !u.is_empty()) {
            e.listing = Some(url.clone());
            let api_key = Some(e.key.trim().to_owned()).filter(|k| !k.is_empty());
            self.out.push(ClientMsg::TestInference { base_url: url, model: String::new(), api_key });
        }
        if save {
            match e.built() {
                Ok(profile) => {
                    let api_key = if e.remove_key { Some(String::new()) } else { Some(e.key.trim().to_owned()).filter(|k| !k.is_empty()) };
                    self.out.push(ClientMsg::UpdateProfile { profile, api_key });
                    self.editor = None;
                }
                Err(err) => e.error = Some(err),
            }
        } else if cancel {
            self.editor = None;
        }
    }

    fn add_form(&mut self, ui: &mut egui::Ui, t: &Tokens, state: Option<&EngineState>) {
        let Some(a) = &mut self.add else { return };
        let mut close = false;
        let mut list_url: Option<String> = None;
        egui::Frame::new().fill(t.panel_bg).stroke(egui::Stroke::new(1.0, t.border)).corner_radius(t.round(8.0)).inner_margin(t.pad(12.0)).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new("Add models").strong().color(t.text_strong));
            if let Some(s) = state.filter(|s| !s.detected.is_empty()) {
                ui.label(RichText::new("Found on this computer").small().color(t.text_dim));
                ui.horizontal_wrapped(|ui| {
                    for d in &s.detected {
                        let name = Preset::by_id(&d.preset).map_or(d.preset.as_str(), |p| p.name);
                        let label = format!("● {name} · {} model{}", d.models.len(), if d.models.len() == 1 { "" } else { "s" });
                        if ui.selectable_label(a.url == d.base_url, RichText::new(label).color(t.text)).on_hover_text(&d.base_url).clicked() {
                            a.url.clone_from(&d.base_url);
                            a.models.clone_from(&d.models);
                            a.picked.clear();
                            a.problem = None;
                        }
                    }
                });
            }
            ui.label(RichText::new("Servers and providers").small().color(t.text_dim));
            ui.horizontal_wrapped(|ui| {
                let current = Preset::matching(&a.url);
                for p in PRESETS {
                    let mut text = p.name.to_owned();
                    if p.needs_key {
                        text.push_str("  key");
                    }
                    if ui.selectable_label(current == Some(p), text).on_hover_text(format!("{}\n{}", p.hint, p.base_url)).clicked() {
                        a.url = p.base_url.into();
                        a.models.clear();
                        a.picked.clear();
                        a.problem = None;
                        if p.local || !a.key.trim().is_empty() {
                            list_url = Some(a.url.clone());
                        }
                    }
                }
            });
            ui.add_space(4.0);
            let w = (ui.available_width() - crate::form::label_w(t, ui.available_width()) - t.pad(crate::form::COL_GAP)).clamp(160.0, 420.0);
            crate::form::grid("add-models", t, ui.available_width()).show(ui, |ui| {
                ui.label("Server URL");
                if ui.add(egui::TextEdit::singleline(&mut a.url).hint_text("http://127.0.0.1:8080/v1").desired_width(w)).changed() {
                    a.models.clear();
                    a.picked.clear();
                }
                ui.end_row();
                ui.label("API key");
                ui.add(egui::TextEdit::singleline(&mut a.key).password(true).hint_text("optional — hosted APIs need one").desired_width(w));
                ui.end_row();
            });
            ui.horizontal(|ui| {
                let listing = a.listing.is_some();
                if ui
                    .add_enabled(!listing && !a.url.trim().is_empty(), egui::Button::new("List models"))
                    .on_hover_text("Ask the server which models it has")
                    .clicked()
                {
                    list_url = Some(a.url.trim().to_owned());
                }
                if listing {
                    theme::spinner(ui, t, Some(12.0));
                }
            });
            if let Some(p) = &a.problem {
                ui.label(RichText::new(p).small().color(t.warning));
            }
            if !a.models.is_empty() {
                ui.label(
                    RichText::new(format!(
                        "{} lists {} model{}; tick the ones to add",
                        short_url(&a.url),
                        a.models.len(),
                        if a.models.len() == 1 { "" } else { "s" }
                    ))
                    .small()
                    .color(t.text_dim),
                );
                egui::ScrollArea::vertical().id_salt("add-model-list").max_height(220.0).show(ui, |ui| {
                    for m in &a.models {
                        let mut on = a.picked.contains(m);
                        if ui.checkbox(&mut on, m).changed() {
                            if on {
                                a.picked.insert(m.clone());
                            } else {
                                a.picked.remove(m);
                            }
                        }
                    }
                });
            }
            ui.horizontal(|ui| {
                ui.label(RichText::new("Model id").small().color(t.text_dim));
                ui.add(egui::TextEdit::singleline(&mut a.manual).hint_text("another one, as the server names it").desired_width((w - 40.0).max(120.0)));
            });
            let mut chosen: Vec<String> = a.picked.iter().cloned().collect();
            let manual = a.manual.trim().to_owned();
            if !manual.is_empty() && !chosen.contains(&manual) {
                chosen.push(manual);
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let n = chosen.len();
                let label = match n {
                    0 => "Add".to_owned(),
                    1 => "Add 1 model".to_owned(),
                    n => format!("Add {n} models"),
                };
                let ok = n > 0 && !a.url.trim().is_empty();
                if ui.add_enabled(ok, egui::Button::new(RichText::new(label).color(t.on_accent)).fill(t.accent)).clicked() {
                    let key = Some(a.key.trim().to_owned()).filter(|k| !k.is_empty());
                    for m in chosen {
                        let profile = Profile { name: m.clone(), base_url: a.url.trim().to_owned(), model: m, ..Profile::default() };
                        self.out.push(ClientMsg::CreateProfile { profile, api_key: key.clone(), assign: None, make_main: false });
                    }
                    close = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if let Some(url) = list_url.filter(|u| !u.is_empty()) {
            a.listing = Some(url.clone());
            a.problem = None;
            let api_key = Some(a.key.trim().to_owned()).filter(|k| !k.is_empty());
            self.out.push(ClientMsg::TestInference { base_url: url, model: String::new(), api_key });
        }
        if close {
            self.add = None;
        }
    }
}

/// A profile's name for menus: "Qwen 3 · 127.0.0.1:8080".
pub fn menu_label(p: &Profile) -> String {
    format!("{} · {}", p.label(), short_url(&p.base_url))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(id: &str, model: &str) -> ProfileView {
        ProfileView {
            profile: Profile { id: id.into(), name: id.into(), base_url: "http://h:1/v1".into(), model: model.into(), ..Profile::default() },
            has_key: false,
            link: Link::Unknown,
            info: None,
        }
    }

    fn render(ui_state: &mut ModelsUi) {
        let ctx = egui::Context::default();
        let t = Tokens::dark();
        for _ in 0..2 {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 900.0))), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| ui_state.show(ui, &t, None));
            out.textures_delta.clear();
        }
    }

    #[test]
    fn the_list_renders_and_edits_build_checked_profiles() {
        let mut m = ModelsUi::default();
        let list = ProfileList { items: vec![view("a", "qwen"), view("b", "llama")], roles: [("main".to_owned(), "a".to_owned())].into() };
        assert!(!m.apply(&ServerMsg::Profiles(Box::new(list.clone()))));
        assert!(m.available());
        m.editor = Some(Editor::new(&list.items[1]));
        m.add = Some(AddForm { url: "http://h:1/v1".into(), models: vec!["x".into(), "y".into()], ..AddForm::default() });
        render(&mut m);
        let mut e = Editor::new(&list.items[1]);
        e.temperature = "0.6".into();
        e.extra = r#"{"top_k": 20}"#.into();
        let p = e.built().unwrap();
        assert_eq!((p.params.temperature, p.params.extra.get("top_k")), (Some(0.6), Some(&serde_json::json!(20))));
        e.extra = "[1]".into();
        assert!(e.built().is_err());
        e.extra.clear();
        e.max_tokens = "lots".into();
        assert!(e.built().is_err());
        // Extra JSON can't set byob's own fields; effort is kept.
        e.max_tokens.clear();
        e.extra = r#"{"stream": false}"#.into();
        assert!(e.built().unwrap_err().contains("stream"));
        e.extra.clear();
        e.profile.params.effort = Some(Effort::Low);
        assert_eq!(e.built().unwrap().params.effort, Some(Effort::Low));
        assert!(extra_json("{oops").unwrap_err().starts_with("extra request JSON"));
        // An Anthropic profile's form (with Effort) renders too.
        let mut claude = view("c", "claude-opus-5-5");
        claude.profile.base_url = "https://api.anthropic.com/v1".into();
        assert!(claude.profile.anthropic());
        m.editor = Some(Editor::new(&claude));
        render(&mut m);
        // Agent CLI profiles take an effort too; Codex has no "max".
        let mut cli = view("cc", "opus");
        cli.profile.base_url = "cli://claude".into();
        assert_eq!(effort_levels(&cli.profile).map(|l| l.0.len()), Some(5));
        m.editor = Some(Editor::new(&cli));
        render(&mut m);
        cli.profile.base_url = "cli://codex".into();
        assert!(effort_levels(&cli.profile).is_some_and(|l| !l.0.contains(&Effort::Max)));
        assert!(effort_levels(&claude.profile).is_some());
        assert!(effort_levels(&view("q", "qwen").profile).is_none());
        // A listing answer fills the add form; a test answer the row.
        m.add = Some(AddForm { url: "http://s/v1".into(), listing: Some("http://s/v1".into()), ..AddForm::default() });
        m.tested(&InferenceTest { base_url: "http://s/v1".into(), models: vec!["one".into()], ok: true, ..InferenceTest::default() });
        let a = m.add.as_ref().unwrap();
        assert_eq!((a.models.len(), a.picked.len(), a.listing.is_none()), (1, 1, true));
        assert!(
            m.apply(&ServerMsg::ProfileTested(byob_protocol::ProfileTest { id: "a".into(), test: InferenceTest { ok: true, ..InferenceTest::default() } }))
        );
        assert!(matches!(m.tests.get("a"), Some(Tested::Done(t)) if t.ok));
        // The edited profile is deleted elsewhere: the form closes.
        m.editor = Some(Editor::new(&list.items[1]));
        m.apply(&ServerMsg::Profiles(Box::new(ProfileList { items: vec![view("a", "qwen")], roles: list.roles.clone() })));
        assert!(m.editor.is_none());
    }
}
