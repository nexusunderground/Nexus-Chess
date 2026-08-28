//! Board overlay drawing — highlights and arrows painted directly onto the
//! transparent fullscreen app canvas.
//!
//! # Architecture
//!
//! RustyChessApp IS the fullscreen transparent eframe window (same pattern as
//! Nexus).  Each frame, `app.rs` calls `draw_board_highlights` via a
//! `ctx.layer_painter` to paint over whatever is on screen.  There is no
//! separate viewport or thread.
//!
//! # Transparency
//!
//! `clear_color` on `RustyChessApp` returns `TRANSPARENT`, clearing the GL
//! framebuffer to nothing each frame.  Win32 `WS_EX_LAYERED` is applied once
//! post-creation in `setup_window()` so DWM composites with per-pixel alpha.
//!
//! # Board-rect detection
//!
//! CDP supplies the board rect from `getBoundingClientRect()` in the page.
//! The Win32 window-scan heuristic is a fallback for the first frame before
//! the first CDP poll completes.

use egui::{Color32, Pos2, Rect, Vec2};
use super::arrows;

// ── Drawing ────────────────────────────────────────────────────────────────────

/// Draw from/to square highlights + directional arrow for `best_uci`.
///
pub fn draw_board_highlights(
    painter: &egui::Painter,
    analysis: &crate::engine::AnalysisResult,
    board_rect: Rect,
    flipped: bool,
    display_lines: u32, 
) {
    let sq_size = board_rect.width() / 8.0;

    // Colour per rank: gold / silver / bronze
    let colors: [(Color32, Color32); 3] = [
        (Color32::from_rgba_unmultiplied(255, 210, 50,  130), Color32::from_rgba_unmultiplied(80, 170, 255, 220)),
        (Color32::from_rgba_unmultiplied(180, 180, 180, 110), Color32::from_rgba_unmultiplied(140, 220, 140, 200)),
        (Color32::from_rgba_unmultiplied(200, 130, 60,  100), Color32::from_rgba_unmultiplied(220, 160, 80,  180)),
    ];
    let count = (display_lines as usize).min(analysis.lines.len()).min(3);

    for (i, line) in analysis.lines.iter().take(count).enumerate() {
        let Some(uci) = line.pv.first() else { continue };
        let Some((from_sq, to_sq)) = arrows::uci_to_squares(uci) else { continue };
        let (sq_color, arrow_color) = colors[i];

        // From square
        let from_rect = square_rect(board_rect, from_sq, sq_size, flipped);
        painter.rect_filled(from_rect, 0.0, sq_color);

        // To square
        let to_rect = square_rect(board_rect, to_sq, sq_size, flipped);
        painter.rect_filled(to_rect, 0.0, sq_color);

        // Arrow
        arrows::draw_arrow(painter, board_rect, from_sq, to_sq, sq_size, arrow_color, flipped);

     
let label_pos = from_rect.center();
let score_color = if line.centipawns > 15 {
    Color32::from_rgb(80, 200, 110)   // green
} else if line.centipawns < -15 {
    Color32::from_rgb(215, 80, 80)    // red
} else {
    Color32::from_rgb(90, 165, 235)   // blue (equal)
};
// Shadow
painter.text(
    label_pos + egui::vec2(1.0, 1.0),
    egui::Align2::CENTER_CENTER,
    &line.score_display,
    egui::FontId::proportional(13.0),
    Color32::from_black_alpha(200),
);
// Score
painter.text(
    label_pos,
    egui::Align2::CENTER_CENTER,
    &line.score_display,
    egui::FontId::proportional(13.0),
    score_color,
);
    }
}

fn square_rect(board: Rect, sq: (u8, u8), sq_size: f32, flipped: bool) -> Rect {
    let (file, rank) = sq;
    let col = if flipped { 7.0 - file as f32 } else { file as f32 };
    let row = if flipped { rank as f32 } else { 7.0 - rank as f32 };
    let min = board.min + Vec2::new(col * sq_size, row * sq_size);
    Rect::from_min_size(min, Vec2::splat(sq_size))
}

// ── Board-rect resolution ──────────────────────────────────────────────────────

/// Resolve the board rect in screen pixels.
///
/// Priority:
/// 1. CDP-supplied rect (accurate, from `getBoundingClientRect` in the page).
/// 2. Win32 window-scan heuristic (rough, Windows only).
/// 3. `None` — caller should suppress the overlay.
pub fn resolve_board_rect(cdp_rect: Option<Rect>) -> Option<Rect> {
    if let Some(r) = cdp_rect {
        if r.width() > 10.0 && r.height() > 10.0 {
            return Some(r);
        }
    }

    #[cfg(target_os = "windows")]
    return find_board_windows();

    #[cfg(not(target_os = "windows"))]
    None
}

/// Win32 fallback: walk top-level windows looking for a Chrome window whose
/// title contains "chess", then estimate the board rect from the client area.
///
/// This is intentionally conservative — the heuristic is only used when CDP
/// hasn't provided a board rect yet (e.g. very first frame before first poll).
#[cfg(target_os = "windows")]
fn find_board_windows() -> Option<Rect> {
    use std::sync::{Arc, Mutex};
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT};
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClientRect, GetWindowTextW, IsWindowVisible,
    };

    let result: Arc<Mutex<Option<Rect>>> = Arc::new(Mutex::new(None));
    let result_clone = result.clone();

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let result = unsafe { &*(lparam.0 as *const Mutex<Option<Rect>>) };
        if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            return BOOL(1);
        }

        let mut buf = [0u16; 512];
        let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
        if len == 0 {
            return BOOL(1);
        }

        let title = String::from_utf16_lossy(&buf[..len as usize]).to_lowercase();
        if !title.contains("chess") {
            return BOOL(1);
        }

        let mut cr = RECT::default();
        if unsafe { GetClientRect(hwnd, &mut cr) }.is_err() {
            return BOOL(1);
        }

        let mut origin = POINT { x: 0, y: 0 };
        unsafe { ClientToScreen(hwnd, &mut origin) };

        let w = (cr.right - cr.left) as f32;
        let h = (cr.bottom - cr.top) as f32;
        let ox = origin.x as f32;
        let oy = origin.y as f32;

        // Conservative heuristic: the board is roughly square, centred
        // vertically, and offset from the left sidebar (~13 % of window width).
        let sidebar_frac = 0.13_f32;
        let avail_w = w * (1.0 - sidebar_frac);
        let board_size = h.min(avail_w * 0.72);
        let board_x = ox + w * sidebar_frac + (avail_w - board_size) * 0.5;
        let board_y = oy + (h - board_size) * 0.5;

        *result.lock().unwrap() = Some(Rect::from_min_size(
            Pos2::new(board_x, board_y),
            Vec2::splat(board_size),
        ));
        BOOL(0) // stop enumeration once we find the first match
    }

    let raw_ptr = Arc::as_ptr(&result_clone) as isize;
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(raw_ptr));
    }
    result.lock().unwrap().take()
}

/// Discrete mode: single small pulsing dot on the target square corner.
/// Much less obvious than arrows but still gives the hint.
pub fn draw_discrete_indicator(
    painter: &egui::Painter,
    analysis: &crate::engine::AnalysisResult,
    board_rect: Rect,
    flipped: bool,
) {
    let Some(line) = analysis.lines.first() else { return };
    let Some(uci) = line.pv.first() else { return };
    let Some((_from, to_sq)) = super::arrows::uci_to_squares(uci) else { return };

    let sq_size = board_rect.width() / 8.0;
    let to_rect = square_rect(board_rect, to_sq, sq_size, flipped);

    // Pulsing radius
    let t = painter.ctx().input(|i| i.time) as f32;
    let pulse = (t * 3.0).sin() * 0.5 + 0.5;
    let radius = 5.0 + pulse * 3.0;
    let alpha = (160.0 + pulse * 80.0) as u8;

    let dot_pos = to_rect.min + egui::vec2(sq_size * 0.18, sq_size * 0.18);
    // Outer glow
    painter.circle_filled(
        dot_pos,
        radius + 2.0,
        egui::Color32::from_rgba_unmultiplied(80, 170, 255, alpha / 3),
    );
    // Core dot
    painter.circle_filled(
        dot_pos,
        radius,
        egui::Color32::from_rgba_unmultiplied(80, 170, 255, alpha),
    );
    // Score text
    painter.text(
        dot_pos + egui::vec2(radius + 3.0, -4.0),
        egui::Align2::LEFT_CENTER,
        &line.score_display,
        egui::FontId::proportional(9.0),
        egui::Color32::from_rgba_unmultiplied(200, 200, 255, alpha),
    );

    painter.ctx().request_repaint_after(std::time::Duration::from_millis(50));
}

// ── Eval bar ──────────────────────────────────────────────────────────────────
 
/// Draw a vertical eval bar to the left of the board.
///
/// White advantage fills from the bottom, black from the top.
/// Clamped to ±800cp so mate doesn't pin the bar at the extreme permanently.
pub fn draw_eval_bar(
    painter: &egui::Painter,
    board_rect: egui::Rect,
    smoothed_cp: f32,
    _flipped: bool,
) {
    let bar_width = 8.0;
    let gap       = 4.0;
    let bar_rect  = egui::Rect::from_min_size(
        egui::Pos2::new(board_rect.min.x - gap - bar_width, board_rect.min.y),
        egui::Vec2::new(bar_width, board_rect.height()),
    );
 
    // Background
    painter.rect_filled(bar_rect, 2.0, egui::Color32::from_rgba_unmultiplied(20, 20, 25, 200));
    painter.rect_stroke(
        bar_rect, 2.0,
        egui::Stroke::new(1.0, egui::Color32::from_rgb(40, 40, 55)),
        egui::StrokeKind::Inside,
    );
 
    let clamped = smoothed_cp.clamp(-800.0, 800.0);
    let t = (clamped + 800.0) / 1600.0;  // t=1 → White winning, t=0 → Black winning

    // When the board is flipped (playing Black) White's pieces are at the top,
    // so the White (light) portion of the bar should fill from the top.
    let white_frac = t;
    let white_h    = bar_rect.height() * white_frac;
    let black_h    = bar_rect.height() - white_h;
 
    // Black portion (top)
    if black_h > 0.0 {
        let black_rect = egui::Rect::from_min_size(
            bar_rect.min,
            egui::Vec2::new(bar_width, black_h),
        );
        painter.rect_filled(black_rect, egui::Rounding { nw: 2, ne: 2, sw: 0, se: 0 },
            egui::Color32::from_rgb(30, 30, 35));
    }
 
    // White portion (bottom)
    if white_h > 0.0 {
        let white_rect = egui::Rect::from_min_size(
            egui::Pos2::new(bar_rect.min.x, bar_rect.max.y - white_h),
            egui::Vec2::new(bar_width, white_h),
        );
        painter.rect_filled(white_rect, egui::Rounding { nw: 0, ne: 0, sw: 2, se: 2 },
            egui::Color32::from_rgb(220, 220, 220));
    }
 
    // Centre line
    painter.hline(
        bar_rect.min.x..=bar_rect.max.x,
        bar_rect.center().y,
        egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 40)),
    );
 
    // Eval text — show numeric score below the bar
    let cp_text = if smoothed_cp.abs() > 700.0 {
        "M".to_string()
    } else {
        format!("{:+.1}", smoothed_cp / 100.0)
    };
    painter.text(
        egui::Pos2::new(bar_rect.center().x, bar_rect.max.y + 4.0),
        egui::Align2::CENTER_TOP,
        &cp_text,
        egui::FontId::proportional(9.0),
        egui::Color32::from_rgba_unmultiplied(180, 180, 190, 200),
    );
}