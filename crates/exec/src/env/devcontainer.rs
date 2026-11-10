//! What a workspace's container is made from: its `.devcontainer/devcontainer.json` (or
//! `.devcontainer.json`), else the default image.
//!
//! Supported: `image`, `build.dockerfile` (+ `build.context`, `build.args`),
//! `containerEnv`, `remoteEnv`, `onCreateCommand` and `postCreateCommand` (string, array
//! or object; run once, as root, when the container is created). Everything else that
//! changes the container — `features`, `mounts`, `runArgs`, `forwardPorts`,
//! `dockerComposeFile`, `workspaceMount`/`workspaceFolder`, `remoteUser`/`containerUser`,
//! the other lifecycle commands — is ignored, and the model is told which ones.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Where devcontainer.json may live, in order.
pub const FILES: &[&str] = &[".devcontainer/devcontainer.json", ".devcontainer.json"];
/// Largest devcontainer.json / Dockerfile read.
const MAX_FILE: u64 = 1 << 20;

/// Keys byob reads, and keys that don't affect commands (editor settings).
const KNOWN: &[&str] = &["name", "image", "build", "containerEnv", "remoteEnv", "onCreateCommand", "postCreateCommand", "customizations", "$schema"];

/// The image to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Image {
    Pull(String),
    Build { dockerfile: PathBuf, context: PathBuf, args: BTreeMap<String, String> },
}

/// A lifecycle command: a shell line or an argument list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Shell(String),
    Args(Vec<String>),
}

/// How to make and use one workspace's container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub image: Image,
    /// Set on the container (`containerEnv`).
    pub env: Vec<(String, String)>,
    /// Set for each command (`remoteEnv`).
    pub remote_env: Vec<(String, String)>,
    /// Run once after creation, in order.
    pub on_create: Vec<Cmd>,
    /// devcontainer.json keys byob doesn't support.
    pub ignored: Vec<String>,
    /// The devcontainer.json it came from (workspace-relative), if any.
    pub file: Option<&'static str>,
}

impl Spec {
    /// The default container: `image`, nothing else.
    pub fn default_image(image: &str) -> Self {
        Self { image: Image::Pull(image.to_owned()), env: Vec::new(), remote_env: Vec::new(), on_create: Vec::new(), ignored: Vec::new(), file: None }
    }

    /// For workspace `ws`: its devcontainer.json if it has one, else `default_image`.
    pub fn for_workspace(ws: &Path, default_image: &str) -> Result<Self, String> {
        match find(ws) {
            None => Ok(Self::default_image(default_image)),
            Some(file) => {
                let path = ws.join(file);
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                if size > MAX_FILE {
                    return Err(format!("{file} is too large"));
                }
                let text = std::fs::read_to_string(&path).map_err(|e| format!("can't read {file}: {e}"))?;
                let base = path.parent().unwrap_or(ws);
                Self::parse(&text, base, ws).map(|mut s| {
                    s.file = Some(file);
                    s
                })
            }
        }
    }

    /// Parse devcontainer.json `text`; relative paths are against `base` (its folder).
    pub fn parse(text: &str, base: &Path, ws: &Path) -> Result<Self, String> {
        let v: Value = serde_json::from_str(&strip_jsonc(text)).map_err(|e| format!("devcontainer.json isn't valid JSON: {e}"))?;
        let obj = v.as_object().ok_or("devcontainer.json must be an object")?;
        let image = if let Some(b) = obj.get("build").and_then(Value::as_object).filter(|b| b.contains_key("dockerfile") || b.contains_key("dockerFile")) {
            let file = b.get("dockerfile").or_else(|| b.get("dockerFile")).and_then(Value::as_str).ok_or("build.dockerfile must be a string")?;
            let dockerfile = inside(base, file, ws)?;
            let context = inside(base, b.get("context").and_then(Value::as_str).unwrap_or("."), ws)?;
            let args = strings(b.get("args"));
            Image::Build { dockerfile, context, args: args.into_iter().collect() }
        } else if let Some(i) = obj.get("image").and_then(Value::as_str).filter(|i| valid_image(i)) {
            Image::Pull(i.to_owned())
        } else if obj.contains_key("dockerComposeFile") {
            return Err("devcontainer.json uses dockerComposeFile, which byob doesn't support (it needs `image` or `build.dockerfile`)".into());
        } else {
            return Err("devcontainer.json has no usable `image` or `build.dockerfile`".into());
        };
        let mut on_create = Vec::new();
        for key in ["onCreateCommand", "postCreateCommand"] {
            on_create.extend(commands(obj.get(key)));
        }
        let ignored = obj.keys().filter(|k| !KNOWN.contains(&k.as_str())).cloned().collect();
        Ok(Self { image, env: strings(obj.get("containerEnv")), remote_env: strings(obj.get("remoteEnv")), on_create, ignored, file: None })
    }

    /// The image name for messages ("python:3-slim", "built from .devcontainer/Dockerfile").
    pub fn describe(&self, ws: &Path) -> String {
        match &self.image {
            Image::Pull(i) => i.clone(),
            // Inside the workspace, written the way devcontainer.json writes paths (`/`).
            Image::Build { dockerfile, .. } => match dockerfile.strip_prefix(ws) {
                Ok(rel) => {
                    let parts: Vec<_> = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect();
                    format!("built from {}", parts.join("/"))
                }
                Err(_) => format!("built from {}", dockerfile.display()),
            },
        }
    }
}

/// The workspace's devcontainer.json, if it has one.
pub fn find(ws: &Path) -> Option<&'static str> {
    FILES.iter().copied().find(|f| ws.join(f).is_file())
}

/// An image reference: no options, spaces or shell characters.
pub fn valid_image(i: &str) -> bool {
    !i.is_empty() && i.len() <= 255 && !i.starts_with('-') && i.chars().all(|c| c.is_ascii_alphanumeric() || "._-/:@".contains(c))
}

/// `rel` against `base`, which must stay inside the workspace (no `..` escapes).
fn inside(base: &Path, rel: &str, ws: &Path) -> Result<PathBuf, String> {
    let mut p = base.to_path_buf();
    for part in Path::new(rel).components() {
        match part {
            std::path::Component::ParentDir => {
                p.pop();
            }
            std::path::Component::Normal(n) => p.push(n),
            std::path::Component::CurDir => {}
            _ => return Err(format!("{rel:?} must be a relative path")),
        }
    }
    if p.starts_with(ws) { Ok(p) } else { Err(format!("{rel:?} is outside the workspace")) }
}

/// A `{"KEY": "value"}` object as pairs (other values skipped; names checked).
fn strings(v: Option<&Value>) -> Vec<(String, String)> {
    v.and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
                .filter_map(|(k, v)| v.as_str().filter(|v| !v.contains('\0')).map(|v| (k.clone(), v.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

/// A lifecycle command: `"string"`, `["argv"]`, or `{"name": either}`.
fn commands(v: Option<&Value>) -> Vec<Cmd> {
    match v {
        Some(Value::String(s)) if !s.trim().is_empty() => vec![Cmd::Shell(s.clone())],
        Some(Value::Array(a)) => {
            let args: Vec<String> = a.iter().filter_map(Value::as_str).map(str::to_owned).collect();
            if args.is_empty() { Vec::new() } else { vec![Cmd::Args(args)] }
        }
        Some(Value::Object(o)) => o.values().flat_map(|v| commands(Some(v))).collect(),
        _ => Vec::new(),
    }
}

/// JSON with comments (`//`, `/* */`) and trailing commas, as devcontainer.json allows,
/// turned into plain JSON. Strings are left alone.
pub fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_str = false;
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    // Trailing commas: a comma followed (after whitespace) by } or ].
    let bytes: Vec<char> = out.chars().collect();
    let mut cleaned = String::with_capacity(out.len());
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        let Some(&c) = bytes.get(i) else { break };
        if in_str {
            cleaned.push(c);
            if c == '\\' {
                if let Some(n) = bytes.get(i + 1) {
                    cleaned.push(*n);
                    i += 1;
                }
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            cleaned.push(c);
        } else if c == ',' {
            let next = bytes.get(i + 1..).unwrap_or_default().iter().find(|c| !c.is_whitespace());
            if !matches!(next, Some('}' | ']')) {
                cleaned.push(c);
            }
        } else {
            cleaned.push(c);
        }
        i += 1;
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc_comments_and_trailing_commas() {
        let t = "{\n  // a comment\n  \"image\": \"x\", /* block */\n  \"url\": \"http://a//b\",\n  \"list\": [1, 2,],\n}\n";
        let v: Value = serde_json::from_str(&strip_jsonc(t)).unwrap();
        assert_eq!(v["image"], "x");
        assert_eq!(v["url"], "http://a//b");
        assert_eq!(v["list"], serde_json::json!([1, 2]));
        assert_eq!(strip_jsonc(r#"{"a": "x,}\"y"}"#), r#"{"a": "x,}\"y"}"#);
    }

    #[test]
    fn devcontainer_specs() {
        let ws = Path::new("/w");
        let base = Path::new("/w/.devcontainer");
        let s = Spec::parse(
            r#"{
              "name": "dev",
              "image": "mcr.microsoft.com/devcontainers/python:3.12",
              "features": {"ghcr.io/devcontainers/features/node:1": {}},
              "forwardPorts": [8000],
              "containerEnv": {"GREETING": "hi", "bad key": "x"},
              "remoteEnv": {"MODE": "dev"},
              "onCreateCommand": "echo one",
              "postCreateCommand": {"a": ["pip", "install", "-r", "requirements.txt"], "b": "echo two"},
              "customizations": {"vscode": {}},
            }"#,
            base,
            ws,
        )
        .unwrap();
        assert_eq!(s.image, Image::Pull("mcr.microsoft.com/devcontainers/python:3.12".into()));
        assert_eq!(s.env, vec![("GREETING".to_owned(), "hi".to_owned())]);
        assert_eq!(s.remote_env, vec![("MODE".to_owned(), "dev".to_owned())]);
        assert_eq!(s.on_create.len(), 3);
        assert_eq!(s.on_create[0], Cmd::Shell("echo one".into()));
        assert_eq!(s.ignored, vec!["features".to_owned(), "forwardPorts".to_owned()]);

        let b = Spec::parse(r#"{"build": {"dockerfile": "Dockerfile", "context": "..", "args": {"V": "1"}}}"#, base, ws).unwrap();
        let Image::Build { dockerfile, context, args } = b.image.clone() else { panic!("not a build") };
        assert_eq!((dockerfile, context), (PathBuf::from("/w/.devcontainer/Dockerfile"), PathBuf::from("/w")));
        assert_eq!(args.get("V").map(String::as_str), Some("1"));
        assert!(b.describe(ws).contains(".devcontainer/Dockerfile"));

        assert!(Spec::parse(r#"{"build": {"dockerfile": "../../etc/x"}}"#, base, ws).unwrap_err().contains("outside"));
        assert!(Spec::parse(r#"{"dockerComposeFile": "c.yml"}"#, base, ws).unwrap_err().contains("dockerComposeFile"));
        assert!(Spec::parse(r#"{"image": "--privileged"}"#, base, ws).is_err());
        assert!(Spec::parse("[]", base, ws).is_err() && Spec::parse("{", base, ws).is_err());
    }

    #[test]
    fn found_in_either_place() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(find(d.path()), None);
        assert_eq!(Spec::for_workspace(d.path(), "python:3-slim").unwrap(), Spec::default_image("python:3-slim"));
        std::fs::write(d.path().join(".devcontainer.json"), r#"{"image": "debian:stable-slim"}"#).unwrap();
        assert_eq!(find(d.path()), Some(".devcontainer.json"));
        std::fs::create_dir(d.path().join(".devcontainer")).unwrap();
        std::fs::write(d.path().join(".devcontainer/devcontainer.json"), r#"{"image": "alpine"}"#).unwrap();
        let s = Spec::for_workspace(d.path(), "x").unwrap();
        assert_eq!((s.image, s.file), (Image::Pull("alpine".into()), Some(".devcontainer/devcontainer.json")));
    }
}
