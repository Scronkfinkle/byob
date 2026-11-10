//! Property tests: random arguments and hostile paths never panic and never touch
//! anything outside the workspace.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use proptest::prelude::*;
use serde_json::{Value, json};

use crate::fs::testutil::run;
use crate::{FileChange, Registry, Tool};

/// Everything under `dir` (following no links): path → content (or "<dir>"/"<link>").
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let m = fs::symlink_metadata(&p).unwrap();
            let v = if m.file_type().is_symlink() {
                "<link>".to_string()
            } else if m.is_dir() {
                stack.push(p.clone());
                "<dir>".to_string()
            } else {
                fs::read_to_string(&p).unwrap_or_default()
            };
            out.insert(p, v);
        }
    }
    out
}

struct Sandbox {
    _tmp: tempfile::TempDir,
    ws: PathBuf,
    outside: PathBuf,
}

/// `<tmp>/ws` (the workspace) next to `<tmp>/outside`, with links from one to the other.
fn sandbox() -> Sandbox {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let ws = base.join("ws");
    let outside = base.join("outside");
    fs::create_dir_all(ws.join("docs/sub")).unwrap();
    fs::create_dir_all(outside.join("dir")).unwrap();
    fs::write(outside.join("secret.txt"), "SECRET").unwrap();
    fs::write(outside.join("dir/x"), "x").unwrap();
    fs::write(ws.join("a.txt"), "a").unwrap();
    fs::write(ws.join("docs/b.md"), "b").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(&outside, ws.join("out")).unwrap();
        symlink(outside.join("secret.txt"), ws.join("secret")).unwrap();
        symlink(outside.join("missing"), ws.join("dangling")).unwrap();
        symlink(ws.join("docs"), ws.join("docs_alias")).unwrap();
        symlink("../../outside", ws.join("docs/rel_out")).unwrap();
    }
    Sandbox { _tmp: tmp, ws, outside }
}

/// Paths built from segments that hit the interesting cases; `/x/ws` and `/x/outside`
/// are placeholders swapped for the real directories at run time.
fn hostile_path() -> impl Strategy<Value = String> {
    let seg = prop_oneof![
        3 => Just("..".to_string()),
        1 => Just(".".to_string()),
        1 => Just(String::new()),
        2 => Just("a.txt".to_string()),
        2 => Just("docs".to_string()),
        1 => Just("sub".to_string()),
        4 => Just("out".to_string()),
        3 => Just("secret".to_string()),
        3 => Just("dangling".to_string()),
        2 => Just("docs_alias".to_string()),
        3 => Just("rel_out".to_string()),
        2 => Just(".byob".to_string()),
        1 => Just("trash".to_string()),
        1 => Just("~".to_string()),
        3 => Just("new".to_string()),
        1 => Just("é\u{0}\n".to_string()),
        1 => "[a-z]{1,3}",
        1 => any::<String>(),
    ];
    let prefix = prop_oneof![
        6 => Just(String::new()),
        1 => Just("/".to_string()),
        1 => Just("/x/outside/".to_string()),
        1 => Just("/x/ws/".to_string()),
        1 => Just("/x/ws/../outside/".to_string()),
    ];
    let sep = prop_oneof![Just("/"), Just("\\"), Just("//")];
    (prefix, prop::collection::vec((seg, sep), 0..5)).prop_map(|(p, segs)| {
        let mut s = p;
        for (seg, sep) in segs {
            s.push_str(&seg);
            s.push_str(sep);
        }
        s.trim_end_matches('/').to_string()
    })
}

fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<f64>().prop_map(|f| serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)),
        any::<String>().prop_map(Value::from),
    ];
    leaf.prop_recursive(3, 16, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map("[a-z]{1,6}", inner, 0..4).prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

fn tools() -> Vec<Arc<dyn Tool>> {
    let reg: Registry = crate::builtin();
    reg.specs().iter().filter_map(|s| reg.get(&s.name).cloned()).collect()
}

fn change_paths(c: &FileChange) -> Vec<&PathBuf> {
    match c {
        FileChange::Created(p) | FileChange::Modified(p) | FileChange::Deleted(p) => vec![p],
        FileChange::Moved { from, to } => vec![from, to],
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn hostile_paths_stay_inside(paths in prop::collection::vec(hostile_path(), 4), content in "[a-z]{0,8}") {
        let sb = sandbox();
        let (out_dir, ws_dir) = (sb.outside.display().to_string(), sb.ws.display().to_string());
        let p: Vec<String> = paths.iter().map(|p| p.replace("/x/outside", &out_dir).replace("/x/ws", &ws_dir)).collect();
        let before = snapshot(&sb.outside);
        let calls = [
            json!({
                "path": p[0], "content": content, "from": p[1], "to": p[2], "depth": 4,
                "moves": [{"from": p[1], "to": p[2]}, {"from": p[3], "to": p[0]}], "folder": p[3],
            }),
            json!({"path": p[2], "content": content, "from": p[0], "to": p[3], "moves": [{"from": p[0], "to": p[1]}]}),
        ];
        for tool in tools() {
            for args in &calls {
                let _ = tool.title(args);
                let _ = tool.access(args);
                let out = run(tool.as_ref(), &sb.ws, args.clone());
                prop_assert!(!out.text.contains("SECRET"), "{}", out.text);
                prop_assert_eq!(&snapshot(&sb.outside), &before, "{} {}", tool.spec().name, args);
                for p in out.changes.iter().flat_map(change_paths) {
                    prop_assert!(p.starts_with(&sb.ws), "{} escaped", p.display());
                }
            }
        }
    }

    #[test]
    fn random_json_never_panics(args in json_value()) {
        let sb = sandbox();
        for tool in tools() {
            let _ = tool.title(&args);
            let _ = tool.access(&args);
            let _ = run(tool.as_ref(), &sb.ws, args.clone());
        }
    }

    #[test]
    fn random_file_contents_read_safely(data in prop::collection::vec(any::<u8>(), 0..20_000), offset in 0u64..50, limit in 0u64..50) {
        let sb = sandbox();
        fs::write(sb.ws.join("blob"), &data).unwrap();
        let out = run(&crate::ReadFile, &sb.ws, json!({"path": "blob", "offset": offset, "limit": limit}));
        prop_assert!(!out.failed, "{}", out.text);
    }
}
