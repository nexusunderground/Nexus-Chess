//! Puzzle-FEN stability gate.
//!
//! chess.com / lichess puzzle boards hydrate over several DOM mutations, so a
//! freshly-observed FEN can be transient (mid-animation, partial piece map).
//! Feeding those straight to the engine causes flicker and wasted searches.
//!
//! [`PuzzleGate`] debounces the incoming FENs: a candidate must be observed
//! unchanged for a number of polls before it is considered committable.  A
//! *new* puzzle (detected by a large piece-count delta vs the current root)
//! must stay stable longer than a continuation move within the same puzzle.
//!
//! The gate is pure and I/O-free, which makes the otherwise-tangled stability
//! heuristic unit-testable without Chrome, a WebSocket, or an engine.

use crate::config::tuning::{PUZZLE_PIECE_DELTA, PUZZLE_STABLE_CONT, PUZZLE_STABLE_NEW};

/// Outcome of feeding a puzzle FEN to the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// FEN not yet stable enough (or already committed) — do nothing.
    Hold,
    /// FEN is stable and should be sent to the engine.
    Ready {
        /// `true` when this represents a brand-new puzzle (caller should reset
        /// puzzle state / send `PuzzleReset`), `false` for a continuation move.
        is_new_puzzle: bool,
    },
}

/// Debounces puzzle FENs until they are stable enough to analyse.
#[derive(Debug, Default)]
pub struct PuzzleGate {
    candidate_fen: Option<String>,
    stable_count:  u32,
    root_fen:      Option<String>,
    committed_fen: Option<String>,
}

impl PuzzleGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear all state — call on reconnect or when leaving puzzle mode.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Feed the latest observed puzzle FEN.
    ///
    /// Returns [`GateDecision::Ready`] once the FEN has been stable for the
    /// required number of polls and differs from the last committed FEN.  The
    /// caller is responsible for applying its own send-rate throttle and then
    /// calling [`commit`](Self::commit) once it has actually sent the FEN.
    pub fn observe(&mut self, fen: &str) -> GateDecision {
        if self.candidate_fen.as_deref() == Some(fen) {
            self.stable_count += 1;
        } else {
            self.candidate_fen = Some(fen.to_string());
            self.stable_count  = 1;
        }

        let is_new        = self.is_new_puzzle(fen);
        let required      = if is_new { PUZZLE_STABLE_NEW } else { PUZZLE_STABLE_CONT };
        let already_sent  = self.committed_fen.as_deref() == Some(fen);

        if self.stable_count >= required && !already_sent {
            GateDecision::Ready { is_new_puzzle: is_new || self.root_fen.is_none() }
        } else {
            GateDecision::Hold
        }
    }

    /// Record that `fen` has been committed (sent to the engine).  When
    /// `is_new_puzzle` is set this FEN becomes the new puzzle root.
    pub fn commit(&mut self, fen: &str, is_new_puzzle: bool) {
        if is_new_puzzle {
            self.root_fen = Some(fen.to_string());
        }
        self.committed_fen = Some(fen.to_string());
    }

    /// A FEN is a new puzzle when its piece count differs from the current
    /// root by more than [`PUZZLE_PIECE_DELTA`] (or when there is no root yet).
    fn is_new_puzzle(&self, fen: &str) -> bool {
        match self.root_fen.as_deref() {
            Some(root) => {
                let old = piece_count(root);
                let new = piece_count(fen);
                (old - new).abs() > PUZZLE_PIECE_DELTA
            }
            None => true,
        }
    }
}

/// Count the pieces in a FEN's placement field (ignores side-to-move letters).
fn piece_count(fen: &str) -> i32 {
    fen.split_whitespace()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_alphabetic() && *c != 'w' && *c != 'b')
        .count() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUZZLE_A: &str = "r1bqkbnr/pppp1ppp/2n5/4p3/4P3/5N2/PPPP1PPP/RNBQKB1R w KQkq - 0 1";
    // One fewer piece than PUZZLE_A (continuation within same puzzle).
    const PUZZLE_A2: &str = "r1bqkbnr/pppp1ppp/2n5/4p3/4P3/5N2/PPPP1PPP/RNBQKB1R b KQkq - 0 1";
    // Far fewer pieces — a brand new puzzle.
    const PUZZLE_B: &str = "8/8/4k3/8/8/4K3/8/8 w - - 0 1";

    #[test]
    fn holds_until_stable_then_ready_for_new_puzzle() {
        let mut gate = PuzzleGate::new();
        // First puzzle (no root yet) needs PUZZLE_STABLE_NEW observations.
        assert_eq!(gate.observe(PUZZLE_A), GateDecision::Hold);
        assert_eq!(gate.observe(PUZZLE_A), GateDecision::Hold);
        assert_eq!(
            gate.observe(PUZZLE_A),
            GateDecision::Ready { is_new_puzzle: true }
        );
    }

    #[test]
    fn does_not_recommit_same_fen() {
        let mut gate = PuzzleGate::new();
        for _ in 0..PUZZLE_STABLE_NEW {
            gate.observe(PUZZLE_A);
        }
        gate.commit(PUZZLE_A, true);
        // Further observations of the committed FEN must Hold.
        assert_eq!(gate.observe(PUZZLE_A), GateDecision::Hold);
    }

    #[test]
    fn continuation_needs_fewer_stable_polls() {
        let mut gate = PuzzleGate::new();
        for _ in 0..PUZZLE_STABLE_NEW {
            gate.observe(PUZZLE_A);
        }
        gate.commit(PUZZLE_A, true);

        // A near-identical FEN (same piece count) is a continuation: it should
        // become Ready after PUZZLE_STABLE_CONT observations.
        let mut decision = GateDecision::Hold;
        for _ in 0..PUZZLE_STABLE_CONT {
            decision = gate.observe(PUZZLE_A2);
        }
        assert_eq!(decision, GateDecision::Ready { is_new_puzzle: false });
    }

    #[test]
    fn large_piece_delta_marks_new_puzzle() {
        let mut gate = PuzzleGate::new();
        for _ in 0..PUZZLE_STABLE_NEW {
            gate.observe(PUZZLE_A);
        }
        gate.commit(PUZZLE_A, true);

        let mut decision = GateDecision::Hold;
        for _ in 0..PUZZLE_STABLE_NEW {
            decision = gate.observe(PUZZLE_B);
        }
        assert_eq!(decision, GateDecision::Ready { is_new_puzzle: true });
    }

    #[test]
    fn reset_clears_state() {
        let mut gate = PuzzleGate::new();
        for _ in 0..PUZZLE_STABLE_NEW {
            gate.observe(PUZZLE_A);
        }
        gate.commit(PUZZLE_A, true);
        gate.reset();
        // After reset the same FEN is treated as a fresh new puzzle again.
        let mut decision = GateDecision::Hold;
        for _ in 0..PUZZLE_STABLE_NEW {
            decision = gate.observe(PUZZLE_A);
        }
        assert_eq!(decision, GateDecision::Ready { is_new_puzzle: true });
    }
}
