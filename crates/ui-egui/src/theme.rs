//! Design tokens. Every widget reads colours from [`Tokens`] so themes apply everywhere.

use egui::Color32;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tokens {
    pub app_bg: Color32,
    pub panel_bg: Color32,
    pub field_bg: Color32,
    pub user_bubble: Color32,
    pub assistant_bubble: Color32,
    pub accent: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub danger: Color32,
}

impl Tokens {
    pub fn dark() -> Self {
        Self {
            app_bg: Color32::from_rgb(0x16, 0x17, 0x1a),
            panel_bg: Color32::from_rgb(0x1e, 0x20, 0x24),
            field_bg: Color32::from_rgb(0x12, 0x13, 0x16),
            user_bubble: Color32::from_rgb(0x26, 0x2d, 0x3d),
            assistant_bubble: Color32::from_rgb(0x22, 0x24, 0x29),
            accent: Color32::from_rgb(0x6c, 0x9e, 0xf8),
            text: Color32::from_rgb(0xe6, 0xe6, 0xe6),
            text_dim: Color32::from_rgb(0x9a, 0x9d, 0xa3),
            danger: Color32::from_rgb(0xf0, 0x6a, 0x6a),
        }
    }

    pub fn apply(&self, ctx: &egui::Context) {
        let mut v = egui::Visuals::dark();
        v.panel_fill = self.panel_bg;
        v.window_fill = self.app_bg;
        v.extreme_bg_color = self.field_bg;
        v.selection.bg_fill = self.accent.gamma_multiply(0.5);
        v.hyperlink_color = self.accent;
        v.override_text_color = Some(self.text);
        // yah has one (dark) theme for now; don't follow the OS/browser light preference.
        ctx.set_theme(egui::ThemePreference::Dark);
        ctx.set_visuals_of(egui::Theme::Dark, v);
    }
}
