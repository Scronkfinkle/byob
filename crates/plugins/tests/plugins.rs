//! Discovery and tools over fixture plugin folders (`fixtures/builtin` holds built-in style
//! manifests, with `native` tools and long prompts).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use byob_plugins::manifest::ToolKind;
use byob_plugins::python::ScriptRun;
use byob_plugins::{
    Discovery, PluginRef, PluginSources, PythonTool, ToolNames, UseSkillTool, discover, load_plugin, python_tools, run_script_unsandboxed, skills_prompt,
};
use byob_tools::{Access, FileChange, Tool, ToolContext};
use serde_json::json;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn found() -> Discovery {
    discover(&PluginSources { builtin_roots: vec![fixtures().join("builtin")], roots: vec![fixtures().join("user")], linked: vec![] })
}

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("byob-plugins-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn has(cmd: &str) -> bool {
    Command::new(cmd).arg("--version").output().is_ok()
}

#[test]
fn builtin_manifests_load() {
    let d = found();
    let ids: Vec<&str> = d.plugins.iter().map(|p| p.id()).collect();
    assert_eq!(&ids[..4], ["calendar-mail", "code", "pdf", "web"]);
    let web = &d.plugins[3];
    assert!(web.builtin && web.manifest.permissions.network);
    assert_eq!(web.native_tools(), ["web_search", "fetch_url"]);
    let pdf = &d.plugins[2];
    assert!(pdf.manifest.starters[0].file);
    assert!(pdf.manifest.prompt.as_deref().is_some_and(|p| p.len() > 600), "built-in prompts may be longer");
    let cal = &d.plugins[0];
    assert_eq!(cal.manifest.platforms.as_deref(), Some(&["darwin".to_string()][..]));
    assert_eq!(cal.supported_here(), cfg!(target_os = "macos"));
    assert!(d.plugins.iter().all(|p| p.problems.is_empty() || !p.builtin));
}

#[test]
fn user_plugins_and_errors() {
    let d = found();
    let ids: Vec<&str> = d.plugins.iter().skip(4).map(|p| p.id()).collect();
    assert_eq!(ids, ["byob-alias", "memory", "missing-script", "recipe-helper", "text-stats"]);

    let errors: Vec<String> = d.errors.iter().map(|e| format!("{}: {}", e.path.file_name().unwrap().to_string_lossy(), e.message)).collect();
    assert_eq!(errors.len(), 3, "{errors:?}");
    assert_eq!(errors[0], "bad-id: byob-plugin.json: id must be lowercase letters, digits and dashes, like \"trip-planner\"");
    assert!(errors[1].starts_with("bad-json: byob-plugin.json: isn't valid JSON"), "{}", errors[1]);
    assert!(errors[2].starts_with("zz-dup-id: id \"memory\" is already used by"), "{}", errors[2]);

    let get = |id: &str| d.plugins.iter().find(|p| p.id() == id).unwrap();
    assert!(get("memory").manifest.mcp_servers["memory"].is_local());
    assert_eq!(get("missing-script").problems, ["byob-plugin.json: tools/go.py is missing"]);
    let recipe = get("recipe-helper");
    assert_eq!(recipe.skills.len(), 1);
    assert!(recipe.problems[0].contains("broken/SKILL.md: add a description"));
    let alias = get("byob-alias");
    assert!(!alias.supported_here());

    // non-builtin plugins can't turn on native tools
    assert!(get("text-stats").native_tools().is_empty());
    assert!(matches!(&get("text-stats").manifest.tools[0].kind, ToolKind::Script(s) if s == "tools/count.py"));

    // prompt and skills only from plugins active here
    let prompt = d.prompt();
    assert!(prompt.contains("use count_words") && prompt.contains("web_search"));
    assert_eq!(prompt.contains("calendar_events"), cfg!(target_os = "macos"));
    let skills = d.skills();
    assert_eq!(skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["fridge"]);
    assert!(skills_prompt(&skills).contains("- fridge: Suggest recipes from ingredients the user already has."));
}

#[test]
fn linked_folder_and_use_skill() {
    let d = discover(&PluginSources { linked: vec![fixtures().join("user/recipe-helper")], ..Default::default() });
    assert_eq!(d.plugins.len(), 1);
    let tool = UseSkillTool::new(d.skills());
    let cancel = AtomicBool::new(false);
    let ctx = ToolContext::new(Path::new("/nonexistent"), &cancel);
    let out = tool.run(&json!({"name": "fridge"}), &ctx);
    assert!(!out.failed);
    assert!(out.text.starts_with("1. List what they have") && out.text.ends_with("files: staples.txt)"), "{}", out.text);
}

/// A plugin in a temp folder whose tool runs `tools/run.sh` with `sh`.
fn sh_plugin(tag: &str, script: &str) -> (PathBuf, byob_plugins::Plugin) {
    let dir = temp_dir(tag);
    fs::create_dir_all(dir.join("tools")).unwrap();
    fs::write(dir.join("tools/run.sh"), script).unwrap();
    fs::write(
        dir.join("byob-plugin.json"),
        json!({"id": "shell", "name": "Shell", "tools": [{"name": "go", "description": "Go.", "run": "tools/run.sh", "title": "Went to {place}"}],
               "settings": [{"key": "API_KEY", "label": "Key"}, {"key": "MODE", "label": "Mode", "default": "fast"}]})
        .to_string(),
    )
    .unwrap();
    let p = load_plugin(&dir, false).unwrap().unwrap();
    (dir, p)
}

#[test]
fn script_tool_protocol() {
    if !has("sh") {
        eprintln!("skipped: no sh");
        return;
    }
    let script = r#"input=$(cat)
printf '%s|%s|%s|%s|%s\n' "$input" "$API_KEY" "$MODE" "$(basename "$HOB_PLUGIN_DIR")" "$HOB_CHAT_DIR" > seen.txt
echo made > chart.png
printf '{"text": "done", "files": ["chart.png", "../escape"]}'
"#;
    let (dir, plugin) = sh_plugin("sh", script);
    let ws = temp_dir("sh-ws");
    let settings = BTreeMap::from([("API_KEY".to_string(), "k1".to_string())]);
    let mut names = ToolNames::new(["go"]);
    let tools = python_tools(&plugin, &settings, &[OsString::from("sh")], &mut names);
    let tool = &tools[0];
    assert_eq!(tool.spec().name, "shell__go");
    assert_eq!(tool.access(&json!({})), Access::Execute);
    assert_eq!(tool.title(&json!({"place": "Lisbon"})), "Went to Lisbon");

    let cancel = AtomicBool::new(false);
    let ctx = ToolContext::new(&ws, &cancel);
    let out = tool.run(&json!({"place": "Lisbon"}), &ctx);
    assert!(!out.failed, "{}", out.text);
    assert_eq!(out.text, "done");
    let seen = fs::read_to_string(ws.join("seen.txt")).unwrap();
    let dir_name = dir.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(seen.trim(), format!("{{\"place\":\"Lisbon\"}}|k1|fast|{dir_name}|{}", ws.display()));
    assert!(out.changes.contains(&FileChange::Created(ws.join("chart.png"))));
    assert!(out.changes.contains(&FileChange::Created(ws.join("seen.txt"))));
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn script_failures() {
    if !has("sh") {
        eprintln!("skipped: no sh");
        return;
    }
    let (dir, plugin) = sh_plugin("sh-fail", "echo partial; echo boom >&2; exit 3\n");
    let ws = temp_dir("sh-fail-ws");
    let r = PluginRef::new(&plugin, &BTreeMap::new());
    let tool = PythonTool::new("go".into(), r.clone(), plugin.manifest.tools[0].clone(), vec![OsString::from("sh")]).unwrap();
    let cancel = AtomicBool::new(false);
    let ctx = ToolContext::new(&ws, &cancel);
    let out = tool.run(&json!({}), &ctx);
    assert!(out.failed);
    assert_eq!(out.text, "Shell ran into a problem:\npartial\nboom");

    // timeout
    fs::write(dir.join("tools/run.sh"), "sleep 5\n").unwrap();
    let slow =
        PythonTool::new("go".into(), r.clone(), plugin.manifest.tools[0].clone(), vec![OsString::from("sh")]).unwrap().with_timeout(Duration::from_millis(200));
    let t = Instant::now();
    let out = slow.run(&json!({}), &ctx);
    assert!(out.failed && out.text.contains("was stopped"), "{}", out.text);
    assert!(t.elapsed() < Duration::from_secs(4));

    // cancelled mid-run
    let cancel = AtomicBool::new(false);
    let ctx = ToolContext::new(&ws, &cancel);
    let out = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            cancel.store(true, Ordering::Relaxed);
        });
        tool.run(&json!({}), &ctx)
    });
    assert_eq!((out.failed, out.text.as_str()), (true, "Stopped by the user."));

    // missing script
    fs::remove_file(dir.join("tools/run.sh")).unwrap();
    let fresh = AtomicBool::new(false);
    let ctx = ToolContext::new(&ws, &fresh);
    assert!(tool.run(&json!({}), &ctx).text.contains("tools/run.sh is missing"));
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn bounded_output_and_ignored_stdin() {
    if !has("sh") {
        eprintln!("skipped: no sh");
        return;
    }
    let dir = temp_dir("sh-big");
    fs::write(dir.join("big.sh"), "i=0; while [ $i -lt 2000 ]; do echo 0123456789; i=$((i+1)); done\n").unwrap();
    let run = ScriptRun {
        program: vec![OsString::from("sh")],
        script: dir.join("big.sh"),
        args: vec![],
        cwd: dir.clone(),
        stdin: vec![b'x'; 1 << 20],
        env: BTreeMap::new(),
        timeout: Duration::from_secs(20),
        max_output: 100,
    };
    let r = run_script_unsandboxed(&run, &|| false).unwrap();
    assert!(r.success() && r.truncated);
    assert_eq!(r.stdout.len(), 100);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn real_python_tool_from_docs() {
    if !has("python3") {
        eprintln!("skipped: no python3");
        return;
    }
    let plugin = load_plugin(&fixtures().join("user/text-stats"), false).unwrap().unwrap();
    let tools = python_tools(&plugin, &BTreeMap::new(), &byob_plugins::default_python(), &mut ToolNames::default());
    let ws = temp_dir("py-ws");
    fs::write(ws.join("notes.txt"), "One two three. Four five!").unwrap();
    let cancel = AtomicBool::new(false);
    let ctx = ToolContext::new(&ws, &cancel);
    let out = tools[0].run(&json!({"file": ws.join("notes.txt")}), &ctx);
    assert_eq!((out.failed, out.text.as_str()), (false, "5 words, 2 sentences"));
    assert_eq!(tools[0].title(&json!({"file": "notes.txt"})), "Counted the words in notes.txt");
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn the_old_manifest_name_is_not_read() {
    let d = tempfile::tempdir().unwrap();
    let old = d.path().join("old");
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("hob-plugin.json"), r#"{"id": "old", "name": "Old"}"#).unwrap();
    assert!(byob_plugins::discover::manifest_path(&old).is_none());
    fs::write(old.join("byob-plugin.json"), r#"{"id": "old", "name": "Old"}"#).unwrap();
    assert_eq!(byob_plugins::discover::manifest_path(&old), Some(old.join("byob-plugin.json")));
}
