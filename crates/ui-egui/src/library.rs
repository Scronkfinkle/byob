//! Two pages over every chat's artifacts, because they are two different things:
//! - **Apps**: what byob built that runs (HTML apps). A grid of apps; opening one shows the
//!   app itself, full page. A row on top starts something new.
//! - **Library**: what you keep (documents, images, charts, data files). A grid of cards
//!   with tabs by kind; images show as themselves.
//!
//! Underneath both are artifacts with versions; only how they are presented differs.

use std::collections::{BTreeSet, HashMap};

use byob_protocol::{ArtifactKind, ClientMsg, LibraryItem, ServerMsg};
use egui::{Color32, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};

use crate::icons::{self, Paint};
use crate::theme::Tokens;
use crate::{glass, tip};

/// Which page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Apps,
    Library,
}

/// The tabs (which ones a page shows: [`Mode::tabs`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    All,
    Pinned,
    Images,
    Documents,
    Data,
}

impl Mode {
    pub fn tabs(self) -> &'static [(Tab, &'static str)] {
        match self {
            Mode::Apps => &[(Tab::All, "All"), (Tab::Pinned, "Pinned")],
            Mode::Library => {
                &[(Tab::All, "All"), (Tab::Images, "Images"), (Tab::Documents, "Documents"), (Tab::Data, "Charts and data"), (Tab::Pinned, "Pinned")]
            }
        }
    }

    fn title(self) -> &'static str {
        match self {
            Mode::Apps => "Apps",
            Mode::Library => "Library",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Mode::Apps => "What byob built for you that runs. Open one to use it.",
            Mode::Library => "What you keep: documents, images, charts and files, from every chat.",
        }
    }

    /// Is `item` this page's kind of thing?
    fn holds(self, item: &LibraryItem) -> bool {
        (item.artifact.kind == ArtifactKind::Html) == (self == Mode::Apps)
    }
}

/// What a page asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Open {
        chat: String,
        id: String,
    },
    TogglePin {
        chat: String,
        id: String,
        title: String,
    },
    /// A new chat with this text ready to send ("Make an app that ").
    Start(String),
    Close,
}

/// "Make something new" on the Apps page: a name, an icon, the words it starts with.
const STARTERS: [(&str, Paint, &str); 5] = [
    ("App", icons::app_window, "Make an app that "),
    ("Document", icons::lines, "Write a document about "),
    ("Chart", icons::artifacts, "Make a chart of "),
    ("Spreadsheet", icons::sliders, "Make a spreadsheet for "),
    ("Website", icons::compose, "Build a one-page website for "),
];

/// An image's preview: asked for, its bytes, or drawn.
enum Thumb {
    Asked,
    Bytes(Vec<u8>),
    Ready(egui::TextureHandle),
    Failed,
}

#[derive(Default)]
pub struct Library {
    items: Vec<LibraryItem>,
    /// The engine answered since the page opened.
    loaded: bool,
    pub mode: Mode,
    pub query: String,
    pub tab: Tab,
    thumbs: HashMap<(String, String), Thumb>,
    out: Vec<ClientMsg>,
    /// "Save a copy": the thing asked for (chat, id, file name, type), then its bytes.
    saving: Option<(String, String, String, String)>,
    to_save: Option<(String, String, Vec<u8>)>,
    exports: crate::export::Exports,
    /// What saving said ("Saved …" / why not), shown under the title for a while.
    pub note: Option<String>,
    /// Waiting for "Delete" to be confirmed: chat, id, title.
    deleting: Option<(String, String, String)>,
}

/// What a card asked for, beyond the page's [`Action`]s.
enum Pick {
    Action(Action),
    Save,
    Reveal,
    Delete,
}

/// Does `item` belong under `tab`?
pub fn in_tab(item: &LibraryItem, tab: Tab, pinned: &BTreeSet<String>) -> bool {
    match tab {
        Tab::All => true,
        Tab::Pinned => pinned.contains(&item.artifact.id),
        Tab::Images => item.artifact.kind == ArtifactKind::Image,
        Tab::Documents => item.artifact.kind == ArtifactKind::Markdown,
        Tab::Data => matches!(item.artifact.kind, ArtifactKind::Chart | ArtifactKind::Text),
    }
}

/// The kind as a word, an icon and a tint for its card.
fn kind_look(t: &Tokens, kind: ArtifactKind) -> (&'static str, Paint, Color32) {
    match kind {
        ArtifactKind::Html => ("App", icons::app_window, Color32::from_rgb(0x6d, 0x5d, 0xf6)),
        ArtifactKind::Markdown => ("Document", icons::lines, t.accent),
        ArtifactKind::Chart => ("Chart", icons::artifacts, Color32::from_rgb(0x1f, 0xa8, 0x7a)),
        ArtifactKind::Image => ("Image", icons::eye, Color32::from_rgb(0xd9, 0x7a, 0x2b)),
        ArtifactKind::Text => ("File", icons::copy, t.text_dim),
    }
}

impl Library {
    /// The engine's answers (the list, a preview's bytes); true when it was one.
    pub fn apply(&mut self, msg: &ServerMsg) -> bool {
        match msg {
            ServerMsg::Library { items } => {
                self.items.clone_from(items);
                self.loaded = true;
            }
            ServerMsg::LibraryFile { chat, id, data, error, .. } => {
                if let Some((c, i, name, mime)) = self.saving.take_if(|(c, i, ..)| c == chat && i == id) {
                    let _ = (c, i);
                    match data.as_deref().and_then(|d| byob_protocol::media::decode(d).ok()) {
                        Some(bytes) => self.to_save = Some((name, mime, bytes)),
                        None => self.note = Some(format!("Couldn't save it: {}", error.as_deref().unwrap_or("no data"))),
                    }
                    return true;
                }
                let thumb = data.as_deref().and_then(|d| byob_protocol::media::decode(d).ok()).map_or(Thumb::Failed, Thumb::Bytes);
                self.thumbs.insert((chat.clone(), id.clone()), thumb);
            }
            _ => return false,
        }
        true
    }

    /// Opening a page: ask for the list again (artifacts change in every chat).
    pub fn ask(&mut self, mode: Mode) -> ClientMsg {
        if self.mode != mode {
            self.tab = Tab::All;
        }
        self.mode = mode;
        self.loaded = false;
        ClientMsg::ListLibrary
    }

    /// Previews to ask the engine for.
    pub fn take(&mut self) -> Vec<ClientMsg> {
        std::mem::take(&mut self.out)
    }

    /// What shows for the page, the tab and the search, in order.
    pub fn shown(&self, pinned: &BTreeSet<String>) -> Vec<&LibraryItem> {
        let q = self.query.trim().to_lowercase();
        self.items
            .iter()
            .filter(|i| self.mode.holds(i) && in_tab(i, self.tab, pinned))
            .filter(|i| q.is_empty() || i.artifact.title.to_lowercase().contains(&q) || i.chat_title.to_lowercase().contains(&q))
            .collect()
    }

    /// The page: title, search, (Apps: what to make), tabs, then the grid.
    pub fn ui(&mut self, ui: &mut Ui, t: &Tokens, pinned: &BTreeSet<String>, now_ms: i64) -> Option<Action> {
        let mut action = None;
        // Esc closes an open menu first (not the page).
        if !egui::Popup::is_any_open(ui.ctx()) && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            return Some(Action::Close);
        }
        if let Some((name, mime, bytes)) = self.to_save.take() {
            self.exports.save(ui.ctx(), &name, &mime, bytes);
        }
        for outcome in self.exports.poll() {
            self.note = Some(match outcome {
                Ok(msg) | Err(msg) => msg,
            });
        }
        self.delete_dialog(ui.ctx(), t);
        let mode = self.mode;
        let side = 28;
        let mine = self.items.iter().filter(|i| mode.holds(i)).count();
        egui::Frame::NONE.inner_margin(egui::Margin { left: side, right: side, top: 18, bottom: 0 }).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(mode.title()).size(t.text_size(22.0)).strong().color(t.text_strong));
                if self.loaded {
                    let word = if mode == Mode::Apps { "app" } else { "item" };
                    ui.label(RichText::new(crate::sidebar::plural(mine, word)).color(t.text_faint));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let hint = if mode == Mode::Apps { "Search apps" } else { "Search the library" };
                    ui.add(egui::TextEdit::singleline(&mut self.query).hint_text(hint).desired_width(220.0));
                });
            });
            ui.label(RichText::new(mode.subtitle()).color(t.text_dim));
            if let Some(note) = &self.note {
                ui.label(RichText::new(note).small().color(t.text_faint));
            }
            if mode == Mode::Apps {
                ui.add_space(14.0);
                ui.label(RichText::new("Make something new").small().color(t.text_faint));
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(10.0, 10.0);
                    for (name, paint, words) in STARTERS {
                        if starter(ui, t, name, paint).clicked() {
                            action = Some(Action::Start(words.to_owned()));
                        }
                    }
                });
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for &(k, name) in mode.tabs() {
                    let on = self.tab == k;
                    let n = self.items.iter().filter(|i| mode.holds(i) && in_tab(i, k, pinned)).count();
                    let label = if n > 0 && k != Tab::All { format!("{name}  {n}") } else { name.to_owned() };
                    if ui.add(egui::Button::selectable(on, RichText::new(label).color(if on { t.text_strong } else { t.text_dim }))).clicked() {
                        self.tab = k;
                    }
                }
            });
            ui.add_space(12.0);
        });
        let shown: Vec<LibraryItem> = self.shown(pinned).into_iter().cloned().collect();
        egui::ScrollArea::vertical().id_salt(("library-grid", mode == Mode::Apps, self.tab as u8)).auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::NONE.inner_margin(egui::Margin { left: side, right: side, top: 0, bottom: 24 }).show(ui, |ui| {
                if !self.loaded {
                    crate::theme::spinner(ui, t, None);
                    return;
                }
                if shown.is_empty() {
                    let (title, sub) = match (mode, self.tab, self.query.trim().is_empty(), mine == 0) {
                        (Mode::Apps, _, _, true) => ("No apps yet", "Ask for one above, or in any chat: \u{201c}make a tip calculator\u{201d}"),
                        (Mode::Library, _, _, true) => ("Nothing here yet", "Documents, images and charts byob makes for you collect here"),
                        (_, Tab::Pinned, true, _) => ("Nothing pinned", "Pin one from its card, or from the pane beside a chat"),
                        (_, _, false, _) => ("Nothing matches", "Try another word, or another tab"),
                        _ => ("None of these yet", "They show up here when a chat makes one"),
                    };
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(title).color(t.text_strong));
                        ui.label(RichText::new(sub).color(t.text_dim));
                    });
                    return;
                }
                let gap = 14.0;
                let width = ui.available_width();
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)] // a few columns
                let cols = ((width + gap) / (200.0 + gap)).floor().max(1.0) as usize;
                #[allow(clippy::cast_precision_loss)] // a few columns
                let card_w = (width - gap * (cols as f32 - 1.0)) / cols as f32;
                for row in shown.chunks(cols) {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        for item in row {
                            let thumb = self.thumb(ui.ctx(), item);
                            match card(ui, t, item, card_w, pinned.contains(&item.artifact.id), now_ms, thumb.as_ref()) {
                                Some(Pick::Action(a)) => action = Some(a),
                                Some(Pick::Save) => self.save_copy(item),
                                Some(Pick::Reveal) => self.out.push(ClientMsg::RevealArtifact { chat: item.chat.clone(), id: item.artifact.id.clone() }),
                                Some(Pick::Delete) => self.deleting = Some((item.chat.clone(), item.artifact.id.clone(), item.artifact.title.clone())),
                                None => {}
                            }
                        }
                    });
                    ui.add_space(gap);
                }
            });
        });
        action
    }

    /// "Save a copy": ask for its newest version; the save dialog opens when it comes.
    fn save_copy(&mut self, item: &LibraryItem) {
        let a = &item.artifact;
        let version = a.latest().map_or(1, |v| v.n);
        self.saving = Some((item.chat.clone(), a.id.clone(), a.filename.clone(), a.content_type().to_owned()));
        self.out.push(ClientMsg::GetLibraryFile { chat: item.chat.clone(), id: a.id.clone(), version });
    }

    /// "Delete" asks first: it's gone for good.
    fn delete_dialog(&mut self, ctx: &egui::Context, t: &Tokens) {
        let Some((_, id, title)) = &self.deleting else { return };
        let live = self.items.iter().any(|i| i.artifact.id == *id && i.live.is_some());
        let body = if live {
            format!("\u{201c}{title}\u{201d} leaves your Library with its saved versions. Your file stays where it is.")
        } else {
            format!("\u{201c}{title}\u{201d} and all its versions are deleted. This can't be undone.")
        };
        let choice =
            crate::menu::Confirm { title: "Delete it for good?", body: &body, action: "Delete", check: None }.show(ctx, t, egui::Id::unique("library-delete"));
        match choice {
            crate::menu::Choice::Waiting => {}
            crate::menu::Choice::Cancel => self.deleting = None,
            crate::menu::Choice::Confirm => {
                if let Some((chat, id, _)) = self.deleting.take() {
                    self.out.push(ClientMsg::DeleteArtifact { chat, id });
                }
            }
        }
    }

    /// An image's preview: asks for it the first time, then draws it once it came.
    fn thumb(&mut self, ctx: &egui::Context, item: &LibraryItem) -> Option<egui::TextureHandle> {
        if item.artifact.kind != ArtifactKind::Image {
            return None;
        }
        let key = (item.chat.clone(), item.artifact.id.clone());
        match self.thumbs.get(&key) {
            None => {
                let version = item.artifact.latest().map_or(1, |v| v.n);
                self.out.push(ClientMsg::GetLibraryFile { chat: key.0.clone(), id: key.1.clone(), version });
                self.thumbs.insert(key, Thumb::Asked);
                None
            }
            Some(Thumb::Bytes(bytes)) => {
                let texture = crate::media::decode_image(bytes)
                    .ok()
                    .map(|img| ctx.load_texture(format!("library-{}-{}", key.0, key.1), img, egui::TextureOptions::LINEAR));
                let thumb = texture.clone().map_or(Thumb::Failed, Thumb::Ready);
                self.thumbs.insert(key, thumb);
                texture
            }
            Some(Thumb::Ready(tex)) => Some(tex.clone()),
            Some(Thumb::Asked | Thumb::Failed) => None,
        }
    }
}

/// A "Make something new" tile.
fn starter(ui: &mut Ui, t: &Tokens, name: &str, paint: Paint) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(112.0, 76.0), Sense::click());
    let over = resp.hovered();
    let p = ui.painter();
    p.rect_filled(rect, t.round(10.0), if over { crate::theme::mix(t.panel_bg, t.text_strong, 0.06) } else { t.panel_bg });
    p.rect_stroke(rect, t.round(10.0), Stroke::new(1.0, if over { t.text_faint } else { t.border }), egui::StrokeKind::Inside);
    paint(p, Rect::from_center_size(rect.center() - vec2(0.0, 10.0), vec2(22.0, 22.0)), if over { t.text_strong } else { t.text_dim });
    p.text(pos2(rect.center().x, rect.bottom() - 16.0), egui::Align2::CENTER_CENTER, name, egui::TextStyle::Body.resolve(ui.style()), t.text);
    tip::note(
        resp.on_hover_cursor(egui::CursorIcon::PointingHand),
        t,
        &format!("New {}", name.to_lowercase()),
        "Starts a chat with the first words written for you",
    )
}

/// One card: a tile (the image itself, or the kind's icon on its tint), then the title
/// and where it came from.
fn card(ui: &mut Ui, t: &Tokens, item: &LibraryItem, w: f32, pinned: bool, now_ms: i64, thumb: Option<&egui::TextureHandle>) -> Option<Pick> {
    let a = &item.artifact;
    let (kind, paint, tint) = kind_look(t, a.kind);
    let h = 168.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    let over = resp.hovered();
    let r = t.round(12.0);
    let p = ui.painter();
    p.rect_filled(rect, r, if over { crate::theme::mix(t.panel_bg, t.text_strong, 0.04) } else { t.panel_bg });
    p.rect_stroke(rect, r, Stroke::new(1.0, if over { t.text_faint } else { t.border }), egui::StrokeKind::Inside);
    let tile = Rect::from_min_max(rect.min + vec2(8.0, 8.0), pos2(rect.right() - 8.0, rect.top() + 100.0));
    match thumb {
        // The picture, cropped to fill the tile.
        Some(tex) => {
            let size = tex.size_vec2();
            let (tw, th) = (tile.width() / size.x.max(1.0), tile.height() / size.y.max(1.0));
            let scale = tw.max(th);
            let shown = vec2(tile.width() / (size.x * scale), tile.height() / (size.y * scale));
            let uv = Rect::from_center_size(pos2(0.5, 0.5), shown);
            egui::Image::new((tex.id(), tile.size())).uv(uv).corner_radius(t.round(8.0)).paint_at(ui, tile);
        }
        None => {
            p.rect_filled(tile, t.round(8.0), tint.gamma_multiply(0.14));
            paint(p, Rect::from_center_size(tile.center(), vec2(30.0, 30.0)), tint);
            let badge = p.layout_no_wrap(kind.to_uppercase(), egui::TextStyle::Small.resolve(ui.style()), tint);
            p.galley(tile.min + vec2(8.0, 6.0), badge, tint);
        }
    }
    let x = rect.left() + 12.0;
    let room = (w - 24.0).max(20.0);
    let line = |text: &str, style: egui::TextStyle, color: Color32| {
        let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), style.resolve(ui.style()), color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(room);
        ui.painter().layout_job(job)
    };
    let title = line(&a.title, egui::TextStyle::Body, t.text_strong);
    let when = a.latest().map(|v| crate::sidebar::ago(v.created_at, now_ms)).unwrap_or_default();
    let origin = match (&item.live, item.chat_gone) {
        (Some(path), _) => format!("Your file · {path}"),
        (None, true) => "From a deleted chat".to_owned(),
        (None, false) => item.chat_title.clone(),
    };
    let from = line(&format!("{origin} · {when}"), egui::TextStyle::Small, t.text_dim);
    let p = ui.painter();
    p.galley(pos2(x, tile.bottom() + 10.0), title, t.text_strong);
    p.galley(pos2(x, tile.bottom() + 32.0), from, t.text_dim);
    let mut out = None;
    if over || pinned {
        let pin_rect = Rect::from_min_size(pos2(tile.right() - 30.0, tile.top() + 4.0), vec2(26.0, 26.0));
        let pr = ui.interact(pin_rect, egui::Id::unique(("library-pin", &a.id)), Sense::click());
        let state = if pr.hovered() { glass::RowState::Hovered } else { glass::RowState::Idle };
        glass::row(ui.painter(), t, pin_rect, state);
        icons::pin(ui.painter(), pin_rect.shrink(6.0), if pinned { t.accent } else { t.text_dim });
        let pr = if pinned {
            tip::note(pr, t, "Unpin from the sidebar", "It stays here")
        } else {
            tip::note(pr, t, "Pin to the sidebar", "One click from any chat")
        };
        if pr.clicked() {
            out = Some(Pick::Action(Action::TogglePin { chat: item.chat.clone(), id: a.id.clone(), title: a.title.clone() }));
        }
        // ⋯: everything you can do with it.
        let more_rect = pin_rect.translate(vec2(-30.0, 0.0));
        let mr = ui.interact(more_rect, egui::Id::unique(("library-more", &a.id)), Sense::click());
        let state = if mr.hovered() { glass::RowState::Hovered } else { glass::RowState::Idle };
        glass::row(ui.painter(), t, more_rect, state);
        icons::more(ui.painter(), more_rect.shrink(6.0), t.text_dim);
        let mr = mr.on_hover_text("More");
        crate::menu::below(&mr, t, 200.0).show(|ui| {
            if let Some(p) = card_menu(ui, t, item, pinned) {
                out = Some(p);
            }
        });
    }
    let how = if item.chat_gone {
        "Its chat was deleted: click to save a copy"
    } else if a.kind == ArtifactKind::Html {
        "Opens the app, full page"
    } else {
        "Opens it beside its chat"
    };
    let resp = tip::note(resp.on_hover_cursor(egui::CursorIcon::PointingHand), t, &a.title, how);
    crate::menu::context(&resp, t, 200.0).show(|ui| {
        if let Some(p) = card_menu(ui, t, item, pinned) {
            out = Some(p);
        }
    });
    if out.is_none() && resp.clicked() {
        out = Some(if item.chat_gone { Pick::Save } else { Pick::Action(Action::Open { chat: item.chat.clone(), id: a.id.clone() }) });
    }
    out
}

/// A card's menu: open it, take it out (save a copy, show its folder), pin, delete.
fn card_menu(ui: &mut Ui, t: &Tokens, item: &LibraryItem, pinned: bool) -> Option<Pick> {
    crate::menu::tighten(ui);
    let a = &item.artifact;
    let mut pick = None;
    if !item.chat_gone && crate::menu::item(ui, t, Some(icons::eye), "Open", None).clicked() {
        pick = Some(Pick::Action(Action::Open { chat: item.chat.clone(), id: a.id.clone() }));
    }
    if crate::menu::item(ui, t, Some(icons::copy), "Save a copy…", None).on_hover_text("Take it out of byob as a file").clicked() {
        pick = Some(Pick::Save);
    }
    if crate::menu::item(ui, t, Some(icons::folder_plus), "Show in Finder", None).on_hover_text("Where byob keeps it on this computer").clicked() {
        pick = Some(Pick::Reveal);
    }
    if crate::menu::item(ui, t, Some(icons::pin), if pinned { "Unpin" } else { "Pin to sidebar" }, None).clicked() {
        pick = Some(Pick::Action(Action::TogglePin { chat: item.chat.clone(), id: a.id.clone(), title: a.title.clone() }));
    }
    crate::menu::separator(ui, t);
    if crate::menu::danger(ui, t, Some(icons::trash), "Delete…", None).clicked() {
        pick = Some(Pick::Delete);
    }
    if pick.is_some() {
        ui.close();
    }
    pick
}

#[cfg(test)]
mod tests {
    use super::*;
    use byob_protocol::{Artifact, ArtifactVersion};

    fn item(id: &str, kind: ArtifactKind, chat: &str) -> LibraryItem {
        LibraryItem {
            chat: chat.into(),
            chat_title: format!("Chat {chat}"),
            workspace: None,
            chat_gone: false,
            live: None,
            artifact: Artifact {
                id: id.into(),
                kind,
                content_type: String::new(),
                title: format!("Title {id}"),
                filename: format!("{id}.txt"),
                versions: vec![ArtifactVersion { n: 1, path: format!("a/{id}"), created_at: 1, note: String::new(), size: 1, turn: String::new() }],
            },
        }
    }

    fn items() -> Vec<LibraryItem> {
        vec![
            item("calc", ArtifactKind::Html, "a"),
            item("notes", ArtifactKind::Markdown, "b"),
            item("sales", ArtifactKind::Chart, "b"),
            item("cat", ArtifactKind::Image, "c"),
        ]
    }

    #[test]
    fn apps_and_the_library_split_what_runs_from_what_you_keep() {
        let mut l = Library::default();
        let none = BTreeSet::new();
        assert_eq!(l.ask(Mode::Apps), ClientMsg::ListLibrary);
        assert!(l.apply(&ServerMsg::Library { items: items() }));
        assert_eq!(l.shown(&none).iter().map(|i| i.artifact.id.as_str()).collect::<Vec<_>>(), ["calc"]);
        l.ask(Mode::Library);
        l.apply(&ServerMsg::Library { items: items() });
        assert_eq!(l.shown(&none).len(), 3, "everything but the app");
        l.tab = Tab::Images;
        assert_eq!(l.shown(&none).iter().map(|i| i.artifact.id.as_str()).collect::<Vec<_>>(), ["cat"]);
        l.tab = Tab::Data;
        assert_eq!(l.shown(&none).iter().map(|i| i.artifact.id.as_str()).collect::<Vec<_>>(), ["sales"]);
        // Switching page resets the tab; search reads titles and chat titles.
        l.ask(Mode::Apps);
        assert_eq!(l.tab, Tab::All);
        l.ask(Mode::Library);
        l.apply(&ServerMsg::Library { items: items() });
        l.query = "chat b".into();
        assert_eq!(l.shown(&none).len(), 2);
    }

    #[test]
    fn images_ask_for_their_preview_once() {
        let ctx = egui::Context::default();
        let mut l = Library::default();
        l.apply(&ServerMsg::Library { items: items() });
        let cat = items().into_iter().find(|i| i.artifact.id == "cat").unwrap();
        assert!(l.thumb(&ctx, &cat).is_none());
        assert!(l.thumb(&ctx, &cat).is_none());
        assert_eq!(l.take(), [ClientMsg::GetLibraryFile { chat: "c".into(), id: "cat".into(), version: 1 }]);
        // A broken answer is remembered (not asked again).
        l.apply(&ServerMsg::LibraryFile { chat: "c".into(), id: "cat".into(), version: 1, data: None, error: Some("gone".into()) });
        assert!(l.thumb(&ctx, &cat).is_none() && l.take().is_empty());
    }

    #[test]
    fn the_apps_page_starts_something_new_and_closes() {
        let ctx = egui::Context::default();
        let mut l = Library::default();
        l.apply(&ServerMsg::Library { items: items() });
        let pinned = BTreeSet::new();
        let run = |l: &mut Library, events: Vec<egui::Event>| {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 700.0))), events, ..Default::default() };
            let mut got = None;
            let mut out = ctx.run_ui(input, |ui| got = l.ui(ui, &Tokens::dark(), &pinned, 10));
            out.textures_delta.clear();
            got
        };
        run(&mut l, vec![]);
        // The first tile under "Make something new" is App.
        let at = pos2(84.0, 140.0);
        let press = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        run(&mut l, vec![egui::Event::PointerMoved(at)]);
        run(&mut l, vec![press(true)]);
        assert_eq!(run(&mut l, vec![press(false)]), Some(Action::Start("Make an app that ".into())));
        let esc = egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        assert_eq!(run(&mut l, vec![esc]), Some(Action::Close));
    }

    /// A thing whose chat was deleted stays: its card says so, a click saves a copy (asks
    /// for its file), and deleting it asks first, then tells the engine.
    #[test]
    fn kept_things_from_deleted_chats_save_a_copy_and_delete_after_asking() {
        let ctx = egui::Context::default();
        let mut l = Library::default();
        l.ask(Mode::Library);
        let mut kept = item("notes", ArtifactKind::Markdown, "gone");
        kept.chat_gone = true;
        l.apply(&ServerMsg::Library { items: vec![kept.clone()] });
        let pinned = BTreeSet::new();
        let frame = |l: &mut Library, events: Vec<egui::Event>| {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 700.0))), events, ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                l.ui(ui, &Tokens::dark(), &pinned, 10);
            });
            out.textures_delta.clear();
            crate::plugins::tests::texts_of(&out)
        };
        let texts = frame(&mut l, vec![]);
        assert!(crate::plugins::tests::has(&texts, "From a deleted chat"), "{texts:?}");
        // Saving a copy asks the engine for its file; the dialog opens once it comes.
        l.save_copy(&kept);
        assert_eq!(l.take(), [ClientMsg::GetLibraryFile { chat: "gone".into(), id: "notes".into(), version: 1 }]);
        l.apply(&ServerMsg::LibraryFile {
            chat: "gone".into(),
            id: "notes".into(),
            version: 1,
            data: Some(byob_protocol::media::encode(b"# hi")),
            error: None,
        });
        assert!(l.to_save.as_ref().is_some_and(|(name, _, bytes)| name == "notes.txt" && bytes == b"# hi"));
        assert!(l.thumbs.is_empty(), "a saved file isn't a preview");
        // Delete asks first; confirming tells the engine.
        l.deleting = Some(("gone".into(), "notes".into(), "Title notes".into()));
        frame(&mut l, vec![]); // a new window measures itself first
        let texts = frame(&mut l, vec![]);
        assert!(crate::plugins::tests::has(&texts, "Delete it for good?"));
        let ok = texts.iter().filter(|t| t.0 == "Delete").map(|t| t.1).next_back().unwrap();
        let press = |pressed| egui::Event::PointerButton { pos: ok.center(), button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        frame(&mut l, vec![egui::Event::PointerMoved(ok.center()), press(true)]);
        frame(&mut l, vec![press(false)]);
        assert!(l.take().contains(&ClientMsg::DeleteArtifact { chat: "gone".into(), id: "notes".into() }));
    }
}
