//! Routines and their runs (see [`byob_types::routine`]).
//!
//! - `<config>/routines.json`: `{"routines": [{id, name, instructions, when, …}]}`, synced
//!   to every client.
//! - `<data>/routine-runs/<id>.json`: `{"last_slot": ms, "runs": [{chat, started, status,
//!   report, …}]}`, newest last: the scheduler's memory (the last scheduled time it handled,
//!   so a restart doesn't run it again) and the run records.
//!
//! Both are read leniently: an entry that can't be read (a hand edit, a newer build's) is
//! logged and kept aside as it was, written back on save, and the others load. Written
//! atomically after each change.

use std::collections::BTreeMap;
use std::path::PathBuf;

use byob_types::routine::{self, MAX_ROUTINES, MAX_RUN_RECORDS, RunRecord};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use byob_types::routine::Routine;

use crate::error::{Result, StoreError};
use crate::files::{Loaded, load_json, save_json};
use crate::model::Millis;
use crate::paths::Paths;

fn raw_list<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Vec<Value>, D::Error> {
    match Value::deserialize(d)? {
        Value::Array(items) => Ok(items),
        other => {
            if !other.is_null() {
                log::warn!("byob-store: ignoring a routines list that isn't a list: {other}");
            }
            Ok(Vec::new())
        }
    }
}

fn lenient_slot<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<Millis>, D::Error> {
    Ok(Value::deserialize(d)?.as_i64())
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RoutinesFile {
    #[serde(default, deserialize_with = "raw_list")]
    routines: Vec<Value>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// Every routine, in memory, saved on change.
#[derive(Debug, Default)]
pub struct Routines {
    file: Option<PathBuf>,
    items: Vec<Routine>,
    /// Entries that couldn't be read, kept as they were.
    aside: Vec<Value>,
    extra: Map<String, Value>,
}

impl Routines {
    /// Read `routines.json` (missing → none). Without `paths`, everything stays in memory.
    pub fn open(paths: Option<&Paths>) -> Loaded<Self> {
        let Some(paths) = paths else { return Loaded { value: Self::default(), warnings: Vec::new() } };
        let file = paths.routines();
        let loaded: Loaded<RoutinesFile> = load_json(&file);
        let mut warnings = loaded.warnings;
        let mut value = Self { file: Some(file), extra: loaded.value.extra, ..Self::default() };
        for v in loaded.value.routines {
            let parsed = serde_json::from_value::<Routine>(v.clone()).map_err(|e| e.to_string()).and_then(Routine::validated);
            match parsed {
                Ok(r) if routine::id_ok(&r.id) && value.get(&r.id).is_none() && value.items.len() < MAX_ROUTINES => value.items.push(r),
                Ok(r) => {
                    warnings.push(format!("ignoring routine {:?}: its id is missing, repeated or past the limit", r.id));
                    value.aside.push(v);
                }
                Err(e) => {
                    warnings.push(format!("ignoring a routine that can't be read ({e}); it's kept in the file"));
                    value.aside.push(v);
                }
            }
        }
        for w in &warnings {
            log::warn!("byob-store: {w}");
        }
        Loaded { value, warnings }
    }

    pub fn items(&self) -> &[Routine] {
        &self.items
    }

    pub fn get(&self, id: &str) -> Option<&Routine> {
        self.items.iter().find(|r| r.id == id)
    }

    /// Add a routine or replace the one with its id, and save. `r` must be validated and
    /// have an id.
    pub fn put(&mut self, r: Routine) -> Result<()> {
        if !routine::id_ok(&r.id) {
            return Err(StoreError::Invalid(format!("{:?} isn't a routine id", r.id)));
        }
        let full = self.items.len() >= MAX_ROUTINES;
        match self.items.iter_mut().find(|x| x.id == r.id) {
            Some(x) => *x = r,
            None if full => return Err(StoreError::Invalid(format!("byob keeps at most {MAX_ROUTINES} routines"))),
            None => self.items.push(r),
        }
        self.save()
    }

    /// Remove a routine and save; `false` if there was none.
    pub fn remove(&mut self, id: &str) -> Result<bool> {
        let before = self.items.len();
        self.items.retain(|r| r.id != id);
        if self.items.len() == before {
            return Ok(false);
        }
        self.save().map(|()| true)
    }

    fn save(&self) -> Result<()> {
        let Some(file) = &self.file else { return Ok(()) };
        let mut routines: Vec<Value> = self.items.iter().filter_map(|r| serde_json::to_value(r).ok()).collect();
        routines.extend(self.aside.iter().cloned());
        save_json(file, &RoutinesFile { routines, extra: self.extra.clone() }, None)
    }
}

/// One routine's runs and the scheduler's memory of it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunLog {
    /// The last scheduled time (ms) that was run or deliberately passed over.
    #[serde(default, deserialize_with = "lenient_slot", skip_serializing_if = "Option::is_none")]
    pub last_slot: Option<Millis>,
    /// Oldest first, at most [`MAX_RUN_RECORDS`].
    #[serde(default, deserialize_with = "lenient_runs")]
    pub runs: Vec<RunRecord>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn lenient_runs<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Vec<RunRecord>, D::Error> {
    let Value::Array(items) = Value::deserialize(d)? else { return Ok(Vec::new()) };
    Ok(items
        .into_iter()
        .filter_map(|v| serde_json::from_value(v.clone()).map_err(|e| log::warn!("byob-store: ignoring a routine run {v}: {e}")).ok())
        .collect())
}

/// Every routine's [`RunLog`], in memory, each saved on change.
#[derive(Debug, Default)]
pub struct RunLogs {
    dir: Option<PathBuf>,
    logs: BTreeMap<String, RunLog>,
}

impl RunLogs {
    /// Read the logs of `ids` (missing → empty). Without `paths`, everything stays in memory.
    pub fn open<'a>(paths: Option<&Paths>, ids: impl IntoIterator<Item = &'a str>) -> Loaded<Self> {
        let mut value = Self { dir: paths.map(Paths::routine_runs), logs: BTreeMap::new() };
        let mut warnings = Vec::new();
        if let Some(dir) = value.dir.clone() {
            for id in ids.into_iter().filter(|id| routine::id_ok(id)) {
                let loaded: Loaded<RunLog> = load_json(&dir.join(format!("{id}.json")));
                warnings.extend(loaded.warnings);
                let mut log = loaded.value;
                cap(&mut log);
                value.logs.insert(id.to_owned(), log);
            }
        }
        Loaded { value, warnings }
    }

    pub fn get(&self, id: &str) -> Option<&RunLog> {
        self.logs.get(id)
    }

    /// Change a routine's log and save it.
    pub fn update<T>(&mut self, id: &str, f: impl FnOnce(&mut RunLog) -> T) -> Result<T> {
        if !routine::id_ok(id) {
            return Err(StoreError::Invalid(format!("{id:?} isn't a routine id")));
        }
        let log = self.logs.entry(id.to_owned()).or_default();
        let out = f(log);
        cap(log);
        if let Some(dir) = &self.dir {
            save_json(&dir.join(format!("{id}.json")), log, None)?;
        }
        Ok(out)
    }

    /// Forget a routine's log (and delete its file).
    pub fn remove(&mut self, id: &str) {
        self.logs.remove(id);
        if let Some(dir) = &self.dir
            && routine::id_ok(id)
        {
            let path = dir.join(format!("{id}.json"));
            if let Err(e) = std::fs::remove_file(&path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!("byob-store: couldn't delete {}: {e}", path.display());
            }
        }
    }
}

fn cap(log: &mut RunLog) {
    let over = log.runs.len().saturating_sub(MAX_RUN_RECORDS);
    log.runs.drain(..over);
}

#[cfg(test)]
mod tests {
    use super::*;
    use byob_types::routine::{RunStatus, Trigger};

    fn paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::at(dir.path());
        (dir, p)
    }

    fn loom(id: &str) -> Routine {
        Routine {
            id: id.into(),
            name: "Loom deals".into(),
            instructions: "Find loom deals.".into(),
            when: vec![Trigger::Schedule { rule: "FREQ=DAILY".into(), at: "08:00".into(), zone: None, start: None }],
            ..Routine::default()
        }
    }

    #[test]
    fn saves_and_reloads() {
        let (_d, p) = paths();
        let mut r = Routines::open(Some(&p)).value;
        r.put(loom("loom")).unwrap();
        r.put(Routine { name: "Looms".into(), ..loom("loom") }).unwrap();
        r.put(loom("other")).unwrap();
        let again = Routines::open(Some(&p)).value;
        assert_eq!(again.items().len(), 2);
        assert_eq!(again.get("loom").unwrap().name, "Looms");
        let mut again = again;
        assert!(again.remove("other").unwrap());
        assert!(!again.remove("other").unwrap());
        assert_eq!(Routines::open(Some(&p)).value.items().len(), 1);
        assert!(r.put(loom("Bad Id")).is_err());
    }

    /// One bad entry (a hand edit, a newer build's) doesn't lose the others, and is kept in
    /// the file when byob saves.
    #[test]
    fn a_bad_entry_doesnt_lose_the_rest() {
        let (_d, p) = paths();
        let good = serde_json::to_value(loom("good")).unwrap();
        let text = serde_json::json!({"routines": [
            {"id": "broken", "name": 5},
            good,
            {"id": "empty", "name": "x", "instructions": ""},
            serde_json::to_value(loom("good")).unwrap(),
        ], "future": true});
        std::fs::create_dir_all(p.routines().parent().unwrap()).unwrap();
        std::fs::write(p.routines(), text.to_string()).unwrap();
        let loaded = Routines::open(Some(&p));
        assert_eq!(loaded.value.items().iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["good"]);
        assert_eq!(loaded.warnings.len(), 3, "{:?}", loaded.warnings);
        let mut r = loaded.value;
        r.put(loom("new")).unwrap();
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(p.routines()).unwrap()).unwrap();
        assert_eq!(saved["routines"].as_array().unwrap().len(), 5, "the unreadable ones are kept");
        assert_eq!(saved["future"], true);
        // a file that isn't JSON at all is kept aside and byob starts empty
        std::fs::write(p.routines(), "{not json").unwrap();
        let l = Routines::open(Some(&p));
        assert!(l.value.items().is_empty() && !l.warnings.is_empty());
    }

    #[test]
    fn run_logs_persist_cap_and_go_with_their_routine() {
        let (_d, p) = paths();
        let mut logs = RunLogs::open(Some(&p), ["loom"]).value;
        logs.update("loom", |l| {
            l.last_slot = Some(42);
            for i in 0..(MAX_RUN_RECORDS + 5) {
                l.runs.push(RunRecord { started: i64::try_from(i).unwrap(), status: RunStatus::Quiet, ..RunRecord::default() });
            }
        })
        .unwrap();
        let again = RunLogs::open(Some(&p), ["loom", "none"]).value;
        let l = again.get("loom").unwrap();
        assert_eq!((l.last_slot, l.runs.len(), l.runs[0].started), (Some(42), MAX_RUN_RECORDS, 5));
        assert_eq!(again.get("none").unwrap().runs.len(), 0);
        // a bad record is dropped, not the log
        std::fs::write(p.routine_runs().join("loom.json"), r#"{"last_slot":"x","runs":[{"started":"no"},{"started":7,"status":"quiet"}]}"#).unwrap();
        let l = RunLogs::open(Some(&p), ["loom"]).value;
        assert_eq!(l.get("loom").unwrap().runs.len(), 1);
        assert_eq!(l.get("loom").unwrap().last_slot, None);
        let mut l = l;
        l.remove("loom");
        assert!(!p.routine_runs().join("loom.json").exists());
        assert!(l.update("../x", |_| ()).is_err());
    }
}
