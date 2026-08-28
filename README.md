# RustyChess

A Windows overlay that connects to a live chess game in Chrome and displays real-time Stockfish analysis — arrows, eval bar, opening name, and move history — drawn directly on top of the browser window.

Supports chess.com and lichess.org (live games, puzzles, Chess960).

---

## Prerequisites

| Requirement | Notes |
|---|---|
| Windows 10/11 | Overlay uses Win32 APIs — Windows only |
| [Rust](https://rustup.rs) (stable) | `rustup default stable` |
| [Stockfish](https://stockfishchess.org/download/) | Any recent release; extract the `.exe` somewhere |
| Chrome, Edge, or Brave | Must be launched with `--remote-debugging-port=9222` (see below) |

---

## Build

```
cargo build --release
```

The binary is written to `target/release/rustychess.exe`.  
Copy it (and `rustychess.toml` if you have one) anywhere you like.

---

## First Run

1. **Launch your browser with remote debugging enabled.**  
   The easiest way is the built-in browser launcher in the RustyChess HUD — press `Insert` to open the menu, go to the Overview tab, select a site, and click **LAUNCH**. That starts Chrome with the correct flags automatically.

   To do it manually:
   ```
   chrome.exe --remote-debugging-port=9222
   ```

2. **Run `rustychess.exe`.**  
   The overlay appears fullscreen and transparent. Press `Insert` to open the menu.

3. **Set your engine path** (Settings → Engine Path) to point at your `stockfish.exe`.  
   On first run the app tries to find Stockfish automatically in common locations.

4. **Enter your chess.com username** (Overview tab, "YOU" field) so the overlay knows which colour you are playing.

5. **Navigate to a game** in the browser. The overlay will connect automatically.

---

## Configuration File

`rustychess.toml` is created next to the `.exe` on first run.  
You can edit it in a text editor while the app is closed.

Key settings:

```toml
[engine]
path       = "C:\\path\\to\\stockfish.exe"
hash_mb    = 256   # hash table size
threads    = 2     # engine threads
depth      = 20    # search depth (0 = infinite)
nodes      = 0     # node cap per move (0 = off; useful for lc0)

[analysis]
multipv        = 3     # number of lines shown
display_lines  = 1     # arrows drawn on the board (1–3)
overlay_enabled = true
show_eval_bar  = true
show_opening_name = true
review_depth   = 18    # depth for post-game review

[cdp]
endpoint = "http://127.0.0.1:9222"
chrome_path = "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe"

username = "your_chesscom_username"
```

---

## Default Hotkeys

All keys are rebindable in the Hotkeys tab of the menu.

| Key | Action |
|---|---|
| `Insert` | Open / close the HUD menu |
| `H` | Toggle overlay on/off |
| `D` | Toggle discrete mode (subtle square tint instead of arrows) |
| `F` | Flip the board |
| `F11` | Reconnect to Chrome (use this if the overlay loses sync) |
| `F12` | Quit |
| `Right Shift` *(hold)* | Peek at best move — only active when Hint Mode is on |

---

## Overlay Features

- **Arrows** — best move(s) drawn on the board, colour-coded by rank
- **Eval bar** — vertical advantage bar on the left edge of the board
- **Opening name** — ECO code + name shown while still in book
- **Move history** — full move list for the current game (Overview tab)
- **Discrete mode** — faint square highlights instead of arrows, less visible at a glance
- **Hint mode** — overlay is hidden; hold Right Shift to briefly reveal the best move
- **Game review** — after a game ends, run a post-game move classification (Brilliant → Blunder) from the Game Review tab

---

## Troubleshooting

**Overlay not tracking the board**  
Press `F11` to reconnect. If that doesn't help, make sure the browser was launched with `--remote-debugging-port=9222` and no other application is using that port.

**Engine not starting**  
Check Settings → Engine Path. The path must point directly to the engine `.exe`. Make sure the file exists.

**Wrong side / board flipped**  
Set your username in the Overview tab. Press `F` to manually flip if needed.

**Overlay visible in OBS / recordings**  
Enable "Screen Capture Exclusion" in Settings → Stealth. This uses `WDA_EXCLUDEFROMCAPTURE` to hide the window from BitBlt-based recorders while keeping it visible on your physical display.
