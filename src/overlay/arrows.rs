use egui::{Color32, Painter, Pos2, Rect, Stroke, Vec2};

/// Draw an arrow from `from_sq` to `to_sq` on the board rect.
///
/// `sq_size` is the pixel size of one square.
/// The arrow head is filled with `color`.
pub fn draw_arrow(
    painter: &Painter,
    board_rect: Rect,
    from_sq: (u8, u8), // (file 0-7, rank 0-7), rank 0 = rank 1
    to_sq: (u8, u8),
    sq_size: f32,
    color: Color32,
    flipped: bool,
) {
    let center = |file: u8, rank: u8| -> Pos2 {
    let f = if flipped { 7.0 - file as f32 } else { file as f32 };
    let r = if flipped { rank as f32 } else { 7.0 - rank as f32 };
    board_rect.min + Vec2::new(f * sq_size + sq_size * 0.5, r * sq_size + sq_size * 0.5)
};

    let from = center(from_sq.0, from_sq.1);
    let to = center(to_sq.0, to_sq.1);

    let stroke_w = sq_size * 0.12;
    let head_len = sq_size * 0.35;

    let dir = (to - from).normalized();
    let perp = Vec2::new(-dir.y, dir.x);

    // Shorten the shaft so the head looks clean.
    let shaft_end = to - dir * head_len * 0.8;

    painter.line_segment([from, shaft_end], Stroke::new(stroke_w, color));

    // Arrowhead triangle.
    let tip = to;
    let left = shaft_end + perp * head_len * 0.5;
    let right = shaft_end - perp * head_len * 0.5;
    painter.add(egui::Shape::convex_polygon(
        vec![tip, left, right],
        color,
        Stroke::NONE,
    ));
}

/// Convert a UCI move string (e.g. "e2e4") to (from_sq, to_sq) tuples.
pub fn uci_to_squares(uci: &str) -> Option<((u8, u8), (u8, u8))> {
    let bytes = uci.as_bytes();
    if bytes.len() < 4 {
        return None;
    }
    let file_of = |b: u8| b.wrapping_sub(b'a');
    let rank_of = |b: u8| b.wrapping_sub(b'1');

    Some((
        (file_of(bytes[0]), rank_of(bytes[1])),
        (file_of(bytes[2]), rank_of(bytes[3])),
    ))
}
