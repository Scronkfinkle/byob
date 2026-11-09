//! [`ChatStore`]: the storage interface the engine talks to, implemented by the SQLite
//! [`Store`] and the plain-files [`FolderStore`]; plus [`open_backend`] and [`export`].

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::folder::FolderStore;
use crate::model::{Chat, ChatCursor, ChatPage, GrantScope, MessageId, MessageStats, NewMessage, NewStep, Step, StepResult, StoredMessage, UndoOp};
use crate::paths::Paths;
use crate::settings::{Settings, StorageKind};
use crate::store::{MAX_PAGE, Store};

/// Chats, messages (including streamed updates), tool steps, always-allow grants and the
/// undo journal. Method names, types and semantics are those of the original SQLite
/// [`Store`]; see its inherent methods for the details of each.
///
/// Implementations are `Send` but need not be `Sync`: the engine owns its store on its
/// state thread. Message ids are only meaningful to the store that issued them.
pub trait ChatStore: Send {
    /// Which backend this is.
    fn kind(&self) -> StorageKind;
    /// The database file or the root folder.
    fn path(&self) -> &Path;

    // ---- chats ----
    fn create_chat(&self, id: &str, title: &str, workspace: &Path) -> Result<Chat>;
    fn chat(&self, id: &str) -> Result<Option<Chat>>;
    /// Most recently active first, `limit` (≤ [`MAX_PAGE`]) at a time, keyset-paginated.
    fn list_chats(&self, limit: usize, after: Option<&ChatCursor>) -> Result<ChatPage>;
    /// Pinned chats, most recently pinned first.
    fn pinned_chats(&self) -> Result<Vec<Chat>>;
    fn rename_chat(&self, id: &str, title: &str) -> Result<()>;
    fn set_chat_workspace(&self, id: &str, workspace: &Path) -> Result<()>;
    fn pin_chat(&self, id: &str, pinned: bool) -> Result<()>;
    /// Delete a chat with everything in it; returns the task ids whose journals went with it.
    fn delete_chat(&self, id: &str) -> Result<Vec<String>>;

    // ---- messages ----
    fn append_message(&self, chat_id: &str, m: &NewMessage) -> Result<MessageId>;
    /// Append batched stream deltas (every ~250 ms, not per token).
    fn append_message_text(&self, id: MessageId, content: &str, reasoning: &str) -> Result<()>;
    fn update_message(&self, id: MessageId, content: &str, reasoning: &str, done: bool) -> Result<()>;
    fn finish_message(&self, id: MessageId, error: Option<&str>, stats: Option<&MessageStats>) -> Result<()>;
    fn set_message_undone(&self, id: MessageId, undone: bool) -> Result<()>;
    /// Close every reply still streaming (after a crash), with `error`. Returns how many.
    fn finish_interrupted(&self, error: &str) -> Result<usize>;
    fn message(&self, id: MessageId) -> Result<Option<StoredMessage>>;
    fn messages(&self, chat_id: &str) -> Result<Vec<StoredMessage>>;
    fn delete_messages_from(&self, chat_id: &str, from: MessageId) -> Result<usize>;

    // ---- steps ----
    fn add_step(&self, message_id: MessageId, step: &NewStep) -> Result<i64>;
    fn finish_step(&self, message_id: MessageId, seq: i64, result: &StepResult) -> Result<()>;
    fn steps(&self, message_id: MessageId) -> Result<Vec<Step>>;
    fn delete_steps(&self, message_id: MessageId) -> Result<usize>;

    // ---- grants ----
    fn grant(&self, scope: &GrantScope, tool: &str) -> Result<()>;
    fn revoke(&self, scope: &GrantScope, tool: &str) -> Result<bool>;
    fn is_allowed(&self, chat_id: &str, tool: &str) -> Result<bool>;
    fn grants(&self, scope: &GrantScope) -> Result<Vec<String>>;

    // ---- undo journal ----
    fn add_undo_op(&self, task_id: &str, op: &UndoOp) -> Result<i64>;
    fn undo_ops(&self, task_id: &str) -> Result<Vec<UndoOp>>;
    fn delete_undo_ops(&self, task_id: &str) -> Result<usize>;

    // ---- interop ----
    /// Everything about one chat, for [`export`]. `None` if there's no such chat.
    fn export_chat(&self, id: &str) -> Result<Option<ChatExport>> {
        let Some(chat) = self.chat(id)? else { return Ok(None) };
        let messages = self.messages(id)?;
        let grants = self.grants(&GrantScope::Chat(id.to_owned()))?;
        let mut undo = Vec::new();
        for m in &messages {
            if let Some(t) = &m.task_id
                && !undo.iter().any(|(seen, _): &(String, Vec<UndoOp>)| seen == t)
            {
                let ops = self.undo_ops(t)?;
                if !ops.is_empty() {
                    undo.push((t.clone(), ops));
                }
            }
        }
        Ok(Some(ChatExport { chat, messages, grants, undo }))
    }

    /// Recreate a chat exactly (timestamps, flags, steps, grants, undo journal). Message ids
    /// in `chat` are ignored; new ones are issued. Fails with [`crate::StoreError::Exists`]
    /// (or a constraint error) if the chat id is taken.
    fn import_chat(&self, chat: &ChatExport) -> Result<()>;
}

/// One chat with everything that belongs to it, backend-neutral.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatExport {
    pub chat: Chat,
    /// In order, each with its steps.
    pub messages: Vec<StoredMessage>,
    /// Tools always allowed in this chat.
    pub grants: Vec<String>,
    /// Undo journals of the chat's tasks, by first appearance.
    pub undo: Vec<(String, Vec<UndoOp>)>,
}

/// What [`export`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportReport {
    pub chats: usize,
    pub messages: usize,
    /// Chats left alone because the destination already had them (ids are global).
    pub skipped: Vec<String>,
    pub global_grants: usize,
}

/// Copy every chat (and the global grants) from `from` into `to`; chats `to` already has
/// are skipped, so running it twice is harmless. Undo entries of tasks no message refers to
/// are not copied.
pub fn export(from: &dyn ChatStore, to: &mut dyn ChatStore) -> Result<ExportReport> {
    let mut report = ExportReport::default();
    for tool in from.grants(&GrantScope::Global)? {
        to.grant(&GrantScope::Global, &tool)?;
        report.global_grants += 1;
    }
    let mut after: Option<ChatCursor> = None;
    loop {
        let page = from.list_chats(MAX_PAGE, after.as_ref())?;
        for c in &page.chats {
            if to.chat(&c.id)?.is_some() {
                report.skipped.push(c.id.clone());
                continue;
            }
            let Some(exp) = from.export_chat(&c.id)? else { continue };
            to.import_chat(&exp)?;
            report.chats += 1;
            report.messages += exp.messages.len();
        }
        match page.next {
            Some(n) => after = Some(n),
            None => break,
        }
    }
    Ok(report)
}

/// Expand a leading `~/` (or a bare `~`) to the home directory.
fn expand_home(p: &Path) -> PathBuf {
    let Ok(rest) = p.strip_prefix("~") else { return p.to_owned() };
    match directories::BaseDirs::new() {
        Some(b) => b.home_dir().join(rest),
        None => p.to_owned(),
    }
}

/// Root folder the folder backend uses for these settings.
pub fn folder_root(settings: &Settings, paths: &Paths) -> PathBuf {
    settings.chats_folder.as_deref().map(expand_home).unwrap_or_else(|| paths.chats_folder())
}

/// Open the backend `settings.storage` selects: `<data>/yah.db`, or the folder store at
/// `settings.chats_folder` (default `<data>/chats-folder`).
pub fn open_backend(settings: &Settings, paths: &Paths) -> Result<Box<dyn ChatStore>> {
    Ok(match settings.storage {
        StorageKind::Sqlite => Box::new(Store::open(paths.db())?),
        StorageKind::Folder => Box::new(FolderStore::open(folder_root(settings, paths))?),
    })
}

impl ChatStore for Store {
    fn kind(&self) -> StorageKind {
        StorageKind::Sqlite
    }
    fn path(&self) -> &Path {
        Store::path(self)
    }
    fn create_chat(&self, id: &str, title: &str, workspace: &Path) -> Result<Chat> {
        Store::create_chat(self, id, title, workspace)
    }
    fn chat(&self, id: &str) -> Result<Option<Chat>> {
        Store::chat(self, id)
    }
    fn list_chats(&self, limit: usize, after: Option<&ChatCursor>) -> Result<ChatPage> {
        Store::list_chats(self, limit, after)
    }
    fn pinned_chats(&self) -> Result<Vec<Chat>> {
        Store::pinned_chats(self)
    }
    fn rename_chat(&self, id: &str, title: &str) -> Result<()> {
        Store::rename_chat(self, id, title)
    }
    fn set_chat_workspace(&self, id: &str, workspace: &Path) -> Result<()> {
        Store::set_chat_workspace(self, id, workspace)
    }
    fn pin_chat(&self, id: &str, pinned: bool) -> Result<()> {
        Store::pin_chat(self, id, pinned)
    }
    fn delete_chat(&self, id: &str) -> Result<Vec<String>> {
        Store::delete_chat(self, id)
    }
    fn append_message(&self, chat_id: &str, m: &NewMessage) -> Result<MessageId> {
        Store::append_message(self, chat_id, m)
    }
    fn append_message_text(&self, id: MessageId, content: &str, reasoning: &str) -> Result<()> {
        Store::append_message_text(self, id, content, reasoning)
    }
    fn update_message(&self, id: MessageId, content: &str, reasoning: &str, done: bool) -> Result<()> {
        Store::update_message(self, id, content, reasoning, done)
    }
    fn finish_message(&self, id: MessageId, error: Option<&str>, stats: Option<&MessageStats>) -> Result<()> {
        Store::finish_message(self, id, error, stats)
    }
    fn set_message_undone(&self, id: MessageId, undone: bool) -> Result<()> {
        Store::set_message_undone(self, id, undone)
    }
    fn finish_interrupted(&self, error: &str) -> Result<usize> {
        Store::finish_interrupted(self, error)
    }
    fn message(&self, id: MessageId) -> Result<Option<StoredMessage>> {
        Store::message(self, id)
    }
    fn messages(&self, chat_id: &str) -> Result<Vec<StoredMessage>> {
        Store::messages(self, chat_id)
    }
    fn delete_messages_from(&self, chat_id: &str, from: MessageId) -> Result<usize> {
        Store::delete_messages_from(self, chat_id, from)
    }
    fn add_step(&self, message_id: MessageId, step: &NewStep) -> Result<i64> {
        Store::add_step(self, message_id, step)
    }
    fn finish_step(&self, message_id: MessageId, seq: i64, result: &StepResult) -> Result<()> {
        Store::finish_step(self, message_id, seq, result)
    }
    fn steps(&self, message_id: MessageId) -> Result<Vec<Step>> {
        Store::steps(self, message_id)
    }
    fn delete_steps(&self, message_id: MessageId) -> Result<usize> {
        Store::delete_steps(self, message_id)
    }
    fn grant(&self, scope: &GrantScope, tool: &str) -> Result<()> {
        Store::grant(self, scope, tool)
    }
    fn revoke(&self, scope: &GrantScope, tool: &str) -> Result<bool> {
        Store::revoke(self, scope, tool)
    }
    fn is_allowed(&self, chat_id: &str, tool: &str) -> Result<bool> {
        Store::is_allowed(self, chat_id, tool)
    }
    fn grants(&self, scope: &GrantScope) -> Result<Vec<String>> {
        Store::grants(self, scope)
    }
    fn add_undo_op(&self, task_id: &str, op: &UndoOp) -> Result<i64> {
        Store::add_undo_op(self, task_id, op)
    }
    fn undo_ops(&self, task_id: &str) -> Result<Vec<UndoOp>> {
        Store::undo_ops(self, task_id)
    }
    fn delete_undo_ops(&self, task_id: &str) -> Result<usize> {
        Store::delete_undo_ops(self, task_id)
    }
    fn import_chat(&self, chat: &ChatExport) -> Result<()> {
        Store::import_chat(self, chat)
    }
}
