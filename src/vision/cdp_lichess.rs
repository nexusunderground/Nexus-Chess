//! Lichess CDP connector.
//!
//! Same push-model architecture as `cdp_chesscom` but tailored for Lichess:
//!   • Targets `lichess.org` URLs instead of `chess.com`.
//!   • Reads **UCI moves** from `<m2 u="e2e4">` elements in `.rmoves`.
//!   • Reads board position for puzzles from CSS-positioned `<piece>` elements.
//!   • `CdpMoveSnapshot.moves_are_uci` is always `true`.

use anyhow::{Context, Result, anyhow};
use egui::{Pos2, Rect, Vec2};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use tungstenite::{Message, WebSocket};
use tungstenite::stream::MaybeTlsStream;
use std::net::TcpStream;
use crate::perf_scope;

use super::cdp_chesscom::{CdpEvent, CdpMoveSnapshot, CdpTarget, fetch_targets, random_token};
const OBSERVER_STALE_SECS:    u64 = 8;
const REINSTALL_BACKOFF_SECS: u64 = 3;

// ── Target selection ──────────────────────────────────────────────────────────

pub fn pick_lichess_target(targets: &[CdpTarget]) -> Option<&CdpTarget> {
    targets
        .iter()
        .filter(|t| {
            let lower = t.url.to_lowercase();
            (lower.contains("lichess.org") || lower.contains("lichess1.org"))
                && !lower.starts_with("devtools://")
                && !lower.contains("lichess1.org/assets")
        })
        .max_by_key(|t| {
            let lower = t.url.to_lowercase();
            let mut score = 0u32;
            if lower.contains("/training")       { score += 50; }
            if lower.contains("/puzzle")         { score += 45; }
            if lower.contains("lichess.org/@")   { score += 5;  }
            // round game: lichess.org/GAMEID or lichess.org/GAMEID/color
            let path = lower.trim_start_matches("https://lichess.org")
                            .trim_start_matches("http://lichess.org");
            let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
            // Accept bare 8-char game IDs and 12-char player-token URLs (GAMEID + 4-char token).
            if segs.len() >= 1 {
                let s = segs[0];
                if (s.len() == 8 || s.len() == 12)
                    && s.chars().all(|c| c.is_ascii_alphanumeric())
                {
                    score += 40;
                }
            }
            score += 10;
            score
        })
}

// ── Persistent connection ─────────────────────────────────────────────────────

pub struct LichessConnection {
    ws:              WebSocket<MaybeTlsStream<TcpStream>>,
    target_id:       String,
    target_url:      String,
    msg_id:          u32,
    last:            Option<CdpMoveSnapshot>,
    last_push:       Instant,
    install_version: u32,
    binding_name:    String,
    flag_name:       String,
    world_name:      String,
}

impl LichessConnection {
    pub fn connect(endpoint: &str) -> Option<Self> {
        let targets = fetch_targets(endpoint).ok()?;
        let target  = pick_lichess_target(&targets)?;
        let ws_url  = target.web_socket_debugger_url.as_deref()?;

        let (mut ws, _) = tungstenite::connect(ws_url).ok()?;
        if let MaybeTlsStream::Plain(s) = ws.get_mut() {
            let _ = s.set_read_timeout(Some(Duration::from_millis(120)));
        }

        let mut conn = Self {
            ws,
            target_id:       target.id.clone().unwrap_or_default(),
            target_url:      target.url.clone(),
            msg_id:          1,
            last:            None,
            last_push:       Instant::now(),
            install_version: 0,
            binding_name:    random_token("__"),
            flag_name:       random_token("__"),
            world_name:      random_token("w"),
        };

        match conn.install_observer() {
            Ok(())  => {}
            Err(e)  => warn!("[lichess-cdp] observer install failed ({e}), falling back to poll"),
        }

        info!("[lichess-cdp] connected to: {}", target.url);
        Some(conn)
    }

    fn send_cmd(&mut self, method: &str, params: Value) -> Result<u32> {
        let id = self.msg_id;
        self.msg_id += 1;
        let req = json!({ "id": id, "method": method, "params": params });
        self.ws.send(Message::Text(req.to_string()))
            .with_context(|| format!("send {method}"))?;
        Ok(id)
    }

    fn wait_for_id(&mut self, id: u32) -> Result<Value> {
        if let MaybeTlsStream::Plain(s) = self.ws.get_mut() {
            let _ = s.set_read_timeout(Some(Duration::from_millis(2000)));
        }
        let result = loop {
            let msg = self.ws.read().context("read during setup")?;
            let Message::Text(txt) = msg else { continue };
            let v: Value = serde_json::from_str(&txt).context("parse setup msg")?;
            if v.get("id").and_then(Value::as_u64) == Some(id as u64) { break v; }
        };
        if let MaybeTlsStream::Plain(s) = self.ws.get_mut() {
            let _ = s.set_read_timeout(Some(Duration::from_millis(120)));
        }
        Ok(result)
    }

    fn main_frame_id(&mut self) -> Result<String> {
        let id = self.send_cmd("Page.getFrameTree", json!({}))?;
        let resp = self.wait_for_id(id)?;
        resp.get("result")
            .and_then(|r| r.get("frameTree"))
            .and_then(|t| t.get("frame"))
            .and_then(|f| f.get("id"))
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| anyhow!("no main frame id"))
    }

    fn create_isolated_world(&mut self, frame_id: &str) -> Result<i64> {
        let id = self.send_cmd("Page.createIsolatedWorld", json!({
            "frameId":             frame_id,
            "worldName":           self.world_name,
            "grantUniveralAccess": true,
        }))?;
        let resp = self.wait_for_id(id)?;
        resp.get("result")
            .and_then(|r| r.get("executionContextId"))
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("no executionContextId"))
    }

    fn install_observer(&mut self) -> Result<()> {
        self.install_version += 1;
        let version = self.install_version;

        let id = self.send_cmd("Page.enable", json!({}))?;
        self.wait_for_id(id)?;
        let id = self.send_cmd("Runtime.enable", json!({}))?;
        self.wait_for_id(id)?;

        let ctx_id: Option<i64> = self
            .main_frame_id()
            .and_then(|frame| self.create_isolated_world(&frame))
            .ok();

        let mut bind_params = json!({ "name": self.binding_name });
        if let Some(cid) = ctx_id {
            bind_params["executionContextId"] = json!(cid);
        }
        let id = self.send_cmd("Runtime.addBinding", bind_params)?;
        self.wait_for_id(id)?;

        let script = lichess_observer_script(&self.binding_name, &self.flag_name);
        let mut eval_params = json!({
            "expression":    script,
            "returnByValue": false,
            "awaitPromise":  false,
        });
        if let Some(cid) = ctx_id {
            eval_params["contextId"] = json!(cid);
        }
        let id = self.send_cmd("Runtime.evaluate", eval_params)?;
        let resp = self.wait_for_id(id)?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("observer inject error: {err}"));
        }

        self.last_push = Instant::now();
        info!("[lichess-cdp] push observer installed (v{version}, isolated={})", ctx_id.is_some());
        Ok(())
    }

    // ── Poll ──────────────────────────────────────────────────────────────────

    // pub fn poll(&mut self) -> Result<Option<CdpMoveSnapshot>> {
    //     perf_scope!("lichess_cdp_poll");
    //     loop {
    //         let msg = match self.ws.read() {
    //             Ok(m) => m,
    //             Err(tungstenite::Error::Io(ref e))
    //                 if matches!(e.kind(),
    //                     std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
    //             {
    //                 if self.last_push.elapsed() > Duration::from_secs(OBSERVER_STALE_SECS) {
    //                     tracing::info!(
    //                         "[lichess-cdp] observer silent >{}s — reinstalling (v{})",
    //                         OBSERVER_STALE_SECS, self.install_version + 1
    //                     );
    //                     self.last = None;
    //                     match self.install_observer() {
    //                         Ok(()) => {}
    //                         Err(e) => {
    //                             warn!("[lichess-cdp] reinstall failed: {e}");
    //                             self.last_push = Instant::now()
    //                                 - Duration::from_secs(
    //                                     OBSERVER_STALE_SECS.saturating_sub(REINSTALL_BACKOFF_SECS)
    //                                 );
    //                         }
    //                     }
    //                     return self.oneshot_eval();
    //                 }
    //                 if self.last.is_some() { return Ok(self.last.clone()); }
    //                 return self.oneshot_eval();
    //             }
    //             Err(e) => return Err(anyhow!("WebSocket read error: {e}")),
    //         };

    //         let Message::Text(txt) = msg else { continue };
    //         let v: Value = match serde_json::from_str(&txt) {
    //             Ok(v) => v,
    //             Err(_) => continue,
    //         };

    //         if v.get("method").and_then(Value::as_str) != Some("Runtime.bindingCalled") { continue; }
    //         let params = match v.get("params") { Some(p) => p, None => continue };
    //         if params.get("name").and_then(Value::as_str) != Some(self.binding_name.as_str()) { continue; }
    //         let payload_str = match params.get("payload").and_then(Value::as_str) {
    //             Some(s) => s,
    //             None => continue,
    //         };

    //         match self.parse_payload(payload_str) {
    //             Ok(snapshot) => {
    //                 debug!(url = %self.target_url, move_count = snapshot.moves.len(),
    //                        is_puzzle = snapshot.is_puzzle, "[lichess-cdp] binding update");
    //                 self.last_push = Instant::now();
    //                 self.last = Some(snapshot.clone());
    //                 return Ok(Some(snapshot));
    //             }
    //             Err(e) => { warn!("[lichess-cdp] payload parse error: {e}"); continue; }
    //         }
    //     }
    // }
    pub fn next_event(&mut self) -> Result<CdpEvent> {
    perf_scope!("lichess_cdp_next_event");
    loop {
        let msg = match self.ws.read() {
            Ok(m) => m,
            Err(tungstenite::Error::Io(ref e))
                if matches!(e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
            {
                // Stale-observer check: if no push has arrived in
                // OBSERVER_STALE_SECS, reinstall the observer.
                if self.last_push.elapsed() > Duration::from_secs(OBSERVER_STALE_SECS) {
                    tracing::info!(
                        "[lichess-cdp] observer silent >{}s — reinstalling (v{})",
                        OBSERVER_STALE_SECS, self.install_version + 1
                    );
                    self.last = None;
                    match self.install_observer() {
                        Ok(()) => {}
                        Err(e) => {
                            warn!("[lichess-cdp] reinstall failed: {e}");
                            self.last_push = Instant::now()
                                - Duration::from_secs(
                                    OBSERVER_STALE_SECS
                                        .saturating_sub(REINSTALL_BACKOFF_SECS)
                                );
                        }
                    }
                    return self.oneshot_eval()
                        .map(|s| CdpEvent::BoardState(
                            s.unwrap_or_else(|| CdpMoveSnapshot::empty())
                        ));
                }
                return match self.last.clone() {
                    Some(snap) => Ok(CdpEvent::BoardState(snap)),
                    None       => self.oneshot_eval()
                        .map(|s| CdpEvent::BoardState(
                            s.unwrap_or_else(|| CdpMoveSnapshot::empty())
                        )),
                };
            }
            Err(e) => return Err(anyhow!("WebSocket read error: {e}")),
        };

        let Message::Text(txt) = msg else { continue };
        let v: Value = match serde_json::from_str(&txt) {
            Ok(v) => v,
            Err(_) => continue,
        };

        match v.get("method").and_then(Value::as_str) {
            Some("Page.frameNavigated") | Some("Page.loadEventFired") => {
                tracing::info!("[lichess-cdp] page navigated — reinstalling observer");
                self.last = None;
                match self.install_observer() {
                    Ok(()) => {
                        self.last = self.oneshot_eval().ok().flatten();
                    }
                    Err(e) => {
                        warn!("[lichess-cdp] post-nav reinstall failed: {e}");
                    }
                }
                return Ok(CdpEvent::PageNavigated);
            }

            Some("Runtime.bindingCalled") => {
                let params = match v.get("params") { Some(p) => p, None => continue };
                if params.get("name").and_then(Value::as_str)
                    != Some(self.binding_name.as_str()) { continue; }
                let payload_str = match params.get("payload").and_then(Value::as_str) {
                    Some(s) => s,
                    None => continue,
                };
                match self.parse_payload(payload_str) {
                    Ok(snapshot) => {
                        debug!(
                            url        = %self.target_url,
                            move_count = snapshot.moves.len(),
                            is_puzzle  = snapshot.is_puzzle,
                            "[lichess-cdp] binding push received"
                        );
                        self.last_push = Instant::now();
                        self.last = Some(snapshot.clone());
                        return Ok(CdpEvent::BoardState(snapshot));
                    }
                    Err(e) => {
                        warn!("[lichess-cdp] payload parse error: {e}");
                        continue;
                    }
                }
            }

            _ => continue,
        }
    }
}

    fn parse_payload(&self, json_str: &str) -> Result<CdpMoveSnapshot> {
        let payload: LichessPayload = serde_json::from_str(json_str)
            .context("decode lichess binding payload")?;
        Ok(build_snapshot(payload, self.target_url.clone()))
    }

    fn oneshot_eval(&mut self) -> Result<Option<CdpMoveSnapshot>> {
        let id = self.msg_id;
        self.msg_id += 1;
        let req = json!({
            "id": id,
            "method": "Runtime.evaluate",
            "params": {
                "expression":    lichess_oneshot_expression(),
                "returnByValue": true,
                "awaitPromise":  true,
            }
        });
        if let MaybeTlsStream::Plain(s) = self.ws.get_mut() {
            let _ = s.set_read_timeout(Some(Duration::from_millis(900)));
        }
        self.ws.send(Message::Text(req.to_string())).context("oneshot send")?;
        let result = loop {
            let msg = match self.ws.read() {
                Ok(m) => m,
                Err(tungstenite::Error::Io(ref e))
                    if matches!(e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                { break None; }
                Err(e) => return Err(anyhow!("oneshot read: {e}")),
            };
            let Message::Text(txt) = msg else { continue };
            let v: Value = serde_json::from_str(&txt).unwrap_or(Value::Null);
            if v.get("id").and_then(Value::as_u64) != Some(id as u64) { continue; }
            if let Some(value) = v.get("result").and_then(|r| r.get("result")).and_then(|r| r.get("value")) {
                if let Ok(payload) = serde_json::from_value::<LichessPayload>(value.clone()) {
                    let snap = build_snapshot(payload, self.target_url.clone());
                    self.last      = Some(snap.clone());
                    self.last_push = Instant::now();
                    break Some(snap);
                }
            }
            break None;
        };
        if let MaybeTlsStream::Plain(s) = self.ws.get_mut() {
            let _ = s.set_read_timeout(Some(Duration::from_millis(120)));
        }
        Ok(result)
    }

    pub fn target_changed(&self, endpoint: &str) -> bool {
        let Ok(targets) = fetch_targets(endpoint) else { return false };
        let Some(best)  = pick_lichess_target(&targets) else { return false };
        best.id.as_deref() != Some(&self.target_id)
    }
}

// ── Payload structs ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct JsBoardRect { x: f32, y: f32, w: f32, h: f32 }

#[derive(Debug, Deserialize)]
struct LichessPayload {
    /// Move strings — UCI ("e2e4") when `moves_are_uci`, else SAN ("e4").
    #[serde(default)]
    moves: Vec<String>,
    /// True when `moves` holds UCI strings (analysis board), false for SAN
    /// (live round move list).
    #[serde(default)]
    moves_are_uci: bool,
    /// "white" or "black" (which side is at the bottom of the board).
    bottom_color: Option<String>,
    top_player:    Option<String>,
    bottom_player: Option<String>,
    top_clock:     Option<String>,
    bottom_clock:  Option<String>,
    board_rect:    Option<JsBoardRect>,
    window_screen_x:       Option<f32>,
    window_screen_y:       Option<f32>,
    window_chrome_height:  Option<f32>,
    /// Square→piece map for puzzles (piece positioned by CSS).
    #[serde(default)]
    piece_map: HashMap<String, String>,
    puzzle_turn: Option<String>,
    is_puzzle: Option<bool>,
    /// Live page URL (`location.href`) at the moment of the snapshot. Used in
    /// preference to the connect-time target URL so SPA navigation between
    /// puzzle modes / games is classified correctly.
    #[serde(default)]
    page_url: Option<String>,
    /// Space-separated SAN move list from puzzle `page-init-data` (`game.pgn`).
    /// This is the authoritative setup line up to the puzzle position.
    #[serde(default)]
    puzzle_pgn: Option<String>,
    /// `puzzle.initialPly` from `page-init-data` (ply before the user's move).
    #[serde(default)]
    puzzle_initial_ply: Option<u32>,
    /// Non-standard starting FEN for Chess960 / variants.
    #[serde(default)]
    initial_fen: Option<String>,
    /// Raw result string from the DOM (e.g. "White wins", "1-0").
    /// `None` while the game is still in progress.
    #[serde(default)]
    game_result: Option<String>,
}

// ── Snapshot builder ──────────────────────────────────────────────────────────

fn build_snapshot(payload: LichessPayload, page_url: String) -> CdpMoveSnapshot {
    let is_puzzle = payload.is_puzzle.unwrap_or(false);

    // Prefer the live page URL reported by the script (handles SPA navigation
    // between puzzle modes / games) over the connect-time target URL.
    let page_url = payload.page_url.clone()
        .filter(|u| !u.is_empty())
        .unwrap_or(page_url);

    // Puzzle FEN: prefer the authoritative PGN line from page-init-data
    // (exact turn / castling / en-passant), and fall back to reconstructing
    // from the CSS-positioned <piece> elements when the PGN isn't available.
    let puzzle_fen = if is_puzzle {
        let from_pgn = payload.puzzle_pgn.as_deref()
            .and_then(|pgn| pgn_to_fen(pgn, payload.puzzle_initial_ply));
        let resolved = from_pgn.or_else(|| {
            if payload.piece_map.is_empty() {
                None
            } else {
                let turn = payload.puzzle_turn.as_deref()
                    .and_then(|t| match t { "w" => Some('w'), "b" => Some('b'), _ => None })
                    .unwrap_or('w');
                Some(piece_map_to_fen(&payload.piece_map, turn))
            }
        });
        debug!(
            "[lichess-cdp] puzzle: pgn_present={} initial_ply={:?} piece_map_len={} moves={} uci={} rect={} fen={:?}",
            payload.puzzle_pgn.is_some(),
            payload.puzzle_initial_ply,
            payload.piece_map.len(),
            payload.moves.len(),
            payload.moves_are_uci,
            payload.board_rect.is_some(),
            resolved.as_deref(),
        );
        resolved
    } else {
        None
    };

    let bottom_is_black = payload.bottom_color.as_deref() == Some("black");

    let (white_player, black_player, white_clock, black_clock) = if bottom_is_black {
        (payload.top_player, payload.bottom_player, payload.top_clock, payload.bottom_clock)
    } else {
        (payload.bottom_player, payload.top_player, payload.bottom_clock, payload.top_clock)
    };

    let board_rect = payload.board_rect.and_then(|br| {
        let win_x    = payload.window_screen_x.unwrap_or(0.0);
        let win_y    = payload.window_screen_y.unwrap_or(0.0);
        let chrome_h = payload.window_chrome_height.unwrap_or(0.0).max(0.0);
        if br.w < 10.0 || br.h < 10.0 { return None; }
        Some(Rect::from_min_size(
            Pos2::new(win_x + br.x, win_y + chrome_h + br.y),
            Vec2::new(br.w, br.h),
        ))
    });

    CdpMoveSnapshot {
        moves: payload.moves,
        moves_are_uci: payload.moves_are_uci,
        page_url,
        white_player,
        black_player,
        white_clock,
        black_clock,
        board_rect,
        puzzle_fen,
        is_puzzle,
        bottom_is_black,
        game_result: payload.game_result,
        initial_fen: payload.initial_fen.filter(|f| {
            !f.is_empty() && !f.starts_with("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR")
        }),
    }
}

/// Build the FEN for a puzzle from its setup line.
///
/// `pgn` is a space-separated SAN move list (no move numbers), e.g.
/// `"e4 e5 Nf3 Nc6"`, taken from `page-init-data`'s `data.game.pgn`.  Lichess
/// truncates this to the puzzle position (the opponent's setup move is the last
/// token), so playing the whole line yields the position the solver faces with
/// the correct side to move.  When `initial_ply` is provided we cap the replay
/// at `initial_ply + 1` half-moves as a safety bound.
fn pgn_to_fen(pgn: &str, initial_ply: Option<u32>) -> Option<String> {
    use shakmaty::{Chess, Position, san::San};
    use shakmaty::fen::Fen;
    use std::str::FromStr;

    let tokens: Vec<&str> = pgn
        .split_whitespace()
        // drop move numbers like "1." or "12..." and result markers
        .filter(|t| {
            !t.is_empty()
                && !t.ends_with('.')
                && !matches!(*t, "1-0" | "0-1" | "1/2-1/2" | "*")
        })
        .collect();
    if tokens.is_empty() {
        return None;
    }

    let limit = initial_ply
        .map(|p| (p as usize) + 1)
        .unwrap_or(tokens.len())
        .min(tokens.len());

    let mut pos = Chess::default();
    for tok in tokens.iter().take(limit) {
        let san = San::from_str(tok).ok()?;
        let mv  = san.to_move(&pos).ok()?;
        pos     = pos.play(&mv).ok()?;
    }
    Some(Fen::from_position(pos, shakmaty::EnPassantMode::Legal).to_string())
}

// ── FEN reconstruction from CSS piece positions (puzzles) ────────────────────
fn piece_map_to_fen(pieces: &HashMap<String, String>, turn: char) -> String {
    let to_char = |code: &str| -> char {
        let c = match code.get(1..2).unwrap_or("") {
            "p" => 'p', "n" => 'n', "b" => 'b',
            "r" => 'r', "q" => 'q', "k" => 'k',
            _ => return '?',
        };
        if code.starts_with('w') { c.to_ascii_uppercase() } else { c }
    };
    let mut ranks = String::new();
    for rank in (1u8..=8).rev() {
        let mut empty = 0u8;
        for file in b'a'..=b'h' {
            let sq = format!("{}{}", file as char, rank);
            match pieces.get(&sq) {
                Some(pc) => {
                    if empty > 0 { ranks.push((b'0' + empty) as char); empty = 0; }
                    ranks.push(to_char(pc));
                }
                None => empty += 1,
            }
        }
        if empty > 0 { ranks.push((b'0' + empty) as char); }
        if rank > 1  { ranks.push('/'); }
    }
    let castling = infer_castling(pieces);
    format!("{ranks} {turn} {castling} - 0 1")
}

fn infer_castling(pieces: &HashMap<String, String>) -> String {
    let has = |sq: &str, pc: &str| pieces.get(sq).map(|p| p == pc).unwrap_or(false);
    let mut rights = String::new();
    if has("e1", "wk") {
        if has("h1", "wr") { rights.push('K'); }
        if has("a1", "wr") { rights.push('Q'); }
    }
    if has("e8", "bk") {
        if has("h8", "br") { rights.push('k'); }
        if has("a8", "br") { rights.push('q'); }
    }
    if rights.is_empty() { "-".to_string() } else { rights }
}

// ── JS: Observer setup script ─────────────────────────────────────────────────

fn lichess_observer_script(binding_name: &str, installed_flag: &str) -> String {
    format!(r#"
(function() {{
    const FLAG = '{installed_flag}';
    if (window[FLAG]) return;
    window[FLAG] = true;

    const PUSH = '{binding_name}';

    function takeSnapshot() {{
        // Board and orientation
        const cgWrap  = document.querySelector('.cg-wrap');
        const cgBoard = cgWrap ? cgWrap.querySelector('cg-board') : document.querySelector('cg-board');
        const flipped = (cgWrap?.classList.contains('orientation-black')) || false;

        let board_rect = null;
        if (cgBoard) {{
            const r = cgBoard.getBoundingClientRect();
            if (r && r.width > 10)
                board_rect = {{ x: r.left, y: r.top, w: r.width, h: r.height }};
        }}

        const path      = window.location.pathname.toLowerCase();
        const is_puzzle = /^\/(?:training|puzzle|streak|storm|racer)/.test(path) ||
                          /\/training\//.test(path);

        // ── Moves ────────────────────────────────────────────────────────────
        // Analysis board uses <m2 u="e2e4"> (UCI). Live round games use the
        // move list <rm6>…<kwdb>e4</kwdb><kwdb>g6</kwdb>… (SAN). Prefer UCI
        // when present; otherwise fall back to SAN from the round move list.
        let uciMoves = Array.from(
            document.querySelectorAll('.tview2 m2[u], .rmoves m2[u], .moves m2[u], l4x m2[u]')
        ).map(el => el.getAttribute('u')).filter(Boolean);

        let moves;
        if (uciMoves.length) {{
            moves = uciMoves;
        }} else {{
            // SAN from the move list. The live round uses <rm6>…<kwdb>e4</kwdb>;
            // puzzles/analysis use the move tree <div class="tview2">…<move>e4</move>.
            // Skip variation moves (inside <lines>) so we only replay the mainline.
            const sanNodes = document.querySelectorAll(
                'rm6 kwdb, l4x kwdb, .round__app__board kwdb, .tview2 move, .puzzle__moves move'
            );
            const san = [];
            for (const el of sanNodes) {{
                if (el.closest && el.closest('lines')) continue;
                let t = '';
                for (const n of el.childNodes) {{ if (n.nodeType === 3) t += n.textContent; }}
                t = (t || el.textContent || '').trim();
                if (!t) continue;
                t = t.replace(/^\d+\.+/, '').replace(/^\d+/, '').trim();
                t = t.replace(/[!?]+$/g, '').trim();
                t = t.replace(/^0-0-0$/, 'O-O-O').replace(/^0-0$/, 'O-O');
                if (!t) continue;
                if (/^(O-O|O-O-O)[+#]?$/.test(t) ||
                    /^[KQRBN]?[a-h]?[1-8]?x?[a-h][1-8](=[QRBN])?[+#]?$/.test(t))
                    san.push(t);
            }}
            moves = san;
        }}
        // Lichess JS boots asynchronously; the <l4x> move list doesn't exist in the
        // static HTML. Fall back to page-init-data steps (always present) so the
        // initial snapshot has correct moves before the DOM is rendered.
        if (!uciMoves.length && !is_puzzle) {{
            try {{
                const d = JSON.parse(document.getElementById('page-init-data')?.textContent || '{{}}');
                const steps = d?.data?.game?.steps;
                if (Array.isArray(steps) && steps.length > 1) {{
                    uciMoves = steps.slice(1).map(s => s.uci).filter(Boolean);
                    if (uciMoves.length) moves = uciMoves;
                }}
            }} catch(_) {{}}
        }}
        const usedUci = uciMoves.length > 0;

        // ── Piece map for puzzles ────────────────────────────────────────────
        // Chessground positions pieces with `transform: translate(Xpx, Ypx)`
        // relative to the board's top-left (square size = boardWidth / 8). Some
        // older/board contexts use `left:%; top:%` instead, so support both.
        const piece_map = {{}};
        if (is_puzzle && cgBoard) {{
            const bw = cgBoard.getBoundingClientRect().width;
            const sq = bw > 0 ? bw / 8 : 0;
            for (const el of cgBoard.querySelectorAll('piece')) {{
                const cls = el.className || '';
                const color = cls.includes('white') ? 'w' : cls.includes('black') ? 'b' : null;
                if (!color) continue;
                const type  = cls.includes('king')   ? 'k' : cls.includes('queen')  ? 'q' :
                              cls.includes('rook')   ? 'r' : cls.includes('bishop') ? 'b' :
                              cls.includes('knight') ? 'n' : cls.includes('pawn')   ? 'p' : null;
                if (!type) continue;
                const style = el.getAttribute('style') || '';
                let fileIdx = null, rankIdx = null;
                // Preferred: transform: translate(Xpx, Ypx)
                const tm = style.match(/translate\(\s*(-?[\d.]+)px\s*,\s*(-?[\d.]+)px\s*\)/);
                if (tm && sq > 0) {{
                    const x = parseFloat(tm[1]);
                    const y = parseFloat(tm[2]);
                    const col = Math.round(x / sq);
                    const row = Math.round(y / sq);
                    if (!flipped) {{ fileIdx = col;     rankIdx = 7 - row; }}
                    else          {{ fileIdx = 7 - col; rankIdx = row;     }}
                }} else {{
                    // Fallback: left:%; top:%
                    const topM  = style.match(/top:\s*([\d.]+)%/);
                    const leftM = style.match(/left:\s*([\d.]+)%/);
                    if (topM && leftM) {{
                        const leftPct = parseFloat(leftM[1]);
                        const topPct  = parseFloat(topM[1]);
                        if (!flipped) {{ fileIdx = Math.round(leftPct/12.5);     rankIdx = 7 - Math.round(topPct/12.5); }}
                        else          {{ fileIdx = 7 - Math.round(leftPct/12.5); rankIdx = Math.round(topPct/12.5);     }}
                    }}
                }}
                if (fileIdx === null || rankIdx === null) continue;
                if (fileIdx < 0 || fileIdx > 7 || rankIdx < 0 || rankIdx > 7) continue;
                const square = String.fromCharCode(97 + fileIdx) + (rankIdx + 1);
                piece_map[square] = color + type;
            }}
        }}

        // ── Puzzle turn ──────────────────────────────────────────────────────
        // Lichess ALWAYS orients a puzzle board with the side-to-move at the
        // bottom, so the orientation is the most reliable turn source. This is
        // the only signal available for racer/storm (no PGN, no single FEN).
        let puzzle_turn = flipped ? 'b' : 'w';
        let puzzle_pgn = null;
        let puzzle_initial_ply = null;
        if (is_puzzle) {{
            // Authoritative setup line from page-init-data (game.pgn + initialPly).
            // Present for /training puzzles; absent for racer/storm/streak.
            try {{
                const d = JSON.parse(document.getElementById('page-init-data')?.textContent || '{{}}');
                const pgn = d?.data?.game?.pgn;
                if (typeof pgn === 'string' && pgn.trim()) puzzle_pgn = pgn.trim();
                const ip = d?.data?.puzzle?.initialPly;
                if (Number.isInteger(ip)) puzzle_initial_ply = ip;
            }} catch(_) {{}}
        }}

        // ── Player names ─────────────────────────────────────────────────────
        // .game__meta__players has .player.is.white and .player.is.black
        const cleanName = (sel) => {{
            const el = document.querySelector(sel);
            if (!el) return null;
            // strip rating in parentheses, trim
            return (el.textContent || '').replace(/\s*\(.*?\)/g,'').trim() || null;
        }};
        const whitePl = cleanName('.game__meta__players .player.is.white .user-link') ||
                        cleanName('.ruser-top.color-icon.white') ||
                        cleanName('.player.color-icon.is.white a');
        const blackPl = cleanName('.game__meta__players .player.is.black .user-link') ||
                        cleanName('.ruser-top.color-icon.black') ||
                        cleanName('.player.color-icon.is.black a');
        // top/bottom depend on orientation
        const topPlayer    = flipped ? whitePl : blackPl;
        const bottomPlayer = flipped ? blackPl : whitePl;

        // ── Clocks ───────────────────────────────────────────────────────────
        const clockText = (sel) => {{
            const el = document.querySelector(sel);
            return el ? (el.textContent || '').trim() || null : null;
        }};
        const topClockText    = clockText('.rclock-top .time')    ||
                                clockText('.rclock.rclock-top .rclock__time') ||
                                clockText('.clock-top .time');
        const bottomClockText = clockText('.rclock-bottom .time') ||
                                clockText('.rclock.rclock-bottom .rclock__time') ||
                                clockText('.clock-bottom .time');

        // ── Game result ──────────────────────────────────────────────────────
        // Lichess shows the result in .result-wrap > .result (score token)
        // and in .game__meta__result (text like "White wins by resignation").
        const gameResultEl =
            document.querySelector('.result-wrap .result') ||
            document.querySelector('.game__meta__result') ||
            document.querySelector('.game-over-modal') ||
            document.querySelector('.game__over');
        const game_result = gameResultEl
            ? (gameResultEl.textContent || '').replace(/\s+/g, ' ').trim() || null
            : null;

        // ── Chess960 / variant initial position ──────────────────────────────────
        // Read the non-standard starting FEN from page-init-data so move replay
        // starts from the correct position.  Only populated for variants;
        // standard games leave this null.
        let initial_fen = null;
        if (!is_puzzle) {{
            try {{
                const d = JSON.parse(
                    document.getElementById('page-init-data')?.textContent || '{{}}'
                );
                const variant = d?.data?.game?.variant?.key;
                if (variant && variant !== 'standard' && variant !== 'fromPosition') {{
                    // Chess960 and other variants store their starting FEN here.
                    const fen960 = d?.data?.game?.initialFen
                        || d?.data?.puzzle?.game?.initialFen;
                    if (typeof fen960 === 'string' && fen960.trim() &&
                        !fen960.startsWith('rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR')) {{
                        initial_fen = fen960.trim();
                    }}
                }}
            }} catch(_) {{}}
        }}

        return {{
            moves,
            moves_are_uci:        usedUci,
            bottom_color:         flipped ? 'black' : 'white',
            top_player:           topPlayer,
            bottom_player:        bottomPlayer,
            top_clock:            topClockText,
            bottom_clock:         bottomClockText,
            board_rect,
            window_screen_x:      window.screenX,
            window_screen_y:      window.screenY,
            window_chrome_height: window.outerHeight - window.innerHeight,
            piece_map,
            puzzle_turn,
            is_puzzle,
            puzzle_pgn,
            puzzle_initial_ply,
            initial_fen,
            // Normalise player-token URLs (/GAMEIDTTTT) to the canonical /GAMEID
            // form so ChessPage::from_url correctly returns LichessGame.
            page_url:             (() => {{
                try {{
                    const d = JSON.parse(document.getElementById('page-init-data')?.textContent || '{{}}');
                    const id = d?.data?.game?.id;
                    if (typeof id === 'string' && id.length === 8 && !is_puzzle)
                        return window.location.origin + '/' + id;
                }} catch(_) {{}}
                return window.location.href;
            }})(),
            game_result,
        }};
    }}

    function push() {{
        try {{ window[PUSH](JSON.stringify(takeSnapshot())); }} catch (_) {{}}
    }}

    let debounceTimer = null;
    function debouncedPush() {{
        clearTimeout(debounceTimer);
        debounceTimer = setTimeout(push, 60);
    }}

    const mo = new MutationObserver(debouncedPush);
    const ro = new ResizeObserver(debouncedPush);
    let roAttached = false;

    function attachObservers() {{
        // The round app REPLACES <cg-board> after boot, which would detach an
        // observer bound directly to it. Bind to the STABLE wrappers instead:
        //   .round__app__board  → persists; its subtree mutates on every move
        //   rm6 / .tview2       → move list (SAN/UCI), added child-by-child
        // We deliberately do NOT observe the clock elements (they tick every
        // ~100ms); their current text is read on each board/move push instead.
        const boardWrap = document.querySelector('.round__app__board') ||
                          document.querySelector('.main-board') ||
                          document.querySelector('.puzzle__board') ||
                          document.querySelector('.storm__board') ||
                          document.querySelector('.racer__board') ||
                          document.querySelector('.cg-wrap');
        const cgBoard   = document.querySelector('cg-board');
        const moveList  = document.querySelector('rm6, l4x, .tview2, .rmoves, .moves');
        const resultWrap = document.querySelector('.result-wrap, .rg-status, .game__over');
        let any = false;
        if (boardWrap) {{
            // childList catches new positions; attributes (style/class) catch
            // in-place piece moves (chessground updates `transform`/`class`
            // rather than adding/removing nodes) — essential for storm/racer
            // which have no move list to observe.
            mo.observe(boardWrap, {{
                childList: true, subtree: true,
                attributes: true, attributeFilter: ['style', 'class'],
            }});
            any = true;
        }}
        if (cgBoard && !roAttached) {{ ro.observe(cgBoard); roAttached = true; }}
        if (moveList) {{
            mo.observe(moveList, {{ childList: true, subtree: true }});
            any = true;
        }}
        // Observe the game-result area so we fire immediately when the result
        // becomes visible (or when its text changes), rather than waiting for
        // the next board/move mutation.
        if (resultWrap) {{
            mo.observe(resultWrap, {{ childList: true, subtree: true, characterData: true }});
        }}
        if (!any) {{
            mo.observe(document.body, {{ childList: true, subtree: true }});
            return false;
        }}
        return true;
    }}

    function reinstall() {{
        mo.disconnect();
        roAttached = false;
        attachObservers();
        push();
    }}

    const titleEl = document.querySelector('title');
    if (titleEl) {{
        const titleObserver = new MutationObserver(() => setTimeout(reinstall, 400));
        titleObserver.observe(titleEl, {{ childList: true }});
    }}

    let lastPath = location.pathname;
    setInterval(() => {{
        if (location.pathname !== lastPath) {{
            lastPath = location.pathname;
            setTimeout(reinstall, 400);
        }}
    }}, 500);

    window.addEventListener('popstate', () => setTimeout(reinstall, 400));

    const fullyAttached = attachObservers();
    push();

    setTimeout(() => {{
        mo.disconnect();
        roAttached = false;
        const reattached = attachObservers();
        if (reattached) push();
    }}, 700);

    if (!fullyAttached) {{
        const retryInterval = setInterval(() => {{
            if (document.querySelector('.round__app__board, cg-board, rm6, l4x, .rmoves, .tview2')) {{
                clearInterval(retryInterval);
                mo.disconnect();
                roAttached = false;
                attachObservers();
                push();
            }}
        }}, 500);
    }}
}})();
"#, binding_name = binding_name, installed_flag = installed_flag)
}

// ── JS: one-shot expression ───────────────────────────────────────────────────

fn lichess_oneshot_expression() -> &'static str {
    r#"(() => {
    const cgWrap=document.querySelector('.cg-wrap');
    const cgBoard=cgWrap?cgWrap.querySelector('cg-board'):document.querySelector('cg-board');
    const flipped=cgWrap?.classList.contains('orientation-black')||false;
    let board_rect=null;
    if(cgBoard){const r=cgBoard.getBoundingClientRect();if(r&&r.width>10)board_rect={x:r.left,y:r.top,w:r.width,h:r.height};}
    const path=window.location.pathname.toLowerCase();
    const is_puzzle=/^\/(?:training|puzzle|streak|storm|racer)/.test(path)||/\/training\//.test(path);
    const uciMoves=Array.from(document.querySelectorAll('.tview2 m2[u],.rmoves m2[u],.moves m2[u],l4x m2[u]')).map(el=>el.getAttribute('u')).filter(Boolean);
    let moves;
    if(uciMoves.length){moves=uciMoves;}else{const san=[];for(const el of document.querySelectorAll('rm6 kwdb,l4x kwdb,.round__app__board kwdb,.tview2 move,.puzzle__moves move')){if(el.closest&&el.closest('lines'))continue;let t='';for(const n of el.childNodes){if(n.nodeType===3)t+=n.textContent;}t=(t||el.textContent||'').trim();if(!t)continue;t=t.replace(/^\d+\.+/,'').replace(/^\d+/,'').trim();t=t.replace(/[!?]+$/g,'').trim();t=t.replace(/^0-0-0$/,'O-O-O').replace(/^0-0$/,'O-O');if(!t)continue;if(/^(O-O|O-O-O)[+#]?$/.test(t)||/^[KQRBN]?[a-h]?[1-8]?x?[a-h][1-8](=[QRBN])?[+#]?$/.test(t))san.push(t);}moves=san;}
    if(!uciMoves.length&&!is_puzzle){try{const d=JSON.parse(document.getElementById('page-init-data')?.textContent||'{}');const steps=d?.data?.game?.steps;if(Array.isArray(steps)&&steps.length>1){const u=steps.slice(1).map(s=>s.uci).filter(Boolean);if(u.length){uciMoves=u;moves=u;}}}catch(_){}}
    const usedUci=uciMoves.length>0;
    const piece_map={};
    if(is_puzzle&&cgBoard){const bw=cgBoard.getBoundingClientRect().width;const sq=bw>0?bw/8:0;for(const el of cgBoard.querySelectorAll('piece')){const cls=el.className||'';const color=cls.includes('white')?'w':cls.includes('black')?'b':null;if(!color)continue;const type=cls.includes('king')?'k':cls.includes('queen')?'q':cls.includes('rook')?'r':cls.includes('bishop')?'b':cls.includes('knight')?'n':cls.includes('pawn')?'p':null;if(!type)continue;const style=el.getAttribute('style')||'';let fi=null,ri=null;const tm=style.match(/translate\(\s*(-?[\d.]+)px\s*,\s*(-?[\d.]+)px\s*\)/);if(tm&&sq>0){const x=parseFloat(tm[1]);const y=parseFloat(tm[2]);const col=Math.round(x/sq);const row=Math.round(y/sq);if(!flipped){fi=col;ri=7-row;}else{fi=7-col;ri=row;}}else{const topM=style.match(/top:\s*([\d.]+)%/);const leftM=style.match(/left:\s*([\d.]+)%/);if(topM&&leftM){const leftPct=parseFloat(leftM[1]);const topPct=parseFloat(topM[1]);if(!flipped){fi=Math.round(leftPct/12.5);ri=7-Math.round(topPct/12.5);}else{fi=7-Math.round(leftPct/12.5);ri=Math.round(topPct/12.5);}}}if(fi===null||ri===null)continue;if(fi<0||fi>7||ri<0||ri>7)continue;piece_map[String.fromCharCode(97+fi)+(ri+1)]=color+type;}}
    let puzzle_turn=flipped?'b':'w';
    let puzzle_pgn=null;
    let puzzle_initial_ply=null;
    if(is_puzzle){try{const d=JSON.parse(document.getElementById('page-init-data')?.textContent||'{}');const pgn=d?.data?.game?.pgn;if(typeof pgn==='string'&&pgn.trim())puzzle_pgn=pgn.trim();const ip=d?.data?.puzzle?.initialPly;if(Number.isInteger(ip))puzzle_initial_ply=ip;}catch(_){}}
    const whitePl=(document.querySelector('.game__meta__players .player.is.white .user-link')?.textContent||'').replace(/\s*\(.*?\)/g,'').trim()||null;
    const blackPl=(document.querySelector('.game__meta__players .player.is.black .user-link')?.textContent||'').replace(/\s*\(.*?\)/g,'').trim()||null;
    const topClockText=document.querySelector('.rclock-top .time')?.textContent?.trim()||null;
    const bottomClockText=document.querySelector('.rclock-bottom .time')?.textContent?.trim()||null;
    return {moves,moves_are_uci:usedUci,bottom_color:flipped?'black':'white',top_player:flipped?whitePl:blackPl,bottom_player:flipped?blackPl:whitePl,top_clock:topClockText,bottom_clock:bottomClockText,board_rect,window_screen_x:window.screenX,window_screen_y:window.screenY,window_chrome_height:window.outerHeight-window.innerHeight,piece_map,puzzle_turn,is_puzzle,puzzle_pgn,puzzle_initial_ply,initial_fen:(()=>{try{if(is_puzzle)return null;const d=JSON.parse(document.getElementById('page-init-data')?.textContent||'{}');const vk=d?.data?.game?.variant?.key;if(vk&&vk!=='standard'&&vk!=='fromPosition'){const f=d?.data?.game?.initialFen||d?.data?.puzzle?.game?.initialFen;if(typeof f==='string'&&f.trim()&&!f.startsWith('rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR'))return f.trim();}return null;}catch(_){return null;}})(),page_url:(()=>{try{const d=JSON.parse(document.getElementById('page-init-data')?.textContent||'{}');const id=d?.data?.game?.id;if(typeof id==='string'&&id.length===8&&!is_puzzle)return window.location.origin+'/'+id;}catch(_){}return window.location.href;})()};
})();"#
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pgn_to_fen_training_puzzle() {
        // Real /training/NNKNi data: full-game PGN + initialPly 106, "Black to play".
        let pgn = "d4 Nf6 Nf3 c5 e3 cxd4 exd4 d5 Bf4 Nc6 c3 Bf5 Bd3 Bxd3 Qxd3 e6 O-O Bd6 Bxd6 Qxd6 Re1 O-O Nbd2 Rae8 Ne5 Nd7 Ndf3 f6 Nxd7 Qxd7 Re2 Re7 Rae1 Rfe8 h3 Qd6 a3 a6 Qc2 e5 dxe5 Nxe5 Nd4 Qd7 Re3 Nc4 Rxe7 Rxe7 Rxe7 Qxe7 Kh2 Qe5+ g3 Nd6 Qe2 Qxe2 Nxe2 Nc4 Nf4 Nxb2 Nxd5 b5 Nc7 Na4 Nxa6 Nxc3 Nc7 Kf7 Kg2 Ke7 Kf3 Kd6 Ne8+ Ke5 Nxg7 Nb1 Ke3 Nxa3 f4+ Kd5 Kd3 Nc4 Kc3 Nd6 Nh5 Ke6 Kb4 Kf7 g4 Ne4 f5 Nf2 Nf4 h5 gxh5 Kg7 Kxb5 Kh6 Kc5 Kg5 h6 Kxh6 Kd5 Kg5 Ke6 Ne4 h4+";
        let fen = pgn_to_fen(pgn, Some(106)).expect("should produce a FEN");
        // Puzzle is Black to move.
        let stm = fen.split(' ').nth(1).unwrap();
        assert_eq!(stm, "b", "side to move should be black, got fen: {fen}");
    }
}

