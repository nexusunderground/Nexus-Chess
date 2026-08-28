use anyhow::{Context, Result, anyhow};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{info, trace, warn};

// ── Shared line buffer ────────────────────────────────────────────────────────
//
// Previously `wait_for` scanned the ENTIRE accumulated buffer on every 10 ms
// tick — O(n) per tick — which ballooned to O(n²) over a long search and
// could spin-lock the calling thread for dozens of milliseconds.
//
// Fix: the reader thread keeps a single `Vec<String>` that is drained by
// `drain_lines`.  `wait_for` gets its own cursor (`seen`) so it only scans
// lines it hasn't checked yet — O(new lines) per tick regardless of history.
// `drain_lines` resets the cursor via the returned drain, which is safe because
// `wait_for` is always called from the same thread as `drain_lines`.

/// A live UCI engine process with stdin/stdout wired up.
pub struct UciEngine {
    process:      Child,
    stdin:        ChildStdin,
    stdout_lines: Arc<Mutex<Vec<String>>>,
}

impl UciEngine {
    /// Spawn the engine at `path`, perform the UCI handshake, and return.
    pub fn spawn(path: &str) -> Result<Self> {
        info!(path = %path, "[uci] spawning engine process");
        let mut cmd = Command::new(path);
        cmd.stdin(Stdio::piped())
           .stdout(Stdio::piped())
           .stderr(Stdio::null());

        // Prevent Stockfish from creating a visible console window.
        // Required because we build with #![windows_subsystem = "windows"] —
        // child processes can still flash a window without this flag.
        //
        // We also drop the engine below the browser's scheduling priority:
        // Stockfish bursts every CPU core to 100% the instant a new position
        // arrives (i.e. exactly when you move a piece and the chess site is
        // animating it).  BELOW_NORMAL_PRIORITY_CLASS makes Windows hand those
        // cycles to the foreground app first, so the move no longer feels laggy,
        // while the engine still runs at full speed whenever CPU is otherwise
        // idle (which is almost always, between moves).
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
            cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
        }

        let mut process = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn engine at '{path}'"))?;

        let stdin        = process.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let child_stdout: ChildStdout =
            process.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;

        // Drain stdout in a background thread, appending to a shared vec.
        let lines_buf: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let lines_clone = Arc::clone(&lines_buf);
        std::thread::spawn(move || {
            let reader = BufReader::new(child_stdout);
            for line in reader.lines().map_while(Result::ok) {
                trace!(engine_out = %line);
                lines_clone.lock().unwrap().push(line);
            }
        });

        let mut engine = Self { process, stdin, stdout_lines: lines_buf };

        // UCI handshake
        info!("[uci] running uci/isready handshake");
        engine.send("uci")?;
        engine.wait_for("uciok",    Duration::from_secs(5))?;
        engine.send("isready")?;
        engine.wait_for("readyok", Duration::from_secs(5))?;

        info!("UCI engine ready: {path}");
        Ok(engine)
    }

    /// Write a raw command line to the engine.
    pub fn send(&mut self, cmd: &str) -> Result<()> {
        if let Some(status) = self.process.try_wait()
            .context("check engine process state")?
        {
            return Err(anyhow!(
                "engine process exited before send ('{cmd}'): {status}"
            ));
        }
        trace!(engine_in = %cmd);
        writeln!(self.stdin, "{cmd}").context("write to engine stdin")?;
        self.stdin.flush().context("flush engine stdin")
    }

    /// Block until a buffered line contains `token`, or until `timeout`.
    ///
    /// Uses a local `seen` cursor so each call only inspects new lines —
    /// O(new lines) per 10 ms tick rather than O(total lines).
    pub fn wait_for(&self, token: &str, timeout: Duration) -> Result<()> {
        trace!(token = %token, timeout_ms = timeout.as_millis(), "[uci] waiting for token");
        let deadline = std::time::Instant::now() + timeout;
        let mut seen = 0usize; // index of next unchecked line

        loop {
            {
                let lines = self.stdout_lines.lock().unwrap_or_else(|e| e.into_inner());
                // Only scan lines we haven't checked yet.
                if lines[seen..].iter().any(|l| l.contains(token)) {
                    trace!(token = %token, total = lines.len(), "[uci] token found");
                    return Ok(());
                }
                seen = lines.len(); // advance cursor past everything we just checked
            }
            if std::time::Instant::now() >= deadline {
                warn!(token = %token, "[uci] timeout while waiting for token");
                return Err(anyhow!("timed out waiting for '{token}'"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Drain all buffered output lines and return them.
    ///
    /// Callers must call this before re-using `wait_for` for a new command —
    /// otherwise the cursor in `wait_for` would need to be reset externally.
    /// Since both are always called from the engine thread this is safe.
    pub fn drain_lines(&self) -> Vec<String> {
        let mut lock = self.stdout_lines.lock().unwrap_or_else(|e| e.into_inner());
        let drained = std::mem::take(&mut *lock);
        trace!(count = drained.len(), "[uci] drained buffered engine lines");
        drained
    }

    pub fn send_stop(&mut self) -> Result<()> {
        // Send stop and flush immediately.  The engine thread will push
        // the "bestmove" line; the caller is expected to call wait_for("bestmove")
        // after this to drain the tail of the search output before
        // re-sending "position".  This prevents a race where residual
        // "info" lines from the previous search are mixed into the next one.
        self.send("stop")
    }

    pub fn send_quit(&mut self) -> Result<()> {
        self.send("quit")
    }

    /// Returns `true` if the engine process has exited (crashed or quit).
    /// Uses `try_wait` so it never blocks.
    pub fn is_dead(&mut self) -> bool {
        self.process
            .try_wait()
            .map(|status| status.is_some())
            .unwrap_or(true)
    }
}

impl Drop for UciEngine {
    fn drop(&mut self) {
        let _ = self.send_quit();
        let _ = self.process.wait();
    }
}

/// Parse a `bestmove e2e4 ponder ...` line into the best move string.
pub fn parse_bestmove(line: &str) -> Option<String> {
    let mut parts = line.split_whitespace();
    while let Some(tok) = parts.next() {
        if tok == "bestmove" {
            // "bestmove (none)" means the engine has no legal move (checkmate/stalemate).
            let mv = parts.next()?;
            if mv == "(none)" || mv == "0000" { return None; }
            return Some(mv.to_string());
        }
    }
    None
}

/// Parse one `info depth … score cp … pv …` line into an [`InfoLine`].
pub fn parse_info(line: &str) -> Option<InfoLine> {
    if !line.starts_with("info") { return None; }

    let mut depth:      Option<u32> = None;
    let mut score_cp:   Option<i32> = None;
    let mut score_mate: Option<i32> = None;
    let mut pv:         Vec<String> = Vec::new();
    let mut multipv:    Option<u32> = None;
    let mut in_pv = false;

    let mut parts = line.split_whitespace().peekable();
    parts.next(); // consume "info"

    while let Some(tok) = parts.next() {
        match tok {
            "depth"   => { in_pv = false; depth   = parts.next().and_then(|v| v.parse().ok()); }
            "multipv" => { in_pv = false; multipv = parts.next().and_then(|v| v.parse().ok()); }
            "score"   => {
                in_pv = false;
                match parts.next() {
                    Some("cp")   => score_cp   = parts.next().and_then(|v| v.parse().ok()),
                    Some("mate") => score_mate = parts.next().and_then(|v| v.parse().ok()),
                    _ => {}
                }
            }
            "pv"   => { in_pv = true; }
            other if in_pv => { pv.push(other.to_string()); }
            _  => { in_pv = false; }
        }
    }

    // Require depth to be present — avoids treating "info string" lines as moves.
    depth?;

    Some(InfoLine {
        depth:      depth.unwrap_or(0),
        multipv:    multipv.unwrap_or(1),
        score_cp,
        score_mate,
        pv,
    })
}

#[derive(Debug, Clone)]
pub struct InfoLine {
    pub depth:      u32,
    pub multipv:    u32,
    pub score_cp:   Option<i32>,
    pub score_mate: Option<i32>,
    pub pv:         Vec<String>,
}

impl InfoLine {
    /// Centipawn value (converts mate scores to ±30 000).
    pub fn centipawns(&self) -> i32 {
        if let Some(mate) = self.score_mate {
            if mate > 0 { 30_000 - mate } else { -30_000 - mate }
        } else {
            self.score_cp.unwrap_or(0)
        }
    }
}