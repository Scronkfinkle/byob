//! Helpers for writing plugins from inside byob (the model's `make_tool`, `edit_plugin`, …):
//!
//! - [`derive_schema`]: the JSON Schema of a Python `def run(arg: type = default, …)`, read
//!   from its type hints by a small Python shim that only parses the code (`ast`), never runs it;
//! - [`tool_script`]: the code plus a runner footer that calls `run()` with the JSON arguments
//!   from stdin and prints what it returns;
//! - [`upsert_tool`] / [`new_manifest`]: add or replace a tool in a plugin's `byob-plugin.json`,
//!   checked with the same parser (and error messages) the loader uses;
//! - [`fingerprint`]: a cheap "did anything change" hash of a plugin folder (sizes and times).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use byob_sandbox::{Sandbox, TempDir};
use serde_json::{Map, Value, json};

use crate::manifest::{MANIFEST, ParseOptions, parse_manifest_str, plugin_id_ok, setting_key_ok, tool_name_ok};
use crate::python::{ScriptRun, run_script_sandboxed, run_script_unsandboxed};

/// Marks where the runner footer starts in a tool script; everything from it on is byob's.
pub const RUNNER_MARK: &str = "# --- byob runner: calls run() with the JSON arguments on stdin. Keep this at the end. ---";

const RUNNER: &str = r#"
if __name__ == "__main__":
    import inspect as _inspect, json as _json, sys as _sys
    _raw = _sys.stdin.read().strip()
    _args = _json.loads(_raw) if _raw else {}
    if not isinstance(_args, dict):
        _args = {}
    _params = _inspect.signature(run).parameters
    if not any(p.kind == p.VAR_KEYWORD for p in _params.values()):
        _args = {k: v for k, v in _args.items() if k in _params}
    _out = run(**_args)
    if _out is None:
        pass
    elif isinstance(_out, str):
        print(_out)
    else:
        print(_json.dumps(_out, ensure_ascii=False, indent=1, default=str))
"#;

/// Reads `{"code": …}` on stdin, prints `{"params": schema, "doc": …}` or `{"error": …}`.
const SCHEMA_SHIM: &str = r#"
import ast, json, sys

SIMPLE = {"int": "integer", "float": "number", "str": "string", "bool": "boolean",
          "list": "array", "tuple": "array", "set": "array", "dict": "object",
          "List": "array", "Tuple": "array", "Set": "array", "Sequence": "array", "Dict": "object", "Mapping": "object"}

def name_of(n):
    if isinstance(n, ast.Name):
        return n.id
    if isinstance(n, ast.Attribute):
        return n.attr
    return None

def schema(n):
    if n is None:
        return None
    if isinstance(n, ast.Constant) and isinstance(n.value, str):
        try:
            return schema(ast.parse(n.value, mode="eval").body)
        except SyntaxError:
            return {"type": "string"}
    if isinstance(n, ast.Constant) and n.value is None:
        return {"type": "null"}
    if isinstance(n, ast.BinOp) and isinstance(n.op, ast.BitOr):
        parts = [schema(n.left), schema(n.right)]
        parts = [p for p in parts if p and p.get("type") != "null"]
        return parts[0] if len(parts) == 1 else ({"anyOf": parts} if parts else {})
    nm = name_of(n)
    if nm is not None:
        if nm in SIMPLE:
            return {"type": SIMPLE[nm]}
        return {}
    if isinstance(n, ast.Subscript):
        base = name_of(n.value)
        inner = n.slice
        args = list(inner.elts) if isinstance(inner, ast.Tuple) else [inner]
        if base in ("Optional",):
            return schema(args[0])
        if base == "Union":
            parts = [schema(a) for a in args]
            parts = [p for p in parts if p and p.get("type") != "null"]
            return parts[0] if len(parts) == 1 else ({"anyOf": parts} if parts else {})
        if base == "Literal":
            values = [a.value for a in args if isinstance(a, ast.Constant)]
            out = {"enum": values}
            kinds = {type(v).__name__ for v in values}
            if len(kinds) == 1 and kinds.pop() in SIMPLE:
                out["type"] = SIMPLE[type(values[0]).__name__]
            return out
        if base in ("list", "List", "Sequence", "set", "Set", "tuple", "Tuple"):
            item = schema(args[0]) if args else None
            return {"type": "array", "items": item} if item else {"type": "array"}
        if base in ("dict", "Dict", "Mapping"):
            return {"type": "object"}
        if base == "Annotated":
            out = schema(args[0]) or {}
            for a in args[1:]:
                if isinstance(a, ast.Constant) and isinstance(a.value, str):
                    out = dict(out, description=a.value)
            return out
    return {}

def from_default(v):
    for t, s in ((bool, "boolean"), (int, "integer"), (float, "number"), (str, "string"), (list, "array"), (dict, "object")):
        if isinstance(v, t):
            return {"type": s}
    return {}

def arg_docs(doc):
    out, on = {}, False
    for line in (doc or "").splitlines():
        s = line.strip()
        if s.lower().rstrip(":") in ("args", "arguments", "parameters", "params"):
            on = True
            continue
        if on and s and ":" in s:
            k, _, v = s.partition(":")
            k = k.split("(")[0].strip().lstrip("-* ").strip()
            if k.isidentifier() and v.strip():
                out[k] = v.strip()
        elif on and s and s.endswith(":"):
            on = False
    return out

def main():
    code = json.loads(sys.stdin.read()).get("code", "")
    try:
        tree = ast.parse(code)
    except SyntaxError as e:
        print(json.dumps({"error": "line %s: %s" % (e.lineno, e.msg)}))
        return
    fns = [n for n in tree.body if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef)) and n.name == "run"]
    if not fns:
        print(json.dumps({"error": "the code must define a top-level function: def run(arg: type = default, ...)"}))
        return
    fn = fns[-1]
    if isinstance(fn, ast.AsyncFunctionDef):
        print(json.dumps({"error": "run must be a plain def, not async def"}))
        return
    doc = ast.get_docstring(fn) or ""
    docs = arg_docs(doc)
    a = fn.args
    positional = list(a.posonlyargs) + list(a.args)
    defaults = [None] * (len(positional) - len(a.defaults)) + list(a.defaults)
    pairs = list(zip(positional, defaults)) + list(zip(a.kwonlyargs, a.kw_defaults))
    props, required = {}, []
    for arg, default in pairs:
        if arg.arg in ("self", "cls"):
            continue
        s = schema(arg.annotation)
        if default is not None:
            try:
                value = ast.literal_eval(default)
                if s in (None, {}):
                    s = from_default(value)
                if value is not None:
                    s = dict(s or {}, default=value)
            except Exception:
                pass
        else:
            required.append(arg.arg)
        s = dict(s or {"type": "string"})
        if arg.arg in docs and "description" not in s:
            s["description"] = docs[arg.arg]
        props[arg.arg] = s
    out = {"type": "object", "properties": props}
    if required:
        out["required"] = required
    summary = doc.strip().split("\n\n")[0].strip()
    print(json.dumps({"params": out, "doc": summary}))

main()
"#;

/// What [`derive_schema`] found.
#[derive(Debug, Clone, PartialEq)]
pub struct Derived {
    /// JSON Schema object of `run`'s arguments.
    pub params: Value,
    /// The first paragraph of `run`'s docstring (may be empty).
    pub doc: String,
}

/// Longest tool code accepted.
pub const MAX_CODE_BYTES: usize = 100_000;

/// The JSON Schema of `def run(…)` in `code`, from its type hints, defaults and docstring
/// (`Args:` lines become descriptions). The code is parsed, never executed. Runs `python`
/// in `sandbox` (no network) when given.
pub fn derive_schema(code: &str, python: &[OsString], sandbox: Option<&Sandbox>) -> Result<Derived, String> {
    if code.len() > MAX_CODE_BYTES {
        return Err(format!("the code is too long ({} KB; at most {} KB)", code.len() / 1024, MAX_CODE_BYTES / 1024));
    }
    let tmp = TempDir::new("byob-schema-")?;
    let script = tmp.path().join("byob_schema.py");
    fs::write(&script, SCHEMA_SHIM).map_err(|e| format!("couldn't write the schema helper: {e}"))?;
    let run = ScriptRun {
        program: python.to_vec(),
        script,
        args: Vec::new(),
        cwd: tmp.path().to_path_buf(),
        stdin: serde_json::to_vec(&json!({ "code": code })).unwrap_or_default(),
        env: BTreeMap::new(),
        timeout: Duration::from_secs(20),
        max_output: 256 * 1024,
    };
    let out = match sandbox {
        Some(sb) => run_script_sandboxed(&run, sb, tmp.path(), false, &|| false),
        None => run_script_unsandboxed(&run, &|| false),
    }
    .map_err(|e| format!("couldn't run Python to read the code: {e}"))?;
    if !out.success() {
        return Err(format!("couldn't read the code's signature: {}", last_lines(&out.stderr, 10)));
    }
    let v: Value = serde_json::from_str(out.stdout.trim()).map_err(|e| format!("couldn't read the code's signature ({e})"))?;
    if let Some(e) = v.get("error").and_then(Value::as_str) {
        return Err(e.to_owned());
    }
    let params = v.get("params").cloned().filter(Value::is_object).ok_or("couldn't read the code's signature")?;
    Ok(Derived { params, doc: v.get("doc").and_then(Value::as_str).unwrap_or("").to_owned() })
}

/// The last `n` lines of `text` (a traceback's useful end).
pub fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let start = lines.len().saturating_sub(n);
    lines.get(start..).unwrap_or_default().join("\n")
}

/// `code` (with any earlier runner footer removed) followed by the runner footer.
pub fn tool_script(code: &str) -> String {
    let own = code.split(RUNNER_MARK).next().unwrap_or(code).trim_end();
    format!("{own}\n\n\n{RUNNER_MARK}{RUNNER}")
}

/// The code part of a tool script (without byob's runner footer).
pub fn script_code(script: &str) -> &str {
    script.split(RUNNER_MARK).next().unwrap_or(script).trim_end()
}

/// A setting a tool asks for (`make_tool`'s `settings`): an environment variable name, and
/// whether its value is secret (an API key) — then it lives in secrets.json.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WantedSetting {
    pub key: String,
    pub label: String,
    pub secret: bool,
}

impl WantedSetting {
    /// `"API_KEY"` or `{"key": "API_KEY", "label": …, "secret": true}`. Names that look like
    /// credentials (KEY, TOKEN, SECRET, PASSWORD) are secret unless said otherwise.
    pub fn parse(v: &Value) -> Result<Self, String> {
        let (key, label, secret) = match v {
            Value::String(k) => (k.trim().to_owned(), None, None),
            Value::Object(o) => (
                o.get("key").and_then(Value::as_str).unwrap_or("").trim().to_owned(),
                o.get("label").and_then(Value::as_str).map(str::to_owned),
                o.get("secret").and_then(Value::as_bool),
            ),
            _ => return Err("each setting is a name like \"API_KEY\" or {\"key\": \"API_KEY\", \"secret\": true}".into()),
        };
        if !setting_key_ok(&key) {
            return Err(format!("setting {key:?} must be an environment variable name in capitals, like WEATHER_API_KEY"));
        }
        let looks_secret = ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| key.contains(w));
        let label = label.filter(|l| !l.trim().is_empty()).unwrap_or_else(|| key.replace('_', " ").to_lowercase());
        Ok(Self { secret: secret.unwrap_or(looks_secret), key, label })
    }
}

/// A tool to put in a manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDecl {
    pub name: String,
    pub description: String,
    pub params: Value,
    /// Script path inside the plugin, e.g. `tools/weather.py`.
    pub run: String,
    /// The plugin's tools may use the internet (`permissions.network`; never turned off here).
    pub network: bool,
    pub settings: Vec<WantedSetting>,
}

/// A fresh manifest for plugin `id`.
pub fn new_manifest(id: &str, name: &str, description: &str) -> Value {
    json!({ "id": id, "name": name, "version": "0.1.0", "description": description })
}

/// The manifest JSON in `dir` (a fresh one for `id` if there is none yet).
pub fn read_manifest_json(dir: &Path, id: &str) -> Result<Value, String> {
    let file = dir.join(MANIFEST);
    if !file.is_file() {
        return Ok(new_manifest(id, id, "Tools made in a chat"));
    }
    let text = fs::read_to_string(&file).map_err(|e| format!("{MANIFEST}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("{MANIFEST} isn't valid JSON: {e}"))
}

/// Check `manifest` with the loader's own rules and write it to `dir` (pretty-printed).
/// Nothing is written if it doesn't pass; the error is the loader's message.
pub fn write_manifest(dir: &Path, manifest: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(manifest).map_err(|e| e.to_string())?;
    parse_manifest_str(&text, ParseOptions { builtin: false, file: Some(MANIFEST) }).map_err(|e| e.to_string())?;
    fs::create_dir_all(dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    fs::write(dir.join(MANIFEST), text + "\n").map_err(|e| format!("couldn't write {MANIFEST}: {e}"))
}

/// Add `tool` to the manifest of plugin `id` in `dir` (replacing a tool with the same name),
/// with its settings and network permission, and write it. Returns whether it replaced one.
pub fn upsert_tool(dir: &Path, id: &str, tool: &ToolDecl) -> Result<bool, String> {
    if !plugin_id_ok(id) {
        return Err(format!("plugin {id:?}: ids are lowercase letters, digits and dashes, like \"trip-planner\""));
    }
    if !tool_name_ok(&tool.name) {
        return Err(format!("tool name {:?} must start with a letter and use only letters, digits and _ (at most 64)", tool.name));
    }
    let mut m = read_manifest_json(dir, id)?;
    let obj = m.as_object_mut().ok_or(format!("{MANIFEST} must be a JSON object"))?;
    let entry = json!({ "name": tool.name, "description": tool.description, "params": tool.params, "run": tool.run });
    let tools = obj.entry("tools").or_insert_with(|| Value::Array(Vec::new()));
    let list = tools.as_array_mut().ok_or(format!("{MANIFEST}: tools must be a list"))?;
    let replaced = match list.iter_mut().find(|t| t.get("name").and_then(Value::as_str) == Some(tool.name.as_str())) {
        Some(t) => {
            *t = entry;
            true
        }
        None => {
            list.push(entry);
            false
        }
    };
    if tool.network {
        let perms = obj.entry("permissions").or_insert_with(|| Value::Object(Map::new()));
        if let Some(p) = perms.as_object_mut() {
            p.insert("network".into(), Value::Bool(true));
        }
    }
    if !tool.settings.is_empty() {
        let settings = obj.entry("settings").or_insert_with(|| Value::Array(Vec::new()));
        let list = settings.as_array_mut().ok_or(format!("{MANIFEST}: settings must be a list"))?;
        for s in &tool.settings {
            if !list.iter().any(|x| x.get("key").and_then(Value::as_str) == Some(s.key.as_str())) {
                list.push(json!({ "key": s.key, "label": s.label, "secret": s.secret, "required": true }));
            }
        }
    }
    write_manifest(dir, &m)?;
    Ok(replaced)
}

/// Remove tool `name` from the manifest in `dir` (its script stays). Whether it was there.
pub fn remove_tool(dir: &Path, name: &str) -> Result<bool, String> {
    let file = dir.join(MANIFEST);
    if !file.is_file() {
        return Ok(false);
    }
    let mut m = read_manifest_json(dir, "x")?;
    let Some(list) = m.get_mut("tools").and_then(Value::as_array_mut) else { return Ok(false) };
    let before = list.len();
    list.retain(|t| t.get("name").and_then(Value::as_str) != Some(name));
    if list.len() == before {
        return Ok(false);
    }
    write_manifest(dir, &m)?;
    Ok(true)
}

/// Files a fingerprint looks at, at most.
const MAX_FINGERPRINT_FILES: usize = 2000;

/// A hash of the names, sizes and modification times of the files in `dir` (three levels
/// down; dot folders and `__pycache__` skipped). Changes when a manifest, SKILL.md, agent
/// file or script is edited, added or removed. Missing → a fixed value.
pub fn fingerprint(dir: &Path) -> u64 {
    let mut h = Fnv::default();
    fingerprint_into(&mut h, dir, 3);
    h.finish()
}

/// Add `dir`'s files to `h` (see [`fingerprint`]).
pub fn fingerprint_into(h: &mut Fnv, dir: &Path, depth: usize) {
    let mut stack: Vec<(PathBuf, usize)> = vec![(dir.to_path_buf(), 0)];
    let mut seen = 0usize;
    while let Some((d, level)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "__pycache__" {
                continue;
            }
            seen += 1;
            if seen > MAX_FINGERPRINT_FILES {
                return;
            }
            let Ok(meta) = e.metadata() else { continue };
            h.write(e.path().as_os_str().as_encoded_bytes());
            if meta.is_dir() {
                h.write_u8(1);
                if level < depth {
                    stack.push((e.path(), level + 1));
                }
            } else {
                h.write_u64(meta.len());
                let t = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
                h.write_u128(t);
            }
        }
    }
}

/// FNV-1a, 64 bit: stable across runs (unlike std's `DefaultHasher`).
#[derive(Debug, Clone, Copy)]
pub struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::load_plugin;

    fn python() -> Vec<OsString> {
        crate::python::default_python()
    }

    #[test]
    fn schemas_come_from_type_hints_defaults_and_docstrings() {
        let code = r#"
from typing import Literal, Optional
def run(city: str, days: int = 3, units: Literal["metric", "imperial"] = "metric", tags: list[str] = [], verbose=False, note: Optional[str] = None, ratio: "float" = 0.5):
    """Weather for a city.

    Args:
        city: The city name, e.g. Paris
        days (int): How many days
    """
    return city
"#;
        let d = derive_schema(code, &python(), None).unwrap();
        assert_eq!(d.doc, "Weather for a city.");
        let p = &d.params;
        assert_eq!(p["required"], json!(["city"]));
        assert_eq!(p["properties"]["city"], json!({"type": "string", "description": "The city name, e.g. Paris"}));
        assert_eq!(p["properties"]["days"], json!({"type": "integer", "default": 3, "description": "How many days"}));
        assert_eq!(p["properties"]["units"], json!({"enum": ["metric", "imperial"], "type": "string", "default": "metric"}));
        assert_eq!(p["properties"]["tags"], json!({"type": "array", "items": {"type": "string"}, "default": []}));
        assert_eq!(p["properties"]["verbose"], json!({"type": "boolean", "default": false}));
        assert_eq!(p["properties"]["note"], json!({"type": "string"}));
        assert_eq!(p["properties"]["ratio"], json!({"type": "number", "default": 0.5}));
    }

    #[test]
    fn bad_code_says_why_without_running_it() {
        let marker = std::env::temp_dir().join(format!("byob-schema-ran-{}", std::process::id()));
        let code = format!("open({:?}, 'w').write('ran')\ndef run(x: int):\n    return x\n", marker.display().to_string());
        assert!(derive_schema(&code, &python(), None).is_ok());
        assert!(!marker.exists(), "the code must only be parsed");
        let e = derive_schema("def run(x:\n", &python(), None).unwrap_err();
        assert!(e.starts_with("line 1"), "{e}");
        assert!(derive_schema("def other(): pass", &python(), None).unwrap_err().contains("def run("));
    }

    #[test]
    fn scripts_get_one_runner_and_run_with_json_arguments() {
        let code = "def run(a: int, b: int = 2):\n    return {'sum': a + b}\n";
        let script = tool_script(code);
        assert_eq!(tool_script(&script), script, "the footer is replaced, not stacked");
        assert_eq!(script_code(&script), code.trim_end());
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("t.py");
        fs::write(&file, &script).unwrap();
        let run = ScriptRun {
            program: python(),
            script: file,
            args: vec![],
            cwd: d.path().to_path_buf(),
            stdin: br#"{"a": 1, "extra": "ignored"}"#.to_vec(),
            env: BTreeMap::new(),
            timeout: Duration::from_secs(20),
            max_output: 10_000,
        };
        let out = run_script_unsandboxed(&run, &|| false).unwrap();
        assert!(out.success(), "{}", out.stderr);
        assert_eq!(serde_json::from_str::<Value>(&out.stdout).unwrap(), json!({"sum": 3}));
    }

    #[test]
    fn manifests_gain_and_replace_tools() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("scratch");
        let decl = ToolDecl {
            name: "weather".into(),
            description: "Weather".into(),
            params: json!({"type": "object", "properties": {}}),
            run: "tools/weather.py".into(),
            network: true,
            settings: vec![WantedSetting::parse(&json!("WEATHER_API_KEY")).unwrap()],
        };
        assert!(!upsert_tool(&dir, "scratch", &decl).unwrap());
        assert!(upsert_tool(&dir, "scratch", &ToolDecl { description: "Better".into(), network: false, ..decl.clone() }).unwrap());
        fs::create_dir_all(dir.join("tools")).unwrap();
        fs::write(dir.join("tools/weather.py"), "").unwrap();
        let p = load_plugin(&dir, false).unwrap().unwrap();
        assert_eq!(p.manifest.tools.len(), 1);
        assert_eq!(p.manifest.tools[0].description, "Better");
        assert!(p.manifest.permissions.network, "network is never turned off again");
        assert!(p.manifest.settings[0].secret && p.manifest.settings[0].key == "WEATHER_API_KEY");
        assert!(upsert_tool(&dir, "Bad Id", &decl).is_err());
        assert!(upsert_tool(&dir, "scratch", &ToolDecl { name: "no spaces".into(), ..decl.clone() }).is_err());
        // A manifest the loader would refuse is not written.
        let bad = ToolDecl { run: "../escape.py".into(), ..decl };
        assert!(upsert_tool(&dir, "scratch", &bad).unwrap_err().contains("byob-plugin.json"));
        assert!(remove_tool(&dir, "weather").unwrap());
        assert!(!remove_tool(&dir, "weather").unwrap());
    }

    #[test]
    fn settings_parse() {
        assert!(WantedSetting::parse(&json!("API_TOKEN")).unwrap().secret);
        assert!(!WantedSetting::parse(&json!("CITY")).unwrap().secret);
        assert!(!WantedSetting::parse(&json!({"key": "API_KEY", "secret": false})).unwrap().secret);
        assert!(WantedSetting::parse(&json!("lower")).is_err());
        assert!(WantedSetting::parse(&json!(3)).is_err());
    }

    #[test]
    fn fingerprints_follow_edits() {
        let d = tempfile::tempdir().unwrap();
        let empty = fingerprint(d.path());
        fs::write(d.path().join("a.py"), "x").unwrap();
        let one = fingerprint(d.path());
        assert_ne!(empty, one);
        assert_eq!(one, fingerprint(d.path()));
        fs::write(d.path().join("a.py"), "xy").unwrap();
        assert_ne!(one, fingerprint(d.path()));
        let two = fingerprint(d.path());
        fs::create_dir(d.path().join("__pycache__")).unwrap();
        fs::write(d.path().join("__pycache__/a.pyc"), "junk").unwrap();
        assert_eq!(two, fingerprint(d.path()), "caches don't count");
    }
}
