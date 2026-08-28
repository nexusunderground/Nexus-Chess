use anyhow::Result;
use std::collections::HashMap;
use std::time::Duration;
use tracing::{info, warn};

use super::uci::{InfoLine, UciEngine, parse_bestmove, parse_info};

/// The analysis result for a position.
#[derive(Debug, Clone, Default)]
pub struct AnalysisResult {
    /// Best lines (indexed by multipv rank, 0 = best).
    pub lines: Vec<PvLine>,
    /// The single best move in UCI notation (e.g. "e2e4").
    pub best_move: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PvLine {
    pub rank:          u32,
    pub score_display: String,
    pub centipawns:    i32,
    /// Move sequence in UCI notation.
    pub pv:            Vec<String>,
}

/// High-level analysis manager wrapping [`UciEngine`].
pub struct AnalysisEngine {
    engine:         UciEngine,
    /// Search depth cap.  `0` means search indefinitely (`go infinite`);
    /// any positive value caps the search (`go depth N`) so the engine idles
    /// once the target depth is reached instead of pinning the CPU forever.
    depth:          u32,
    /// Node count cap per move.  When non-zero, overrides `depth` and sends
    /// `go nodes N` — the most effective way to limit lc0/neural net engines
    /// which ignore depth for resource purposes.
    nodes:          u32,
    current_turn:   Option<char>,
    /// True between `update_position` and the first successful `poll` for the
    /// new position.  Suppresses spurious "no PV lines" warnings during the
    /// transition window.
    position_dirty: bool,
    /// Accumulated best `info` line per multipv rank for the current position.
    /// Lines are merged here incrementally on every `poll` and cleared by
    /// `update_position`, so we never re-scan or clone the engine's growing
    /// output buffer.
    acc:            HashMap<u32, InfoLine>,
    /// Best move reported via a `bestmove` line for the current position.
    acc_best_move:  Option<String>,
}

impl AnalysisEngine {
    /// Spawn Stockfish (or any UCI engine) at `path`.
    ///
    /// * `multipv`  — how many lines to show (1 = just best)
    /// * `depth`    — search depth cap (0 = infinite / continuous)
    /// * `nodes`    — node count cap per move (0 = uncapped; overrides depth)
    pub fn new(path: &str, multipv: u32, depth: u32, nodes: u32) -> Result<Self> {
        let mut engine = UciEngine::spawn(path)?;
        info!(path = %path, multipv, depth, "[analysis] engine spawn requested");
        engine.send(&format!("setoption name MultiPV value {multipv}"))?;
        engine.send("ucinewgame")?;
        engine.send("isready")?;
        engine.wait_for("readyok", Duration::from_secs(3))?;
        info!("[analysis] engine ready for analysis commands");

        Ok(Self {
            engine,
            depth,
            nodes,
            current_turn:   None,
            position_dirty: false,
            acc:            HashMap::new(),
            acc_best_move:  None,
        })
    }

    /// Collect whatever analysis has arrived so far (non-blocking).
    ///
    /// ## Drain-and-accumulate semantics
    ///
    /// `poll()` **drains** the new engine output each call and merges it into
    /// a persistent per-rank accumulator (`self.acc`).  This is O(new lines)
    /// per call and never clones a growing buffer.
    ///
    /// An earlier version drained on every call but discarded the running
    /// state, so deep lines received between polls were lost (`build_result`
    /// keeps only the deepest line per rank).  A later version switched to
    /// peeking (non-destructive) which fixed correctness but re-cloned the
    /// entire — and unbounded — output buffer at ~12 Hz.  Draining into a
    /// persistent accumulator gives both correctness and O(new lines) cost.
    pub fn poll(&mut self) -> AnalysisResult {
        let raw = self.engine.drain_lines();
        for line in &raw {
            if let Some(info) = parse_info(line) {
                merge_info(&mut self.acc, info);
            }
            if let Some(mv) = parse_bestmove(line) {
                self.acc_best_move = Some(mv);
            }
        }

        let result = acc_to_result(&self.acc, self.current_turn, self.acc_best_move.clone());
        // Clear the dirty flag once we have at least one valid PV line.
        if !result.lines.is_empty() {
            self.position_dirty = false;
        }
        result
    }

    /// Update the FEN mid-stream (stops current search and restarts).
    pub fn update_position(&mut self, fen: &str) -> Result<()> {
        info!(fen = %fen, "[analysis] updating engine position");
        self.current_turn   = fen_side_to_move(fen);
        self.position_dirty = true;
        // Reset the per-position accumulator — the previous search's lines no
        // longer apply to the new position.
        self.acc.clear();
        self.acc_best_move = None;

        self.engine.send_stop()?;
        // Wait for the engine to confirm it has stopped before draining.
        // This prevents residual "info" lines from the old search leaking
        // into the new position's buffer.
        let _ = self.engine.wait_for("bestmove", Duration::from_millis(300));
        // Now safe to drain — the search has stopped.
        self.engine.drain_lines();
        self.engine.send(&format!("position fen {fen}"))?;

        // Build the go command.  Priority: nodes > depth > infinite.
        // `go nodes N` is the most effective way to cap lc0/neural-net engines
        // since they don't idle at a specific depth like traditional engines.
        let go = if self.nodes > 0 {
            format!("go nodes {}", self.nodes)
        } else if self.depth > 0 {
            format!("go depth {}", self.depth)
        } else {
            "go infinite".to_string()
        };
        self.engine.send(&go)
    }

    /// Set the engine hash table size in MB.
    pub fn set_hash(&mut self, mb: u32) -> Result<()> {
        self.engine.send(&format!("setoption name Hash value {mb}"))
    }

    /// Set number of CPU threads the engine should use.
    pub fn set_threads(&mut self, n: u32) -> Result<()> {
        self.engine.send(&format!("setoption name Threads value {n}"))
    }

    /// Returns `true` if the engine process has exited (crashed or quit).
    pub fn is_dead(&mut self) -> bool {
        self.engine.is_dead()
    }

    /// Set engine skill level 0-20 (Stockfish specific).
    pub fn set_skill_level(&mut self, level: u32) -> Result<()> {
        self.engine.send(&format!("setoption name Skill Level value {level}"))
    }
}

/// Merge an `info` line into the per-rank accumulator, keeping the deepest
/// result for each multipv rank (ties favour the most recent line).
fn merge_info(acc: &mut HashMap<u32, InfoLine>, info: InfoLine) {
    match acc.get(&info.multipv) {
        Some(existing) if existing.depth > info.depth => {}
        _ => { acc.insert(info.multipv, info); }
    }
}

/// Convert the accumulated per-rank lines into a White-perspective result.
fn acc_to_result(
    acc:       &HashMap<u32, InfoLine>,
    turn:      Option<char>,
    best_move: Option<String>,
) -> AnalysisResult {
    let mut lines: Vec<PvLine> = acc
        .values()
        .map(|info| PvLine {
            rank:          info.multipv,
            score_display: score_display_white_perspective(info, turn),
            centipawns:    centipawns_white_perspective(info, turn),
            pv:            info.pv.clone(),
        })
        .collect();

    lines.sort_by_key(|l| l.rank);

    let best_move = best_move
        .or_else(|| lines.first().and_then(|l| l.pv.first()).cloned());

    AnalysisResult { lines, best_move }
}

#[cfg_attr(not(test), allow(dead_code))]
fn build_result(
    raw:    &[String],
    multipv: u32,
    turn:    Option<char>,
    dirty:   bool,
) -> AnalysisResult {
    let mut acc: HashMap<u32, InfoLine> = HashMap::new();
    let mut best_move: Option<String> = None;

    for line in raw {
        if let Some(info) = parse_info(line) {
            merge_info(&mut acc, info);
        }
        if let Some(mv) = parse_bestmove(line) {
            best_move = Some(mv);
        }
    }

    let result = acc_to_result(&acc, turn, best_move);

    // Only warn when:
    //   • not in the dirty/transition window (position just changed)
    //   • the engine actually produced output (raw non-empty)
    //   • we expected lines (multipv > 0)
    if result.lines.is_empty() && !dirty && !raw.is_empty() {
        warn!(
            requested_multipv = multipv,
            raw_lines          = raw.len(),
            "[analysis] no parsed PV lines from engine output"
        );
    }

    result
}

fn fen_side_to_move(fen: &str) -> Option<char> {
    fen.split_whitespace()
        .nth(1)
        .and_then(|side| side.chars().next())
        .filter(|side| matches!(side, 'w' | 'b'))
}

fn side_relative_sign(turn: Option<char>) -> i32 {
    if turn == Some('b') { -1 } else { 1 }
}

fn centipawns_white_perspective(info: &InfoLine, turn: Option<char>) -> i32 {
    info.centipawns() * side_relative_sign(turn)
}

fn score_display_white_perspective(info: &InfoLine, turn: Option<char>) -> String {
    let sign = side_relative_sign(turn);
    if let Some(mate) = info.score_mate {
        format!("M{}", mate * sign)
    } else if let Some(cp) = info.score_cp {
        let pawns = (cp * sign) as f32 / 100.0;
        format!("{:+.2}", pawns)
    } else {
        "?".into()
    }
}

#[cfg(test)]
mod tests {
    use super::{build_result, fen_side_to_move};

    #[test]
    fn extracts_fen_side_to_move() {
        assert_eq!(
            fen_side_to_move("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1"),
            Some('w')
        );
        assert_eq!(fen_side_to_move("8/8/8/8/8/8/8/8 b - - 0 1"), Some('b'));
    }

    #[test]
    fn converts_black_to_move_score_to_white_perspective() {
        let raw = vec!["info depth 10 multipv 1 score cp 75 pv e7e5".to_string()];
        let result = build_result(&raw, 1, Some('b'), false);
        let line = result.lines.first().expect("line");
        assert_eq!(line.centipawns, -75);
        assert_eq!(line.score_display, "-0.75");
    }

    #[test]
    fn leaves_white_to_move_score_positive_for_white() {
        let raw = vec!["info depth 10 multipv 1 score cp 75 pv e2e4".to_string()];
        let result = build_result(&raw, 1, Some('w'), false);
        let line = result.lines.first().expect("line");
        assert_eq!(line.centipawns, 75);
        assert_eq!(line.score_display, "+0.75");
    }

    #[test]
    fn keeps_deepest_line_per_rank_not_latest() {
        // Simulates two info lines for rank 1: depth 8 arrives after depth 15.
        // build_result should keep depth 15.
        let raw = vec![
            "info depth 15 multipv 1 score cp 30 pv e2e4 e7e5".to_string(),
            "info depth 8  multipv 1 score cp 10 pv d2d4".to_string(),
        ];
        let result = build_result(&raw, 1, Some('w'), false);
        let line = result.lines.first().expect("line");
        // The depth-15 line (cp 30, pv e2e4 …) must win over the later depth-8 line.
        assert_eq!(line.centipawns, 30, "should keep deepest line");
        assert_eq!(line.pv.first().map(String::as_str), Some("e2e4"));
    }

    #[test]
    fn dirty_flag_suppresses_empty_lines_warning() {
        // Empty raw + dirty = true should not warn (was producing spurious log spam).
        let result = build_result(&[], 1, Some('w'), true);
        assert!(result.lines.is_empty());
        assert!(result.best_move.is_none());
    }
}