//! Every app setting, declared once: what it is called, what it does, the words people search
//! for it by, and where it lives (a tab and a section). The pages draw their settings from
//! this list in order and the search matches against it, so the two can't drift apart.
//!
//! How each one is drawn is [`ByobApp::control`](crate::ByobApp) (an exhaustive match on
//! [`Key`]). Lists that aren't settings (saved models, plugins, themes, colour roles) are one
//! [`Kind::Block`] entry each here; their items are searched separately ([`super::search`]).

use super::Tab;

macro_rules! keys {
    ($($(#[$doc:meta])* $k:ident),* $(,)?) => {
        /// One setting (or block) of the settings screen.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Key { $($(#[$doc])* $k,)* }
        impl Key {
            /// Every key, in declaration order.
            #[cfg_attr(not(test), allow(dead_code))]
            pub const ALL: &'static [Key] = &[$(Key::$k,)*];
        }
    };
}

keys! {
    ModelList, VisionRole, SubagentRole, UtilityRole,
    Instructions, Approvals, ToolCalls, Subagents, Compaction, ModelImages, ToolCalling, WorkspaceSettings,
    PluginList,
    Mode, ThemePair, Gallery, Accent, Radius, ChartColours, MessageLayout, UserBubble, ReplyBubble, BubblePadding, LineSpacing, ShownPreset, ShownList, Controls, Backdrop, Glass, Frost, PanelGap, Arrangement, ImportTheme, ExportTheme, ResetLook, ResetAppearance,
    Zoom, TextSize, CodeSize, Font, CodeFont, Density, ColumnWidth, ChatListWidth, ArtifactPane, HighContrast, ReduceMotion, ResetText,
    ColourRoles,
}

/// How a setting is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Name and description on the left, the control on the right.
    Row,
    /// Name and description, the control under them (long text).
    Wide,
    /// A list drawn across the section (no name of its own: the section heading names it).
    Block,
}

/// A group of settings within a tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    pub tab: Tab,
    pub id: &'static str,
    /// Empty: no heading (a page with one block).
    pub title: &'static str,
    pub blurb: &'static str,
}

/// One setting.
#[derive(Debug, Clone, Copy)]
pub struct Setting {
    pub key: Key,
    /// For `--settings-section` and deep links.
    pub id: &'static str,
    pub section: &'static Section,
    pub name: &'static str,
    pub desc: &'static str,
    /// Other words people use for it (lower case, space separated).
    pub keywords: &'static str,
    pub kind: Kind,
}

const fn sec(tab: Tab, id: &'static str, title: &'static str, blurb: &'static str) -> Section {
    Section { tab, id, title, blurb }
}

pub static SAVED_MODELS: Section =
    sec(Tab::Models, "saved-models", "Saved models", "Chats use the default one unless their workspace or the chat picks another.");
pub static ROLES: Section = sec(Tab::Models, "roles", "Roles", "Saved models that do jobs for the others.");
pub static INSTRUCTIONS: Section = sec(Tab::Defaults, "instructions", "", "");
pub static AUTONOMY: Section = sec(Tab::Defaults, "autonomy", "Permissions and limits", "What the model may do on its own before it has to ask or answer.");
pub static REQUESTS: Section = sec(Tab::Defaults, "requests", "Context and requests", "What goes to the model with each request.");
pub static SCOPES: Section = sec(
    Tab::Defaults,
    "workspaces",
    "Workspaces and chats",
    "A workspace can change each of these for its chats, and a chat for itself (the sliders next to its title in the header).",
);
pub static PLUGINS: Section = sec(Tab::Plugins, "plugins", "", "");
pub static THEME: Section = sec(Tab::Appearance, "theme", "Theme", "");
pub static STYLE: Section = sec(Tab::Appearance, "style", "Accent and shape", "Used across the whole app, on top of the theme.");
pub static MESSAGES: Section = sec(Tab::Appearance, "messages", "Messages", "How the conversation is drawn.");
pub static SHOWN: Section = sec(
    Tab::Appearance,
    "shown",
    "What's shown",
    "Details, labels and hints, and the order of the header and of a reply's details. Kept for every window and browser tab. Or ask byob, e.g. \"hide the token counts\".",
);
pub static COCKPIT: Section = sec(
    Tab::Appearance,
    "cockpit",
    "Cockpit (experimental)",
    "A scene drawn behind byob with the panels as glass, the parts placed anywhere, and controls to match. Or ask byob, e.g. \"make it a starship's bridge\".",
);
pub static THEME_FILES: Section =
    sec(Tab::Appearance, "theme-files", "Theme files", "Keep the current look (colours and the settings above) as a .json theme file, or add someone else's.");
pub static RESET_LOOK: Section = sec(Tab::Appearance, "reset", "Reset", "");
pub static SIZE: Section = sec(Tab::Text, "size", "Size", "");
pub static FONTS: Section = sec(Tab::Text, "fonts", "Fonts", "");
pub static LAYOUT: Section = sec(Tab::Text, "layout", "Layout", "How the window shares its space.");
pub static ACCESSIBILITY: Section = sec(Tab::Text, "accessibility", "Accessibility", "");
pub static RESET_TEXT: Section = sec(Tab::Text, "reset", "Reset", "");
pub static COLOURS: Section = sec(Tab::Colours, "colours", "", "");

/// Every section, in page order.
pub static SECTIONS: &[&Section] = &[
    &SAVED_MODELS,
    &ROLES,
    &INSTRUCTIONS,
    &AUTONOMY,
    &REQUESTS,
    &SCOPES,
    &PLUGINS,
    &THEME,
    &STYLE,
    &MESSAGES,
    &SHOWN,
    &COCKPIT,
    &THEME_FILES,
    &RESET_LOOK,
    &SIZE,
    &FONTS,
    &LAYOUT,
    &ACCESSIBILITY,
    &RESET_TEXT,
    &COLOURS,
];

const fn s(key: Key, id: &'static str, section: &'static Section, name: &'static str, desc: &'static str, keywords: &'static str, kind: Kind) -> Setting {
    Setting { key, id, section, name, desc, keywords, kind }
}

use Kind::{Block, Row, Wide};

/// Every setting, in page order.
pub static SETTINGS: &[Setting] = &[
    // Models: engine-wide, for every window and browser tab.
    s(
        Key::ModelList,
        "model-list",
        &SAVED_MODELS,
        "Saved models",
        "Add the models your servers run, test them, pick the default.",
        "model models add server endpoint connection url api key provider openai ollama llama vllm lm studio default test duplicate delete profile",
        Block,
    ),
    s(
        Key::VisionRole,
        "vision-role",
        &ROLES,
        "Vision helper",
        "Answers questions about images for a model that can't see them.",
        "images pictures see vision role",
        Row,
    ),
    s(
        Key::SubagentRole,
        "subagent-role",
        &ROLES,
        "Subagent model",
        "The model delegated tasks run on, unless the call, the agent, a workspace or a chat picks another.",
        "subagents delegate role",
        Row,
    ),
    s(
        Key::UtilityRole,
        "utility-role",
        &ROLES,
        "Titles & summaries",
        "Names chats and writes the summaries that compact them (else the chat's model); a small, fast one will do.",
        "utility title summary compaction small fast role",
        Row,
    ),
    // Chats: the global layer of the layered settings (settings.json).
    s(
        Key::Instructions,
        "instructions",
        &INSTRUCTIONS,
        "Custom instructions",
        "Added to the system prompt of every chat: how to answer, what to know about you.",
        "system prompt custom instructions persona rules",
        Wide,
    ),
    s(
        Key::Approvals,
        "approvals",
        &AUTONOMY,
        "Approvals",
        "Which tool calls run without asking. Reads always run; edits, network and commands can ask first.",
        "permissions ask confirm allow safety read only edits sandbox trust",
        Row,
    ),
    s(
        Key::ToolCalls,
        "tool-calls",
        &AUTONOMY,
        "Tool calls per turn",
        "How many tools the model may call in one reply before it has to answer. Long jobs need more.",
        "limit max maximum steps loop iterations",
        Row,
    ),
    s(
        Key::Subagents,
        "subagents",
        &AUTONOMY,
        "Subagents",
        "Whether the model may hand tasks to subagents (the delegate tool).",
        "delegate agents parallel",
        Row,
    ),
    s(
        Key::Compaction,
        "compaction",
        &REQUESTS,
        "Compaction",
        "Summarise older messages when a request passes this much of the context window.",
        "context window summarise summarize summary tokens auto compact memory",
        Row,
    ),
    s(
        Key::ModelImages,
        "model-images",
        &REQUESTS,
        "Images to the model",
        "Re-encode the images the model looks at as JPEG when that makes them smaller.",
        "jpeg quality vision attachments size compress pictures",
        Row,
    ),
    s(
        Key::ToolCalling,
        "tool-calling",
        &REQUESTS,
        "Tool calling",
        "How tools reach the model: as native tool calls, or described in the prompt for models without them.",
        "function calling native prompt tools format mode",
        Row,
    ),
    s(
        Key::WorkspaceSettings,
        "workspace-settings",
        &SCOPES,
        "This workspace",
        "Its defaults for its chats, and where its commands run (Docker, Nix, this machine).",
        "workspace docker container nix environment folder project",
        Row,
    ),
    // Plugins.
    s(
        Key::PluginList,
        "plugin-list",
        &PLUGINS,
        "Plugins",
        "Tools, skills and servers byob can use; their switches and settings.",
        "plugins extensions tools mcp servers skills trust enable disable reload",
        Block,
    ),
    // Appearance: this device only.
    s(Key::Mode, "mode", &THEME, "Mode", "Light, dark, or follow the system.", "dark mode light mode night system appearance", Row),
    s(
        Key::ThemePair,
        "theme-pair",
        &THEME,
        "Light and dark themes",
        "The theme each mode uses; System switches between them with the computer.",
        "scheme palette light theme dark theme",
        Row,
    ),
    s(
        Key::Gallery,
        "gallery",
        &THEME,
        "Theme gallery",
        "Click a theme to use it; right-click one you added to remove it.",
        "themes scheme palette preset catppuccin gruvbox nord solarized dracula tokyo rose pine",
        Block,
    ),
    s(Key::Accent, "accent", &STYLE, "Accent", "The colour of buttons, links and the selection.", "colour color highlight tint brand", Row),
    s(Key::Radius, "radius", &STYLE, "Corner radius", "How round buttons, cards and bubbles are.", "rounded corners shape roundness", Row),
    s(
        Key::ChartColours,
        "chart-colours",
        &STYLE,
        "Chart colours",
        "The theme's colours, or a palette people with colour blindness can tell apart.",
        "colorblind colour blind palette graphs plots accessibility",
        Row,
    ),
    s(
        Key::MessageLayout,
        "message-layout",
        &MESSAGES,
        "Layout",
        "One column, or a chat with your messages on the right.",
        "align alignment stacked side right left bubbles",
        Row,
    ),
    s(Key::UserBubble, "user-bubble", &MESSAGES, "Your messages", "How your messages are drawn.", "bubble user style filled outlined minimal", Row),
    s(Key::ReplyBubble, "reply-bubble", &MESSAGES, "Replies", "How the model's replies are drawn.", "assistant bubble response answer style", Row),
    s(
        Key::BubblePadding,
        "bubble-padding",
        &MESSAGES,
        "Bubble padding",
        "Space between a message's edge and its text.",
        "padding inset margin space bubble indent",
        Row,
    ),
    s(Key::LineSpacing, "line-spacing", &MESSAGES, "Line spacing", "Space between the lines of messages.", "line height leading spacing density text", Row),
    s(
        Key::ShownPreset,
        "shown-preset",
        &SHOWN,
        "Detail",
        "Everything, or details on hover and fewer labels.",
        "layout minimal clean clutter busy details hide show preset simple",
        Row,
    ),
    s(
        Key::ShownList,
        "shown-list",
        &SHOWN,
        "Each part",
        "Show, hide or move each part.",
        "layout hide show hover order reorder move tokens time speed stats header model chip context meter wordmark shortcuts hints labels you byob thinking plugins toolbar",
        Block,
    ),
    s(Key::Controls, "controls", &COCKPIT, "Controls", "How buttons are drawn.", "buttons hud plate pill style widgets", Row),
    s(
        Key::Backdrop,
        "backdrop",
        &COCKPIT,
        "Backdrop",
        "A scene behind the panels.",
        "background shader scene wallpaper animated stars aurora cockpit wgsl",
        Row,
    ),
    s(Key::Glass, "glass", &COCKPIT, "Glass", "How much the panels hide the backdrop.", "transparency opacity translucent see-through panels", Row),
    s(Key::Frost, "frost", &COCKPIT, "Frosted glass", "Blur the backdrop behind the panels.", "blur frosted glass acrylic mica", Row),
    s(Key::PanelGap, "panel-gap", &COCKPIT, "Space between panels", "Where the backdrop shows around docked panels.", "gap margin spacing panels", Row),
    s(
        Key::Arrangement,
        "arrangement",
        &COCKPIT,
        "Arrangement",
        "Where the window's parts are.",
        "layout free floating move panels place drag focus cockpit docked",
        Row,
    ),
    s(Key::ImportTheme, "import-theme", &THEME_FILES, "Add a theme", "Import a .json theme file.", "import load file json reload folder", Row),
    s(
        Key::ExportTheme,
        "export-theme",
        &THEME_FILES,
        "Keep this look",
        "Save the current look under a name, as a file or in the gallery.",
        "save export share json file",
        Row,
    ),
    s(Key::ResetLook, "reset-look", &RESET_LOOK, "Reset the look", "Accent, corners, bubbles and chart colours back to the theme's.", "defaults restore", Row),
    s(
        Key::ResetAppearance,
        "reset-appearance",
        &RESET_LOOK,
        "Reset all appearance",
        "Themes, accent, text, layout and colours back to the defaults. Imported themes stay.",
        "defaults restore factory everything",
        Row,
    ),
    // Text & layout: this device only.
    s(Key::Zoom, "zoom", &SIZE, "Zoom", "Everything bigger or smaller: text, icons and spacing.", "scale magnify interface size bigger smaller ui", Row),
    s(Key::TextSize, "text-size", &SIZE, "Text size", "Text in the chat and the interface.", "font size bigger larger smaller", Row),
    s(Key::CodeSize, "code-size", &SIZE, "Code size", "Code blocks and inline code.", "monospace font size", Row),
    s(Key::Font, "font", &FONTS, "Font", "The typeface of the interface and the chat.", "typeface inter ubuntu", Row),
    s(Key::CodeFont, "code-font", &FONTS, "Code font", "The typeface of code.", "monospace typeface hack jetbrains mono", Row),
    s(Key::Density, "density", &LAYOUT, "Density", "How much space around and between things.", "compact spacious comfortable spacing padding", Row),
    s(
        Key::ColumnWidth,
        "column-width",
        &LAYOUT,
        "Conversation width",
        "How wide the conversation gets in a wide window.",
        "column max width reading measure",
        Row,
    ),
    s(Key::ChatListWidth, "chat-list-width", &LAYOUT, "Chat list width", "You can also drag the list's edge.", "sidebar width", Row),
    s(Key::ArtifactPane, "artifact-pane", &LAYOUT, "Artifact pane", "How much of the window the artifact pane takes.", "side panel split width share", Row),
    s(
        Key::HighContrast,
        "high-contrast",
        &ACCESSIBILITY,
        "High contrast",
        "Brighter secondary text and borders.",
        "contrast visibility vision legibility",
        Row,
    ),
    s(Key::ReduceMotion, "reduce-motion", &ACCESSIBILITY, "Reduce motion", "No spinners, animations or smooth scrolling.", "animation motion vestibular", Row),
    s(
        Key::ResetText,
        "reset-text",
        &RESET_TEXT,
        "Reset text & layout",
        "Zoom, sizes, fonts, spacing and layout back to the defaults.",
        "defaults restore",
        Row,
    ),
    // Colours: every role of the current theme, this device only.
    s(
        Key::ColourRoles,
        "colour-roles",
        &COLOURS,
        "Colour roles",
        "Every colour of the current theme, each with a picker.",
        "colour color override hex palette custom roles background text border syntax code",
        Block,
    ),
];

/// The setting for `key`.
pub fn get(key: Key) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.key == key)
}

/// A setting by its id.
pub fn by_id(id: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.id == id)
}

/// The words of a query: lower case, split at spaces.
pub fn words(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_ascii_lowercase).collect()
}

/// How well `s` matches every word (`None`: some word doesn't match): its name counts most,
/// then the description, then keywords, its section and its tab.
pub fn score(s: &Setting, words: &[String]) -> Option<u32> {
    if words.is_empty() {
        return None;
    }
    let name = s.name.to_ascii_lowercase();
    let desc = s.desc.to_ascii_lowercase();
    let rest = format!("{} {} {}", s.keywords, s.section.title.to_ascii_lowercase(), s.section.tab.label().to_ascii_lowercase());
    let mut total = 0;
    for w in words {
        total += if name.split(|c: char| !c.is_alphanumeric()).any(|p| p.starts_with(w.as_str())) {
            8
        } else if name.contains(w.as_str()) {
            6
        } else if desc.contains(w.as_str()) {
            3
        } else if rest.contains(w.as_str()) {
            2
        } else {
            return None;
        };
    }
    Some(total)
}

/// `text` contains every word (ASCII case ignored).
pub fn matches_all(text: &str, words: &[String]) -> bool {
    let lower = text.to_ascii_lowercase();
    !words.is_empty() && words.iter().all(|w| lower.contains(w.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_is_declared_once_with_a_unique_id_in_page_order() {
        for k in Key::ALL {
            assert_eq!(SETTINGS.iter().filter(|s| s.key == *k).count(), 1, "{k:?}");
        }
        let mut ids: Vec<&str> = SETTINGS.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), SETTINGS.len(), "ids are unique");
        // Settings run in section order, sections in tab order: a page draws them as listed.
        let at = |s: &Setting| SECTIONS.iter().position(|x| std::ptr::eq(*x, s.section));
        let order: Vec<usize> = SETTINGS.iter().map(|s| at(s).unwrap()).collect();
        assert!(order.windows(2).all(|w| w[0] <= w[1]), "{order:?}");
        let tabs: Vec<usize> = SECTIONS.iter().map(|s| Tab::ALL.iter().position(|t| *t == s.tab).unwrap()).collect();
        assert!(tabs.windows(2).all(|w| w[0] <= w[1]), "{tabs:?}");
        // Every section has a setting; every tab a section.
        for sec in SECTIONS {
            assert!(SETTINGS.iter().any(|s| std::ptr::eq(s.section, *sec)), "{}", sec.id);
        }
        for tab in Tab::ALL {
            assert!(SECTIONS.iter().any(|s| s.tab == tab), "{tab:?}");
        }
        // Ids don't clash with the tab names `--settings-section` also takes.
        for s in SETTINGS {
            assert!(Tab::ALL.iter().all(|t| t.id() != s.id), "{}", s.id);
        }
    }

    #[test]
    fn descriptions_are_one_readable_sentence_or_two() {
        for s in SETTINGS {
            assert!(!s.name.is_empty() && s.desc.len() <= 120, "{}: {} characters", s.id, s.desc.len());
            assert!(s.desc.ends_with('.'), "{}", s.id);
            assert_eq!(s.keywords, s.keywords.to_ascii_lowercase(), "{}", s.id);
        }
    }

    #[test]
    fn search_matches_names_descriptions_and_keywords_and_ranks_names_first() {
        let find = |q: &str| {
            let w = words(q);
            let mut hits: Vec<(&str, u32)> = SETTINGS.iter().filter_map(|s| score(s, &w).map(|n| (s.id, n))).collect();
            hits.sort_by_key(|h| std::cmp::Reverse(h.1));
            hits.into_iter().map(|h| h.0).collect::<Vec<_>>()
        };
        assert_eq!(find("accent").first(), Some(&"accent"), "by name");
        assert!(find("colour blindness").contains(&"chart-colours"), "by description");
        assert!(find("dark mode").contains(&"mode"), "by keyword");
        assert!(find("SYSTEM PROMPT").contains(&"instructions"), "case ignored");
        assert!(find("sidebar").contains(&"chat-list-width"));
        assert!(find("font").iter().take(2).all(|id| ["font", "code-font"].contains(id)), "{:?}", find("font"));
        assert!(find("xyzzy").is_empty());
        assert!(find("").is_empty());
    }
}
