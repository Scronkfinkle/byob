//! Built-in theme presets, as data. Each preset is a [`Palette`]: the scheme's own named
//! colours mapped onto a small set of slots, from which [`super::Tokens::derive`] builds
//! every role. Hex values are the schemes' official ones (credits in
//! `crates/ui-egui/assets/themes/palettes.attribution`); slots a scheme has no colour for are
//! derived from its neighbours and marked as such.

/// One preset. Colours are `0xRRGGBB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Stable id used in preferences and theme files (`catppuccin-mocha`).
    pub id: &'static str,
    pub name: &'static str,
    pub dark: bool,
    /// Who made the scheme (shown in the gallery tooltip).
    pub credit: &'static str,
    /// Window background (the transcript).
    pub bg: u32,
    /// Header, composer, settings, sidebar.
    pub panel: u32,
    /// Text inputs (and code blocks on dark themes).
    pub field: u32,
    /// Three raised surface steps: buttons, hover, pressed (also borders).
    pub surface: [u32; 3],
    /// Faint text, comments, placeholders.
    pub overlay: u32,
    /// Secondary text.
    pub subtext: u32,
    pub text: u32,
    /// Headings and bold; `None` → derived from `text`.
    pub strong: Option<u32>,
    pub red: u32,
    pub yellow: u32,
    pub green: u32,
    /// Named accent colours the user can pick from; the first is the default.
    pub accents: &'static [(&'static str, u32)],
    /// `inline code` text.
    pub inline_code: u32,
    /// Code block syntax: keyword, string, number, comment, function, type.
    pub syntax: [u32; 6],
    /// Chart series, in order.
    pub chart: [u32; 8],
}

impl Palette {
    pub fn accent(&self, name: &str) -> Option<u32> {
        self.accents.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, c)| *c)
    }

    pub fn default_accent(&self) -> u32 {
        self.accents.first().map_or(self.text, |(_, c)| *c)
    }
}

/// yah's validated categorical palette (colour-vision-deficiency ordered), dark surfaces.
pub const ACCESSIBLE_CHART_DARK: [u32; 8] = [0x3987e5, 0xd95926, 0x199e70, 0xc98500, 0xd55181, 0x008300, 0x9085e9, 0xe66767];
/// The same, stepped for light surfaces.
pub const ACCESSIBLE_CHART_LIGHT: [u32; 8] = [0x2a78d6, 0xeb6834, 0x1baf7a, 0xeda100, 0xe87ba4, 0x008300, 0x6250d6, 0xe34948];

pub const YAH_DARK: Palette = Palette {
    id: "yah-dark",
    name: "yah Dark",
    dark: true,
    credit: "yah",
    bg: 0x16171a,
    panel: 0x1c1d21,
    field: 0x121316,
    surface: [0x2b2d33, 0x34373e, 0x3e4149],
    overlay: 0x75787f,
    subtext: 0xa8abb2,
    text: 0xe4e5e8,
    strong: Some(0xffffff),
    red: 0xf78585,
    yellow: 0xe8b75a,
    green: 0x5cc98a,
    accents: &[("blue", 0x6c9ef8), ("violet", 0xa78bfa), ("green", 0x4fc28a), ("orange", 0xf59e4c), ("pink", 0xf472b6), ("teal", 0x2dd4bf)],
    inline_code: 0xf0c68a,
    syntax: [0xc59cf5, 0x9fd28c, 0xf0b37e, 0x8a8e97, 0x7cb4f5, 0xe5c07b],
    chart: ACCESSIBLE_CHART_DARK,
};

pub const YAH_LIGHT: Palette = Palette {
    id: "yah-light",
    name: "yah Light",
    dark: false,
    credit: "yah",
    bg: 0xf6f6f4,
    panel: 0xefefec,
    field: 0xffffff,
    surface: [0xe4e4e7, 0xd9d9de, 0xcbcbd2],
    overlay: 0x80838a,
    subtext: 0x55585f,
    text: 0x1d1e21,
    strong: Some(0x000000),
    red: 0xb42323,
    yellow: 0x8a5600,
    green: 0x1a6e3e,
    accents: &[("blue", 0x1f56c4), ("violet", 0x653fcc), ("green", 0x167347), ("orange", 0xa34a05), ("pink", 0xb0256c), ("teal", 0x0d6b63)],
    inline_code: 0x9a2f1f,
    syntax: [0x7b3fc4, 0x2f7a32, 0xa3500c, 0x6b6f78, 0x1f5fbf, 0x8a6100],
    chart: ACCESSIBLE_CHART_LIGHT,
};

/// A Catppuccin flavour from its named colours. Accents are listed mauve first (the default).
macro_rules! catppuccin {
    ($id:literal, $name:literal, dark: $dark:literal,
     rosewater $rw:literal, flamingo $fl:literal, pink $pk:literal, mauve $mv:literal, red $rd:literal, maroon $mr:literal,
     peach $pe:literal, yellow $ye:literal, green $gr:literal, teal $te:literal, sky $sk:literal, sapphire $sa:literal,
     blue $bl:literal, lavender $la:literal, text $tx:literal, subtext1 $_s1:literal, subtext0 $s0:literal,
     overlay2 $o2:literal, overlay1 $o1:literal, overlay0 $_o0:literal, surface2 $u2:literal, surface1 $u1:literal,
     surface0 $u0:literal, base $ba:literal, mantle $ma:literal, crust $cr:literal) => {
        Palette {
            id: $id,
            name: $name,
            dark: $dark,
            credit: "Catppuccin (MIT)",
            bg: $ba,
            panel: $ma,
            // Light flavours have nothing lighter than base: fields are derived (base → white).
            field: if $dark { $cr } else { 0xf9fafb },
            surface: [$u0, $u1, $u2],
            overlay: $o1,
            subtext: $s0,
            text: $tx,
            strong: None,
            red: $rd,
            yellow: $ye,
            green: $gr,
            accents: &[
                ("mauve", $mv),
                ("blue", $bl),
                ("lavender", $la),
                ("sapphire", $sa),
                ("sky", $sk),
                ("teal", $te),
                ("green", $gr),
                ("yellow", $ye),
                ("peach", $pe),
                ("maroon", $mr),
                ("red", $rd),
                ("pink", $pk),
                ("flamingo", $fl),
                ("rosewater", $rw),
            ],
            inline_code: $pe,
            // Catppuccin's style guide: keywords mauve, strings green, numbers peach,
            // comments overlay2, functions blue, types yellow.
            syntax: [$mv, $gr, $pe, $o2, $bl, $ye],
            chart: [$bl, $pe, $gr, $mv, $te, $ye, $pk, $rd],
        }
    };
}

pub const CATPPUCCIN_LATTE: Palette = catppuccin!("catppuccin-latte", "Catppuccin Latte", dark: false,
    rosewater 0xdc8a78, flamingo 0xdd7878, pink 0xea76cb, mauve 0x8839ef, red 0xd20f39, maroon 0xe64553,
    peach 0xfe640b, yellow 0xdf8e1d, green 0x40a02b, teal 0x179299, sky 0x04a5e5, sapphire 0x209fb5,
    blue 0x1e66f5, lavender 0x7287fd, text 0x4c4f69, subtext1 0x5c5f77, subtext0 0x6c6f85,
    overlay2 0x7c7f93, overlay1 0x8c8fa1, overlay0 0x9ca0b0, surface2 0xacb0be, surface1 0xbcc0cc,
    surface0 0xccd0da, base 0xeff1f5, mantle 0xe6e9ef, crust 0xdce0e8);

pub const CATPPUCCIN_FRAPPE: Palette = catppuccin!("catppuccin-frappe", "Catppuccin Frappé", dark: true,
    rosewater 0xf2d5cf, flamingo 0xeebebe, pink 0xf4b8e4, mauve 0xca9ee6, red 0xe78284, maroon 0xea999c,
    peach 0xef9f76, yellow 0xe5c890, green 0xa6d189, teal 0x81c8be, sky 0x99d1db, sapphire 0x85c1dc,
    blue 0x8caaee, lavender 0xbabbf1, text 0xc6d0f5, subtext1 0xb5bfe2, subtext0 0xa5adce,
    overlay2 0x949cbb, overlay1 0x838ba7, overlay0 0x737994, surface2 0x626880, surface1 0x51576d,
    surface0 0x414559, base 0x303446, mantle 0x292c3c, crust 0x232634);

pub const CATPPUCCIN_MACCHIATO: Palette = catppuccin!("catppuccin-macchiato", "Catppuccin Macchiato", dark: true,
    rosewater 0xf4dbd6, flamingo 0xf0c6c6, pink 0xf5bde6, mauve 0xc6a0f6, red 0xed8796, maroon 0xee99a0,
    peach 0xf5a97f, yellow 0xeed49f, green 0xa6da95, teal 0x8bd5ca, sky 0x91d7e3, sapphire 0x7dc4e4,
    blue 0x8aadf4, lavender 0xb7bdf8, text 0xcad3f5, subtext1 0xb8c0e0, subtext0 0xa5adcb,
    overlay2 0x939ab7, overlay1 0x8087a2, overlay0 0x6e738d, surface2 0x5b6078, surface1 0x494d64,
    surface0 0x363a4f, base 0x24273a, mantle 0x1e2030, crust 0x181926);

pub const CATPPUCCIN_MOCHA: Palette = catppuccin!("catppuccin-mocha", "Catppuccin Mocha", dark: true,
    rosewater 0xf5e0dc, flamingo 0xf2cdcd, pink 0xf5c2e7, mauve 0xcba6f7, red 0xf38ba8, maroon 0xeba0ac,
    peach 0xfab387, yellow 0xf9e2af, green 0xa6e3a1, teal 0x94e2d5, sky 0x89dceb, sapphire 0x74c7ec,
    blue 0x89b4fa, lavender 0xb4befe, text 0xcdd6f4, subtext1 0xbac2de, subtext0 0xa6adc8,
    overlay2 0x9399b2, overlay1 0x7f849c, overlay0 0x6c7086, surface2 0x585b70, surface1 0x45475a,
    surface0 0x313244, base 0x1e1e2e, mantle 0x181825, crust 0x11111b);

pub const GRUVBOX_DARK: Palette = Palette {
    id: "gruvbox-dark",
    name: "Gruvbox Dark",
    dark: true,
    credit: "Gruvbox by Pavel Pertsev (MIT)",
    bg: 0x282828,                            // bg0
    panel: 0x32302f,                         // bg0_s
    field: 0x1d2021,                         // bg0_h
    surface: [0x3c3836, 0x504945, 0x665c54], // bg1, bg2, bg3
    overlay: 0x928374,                       // gray
    subtext: 0xbdae93,                       // fg3
    text: 0xebdbb2,                          // fg
    strong: Some(0xfbf1c7),                  // fg0
    red: 0xfb4934,
    yellow: 0xfabd2f,
    green: 0xb8bb26,
    accents: &[
        ("orange", 0xfe8019),
        ("yellow", 0xfabd2f),
        ("green", 0xb8bb26),
        ("aqua", 0x8ec07c),
        ("blue", 0x83a598),
        ("purple", 0xd3869b),
        ("red", 0xfb4934),
    ],
    inline_code: 0xfe8019,
    syntax: [0xfb4934, 0xb8bb26, 0xd3869b, 0x928374, 0x8ec07c, 0xfabd2f],
    chart: [0x83a598, 0xfe8019, 0xb8bb26, 0xd3869b, 0x8ec07c, 0xfabd2f, 0xfb4934, 0xa89984],
};

pub const GRUVBOX_LIGHT: Palette = Palette {
    id: "gruvbox-light",
    name: "Gruvbox Light",
    dark: false,
    credit: "Gruvbox by Pavel Pertsev (MIT)",
    bg: 0xfbf1c7,                            // bg0
    panel: 0xf2e5bc,                         // bg0_s
    field: 0xf9f5d7,                         // bg0_h
    surface: [0xebdbb2, 0xd5c4a1, 0xbdae93], // bg1, bg2, bg3
    overlay: 0x928374,                       // gray
    subtext: 0x665c54,                       // fg3
    text: 0x3c3836,                          // fg
    strong: Some(0x282828),                  // fg0
    red: 0x9d0006,
    yellow: 0xb57614,
    green: 0x79740e,
    accents: &[
        ("orange", 0xaf3a03),
        ("yellow", 0xb57614),
        ("green", 0x79740e),
        ("aqua", 0x427b58),
        ("blue", 0x076678),
        ("purple", 0x8f3f71),
        ("red", 0x9d0006),
    ],
    inline_code: 0xaf3a03,
    syntax: [0x9d0006, 0x79740e, 0x8f3f71, 0x928374, 0x427b58, 0xb57614],
    chart: [0x076678, 0xaf3a03, 0x79740e, 0x8f3f71, 0x427b58, 0xb57614, 0x9d0006, 0x7c6f64],
};

pub const NORD: Palette = Palette {
    id: "nord",
    name: "Nord",
    dark: true,
    credit: "Nord by Arctic Ice Studio & Sven Greb (MIT)",
    bg: 0x2e3440,                            // nord0
    panel: 0x3b4252,                         // nord1
    field: 0x292e39,                         // derived: nord0, darker
    surface: [0x3b4252, 0x434c5e, 0x4c566a], // nord1..nord3
    overlay: 0x7b88a1,                       // derived: nord3, brighter (comments)
    subtext: 0xb4bccb,                       // derived: between nord4 and nord3
    text: 0xd8dee9,                          // nord4
    strong: Some(0xeceff4),                  // nord6
    red: 0xbf616a,
    yellow: 0xebcb8b,
    green: 0xa3be8c,
    accents: &[
        ("frost", 0x88c0d0),
        ("teal", 0x8fbcbb),
        ("blue", 0x81a1c1),
        ("deep blue", 0x5e81ac),
        ("purple", 0xb48ead),
        ("green", 0xa3be8c),
        ("yellow", 0xebcb8b),
        ("orange", 0xd08770),
        ("red", 0xbf616a),
    ],
    inline_code: 0xebcb8b,
    syntax: [0x81a1c1, 0xa3be8c, 0xb48ead, 0x7b88a1, 0x88c0d0, 0x8fbcbb],
    chart: [0x88c0d0, 0xd08770, 0xa3be8c, 0xb48ead, 0xebcb8b, 0x5e81ac, 0xbf616a, 0x8fbcbb],
};

pub const DRACULA: Palette = Palette {
    id: "dracula",
    name: "Dracula",
    dark: true,
    credit: "Dracula by Zeno Rocha (MIT)",
    bg: 0x282a36,
    panel: 0x21222c,                         // Dracula UI "darker"
    field: 0x191a21,                         // Dracula UI "darkest"
    surface: [0x44475a, 0x515469, 0x6272a4], // current line, derived, comment
    overlay: 0x6272a4,                       // comment
    subtext: 0xbcc2cd,                       // derived: foreground, dimmed
    text: 0xf8f8f2,
    strong: Some(0xffffff),
    red: 0xff5555,
    yellow: 0xf1fa8c,
    green: 0x50fa7b,
    accents: &[
        ("purple", 0xbd93f9),
        ("pink", 0xff79c6),
        ("cyan", 0x8be9fd),
        ("green", 0x50fa7b),
        ("orange", 0xffb86c),
        ("yellow", 0xf1fa8c),
        ("red", 0xff5555),
    ],
    inline_code: 0x50fa7b,
    syntax: [0xff79c6, 0xf1fa8c, 0xbd93f9, 0x6272a4, 0x50fa7b, 0x8be9fd],
    chart: [0xbd93f9, 0xffb86c, 0x50fa7b, 0xff79c6, 0x8be9fd, 0xf1fa8c, 0xff5555, 0x6272a4],
};

const SOLARIZED_ACCENTS: &[(&str, u32)] = &[
    ("blue", 0x268bd2),
    ("cyan", 0x2aa198),
    ("green", 0x859900),
    ("yellow", 0xb58900),
    ("orange", 0xcb4b16),
    ("red", 0xdc322f),
    ("magenta", 0xd33682),
    ("violet", 0x6c71c4),
];

pub const SOLARIZED_DARK: Palette = Palette {
    id: "solarized-dark",
    name: "Solarized Dark",
    dark: true,
    credit: "Solarized by Ethan Schoonover (MIT)",
    bg: 0x002b36,                            // base03
    panel: 0x073642,                         // base02
    field: 0x00212a,                         // derived: base03, darker
    surface: [0x073642, 0x0f4351, 0x586e75], // base02, derived, base01
    overlay: 0x657b83,                       // base00
    subtext: 0x839496,                       // base0
    // Body text uses base1 (Solarized's "emphasized content"): base0 is only ~4.7:1.
    text: 0x93a1a1,         // base1
    strong: Some(0xeee8d5), // base2
    red: 0xdc322f,
    yellow: 0xb58900,
    green: 0x859900,
    accents: SOLARIZED_ACCENTS,
    inline_code: 0xcb4b16,
    syntax: [0x859900, 0x2aa198, 0xd33682, 0x586e75, 0x268bd2, 0xb58900],
    chart: [0x268bd2, 0xcb4b16, 0x859900, 0xd33682, 0x2aa198, 0xb58900, 0x6c71c4, 0xdc322f],
};

pub const SOLARIZED_LIGHT: Palette = Palette {
    id: "solarized-light",
    name: "Solarized Light",
    dark: false,
    credit: "Solarized by Ethan Schoonover (MIT)",
    bg: 0xfdf6e3,                            // base3
    panel: 0xeee8d5,                         // base2
    field: 0xfffcf3,                         // derived: base3, lighter
    surface: [0xeee8d5, 0xe2dcc8, 0x93a1a1], // base2, derived, base1
    overlay: 0x93a1a1,                       // base1
    subtext: 0x657b83,                       // base00
    // Body text uses base01 (Solarized's "emphasized content"): base00 is only ~4.4:1.
    text: 0x586e75,         // base01
    strong: Some(0x073642), // base02
    red: 0xdc322f,
    yellow: 0xb58900,
    green: 0x859900,
    accents: SOLARIZED_ACCENTS,
    inline_code: 0xcb4b16,
    syntax: [0x859900, 0x2aa198, 0xd33682, 0x93a1a1, 0x268bd2, 0xb58900],
    chart: [0x268bd2, 0xcb4b16, 0x859900, 0xd33682, 0x2aa198, 0xb58900, 0x6c71c4, 0xdc322f],
};

pub const TOKYO_NIGHT: Palette = Palette {
    id: "tokyo-night",
    name: "Tokyo Night",
    dark: true,
    credit: "Tokyo Night by enkia (MIT)",
    bg: 0x1a1b26,
    panel: 0x16161e,                         // bg_dark
    field: 0x13131a,                         // derived: bg_dark, darker
    surface: [0x292e42, 0x3b4261, 0x414868], // bg_highlight, fg_gutter, terminal_black
    overlay: 0x565f89,                       // comment
    subtext: 0xa9b1d6,                       // fg_dark
    text: 0xc0caf5,                          // fg
    strong: None,
    red: 0xf7768e,
    yellow: 0xe0af68,
    green: 0x9ece6a,
    accents: &[
        ("blue", 0x7aa2f7),
        ("magenta", 0xbb9af7),
        ("cyan", 0x7dcfff),
        ("teal", 0x73daca),
        ("green", 0x9ece6a),
        ("yellow", 0xe0af68),
        ("orange", 0xff9e64),
        ("red", 0xf7768e),
    ],
    inline_code: 0x7dcfff,
    syntax: [0xbb9af7, 0x9ece6a, 0xff9e64, 0x565f89, 0x7aa2f7, 0x2ac3de],
    chart: [0x7aa2f7, 0xff9e64, 0x9ece6a, 0xbb9af7, 0x7dcfff, 0xe0af68, 0xf7768e, 0x73daca],
};

pub const ROSE_PINE: Palette = Palette {
    id: "rose-pine",
    name: "Rosé Pine",
    dark: true,
    credit: "Rosé Pine (MIT)",
    bg: 0x191724,                            // base
    panel: 0x1f1d2e,                         // surface
    field: 0x13111d,                         // derived: base, darker
    surface: [0x26233a, 0x403d52, 0x524f67], // overlay, highlight med, highlight high
    overlay: 0x6e6a86,                       // muted
    subtext: 0x908caa,                       // subtle
    text: 0xe0def4,
    strong: None,
    red: 0xeb6f92,    // love
    yellow: 0xf6c177, // gold
    green: 0x9ccfd8,  // foam (Rosé Pine has no green)
    accents: &[("iris", 0xc4a7e7), ("rose", 0xebbcba), ("foam", 0x9ccfd8), ("pine", 0x31748f), ("gold", 0xf6c177), ("love", 0xeb6f92)],
    inline_code: 0xebbcba,
    syntax: [0x31748f, 0xf6c177, 0xc4a7e7, 0x6e6a86, 0xebbcba, 0x9ccfd8],
    chart: [0x9ccfd8, 0xf6c177, 0x31748f, 0xc4a7e7, 0xeb6f92, 0xebbcba, 0x908caa, 0x6e6a86],
};

pub const ROSE_PINE_DAWN: Palette = Palette {
    id: "rose-pine-dawn",
    name: "Rosé Pine Dawn",
    dark: false,
    credit: "Rosé Pine (MIT)",
    bg: 0xfaf4ed,                            // base
    panel: 0xf2e9e1,                         // overlay
    field: 0xfffaf3,                         // surface
    surface: [0xf2e9e1, 0xdfdad9, 0xcecacd], // overlay, highlight med, highlight high
    overlay: 0x9893a5,                       // muted
    subtext: 0x797593,                       // subtle
    text: 0x575279,
    strong: None,
    red: 0xb4637a,    // love
    yellow: 0xea9d34, // gold
    green: 0x286983,  // pine (Rosé Pine has no green)
    accents: &[("iris", 0x907aa9), ("rose", 0xd7827e), ("foam", 0x56949f), ("pine", 0x286983), ("gold", 0xea9d34), ("love", 0xb4637a)],
    inline_code: 0xd7827e,
    syntax: [0x286983, 0xea9d34, 0x907aa9, 0x9893a5, 0xd7827e, 0x56949f],
    chart: [0x286983, 0xea9d34, 0x56949f, 0x907aa9, 0xb4637a, 0xd7827e, 0x797593, 0x9893a5],
};

/// A preset by id (case-insensitive).
pub fn find(id: &str) -> Option<&'static Palette> {
    PRESETS.iter().copied().find(|p| p.id.eq_ignore_ascii_case(id))
}

/// Every preset, in gallery order.
pub const PRESETS: &[&Palette] = &[
    &YAH_DARK,
    &YAH_LIGHT,
    &CATPPUCCIN_MOCHA,
    &CATPPUCCIN_MACCHIATO,
    &CATPPUCCIN_FRAPPE,
    &CATPPUCCIN_LATTE,
    &GRUVBOX_DARK,
    &GRUVBOX_LIGHT,
    &NORD,
    &DRACULA,
    &SOLARIZED_DARK,
    &SOLARIZED_LIGHT,
    &TOKYO_NIGHT,
    &ROSE_PINE,
    &ROSE_PINE_DAWN,
];
