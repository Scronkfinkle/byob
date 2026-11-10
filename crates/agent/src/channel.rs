//! The plugin channel: free-form messages between clients and a plugin's handler in the
//! engine ([`ClientMsg::Plugin`](byob_protocol::ClientMsg::Plugin) /
//! [`ServerMsg::Plugin`](byob_protocol::ServerMsg::Plugin)), used only when both sides list
//! [`CAP_PLUGIN_CHANNEL`](byob_protocol::version::CAP_PLUGIN_CHANNEL).
//!
//! The engine routes a client's message to the handler registered for its plugin id and
//! sends back what the handler answers. No handlers are built in yet; long-lived plugin
//! backends will register here (forwarding to their process), as will native built-ins.
//! A handler runs on the engine thread: it must answer quickly and hand slow work to a
//! thread of its own.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

/// What a handler wants sent.
#[derive(Debug, Clone, PartialEq)]
pub enum PluginOut {
    /// To the client that sent the message.
    Reply(Value),
    /// To every attached client that takes plugin messages.
    Broadcast(Value),
}

/// A plugin's engine-side end of the channel.
pub trait PluginHandler: Send + Sync {
    /// Client `client` sent `msg` (already size-checked).
    fn handle(&self, client: u64, msg: Value) -> Vec<PluginOut>;
}

/// The handlers, by plugin id.
#[derive(Clone, Default)]
pub struct PluginChannels {
    handlers: BTreeMap<String, Arc<dyn PluginHandler>>,
}

impl PluginChannels {
    /// Route messages for plugin `plugin` to `handler` (replacing an earlier one).
    pub fn register(&mut self, plugin: &str, handler: Arc<dyn PluginHandler>) -> Result<(), String> {
        if !byob_types::config::plugin_id_ok(plugin) {
            return Err(format!("{plugin:?} isn't a plugin id"));
        }
        self.handlers.insert(plugin.to_owned(), handler);
        Ok(())
    }

    pub(crate) fn get(&self, plugin: &str) -> Option<Arc<dyn PluginHandler>> {
        self.handlers.get(plugin).cloned()
    }
}
