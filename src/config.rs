use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ── Behavioural tuning ──────────────────────────────────────────────────────────
//
// Magic numbers that govern timing/heuristics, gathered in one place so they
// can be adjusted without hunting through the codebase.
pub mod tuning {
    /// Exponential-smoothing factor for the eval bar (0..1, higher = snappier).
    pub const EVAL_SMOOTHING: f32 = 0.15;
    /// Centipawn change required to flag the analysis as "changed" for repaint.
    pub const SCORE_CHANGE_THRESHOLD: i32 = 5;
    /// Seconds of FEN silence before the game is considered idle/over.
    pub const GAME_IDLE_SECS: u64 = 45;
    /// Polls a puzzle FEN must stay stable before committing a NEW puzzle.
    pub const PUZZLE_STABLE_NEW: u32 = 3;
    /// Polls a puzzle FEN must stay stable for a continuation move.
    pub const PUZZLE_STABLE_CONT: u32 = 2;
    /// Piece-count delta vs the puzzle root that marks a brand-new puzzle.
    pub const PUZZLE_PIECE_DELTA: i32 = 3;
}

// ── Top-level config ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub engine:  EngineConfig,
    pub analysis: AnalysisConfig,
    pub cdp:     CdpConfig,
    /// Your chess.com username — used to detect which side you play.
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub window: WindowConfig,
    #[serde(default)]
    pub hotkeys: HotkeyConfig,
}

// ── Window / visibility config ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowConfig {
    #[serde(default = "default_true")]
    pub hide_from_taskbar: bool,
    #[serde(default)]
    pub capture_exclusion: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            hide_from_taskbar: true,
            capture_exclusion: false,
        }
    }
}

// ── Sub-configs ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    /// Path to Stockfish (or any UCI engine) binary.
    pub path: String,
    /// Hash table size in MB.
    pub hash_mb: u32,
    /// Number of CPU threads.
    pub threads: u32,
    /// Stockfish skill level 0–20.
    pub skill_level: u32,
    /// Original filename before the engine was disguised (e.g. "lc0.exe").
    /// Used by the Unhide feature to restore the binary with its original name.
    #[serde(default)]
    pub original_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisConfig {
    /// How many PV lines to request (MultiPV).
    pub multipv: u32,
    /// Search depth cap (0 = infinite / time-limited).
    pub depth: u32,
    /// Node count cap per move (0 = uncapped).  Recommended for lc0: 800–2000.
    /// Takes priority over `depth` when non-zero.  Works with any UCI engine.
    #[serde(default)]
    pub nodes: u32,
    #[serde(default = "default_display_lines")]
    pub display_lines: u32,
    #[serde(default)]
    pub discrete_mode: bool,
    #[serde(default = "default_true")]
    pub overlay_enabled: bool,
    #[serde(default = "default_true")]
    pub show_eval_bar: bool,
    /// Hint mode: hide the overlay completely and only reveal it (in the
    /// discrete style) while the `hint_hold` key is held down.  A "peek"
    /// binding — nothing is shown until you deliberately press and hold.
    #[serde(default)]
    pub hint_mode: bool,
    /// Show opening name label on the board overlay.
    #[serde(default = "default_true")]
    pub show_opening_name: bool,
    /// Search depth used by the manual Game Review analysis (post-game move
    /// classification).  Higher = more accurate but slower.  Default 18.
    #[serde(default = "default_review_depth")]
    pub review_depth: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdpConfig {
    /// Chrome remote-debugging endpoint.
    /// Launch with:  --remote-debugging-port=9222
    #[serde(default = "default_cdp_endpoint")]
    pub endpoint: String,
    /// Milliseconds between CDP board polls.
    #[serde(default = "default_cdp_poll_ms")]
    pub poll_interval_ms: u64,
    /// Path to Chrome/Chromium executable (used by the launch button).
    #[serde(default)]
    pub chrome_path: String,
    /// Extra CLI args appended when launching Chrome from the HUD.
    #[serde(default)]
    pub chrome_extra_args: String,
}

/// Configurable hotkey bindings stored as Win32 VK codes.
///
/// Defaults:
///   toggle_menu    = Insert  (0x2D)
///   flip_board     = F       (0x46)
///   toggle_discrete= D       (0x44)
///   toggle_overlay = H       (0x48)
///   exit           = F12     (0x7B)
///  ReconnectCdp    = F11     (0x7A)  // TODO: separate rebind target for this
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeyConfig {
    #[serde(default = "default_vk_insert")] pub toggle_menu:     u32,
    #[serde(default = "default_vk_f")]      pub flip_board:      u32,
    #[serde(default = "default_vk_d")]      pub toggle_discrete: u32,
    #[serde(default = "default_vk_h")]      pub toggle_overlay:  u32,
    #[serde(default = "default_vk_f12")]    pub exit:            u32,
    #[serde(default = "default_vk_f11")]    pub reconnect_cdp:   u32,
    /// Hold-to-reveal "hint" key (default Right Shift).  Used only when
    /// `analysis.hint_mode` is enabled.
    #[serde(default = "default_vk_rshift")] pub hint_hold:       u32,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            toggle_menu:     default_vk_insert(),
            flip_board:      default_vk_f(),
            toggle_discrete: default_vk_d(),
            toggle_overlay:  default_vk_h(),
            exit:            default_vk_f12(),
            reconnect_cdp: default_vk_f11(),
            hint_hold:       default_vk_rshift(),
        }
    }
}

// ── Page type detection ───────────────────────────────────────────────────────

/// What kind of chess page the CDP thread is currently looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChessPage {
    #[default]
    Unknown,
    /// chess.com /play/online or /play/online/* — live game
    LiveGame,
    /// chess.com /play/computer — vs computer
    VsComputer,
    /// chess.com /puzzles (or /puzzles/rated)
    PuzzleNormal,
    /// chess.com /puzzles/daily
    PuzzleDaily,
    /// chess.com /puzzles/rush
    PuzzleRush,
    /// chess.com /puzzles/battle
    PuzzleBattle,
    /// lichess.org round game (live or correspondence)
    LichessGame,
    /// lichess.org /training puzzle
    LichessPuzzle,
    /// chess.com /analysis/* — post-game review board (engine overlay only, no recording)
    ChessComAnalysis,
}

impl ChessPage {
    /// Classify a URL string into a page type.
    pub fn from_url(url: &str) -> Self {
        let url = url.to_ascii_lowercase();
        if url.contains("chess.com/puzzles/daily")  { return Self::PuzzleDaily; }
        if url.contains("chess.com/daily")          { return Self::PuzzleDaily; }
        if url.contains("chess.com/puzzles/rush")   { return Self::PuzzleRush; }
        if url.contains("chess.com/puzzles/battle") { return Self::PuzzleBattle; }
        if url.contains("chess.com/puzzles")        { return Self::PuzzleNormal; }
        // Analysis board must be checked BEFORE /game so /analysis/game/... doesn't
        // accidentally match the chess.com/game catch-all.
        if url.contains("chess.com/analysis")       { return Self::ChessComAnalysis; }
        if url.contains("chess.com/play/computer")  { return Self::VsComputer; }
        if url.contains("chess.com/play")           { return Self::LiveGame; }
        // chess.com live games navigate to /game/live/ID after the game starts
        if url.contains("chess.com/game/computer")  { return Self::VsComputer; }
        if url.contains("chess.com/game")           { return Self::LiveGame; }
        if url.contains("lichess.org/training") || url.contains("lichess.org/puzzle")
            || url.contains("lichess.org/streak") || url.contains("lichess.org/storm")
            || url.contains("lichess.org/racer")
        {
            return Self::LichessPuzzle;
        }
        // Lichess round: lichess.org/GAMEID or lichess.org/GAMEID/white|black
        if url.contains("lichess.org/") {
            let path = url.trim_start_matches("https://").trim_start_matches("http://");
            let after = path.trim_start_matches("lichess.org/");
            let seg = after.split('/').next().unwrap_or("");
            // Accept /GAMEID (8 chars) and /GAMEIDTTTT (12 chars = game ID + 4-char player token).
            if (seg.len() == 8 || seg.len() == 12)
                && seg.chars().all(|c| c.is_ascii_alphanumeric())
            {
                return Self::LichessGame;
            }
        }
        Self::Unknown
    }

    pub fn is_puzzle(self) -> bool {
        matches!(
            self,
            Self::PuzzleNormal
                | Self::PuzzleDaily
                | Self::PuzzleRush
                | Self::PuzzleBattle
                | Self::LichessPuzzle
                // Analysis board: routed to the puzzle branch so the bg_thread
                // never fires GameIdle / GameResultDetected for it, preventing
                // the analysis board from generating fake game history fragments.
                | Self::ChessComAnalysis
        )
    }

    // pub fn label(self) -> &'static str {
    //     match self {
    //         Self::Unknown       => "—",
    //         Self::LiveGame      => "Live Game",
    //         Self::VsComputer    => "vs Computer",
    //         Self::PuzzleNormal  => "Puzzle",
    //         Self::PuzzleDaily   => "Daily Puzzle",
    //         Self::PuzzleRush    => "Puzzle Rush",
    //         Self::PuzzleBattle  => "Puzzle Battle",
    //     }
    // }

    #[allow(dead_code)]
    pub fn url(self) -> Option<&'static str> {
        match self {
            Self::LiveGame      => Some("https://www.chess.com/play/online"),
            Self::VsComputer    => Some("https://www.chess.com/play/computer"),
            Self::PuzzleNormal  => Some("https://www.chess.com/puzzles"),
            Self::PuzzleDaily   => Some("https://www.chess.com/daily"),
            Self::PuzzleRush    => Some("https://www.chess.com/puzzles/rush"),
            Self::PuzzleBattle  => Some("https://www.chess.com/puzzles/battle"),
            Self::LichessGame   => Some("https://lichess.org/?any#hook"),
            Self::LichessPuzzle    => Some("https://lichess.org/training"),
            Self::ChessComAnalysis => None,
            Self::Unknown          => None,
        }
    }
}

// ── VK helpers ────────────────────────────────────────────────────────────────

/// Human-readable label for a Win32 VK code.
pub fn vk_to_label(vk: u32) -> String {
    match vk {
         0x2D => "Ins".into(),   0x2E => "Del".into(),
        0x21 => "PgUp".into(),  0x22 => "PgDn".into(),
        0x23 => "End".into(),   0x24 => "Home".into(),
        0x70 => "F1".into(),    0x71 => "F2".into(),   0x72 => "F3".into(),
        0x73 => "F4".into(),    0x74 => "F5".into(),   0x75 => "F6".into(),
        0x76 => "F7".into(),    0x77 => "F8".into(),   0x78 => "F9".into(),
        0x79 => "F10".into(),   0x7A => "F11".into(),  0x7B => "F12".into(),
        0x60 => "Num0".into(),  0x61 => "Num1".into(), 0x62 => "Num2".into(),
        0x63 => "Num3".into(),  0x64 => "Num4".into(), 0x65 => "Num5".into(),
        0x66 => "Num6".into(),  0x67 => "Num7".into(), 0x68 => "Num8".into(),
        0x69 => "Num9".into(),
        0x30..=0x39 => format!("{}", (vk - 0x30) as u8 as char),
        0x41..=0x5A => format!("{}", vk as u8 as char),
        _ => format!("0x{vk:02X}"),
    }
}

// ── Defaults ──────────────────────────────────────────────────────────────────

fn default_true()          -> bool  { true }
fn default_display_lines() -> u32   { 1 }
fn default_review_depth()  -> u32   { 18 }
fn default_vk_insert()     -> u32   { 0x2D }
fn default_vk_f()          -> u32   { 0x46 }
fn default_vk_d()          -> u32   { 0x44 }
fn default_vk_h()          -> u32   { 0x48 }
fn default_vk_f12()        -> u32   { 0x7B }
fn default_vk_f11()        -> u32   { 0x7A }
fn default_vk_rshift()     -> u32   { 0xA1 }
fn default_cdp_endpoint()  -> String { "http://127.0.0.1:9222".into() }
fn default_cdp_poll_ms()   -> u64   { 350 }

// ── Config impl ───────────────────────────────────────────────────────────────

impl Default for Config {
    fn default() -> Self {
        Self {
            engine: EngineConfig {
                path: find_stockfish(),
                hash_mb: 256,
                threads: 2,
                skill_level: 20,
                original_name: String::new(),
            },
            analysis: AnalysisConfig {
                multipv: 3,
                depth: 20,
                nodes: 0,
                display_lines: default_display_lines(),
                discrete_mode: false,
                overlay_enabled: true,
                show_eval_bar: true,
                show_opening_name: true,
                hint_mode: false,
                review_depth: default_review_depth(),
            },
            cdp: CdpConfig {
                endpoint: default_cdp_endpoint(),
                poll_interval_ms: default_cdp_poll_ms(),
                chrome_path: find_chrome(),
                chrome_extra_args: String::new(),
            },
            username: String::new(),
            window: WindowConfig::default(),
            hotkeys: HotkeyConfig::default(),
        }
    }
}

impl Config {
    /// Load from `rustychess.toml` next to the binary, or write defaults.
    pub fn load() -> Self {
        let path = config_path();
        if path.exists() {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(mut cfg) = toml::from_str::<Config>(&text) {
                    if !std::path::Path::new(&cfg.engine.path).exists() {
                        let found = find_stockfish();
                        if std::path::Path::new(&found).exists() {
                            cfg.engine.path = found;
                        }
                    }
                    return cfg;
                }
            }
        }
        let defaults = Self::default();
        let _ = defaults.save();
        defaults
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self)?;
        std::fs::write(config_path(), text)?;
        Ok(())
    }
}

fn config_path() -> PathBuf {
    std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from("."))
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("rustychess.toml")
}

/// Find a UCI engine binary near the executable.
/// Priority:
///   1. Any file starting with "stockfish" in the same dirs (legacy)
///   2. Any file starting with a known stealth alias (analysis, engine, chess_helper, etc.)
///   3. Falls back to "stockfish.exe" as a placeholder.
fn find_stockfish() -> String {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() { dirs.push(cwd); }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
            if let Some(p) = dir.parent().and_then(|d| d.parent()) {
                dirs.push(p.to_path_buf());
            }
        }
    }
    // Known stealth prefixes — users are encouraged to rename their Stockfish
    // to one of these to avoid "stockfish.exe" showing in Task Manager.
    const STEALTH_PREFIXES: &[&str] = &[
        "stockfish",
        "analysis",
        "engine",
        "chess_helper",
        "chessengine",
        "helper",
    ];
    for dir in &dirs {
        if let Ok(entries) = std::fs::read_dir(dir) {
            let mut matches: Vec<(usize, PathBuf)> = entries.flatten().filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_lowercase();
                #[cfg(windows)]    { if !name.ends_with(".exe") { return None; } }
                #[cfg(not(windows))] { if name.contains('.') { return None; } }
                // Score by priority: lower index = higher priority
                STEALTH_PREFIXES.iter().enumerate()
                    .find(|(_, pfx)| name.starts_with(*pfx))
                    .map(|(idx, _)| (idx, e.path()))
            }).collect();
            matches.sort_by_key(|(idx, p)| (*idx, p.clone()));
            if let Some((_, p)) = matches.first() { return p.to_string_lossy().into(); }
        }
    }
    #[cfg(windows)]    { "stockfish.exe".into() }
    #[cfg(not(windows))] { "stockfish".into() }
}

/// Search common Chrome install locations.
fn find_chrome() -> String {
    #[cfg(windows)]
    {
        let candidates = [
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe",
        ];
        for c in &candidates {
            if std::path::Path::new(c).exists() { return (*c).into(); }
        }
        "chrome.exe".into()
    }
    #[cfg(target_os = "macos")]
    {
        let candidates = [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        ];
        for c in &candidates { if std::path::Path::new(c).exists() { return (*c).into(); } }
        "google-chrome".into()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        for bin in &["google-chrome", "chromium-browser", "chromium", "brave-browser"] {
            if std::process::Command::new("which").arg(bin).output()
                .map(|o| o.status.success()).unwrap_or(false)
            { return (*bin).into(); }
        }
        "google-chrome".into()
    }
}