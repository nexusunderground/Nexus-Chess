#![windows_subsystem = "windows"]

mod app;
mod chess;
mod config;
mod engine;
mod game_store;
mod overlay;
mod vision;
mod hotkeys;
mod perf;
mod hidden;

use app::RustyChessApp;
use config::Config;
use tracing_subscriber::EnvFilter;

#[cfg(feature = "tracy")]
fn init_tracy() -> tracy_client::Client {
    tracy_client::Client::start()
}

#[cfg(not(feature = "tracy"))]
fn init_tracy() {}

fn main() {
    let _tracy = init_tracy();

    let debug_mode = std::env::args().any(|a| a == "--debug" || a == "-debug");

    if debug_mode {
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info,rustychess=info"));

        let log_dir = std::env::var_os("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("rustychess");
        let _ = std::fs::create_dir_all(&log_dir);

        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join("rustychess.log"))
        {
            Ok(file) => {
                tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_writer(std::sync::Mutex::new(file))
                    .with_ansi(false)
                    .init();
            }
            Err(_) => {
                let _ = tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .try_init();
            }
        }
    }

    let cfg = Config::load();

    if cfg.window.capture_exclusion {
        hidden::stealth::enable_capture_exclusion("RustyChess");
    }

    // ── Pre-emptive taskbar suppression ──────────────────────────────────────
    //
    // eframe creates the window, then makes it visible — two separate steps
    // with a small gap.  The shell registers a new window in the taskbar
    // roughly 200 ms after it first becomes visible.  Our first `update()`
    // call (where WS_EX_TOOLWINDOW is normally applied) fires *after* that
    // registration has already happened, so clicking the taskbar button is
    // required to dismiss it.
    //
    // Fix: race a background thread that polls for the HWND every 5 ms and
    // applies WS_EX_TOOLWINDOW + strips WS_EX_APPWINDOW the instant the
    // window exists — before the shell's registration window closes.
    // The thread exits as soon as it succeeds (or after 2 s timeout).
    #[cfg(target_os = "windows")]
    std::thread::spawn(|| {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            FindWindowW, GetWindowLongW, SetWindowLongW, SetWindowPos,
            GWL_EXSTYLE, HWND_TOPMOST,
            SWP_NOMOVE, SWP_NOSIZE, SWP_NOACTIVATE, SWP_FRAMECHANGED,
            WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_NOACTIVATE, WS_EX_APPWINDOW,
        };

        let title: Vec<u16> = "RustyChess"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let ptr = windows::core::PCWSTR::from_raw(title.as_ptr());

        // Up to 400 attempts × 5 ms = 2 s maximum wait.
        for _ in 0..400 {
            std::thread::sleep(std::time::Duration::from_millis(5));
            unsafe {
                let Ok(hwnd) = FindWindowW(None, ptr) else { continue };
                if hwnd.is_invalid() { continue }

                let ex   = GetWindowLongW(hwnd, GWL_EXSTYLE);
                let want = (ex
                    | WS_EX_LAYERED.0   as i32
                    | WS_EX_TOOLWINDOW.0 as i32
                    | WS_EX_NOACTIVATE.0 as i32)
                    & !(WS_EX_APPWINDOW.0 as i32);

                SetWindowLongW(hwnd, GWL_EXSTYLE, want);
                let _ = SetWindowPos(
                    hwnd, HWND_TOPMOST, 0, 0, 0, 0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
                // Done — shell re-evaluates taskbar membership on FRAMECHANGED.
                return;
            }
        }
    });

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RustyChess")
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_close_button(false)
            .with_maximized(true)
            .with_minimize_button(false)
            .with_maximize_button(false)
            .with_mouse_passthrough(true)
            .with_taskbar(false),
        renderer: eframe::Renderer::Glow,
        multisampling: 0,
        vsync: true,  // sync to display — prevents uncapped GPU render loop
        ..Default::default()
    };

    eframe::run_native(
        "RustyChess",
        native_options,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            install_symbol_font(&cc.egui_ctx);
            configure_style(&cc.egui_ctx);
            Ok(Box::new(RustyChessApp::new(cfg)))
        }),
    )
    .expect("eframe failed to start");
}

fn install_symbol_font(ctx: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\seguisym.ttf",
        r"C:\Windows\Fonts\SEGUISYM.TTF",
        r"C:\Windows\Fonts\segoeui.ttf",
    ];

    let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) else {
        tracing::warn!("[font] no symbol fallback font found — some glyphs may not render");
        return;
    };

    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "symbols".to_owned(),
        std::sync::Arc::new(egui::FontData::from_owned(bytes)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("symbols".to_owned());
    }
    ctx.set_fonts(fonts);
}

fn configure_style(ctx: &egui::Context) {
    use overlay::theme;
    let mut style = (*ctx.style()).clone();
    style.visuals.window_fill      = theme::BG_DARK;
    style.visuals.panel_fill       = theme::BG_DARK;
    style.visuals.window_stroke    = egui::Stroke::new(1.0, theme::BORDER_DEFAULT);
    style.visuals.extreme_bg_color = theme::BG_MEDIUM;
    ctx.set_style(style);
}