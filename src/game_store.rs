//! Persistent game history.
//!
//! Each completed game is saved as a `GameRecord`.  The store is capped at
//! `MAX_GAMES` entries (oldest dropped first) and serialised to
//! `%LOCALAPPDATA%\rustychess\games.json`.

use serde::{Deserialize, Serialize};

const MAX_GAMES: usize = 100;

fn games_path() -> std::path::PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("rustychess")
        .join("games.json")
}

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum GameSite {
    ChessCom,
    Lichess,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum GameResult {
    WhiteWins,
    BlackWins,
    Draw,
    /// Game ended without a clear result (idle timeout, navigation away, etc.)
    Unknown,
}

impl GameResult {
    pub fn display(&self) -> &'static str {
        match self {
            Self::WhiteWins => "WHITE WON",
            Self::BlackWins => "BLACK WON",
            Self::Draw      => "DRAW",
            Self::Unknown   => "—",
        }
    }
    pub fn colour(&self, white_is_you: bool) -> GameResultColour {
        match self {
            Self::WhiteWins => if white_is_you { GameResultColour::Win  } else { GameResultColour::Loss },
            Self::BlackWins => if white_is_you { GameResultColour::Loss } else { GameResultColour::Win  },
            Self::Draw      => GameResultColour::Draw,
            Self::Unknown   => GameResultColour::Neutral,
        }
    }
    /// Parse the result string extracted from the DOM.
    pub fn from_dom(s: &str) -> Self {
        let lower = s.to_ascii_lowercase();

        // ── Explicit score tokens ──────────────────────────────────────────
        if lower.contains("1-0") { return Self::WhiteWins; }
        if lower.contains("0-1") { return Self::BlackWins; }
        if lower.contains("1/2") || lower.contains("½") { return Self::Draw; }

        // ── Draws ──────────────────────────────────────────────────────────
        if lower.contains("draw") || lower.contains("stalemate")
            || lower.contains("repetition") || lower.contains("insufficient material")
            || lower.contains("50-move") || lower.contains("agreement")
        {
            return Self::Draw;
        }

        // ── White wins ─────────────────────────────────────────────────────
        if lower.contains("white wins")
            || lower.contains("white is victorious")
            || lower.contains("white won")
            || (lower.contains("checkmate") && lower.contains("white"))
            || (lower.contains("black") && (
                lower.contains("ran out of time")
                || lower.contains("out of time")
                || lower.contains("resigned")
                || lower.contains("forfeited")
                || lower.contains("abandoned")
                || lower.contains("disconnected")
                || lower.contains("left the game")
            ))
        {
            return Self::WhiteWins;
        }

        // ── Black wins ─────────────────────────────────────────────────────
        if lower.contains("black wins")
            || lower.contains("black is victorious")
            || lower.contains("black won")
            || (lower.contains("checkmate") && lower.contains("black"))
            || (lower.contains("white") && (
                lower.contains("ran out of time")
                || lower.contains("out of time")
                || lower.contains("resigned")
                || lower.contains("forfeited")
                || lower.contains("abandoned")
                || lower.contains("disconnected")
                || lower.contains("left the game")
            ))
        {
            return Self::BlackWins;
        }

        // ── Time forfeit without explicit side ─────────────────────────────
        if lower.contains("won on time") || lower.contains("time forfeit") {
            if lower.contains("black won") || lower.contains("black wins") {
                return Self::BlackWins;
            }
            if lower.contains("white won") || lower.contains("white wins") {
                return Self::WhiteWins;
            }
            return Self::Unknown;
        }

        // ── Lichess result text ────────────────────────────────────────────
        // e.g. "White wins by resignation", "Black wins by time forfeit"
        if lower.contains("by resignation") || lower.contains("by forfeit")
            || lower.contains("by abandonment") || lower.contains("by timeout")
        {
            if lower.contains("white") { return Self::WhiteWins; }
            if lower.contains("black") { return Self::BlackWins; }
        }

        Self::Unknown
    }
}

pub enum GameResultColour { Win, Loss, Draw, Neutral }

// ── Move review / classification ──────────────────────────────────────────────

/// Per-move quality classification, in roughly descending order of quality.
/// Mirrors the familiar chess.com / Lichess vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MoveClass {
    /// A strong move that sacrifices material yet keeps the position winning.
    Brilliant,
    /// The only good move — every alternative is significantly worse.
    Great,
    /// Matches the engine's top choice.
    Best,
    /// Near-optimal (tiny centipawn loss).
    Excellent,
    /// Solid (small centipawn loss).
    Good,
    /// A known opening-book move.
    Book,
    /// Slightly inaccurate.
    Inaccuracy,
    /// A clear mistake.
    Mistake,
    /// A serious error losing significant evaluation.
    Blunder,
}

impl MoveClass {
    /// Short glyph shown beside the move in the UI.
    pub fn glyph(&self) -> &'static str {
        match self {
            Self::Brilliant  => "!!",
            Self::Great      => "!",
            Self::Best       => "★",
            Self::Excellent  => "✓",
            Self::Good       => "·",
            Self::Book       => "📖",
            Self::Inaccuracy => "?!",
            Self::Mistake    => "?",
            Self::Blunder    => "??",
        }
    }

    /// Human-readable label.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Brilliant  => "Brilliant",
            Self::Great      => "Great",
            Self::Best       => "Best",
            Self::Excellent  => "Excellent",
            Self::Good       => "Good",
            Self::Book       => "Book",
            Self::Inaccuracy => "Inaccuracy",
            Self::Mistake    => "Mistake",
            Self::Blunder    => "Blunder",
        }
    }

    /// RGB colour used to tint the move token and badges.
    pub fn rgb(&self) -> (u8, u8, u8) {
        match self {
            Self::Brilliant  => (38, 194, 167),  // teal
            Self::Great      => (92, 158, 222),   // blue
            Self::Best       => (129, 182, 76),   // green
            Self::Excellent  => (149, 187, 79),   // light green
            Self::Good       => (160, 168, 130),  // muted olive
            Self::Book       => (168, 138, 95),   // tan
            Self::Inaccuracy => (240, 193, 90),   // yellow
            Self::Mistake    => (231, 144, 60),   // orange
            Self::Blunder    => (202, 70, 70),    // red
        }
    }

    /// Numeric Annotation Glyph code for PGN export (`$N`).
    /// Returns `None` for ordinary play so the exported PGN stays readable —
    /// only exceptional moves (a brilliancy or an error) carry a glyph.
    pub fn nag(&self) -> Option<u8> {
        match self {
            Self::Brilliant  => Some(3),  // !!
            Self::Inaccuracy => Some(6),  // ?!
            Self::Mistake    => Some(2),  // ?
            Self::Blunder    => Some(4),  // ??
            // Best / Great / Excellent / Good / Book are normal good play and
            // are intentionally left unmarked in the PGN.
            _ => None,
        }
    }
}

/// A single classified move within a reviewed game.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveAnnotation {
    /// 0-based ply index into the game's move list.
    pub ply:        u32,
    /// The move that was played, in clean SAN (e.g. "Nf3").
    pub san:        String,
    /// Classification bucket.
    pub class:      MoveClass,
    /// Evaluation (White's perspective, centipawns) of the position *before*
    /// this move, assuming best play.
    pub cp_before:  i32,
    /// Evaluation (White's perspective, centipawns) of the position *after*
    /// this move, assuming best play.
    pub cp_after:   i32,
    /// The engine's preferred move at this position, in SAN (for "best was …").
    pub best_san:   String,
}

/// The result of a completed Game Review pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameReview {
    /// Search depth the review was run at.
    pub depth:           u32,
    /// One annotation per played move, in order.
    pub annotations:     Vec<MoveAnnotation>,
    /// Overall accuracy for each side, 0–100 (Lichess formula).
    pub accuracy_white:  f32,
    pub accuracy_black:  f32,
}

impl GameReview {
    /// Count moves of a given class for one side.  White plies are even
    /// (the game is always recorded from move 1), Black plies are odd.
    pub fn class_count_side(&self, class: MoveClass, white: bool) -> usize {
        self.annotations.iter()
            .filter(|a| a.class == class && (a.ply % 2 == 0) == white)
            .count()
    }

    /// Total classified moves for one side.
    pub fn move_count_side(&self, white: bool) -> usize {
        self.annotations.iter()
            .filter(|a| (a.ply % 2 == 0) == white)
            .count()
    }

    /// Rough playing-strength estimate (Elo) derived from a side's accuracy.
    ///
    /// This is a heuristic, not a calibrated rating: accuracy depends on how
    /// forcing the position was, so the figure is best read as a ballpark.
    /// The mapping is a monotonic interpolation through hand-picked anchors
    /// spanning beginner → super-GM.
    pub fn accuracy_to_elo(acc: f32) -> u32 {
        // (accuracy %, approx Elo) anchor points, ascending.
        const ANCHORS: &[(f32, f32)] = &[
            (50.0,  600.0),
            (60.0,  850.0),
            (70.0, 1150.0),
            (75.0, 1350.0),
            (80.0, 1550.0),
            (85.0, 1800.0),
            (90.0, 2050.0),
            (93.0, 2250.0),
            (95.0, 2400.0),
            (97.0, 2600.0),
            (99.0, 2800.0),
            (100.0, 3000.0),
        ];
        let a = acc.clamp(0.0, 100.0);
        if a <= ANCHORS[0].0 {
            // Extrapolate gently below the lowest anchor, floored at 400.
            return (600.0 - (ANCHORS[0].0 - a) * 8.0).max(400.0).round() as u32;
        }
        for win in ANCHORS.windows(2) {
            let (a0, e0) = win[0];
            let (a1, e1) = win[1];
            if a <= a1 {
                let t = (a - a0) / (a1 - a0);
                return (e0 + t * (e1 - e0)).round() as u32;
            }
        }
        3000
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameRecord {
    /// Milliseconds since UNIX epoch — used as stable key.
    pub id:          u64,
    pub site:        GameSite,
    pub white:       String,
    pub black:       String,
    pub result:      GameResult,
    /// ECO code + opening name if one was detected.
    pub opening:     Option<(String, String)>,
    /// Flat move list, e.g. ["1. e4", "1… e5", "2. Nf3", …]
    pub moves:       Vec<String>,
    /// ISO-8601 local date/time, e.g. "2026-06-13 14:22"
    pub played_at:   String,
    /// Post-game review (move classifications + accuracy), or `None` until the
    /// user runs the manual analysis.
    #[serde(default)]
    pub review:      Option<GameReview>,
}

impl GameRecord {
    pub fn move_count(&self) -> usize {
        self.moves.len()
    }

    /// Format moves as a PGN-style move text string for clipboard copy.
    /// Preserves the original starting move number (e.g. if history starts at
    /// move 6, the output starts "6. Nh7 cxd4 7. …" rather than "1. …").
    pub fn pgn_moves(&self) -> String {
        fn strip(s: &str) -> &str {
            if let Some(pos) = s.find(". ") {
                s[pos + 2..].trim()
            } else if let Some(pos) = s.find("\u{2026} ") {
                // U+2026 ELLIPSIS = 3 UTF-8 bytes + 1 space = 4 bytes total
                s[pos + 4..].trim()
            } else {
                s.trim()
            }
        }
        // Recover the starting move number from the first stored entry.
        // Entries look like "6. e4" (white) or "6… e5" (black).
        fn parse_mnum(s: &str) -> Option<u32> {
            let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse().ok()
        }
        let start_mnum = self.moves.first()
            .and_then(|s| parse_mnum(s))
            .unwrap_or(1);
        let mut out = String::new();
        let mut i = 0usize;
        let mut mnum = start_mnum;
        while i < self.moves.len() {
            let white = strip(&self.moves[i]);
            let black = self.moves.get(i + 1).map(|s| strip(s)).unwrap_or("");
            if !white.is_empty() {
                if !out.is_empty() { out.push(' '); }
                out.push_str(&format!("{mnum}. {white}"));
                if !black.is_empty() { out.push_str(&format!(" {black}")); }
            }
            i += 2;
            mnum += 1;
        }
        // Append result token
        let result_tok = match self.result {
            GameResult::WhiteWins => " 1-0",
            GameResult::BlackWins => " 0-1",
            GameResult::Draw      => " 1/2-1/2",
            GameResult::Unknown   => "",
        };
        out.push_str(result_tok);
        out
    }

    /// Full PGN with headers.
    pub fn pgn_full(&self) -> String {
        let site_str = match self.site {
            GameSite::ChessCom => "Chess.com",
            GameSite::Lichess  => "Lichess.org",
            GameSite::Unknown  => "?",
        };
        let result_str = match self.result {
            GameResult::WhiteWins => "1-0",
            GameResult::BlackWins => "0-1",
            GameResult::Draw      => "1/2-1/2",
            GameResult::Unknown   => "*",
        };
        format!(
            "[Event \"?\"]\n[Site \"{}\"]\n[Date \"{}\"]\n[White \"{}\"]\n[Black \"{}\"]\n[Result \"{}\"]\n\n{}\n",
            site_str,
            self.played_at,
            self.white,
            self.black,
            result_str,
            self.pgn_moves(),
        )
    }

    /// Full PGN with per-move NAG annotations and eval comments, using the
    /// stored review.  Falls back to [`Self::pgn_full`] when no review exists.
    ///
    /// The output is standard PGN that any chess GUI (Lichess, chess.com,
    /// ChessBase, SCID, …) parses — paste it into an analysis board to get the
    /// classified moves on a real board.
    pub fn pgn_annotated(&self) -> String {
        let Some(review) = &self.review else { return self.pgn_full(); };

        let site_str = match self.site {
            GameSite::ChessCom => "Chess.com",
            GameSite::Lichess  => "Lichess.org",
            GameSite::Unknown  => "?",
        };
        let result_str = match self.result {
            GameResult::WhiteWins => "1-0",
            GameResult::BlackWins => "0-1",
            GameResult::Draw      => "1/2-1/2",
            GameResult::Unknown   => "*",
        };

        fn strip(s: &str) -> &str {
            if let Some(pos) = s.find(". ") {
                s[pos + 2..].trim()
            } else if let Some(pos) = s.find("\u{2026} ") {
                s[pos + 4..].trim()
            } else {
                s.trim()
            }
        }
        fn parse_mnum(s: &str) -> Option<u32> {
            let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse().ok()
        }
        // Format a White-perspective centipawn score as a PGN-friendly eval.
        fn fmt_eval(cp: i32) -> String {
            if cp.abs() >= 29_000 {
                let mate = (30_000 - cp.abs()).max(0);
                if cp > 0 { format!("#{mate}") } else { format!("#-{mate}") }
            } else {
                format!("{:+.2}", cp as f32 / 100.0)
            }
        }

        let start_mnum = self.moves.first()
            .and_then(|s| parse_mnum(s))
            .unwrap_or(1);

        let mut out = String::new();
        let mut i = 0usize;
        let mut mnum = start_mnum;
        while i < self.moves.len() {
            // White ply.
            let white = strip(&self.moves[i]);
            if !white.is_empty() {
                if !out.is_empty() { out.push(' '); }
                out.push_str(&format!("{mnum}. {white}"));
                append_annotation(&mut out, review.annotations.get(i), fmt_eval);
            }
            // Black ply.
            if let Some(b_entry) = self.moves.get(i + 1) {
                let black = strip(b_entry);
                if !black.is_empty() {
                    out.push(' ');
                    // If the white move had a comment, PGN needs the move number
                    // repeated with "..." before a standalone black move; but
                    // since we keep them on one logical line this is optional and
                    // most parsers accept the bare SAN. Keep it simple/compatible.
                    out.push_str(black);
                    append_annotation(&mut out, review.annotations.get(i + 1), fmt_eval);
                }
            }
            i += 2;
            mnum += 1;
        }

        let result_tok = match self.result {
            GameResult::WhiteWins => " 1-0",
            GameResult::BlackWins => " 0-1",
            GameResult::Draw      => " 1/2-1/2",
            GameResult::Unknown   => "",
        };
        out.push_str(result_tok);

        format!(
            "[Event \"?\"]\n[Site \"{}\"]\n[Date \"{}\"]\n[White \"{}\"]\n[Black \"{}\"]\n[Result \"{}\"]\n\
             [Annotator \"RustyChess (depth {})\"]\n[WhiteAccuracy \"{:.1}\"]\n[BlackAccuracy \"{:.1}\"]\n\n{}\n",
            site_str,
            self.played_at,
            self.white,
            self.black,
            result_str,
            review.depth,
            review.accuracy_white,
            review.accuracy_black,
            out,
        )
    }
}

/// Append a NAG glyph and (for notable moves) an eval/best-move comment for one
/// annotated ply to a PGN move-text buffer.
fn append_annotation(
    out:     &mut String,
    ann:     Option<&MoveAnnotation>,
    fmt_eval: fn(i32) -> String,
) {
    let Some(ann) = ann else { return; };
    if let Some(nag) = ann.class.nag() {
        out.push_str(&format!(" ${nag}"));
    }
    // Add an explanatory comment only for moves worth narrating.  Best/Great
    // play is left bare so a forcing sequence of only-moves doesn't drown the
    // PGN in comments — only brilliancies and errors get a note.
    let notable = matches!(
        ann.class,
        MoveClass::Brilliant
            | MoveClass::Inaccuracy | MoveClass::Mistake | MoveClass::Blunder
    );
    if notable {
        let mut comment = format!("{}. {}", ann.class.label(), fmt_eval(ann.cp_after));
        if !ann.best_san.is_empty() && ann.best_san != ann.san {
            comment.push_str(&format!(". Best: {}", ann.best_san));
        }
        out.push_str(&format!(" {{{comment}}}"));
    }
}

// ── Store ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GameStore {
    pub games: Vec<GameRecord>,
}

impl GameStore {
    pub fn load() -> Self {
        let path = games_path();
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = games_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, json);
        }
    }

    /// Add a new game record, dropping the oldest if over the cap.
    /// If a record with the same id already exists and the new record has more
    /// moves, it is replaced (so a game committed early with few moves gets
    /// updated when it is committed again at the end with the full move list).
    /// Returns `false` if the game is too short to bother.
    pub fn commit(&mut self, record: GameRecord) -> bool {
        if record.moves.len() < 4 { return false; }
        // If already stored: update only when the new record has more moves
        // (or a resolved result vs Unknown), otherwise leave the existing one.
        if let Some(pos) = self.games.iter().position(|g| g.id == record.id) {
            let existing = &self.games[pos];
            let update = record.moves.len() > existing.moves.len()
                || (existing.result == GameResult::Unknown
                    && record.result != GameResult::Unknown);
            if !update { return false; }
            let mut record = record;
            // Preserve an existing review when the move list is unchanged — a
            // re-commit (e.g. a second idle/result event) must not wipe a
            // review the user already ran.  If the moves changed, the old
            // review is stale and correctly dropped.
            if record.review.is_none()
                && existing.review.is_some()
                && record.moves == existing.moves
            {
                record.review = existing.review.clone();
            }
            self.games[pos] = record;
            self.save();
            return true;
        }
        self.games.insert(0, record);
        if self.games.len() > MAX_GAMES {
            self.games.truncate(MAX_GAMES);
        }
        self.save();
        true
    }

    pub fn clear(&mut self) {
        self.games.clear();
        self.save();
    }
}

// ── Timestamp helper ──────────────────────────────────────────────────────────

/// Returns milliseconds since UNIX epoch (used as record ID).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Returns a human-readable local datetime string ("2026-06-13 14:22").
/// Falls back to the ms timestamp on error.
pub fn now_display() -> String {
    // No chrono dep — use a simple UTC approximation via UNIX time.
    // For local time we'd need chrono or a platform call; UTC is acceptable
    // for a revision log and avoids an extra dependency.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Simple conversion: days since epoch, no DST/timezone.
    let s = secs;
    let days = s / 86400;
    let time_in_day = s % 86400;
    let h = time_in_day / 3600;
    let m = (time_in_day % 3600) / 60;
    // Gregorian calendar calculation (Zeller-ish, good for 1970–2100).
    let mut y = 1970u64;
    let mut remaining_days = days;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let days_in_year = if leap { 366 } else { 365 };
        if remaining_days < days_in_year { break; }
        remaining_days -= days_in_year;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let month_days: [u64; 12] = [31, if leap {29} else {28}, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut mo = 1u64;
    for &md in &month_days {
        if remaining_days < md { break; }
        remaining_days -= md;
        mo += 1;
    }
    let d = remaining_days + 1;
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}")
}
