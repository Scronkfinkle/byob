//! Workspaces and layered settings in the UI: the New workspace form (with a folder
//! browser that lists the engine's folders, so it works from a browser too), a workspace's
//! settings window, a chat's settings popover, and the global defaults in Settings.
//!
//! Every settings editor shows, per field, what applies and where it comes from
//! ("from workspace"), and a Reset that makes the level inherit again. The engine resolves
//! and validates; this only edits one level's [`Layer`] and sends it.

use byob_protocol::workspace::{Approvals, EnvKind, Field, Layer, Level, ToolMode};
use byob_protocol::{ClientMsg, ConfigScope, ConfigView, EnvSupport, FolderList, NewFolder, ServerMsg, WorkspaceSummary};
use egui::RichText;

use crate::envpanel::EnvPanel;
use crate::theme::{self, Tokens};

/// Settings a chat can change (and the order they're shown in).
pub const CHAT_FIELDS: &[Field] = &[Field::Endpoint, Field::Env, Field::Approvals, Field::ToolMode, Field::Compaction, Field::Subagents, Field::Instructions];
/// Settings of a workspace.
pub const WORKSPACE_FIELDS: &[Field] =
    &[Field::Env, Field::Image, Field::Endpoint, Field::Approvals, Field::ToolMode, Field::Compaction, Field::Subagents, Field::Instructions];
/// Defaults in Settings (the endpoint has its own form there).
pub const GLOBAL_FIELDS: &[Field] = &[Field::Approvals, Field::ToolMode, Field::Compaction, Field::Subagents, Field::Instructions];

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
        }
    }

    /// The engine's answer; ignored unless it is for this scope.
    pub fn received(&mut self, v: &ConfigView) -> bool {
        if v.scope != self.scope {
            return false;
        }
        self.draft = v.own.clone();
        self.endpoint_open = v.own.base_url.is_some() || v.own.model.is_some();
        self.url = v.own.base_url.clone().unwrap_or_else(|| v.effective.base_url.value.clone());
        self.model = v.own.model.clone().unwrap_or_else(|| v.effective.model.value.clone());
        self.key.clear();
        self.instructions = v.own.instructions.clone().unwrap_or_default();
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
        let narrow = ui.available_width() < 420.0;
        let id = egui::Id::unique(("layer", format!("{:?}", self.scope)));
        egui::Grid::new(id).num_columns(2).spacing([10.0, 8.0]).min_col_width(if narrow { 70.0 } else { 100.0 }).show(ui, |ui| {
            for &f in fields {
                if f == Field::Image && view.effective.env.value != EnvKind::Container {
                    continue;
                }
                ui.label(RichText::new(f.label()).color(t.text));
                ui.vertical(|ui| {
                    if let Some(m) = self.field(ui, t, &view, level, f) {
                        out = Some(m);
                    }
                });
                ui.end_row();
            }
        });
        out
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
            Field::Instructions => level,
            Field::Env => e.env.from,
            Field::Image => Level::Workspace,
        };
        if view.own.has(f) {
            if from == Level::Flag {
                ui.label(RichText::new("overridden on the command line").small().color(t.warning));
            }
            let reset = ui.small_button("Reset").on_hover_text(format!("Use the inherited value again (stop setting it per {})", level.label()));
            if reset.clicked() {
                self.draft.reset(f);
                self.endpoint_open = self.draft.base_url.is_some();
                return Some(self.send(if f == Field::Endpoint { Some(String::new()) } else { None }));
            }
        } else if f != Field::Instructions && f != Field::Image && !(level == Level::Global && from == Level::Default) {
            ui.label(RichText::new(inherited(from)).small().color(t.text_faint));
        }
        None
    }

    fn field(&mut self, ui: &mut egui::Ui, t: &Tokens, view: &ConfigView, level: Level, f: Field) -> Option<ClientMsg> {
        let e = &view.effective;
        // The global level inherits only the built-in defaults.
        let inh = if level == Level::Global { "Default" } else { "Inherit" };
        let mut out = None;
        ui.horizontal_wrapped(|ui| {
            match f {
                Field::Endpoint => out = self.endpoint(ui, t, view),
                Field::ToolMode => {
                    let mut v = self.draft.tool_mode;
                    let shown = |m: Option<ToolMode>| m.map_or_else(|| format!("{inh} ({})", e.tool_mode.value.label()), |m| m.label().to_owned());
                    egui::ComboBox::from_id_salt(("tool_mode", level)).selected_text(shown(v)).show_ui(ui, |ui| {
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
                    let shown = |a: Option<Approvals>| a.map_or_else(|| format!("{inh} ({})", e.approvals.value.label()), |a| a.label().to_owned());
                    egui::ComboBox::from_id_salt(("approvals", level)).selected_text(shown(v)).show_ui(ui, |ui| {
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
                    let mut auto = self.draft.auto_compact;
                    let shown = |a: Option<bool>| match a {
                        None => format!("{inh} ({})", if e.auto_compact.value { "on" } else { "off" }),
                        Some(true) => "On".into(),
                        Some(false) => "Off".into(),
                    };
                    egui::ComboBox::from_id_salt(("compact", level)).width(110.0).selected_text(shown(auto)).show_ui(ui, |ui| {
                        ui.selectable_value(&mut auto, None, shown(None));
                        ui.selectable_value(&mut auto, Some(true), "On");
                        ui.selectable_value(&mut auto, Some(false), "Off");
                    });
                    if auto != self.draft.auto_compact {
                        self.draft.auto_compact = auto;
                        out = Some(self.send(None));
                    }
                    if e.auto_compact.value || auto == Some(true) {
                        let mut p = self.draft.compact_percent.unwrap_or(e.compact_percent.value);
                        let r = ui
                            .scope(|ui| {
                                ui.spacing_mut().slider_width = 80.0;
                                ui.add(egui::Slider::new(&mut p, 10..=95).suffix("%"))
                            })
                            .inner
                            .on_hover_text("Summarise older messages when a request passes this much of the context window");
                        if (r.drag_stopped() || (r.changed() && !r.dragged())) && Some(p) != self.draft.compact_percent {
                            self.draft.compact_percent = Some(p);
                            out = Some(self.send(None));
                        } else if r.dragged() {
                            self.draft.compact_percent = Some(p);
                        }
                    }
                }
                Field::Subagents => {
                    let mut v = self.draft.subagents;
                    let shown = |s: Option<bool>| match s {
                        None => format!("{inh} ({})", if e.subagents.value { "on" } else { "off" }),
                        Some(true) => "On".into(),
                        Some(false) => "Off".into(),
                    };
                    egui::ComboBox::from_id_salt(("subagents", level))
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
                Field::Instructions => {
                    ui.vertical(|ui| {
                        for (from, text) in e.instructions.iter().filter(|(l, _)| *l != level) {
                            let first: String = text.chars().take(120).collect();
                            let more = if text.chars().count() > 120 { "…" } else { "" };
                            ui.label(RichText::new(format!("{} adds: {first}{more}", from.label())).small().color(t.text_faint)).on_hover_text(text);
                        }
                        let hint = match level {
                            Level::Global => "Added to the system prompt of every chat",
                            Level::Workspace => "Added to the system prompt of this workspace's chats",
                            _ => "Added to the system prompt of this chat",
                        };
                        let r = ui.add(
                            egui::TextEdit::multiline(&mut self.instructions).desired_rows(2).desired_width(ui.available_width().min(420.0)).hint_text(hint),
                        );
                        let text = Some(self.instructions.trim().to_owned()).filter(|s| !s.is_empty());
                        if r.lost_focus() && text != self.draft.instructions {
                            self.draft.instructions = text;
                            out = Some(self.send(None));
                        }
                    });
                }
                Field::Env => {
                    let chat = level == Level::Chat;
                    let mut v = self.draft.env;
                    let shown = |k: Option<EnvKind>| match k {
                        None if chat => format!("{inh} ({})", e.env.value.label()),
                        None => "Auto".to_owned(),
                        Some(k) => k.label().to_owned(),
                    };
                    egui::ComboBox::from_id_salt(("env", level))
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

    /// The endpoint: what applies, or this level's own URL, model and key.
    fn endpoint(&mut self, ui: &mut egui::Ui, t: &Tokens, view: &ConfigView) -> Option<ClientMsg> {
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
    if s.is_empty() { "workspace".into() } else { s }
}

/// "New workspace": a name, a folder (fresh, or an existing one on the engine's machine),
/// and where its commands run.
pub struct NewWorkspaceForm {
    pub name: String,
    fresh: bool,
    pub path: String,
    browser: Option<FolderList>,
    browsing: bool,
    pub env: EnvKind,
    use_image: bool,
    image: String,
    check: Option<(bool, String)>,
    checking: bool,
    creating: bool,
    error: Option<String>,
    #[cfg(not(target_arch = "wasm32"))]
    picked: (std::sync::mpsc::Sender<Option<String>>, std::sync::mpsc::Receiver<Option<String>>),
}

impl Default for NewWorkspaceForm {
    fn default() -> Self {
        Self {
            name: String::new(),
            fresh: true,
            path: String::new(),
            browser: None,
            browsing: false,
            env: EnvKind::Auto,
            use_image: false,
            image: String::new(),
            check: None,
            checking: false,
            creating: false,
            error: None,
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
    /// Start with the Container environment chosen (scripted runs).
    pub fn with_env(env: EnvKind) -> Self {
        Self { env, ..Self::default() }
    }

    fn apply(&mut self, msg: &ServerMsg) -> bool {
        match msg {
            ServerMsg::Folders(list) => {
                if list.error.is_none() {
                    self.path.clone_from(&list.path);
                }
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

    /// The layer the new workspace starts with.
    fn layer(&self) -> Layer {
        Layer {
            env: Some(self.env).filter(|e| *e != EnvKind::Auto),
            image: Some(self.image.trim().to_owned()).filter(|i| self.env == EnvKind::Container && self.use_image && !i.is_empty()),
            ..Layer::default()
        }
    }

    /// Create was pressed: the message for the engine.
    pub fn create_msg(&mut self) -> ClientMsg {
        self.creating = true;
        self.error = None;
        let folder = if self.fresh { NewFolder::Create } else { NewFolder::Existing { path: self.path.trim().to_owned() } };
        ClientMsg::CreateWorkspace { name: self.name.trim().to_owned(), folder, config: self.layer() }
    }

    /// Why Create can't be pressed yet, if so.
    fn problem(&self, support: &EnvSupport) -> Option<String> {
        if self.name.trim().is_empty() {
            return Some("Give it a name.".into());
        }
        if !self.fresh && self.path.trim().is_empty() {
            return Some("Pick a folder.".into());
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

    pub fn show(&mut self, ui: &mut egui::Ui, t: &Tokens, root: &str, support: &EnvSupport) -> FormOutput {
        let mut out = FormOutput::default();
        #[cfg(not(target_arch = "wasm32"))]
        while let Ok(p) = self.picked.1.try_recv() {
            if let Some(p) = p {
                self.path = p;
                self.browser = None;
            }
        }
        let field_w = ui.available_width().min(420.0);
        ui.label(RichText::new("Name").color(t.text));
        let name = ui.add(egui::TextEdit::singleline(&mut self.name).hint_text("e.g. Data analysis").desired_width(field_w));
        if self.name.is_empty() && !name.has_focus() && ui.memory(|m| m.focused().is_none()) {
            name.request_focus();
        }
        ui.add_space(6.0);
        ui.label(RichText::new("Folder").color(t.text));
        let root_shown = if root.is_empty() { "~/byob-workspaces".to_owned() } else { root.trim_end_matches('/').to_owned() };
        ui.radio_value(&mut self.fresh, true, format!("New folder: {root_shown}/{}", slug(&self.name)));
        ui.radio_value(&mut self.fresh, false, "A folder of your choice").on_hover_text("Made if it doesn't exist yet");
        if !self.fresh {
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.path).hint_text("/home/me/project").desired_width((field_w - 150.0).max(140.0)));
                if ui.button("Browse…").on_hover_text("List folders on the machine byob runs on").clicked() {
                    self.browsing = true;
                    let path = Some(self.path.trim().to_owned()).filter(|p| !p.is_empty());
                    out.msgs.push(ClientMsg::ListFolders { path });
                }
                #[cfg(not(target_arch = "wasm32"))]
                if crate::export::dialog_unavailable_reason().is_none() && ui.button("Choose…").on_hover_text("The system's folder dialog").clicked() {
                    pick_folder(ui.ctx(), self.picked.0.clone());
                }
            });
            if self.browsing {
                theme::spinner(ui, t, None);
            }
            if let Some(list) = self.browser.clone()
                && let Some(go) = browser(ui, t, &list, field_w)
            {
                match go {
                    Some(path) => {
                        self.browsing = true;
                        out.msgs.push(ClientMsg::ListFolders { path: Some(path) });
                    }
                    None => self.browser = None,
                }
            }
        }
        ui.add_space(6.0);
        ui.label(RichText::new("Where commands run").color(t.text));
        ui.horizontal_wrapped(|ui| {
            for k in EnvKind::ALL {
                // Icon and choice wrap as one.
                let r = ui
                    .horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 3.0;
                        crate::icons::env_icon(ui, t, k, t.text_size(14.0));
                        ui.radio_value(&mut self.env, k, k.label())
                    })
                    .inner;
                if r.changed() && k == EnvKind::Container && support.runtime.is_some() && self.check.is_none() && !self.checking {
                    self.checking = true;
                    out.msgs.push(ClientMsg::CheckContainers);
                }
            }
        });
        ui.label(RichText::new(env_help(self.env)).small().color(t.text_dim));
        match self.env {
            EnvKind::Nix if !support.nix => {
                ui.label(RichText::new("Nix isn't installed on the machine byob runs on; commands would run without a dev shell.").small().color(t.warning));
            }
            EnvKind::Container => self.container_rows(ui, t, support, field_w, &mut out),
            _ => {}
        }
        ui.add_space(8.0);
        let problem = self.problem(support);
        if let Some(e) = &self.error {
            ui.label(RichText::new(e).color(t.danger));
        }
        ui.horizontal(|ui| {
            let can = problem.is_none() && !self.creating;
            let create = ui.add_enabled(can, egui::Button::new(RichText::new("Create workspace").color(if can { t.text_strong } else { t.text_dim })));
            let create = match &problem {
                Some(p) => create.on_disabled_hover_text(p),
                None => create,
            };
            if create.clicked() {
                out.msgs.push(self.create_msg());
            }
            if ui.button("Cancel").clicked() {
                out.close = true;
            }
            if self.creating {
                theme::spinner(ui, t, None);
            }
        });
        out
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

/// The desktop's folder dialog, on a thread (rfd talks to the desktop portal).
#[cfg(not(target_arch = "wasm32"))]
fn pick_folder(ctx: &egui::Context, tx: std::sync::mpsc::Sender<Option<String>>) {
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new().name("byob-folder-picker".into()).spawn(move || {
        let picked = pollster::block_on(rfd::AsyncFileDialog::new().set_title("Workspace folder").pick_folder());
        let _ = tx.send(picked.map(|f| f.path().display().to_string()));
        ctx.request_repaint();
    });
}

/// One workspace's settings window: name, folder, its settings, its environment's state,
/// and Remove.
pub struct WorkspaceWindow {
    pub id: String,
    name: String,
    editor: LayerEditor,
    env: EnvPanel,
    removing: bool,
    clean: bool,
}

impl WorkspaceWindow {
    pub fn new(id: &str, remove: bool) -> Self {
        Self {
            id: id.to_owned(),
            name: String::new(),
            editor: LayerEditor::new(ConfigScope::Workspace { id: id.to_owned() }),
            env: EnvPanel::default(),
            removing: remove,
            clean: false,
        }
    }

    /// Draw; returns messages and whether to close.
    fn show(&mut self, ui: &mut egui::Ui, t: &Tokens, ws: Option<&WorkspaceSummary>, plugins: &mut crate::plugins::PluginsUi) -> FormOutput {
        let mut out = FormOutput::default();
        let Some(ws) = ws else {
            ui.label(RichText::new("This workspace was removed.").color(t.text_dim));
            out.close = ui.button("Close").clicked();
            return out;
        };
        if self.name.is_empty() {
            self.name.clone_from(&ws.name);
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("Name").color(t.text));
            ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(220.0));
            if self.name.trim() != ws.name && !self.name.trim().is_empty() && ui.button("Rename").clicked() {
                out.msgs.push(ClientMsg::RenameWorkspace { id: ws.id.clone(), name: self.name.trim().to_owned() });
            }
        });
        ui.label(RichText::new(&ws.folder).small().monospace().color(t.text_dim))
            .on_hover_text("Its folder on the machine byob runs on; byob never writes its settings there");
        ui.add_space(6.0);
        let before = self.editor.view().map(|v| v.own.env);
        if let Some(m) = self.editor.show(ui, t, WORKSPACE_FIELDS) {
            out.msgs.push(m);
        }
        if before.is_some() && self.editor.view().map(|v| v.own.env) != before {
            self.env.refresh();
        }
        ui.add_space(6.0);
        egui::CollapsingHeader::new(RichText::new("Environment state").color(t.text)).id_salt(("envstate", &ws.id)).default_open(true).show(ui, |ui| {
            if let Some(m) = self.env.show(ui, t, &ws.id) {
                out.msgs.push(m);
            }
        });
        ui.add_space(6.0);
        egui::CollapsingHeader::new(RichText::new("Plugins").color(t.text)).id_salt(("wsplugins", &ws.id)).default_open(true).show(ui, |ui| {
            ui.label(RichText::new("Its .byob/plugins folder, committed with the repository; switches here apply to its chats.").small().color(t.text_faint));
            plugins.panel(ui, t, &crate::plugins::Filter::Workspace(ws.id.clone()));
        });
        ui.add_space(8.0);
        if self.removing {
            egui::Frame::new().stroke(egui::Stroke::new(1.0, t.danger)).corner_radius(t.round(6.0)).inner_margin(8.0).show(ui, |ui| {
                let n = ws.chats;
                ui.label(
                    RichText::new(format!(
                        "Remove \u{201c}{}\u{201d} and its {n} chat{} from byob? The folder and its files stay.",
                        ws.name,
                        if n == 1 { "" } else { "s" }
                    ))
                    .color(t.text),
                );
                ui.checkbox(&mut self.clean, "Also delete what byob keeps for it elsewhere: installed packages, its container");
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Remove").color(t.danger)).clicked() {
                        out.msgs.push(ClientMsg::RemoveWorkspace { id: ws.id.clone(), clean: self.clean });
                        out.close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        self.removing = false;
                    }
                });
            });
        } else if ui.button(RichText::new("Remove workspace…").color(t.danger)).clicked() {
            self.removing = true;
        }
        out
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
}

impl Default for WorkspaceUi {
    fn default() -> Self {
        Self { root: String::new(), support: EnvSupport::default(), form: None, window: None, chat: None, defaults: LayerEditor::new(ConfigScope::Global) }
    }
}

impl WorkspaceUi {
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
    }

    pub fn open_settings(&mut self, id: &str, remove: bool) {
        self.window = Some(WorkspaceWindow::new(id, remove));
    }

    pub fn toggle_chat(&mut self) {
        self.chat = match self.chat.take() {
            Some(_) => None,
            None => Some(LayerEditor::new(ConfigScope::Chat)),
        };
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
        if let Some(form) = &mut self.form {
            let mut open = true;
            let (root, support) = (self.root.clone(), self.support.clone());
            let mut close = false;
            egui::Window::new("New workspace")
                .id(egui::Id::unique("new-workspace"))
                .open(&mut open)
                .collapsible(false)
                .fade_in(false)
                .resizable(false)
                .default_width(max_w.min(460.0))
                .max_width(max_w)
                .max_height(max_h)
                .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 60.0))
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().max_height(max_h - 40.0).min_scrolled_height((max_h - 40.0).min(560.0)).show(ui, |ui| {
                        let o = form.show(ui, t, &root, &support);
                        msgs.extend(o.msgs);
                        close = o.close;
                    });
                });
            if !open || close {
                self.form = None;
            }
        }
        if let Some(w) = &mut self.window {
            let mut open = true;
            let mut close = false;
            let ws = workspaces.iter().find(|x| x.id == w.id);
            let title = ws.map_or_else(|| "Workspace".to_owned(), |x| format!("Workspace \u{201c}{}\u{201d}", x.name));
            egui::Window::new(title)
                .id(egui::Id::unique("workspace-settings"))
                .open(&mut open)
                .collapsible(false)
                .fade_in(false)
                .resizable(false)
                .default_width(max_w)
                .max_width(max_w)
                .max_height(max_h)
                .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 50.0))
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().max_height(max_h - 40.0).min_scrolled_height((max_h - 40.0).min(560.0)).show(ui, |ui| {
                        let o = w.show(ui, t, ws, plugins);
                        msgs.extend(o.msgs);
                        close = o.close;
                    });
                });
            if !open || close {
                self.window = None;
            }
        }
        if let Some(editor) = &mut self.chat {
            let mut open = true;
            let w = max_w.min(440.0);
            let x = anchor.x.clamp(8.0, (screen.right() - w - 8.0).max(8.0));
            egui::Window::new("Chat settings")
                .id(egui::Id::unique("chat-settings"))
                .open(&mut open)
                .collapsible(false)
                .fade_in(false)
                .resizable(false)
                .title_bar(true)
                .default_width(w)
                .max_width(w)
                .max_height(max_h)
                .fixed_pos(egui::pos2(x, anchor.y))
                .show(ctx, |ui| {
                    let ws = editor.view().and_then(|v| v.workspace.as_ref()).map(|w| w.name.clone());
                    let line = match ws {
                        Some(ws) => format!("{chat_title} · in {ws}"),
                        None => chat_title.to_owned(),
                    };
                    ui.add(egui::Label::new(RichText::new(line).small().color(t.text_dim)).truncate());
                    ui.label(
                        RichText::new("Only for this chat; everything else comes from its workspace and the global settings.").small().color(t.text_faint),
                    );
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical().max_height(max_h - 60.0).min_scrolled_height((max_h - 60.0).min(520.0)).show(ui, |ui| {
                        if let Some(m) = editor.show(ui, t, CHAT_FIELDS) {
                            msgs.push(m);
                        }
                        plugins.chat_switches(ui, t);
                    });
                });
            if !open {
                self.chat = None;
            }
        }
        msgs
    }
}

#[cfg(test)]
mod tests {
    use byob_types::config::{Forced, Keys, Levels, resolve};

    use super::*;

    fn view(scope: ConfigScope, own: Layer, ws: Option<&Layer>) -> ConfigView {
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
    fn the_new_workspace_form_checks_before_creating() {
        let support = EnvSupport { nix: false, installer: None, runtime: Some("docker".into()) };
        let mut f = NewWorkspaceForm::default();
        assert!(f.problem(&support).is_some_and(|p| p.contains("name")));
        f.name = "Data".into();
        assert!(f.problem(&support).is_none());
        f.fresh = false;
        assert!(f.problem(&support).is_some_and(|p| p.contains("folder")));
        f.path = "/srv/data".into();
        f.env = EnvKind::Container;
        f.use_image = true;
        f.image = "bad image!".into();
        assert!(f.problem(&support).is_some_and(|p| p.contains("image")));
        f.image = "debian:stable-slim".into();
        assert_eq!(f.layer(), Layer { env: Some(EnvKind::Container), image: Some("debian:stable-slim".into()), ..Layer::default() });
        assert!(f.problem(&EnvSupport::default()).is_some_and(|p| p.contains("Docker")));
        // Answers from the engine land in the form.
        f.apply(&ServerMsg::Folders(FolderList { path: "/srv".into(), parent: Some("/".into()), dirs: vec!["data".into()], truncated: false, error: None }));
        assert_eq!(f.path, "/srv");
        f.apply(&ServerMsg::ContainersChecked { ok: false, message: "Docker isn't running".into() });
        assert_eq!(f.check, Some((false, "Docker isn't running".to_owned())));
        assert_eq!(slug("My Data!"), "my-data");
        let mut sent = Vec::new();
        render(|ui| sent = f.show(ui, &Tokens::dark(), "/home/me/byob-workspaces", &support).msgs);
        assert!(sent.is_empty());
    }

    #[test]
    fn created_workspaces_close_the_form_and_errors_stay() {
        let mut w = WorkspaceUi::default();
        w.open_new(EnvKind::Container);
        assert!(w.apply(&ServerMsg::WorkspaceCreated { id: None, error: Some("already a workspace".into()) }));
        assert_eq!(w.form.as_ref().and_then(|f| f.error.as_deref()), Some("already a workspace"));
        w.apply(&ServerMsg::WorkspaceCreated { id: Some("w1".into()), error: None });
        assert!(w.form.is_none());
        w.toggle_chat();
        assert!(w.chat.is_some());
        w.toggle_chat();
        assert!(w.chat.is_none());
    }
}
