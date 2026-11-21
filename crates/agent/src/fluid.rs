//! `ui_fluid` (experimental): any element in the byob window showing this chat, found by its
//! path, moved, scaled, restyled, retitled or hidden, and new elements put beside any of
//! them. Sent to the window as a `fluid` look change; the window answers in a look note.

use std::path::Path;
use std::sync::mpsc::Sender;

use byob_tools::{Access, Tool, ToolContext, ToolOutput, ToolSpec};
use serde_json::{Value, json};

use crate::Input;
use crate::look::{NO_WINDOW, NOT_DRAWING, request};

pub const TOOL: &str = "ui_fluid";

pub(crate) struct UiFluid {
    pub inbox: Sender<Input>,
}

const OPS: [&str; 6] = ["set", "clear", "insert", "remove", "reset", "inspect"];

impl Tool for UiFluid {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TOOL.into(),
            description: "Change ANY element of the byob window showing this chat, however small: a button, a label, a row, a message, the whole sidebar. Every element has a path (`composer/button:Send`, `sidebar/text#3`, `header`); inspect lists them with their role, label and where they're drawn. `set` moves (dx/dy in points, right/down), scales, fades, recolors, retitles (text), resizes text (size), backs (fill, outline, radius), animates (motion) or hides them; paths may be globs (`**/button:*`, `sidebar/*/text#1`). `insert` adds new elements (a view tree) after/before/above/below/over any element, or anywhere (at: free, x/y in %), and their buttons can act (send:TEXT sends a message, draft:TEXT, toggle:PATH, remove, fluid:{ops}, look:key=value). Templates in their text update live: {time} {clock} {date} {chat} {model} {project} {messages} {status}. Changes animate in. Without arguments: inspect everything.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "inspect": {"type": "object", "description": "List elements: {\"under\": path or glob (default all), \"find\": text in a path or label}. Do this first to get paths.", "properties": {"under": {"type": "string"}, "find": {"type": "string"}}},
                    "set": {"type": "array", "description": "Changes, each {\"path\": …, and any of: dx, dy (points; or left/up), scale (0.2-6), opacity (0-1), hidden (bool), text, size (text factor), color, fill, outline (#rrggbb or a name), radius, motion (float, bounce, pulse, shake, glow, none)}. null clears a field. \"Over a bit\" is about 8-16 points.", "items": {"type": "object"}},
                    "clear": {"type": "array", "description": "Paths (or globs) to put back as byob draws them.", "items": {"type": "string"}},
                    "insert": {"type": "array", "description": "New elements: {\"id\", \"anchor\": path, \"at\": after|before|above|below|over|free, \"x\"/\"y\" (% of the window, for free), \"width\" (points), \"card\" (default true), \"tree\": a view tree: {\"type\": column|row|card|text|markdown|chip|button|input|toggle|select|progress|separator|chart, …}; text has \"text\", buttons \"label\" and \"action\", inputs \"name\" and \"submit\" (an action; {name} in it is the input's text)}. Same id replaces.", "items": {"type": "object"}},
                    "remove": {"type": "array", "description": "Ids of inserted elements to take away (`*` matches any).", "items": {"type": "string"}},
                    "reset": {"type": "boolean", "description": "Every element back as byob draws it and every inserted one gone, first."},
                    "undo": {"type": "boolean", "description": "Put back the window from before the last change (fluid or look)."}
                }
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        if args.get("undo").and_then(Value::as_bool) == Some(true) {
            return "Fluid: undid the last change".into();
        }
        let n = |k: &str| args.get(k).and_then(Value::as_array).map_or(0, Vec::len);
        let (set, ins, rm) = (n("set") + n("clear"), n("insert"), n("remove"));
        if set + ins + rm == 0 {
            return if args.get("reset").and_then(Value::as_bool) == Some(true) { "Fluid: reset".into() } else { "Looked at the screen's elements".into() };
        }
        let mut parts = Vec::new();
        if set > 0 {
            let first = args.get("set").and_then(|s| s.get(0)).and_then(|s| s.get("path")).and_then(Value::as_str).unwrap_or("elements");
            parts.push(if set == 1 { format!("changed {first}") } else { format!("changed {set} elements") });
        }
        if ins > 0 {
            parts.push(format!("added {ins}"));
        }
        if rm > 0 {
            parts.push(format!("removed {rm}"));
        }
        format!("Fluid: {}", parts.join(", "))
    }

    fn pending_title(&self, args: &Value) -> String {
        if OPS.iter().filter(|k| **k != "inspect").any(|k| args.get(*k).is_some()) || args.get("undo").is_some() {
            "Change the screen".into()
        } else {
            "Look at the screen".into()
        }
    }

    fn preview(&self, _args: &Value, _workspace: &Path) -> Option<byob_tools::Preview> {
        None
    }

    /// Only how this window looks changes, at once and visibly, and F2 → Reset all puts it
    /// back: nothing to approve.
    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn repeatable(&self, _args: &Value) -> bool {
        true
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let Some(chat) = ctx.chat() else { return ToolOutput::err(NO_WINDOW) };
        let mut changes = byob_protocol::look::LookChanges::new();
        if args.get("undo").and_then(Value::as_bool) == Some(true) {
            changes.insert("undo".into(), Value::Bool(true));
        } else {
            let mut ops = serde_json::Map::new();
            for k in OPS {
                if let Some(v) = args.get(k).filter(|v| !v.is_null()) {
                    ops.insert(k.into(), v.clone());
                }
            }
            if ops.is_empty() {
                ops.insert("inspect".into(), json!({}));
            }
            changes.insert("fluid".into(), Value::Object(ops));
        }
        let after = match request(&self.inbox, chat, Some(changes), ctx.cancel_flag()) {
            Ok((_, Some(a), _)) => a,
            Ok((_, None, _)) => return ToolOutput::err(NOT_DRAWING),
            Err(e) => return ToolOutput::err(e),
        };
        let mut out = after.notes.get("fluid").cloned().unwrap_or_else(|| "Done.".into());
        if !after.problems.is_empty() {
            out.push_str("\nNot applied:\n");
            for p in &after.problems {
                out.push_str(&format!("- {p}\n"));
            }
        }
        ToolOutput::ok(out)
    }
}
