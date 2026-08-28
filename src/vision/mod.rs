pub mod chrome_launcher;
pub mod cdp_chesscom;
pub mod cdp_lichess;
pub mod stability;

use crate::config::ChessPage;
use crate::vision::cdp_chesscom::CdpMoveSnapshot;
use egui::Rect;

#[derive(Debug, Clone, Default)]
pub struct BoardState {
    pub fen:             String,
    pub board_rect:      Option<Rect>,
    pub page:            ChessPage,
    pub player_white:    Option<String>,
    pub player_black:    Option<String>,
    pub clock_white:     Option<String>,
    pub clock_black:     Option<String>,
    /// Board orientation: true when the Black pieces are on the bottom
    /// (i.e. the local player is seated as Black).
    pub bottom_is_black: bool,
    /// The last move actually played (SAN), taken from the move list.
    /// `None` for puzzles (reconstructed from a piece map, no move list).
    pub last_move_san:   Option<String>,
    /// Raw game-result string from the DOM (e.g. "White is victorious").
    /// `None` while the game is in progress.
    pub game_result:     Option<String>,
}

impl BoardState {
    pub fn is_puzzle(&self) -> bool { self.page.is_puzzle() }
}

/// Convert a raw CDP snapshot into a `BoardState` (FEN + metadata).
/// Called from `bg_thread`, works for both persistent and stateless CDP access.
pub fn snapshot_to_board_state(snapshot: CdpMoveSnapshot) -> BoardState {
    let page = ChessPage::from_url(&snapshot.page_url);

    // When we can't compute a FEN yet (board still hydrating, missing PGN, …)
    // we must still preserve the detected `page` so the UI/background loop keep
    // treating the tab as the puzzle/game it actually is.  Returning a bare
    // `BoardState::default()` would reset `page` to `Unknown` and make the app
    // mis-handle the tab.
    let unresolved = || BoardState { page, ..Default::default() };

    let fen = if snapshot.is_puzzle {
        match snapshot.puzzle_fen {
            Some(f) => f,
            None    => {
                // page-init-data is consumed after Lichess boots and the CSS
                // piece map is often empty, so fall back to replaying the move
                // list (`.tview2`) which holds the full game line up to the
                // current puzzle position.
                let replayed = if snapshot.moves_are_uci {
                    moves_to_fen_uci_from(&snapshot.moves, snapshot.initial_fen.as_deref())
                } else {
                    moves_to_fen(&snapshot.moves)
                };
                match replayed {
                    Some(f) if !snapshot.moves.is_empty() => f,
                    _ => return unresolved(),
                }
            }
        }
    } else if snapshot.moves_are_uci {
        // Chess960: replay from the variant starting position when provided.
        match moves_to_fen_uci_from(&snapshot.moves, snapshot.initial_fen.as_deref()) {
            Some(f) => f,
            None    => return unresolved(),
        }
    } else {
        // SAN path — chess.com and similar.  For Chess960 we replay from the
        // variant starting position instead of the standard one.
        match moves_to_fen_san_from(&snapshot.moves, snapshot.initial_fen.as_deref()) {
            Some(f) => f,
            None    => return unresolved(),
        }
    };

    // The actual last move played (live games only — puzzles have no move list).
    let last_move_san = if snapshot.is_puzzle {
        None
    } else {
        snapshot.moves.last().cloned()
    };

    // ── Player-side detection ─────────────────────────────────────────────────
    //
    // Board orientation (`bottom_is_black`) is the source of truth for which
    // side the local player occupies — chess.com sets it based on your seat.
    // We deliberately do NOT use side-to-move, which would un-flip the board
    // on the opponent's turn.

    BoardState {
        fen,
        board_rect:      snapshot.board_rect,
        page,
        player_white:    snapshot.white_player,
        player_black:    snapshot.black_player,
        clock_white:     snapshot.white_clock,
        clock_black:     snapshot.black_clock,
        bottom_is_black: snapshot.bottom_is_black,
        last_move_san,
        game_result:     snapshot.game_result,
    }
}

fn moves_to_fen(moves: &[String]) -> Option<String> {
    moves_to_fen_san_from(moves, None)
}

/// Replay SAN moves from an optional custom starting position (Chess960/variant).
/// Falls back to the standard starting position when `start_fen` is `None`.
fn moves_to_fen_san_from(moves: &[String], start_fen: Option<&str>) -> Option<String> {
    use shakmaty::{Chess, CastlingMode, Position};
    use shakmaty::san::San;
    use shakmaty::fen::Fen;
    use std::str::FromStr;

    let mut pos: Chess = match start_fen {
        Some(f) => Fen::from_str(f).ok()?.into_position(CastlingMode::Chess960).ok()?,
        None    => Chess::default(),
    };
    for m in moves {
        let san = San::from_str(m).ok()?;
        let mv  = san.to_move(&pos).ok()?;
        pos     = pos.play(&mv).ok()?;
    }
    Some(Fen::from_position(pos, shakmaty::EnPassantMode::Legal).to_string())
}

/// Replay UCI moves from an optional custom starting position (Chess960/variant).
/// Pass `None` for `start_fen` to use the standard starting position.
fn moves_to_fen_uci_from(moves: &[String], start_fen: Option<&str>) -> Option<String> {
    use shakmaty::{Chess, CastlingMode, Position};
    use shakmaty::uci::UciMove;
    use shakmaty::fen::Fen;
    use std::str::FromStr;

    let mut pos: Chess = match start_fen {
        Some(f) => Fen::from_str(f).ok()?.into_position(CastlingMode::Chess960).ok()?,
        None    => Chess::default(),
    };
    for m in moves {
        let uci = UciMove::from_str(m).ok()?;
        let mv  = uci.to_move(&pos).ok()?;
        pos     = pos.play(&mv).ok()?;
    }
    Some(Fen::from_position(pos, shakmaty::EnPassantMode::Legal).to_string())
}