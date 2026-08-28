//! Browser launcher — opens Chrome, Edge, Brave (or any Chromium) with
//! --remote-debugging-port=9222.  The browser variant is detected from the
//! binary name so the correct profile directory and flags are used.

use std::process::Command;
use serde::Deserialize;
use tracing::{info, warn};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchResult {
    Launched,
    AlreadyRunning,
    Failed(String),
}

/// Which Chromium variant we detected from the binary path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserKind {
    Chrome,
    Edge,
    Brave,
    Chromium,
    Other,
}

impl BrowserKind {
    pub fn detect(path: &str) -> Self {
        let lower = path.to_lowercase();
        if lower.contains("msedge") || lower.contains("edge") { Self::Edge }
        else if lower.contains("brave")                        { Self::Brave }
        else if lower.contains("chromium")                     { Self::Chromium }
        else if lower.contains("chrome")                       { Self::Chrome }
        else                                                   { Self::Other }
    }

    /// Subdirectory name used for the isolated --user-data-dir so Chrome and
    /// Edge profiles never collide with each other.
    fn profile_subdir(self) -> &'static str {
        match self {
            Self::Chrome   => "rustychess-chrome",
            Self::Edge     => "rustychess-edge",
            Self::Brave    => "rustychess-brave",
            Self::Chromium => "rustychess-chromium",
            Self::Other    => "rustychess-browser",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Chrome   => "Chrome",
            Self::Edge     => "Edge",
            Self::Brave    => "Brave",
            Self::Chromium => "Chromium",
            Self::Other    => "Browser",
        }
    }
}

pub fn debug_port_open(endpoint: &str) -> bool {
    let addr = cdp_addr(endpoint).unwrap_or_else(|| "127.0.0.1:9222".to_string());
    std::net::TcpStream::connect_timeout(
        &addr.parse().unwrap_or_else(|_| "127.0.0.1:9222".parse().unwrap()),
        std::time::Duration::from_millis(300),
    )
    .is_ok()
}

pub fn launch_chrome(
    chrome_path: &str,
    cdp_endpoint: &str,
    extra_args: &str,
    target_url: Option<&str>,
) -> LaunchResult {
    if debug_port_open(cdp_endpoint) {
        info!("[browser] debug port already open — navigating existing window");
        if let Some(url) = target_url {
            navigate_existing(cdp_endpoint, url);
        }
        return LaunchResult::AlreadyRunning;
    }

    let kind = BrowserKind::detect(chrome_path);
    info!("[browser] detected kind: {:?} from path: {chrome_path}", kind);

    // Each browser gets its own isolated profile dir under %LOCALAPPDATA% so:
    //   • Chrome and Edge profiles never conflict
    //   • The profile persists between sessions (faster startup, no re-login)
    //   • chess.com cannot see this directory — it is local filesystem only
    //   • The CDP session cookies live here; they are NOT shared with your
    //     real browser profile, keeping your real account separate
    let local = std::env::var("LOCALAPPDATA")
        .unwrap_or_else(|_| std::env::var("TEMP").unwrap_or_else(|_| "C:\\Temp".into()));
    let profile_path = std::path::PathBuf::from(&local)
        .join("rustychess-cdp-profiles")
        .join(kind.profile_subdir());

    // Ensure the directory exists.
    if let Err(e) = std::fs::create_dir_all(&profile_path) {
        warn!("[browser] could not create profile dir: {e}");
    }

    let port = cdp_port(cdp_endpoint).unwrap_or(9222);

    let mut cmd = Command::new(chrome_path);
    cmd
        .arg(format!("--remote-debugging-port={port}"))
        // ── Anti-detection ────────────────────────────────────────────────
        .arg("--disable-blink-features=AutomationControlled")
        .arg("--exclude-switches=enable-automation")
        .arg("--test-type")            // suppresses "unsupported flag" infobar
        // ── Isolated profile (per-browser) ────────────────────────────────
        .arg(format!("--user-data-dir={}", profile_path.display()))
        // ── Housekeeping ──────────────────────────────────────────────────
        .arg("--new-window")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-sync")
        .arg("--disable-extensions")
        .arg("--disable-background-networking")
        .arg("--disable-client-side-phishing-detection")
        .arg("--disable-default-apps");

    // Edge-specific: suppress the "Set as default" nag on first run.
    if kind == BrowserKind::Edge {
        cmd.arg("--hide-crash-restore-bubble");
    }

    for arg in extra_args.split_whitespace() {
        cmd.arg(arg);
    }

    if let Some(url) = target_url {
        cmd.arg(url);
    }

    info!("[browser] launching {}: {:?}", kind.display_name(), cmd);

    match cmd.spawn() {
        Ok(_) => {
            info!("[browser] {} spawned OK", kind.display_name());
            LaunchResult::Launched
        }
        Err(e) => {
            warn!("[browser] failed to spawn {}: {e}", kind.display_name());
            LaunchResult::Failed(e.to_string())
        }
    }
}

// ── Existing-window navigation ────────────────────────────────────────────────

#[derive(Deserialize)]
struct NavTarget {
    #[serde(rename = "type")]
    kind: Option<String>,
    url: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    ws: Option<String>,
}

/// When the browser is already running, navigate its active page tab to `url`
/// over CDP (`Page.navigate`) instead of spawning a second process — otherwise
/// the new URL would be ignored and nothing would appear to happen.
pub fn navigate_existing(cdp_endpoint: &str, url: &str) -> bool {
    let list_url = format!("{cdp_endpoint}/json/list");
    let targets: Vec<NavTarget> = match ureq::get(&list_url)
        .timeout(std::time::Duration::from_millis(700))
        .call()
    {
        Ok(resp) => match resp.into_json() {
            Ok(t) => t,
            Err(e) => { warn!("[browser] navigate: decode targets failed: {e}"); return false; }
        },
        Err(e) => { warn!("[browser] navigate: list request failed: {e}"); return false; }
    };

    // Prefer a real page target (not devtools / extension / service worker).
    let ws_url = targets
        .iter()
        .find(|t| {
            t.kind.as_deref() == Some("page")
                && t.ws.is_some()
                && !t.url.starts_with("devtools://")
        })
        .and_then(|t| t.ws.as_deref());

    let ws_url = match ws_url {
        Some(u) => u,
        None => { warn!("[browser] navigate: no page target found"); return false; }
    };

    let mut ws = match tungstenite::connect(ws_url) {
        Ok((ws, _)) => ws,
        Err(e) => { warn!("[browser] navigate: websocket connect failed: {e}"); return false; }
    };

    let navigate = serde_json::json!({
        "id": 1, "method": "Page.navigate", "params": { "url": url }
    });
    let bring_to_front = serde_json::json!({
        "id": 2, "method": "Page.bringToFront"
    });

    let ok = ws.send(tungstenite::Message::Text(navigate.to_string())).is_ok()
        && ws.send(tungstenite::Message::Text(bring_to_front.to_string())).is_ok();
    let _ = ws.flush();
    let _ = ws.close(None);

    if ok {
        info!("[browser] navigated existing tab to {url}");
    } else {
        warn!("[browser] navigate: send failed");
    }
    ok
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn cdp_addr(endpoint: &str) -> Option<String> {
    let without_scheme = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))?;
    let slash = without_scheme.find('/').unwrap_or(without_scheme.len());
    Some(without_scheme[..slash].to_string())
}

fn cdp_port(endpoint: &str) -> Option<u16> {
    let addr = cdp_addr(endpoint)?;
    addr.split(':').last()?.parse().ok()
}