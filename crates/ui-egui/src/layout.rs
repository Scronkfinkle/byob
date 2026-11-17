//! The layout on this side (`byob_protocol::layout`): the engine's, as it last sent it,
//! kept in egui's memory for the frame so whatever draws an element can ask whether to.
//! An engine without the `layout` cap leaves the default: everything as it always was.

use std::sync::Arc;

use byob_protocol::layout::{Layout, Show};
use egui::{Context, Id, Rect};

fn key() -> Id {
    Id::unique("byob-layout")
}

fn header_key() -> Id {
    Id::unique("byob-layout-header-rect")
}

/// Make `layout` the one this frame draws with.
pub fn store(ctx: &Context, layout: &Arc<Layout>) {
    ctx.data_mut(|d| d.insert_temp(key(), Arc::clone(layout)));
}

/// The layout in use.
pub fn get(ctx: &Context) -> Arc<Layout> {
    ctx.data(|d| d.get_temp::<Arc<Layout>>(key())).unwrap_or_default()
}

/// How element `id` is shown.
pub fn show(ctx: &Context, id: &str) -> Show {
    get(ctx).show(id)
}

/// Whether element `id` is drawn now (`hovered`: the pointer is on what it belongs to).
pub fn visible(ctx: &Context, id: &str, hovered: bool) -> bool {
    get(ctx).visible(id, hovered)
}

/// Remember where the header was drawn (its elements on hover show while the pointer is in it).
pub fn set_header_rect(ctx: &Context, rect: Rect) {
    ctx.data_mut(|d| d.insert_temp(header_key(), rect));
}

/// The pointer is on the header (as last drawn).
pub fn header_hovered(ctx: &Context) -> bool {
    let rect = ctx.data(|d| d.get_temp::<Rect>(header_key()));
    rect.zip(ctx.pointer_hover_pos()).is_some_and(|(r, p)| r.contains(p))
}

/// Element `id` of the header is drawn now.
pub fn in_header(ctx: &Context, id: &str) -> bool {
    let l = get(ctx);
    match l.show(id) {
        Show::Hover => header_hovered(ctx),
        s => s.visible(false),
    }
}
