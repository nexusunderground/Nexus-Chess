//! HUD menu — draggable egui::Area rendered on the transparent overlay canvas.

use std::sync::atomic::Ordering;

use egui::RichText;
use super::theme;
use super::widgets;
use crate::config::{ChessPage, Config, vk_to_label};
use crate::engine::AnalysisResult;
use crate::game_store::{GameResult, GameSite, GameStore};
use crate::hidden;

/// Brand logos authored as SVG and embedded so the binary is self-contained.
/// Rendered via the `egui_extras` SVG image loader (installed in `main`).
const LICHESS_LOGO:  &[u8] = include_bytes!("../../assets/lichess.svg");
const CHESSCOM_LOGO: &[u8] = include_bytes!("../../assets/chesscom.svg");

#[derive(PartialEq, Clone, Copy, Default)]
pub enum MenuTab { #[default] Overview, Settings, Engine, Hotkeys, GameReview, About }

const MENU_WIDTH: f32 = 340.0;

/// All selectable keys in the hotkey dropdowns.
/// Each entry is `(display_label, VK_code)`.
const KEY_OPTIONS: &[(&str, u32)] = &[
    // Function keys
    ("F1",  0x70), ("F2",  0x71), ("F3",  0x72), ("F4",  0x73),
    ("F5",  0x74), ("F6",  0x75), ("F7",  0x76), ("F8",  0x77),
    ("F9",  0x78), ("F10", 0x79), ("F11", 0x7A), ("F12", 0x7B),
    // Letters
    ("A", 0x41), ("B", 0x42), ("C", 0x43), ("D", 0x44),
    ("E", 0x45), ("F", 0x46), ("G", 0x47), ("H", 0x48),
    ("I", 0x49), ("J", 0x4A), ("K", 0x4B), ("L", 0x4C),
    ("M", 0x4D), ("N", 0x4E), ("O", 0x4F), ("P", 0x50),
    ("Q", 0x51), ("R", 0x52), ("S", 0x53), ("T", 0x54),
    ("U", 0x55), ("V", 0x56), ("W", 0x57), ("X", 0x58),
    ("Y", 0x59), ("Z", 0x5A),
    // Digits
    ("0", 0x30), ("1", 0x31), ("2", 0x32), ("3", 0x33), ("4", 0x34),
    ("5", 0x35), ("6", 0x36), ("7", 0x37), ("8", 0x38), ("9", 0x39),
    // Numpad
    ("Num0", 0x60), ("Num1", 0x61), ("Num2", 0x62), ("Num3", 0x63),
    ("Num4", 0x64), ("Num5", 0x65), ("Num6", 0x66), ("Num7", 0x67),
    ("Num8", 0x68), ("Num9", 0x69),
    // Special / nav
    ("Insert",   0x2D), ("Delete",    0x2E), ("Home",   0x24),
    ("End",      0x23), ("PageUp",    0x21), ("PageDn", 0x22),
    ("Up",       0x26), ("Down",      0x28), ("Left",   0x25), ("Right", 0x27),
    // Modifiers — useful for hold-style bindings
    ("LShift",   0xA0), ("RShift",  0xA1),
    ("LCtrl",    0xA2), ("RCtrl",   0xA3),
    ("LAlt",     0xA4), ("RAlt",    0xA5),
    ("CapsLock", 0x14), ("Tab",     0x09), ("Space",  0x20),
    ("Enter",    0x0D), ("Backspace",0x08), ("Escape", 0x1B),
    ("PrintScr", 0x2C), ("ScrollLk",0x91), ("Pause",  0x13),
];

/// Look up the display label for a VK code in KEY_OPTIONS.
fn vk_to_option_label(vk: u32) -> &'static str {
    KEY_OPTIONS
        .iter()
        .find(|(_, v)| *v == vk)
        .map(|(s, _)| *s)
        .unwrap_or("?")
}

/// An action requested by the menu during a frame, drained by the app after
/// rendering.  Replaces the previous `&mut bool` / `&mut Option<Option<String>>`
/// out-parameters that had to be threaded through every render function.
pub enum MenuCommand {
    /// Launch / navigate to a URL (`None` = launch the browser with no nav).
    LaunchUrl(Option<String>),
    /// Reconnect the CDP board observer (⟳ button).
    ReconnectCdp,
    /// Kill and respawn the engine process.
    RestartEngine,
    /// Run a post-game review (move classification) on the stored game `id`.
    AnalyseGame(u64),
    /// The user asked to quit (✕ button).
    Quit,
}

/// Bundles all the borrowed state the menu reads/writes for one frame, plus a
/// `commands` sink.  Passing a single `&mut MenuContext` keeps the render
/// functions to a sane argument count and removes the deep out-parameter
/// threading that previously required `#[allow(clippy::too_many_arguments)]`.
pub struct MenuContext<'a> {
    pub config:          &'a mut Config,
    pub analysis:        &'a AnalysisResult,
    pub is_analysing:    bool,
    pub flipped:         &'a mut bool,
    pub engine_status:   &'a str,
    pub current_page:    ChessPage,
    pub move_history:    &'a [String],
    pub player_white:    Option<&'a str>,
    pub player_black:    Option<&'a str>,
    pub game_time_white: Option<&'a str>,
    pub game_time_black: Option<&'a str>,
    pub chrome_status:   &'a str,
    pub nav_selected:    &'a mut Option<(String, String)>,
    /// Current opening (ECO code, name), or `None` if no named line has been
    /// reached yet this game.
    pub current_opening: Option<(&'a str, &'a str)>,
    /// Game history store (read + clear).
    pub game_store:      &'a mut GameStore,
    /// Which game record id is currently expanded in the review tab.
    pub expanded_game:   &'a mut Option<u64>,
    /// In-flight review `(game id, done, total)`, if a review is running.
    pub reviewing:       Option<(u64, usize, usize)>,
    /// Last review error `(game id, message)` for inline display.
    pub review_error:    Option<(u64, &'a str)>,
    /// Actions requested this frame; drained by the caller after `render_menu`.
    pub commands:        Vec<MenuCommand>,
}

pub fn render_menu(
    ctx:         &egui::Context,
    menu_pos:    &mut egui::Pos2,
    current_tab: &mut MenuTab,
    m:           &mut MenuContext,
) {
    let accent = theme::ACCENT_PRIMARY;

    let area_resp = egui::Area::new(egui::Id::new("rustychess_menu"))
        .current_pos(*menu_pos)
        .movable(true)
        .constrain(true)
        .order(egui::Order::Foreground)
        .interactable(true)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(theme::BG_DARK)
                .corner_radius(6.0)
                .stroke(egui::Stroke::new(2.0, accent))
                .inner_margin(egui::Margin::same(2))
                .show(ui, |ui| {
                    egui::Frame::new()
                        .fill(theme::BG_DARK)
                        .corner_radius(4.0)
                        .stroke(egui::Stroke::new(1.0, theme::BORDER_DEFAULT))
                        .show(ui, |ui| {
                            ui.set_width(MENU_WIDTH);
                            render_header(ui, m);
                            render_tab_bar(ui, current_tab);
                            separator(ui);

                            egui::Frame::new()
                                .inner_margin(egui::Margin::symmetric(8, 6))
                                .show(ui, |ui| {
                                    let max_h = match *current_tab {
                                        MenuTab::Overview | MenuTab::GameReview => 520.0,
                                        _ => 440.0,
                                    };
                                    egui::ScrollArea::vertical().max_height(max_h).show(ui, |ui| {
                                        match *current_tab {
                                            MenuTab::Overview   => render_overview_tab(ui, m, accent),
                                            MenuTab::Settings   => render_settings_tab(ui, m, accent),
                                            MenuTab::Engine     => render_engine_tab(ui, m.config, accent),
                                            MenuTab::Hotkeys    => render_hotkeys_tab(ui, m.config, accent),
                                            MenuTab::GameReview => render_game_review_tab(ui, m, accent),
                                            MenuTab::About      => render_about_tab(ui, accent, &mut m.commands),
                                        }
                                    });
                                });

                            render_footer(ui, m.is_analysing, m.config, false);
                        });
                });
        });

    if area_resp.response.dragged() {
        *menu_pos = ctx.input(|i| *menu_pos + i.pointer.delta());
    }
}

// ── Header ────────────────────────────────────────────────────────────────────

fn render_header(ui: &mut egui::Ui, m: &mut MenuContext) {
    let engine_status = m.engine_status;
    let page = m.current_page;
    egui::Frame::new()
        .fill(theme::BG_MEDIUM)
        .corner_radius(egui::CornerRadius { nw: 4, ne: 4, sw: 0, se: 0 })
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let t = ui.ctx().input(|i| i.time) as f32;
                let pulse = (t * 2.5 + (t * 7.3).sin() * 0.5).sin() * 0.5 + 0.5;
                let glow_alpha = (pulse * 180.0 + 50.0) as u8;

                ui.label(RichText::new("♟").size(13.0).color(theme::ACCENT_PRIMARY));
                ui.add_space(4.0);
                let tr = ui.label(
                    RichText::new("RustyChess").size(12.0).strong()
                        .color(egui::Color32::from_rgba_unmultiplied(220, 180, 90, glow_alpha)),
                );
                let glow = tr.rect.expand(2.0 + pulse * 1.5);
                ui.painter().rect_filled(glow, 4.0,
                    egui::Color32::from_rgba_unmultiplied(220, 180, 90, (pulse * 20.0) as u8));

                ui.add_space(6.0);

                if page != ChessPage::Unknown {
                    let (badge_col, badge_txt) = page_badge(page);
                    ui.label(RichText::new(badge_txt).size(9.0).color(badge_col));
                }

                let (dot_col, dot_txt) = if engine_status.contains("ready") || engine_status.contains("analysing") {
                    (theme::ACCENT_SUCCESS, engine_status)
                } else if engine_status.contains("starting") {
                    (theme::ACCENT_WARNING, engine_status)
                } else {
                    (theme::ACCENT_DANGER, engine_status)
                };
                ui.label(RichText::new(format!("[{dot_txt}]")).size(9.0).color(dot_col));
                ui.ctx().request_repaint();

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(
                        egui::Button::new(RichText::new("✕").size(11.0).color(theme::ACCENT_DANGER))
                            .fill(egui::Color32::from_rgba_unmultiplied(200, 70, 70, 30))
                            .stroke(egui::Stroke::new(1.0, theme::ACCENT_DANGER))
                            .min_size(egui::vec2(20.0, 20.0)),
                    ).clicked() {
                        m.commands.push(MenuCommand::Quit);
                    }
                });
            });
        });
}

fn page_badge(page: ChessPage) -> (egui::Color32, &'static str) {
    match page {
        ChessPage::LiveGame      => (theme::ACCENT_SUCCESS,                    "[LIVE]"),
        ChessPage::VsComputer    => (theme::ACCENT_INFO,                       "[vs CPU]"),
        ChessPage::PuzzleNormal  => (theme::ACCENT_WARNING,                    "[PUZZLE]"),
        ChessPage::PuzzleDaily   => (theme::ACCENT_WARNING,                    "[DAILY]"),
        ChessPage::PuzzleRush    => (egui::Color32::from_rgb(200, 100, 255),   "[RUSH]"),
        ChessPage::PuzzleBattle  => (theme::ACCENT_DANGER,                     "[BATTLE]"),
        ChessPage::LichessGame   => (egui::Color32::from_rgb(190, 230, 255),   "[LICHESS]"),
        ChessPage::LichessPuzzle    => (egui::Color32::from_rgb(190, 230, 255),   "[LICHESS ✦]"),
        ChessPage::ChessComAnalysis => (egui::Color32::from_rgb(150, 200, 150),   "[ANALYSIS]"),
        ChessPage::Unknown          => (theme::TEXT_MUTED,                        ""),
    }
}

// ── Tab bar ───────────────────────────────────────────────────────────────────

fn render_tab_bar(ui: &mut egui::Ui, current_tab: &mut MenuTab) {
    egui::Frame::new()
        .fill(theme::BG_DARK)
        .inner_margin(egui::Margin::symmetric(4, 4))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(2.0, 2.0);
                for (label, tab) in [
                    ("OVERVIEW",     MenuTab::Overview),
                    ("SETTINGS",     MenuTab::Settings),
                    ("ENGINE",       MenuTab::Engine),
                    ("HOTKEYS",      MenuTab::Hotkeys),
                    ("GAME REVIEW",  MenuTab::GameReview),
                    ("ABOUT",        MenuTab::About),
                ] {
                    let active = *current_tab == tab;
                    let btn = egui::Button::new(
                        RichText::new(label).size(10.0).color(
                            if active { theme::TEXT_PRIMARY } else { theme::TEXT_SECONDARY }
                        ),
                    )
                    .fill(if active { theme::BG_LIGHT } else { egui::Color32::TRANSPARENT })
                    .stroke(if active { egui::Stroke::new(1.0, theme::BORDER_FRAME) } else { egui::Stroke::NONE })
                    .corner_radius(2.0)
                    .min_size(egui::vec2(66.0, 18.0));
                    if ui.add(btn).clicked() { *current_tab = tab; }
                }
            });
        });
}

// ── Overview tab ──────────────────────────────────────────────────────────────

fn render_overview_tab(
    ui:     &mut egui::Ui,
    m:      &mut MenuContext,
    accent: egui::Color32,
) {
    // Reborrow disjoint context fields into the local names the body uses.
    let config          = &mut *m.config;
    let analysis        = m.analysis;
    let is_analysing    = m.is_analysing;
    let engine_status   = m.engine_status;
    let move_history    = m.move_history;
    let player_white    = m.player_white;
    let player_black    = m.player_black;
    let game_time_white = m.game_time_white;
    let game_time_black = m.game_time_black;
    let chrome_status   = m.chrome_status;
    let current_opening = m.current_opening;
    let nav_selected    = &mut *m.nav_selected;
    let commands        = &mut m.commands;

    widgets::double_border_frame(ui, "QUICK NAVIGATION", accent, |ui| {
        render_page_buttons(ui, commands, nav_selected);
    });

    ui.add_space(4.0);

    widgets::double_border_frame(ui, "STATUS", accent, |ui| {
        ui.horizontal(|ui| {
            let (eng_col, eng_txt) = if is_analysing {
                (theme::ACCENT_SUCCESS, "ENGINE  LIVE")
            } else if engine_status.contains("error") {
                (theme::ACCENT_DANGER,  "ENGINE  ERROR")
            } else {
                (theme::TEXT_MUTED,     "ENGINE  IDLE")
            };
            let t = ui.ctx().input(|i| i.time) as f32;
            let pulse = (t * 3.0).sin() * 0.5 + 0.5;
            let dot_alpha = if is_analysing { (pulse * 180.0 + 75.0) as u8 } else { 100 };
            ui.painter().circle_filled(
                ui.next_widget_position() + egui::vec2(5.0, 6.0), 4.0,
                egui::Color32::from_rgba_unmultiplied(eng_col.r(), eng_col.g(), eng_col.b(), dot_alpha),
            );
            ui.add_space(12.0);
            ui.label(RichText::new(eng_txt).size(10.0).color(eng_col).strong());

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (col, txt) = if player_white.is_some() || player_black.is_some() {
                    (theme::ACCENT_SUCCESS, "GAME DETECTED")
                } else {
                    (theme::TEXT_MUTED, "NO GAME")
                };
                ui.label(RichText::new(txt).size(9.0).color(col));
            });
        });

        if config.analysis.hint_mode {
            ui.add_space(3.0);
            let t = ui.ctx().input(|i| i.time) as f32;
            let pulse = (t * 2.0).sin() * 0.5 + 0.5;
            let alpha = (pulse * 80.0 + 120.0) as u8;
            ui.horizontal(|ui| {
                ui.add(
                    egui::Button::new(
                        RichText::new("◉ HINT MODE ON — hold key to peek")
                            .size(9.0)
                            .color(egui::Color32::from_rgba_unmultiplied(255, 200, 60, alpha))
                            .strong(),
                    )
                    .fill(egui::Color32::from_rgba_unmultiplied(220, 140, 40, 25))
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(220, 140, 40, 160)))
                    .corner_radius(3.0)
                    .min_size(egui::vec2(ui.available_width(), 20.0))
                    .sense(egui::Sense::hover()),
                )
                .on_hover_text(format!(
                    "Hint mode is ON — arrows are hidden.\nHold [{}] to reveal the best move.\nDisable in Settings → Overlay.",
                    crate::config::vk_to_label(config.hotkeys.hint_hold)
                ));
            });
        }

        if is_analysing {
            if let Some(bm) = &analysis.best_move {
                ui.add_space(3.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("BEST MOVE").size(9.0).color(theme::TEXT_MUTED));
                    ui.add_space(6.0);
                    ui.label(RichText::new(bm).size(13.0).color(theme::ACCENT_PRIMARY).strong());
                });
            }
        }

        if let Some((eco, name)) = current_opening {
            ui.add_space(3.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(RichText::new(eco).size(9.0).color(theme::ACCENT_PRIMARY).strong());
                ui.label(RichText::new(name).size(9.0).color(theme::TEXT_MUTED));
            });
        }

        ui.add_space(4.0);
        ui.separator();
        ui.add_space(2.0);
        render_chrome_section(ui, config, chrome_status, commands, false);
    });

    ui.add_space(4.0);

    widgets::double_border_frame(ui, "GAME", accent, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("YOU").size(9.0).color(theme::TEXT_MUTED));
            ui.add_space(6.0);
            let mut username = config.username.clone();
            let r = ui.add(
                egui::TextEdit::singleline(&mut username)
                    .desired_width(180.0)
                    .hint_text("your chess.com username")
                    .font(egui::TextStyle::Small),
            );
            if r.changed() { config.username = username; }
        });
        ui.add_space(6.0);

        let sep = ui.available_rect_before_wrap();
        ui.painter().hline(sep.left()..=sep.right(), sep.top(),
            egui::Stroke::new(1.0, theme::BORDER_DEFAULT));
        ui.add_space(4.0);

        let wn = player_white.unwrap_or("—");
        let bn = player_black.unwrap_or("—");
        let wt = game_time_white.unwrap_or("—");
        let bt = game_time_black.unwrap_or("—");

        for (symbol, name, time, sym_col) in [
            ("■", bn, bt, egui::Color32::from_rgb(60, 60, 70)),
            ("□", wn, wt, egui::Color32::from_rgb(230, 230, 220)),
        ] {
            ui.horizontal(|ui| {
                ui.label(RichText::new(symbol).size(10.0).color(sym_col));
                ui.add_space(4.0);
                ui.label(RichText::new(name).size(11.0).color(theme::TEXT_PRIMARY).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(time).size(12.0).color(theme::TEXT_HEADER).strong().monospace());
                });
            });
            ui.add_space(3.0);
        }
    });

    ui.add_space(4.0);

    widgets::double_border_frame(ui, "ENGINE LINES", accent, |ui| {
        // Engine name + status row
        let engine_name = std::path::Path::new(&config.engine.path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_uppercase())
            .unwrap_or_else(|| "—".into());

        ui.horizontal(|ui| {
            let status_col = if engine_status.contains("error") || engine_status.contains("restarting") {
                theme::ACCENT_DANGER
            } else if engine_status.contains("ready") || engine_status.contains("analysing") {
                theme::ACCENT_SUCCESS
            } else {
                theme::TEXT_MUTED
            };
            ui.label(RichText::new(&engine_name).size(10.0).color(theme::TEXT_HEADER).strong());
            ui.add_space(6.0);
            ui.label(RichText::new(engine_status).size(9.0).color(status_col));

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(
                    egui::Button::new(RichText::new("⟳ RESTART").size(9.0).color(theme::ACCENT_WARNING))
                        .fill(egui::Color32::from_rgba_unmultiplied(220, 140, 40, 20))
                        .stroke(egui::Stroke::new(1.0, theme::ACCENT_WARNING))
                        .corner_radius(3.0)
                        .min_size(egui::vec2(68.0, 18.0)),
                )
                .on_hover_text("Kill and respawn the engine process")
                .clicked() {
                    commands.push(MenuCommand::RestartEngine);
                }
            });
        });
        ui.add_space(4.0);

        if analysis.lines.is_empty() {
            ui.label(RichText::new("WAITING FOR ENGINE…").size(10.0).color(theme::TEXT_MUTED).italics());
        } else {
            for line in &analysis.lines {
                widgets::pv_row_large(ui, line.rank, &line.score_display, line.centipawns, &line.pv, accent);
            }
        }
    });

    ui.add_space(4.0);

    widgets::double_border_frame(ui, "MOVE HISTORY", accent, |ui| {
        if move_history.is_empty() {
            ui.label(RichText::new("NO MOVES YET…").size(9.0).color(theme::TEXT_MUTED).italics());
        } else {
            let mut i = 0u32;
            let mut mnum = 1u32;
            while (i as usize) < move_history.len() {
                let white = &move_history[i as usize];
                let black = move_history.get(i as usize + 1).map(String::as_str).unwrap_or("");
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{mnum}.")).size(9.0).color(theme::TEXT_MUTED));
                    ui.add_space(2.0);
                    ui.label(RichText::new(white.as_str()).size(10.0).color(theme::TEXT_PRIMARY).strong());
                    if !black.is_empty() {
                        ui.add_space(6.0);
                        ui.label(RichText::new(black).size(10.0).color(theme::TEXT_SECONDARY));
                    }
                });
                i += 2;
                mnum += 1;
            }
        }
    });
}

// ── Quick navigation (branded, select-then-launch) ─────────────────────────────

fn render_page_buttons(
    ui: &mut egui::Ui,
    commands: &mut Vec<MenuCommand>,
    selected: &mut Option<(String, String)>,
) {
    let lichess_col = egui::Color32::from_rgb(190, 230, 255);
    let chesscom_col = egui::Color32::from_rgb(129, 182, 76);

    // ── Lichess ──────────────────────────────────────────────────────────────
    nav_logo_header(
        ui, "bytes://lichess.svg", LICHESS_LOGO,
        "LICHESS", "https://lichess.org/", lichess_col, selected,
    );
    ui.add_space(3.0);
    nav_mode_row(
        ui, lichess_col, selected,
        &[
            ("Play",   "https://lichess.org/?any#hook"),
            ("Puzzle", "https://lichess.org/training"),
            ("Streak", "https://lichess.org/streak"),
            ("Storm",  "https://lichess.org/storm"),
            ("Racer",  "https://lichess.org/racer"),
        ],
    );

    ui.add_space(8.0);
    let sep = ui.available_rect_before_wrap();
    ui.painter().hline(sep.left()..=sep.right(), sep.top(),
        egui::Stroke::new(1.0, theme::BORDER_DEFAULT));
    ui.add_space(8.0);

    // ── Chess.com ─────────────────────────────────────────────────────────────
    nav_logo_header(
        ui, "bytes://chesscom.svg", CHESSCOM_LOGO,
        "CHESS.COM", "https://www.chess.com/home", chesscom_col, selected,
    );
    ui.add_space(3.0);
    nav_mode_row(
        ui, chesscom_col, selected,
        &[
            ("Live",        "https://www.chess.com/play/online"),
            ("vs Computer", "https://www.chess.com/play/computer"),
            ("Puzzle",      "https://www.chess.com/puzzles"),
            ("Daily",       "https://www.chess.com/daily"),
            ("Rush",        "https://www.chess.com/puzzles/rush"),
            ("Battle",      "https://www.chess.com/puzzles/battle"),
        ],
    );

    ui.add_space(8.0);

    // ── Launch selected ────────────────────────────────────────────────────────
    match selected.as_ref() {
        Some((label, _)) => {
            ui.label(RichText::new(format!("▸ {label}")).size(9.5)
                .color(theme::TEXT_SECONDARY));
        }
        None => {
            ui.label(RichText::new("Select a logo or mode to launch")
                .size(9.5).color(theme::TEXT_MUTED).italics());
        }
    }
    ui.add_space(3.0);

    let enabled = selected.is_some();
    let launch_btn = egui::Button::new(
        RichText::new("▶  LAUNCH").size(11.0)
            .color(if enabled { theme::BG_DARK } else { theme::TEXT_MUTED }).strong())
        .fill(if enabled { theme::ACCENT_PRIMARY } else { egui::Color32::from_rgb(40, 44, 52) })
        .corner_radius(4.0)
        .min_size(egui::vec2(ui.available_width(), 26.0));
    if ui.add_enabled(enabled, launch_btn).clicked() {
        if let Some((_, url)) = selected.as_ref() {
            commands.push(MenuCommand::LaunchUrl(Some(url.clone())));
        }
    }
}

/// A branded site header: clickable logo + name that selects the site's main page.
fn nav_logo_header(
    ui:       &mut egui::Ui,
    uri:      &'static str,
    bytes:    &'static [u8],
    name:     &str,
    main_url: &str,
    col:      egui::Color32,
    selected: &mut Option<(String, String)>,
) {
    let is_sel = selected.as_ref().is_some_and(|(_, u)| u == main_url);
    let fill = if is_sel {
        egui::Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), 36)
    } else {
        egui::Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), 12)
    };
    let stroke_a = if is_sel { 160 } else { 70 };
    let img = egui::Image::new(egui::ImageSource::Bytes {
        uri:   uri.into(),
        bytes: bytes.into(),
    })
    .fit_to_exact_size(egui::vec2(22.0, 22.0));

    let btn = egui::Button::image_and_text(
        img,
        RichText::new(format!("  {name}")).size(13.0).color(col).strong(),
    )
    .fill(fill)
    .stroke(egui::Stroke::new(if is_sel { 1.5 } else { 1.0 },
        egui::Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), stroke_a)))
    .corner_radius(5.0)
    .min_size(egui::vec2(ui.available_width(), 32.0));

    if ui.add(btn).on_hover_text(format!("Select {name} home — {main_url}")).clicked() {
        *selected = Some((format!("{name} · Home"), main_url.to_string()));
    }
}

/// A wrapped row of selectable mode chips for one site.
fn nav_mode_row(
    ui:       &mut egui::Ui,
    col:      egui::Color32,
    selected: &mut Option<(String, String)>,
    modes:    &[(&str, &str)],
) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
        for (label, url) in modes {
            let is_sel = selected.as_ref().is_some_and(|(_, u)| u == url);
            let fill = if is_sel {
                egui::Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), 40)
            } else {
                egui::Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), 14)
            };
            let stroke_a = if is_sel { 170 } else { 60 };
            let btn = egui::Button::new(RichText::new(*label).size(9.5)
                    .color(col).strong())
                .fill(fill)
                .stroke(egui::Stroke::new(if is_sel { 1.5 } else { 1.0 },
                    egui::Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), stroke_a)))
                .corner_radius(3.0)
                .min_size(egui::vec2(0.0, 18.0));
            if ui.add(btn).on_hover_text(*url).clicked() {
                *selected = Some(((*label).to_string(), (*url).to_string()));
            }
        }
    });
}

// ── Chrome section ────────────────────────────────────────────────────────────

fn render_chrome_section(
    ui:             &mut egui::Ui,
    config:         &mut Config,
    chrome_status:  &str,
    commands:       &mut Vec<MenuCommand>,
    show_path_edit: bool,
) {
    ui.horizontal(|ui| {
        if ui.add(
            egui::Button::new(RichText::new("⟳ REFRESH").size(10.0).color(theme::ACCENT_INFO))
                .fill(egui::Color32::from_rgba_unmultiplied(80, 160, 220, 30))
                .stroke(egui::Stroke::new(1.0, theme::ACCENT_INFO))
                .corner_radius(3.0)
                .min_size(egui::vec2(76.0, 22.0)),
        )
        .on_hover_text("Reconnect to Chrome and reinstall board observer")
        .clicked() {
            commands.push(MenuCommand::ReconnectCdp);
        }

        if !chrome_status.is_empty() {
            ui.add_space(4.0);
            let col = if chrome_status.contains("reconnecting") {
                theme::ACCENT_INFO
            } else if chrome_status.contains('✓') || chrome_status == "reconnected" {
                theme::ACCENT_SUCCESS
            } else if chrome_status.contains("Already") {
                theme::ACCENT_INFO
            } else {
                theme::ACCENT_DANGER
            };
            ui.label(RichText::new(chrome_status).size(9.0).color(col));
        }
    });

    if show_path_edit {
        ui.add_space(4.0);
        ui.label(RichText::new("chrome path").size(9.0).color(theme::TEXT_MUTED));
        let mut path = config.cdp.chrome_path.clone();
        let r = ui.add(
            egui::TextEdit::singleline(&mut path)
                .desired_width(ui.available_width())
                .hint_text("e.g. C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe")
                .font(egui::TextStyle::Small),
        );
        if r.changed() { config.cdp.chrome_path = path; }

        ui.add_space(2.0);
        let mut extra = config.cdp.chrome_extra_args.clone();
        let r2 = ui.add(
            egui::TextEdit::singleline(&mut extra)
                .desired_width(ui.available_width())
                .hint_text("extra launch args (optional)")
                .font(egui::TextStyle::Small),
        );
        if r2.changed() { config.cdp.chrome_extra_args = extra; }
    }
}

// ── Settings tab ──────────────────────────────────────────────────────────────

fn render_settings_tab(
    ui:     &mut egui::Ui,
    m:      &mut MenuContext,
    accent: egui::Color32,
) {
    let config        = &mut *m.config;
    let flipped       = &mut *m.flipped;
    let chrome_status = m.chrome_status;
    let commands      = &mut m.commands;

    widgets::double_border_frame(ui, "BOARD", accent, |ui| {
        widgets::styled_toggle(ui, flipped, "Flip board", None);
        ui.add_space(2.0);
        let mut dl = config.analysis.display_lines as f32;
        widgets::styled_slider(ui, "Arrows on board", &mut dl, 1.0..=3.0, "");
        config.analysis.display_lines = dl.round().clamp(1.0, 3.0) as u32;
        let mut mpv = config.analysis.multipv as f32;
        widgets::styled_slider(ui, "Engine lines (MultiPV)", &mut mpv, 1.0..=5.0, "");
        config.analysis.multipv = mpv.round().clamp(1.0, 5.0) as u32;
        if config.analysis.multipv < config.analysis.display_lines {
            config.analysis.multipv = config.analysis.display_lines;
        }
    });

    ui.add_space(4.0);

    widgets::double_border_frame(ui, "OVERLAY", accent, |ui| {
        let r = widgets::styled_toggle(ui, &mut config.analysis.overlay_enabled, "Arrows / highlights",
            Some(&format!("[{}]", vk_to_label(config.hotkeys.toggle_overlay))));
        r.on_hover_text("Show engine move arrows on the board");
        ui.add_space(2.0);
        let r = widgets::styled_toggle(ui, &mut config.analysis.discrete_mode, "Discrete mode",
            Some(&format!("[{}]", vk_to_label(config.hotkeys.toggle_discrete))));
        r.on_hover_text("Hides arrows — tints the from/to squares like a native highlight instead");
        ui.add_space(2.0);
        let r = widgets::styled_toggle(ui, &mut config.analysis.hint_mode, "Hint mode (hold to peek)",
            Some(&format!("[hold {}]", vk_to_label(config.hotkeys.hint_hold))));
        r.on_hover_text("Overlay stays hidden. Hold the hint key to reveal the best move briefly.");
        ui.add_space(2.0);
        let r = widgets::styled_toggle(ui, &mut config.analysis.show_eval_bar, "Eval bar", None);
        r.on_hover_text("Vertical advantage bar drawn to the left of the board");
        ui.add_space(2.0);
        let r = widgets::styled_toggle(ui, &mut config.analysis.show_opening_name, "Opening name", None);
        r.on_hover_text("Show the current opening name on the board overlay");
    });

    ui.add_space(4.0);

    widgets::double_border_frame(ui, "ENGINE PATH", accent, |ui| {
        let mut path = config.engine.path.clone();
        let r = ui.add(
            egui::TextEdit::singleline(&mut path)
                .desired_width(ui.available_width())
                .hint_text(r"path to engine .exe  (stockfish, analysis.exe, etc.)")
                .font(egui::TextStyle::Small),
        );
        if r.changed() { config.engine.path = path; }
        ui.add_space(4.0);

        let exists = std::path::Path::new(&config.engine.path).exists();
        if exists {
            let display_name = std::path::Path::new(&config.engine.path)
                .file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let is_obvious = display_name.to_lowercase().starts_with("stockfish");

            ui.horizontal(|ui| {
                ui.label(RichText::new("✓").size(9.0).color(theme::ACCENT_SUCCESS).strong());
                let col = if is_obvious { theme::ACCENT_WARNING } else { theme::TEXT_MUTED };
                ui.label(RichText::new(format!("Task Manager: {display_name}")).size(8.5).color(col))
                    .on_hover_text(if is_obvious {
                        "Rename to analysis.exe or similar to be less obvious in Task Manager"
                    } else {
                        "This name appears in Task Manager — looks like a system process"
                    });
            });

            ui.add_space(4.0);
            let disguise_target = {
                let local = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".\\data".into());
                std::path::PathBuf::from(local).join("Microsoft\\AudioSrv\\AudioEndpointSrv.exe")
            };
            let already_disguised = config.engine.path == disguise_target.to_string_lossy();

            if already_disguised {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("+ Disguised as AudioEndpointSrv.exe")
                        .size(8.5).color(theme::ACCENT_SUCCESS));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(
                            egui::Button::new(RichText::new("Unhide").size(9.0)
                                .color(theme::ACCENT_WARNING))
                                .fill(egui::Color32::from_rgba_unmultiplied(220, 140, 40, 20))
                                .stroke(egui::Stroke::new(1.0, theme::ACCENT_WARNING))
                                .corner_radius(3.0),
                        )
                        .on_hover_text("Copies AudioEndpointSrv.exe back to stockfish.exe next to the disguised binary, then points the path there")
                        .clicked() {
                            let original_name = if config.engine.original_name.is_empty() {
                                "engine.exe".to_string()
                            } else {
                                config.engine.original_name.clone()
                            };
                            let restore_path = disguise_target.parent()
                                .map(|p| p.join(&original_name))
                                .unwrap_or_else(|| std::path::PathBuf::from(&original_name));
                            if let Ok(_) = std::fs::copy(&disguise_target, &restore_path) {
                                config.engine.original_name = String::new();
                                config.engine.path = restore_path.to_string_lossy().into_owned();
                            }
                        }
                    });
                });
            } else {
                if ui.add(
                    egui::Button::new(RichText::new("[>] Disguise as audio service").size(9.5)
                        .color(theme::BG_DARK).strong())
                        .fill(egui::Color32::from_rgb(70, 120, 185))
                        .corner_radius(3.0)
                        .min_size(egui::vec2(180.0, 20.0)),
                )
                .on_hover_text(format!("Copies engine to\n{}\nSo it shows as an audio service in Task Manager",
                    disguise_target.display()))
                .clicked() {
                    if let Some(p) = disguise_target.parent() { let _ = std::fs::create_dir_all(p); }
                    if let Ok(_) = std::fs::copy(&config.engine.path, &disguise_target) {
                        config.engine.original_name = std::path::Path::new(&config.engine.path)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "engine.exe".into());
                        config.engine.path = disguise_target.to_string_lossy().into_owned();
                    }
                }
            }
        } else {
            ui.horizontal(|ui| {
                ui.label(RichText::new("⚠ Not found").size(9.0).color(theme::ACCENT_DANGER).strong());
                ui.add_space(6.0);
                ui.label(RichText::new("See About tab for engine downloads.").size(8.5).color(theme::TEXT_MUTED));
            });
        }
    });

    ui.add_space(4.0);

    render_browser_section(ui, config, chrome_status, commands);

    ui.add_space(4.0);

    render_stealth_section(ui, config, accent);
}

// ── Stealth / capture exclusion ────────────────────────────────────────────────

fn render_stealth_section(ui: &mut egui::Ui, config: &mut Config, accent: egui::Color32) {
    widgets::double_border_frame(ui, "CAPTURE EXCLUSION", accent, |ui| {
        let active = hidden::stealth::CAPTURE_EXCLUSION_ACTIVE.load(Ordering::Relaxed);

        ui.horizontal(|ui| {
            let (col, txt) = if active { (theme::ACCENT_SUCCESS, "● ACTIVE") }
                             else      { (theme::TEXT_MUTED,     "○ INACTIVE") };
            ui.label(RichText::new(txt).size(9.0).color(col).strong())
                .on_hover_text(if active {
                    "Overlay is excluded from OBS, ShadowPlay, and BitBlt screen recorders"
                } else {
                    "Overlay will appear in screen recordings"
                });
        });
        ui.add_space(4.0);

        let r = widgets::styled_toggle(
            ui, &mut config.window.capture_exclusion, "Hide from screen capture", None,
        );
        r.on_hover_text("Applies WDA_EXCLUDEFROMCAPTURE — invisible to OBS/ShadowPlay, visible on your display");

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let en_col = if active { theme::TEXT_MUTED } else { theme::BG_DARK };
            let en_fill = if active {
                egui::Color32::from_rgba_unmultiplied(40, 40, 55, 80)
            } else {
                theme::ACCENT_SUCCESS
            };
            if ui.add(
                egui::Button::new(RichText::new("[ON] ENABLE").size(10.0).color(en_col).strong())
                    .fill(en_fill)
                    .stroke(egui::Stroke::new(1.0, theme::ACCENT_SUCCESS))
                    .corner_radius(3.0)
                    .min_size(egui::vec2(110.0, 22.0)),
            )
            .on_hover_text("Apply WDA_EXCLUDEFROMCAPTURE immediately")
            .clicked() && !active {
                hidden::stealth::enable_capture_exclusion("RustyChess");
                config.window.capture_exclusion = true;
            }

            ui.add_space(4.0);

            let dis_col = if active { theme::ACCENT_DANGER } else { theme::TEXT_MUTED };
            let dis_stroke = if active { theme::ACCENT_DANGER } else { theme::BORDER_DEFAULT };
            if ui.add(
                egui::Button::new(RichText::new("[OFF] DISABLE").size(10.0).color(dis_col))
                    .fill(egui::Color32::from_rgba_unmultiplied(60, 20, 20, 80))
                    .stroke(egui::Stroke::new(1.0, dis_stroke))
                    .corner_radius(3.0)
                    .min_size(egui::vec2(110.0, 22.0)),
            )
            .on_hover_text("Remove capture exclusion — overlay will appear in recordings")
            .clicked() && active {
                hidden::stealth::disable_capture_exclusion("RustyChess");
                config.window.capture_exclusion = false;
            }
        });
    });
}

// ── Hotkeys ─────────────────────────────────────────────────────────────────--

/// Render a single hotkey row: label on the left, ComboBox dropdown on the right.
/// `id` must be unique per row.
fn key_row(ui: &mut egui::Ui, id: &str, desc: &str, vk: &mut u32) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(desc).size(10.0).color(theme::TEXT_LABEL));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let current = vk_to_option_label(*vk);

            // Override the transparent-overlay visuals locally so the ComboBox
            // popup frame renders with a solid background.  These only affect
            // this scope because egui clones the style per-ui.
            ui.visuals_mut().window_fill      = theme::BG_MEDIUM;
            ui.visuals_mut().panel_fill       = theme::BG_MEDIUM;
            ui.visuals_mut().extreme_bg_color = theme::BG_DARK;
            ui.visuals_mut().widgets.inactive.weak_bg_fill =
                egui::Color32::from_rgba_unmultiplied(45, 45, 60, 255);
            ui.visuals_mut().widgets.hovered.weak_bg_fill =
                egui::Color32::from_rgba_unmultiplied(65, 65, 85, 255);
            ui.visuals_mut().selection.bg_fill =
                egui::Color32::from_rgba_unmultiplied(220, 180, 90, 60);

            egui::ComboBox::from_id_salt(id)
                .selected_text(RichText::new(current).size(10.0)
                    .color(theme::ACCENT_PRIMARY).strong())
                .width(78.0)
                .show_ui(ui, |ui| {
                    // Same solid-background overrides inside the popup content.
                    ui.visuals_mut().window_fill      = theme::BG_MEDIUM;
                    ui.visuals_mut().panel_fill       = theme::BG_MEDIUM;
                    ui.visuals_mut().extreme_bg_color = theme::BG_DARK;
                    ui.visuals_mut().widgets.inactive.weak_bg_fill =
                        egui::Color32::from_rgba_unmultiplied(45, 45, 60, 255);
                    ui.visuals_mut().widgets.hovered.weak_bg_fill =
                        egui::Color32::from_rgba_unmultiplied(70, 70, 90, 255);

                    for (label, code) in KEY_OPTIONS {
                        let selected = *code == *vk;
                        let resp = ui.selectable_label(
                            selected,
                            RichText::new(*label).size(10.0)
                                .color(if selected { theme::ACCENT_PRIMARY } else { theme::TEXT_LABEL }),
                        );
                        if resp.clicked() { *vk = *code; }
                        // NOTE: no scroll_to_me here — calling it every frame
                        // while the popup is open tells the *outer* menu
                        // ScrollArea to jump, causing the springboard effect.
                    }
                });
        });
    });
    ui.add_space(3.0);
}

fn render_hotkeys_section(
    ui:     &mut egui::Ui,
    config: &mut Config,
    accent: egui::Color32,
) {
    widgets::double_border_frame(ui, "HOTKEYS", accent, |ui| {
        key_row(ui, "hk_toggle_menu",  "Menu",        &mut config.hotkeys.toggle_menu);
        key_row(ui, "hk_flip_board",   "Flip board",  &mut config.hotkeys.flip_board);
        key_row(ui, "hk_discrete",     "Discrete",    &mut config.hotkeys.toggle_discrete);
        key_row(ui, "hk_overlay",      "Overlay",     &mut config.hotkeys.toggle_overlay);
        key_row(ui, "hk_exit",         "Exit",        &mut config.hotkeys.exit);
        key_row(ui, "hk_reconnect",    "Reconnect",   &mut config.hotkeys.reconnect_cdp);
        key_row(ui, "hk_hint_hold",    "Hint (hold)", &mut config.hotkeys.hint_hold);
    });
}

// ── Hotkeys tab ───────────────────────────────────────────────────────────────

fn render_hotkeys_tab(ui: &mut egui::Ui, config: &mut Config, accent: egui::Color32) {
    render_hotkeys_section(ui, config, accent);

    ui.add_space(6.0);
    ui.label(RichText::new("Pick any key for each action. Changes apply immediately.")
        .size(8.5).color(theme::TEXT_MUTED).italics());
}

// ── Engine tab ────────────────────────────────────────────────────────────────

fn render_engine_tab(
    ui:            &mut egui::Ui,
    config:        &mut Config,
    accent:        egui::Color32,
) {
    widgets::double_border_frame(ui, "RESOURCES", accent, |ui| {
        let mut hash = config.engine.hash_mb as f32;
        widgets::styled_slider(ui, "Hash size", &mut hash, 64.0..=2048.0, "MB");
        config.engine.hash_mb = hash.round() as u32;

        let mut threads = config.engine.threads as f32;
        widgets::styled_slider(ui, "Threads", &mut threads, 1.0..=16.0, "");
        config.engine.threads = threads.round() as u32;
    });

    ui.add_space(4.0);

    widgets::double_border_frame(ui, "ANALYSIS", accent, |ui| {
        let mut nodes = config.analysis.nodes as f32;
        widgets::styled_slider(ui, "Nodes/move  (0 = unlimited)", &mut nodes, 0.0..=5000.0, "");
        config.analysis.nodes = nodes.round() as u32;
        ui.label(
            egui::RichText::new(if config.analysis.nodes == 0 {
                "  unlimited — may pin CPU/GPU (use 800–2000 for lc0)".to_string()
            } else {
                format!("  ~{} nodes per move", config.analysis.nodes)
            })
            .size(8.0)
            .color(if config.analysis.nodes == 0 { theme::ACCENT_WARNING } else { theme::ACCENT_SUCCESS }),
        );

        ui.add_space(4.0);

        let mut depth = config.analysis.depth as f32;
        widgets::styled_slider(ui, "Depth  (0 = infinite, ignored when nodes set)", &mut depth, 0.0..=30.0, "");
        config.analysis.depth = depth.round() as u32;

        let mut mpv = config.analysis.multipv as f32;
        widgets::styled_slider(ui, "MultiPV lines", &mut mpv, 1.0..=5.0, "");
        config.analysis.multipv = mpv.round().clamp(1.0, 5.0) as u32;

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(2.0);
        ui.label(RichText::new("GAME REVIEW").size(8.5).color(theme::TEXT_MUTED).strong());
        ui.add_space(2.0);
        let mut rdepth = config.analysis.review_depth as f32;
        widgets::styled_slider(ui, "Review depth (manual analysis)", &mut rdepth, 10.0..=30.0, "");
        config.analysis.review_depth = rdepth.round().clamp(10.0, 30.0) as u32;
        ui.label(
            egui::RichText::new("  higher = more accurate move classifications, but slower")
                .size(8.0)
                .color(theme::TEXT_MUTED),
        );
    });
}

// ── Browser / CDP section (shared) ──────────────────────────────────────────--

fn render_browser_section(
    ui:            &mut egui::Ui,
    config:        &mut Config,
    chrome_status: &str,
    commands:      &mut Vec<MenuCommand>,
) {
    widgets::double_border_frame(ui, "CDP / BROWSER", theme::ACCENT_PRIMARY, |ui| {
        use crate::vision::chrome_launcher::BrowserKind;

        // Detect browser from the currently configured path
        let kind = BrowserKind::detect(&config.cdp.chrome_path);
        let (kind_col, kind_txt) = match kind {
            BrowserKind::Edge     => (theme::ACCENT_INFO,    "Microsoft Edge"),
            BrowserKind::Brave    => (egui::Color32::from_rgb(255, 140, 60), "Brave"),
            BrowserKind::Chromium => (theme::TEXT_SECONDARY, "Chromium"),
            BrowserKind::Chrome   => (theme::ACCENT_SUCCESS, "Google Chrome"),
            BrowserKind::Other    => (theme::TEXT_MUTED,     "Unknown browser"),
        };

        ui.horizontal(|ui| {
            ui.label(RichText::new("Browser:").size(8.5).color(theme::TEXT_MUTED));
            ui.label(RichText::new(kind_txt).size(9.0).color(kind_col).strong())
                .on_hover_text("Detected from the path below. Chrome, Edge, Brave, Opera, Vivaldi all work.");

            // Show profile path as a small ℹ badge
            let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
            if !local.is_empty() {
                let sub = match kind {
                    BrowserKind::Edge     => "rustychess-edge",
                    BrowserKind::Brave    => "rustychess-brave",
                    BrowserKind::Chromium => "rustychess-chromium",
                    _                     => "rustychess-chrome",
                };
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("ℹ profile").size(8.0).color(theme::TEXT_MUTED))
                        .on_hover_text(format!(
                            "Isolated profile (separate from your real browser):\n\
                             {}\\rustychess-cdp-profiles\\{}\n\n\
                             chess.com cannot read this directory.\n\
                             Persists between sessions — you stay logged in.",
                            local, sub
                        ));
                });
            }
        });
        ui.add_space(5.0);

        ui.horizontal(|ui| {
            ui.label(RichText::new("endpoint:").size(9.0).color(theme::TEXT_MUTED));
            ui.label(RichText::new(&config.cdp.endpoint).size(9.0).color(theme::ACCENT_INFO));
        });

        ui.add_space(4.0);

        let mut endpoint = config.cdp.endpoint.clone();
        let r = ui.add(
            egui::TextEdit::singleline(&mut endpoint)
                .desired_width(ui.available_width())
                .hint_text("http://127.0.0.1:9222")
                .font(egui::TextStyle::Small),
        );
        if r.changed() { config.cdp.endpoint = endpoint; }

        ui.add_space(4.0);
        let mut poll = config.cdp.poll_interval_ms as f32;
        widgets::styled_slider(ui, "poll interval", &mut poll, 100.0..=2000.0, "ms");
        config.cdp.poll_interval_ms = poll.round() as u64;

        ui.add_space(4.0);
        ui.label(RichText::new(format!("LAUNCH {}", kind_txt.to_uppercase()))
            .size(9.0).color(theme::TEXT_MUTED).strong());
        ui.add_space(2.0);
        render_chrome_section(ui, config, chrome_status, commands, true);
    });
}

// ── Game Review tab ───────────────────────────────────────────────────────────

fn render_game_review_tab(ui: &mut egui::Ui, m: &mut MenuContext, _accent: egui::Color32) {
    let my_username  = m.config.username.to_ascii_lowercase();
    let review_depth = m.config.analysis.review_depth;
    let reviewing    = m.reviewing;
    let review_error = m.review_error;
    let game_store   = &mut *m.game_store;
    let expanded     = &mut *m.expanded_game;
    let commands     = &mut m.commands;

    // Header row
    ui.horizontal(|ui| {
        let count = game_store.games.len();
        let label = if count == 1 { "1 game stored".to_string() }
                    else          { format!("{count} games stored") };
        ui.label(RichText::new(format!("♟ {label}")).size(10.0).color(theme::TEXT_MUTED));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !game_store.games.is_empty() {
                if ui.add(
                    egui::Button::new(RichText::new("✕ CLEAR ALL").size(9.0).color(theme::ACCENT_DANGER))
                        .fill(egui::Color32::from_rgba_unmultiplied(200, 70, 70, 20))
                        .stroke(egui::Stroke::new(1.0, theme::ACCENT_DANGER))
                        .corner_radius(3.0),
                ).on_hover_text("Delete all saved games (cannot be undone)").clicked() {
                    game_store.clear();
                    *expanded = None;
                }
            }
        });
    });

    ui.add_space(4.0);

    if game_store.games.is_empty() {
        ui.add_space(16.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("No games recorded yet.").size(10.0).color(theme::TEXT_MUTED).italics());
            ui.add_space(4.0);
            ui.label(RichText::new("Games are saved automatically when they end.")
                .size(9.0).color(theme::TEXT_MUTED).italics());
        });
        return;
    }

    let ids: Vec<u64> = game_store.games.iter().map(|g| g.id).collect();
    let mut delete_id: Option<u64> = None;

    for id in ids {
        let Some(idx) = game_store.games.iter().position(|g| g.id == id) else { continue };
        let is_open = *expanded == Some(id);

        // Read-only view of the game for rendering header.
        let (site_badge, site_col, white, black, opening_str, move_count, result_txt, result_col, played_at, start_mnum) = {
            let g = &game_store.games[idx];
            let you_are_white = !my_username.is_empty()
                && g.white.to_ascii_lowercase().contains(&my_username);
            let (rc, rt) = match g.result.colour(you_are_white) {
                crate::game_store::GameResultColour::Win     => (theme::ACCENT_SUCCESS, g.result.display()),
                crate::game_store::GameResultColour::Loss    => (theme::ACCENT_DANGER,  g.result.display()),
                crate::game_store::GameResultColour::Draw    => (theme::ACCENT_WARNING, g.result.display()),
                crate::game_store::GameResultColour::Neutral => (theme::TEXT_MUTED,     g.result.display()),
            };
            let badge: Option<&str> = match g.site {
                GameSite::Lichess  => Some("lichess"),
                GameSite::ChessCom => Some("chess.com"),
                _                  => None,
            };
            let scol  = match g.site {
                GameSite::Lichess  => egui::Color32::from_rgb(150, 210, 255),
                GameSite::ChessCom => egui::Color32::from_rgb(129, 182, 76),
                _                  => theme::TEXT_MUTED,
            };
            let op = g.opening.as_ref()
                .map(|(eco, name)| format!("{eco} {name}"))
                .unwrap_or_default();
            (badge, scol, g.white.clone(), g.black.clone(), op, g.move_count(), rt, rc, g.played_at.clone(), g.moves.first().and_then(|s| s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u32>().ok()).unwrap_or(1))
        };

        let border_col = if is_open {
            site_col
        } else {
            egui::Color32::from_rgba_unmultiplied(site_col.r(), site_col.g(), site_col.b(), 90)
        };
        let fill_col = if is_open {
            egui::Color32::from_rgba_unmultiplied(
                (site_col.r() / 6).saturating_add(28),
                (site_col.g() / 6).saturating_add(28),
                (site_col.b() / 6).saturating_add(28),
                210,
            )
        } else {
            egui::Color32::from_rgba_unmultiplied(28, 28, 42, 180)
        };
        egui::Frame::new()
            .fill(fill_col)
            .stroke(egui::Stroke::new(if is_open { 1.5 } else { 1.0 }, border_col))
            .corner_radius(4.0)
            .inner_margin(egui::Margin::symmetric(8, 5))
            .show(ui, |ui| {
                let header_inner = ui.horizontal(|ui| {
                    let arrow = if is_open { "▼" } else { "▶" };
                    ui.label(RichText::new(arrow).size(11.0).color(site_col).strong());
                    ui.add_space(2.0);
                    if let Some(b) = site_badge {
                        ui.label(RichText::new(b).size(8.5).color(site_col).strong());
                        ui.add_space(4.0);
                    }
                    ui.label(RichText::new(&white).size(10.0).color(egui::Color32::from_rgb(230,230,220)).strong());
                    ui.label(RichText::new(" vs ").size(9.0).color(theme::TEXT_MUTED));
                    ui.label(RichText::new(&black).size(10.0).color(egui::Color32::from_rgb(90,90,100)).strong());
                    let mut del_clicked = false;
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Timestamp (leftmost in RTL = rendered last)
                        ui.label(RichText::new(&played_at).size(8.5).color(theme::TEXT_MUTED));
                        ui.add_space(4.0);
                        // Delete button (rightmost in RTL = rendered first)
                        if ui.add(
                            egui::Button::new(
                                RichText::new("✕").size(8.5)
                                    .color(theme::ACCENT_DANGER)
                            )
                            .fill(egui::Color32::TRANSPARENT)
                            .stroke(egui::Stroke::new(
                                1.0,
                                egui::Color32::from_rgba_unmultiplied(215, 80, 80, 80),
                            ))
                            .corner_radius(3.0)
                            .min_size(egui::vec2(16.0, 14.0)),
                        ).on_hover_text("Delete this game").clicked() {
                            del_clicked = true;
                        }
                    });
                    del_clicked
                });
                let header_resp = header_inner.response;
                let del_clicked = header_inner.inner;

                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    if !opening_str.is_empty() {
                        ui.label(RichText::new(&opening_str).size(8.5).color(theme::ACCENT_PRIMARY));
                        ui.label(RichText::new("·").size(8.5).color(theme::BORDER_DEFAULT));
                    }
                    ui.label(RichText::new(format!("{move_count} moves")).size(8.5).color(theme::TEXT_MUTED));
                    ui.label(RichText::new("·").size(8.5).color(theme::BORDER_DEFAULT));
                    ui.label(RichText::new(result_txt).size(8.5).color(result_col).strong());
                });

                if del_clicked {
                    delete_id = Some(id);
                } else if header_resp.interact(egui::Sense::click()).clicked()
                    || header_resp.double_clicked()
                {
                    *expanded = if is_open { None } else { Some(id) };
                }

                if is_open {
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(4.0);

                    let moves = game_store.games[idx].moves.clone();
                    let review = game_store.games[idx].review.clone();
                    egui::ScrollArea::vertical()
                        .id_salt(format!("gmv_{id}"))
                        .max_height(140.0)
                        .show(ui, |ui| {
                            fn strip_mv(s: &str) -> &str {
                                if let Some(p) = s.find(". ") {
                                    s[p + 2..].trim()
                                } else if let Some(p) = s.find("\u{2026} ") {
                                    // U+2026 ELLIPSIS = 3 UTF-8 bytes + 1 space = 4 bytes
                                    s[p + 4..].trim()
                                } else {
                                    s.trim()
                                }
                            }
                            let anns = review.as_ref().map(|r| &r.annotations);
                            let mut i = 0usize;
                            let mut mnum = start_mnum;
                            while i < moves.len() {
                                let white_mv = strip_mv(&moves[i]);
                                let black_mv = moves.get(i + 1).map(|s| strip_mv(s)).unwrap_or("");
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(format!("{mnum}.")).size(9.0).color(theme::TEXT_MUTED));
                                    ui.add_space(2.0);
                                    render_move_token(
                                        ui, white_mv,
                                        anns.and_then(|a| a.get(i)),
                                        theme::TEXT_PRIMARY,
                                    );
                                    if !black_mv.is_empty() {
                                        ui.add_space(6.0);
                                        render_move_token(
                                            ui, black_mv,
                                            anns.and_then(|a| a.get(i + 1)),
                                            theme::TEXT_SECONDARY,
                                        );
                                    }
                                });
                                i += 2;
                                mnum += 1;
                            }
                        });

                    ui.add_space(6.0);

                    // ── Review summary (accuracy + class counts) ──────────────
                    if let Some(rev) = &review {
                        let you_are_white = if my_username.is_empty() {
                            None
                        } else if white.to_ascii_lowercase().contains(&my_username) {
                            Some(true)
                        } else if black.to_ascii_lowercase().contains(&my_username) {
                            Some(false)
                        } else {
                            None
                        };
                        render_review_summary(ui, id, rev, &white, &black, you_are_white);
                        ui.add_space(4.0);
                    }

                    // ── Manual result picker (only shown when result is Unknown) ──
                    if game_store.games[idx].result == GameResult::Unknown {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Result:").size(8.5).color(theme::TEXT_MUTED));
                            ui.add_space(4.0);
                            if ui.add(
                                egui::Button::new(RichText::new("White Won").size(8.5).color(theme::TEXT_PRIMARY))
                                    .fill(egui::Color32::from_rgba_unmultiplied(255,255,200,18))
                                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(200,200,160)))
                                    .corner_radius(3.0)
                            ).clicked() {
                                game_store.games[idx].result = GameResult::WhiteWins;
                                game_store.save();
                            }
                            ui.add_space(3.0);
                            if ui.add(
                                egui::Button::new(RichText::new("Black Won").size(8.5).color(theme::TEXT_SECONDARY))
                                    .fill(egui::Color32::from_rgba_unmultiplied(80,80,80,40))
                                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(120,120,120)))
                                    .corner_radius(3.0)
                            ).clicked() {
                                game_store.games[idx].result = GameResult::BlackWins;
                                game_store.save();
                            }
                            ui.add_space(3.0);
                            if ui.add(
                                egui::Button::new(RichText::new("Draw").size(8.5).color(theme::ACCENT_WARNING))
                                    .fill(egui::Color32::from_rgba_unmultiplied(220,180,60,18))
                                    .stroke(egui::Stroke::new(1.0, theme::ACCENT_WARNING))
                                    .corner_radius(3.0)
                            ).clicked() {
                                game_store.games[idx].result = GameResult::Draw;
                                game_store.save();
                            }
                        });
                        ui.add_space(4.0);
                    }

                    // ── Review controls (progress / analyse button) ──────────
                    let is_reviewing_this = reviewing.map(|(rid, _, _)| rid == id).unwrap_or(false);
                    if is_reviewing_this {
                        let (_, done, total) = reviewing.unwrap();
                        let frac = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .desired_height(16.0)
                                .corner_radius(3.0)
                                .text(RichText::new(format!("Analysing… {done}/{total}")).size(8.5)),
                        );
                    } else {
                        let has_review = review.is_some();
                        let label = if has_review {
                            format!("↻ RE-ANALYSE  (depth {review_depth})")
                        } else {
                            format!("🔍 ANALYSE GAME  (depth {review_depth})")
                        };
                        // Disabled while a *different* game is being reviewed.
                        let busy = reviewing.is_some();
                        let btn = egui::Button::new(
                            RichText::new(label).size(9.5).color(theme::ACCENT_SUCCESS),
                        )
                        .fill(egui::Color32::from_rgba_unmultiplied(90, 180, 100, 22))
                        .stroke(egui::Stroke::new(1.0, theme::ACCENT_SUCCESS))
                        .corner_radius(3.0)
                        .min_size(egui::vec2(ui.available_width(), 22.0));
                        if ui.add_enabled(!busy, btn)
                            .on_hover_text("Run engine analysis to classify every move and compute accuracy")
                            .clicked()
                        {
                            commands.push(MenuCommand::AnalyseGame(id));
                        }
                    }

                    // Inline error for this game, if the last review failed.
                    if let Some((eid, msg)) = review_error {
                        if eid == id {
                            ui.add_space(2.0);
                            ui.label(
                                RichText::new(format!("⚠ {msg}"))
                                    .size(8.5).color(theme::ACCENT_DANGER),
                            );
                        }
                    }

                    ui.add_space(4.0);

                    // ── Copy PGN (annotated when a review exists) ─────────────
                    let has_review = review.is_some();
                    let pgn = game_store.games[idx].pgn_annotated();
                    let copy_label = if has_review { "⧉ COPY ANNOTATED PGN" } else { "⧉ COPY PGN" };
                    let copy_hover = if has_review {
                        "Copy PGN with move classifications (NAGs) + eval comments — paste into Lichess/chess.com analysis"
                    } else {
                        "Copy full PGN (headers + moves) to clipboard"
                    };
                    if ui.add(
                        egui::Button::new(RichText::new(copy_label).size(9.5).color(theme::ACCENT_INFO))
                            .fill(egui::Color32::from_rgba_unmultiplied(80, 160, 220, 20))
                            .stroke(egui::Stroke::new(1.0, theme::ACCENT_INFO))
                            .corner_radius(3.0)
                            .min_size(egui::vec2(ui.available_width(), 22.0)),
                    ).on_hover_text(copy_hover).clicked() {
                        ui.ctx().copy_text(pgn);
                    }
                }
            });

        ui.add_space(3.0);
    }

    // Deferred deletion — outside the loop to avoid borrow conflicts.
    if let Some(did) = delete_id {
        game_store.games.retain(|g| g.id != did);
        game_store.save();
        if *expanded == Some(did) { *expanded = None; }
    }
}

/// Render a single move token, tinted and glyph-suffixed by its review class
/// when an annotation is present.
fn render_move_token(
    ui:          &mut egui::Ui,
    san:         &str,
    ann:         Option<&crate::game_store::MoveAnnotation>,
    default_col: egui::Color32,
) {
    match ann {
        Some(a) => {
            let (r, g, b) = a.class.rgb();
            let col = egui::Color32::from_rgb(r, g, b);
            let best = if a.best_san.is_empty() { "—" } else { a.best_san.as_str() };
            ui.label(RichText::new(san).size(10.0).color(col).strong())
                .on_hover_text(format!(
                    "{} ({:+.2})\nBest: {}",
                    a.class.label(),
                    a.cp_after as f32 / 100.0,
                    best,
                ));
            ui.label(RichText::new(a.class.glyph()).size(8.0).color(col));
        }
        None => {
            ui.label(RichText::new(san).size(10.0).color(default_col).strong());
        }
    }
}

/// Render the per-side review summary: accuracy, estimated playing strength,
/// and a full move-classification breakdown laid out as a clean White / Black
/// table with fixed, aligned columns.
fn render_review_summary(
    ui:            &mut egui::Ui,
    id:            u64,
    rev:           &crate::game_store::GameReview,
    white_name:    &str,
    black_name:    &str,
    you_are_white: Option<bool>,
) {
    use crate::game_store::{GameReview, MoveClass};

    // Colour an accuracy figure by quality band.
    fn acc_color(acc: f32) -> egui::Color32 {
        if acc >= 90.0      { egui::Color32::from_rgb(129, 182, 76) }   // green
        else if acc >= 80.0 { egui::Color32::from_rgb(149, 187, 79) }   // light green
        else if acc >= 70.0 { egui::Color32::from_rgb(240, 193, 90) }   // yellow
        else if acc >= 60.0 { egui::Color32::from_rgb(231, 144, 60) }   // orange
        else                { egui::Color32::from_rgb(202, 70, 70) }    // red
    }
    fn trunc(name: &str, n: usize) -> String {
        if name.chars().count() > n {
            format!("{}…", name.chars().take(n).collect::<String>())
        } else if name.is_empty() {
            "—".to_string()
        } else {
            name.to_string()
        }
    }
    let round25 = |e: u32| ((e + 12) / 25) * 25;

    let elo_w = round25(GameReview::accuracy_to_elo(rev.accuracy_white));
    let elo_b = round25(GameReview::accuracy_to_elo(rev.accuracy_black));
    let (tag_w, tag_b) = match you_are_white {
        Some(true)  => ("You", "Opponent"),
        Some(false) => ("Opponent", "You"),
        None        => ("", ""),
    };

    egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(22, 24, 34, 230))
        .stroke(egui::Stroke::new(1.0, theme::BORDER_DEFAULT))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(10, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());

            // ── Title row ─────────────────────────────────────────────────
            ui.horizontal(|ui| {
                ui.label(RichText::new("GAME REVIEW").size(13.0)
                    .color(theme::TEXT_PRIMARY).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("depth {}", rev.depth))
                        .size(9.0).color(theme::TEXT_MUTED));
                });
            });
            ui.add_space(6.0);

            // Fixed column geometry shared by every row → perfectly aligned.
            let total_w = ui.available_width();
            let val_w   = 80.0_f32;
            let gap_x   = 6.0_f32;
            let label_w = (total_w - 2.0 * val_w - 2.0 * gap_x).clamp(96.0, 240.0);
            let row_h   = 20.0_f32;

            // Cell helpers — every cell is a fixed-size box so columns never drift.
            let label_cell = |ui: &mut egui::Ui, text: egui::RichText| {
                ui.allocate_ui_with_layout(
                    egui::vec2(label_w, row_h),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| { ui.label(text); },
                );
            };
            let val_cell = |ui: &mut egui::Ui, text: egui::RichText| {
                ui.allocate_ui_with_layout(
                    egui::vec2(val_w, row_h),
                    egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                    |ui| { ui.label(text); },
                );
            };

            egui::Grid::new(format!("review_grid_{id}"))
                .num_columns(3)
                .striped(true)
                .min_row_height(row_h)
                .spacing(egui::vec2(gap_x, 4.0))
                .show(ui, |ui| {
                    // ── Header: side names + you/opponent ─────────────────
                    label_cell(ui, RichText::new("Player").size(10.0)
                        .color(theme::TEXT_MUTED).strong());
                    let name_header = |ui: &mut egui::Ui, glyph: &str, name: &str,
                                       tag: &str, name_col: egui::Color32| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(val_w, 30.0),
                            egui::Layout::top_down(egui::Align::Center),
                            |ui| {
                                ui.label(RichText::new(format!("{glyph} {}", trunc(name, 9)))
                                    .size(10.5).color(name_col).strong());
                                if !tag.is_empty() {
                                    ui.label(RichText::new(tag).size(8.0)
                                        .color(theme::ACCENT_INFO));
                                }
                            },
                        );
                    };
                    name_header(ui, "♔", white_name, tag_w, theme::TEXT_PRIMARY);
                    name_header(ui, "♚", black_name, tag_b, theme::TEXT_SECONDARY);
                    ui.end_row();

                    // ── Accuracy ──────────────────────────────────────────
                    label_cell(ui, RichText::new("Accuracy").size(11.0).color(theme::TEXT_MUTED));
                    val_cell(ui, RichText::new(format!("{:.1}%", rev.accuracy_white))
                        .size(13.0).color(acc_color(rev.accuracy_white)).strong());
                    val_cell(ui, RichText::new(format!("{:.1}%", rev.accuracy_black))
                        .size(13.0).color(acc_color(rev.accuracy_black)).strong());
                    ui.end_row();

                    // ── Estimated playing strength ────────────────────────
                    label_cell(ui, RichText::new("Est. strength").size(11.0).color(theme::TEXT_MUTED));
                    val_cell(ui, RichText::new(format!("~{elo_w}")).size(12.0)
                        .color(theme::TEXT_PRIMARY).strong());
                    val_cell(ui, RichText::new(format!("~{elo_b}")).size(12.0)
                        .color(theme::TEXT_SECONDARY).strong());
                    ui.end_row();

                    // ── Move-count subheader ──────────────────────────────
                    label_cell(ui, RichText::new("Moves").size(10.5)
                        .color(theme::TEXT_MUTED).strong());
                    val_cell(ui, RichText::new(format!("{}", rev.move_count_side(true)))
                        .size(9.5).color(theme::TEXT_MUTED));
                    val_cell(ui, RichText::new(format!("{}", rev.move_count_side(false)))
                        .size(9.5).color(theme::TEXT_MUTED));
                    ui.end_row();

                    // ── Per-class breakdown ───────────────────────────────
                    const ORDER: [MoveClass; 9] = [
                        MoveClass::Brilliant, MoveClass::Great, MoveClass::Best,
                        MoveClass::Excellent, MoveClass::Good, MoveClass::Book,
                        MoveClass::Inaccuracy, MoveClass::Mistake, MoveClass::Blunder,
                    ];
                    for class in ORDER {
                        let w = rev.class_count_side(class, true);
                        let b = rev.class_count_side(class, false);
                        if w == 0 && b == 0 { continue; }
                        let (r, g, bl) = class.rgb();
                        let col  = egui::Color32::from_rgb(r, g, bl);
                        let wcol = if w == 0 { theme::TEXT_MUTED } else { col };
                        let bcol = if b == 0 { theme::TEXT_MUTED } else { col };
                        label_cell(ui, RichText::new(format!("{}  {}", class.glyph(), class.label()))
                            .size(11.0).color(col));
                        val_cell(ui, RichText::new(w.to_string()).size(12.0).color(wcol).strong());
                        val_cell(ui, RichText::new(b.to_string()).size(12.0).color(bcol).strong());
                        ui.end_row();
                    }
                });
        });
}

// ── About tab ─────────────────────────────────────────────────────────────────

fn render_about_tab(ui: &mut egui::Ui, accent: egui::Color32, commands: &mut Vec<MenuCommand>) {
    ui.add_space(8.0);
    ui.vertical_centered(|ui| {
        egui::Frame::new()
            .fill(accent).corner_radius(8.0)
            .inner_margin(egui::Margin::symmetric(16, 8))
            .show(ui, |ui| {
                ui.label(RichText::new("♟").size(36.0).color(theme::BG_DARK).strong());
            });
        ui.add_space(10.0);
        ui.label(RichText::new("RUSTYCHESS").size(18.0).color(theme::TEXT_PRIMARY).strong());
        ui.add_space(4.0);
        ui.label(RichText::new("UCI engine overlay for chess.com").size(10.0).color(theme::TEXT_MUTED));
        ui.add_space(16.0);
        ui.label(RichText::new("By NexusUnderground").size(9.0).color(theme::TEXT_HEADER).italics());
    });

    ui.add_space(12.0);

   
    widgets::double_border_frame(ui, "ENGINE DOWNLOADS", accent, |ui| {
        ui.label(RichText::new("Any UCI engine works. Click to open download page:")
            .size(8.5).color(theme::TEXT_MUTED));
        ui.add_space(6.0);

        let engines: &[(&str, &str, &str, egui::Color32)] = &[
            (
                "Stockfish",
                "#1 rated engine. Fast, tactical, strongest overall.",
                "https://github.com/official-stockfish/Stockfish/releases/latest",
                theme::ACCENT_PRIMARY,
            ),
            (
                "Leela (lc0)",
                "Neural net engine. Positional, human-like style. Needs GPU.",
                "https://github.com/LeelaChessZero/lc0/releases/latest",
                egui::Color32::from_rgb(120, 200, 255),
            ),
            (
                "Komodo Dragon",
                "Top commercial engine. Balanced style. Free trial available.",
                "https://komodochess.com/",
                egui::Color32::from_rgb(200, 100, 255),
            ),
            (
                "Berserk",
                "Open source, strong, fast startup. Good Stockfish alternative.",
                "https://github.com/jhonnold/berserk/releases/latest",
                theme::ACCENT_DANGER,
            ),
            (
                "Ethereal",
                "Lightweight UCI engine. Low resource usage.",
                "https://github.com/AndyGrant/Ethereal/releases/latest",
                theme::ACCENT_SUCCESS,
            ),
        ];

        for (name, desc, url, col) in engines {
            ui.horizontal(|ui| {
                if ui.add(
                    egui::Button::new(RichText::new(*name).size(10.0).color(theme::BG_DARK).strong())
                        .fill(*col)
                        .corner_radius(3.0)
                        .min_size(egui::vec2(100.0, 20.0)),
                )
                .on_hover_text(format!("{desc}\n\n{url}"))
                .clicked() {
                    commands.push(MenuCommand::LaunchUrl(Some(url.to_string())));
                }
                ui.label(RichText::new(*desc).size(8.0).color(theme::TEXT_MUTED));
            });
            ui.add_space(3.0);
        }

        ui.add_space(2.0);
        ui.label(RichText::new("Tip: rename the .exe before use (Settings > Engine) to disguise it in Task Manager.")
            .size(9.5).color(theme::TEXT_MUTED));
    });
}

// ── Footer ────────────────────────────────────────────────────────────────────

fn render_footer(ui: &mut egui::Ui, is_analysing: bool, config: &Config, is_rebinding: bool) {
    ui.add_space(2.0);
    separator(ui);
    egui::Frame::new()
        .fill(theme::BG_MEDIUM)
        .corner_radius(egui::CornerRadius { nw: 0, ne: 0, sw: 4, se: 4 })
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            let hk = &config.hotkeys;

            // A subtle bordered pill: "LABEL [KEY]" centered, filling its column.
            fn pill(ui: &mut egui::Ui, label: &str, vk: u32, color: egui::Color32) {
                let key = vk_to_label(vk);
                egui::Frame::new()
                    .fill(egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 24))
                    .stroke(egui::Stroke::new(1.0,
                        egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 140)))
                    .corner_radius(4.0)
                    .inner_margin(egui::Margin::symmetric(5, 4))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(3.0, 0.0);
                            ui.add_space(2.0);
                            ui.label(RichText::new(label).size(9.5).color(theme::TEXT_LABEL).strong());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.add_space(2.0);
                                ui.label(RichText::new(format!("[{key}]"))
                                    .size(9.5).color(color).strong().monospace());
                            });
                        });
                    });
            }

            let (status_col, status_txt) =
                if is_rebinding      { (theme::ACCENT_WARNING, "rebinding") }
                else if is_analysing { (theme::ACCENT_SUCCESS, "● live") }
                else                 { (theme::TEXT_MUTED,     "○ idle") };

            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);

            // ── Row 1: status · MENU · EXIT ───────────────────────────────────
            ui.columns(3, |c| {
                egui::Frame::new()
                    .fill(egui::Color32::from_rgba_unmultiplied(
                        status_col.r(), status_col.g(), status_col.b(), 16))
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(
                        status_col.r(), status_col.g(), status_col.b(), 90)))
                    .corner_radius(4.0)
                    .inner_margin(egui::Margin::symmetric(4, 3))
                    .show(&mut c[0], |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.add_space(2.0);
                            ui.label(RichText::new(status_txt).size(8.0).color(status_col).strong());
                        });
                    });
                pill(&mut c[1], "MENU", hk.toggle_menu, theme::ACCENT_PRIMARY);
                pill(&mut c[2], "EXIT", hk.exit,        theme::ACCENT_DANGER);
            });

            // ── Row 2: REFRESH · HIDE · DOT ───────────────────────────────────
            ui.columns(3, |c| {
                pill(&mut c[0], "REFRESH", hk.reconnect_cdp,   theme::ACCENT_SUCCESS);
                pill(&mut c[1], "HIDE",    hk.toggle_overlay,  theme::ACCENT_INFO);
                pill(&mut c[2], "DOT",     hk.toggle_discrete, egui::Color32::from_rgb(200, 130, 255));
            });
        });
}

// ── Shared helpers ────────────────────────────────────────────────────────────

fn separator(ui: &mut egui::Ui) {
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.left()..=r.right(), r.top(),
        egui::Stroke::new(1.0, theme::BORDER_DEFAULT));
}
