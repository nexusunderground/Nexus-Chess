use anyhow::{Result, anyhow};
use shakmaty::{
    CastlingMode, Chess, EnPassantMode, Move, Position,  fen::Fen, uci::UciMove,
};

/// Thin wrapper around shakmaty's [`Chess`] position.
#[derive(Clone)]
pub struct Board {
    pub pos: Chess,
}

impl Default for Board {
    fn default() -> Self {
        Self {
            pos: Chess::default(),
        }
    }
}

impl Board {
    /// Construct from a FEN string.
    pub fn from_fen(fen: &str) -> Result<Self> {
        let fen: Fen = fen.parse().map_err(|e| anyhow!("invalid FEN: {e}"))?;
        let pos: Chess = fen
            .into_position(CastlingMode::Standard)
            .map_err(|e| anyhow!("illegal position: {e}"))?;
        Ok(Self { pos })
    }

    /// Return the current FEN string.
    pub fn fen(&self) -> String {
        Fen::from_position(self.pos.clone(), EnPassantMode::Always).to_string()
    }

    /// Apply a move given in UCI notation (e.g. "e2e4", "e1g1" for castling).
    pub fn apply_uci(&mut self, uci: &str) -> Result<()> {
        let uci_move: UciMove = uci.parse().map_err(|e| anyhow!("bad UCI move: {e}"))?;
        let mv: Move = uci_move
            .to_move(&self.pos)
            .map_err(|e| anyhow!("illegal move {uci}: {e}"))?;
        self.pos.play_unchecked(&mv);
        Ok(())
    }
}
