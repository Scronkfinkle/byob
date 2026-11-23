//! The Projects page: every project in one place, as a list or as cards. Search (name or
//! folder), sort (last active or name), pin (pinned projects sit in the sidebar's Pinned
//! group), open (its latest chat, or a new one), its settings, and a new project.
//!
//! A project gathers chats around what you're working on: a name, a goal (what you're trying
//! to achieve: every chat in it gets it as instructions), and the folders it works in, if
//! any (with none, byob keeps its files for it). The page leads with the goal and says what
//! each project holds. With Spaces, it lists the Space in view, with a switch to see all.

use byob_protocol::workspace::WorkspaceSummary;
use egui::{Rect, RichText, Sense, Stroke, Ui, pos2, vec2};
use serde::{Deserialize, Serialize};

use crate::icons;
use crate::theme::Tokens;

/// How the page is sorted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sort {
    #[default]
    Active,
    Name,
    /// The most chats first.
    Chats,
}

impl Sort {
    const ALL: [Sort; 3] = [Sort::Active, Sort::Name, Sort::Chats];

    fn label(self) -> &'static str {
        match self {
            Sort::Active => "Last active",
            Sort::Name => "Name",
            Sort::Chats => "Most chats",
        }
    }
}

/// Which projects the page shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Show {
    #[default]
    All,
    Pinned,
    /// Those that work in folders of yours.
    Folders,
    /// Active in the last 7 days.
    Recent,
}

impl Show {
    const ALL: [Show; 4] = [Show::All, Show::Pinned, Show::Folders, Show::Recent];

    fn label(self) -> &'static str {
        match self {
            Show::All => "All projects",
            Show::Pinned => "Pinned",
            Show::Folders => "With folders",
            Show::Recent => "Active this week",
        }
    }
}

/// A week, in milliseconds.
const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// How the page shows them: rows with columns, or cards.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum View {
    #[default]
    List,
    Grid,
}

/// Which way a new project starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// The dialog, with nothing chosen yet.
    Choose,
    /// Link a folder you have (the folder picker opens).
    Link,
    /// byob makes a new folder.
    Empty,
}

/// What the page asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Open the project: its latest chat, or a new chat in it.
    Open(String),
    TogglePin(String),
    /// A new chat in it.
    NewChat(String),
    Settings(String),
    /// Its settings, at Remove.
    Remove(String),
    New(Start),
    Close,
}

/// The Space in view, for the page: its name and the projects in it.
pub struct SpaceScope {
    pub name: String,
    pub ids: Vec<String>,
}

/// What the Show filters need to know: the pins, where byob makes folders, the time.
pub struct Facts<'a> {
    pub pinned: &'a [String],
    pub root: &'a str,
    pub now_ms: i64,
}

#[derive(Default)]
pub struct Projects {
    pub query: String,
    pub sort: Sort,
    /// The sort turned around (a second click on its column's header).
    pub reverse: bool,
    pub show: Show,
    pub view: View,
    /// With Spaces: list every Space's projects, not only this one's.
    pub all_spaces: bool,
    /// Put the keyboard in the search field on the next frame (⌘F).
    focus_search: bool,
    /// Where the search field was drawn last (its clear button sits in its right end).
    search_rect: Option<Rect>,
    /// The search had the keyboard last frame (egui lets go of it on Esc before anyone
    /// sees the key, so this is how Esc knows it was pressed in the search).
    search_focused: bool,
}

/// The folders a project works in, as people see them: its own folder when it's one they
/// have (not one byob made for it), then the ones it links.
fn their_folders(w: &WorkspaceSummary, root: &str) -> Vec<String> {
    let root = root.trim_end_matches(['/', '\\']);
    let made = !root.is_empty() && w.folder.starts_with(root) && w.folder[root.len()..].starts_with(['/', '\\']);
    let own = (!made).then(|| w.folder.clone());
    own.into_iter().chain(w.folders.iter().cloned()).collect()
}

/// A goal on one line (its line breaks as spaces), for rows and cards.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What a project holds, in a few words ("byob keeps its files", "1 folder", "3 folders"),
/// and the full paths for its tooltip.
fn holds(w: &WorkspaceSummary, root: &str) -> (String, String) {
    let folders = their_folders(w, root);
    match folders.len() {
        0 => ("byob keeps its files".to_owned(), crate::workspace::short_home(&w.folder)),
        n => (crate::sidebar::plural(n, "folder"), folders.iter().map(|f| crate::workspace::short_home(f)).collect::<Vec<_>>().join("\n")),
    }
}

impl Projects {
    /// The projects shown, in order, for the search, the sort and the Space.
    pub fn shown<'a>(&self, all: &'a [WorkspaceSummary], scope: Option<&SpaceScope>, ctx: &Facts<'_>) -> Vec<&'a WorkspaceSummary> {
        let mut list = self.searched(all, scope);
        list.retain(|w| match self.show {
            Show::All => true,
            Show::Pinned => ctx.pinned.contains(&w.id),
            Show::Folders => !their_folders(w, ctx.root).is_empty(),
            Show::Recent => ctx.now_ms.saturating_sub(w.updated_at) <= WEEK_MS,
        });
        match self.sort {
            Sort::Active => list.sort_by_key(|w| std::cmp::Reverse(w.updated_at)),
            Sort::Name => list.sort_by_key(|w| w.name.to_lowercase()),
            Sort::Chats => list.sort_by_key(|w| (std::cmp::Reverse(w.chats), std::cmp::Reverse(w.updated_at))),
        }
        if self.reverse {
            list.reverse();
        }
        list
    }

    /// The Space's projects that match the search (every word, in its name, goal or
    /// folders, any case), before the Show filter.
    fn searched<'a>(&self, all: &'a [WorkspaceSummary], scope: Option<&SpaceScope>) -> Vec<&'a WorkspaceSummary> {
        let words: Vec<String> = self.query.split_whitespace().map(str::to_lowercase).collect();
        all.iter()
            .filter(|w| self.all_spaces || scope.is_none_or(|s| s.ids.contains(&w.id)))
            .filter(|w| {
                let (name, goal) = (w.name.to_lowercase(), w.goal.to_lowercase());
                let folders: Vec<String> = std::iter::once(&w.folder).chain(&w.folders).map(|f| f.to_lowercase()).collect();
                words.iter().all(|q| name.contains(q) || goal.contains(q) || folders.iter().any(|f| f.contains(q)))
            })
            .collect()
    }

    /// Search or Show narrows the page.
    fn narrowed(&self) -> bool {
        !self.query.trim().is_empty() || self.show != Show::All
    }

    fn clear_filters(&mut self) {
        self.query.clear();
        self.show = Show::All;
    }

    /// Put the keyboard in the search on the next frame (⌘F, Ctrl F elsewhere).
    pub fn find(&mut self) {
        self.focus_search = true;
    }

    /// Restore the view and sort, and which way it goes (remembered per device).
    pub fn set_prefs(&mut self, view: View, sort: Sort, reverse: bool) {
        self.view = view;
        self.sort = sort;
        self.reverse = reverse;
    }

    /// The page: title and New project, the toolbar (search, view, sort), the Space line,
    /// then the projects as rows or cards.
    #[allow(clippy::too_many_arguments)] // a page: its data, the pins, where byob makes folders, the Space, the time
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        t: &Tokens,
        all: &[WorkspaceSummary],
        pinned: &[String],
        root: &str,
        scope: Option<&SpaceScope>,
        now_ms: i64,
    ) -> Option<Action> {
        // Esc goes back to the chat, unless a menu or a dialog over the page has it, or it's
        // in the search (there it clears what's typed, then leaves the field).
        let menus_open = egui::Popup::is_any_open(ui.ctx());
        let behind_dialog = !ui.ctx().memory(|m| m.is_above_modal_layer(ui.layer_id()));
        // Last frame's, then this frame's (the search may not be drawn).
        let searching = std::mem::take(&mut self.search_focused);
        if !menus_open && !behind_dialog && !searching && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            return Some(Action::Close);
        }
        let mut action = None;
        let page_w = (ui.available_width() - 64.0).clamp(240.0, 880.0);
        egui::ScrollArea::vertical().id_salt("projects-page").auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(28.0);
            let extra = ((ui.available_width() - page_w) / 2.0).max(0.0);
            let rect = Rect::from_min_size(ui.cursor().min + vec2(extra, 0.0), vec2(page_w, f32::INFINITY));
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::top_down(egui::Align::Min)), |ui| {
                ui.set_max_width(page_w);
                // The title with how many there are, what the page is, and the one main action.
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(RichText::new("Projects").size(t.text_size(24.0)).strong().color(t.text_strong));
                    if !all.is_empty() {
                        ui.label(RichText::new(all.len().to_string()).size(t.text_size(15.0)).color(t.text_faint));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(a) = new_button(ui, t) {
                            action = Some(a);
                        }
                    });
                });
                ui.add_space(2.0);
                ui.label(
                    RichText::new("Group related chats. Each project gives its chats the same instructions and folders.")
                        .size(t.text_size(13.0))
                        .color(t.text_dim),
                );
                ui.add_space(20.0);
                if all.is_empty() {
                    if let Some(a) = empty_page(ui, t) {
                        action = Some(a);
                    }
                    return;
                }
                self.toolbar(ui, t, page_w, searching);
                let facts = Facts { pinned, root, now_ms };
                let shown = self.shown(all, scope, &facts);
                if let Some(s) = scope {
                    self.space_line(ui, t, s, all.len(), shown.len());
                }
                if self.narrowed() {
                    self.results_line(ui, t, shown.len(), self.searched_total(all, scope));
                }
                ui.add_space(16.0);
                if shown.is_empty() {
                    self.nothing_shown(ui, t);
                    return;
                }
                let a = match self.view {
                    View::List => self.list(ui, t, &shown, pinned, root, page_w, now_ms),
                    View::Grid => grid(ui, t, &shown, pinned, root, page_w, now_ms),
                };
                if a.is_some() {
                    action = a;
                }
            });
            ui.add_space(40.0);
        });
        action
    }

    /// Search on the left (a clear button once you type, ⌘F while it's empty); Show, Sort
    /// and the view on the right. One height, one corner, one border for all of them.
    fn toolbar(&mut self, ui: &mut Ui, t: &Tokens, page_w: f32, searching: bool) {
        // Narrow (a phone): the search across the page, Show, Sort and the view under it
        // (side by side they ran over one another).
        let narrow = page_w < COMPACT;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let id = egui::Id::unique("projects-search");
            let width = if narrow { page_w } else { (page_w - 420.0).clamp(160.0, 420.0) };
            let r = crate::workspace::form_field_margin(ui, t, id, egui::Margin { left: 34, right: 34, top: 9, bottom: 9 }, |frame| {
                egui::TextEdit::singleline(&mut self.query).id(id).hint_text("Search projects").char_limit(200).desired_width(width).frame(frame)
            });
            icons::search(ui.painter(), Rect::from_center_size(pos2(r.rect.left() + 18.0, r.rect.center().y), vec2(15.0, 15.0)), t.text_faint);
            // The right end: × to clear what you typed, or the shortcut while it's empty.
            self.search_rect = Some(r.rect);
            let end = Rect::from_center_size(pos2(r.rect.right() - 18.0, r.rect.center().y), vec2(22.0, 22.0));
            if self.query.is_empty() {
                if !r.has_focus() {
                    let key = if cfg!(target_os = "macos") { "⌘F" } else { "Ctrl F" };
                    let g = ui.painter().layout_no_wrap(key.to_owned(), egui::FontId::proportional(t.text_size(11.0)), t.text_faint);
                    let chip = Rect::from_center_size(pos2(r.rect.right() - 12.0 - g.size().x / 2.0 - 5.0, r.rect.center().y), g.size() + vec2(10.0, 4.0));
                    ui.painter().rect_stroke(chip, 4.0, Stroke::new(1.0, t.separator), egui::StrokeKind::Inside);
                    ui.painter().galley(chip.center() - g.size() / 2.0, g, t.text_faint);
                }
            } else {
                let x = ui.interact(end, id.with("clear"), Sense::click());
                if x.hovered() {
                    ui.painter().rect_filled(end, 5.0, crate::form::soft(t));
                }
                icons::cross(ui.painter(), end.shrink(5.0), if x.hovered() { t.text_strong } else { t.text_faint });
                if x.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Clear the search").clicked() {
                    self.query.clear();
                    r.request_focus();
                }
            }
            if std::mem::take(&mut self.focus_search) {
                r.request_focus();
            }
            if searching && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                if self.query.is_empty() {
                    r.surrender_focus();
                } else {
                    self.query.clear();
                    r.request_focus();
                }
            }
            self.search_focused = ui.ctx().memory(|m| m.has_focus(id));
            let h = r.rect.height().max(34.0);
            if !narrow {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| self.toolbar_menus(ui, t, h, 8.0));
            }
        });
        if narrow {
            ui.add_space(8.0);
            // One row high: a right-to-left layout would otherwise take all the height left.
            let row = vec2(ui.available_width(), 34.0);
            ui.allocate_ui_with_layout(row, egui::Layout::right_to_left(egui::Align::Center), |ui| self.toolbar_menus(ui, t, 34.0, 6.0));
        }
    }

    /// The toolbar's right end, drawn right to left: the view, Sort and Show.
    fn toolbar_menus(&mut self, ui: &mut Ui, t: &Tokens, h: f32, gap: f32) {
        ui.spacing_mut().item_spacing.x = gap;
        view_switch(ui, t, &mut self.view, h);
        let b = menu_button(ui, t, None, self.sort.label(), false, h).on_hover_text("Sort");
        crate::menu::below(&b, t, 180.0).show(|ui| {
            crate::menu::tighten(ui);
            crate::menu::heading(ui, t, "Sort by");
            for s in Sort::ALL {
                if crate::chatbar::choice_row(ui, t, s.label(), self.sort == s).clicked() {
                    self.sort = s;
                    self.reverse = false;
                    ui.close();
                }
            }
        });
        let on = self.show != Show::All;
        let b = menu_button(ui, t, Some(icons::funnel), self.show.label(), on, h).on_hover_text("Show");
        crate::menu::below(&b, t, 200.0).show(|ui| {
            crate::menu::tighten(ui);
            crate::menu::heading(ui, t, "Show");
            for s in Show::ALL {
                if crate::chatbar::choice_row(ui, t, s.label(), self.show == s).clicked() {
                    self.show = s;
                    ui.close();
                }
            }
        });
    }

    /// How many of how many the search and filter leave, and a way back to all of them.
    fn results_line(&mut self, ui: &mut Ui, t: &Tokens, shown: usize, total: usize) {
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            ui.label(RichText::new(format!("{shown} of {}", crate::sidebar::plural(total, "project"))).size(t.text_size(12.5)).color(t.text_faint));
            let r = ui.add(egui::Label::new(RichText::new("Clear filters").size(t.text_size(12.5)).color(t.text_dim)).sense(Sense::click()));
            let r = r.on_hover_cursor(egui::CursorIcon::PointingHand);
            if r.hovered() {
                ui.painter().hline(r.rect.x_range(), r.rect.bottom(), Stroke::new(1.0, t.text_dim));
            }
            if r.clicked() {
                self.clear_filters();
            }
        });
    }

    /// The projects in view before search and Show (the "of" in "2 of 5").
    fn searched_total(&self, all: &[WorkspaceSummary], scope: Option<&SpaceScope>) -> usize {
        all.iter().filter(|w| self.all_spaces || scope.is_none_or(|s| s.ids.contains(&w.id))).count()
    }

    /// Nothing to show: say why, and offer the way out.
    fn nothing_shown(&mut self, ui: &mut Ui, t: &Tokens) {
        ui.add_space(48.0);
        ui.vertical_centered(|ui| {
            let typed = !self.query.trim().is_empty();
            let (title, sub) = match (typed, self.show) {
                (true, _) => {
                    (format!("No projects match \u{201c}{}\u{201d}", one_line(self.query.trim())), "Try another word from a name, goal or folder.".to_owned())
                }
                (false, Show::Pinned) => ("No pinned projects".to_owned(), "Pin a project to keep it at the top of the sidebar.".to_owned()),
                (false, Show::Folders) => ("No projects with folders".to_owned(), "Add a folder in a project's settings to work on your files.".to_owned()),
                (false, Show::Recent) => ("Nothing active this week".to_owned(), "Projects you chat in show up here.".to_owned()),
                (false, Show::All) => ("No projects in this Space".to_owned(), "Show all Spaces, or make one here.".to_owned()),
            };
            ui.label(RichText::new(title).size(t.text_size(15.0)).color(t.text_strong));
            ui.add_space(4.0);
            ui.label(RichText::new(sub).size(t.text_size(13.0)).color(t.text_faint));
            if self.narrowed() {
                ui.add_space(14.0);
                if crate::form::button(ui, t, "Clear filters", true).clicked() {
                    self.clear_filters();
                }
            }
        });
    }

    /// "4 of 6 projects are in Work" and the switch to see them all.
    fn space_line(&mut self, ui: &mut Ui, t: &Tokens, s: &SpaceScope, total: usize, shown: usize) {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let text = if self.all_spaces {
                format!("All Spaces · {}", crate::sidebar::plural(total, "project"))
            } else {
                format!("In {} · {}", s.name, crate::sidebar::plural(shown, "project"))
            };
            ui.label(RichText::new(text).small().color(t.text_faint));
            let switch = if self.all_spaces { format!("Only {}", s.name) } else { "Show all Spaces".to_owned() };
            let r = ui.add(egui::Label::new(RichText::new(switch).small().color(t.accent)).sense(Sense::click()));
            if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                self.all_spaces = !self.all_spaces;
            }
        });
    }

    /// The list: a header whose names sort it, then one row per project.
    #[allow(clippy::too_many_arguments)]
    fn list(&mut self, ui: &mut Ui, t: &Tokens, shown: &[&WorkspaceSummary], pinned: &[String], root: &str, width: f32, now_ms: i64) -> Option<Action> {
        let cols = Columns::new(width);
        // The header: Name and Updated sort (an arrow on the one in use).
        let (rect, _) = ui.allocate_exact_size(vec2(width, 30.0), Sense::hover());
        let small = egui::TextStyle::Small.resolve(ui.style());
        // Each header sorts by its column; a second click turns the order around. The arrow
        // shows the column in use and which way: A to Z is ↑, newest or most first is ↓.
        let mut head = |ui: &mut Ui, x: f32, text: &str, s: Sort, right_edge: bool| {
            let on = s == self.sort;
            let up = (s == Sort::Name) != self.reverse;
            let label = if on { format!("{text} {}", if up { "↑" } else { "↓" }) } else { text.to_owned() };
            let g = ui.painter().layout_no_wrap(label, small.clone(), t.text_faint);
            let left = if right_edge { x - g.size().x } else { x };
            let r = Rect::from_min_size(pos2(left, rect.center().y - g.size().y / 2.0), g.size());
            let resp = ui.interact(r.expand2(vec2(6.0, 6.0)), egui::Id::unique(("projects-head", text)), Sense::click());
            let color = if resp.hovered() {
                t.text_strong
            } else if on {
                t.text_dim
            } else {
                t.text_faint
            };
            if resp.hovered() {
                ui.painter().rect_filled(r.expand2(vec2(6.0, 4.0)), 5.0, crate::form::soft(t));
            }
            ui.painter().galley(r.min, g, color);
            let tip = match s {
                Sort::Name => "Sort by name",
                Sort::Chats => "Sort by number of chats",
                Sort::Active => "Sort by last active",
            };
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked() {
                if on {
                    self.reverse = !self.reverse;
                } else {
                    self.sort = s;
                    self.reverse = false;
                }
            }
        };
        head(ui, rect.left() + TEXT_X, "Name", Sort::Name, false);
        if !cols.compact {
            // "Chats" ends where its numbers end (they're right-aligned).
            head(ui, rect.left() + cols.chats_right, "Chats", Sort::Chats, true);
            head(ui, rect.left() + cols.when, "Updated", Sort::Active, false);
        }
        ui.painter().hline(rect.left()..=rect.right(), rect.bottom(), Stroke::new(1.0, t.separator.gamma_multiply(0.7)));
        let mut action = None;
        for (i, w) in shown.iter().enumerate() {
            let last = i + 1 == shown.len();
            if let Some(a) = row(ui, t, w, pinned.contains(&w.id), root, &cols, width, now_ms, last) {
                action = Some(a);
            }
        }
        action
    }
}

/// A toolbar menu: an icon, what's chosen, and a caret, outlined like the settings'
/// selects. `on` (a filter in use) fills it soft grey and inks its text, so it's plain
/// at a glance that the page is narrowed.
fn menu_button(ui: &mut Ui, t: &Tokens, icon: Option<icons::Paint>, label: &str, on: bool, h: f32) -> egui::Response {
    let text = if on { t.text_strong } else { t.text };
    let g = ui.painter().layout_no_wrap(label.to_owned(), egui::TextStyle::Body.resolve(ui.style()), text);
    let lead = if icon.is_some() { 34.0 } else { 12.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + lead + 30.0, h), Sense::click());
    if ui.is_rect_visible(rect) {
        let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
        let fill = if on {
            t.selection
        } else if resp.hovered() || open {
            crate::form::soft(t)
        } else {
            egui::Color32::TRANSPARENT
        };
        let edge = if on { egui::Color32::TRANSPARENT } else { t.field_border };
        ui.painter().rect(rect, t.round(crate::form::CONTROL_R), fill, Stroke::new(1.0, edge), egui::StrokeKind::Inside);
        if let Some(icon) = icon {
            icon(
                ui.painter(),
                Rect::from_center_size(pos2(rect.left() + 18.0, rect.center().y), vec2(15.0, 15.0)),
                if on { t.text_strong } else { t.text_dim },
            );
        }
        ui.painter().galley(pos2(rect.left() + lead, rect.center().y - g.size().y / 2.0), g, text);
        icons::chevron_down(ui.painter(), Rect::from_center_size(pos2(rect.right() - 15.0, rect.center().y), vec2(11.0, 11.0)), t.text_dim);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// List or cards: one outlined control with two halves; the one in use is filled soft
/// grey (no border of its own), the other just its icon.
fn view_switch(ui: &mut Ui, t: &Tokens, view: &mut View, h: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(h * 2.0 - 4.0, h), Sense::hover());
    let r = t.round(crate::form::CONTROL_R);
    ui.painter().rect_stroke(rect, r, Stroke::new(1.0, t.field_border), egui::StrokeKind::Inside);
    let inset = 4.0;
    let half = (rect.width() - inset * 2.0) / 2.0;
    for (i, (v, paint, tip)) in [(View::List, icons::lines as icons::Paint, "List"), (View::Grid, icons::apps_grid, "Cards")].into_iter().enumerate() {
        #[allow(clippy::cast_precision_loss)] // two halves
        let cell = Rect::from_min_size(pos2(rect.left() + inset + i as f32 * half, rect.top() + inset), vec2(half, h - inset * 2.0));
        let resp = ui.interact(cell, egui::Id::unique(("projects-view", tip)), Sense::click());
        let on = *view == v;
        if on {
            ui.painter().rect_filled(cell, (r - inset).max(2.0), t.selection);
        } else if resp.hovered() {
            ui.painter().rect_filled(cell, (r - inset).max(2.0), crate::form::soft(t));
        }
        let color = if on || resp.hovered() { t.text_strong } else { t.text_dim };
        paint(ui.painter(), Rect::from_center_size(cell.center(), vec2(15.0, 15.0)), color);
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked() {
            *view = v;
        }
    }
}

/// Where a row's text starts (after its tile), from its left edge.
const TEXT_X: f32 = 60.0;

/// Narrower than this (a phone), the page stacks its toolbar and its list has no Chats and
/// Updated columns (each row says them under its name).
const COMPACT: f32 = 520.0;

/// The list's columns, from the row's left edge: where the chat counts end (right-aligned
/// numbers) and where "updated" starts; both sit just before the row's tools.
struct Columns {
    chats_right: f32,
    when: f32,
    /// No columns: the name runs up to the row's tools.
    compact: bool,
}

impl Columns {
    fn new(width: f32) -> Self {
        if width < COMPACT {
            // The tools' room (⋯, pin, new chat) and the name's gap before them.
            return Self { chats_right: width - 116.0 + 60.0, when: width, compact: true };
        }
        // Room on the right for the row's three tools (⋯, pin, new chat).
        let when = width - 120.0 - 96.0;
        Self { chats_right: when - 28.0, when, compact: false }
    }
}

/// "New project": the page's one main action, in the dialogs' button style.
fn new_button(ui: &mut Ui, t: &Tokens) -> Option<Action> {
    let b = crate::workspace::dialog_button(ui, t, "New project", true, true, 36.0);
    b.on_hover_text("Name it, say what it's for, add folders if it needs them").clicked().then_some(Action::New(Start::Choose))
}

/// A project's tile: its first letter on a tint of a colour of its own (from the theme's
/// chart colours, chosen by its id, so it stays the same).
fn tile(p: &egui::Painter, t: &Tokens, r: Rect, w: &WorkspaceSummary) {
    let hash = w.id.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    let color = t.chart[usize::try_from(hash).unwrap_or(0) % t.chart.len()];
    p.rect_filled(r, t.round(crate::form::CONTROL_R), color.gamma_multiply(if t.dark { 0.22 } else { 0.16 }));
    match w.name.chars().find(|c| c.is_alphanumeric()) {
        Some(c) => {
            let letter: String = c.to_uppercase().collect();
            p.text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                letter,
                egui::FontId::new(r.height() * 0.48, crate::theme::bold_or_regular(&p.ctx().clone())),
                color,
            );
        }
        None => icons::folder(p, r.shrink(r.height() * 0.28), color),
    }
}

/// What a project's second line says: its goal, else what it holds ("1 folder", "byob
/// keeps its files"). Paths stay in the tooltip: they're long and rarely what you scan for.
fn sub_line(w: &WorkspaceSummary, root: &str) -> (String, bool) {
    if w.goal.is_empty() { (holds(w, root).0, false) } else { (one_line(&w.goal), true) }
}

/// When it was last active, in words: "Just now", "3m ago", "2d ago".
fn when_label(updated_at: i64, now_ms: i64) -> String {
    match crate::sidebar::ago(updated_at, now_ms) {
        s if s == "now" => "Just now".to_owned(),
        s => format!("{s} ago"),
    }
}

/// No projects at all: what a project is, and the two ways to have one.
fn empty_page(ui: &mut Ui, t: &Tokens) -> Option<Action> {
    let mut action = None;
    ui.add_space(48.0);
    ui.vertical_centered(|ui| {
        icons::folder(ui.painter(), Rect::from_center_size(pos2(ui.max_rect().center().x, ui.cursor().top() + 20.0), vec2(36.0, 36.0)), t.text_faint);
        ui.add_space(48.0);
        ui.label(RichText::new("No projects yet").size(t.text_size(17.0)).color(t.text_strong));
        ui.add_space(4.0);
        ui.label(RichText::new("A project gathers chats around what you're working on: its goal, and any folders it works in.").color(t.text_dim));
        ui.add_space(18.0);
    });
    let w = ui.available_width().min(520.0);
    let x = ui.max_rect().left() + (ui.available_width() - w) / 2.0;
    let card_w = (w - 10.0) / 2.0;
    let top = ui.cursor().top();
    let cards = [
        (Start::Empty, icons::folder_plus as icons::Paint, "New project", "Name it and say what it's for"),
        (Start::Link, icons::folder, "From a folder", "Start from a folder you have"),
    ];
    for (i, (start, icon, title, sub)) in cards.into_iter().enumerate() {
        #[allow(clippy::cast_precision_loss)] // two cards
        let r = Rect::from_min_size(pos2(x + i as f32 * (card_w + 10.0), top), vec2(card_w, 64.0));
        if start_card(ui, t, r, icon, title, sub).clicked() {
            action = Some(Action::New(start));
        }
    }
    ui.allocate_space(vec2(w, 70.0));
    action
}

/// A way to start a project, as a card: icon, title, what it means.
pub(crate) fn start_card(ui: &mut Ui, t: &Tokens, r: Rect, icon: icons::Paint, title: &str, sub: &str) -> egui::Response {
    let resp = ui.interact(r, egui::Id::unique(("start-card", title)), Sense::click());
    if ui.is_rect_visible(r) {
        let p = ui.painter();
        p.rect_filled(r, t.round(10.0), t.text_strong.gamma_multiply(if t.dark { 0.04 } else { 0.035 }));
        if resp.hovered() {
            crate::glass::row(p, t, r, crate::glass::RowState::Hovered);
        }
        p.rect_stroke(r, t.round(10.0), Stroke::new(1.0, t.text_strong.gamma_multiply(if resp.hovered() { 0.16 } else { 0.08 })), egui::StrokeKind::Inside);
        icon(p, Rect::from_center_size(pos2(r.left() + 26.0, r.center().y), vec2(20.0, 20.0)), if resp.hovered() { t.text_strong } else { t.text_dim });
        let x = r.left() + 48.0;
        let body = egui::TextStyle::Body.resolve(ui.style());
        let small = egui::TextStyle::Small.resolve(ui.style());
        let mut job = egui::text::LayoutJob::simple_singleline(title.to_owned(), body, t.text_strong);
        job.wrap = egui::text::TextWrapping::truncate_at_width(r.right() - 10.0 - x);
        let g = p.layout_job(job);
        let mut job = egui::text::LayoutJob::simple_singleline(sub.to_owned(), small, t.text_dim);
        job.wrap = egui::text::TextWrapping::truncate_at_width(r.right() - 10.0 - x);
        let s = p.layout_job(job);
        let top = r.center().y - (g.size().y + 3.0 + s.size().y) / 2.0;
        let gh = g.size().y;
        p.galley(pos2(x, top), g, t.text_strong);
        p.galley(pos2(x, top + gh + 3.0), s, t.text_dim);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// What a project can do, in its ⋯ menu and on right-click: a new chat, pin, edit, remove.
fn menu_items(ui: &mut Ui, t: &Tokens, w: &WorkspaceSummary, pinned: bool, action: &mut Option<Action>) {
    crate::menu::tighten(ui);
    if crate::menu::item(ui, t, Some(icons::compose), "New chat", None).clicked() {
        *action = Some(Action::NewChat(w.id.clone()));
        ui.close();
    }
    if crate::menu::item(ui, t, Some(icons::pin), if pinned { "Unpin project" } else { "Pin project" }, None).clicked() {
        *action = Some(Action::TogglePin(w.id.clone()));
        ui.close();
    }
    if crate::menu::item(ui, t, Some(icons::gear), "Edit project", None).clicked() {
        *action = Some(Action::Settings(w.id.clone()));
        ui.close();
    }
    crate::menu::separator(ui, t);
    if crate::menu::danger(ui, t, Some(icons::cross), "Remove project", None).on_hover_text("Asks first; the folders and their files stay").clicked() {
        *action = Some(Action::Remove(w.id.clone()));
        ui.close();
    }
}

/// Right-click on a project: the same as its ⋯ menu.
fn context_menu(resp: &egui::Response, t: &Tokens, w: &WorkspaceSummary, pinned: bool, action: &mut Option<Action>) {
    crate::menu::context(resp, t, 210.0).show(|ui| menu_items(ui, t, w, pinned, action));
}

/// A row's tool: a square that lights on hover, its icon dim until then.
fn tool(ui: &mut Ui, t: &Tokens, r: Rect, id: egui::Id, icon: icons::Paint, color: Option<egui::Color32>, tip: &str) -> egui::Response {
    let resp = ui.interact(r, id, Sense::click());
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    if resp.hovered() || open {
        ui.painter().rect_filled(r, t.round(crate::form::CONTROL_R), crate::form::soft_hover(t));
    }
    let c = color.unwrap_or(if resp.hovered() || open { t.text_strong } else { t.text_dim });
    icon(ui.painter(), r.shrink(8.0), c);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip)
}

/// One project as a row: its tile, its name and second line (goal, else where it works),
/// chats (right-aligned) and when, then pin and settings. Separators span the whole row.
#[allow(clippy::too_many_arguments)] // one row: its project, pin, where byob makes folders, columns, width, time, and whether it's the last
fn row(ui: &mut Ui, t: &Tokens, w: &WorkspaceSummary, pinned: bool, root: &str, cols: &Columns, width: f32, now_ms: i64, last: bool) -> Option<Action> {
    let h = 64.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(width, h), Sense::click());
    let over = resp.contains_pointer() || resp.context_menu_opened();
    let p = ui.painter();
    // Its ⋯ menu open keeps the row lit (egui keys a popup by its button's id + "popup").
    let menu_open = egui::Popup::is_id_open(ui.ctx(), egui::Id::unique(("project-more", &w.id)).with("popup"));
    let over = over || menu_open;
    if over {
        p.rect_filled(rect.shrink2(vec2(0.0, 2.0)), t.round(crate::form::CARD_R - 2.0), crate::form::row_hover(t));
    }
    tile(p, t, Rect::from_center_size(pos2(rect.left() + 12.0 + 18.0, rect.center().y), vec2(36.0, 36.0)), w);
    let x = rect.left() + TEXT_X;
    let body = egui::TextStyle::Body.resolve(ui.style());
    let small = egui::FontId::proportional(t.text_size(12.5));
    let room = (rect.left() + cols.chats_right - 60.0 - x).max(40.0);
    let (held, paths) = holds(w, root);
    let mut name = egui::text::LayoutJob::simple_singleline(w.name.clone(), body.clone(), t.text_strong);
    name.wrap = egui::text::TextWrapping::truncate_at_width(room);
    let name = p.layout_job(name);
    let (line, is_goal) = sub_line(w, root);
    let (line, is_goal) = if cols.compact {
        // No columns: what they'd say, under the name.
        (format!("{} · {}", crate::sidebar::chats_label(w.chats), when_label(w.updated_at, now_ms)), false)
    } else {
        (line, is_goal)
    };
    let mut sub = egui::text::LayoutJob::simple_singleline(line, small, if is_goal { t.text_dim } else { t.text_faint });
    sub.wrap = egui::text::TextWrapping::truncate_at_width(room);
    let sub = p.layout_job(sub);
    let block = name.size().y + 3.0 + sub.size().y;
    let top = rect.center().y - block / 2.0;
    let name_h = name.size().y;
    p.galley(pos2(x, top), name, t.text_strong);
    p.galley(pos2(x, top + name_h + 3.0), sub, t.text_faint);
    if !cols.compact {
        let chats = p.layout_no_wrap(w.chats.to_string(), body.clone(), t.text_dim);
        p.galley(pos2(rect.left() + cols.chats_right - chats.size().x, rect.center().y - chats.size().y / 2.0), chats, t.text_dim);
        let when = p.layout_no_wrap(when_label(w.updated_at, now_ms), body, t.text_dim);
        p.galley(pos2(rect.left() + cols.when, rect.center().y - when.size().y / 2.0), when, t.text_dim);
    }
    // Soft lines between rows, as in the settings cards; none under the last (the card ends).
    if !last {
        p.hline(rect.left()..=rect.right(), rect.bottom(), Stroke::new(1.0, t.separator.gamma_multiply(0.7)));
    }
    // On hover: ⋯ (its menu), pin, new chat. A pinned project keeps its pin showing.
    let size = vec2(32.0, 32.0);
    let more_r = Rect::from_center_size(pos2(rect.right() - 96.0, rect.center().y), size);
    let pin_r = Rect::from_center_size(pos2(rect.right() - 58.0, rect.center().y), size);
    let new_r = Rect::from_center_size(pos2(rect.right() - 20.0, rect.center().y), size);
    let pointer = ui.ctx().pointer_hover_pos().filter(|p| rect.contains(*p));
    let on = |r: Rect| pointer.is_some_and(|p| r.contains(p));
    // The folders' paths only over the line that names them; no tip on the rest of the row.
    let over_sub = on(Rect::from_min_max(pos2(x, rect.center().y), pos2(rect.left() + cols.chats_right - 60.0, rect.bottom())));
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let resp = if over_sub && held != "byob keeps its files" { resp.on_hover_text(paths.as_str()) } else { resp };
    let mut action = resp.clicked().then(|| Action::Open(w.id.clone()));
    // Always there, quiet until hovered: pin (inked when pinned), ⋯, new chat.
    {
        let pin_color = pinned.then_some(t.text_strong);
        if tool(ui, t, pin_r, egui::Id::unique(("project-pin", &w.id)), icons::pin, pin_color, if pinned { "Unpin" } else { "Pin to the sidebar" }).clicked() {
            action = Some(Action::TogglePin(w.id.clone()));
        }
    }
    {
        let more = tool(ui, t, more_r, egui::Id::unique(("project-more", &w.id)), icons::more, None, "More");
        crate::menu::below(&more, t, 210.0).show(|ui| menu_items(ui, t, w, pinned, &mut action));
        if tool(ui, t, new_r, egui::Id::unique(("project-new-chat", &w.id)), icons::compose, None, "New chat in this project").clicked() {
            action = Some(Action::NewChat(w.id.clone()));
        }
    }
    context_menu(&resp, t, w, pinned, &mut action);
    action
}

/// Lay out the projects as cards, as many across as fit (up to three).
fn grid(ui: &mut Ui, t: &Tokens, shown: &[&WorkspaceSummary], pinned: &[String], root: &str, width: f32, now_ms: i64) -> Option<Action> {
    let gap = 16.0;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // a small positive count
    let across = ((width + gap) / (250.0 + gap)).floor().clamp(1.0, 3.0) as usize;
    #[allow(clippy::cast_precision_loss)]
    let card_w = (width - gap * (across as f32 - 1.0)) / across as f32;
    let card_h = 148.0;
    let mut action = None;
    for chunk in shown.chunks(across) {
        let top = ui.cursor().top();
        let left = ui.max_rect().left();
        for (i, w) in chunk.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let r = Rect::from_min_size(pos2(left + i as f32 * (card_w + gap), top), vec2(card_w, card_h));
            if let Some(a) = card(ui, t, r, w, pinned.contains(&w.id), root, now_ms) {
                action = Some(a);
            }
        }
        ui.allocate_space(vec2(width, card_h + gap));
    }
    action
}

/// One project as a card: its tile, ⋯ and pin, its name and second line, then chats and
/// when at the bottom. The settings cards' fill, a soft border that firms up on hover.
fn card(ui: &mut Ui, t: &Tokens, r: Rect, w: &WorkspaceSummary, pinned: bool, root: &str, now_ms: i64) -> Option<Action> {
    let resp = ui.interact(r, egui::Id::unique(("project-card", &w.id)), Sense::click());
    let more_id = egui::Id::unique(("project-card-more", &w.id));
    let menu_open = egui::Popup::is_id_open(ui.ctx(), more_id.with("popup"));
    let over = resp.contains_pointer() || resp.context_menu_opened() || menu_open;
    let p = ui.painter();
    let fill = crate::form::card_fill(t);
    // A soft border always (cards on a white page need their edge), firmer on hover.
    let edge = if over { t.field_border } else { t.separator };
    let fill = if over { crate::theme::mix(fill, t.text_strong, if t.dark { 0.04 } else { 0.015 }) } else { fill };
    p.rect(r, t.round(crate::form::CARD_R), fill, Stroke::new(1.0, edge), egui::StrokeKind::Inside);
    let pad = 16.0;
    tile(p, t, Rect::from_min_size(r.min + vec2(pad, pad), vec2(36.0, 36.0)), w);
    let x = r.left() + pad;
    let room = r.width() - pad * 2.0;
    let body = egui::TextStyle::Body.resolve(ui.style());
    let small = egui::FontId::proportional(t.text_size(12.5));
    let line = |text: String, font: egui::FontId, color| {
        let mut job = egui::text::LayoutJob::simple_singleline(text, font, color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(room);
        p.layout_job(job)
    };
    p.galley(pos2(x, r.top() + pad + 36.0 + 12.0), line(w.name.clone(), body, t.text_strong), t.text_strong);
    let (sub, is_goal) = sub_line(w, root);
    let sub_color = if is_goal { t.text_dim } else { t.text_faint };
    p.galley(pos2(x, r.top() + pad + 36.0 + 34.0), line(sub, small.clone(), sub_color), sub_color);
    let meta = line(format!("{} · {}", crate::sidebar::chats_label(w.chats), when_label(w.updated_at, now_ms)), small, t.text_faint);
    p.galley(pos2(x, r.bottom() - pad - meta.size().y), meta, t.text_faint);
    let size = vec2(30.0, 30.0);
    let more_r = Rect::from_center_size(pos2(r.right() - pad - 15.0, r.top() + pad + 18.0), size);
    let pin_r = Rect::from_center_size(pos2(more_r.left() - 4.0 - 15.0, more_r.center().y), size);
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let mut action = resp.clicked().then(|| Action::Open(w.id.clone()));
    {
        let pin_color = pinned.then_some(t.text_strong);
        if tool(ui, t, pin_r, egui::Id::unique(("project-card-pin", &w.id)), icons::pin, pin_color, if pinned { "Unpin" } else { "Pin to the sidebar" })
            .clicked()
        {
            action = Some(Action::TogglePin(w.id.clone()));
        }
    }
    {
        let more = tool(ui, t, more_r, more_id, icons::more, None, "More");
        crate::menu::below(&more, t, 210.0).show(|ui| menu_items(ui, t, w, pinned, &mut action));
    }
    context_menu(&resp, t, w, pinned, &mut action);
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(id: &str, name: &str, updated_at: i64) -> WorkspaceSummary {
        WorkspaceSummary { id: id.into(), name: name.into(), folder: format!("/p/{id}"), chats: 2, updated_at, env: Default::default(), ..Default::default() }
    }

    #[test]
    fn projects_search_and_sort_by_last_active_or_name() {
        let all = vec![ws("a", "Zeta", 1), ws("b", "alpha", 3), ws("c", "Mid", 2)];
        let mut p = Projects::default();
        let ids = |p: &Projects| p.shown(&all, None, &facts()).iter().map(|w| w.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&p), ["b", "c", "a"]);
        p.sort = Sort::Name;
        assert_eq!(ids(&p), ["b", "c", "a"]);
        p.query = "ze".into();
        assert_eq!(ids(&p), ["a"]);
        // The folder counts, words in any order, case and extra spaces don't.
        p.query = "  /P/C  ".into();
        assert_eq!(ids(&p), ["c"]);
        p.query = "c mid".into();
        assert_eq!(ids(&p), ["c"]);
        p.query = "(((".into();
        assert!(ids(&p).is_empty());
    }

    /// With Spaces, the page lists the Space in view until asked for all of them.
    #[test]
    fn the_page_lists_this_space_or_all_of_them() {
        let all = vec![ws("a", "Alpha", 1), ws("b", "Beta", 2)];
        let scope = SpaceScope { name: "Work".into(), ids: vec!["a".into()] };
        let mut p = Projects::default();
        assert_eq!(p.shown(&all, Some(&scope), &facts()).len(), 1);
        p.all_spaces = true;
        assert_eq!(p.shown(&all, Some(&scope), &facts()).len(), 2);
    }

    /// What a project holds: byob's own folder for it isn't one of "its folders"; its own
    /// folder when it's yours, and the ones it links, are.
    #[test]
    fn a_project_says_what_folders_it_works_in() {
        let root = "/home/me/byob-workspaces/";
        let made = WorkspaceSummary { folder: "/home/me/byob-workspaces/alpha".into(), ..ws("a", "Alpha", 1) };
        let yours = WorkspaceSummary { folder: "/home/me/code/alpha".into(), folders: vec!["/home/me/code/api".into()], ..ws("b", "Alpha", 1) };
        let lookalike = WorkspaceSummary { folder: "/home/me/byob-workspaces-old/x".into(), ..ws("c", "X", 1) };
        let made_linked = WorkspaceSummary { folders: vec!["/srv/docs".into()], ..made.clone() };
        assert_eq!(holds(&made, root).0, "byob keeps its files");
        assert_eq!(holds(&yours, root).0, "2 folders");
        assert_eq!(holds(&lookalike, root).0, "1 folder", "a folder whose name only starts the same isn't byob's");
        assert_eq!(holds(&made_linked, root).0, "1 folder");
        assert_eq!(holds(&made, "").0, "1 folder", "no root known yet: it's shown as a folder");
        assert_eq!(one_line("Ship it\n\nby  June"), "Ship it by June");
    }

    /// A row's second line: the goal; else what it holds (never a path). Times read as
    /// words.
    #[test]
    fn a_rows_second_line_says_the_most_useful_thing() {
        let root = "/home/me/byob-workspaces";
        let goal = WorkspaceSummary { goal: "Ship it\nby June".into(), folder: "/home/me/code/app".into(), ..ws("a", "App", 1) };
        let one = WorkspaceSummary { folder: "/srv/code/app".into(), ..ws("b", "App", 1) };
        let two = WorkspaceSummary { folder: "/srv/code/app".into(), folders: vec!["/srv/code/api".into()], ..ws("c", "App", 1) };
        let made = WorkspaceSummary { folder: "/home/me/byob-workspaces/x".into(), ..ws("d", "X", 1) };
        assert_eq!(sub_line(&goal, root), ("Ship it by June".to_owned(), true));
        assert_eq!(sub_line(&one, root), ("1 folder".to_owned(), false), "the path stays in the tooltip");
        assert_eq!(sub_line(&two, root).0, "2 folders");
        assert_eq!(sub_line(&made, root).0, "byob keeps its files");
        assert_eq!(when_label(1_000, 1_000), "Just now");
        assert_eq!(when_label(0, 2 * 24 * 60 * 60 * 1000), "2d ago");
    }

    /// Search finds a project by its goal too.
    #[test]
    fn search_finds_a_project_by_its_goal() {
        let all = vec![WorkspaceSummary { goal: "Track monthly spending".into(), ..ws("a", "Money", 1) }, ws("b", "Other", 2)];
        let p = Projects { query: "spending".into(), ..Projects::default() };
        assert_eq!(p.shown(&all, None, &facts()).iter().map(|w| w.id.as_str()).collect::<Vec<_>>(), ["a"]);
    }

    fn facts() -> Facts<'static> {
        Facts { pinned: &[], root: "", now_ms: 0 }
    }

    fn page(p: &mut Projects, ctx: &egui::Context, all: &[WorkspaceSummary], events: Vec<egui::Event>) -> (Option<Action>, crate::plugins::tests::Texts) {
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 700.0));
        let mut got = None;
        let mut out = ctx
            .run_ui(egui::RawInput { screen_rect: Some(screen), events, ..Default::default() }, |ui| got = p.ui(ui, &Tokens::dark(), all, &[], "", None, 10));
        out.textures_delta.clear();
        (got, crate::plugins::tests::texts_of(&out))
    }

    fn click(p: &mut Projects, ctx: &egui::Context, all: &[WorkspaceSummary], at: egui::Pos2) -> Option<Action> {
        let press = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        page(p, ctx, all, vec![egui::Event::PointerMoved(at)]);
        page(p, ctx, all, vec![press(true)]);
        page(p, ctx, all, vec![press(false)]).0
    }

    /// A click on a row's pin pins it (it used to open the project instead); elsewhere on
    /// the row it opens. In cards too.
    #[test]
    fn a_rows_pin_pins_and_the_rest_of_the_row_opens() {
        let ctx = egui::Context::default();
        let all = vec![ws("a", "Alpha", 1)];
        let mut p = Projects::default();
        let (_, texts) = page(&mut p, &ctx, &all, vec![]);
        let name = texts.iter().find(|t| t.0 == "Alpha").map(|t| t.1).unwrap();
        // The row spans the column: its pin sits 58 points in from the right edge.
        let page_w = (900.0_f32 - 64.0).clamp(240.0, 880.0);
        let row_right = 900.0 - (900.0 - page_w) / 2.0;
        let pin = pos2(row_right - 58.0, name.center().y + 8.0);
        assert_eq!(click(&mut p, &ctx, &all, pin), Some(Action::TogglePin("a".into())));
        assert_eq!(click(&mut p, &ctx, &all, name.center()), Some(Action::Open("a".into())));
        // Cards: the name opens it.
        p.view = View::Grid;
        let (_, texts) = page(&mut p, &ctx, &all, vec![]);
        let name = texts.iter().find(|t| t.0 == "Alpha").map(|t| t.1).unwrap();
        assert_eq!(click(&mut p, &ctx, &all, name.center()), Some(Action::Open("a".into())));
    }

    /// The list's headers sort it; the toolbar switches between the list and cards.
    #[test]
    fn headers_sort_and_the_view_switches() {
        let ctx = egui::Context::default();
        let all = vec![ws("a", "Zeta", 1), ws("b", "Alpha", 3)];
        let mut p = Projects::default();
        let (_, texts) = page(&mut p, &ctx, &all, vec![]);
        assert!(crate::plugins::tests::has(&texts, "Updated ↓"), "{texts:?}");
        let name_head = texts.iter().find(|t| t.0 == "Name").map(|t| t.1.center()).unwrap();
        click(&mut p, &ctx, &all, name_head);
        assert_eq!(p.sort, Sort::Name);
        let (_, texts) = page(&mut p, &ctx, &all, vec![]);
        assert!(crate::plugins::tests::has(&texts, "Name ↑"));
        // Cards show each project's chats and when.
        p.view = View::Grid;
        let (_, texts) = page(&mut p, &ctx, &all, vec![]);
        assert!(texts.iter().any(|t| t.0.starts_with("2 chats")), "{texts:?}");
        assert!(!crate::plugins::tests::has(&texts, "Updated ↓"), "no list header on cards");
    }

    /// A row's tools: new chat starts one in the project; ⋯ opens its menu, whose Edit
    /// project opens its settings.
    #[test]
    fn a_rows_tools_start_a_chat_and_open_its_menu() {
        let ctx = egui::Context::default();
        let all = vec![ws("a", "Alpha", 1)];
        let mut p = Projects::default();
        let (_, texts) = page(&mut p, &ctx, &all, vec![]);
        let name = texts.iter().find(|t| t.0 == "Alpha").map(|t| t.1).unwrap();
        let page_w = (900.0_f32 - 64.0).clamp(240.0, 880.0);
        let row_right = 900.0 - (900.0 - page_w) / 2.0;
        let y = name.center().y + 8.0;
        assert_eq!(click(&mut p, &ctx, &all, pos2(row_right - 20.0, y)), Some(Action::NewChat("a".into())));
        click(&mut p, &ctx, &all, pos2(row_right - 96.0, y));
        let (_, texts) = page(&mut p, &ctx, &all, vec![]);
        let edit = texts.iter().find(|t| t.0 == "Edit project").map(|t| t.1.center()).unwrap_or_else(|| panic!("{texts:?}"));
        assert_eq!(click(&mut p, &ctx, &all, edit), Some(Action::Settings("a".into())));
    }

    /// Every column header sorts; a second click on the one in use turns the order around.
    #[test]
    fn column_headers_sort_both_ways() {
        let ctx = egui::Context::default();
        let mut busy = ws("b", "Busy", 1);
        busy.chats = 9;
        let all = vec![ws("a", "Alpha", 5), busy];
        let mut p = Projects::default();
        let order = |p: &Projects| p.shown(&all, None, &facts()).iter().map(|w| w.id.clone()).collect::<Vec<_>>();
        let at = |p: &mut Projects, label: &str| {
            let (_, texts) = page(p, &ctx, &all, vec![]);
            // The header row, under the toolbar (whose Sort button may read "Name" too).
            texts
                .iter()
                .filter(|t| t.0.starts_with(label) && t.1.top() < 200.0)
                .max_by(|a, b| a.1.top().total_cmp(&b.1.top()))
                .map(|t| t.1.center())
                .unwrap_or_else(|| panic!("{label}: {texts:?}"))
        };
        assert_eq!(order(&p), ["a", "b"], "newest first");
        let updated = at(&mut p, "Updated");
        click(&mut p, &ctx, &all, updated);
        assert!(p.reverse);
        assert_eq!(order(&p), ["b", "a"], "oldest first after a second click");
        let chats = at(&mut p, "Chats");
        click(&mut p, &ctx, &all, chats);
        assert_eq!((p.sort, p.reverse), (Sort::Chats, false));
        assert_eq!(order(&p), ["b", "a"], "most chats first");
        let name = at(&mut p, "Name");
        click(&mut p, &ctx, &all, name);
        assert_eq!(order(&p), ["a", "b"]);
        let name = at(&mut p, "Name");
        click(&mut p, &ctx, &all, name);
        assert_eq!(order(&p), ["b", "a"], "Z to A");
        let (_, texts) = page(&mut p, &ctx, &all, vec![]);
        assert!(crate::plugins::tests::has(&texts, "Name ↓"), "{texts:?}");
    }

    /// The sort menu sorts: open it, pick Name, and the list goes A to Z.
    #[test]
    fn the_sort_menu_changes_the_order() {
        let ctx = egui::Context::default();
        let all = vec![ws("a", "Zeta", 5), ws("b", "Alpha", 1)];
        // Cards: no list header, so the only "Name" on the page is the menu's.
        let mut p = Projects { view: View::Grid, ..Projects::default() };
        let (_, texts) = page(&mut p, &ctx, &all, vec![]);
        let button = texts.iter().find(|t| t.0 == "Last active").map(|t| t.1.center()).unwrap();
        click(&mut p, &ctx, &all, button);
        let (_, texts) = page(&mut p, &ctx, &all, vec![]);
        let name = texts.iter().find(|t| t.0 == "Name").map(|t| t.1);
        let name = name.unwrap_or_else(|| panic!("the menu should list Name: {texts:?}"));
        click(&mut p, &ctx, &all, name.center());
        assert_eq!(p.sort, Sort::Name);
        let order: Vec<&str> = p.shown(&all, None, &facts()).iter().map(|w| w.name.as_str()).collect();
        assert_eq!(order, ["Alpha", "Zeta"]);
    }

    /// Search's edges: accents and other scripts, a word in the goal, only spaces, a word
    /// that's in no project, a very long query, and every word having to match.
    #[test]
    fn search_handles_its_edge_cases() {
        let mut cafe = ws("a", "Café Menü", 1);
        cafe.goal = "Plan the spring menu".into();
        let all = vec![cafe, ws("b", "日本語ノート", 2), ws("c", "Taxes 2026", 3)];
        let mut p = Projects::default();
        let ids = |p: &Projects| p.shown(&all, None, &facts()).iter().map(|w| w.id.clone()).collect::<Vec<_>>();
        p.query = "   ".into();
        assert_eq!(ids(&p).len(), 3, "only spaces is no search");
        p.query = "CAFÉ".into();
        assert_eq!(ids(&p), ["a"], "case folds beyond ASCII");
        p.query = "日本".into();
        assert_eq!(ids(&p), ["b"]);
        p.query = "spring".into();
        assert_eq!(ids(&p), ["a"], "the goal counts");
        p.query = "spring taxes".into();
        assert!(ids(&p).is_empty(), "every word must match the same project");
        p.query = "2026".into();
        assert_eq!(ids(&p), ["c"]);
        p.query = "x".repeat(500);
        assert!(ids(&p).is_empty());
        assert!(!p.narrowed() || !p.query.is_empty());
    }

    /// Show narrows the page: pinned ones, those with folders of yours, those active this
    /// week. Most chats sorts by chat count.
    #[test]
    fn show_filters_and_most_chats_sort() {
        let now = 30 * 24 * 60 * 60 * 1000;
        let mut linked = ws("a", "Linked", now - 1000);
        linked.folder = "/Users/me/code".into();
        let mut busy = ws("b", "Busy", now - 20 * 24 * 60 * 60 * 1000);
        busy.folder = "/byob/projects/busy".into();
        busy.chats = 9;
        let all = vec![linked, busy];
        let pinned = vec!["b".to_owned()];
        let facts = Facts { pinned: &pinned, root: "/byob/projects", now_ms: now };
        let mut p = Projects::default();
        let ids = |p: &Projects| p.shown(&all, None, &facts).iter().map(|w| w.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&p), ["a", "b"]);
        p.show = Show::Pinned;
        assert_eq!(ids(&p), ["b"]);
        p.show = Show::Folders;
        assert_eq!(ids(&p), ["a"], "byob's own folder isn't one of yours");
        p.show = Show::Recent;
        assert_eq!(ids(&p), ["a"], "Busy was last active 20 days ago");
        p.show = Show::All;
        p.sort = Sort::Chats;
        assert_eq!(ids(&p), ["b", "a"]);
    }

    /// A narrowed page says how many it shows and clears back to all; an empty result
    /// says why and offers the same.
    #[test]
    fn filters_say_what_they_hide_and_clear() {
        let ctx = egui::Context::default();
        let all = vec![ws("a", "Alpha", 1), ws("b", "Beta", 2)];
        let mut p = Projects { query: "alp".into(), ..Projects::default() };
        let (_, texts) = page(&mut p, &ctx, &all, vec![]);
        assert!(crate::plugins::tests::has(&texts, "1 of 2 projects"), "{texts:?}");
        let clear = texts.iter().find(|t| t.0 == "Clear filters").map(|t| t.1.center()).unwrap();
        click(&mut p, &ctx, &all, clear);
        assert!(p.query.is_empty());
        p.show = Show::Pinned;
        let (_, texts) = page(&mut p, &ctx, &all, vec![]);
        assert!(crate::plugins::tests::has(&texts, "No pinned projects"), "{texts:?}");
        let clear = texts.iter().filter(|t| t.0 == "Clear filters").map(|t| t.1).max_by(|a, b| a.top().total_cmp(&b.top())).unwrap();
        click(&mut p, &ctx, &all, clear.center());
        assert_eq!(p.show, Show::All);
    }

    /// Regression: on a phone the toolbar's search, Show and Sort were drawn over one
    /// another, and the list's Chats and Updated columns over the names ("pho…").
    #[test]
    fn a_phone_stacks_the_toolbar_and_drops_the_columns() {
        let ctx = egui::Context::default();
        let all = vec![ws("a", "Groceries", 1)];
        let mut p = Projects::default();
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(390.0, 844.0));
        let mut texts = Vec::new();
        for _ in 0..3 {
            let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), ..Default::default() }, |ui| {
                p.ui(ui, &Tokens::dark(), &all, &[], "", None, 10);
            });
            out.textures_delta.clear();
            texts = crate::plugins::tests::texts_of(&out);
        }
        let at = |want: &str| texts.iter().find(|t| t.0 == want).map(|t| t.1).unwrap_or_else(|| panic!("no {want:?} in {texts:?}"));
        let search = p.search_rect.unwrap();
        for menu in ["All projects", "Last active"] {
            let r = at(menu);
            assert!(!search.intersects(r) && screen.contains_rect(r), "{menu:?} at {r:?}, the search at {search:?}");
        }
        assert!(search.right() <= screen.right() && search.width() > 300.0, "the search across the page: {search:?}");
        // The name in full, no column header over it.
        let name = at("Groceries");
        assert!(texts.iter().all(|t| t.0 != "Chats" && t.0 != "Updated"), "{texts:?}");
        assert!(texts.iter().any(|t| t.0.contains("2 chats") && t.1.top() > name.bottom()), "chats and when under the name: {texts:?}");
    }

    /// The × in the search clears it.
    #[test]
    fn the_search_clears_with_its_cross() {
        let ctx = egui::Context::default();
        let all = vec![ws("a", "Alpha", 1)];
        let mut p = Projects { query: "zzz".into(), ..Projects::default() };
        page(&mut p, &ctx, &all, vec![]);
        // The cross sits 18 points in from the field's right end.
        let field = p.search_rect.unwrap();
        let cross = pos2(field.right() - 18.0, field.center().y);
        click(&mut p, &ctx, &all, cross);
        assert!(p.query.is_empty(), "query left: {}", p.query);
    }

    /// No projects: the page offers both ways to start one.
    #[test]
    fn an_empty_page_offers_a_new_project_or_one_from_a_folder() {
        let ctx = egui::Context::default();
        let mut p = Projects::default();
        let (_, texts) = page(&mut p, &ctx, &[], vec![]);
        let link = texts.iter().find(|t| t.0 == "From a folder").map(|t| t.1.center()).unwrap();
        let empty = texts.iter().find(|t| t.0 == "New project" && t.1.top() > 200.0).map(|t| t.1.center()).unwrap();
        assert_eq!(click(&mut p, &ctx, &[], link), Some(Action::New(Start::Link)));
        assert_eq!(click(&mut p, &ctx, &[], empty), Some(Action::New(Start::Empty)));
        // Search and the view are for when there's something to find.
        assert!(!crate::plugins::tests::has(&texts, "Search projects"));
    }
}
