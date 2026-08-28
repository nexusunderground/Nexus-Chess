//! Stealth / capture-exclusion utilities for RustyChess.
//!
//! Provides `WDA_EXCLUDEFROMCAPTURE` support so the overlay window is
//! invisible to OBS, NVIDIA ShadowPlay, and BitBlt-based recorders while
//! remaining fully visible on the physical display.
//!
//! The process-disguise / binary-copy logic from Nexus is intentionally
//! omitted here — RustyChess does not need it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// ── Public state ──────────────────────────────────────────────────────────────

/// `true` after `WDA_EXCLUDEFROMCAPTURE` has been successfully applied.
/// Read by the Vision tab to show the current live state.
pub static CAPTURE_EXCLUSION_ACTIVE: AtomicBool = AtomicBool::new(false);

// ── Public API ────────────────────────────────────────────────────────────────

/// Apply `WDA_EXCLUDEFROMCAPTURE` to the overlay window.
///
/// Because eframe creates its window asynchronously, this polls
/// `FindWindowW` by title for up to 10 seconds (50 × 200 ms) in a
/// background thread before giving up.  Safe to call from the egui
/// update loop — spawns only if not already active.
pub fn enable_capture_exclusion(window_title: &str) {
    if CAPTURE_EXCLUSION_ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let title = window_title.to_string();
    std::thread::spawn(move || {
        apply_capture_exclusion(&title);
        CAPTURE_EXCLUSION_ACTIVE.store(true, Ordering::Relaxed);
    });
}

/// Remove `WDA_EXCLUDEFROMCAPTURE` — overlay will reappear in recordings.
#[cfg(windows)]
pub fn disable_capture_exclusion(window_title: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SetWindowDisplayAffinity, WDA_NONE,
    };
    use windows::core::PCWSTR;

    let title_wide: Vec<u16> = window_title
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        if let Ok(hwnd) = FindWindowW(PCWSTR::null(), PCWSTR(title_wide.as_ptr())) {
            let _ = SetWindowDisplayAffinity(hwnd, WDA_NONE);
        }
    }
    CAPTURE_EXCLUSION_ACTIVE.store(false, Ordering::Relaxed);
}

#[cfg(not(windows))]
pub fn disable_capture_exclusion(_window_title: &str) {
    CAPTURE_EXCLUSION_ACTIVE.store(false, Ordering::Relaxed);
}

// ── Internal ──────────────────────────────────────────────────────────────────

/// Core Win32 implementation — polls for the HWND and applies the flag.
#[cfg(windows)]
fn apply_capture_exclusion(window_title: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
    };
    use windows::core::PCWSTR;

    let title_wide: Vec<u16> = window_title
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    // Poll up to 10 s (50 × 200 ms) for the eframe window to exist.
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(200));
        unsafe {
            if let Ok(hwnd) = FindWindowW(PCWSTR::null(), PCWSTR(title_wide.as_ptr())) {
                match SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) {
                    Ok(()) => {
                        tracing::info!(
                            "[stealth] WDA_EXCLUDEFROMCAPTURE applied — overlay hidden from capture"
                        );
                    }
                    Err(e) => {
                        tracing::warn!("[stealth] SetWindowDisplayAffinity failed: {}", e);
                    }
                }
                return;
            }
        }
    }
    tracing::warn!(
        "[stealth] apply_capture_exclusion: window '{}' not found within timeout",
        window_title
    );
}

#[cfg(not(windows))]
fn apply_capture_exclusion(_window_title: &str) {}