//! yah's persistence: one SQLite file (`yah.db`) for chats, messages, tool steps,
//! always-allow grants and the undo journal, plus `settings.json` and `secrets.json`.
//!
//! - [`Paths`] finds the directories (`YAH_HOME` overrides them).
//! - [`Store`] is the database. It is `Send` but not `Sync`: the engine owns it on its
//!   state thread. Every call returns a typed [`StoreError`]; a corrupt or too-new
//!   database is reported, never panicked on.
//! - [`Settings`] / [`Secrets`] are forgiving JSON files written atomically.
//!
//! The row types in [`model`] are this crate's own; the engine maps protocol and tool
//! types to them, so the on-disk format is independent of the wire format.
//!
//! ```
//! use yah_store::{NewMessage, Store};
//! let store = Store::open_in_memory()?;
//! let chat = store.create_chat("c1", "New chat", std::path::Path::new("/tmp"))?;
//! store.append_message(&chat.id, &NewMessage::user("hello"))?;
//! let reply = store.append_message(&chat.id, &NewMessage::assistant_streaming(None))?;
//! store.append_message_text(reply, "Hi", "")?; // batched stream deltas
//! store.finish_message(reply, None, None)?;
//! assert_eq!(store.messages("c1")?.len(), 2);
//! # Ok::<(), yah_store::StoreError>(())
//! ```
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

mod error;
mod files;
mod grants;
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

pub use error::{Result, StoreError};
pub use files::Loaded;
pub use model::{
    Chat, ChatCursor, ChatPage, GrantScope, MessageId, MessageStats, Millis, NewMessage, NewStep, Role, Step, StepAccess, StepChange, StepResult,
    StoredMessage, UndoOp,
};
pub use paths::{HOME_ENV, Paths};
pub use schema::LATEST as SCHEMA_VERSION;
pub use settings::{InferenceSettings, SECRETS_MODE, Secrets, Settings, Theme};
pub use steps::MAX_STEP_OUTPUT;
pub use store::{BUSY_TIMEOUT, MAX_PAGE, Store};
