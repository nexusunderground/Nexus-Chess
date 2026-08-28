#![allow(dead_code)]

use egui::Color32;

// ── Background layers ────────────────────────────────────────────────────────
// More spread between stops — layers are now visually distinct
pub const BG_DARK:        Color32 = Color32::from_rgb(8,  8,  14);
pub const BG_MEDIUM:      Color32 = Color32::from_rgb(14, 14, 22);
pub const BG_LIGHT:       Color32 = Color32::from_rgb(22, 22, 34);
pub const BG_HOVER:       Color32 = Color32::from_rgb(32, 32, 50);
pub const BG_FRAME:       Color32 = Color32::from_rgb(11, 11, 18);
pub const BG_FRAME_INNER: Color32 = Color32::from_rgb(7,  7,  12);

// ── Accent palette ───────────────────────────────────────────────────────────
pub const ACCENT_PRIMARY:     Color32 = Color32::from_rgb(220, 180, 90);
pub const ACCENT_SECONDARY:   Color32 = Color32::from_rgb(248, 218, 130);
pub const ACCENT_PRIMARY_DIM: Color32 = Color32::from_rgb(170, 130, 60);  // was too dark to read
pub const ACCENT_SUCCESS:     Color32 = Color32::from_rgb(90,  215, 130);
pub const ACCENT_WARNING:     Color32 = Color32::from_rgb(225, 185, 85);
pub const ACCENT_DANGER:      Color32 = Color32::from_rgb(215, 80,  80);
pub const ACCENT_INFO:        Color32 = Color32::from_rgb(90,  165, 235);

// Arrow overlay colors (unchanged — already work well)
pub const ARROW_BLUE:   Color32 = Color32::from_rgba_premultiplied(80,  170, 255, 210);
pub const ARROW_GREEN:  Color32 = Color32::from_rgba_premultiplied(60,  220, 100, 200);
pub const ARROW_ORANGE: Color32 = Color32::from_rgba_premultiplied(255, 160, 50,  200);

// ── Text ─────────────────────────────────────────────────────────────────────
// Lifted across the board; MUTED bumped significantly (+25 lightness)
pub const TEXT_PRIMARY:   Color32 = Color32::from_rgb(235, 235, 245);
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(175, 175, 195);
pub const TEXT_MUTED:     Color32 = Color32::from_rgb(110, 110, 130); // was 85 — marginal contrast
pub const TEXT_LABEL:     Color32 = Color32::from_rgb(195, 195, 215);
pub const TEXT_HEADER:    Color32 = Color32::from_rgb(225, 225, 245);

// ── Borders ──────────────────────────────────────────────────────────────────
// All bumped ~10 units — actually visible now against BG_MEDIUM
pub const BORDER_DEFAULT:     Color32 = Color32::from_rgb(50, 50, 70);
pub const BORDER_FRAME:       Color32 = Color32::from_rgb(62, 62, 86);
pub const BORDER_FRAME_INNER: Color32 = Color32::from_rgb(44, 44, 64);
pub const BORDER_FOCUS:       Color32 = Color32::from_rgb(220, 180, 90);
pub const BORDER_ACTIVE:      Color32 = Color32::from_rgb(90,  215, 130);

// ── Helpers ──────────────────────────────────────────────────────────────────
pub fn accent_from_rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

pub fn eval_color(centipawns: i32) -> Color32 {
    if centipawns > 150 {
        Color32::from_rgb(90, 215, 130)
    } else if centipawns > 0 {
        Color32::from_rgb(185, 225, 145)
    } else if centipawns > -150 {
        Color32::from_rgb(225, 185, 120)
    } else {
        Color32::from_rgb(210, 95, 95)
    }
}