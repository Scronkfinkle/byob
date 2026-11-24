//! Routines: saved instructions byob carries out by itself, on a schedule or by hand ("Every
//! morning, search the web for a deal on a new loom"). Each run is a chat; a run that finds
//! something calls `report`, and the user hears about it.
//!
//! The engine keeps routines in `<config>/routines.json` and each one's runs in
//! `<data>/routine-runs/<id>.json`; clients get them over the protocol (cap `routines`).
//! Everything here is untrusted when it arrives (a client's save, a model's tool call, a hand
//! edit): [`Routine::validated`] trims and refuses what doesn't fit. Whether a schedule's rule
//! makes sense is the engine's check (it reads rules with `byob-when`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Approvals;

/// Most routines one byob keeps.
pub const MAX_ROUTINES: usize = 200;
/// Longest routine name (characters).
pub const MAX_NAME_CHARS: usize = 80;
/// Longest instructions (bytes).
pub const MAX_INSTRUCTIONS_BYTES: usize = 8 * 1024;
/// Longest routine id.
pub const MAX_ID_CHARS: usize = 64;
/// Most triggers on one routine.
pub const MAX_TRIGGERS: usize = 8;
/// Most per-tool grants on one routine.
pub const MAX_GRANTS: usize = 64;
/// Most report channels on one routine.
pub const MAX_CHANNELS: usize = 8;
/// Most runs kept (with their chats) per routine.
pub const MAX_KEEP: u32 = 200;
/// Runs kept when a routine doesn't say.
pub const DEFAULT_KEEP: u32 = 30;
/// Most run records kept per routine (older runs beyond `keep` keep only their report).
pub const MAX_RUN_RECORDS: usize = 200;
/// Longest report title (characters).
pub const MAX_REPORT_TITLE_CHARS: usize = 200;
/// Longest report body (bytes).
pub const MAX_REPORT_BODY_BYTES: usize = 8 * 1024;
/// Longest text field of a trigger (a rule, a time, a zone, a plugin or trigger id).
pub const MAX_TRIGGER_TEXT: usize = 500;
/// Largest plugin trigger `params` (JSON bytes).
pub const MAX_TRIGGER_PARAMS_BYTES: usize = 8 * 1024;
/// The built-in channel: a desktop notification.
pub const CHANNEL_DESKTOP: &str = "desktop";

fn yes() -> bool {
    true
}

fn read_only() -> Approvals {
    Approvals::ReadOnly
}

fn default_keep() -> u32 {
    DEFAULT_KEEP
}

fn desktop() -> Vec<String> {
    vec![CHANNEL_DESKTOP.to_owned()]
}

/// A saved routine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Routine {
    /// `[a-z0-9-]`, made by the engine for new routines.
    #[serde(default)]
    pub id: String,
    pub name: String,
    /// What a run is asked to do (the prompt).
    pub instructions: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// What starts it besides Run now.
    #[serde(default)]
    pub when: Vec<Trigger>,
    /// The project (workspace id) its runs work in; `None`: a folder of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The model profile its runs use; `None`: the default model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What its runs may do without asking (default Read only).
    #[serde(default = "read_only")]
    pub allowed: Approvals,
    /// Tools its runs may call without asking beyond `allowed` ("Always for this routine").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<String>,
    /// When a run tells the user.
    #[serde(default)]
    pub tell: Tell,
    /// Where reports go (`desktop`, or a plugin's `plugin:channel`).
    #[serde(default = "desktop")]
    pub channels: Vec<String>,
    /// What happens to runs missed while byob wasn't running.
    #[serde(default)]
    pub missed: Missed,
    /// How many runs (with their chats) to keep.
    #[serde(default = "default_keep")]
    pub keep: u32,
    /// Milliseconds since the epoch; set by the engine.
    #[serde(default)]
    pub created_at: i64,
}

impl Default for Routine {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            instructions: String::new(),
            enabled: true,
            when: Vec::new(),
            project: None,
            model: None,
            allowed: Approvals::ReadOnly,
            grants: Vec::new(),
            tell: Tell::default(),
            channels: desktop(),
            missed: Missed::default(),
            keep: DEFAULT_KEEP,
            created_at: 0,
        }
    }
}

/// What starts a routine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    /// A schedule: `rule` an RRULE (`FREQ=DAILY`, `FREQ=WEEKLY;BYDAY=MO,WE`,
    /// `FREQ=HOURLY;INTERVAL=2`), `at` a 24-hour `HH:MM`, `zone` an IANA zone (none: the
    /// user's), `start` the first day `YYYY-MM-DD`.
    Schedule {
        rule: String,
        #[serde(default)]
        at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        zone: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<String>,
    },
    /// A plugin's trigger (`trigger` of plugin `plugin`, with its `params`).
    Plugin {
        plugin: String,
        trigger: String,
        #[serde(default)]
        params: Value,
    },
    /// A trigger a newer byob wrote: kept out of the way, never fires.
    #[serde(other)]
    Unknown,
}

/// When a run tells the user.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tell {
    /// Only runs that call `report`.
    #[default]
    Found,
    /// Every run (a quiet one says it found nothing).
    Always,
    /// Never: reports are only kept.
    Never,
}

/// Runs missed while byob wasn't running (or the computer slept).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Missed {
    /// Made up once, never once per missed slot.
    #[default]
    Once,
    /// Not made up.
    Skip,
}

/// `[a-z0-9-]`, 1 to [`MAX_ID_CHARS`].
pub fn id_ok(id: &str) -> bool {
    (1..=MAX_ID_CHARS).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A new id from a name: `loom-deals`, `loom-deals-2` when taken.
pub fn new_id(name: &str, taken: impl Fn(&str) -> bool) -> String {
    let mut slug = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    let base = if slug.is_empty() { "routine" } else { slug };
    if !taken(base) {
        return base.to_owned();
    }
    (2..10_000u32).map(|n| format!("{base}-{n}")).find(|id| !taken(id)).unwrap_or_else(|| format!("{base}-{}", name.len()))
}

fn long(s: &str, max: usize) -> bool {
    s.chars().count() > max
}

impl Routine {
    /// Check an untrusted routine: trims text, refuses what doesn't fit. The id may be empty
    /// (a new routine: the engine makes one); otherwise it must be [`id_ok`].
    pub fn validated(mut self) -> Result<Self, String> {
        self.id = self.id.trim().to_owned();
        self.name = self.name.trim().to_owned();
        self.instructions = self.instructions.trim().to_owned();
        if !self.id.is_empty() && !id_ok(&self.id) {
            return Err(format!("{:?} isn't a routine id", self.id.chars().take(80).collect::<String>()));
        }
        if self.name.is_empty() {
            return Err("a routine needs a name".into());
        }
        if long(&self.name, MAX_NAME_CHARS) {
            return Err(format!("routine names are limited to {MAX_NAME_CHARS} characters"));
        }
        if self.instructions.is_empty() {
            return Err("a routine needs instructions (what each run should do)".into());
        }
        if self.instructions.len() > MAX_INSTRUCTIONS_BYTES {
            return Err(format!("a routine's instructions are limited to {} KB", MAX_INSTRUCTIONS_BYTES / 1024));
        }
        if self.when.len() > MAX_TRIGGERS {
            return Err(format!("a routine has at most {MAX_TRIGGERS} triggers"));
        }
        for t in &mut self.when {
            match t {
                Trigger::Schedule { rule, at, zone, start } => {
                    *rule = rule.trim().to_owned();
                    *at = at.trim().to_owned();
                    for s in [&mut *zone, &mut *start] {
                        if let Some(v) = s {
                            *v = v.trim().to_owned();
                        }
                        if s.as_deref().is_some_and(str::is_empty) {
                            *s = None;
                        }
                    }
                    let texts = [Some(rule.as_str()), Some(at.as_str()), zone.as_deref(), start.as_deref()];
                    if texts.into_iter().flatten().any(|s| long(s, MAX_TRIGGER_TEXT)) {
                        return Err("a schedule's text is too long".into());
                    }
                }
                Trigger::Plugin { plugin, trigger, params } => {
                    if !crate::config::plugin_id_ok(plugin) {
                        return Err(format!("{:?} isn't a plugin id", plugin.chars().take(60).collect::<String>()));
                    }
                    if trigger.is_empty() || long(trigger, MAX_TRIGGER_TEXT) {
                        return Err("bad plugin trigger".into());
                    }
                    if serde_json::to_string(params).map_or(usize::MAX, |s| s.len()) > MAX_TRIGGER_PARAMS_BYTES {
                        return Err("a trigger's settings are too large".into());
                    }
                }
                Trigger::Unknown => {}
            }
        }
        for o in [&mut self.project, &mut self.model] {
            if let Some(v) = o {
                *v = v.trim().to_owned();
            }
            if o.as_deref().is_some_and(str::is_empty) {
                *o = None;
            }
            if o.as_deref().is_some_and(|v| long(v, 200)) {
                return Err("bad project or model id".into());
            }
        }
        self.grants.retain(|g| !g.trim().is_empty());
        self.grants.sort();
        self.grants.dedup();
        if self.grants.len() > MAX_GRANTS || self.grants.iter().any(|g| long(g, 200)) {
            return Err(format!("a routine has at most {MAX_GRANTS} tool grants"));
        }
        self.channels.retain(|c| !c.trim().is_empty());
        self.channels.dedup();
        if self.channels.len() > MAX_CHANNELS || self.channels.iter().any(|c| long(c, 200)) {
            return Err(format!("a routine has at most {MAX_CHANNELS} channels"));
        }
        if self.keep == 0 || self.keep > MAX_KEEP {
            return Err(format!("a routine keeps between 1 and {MAX_KEEP} runs"));
        }
        Ok(self)
    }

    /// Its schedules (rule, at, zone, start).
    pub fn schedules(&self) -> impl Iterator<Item = (&str, &str, Option<&str>, Option<&str>)> {
        self.when.iter().filter_map(|t| match t {
            Trigger::Schedule { rule, at, zone, start } => Some((rule.as_str(), at.as_str(), zone.as_deref(), start.as_deref())),
            _ => None,
        })
    }
}

/// How a run went.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Due, waiting for a free run slot (at most a few routine runs at once).
    Queued,
    /// Working.
    #[default]
    Running,
    /// Stopped at a call it isn't allowed to make: waiting for a person.
    Waiting,
    /// Finished and reported something.
    Reported,
    /// Finished with nothing to report.
    Quiet,
    /// Ended with an error.
    Failed,
    /// Stopped by a person (or by byob quitting).
    Stopped,
    /// Due while the previous run was still going: not started.
    Skipped,
    /// A status a newer byob wrote.
    #[serde(other)]
    Unknown,
}

impl RunStatus {
    /// Still going (or about to).
    pub fn active(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Waiting)
    }

    /// In words: `Nothing new`, `Waiting for you`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "Waiting to start",
            Self::Running => "Running",
            Self::Waiting => "Waiting for you",
            Self::Reported => "Reported",
            Self::Quiet => "Nothing new",
            Self::Failed => "Failed",
            Self::Stopped => "Stopped",
            Self::Skipped => "Skipped (the last run was still going)",
            Self::Unknown => "Unknown",
        }
    }
}

/// What started a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunCause {
    /// Its schedule.
    #[default]
    Schedule,
    /// Run now (a person or the model's `run_routine`).
    Manual,
    /// A plugin's trigger.
    Plugin,
    #[serde(other)]
    Unknown,
}

/// What a run reported (`report { title, body }`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub title: String,
    #[serde(default)]
    pub body: String,
}

impl Report {
    /// Trimmed and cut to size (a model wrote it).
    pub fn clean(title: &str, body: &str) -> Self {
        let title: String = title.trim().chars().take(MAX_REPORT_TITLE_CHARS).collect();
        let mut body = body.trim().to_owned();
        if body.len() > MAX_REPORT_BODY_BYTES {
            let mut cut = MAX_REPORT_BODY_BYTES;
            while !body.is_char_boundary(cut) {
                cut -= 1;
            }
            body.truncate(cut);
            body.push('…');
        }
        Self { title, body }
    }
}

/// One run of a routine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    /// Its chat; `None` once pruned (beyond `keep`) or for a run that never started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<String>,
    /// Milliseconds since the epoch.
    #[serde(default)]
    pub started: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<i64>,
    #[serde(default)]
    pub status: RunStatus,
    #[serde(default)]
    pub cause: RunCause,
    /// The scheduled time it stood for (ms), for scheduled runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<Report>,
    /// Why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Tokens the run used (prompt + completion), once finished.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tokens: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loom() -> Routine {
        Routine {
            name: " Loom deals ".into(),
            instructions: "Search the web for deals on a new floor loom.".into(),
            when: vec![Trigger::Schedule { rule: "FREQ=DAILY".into(), at: "08:00".into(), zone: Some(" ".into()), start: None }],
            ..Routine::default()
        }
    }

    #[test]
    fn validates_and_trims() {
        let r = loom().validated().unwrap();
        assert_eq!(r.name, "Loom deals");
        assert_eq!(r.allowed, Approvals::ReadOnly, "runs are read only unless said otherwise");
        assert!(matches!(&r.when[0], Trigger::Schedule { zone: None, .. }), "a blank zone is the user's");
        assert!(Routine { name: String::new(), ..loom() }.validated().is_err());
        assert!(Routine { instructions: "x".repeat(MAX_INSTRUCTIONS_BYTES + 1), ..loom() }.validated().is_err());
        assert!(Routine { keep: 0, ..loom() }.validated().is_err());
        assert!(Routine { id: "../etc".into(), ..loom() }.validated().is_err());
        assert!(Routine { when: vec![Trigger::Unknown; MAX_TRIGGERS + 1], ..loom() }.validated().is_err());
    }

    #[test]
    fn reads_what_older_and_newer_builds_write() {
        // the least a hand edit can say
        let r: Routine = serde_json::from_str(r#"{"name":"A","instructions":"B"}"#).unwrap();
        assert!(r.enabled);
        assert_eq!((r.keep, r.allowed, r.tell, r.missed), (DEFAULT_KEEP, Approvals::ReadOnly, Tell::Found, Missed::Once));
        assert_eq!(r.channels, [CHANNEL_DESKTOP]);
        // a newer trigger kind and run status don't make it unreadable
        let r: Routine = serde_json::from_str(r#"{"name":"A","instructions":"B","when":[{"type":"geofence","lat":1}]}"#).unwrap();
        assert_eq!(r.when, [Trigger::Unknown]);
        let run: RunRecord = serde_json::from_str(r#"{"status":"teleported","started":5}"#).unwrap();
        assert_eq!((run.status, run.started), (RunStatus::Unknown, 5));
    }

    #[test]
    fn ids_and_reports() {
        assert_eq!(new_id("Loom deals!", |_| false), "loom-deals");
        assert_eq!(new_id("Loom deals", |id| id == "loom-deals"), "loom-deals-2");
        assert_eq!(new_id("???", |_| false), "routine");
        assert!(id_ok("loom-deals-2") && !id_ok("") && !id_ok("Loom"));
        let r = Report::clean(&"t".repeat(500), &"é".repeat(MAX_REPORT_BODY_BYTES));
        assert_eq!(r.title.chars().count(), MAX_REPORT_TITLE_CHARS);
        assert!(r.body.len() <= MAX_REPORT_BODY_BYTES + 3 && r.body.ends_with('…'));
    }
}
