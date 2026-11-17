//! The layout on the wire (13.11+, cap [`CAP_LAYOUT`]): what the interface shows and in
//! what order (see [`byob_types::layout`]).
//!
//! The engine owns the one layout: it sends [`crate::ServerMsg::Layout`] after the Welcome
//! and whenever it changes (a client's [`crate::ClientMsg::SetLayout`], or the model's
//! `ui_layout` tool), to every client that lists the cap.

pub use byob_types::layout::{ELEMENTS, Element, HEADER, Layout, PRESETS, REGIONS, REPLY, Region, Show, TOOLBAR, element, region, view_id};

/// Cap: the peer handles layout messages.
pub const CAP_LAYOUT: &str = "layout";
