//! Post-game move-by-move review.
//!
//! Replays a stored game, evaluates every position with a dedicated UCI engine
//! instance, and classifies each move (Brilliant / Best / … / Blunder) plus a
//! per-side accuracy score.  This runs off the UI thread (spawned from
//! `app.rs`) because it performs N+1 fixed-depth searches.
//!
//! ## Centipawn maths (negamax)
//!
//! `e_i` is the engine's best evaluation at position *i*, from the perspective
//! of the side to move there.  After the player's move we reach position
//! *i+1*, where the *opponent* is to move; that position's value from the
//! original mover's perspective is `-e_{i+1}`.  Hence the centipawn loss of
//! the move actually played is `e_i - (-e_{i+1})`.
//!
//! Classification is bucketed on the **win-percentage drop** (Lichess model)
//! rather than raw centipawns, so giving back a sliver of a winning position is
//! not punished like a blunder.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use anyhow::{Result, anyhow};
use shakmaty::{CastlingMode, Chess, Color, Move, Position};
use shakmaty::san::San;
use shakmaty::uci::UciMove;
use tracing::info;

use crate::chess::openings;
use crate::engine::uci::{InfoLine, UciEngine, parse_bestmove, parse_info};
use crate::game_store::{GameReview, MoveAnnotation, MoveClass};

// ── Tuning constants ──────────────────────────────────────────────────────────

/// Win-percentage-loss thresholds (in percentage points) for non-best moves.
const WP_EXCELLENT: f64 = 2.0;
const WP_GOOD:      f64 = 5.0;
const WP_INACCURACY: f64 = 10.0;
const WP_MISTAKE:   f64 = 20.0;
// ≥ WP_MISTAKE → Blunder.

/// "Great" (only-move) gap: the best move must beat the 2nd-best by at least
/// this many centipawns, and the position must not already be crushing.
const GREAT_GAP_CP:    i32 = 100;
const GREAT_MAX_EVAL:  i32 = 1500;

/// Brilliant: after the forced recapture the mover must be at least this far
/// down in material (centipawns) yet keep at least `BRILLIANT_MIN_EVAL`.
const BRILLIANT_MIN_DOWN: i32 = 150;
const BRILLIANT_MIN_EVAL: i32 = 30;

/// Per-position search-completion timeout.
const EVAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound on parallel engine instances spawned for a review.  More than
/// this fragments the hash table without meaningful gains on typical hardware.
const MAX_REVIEW_ENGINES: usize = 8;

// ── Public entry point ────────────────────────────────────────────────────────

/// Review a stored game.  `moves` are the raw stored entries
/// (e.g. `"1. e4"`, `"1… e5"`).  `progress(done, total)` is called as positions
/// finish; set `cancel` to abort early.
pub fn review_game(
    moves: &[String],
    engine_path: &str,
    depth: u32,
    hash_mb: u32,
    cancel: Arc<AtomicBool>,
    mut progress: impl FnMut(usize, usize),
) -> Result<GameReview> {
    // ── Replay the game, capturing every position and played move ─────────────
    let clean: Vec<String> = moves.iter().map(|s| clean_san(s)).collect();

    // Guard against truncated history: a game we can review must start from the
    // initial position (move 1, White).  Otherwise replaying SAN from the start
    // position would silently produce a different game.
    if !starts_from_move_one(moves) {
        return Err(anyhow!(
            "incomplete history — this game was not recorded from move 1"
        ));
    }

    // Defensive replay: the captured move list can occasionally contain a gap
    // (a move the DOM poller missed) or a placeholder token, which would make a
    // later SAN illegal.  Rather than failing the whole review we stop at the
    // first move that doesn't apply and review the legal prefix — the UI simply
    // leaves the remaining moves un-annotated.
    let mut positions: Vec<Chess> = Vec::with_capacity(clean.len() + 1);
    let mut played:    Vec<Move>  = Vec::with_capacity(clean.len());
    let mut pos = Chess::default();
    positions.push(pos.clone());
    for san_str in clean.iter() {
        let Ok(san) = san_str.parse::<San>() else { break; };
        let Ok(m)   = san.to_move(&pos)       else { break; };
        played.push(m.clone());
        pos.play_unchecked(&m);
        positions.push(pos.clone());
    }

    let n = played.len();
    if n < 2 {
        return Err(anyhow!(
            "could not replay this game — the recorded moves are incomplete"
        ));
    }

    let n_pos = positions.len(); // n + 1

    // ── Resolve terminal positions; queue the rest as engine work ─────────────
    let mut evals: Vec<Option<PosEval>> = (0..n_pos).map(|_| None).collect();
    let mut work:  Vec<usize>           = Vec::with_capacity(n_pos);
    let mut fens:  Vec<String>          = Vec::with_capacity(n_pos);
    for (i, p) in positions.iter().enumerate() {
        if p.is_checkmate() {
            evals[i] = Some(PosEval { score_cp: -30_000, best_uci: None, second_cp: None });
            fens.push(String::new());
        } else if p.is_stalemate() || p.is_insufficient_material() {
            evals[i] = Some(PosEval { score_cp: 0, best_uci: None, second_cp: None });
            fens.push(String::new());
        } else {
            work.push(i);
            fens.push(
                shakmaty::fen::Fen::from_position(p.clone(), shakmaty::EnPassantMode::Legal)
                    .to_string(),
            );
        }
    }

    // ── Spawn a pool of engines and evaluate positions in parallel ────────────
    //
    // Game review is an embarrassingly-parallel batch of independent fixed-depth
    // searches.  For this workload many single-threaded engines beat fewer
    // multi-threaded ones: Stockfish's SMP scaling is sub-linear (2 threads is
    // ~1.7x a single thread, not 2x), so N single-thread searches finish sooner
    // than N/2 dual-thread ones.  We therefore run ONE search thread per engine
    // and spawn as many engines as the machine has logical cores (capped).
    let cores = std::thread::available_parallelism().map(|c| c.get()).unwrap_or(2);
    let per_engine_threads = 1u32;
    let k = cores
        .clamp(1, MAX_REVIEW_ENGINES)
        .min(work.len().max(1));
    let per_hash = (hash_mb / k as u32).max(16);

    // Pre-spawn engines so a bad path fails fast with a clean error.
    let mut engines: Vec<UciEngine> = Vec::with_capacity(k);
    for _ in 0..k {
        match UciEngine::spawn(engine_path) {
            Ok(mut e) => {
                if configure_engine(&mut e, per_hash, per_engine_threads).is_ok() {
                    engines.push(e);
                }
            }
            Err(e) => {
                if engines.is_empty() {
                    return Err(anyhow!("failed to start review engine: {e}"));
                }
                break; // some engines started — proceed with those
            }
        }
    }
    if engines.is_empty() {
        return Err(anyhow!("failed to start review engine"));
    }

    let work   = Arc::new(work);
    let fens   = Arc::new(fens);
    let cursor = Arc::new(AtomicUsize::new(0));
    let (etx, erx) = mpsc::channel::<(usize, PosEval)>();

    let mut handles = Vec::with_capacity(engines.len());
    for mut eng in engines {
        let work   = work.clone();
        let fens   = fens.clone();
        let cursor = cursor.clone();
        let cancel = cancel.clone();
        let etx    = etx.clone();
        handles.push(std::thread::spawn(move || {
            loop {
                if cancel.load(Ordering::Relaxed) { break; }
                let w = cursor.fetch_add(1, Ordering::Relaxed);
                if w >= work.len() { break; }
                let idx  = work[w];
                let (s1, bm, s2) = eval_position(&mut eng, &fens[idx], depth);
                let eval = PosEval { score_cp: s1, best_uci: bm, second_cp: s2 };
                if etx.send((idx, eval)).is_err() { break; }
            }
            // `eng` dropped here → its Drop sends `quit` and reaps the process.
        }));
    }
    drop(etx); // close the channel once the worker clones are gone

    // Collect results as they arrive, updating progress.  Terminal positions are
    // already done before the first engine result.
    let mut done = n_pos - work.len();
    progress(done, n_pos);
    for (idx, eval) in erx {
        evals[idx] = Some(eval);
        done += 1;
        progress(done, n_pos);
    }
    for h in handles { let _ = h.join(); }

    if cancel.load(Ordering::Relaxed) {
        return Err(anyhow!("cancelled"));
    }

    // Flatten the parallel results into the per-position arrays.
    let e_stm:  Vec<i32>             = evals.iter()
        .map(|e| e.as_ref().map(|x| x.score_cp).unwrap_or(0)).collect();
    let best_u: Vec<Option<String>> = evals.iter()
        .map(|e| e.as_ref().and_then(|x| x.best_uci.clone())).collect();
    let second: Vec<Option<i32>>    = evals.iter()
        .map(|e| e.as_ref().and_then(|x| x.second_cp)).collect();

    // ── Classify every played move ────────────────────────────────────────────
    let mut annotations: Vec<MoveAnnotation> = Vec::with_capacity(n);
    let mut white_accs: Vec<f64> = Vec::new();
    let mut black_accs: Vec<f64> = Vec::new();

    for i in 0..n {
        let mover = positions[i].turn();
        let best_eval_mover   = e_stm[i];
        let played_eval_mover = -e_stm[i + 1];
        let cp_loss = (best_eval_mover - played_eval_mover).max(0);

        let wp_before = win_percent(best_eval_mover);
        let wp_after  = win_percent(played_eval_mover);
        let wp_loss   = (wp_before - wp_after).max(0.0);

        // Played move in UCI (Standard castling) for the "is best?" comparison.
        let played_uci = played[i].to_uci(CastlingMode::Standard).to_string();
        let is_best = best_u[i].as_deref() == Some(played_uci.as_str());

        // Book move? (resulting position is a known opening line)
        let epd_after = openings::fen_to_epd(
            &shakmaty::fen::Fen::from_position(positions[i + 1].clone(),
                shakmaty::EnPassantMode::Legal).to_string()
        ).to_string();
        let in_book = openings::lookup(&epd_after).is_some();

        let class = classify(
            in_book,
            is_best,
            cp_loss,
            wp_loss,
            best_eval_mover,
            played_eval_mover,
            second[i],
            &positions,
            i,
            n,
            best_u[i + 1].as_deref(),
            mover,
        );

        // Engine's preferred move at this position, as SAN (for "best was …").
        let best_san = best_u[i].as_deref()
            .and_then(|u| uci_to_san(&positions[i], u))
            .unwrap_or_default();

        // Per-move accuracy (Lichess formula), bucketed by the mover.
        let acc = (103.1668 * (-0.04354 * wp_loss).exp() - 3.1669).clamp(0.0, 100.0);
        if mover == Color::White { white_accs.push(acc); } else { black_accs.push(acc); }

        annotations.push(MoveAnnotation {
            ply:       i as u32,
            san:       clean[i].clone(),
            class,
            cp_before: white_persp(best_eval_mover, mover),
            cp_after:  white_persp(played_eval_mover, mover),
            best_san,
        });
    }

    let accuracy_white = mean(&white_accs) as f32;
    let accuracy_black = mean(&black_accs) as f32;

    info!(
        plies = n, depth,
        acc_w = accuracy_white, acc_b = accuracy_black,
        "[review] game review complete"
    );

    Ok(GameReview { depth, annotations, accuracy_white, accuracy_black })
}

// ── Classification ────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn classify(
    in_book: bool,
    is_best: bool,
    _cp_loss: i32,
    wp_loss: f64,
    best_eval_mover: i32,
    played_eval_mover: i32,
    second_eval: Option<i32>,
    positions: &[Chess],
    i: usize,
    n: usize,
    opp_best_reply: Option<&str>,
    mover: Color,
) -> MoveClass {
    if in_book {
        return MoveClass::Book;
    }

    if is_best {
        // Brilliant: a sound sacrifice that keeps the position at least equal.
        if is_sacrifice(positions, i, n, opp_best_reply, mover, played_eval_mover) {
            return MoveClass::Brilliant;
        }
        // Great: the only move that holds — clearly better than the alternative.
        if let Some(s2) = second_eval {
            if best_eval_mover - s2 >= GREAT_GAP_CP && best_eval_mover < GREAT_MAX_EVAL {
                return MoveClass::Great;
            }
        }
        return MoveClass::Best;
    }

    if wp_loss < WP_EXCELLENT      { MoveClass::Excellent }
    else if wp_loss < WP_GOOD      { MoveClass::Good }
    else if wp_loss < WP_INACCURACY { MoveClass::Inaccuracy }
    else if wp_loss < WP_MISTAKE   { MoveClass::Mistake }
    else                           { MoveClass::Blunder }
}

/// Heuristic sacrifice detector: after the opponent's best reply (a forced
/// recapture), the mover is materially down yet the engine still rates the
/// position as at least equal for the mover.
fn is_sacrifice(
    positions: &[Chess],
    i: usize,
    n: usize,
    opp_best_reply: Option<&str>,
    mover: Color,
    played_eval_mover: i32,
) -> bool {
    if i + 1 >= n { return false; }
    if played_eval_mover < BRILLIANT_MIN_EVAL { return false; }

    let Some(reply_uci) = opp_best_reply else { return false; };
    let after_move = &positions[i + 1];
    let Ok(uci) = reply_uci.parse::<UciMove>() else { return false; };
    let Ok(reply) = uci.to_move(after_move) else { return false; };

    let mut reply_pos = after_move.clone();
    reply_pos.play_unchecked(&reply);

    // Material balance from the mover's perspective after the recapture.
    let bal_after = material_points(&reply_pos, mover)
        - material_points(&reply_pos, mover.other());

    bal_after <= -BRILLIANT_MIN_DOWN
}

// ── Engine evaluation ─────────────────────────────────────────────────────────

/// Engine evaluation of a single position (side-to-move perspective).
struct PosEval {
    score_cp:  i32,
    best_uci:  Option<String>,
    second_cp: Option<i32>,
}

/// Apply the standard review options to a freshly-spawned engine.
fn configure_engine(eng: &mut UciEngine, hash_mb: u32, threads: u32) -> Result<()> {
    eng.send("setoption name MultiPV value 2")?;
    eng.send(&format!("setoption name Hash value {hash_mb}"))?;
    eng.send(&format!("setoption name Threads value {threads}"))?;
    eng.send("setoption name Skill Level value 20")?;
    eng.send("ucinewgame")?;
    eng.send("isready")?;
    eng.wait_for("readyok", Duration::from_secs(5))?;
    Ok(())
}

/// Evaluate a single FEN to `depth`, returning
/// `(best_score_cp, best_move_uci, second_best_score_cp)` — all centipawns from
/// the side-to-move perspective.
fn eval_position(
    eng:   &mut UciEngine,
    fen:   &str,
    depth: u32,
) -> (i32, Option<String>, Option<i32>) {
    let _ = eng.drain_lines(); // discard any stale output
    if eng.send(&format!("position fen {fen}")).is_err() { return (0, None, None); }
    if eng.send(&format!("go depth {depth}")).is_err()   { return (0, None, None); }

    if eng.wait_for("bestmove", EVAL_TIMEOUT).is_err() {
        // Search ran long — stop it and use whatever we have.
        let _ = eng.send("stop");
        let _ = eng.wait_for("bestmove", Duration::from_secs(2));
    }

    let lines = eng.drain_lines();
    let mut best_by_pv: HashMap<u32, InfoLine> = HashMap::new();
    let mut best_move: Option<String> = None;
    for l in &lines {
        if let Some(info) = parse_info(l) {
            match best_by_pv.get(&info.multipv) {
                Some(existing) if existing.depth >= info.depth => {}
                _ => { best_by_pv.insert(info.multipv, info); }
            }
        }
        if let Some(mv) = parse_bestmove(l) { best_move = Some(mv); }
    }

    let pv1   = best_by_pv.get(&1);
    let score = pv1.map(|i| i.centipawns()).unwrap_or(0);
    let bm    = best_move.or_else(|| pv1.and_then(|i| i.pv.first().cloned()));
    let s2    = best_by_pv.get(&2).map(|i| i.centipawns());
    (score, bm, s2)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Strip the move-number prefix from a stored entry, returning clean SAN.
/// `"6. Nf3"` → `"Nf3"`, `"6… cxd4"` → `"cxd4"`.
fn clean_san(entry: &str) -> String {
    let s = if let Some(p) = entry.find(". ") {
        &entry[p + 2..]
    } else if let Some(p) = entry.find("\u{2026} ") {
        // U+2026 ELLIPSIS = 3 UTF-8 bytes + 1 space = 4 bytes
        &entry[p + 4..]
    } else {
        entry
    };
    s.trim().to_string()
}

/// True when the first stored entry is White's move 1 — i.e. the full game was
/// recorded from the start.
fn starts_from_move_one(moves: &[String]) -> bool {
    let Some(first) = moves.first() else { return false; };
    let num: String = first.chars().take_while(|c| c.is_ascii_digit()).collect();
    let is_white = first.contains(". ");
    num.parse::<u32>().ok() == Some(1) && is_white
}

/// Convert a UCI move to SAN in the given position (best-effort).
fn uci_to_san(pos: &Chess, uci: &str) -> Option<String> {
    let uci = uci.parse::<UciMove>().ok()?;
    let m   = uci.to_move(pos).ok()?;
    Some(San::from_move(pos, &m).to_string())
}

/// Material value (centipawns) of one side's non-king pieces.
fn material_points(pos: &Chess, color: Color) -> i32 {
    let m = pos.board().material_side(color);
    m.pawn as i32 * 100
        + m.knight as i32 * 300
        + m.bishop as i32 * 300
        + m.rook as i32 * 500
        + m.queen as i32 * 900
}

/// Convert a side-to-move centipawn score to White's perspective.
fn white_persp(cp_mover: i32, mover: Color) -> i32 {
    if mover == Color::White { cp_mover } else { -cp_mover }
}

/// Lichess win-percentage model: maps a centipawn score (side-to-move
/// perspective) to a 0–100 winning percentage for that side.
fn win_percent(cp: i32) -> f64 {
    let cp = cp.clamp(-30_000, 30_000) as f64;
    50.0 + 50.0 * (2.0 / (1.0 + (-0.003_682_08 * cp).exp()) - 1.0)
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() { return 0.0; }
    v.iter().sum::<f64>() / v.len() as f64
}
