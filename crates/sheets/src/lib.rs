//! byob's spreadsheets: the bundled `sheets` plugin's backend, in-process, on the IronCalc
//! engine: tools to create, describe, read, write, fill, restructure and format sheets, with
//! lints, artifact versions, drafts and an editable grid view. The plugin's manifest
//! (`crates/bundled/plugins/sheets`) names it with `"backend": {"native": "sheets"}`; the app
//! registers [`factory`] under [`ID`].
//!
//! It implements [`byob_plugins::native::NativeBackend`], so the engine hosts it like a
//! process backend (its own thread, restarted after a panic).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod book;
pub mod refs;
pub mod store;
pub mod text;
pub mod tools;
pub mod view;

use std::sync::Arc;

use byob_plugins::native::{NativeBackend, NativeFactory, Outbox};
use serde_json::{Value, json};

use crate::store::Store;
use crate::text::repr_str;
use crate::tools::Ctx;
use crate::view::Views;

/// The id manifests name (`"backend": {"native": "sheets"}`).
pub const ID: &str = "sheets";
/// The artifact-site view it draws.
const VIEW: &str = "sheet";
/// Artifact views drawn again after changes (the newest ones).
const MAX_ARTIFACT_KEYS: usize = 16;

/// A problem the model can act on (its text is a plain sentence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SheetError {
    Msg(String),
    /// The engine refused (an IronCalc error).
    Engine(String),
    /// A file couldn't be read or written.
    Io(String),
}

impl SheetError {
    pub fn text(&self) -> String {
        match self {
            Self::Msg(m) => m.clone(),
            Self::Engine(e) => format!("The spreadsheet engine refused: {e}."),
            Self::Io(e) => format!("A file couldn't be read or written: {e}."),
        }
    }
}

/// The tools it serves, in the manifest's order.
pub const TOOLS: [&str; 7] = ["create_sheet", "describe_sheet", "read_cells", "write_cells", "fill", "edit_sheet", "format_cells"];

/// The sheets backend's state: workbooks, views, and the artifacts its views show.
#[derive(Default)]
pub struct Sheets {
    store: Store,
    views: Views,
    /// Keys of the drawn views with the artifact byob last said each shows (oldest first).
    drawn: Vec<(String, Value)>,
}

/// Makes a fresh backend (registered as [`ID`]).
pub fn factory() -> NativeFactory {
    Arc::new(|| Box::new(Sheets::default()) as Box<dyn NativeBackend>)
}

impl Sheets {
    /// A tool call: its result, or the error sentence.
    pub fn tool(&mut self, name: &str, args: &Value, workspace: &str, chat: Option<&str>) -> Result<Value, String> {
        let cx = Ctx { workspace, chat };
        let empty = json!({});
        let args = if args.is_object() { args } else { &empty };
        let st = &mut self.store;
        let r = match name {
            "create_sheet" => tools::create_sheet(st, &cx, args),
            "describe_sheet" => tools::describe_sheet(st, &cx, args),
            "read_cells" => tools::read_cells(st, &cx, args),
            "write_cells" => tools::write_cells(st, &cx, args),
            "fill" => tools::fill(st, &cx, args),
            "edit_sheet" => tools::edit_sheet(st, &cx, args),
            "format_cells" => tools::format_cells(st, &cx, args),
            other => return Err(format!("this backend has no tool {} (known: {})", repr_str(other), TOOLS.join(", "))),
        };
        r.map_err(|e| e.text())
    }

    /// Remember the artifact byob said view key `key` shows.
    fn remember(&mut self, key: &str, params: &Value) {
        let Some(a) = params.get("artifact").filter(|a| a.is_object()) else {
            if !self.drawn.iter().any(|(k, _)| k == key) {
                self.drawn.push((key.to_owned(), Value::Null));
            }
            return;
        };
        match self.drawn.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = a.clone(),
            None => self.drawn.push((key.to_owned(), a.clone())),
        }
        while self.drawn.iter().filter(|(_, a)| a.is_object()).count() > MAX_ARTIFACT_KEYS {
            if let Some(i) = self.drawn.iter().position(|(_, a)| a.is_object()) {
                self.drawn.remove(i);
            }
        }
    }

    fn artifact_of(&self, key: &str) -> Option<Value> {
        self.drawn.iter().find(|(k, _)| k == key).map(|(_, a)| a.clone()).filter(Value::is_object)
    }

    fn draw(&mut self, key: &str) -> Value {
        let a = self.artifact_of(key);
        self.views.draw(&mut self.store, a.as_ref())
    }

    /// Draw every view again and send it (after a tool call or a view event).
    fn refresh(&mut self, out: &Outbox) {
        let mut keys: Vec<String> = self.drawn.iter().map(|(k, _)| k.clone()).collect();
        keys.sort();
        for key in keys {
            let tree = self.draw(&key);
            out.view(VIEW, &key, tree);
        }
    }
}

impl NativeBackend for Sheets {
    fn event(&mut self, ev: &Value, out: &Outbox) {
        match ev.get("event").and_then(Value::as_str).unwrap_or_default() {
            "view" => {
                let view = ev.get("view").and_then(Value::as_str).unwrap_or_default();
                let key = ev.get("key").and_then(Value::as_str).unwrap_or_default().to_owned();
                if view != VIEW {
                    out.fault(&format!("view {view}: nothing handles its events"));
                    self.refresh(out);
                    return;
                }
                self.remember(&key, ev);
                let a = ev.get("artifact").filter(|a| a.is_object()).cloned().or_else(|| self.artifact_of(&key));
                if let Some(a) = a
                    && let Err(e) = self.views.event(&mut self.store, ev, &a, out)
                {
                    let action = ev.get("action").and_then(Value::as_str).unwrap_or_default();
                    out.fault(&format!("view {view}: the event handler failed on {}:\n{}", repr_str(action), e.text()));
                }
                self.refresh(out);
            }
            "client" => self.refresh(out),
            "web" => {
                out.fault(&format!("web view {}: nothing handles its messages", ev.get("view").map(text::repr).unwrap_or_default()));
                self.refresh(out);
            }
            _ => {}
        }
    }

    fn request(&mut self, method: &str, params: &Value, _out: &Outbox) -> Result<Value, String> {
        match method {
            "tool" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
                let ws = params.get("workspace").and_then(Value::as_str).unwrap_or_default();
                let chat = params.get("chat").and_then(Value::as_str);
                self.tool(name, params.get("args").unwrap_or(&Value::Null), ws, chat)
            }
            "view" => {
                let view = params.get("view").and_then(Value::as_str).unwrap_or_default();
                if view != VIEW {
                    return Err(format!("this backend draws no view {}", repr_str(view)));
                }
                let key = params.get("key").and_then(Value::as_str).unwrap_or_default().to_owned();
                self.remember(&key, params);
                Ok(self.draw(&key))
            }
            "hook" => Ok(json!({"action": "continue"})),
            other => Err(format!("unknown request {}", repr_str(other))),
        }
    }

    fn after_request(&mut self, method: &str, out: &Outbox) {
        if method == "tool" {
            self.refresh(out);
        }
    }

    fn shutdown(&mut self) {
        self.store.cleanup();
    }
}
