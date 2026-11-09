//! Step presentation and per-turn undo: bounding tool previews, collecting a call's undo
//! ops, and reversing a reply's file changes on [`ClientMsg::Undo`](yah_protocol::ClientMsg::Undo).

use std::collections::BTreeMap;
use std::path::PathBuf;

use yah_protocol::{DiffLine, MAX_PREVIEW_LINES, NoticeLevel, Preview, ServerMsg};
use yah_tools::preview::clip_line;
use yah_tools::undo::Fingerprint;
use yah_tools::{FileChange, UndoOp};

use crate::{ClientId, Engine};

/// Most characters of a preview's code block.
const MAX_CODE_CHARS: usize = 4000;

fn clip_pair((a, b): (String, String)) -> (String, String) {
    (clip_line(&a), clip_line(&b))
}

/// Previews come from tool code (plugins too): cap every list and string before they go
/// to clients.
pub(crate) fn bound_preview(p: Preview) -> Preview {
    let lines = |v: Vec<DiffLine>, more: usize| {
        let extra = v.len().saturating_sub(MAX_PREVIEW_LINES);
        let v: Vec<DiffLine> = v.into_iter().take(MAX_PREVIEW_LINES).map(|l| DiffLine { kind: l.kind, text: clip_line(&l.text) }).collect();
        (v, more.saturating_add(extra))
    };
    match p {
        Preview::Write { path, bytes, replaces, lines: l, more } => {
            let (l, more) = lines(l, more);
            Preview::Write { path: clip_line(&path), bytes, replaces, lines: l, more }
        }
        Preview::Move { moves, more } => {
            let more = more.saturating_add(moves.len().saturating_sub(MAX_PREVIEW_LINES));
            Preview::Move { moves: moves.into_iter().take(MAX_PREVIEW_LINES).map(clip_pair).collect(), more }
        }
        Preview::Delete { path, what } => Preview::Delete { path: clip_line(&path), what: clip_line(&what) },
        Preview::Args { fields, code, more } => {
            let more = more.saturating_add(fields.len().saturating_sub(MAX_PREVIEW_LINES));
            let code = code.map(|c| {
                let mut out: String = c.lines().take(MAX_PREVIEW_LINES).map(clip_line).collect::<Vec<_>>().join("\n");
                if out.len() > MAX_CODE_CHARS {
                    let mut end = MAX_CODE_CHARS;
                    while !out.is_char_boundary(end) {
                        end -= 1;
                    }
                    out.truncate(end);
                    out.push('…');
                } else if c.lines().count() > MAX_PREVIEW_LINES {
                    out.push_str("\n…");
                }
                out
            });
            Preview::Args { fields: fields.into_iter().take(MAX_PREVIEW_LINES).map(clip_pair).collect(), code, more }
        }
    }
}

/// The undo ops for one call: what the tool journaled, plus ops derived from reported
/// changes it didn't journal (plugins). Returns `false` if some change can't be undone.
pub(crate) fn undo_ops(mut ops: Vec<UndoOp>, changes: &[FileChange]) -> (Vec<UndoOp>, bool) {
    let mut complete = true;
    for change in changes {
        let covered = ops.iter().any(|op| match (change, op) {
            (FileChange::Created(p), UndoOp::Created { path, .. }) => p == path,
            (FileChange::Modified(p), UndoOp::Replaced { path, .. } | UndoOp::Created { path, .. }) => p == path,
            (FileChange::Deleted(p), UndoOp::Deleted { path, .. }) => p == path,
            (FileChange::Moved { from, to }, UndoOp::Moved { from: f, to: t }) => from == f && to == t,
            _ => false,
        });
        if covered {
            continue;
        }
        match change {
            // fingerprint it now, so undo only deletes it if nobody changed it since
            FileChange::Created(p) => match Fingerprint::of_file(p) {
                Some(fp) => ops.push(UndoOp::Created { path: p.clone(), fingerprint: Some(fp) }),
                None => complete = false,
            },
            FileChange::Moved { from, to } => ops.push(UndoOp::Moved { from: from.clone(), to: to.clone() }),
            // no copy of the old content
            FileChange::Modified(_) | FileChange::Deleted(_) => complete = false,
        }
    }
    (ops, complete)
}

/// One reply's undo journal.
#[derive(Debug, Default, Clone)]
pub struct TurnJournal {
    /// Oldest first.
    pub ops: Vec<UndoOp>,
    /// Every change of the reply is in `ops`.
    pub complete: bool,
    pub undone: bool,
}

impl TurnJournal {
    pub fn undoable(&self) -> bool {
        self.complete && !self.undone && !self.ops.is_empty()
    }
}

/// Undo journals by assistant entry id. In memory for now; the ops serialize with the same
/// JSON shape as `yah_store::UndoOp`, for persisting.
#[derive(Debug, Default)]
pub struct Journals {
    by_entry: BTreeMap<u64, TurnJournal>,
}

impl Journals {
    pub fn get(&self, entry: u64) -> Option<&TurnJournal> {
        self.by_entry.get(&entry)
    }

    fn add(&mut self, entry: u64, ops: Vec<UndoOp>, complete: bool) -> &TurnJournal {
        let j = self.by_entry.entry(entry).or_insert_with(|| TurnJournal { complete: true, ..TurnJournal::default() });
        j.ops.extend(ops);
        j.complete &= complete;
        j
    }
}

impl Engine {
    /// A turn reported undo ops for entry `id`.
    pub(crate) fn add_undo(&mut self, id: u64, ops: Vec<UndoOp>, complete: bool) {
        let j = self.journals.add(id, ops, complete);
        let (undoable, undone) = (j.undoable(), j.undone);
        self.set_undo_flags(id, undoable, undone);
    }

    /// The chat holding entry `id` (among loaded chats) and the entry.
    fn locate(&mut self, id: u64) -> Option<(String, &mut yah_protocol::Entry)> {
        self.live.iter_mut().find_map(|(chat, l)| l.entries.iter_mut().rev().find(|e| e.id == id).map(|e| (chat.clone(), e)))
    }

    fn set_undo_flags(&mut self, id: u64, undoable: bool, undone: bool) {
        let changed = self.locate(id).and_then(|(chat, e)| {
            let changed = e.undoable != undoable || e.undone != undone;
            e.undoable = undoable;
            e.undone = undone;
            changed.then_some(chat)
        });
        if let Some(chat) = changed {
            self.send_viewers(&chat, ServerMsg::EntryUndo { id, undoable, undone });
        }
    }

    /// A client asked to undo entry `entry`'s file changes.
    pub(crate) fn undo(&mut self, client: ClientId, entry: u64) {
        let refuse = |text: String| ServerMsg::Notice { level: NoticeLevel::Error, text };
        let Some(chat) = self.locate(entry).map(|(chat, _)| chat) else {
            self.send_to(client, refuse("That reply isn't loaded; open its chat first.".into()));
            return;
        };
        if self.chat_busy(&chat) {
            self.send_to(client, refuse("Can't undo while a reply is running in this chat; stop it or wait.".into()));
            return;
        }
        let Some(j) = self.journals.get(entry).cloned() else {
            self.send_to(client, refuse("That reply didn't change any files.".into()));
            return;
        };
        if j.undone {
            self.send_to(client, refuse("Those changes were already undone.".into()));
            return;
        }
        if !j.complete {
            self.send_to(client, refuse("Some of that reply's changes can't be undone, so nothing was changed.".into()));
            return;
        }
        let Some(workspace) = self.live.get(&chat).map(|l| PathBuf::clone(&l.workspace)) else { return };
        match yah_tools::undo::undo(&j.ops, &workspace) {
            Ok(report) => {
                if let Some(j) = self.journals.by_entry.get_mut(&entry) {
                    j.undone = true;
                }
                self.set_undo_flags(entry, false, true);
                let n = report.done.len();
                let text = format!("Undid {n} change{}: {}.", if n == 1 { "" } else { "s" }, report.done.join("; "));
                self.send_viewers(&chat, ServerMsg::Notice { level: NoticeLevel::Info, text });
            }
            Err(e) => self.send_to(client, refuse(format!("Can't undo: {e}."))),
        }
    }
}

#[cfg(test)]
mod tests {
    use yah_protocol::{LineKind, MAX_PREVIEW_LINE_CHARS};

    use super::*;

    #[test]
    fn previews_are_bounded() {
        let long = "x".repeat(10_000);
        let p = bound_preview(Preview::Write {
            path: long.clone(),
            bytes: 1,
            replaces: false,
            lines: vec![DiffLine { kind: LineKind::Added, text: long.clone() }; 100],
            more: 5,
        });
        let Preview::Write { path, lines, more, .. } = p else { panic!() };
        assert!(path.chars().count() <= MAX_PREVIEW_LINE_CHARS + 1);
        assert_eq!((lines.len(), more), (MAX_PREVIEW_LINES, 65));
        let p = bound_preview(Preview::Args { fields: vec![(long.clone(), long.clone())], code: Some(format!("{long}\n").repeat(100)), more: 0 });
        let Preview::Args { fields, code, .. } = p else { panic!() };
        assert!(fields[0].1.chars().count() <= MAX_PREVIEW_LINE_CHARS + 1);
        assert!(code.unwrap().len() <= MAX_CODE_CHARS + 4);
    }

    #[test]
    fn undo_ops_cover_unjournaled_changes() {
        let d = tempfile::tempdir().unwrap();
        let chart = d.path().join("chart.png");
        std::fs::write(&chart, "png").unwrap();
        // a plugin created a file and moved one: both undoable without a journal
        let (ops, complete) = undo_ops(vec![], &[FileChange::Created(chart.clone()), FileChange::Moved { from: "/w/a".into(), to: "/w/b".into() }]);
        assert!(complete);
        assert_eq!(ops[0], UndoOp::Created { path: chart.clone(), fingerprint: Some(Fingerprint::of_bytes(b"png")) });
        // a modification without a backup is not
        let (_, complete) = undo_ops(vec![], &[FileChange::Modified(chart.clone())]);
        assert!(!complete);
        // journaled changes are not doubled
        let journaled = vec![UndoOp::Replaced { path: chart.clone(), backup: "/b".into(), fingerprint: None }];
        let (ops, complete) = undo_ops(journaled.clone(), &[FileChange::Modified(chart)]);
        assert_eq!((ops, complete), (journaled, true));
    }
}
