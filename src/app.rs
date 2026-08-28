//! `RustyChessApp` — the eframe::App that IS the overlay.
//!
//! # Architecture
//!
//! Three threads communicate via typed channels:
//!   - `bg_thread`     — CDP WebSocket event loop (push model, no polling)
//!   - `engine_thread` — Stockfish UCI wrapper
//!   - render thread   — egui/eframe (this file)
//!
//! # GPU budget
//!
//!   menu open          → 30 Hz
//!   new analysis       → 20 Hz
//!   idle               →  5 Hz
//!
//! `analysis_changed` is evaluated BEFORE being cleared each frame so a result
//! arriving during `drain_messages` is never missed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use crate::perf_scope;

use egui::Rect;

use crate::config::{ChessPage, Config};
use crate::config::tuning::{EVAL_SMOOTHING, GAME_IDLE_SECS, SCORE_CHANGE_THRESHOLD};
use crate::engine::{AnalysisEngine, AnalysisResult};
use crate::game_store::{GameRecord, GameResult, GameSite, GameStore, now_display, now_ms};
use crate::hotkeys::{self, Bindings};
use crate::overlay::board_overlay;
use crate::overlay::menu::{self, MenuTab};
use crate::vision::chrome_launcher;
use crate::vision::stability::{GateDecision, PuzzleGate};

// ── Engine command ────────────────────────────────────────────────────────────

pub enum EngineCommand {
    Analyse(String),
    AnalyseSpeculative(String),
    UpdateConfig(Config),
    Shutdown,
}

// ── Background → App messages ─────────────────────────────────────────────────

pub enum AppMessage {
    NewFen { fen: String, last_move: Option<String> },
    BoardRect(Rect),
    EngineStatus(String),
    PlayerSide(bool),
    PlayerNames { white: String, black: String },
    ClockUpdate { white: String, black: String },
    PageType(ChessPage),
    PuzzleReset,
    EngineResult(AnalysisResult),
    CdpConnected,
    GameIdle,
    GameResultDetected(String),
    ReviewProgress { id: u64, done: usize, total: usize },
    ReviewDone { id: u64, review: Box<crate::game_store::GameReview> },
    ReviewFailed { id: u64, error: String },
}

// ── Game-review progress ──────────────────────────────────────────────────────

pub struct ReviewProgress {
    pub id:    u64,
    pub done:  usize,
    pub total: usize,
}

// ── Win32 window state ────────────────────────────────────────────────────────

/// Typed state for the Win32 window setup state machine.
/// Replaces the `window_initialized: bool` + scattered if/else chains.
#[derive(Debug, Clone, Copy, PartialEq)]
enum WindowState {
    /// HWND not yet found — retry next frame.
    AwaitingHwnd,
    /// HWND found, DWM glass + ex-styles applied, ready for normal operation.
    Ready,
}

// ── App ───────────────────────────────────────────────────────────────────────

pub struct RustyChessApp {
    cfg: Config,

    // UI state
    menu_open:     bool,
    current_tab:   MenuTab,
    menu_pos:      egui::Pos2,
    board_flipped: bool,
    nav_selected:  Option<(String, String)>,

    // Window
    window_state:          WindowState,
    overlay_hwnd:          Option<isize>,
    last_style_enforce:    Option<Instant>,
    keyboard_focus_active: bool,
    overlay_origin:        egui::Vec2,
    current_monitor_rect:  Option<(i32, i32, i32, i32)>,
    last_monitor_check:    Option<Instant>,
    last_pixels_per_point: f32,
    /// True for one frame after a multi-monitor reposition to re-maximize
    /// the window on the new monitor without a second SetWindowPos call.
    overlay_remaximize_pending: bool,
    /// Cached last value sent for MousePassthrough — only re-send on change
    /// to avoid calling SetWindowLongW every frame via the viewport command.
    last_passthrough: Option<bool>,

    // Engine — unbounded channel so sends never silently drop positions
    engine_tx: Option<std::sync::mpsc::Sender<EngineCommand>>,

    // Analysis state
    analysis:         AnalysisResult,
    current_fen:      String,
    is_analysing:     bool,
    engine_status:    String,
    analysis_changed: bool,
    premove_fen:      String,
    smoothed_cp:      f32,

    // Board detection
    cdp_board_rect:      Option<Rect>,
    last_board_rect:     Option<Rect>,
    last_clocks:         Option<(String, String)>,

    // Channels
    rx: std::sync::mpsc::Receiver<AppMessage>,
    tx: std::sync::mpsc::Sender<AppMessage>,

    // Hotkeys
    bindings:   Bindings,
    key_states: [bool; 6],

    // Game info
    move_history:    Vec<String>,
    epd_history:     Vec<String>,
    current_opening: Option<(String, String)>,
    prev_fen:        String,
    prev_fullmove:   u32,
    player_white:    Option<String>,
    player_black:    Option<String>,
    game_time_white: Option<String>,
    game_time_black: Option<String>,
    current_page:    ChessPage,

    // Chrome launcher
    chrome_status:       String,
    chrome_status_timer: Option<Instant>,

    // Game history
    game_store:      GameStore,
    current_game_id: u64,
    pending_result:  Option<String>,
    expanded_game:   Option<u64>,
    reviewing:       Option<ReviewProgress>,
    review_cancel:   Arc<AtomicBool>,
    review_error:    Option<(u64, String)>,

    // Shared flags
    cdp_reconnect:  Arc<AtomicBool>,
    engine_restart: Arc<AtomicBool>,

    // Shutdown
    want_quit:     bool,
    bg_shutdown:   Arc<AtomicBool>,
    engine_handle: Option<std::thread::JoinHandle<()>>,
}

impl RustyChessApp {
    pub fn new(cfg: Config) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<AppMessage>();

        let cdp_reconnect  = Arc::new(AtomicBool::new(false));
        let engine_restart = Arc::new(AtomicBool::new(false));
        let bg_shutdown    = Arc::new(AtomicBool::new(false));

        // bg_thread
        let cfg_clone        = cfg.clone();
        let tx_clone         = tx.clone();
        let reconnect_flag   = cdp_reconnect.clone();
        let bg_shutdown_flag = bg_shutdown.clone();
        std::thread::spawn(move || {
            bg_thread(cfg_clone, tx_clone, reconnect_flag, bg_shutdown_flag)
        });

        // engine_thread — unbounded channel: send never blocks or drops
        let (eng_tx, eng_rx) = std::sync::mpsc::channel::<EngineCommand>();
        let cfg_eng          = cfg.clone();
        let tx_eng           = tx.clone();
        let eng_restart_flag = engine_restart.clone();
        let engine_handle = std::thread::spawn(move || {
            engine_thread(cfg_eng, eng_rx, tx_eng, eng_restart_flag)
        });

        let bindings = Bindings::from_config(&cfg.hotkeys);

        Self {
            cfg,
            menu_open:     true,
            current_tab:   MenuTab::Overview,
            menu_pos:      egui::pos2(20.0, 20.0),
            board_flipped: false,
            nav_selected:  None,

            window_state:          WindowState::AwaitingHwnd,
            overlay_hwnd:          None,
            last_style_enforce:    None,
            keyboard_focus_active: false,
            overlay_origin:        egui::Vec2::ZERO,
            current_monitor_rect:  None,
            last_monitor_check:    None,
            last_pixels_per_point: 1.0,
            overlay_remaximize_pending: false,
            last_passthrough: None,

            engine_tx:        Some(eng_tx),
            analysis:         AnalysisResult::default(),
            current_fen:      String::new(),
            is_analysing:     false,
            engine_status:    "starting…".into(),
            analysis_changed: false,
            premove_fen:      String::new(),
            smoothed_cp:      0.0,

            cdp_board_rect:  None,
            last_board_rect: None,
            last_clocks:     None,

            rx,
            tx,
            bindings,
            key_states: [false; 6],

            move_history:    Vec::new(),
            epd_history:     Vec::new(),
            current_opening: None,
            prev_fen:        String::new(),
            prev_fullmove:   0,
            player_white:    None,
            player_black:    None,
            game_time_white: None,
            game_time_black: None,
            current_page:    ChessPage::Unknown,

            chrome_status:       String::new(),
            chrome_status_timer: None,

            game_store:      GameStore::load(),
            current_game_id: now_ms(),
            pending_result:  None,
            expanded_game:   None,
            reviewing:       None,
            review_cancel:   Arc::new(AtomicBool::new(false)),
            review_error:    None,

            cdp_reconnect,
            engine_restart,
            want_quit:     false,
            bg_shutdown,
            engine_handle: Some(engine_handle),
        }
    }

    // ── Win32 window setup ────────────────────────────────────────────────────
    //
    // State machine: AwaitingHwnd → Ready
    //
    // AwaitingHwnd: FindWindowW every frame until the HWND appears (eframe
    //   creates it asynchronously).  Once found, apply DWM glass + all
    //   ex-styles immediately in the same call so the taskbar never sees
    //   the window without WS_EX_TOOLWINDOW.
    //
    // Ready: re-enforce styles at 150 ms (tight enough to beat the taskbar
    //   re-evaluation that eframe's MousePassthrough triggers), and re-fit
    //   the overlay to the monitor containing the board rect at 1 s.

    fn setup_window(&mut self, ctx: &egui::Context) {
        self.last_pixels_per_point = ctx.pixels_per_point().max(0.01);

        // Set transparent visuals once on the very first frame regardless of
        // whether the HWND is ready yet.
        if self.window_state == WindowState::AwaitingHwnd && self.overlay_hwnd.is_none() {
            let mut v = egui::Visuals::dark();
            v.panel_fill                     = egui::Color32::TRANSPARENT;
            v.window_fill                    = egui::Color32::TRANSPARENT;
            v.extreme_bg_color               = egui::Color32::TRANSPARENT;
            v.faint_bg_color                 = egui::Color32::TRANSPARENT;
            v.widgets.noninteractive.bg_fill = egui::Color32::TRANSPARENT;
            v.widgets.inactive.bg_fill       = egui::Color32::TRANSPARENT;
            ctx.set_visuals(v);
        }

        #[cfg(target_os = "windows")]
        self.setup_window_win32(ctx);

        #[cfg(not(target_os = "windows"))]
        { self.window_state = WindowState::Ready; }
    }

    #[cfg(target_os = "windows")]
    fn setup_window_win32(&mut self, ctx: &egui::Context) {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            FindWindowW, GetWindowLongW, SetWindowLongW, SetWindowPos,
            SetForegroundWindow, GWL_EXSTYLE, HWND_TOPMOST,
            SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_FRAMECHANGED,
            WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_NOACTIVATE, WS_EX_APPWINDOW,
        };

        // ── Resolve HWND (AwaitingHwnd state) ────────────────────────────────
        if self.overlay_hwnd.is_none() {
            let title: Vec<u16> = "RustyChess"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            unsafe {
                if let Ok(hwnd) = FindWindowW(
                    None,
                    windows::core::PCWSTR::from_raw(title.as_ptr()),
                ) {
                    if !hwnd.is_invalid() {
                        self.overlay_hwnd = Some(hwnd.0 as isize);
                    }
                }
            }
        }

        let hwnd_raw = match self.overlay_hwnd {
            Some(h) => h,
            None    => return, // still waiting — retry next frame
        };
        let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);

        // ── State machine ─────────────────────────────────────────────────────
        match self.window_state {
            WindowState::AwaitingHwnd => {
                // First time we have the HWND: apply everything at once.
                // Order matters:
                //   1. DWM glass sheet (must precede ex-style changes on some drivers)
                //   2. Ex-styles (TOOLWINDOW strips APPWINDOW from the taskbar)
                //   3. SetWindowPos TOPMOST + FRAMECHANGED (notifies shell)
                unsafe {
                    // 1. DWM glass — extends into client area for per-pixel alpha
                    use windows::Win32::Graphics::Dwm::DwmExtendFrameIntoClientArea;
                    use windows::Win32::UI::Controls::MARGINS;
                    let margins = MARGINS {
                        cxLeftWidth: -1, cxRightWidth: -1,
                        cyTopHeight: -1, cyBottomHeight: -1,
                    };
                    if let Err(e) = DwmExtendFrameIntoClientArea(hwnd, &margins) {
                        tracing::warn!("[app] DwmExtendFrameIntoClientArea failed: {e}");
                    }

                    // 2. Ex-styles applied immediately — taskbar evaluates
                    //    WS_EX_TOOLWINDOW at window creation; being late causes
                    //    the taskbar button to appear and require a click to dismiss.
                    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
                    let want = (ex
                        | WS_EX_LAYERED.0 as i32
                        | WS_EX_TOOLWINDOW.0 as i32
                        | WS_EX_NOACTIVATE.0 as i32)
                        & !(WS_EX_APPWINDOW.0 as i32);
                    SetWindowLongW(hwnd, GWL_EXSTYLE, want);

                    // 3. TOPMOST + FRAMECHANGED — forces the shell to re-evaluate
                    //    z-order and taskbar membership. SWP_NOMOVE | SWP_NOSIZE
                    //    so we don't touch the position that with_maximized(true)
                    //    already set correctly (touching it causes the white gap).
                    let _ = SetWindowPos(
                        hwnd, HWND_TOPMOST, 0, 0, 0, 0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                    );
                }

                // Reset throttle so the first re-enforce fires after 150 ms,
                // not immediately (we just set the styles).
                self.last_style_enforce = Some(Instant::now());
                self.window_state       = WindowState::Ready;
                tracing::info!("[app] window initialised — styles applied");
            }

            WindowState::Ready => {
                // ── Phase 2 of multi-monitor reposition ──────────────────────
                // After the previous frame moved the window via OuterPosition,
                // re-maximize here so the overlay covers the full monitor without
                // leaving a white strip at the bottom.
                if self.overlay_remaximize_pending {
                    self.overlay_remaximize_pending = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
                    // Fall through — allow style re-enforce etc. to run this frame.
                }

                // ── Keyboard focus handoff ────────────────────────────────────
                // WS_EX_NOACTIVATE blocks keyboard delivery to our window.
                // Drop it when egui has a focused TextEdit, restore when done.
                let egui_wants_kbd = ctx.memory(|m| m.focused().is_some());
                match (egui_wants_kbd, self.keyboard_focus_active) {
                    (true, false) => {
                        unsafe {
                            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
                            SetWindowLongW(hwnd, GWL_EXSTYLE,
                                ex & !(WS_EX_NOACTIVATE.0 as i32));
                            let _ = SetForegroundWindow(hwnd);
                        }
                        self.keyboard_focus_active = true;
                        self.last_style_enforce    = None; // re-enforce promptly on release
                    }
                    (false, true) => {
                        unsafe {
                            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
                            SetWindowLongW(hwnd, GWL_EXSTYLE,
                                ex | WS_EX_NOACTIVATE.0 as i32);
                        }
                        self.keyboard_focus_active = false;
                    }
                    _ => {}
                }

                // ── Monitor re-fit (1 s throttle) ────────────────────────────
                // Only move/resize when the board has migrated to a different
                // monitor. Uses rcMonitor (full rect including taskbar strip).
                let monitor_check_due = self.last_monitor_check
                    .map(|t| t.elapsed() >= Duration::from_millis(1000))
                    .unwrap_or(true);

                if monitor_check_due {
                    self.last_monitor_check = Some(Instant::now());
                    match self.target_monitor_rect() {
                        Some(target) if self.current_monitor_rect != Some(target) => {
                            let (mx, my, _mw, _mh) = target;
                            let ppp = self.last_pixels_per_point;

                            // Check whether the overlay window is already positioned
                            // on the target monitor.  If yes, just record the rect and
                            // skip the move — calling SetWindowPos with explicit coords
                            // removes WS_MAXIMIZE and leaves a white bar at the bottom.
                            let already_correct = unsafe {
                                use windows::Win32::Foundation::RECT;
                                use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
                                let mut wr = RECT::default();
                                GetWindowRect(hwnd, &mut wr).is_ok()
                                    && wr.left == mx && wr.top == my
                            };

                            if already_correct {
                                // Same monitor — nothing to move, just record.
                                self.overlay_origin       = egui::vec2(mx as f32 / ppp, my as f32 / ppp);
                                self.current_monitor_rect = Some(target);
                            } else {
                                // Genuinely different monitor: use egui viewport commands
                                // so the window is properly re-maximized there without
                                // an explicit-coords SetWindowPos causing a white bar.
                                let logical_x = mx as f32 / ppp;
                                let logical_y = my as f32 / ppp;
                                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(false));
                                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(
                                    egui::pos2(logical_x, logical_y),
                                ));
                                self.overlay_remaximize_pending = true;
                                self.overlay_origin             = egui::vec2(logical_x, logical_y);
                                self.current_monitor_rect       = Some(target);
                                tracing::info!(
                                    "[app] overlay moving to monitor origin=({mx},{my})"
                                );
                            }
                        }
                        _ => {}
                    }
                }

                // ── Style re-enforce (150 ms throttle) ───────────────────────
                // eframe's MousePassthrough viewport command rewrites ex-styles
                // every frame, clobbering WS_EX_TOOLWINDOW and re-adding
                // WS_EX_APPWINDOW.  We re-apply only when the styles have
                // actually drifted (guarded by the `want != ex` check) so
                // we're not calling SetWindowLongW every 150 ms unnecessarily.
                let enforce_due = self.last_style_enforce
                    .map(|t| t.elapsed() >= Duration::from_millis(150))
                    .unwrap_or(true);

                if enforce_due {
                    self.last_style_enforce = Some(Instant::now());
                    unsafe {
                        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
                        let noactivate = if self.keyboard_focus_active {
                            0
                        } else {
                            WS_EX_NOACTIVATE.0 as i32
                        };
                        let want = (ex
                            | WS_EX_LAYERED.0 as i32
                            | WS_EX_TOOLWINDOW.0 as i32
                            | noactivate)
                            & !(WS_EX_APPWINDOW.0 as i32);
                        if want != ex {
                            SetWindowLongW(hwnd, GWL_EXSTYLE, want);
                            let _ = SetWindowPos(
                                hwnd, HWND_TOPMOST, 0, 0, 0, 0,
                                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                            );
                            tracing::debug!("[app] ex-styles re-enforced (drifted)");
                        }
                    }
                }
            }
        }
    }

    /// Physical-pixel monitor rect `(x, y, w, h)` containing the chess board,
    /// or `None` if no board rect has been received yet.
    #[cfg(target_os = "windows")]
    fn target_monitor_rect(&self) -> Option<(i32, i32, i32, i32)> {
        use windows::Win32::Foundation::{POINT, RECT};
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        };

        let rect = self.cdp_board_rect?;
        let ppp  = self.last_pixels_per_point.max(0.01);
        let cx   = (rect.center().x * ppp) as i32;
        let cy   = (rect.center().y * ppp) as i32;

        unsafe {
            let hmon = MonitorFromPoint(POINT { x: cx, y: cy }, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(hmon, &mut info).as_bool() {
                let RECT { left, top, right, bottom } = info.rcMonitor;
                return Some((left, top, right - left, bottom - top));
            }
        }
        None
    }

    // ── Hotkeys ───────────────────────────────────────────────────────────────

    const KS_MENU:      usize = 0;
    const KS_FLIP:      usize = 1;
    const KS_DISCRETE:  usize = 2;
    const KS_OVERLAY:   usize = 3;
    const KS_EXIT:      usize = 4;
    const KS_RECONNECT: usize = 5;

    fn handle_hotkeys(&mut self) {
        let b = &self.bindings;
        if hotkeys::is_key_pressed(b.toggle_menu,     &mut self.key_states[Self::KS_MENU])      { self.menu_open = !self.menu_open; }
        if hotkeys::is_key_pressed(b.flip_board,      &mut self.key_states[Self::KS_FLIP])      { self.board_flipped = !self.board_flipped; }
        if hotkeys::is_key_pressed(b.toggle_discrete, &mut self.key_states[Self::KS_DISCRETE])  { self.cfg.analysis.discrete_mode = !self.cfg.analysis.discrete_mode; }
        if hotkeys::is_key_pressed(b.toggle_overlay,  &mut self.key_states[Self::KS_OVERLAY])   { self.cfg.analysis.overlay_enabled = !self.cfg.analysis.overlay_enabled; }
        if hotkeys::is_key_pressed(b.exit,            &mut self.key_states[Self::KS_EXIT])      { self.want_quit = true; }
        if hotkeys::is_key_pressed(b.reconnect_cdp,   &mut self.key_states[Self::KS_RECONNECT]) {
            self.cdp_reconnect.store(true, Ordering::Relaxed);
            self.cdp_board_rect  = None;
            self.last_board_rect = None;
            self.chrome_status   = "reconnecting…".into();
            self.chrome_status_timer = Some(Instant::now());
            tracing::info!("[app] manual CDP reconnect requested");
        }
    }

    // ── Thread shutdown ───────────────────────────────────────────────────────

    fn shutdown_threads(&mut self) {
        self.bg_shutdown.store(true, Ordering::Relaxed);
        if let Some(tx) = self.engine_tx.take() {
            // Unbounded channel — send always succeeds.
            let _ = tx.send(EngineCommand::Shutdown);
            // Drop tx so the channel disconnects and engine_thread exits cleanly.
        }
        if let Some(handle) = self.engine_handle.take() {
            let _ = handle.join();
        }
    }

    // ── Message drain ─────────────────────────────────────────────────────────

    fn drain_messages(&mut self) {
        perf_scope!("drain_messages");

        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                AppMessage::NewFen { fen, last_move } => {
                    if fen != self.current_fen {
                        self.on_new_fen(fen, last_move);
                    }
                }
                AppMessage::BoardRect(rect) => {
                    // Deduplicate — only update when the rect actually changed.
                    if self.last_board_rect != Some(rect) {
                        self.last_board_rect = Some(rect);
                        self.cdp_board_rect  = Some(rect);
                    }
                }
                AppMessage::EngineStatus(s) => {
                    self.engine_status = s;
                }
                AppMessage::PlayerSide(is_black) => {
                    if self.board_flipped != is_black {
                        self.board_flipped = is_black;
                        tracing::info!("[app] auto-flipped: playing_black={is_black}");
                    }
                }
                AppMessage::PlayerNames { white, black } => {
                    self.player_white = Some(white);
                    self.player_black = Some(black);
                }
                AppMessage::ClockUpdate { white, black } => {
                    // Deduplicate clocks — they arrive on every board push
                    // but rarely actually change between adjacent frames.
                    let pair = (white.clone(), black.clone());
                    if self.last_clocks.as_ref() != Some(&pair) {
                        self.last_clocks     = Some(pair);
                        self.game_time_white = Some(white);
                        self.game_time_black = Some(black);
                    }
                }
                AppMessage::PageType(page) => {
                    self.current_page = page;
                }
                AppMessage::PuzzleReset => {
                    tracing::info!("[app] puzzle reset — clearing history and analysis");
                    // Commit any in-progress game before wiping state, so a
                    // browser refresh mid-game doesn't permanently discard moves.
                    let pending = self.pending_result.clone();
                    self.commit_game(pending);
                    self.reset_game_state();
                    self.current_fen  = String::new();
                    self.prev_fullmove = 0;
                }
                AppMessage::EngineResult(result) => {
                    self.apply_engine_result(result);
                }
                AppMessage::CdpConnected => {
                    if self.chrome_status == "reconnecting…" {
                        self.chrome_status       = "reconnected ✓".into();
                        self.chrome_status_timer = Some(Instant::now());
                    }
                }
                AppMessage::GameIdle => {
                    self.commit_game(None);
                }
                AppMessage::GameResultDetected(result_str) => {
                    self.pending_result = Some(result_str.clone());
                    self.commit_game(Some(result_str));
                    if self.is_analysing {
                        tracing::info!("[app] game result confirmed — stopping engine");
                        self.is_analysing  = false;
                        self.engine_status = "idle (game ended)".into();
                    }
                }
                AppMessage::ReviewProgress { id, done, total } => {
                    self.reviewing = Some(ReviewProgress { id, done, total });
                }
                AppMessage::ReviewDone { id, review } => {
                    if let Some(g) = self.game_store.games.iter_mut().find(|g| g.id == id) {
                        g.review = Some(*review);
                        self.game_store.save();
                    }
                    if self.reviewing.as_ref().map(|r| r.id) == Some(id) {
                        self.reviewing = None;
                    }
                    self.review_error = None;
                }
                AppMessage::ReviewFailed { id, error } => {
                    tracing::warn!("[app] game review failed for {id}: {error}");
                    if self.reviewing.as_ref().map(|r| r.id) == Some(id) {
                        self.reviewing = None;
                    }
                    self.review_error = Some((id, error));
                }
            }
        }
    }

    // ── Engine result ─────────────────────────────────────────────────────────

    fn apply_engine_result(&mut self, result: AnalysisResult) {
        if result.lines.is_empty() { return; }

        let raw_cp = result.lines.first()
            .map(|l| l.centipawns as f32)
            .unwrap_or(self.smoothed_cp);
        self.smoothed_cp += (raw_cp - self.smoothed_cp) * EVAL_SMOOTHING;

        let prev_best  = self.analysis.best_move.clone();
        let prev_score = self.analysis.lines.first().map(|l| l.centipawns);
        self.analysis  = result;
        let new_score  = self.analysis.lines.first().map(|l| l.centipawns);

        let score_delta = match (prev_score, new_score) {
            (Some(a), Some(b)) => (a - b).abs(),
            _                  => 999,
        };
        if self.analysis.best_move != prev_best || score_delta > SCORE_CHANGE_THRESHOLD {
            self.analysis_changed = true;
        }
    }

    // ── Game state reset ──────────────────────────────────────────────────────

    fn reset_game_state(&mut self) {
        self.move_history.clear();
        self.epd_history.clear();
        self.current_opening = None;
        self.prev_fen        = String::new();
        self.analysis        = AnalysisResult::default();
        self.premove_fen     = String::new();
        self.smoothed_cp     = 0.0;
        self.current_game_id = now_ms();
        self.pending_result  = None;
        self.last_clocks     = None;
        self.last_board_rect = None;
        self.prev_fullmove   = 0;
    }

    // ── Game commit ───────────────────────────────────────────────────────────

    fn commit_game(&mut self, result_str: Option<String>) {
        if self.move_history.len() < 4 { return; }
        let site = match self.current_page {
            ChessPage::LichessGame | ChessPage::LichessPuzzle => GameSite::Lichess,
            ChessPage::Unknown                                 => GameSite::Unknown,
            _                                                  => GameSite::ChessCom,
        };
        let result = result_str
            .as_deref()
            .map(|s| {
                let mut r = GameResult::from_dom(s);
                if r == GameResult::Unknown {
                    let lower    = s.to_ascii_lowercase();
                    let white_lc = self.player_white.as_deref().unwrap_or("").to_ascii_lowercase();
                    let black_lc = self.player_black.as_deref().unwrap_or("").to_ascii_lowercase();
                    let white_won  = !white_lc.is_empty() && (lower.contains(&format!("{white_lc} won")) || lower.contains(&format!("{white_lc} wins")));
                    let black_won  = !black_lc.is_empty() && (lower.contains(&format!("{black_lc} won")) || lower.contains(&format!("{black_lc} wins")));
                    let white_lost = !white_lc.is_empty() && (lower.contains(&format!("{white_lc} resigned")) || lower.contains(&format!("{white_lc} ran out")) || lower.contains(&format!("{white_lc} abandoned")) || lower.contains(&format!("{white_lc} disconnected")));
                    let black_lost = !black_lc.is_empty() && (lower.contains(&format!("{black_lc} resigned")) || lower.contains(&format!("{black_lc} ran out")) || lower.contains(&format!("{black_lc} abandoned")) || lower.contains(&format!("{black_lc} disconnected")));
                    match (white_won || black_lost, black_won || white_lost) {
                        (true, _) => r = GameResult::WhiteWins,
                        (_, true) => r = GameResult::BlackWins,
                        _         => {}
                    }
                }
                r
            })
            .unwrap_or(GameResult::Unknown);

        self.game_store.commit(GameRecord {
            id:        self.current_game_id,
            site,
            white:     self.player_white.clone().unwrap_or_else(|| "?".into()),
            black:     self.player_black.clone().unwrap_or_else(|| "?".into()),
            result,
            opening:   self.current_opening.clone(),
            moves:     self.move_history.clone(),
            played_at: now_display(),
            review:    None,
        });
    }

    // ── Game review ───────────────────────────────────────────────────────────

    fn start_review(&mut self, id: u64) {
        if self.reviewing.as_ref().map(|r| r.id) == Some(id) { return; }
        let Some(game) = self.game_store.games.iter().find(|g| g.id == id) else { return; };

        self.review_cancel.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.review_cancel = cancel.clone();

        let moves       = game.moves.clone();
        let tx          = self.tx.clone();
        let engine_path = self.cfg.engine.path.clone();
        let depth       = self.cfg.analysis.review_depth.max(1);
        let hash_mb     = self.cfg.engine.hash_mb;

        self.reviewing    = Some(ReviewProgress { id, done: 0, total: moves.len() + 1 });
        self.review_error = None;

        std::thread::spawn(move || {
            let tx_progress = tx.clone();
            let result = crate::engine::review::review_game(
                &moves, &engine_path, depth, hash_mb, cancel.clone(),
                |done, total| { let _ = tx_progress.send(AppMessage::ReviewProgress { id, done, total }); },
            );
            match result {
                Ok(review) => { let _ = tx.send(AppMessage::ReviewDone { id, review: Box::new(review) }); }
                Err(e) => {
                    if !cancel.load(Ordering::Relaxed) {
                        let _ = tx.send(AppMessage::ReviewFailed { id, error: e.to_string() });
                    }
                }
            }
        });
    }

    // ── FEN handling ──────────────────────────────────────────────────────────

    fn on_new_fen(&mut self, fen: String, last_move: Option<String>) {
        let parts: Vec<&str> = fen.split_whitespace().collect();
        let side_to_move     = parts.get(1).copied().unwrap_or("w");
        let fullmove: u32    = parts.get(5).and_then(|s| s.parse().ok()).unwrap_or(1);

        // Puzzle / racer pages (training, storm, racer, ChessComAnalysis, …) send
        // NewFen for engine analysis only — they must never trigger game recording.
        // Without this guard every new puzzle position would be appended to
        // move_history and committed as a "game fragment" on PuzzleReset.
        let is_puzzle_page = self.current_page.is_puzzle();

        if !is_puzzle_page {
            if fullmove == 1 && side_to_move == "w" && self.prev_fullmove > 1 {
                tracing::info!("[app] new game detected — committing previous and resetting");
                self.commit_game(self.pending_result.clone());
                self.reset_game_state();
                self.player_white    = None;
                self.player_black    = None;
                self.game_time_white = None;
                self.game_time_black = None;
            }
            self.prev_fullmove = fullmove;
            self.record_fen_change(&fen, last_move.as_deref());
        }

        self.current_fen = fen.clone();

        let epd = crate::chess::openings::fen_to_epd(&fen).to_string();
        self.epd_history.push(epd);
        self.current_opening = self.epd_history.iter().rev()
            .find_map(|e| crate::chess::openings::lookup(e)
                .map(|o| (o.eco.clone(), o.name.clone())));

        self.start_analysis(&fen);
        self.schedule_premove_analysis(&fen, side_to_move);
    }

    fn schedule_premove_analysis(&mut self, fen: &str, side_to_move: &str) {
        let our_side = if self.board_flipped { "b" } else { "w" };
        if side_to_move != our_side { return; }
        let Some(best_uci) = self.analysis.best_move.clone() else { return };
        let Some(opp_fen)  = apply_uci_to_fen(fen, &best_uci) else { return };
        if opp_fen == self.premove_fen { return; }
        self.premove_fen = opp_fen.clone();
        if let Some(tx) = &self.engine_tx {
            let _ = tx.send(EngineCommand::AnalyseSpeculative(opp_fen));
        }
    }

    fn record_fen_change(&mut self, new_fen: &str, last_move: Option<&str>) {
        if self.prev_fen.is_empty() {
            self.prev_fen = new_fen.to_string();
            return;
        }
        let prev_parts: Vec<&str> = self.prev_fen.split_whitespace().collect();
        let side = prev_parts.get(1).copied().unwrap_or("?");
        let mnum = prev_parts.get(5).copied().unwrap_or("1");
        let mv   = last_move
            .map(str::to_string)
            .or_else(|| self.analysis.best_move.clone())
            .unwrap_or_else(|| "???".to_string());
        let entry = if side == "w" {
            format!("{mnum}. {mv}")
        } else {
            format!("{mnum}… {mv}")
        };
        self.move_history.push(entry);
        if self.move_history.len() > 600 { self.move_history.drain(0..20); }
        self.prev_fen = new_fen.to_string();
    }

    fn start_analysis(&mut self, fen: &str) {
        if let Some(tx) = &self.engine_tx {
            // Unbounded channel — never drops.
            let _ = tx.send(EngineCommand::Analyse(fen.to_string()));
            self.is_analysing  = true;
            self.engine_status = "analysing".into();
        }
    }

    // ── Chrome launcher ───────────────────────────────────────────────────────

    pub fn launch_chrome(&mut self, target_url: Option<&str>) {
        let result = chrome_launcher::launch_chrome(
            &self.cfg.cdp.chrome_path,
            &self.cfg.cdp.endpoint,
            &self.cfg.cdp.chrome_extra_args,
            target_url,
        );
        self.chrome_status = match result {
            chrome_launcher::LaunchResult::Launched       => "Chrome launched ✓".into(),
            chrome_launcher::LaunchResult::AlreadyRunning => "Already running".into(),
            chrome_launcher::LaunchResult::Failed(e)      => format!("Failed: {e}"),
        };
        self.chrome_status_timer = Some(Instant::now());
    }
}

// ── eframe::App ───────────────────────────────────────────────────────────────

impl eframe::App for RustyChessApp {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.commit_game(self.pending_result.clone());
        self.shutdown_threads();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        perf_scope!("app_update");
        #[cfg(feature = "tracy")]
        tracy_client::frame_mark();

        let frame_start = Instant::now();
        self.drain_messages();
        self.handle_hotkeys();
        self.setup_window(ctx);

        // ── Quit ──────────────────────────────────────────────────────────────
        if self.want_quit {
            self.commit_game(self.pending_result.clone());
            self.shutdown_threads();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        // Expire chrome status after 4 s
        if let Some(t) = self.chrome_status_timer {
            if t.elapsed() > Duration::from_secs(4) {
                self.chrome_status.clear();
                self.chrome_status_timer = None;
            }
        }

        // ── Mouse passthrough ─────────────────────────────────────────────────
        // Only issue the viewport command when the value actually changes.
        // Without this guard eframe calls SetWindowLongW (Win32) every frame,
        // which prompts DWM to re-evaluate the window and raises GPU usage.
        let want_passthrough = !self.menu_open;
        if self.last_passthrough != Some(want_passthrough) {
            self.last_passthrough = Some(want_passthrough);
            ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(want_passthrough));
        }

        // ── Repaint budget ────────────────────────────────────────────────────
        // Snapshot analysis_changed BEFORE clearing — a result arriving in
        // drain_messages() would otherwise be invisible to the fps selector.
        let has_new_analysis  = self.analysis_changed;
        self.analysis_changed = false;

        // With vsync on the GPU renders one frame per display interval.
        // Aggressive idle rates = DWM reuses the cached overlay texture for
        // most of its 60 Hz composites → near-zero GPU use when idle.
        let hint_mode   = self.cfg.analysis.hint_mode;
        let hint_active = !hint_mode || hotkeys::is_key_down(self.bindings.hint_hold);
        let hint_held   = hint_mode && hint_active;

        let fps: u32 = match (self.menu_open, has_new_analysis, hint_held) {
            (true, _, _)           => 20,  // menu open: smooth interaction
            (_, _, true)           => 20,  // hint key held: smooth overlay
            (false, true, false)   => 10,  // new engine result arriving
            // 15 Hz idle: hotkeys are polled every ~67 ms — fast enough to
            // catch any normal key press (human dwell time ≥ 100 ms) while
            // still keeping GPU load negligible (DWM reuses the overlay texture).
            (false, false, false)  => 15,
        };
        tracing::debug!(
            "[app] fps={fps} frame={:.2}ms",
            frame_start.elapsed().as_secs_f64() * 1000.0
        );
        ctx.request_repaint_after(Duration::from_millis(1000 / fps as u64));

        // When hint mode is enabled but key is not held, poll at 10 Hz to
        // catch the key press quickly without burning GPU at 30 FPS.
        if hint_mode && !hint_held {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        // ── Board overlay ─────────────────────────────────────────────────────

        if hint_active && (self.cfg.analysis.overlay_enabled || self.cfg.analysis.show_eval_bar) {
            perf_scope!("overlay_render");
            if let Some(rect) = board_overlay::resolve_board_rect(self.cdp_board_rect) {
                let rect    = rect.translate(-self.overlay_origin);
                let painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Background,
                    egui::Id::new("board_overlay"),
                ));

                if self.cfg.analysis.show_eval_bar {
                    board_overlay::draw_eval_bar(
                        &painter, rect, self.smoothed_cp, self.board_flipped,
                    );
                }

                if self.cfg.analysis.overlay_enabled {
                    match (hint_mode || self.cfg.analysis.discrete_mode, ()) {
                        (true, _) => board_overlay::draw_discrete_indicator(
                            &painter, &self.analysis, rect, self.board_flipped,
                        ),
                        _ => board_overlay::draw_board_highlights(
                            &painter, &self.analysis, rect, self.board_flipped,
                            self.cfg.analysis.display_lines,
                        ),
                    }
                }

                // Opening name label
                if self.cfg.analysis.show_opening_name {
                    if let Some((eco, name)) = &self.current_opening {
                        let label = format!("{eco}  {name}");
                        let font  = egui::FontId::proportional(12.0);

                        let is_lichess = matches!(
                            self.current_page,
                            ChessPage::LichessGame | ChessPage::LichessPuzzle
                        );
                        let (pos, align) = if is_lichess {
                            (egui::pos2(rect.max.x, rect.min.y - 6.0), egui::Align2::RIGHT_BOTTOM)
                        } else {
                            (egui::pos2(rect.center().x, rect.max.y + 6.0), egui::Align2::CENTER_TOP)
                        };

                        let galley    = painter.layout_no_wrap(label.clone(), font.clone(), egui::Color32::WHITE);
                        let text_size = galley.size();
                        let pad       = egui::vec2(8.0, 3.0);
                        let text_rect = if is_lichess {
                            egui::Rect::from_min_size(
                                egui::pos2(pos.x - text_size.x, pos.y - text_size.y), text_size)
                        } else {
                            egui::Rect::from_min_size(
                                egui::pos2(pos.x - text_size.x * 0.5, pos.y), text_size)
                        };
                        painter.rect_filled(text_rect.expand2(pad), 4.0,
                            egui::Color32::from_black_alpha(160));
                        painter.text(pos, align, &label, font,
                            egui::Color32::from_rgba_unmultiplied(200, 220, 255, 230));
                    }
                }
            }
        }

        // ── HUD menu ──────────────────────────────────────────────────────────
        if self.menu_open {
            perf_scope!("menu_render");
            let mut cfg     = self.cfg.clone();
            let mut flipped = self.board_flipped;

            let mut mctx = menu::MenuContext {
                config:          &mut cfg,
                analysis:        &self.analysis,
                is_analysing:    self.is_analysing,
                flipped:         &mut flipped,
                engine_status:   &self.engine_status,
                current_page:    self.current_page,
                move_history:    &self.move_history,
                player_white:    self.player_white.as_deref(),
                player_black:    self.player_black.as_deref(),
                game_time_white: self.game_time_white.as_deref(),
                game_time_black: self.game_time_black.as_deref(),
                chrome_status:   &self.chrome_status,
                nav_selected:    &mut self.nav_selected,
                current_opening: self.current_opening.as_ref().map(|(e, n)| (e.as_str(), n.as_str())),
                game_store:      &mut self.game_store,
                expanded_game:   &mut self.expanded_game,
                reviewing:       self.reviewing.as_ref().map(|r| (r.id, r.done, r.total)),
                review_error:    self.review_error.as_ref().map(|(id, e)| (*id, e.as_str())),
                commands:        Vec::new(),
            };

            menu::render_menu(ctx, &mut self.menu_pos, &mut self.current_tab, &mut mctx);

            let commands = std::mem::take(&mut mctx.commands);
            drop(mctx);

            self.board_flipped = flipped;

            for cmd in commands {
                match cmd {
                    menu::MenuCommand::LaunchUrl(url) => {
                        let is_game_url = url.as_deref()
                            .map(|u| u.contains("chess.com") || u.contains("lichess.org"))
                            .unwrap_or(false);
                        if is_game_url {
                            self.launch_chrome(url.as_deref());
                            self.cdp_reconnect.store(true, Ordering::Relaxed);
                            self.cdp_board_rect  = None;
                            self.last_board_rect = None;
                        } else if let Some(u) = url {
                            use std::os::windows::process::CommandExt;
                            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                            let _ = std::process::Command::new("cmd")
                                .args(["/c", "start", "", &u])
                                .creation_flags(CREATE_NO_WINDOW)
                                .spawn();
                        }
                    }
                    menu::MenuCommand::ReconnectCdp => {
                        self.cdp_reconnect.store(true, Ordering::Relaxed);
                        self.cdp_board_rect      = None;
                        self.last_board_rect     = None;
                        self.chrome_status       = "reconnecting…".into();
                        self.chrome_status_timer = Some(Instant::now());
                        tracing::info!("[app] manual CDP reconnect requested");
                    }
                    menu::MenuCommand::RestartEngine => {
                        self.engine_restart.store(true, Ordering::Relaxed);
                        self.engine_status = "restarting…".into();
                        tracing::info!("[app] manual engine restart requested");
                    }
                    menu::MenuCommand::AnalyseGame(id) => {
                        tracing::info!("[app] game review requested for {id}");
                        self.start_review(id);
                    }
                    menu::MenuCommand::Quit => {
                        self.want_quit = true;
                    }
                }
            }

            apply_config_changes(self, cfg);
        }
    }
}

// ── Config change detection ───────────────────────────────────────────────────

fn apply_config_changes(app: &mut RustyChessApp, new_cfg: Config) {
    let hk_changed = new_cfg.hotkeys.toggle_menu     != app.cfg.hotkeys.toggle_menu
        || new_cfg.hotkeys.flip_board      != app.cfg.hotkeys.flip_board
        || new_cfg.hotkeys.toggle_discrete != app.cfg.hotkeys.toggle_discrete
        || new_cfg.hotkeys.toggle_overlay  != app.cfg.hotkeys.toggle_overlay
        || new_cfg.hotkeys.exit            != app.cfg.hotkeys.exit
        || new_cfg.hotkeys.hint_hold       != app.cfg.hotkeys.hint_hold;

    if hk_changed {
        app.bindings = Bindings::from_config(&new_cfg.hotkeys);
    }

    let engine_changed = new_cfg.engine.path         != app.cfg.engine.path
        || new_cfg.engine.hash_mb     != app.cfg.engine.hash_mb
        || new_cfg.engine.threads     != app.cfg.engine.threads
        || new_cfg.engine.skill_level != app.cfg.engine.skill_level
        || new_cfg.analysis.multipv   != app.cfg.analysis.multipv
        || new_cfg.analysis.depth     != app.cfg.analysis.depth
        || new_cfg.analysis.nodes     != app.cfg.analysis.nodes;

    let any_changed = hk_changed || engine_changed
        || new_cfg.analysis.discrete_mode   != app.cfg.analysis.discrete_mode
        || new_cfg.analysis.overlay_enabled != app.cfg.analysis.overlay_enabled
        || new_cfg.analysis.show_eval_bar   != app.cfg.analysis.show_eval_bar
        || new_cfg.analysis.hint_mode       != app.cfg.analysis.hint_mode
        || new_cfg.username                 != app.cfg.username
        || new_cfg.cdp.poll_interval_ms     != app.cfg.cdp.poll_interval_ms
        || new_cfg.cdp.chrome_path          != app.cfg.cdp.chrome_path;

    app.cfg = new_cfg;
    if any_changed { let _ = app.cfg.save(); }

    if engine_changed {
        if let Some(tx) = &app.engine_tx {
            let _ = tx.send(EngineCommand::UpdateConfig(app.cfg.clone()));
        }
        app.engine_status = "restarting…".into();
        tracing::info!("[app] engine settings changed — auto-restarting engine");
    }
}

// ── Engine thread ─────────────────────────────────────────────────────────────

fn engine_thread(
    cfg:          Config,
    cmd_rx:       std::sync::mpsc::Receiver<EngineCommand>,
    tx:           std::sync::mpsc::Sender<AppMessage>,
    restart_flag: Arc<AtomicBool>,
) {
    #[cfg(feature = "tracy")]
    tracy_client::set_thread_name!("engine");

    let mut cfg = cfg;
    let spawn_engine = |cfg: &Config, tx: &std::sync::mpsc::Sender<AppMessage>| {
        match AnalysisEngine::new(
            &cfg.engine.path,
            cfg.analysis.multipv,
            cfg.analysis.depth,
            cfg.analysis.nodes,
        ) {
            Ok(mut e) => {
                let _ = e.set_hash(cfg.engine.hash_mb);
                let _ = e.set_threads(cfg.engine.threads);
                let _ = e.set_skill_level(cfg.engine.skill_level);
                let _ = tx.send(AppMessage::EngineStatus("ready".into()));
                Some(e)
            }
            Err(e) => {
                tracing::error!("[engine] failed to start: {e}");
                let _ = tx.send(AppMessage::EngineStatus(format!("error: {e}")));
                None
            }
        }
    };

    let Some(mut eng) = spawn_engine(&cfg, &tx) else { return };

    let mut current_fen:      String                       = String::new();
    let mut analysing:        bool                         = false;
    let mut aggressive_until: Option<std::time::Instant>  = None;
    let mut respawn_backoff:  Duration                     = Duration::from_millis(500);
    let mut last_fen:         String                       = String::new();

    loop {
        // ── Drain command queue ───────────────────────────────────────────────
        // Primary Analyse beats speculative. All variants matched explicitly
        // so the compiler flags any unhandled future variants.
        let mut next_fen:       Option<String> = None;
        let mut spec_fen:       Option<String> = None;
        let mut config_updated: bool           = false;

        loop {
            match cmd_rx.try_recv() {
                Ok(EngineCommand::Analyse(fen)) => {
                    next_fen = Some(fen);
                    spec_fen = None; // primary beats speculative
                }
                Ok(EngineCommand::AnalyseSpeculative(fen)) => {
                    if next_fen.is_none() { spec_fen = Some(fen); }
                }
                Ok(EngineCommand::UpdateConfig(new_cfg)) => {
                    cfg            = new_cfg;
                    config_updated = true;
                }
                Ok(EngineCommand::Shutdown) => {
                    tracing::info!("[engine] shutdown — stopping");
                    return; // Drop eng → UciEngine::Drop sends quit
                }
                Err(std::sync::mpsc::TryRecvError::Empty)        => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    tracing::info!("[engine] channel closed — stopping");
                    return;
                }
            }
        }

        // ── Config update → respawn ───────────────────────────────────────────
        if config_updated {
            let _ = tx.send(AppMessage::EngineStatus("restarting…".into()));
            match spawn_engine(&cfg, &tx) {
                Some(new_eng) => {
                    eng             = new_eng;
                    respawn_backoff = Duration::from_millis(500);
                    current_fen.clear();
                    analysing = false;
                    tracing::info!("[engine] config updated — respawned");
                }
                None => tracing::error!("[engine] config update respawn failed"),
            }
        }

        // ── New position ──────────────────────────────────────────────────────
        if let Some(fen) = next_fen.or(spec_fen) {
            if fen != current_fen {
                current_fen = fen.clone();
                last_fen    = fen.clone();

                if let Err(e) = eng.update_position(&fen) {
                    tracing::warn!("[engine] update_position: {e} — respawning");
                    let _ = tx.send(AppMessage::EngineStatus("respawning…".into()));
                    std::thread::sleep(respawn_backoff);

                    match spawn_engine(&cfg, &tx) {
                        Some(new_eng) => {
                            eng             = new_eng;
                            respawn_backoff = Duration::from_millis(500);
                            if let Err(e2) = eng.update_position(&current_fen) {
                                tracing::error!("[engine] post-respawn position failed: {e2}");
                            }
                        }
                        None => {
                            respawn_backoff = (respawn_backoff * 2).min(Duration::from_secs(8));
                            current_fen.clear();
                            continue;
                        }
                    }
                }

                analysing        = true;
                aggressive_until = Some(Instant::now() + Duration::from_millis(500));
            }
        }

        // ── Poll results / crash detection ────────────────────────────────────
        if analysing && !current_fen.is_empty() {
            if eng.is_dead() {
                tracing::warn!("[engine] silent crash — respawning");
                let _ = tx.send(AppMessage::EngineStatus("respawning…".into()));
                std::thread::sleep(respawn_backoff);
                match spawn_engine(&cfg, &tx) {
                    Some(new_eng) => {
                        eng             = new_eng;
                        respawn_backoff = Duration::from_millis(500);
                        if !last_fen.is_empty() {
                            if let Err(e) = eng.update_position(&last_fen) {
                                tracing::error!("[engine] post-silent-respawn failed: {e}");
                            }
                        }
                    } 
                    None => {
                        respawn_backoff = (respawn_backoff * 2).min(Duration::from_secs(8));
                        analysing = false;
                    }
                }
                continue;
            }

            let result = eng.poll();
            if !result.lines.is_empty() {
                if tx.send(AppMessage::EngineResult(result)).is_err() { break; }
            }
        }

        // ── Manual restart ────────────────────────────────────────────────────
        if restart_flag.swap(false, Ordering::Relaxed) {
            let _ = tx.send(AppMessage::EngineStatus("restarting…".into()));
            match spawn_engine(&cfg, &tx) {
                Some(new_eng) => {
                    eng             = new_eng;
                    respawn_backoff = Duration::from_millis(500);
                    current_fen.clear();
                    analysing = false;
                    tracing::info!("[engine] manual restart completed");
                }
                None => tracing::error!("[engine] manual restart failed"),
            }
        }

        // ── Adaptive sleep ────────────────────────────────────────────────────
        // 10 ms aggressive window (first 500 ms after new position so we get
        // quick initial results), 20 ms otherwise (was 80 ms — halves latency).
        let sleep_ms = match aggressive_until {
            Some(t) if Instant::now() < t => 10,
            _ => { aggressive_until = None; 20 }
        };
        std::thread::sleep(Duration::from_millis(sleep_ms));
    }
}

// ── UCI helper ────────────────────────────────────────────────────────────────

fn apply_uci_to_fen(fen: &str, uci: &str) -> Option<String> {
    let mut board = crate::chess::Board::from_fen(fen).ok()?;
    board.apply_uci(uci).ok()?;
    Some(board.fen())
}

// ── Background (CDP) thread ───────────────────────────────────────────────────

fn bg_thread(
    config:         Config,
    tx:             std::sync::mpsc::Sender<AppMessage>,
    reconnect_flag: Arc<AtomicBool>,
    shutdown_flag:  Arc<AtomicBool>,
) {
    #[cfg(feature = "tracy")]
    tracy_client::set_thread_name!("CDP");

    tracing::info!("[bg] started");
    std::thread::sleep(Duration::from_millis(500));

    let mut last_side:         Option<bool>             = None;
    let mut last_names:        Option<(String, String)> = None;
    let mut seen_nonstart_fen: bool                     = false;
    // Consecutive start-FEN event count. Resets to 0 on any non-start FEN.
    // We require ≥2 consecutive start-FEN snapshots before forwarding to the
    // app thread, so a single empty-move-list glitch from the DOM observer
    // can never trigger a false new-game reset and wipe move_history.
    let mut start_fen_streak:  u8                       = 0;
    let mut puzzle_gate        = PuzzleGate::new();
    let mut last_page:         ChessPage                = ChessPage::Unknown;
    let mut last_engine_send   = Instant::now() - Duration::from_secs(10);
    let mut last_fen_change    = Instant::now();
    let mut game_idle_sent     = false;
    let mut last_live_fen      = String::new();
    let mut game_result_sent   = false;

    use crate::vision::cdp_chesscom::{CdpConnection as ChessComConn, CdpEvent};
    use crate::vision::cdp_lichess::LichessConnection;

    enum CdpConn { ChessCom(ChessComConn), Lichess(LichessConnection) }
    impl CdpConn {
        fn next_event(&mut self) -> anyhow::Result<CdpEvent> {
            match self {
                CdpConn::ChessCom(c) => c.next_event(),
                CdpConn::Lichess(c)  => c.next_event(),
            }
        }
        fn target_changed(&self, endpoint: &str) -> bool {
            match self {
                CdpConn::ChessCom(c) => c.target_changed(endpoint),
                CdpConn::Lichess(c)  => c.target_changed(endpoint),
            }
        }
    }

    let mut cdp_conn:         Option<CdpConn> = None;
    let mut cdp_error_streak: u32             = 0;
    let mut last_target_check = Instant::now() - Duration::from_secs(10);

    loop {
        perf_scope!("bg_loop");

        // ── Shutdown ──────────────────────────────────────────────────────────
        if shutdown_flag.load(Ordering::Relaxed) {
            tracing::info!("[bg] shutdown — returning");
            return;
        }

        // ── Manual reconnect ──────────────────────────────────────────────────
        if reconnect_flag.swap(false, Ordering::Relaxed) {
            cdp_conn         = None;
            cdp_error_streak = 0;
            last_side        = None;
            last_names       = None;
            seen_nonstart_fen = false;
            start_fen_streak  = 0;
            puzzle_gate.reset();
            last_engine_send  = Instant::now() - Duration::from_secs(10);
            last_fen_change   = Instant::now();
            game_idle_sent    = false;
            last_live_fen.clear();
            game_result_sent  = false;
            tracing::info!("[bg] manual reconnect — connection dropped");
        }

        // ── Ensure connection ─────────────────────────────────────────────────
        if cdp_conn.is_none() {
            cdp_conn = ChessComConn::connect(&config.cdp.endpoint)
                .map(CdpConn::ChessCom)
                .or_else(|| LichessConnection::connect(&config.cdp.endpoint).map(CdpConn::Lichess));

            match &cdp_conn {
                None => {
                    std::thread::sleep(Duration::from_millis(
                        (500 * (cdp_error_streak + 1)).min(3000) as u64,
                    ));
                    cdp_error_streak += 1;
                    continue;
                }
                Some(_) => {
                    cdp_error_streak = 0;
                    tracing::info!("[bg] CDP connection established");
                    let _ = tx.send(AppMessage::CdpConnected);
                }
            }
        }

        // Invariant: cdp_conn is Some — the is_none() block above continues if it isn't.
        let Some(conn) = cdp_conn.as_mut() else { continue };
        match conn.next_event() {
            Err(e) => {
                tracing::debug!("[bg] event error: {e} — reconnecting");
                cdp_conn         = None;
                cdp_error_streak += 1;
                std::thread::sleep(Duration::from_millis(200));
            }

            Ok(CdpEvent::PageNavigated) => {
                tracing::info!("[bg] page navigated — resetting state");
                last_side         = None;
                last_names        = None;
                seen_nonstart_fen = false;
                start_fen_streak  = 0;
                puzzle_gate.reset();
                last_engine_send  = Instant::now() - Duration::from_secs(10);
                last_fen_change   = Instant::now();
                game_idle_sent    = false;
                last_live_fen.clear();
                game_result_sent  = false;
                let _ = tx.send(AppMessage::PuzzleReset);
            }

            Ok(CdpEvent::BoardState(snap)) => {
                let state = crate::vision::snapshot_to_board_state(snap);

                // Page type
                if state.page != last_page {
                    last_page = state.page;
                    let _ = tx.send(AppMessage::PageType(state.page));
                }

                match state.is_puzzle() {
                    true => {
                        // ── Puzzle branch ─────────────────────────────────────
                        let current = state.fen.as_str();
                        if let GateDecision::Ready { is_new_puzzle } = puzzle_gate.observe(current) {
                            if last_engine_send.elapsed() >= Duration::from_millis(300) {
                                if is_new_puzzle {
                                    let _ = tx.send(AppMessage::PuzzleReset);
                                }
                                puzzle_gate.commit(current, is_new_puzzle);
                                last_engine_send = Instant::now();
                                last_fen_change  = Instant::now();
                                game_idle_sent   = false;
                                if tx.send(AppMessage::NewFen {
                                    fen: state.fen.clone(), last_move: None,
                                }).is_err() { break; }
                            }
                        }
                        if let Some(rect) = state.board_rect {
                            let _ = tx.send(AppMessage::BoardRect(rect));
                        }
                        if last_side != Some(state.bottom_is_black) {
                            last_side = Some(state.bottom_is_black);
                            let _ = tx.send(AppMessage::PlayerSide(state.bottom_is_black));
                        }
                    }

                    false => {
                        // ── Live game branch ──────────────────────────────────
                        puzzle_gate.reset();

                        const START_FEN: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w";
                        let at_start = state.fen.starts_with(START_FEN);

                        // Track how many consecutive snapshots have been at the start position.
                        // Resets to 0 the moment any non-start (real game) FEN is observed.
                        if at_start { start_fen_streak = start_fen_streak.saturating_add(1); }
                        else        { start_fen_streak = 0; }

                        // Track whether we have seen real game moves on this page session.
                        // Once true, only a Page.frameNavigated (handled above) resets it.
                        // This prevents any start-FEN glitch from masquerading as a new game.
                        if !at_start && !state.fen.is_empty() {
                            seen_nonstart_fen = true;
                        }

                        if last_side != Some(state.bottom_is_black) {
                            last_side = Some(state.bottom_is_black);
                            let _ = tx.send(AppMessage::PlayerSide(state.bottom_is_black));
                        }

                        if let (Some(w), Some(b)) = (&state.player_white, &state.player_black) {
                            let pair = (w.clone(), b.clone());
                            if last_names.as_ref() != Some(&pair) {
                                last_names = Some(pair);
                                let _ = tx.send(AppMessage::PlayerNames {
                                    white: w.clone(), black: b.clone(),
                                });
                            }
                        }

                        if let (Some(cw), Some(cb)) = (&state.clock_white, &state.clock_black) {
                            let _ = tx.send(AppMessage::ClockUpdate {
                                white: cw.clone(), black: cb.clone(),
                            });
                        }

                        if let Some(rect) = state.board_rect {
                            let _ = tx.send(AppMessage::BoardRect(rect));
                        }

                        // Forward a start-position FEN only when we have NOT yet seen any
                        // real game moves on this page session AND ≥2 consecutive start-FEN
                        // observations (debounces single-observation noise on page load).
                        //
                        // When seen_nonstart_fen=true a game is in progress.  Any DOM glitch
                        // that briefly empties the move list produces a start FEN, but that
                        // FEN is silently dropped here — it never reaches on_new_fen and
                        // therefore never triggers the false commit + reset_game_state().
                        //
                        // seen_nonstart_fen is reset to false ONLY by Page.frameNavigated
                        // (main frame), ensuring a clean slate for each new game page.
                        if !at_start || (!seen_nonstart_fen && start_fen_streak >= 2) {
                            if state.fen != last_live_fen {
                                last_live_fen    = state.fen.clone();
                                last_fen_change  = Instant::now();
                                game_idle_sent   = false;
                                game_result_sent = false;
                            }

                            if !state.fen.is_empty() {
                                if tx.send(AppMessage::NewFen {
                                    fen: state.fen.clone(),
                                    last_move: state.last_move_san.clone(),
                                }).is_err() { break; }
                            }
                        }

                        if !game_result_sent {
                            if let Some(result_str) = &state.game_result {
                                game_result_sent = true;
                                game_idle_sent   = true;
                                tracing::info!("[bg] game result: {result_str}");
                                let _ = tx.send(AppMessage::GameResultDetected(result_str.clone()));
                                let _ = tx.send(AppMessage::GameIdle);
                            }
                        }

                        // Game-idle detection
                        if !game_idle_sent
                            && last_fen_change.elapsed() > Duration::from_secs(GAME_IDLE_SECS)
                        {
                            game_idle_sent = true;
                            if let Some(result_str) = &state.game_result {
                                let _ = tx.send(AppMessage::GameResultDetected(result_str.clone()));
                            }
                            let _ = tx.send(AppMessage::GameIdle);
                        } 

                        // Tab-switch detection (throttled — HTTP call)
                        if last_target_check.elapsed() >= Duration::from_secs(10) {
                            last_target_check = Instant::now();
                            if let Some(conn) = &cdp_conn {
                                if conn.target_changed(&config.cdp.endpoint) {
                                    tracing::info!("[bg] target tab changed — reconnecting");
                                    cdp_conn = None;
                                }
                            }
                        }
                    }
                }
            }
        }
        // No sleep — next_event() blocks on the WebSocket (120 ms read timeout).
    }
}