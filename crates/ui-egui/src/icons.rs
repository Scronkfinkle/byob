//! byob's icons: the Phosphor set (MIT, phosphoricons.com), drawn from its icon font so
//! every icon comes from one designed family. Each function draws one glyph into a square
//! (`Paint`), outlined (`phosphor`) or filled (`phosphor-fill`, for selected states and
//! small marks). The fonts are installed with the UI fonts ([`install_fonts`]).

use byob_protocol::workspace::EnvKind;
use egui::{Color32, FontFamily, FontId, Rect, Response, Sense, Ui, pos2, vec2};
use egui_phosphor::{fill, regular};

use crate::theme::Tokens;

/// A shape drawn into a square: (painter, rect, color).
pub type Paint = fn(&egui::Painter, Rect, Color32);

const OUTLINE: &str = "phosphor";
const FILLED: &str = "phosphor-fill";

/// Add the Phosphor fonts (outline and filled) as their own families, not mixed into text.
/// Each family holds only its Phosphor font: the icons sit in Unicode's private-use range,
/// where the text fonts (Inter) have glyphs of their own that would otherwise win.
pub fn install_fonts(fonts: &mut egui::FontDefinitions) {
    for (name, variant) in [(OUTLINE, egui_phosphor::Variant::Regular), (FILLED, egui_phosphor::Variant::Fill)] {
        fonts.font_data.insert(name.into(), egui::FontData::from_static(variant.font_bytes()).into());
        fonts.families.insert(FontFamily::Name(name.into()), vec![name.into()]);
    }
}

/// One glyph of `family`, filling the square `r`.
fn glyph(p: &egui::Painter, r: Rect, icon: &str, family: &str, color: Color32) {
    let size = r.width().min(r.height());
    p.text(r.center(), egui::Align2::CENTER_CENTER, icon, FontId::new(size, FontFamily::Name(family.into())), color);
}

/// An outlined icon.
fn outline(p: &egui::Painter, r: Rect, icon: &str, color: Color32) {
    glyph(p, r, icon, OUTLINE, color);
}

/// A filled icon (the selected state of an outlined one, and small solid marks).
pub fn filled(p: &egui::Painter, r: Rect, icon: &str, color: Color32) {
    glyph(p, r, icon, FILLED, color);
}

/// "Chat settings".
pub fn sliders(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::SLIDERS_HORIZONTAL, c);
}

/// "Settings".
pub fn gear(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::GEAR_SIX, c);
}

/// "Hide or show the sidebar".
pub fn sidebar(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::SIDEBAR_SIMPLE, c);
}

/// "More (overflow menus)".
pub fn more(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::DOTS_THREE, c);
}

/// "A plugin".
pub fn puzzle(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::PUZZLE_PIECE, c);
}

/// "Artifacts".
pub fn artifacts(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::CARDS, c);
}

/// "Back".
pub fn back(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::CARET_LEFT, c);
}

/// "Forward".
pub fn forward(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::CARET_RIGHT, c);
}

/// "A list, a section".
pub fn lines(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::LIST, c);
}

/// "Rename, edit".
pub fn pencil(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::PENCIL_SIMPLE, c);
}

/// "Pin".
pub fn pin(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::PUSH_PIN, c);
}

/// "Delete".
pub fn trash(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::TRASH, c);
}

/// "Copy".
pub fn copy(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::COPY, c);
}

/// "New, add".
pub fn plus(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::PLUS, c);
}

/// "New chat".
pub fn compose(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::NOTE_PENCIL, c);
}

/// "Search".
pub fn search(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::MAGNIFYING_GLASS, c);
}

/// "Close, clear".
pub fn cross(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::X, c);
}

/// "A folder, a project".
pub fn folder(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::FOLDER, c);
}

/// "New project".
pub fn folder_plus(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::FOLDER_PLUS, c);
}

/// "A model".
pub fn chip(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::CPU, c);
}

/// "Appearance".
pub fn contrast(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::CIRCLE_HALF, c);
}

/// "Text".
pub fn text_size(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::TEXT_AA, c);
}

/// "Colors".
pub fn colors(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::PALETTE, c);
}

/// "A chevron pointing down (menus, split buttons)".
pub fn chevron_down(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::CARET_DOWN, c);
}

/// "A chevron pointing right (a row that opens a page)".
pub fn chevron_right(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::CARET_RIGHT, c);
}

/// "Back to".
pub fn arrow_left(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::ARROW_LEFT, c);
}

/// "Read only, open".
pub fn eye(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::EYE, c);
}

/// "Kept safe".
pub fn shield(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::SHIELD, c);
}

/// "Fast, power".
pub fn bolt(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::LIGHTNING, c);
}

/// "Done, chosen".
pub fn check(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::CHECK, c);
}

/// "Attach".
pub fn paperclip(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::PAPERCLIP, c);
}

/// "Slash commands".
pub fn slash_box(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::COMMAND, c);
}

/// "Speak".
pub fn mic(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::MICROPHONE, c);
}

/// "Send (Enter)".
pub fn enter(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::ARROW_ELBOW_DOWN_LEFT, c);
}

/// "Archive".
pub fn archive(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::ARCHIVE, c);
}

/// "A chat".
pub fn bubble(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::CHAT_CIRCLE, c);
}

/// "Mail".
pub fn mail(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::ENVELOPE_SIMPLE, c);
}

/// "Calendar".
pub fn calendar(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::CALENDAR_BLANK, c);
}

/// "The web".
pub fn globe(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::GLOBE_SIMPLE, c);
}

/// "An app".
pub fn app_window(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::APP_WINDOW, c);
}

/// "A spreadsheet".
pub fn sheet(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::TABLE, c);
}

/// "A chart".
pub fn chart_column(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::CHART_BAR, c);
}

/// "A document".
pub fn file_text(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::FILE_TEXT, c);
}

/// "An image".
pub fn image(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::IMAGE, c);
}

/// "Math".
pub fn sigma(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::SIGMA, c);
}

/// "Memory, a brain".
pub fn brain(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::BRAIN, c);
}

/// "Code, commands".
pub fn terminal(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::TERMINAL, c);
}

/// "Helpers, people".
pub fn users(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::USERS, c);
}

/// "A plan".
pub fn list_checks(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::LIST_CHECKS, c);
}

/// "Help".
pub fn life_buoy(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::LIFEBUOY, c);
}

/// "A skill".
pub fn book_mark(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::BOOK_BOOKMARK, c);
}

/// "Apps".
pub fn apps_grid(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::SQUARES_FOUR, c);
}

/// "Plugins".
pub fn puzzle_piece(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::PUZZLE_PIECE, c);
}

/// "Models and connection".
pub fn plug(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::PLUG, c);
}

/// "Get help".
pub fn help_circle(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::QUESTION, c);
}

/// "Send feedback".
pub fn megaphone(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::MEGAPHONE, c);
}

/// "Library".
pub fn pages(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::FILES, c);
}

/// "Show and sort" (how a list is arranged).
pub fn funnel(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::FUNNEL_SIMPLE, c);
}

/// "New section" (a group of your own in a list).
pub fn list_plus(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::LIST_PLUS, c);
}

/// Two arrows going round (read again, refresh).
pub fn refresh(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::ARROWS_CLOCKWISE, c);
}

/// A chevron pointing up (move earlier).
pub fn chevron_up(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::CARET_UP, c);
}

/// "Import" (a file into byob).
pub fn download(p: &egui::Painter, r: Rect, c: Color32) {
    outline(p, r, regular::DOWNLOAD_SIMPLE, c);
}

/// "Stop" (a running reply): a solid square.
pub fn stop(p: &egui::Painter, r: Rect, c: Color32) {
    filled(p, r, fill::STOP, c);
}

/// A chevron: down when `open`, right when not (a folding heading).
pub fn chevron(p: &egui::Painter, r: Rect, open: bool, c: Color32) {
    outline(p, r, if open { regular::CARET_DOWN } else { regular::CARET_RIGHT }, c);
}

/// A solid triangle pointing right (`open` = false) or down.
pub fn disclosure(p: &egui::Painter, r: Rect, open: bool, c: Color32) {
    filled(p, r, if open { fill::CARET_DOWN } else { fill::CARET_RIGHT }, c);
}

/// Where a workspace runs its commands: a cube (container), a snowflake (Nix dev shell), a
/// desktop (this machine) or a sparkle (Auto: decided per folder).
pub fn env(p: &egui::Painter, r: Rect, kind: EnvKind, c: Color32) {
    let icon = match kind {
        EnvKind::Container => regular::CUBE,
        EnvKind::Nix => regular::SNOWFLAKE,
        EnvKind::Host => regular::DESKTOP,
        EnvKind::Auto => regular::SPARKLE,
    };
    outline(p, r, icon, c);
}

/// The height of a text button (what egui gives `ui.button`).
pub fn button_height(ui: &Ui) -> f32 {
    let text = ui.text_style_height(&egui::TextStyle::Button);
    ui.spacing().interact_size.y.max(text + 2.0 * ui.spacing().button_padding.y)
}

/// A header-style button: an icon and, if `label` is given, text after it. Frameless until
/// hovered; filled while `selected`.
pub fn icon_button(ui: &mut Ui, t: &Tokens, paint: Paint, label: Option<&str>, selected: bool) -> Response {
    // Exactly a button's height, so it lines up with the buttons and labels beside it.
    let height = button_height(ui);
    let icon = t.text_size(16.0).min(height - 2.0).max(8.0);
    let pad = 5.0;
    let color = if selected { t.text_strong } else { t.text };
    let galley = label.map(|l| ui.painter().layout_no_wrap(l.to_owned(), egui::TextStyle::Button.resolve(ui.style()), color));
    let text_w = galley.as_ref().map_or(0.0, |g| g.size().x + 6.0);
    let size = vec2(pad + icon + text_w + pad, height);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let name = label.or_else(|| name_of(paint)).unwrap_or("icon").to_owned();
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), &name));
    if ui.is_rect_visible(rect) {
        let state = if resp.is_pointer_button_down_on() {
            crate::glass::RowState::Pressed
        } else if selected {
            crate::glass::RowState::Selected
        } else if resp.hovered() {
            crate::glass::RowState::Hovered
        } else {
            crate::glass::RowState::Idle
        };
        crate::glass::row(ui.painter(), t, rect, state);
        let icon_rect = Rect::from_min_size(pos2(rect.left() + pad, rect.center().y - icon / 2.0), vec2(icon, icon));
        let icon_color = if resp.hovered() || selected { t.text_strong } else { t.text_dim };
        paint(ui.painter(), icon_rect, icon_color);
        if let Some(g) = galley {
            let pos = pos2(icon_rect.right() + 6.0, rect.center().y - g.size().y / 2.0);
            ui.painter().galley(pos, g, color);
        }
    }
    resp
}

/// The color an environment's icon is drawn in.
pub fn env_color(t: &Tokens, kind: EnvKind) -> Color32 {
    match kind {
        EnvKind::Container => t.link,
        EnvKind::Nix => t.accent,
        EnvKind::Host => t.text_dim,
        EnvKind::Auto => t.text_faint,
    }
}

/// One line on where commands run, for the icon's tooltip.
pub fn env_tip(kind: EnvKind) -> &'static str {
    match kind {
        EnvKind::Container => "Container: commands run in this workspace's Docker/Podman container",
        EnvKind::Nix => "Nix dev shell: commands run with the folder's Nix environment loaded",
        EnvKind::Host => "This machine: commands run here, in byob's sandbox",
        EnvKind::Auto => "Auto: byob checks the folder (flake, devcontainer…) and asks once",
    }
}

/// The icon for a Lucide name a plugin declares (plugins name their icons the Lucide way).
pub fn lucide(name: &str) -> Option<Paint> {
    Some(match name {
        "mail" | "inbox" => mail,
        "calendar" | "calendar-days" => calendar,
        "globe" | "search" => globe,
        "app-window" | "layout" => app_window,
        "sheet" | "table" => sheet,
        "chart-column" | "chart-bar" | "bar-chart" => chart_column,
        "file-text" | "scroll-text" | "book-open" => file_text,
        "image" => image,
        "sigma" => sigma,
        "brain" => brain,
        "terminal" | "code" => terminal,
        "users" => users,
        "list-checks" | "list" => list_checks,
        "life-buoy" => life_buoy,
        "message-circle-question" | "message-circle" => bubble,
        "files" | "folder" => folder,
        "layout-dashboard" | "palette" => colors,
        "boxes" | "box" => chip,
        "plug" => plug,
        "fold-vertical" => lines,
        "puzzle" => puzzle_piece,
        _ => return None,
    })
}

/// What an icon means, in words (for fluid's element names), when it's one of byob's.
pub fn name_of(paint: Paint) -> Option<&'static str> {
    const NAMES: &[(Paint, &str)] = &[
        (sliders, "chat settings"),
        (gear, "settings"),
        (sidebar, "sidebar"),
        (more, "more"),
        (puzzle, "plugins"),
        (artifacts, "artifacts"),
        (back, "back"),
        (forward, "forward"),
        (lines, "list"),
        (pencil, "edit"),
        (pin, "pin"),
        (trash, "delete"),
        (copy, "copy"),
        (plus, "add"),
        (compose, "new chat"),
        (search, "search"),
        (cross, "close"),
        (folder, "folder"),
        (folder_plus, "new project"),
        (chip, "model"),
        (contrast, "appearance"),
        (text_size, "text"),
        (colors, "colors"),
        (chevron_down, "menu"),
        (chevron_right, "open"),
        (arrow_left, "back"),
        (eye, "view"),
        (shield, "safety"),
        (bolt, "power"),
        (check, "done"),
        (paperclip, "attach"),
        (slash_box, "commands"),
        (mic, "speak"),
        (enter, "send"),
        (archive, "archive"),
        (bubble, "chat"),
        (mail, "mail"),
        (calendar, "calendar"),
        (globe, "web"),
        (app_window, "app"),
        (sheet, "sheet"),
        (chart_column, "chart"),
        (file_text, "file"),
        (image, "image"),
        (sigma, "math"),
        (brain, "memory"),
        (terminal, "terminal"),
        (users, "people"),
        (list_checks, "tasks"),
        (life_buoy, "help"),
        (book_mark, "bookmark"),
        (apps_grid, "apps"),
        (puzzle_piece, "plugin"),
        (plug, "connect"),
        (help_circle, "help"),
        (megaphone, "news"),
        (pages, "pages"),
        (funnel, "filter"),
        (list_plus, "add to list"),
        (refresh, "refresh"),
        (chevron_up, "collapse"),
        (download, "download"),
        (stop, "stop"),
    ];
    NAMES.iter().find(|(p, _)| std::ptr::fn_addr_eq(*p, paint)).map(|(_, n)| *n)
}

/// What an icon glyph (as painted text) means, in words, when it's one of byob's.
pub fn glyph_name(text: &str) -> Option<&'static str> {
    const GLYPHS: &[(&str, &str)] = &[
        (regular::SLIDERS_HORIZONTAL, "chat settings"),
        (regular::GEAR_SIX, "settings"),
        (regular::SIDEBAR_SIMPLE, "sidebar"),
        (regular::DOTS_THREE, "more"),
        (regular::PUZZLE_PIECE, "plugins"),
        (regular::CARDS, "artifacts"),
        (regular::CARET_LEFT, "back"),
        (regular::CARET_RIGHT, "forward"),
        (regular::LIST, "list"),
        (regular::PENCIL_SIMPLE, "edit"),
        (regular::PUSH_PIN, "pin"),
        (regular::TRASH, "delete"),
        (regular::COPY, "copy"),
        (regular::PLUS, "add"),
        (regular::NOTE_PENCIL, "new chat"),
        (regular::MAGNIFYING_GLASS, "search"),
        (regular::X, "close"),
        (regular::FOLDER, "folder"),
        (regular::FOLDER_PLUS, "new project"),
        (regular::CPU, "model"),
        (regular::CIRCLE_HALF, "appearance"),
        (regular::TEXT_AA, "text"),
        (regular::PALETTE, "colors"),
        (regular::CARET_DOWN, "menu"),
        (regular::CARET_RIGHT, "open"),
        (regular::ARROW_LEFT, "back"),
        (regular::EYE, "view"),
        (regular::SHIELD, "safety"),
        (regular::LIGHTNING, "power"),
        (regular::CHECK, "done"),
        (regular::PAPERCLIP, "attach"),
        (regular::COMMAND, "commands"),
        (regular::MICROPHONE, "speak"),
        (regular::ARROW_ELBOW_DOWN_LEFT, "send"),
        (regular::ARCHIVE, "archive"),
        (regular::CHAT_CIRCLE, "chat"),
        (regular::ENVELOPE_SIMPLE, "mail"),
        (regular::CALENDAR_BLANK, "calendar"),
        (regular::GLOBE_SIMPLE, "web"),
        (regular::APP_WINDOW, "app"),
        (regular::TABLE, "sheet"),
        (regular::CHART_BAR, "chart"),
        (regular::FILE_TEXT, "file"),
        (regular::IMAGE, "image"),
        (regular::SIGMA, "math"),
        (regular::BRAIN, "memory"),
        (regular::TERMINAL, "terminal"),
        (regular::USERS, "people"),
        (regular::LIST_CHECKS, "tasks"),
        (regular::LIFEBUOY, "help"),
        (regular::BOOK_BOOKMARK, "bookmark"),
        (regular::SQUARES_FOUR, "apps"),
        (regular::PUZZLE_PIECE, "plugin"),
        (regular::PLUG, "connect"),
        (regular::QUESTION, "help"),
        (regular::MEGAPHONE, "news"),
        (regular::FILES, "pages"),
        (regular::FUNNEL_SIMPLE, "filter"),
        (regular::LIST_PLUS, "add to list"),
        (regular::ARROWS_CLOCKWISE, "refresh"),
        (regular::CARET_UP, "collapse"),
        (regular::DOWNLOAD_SIMPLE, "download"),
    ];
    let text = text.trim();
    GLYPHS.iter().find(|(g, _)| *g == text).map(|(_, n)| *n)
}
