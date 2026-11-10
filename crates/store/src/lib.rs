//! yah's persistence: chats, messages, tool steps, subagent runs, context compactions, always-allow grants
//! and the undo journal behind the [`ChatStore`] trait, plus `settings.json` and `secrets.json`.
//!
//! - [`Paths`] finds the directories (`YAH_HOME` overrides them).
//! - [`ChatStore`] is what the engine talks to; [`open_backend`] picks the implementation
//!   from `settings.storage`:
//!   - [`Store`] (`"sqlite"`, the default): one SQLite file, `yah.db`. A corrupt or too-new
//!     database is reported, never panicked on.
//!   - [`FolderStore`] (`"folder"`): one folder per chat with an append-only
//!     `events.jsonl` (canonical) and a generated `chat.md` — greppable, syncable files.
//!
//!   Both are `Send`; the engine owns its store on its state thread. Every call returns a
//!   typed [`StoreError`]. [`export`] copies everything from one backend to another.
//! - [`Settings`] / [`Secrets`] are forgiving JSON files written atomically.
//!
//! The row types in [`model`] are this crate's own; the engine maps protocol and tool
//! types to them, so the on-disk format is independent of the wire format.
//!
//! ```
//! use yah_store::{ChatStore, NewMessage, Store};
//! let store: Box<dyn ChatStore> = Box::new(Store::open_in_memory()?);
//! let chat = store.create_chat("c1", "New chat", std::path::Path::new("/tmp"))?;
//! store.append_message(&chat.id, &NewMessage::user("hello"))?;
//! let reply = store.append_message(&chat.id, &NewMessage::assistant_streaming(None))?;
//! store.append_message_text(reply, "Hi", "")?; // batched stream deltas
//! store.finish_message(reply, None, None)?;
//! assert_eq!(store.messages("c1")?.len(), 2);
//! # Ok::<(), yah_store::StoreError>(())
//! ```
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

mod agent_runs;
mod backend;
mod compactions;
mod error;
mod files;
mod folder;
mod grants;
mod import;
mod journal;
mod messages;
pub mod model;
mod paths;
mod schema;
mod settings;
mod steps;
mod store;

#[cfg(test)]
mod tests;

pub use agent_runs::MAX_AGENT_TASK;
pub use backend::{ChatExport, ChatStore, ExportReport, ExportedAgentRun, ExportedCompaction, export, folder_root, open_backend};
pub use compactions::MAX_SUMMARY;
pub use error::{Result, StoreError};
pub use files::Loaded;
pub use folder::FolderStore;
pub use model::{
    AgentRun, Chat, ChatCursor, ChatPage, Compaction, GrantScope, MessageId, MessageStats, Millis, NewAgentRun, NewCompaction, NewMessage, NewStep, Role, Step,
    StepAccess, StepChange, StepResult, StoredMessage, UndoOp, agent_run_id_ok,
};
pub use paths::{HOME_ENV, Paths};
pub use schema::LATEST as SCHEMA_VERSION;
pub use settings::{CompactionSettings, InferenceSettings, ModelRole, SECRETS_MODE, Secrets, Settings, StorageKind, SubagentSettings, Theme};
pub use steps::MAX_STEP_OUTPUT;
pub use store::{BUSY_TIMEOUT, MAX_PAGE, Store};
