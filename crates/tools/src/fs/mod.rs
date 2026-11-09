//! Built-in file tools. Every path goes through [`crate::sandbox`]; output is bounded.

mod delete;
mod list;
mod moves;
mod read;
mod write;

#[cfg(test)]
mod fuzz;

use std::path::Path;

use serde_json::Value;

pub use delete::{DEFAULT_TRASH_DIR, DeleteFile};
pub use list::ListFiles;
pub use moves::{MoveFile, MoveFiles};
pub use read::ReadFile;
pub use write::WriteFile;

use crate::sandbox::{self, Resolved};

/// Human-readable size.
fn bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    // Precision loss above 2^52 bytes doesn't matter for a label.
    #[allow(clippy::cast_precision_loss)]
    let f = n as f64;
    if n < KB {
        format!("{n} B")
    } else if n < MB {
        format!("{:.1} KB", f / KB as f64)
    } else if n < GB {
        format!("{:.1} MB", f / MB as f64)
    } else {
        format!("{:.1} GB", f / GB as f64)
    }
}

/// A string argument from untrusted JSON, for titles.
fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// Optional string argument; wrong types are an error the model can read.
fn opt_string(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("invalid arguments: `{key}` must be a string")),
    }
}

/// Required string argument.
fn req_string(args: &Value, key: &str) -> Result<String, String> {
    opt_string(args, key)?.ok_or_else(|| format!("invalid arguments: missing `{key}`"))
}

/// Optional non-negative integer; small models sometimes send numbers as strings.
fn opt_u64(args: &Value, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_u64().map(Some).ok_or_else(|| format!("invalid arguments: `{key}` must be a whole number ≥ 0")),
        Some(Value::String(s)) => s.trim().parse().map(Some).map_err(|_| format!("invalid arguments: `{key}` must be a whole number ≥ 0")),
        Some(_) => Err(format!("invalid arguments: `{key}` must be a whole number ≥ 0")),
    }
}

/// Last path component of an untrusted string, cleaned up for a title.
fn short_name(p: &str) -> String {
    let p = p.trim().trim_end_matches(['/', '\\']);
    let name = Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string());
    let name = if name.is_empty() { "the workspace".to_string() } else { name };
    clip(&name, 60)
}

/// Strip control characters and cap at `max` characters.
fn clip(s: &str, max: usize) -> String {
    let mut out: String = s.chars().filter(|c| !c.is_control()).take(max).collect();
    if s.chars().filter(|c| !c.is_control()).count() > max {
        out.push('…');
    }
    out
}

/// Resolve a path for a tool that changes things: never the root, never `.yah/`.
fn resolve_mut(ctx_ws: &Path, p: &str) -> Result<Resolved, String> {
    let r = sandbox::resolve(ctx_ws, p)?;
    if r.is_root() {
        return Err("that is the whole workspace folder; name a file or folder inside it".into());
    }
    if r.is_reserved() {
        return Err(format!("{} is reserved for yah's own data", r.show()));
    }
    Ok(r)
}

/// True if anything is at `p`, including a broken symlink.
fn lexists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    use serde_json::Value;

    use crate::{Tool, ToolContext, ToolOutput};

    pub fn run(tool: &dyn Tool, ws: &Path, args: Value) -> ToolOutput {
        let cancel = AtomicBool::new(false);
        tool.run(&args, &ToolContext::new(ws, &cancel))
    }

    pub fn run_journaled(tool: &dyn Tool, ws: &Path, journal: &crate::Journal, args: Value) -> ToolOutput {
        let cancel = AtomicBool::new(false);
        tool.run(&args, &ToolContext::new(ws, &cancel).with_journal(journal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1536), "1.5 KB");
        assert_eq!(bytes(3 * 1024 * 1024), "3.0 MB");
        assert!(bytes(u64::MAX).ends_with(" GB"));
    }

    #[test]
    fn names_for_titles() {
        assert_eq!(short_name("a/b/notes.md"), "notes.md");
        assert_eq!(short_name("docs/"), "docs");
        assert_eq!(short_name(""), "the workspace");
        assert_eq!(short_name("a\nb"), "ab");
        assert_eq!(short_name(&"x".repeat(100)).chars().count(), 61);
    }
}
