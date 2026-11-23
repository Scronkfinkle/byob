//! Fluid in the app: the document kept in the prefs, edit mode, inserted elements and
//! their actions, and the model's changes (a `fluid` key in a look change).

use std::sync::Arc;

use byob_protocol::tree::Event;
use serde_json::Value;

use crate::fluid;
use crate::{ByobApp, theme};

/// Fluid's environment switches, read once at startup.
#[derive(Debug, Default)]
pub(crate) struct FluidEnv {
    /// `BYOB_NO_FLUID=1`: start with fluid off (no saved or plugin change drawn, no edit
    /// mode), the way out of a change that makes the window unusable. What's saved stays.
    off: bool,
    /// `BYOB_FLUID_EDIT=1` (or `=PATH`): start in edit mode, with that element selected.
    edit: Option<String>,
    /// `BYOB_FLUID_DOC=FILE`: start from these changes (`ui_fluid` ops, as JSON). They
    /// replace the saved ones, so only with `BYOB_HOME` set (a throwaway home).
    doc: Option<std::path::PathBuf>,
    /// `BYOB_FLUID_DUMP=FILE`: the elements written to FILE now and then.
    dump: Option<std::path::PathBuf>,
}

impl FluidEnv {
    pub(crate) fn read() -> Self {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
        let doc = var("BYOB_FLUID_DOC").map(std::path::PathBuf::from);
        if doc.is_some() && var("BYOB_HOME").is_none() {
            log::warn!("BYOB_FLUID_DOC is only read with BYOB_HOME set (it replaces the saved changes)");
        }
        Self {
            off: var("BYOB_NO_FLUID").is_some_and(|v| v != "0"),
            edit: var("BYOB_FLUID_EDIT").map(|v| v.to_string_lossy().into_owned()),
            doc: doc.filter(|_| var("BYOB_HOME").is_some()),
            dump: var("BYOB_FLUID_DUMP").map(std::path::PathBuf::from),
        }
    }
}

impl ByobApp {
    /// At the start of a frame: the plugin is installed and has the prefs' document (unless
    /// the window changed it and that isn't saved yet); F2 toggles edit mode.
    pub(crate) fn fluid_begin(&mut self, ctx: &egui::Context) {
        if self.fluid_env.off {
            return;
        }
        if ctx.plugin_opt::<fluid::Fluid>().is_none() {
            fluid::install(ctx);
            // For checking it headless: start editing, with an element selected.
            if let Some(sel) = &self.fluid_env.edit {
                fluid::with(ctx, |f| f.editing = true);
                self.fluid_editor.selected = Some(sel.clone()).filter(|s| s != "1");
            }
            // For checking it headless: start from these changes.
            if let Some(ops) = self.fluid_env.doc.as_ref().and_then(|f| std::fs::read_to_string(f).ok()).and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
                let mut doc = fluid::FluidDoc::default();
                for p in fluid::doc::apply(&mut doc, &ops, "you").problems {
                    log::warn!("BYOB_FLUID_DOC: {p}");
                }
                self.prefs.fluid = doc;
            }
        }
        fluid::edit::toggle_keys(ctx);
        let saved = self.fluid_saved;
        let doc = &self.prefs.fluid;
        // Plugins' changes: a layer under the user's, their inserts named after the plugin.
        let layers = self.plugins.fluid_layers();
        let base = (layers != self.fluid_layers).then(|| {
            let mut base = fluid::FluidDoc::default();
            for (id, ops) in &layers {
                let mut one = fluid::FluidDoc::default();
                let a = fluid::doc::apply(&mut one, ops, id);
                for p in a.problems {
                    log::warn!("plugin {id}: fluid: {p}");
                }
                base.rules.extend(one.rules);
                base.inserts.extend(one.inserts.into_iter().map(|mut i| {
                    i.id = format!("{id}.{}", i.id);
                    i
                }));
            }
            base
        });
        if base.is_some() {
            self.fluid_layers = layers;
        }
        fluid::with(ctx, |f| {
            if f.edited == saved && *f.doc != *doc {
                f.doc = Arc::new(doc.clone());
            }
            if let Some(b) = base {
                f.base = Arc::new(b);
            }
        });
    }

    /// At the end of a frame: inserted elements, edit mode, and what the window changed
    /// saved into the prefs.
    pub(crate) fn fluid_end(&mut self, ctx: &egui::Context) {
        let Some((doc, base, els, edited, dragging)) =
            // Plugins' inserts are left out with their rules (edit mode's Plugins' changes switch).
            fluid::with(ctx, |f| (Arc::clone(&f.doc), if f.ignore_base { Arc::default() } else { Arc::clone(&f.base) }, f.elements.clone(), f.edited, f.dragging.clone()))
        else {
            return;
        };
        let data = fluid::inserts::Data {
            chat: self.sidebar.current().map(|c| c.title.clone()).unwrap_or_default(),
            model: self.state.as_ref().map(|s| s.inference_model.clone()).unwrap_or_default(),
            project: self.sidebar.current_workspace().map(|w| w.name.clone()).unwrap_or_default(),
            messages: self.entries.len(),
            busy: self.busy(),
        };
        let t = self.tokens;
        let sites = &mut self.plugins.sites;
        let mut views = |ui: &mut egui::Ui, spec: &str| sites.draw_spec(ui, &t, spec);
        let inserts = fluid::doc::drawn_inserts(&base, &doc);
        let rules = |path: &str| fluid::doc::merged(&base, &doc, path);
        let events = fluid::inserts::show(
            ctx,
            &t,
            &inserts,
            &mut self.fluid_trees,
            &data,
            &fluid::inserts::Moves { els: &els, rules: &rules, dragging: dragging.as_deref() },
            &mut views,
        );
        for (id, e) in events {
            self.fluid_action(ctx, &id, &e);
        }
        if fluid::with(ctx, |f| f.editing).unwrap_or(false) {
            let out = self.fluid_editor.show(ctx, &t);
            if let Some(text) = out.ask {
                self.submit(text);
            }
        }
        // For checking it headless: the elements, written out now and then.
        if let Some(file) = &self.fluid_env.dump {
            // Listed even with nothing changed.
            fluid::with(ctx, fluid::Fluid::wake);
            if ctx.cumulative_pass_nr().is_multiple_of(3) {
                let editing = fluid::with(ctx, |f| f.editing).unwrap_or(false);
                let head = format!("editing: {editing} selected: {}\n", self.fluid_editor.selected.as_deref().unwrap_or("-"));
                let _ = std::fs::write(file, head + &fluid::inventory(&els, "", "", 5000));
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        let edited = fluid::with(ctx, |f| f.edited).unwrap_or(edited);
        if edited != self.fluid_saved {
            self.fluid_saved = edited;
            if let Some(doc) = fluid::with(ctx, |f| Arc::clone(&f.doc)) {
                self.prefs.fluid = (*doc).clone();
            }
        }
    }

    /// An inserted element's button (or input's submit) was used. Actions:
    /// `send:TEXT` sends a message, `draft:TEXT` puts it in the composer, `toggle:PATH`
    /// hides or shows an element, `remove` takes this element away, `fluid:{ops}` applies
    /// changes, `look:KEY=VALUE` changes the look; `{field}` in them is that input's text.
    pub(crate) fn fluid_action(&mut self, ctx: &egui::Context, id: &str, e: &Event) {
        // `{now}`: this moment, in seconds since 1970 (`set:start={now}` starts a stopwatch;
        // `set:end={now+1500}` a 25-minute timer).
        let now = chrono::Utc::now().timestamp();
        let mut action = e.action.clone();
        while let Some(i) = action.find("{now") {
            let Some(j) = action[i..].find('}').map(|j| i + j) else { break };
            let add = action.get(i + 4..j).unwrap_or_default().trim().trim_start_matches('+').trim().parse::<i64>().unwrap_or(0);
            action.replace_range(i..=j, &now.saturating_add(add).to_string());
        }
        for (k, v) in &e.values {
            let text = match v {
                Value::String(s) => s.clone(),
                v => v.to_string(),
            };
            action = action.replace(&format!("{{{k}}}"), &text);
        }
        let (verb, rest) = action.split_once(':').unwrap_or((action.as_str(), ""));
        let rest = rest.trim().to_owned();
        // The element's own state; several actions can go in one, separated by `;`.
        if ["set", "add", "drop", "flip", "inc"].contains(&verb.trim()) {
            fluid::with(ctx, |f| {
                let mut doc = (*f.doc).clone();
                let base = Arc::clone(&f.base);
                if let Some(ins) = doc.state_of_base(&base, id) {
                    for part in action.split(';') {
                        let (v, r) = part.split_once(':').unwrap_or((part, ""));
                        ins.act(v.trim(), r);
                    }
                }
                f.doc = Arc::new(doc);
                f.edited += 1;
            });
            // A field that was typed into and sent starts empty again.
            if let Some(st) = self.fluid_trees.get_mut(id) {
                let cleared: serde_json::Map<String, Value> =
                    e.values.iter().filter(|(_, v)| v.is_string()).map(|(k, _)| (k.clone(), Value::String(String::new()))).collect();
                st.replied(&cleared);
            }
            return;
        }
        match verb.trim() {
            "send" | "ask" if !rest.is_empty() => self.submit(rest),
            "draft" => self.set_draft(&rest),
            "remove" | "close" => self.fluid_change(ctx, &serde_json::json!({"remove": [id]})),
            "toggle" => {
                let hidden = fluid::with(ctx, |f| f.rule_of(&rest).is_some_and(|r| r.is_hidden())).unwrap_or(false);
                self.fluid_change(ctx, &serde_json::json!({"set": [{"path": rest, "hidden": !hidden}]}));
            }
            "fluid" => match serde_json::from_str::<Value>(&rest) {
                Ok(ops) => self.fluid_change(ctx, &ops),
                Err(err) => self.notice(crate::NoticeLevel::Error, format!("{id}: its fluid action isn't JSON: {err}")),
            },
            "look" => {
                if let Some((k, v)) = rest.split_once('=') {
                    let v = serde_json::from_str::<Value>(v.trim()).unwrap_or_else(|_| Value::String(v.trim().to_owned()));
                    let changes = [(k.trim().to_owned(), v)].into_iter().collect();
                    let mut prefs = self.prefs.clone();
                    let problems =
                        crate::look::change(&mut prefs, &mut self.look_history, &changes, self.tokens.dark, &self.user_themes, self.stage.available());
                    self.set_prefs(prefs);
                    for p in problems {
                        self.notice(crate::NoticeLevel::Error, p);
                    }
                }
            }
            _ => self.notice(crate::NoticeLevel::Error, format!("{id}: no action {action:?} (send:, draft:, toggle:, remove, fluid:, look:)")),
        }
    }

    /// Apply changes made in the window (not the model's): kept, undoable from edit mode.
    fn fluid_change(&mut self, ctx: &egui::Context, ops: &Value) {
        fluid::with(ctx, |f| {
            let mut doc = (*f.doc).clone();
            let _ = fluid::doc::apply(&mut doc, ops, "you");
            f.doc = Arc::new(doc);
            f.edited += 1;
        });
    }

    /// The model's `fluid` changes (from a look change): applied to `prefs`, their answer
    /// kept for the look report. What couldn't be done.
    pub(crate) fn fluid_from_model(&mut self, prefs: &mut theme::Prefs, ops: &Value) -> Vec<String> {
        if self.fluid_env.off {
            return vec!["fluid is off in this window (BYOB_NO_FLUID): nothing changes until byob starts without it".into()];
        }
        let a = fluid::doc::apply(&mut prefs.fluid, ops, "model");
        let inspect = a.inspect.clone();
        let mut done = a.done.join("\n");
        if done.is_empty() && inspect.is_none() {
            done = "nothing changed".into();
        }
        self.fluid_note = Some((done, inspect.unwrap_or_else(|| ("-".into(), String::new()))));
        a.problems
    }

    /// Can the look report go now? Not while the model's asked to see the elements and
    /// fluid (idle until then) hasn't listed them yet; that waits a second at most (a hidden
    /// window draws nothing).
    pub(crate) fn fluid_ready(&mut self, ctx: &egui::Context) -> bool {
        let wants_list = self.fluid_note.as_ref().is_some_and(|(_, (under, _))| under != "-");
        if !wants_list {
            self.fluid_wait = None;
            return true;
        }
        let fresh = fluid::with(ctx, |f| {
            f.wake();
            f.fresh()
        });
        let since = *self.fluid_wait.get_or_insert_with(web_time::Instant::now);
        if fresh != Some(false) || since.elapsed() > std::time::Duration::from_secs(1) {
            self.fluid_wait = None;
            return true;
        }
        ctx.request_repaint();
        false
    }

    /// The answer to the model's last fluid request, as a look note: what was done, and the
    /// elements it asked to see.
    pub(crate) fn fluid_report(&mut self, ctx: &egui::Context) -> Option<String> {
        let (done, (under, find)) = self.fluid_note.take()?;
        let mut out = String::new();
        if !done.is_empty() {
            out.push_str("Done:\n");
            out.push_str(&done);
            out.push_str("\n\n");
        }
        if under != "-" {
            let els = fluid::with(ctx, |f| f.elements.clone()).unwrap_or_default();
            let screen = ctx.content_rect();
            out.push_str(&format!(
                "Elements on screen ({:.0}×{:.0} window; paths, [role \"label\"], where drawn, and any change):\n",
                screen.width(),
                screen.height()
            ));
            out.push_str(&fluid::inventory(&els, &under, &find, 260));
            let views: Vec<String> = self.plugins.sites.declared().iter().map(|d| format!("{}/{} ({})", d.plugin, d.decl.id, d.decl.title)).collect();
            if !views.is_empty() {
                out.push_str(&format!("\nPlugin views you can insert (\"view\": …): {}\n", views.join(", ")));
            }
            let doc = &self.prefs.fluid;
            if !doc.scenes.is_empty() {
                out.push_str(&format!("\nSaved scenes (load one): {}\n", doc.scenes.keys().cloned().collect::<Vec<_>>().join(", ")));
            }
            if !doc.inserts.is_empty() {
                out.push_str("\nInserted elements:\n");
                for i in &doc.inserts {
                    out.push_str(&format!(
                        "- +{} ({} {}, by {})\n",
                        i.id,
                        i.at.name(),
                        if i.anchor.is_empty() { format!("{}%,{}%", i.x, i.y) } else { i.anchor.clone() },
                        i.by
                    ));
                }
            }
        }
        Some(out)
    }
}
