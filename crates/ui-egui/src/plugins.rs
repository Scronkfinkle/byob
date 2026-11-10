//! Plugins as the UI shows them, from the engine's `ServerMsg::Extensions` for the chat in
//! view:
//!
//! - the **plugin card** under a step that made, changed, kept or removed a plugin: what it
//!   adds, the test run, problems, the settings it still needs (typed in place; a secret is
//!   a password field and is never shown again) and Keep / Files;
//! - the **Plugins panel** (a section of the app settings, a filtered one in a workspace's
//!   settings, switches in the chat settings): each plugin with where it lives and who wrote
//!   it, its switch and where that comes from, tools, skills, subagents, commands, servers,
//!   settings, problems, and keep / trust / remove;
//! - the **first-open card** when a workspace's folder brought plugins nobody decided on;
//! - the **header chip**: how many plugins are on, with a badge when one has trouble.
//!
//! Setting values never come back from the engine; the UI forgets what was typed as soon as
//! it is sent.

use std::collections::{BTreeMap, BTreeSet};

use byob_protocol::workspace::Level;
use byob_protocol::{
    ClientMsg, ConfigScope, Entry, Extensions, PluginAction, PluginCard, PluginInfo, PluginOrigin, PluginScope, PluginSetting, ServerMsg, ServerState,
    SlashItem, StepStatus,
};
use egui::{RichText, Stroke, vec2};

use crate::icons;
use crate::theme::{Tokens, bold_family, mix};

/// Which plugins a panel lists, and where its switches act.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// Every plugin the chat in view sees (app settings); a switch acts at the level that
    /// decides it now (everywhere when nobody switched it).
    All,
    /// A workspace's own plugins; switches act for the workspace.
    Workspace(String),
}

/// Plugin state between frames.
#[derive(Default)]
pub struct PluginsUi {
    ext: Option<Extensions>,
    /// Setting values being typed, by (plugin, key). Dropped once sent.
    drafts: BTreeMap<(String, String), String>,
    /// Settings that have a value whose "Change" was clicked.
    changing: BTreeSet<(String, String)>,
    /// The id to keep a scratch plugin under, by the step whose card offers it.
    keep_as: BTreeMap<u64, String>,
    /// A copy waiting for "Remove" to be confirmed: (plugin, scope).
    removing: Option<(String, PluginScope)>,
    /// The first-open card's "Choose…" picks.
    choosing: Option<BTreeSet<String>>,
    /// The plugin the panel shows open (a card's "Files").
    focus: Option<String>,
    /// Open the first plugin with trouble once the list is known.
    focus_trouble: bool,
    /// Scroll the settings to the Plugins section on the next frame.
    pub(crate) reveal: bool,
    /// Open the settings at the Plugins section.
    open_requested: bool,
    out: Vec<ClientMsg>,
}

impl PluginsUi {
    /// Take in a message meant for plugins; false if it isn't one.
    pub fn apply(&mut self, msg: &ServerMsg) -> bool {
        let ServerMsg::Extensions(e) = msg else { return false };
        self.ext = Some((**e).clone());
        true
    }

    pub fn extensions(&self) -> Option<&Extensions> {
        self.ext.as_ref()
    }

    /// What the composer's `/` menu offers.
    pub fn slash(&self) -> &[SlashItem] {
        self.ext.as_ref().map_or(&[], |e| e.slash.as_slice())
    }

    /// Messages for the engine since the last call.
    pub fn take(&mut self) -> Vec<ClientMsg> {
        std::mem::take(&mut self.out)
    }

    /// Something asked to show the Plugins panel (a card's Files, the header chip).
    pub fn take_open_request(&mut self) -> bool {
        std::mem::take(&mut self.open_requested)
    }

    /// Show the Plugins panel, with `plugin` open.
    pub fn open(&mut self, plugin: Option<&str>) {
        self.open_requested = true;
        self.reveal = true;
        if let Some(p) = plugin {
            self.focus = Some(p.to_owned());
        }
    }

    /// Show the Plugins panel, open on the first plugin with trouble (if any).
    pub fn open_on_trouble(&mut self) {
        self.focus_trouble = true;
        self.open(None);
    }

    /// The copy of `id` the chat uses.
    fn used(&self, id: &str) -> Option<&PluginInfo> {
        self.ext.as_ref()?.plugins.iter().find(|p| p.id == id && p.shadowed_by.is_none())
    }

    /// (plugins on, any trouble): for the header chip. `None` before the engine said.
    pub fn summary(&self) -> Option<(usize, bool)> {
        let e = self.ext.as_ref()?;
        let used = e.plugins.iter().filter(|p| p.shadowed_by.is_none());
        let on = used.clone().filter(|p| p.enabled).count();
        let trouble = !e.errors.is_empty() || used.filter(|p| p.enabled).any(has_trouble);
        Some((on, trouble))
    }

    // ---- the plugin card ----------------------------------------------------------

    /// The card under step `step` that changed a plugin. Only the `latest` card of a plugin
    /// in the chat offers its settings and Keep; earlier ones are a record.
    pub fn card(&mut self, ui: &mut egui::Ui, t: &Tokens, step: u64, card: &PluginCard, latest: bool) {
        let info = self.used(&card.plugin).filter(|p| p.scope == card.scope).cloned();
        let failed = card.test.as_ref().is_some_and(|x| !x.ok) || !card.problems.is_empty();
        let edge = if failed { mix(t.border, t.warning, 0.6) } else { t.border };
        let frame = egui::Frame::new().fill(t.code_bg).stroke(Stroke::new(1.0, edge)).corner_radius(t.round(8.0)).inner_margin(vec2(t.pad(10.0), t.pad(8.0)));
        frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                puzzle_icon(ui, t, 15.0);
                let name = info.as_ref().map_or(card.plugin.as_str(), |p| p.name.as_str());
                ui.label(RichText::new(name).strong().color(t.text_strong));
                scope_badge(ui, t, card.scope);
                ui.label(RichText::new(action_text(card.action)).small().color(t.text_dim));
            });
            tools_line(ui, t, card);
            if let Some(test) = &card.test {
                let (icon, text, color) = if test.ok { ("✓", "Test run passed", t.success) } else { ("✗", "Test run failed", t.danger) };
                let id = ui.make_persistent_id(("plugin-test", step));
                egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, !test.ok)
                    .show_header(ui, |ui| {
                        ui.label(RichText::new(format!("{icon} {text}")).color(color));
                        // A passing run's answer, at a glance.
                        let first = test.output.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
                        if test.ok && !first.is_empty() {
                            ui.add(egui::Label::new(RichText::new(format!("→ {first}")).small().monospace().color(t.text_dim)).truncate());
                        }
                    })
                    .body(|ui| code_block(ui, t, &test.output));
            }
            // Missing settings get their own fields below.
            for p in card.problems.iter().filter(|p| !p.starts_with(NEEDS_SETTINGS)) {
                ui.add(egui::Label::new(RichText::new(format!("⚠ {p}")).small().color(t.warning)).wrap());
            }
            if !latest {
                return;
            }
            // The settings it needs now (the card's list is from when it was made).
            let needed: Vec<PluginSetting> = match &info {
                Some(p) => p.settings.iter().filter(|s| card.missing_settings.contains(&s.label) || (s.required && !s.set)).cloned().collect(),
                None => Vec::new(),
            };
            if !needed.is_empty() {
                ui.add_space(2.0);
                ui.label(RichText::new("Settings it needs").small().color(t.text_dim));
                for s in &needed {
                    self.setting_row(ui, t, &card.plugin, s);
                }
            } else if info.is_none() && !card.missing_settings.is_empty() {
                ui.label(RichText::new(format!("Needs: {}", card.missing_settings.join(", "))).small().color(t.warning));
            }
            ui.add_space(2.0);
            self.card_actions(ui, t, step, card, info.as_ref());
        });
    }

    fn card_actions(&mut self, ui: &mut egui::Ui, t: &Tokens, step: u64, card: &PluginCard, info: Option<&PluginInfo>) {
        ui.horizontal_wrapped(|ui| {
            let keepable = card.scope == PluginScope::Chat && card.action != PluginAction::Removed && info.is_some();
            if keepable {
                let name = self.keep_as.entry(step).or_insert_with(|| suggested_id(card));
                if card.plugin == SCRATCH {
                    ui.label(RichText::new("Name").color(t.text_dim));
                    ui.add(egui::TextEdit::singleline(name).desired_width(170.0).hint_text("plugin-name"))
                        .on_hover_text("The plugin's name where it is kept: lowercase words with dashes");
                }
                let new_id = (card.plugin == SCRATCH).then(|| name.trim().to_owned());
                let ok = new_id.as_deref().is_none_or(byob_types::config::plugin_id_ok);
                for (scope, text, tip) in [
                    (PluginScope::Workspace, "Keep in this workspace", "Copy it into the workspace's .byob/plugins: every chat there has it"),
                    (PluginScope::Global, "Keep everywhere", "Copy it into your plugins folder: every chat has it"),
                ] {
                    if ui.add_enabled(ok, egui::Button::new(text)).on_hover_text(tip).clicked() {
                        self.out.push(ClientMsg::KeepPlugin { plugin: card.plugin.clone(), scope, new_id: new_id.clone() });
                    }
                }
            }
            let dir = info.map(|p| p.dir.clone()).unwrap_or_default();
            let files = ui.button("Files");
            let files = if dir.is_empty() {
                files.on_hover_text("Show it in Settings → Plugins")
            } else {
                files.on_hover_text(format!("{dir}\n\nShow it in Settings → Plugins"))
            };
            if files.clicked() {
                self.open(Some(&card.plugin));
            }
            let _ = t;
        });
    }

    /// One setting: "set ✓" (Change, Clear) or a field (a password field if secret) with Save.
    fn setting_row(&mut self, ui: &mut egui::Ui, t: &Tokens, plugin: &str, s: &PluginSetting) {
        let key = (plugin.to_owned(), s.key.clone());
        ui.horizontal_wrapped(|ui| {
            let label = ui.label(RichText::new(&s.label).color(t.text));
            if let Some(h) = &s.help {
                label.on_hover_text(h);
            }
            if s.set && !self.changing.contains(&key) {
                ui.label(RichText::new("set ✓").small().color(t.success));
                if ui.small_button("Change").clicked() {
                    self.changing.insert(key.clone());
                }
                if ui.small_button("Clear").on_hover_text("Forget its value").clicked() {
                    self.out.push(ClientMsg::SetPluginSetting { plugin: plugin.to_owned(), key: s.key.clone(), value: None });
                }
                return;
            }
            let draft = self.drafts.entry(key.clone()).or_default();
            let hint = if s.secret { "secret value" } else { "value" };
            let field = ui.add(egui::TextEdit::singleline(draft).password(s.secret).desired_width(180.0).hint_text(hint));
            let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let ready = !draft.trim().is_empty();
            let save = ui.add_enabled(ready, egui::Button::new("Save")).clicked();
            if s.set && ui.small_button("Cancel").clicked() {
                self.changing.remove(&key);
                self.drafts.remove(&key);
                return;
            }
            if ready && (save || entered) {
                let value = self.drafts.remove(&key).unwrap_or_default();
                self.changing.remove(&key);
                self.out.push(ClientMsg::SetPluginSetting { plugin: plugin.to_owned(), key: s.key.clone(), value: Some(value.trim().to_owned()) });
            }
        });
    }

    // ---- the panel ----------------------------------------------------------------

    /// The Plugins panel: load errors, then each scope's plugins.
    pub fn panel(&mut self, ui: &mut egui::Ui, t: &Tokens, filter: &Filter) {
        let Some(ext) = self.ext.clone() else {
            ui.label(RichText::new("Waiting for the engine…").small().color(t.text_dim));
            return;
        };
        if std::mem::take(&mut self.focus_trouble) {
            let first = ext.plugins.iter().find(|p| p.shadowed_by.is_none() && p.enabled && has_trouble(p));
            if let Some(p) = first {
                self.focus = Some(p.id.clone());
            }
        }
        if let Filter::Workspace(id) = filter
            && ext.workspace.as_deref() != Some(id.as_str())
        {
            ui.label(RichText::new("Open a chat in this workspace to see its plugins.").small().color(t.text_dim));
            return;
        }
        ui.horizontal_wrapped(|ui| {
            let used: Vec<&PluginInfo> = ext.plugins.iter().filter(|p| p.shadowed_by.is_none()).collect();
            let on = used.iter().filter(|p| p.enabled).count();
            let trouble = used.iter().filter(|p| has_trouble(p)).count();
            let mut text = format!("{on} of {} on", used.len());
            if trouble > 0 {
                text.push_str(&format!(" · {trouble} with problems"));
            }
            ui.label(RichText::new(text).small().color(t.text_dim));
            if ui.small_button("Reload").on_hover_text("Read every plugin folder again; failed servers retry").clicked() {
                self.out.push(ClientMsg::ReloadPlugins);
            }
        });
        if *filter == Filter::All {
            for e in &ext.errors {
                ui.add(egui::Label::new(RichText::new(format!("✗ {} ({}): {}", e.path, e.scope.label(), e.message)).small().color(t.danger)).wrap());
            }
        }
        let scopes: &[PluginScope] = match filter {
            Filter::All => &[PluginScope::Chat, PluginScope::Workspace, PluginScope::Global],
            Filter::Workspace(_) => &[PluginScope::Workspace],
        };
        let mut any = false;
        for scope in scopes {
            let list: Vec<&PluginInfo> = ext.plugins.iter().filter(|p| p.scope == *scope).collect();
            if list.is_empty() {
                continue;
            }
            any = true;
            ui.add_space(4.0);
            ui.label(RichText::new(scope_heading(*scope)).small().strong().color(t.text_dim));
            for p in list {
                self.plugin_row(ui, t, p, filter, ext.workspace.as_deref());
            }
        }
        if !any {
            let text = match filter {
                Filter::All => "No plugins yet. Ask byob to make you a tool, or put plugin folders in your plugins folder.",
                Filter::Workspace(_) => "This workspace has no plugins of its own (.byob/plugins).",
            };
            ui.label(RichText::new(text).small().color(t.text_faint));
        }
    }

    fn plugin_row(&mut self, ui: &mut egui::Ui, t: &Tokens, p: &PluginInfo, filter: &Filter, workspace: Option<&str>) {
        let id = ui.make_persistent_id(("plugin-row", p.scope, &p.id));
        let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
        if self.focus.as_deref() == Some(p.id.as_str()) && p.shadowed_by.is_none() {
            state.set_open(true);
            self.focus = None;
            ui.scroll_to_cursor(Some(egui::Align::Center));
        }
        let shadowed = p.shadowed_by.is_some();
        // Where the switch acts: the workspace in its own settings; else the level that
        // decides it now (everywhere when nobody switched it).
        let switch_at = match filter {
            Filter::Workspace(w) => Some(ConfigScope::Workspace { id: w.clone() }),
            Filter::All => scope_for(p.enabled_from, workspace),
        };
        state
            .show_header(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let mut on = p.enabled;
                let tip = switch_tip(p, switch_at.as_ref());
                if ui.add_enabled(!shadowed && switch_at.is_some(), egui::Checkbox::without_text(&mut on)).on_hover_text(tip).changed()
                    && let Some(scope) = switch_at.clone()
                {
                    self.out.push(ClientMsg::SetPluginEnabled { scope, plugin: p.id.clone(), enabled: Some(on) });
                }
                let color = if shadowed { t.text_faint } else { t.text_strong };
                ui.label(RichText::new(&p.name).strong().color(color));
                ui.label(RichText::new(&p.version).small().color(t.text_faint));
                ui.label(RichText::new(origin_text(p.origin)).small().color(t.text_dim));
                if let Some(by) = p.shadowed_by {
                    ui.label(RichText::new(format!("replaced by the copy in {}", by.label())).small().color(t.text_faint));
                } else if p.origin == PluginOrigin::Repo && !p.trusted {
                    let text = if p.declined { "code off" } else { "waiting for your OK" };
                    ui.label(RichText::new(text).small().color(t.warning));
                } else if has_trouble(p) {
                    ui.label(RichText::new("⚠").color(t.warning)).on_hover_text("Has problems: open it for details");
                }
            })
            .body(|ui| self.plugin_details(ui, t, p, workspace));
    }

    fn plugin_details(&mut self, ui: &mut egui::Ui, t: &Tokens, p: &PluginInfo, workspace: Option<&str>) {
        if !p.description.is_empty() {
            ui.add(egui::Label::new(RichText::new(&p.description).small().color(t.text)).wrap());
        }
        // The switch and where it comes from.
        ui.horizontal_wrapped(|ui| {
            let state = if p.enabled { "On" } else { "Off" };
            let from = match p.enabled_from {
                Level::Default => "nobody switched it".to_owned(),
                Level::Global => "set for every chat".to_owned(),
                Level::Workspace => "set in this workspace".to_owned(),
                Level::Chat => "set in this chat".to_owned(),
                Level::Flag => "set on the command line".to_owned(),
            };
            ui.label(RichText::new(format!("{state}: {from}")).small().color(t.text_dim));
            if p.enabled_from != Level::Default
                && let Some(scope) = scope_for(p.enabled_from, workspace)
                && ui.small_button("Reset").on_hover_text("Forget this switch: the less specific one (or on) applies").clicked()
            {
                self.out.push(ClientMsg::SetPluginEnabled { scope, plugin: p.id.clone(), enabled: None });
            }
            ui.menu_button(RichText::new("Switch…").small(), |ui| {
                for (level, text) in [(Level::Chat, "in this chat"), (Level::Workspace, "in this workspace"), (Level::Global, "everywhere")] {
                    let Some(scope) = scope_for(level, workspace) else { continue };
                    for on in [true, false] {
                        let label = format!("{} {text}", if on { "On" } else { "Off" });
                        if ui.button(label).clicked() {
                            self.out.push(ClientMsg::SetPluginEnabled { scope: scope.clone(), plugin: p.id.clone(), enabled: Some(on) });
                            ui.close();
                        }
                    }
                }
            });
        });
        list_line(ui, t, "Tools", &p.tools, "");
        list_line(ui, t, "Skills", &p.skills, "");
        list_line(ui, t, "Subagents", &p.agents, "");
        list_line(ui, t, "Commands", &p.commands, "/");
        for s in &p.servers {
            let (state, color) = match s.state {
                ServerState::Connecting => ("connecting…".to_owned(), t.text_dim),
                ServerState::Ready => (format!("ready · {} tools", s.tools), t.success),
                ServerState::Failed => ("failed".to_owned(), t.danger),
                ServerState::Off => ("off".to_owned(), t.text_faint),
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("Server {}", s.name)).small().color(t.text));
                ui.label(RichText::new(state).small().color(color));
                if let Some(e) = &s.error {
                    ui.add(egui::Label::new(RichText::new(e).small().color(if s.state == ServerState::Failed { t.danger } else { t.text_dim })).wrap());
                }
            });
        }
        if p.network {
            ui.label(RichText::new("Its tools may use the internet").small().color(t.text_dim));
        }
        for s in &p.settings {
            self.setting_row(ui, t, &p.id, s);
        }
        for problem in p.problems.iter().filter(|p| !p.starts_with(NEEDS_SETTINGS)) {
            ui.add(egui::Label::new(RichText::new(format!("⚠ {problem}")).small().color(t.warning)).wrap());
        }
        if let Some(e) = &p.last_error {
            ui.add(egui::Label::new(RichText::new(format!("Last failure: {e}")).small().color(t.danger)).wrap());
        }
        ui.add(egui::Label::new(RichText::new(&p.dir).small().monospace().color(t.text_faint)).selectable(true).wrap());
        self.plugin_actions(ui, t, p, workspace);
    }

    fn plugin_actions(&mut self, ui: &mut egui::Ui, t: &Tokens, p: &PluginInfo, workspace: Option<&str>) {
        let removing = self.removing.as_ref().is_some_and(|(id, scope)| *id == p.id && *scope == p.scope);
        if removing {
            egui::Frame::new().stroke(Stroke::new(1.0, t.danger)).corner_radius(t.round(6.0)).inner_margin(6.0).show(ui, |ui| {
                ui.label(RichText::new(format!("Delete the folder of {} ({})?", p.name, p.scope.label())).small().color(t.text));
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Remove").color(t.danger)).clicked() {
                        self.out.push(ClientMsg::RemovePlugin { plugin: p.id.clone(), scope: p.scope });
                        self.removing = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.removing = None;
                    }
                });
            });
            return;
        }
        ui.horizontal_wrapped(|ui| {
            if p.shadowed_by.is_none() {
                let keep: &[(PluginScope, &str)] = match p.scope {
                    PluginScope::Chat => &[(PluginScope::Workspace, "Keep in this workspace"), (PluginScope::Global, "Keep everywhere")],
                    PluginScope::Workspace => &[(PluginScope::Global, "Keep everywhere")],
                    PluginScope::Global => &[],
                };
                for (scope, text) in keep {
                    if ui.small_button(*text).clicked() {
                        self.out.push(ClientMsg::KeepPlugin { plugin: p.id.clone(), scope: *scope, new_id: None });
                    }
                }
            }
            if p.origin == PluginOrigin::Repo
                && let Some(ws) = workspace
            {
                let (text, trust) = if p.trusted { ("Turn its code off", false) } else { ("Let its code run", true) };
                if ui.small_button(text).on_hover_text("Its tools and servers (tool calls still follow your approvals)").clicked() {
                    self.out.push(ClientMsg::TrustPlugins { workspace: ws.to_owned(), plugins: vec![p.id.clone()], trust });
                }
            }
            if p.origin != PluginOrigin::Bundled && ui.small_button(RichText::new("Remove…").color(t.danger)).clicked() {
                self.removing = Some((p.id.clone(), p.scope));
            }
        });
    }

    /// The chat settings popover's part: each plugin's switch for this chat only.
    pub fn chat_switches(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        let Some(ext) = self.ext.as_ref() else { return };
        let used: Vec<PluginInfo> = ext.plugins.iter().filter(|p| p.shadowed_by.is_none()).cloned().collect();
        if used.is_empty() {
            return;
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Plugins").family(bold_family()).color(t.text_strong));
            if ui.small_button("All plugins…").clicked() {
                self.open(None);
            }
        });
        for p in &used {
            ui.horizontal_wrapped(|ui| {
                let mut on = p.enabled;
                let text = RichText::new(&p.name).color(t.text);
                if ui.checkbox(&mut on, text).on_hover_text(format!("{}\n{}", p.description, switch_tip(p, Some(&ConfigScope::Chat)))).changed() {
                    self.out.push(ClientMsg::SetPluginEnabled { scope: ConfigScope::Chat, plugin: p.id.clone(), enabled: Some(on) });
                }
                if p.enabled_from == Level::Chat {
                    if ui.small_button("Reset").on_hover_text("Use the workspace's or global switch").clicked() {
                        self.out.push(ClientMsg::SetPluginEnabled { scope: ConfigScope::Chat, plugin: p.id.clone(), enabled: None });
                    }
                } else if p.enabled_from != Level::Default {
                    ui.label(RichText::new(format!("from {}", p.enabled_from.label())).small().color(t.text_faint));
                }
            });
        }
    }

    // ---- the first-open card ------------------------------------------------------

    /// Plugins the workspace's folder brought that nobody decided on.
    pub fn undecided(&self) -> Vec<&PluginInfo> {
        let Some(e) = &self.ext else { return Vec::new() };
        if e.workspace.is_none() {
            return Vec::new();
        }
        e.plugins
            .iter()
            .filter(|p| p.scope == PluginScope::Workspace && p.origin == PluginOrigin::Repo && !p.trusted && !p.declined && p.shadowed_by.is_none())
            .collect()
    }

    /// The card asking about the workspace's plugins, unless the turn's approval card asks
    /// the same right now (`entries`: the chat in view).
    pub fn trust_card(&mut self, ui: &mut egui::Ui, t: &Tokens, entries: &[Entry]) {
        let asking = entries.iter().flat_map(|e| &e.steps).any(|s| s.status == StepStatus::Waiting && s.tool == TRUST_STEP);
        let pending: Vec<PluginInfo> = self.undecided().into_iter().cloned().collect();
        let Some(ws) = self.ext.as_ref().and_then(|e| e.workspace.clone()) else { return };
        if pending.is_empty() || asking {
            self.choosing = None;
            return;
        }
        let ids: Vec<String> = pending.iter().map(|p| p.id.clone()).collect();
        let fill = mix(t.assistant_bubble, t.accent, 0.05);
        let frame = egui::Frame::new().fill(fill).stroke(Stroke::new(1.0, mix(t.border, t.accent, 0.6))).corner_radius(t.radius).inner_margin(t.pad(10.0));
        crate::readable_column(ui, t, |ui| {
            ui.add_space(t.pad(6.0));
            frame.show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    puzzle_icon(ui, t, 16.0);
                    let title = if pending.len() == 1 { "This workspace brings a plugin" } else { "This workspace brings plugins" };
                    ui.label(RichText::new(title).family(bold_family()).size(t.text_size(15.0)).color(t.text_strong));
                });
                ui.add(
                    egui::Label::new(
                        RichText::new(if pending.len() == 1 {
                            "It came with the folder; byob didn't write it. Its skills are in use; its tools and servers run only if you turn it on (tool calls still follow your approvals)."
                        } else {
                            "They came with the folder; byob didn't write them. Their skills are in use; their tools and servers run only if you turn them on (tool calls still follow your approvals)."
                        })
                            .small()
                            .color(t.text_dim),
                    )
                    .wrap(),
                );
                ui.add_space(4.0);
                for p in &pending {
                    ui.horizontal_wrapped(|ui| {
                        if let Some(picked) = &mut self.choosing {
                            let mut on = picked.contains(&p.id);
                            if ui.checkbox(&mut on, "").changed() {
                                if on {
                                    picked.insert(p.id.clone());
                                } else {
                                    picked.remove(&p.id);
                                }
                            }
                        }
                        ui.label(RichText::new(&p.name).strong().color(t.text));
                        ui.label(RichText::new(&p.version).small().color(t.text_faint));
                        if !p.description.is_empty() {
                            ui.label(RichText::new(&p.description).small().color(t.text_dim));
                        }
                    });
                    let adds = adds_text(p);
                    if !adds.is_empty() {
                        ui.label(RichText::new(adds).small().color(t.text_dim));
                    }
                }
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| match self.choosing.clone() {
                    Some(picked) => {
                        let label = RichText::new(format!("Turn on {} selected", picked.len())).color(t.on_accent).family(bold_family());
                        if ui.add(egui::Button::new(label).fill(t.accent)).clicked() {
                            let (yes, no): (Vec<String>, Vec<String>) = ids.iter().cloned().partition(|id| picked.contains(id));
                            if !yes.is_empty() {
                                self.out.push(ClientMsg::TrustPlugins { workspace: ws.clone(), plugins: yes, trust: true });
                            }
                            if !no.is_empty() {
                                self.out.push(ClientMsg::TrustPlugins { workspace: ws.clone(), plugins: no, trust: false });
                            }
                            self.choosing = None;
                        }
                        if ui.button("Back").clicked() {
                            self.choosing = None;
                        }
                    }
                    None => {
                        let all = RichText::new(if pending.len() == 1 { "Enable" } else { "Enable all" }).color(t.on_accent).family(bold_family());
                        if ui.add(egui::Button::new(all).fill(t.accent)).clicked() {
                            self.out.push(ClientMsg::TrustPlugins { workspace: ws.clone(), plugins: ids.clone(), trust: true });
                        }
                        if pending.len() > 1 && ui.button("Choose…").clicked() {
                            self.choosing = Some(BTreeSet::new());
                        }
                        if ui.button("Not now").on_hover_text("Keep their code off; turn them on any time in Settings → Plugins").clicked() {
                            self.out.push(ClientMsg::TrustPlugins { workspace: ws.clone(), plugins: ids.clone(), trust: false });
                        }
                    }
                });
            });
        });
    }

    // ---- the header chip ----------------------------------------------------------

    /// The puzzle chip: plugins on (`count`), a badge when one has trouble. `None` before
    /// the engine said, or when there are no plugins and no errors.
    pub fn chip(&mut self, ui: &mut egui::Ui, t: &Tokens, count: bool, selected: bool) -> Option<egui::Response> {
        let (on, trouble) = self.summary()?;
        let any = self.ext.as_ref().is_some_and(|e| !e.plugins.is_empty() || !e.errors.is_empty());
        if !any {
            return None;
        }
        let label = count.then(|| on.to_string());
        let resp = icons::icon_button(ui, t, icons::puzzle, label.as_deref(), selected);
        if trouble {
            let r = resp.rect;
            let c = egui::pos2(r.left() + 5.0 + t.text_size(16.0).min(r.height() - 2.0), r.top() + 5.0);
            ui.painter().circle(c, 3.5, t.danger, Stroke::new(1.5, t.panel_bg));
        }
        let tip = match (on, trouble) {
            (1, false) => "1 plugin on".to_owned(),
            (n, false) => format!("{n} plugins on"),
            (n, true) => format!("{n} plugins on; one has a problem"),
        };
        let resp = resp.on_hover_text(format!("{tip}\nClick for Settings → Plugins"));
        if resp.clicked() {
            self.open_on_trouble();
        }
        Some(resp)
    }
}

/// The id of a chat's plugin `make_tool` writes into by default.
pub const SCRATCH: &str = "scratch";
/// How the engine words a missing setting among a plugin's problems (shown as fields instead).
const NEEDS_SETTINGS: &str = "needs settings:";
/// The step name of the engine's "turn on this workspace's plugins?" approval card.
pub const TRUST_STEP: &str = "plugins";

/// An id to keep a plugin under: its own, or for the scratch plugin its first tool's name.
fn suggested_id(card: &PluginCard) -> String {
    if card.plugin != SCRATCH {
        return card.plugin.clone();
    }
    let from = card.tools.first().map_or("my-tools", String::as_str);
    let mut id: String = from.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    id = id.trim_matches('-').chars().take(48).collect();
    if byob_types::config::plugin_id_ok(&id) { id } else { "my-tools".into() }
}

/// A plugin with a problem worth a badge: loader problems, a failed tool or server.
fn has_trouble(p: &PluginInfo) -> bool {
    !p.problems.is_empty() || p.last_error.is_some() || p.servers.iter().any(|s| s.state == ServerState::Failed)
}

/// The settings level a switch at `level` is stored in (`None`: no such level here).
fn scope_for(level: Level, workspace: Option<&str>) -> Option<ConfigScope> {
    match level {
        Level::Default | Level::Global | Level::Flag => Some(ConfigScope::Global),
        Level::Workspace => workspace.map(|id| ConfigScope::Workspace { id: id.to_owned() }),
        Level::Chat => Some(ConfigScope::Chat),
    }
}

fn switch_tip(p: &PluginInfo, at: Option<&ConfigScope>) -> String {
    if let Some(by) = p.shadowed_by {
        return format!("The copy in {} is used instead", by.label());
    }
    let where_ = match at {
        Some(ConfigScope::Global) => "for every chat",
        Some(ConfigScope::Workspace { .. }) => "for this workspace's chats",
        Some(ConfigScope::Chat) => "for this chat only",
        None => return "Open a chat in its workspace to switch it there".into(),
    };
    format!("Switch {} {where_}", if p.enabled { "off" } else { "on" })
}

fn scope_heading(scope: PluginScope) -> &'static str {
    match scope {
        PluginScope::Chat => "THIS CHAT",
        PluginScope::Workspace => "THIS WORKSPACE",
        PluginScope::Global => "EVERYWHERE",
    }
}

fn origin_text(o: PluginOrigin) -> &'static str {
    match o {
        PluginOrigin::Bundled => "built in",
        PluginOrigin::User => "yours",
        PluginOrigin::Agent => "made by byob",
        PluginOrigin::Repo => "came with the folder",
    }
}

fn action_text(a: PluginAction) -> &'static str {
    match a {
        PluginAction::Made => "tool made",
        PluginAction::Edited => "edited",
        PluginAction::Kept => "kept",
        PluginAction::Removed => "removed",
    }
}

/// What a plugin adds, in one line: servers, skills, subagents, commands, the internet.
fn adds_text(p: &PluginInfo) -> String {
    let mut parts = Vec::new();
    let names = |what: &str, list: &[String], prefix: &str| {
        (!list.is_empty()).then(|| format!("{what}: {}", list.iter().map(|n| format!("{prefix}{n}")).collect::<Vec<_>>().join(", ")))
    };
    let servers: Vec<String> = p.servers.iter().map(|s| s.name.clone()).collect();
    parts.extend(names("servers", &servers, ""));
    parts.extend(names("tools", &p.tools, ""));
    parts.extend(names("skills", &p.skills, ""));
    parts.extend(names("subagents", &p.agents, ""));
    parts.extend(names("commands", &p.commands, "/"));
    if p.network {
        parts.push("uses the internet".into());
    }
    parts.join(" · ")
}

fn scope_badge(ui: &mut egui::Ui, t: &Tokens, scope: PluginScope) {
    egui::Frame::new().fill(mix(t.accent, t.code_bg, 0.8)).corner_radius(t.round(4.0)).inner_margin(vec2(t.pad(5.0), t.pad(1.0))).show(ui, |ui| {
        ui.label(RichText::new(scope.label()).small().color(t.accent));
    });
}

fn puzzle_icon(ui: &mut egui::Ui, t: &Tokens, size: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), egui::Sense::hover());
    icons::puzzle(ui.painter(), rect, t.accent);
}

/// "Tools: a, +b, −c".
fn tools_line(ui: &mut egui::Ui, t: &Tokens, card: &PluginCard) {
    if card.tools.is_empty() && card.removed.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(RichText::new("Tools").small().color(t.text_dim));
        for tool in &card.tools {
            let added = card.added.contains(tool);
            let text = if added { format!("+{tool}") } else { tool.clone() };
            ui.label(RichText::new(text).small().monospace().color(if added { t.success } else { t.text }));
        }
        for tool in &card.removed {
            ui.label(RichText::new(format!("−{tool}")).small().monospace().strikethrough().color(t.danger));
        }
    });
}

fn list_line(ui: &mut egui::Ui, t: &Tokens, what: &str, list: &[String], prefix: &str) {
    if list.is_empty() {
        return;
    }
    let names = list.iter().map(|n| format!("{prefix}{n}")).collect::<Vec<_>>().join(", ");
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(what).small().color(t.text_dim));
        ui.add(egui::Label::new(RichText::new(names).small().monospace().color(t.text)).wrap());
    });
}

fn code_block(ui: &mut egui::Ui, t: &Tokens, text: &str) {
    egui::Frame::new().fill(t.app_bg).stroke(Stroke::new(1.0, t.code_border)).corner_radius(t.round(4.0)).inner_margin(t.pad(6.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::ScrollArea::vertical().max_height(180.0).auto_shrink([false, true]).show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(text).small().monospace().color(t.code_text)).selectable(true).wrap());
        });
    });
}

#[cfg(test)]
#[path = "plugins_tests.rs"]
pub(crate) mod tests;
