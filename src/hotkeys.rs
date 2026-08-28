//! Global hotkey detection — Nexus pattern: `GetAsyncKeyState` polled every
//! frame inside `update()`.
//!
//! `GetAsyncKeyState` reports the instantaneous key state at the OS level,
//! regardless of which window has focus.  This is the proven approach used by
//! the Nexus overlay (see guides/Nexus src/utils/input.rs).
//!
//! `is_key_pressed` implements edge detection (down AND was-not-down) so a
//! held key fires only once, exactly like Nexus `Input::is_key_pressed`.
//! The per-key `state: &mut bool` must be stored on the caller (see
//! `RustyChessApp::key_states`).

// ── Binding table ─────────────────────────────────────────────────────────────

/// VK codes for each action, loaded from config and updated live on rebind.
#[derive(Debug, Clone)]
pub struct Bindings {
    pub toggle_menu:     u32,
    pub flip_board:      u32,
    pub toggle_discrete: u32,
    pub toggle_overlay:  u32,
    pub exit:            u32,
    pub reconnect_cdp:   u32,
    pub hint_hold:       u32,
}

impl Bindings {
    pub fn from_config(cfg: &crate::config::HotkeyConfig) -> Self {
        Self {
            toggle_menu:     cfg.toggle_menu,
            flip_board:      cfg.flip_board,
            toggle_discrete: cfg.toggle_discrete,
            toggle_overlay:  cfg.toggle_overlay,
            exit:            cfg.exit,
            reconnect_cdp:   cfg.reconnect_cdp,
            hint_hold:       cfg.hint_hold,
        }
    }
}

// ── Key polling ───────────────────────────────────────────────────────────────

/// Returns `true` once when `vk` transitions from up → down (edge detect).
///
/// `state` is the previous frame's pressed state; pass `&mut self.key_states[N]`.
/// Works system-wide regardless of window focus — identical to Nexus
/// `Input::is_key_pressed`.
#[cfg(target_os = "windows")]
pub fn is_key_pressed(vk: u32, state: &mut bool) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    // High bit set = key is currently down.
    let down = unsafe { GetAsyncKeyState(vk as i32) } < 0;
    let just_pressed = down && !*state;
    *state = down;
    just_pressed
}

#[cfg(not(target_os = "windows"))]
pub fn is_key_pressed(_vk: u32, _state: &mut bool) -> bool { false }

/// Returns `true` while `vk` is currently held down (no edge detection).
///
/// Used for hold-to-reveal "hint" mode — the overlay is painted only on the
/// frames where this returns `true`.  Works system-wide regardless of focus.
#[cfg(target_os = "windows")]
pub fn is_key_down(vk: u32) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    (unsafe { GetAsyncKeyState(vk as i32) }) < 0
}

#[cfg(not(target_os = "windows"))]
pub fn is_key_down(_vk: u32) -> bool { false }