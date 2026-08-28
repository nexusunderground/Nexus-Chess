use egui::{Color32, RichText, Ui};
use super::theme;

// ── Section frame ─────────────────────────────────────────────────────────────

/// Double-border titled section frame.
pub fn double_border_frame(
    ui: &mut Ui,
    title: &str,
    accent: Color32,
    add_contents: impl FnOnce(&mut Ui),
) {
    egui::Frame::new()
        .fill(theme::BG_FRAME)
        .stroke(egui::Stroke::new(1.0, theme::BORDER_FRAME))
        .corner_radius(3.0)
        .inner_margin(egui::Margin::same(2))
        .show(ui, |ui| {
            egui::Frame::new()
                .fill(theme::BG_FRAME_INNER)
                .stroke(egui::Stroke::new(1.0, theme::BORDER_FRAME_INNER))
                .corner_radius(2.0)
                .inner_margin(egui::Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(title.to_uppercase())
                            .size(10.0)
                            .color(theme::TEXT_HEADER)
                            .strong(),
                    );
                    // Accent underline bar
                    let bar_width = ui.available_width().min(55.0);
                    let (bar_rect, _) =
                        ui.allocate_exact_size(egui::vec2(bar_width, 2.0), egui::Sense::hover());
                    ui.painter().rect_filled(bar_rect, 1.0, accent);
                    ui.add_space(5.0);
                    add_contents(ui);
                });
        });
}

// ── Score colour ──────────────────────────────────────────────────────────────

/// Colour-code a score string:
///   positive (e.g. "+1.4")  → green
///   negative (e.g. "-0.3")  → red
///   near-zero / mate = 0    → blue
pub fn score_color(cp: i32, score_str: &str) -> Color32 {
    // Mate scores passed through as "M0" / "#M3" etc. from the engine.
    let is_mate = score_str.contains('M') || score_str.contains('#');
    if is_mate && cp == 0 {
        // M0 = game over
        return Color32::from_rgb(120, 120, 140);
    }
    if cp.abs() <= 15 {
        // Roughly equal
        Color32::from_rgb(90, 165, 235)   // blue
    } else if cp > 0 {
        Color32::from_rgb(80, 200, 110)   // green
    } else {
        Color32::from_rgb(215, 80, 80)    // red
    }
}

// ── Toggle / checkbox ─────────────────────────────────────────────────────────

pub fn styled_toggle(ui: &mut Ui, value: &mut bool, label: &str, hotkey: Option<&str>) -> egui::Response {
    let resp = ui.horizontal(|ui| {
        let size = egui::vec2(12.0, 12.0);
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
        if response.clicked() {
            *value = !*value;
        }
        if ui.is_rect_visible(rect) {
            if *value {
                let glow = rect.expand(2.0);
                ui.painter()
                    .rect_filled(glow, 3.0, Color32::from_rgba_unmultiplied(220, 180, 90, 35));
                ui.painter().rect_filled(rect, 2.0, theme::ACCENT_PRIMARY);
                ui.painter().rect_stroke(
                    rect, 2.0,
                    egui::Stroke::new(1.0, theme::ACCENT_SECONDARY),
                    egui::StrokeKind::Inside,
                );
            } else {
                ui.painter().rect_stroke(
                    rect, 2.0,
                    egui::Stroke::new(1.0, theme::BORDER_DEFAULT),
                    egui::StrokeKind::Inside,
                );
            }
        }
        ui.add_space(6.0);
        let col = if *value { theme::ACCENT_SECONDARY } else { theme::TEXT_SECONDARY };
        let lbl = ui.label(RichText::new(label).size(11.0).color(col));
        if let Some(key) = hotkey {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(key).size(9.0).color(theme::TEXT_MUTED));
            });
        }
        lbl
    });
    ui.add_space(2.0);
    resp.inner
}

// ── Slider ────────────────────────────────────────────────────────────────────

pub fn styled_slider(
    ui: &mut Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    suffix: &str,
) {
    ui.label(RichText::new(label.to_lowercase()).size(10.0).color(theme::TEXT_SECONDARY));
    ui.add_space(2.0);
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 14.0), egui::Sense::click_and_drag());
    if response.dragged() || response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let t = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            *value = egui::lerp(range.clone(), t);
        }
    }
    if ui.is_rect_visible(rect) {
        let min = *range.start();
        let max = *range.end();
        let t = (*value - min) / (max - min);
        ui.painter().rect_filled(rect, 2.0, theme::BG_LIGHT);
        ui.painter().rect_stroke(
            rect, 2.0,
            egui::Stroke::new(1.0, theme::BORDER_DEFAULT),
            egui::StrokeKind::Inside,
        );
        let fw = rect.width() * t;
        if fw > 0.0 {
            let fill = egui::Rect::from_min_size(rect.min, egui::vec2(fw, rect.height()));
            ui.painter().rect_filled(fill, 2.0, theme::ACCENT_PRIMARY);
        }
    }
    let text = if suffix.is_empty() {
        format!("{:.1}", *value)
    } else {
        format!("{:.0} {}", *value, suffix)
    };
    ui.label(RichText::new(text).size(9.0).color(theme::TEXT_MUTED));
    ui.add_space(4.0);
}

// ── PV line row (large) ───────────────────────────────────────────────────────

/// Larger PV row used on the Overview tab.
pub fn pv_row_large(
    ui: &mut Ui,
    rank: u32,
    score: &str,
    cp: i32,
    pv_moves: &[String],
    accent: Color32,
) {
    let rank_color = match rank {
        1 => accent,
        2 => theme::ACCENT_INFO,
        _ => Color32::from_rgb(120, 100, 60),
    };

    egui::Frame::new()
        .fill(theme::BG_LIGHT)
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            // Top row: rank + score + first move
            ui.horizontal(|ui| {
                let rank_str = match rank { 1 => "①", 2 => "②", _ => "③" };
                ui.label(RichText::new(rank_str).size(13.0).color(rank_color).strong());
                ui.add_space(6.0);

                // Colour-coded score
                let sc = score_color(cp, score);
                ui.label(RichText::new(score).size(14.0).color(sc).strong());
                ui.add_space(8.0);

                // First move — largest element
                if let Some(first) = pv_moves.first() {
                    ui.label(RichText::new(first).size(16.0).color(theme::TEXT_PRIMARY).strong());
                }

                // Continuation moves (right-aligned)
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if pv_moves.len() > 5 {
                        ui.label(RichText::new("…").size(9.0).color(theme::TEXT_MUTED));
                    }
                    for mv in pv_moves.iter().skip(1).take(4).rev() {
                        ui.label(RichText::new(mv).size(10.0).color(theme::TEXT_SECONDARY));
                        ui.add_space(2.0);
                    }
                });
            });

            // Continuation preview row
            if pv_moves.len() > 1 {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.add_space(28.0);
                    let continuation: Vec<&str> = pv_moves.iter().skip(1).take(5).map(|s| s.as_str()).collect();
                    ui.label(
                        RichText::new(continuation.join("  "))
                            .size(9.0)
                            .color(theme::TEXT_MUTED),
                    );
                });
            }
        });
    ui.add_space(3.0);
}