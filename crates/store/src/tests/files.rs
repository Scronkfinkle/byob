use std::path::PathBuf;

use serde_json::{Value, json};

use super::TempDir;
use crate::*;

fn read_json(p: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn missing_settings_are_defaults_without_warnings() {
    let dir = TempDir::new();
    let l = Settings::load(&dir.path().join("settings.json"));
    assert_eq!(l.value, Settings::default());
    assert!(l.warnings.is_empty());
}

#[test]
fn settings_round_trip() {
    let dir = TempDir::new();
    let p = Paths::at(dir.path().join("cfg")).settings();
    let s = Settings {
        inference: InferenceSettings { base_url: Some("http://localhost:8080/v1".into()), model: Some("local".into()), ..Default::default() },
        theme: Theme::Dark,
        workspace: Some(PathBuf::from("/home/me/Documents/byob")),
        plugin_dirs: vec!["/opt/plugins".into(), "/home/me/ünï plugins".into()],
        ..Default::default()
    };
    s.save(&p).unwrap();
    let l = Settings::load(&p);
    assert!(l.warnings.is_empty(), "{:?}", l.warnings);
    assert_eq!(l.value, s);
    // No temp files left behind.
    let names: Vec<_> = std::fs::read_dir(p.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names, ["settings.json"]);
}

#[test]
fn unknown_fields_are_preserved() {
    let dir = TempDir::new();
    let p = dir.path().join("settings.json");
    std::fs::write(&p, r#"{"theme":"light","future":{"x":[1,2]},"inference":{"model":"m","temperature":0.2}}"#).unwrap();
    let s = Settings::update(&p, |s| s.workspace = Some("/w".into())).unwrap();
    assert_eq!(s.theme, Theme::Light);
    let v = read_json(&p);
    assert_eq!(v["future"], json!({"x": [1, 2]}));
    assert_eq!(v["inference"]["temperature"], json!(0.2));
    assert_eq!(v["inference"]["model"], json!("m"));
    assert_eq!(v["workspace"], json!("/w"));
}

#[test]
fn compaction_settings_default_and_partial() {
    let dir = TempDir::new();
    let p = dir.path().join("settings.json");
    Settings::default().save(&p).unwrap();
    assert!(read_json(&p).get("compaction").is_none(), "defaults are not written");
    std::fs::write(&p, r#"{"compaction":{"threshold_percent":60}}"#).unwrap();
    assert_eq!(Settings::load(&p).value.compaction, CompactionSettings { auto: true, threshold_percent: 60, keep_turns: 2 });
    std::fs::write(&p, r#"{"compaction":"off"}"#).unwrap();
    assert_eq!(Settings::load(&p).value.compaction, CompactionSettings::default());
}

#[test]
fn wrong_typed_fields_fall_back_individually() {
    let dir = TempDir::new();
    let p = dir.path().join("settings.json");
    std::fs::write(&p, r#"{"theme":"purple","plugin_dirs":"not a list","workspace":"/ok","inference":{"base_url":42,"model":"kept"}}"#).unwrap();
    let s = Settings::load(&p).value;
    assert_eq!(s.theme, Theme::System);
    assert!(s.plugin_dirs.is_empty());
    assert_eq!(s.workspace, Some("/ok".into()));
    assert_eq!(s.inference.base_url, None);
    assert_eq!(s.inference.model.as_deref(), Some("kept"));
}

#[test]
fn corrupt_settings_give_defaults_warning_and_a_backup() {
    let dir = TempDir::new();
    let p = dir.path().join("settings.json");
    for bad in ["{\"theme\": \"dark\",", "", "[1,2,3]", "\u{0}\u{1}binary"] {
        std::fs::write(&p, bad).unwrap();
        let l = Settings::load(&p);
        assert_eq!(l.value, Settings::default(), "input {bad:?}");
        assert_eq!(l.warnings.len(), 1, "input {bad:?}");
        assert_eq!(std::fs::read_to_string(dir.path().join("settings.json.corrupt")).unwrap(), bad);
    }
}

#[test]
fn api_keys_never_land_in_settings() {
    let dir = TempDir::new();
    let p = dir.path().join("settings.json");
    std::fs::write(&p, r#"{"api_key":"sk-top","inference":{"api_key":"sk-nested","model":"m"}}"#).unwrap();
    let l = Settings::load(&p);
    assert_eq!(l.warnings.len(), 2, "{:?}", l.warnings);
    assert!(!l.value.extra.contains_key("api_key"));
    let mut s = l.value;
    s.inference.extra.insert("api_key".into(), json!("sk-sneaky"));
    s.save(&p).unwrap();
    assert!(!std::fs::read_to_string(&p).unwrap().contains("sk-"));
}

#[test]
fn model_roles_load_without_their_keys() {
    let dir = TempDir::new();
    let p = dir.path().join("settings.json");
    let text = r#"{"models":{"fast":{"base_url":"http://h:1/v1","model":"small","api_key":"sk-role"},"vision":{"model":"vl"}},"subagents":{"max_parallel":2}}"#;
    std::fs::write(&p, text).unwrap();
    let l = Settings::load(&p);
    assert_eq!(l.warnings.len(), 1, "{:?}", l.warnings);
    let fast = &l.value.models["fast"];
    assert_eq!((fast.base_url.as_deref(), fast.model.as_deref()), (Some("http://h:1/v1"), Some("small")));
    assert!(fast.extra.is_empty());
    assert_eq!(l.value.models["vision"].base_url, None);
    assert_eq!(l.value.subagents, SubagentSettings { max_parallel: Some(2), ..Default::default() });
    l.value.save(&p).unwrap();
    let saved = std::fs::read_to_string(&p).unwrap();
    assert!(!saved.contains("sk-") && saved.contains("\"small\""), "{saved}");
    // Role keys belong in secrets.json.
    let sp = Paths::at(dir.path()).secrets();
    let s = Secrets { model_keys: [("fast".to_owned(), "sk-fast".to_owned())].into(), ..Default::default() };
    assert!(!format!("{s:?}").contains("sk-fast"));
    s.save(&sp).unwrap();
    assert_eq!(Secrets::load(&sp).value.model_keys["fast"], "sk-fast");
}

#[test]
fn secrets_round_trip_private_and_redacted() {
    let dir = TempDir::new();
    let p = Paths::at(dir.path()).secrets();
    let s = Secrets { api_key: Some("sk-secret".into()), ..Default::default() };
    assert!(!format!("{s:?}").contains("sk-secret"));
    s.save(&p).unwrap();
    let l = Secrets::load(&p);
    assert!(l.warnings.is_empty(), "{:?}", l.warnings);
    assert_eq!(l.value, s);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[cfg(unix)]
#[test]
fn loose_secrets_permissions_are_tightened() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new();
    let p = dir.path().join("secrets.json");
    std::fs::write(&p, r#"{"api_key":"k"}"#).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    let l = Secrets::load(&p);
    assert_eq!(l.value.api_key.as_deref(), Some("k"));
    assert_eq!(l.warnings.len(), 1);
    assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn save_replaces_existing_file_atomically() {
    let dir = TempDir::new();
    let p = dir.path().join("settings.json");
    // A stale temp file from a crashed save must not break the next one.
    std::fs::write(dir.path().join(format!("settings.json.tmp-{}", std::process::id())), "stale").unwrap();
    for model in ["a", "b", "c"] {
        Settings::update(&p, |s| s.inference.model = Some(model.into())).unwrap();
    }
    assert_eq!(Settings::load(&p).value.inference.model.as_deref(), Some("c"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn ensure_creates_dirs() {
    let dir = TempDir::new();
    let paths = Paths { config_dir: dir.path().join("c"), data_dir: dir.path().join("d") };
    paths.ensure().unwrap();
    assert!(paths.config_dir.is_dir() && paths.data_dir.is_dir() && paths.undo_dir().is_dir());
    assert_eq!(paths.db(), dir.path().join("d/byob.db"));
}

#[test]
fn storage_settings_parse_and_default_to_sqlite() {
    let dir = TempDir::new();
    let p = dir.path().join("settings.json");
    assert_eq!(Settings::default().storage, StorageKind::Sqlite);
    std::fs::write(&p, r#"{"storage": "folder", "chats_folder": "~/byob"}"#).unwrap();
    let s = Settings::load(&p).value;
    assert_eq!(s.storage, StorageKind::Folder);
    let root = folder_root(&s, &Paths::at(dir.path()));
    assert!(root.ends_with("byob") && !root.starts_with("~"), "{}", root.display());
    assert_eq!(folder_root(&Settings::default(), &Paths::at(dir.path())), dir.path().join("chats-folder"));
    // An unknown backend falls back to the default rather than resetting everything.
    std::fs::write(&p, r#"{"storage": "mongo", "theme": "dark"}"#).unwrap();
    let s = Settings::load(&p).value;
    assert_eq!((s.storage, s.theme), (StorageKind::Sqlite, Theme::Dark));
    // The default isn't written out.
    Settings::default().save(&p).unwrap();
    assert!(!std::fs::read_to_string(&p).unwrap().contains("storage"));
}
