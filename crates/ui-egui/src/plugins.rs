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
    /// Show the settings' Plugins tab on the next frame.
    pub(crate) reveal: bool,
    /// Open the settings at the Plugins section.
    open_requested: bool,
    out: Vec<ClientMsg>,
    /// The Plugins page: its tab, the plugin open full page, byob's own abilities shown,
    /// and the logos plugins ship.
    page_tab: PageTab,
    detail: Option<String>,
    show_abilities: bool,
    /// The plugin whose settings are open inline in the list ("Set up").
    setup_open: Option<String>,
    /// The setup dialog is on its Advanced step.
    setup_advanced: bool,
    /// Secret fields shown in clear while setting up.
    revealed: BTreeSet<(String, String)>,
    logos: std::collections::HashMap<String, Logo>,
    /// The plugins' live views at render sites (pane, status items, settings sections, cards).
    pub sites: crate::sites::Sites,
}

impl PluginsUi {
    /// Take in a message meant for plugins; false if it isn't one.
    pub fn apply(&mut self, msg: &ServerMsg) -> bool {
        if self.sites.apply(msg) {
            return true;
        }
        let ServerMsg::Extensions(e) = msg else { return false };
        self.ext = Some((**e).clone());
        true
    }

    pub fn extensions(&self) -> Option<&Extensions> {
        self.ext.as_ref()
    }

    /// Prompt suggestions for an empty chat: the starters of switched-on plugins (at most 6).
    pub fn starters(&self) -> Vec<byob_protocol::StarterDecl> {
        let Some(e) = self.ext.as_ref() else { return Vec::new() };
        e.plugins.iter().filter(|p| p.enabled && p.in_use()).flat_map(|p| p.starters.iter().cloned()).take(6).collect()
    }

    /// What the composer's `/` menu offers.
    pub fn slash(&self) -> &[SlashItem] {
        self.ext.as_ref().map_or(&[], |e| e.slash.as_slice())
    }

    /// Messages for the engine since the last call.
    pub fn take(&mut self) -> Vec<ClientMsg> {
        let mut out = std::mem::take(&mut self.out);
        out.extend(self.sites.take());
        out
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

    /// (plugins on, any trouble): for the header chip. `None` before the engine said. The
    /// built-in plugins count only when one has trouble (they are always there).
    pub fn summary(&self) -> Option<(usize, bool)> {
        let e = self.ext.as_ref()?;
        let used = e.plugins.iter().filter(|p| p.shadowed_by.is_none());
        let on = used.clone().filter(|p| p.enabled && p.scope != PluginScope::Bundled).count();
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
                    (PluginScope::Workspace, "Keep in this project", "Copy it into the workspace's .byob/plugins: every chat there has it"),
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
            ui.label(RichText::new("Open a chat in this project to see its plugins.").small().color(t.text_dim));
            return;
        }
        let scopes: &[PluginScope] = match filter {
            Filter::All => &[PluginScope::Chat, PluginScope::Workspace, PluginScope::Global, PluginScope::Bundled],
            Filter::Workspace(_) => &[PluginScope::Workspace],
        };
        ui.horizontal_wrapped(|ui| {
            let used: Vec<&PluginInfo> = ext.plugins.iter().filter(|p| p.shadowed_by.is_none() && scopes.contains(&p.scope)).collect();
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
                let tip = if p.locked { "Part of byob's core: always on (fork it to change it)".to_owned() } else { switch_tip(p, switch_at.as_ref()) };
                if ui
                    .add_enabled(!shadowed && !p.locked && switch_at.is_some(), egui::Checkbox::without_text(&mut on))
                    .on_disabled_hover_text(&tip)
                    .on_hover_text(tip)
                    .changed()
                    && let Some(scope) = switch_at.clone()
                {
                    self.out.push(ClientMsg::SetPluginEnabled { scope, plugin: p.id.clone(), enabled: Some(on) });
                }
                let color = if shadowed { t.text_faint } else { t.text_strong };
                ui.label(RichText::new(&p.name).strong().color(color));
                ui.label(RichText::new(&p.version).small().color(t.text_faint));
                ui.label(RichText::new(origin_text(p.origin)).small().color(t.text_dim));
                if p.locked && !shadowed {
                    ui.label(RichText::new("always on").small().color(t.text_faint));
                }
                if let Some(by) = p.shadowed_by {
                    ui.label(RichText::new(format!("replaced by the copy in {}", by.label())).small().color(t.text_faint));
                } else if let Some(winner) = &p.superseded_by {
                    self.superseded_note(ui, t, p, winner, switch_at.as_ref());
                } else if p.origin == PluginOrigin::Repo && !p.trusted {
                    let text = if p.declined { "code off" } else { "waiting for your OK" };
                    ui.label(RichText::new(text).small().color(t.warning));
                } else if has_trouble(p) {
                    ui.label(RichText::new("⚠").color(t.warning)).on_hover_text("Has problems: open it for details");
                }
            })
            .body(|ui| self.plugin_details(ui, t, p, workspace));
    }

    /// On a switched-on plugin another one supersedes (both `provides` the same thing): which
    /// is used, and a button that switches over to this one.
    fn superseded_note(&mut self, ui: &mut egui::Ui, t: &Tokens, p: &PluginInfo, winner: &str, at: Option<&ConfigScope>) {
        let winner_name =
            self.ext.as_ref().and_then(|e| e.plugins.iter().find(|q| q.id == winner && q.in_use())).map_or_else(|| winner.to_owned(), |q| q.name.clone());
        ui.label(RichText::new(format!("not used: {winner_name} is")).small().color(t.warning)).on_hover_text(format!(
            "Both provide the same thing; only one is used. {winner_name} wins (a more specific scope, or switched on in settings, then by id)."
        ));
        if let Some(scope) = at.cloned()
            && ui.small_button("Use this one").on_hover_text(format!("Switch {winner_name} off and use {} instead", p.name)).clicked()
        {
            self.out.push(ClientMsg::SetPluginEnabled { scope: scope.clone(), plugin: winner.to_owned(), enabled: Some(false) });
            self.out.push(ClientMsg::SetPluginEnabled { scope, plugin: p.id.clone(), enabled: Some(true) });
        }
    }

    fn plugin_details(&mut self, ui: &mut egui::Ui, t: &Tokens, p: &PluginInfo, workspace: Option<&str>) {
        if !p.description.is_empty() {
            ui.add(egui::Label::new(RichText::new(&p.description).small().color(t.text)).wrap());
        }
        if p.shadowed_by.is_none() {
            self.sites.settings_sections(ui, t, &p.id);
        }
        // The switch and where it comes from.
        if p.locked {
            ui.label(RichText::new("Always on: part of byob's core. Fork it to change how it works.").small().color(t.text_dim));
        }
        if !p.locked {
            ui.horizontal_wrapped(|ui| {
                let state = if p.enabled { "On" } else { "Off" };
                let from = match p.enabled_from {
                    Level::Default => "nobody switched it".to_owned(),
                    Level::Global => "set for every chat".to_owned(),
                    Level::Workspace => "set in this project".to_owned(),
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
                    for (level, text) in [(Level::Chat, "in this chat"), (Level::Workspace, "in this project"), (Level::Global, "everywhere")] {
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
        }
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
        if let Some(b) = &p.backend {
            let color = match b.state.as_str() {
                "running" => t.success,
                "crashed" | "failed" => t.danger,
                _ => t.text_dim,
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Backend").small().color(t.text));
                ui.label(RichText::new(&b.state).small().color(color));
                if b.starts > 1 {
                    ui.label(RichText::new(format!("started {} times", b.starts)).small().color(t.text_faint));
                }
            });
            for e in b.error.iter().chain(&b.faults) {
                ui.add(egui::Label::new(RichText::new(e).small().color(t.danger)).wrap());
            }
            if !b.stderr.is_empty() {
                egui::CollapsingHeader::new(RichText::new("Backend output").small().color(t.text_dim)).id_salt(("backend-output", &p.id, &p.dir)).show(
                    ui,
                    |ui| {
                        ui.add(egui::Label::new(RichText::new(tail_lines(&b.stderr, 30)).small().monospace().color(t.text_dim)).wrap());
                    },
                );
            }
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
                    PluginScope::Chat => &[(PluginScope::Workspace, "Keep in this project"), (PluginScope::Global, "Keep everywhere")],
                    PluginScope::Workspace => &[(PluginScope::Global, "Keep everywhere")],
                    PluginScope::Global => &[],
                    // Fork to edit: a copy everywhere wins by id; its native parts stay.
                    PluginScope::Bundled => &[(PluginScope::Global, "Fork to edit")],
                };
                for (scope, text) in keep {
                    let tip = if p.scope == PluginScope::Bundled {
                        "Copy it to your plugins folder to change it (its prompt text, guidance, manifest); the copy is used instead of the built-in one"
                    } else {
                        "Move it there so later chats have it too"
                    };
                    if ui.small_button(*text).on_hover_text(tip).clicked() {
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
        // Locked plugins (byob's core) have no switch.
        let used: Vec<PluginInfo> = ext.plugins.iter().filter(|p| p.shadowed_by.is_none() && !p.locked).cloned().collect();
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
                    if ui.small_button("Reset").on_hover_text("Use the project's or global switch").clicked() {
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
                    let title = if pending.len() == 1 { "This project brings a plugin" } else { "This project brings plugins" };
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
        let any = trouble || self.ext.as_ref().is_some_and(|e| e.plugins.iter().any(|p| p.scope != PluginScope::Bundled) || !e.errors.is_empty());
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
/// The step name of the engine's "turn on this project's plugins?" approval card.
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
    !p.problems.is_empty()
        || p.last_error.is_some()
        || p.servers.iter().any(|s| s.state == ServerState::Failed)
        || p.backend.as_ref().is_some_and(|b| matches!(b.state.as_str(), "crashed" | "failed") || !b.faults.is_empty())
}

/// A setting it can't work without is still empty.
fn missing_required(p: &PluginInfo) -> bool {
    p.settings.iter().any(|s| s.required && !s.set)
}

/// The last `n` lines of a backend's output.
impl PluginsUi {
    /// The fluid changes of the plugins that are on (id, ops), for the screen.
    pub fn fluid_layers(&self) -> Vec<(String, serde_json::Value)> {
        let Some(ext) = &self.ext else { return Vec::new() };
        ext.plugins
            .iter()
            .filter(|p| p.enabled && p.shadowed_by.is_none() && p.superseded_by.is_none())
            .filter_map(|p| Some((p.id.clone(), p.fluid.clone()?)))
            .collect()
    }
}

fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    lines.get(lines.len().saturating_sub(n)..).unwrap_or_default().join("\n")
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
        Some(ConfigScope::Workspace { .. }) => "for this project's chats",
        Some(ConfigScope::Chat) => "for this chat only",
        None => return "Open a chat in its project to switch it there".into(),
    };
    format!("Switch {} {where_}", if p.enabled { "off" } else { "on" })
}

fn scope_heading(scope: PluginScope) -> &'static str {
    match scope {
        PluginScope::Chat => "THIS CHAT",
        PluginScope::Workspace => "THIS WORKSPACE",
        PluginScope::Global => "EVERYWHERE",
        PluginScope::Bundled => "BUILT IN",
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
        PluginAction::Other => "changed",
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

/// What the Plugins page asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageAction {
    /// A new chat with these first words ("Create a plugin that ", a plugin's prompt).
    Start(String),
    Close,
}

/// The page's tabs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PageTab {
    #[default]
    Plugins,
    Skills,
}

/// What the Add menu offers: a name, a line saying what it does, the first words of
/// the chat it starts (byob writes plugins on request).
const ADD: [(&str, &str, &str); 3] = [
    ("Create a plugin", "Describe what it should do; byob builds it", "Create a plugin that "),
    ("Connect an MCP server", "Give its address or command; byob wraps it as a plugin", "Connect this MCP server as a plugin: "),
    ("Add from a folder", "A plugin someone shared, or one you wrote", "Install the plugin in this folder: "),
];

/// A small on/off switch, `on` filled with the accent.
fn switch(ui: &mut egui::Ui, t: &Tokens, on: bool, enabled: bool) -> egui::Response {
    switch_in(ui, t, on, enabled, t.accent)
}

/// The switch, on in `color` (a tile's family colour).
fn switch_in(ui: &mut egui::Ui, t: &Tokens, on: bool, enabled: bool, color: egui::Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(34.0, 20.0), if enabled { egui::Sense::click() } else { egui::Sense::hover() });
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let track = if on { color } else { mix(t.border, t.text_faint, 0.3) };
        let track = if enabled { track } else { track.gamma_multiply(0.5) };
        p.rect_filled(rect, 10.0, track);
        let x = if on { rect.right() - 10.0 } else { rect.left() + 10.0 };
        p.circle_filled(egui::pos2(x, rect.center().y), 7.5, if on { egui::Color32::WHITE } else { t.field_bg });
    }
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// A shelf's heading: its name, and what's on it in a quieter voice beside it.
fn group_head(ui: &mut egui::Ui, t: &Tokens, title: &str, sub: &str) {
    ui.add_space(22.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        ui.label(RichText::new(title).strong().color(t.text_strong));
        ui.label(RichText::new(sub).small().color(t.text_faint));
    });
    ui.add_space(8.0);
}

/// Who made it, in words.
fn made_by(p: &PluginInfo) -> &'static str {
    match p.origin {
        PluginOrigin::Bundled => "byob",
        PluginOrigin::Agent => "byob, on your request",
        PluginOrigin::User => "you",
        PluginOrigin::Repo => "this project's folder",
    }
}

/// A section heading with its count ("Tools 4").
fn section_head(ui: &mut egui::Ui, t: &Tokens, title: &str, n: usize) {
    ui.add_space(18.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(t.text_size(16.0)).strong().color(t.text_strong));
        if n > 0 {
            ui.label(RichText::new(n.to_string()).size(t.text_size(16.0)).color(t.text_faint));
        }
    });
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), r.top() + 4.0, Stroke::new(1.0, t.separator));
    ui.add_space(10.0);
}

/// byob's own plugins people think of as connections: they reach outside byob.
const CONNECT: [&str; 3] = ["mail", "calendar", "web"];
/// byob's own plugins that make a kind of thing.
const MAKE: [&str; 6] = ["html-apps", "sheets", "charts", "markdown", "images", "math"];

/// byob's own plugins in a line people read (their full text stays on their page).
pub fn plain_line(p: &PluginInfo) -> &str {
    if p.scope != PluginScope::Bundled {
        return &p.description;
    }
    match p.id.as_str() {
        "mail" => "Read, search and send your email",
        "calendar" => "See your week, find free time, add events",
        "web" => "Search the web and read pages",
        "charts" => "Draw charts from numbers",
        "html-apps" => "Build small apps that run beside the chat",
        "sheets" => "Spreadsheets with formulas you can edit",
        "markdown" => "Write reports, summaries and notes",
        "images" => "Show and keep the images tools make",
        "math" => "Write math the way it looks in a book",
        "memory" => "Remember what you tell it, across chats",
        "ask-user" => "Ask you a question when it needs to",
        "routines" => "Do things on a schedule and tell you what it finds",
        "help" => "Answer questions about byob itself",
        "plan" => "Keep a short plan for longer tasks",
        "subagents" => "Hand a task to a helper with fresh context",
        "compaction" => "Shorten long chats so they keep going",
        "appearance" => "Change how byob looks when you ask",
        "environments" => "Run commands the way a project says",
        "workspace-instructions" => "Follow a project's own instructions",
        "core-tools" => "Read, search and change files in the folder",
        "artifacts" => "Keep what it makes beside the chat",
        "prompt" => "Put together what the model is told",
        _ => &p.description,
    }
}

/// Which group a plugin goes under on the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    NeedsSetup,
    Connections,
    Makes,
    Yours,
    /// byob's own abilities (asking you, help, files, memory…): folded at the bottom.
    Abilities,
}

/// The page column's width: up to 820 points, with room at the sides and for the scroll bar.
fn column_width(ui: &egui::Ui) -> f32 {
    (ui.available_width() - 12.0 - 48.0).clamp(240.0, 820.0)
}

/// Lay out `add` in a column `w` wide, centered in what's left of `ui` (however wide the
/// window, or zoomed out, it is), `top` below the top.
fn centered(ui: &mut egui::Ui, w: f32, top: f32, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(top);
    let extra = ((ui.available_width() - w) / 2.0).max(0.0);
    let rect = egui::Rect::from_min_size(ui.cursor().min + vec2(extra, 0.0), vec2(w, f32::INFINITY));
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::top_down(egui::Align::Min)), |ui| {
        ui.set_max_width(w);
        add(ui);
    });
    ui.add_space(32.0);
}

/// The line under the title: what byob is connected to and can make, now; what waits for
/// you, in the warning colour.
fn summary(all: &[PluginInfo], t: &Tokens, font: egui::FontId) -> egui::text::LayoutJob {
    let names = |v: &[&str]| match v {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    let ok: Vec<&str> =
        all.iter().filter(|p| family(p) == Group::Connections && p.enabled && group_of(p) != Group::NeedsSetup).map(|p| p.name.as_str()).collect();
    let makes = all.iter().filter(|p| family(p) == Group::Makes && p.enabled).count();
    let waiting: Vec<&str> = all.iter().filter(|p| group_of(p) == Group::NeedsSetup).map(|p| p.name.as_str()).collect();
    let mut text = if ok.is_empty() { "Not connected to anything yet".to_owned() } else { format!("Connected to {}", names(&ok)) };
    if makes > 0 {
        text.push_str(&format!(" · makes {makes} kinds of things"));
    }
    let mut job = egui::text::LayoutJob::default();
    job.append(&text, 0.0, egui::TextFormat::simple(font.clone(), t.text_dim));
    if !waiting.is_empty() {
        let verb = if waiting.len() == 1 { "is" } else { "are" };
        job.append(&format!(" · {} {verb} waiting for you", names(&waiting)), 0.0, egui::TextFormat::simple(font, t.warning));
    }
    job
}

/// The last slot on your board: make a plugin by saying what it should do.
fn make_tile(ui: &mut egui::Ui, t: &Tokens, r: egui::Rect) -> bool {
    let resp =
        ui.scope_builder(egui::UiBuilder::new().max_rect(r).id_salt("plugin-make-tile").sense(egui::Sense::click()), |ui| ui.allocate_space(r.size())).response;
    let tint = family_tint(t, Group::Yours);
    let paint = ui.painter();
    let radius = t.round(16.0);
    if resp.hovered() {
        paint.rect_filled(r, radius, tint.gamma_multiply(0.06));
    }
    dashed_outline(paint, r.shrink(0.5), radius, Stroke::new(1.2, if resp.hovered() { tint } else { t.border }));
    let c = r.center() - vec2(0.0, 18.0);
    paint.circle_filled(c, 16.0, tint.gamma_multiply(0.14));
    icons::plus(paint, egui::Rect::from_center_size(c, vec2(14.0, 14.0)), tint);
    paint.text(c + vec2(0.0, 30.0), egui::Align2::CENTER_CENTER, "Make your own", egui::TextStyle::Body.resolve(ui.style()), t.text_strong);
    paint.text(c + vec2(0.0, 50.0), egui::Align2::CENTER_CENTER, "Say what it should do", egui::TextStyle::Small.resolve(ui.style()), t.text_dim);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// A rounded outline in dashes: a slot that's still empty.
fn dashed_outline(p: &egui::Painter, r: egui::Rect, radius: f32, stroke: Stroke) {
    let mut pts = Vec::new();
    let corners = [
        (egui::pos2(r.right() - radius, r.top() + radius), -90.0_f32),
        (egui::pos2(r.right() - radius, r.bottom() - radius), 0.0),
        (egui::pos2(r.left() + radius, r.bottom() - radius), 90.0),
        (egui::pos2(r.left() + radius, r.top() + radius), 180.0),
    ];
    for (c, start) in corners {
        for i in 0..=6 {
            #[allow(clippy::cast_precision_loss)] // 0..=6
            let a = (start + 15.0 * i as f32).to_radians();
            pts.push(c + vec2(a.cos(), a.sin()) * radius);
        }
    }
    if let Some(first) = pts.first().copied() {
        pts.push(first);
    }
    p.extend(egui::Shape::dashed_line(&pts, stroke, 5.0, 4.0));
}

/// A label inside a sentence: "Username" → "username", but "API key" stays as it is.
fn in_sentence(label: &str) -> String {
    let mut c = label.chars();
    match (c.next(), c.next()) {
        (Some(f), Some(n)) if f.is_uppercase() && n.is_lowercase() => f.to_lowercase().chain(label.chars().skip(1)).collect(),
        _ => label.to_owned(),
    }
}

/// The family a plugin belongs to, whatever its state: what its colour and shelf are.
pub fn family(p: &PluginInfo) -> Group {
    if p.scope != PluginScope::Bundled {
        Group::Yours
    } else if CONNECT.contains(&p.id.as_str()) {
        Group::Connections
    } else if MAKE.contains(&p.id.as_str()) {
        Group::Makes
    } else {
        Group::Abilities
    }
}

/// A family's colour: sea for what byob connects to, clay for what it makes, violet for
/// yours, slate for what's built in. One colour per family, so the colour means something.
fn family_tint(t: &Tokens, g: Group) -> egui::Color32 {
    let (hue, sat) = match g {
        Group::Connections => (198.0, 0.78),
        Group::Makes => (24.0, 0.74),
        Group::Yours | Group::NeedsSetup => (262.0, 0.55),
        Group::Abilities => (215.0, 0.16),
    };
    let value = if t.dark { 0.88 } else { 0.56 };
    egui::ecolor::Hsva::new(hue / 360.0, sat, value, 1.0).into()
}

/// Where `p` goes on the page.
pub fn group_of(p: &PluginInfo) -> Group {
    let stuck = p.enabled && (has_trouble(p) || missing_required(p));
    if stuck && (p.scope != PluginScope::Bundled || CONNECT.contains(&p.id.as_str())) {
        return Group::NeedsSetup;
    }
    if p.scope != PluginScope::Bundled {
        return Group::Yours;
    }
    if CONNECT.contains(&p.id.as_str()) {
        Group::Connections
    } else if MAKE.contains(&p.id.as_str()) {
        Group::Makes
    } else {
        Group::Abilities
    }
}

/// A plugin's logo: asked for, its bytes, drawn, or none.
pub(crate) enum Logo {
    Asked,
    Bytes(Vec<u8>),
    Ready(egui::TextureHandle),
    None,
}

/// A plugin's mark on a rounded tile: its logo when it ships one, else its icon, else its
/// first letter.
fn plugin_icon(ui: &mut egui::Ui, t: &Tokens, p: &PluginInfo, size: f32, logo: Option<&egui::TextureHandle>) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), egui::Sense::click());
    let r = t.round(size * 0.24);
    if let Some(tex) = logo {
        ui.painter().rect_filled(rect, r, egui::Color32::WHITE);
        egui::Image::new((tex.id(), rect.size())).corner_radius(r).paint_at(ui, rect.shrink(size * 0.08));
        ui.painter().rect_stroke(rect, r, Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
        return resp;
    }
    // Its family's colour: a deep mark on a soft tile (light), a light one (dark).
    let tint = family_tint(t, family(p));
    let paint = ui.painter();
    paint.rect_filled(rect, r, tint.gamma_multiply(if t.dark { 0.2 } else { 0.14 }));
    match icons::lucide(&p.icon) {
        Some(draw) => draw(paint, egui::Rect::from_center_size(rect.center(), vec2(size * 0.52, size * 0.52)), tint),
        None => {
            let glyph: String = p.name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
            paint.text(rect.center(), egui::Align2::CENTER_CENTER, glyph, egui::FontId::proportional(size * 0.46), tint);
        }
    }
    resp
}

impl PluginsUi {
    /// Something on the Plugins page waits for you (an account to add, a problem).
    pub fn needs_attention(&self) -> bool {
        self.ext.as_ref().is_some_and(|e| e.plugins.iter().any(|p| p.shadowed_by.is_none() && group_of(p) == Group::NeedsSetup))
    }

    /// The plugin shown full page (`None`: the list).
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    /// Show `plugin` full page (or the list again with `None`).
    pub fn show_detail(&mut self, plugin: Option<&str>) {
        self.detail = plugin.map(str::to_owned);
    }

    /// The engine's answer to a logo request; true when it was one.
    pub fn apply_logo(&mut self, msg: &ServerMsg) -> bool {
        let ServerMsg::PluginLogo { plugin, data } = msg else { return false };
        let logo = data.as_deref().and_then(|d| byob_protocol::media::decode(d).ok()).map_or(Logo::None, Logo::Bytes);
        self.logos.insert(plugin.clone(), logo);
        true
    }

    /// `p`'s logo once it came (asks for it the first time).
    fn logo_for(&mut self, ctx: &egui::Context, p: &PluginInfo) -> Option<egui::TextureHandle> {
        if !p.logo {
            return None;
        }
        match self.logos.get(&p.id) {
            None => {
                self.out.push(ClientMsg::GetPluginLogo { plugin: p.id.clone() });
                self.logos.insert(p.id.clone(), Logo::Asked);
                None
            }
            Some(Logo::Bytes(bytes)) => {
                let tex =
                    crate::media::decode_image(bytes).ok().map(|img| ctx.load_texture(format!("plugin-logo-{}", p.id), img, egui::TextureOptions::LINEAR));
                let logo = tex.clone().map_or(Logo::None, Logo::Ready);
                self.logos.insert(p.id.clone(), logo);
                tex
            }
            Some(Logo::Ready(tex)) => Some(tex.clone()),
            Some(Logo::Asked | Logo::None) => None,
        }
    }

    fn icon(&mut self, ui: &mut egui::Ui, t: &Tokens, p: &PluginInfo, size: f32) -> egui::Response {
        let logo = self.logo_for(ui.ctx(), p);
        plugin_icon(ui, t, p, size, logo.as_ref())
    }

    /// The Plugins page, in place of the chat: a header with Add, a search field, tabs
    /// (Plugins, Skills), then one list in groups; a plugin's own page when one is open.
    pub fn page(&mut self, ui: &mut egui::Ui, t: &Tokens, query: &mut String) -> Option<PageAction> {
        // Esc closes an open menu first (not the page).
        if self.setup_open.is_none() && !egui::Popup::is_any_open(ui.ctx()) && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            if self.detail.take().is_some() {
                return None;
            }
            return Some(PageAction::Close);
        }
        let Some(ext) = self.ext.clone() else {
            ui.label(RichText::new("Waiting for the engine…").color(t.text_dim));
            return None;
        };
        // The setup dialog, over the list or a plugin's page.
        if let Some(id) = self.setup_open.clone() {
            match ext.plugins.iter().find(|p| p.id == id).cloned() {
                Some(p) => self.setup_modal(&ui.ctx().clone(), t, &p),
                None => self.setup_open = None,
            }
        }
        if let Some(id) = self.detail.clone() {
            match ext.plugins.iter().find(|p| p.id == id && p.shadowed_by.is_none()).cloned() {
                Some(p) => return self.detail_page(ui, t, &p, ext.workspace.as_deref()),
                None => self.detail = None,
            }
        }
        let mut action = None;
        let q = query.trim().to_lowercase();
        let all: Vec<PluginInfo> = ext.plugins.iter().filter(|p| p.shadowed_by.is_none()).cloned().collect();
        // One centered column, as wide as reads well.
        let page_w = column_width(ui);
        egui::ScrollArea::vertical().id_salt(("plugins-page", self.page_tab == PageTab::Skills)).auto_shrink([false, false]).show(ui, |ui| {
            centered(ui, page_w, 24.0, |ui| {
                // Title, what it is; Add at the right.
                let (row, _) = ui.allocate_exact_size(vec2(page_w, 56.0), egui::Sense::hover());
                let add_rect = egui::Rect::from_min_size(egui::pos2(row.right() - 92.0, row.top() + 6.0), vec2(92.0, 34.0));
                let add = ui.interact(add_rect, egui::Id::unique("plugins-add"), egui::Sense::click());
                {
                    let paint = ui.painter();
                    let fill = if add.hovered() { mix(t.text_strong, t.text_dim, 0.15) } else { t.text_strong };
                    paint.rect_filled(add_rect, 17.0, fill);
                    icons::plus(paint, egui::Rect::from_center_size(egui::pos2(add_rect.left() + 22.0, add_rect.center().y), vec2(13.0, 13.0)), t.app_bg);
                    paint.text(
                        egui::pos2(add_rect.left() + 34.0, add_rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        "Add",
                        egui::TextStyle::Body.resolve(ui.style()),
                        t.app_bg,
                    );
                    icons::chevron_down(
                        paint,
                        egui::Rect::from_center_size(egui::pos2(add_rect.right() - 16.0, add_rect.center().y), vec2(10.0, 10.0)),
                        t.app_bg,
                    );
                    paint.text(row.left_top(), egui::Align2::LEFT_TOP, "Plugins", egui::FontId::proportional(t.text_size(24.0)), t.text_strong);
                    let mut job = summary(&all, t, egui::TextStyle::Body.resolve(ui.style()));
                    job.wrap = egui::text::TextWrapping::truncate_at_width((page_w - 150.0).max(80.0));
                    paint.galley(row.left_top() + vec2(0.0, 34.0), paint.layout_job(job), t.text_dim);
                }
                let add = add.on_hover_cursor(egui::CursorIcon::PointingHand);
                crate::menu::below(&add, t, 300.0).show(|ui| {
                    crate::menu::tighten(ui);
                    for (name, what, words) in ADD {
                        if crate::chatbar::row_with_note(ui, t, name, what, false).clicked() {
                            action = Some(PageAction::Start(words.to_owned()));
                            ui.close();
                        }
                    }
                    crate::menu::separator(ui, t);
                    if crate::menu::item(ui, t, Some(icons::lines), "Reload plugins", None)
                        .on_hover_text("Read every plugin folder again; failed servers retry")
                        .clicked()
                    {
                        self.out.push(ClientMsg::ReloadPlugins);
                        ui.close();
                    }
                });
                ui.add_space(18.0);
                // Search: one wide field.
                let (field, _) = ui.allocate_exact_size(vec2(page_w, 40.0), egui::Sense::hover());
                ui.painter().rect_filled(field, 20.0, t.field_bg);
                ui.painter().rect_stroke(field, 20.0, Stroke::new(1.0, t.field_border), egui::StrokeKind::Inside);
                icons::search(ui.painter(), egui::Rect::from_center_size(egui::pos2(field.left() + 22.0, field.center().y), vec2(15.0, 15.0)), t.text_dim);
                let hint = if self.page_tab == PageTab::Skills { "Search skills" } else { "Search plugins" };
                let edit_rect = egui::Rect::from_min_max(egui::pos2(field.left() + 40.0, field.top()), egui::pos2(field.right() - 16.0, field.bottom()));
                ui.put(
                    edit_rect,
                    egui::TextEdit::singleline(query)
                        .frame(egui::Frame::NONE)
                        .hint_text(hint)
                        .align(egui::Align2::LEFT_CENTER)
                        .margin(egui::Margin::ZERO)
                        .desired_width(edit_rect.width()),
                );
                ui.add_space(14.0);
                // Tabs.
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 18.0;
                    for (k, name) in [(PageTab::Plugins, "Plugins"), (PageTab::Skills, "Skills")] {
                        let on = self.page_tab == k;
                        let r = ui.add(egui::Label::new(RichText::new(name).color(if on { t.text_strong } else { t.text_dim })).sense(egui::Sense::click()));
                        if on {
                            ui.painter().hline(r.rect.x_range(), r.rect.bottom() + 4.0, Stroke::new(2.0, t.text_strong));
                        }
                        if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            self.page_tab = k;
                        }
                    }
                });
                let r = ui.available_rect_before_wrap();
                ui.painter().hline(r.left()..=r.left() + page_w, r.top() + 6.0, Stroke::new(1.0, t.separator));
                ui.add_space(12.0);
                if self.page_tab == PageTab::Skills {
                    if let Some(a) = self.skills_list(ui, t, &all, &q, page_w) {
                        action = Some(a);
                    }
                    return;
                }
                let hit = |p: &PluginInfo| q.is_empty() || p.name.to_lowercase().contains(&q) || p.description.to_lowercase().contains(&q) || p.id.contains(&q);
                // A board per family: tiles you can switch right there; one that still needs
                // you is an empty slot, outlined and waiting; yours end with a slot to make one.
                let groups = [
                    (Group::Connections, "Connects to", "Your accounts and the web"),
                    (Group::Makes, "Makes", "What byob can create in a chat"),
                    (Group::Yours, "Made for you", "Added by you, or built by byob when you asked"),
                ];
                let mut any = false;
                let workspace = ext.workspace.clone();
                for (g, title, sub) in groups {
                    let list: Vec<&PluginInfo> = all.iter().filter(|p| family(p) == g && hit(p)).collect();
                    let ghost = g == Group::Yours && q.is_empty();
                    if list.is_empty() && !ghost {
                        continue;
                    }
                    any = true;
                    group_head(ui, t, title, sub);
                    if let Some(a) = self.board(ui, t, &list, page_w, workspace.as_deref(), ghost) {
                        action = Some(a);
                    }
                }
                // byob's own abilities: folded until asked for (always shown while searching).
                let own: Vec<&PluginInfo> = all.iter().filter(|p| family(p) == Group::Abilities && hit(p)).collect();
                if !own.is_empty() {
                    any = true;
                    ui.add_space(18.0);
                    let open = self.show_abilities || !q.is_empty();
                    let (rr, r) = ui.allocate_exact_size(vec2(page_w, 30.0), egui::Sense::click());
                    if r.hovered() {
                        crate::glass::row(ui.painter(), t, rr, crate::glass::RowState::Hovered);
                    }
                    icons::chevron(ui.painter(), egui::Rect::from_center_size(egui::pos2(rr.left() + 14.0, rr.center().y), vec2(10.0, 10.0)), open, t.text_dim);
                    ui.painter().text(
                        egui::pos2(rr.left() + 28.0, rr.center().y),
                        egui::Align2::LEFT_CENTER,
                        format!("Built into byob · {}", own.len()),
                        egui::TextStyle::Body.resolve(ui.style()),
                        t.text_dim,
                    );
                    let r = r.on_hover_text("How byob asks you, remembers, plans and finds its way. Always there; open one to see what it does");
                    if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        self.show_abilities = !self.show_abilities;
                    }
                    if open {
                        ui.add_space(6.0);
                        let ws = ext.workspace.clone();
                        if let Some(a) = self.board(ui, t, &own, page_w, ws.as_deref(), false) {
                            action = Some(a);
                        }
                    }
                }
                if !any {
                    ui.add_space(30.0);
                    ui.label(RichText::new(if q.is_empty() { "No plugins yet" } else { "Nothing matches" }).color(t.text_dim));
                }
            });
        });
        action
    }

    /// The Skills tab: every skill, the plugin it comes from; a click opens that plugin.
    fn skills_list(&mut self, ui: &mut egui::Ui, t: &Tokens, all: &[PluginInfo], q: &str, w: f32) -> Option<PageAction> {
        let skills: Vec<(PluginInfo, String)> = all
            .iter()
            .flat_map(|p| p.skills.iter().map(move |s| (p.clone(), s.clone())))
            .filter(|(p, s)| q.is_empty() || s.to_lowercase().contains(q) || p.name.to_lowercase().contains(q))
            .collect();
        if skills.is_empty() {
            let mut start = None;
            ui.add_space(36.0);
            ui.vertical_centered(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(48.0, 48.0), egui::Sense::hover());
                ui.painter().rect_filled(r, t.round(12.0), t.accent.gamma_multiply(0.12));
                icons::book_mark(ui.painter(), r.shrink(12.0), t.accent);
                ui.add_space(10.0);
                ui.label(RichText::new(if q.is_empty() { "No skills yet" } else { "Nothing matches" }).size(t.text_size(16.0)).strong().color(t.text_strong));
                ui.label(
                    RichText::new("A skill is know-how byob reads when a task needs it: how you like reports, how to plan a trip, your team's checklist.")
                        .color(t.text_dim),
                );
                ui.add_space(12.0);
                let b = ui.add(
                    egui::Button::new(RichText::new("Teach byob a skill").color(t.on_accent))
                        .fill(t.accent)
                        .corner_radius(t.round(16.0))
                        .min_size(vec2(170.0, 32.0)),
                );
                if b.clicked() {
                    start = Some(PageAction::Start("Create a plugin with a skill that teaches you how to ".into()));
                }
            });
            return start;
        }
        for (p, skill) in skills {
            let (rect, resp) = ui.allocate_exact_size(vec2(w, 60.0), egui::Sense::click());
            if resp.hovered() {
                crate::glass::row(ui.painter(), t, rect, crate::glass::RowState::Hovered);
            }
            let icon = egui::Rect::from_min_size(rect.min + vec2(8.0, 12.0), vec2(36.0, 36.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(icon), |ui| self.icon(ui, t, &p, 36.0));
            let x = icon.right() + 14.0;
            let paint = ui.painter();
            paint.text(egui::pos2(x, rect.top() + 20.0), egui::Align2::LEFT_CENTER, &skill, egui::TextStyle::Body.resolve(ui.style()), t.text_strong);
            paint.text(
                egui::pos2(x, rect.top() + 40.0),
                egui::Align2::LEFT_CENTER,
                format!("from {}", p.name),
                egui::TextStyle::Small.resolve(ui.style()),
                t.text_dim,
            );
            paint.hline(rect.left() + 8.0..=rect.right(), rect.bottom(), Stroke::new(1.0, t.separator));
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                self.detail = Some(p.id.clone());
            }
        }
        None
    }

    /// A family's board: tiles in as many columns as fit (one to three); for yours, a last
    /// empty slot to make one.
    fn board(&mut self, ui: &mut egui::Ui, t: &Tokens, list: &[&PluginInfo], w: f32, workspace: Option<&str>, ghost: bool) -> Option<PageAction> {
        let gap = 12.0;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)] // 1..=3
        let cols = (((w + gap) / (220.0 + gap)).floor() as usize).clamp(1, 3);
        #[allow(clippy::cast_precision_loss)] // 1..=3
        let tile_w = (w - gap * (cols as f32 - 1.0)) / cols as f32;
        let tile_h = 128.0;
        let n = list.len() + usize::from(ghost);
        let rows = n.div_ceil(cols);
        // Where the board goes; its space is taken after the tiles (each tile's scope would
        // otherwise move the cursor back up to its own bottom).
        #[allow(clippy::cast_precision_loss)] // a few rows
        let area = egui::Rect::from_min_size(ui.cursor().min, vec2(w, rows as f32 * (tile_h + gap) - gap));
        let mut action = None;
        for i in 0..n {
            #[allow(clippy::cast_precision_loss)] // a few tiles
            let at = area.min + vec2((i % cols) as f32 * (tile_w + gap), (i / cols) as f32 * (tile_h + gap));
            let r = egui::Rect::from_min_size(at, vec2(tile_w, tile_h));
            match list.get(i) {
                Some(p) => self.tile(ui, t, p, r, workspace),
                None => {
                    if make_tile(ui, t, r) {
                        action = Some(PageAction::Start("Create a plugin that ".into()));
                    }
                }
            }
        }
        ui.advance_cursor_after_rect(area);
        ui.add_space(4.0);
        action
    }

    /// One plugin as a tile: its mark, its switch, its name and what it does. One that needs
    /// you is an outlined slot with "Set up" where the switch goes. The tile opens its page.
    fn tile(&mut self, ui: &mut egui::Ui, t: &Tokens, p: &PluginInfo, r: egui::Rect, workspace: Option<&str>) {
        // The tile senses clicks as a scope, so its switch and button sit on top of it.
        let resp = ui
            .scope_builder(egui::UiBuilder::new().max_rect(r).id_salt(("plugin-tile", &p.id)).sense(egui::Sense::click()), |ui| ui.allocate_space(r.size()))
            .response;
        let waiting = group_of(p) == Group::NeedsSetup;
        let tint = family_tint(t, family(p));
        let radius = t.round(16.0);
        let hover = resp.hovered();
        {
            let paint = ui.painter();
            if waiting {
                paint.rect_filled(r, radius, t.warning.gamma_multiply(if hover { 0.1 } else { 0.05 }));
                dashed_outline(paint, r.shrink(0.5), radius, Stroke::new(1.2, t.warning.gamma_multiply(0.7)));
            } else {
                let fill = if t.dark { t.panel_bg } else { t.field_bg };
                paint.rect_filled(r, radius, fill);
                let edge = if hover { tint.gamma_multiply(0.6) } else { t.border };
                paint.rect_stroke(r, radius, Stroke::new(1.0, edge), egui::StrokeKind::Inside);
                if !p.enabled {
                    // Off: the tile goes quiet.
                    paint.rect_filled(r, radius, t.app_bg.gamma_multiply(0.45));
                }
            }
        }
        let pad = 16.0;
        let mark = egui::Rect::from_min_size(r.min + vec2(pad, pad), vec2(40.0, 40.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(mark), |ui| self.icon(ui, t, p, 40.0));
        // Top right: Set up, the switch, or nothing for what's always on.
        let corner = egui::Rect::from_min_max(egui::pos2(r.right() - pad - 90.0, r.top() + pad), egui::pos2(r.right() - pad, r.top() + pad + 30.0));
        let mut on_control = false;
        if waiting {
            ui.scope_builder(egui::UiBuilder::new().max_rect(corner).layout(egui::Layout::right_to_left(egui::Align::Center)), |ui| {
                let b = ui.add(
                    egui::Button::new(RichText::new("Set up").color(t.field_bg)).fill(t.text_strong).corner_radius(t.round(14.0)).min_size(vec2(72.0, 28.0)),
                );
                on_control = b.hovered();
                if b.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.setup_open = Some(p.id.clone());
                    self.setup_advanced = false;
                }
            });
        } else if !p.locked {
            let at = scope_for(p.enabled_from, workspace);
            let can = at.is_some();
            ui.scope_builder(egui::UiBuilder::new().max_rect(corner).layout(egui::Layout::right_to_left(egui::Align::Center)), |ui| {
                let sw = switch_in(ui, t, p.enabled, can, tint).on_hover_text(switch_tip(p, at.as_ref()));
                on_control = sw.hovered();
                if sw.clicked()
                    && let Some(scope) = at
                {
                    self.out.push(ClientMsg::SetPluginEnabled { scope, plugin: p.id.clone(), enabled: Some(!p.enabled) });
                }
            });
        }
        // Name, then what it does in up to two lines.
        let text_w = r.width() - 2.0 * pad;
        let paint = ui.painter();
        let mut name = egui::text::LayoutJob::simple_singleline(p.name.clone(), egui::TextStyle::Body.resolve(ui.style()), t.text_strong);
        name.wrap = egui::text::TextWrapping::truncate_at_width(text_w);
        let name = paint.layout_job(name);
        paint.galley(egui::pos2(r.left() + pad, r.top() + 66.0), name, t.text_strong);
        let line = if waiting {
            let missing: Vec<String> = p.settings.iter().filter(|s| s.required && !s.set).map(|s| in_sentence(&s.label)).collect();
            if missing.is_empty() { "Something's wrong: open it to see".to_owned() } else { format!("Needs your {}", missing.join(" and ")) }
        } else {
            let plain = plain_line(p);
            if plain.is_empty() { "No description".to_owned() } else { plain.to_owned() }
        };
        let color = if waiting { t.warning } else { t.text_dim };
        let mut what = egui::text::LayoutJob::simple(line, egui::TextStyle::Small.resolve(ui.style()), color, text_w);
        what.wrap.max_rows = 2;
        what.wrap.break_anywhere = false;
        what.wrap.overflow_character = Some('…');
        let what = paint.layout_job(what);
        paint.galley(egui::pos2(r.left() + pad, r.top() + 88.0), what, color);
        if !p.enabled && !waiting {
            paint.text(
                egui::pos2(r.right() - pad - 44.0, r.top() + pad + 15.0),
                egui::Align2::RIGHT_CENTER,
                "Off",
                egui::TextStyle::Small.resolve(ui.style()),
                t.text_faint,
            );
        }
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !on_control {
            self.detail = Some(p.id.clone());
        }
    }

    /// Setting a plugin up, as a dialog: only what it can't work without, each field with
    /// its help under it; everything else on an Advanced step of its own. One button saves.
    fn setup_modal(&mut self, ctx: &egui::Context, t: &Tokens, p: &PluginInfo) {
        let (mut basics, mut extra): (Vec<&PluginSetting>, Vec<&PluginSetting>) = p.settings.iter().partition(|s| s.required);
        if basics.is_empty() {
            // Nothing required: the first few up front, the rest behind Advanced.
            let n = extra.len().min(3);
            basics = extra.drain(..n).collect();
        }
        let advanced = self.setup_advanced && !extra.is_empty();
        let connect = family(p) == Group::Connections;
        let mut enter = false;
        let mut close = false;
        let w = 440.0;
        let resp =
            egui::Modal::new(egui::Id::unique(("plugin-setup", &p.id))).frame(crate::menu::frame(t).inner_margin(egui::Margin::same(0))).show(ctx, |ui| {
                ui.set_width(w);
                ui.spacing_mut().item_spacing.y = 0.0;
                // Head: its mark and what this is; on Advanced, the way back.
                egui::Frame::NONE.inner_margin(egui::Margin { left: 22, right: 22, top: 20, bottom: 14 }).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;
                        self.icon(ui, t, p, 40.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 3.0;
                            let title = if advanced {
                                "Advanced settings".to_owned()
                            } else if connect {
                                format!("Connect {}", p.name)
                            } else {
                                format!("Set up {}", p.name)
                            };
                            ui.label(RichText::new(title).size(t.text_size(17.0)).strong().color(t.text_strong));
                            let sub = if advanced { "Most people never change these. Leave one empty to keep its default." } else { plain_line(p) };
                            ui.label(RichText::new(sub).color(t.text_dim));
                        });
                    });
                });
                ui.painter().hline(ui.min_rect().x_range(), ui.cursor().top(), Stroke::new(1.0, t.separator));
                // The fields.
                egui::ScrollArea::vertical().max_height(380.0).auto_shrink([false, true]).show(ui, |ui| {
                    egui::Frame::NONE.inner_margin(egui::Margin { left: 22, right: 22, top: 16, bottom: 6 }).show(ui, |ui| {
                        let list = if advanced { &extra } else { &basics };
                        for s in list {
                            self.setup_field(ui, t, p, s, w - 44.0, !advanced, &mut enter);
                        }
                        if !advanced && !extra.is_empty() {
                            let (rr, r) = ui.allocate_exact_size(vec2(w - 44.0, 40.0), egui::Sense::click());
                            let fill = if r.hovered() { t.text_strong.gamma_multiply(0.06) } else { t.text_strong.gamma_multiply(0.03) };
                            ui.painter().rect_filled(rr, t.round(10.0), fill);
                            let font = egui::TextStyle::Body.resolve(ui.style());
                            ui.painter().text(
                                egui::pos2(rr.left() + 12.0, rr.center().y),
                                egui::Align2::LEFT_CENTER,
                                "Advanced settings",
                                font.clone(),
                                t.text_strong,
                            );
                            let changed = extra.iter().filter(|s| s.set).count();
                            let note = if changed > 0 { format!("{changed} changed") } else { format!("{} · usually fine as they are", extra.len()) };
                            ui.painter().text(
                                egui::pos2(rr.right() - 30.0, rr.center().y),
                                egui::Align2::RIGHT_CENTER,
                                note,
                                egui::TextStyle::Small.resolve(ui.style()),
                                t.text_faint,
                            );
                            icons::chevron_right(
                                ui.painter(),
                                egui::Rect::from_center_size(egui::pos2(rr.right() - 15.0, rr.center().y), vec2(10.0, 10.0)),
                                t.text_dim,
                            );
                            if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                self.setup_advanced = true;
                            }
                            ui.add_space(10.0);
                        }
                    });
                });
                ui.painter().hline(ui.min_rect().x_range(), ui.cursor().top(), Stroke::new(1.0, t.separator));
                // Foot: where secrets go; Back or Cancel, and the one button.
                let pending: Vec<(String, String)> = p
                    .settings
                    .iter()
                    .filter_map(|s| {
                        self.drafts.get(&(p.id.clone(), s.key.clone())).filter(|d| !d.trim().is_empty()).map(|d| (s.key.clone(), d.trim().to_owned()))
                    })
                    .collect();
                let missing: Option<&PluginSetting> = p.settings.iter().find(|s| s.required && !s.set && !pending.iter().any(|(k, _)| *k == s.key));
                egui::Frame::NONE.inner_margin(egui::Margin { left: 22, right: 22, top: 14, bottom: 16 }).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if p.settings.iter().any(|s| s.secret) {
                            let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), egui::Sense::hover());
                            icons::shield(ui.painter(), r, t.text_faint);
                            ui.label(RichText::new("Kept on this computer only").small().color(t.text_faint));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 10.0;
                            let ready = !pending.is_empty() && missing.is_none();
                            let label = if connect && !p.settings.iter().any(|s| s.set) { "Connect" } else { "Save" };
                            let b = ui
                                .add_enabled(
                                    ready,
                                    egui::Button::new(RichText::new(label).color(t.field_bg))
                                        .fill(t.text_strong)
                                        .corner_radius(t.round(15.0))
                                        .min_size(vec2(92.0, 32.0)),
                                )
                                .on_disabled_hover_text(missing.map_or_else(|| "Type in a field first".to_owned(), |m| format!("Fill in {} first", m.label)));
                            if (b.clicked() || enter) && ready {
                                for (k, v) in &pending {
                                    self.out.push(ClientMsg::SetPluginSetting { plugin: p.id.clone(), key: k.clone(), value: Some(v.clone()) });
                                    self.drafts.remove(&(p.id.clone(), k.clone()));
                                }
                                close = true;
                            }
                            let back = if advanced { "Back" } else { "Cancel" };
                            if ui
                                .add(egui::Button::new(RichText::new(back).color(t.text_dim)).frame(false))
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .clicked()
                            {
                                if advanced {
                                    self.setup_advanced = false;
                                } else {
                                    close = true;
                                }
                            }
                        });
                    });
                });
            });
        if close || resp.should_close() {
            self.setup_open = None;
            self.setup_advanced = false;
            self.revealed.retain(|(id, _)| *id != p.id);
        }
    }

    /// One field in the setup dialog: its name (and "Optional" up front when it is), the
    /// field (a secret hidden unless shown), what's saved, its help under it.
    #[allow(clippy::too_many_arguments)] // one call site, all of them needed
    fn setup_field(&mut self, ui: &mut egui::Ui, t: &Tokens, p: &PluginInfo, s: &PluginSetting, w: f32, mark_optional: bool, enter: &mut bool) {
        let key = (p.id.clone(), s.key.clone());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(RichText::new(&s.label).color(t.text_strong));
            if mark_optional && !s.required {
                ui.label(RichText::new("Optional").small().color(t.text_faint));
            }
            if s.secret {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let shown = self.revealed.contains(&key);
                    let r = ui.add(egui::Label::new(RichText::new(if shown { "Hide" } else { "Show" }).small().color(t.text_dim)).sense(egui::Sense::click()));
                    if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !self.revealed.remove(&key) {
                        self.revealed.insert(key.clone());
                    }
                });
            }
        });
        ui.add_space(6.0);
        let shown = self.revealed.contains(&key);
        let draft = self.drafts.entry(key.clone()).or_default();
        let hint = if s.set { "Saved · type to replace" } else { "" };
        let r = ui.add(egui::TextEdit::singleline(draft).password(s.secret && !shown).desired_width(w).hint_text(hint).margin(egui::Margin::symmetric(10, 8)));
        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            *enter = true;
        }
        let empty = draft.is_empty();
        ui.add_space(5.0);
        if s.set && empty {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(RichText::new("✓ Saved").small().color(t.success));
                if ui
                    .add(egui::Label::new(RichText::new("Clear").small().color(t.text_dim)).sense(egui::Sense::click()))
                    .on_hover_text("Forget this value")
                    .clicked()
                {
                    self.out.push(ClientMsg::SetPluginSetting { plugin: p.id.clone(), key: s.key.clone(), value: None });
                }
            });
        }
        if let Some(h) = &s.help {
            ui.add(egui::Label::new(RichText::new(h).small().color(t.text_faint)).wrap());
        }
        ui.add_space(14.0);
    }

    /// On a plugin's page: what's set and what isn't, at a glance; the dialog does the rest.
    fn settings_summary(&mut self, ui: &mut egui::Ui, t: &Tokens, p: &PluginInfo) {
        let shown: Vec<&PluginSetting> = {
            let req: Vec<&PluginSetting> = p.settings.iter().filter(|s| s.required || s.set).collect();
            if req.is_empty() { p.settings.iter().take(3).collect() } else { req }
        };
        egui::Frame::new().fill(t.panel_bg).stroke(Stroke::new(1.0, t.border)).corner_radius(t.round(12.0)).inner_margin(egui::Margin::symmetric(16, 12)).show(
            ui,
            |ui| {
                ui.set_width(ui.available_width());
                for s in &shown {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&s.label).color(t.text_strong));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if s.set {
                                ui.label(RichText::new("✓ Saved").small().color(t.success));
                            } else if s.required {
                                ui.label(RichText::new("Not set").small().color(t.warning));
                            } else {
                                ui.label(RichText::new("Default").small().color(t.text_faint));
                            }
                        });
                    });
                    ui.add_space(6.0);
                }
                let rest = p.settings.len().saturating_sub(shown.len());
                ui.horizontal(|ui| {
                    let any_set = p.settings.iter().any(|s| s.set);
                    let label = if any_set { "Change settings" } else { "Set up" };
                    let b = ui.add(
                        egui::Button::new(RichText::new(label).color(t.field_bg)).fill(t.text_strong).corner_radius(t.round(14.0)).min_size(vec2(120.0, 30.0)),
                    );
                    if b.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        self.setup_open = Some(p.id.clone());
                        self.setup_advanced = false;
                    }
                    if rest > 0 {
                        ui.label(RichText::new(format!("{rest} more in Advanced")).small().color(t.text_faint));
                    }
                });
            },
        );
    }

    /// A plugin's own page: who made it, its switch and "Try in chat", what to ask it, what
    /// it does, its settings, what it brings (tools, skills, commands), and its details.
    fn detail_page(&mut self, ui: &mut egui::Ui, t: &Tokens, p: &PluginInfo, workspace: Option<&str>) -> Option<PageAction> {
        let mut action = None;
        egui::ScrollArea::vertical().id_salt(("plugin-detail", &p.id)).auto_shrink([false, false]).show(ui, |ui| {
            // A readable column: up to 820 points, never wider than the window.
            let page_w = column_width(ui);
            centered(ui, page_w, 16.0, |ui| {
                if ui
                    .add(egui::Label::new(RichText::new("‹ Plugins").color(t.text_dim)).sense(egui::Sense::click()))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    self.detail = None;
                }
                ui.add_space(14.0);
                // Header: the actions at the right first, so a long name can't push them out.
                let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 68.0), egui::Sense::hover());
                let actions_left = ui
                    .scope_builder(egui::UiBuilder::new().max_rect(row).layout(egui::Layout::right_to_left(egui::Align::Center)), |ui| {
                        let try_words = p.starters.first().map_or_else(|| format!("Use {} to ", p.name), |s| s.prompt.clone());
                        let tr = ui.add(
                            egui::Button::new(RichText::new("Try in chat").color(t.field_bg))
                                .fill(t.text_strong)
                                .corner_radius(t.round(14.0))
                                .min_size(vec2(100.0, 30.0)),
                        );
                        if tr.clicked() {
                            action = Some(PageAction::Start(try_words));
                        }
                        ui.add_space(6.0);
                        let at = scope_for(p.enabled_from, workspace);
                        let can = !p.locked && at.is_some();
                        let r = switch(ui, t, p.enabled, can).on_hover_text(if p.locked {
                            "Part of byob's core: always on".to_owned()
                        } else {
                            switch_tip(p, at.as_ref())
                        });
                        ui.label(
                            RichText::new(if p.locked {
                                "Always on"
                            } else if p.enabled {
                                "On"
                            } else {
                                "Off"
                            })
                            .color(t.text_dim),
                        );
                        if r.clicked()
                            && let Some(scope) = at
                        {
                            self.out.push(ClientMsg::SetPluginEnabled { scope, plugin: p.id.clone(), enabled: Some(!p.enabled) });
                        }
                        ui.min_rect().left()
                    })
                    .inner;
                let left = egui::Rect::from_min_max(row.min, egui::pos2((actions_left - 12.0).max(row.left() + 80.0), row.bottom()));
                ui.scope_builder(egui::UiBuilder::new().max_rect(left).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
                    self.icon(ui, t, p, 64.0);
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.add_space(8.0);
                        ui.add(egui::Label::new(RichText::new(&p.name).size(t.text_size(22.0)).strong().color(t.text_strong)).truncate());
                        let mut place = p.scope.label().to_owned();
                        if let Some(c) = place.get_mut(0..1) {
                            c.make_ascii_uppercase();
                        }
                        ui.add(egui::Label::new(RichText::new(format!("By {} · {place} · v{}", made_by(p), p.version)).color(t.text_dim)).truncate());
                    });
                });
                // What's wrong, first.
                let problems: Vec<&String> = p.problems.iter().filter(|x| !x.starts_with(NEEDS_SETTINGS)).collect();
                if !problems.is_empty() || p.last_error.is_some() {
                    ui.add_space(12.0);
                    egui::Frame::new().stroke(Stroke::new(1.0, t.warning)).corner_radius(t.round(8.0)).inner_margin(10.0).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        for x in problems {
                            ui.add(egui::Label::new(RichText::new(x).color(t.warning)).wrap());
                        }
                        if let Some(e) = &p.last_error {
                            ui.add(egui::Label::new(RichText::new(format!("Last failure: {e}")).color(t.danger)).wrap());
                        }
                    });
                }
                // What to ask it.
                if !p.starters.is_empty() {
                    ui.add_space(16.0);
                    egui::Frame::new().fill(t.accent.gamma_multiply(0.10)).corner_radius(t.round(14.0)).inner_margin(egui::Margin::symmetric(16, 14)).show(
                        ui,
                        |ui| {
                            ui.set_width(ui.available_width());
                            for s in &p.starters {
                                let r = ui.add(
                                    egui::Button::new(RichText::new(format!("{}   →", s.title)).color(t.text_strong))
                                        .fill(t.panel_bg)
                                        .corner_radius(t.round(16.0))
                                        .min_size(vec2(0.0, 30.0)),
                                );
                                if r.on_hover_text(&s.prompt).clicked() {
                                    action = Some(PageAction::Start(s.prompt.clone()));
                                }
                            }
                        },
                    );
                }
                if !p.description.is_empty() {
                    ui.add_space(16.0);
                    ui.add(egui::Label::new(RichText::new(&p.description).color(t.text)).wrap());
                }
                // Its settings.
                if !p.settings.is_empty() || p.locked {
                    section_head(ui, t, "Settings", 0);
                    if p.locked {
                        ui.label(RichText::new("Always on: part of byob's core. Fork it to change how it works.").color(t.text_dim));
                    }
                    self.settings_summary(ui, t, p);
                    self.sites.settings_sections(ui, t, &p.id);
                }
                // What it brings.
                for (title, items, prefix) in [("Tools", &p.tools, ""), ("Skills", &p.skills, ""), ("Commands", &p.commands, "/"), ("Helpers", &p.agents, "")] {
                    if items.is_empty() {
                        continue;
                    }
                    section_head(ui, t, title, items.len());
                    for it in items {
                        ui.horizontal(|ui| {
                            ui.add_space(4.0);
                            ui.label(RichText::new(format!("{prefix}{it}")).color(t.text_strong));
                        });
                        ui.add_space(4.0);
                    }
                }
                if !p.servers.is_empty() {
                    section_head(ui, t, "Servers", p.servers.len());
                    for s in &p.servers {
                        let (state, color) = match s.state {
                            ServerState::Connecting => ("connecting".to_owned(), t.text_dim),
                            ServerState::Ready => (format!("ready · {} tools", s.tools), t.success),
                            ServerState::Failed => ("failed".to_owned(), t.danger),
                            ServerState::Off => ("off".to_owned(), t.text_faint),
                        };
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(&s.name).color(t.text_strong));
                            ui.label(RichText::new(state).color(color));
                        });
                        if let Some(e) = &s.error {
                            ui.add(egui::Label::new(RichText::new(e).small().color(t.text_dim)).wrap());
                        }
                    }
                }
                // Details.
                section_head(ui, t, "Information", 0);
                egui::Grid::new(("plugin-info", &p.id)).num_columns(2).spacing([40.0, 8.0]).show(ui, |ui| {
                    let row = |ui: &mut egui::Ui, k: &str, v: &str| {
                        ui.label(RichText::new(k).color(t.text_faint));
                        ui.label(RichText::new(v).color(t.text));
                        ui.end_row();
                    };
                    row(ui, "Made by", made_by(p));
                    row(ui, "Version", &p.version);
                    row(ui, "Where", p.scope.label());
                    row(ui, "Internet", if p.network { "its tools may use it" } else { "no" });
                    ui.label(RichText::new("Folder").color(t.text_faint));
                    ui.add(egui::Label::new(RichText::new(&p.dir).monospace().small().color(t.text_dim)).selectable(true).wrap());
                    ui.end_row();
                });
                ui.add_space(12.0);
                self.plugin_actions(ui, t, p, workspace);
            });
        });
        action
    }
}

#[cfg(test)]
#[path = "plugins_tests.rs"]
pub(crate) mod tests;
