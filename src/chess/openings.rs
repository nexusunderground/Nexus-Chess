//! Chess opening name lookup, keyed by EPD position string.
//!
//! Data sourced from lichess-org/chess-openings (CC0), pre-processed with
//! `bin/gen.py` to add `uci` and `epd` columns.  The five TSV files are
//! embedded at compile time; the `HashMap` is built once on first call via
//! `OnceLock` so look-ups are O(1) thereafter.

use std::collections::HashMap;
use std::sync::OnceLock;

static OPENINGS: OnceLock<HashMap<String, Opening>> = OnceLock::new();

pub struct Opening {
    pub eco:  String,
    pub name: String,
}

fn load() -> HashMap<String, Opening> {
    const FILES: &[&[u8]] = &[
        include_bytes!("../../assets/openings/a.tsv"),
        include_bytes!("../../assets/openings/b.tsv"),
        include_bytes!("../../assets/openings/c.tsv"),
        include_bytes!("../../assets/openings/d.tsv"),
        include_bytes!("../../assets/openings/e.tsv"),
    ];

    let mut map = HashMap::with_capacity(4096);
    for bytes in FILES {
        let text = std::str::from_utf8(bytes).unwrap_or_default();
        for line in text.lines().skip(1) { // skip header row
            let mut cols = line.split('\t');
            let eco  = match cols.next() { Some(v) => v.trim(), None => continue };
            let name = match cols.next() { Some(v) => v.trim(), None => continue };
            let _pgn = cols.next(); // pgn — not needed
            let _uci = cols.next(); // uci — not needed
            let epd  = match cols.next() { Some(v) => v.trim(), None => continue };
            if eco.is_empty() || epd.is_empty() { continue; }
            // Later/deeper entries for the same position win (more specific name).
            map.insert(epd.to_string(), Opening {
                eco:  eco.to_string(),
                name: name.to_string(),
            });
        }
    }
    map
}

/// Look up the opening name for a given EPD string.
/// Returns `None` if the position is not in the opening book.
pub fn lookup(epd: &str) -> Option<&'static Opening> {
    OPENINGS.get_or_init(load).get(epd)
}

/// Extract the EPD (first 4 space-separated fields) from a full FEN string.
/// Standard FEN has 6 fields; EPD omits the halfmove clock and fullmove number.
pub fn fen_to_epd(fen: &str) -> &str {
    let mut spaces = 0u8;
    for (i, b) in fen.bytes().enumerate() {
        if b == b' ' {
            spaces += 1;
            if spaces == 4 {
                return &fen[..i];
            }
        }
    }
    fen // fewer than 4 fields — return as-is (handles already-EPD input)
}
