use anyhow::{Context, Result, anyhow};
use egui::{Pos2, Rect, Vec2};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, info, warn};
use tungstenite::{Message, WebSocket};
use tungstenite::stream::MaybeTlsStream;
use std::net::TcpStream;
use crate::perf_scope;

// ── Public snapshot type ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CdpMoveSnapshot {
    pub moves:           Vec<String>,
    pub moves_are_uci:   bool,
    pub page_url:        String,
    pub white_player:    Option<String>,
    pub black_player:    Option<String>,
    pub white_clock:     Option<String>,
    pub black_clock:     Option<String>,
    pub board_rect:      Option<Rect>,
    pub puzzle_fen:      Option<String>,
    pub is_puzzle:       bool,
    pub bottom_is_black: bool,
    pub game_result:     Option<String>,
    /// For Chess960 / variants: the non-standard starting position FEN.
    /// When `Some`, moves are replayed from this position instead of the
    /// default starting position.  `None` for standard chess.
    pub initial_fen:     Option<String>,
}

// ── CDP event — what bg_thread matches on ─────────────────────────────────────

/// Typed event returned from `CdpConnection::next_event`.
/// Every path in `bg_thread` is a match arm — no timers, no race windows.
pub enum CdpEvent {
    /// The MutationObserver pushed a fresh board state.
    BoardState(CdpMoveSnapshot),
    /// SPA navigation detected — observer has been reinstalled and an initial
    /// snapshot was taken.  Caller should reset puzzle gate / game state.
    PageNavigated,
}

// ── CDP target discovery ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CdpTarget {
    pub url: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    pub web_socket_debugger_url: Option<String>,
    pub id: Option<String>,
}

pub fn fetch_targets(endpoint: &str) -> Result<Vec<CdpTarget>> {
    let list_url = format!("{endpoint}/json/list");
    ureq::get(&list_url)
        .timeout(Duration::from_millis(700))
        .call()
        .with_context(|| format!("fetch CDP targets from {list_url}"))?
        .into_json()
        .context("decode CDP target list JSON")
}

fn pick_target<'a>(targets: &'a [CdpTarget]) -> Option<&'a CdpTarget> {
    targets
        .iter()
        .filter(|t| {
            let lower = t.url.to_lowercase();
            (lower.starts_with("https://www.chess.com")
                || lower.starts_with("http://www.chess.com"))
                && !lower.starts_with("devtools://")
        })
        .max_by_key(|t| {
            let lower = t.url.to_lowercase();
            let mut score = 0u32;
            if lower.contains("/puzzles/daily")  { score += 60; }
            if lower.contains("/puzzles/rated")  { score += 55; }
            if lower.contains("/puzzles")        { score += 50; }
            if lower.contains("/learn")          { score += 45; }
            if lower.contains("/game/")          { score += 40; }
            if lower.contains("/play/online")    { score += 30; }
            if lower.contains("/play")           { score += 20; }
            score += 10;
            score
        })
}

// ── Persistent CDP connection ─────────────────────────────────────────────────
//
// Push model: a MutationObserver installed in the page calls a randomly-named
// binding on every board change.  `next_event()` blocks on the WebSocket until
// a typed `CdpEvent` arrives — no timers, no stale checks, no polling loop.
//
// On connect: observer is installed + one `oneshot_eval` captures the current
// state immediately.
//
// On `Page.frameNavigated` / `Page.loadEventFired`: observer is reinstalled and
// `oneshot_eval` is called once, returning `CdpEvent::PageNavigated` so
// `bg_thread` can reset game state cleanly.
//
// Anti-detection: observer injected into an isolated world; binding/flag/world
// names randomised per session.

/// Generate a short, innocuous-looking random identifier with the given prefix.
pub fn random_token(prefix: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mix = nanos
        .rotate_left(17)
        ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (std::process::id() as u64);
    format!("{prefix}{mix:016x}")
}

pub struct CdpConnection {
    ws:              WebSocket<MaybeTlsStream<TcpStream>>,
    target_id:       String,
    target_url:      String,
    msg_id:          u32,
    /// Last known good snapshot — returned on `PageNavigated` after reinstall.
    last:            Option<CdpMoveSnapshot>,
    install_version: u32,
    binding_name:    String,
    flag_name:       String,
    world_name:      String,
}

impl CdpConnection {
    pub fn connect(endpoint: &str) -> Option<Self> {
        let targets = fetch_targets(endpoint).ok()?;
        let target  = pick_target(&targets)?;
        let ws_url  = target.web_socket_debugger_url.as_deref()?;

        let (mut ws, _) = tungstenite::connect(ws_url).ok()?;
        // Blocking read with a timeout so next_event() can return periodically
        // even with no events (e.g. game is idle).  120 ms is tight enough to
        // feel instant while barely loading the CPU.
        if let MaybeTlsStream::Plain(s) = ws.get_mut() {
            let _ = s.set_read_timeout(Some(Duration::from_millis(120)));
        }

        let mut conn = Self {
            ws,
            target_id:       target.id.clone().unwrap_or_default(),
            target_url:      target.url.clone(),
            msg_id:          1,
            last:            None,
            install_version: 0,
            binding_name:    random_token("__"),
            flag_name:       random_token("__"),
            world_name:      random_token("w"),
        };

        // Enable Page events so frameNavigated fires.
        // If observer install fails we still return Some(conn) — next_event()
        // will keep trying to read and the bg_thread reconnect loop handles it.
        match conn.install_observer() {
            Ok(()) => {
                // Capture current state immediately so bg_thread has something
                // to work with before the first mutation fires.
                conn.last = conn.oneshot_eval().ok().flatten();
            }
            Err(e) => {
                warn!("[cdp] observer install failed ({e}), will retry on next event");
            }
        }

        info!("[cdp] connected to: {}", target.url);
        Some(conn)
    }

    // ── Setup helpers ─────────────────────────────────────────────────────────

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
            .ok_or_else(|| anyhow!("no main frame id in getFrameTree response"))
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
            .ok_or_else(|| anyhow!("no executionContextId from createIsolatedWorld"))
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

        let script = observer_setup_script(&self.binding_name, &self.flag_name);
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

        info!(
            "[cdp] push observer installed (v{version}, isolated={})",
            ctx_id.is_some()
        );
        Ok(())
    }

    // ── Event loop ────────────────────────────────────────────────────────────

    /// Block until the next typed `CdpEvent` arrives, then return it.
    ///
    /// Possible return values:
    /// - `Ok(CdpEvent::BoardState(snap))` — observer fired, new board state.
    /// - `Ok(CdpEvent::PageNavigated)`    — SPA nav; observer reinstalled,
    ///                                       caller should reset game state.
    /// - `Err(_)`                         — WebSocket dead; caller reconnects.
    ///
    /// When the socket times out with no event and we have a cached snapshot,
    /// we return it so the idle-detection logic in `bg_thread` still fires.
    /// When there's no cached snapshot we take a one-shot eval to get the
    /// initial state (e.g. the very first frame after connect).
    pub fn next_event(&mut self) -> Result<CdpEvent> {
        perf_scope!("cdp_next_event");
        loop {
            let msg = match self.ws.read() {
                Ok(m) => m,
                Err(tungstenite::Error::Io(ref e))
                    if matches!(e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                {
                    // Nothing arrived within the 120 ms window — return the
                    // cached snapshot (so idle detection keeps working) or
                    // take a fresh one-shot if we have nothing yet.
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
            perf_scope!("cdp_json_parse");
            let v: Value = match serde_json::from_str(&txt) {
                Ok(v) => v,
                Err(_) => continue,
            };

            match v.get("method").and_then(Value::as_str) {
                // ── SPA navigation ────────────────────────────────────────────
                // chess.com is a SPA. When the user navigates between pages
                // (game → puzzles, etc.) the JS context is destroyed, silently
                // killing the MutationObserver.  `Page.frameNavigated` fires at
                // that moment — we reinstall immediately and take a one-shot so
                // the caller gets the new page's state right away.
                Some("Page.frameNavigated") => {
                    // `Page.frameNavigated` fires for ALL frames, including ads and
                    // analytics iframes.  Only treat it as a real navigation when
                    // it's the main frame (parentId is absent ↔ Value::Null).
                    let is_main_frame = v["params"]["frame"]["parentId"].is_null();
                    if !is_main_frame { continue; }

                    tracing::info!("[cdp] main frame navigated — reinstalling observer");
                    self.last = None;
                    match self.install_observer() {
                        Ok(()) => {
                            self.last = self.oneshot_eval().ok().flatten();
                        }
                        Err(e) => {
                            warn!("[cdp] post-nav reinstall failed: {e}");
                        }
                    }
                    return Ok(CdpEvent::PageNavigated);
                }

                Some("Page.loadEventFired") => {
                    tracing::info!("[cdp] load event fired — reinstalling observer");
                    self.last = None;
                    match self.install_observer() {
                        Ok(()) => {
                            self.last = self.oneshot_eval().ok().flatten();
                        }
                        Err(e) => {
                            warn!("[cdp] post-nav reinstall failed: {e}");
                        }
                    }
                    return Ok(CdpEvent::PageNavigated);
                }

                // ── Observer push ─────────────────────────────────────────────
                Some("Runtime.bindingCalled") => {
                    let params = match v.get("params") { Some(p) => p, None => continue };
                    if params.get("name").and_then(Value::as_str)
                        != Some(self.binding_name.as_str()) { continue; }
                    let payload_str = match params.get("payload").and_then(Value::as_str) {
                        Some(s) => s,
                        None => continue,
                    };
                    match self.parse_binding_payload(payload_str) {
                        Ok(snapshot) => {
                            debug!(
                                url        = %self.target_url,
                                move_count = snapshot.moves.len(),
                                is_puzzle  = snapshot.is_puzzle,
                                "[cdp] binding push received"
                            );
                            self.last = Some(snapshot.clone());
                            return Ok(CdpEvent::BoardState(snapshot));
                        }
                        Err(e) => {
                            warn!("[cdp] payload parse error: {e}");
                            continue;
                        }
                    }
                }

                // ── All other CDP events — ignore ─────────────────────────────
                _ => continue,
            }
        }
    }

    fn parse_binding_payload(&self, json_str: &str) -> Result<CdpMoveSnapshot> {
        let payload: EvalPayload = serde_json::from_str(json_str)
            .context("decode binding payload")?;
        Ok(build_snapshot(payload, self.target_url.clone()))
    }

    /// One-shot JS eval — used only at connect time and after page navigation.
    /// Not called on a timer; called exactly once per event that warrants it.
    fn oneshot_eval(&mut self) -> Result<Option<CdpMoveSnapshot>> {
        let id = self.msg_id;
        self.msg_id += 1;
        let req = json!({
            "id": id,
            "method": "Runtime.evaluate",
            "params": {
                "expression":    unified_expression_oneshot(),
                "returnByValue": true,
                "awaitPromise":  true,
            }
        });
        // Temporarily widen the read timeout for the round-trip eval.
        if let MaybeTlsStream::Plain(s) = self.ws.get_mut() {
            let _ = s.set_read_timeout(Some(Duration::from_millis(300)));
        }
        self.ws.send(Message::Text(req.to_string()))
            .context("oneshot_eval send")?;
        let result = loop {
            let msg = match self.ws.read() {
                Ok(m) => m,
                Err(tungstenite::Error::Io(ref e))
                    if matches!(e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                { break None; }
                Err(e) => return Err(anyhow!("oneshot_eval read: {e}")),
            };
            let Message::Text(txt) = msg else { continue };
            let v: Value = serde_json::from_str(&txt).unwrap_or(Value::Null);
            if v.get("id").and_then(Value::as_u64) != Some(id as u64) { continue; }
            if let Some(value) = v
                .get("result")
                .and_then(|r| r.get("result"))
                .and_then(|r| r.get("value"))
            {
                if let Ok(payload) = serde_json::from_value::<EvalPayload>(value.clone()) {
                    let snap = build_snapshot(payload, self.target_url.clone());
                    self.last = Some(snap.clone());
                    break Some(snap);
                }
            }
            break None;
        };
        // Restore normal read timeout.
        if let MaybeTlsStream::Plain(s) = self.ws.get_mut() {
            let _ = s.set_read_timeout(Some(Duration::from_millis(120)));
        }
        Ok(result)
    }

    // ── Target-change detection ───────────────────────────────────────────────

    pub fn target_changed(&self, endpoint: &str) -> bool {
        let Ok(targets) = fetch_targets(endpoint) else { return false };
        let Some(best)  = pick_target(&targets)   else { return false };
        best.id.as_deref() != Some(&self.target_id)
    }
}

// ── Empty snapshot (returned when nothing is known yet) ───────────────────────

impl CdpMoveSnapshot {
   pub(super)  fn empty() -> Self {
        Self {
            moves:           Vec::new(),
            moves_are_uci:   false,
            page_url:        String::new(),
            white_player:    None,
            black_player:    None,
            white_clock:     None,
            black_clock:     None,
            board_rect:      None,
            puzzle_fen:      None,
            is_puzzle:       false,
            bottom_is_black: false,
            game_result:     None,
            initial_fen:     None,
        }
    }
}

// ── Snapshot builder ──────────────────────────────────────────────────────────

fn build_snapshot(payload: EvalPayload, page_url: String) -> CdpMoveSnapshot {
    let is_puzzle = payload.is_puzzle.unwrap_or(false);

    let puzzle_fen = if is_puzzle && !payload.piece_map.is_empty() {
        let turn = payload.puzzle_turn.as_deref()
            .and_then(|t| match t { "w" => Some('w'), "b" => Some('b'), _ => None })
            .unwrap_or('w');
        Some(piece_map_to_fen(&payload.piece_map, turn))
    } else {
        None
    };

    let normalized_moves = normalize_sidebar_moves(payload.moves);
    let bottom_is_black  = payload.bottom_color.as_deref() == Some("black");

    let (white_player, black_player, white_clock, black_clock) = if bottom_is_black {
        (payload.top_player,    payload.bottom_player,
         payload.top_clock,     payload.bottom_clock)
    } else {
        (payload.bottom_player, payload.top_player,
         payload.bottom_clock,  payload.top_clock)
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
        moves: normalized_moves,
        moves_are_uci: false,
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
            // Only use initial_fen when it differs from the standard start position.
            // This avoids any overhead for the vast majority of standard games.
            !f.is_empty() && !f.starts_with("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR")
        }),
    }
}

// ── Payload structs ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct JsBoardRect { x: f32, y: f32, w: f32, h: f32 }

#[derive(Debug, Deserialize)]
struct EvalPayload {
    #[serde(default)]
    moves: Vec<String>,
    bottom_color: Option<String>,
    top_player: Option<String>,
    bottom_player: Option<String>,
    top_clock: Option<String>,
    bottom_clock: Option<String>,
    board_rect: Option<JsBoardRect>,
    window_screen_x: Option<f32>,
    window_screen_y: Option<f32>,
    window_chrome_height: Option<f32>,
    #[serde(default)]
    piece_map: HashMap<String, String>,
    puzzle_turn: Option<String>,
    is_puzzle: Option<bool>,
    game_result: Option<String>,
    /// Non-standard starting FEN for Chess960 / variants.
    #[serde(default)]
    initial_fen: Option<String>,
}

// ── FEN builder ───────────────────────────────────────────────────────────────

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

// ── Move normaliser ───────────────────────────────────────────────────────────

fn normalize_sidebar_moves(raw: Vec<String>) -> Vec<String> {
    fn is_clock_token(s: &str) -> bool {
        let lower = s.trim().to_ascii_lowercase();
        if let Some(num) = lower.strip_suffix('s') {
            return !num.is_empty() && num.chars().all(|c| c.is_ascii_digit());
        }
        false
    }
    fn is_san_like(s: &str) -> bool {
        if s.is_empty() { return false; }
        if matches!(s, "O-O" | "O-O+" | "O-O#" | "O-O-O" | "O-O-O+" | "O-O-O#") {
            return true;
        }
        if is_clock_token(s) { return false; }
        let has_file = s.chars().any(|c| ('a'..='h').contains(&c));
        let has_rank = s.chars().any(|c| ('1'..='8').contains(&c));
        if !has_file || !has_rank { return false; }
        s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, 'x'|'X'|'='|'+'|'#'|'-'))
    }

    let mut out = Vec::new();
    for tok in raw {
        let mut s = tok.trim().to_string();
        if s.is_empty() { continue; }
        if let Some(idx) = s.find('.') {
            let (left, right) = s.split_at(idx);
            if !left.is_empty() && left.chars().all(|c| c.is_ascii_digit()) {
                s = right.trim_start_matches('.').trim().to_string();
            }
        }
        if s.is_empty() || s == "..." || s == "1-0" || s == "0-1"
            || s == "1/2-1/2" || s == "*"
            || s.chars().all(|c| c.is_ascii_digit() || c == '.') { continue; }

        s = s.replace("0-0-0", "O-O-O").replace("0-0", "O-O");
        s = s.replace(['♔','♚'], "K").replace(['♕','♛'], "Q")
             .replace(['♖','♜'], "R").replace(['♗','♝'], "B")
             .replace(['♘','♞'], "N").replace(['♙','♟'], "");
        s = s.split_whitespace().collect::<String>();
        while s.ends_with('!') || s.ends_with('?') { s.pop(); }
        if s.is_empty() || !is_san_like(&s) { continue; }
        out.push(s);
    }

    if out.len() >= 4 && out.len() % 2 == 0 {
        let half = out.len() / 2;
        if out[..half] == out[half..] { out.truncate(half); }
    }
    out
}

// ── JS: Observer setup script ─────────────────────────────────────────────────

fn observer_setup_script(binding_name: &str, installed_flag: &str) -> String {
    format!(r#"
(function() {{
    const FLAG = '{installed_flag}';
    if (window[FLAG]) return;
    window[FLAG] = true;

    const PUSH = '{binding_name}';

    const cleanText = (v) => {{
        const s = (v || '').replace(/\s+/g, ' ').trim();
        return s || null;
    }};
    const firstText = (selectors) => {{
        for (const sel of selectors) {{
            const el = document.querySelector(sel);
            const text = cleanText(el?.textContent || '');
            if (text) return text;
        }}
        return null;
    }};
    const normalizeCastle = (text) => {{
        const t = (text || '').trim()
            .replace(/^0-0-0([+#]?)$/, 'O-O-O$1')
            .replace(/^0-0([+#]?)$/,   'O-O$1');
        return /^(O-O|O-O-O)[+#]?$/.test(t) ? t : null;
    }};
    const isClockToken = (t) => /^\d+s$/i.test((t || '').trim());
    const isLikelySan  = (text) => {{
        const t = (text || '').trim();
        if (!t) return false;
        if (normalizeCastle(t)) return true;
        if (isClockToken(t)) return false;
        if (!/[a-h]/.test(t) || !/[1-8]/.test(t)) return false;
        return /^[KQRBNa-h1-8xX=+#-]+$/.test(t);
    }};

    function takeSnapshot() {{
        const boardEl = document.querySelector(
            '.board, .board-layout-board, wc-chess-board, chess-board, cg-board'
        );
        const boardClass = [
            boardEl?.className || '',
            boardEl?.getAttribute?.('class') || '',
            boardEl?.getAttribute?.('orientation') || '',
            boardEl?.getAttribute?.('data-board-orientation') || '',
        ].join(' ').toLowerCase();
        const bottomColor = /(^|\s|-)black($|\s|-)|flipped|orientation-black/.test(boardClass)
            ? 'black' : 'white';
        const flipped = bottomColor === 'black';

        let board_rect = null;
        if (boardEl) {{
            const r = boardEl.getBoundingClientRect();
            if (r && r.width > 10) board_rect = {{ x: r.left, y: r.top, w: r.width, h: r.height }};
        }}

        const path      = window.location.pathname.toLowerCase();
        const is_puzzle = /\/(puzzles|daily|learn|vision|practice)/.test(path);

        let piece_map = {{}};
        let puzzle_turn = '';
        if (is_puzzle) {{
            document.querySelectorAll('.piece').forEach(el => {{
                const cls = el.className || '';
                let sq = null;
                const am = cls.match(/square-([a-h][1-8])/);
                if (am) {{ sq = am[1]; }} else {{
                    const nm = cls.match(/square-(\d)(\d)/);
                    if (nm) sq = String.fromCharCode(96 + parseInt(nm[1])) + nm[2];
                }}
                if (!sq) return;
                const pm = cls.match(/\b([wb][pnbrqk])\b/);
                if (pm) piece_map[sq] = pm[1];
            }});
            const turnEl = document.querySelector(
                '[data-cy="puzzle-turn-label"],.puzzle-turn,.mini-board-pull,' +
                '.training-turn,[class*="turn-label"]'
            );
            const tt = (turnEl?.textContent || '').toLowerCase();
            if (tt.includes('black')) {{ puzzle_turn = 'b'; }}
            else if (tt.includes('white')) {{ puzzle_turn = 'w'; }}
            else {{
                const instrEl = document.querySelector(
                    '[data-cy="puzzle-instruction"],[class*="instruction-text"],[class*="puzzle-header"]'
                );
                const it = (instrEl?.textContent || '').toLowerCase();
                if (it.includes('black')) {{ puzzle_turn = 'b'; }}
                else if (it.includes('white')) {{ puzzle_turn = 'w'; }}
                else {{
                    const ta = document.querySelector('.clock-top.clock-player-turn');
                    const ba = document.querySelector('.clock-bottom.clock-player-turn');
                    if (ta) {{ puzzle_turn = flipped ? 'w' : 'b'; }}
                    else if (ba) {{ puzzle_turn = flipped ? 'b' : 'w'; }}
                    else {{
                        const hl = document.querySelectorAll('[class*="highlight"]');
                        for (const sq of hl) {{
                            const m = (sq.className || '').match(/square-(\d)(\d)/);
                            if (!m) continue;
                            const sn = String.fromCharCode(96 + parseInt(m[1])) + m[2];
                            const pc = piece_map[sn];
                            if (pc) {{ puzzle_turn = pc.startsWith('w') ? 'b' : 'w'; break; }}
                        }}
                        if (!puzzle_turn) {{ puzzle_turn = bottomColor === 'black' ? 'b' : 'w'; }}
                    }}
                }}
            }}
        }}

        const plfc = (c) => {{
            c = (c || '').toLowerCase();
            if (/\b(knight|horse|[wb]n)\b/.test(c)) return 'N';
            if (/\b(bishop|[wb]b)\b/.test(c))       return 'B';
            if (/\b(rook|[wb]r)\b/.test(c))         return 'R';
            if (/\b(queen|[wb]q)\b/.test(c))        return 'Q';
            if (/\b(king|[wb]k)\b/.test(c))         return 'K';
            if (/\b(pawn|[wb]p)\b/.test(c))         return 'P';
            return null;
        }};
        const plfn = (node) => {{
            if (!node?.querySelectorAll) return null;
            for (const el of node.querySelectorAll('[class]')) {{
                const r = plfc(el.className)
                    || plfc(el.getAttribute('aria-label'))
                    || plfc(el.getAttribute('data-piece'));
                if (r) return r;
            }}
            return null;
        }};
        const buildSan = (node) => {{
            if (!node) return null;
            let text = cleanText(node.textContent || '');
            if (!text) return null;
            text = text.replace(/^\d+\.{{1,3}}\s*/, '').trim();
            if (!text || ['...','1-0','0-1','1/2-1/2','*'].includes(text)) return null;
            const c = normalizeCastle(text);
            if (c) return c;
            text = text
                .replace(/[♔♚]/g,'K').replace(/[♕♛]/g,'Q')
                .replace(/[♖♜]/g,'R').replace(/[♗♝]/g,'B')
                .replace(/[♘♞]/g,'N').replace(/[♙♟]/g,'')
                .replace(/\s+/g,'');
            const c2 = normalizeCastle(text);
            if (c2) return c2;
            const p = plfn(node);
            if (p && p !== 'P' && !/^[KQRBN]/.test(text)) text = p + text;
            text = text.replace(/[!?]+$/g,'').trim();
            return (!text || !isLikelySan(text)) ? null : text;
        }};

        const moves = [];
        for (const row of document.querySelectorAll('[data-whole-move-number],.move-list-row')) {{
            let cols = row.querySelectorAll('.node,.move-node,.move-text-component,[data-ply]');
            if (!cols.length) cols = row.querySelectorAll('button,span[data-figurine],span');
            for (const col of cols) {{
                const tok = buildSan(col);
                if (tok) moves.push(tok);
            }}
        }}
        if (!moves.length) {{
            const panel = document.querySelector(
                '[data-cy="move-list"],.vertical-move-list,.move-list-component,.analysis-moves'
            );
            const text = (panel?.textContent || '').trim();
            if (text) {{
                for (const raw of text.split(/\s+/)) {{
                    let clean = raw.trim().replace(/[!?]+$/,'');
                    if (!clean || /^\d+\.{{1,3}}$/.test(clean)) continue;
                    if (['...','1-0','0-1','1/2-1/2','*'].includes(clean)) continue;
                    clean = normalizeCastle(clean) || clean;
                    if (isLikelySan(clean)) moves.push(clean);
                }}
            }}
        }}

        const topPlayer    = firstText(['#board-layout-player-top [data-test-element="user-tagline-username"]','.board-layout-top [data-test-element="user-tagline-username"]','#board-layout-player-top [class*="cc-user-username"]','.board-layout-top [class*="cc-user-username"]','[data-cy="player-top"] [data-test-element="user-tagline-username"]','#board-layout-player-top [class*="username"]']);
        const bottomPlayer = firstText(['#board-layout-player-bottom [data-test-element="user-tagline-username"]','.board-layout-bottom [data-test-element="user-tagline-username"]','#board-layout-player-bottom [class*="cc-user-username"]','.board-layout-bottom [class*="cc-user-username"]','[data-cy="player-bottom"] [data-test-element="user-tagline-username"]','#board-layout-player-bottom [class*="username"]']);
        const topClockText    = firstText(['.clock-top .clock-component-time','.clock-top .clock-time-monospace','.clock-top .clock-time']);
        const bottomClockText = firstText(['.clock-bottom .clock-component-time','.clock-bottom .clock-time-monospace','.clock-bottom .clock-time']);

        // ── Chess960 / variant initial position ──────────────────────────────
        // chess.com stores game config in several locations; try each in order.
        // `start-fen` on the `<chess-board>` element is the most reliable.
        let initial_fen = null;
        if (!is_puzzle) {{
            try {{
                const boardEl = document.querySelector('chess-board');
                const sf = boardEl?.getAttribute('start-fen');
                if (sf && !sf.startsWith('rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR'))
                    initial_fen = sf;
            }} catch(_) {{}}
        }}

        return {{
            moves,
            bottom_color:          bottomColor,
            top_player:            topPlayer,
            bottom_player:         bottomPlayer,
            top_clock:             topClockText,
            bottom_clock:          bottomClockText,
            board_rect,
            window_screen_x:       window.screenX,
            window_screen_y:       window.screenY,
            window_chrome_height:  window.outerHeight - window.innerHeight,
            piece_map,
            puzzle_turn,
            is_puzzle,
            initial_fen,
            game_result: firstText([
                '[data-cy="game-result"]',
                '.game-over-modal-content [class*="header"]',
                '.game-over-modal-content [class*="message"]',
                '[class*="game-over-modal"] [class*="header"]',
                '[class*="game-over-modal"] [class*="result"]',
                '.game-over-modal-content .result-component',
                '.game-result-component',
                '.result-wrap .result',
                '.game__result',
                '.game-over-message',
            ]),
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
        const boardWrap = document.querySelector(
            '.board-layout-main, .board-layout-board, #board-layout-main'
        ) || document.querySelector('.board') || document.body;
        const moveList = document.querySelector(
            '[data-cy="move-list"], .vertical-move-list, .move-list-component, .analysis-moves'
        );
        const boardEl = document.querySelector('wc-chess-board, chess-board, .board');

        mo.observe(boardWrap, {{ childList: true, subtree: true }});
        if (moveList) mo.observe(moveList, {{ childList: true, subtree: true, characterData: true }});
        if (boardEl && !roAttached) {{ ro.observe(boardEl); roAttached = true; }}
    }}

    attachObservers();
    push();

    // Retry attach once after 700ms to catch late DOM hydration.
    setTimeout(() => {{
        mo.disconnect();
        roAttached = false;
        attachObservers();
        push();
    }}, 700);
}})();
"#, binding_name = binding_name, installed_flag = installed_flag)
}

// ── JS: One-shot expression ───────────────────────────────────────────────────

fn unified_expression_oneshot() -> &'static str {
    r#"(() => {
    const cleanText=(v)=>{const s=(v||'').replace(/\s+/g,' ').trim();return s||null;};
    const firstText=(selectors)=>{for(const sel of selectors){const el=document.querySelector(sel);const text=cleanText(el?.textContent||'');if(text)return text;}return null;};
    const normalizeCastle=(text)=>{const t=(text||'').trim().replace(/^0-0-0([+#]?)$/,'O-O-O$1').replace(/^0-0([+#]?)$/,'O-O$1');return /^(O-O|O-O-O)[+#]?$/.test(t)?t:null;};
    const isClockToken=(t)=>/^\d+s$/i.test((t||'').trim());
    const isLikelySan=(text)=>{const t=(text||'').trim();if(!t)return false;if(normalizeCastle(t))return true;if(isClockToken(t))return false;if(!/[a-h]/.test(t)||!/[1-8]/.test(t))return false;return /^[KQRBNa-h1-8xX=+#-]+$/.test(t);};
    const boardEl=document.querySelector('.board,.board-layout-board,wc-chess-board,chess-board,cg-board');
    const boardClass=[boardEl?.className||'',boardEl?.getAttribute?.('class')||'',boardEl?.getAttribute?.('orientation')||'',boardEl?.getAttribute?.('data-board-orientation')||''].join(' ').toLowerCase();
    const bottomColor=/(^|\s|-)black($|\s|-)|flipped|orientation-black/.test(boardClass)?'black':'white';
    const flipped=bottomColor==='black';
    let board_rect=null;
    if(boardEl){const r=boardEl.getBoundingClientRect();if(r&&r.width>10)board_rect={x:r.left,y:r.top,w:r.width,h:r.height};}
    const path=window.location.pathname.toLowerCase();
    const is_puzzle=/\/(puzzles|daily|learn|vision|practice)/.test(path);
    let piece_map={},puzzle_turn='';
    if(is_puzzle){
        document.querySelectorAll('.piece').forEach(el=>{
            const cls=el.className||'';let sq=null;
            const am=cls.match(/square-([a-h][1-8])/);
            if(am){sq=am[1];}else{const nm=cls.match(/square-(\d)(\d)/);if(nm){sq=String.fromCharCode(96+parseInt(nm[1]))+nm[2];}}
            if(!sq)return;
            const pm=cls.match(/\b([wb][pnbrqk])\b/);if(pm)piece_map[sq]=pm[1];
        });
        const turnEl=document.querySelector('[data-cy="puzzle-turn-label"],.puzzle-turn,.mini-board-pull,.training-turn,[class*="turn-label"]');
        const tt=(turnEl?.textContent||'').toLowerCase();
        if(tt.includes('black')){puzzle_turn='b';}
        else if(tt.includes('white')){puzzle_turn='w';}
        else{const instrEl=document.querySelector('[data-cy="puzzle-instruction"],[class*="instruction-text"],[class*="puzzle-header"]');const it=(instrEl?.textContent||'').toLowerCase();if(it.includes('black')){puzzle_turn='b';}else if(it.includes('white')){puzzle_turn='w';}else{const ta=document.querySelector('.clock-top.clock-player-turn');const ba=document.querySelector('.clock-bottom.clock-player-turn');if(ta){puzzle_turn=flipped?'w':'b';}else if(ba){puzzle_turn=flipped?'b':'w';}else{const hl=document.querySelectorAll('[class*="highlight"]');for(const sq of hl){const m=(sq.className||'').match(/square-(\d)(\d)/);if(!m)continue;const sn=String.fromCharCode(96+parseInt(m[1]))+m[2];const pc=piece_map[sn];if(pc){puzzle_turn=pc.startsWith('w')?'b':'w';break;}}if(!puzzle_turn){puzzle_turn=bottomColor==='black'?'b':'w';}}}}
    }
    const plfc=(c)=>{c=(c||'').toLowerCase();if(/\b(knight|horse|[wb]n)\b/.test(c))return'N';if(/\b(bishop|[wb]b)\b/.test(c))return'B';if(/\b(rook|[wb]r)\b/.test(c))return'R';if(/\b(queen|[wb]q)\b/.test(c))return'Q';if(/\b(king|[wb]k)\b/.test(c))return'K';if(/\b(pawn|[wb]p)\b/.test(c))return'P';return null;};
    const plfn=(node)=>{if(!node?.querySelectorAll)return null;for(const el of node.querySelectorAll('[class]')){const r=plfc(el.className)||plfc(el.getAttribute('aria-label'))||plfc(el.getAttribute('data-piece'));if(r)return r;}return null;};
    const buildSan=(node)=>{if(!node)return null;let text=cleanText(node.textContent||'');if(!text)return null;text=text.replace(/^\d+\.{1,3}\s*/,'').trim();if(!text||['...','1-0','0-1','1/2-1/2','*'].includes(text))return null;const c=normalizeCastle(text);if(c)return c;text=text.replace(/[♔♚]/g,'K').replace(/[♕♛]/g,'Q').replace(/[♖♜]/g,'R').replace(/[♗♝]/g,'B').replace(/[♘♞]/g,'N').replace(/[♙♟]/g,'').replace(/\s+/g,'');const c2=normalizeCastle(text);if(c2)return c2;const p=plfn(node);if(p&&p!=='P'&&!/^[KQRBN]/.test(text))text=p+text;text=text.replace(/[!?]+$/g,'').trim();return(!text||!isLikelySan(text))?null:text;};
    const moves=[];
    for(const row of document.querySelectorAll('[data-whole-move-number],.move-list-row')){let cols=row.querySelectorAll('.node,.move-node,.move-text-component,[data-ply]');if(!cols.length)cols=row.querySelectorAll('button,span[data-figurine],span');for(const col of cols){const tok=buildSan(col);if(tok)moves.push(tok);}}
    if(!moves.length){const panel=document.querySelector('[data-cy="move-list"],.vertical-move-list,.move-list-component,.analysis-moves');const text=(panel?.textContent||'').trim();if(text){for(const raw of text.split(/\s+/)){let clean=raw.trim().replace(/[!?]+$/g,'');if(!clean||/^\d+\.{1,3}$/.test(clean))continue;if(['...','1-0','0-1','1/2-1/2','*'].includes(clean))continue;clean=normalizeCastle(clean)||clean;if(isLikelySan(clean))moves.push(clean);}}}
    const topPlayer=firstText(['#board-layout-player-top [data-test-element="user-tagline-username"]','.board-layout-top [data-test-element="user-tagline-username"]','#board-layout-player-top [class*="cc-user-username"]','.board-layout-top [class*="cc-user-username"]','[data-cy="player-top"] [data-test-element="user-tagline-username"]','#board-layout-player-top [class*="username"]']);
    const bottomPlayer=firstText(['#board-layout-player-bottom [data-test-element="user-tagline-username"]','.board-layout-bottom [data-test-element="user-tagline-username"]','#board-layout-player-bottom [class*="cc-user-username"]','.board-layout-bottom [class*="cc-user-username"]','[data-cy="player-bottom"] [data-test-element="user-tagline-username"]','#board-layout-player-bottom [class*="username"]']);
    const topClockText=firstText(['.clock-top .clock-component-time','.clock-top .clock-time-monospace','.clock-top .clock-time']);
    const bottomClockText=firstText(['.clock-bottom .clock-component-time','.clock-bottom .clock-time-monospace','.clock-bottom .clock-time']);
    return {moves,bottom_color:bottomColor,top_player:topPlayer,bottom_player:bottomPlayer,top_clock:topClockText,bottom_clock:bottomClockText,board_rect,window_screen_x:window.screenX,window_screen_y:window.screenY,window_chrome_height:window.outerHeight-window.innerHeight,piece_map,puzzle_turn,is_puzzle,initial_fen:(()=>{try{const b=document.querySelector('chess-board');const sf=b?.getAttribute('start-fen');return(sf&&!sf.startsWith('rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR'))?sf:null;}catch(_){return null;}})(),game_result:firstText(['[data-cy="game-result"]','.game-over-modal-content [class*="header"]','.game-over-modal-content [class*="message"]','[class*="game-over-modal"] [class*="header"]','[class*="game-over-modal"] [class*="result"]','.game-over-modal-content .result-component','.game-result-component','.result-wrap .result','.game__result','.game-over-message'])};
})()
"#
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_legal_consecutive_identical_san_moves() {
        let moves = normalize_sidebar_moves(vec![
            "e4".into(), "e6".into(), "d4".into(), "d5".into(),
            "exd5".into(), "exd5".into(), "Nf3".into(),
        ]);
        assert_eq!(moves, ["e4", "e6", "d4", "d5", "exd5", "exd5", "Nf3"]);
    }

    #[test]
    fn normalizes_literal_figurine_san() {
        let moves = normalize_sidebar_moves(vec!["♘ f3".into(), "♝ g4".into()]);
        assert_eq!(moves, ["Nf3", "Bg4"]);
    }

    #[test]
    fn keeps_castling_check_suffixes() {
        let moves = normalize_sidebar_moves(vec![
            "O-O+".into(), "O-O-O+".into(), "0-0#".into(), "0-0-0#".into(),
        ]);
        assert_eq!(moves, ["O-O+", "O-O-O+", "O-O#", "O-O-O#"]);
    }

    #[test]
    fn piece_map_to_fen_starting_position() {
        let mut pieces = HashMap::new();
        pieces.insert("e1".into(), "wk".into());
        pieces.insert("e8".into(), "bk".into());
        pieces.insert("h1".into(), "wr".into());
        pieces.insert("a1".into(), "wr".into());
        pieces.insert("h8".into(), "br".into());
        pieces.insert("a8".into(), "br".into());
        let fen = piece_map_to_fen(&pieces, 'w');
        assert!(fen.contains("KQkq"), "castling rights should be KQkq, got: {fen}");
        assert!(fen.ends_with("- 0 1"));
    }

    #[test]
    fn piece_map_to_fen_no_castling() {
        let mut pieces = HashMap::new();
        pieces.insert("d4".into(), "wk".into());
        pieces.insert("d5".into(), "bk".into());
        let fen = piece_map_to_fen(&pieces, 'b');
        assert!(fen.contains(" b - - 0 1"), "got: {fen}");
    }

    #[test]
    fn observer_script_contains_binding_name() {
        let script = observer_setup_script("__ccMoveReady", "__ccObserverReady");
        assert!(script.contains("__ccMoveReady"));
        assert!(script.contains("__ccObserverReady"));
    }

    #[test]
    fn numeric_square_to_algebraic_conversion() {
        let file = char::from(96 + 4u8);
        let rank = '5';
        assert_eq!(format!("{file}{rank}"), "d5");
    }
}