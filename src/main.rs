//! Rustok Console — resident terminal wallet screen (the human face).
//!
//! v0.2: connect → hello → PIN → live on the queue — approve/deny returns to
//! the queue (the console no longer exits per decision). Keys are mapped to
//! [`Msg`] and folded into the [`Model`]; the socket worker runs on its own
//! thread so a slow core never freezes the UI. UI goes to stderr (ratatui's
//! alternate screen); machine decisions go to stdout as one JSON line each,
//! emitted only when stdout is not a TTY (invariant #7, ADR
//! `2026-07-12-invariant-7-decision-stream`); the exit code reports how the
//! session ended (aborted / no-tty / fatal / upgrade).

use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Stderr, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use rustok_console::app::{
    Decision, DecisionKind, HISTORY_CAP, HistoryEntry, Model, Msg, Phase, View,
};
use rustok_console::transport::{Transport, TransportError};
use rustok_console::ui;

/// The console's terminal — rendered to **stderr**, so stdout stays clean for the
/// machine-readable decision (invariant #7). `ratatui::init` would use stdout, so
/// the setup is done by hand.
type Tui = Terminal<CrosstermBackend<Stderr>>;

/// Default approver socket path (overridable for tests / non-standard layouts).
const DEFAULT_SOCKET: &str = "/run/wallet/approve.sock";
/// How often the watch screen polls `list`.
const POLL: Duration = Duration::from_millis(2500);
/// Longest the loop sleeps between redraws — the countdown ticks in seconds, so it
/// must repaint far more often than the `list` poll.
const FRAME: Duration = Duration::from_millis(250);

// Exit codes (AGENTS.md #7) — the session's end, not a decision: decisions
// stream to a non-TTY stdout as JSON lines (ADR 2026-07-12).
/// The connection broke, or the terminal could not be drawn — the reason is on
/// the screen.
const EXIT_FATAL: u8 = 1;
/// The server speaks a protocol major this build does not.
const EXIT_UPGRADE: u8 = 2;
/// No interactive terminal: an approval may never come from a pipe (invariant #4).
const EXIT_NO_TTY: u8 = 3;
/// Quit from the queue (the resident session's normal end). Keeps its v0.1
/// value — the four surviving codes are preserved, not renumbered (Gate-1).
const EXIT_ABORTED: u8 = 6;

fn main() -> ExitCode {
    let path = std::env::var("RUSTOK_APPROVE_SOCK").unwrap_or_else(|_| DEFAULT_SOCKET.to_owned());

    // Invariant #4, checked before anything is opened: an approval may never come
    // from a pipe. `enable_raw_mode` opens `/dev/tty` directly, so a successful
    // `try_init` proves nothing about stdin — only this does. Nothing is connected
    // and no terminal is touched until stdin is known to be interactive.
    if !io::stdin().is_terminal() {
        eprintln!("rustok-console needs an interactive terminal (a TTY).");
        eprintln!("Approval from a pipe is never accepted.");
        return ExitCode::from(EXIT_NO_TTY);
    }

    let Ok(terminal) = try_init() else {
        eprintln!("rustok-console could not take the terminal.");
        return ExitCode::from(EXIT_NO_TTY);
    };

    let transport = Transport::connect(&path);
    // Decisions stream to stdout only when it is NOT a TTY: interactively
    // (docker exec -it) stdout is the same terminal the alternate screen owns,
    // and a mid-session write would land on top of the TUI frame. A piped
    // caller gets one JSON line per decision, as the decisions happen.
    let stream_decisions = !io::stdout().is_terminal();
    let code = run(terminal, &transport, stream_decisions);
    let _ = restore();
    ExitCode::from(code)
}

/// Set up the terminal on **stderr** (raw mode + alternate screen) and install a
/// panic hook that restores it. Mirrors `ratatui::init`, but targets stderr so
/// stdout stays clean. `enable_raw_mode` opens `/dev/tty` directly, so only the
/// alternate-screen escape and the backend are pointed at stderr.
///
/// # Errors
/// Propagates the raw-mode / alternate-screen error — notably when there is no TTY.
fn try_init() -> io::Result<Tui> {
    set_panic_hook();
    enable_raw_mode()?;
    let mut stderr = io::stderr();
    execute!(stderr, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stderr))
}

/// Restore the terminal: leave the alternate screen, disable raw mode.
fn restore() -> io::Result<()> {
    execute!(io::stderr(), LeaveAlternateScreen)?;
    disable_raw_mode()
}

/// Restore the terminal before a panic prints, so the message stays readable.
fn set_panic_hook() {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        hook(info);
    }));
}

/// The run loop's side of the history pipeline (Stage 7): the model has no
/// clock and no filesystem, so stamping, persistence and the id-dedup set
/// (§3.9) live here.
struct HistoryLog {
    /// Append target; `None` = session-only (no path configured, or a write
    /// failed and persistence was honestly switched off).
    path: Option<PathBuf>,
    /// Every id in the log or recorded this session — the dedup set.
    seen: HashSet<String>,
    /// Appends since the last compaction — a resident session compacts
    /// periodically, not only at startup (Gate-2 МИНОР-3).
    appended: usize,
}

impl HistoryLog {
    /// Env wrapper over [`Self::open_at`] — kept apart so the open path is
    /// testable without touching the process environment (Gate-2 МИНОР-8).
    fn open(model: &mut Model) -> Self {
        Self::open_at(history_log_path(), model)
    }

    /// Load and merge the file at `path` (or none = session-only), compact
    /// it when oversized (best-effort), and seed the model with the entries
    /// and a degradation note when there is one.
    fn open_at(path: Option<PathBuf>, model: &mut Model) -> Self {
        let Some(path) = path else {
            model.set_history_note(
                "history is session-only — set RUSTOK_CONSOLE_LOG or RUSTOK_DATA_DIR to persist"
                    .to_owned(),
            );
            return Self {
                path: None,
                seen: HashSet::new(),
                appended: 0,
            };
        };
        let loaded = load_history(&path);
        if loaded.unreadable {
            // NOT the missing-file case (that is a normal empty history):
            // permissions or I/O — a history may exist but cannot be read.
            // Say so instead of presenting a healthy empty view (BLOCKER-2).
            model.set_history_note(
                "activity log could not be read — history may be incomplete".to_owned(),
            );
        } else if loaded.skipped > 0 {
            model.set_history_note(format!(
                "activity log partially unreadable — {} line(s) skipped",
                loaded.skipped
            ));
        }
        // The trigger counts RAW lines, not deduped entries — duplicate junk
        // must not grow unbounded below the entry count (Gate-2 МИНОР-2).
        if loaded.raw_lines > 2 * HISTORY_CAP {
            compact_history(&path);
        }
        let seen = loaded.entries.iter().map(|e| e.id.clone()).collect();
        model.set_history(loaded.entries);
        Self {
            path: Some(path),
            seen,
            appended: 0,
        }
    }

    /// Record one stamped entry: a NEW id is appended to the file (when
    /// persisting) and pushed into the model; a known id only fills the
    /// missing fields of its existing row (fill-missing merge, spec /check-2
    /// — the file keeps its first record, append-only). A failed write
    /// switches persistence off with a visible note — never a crash, never
    /// silence. Every [`HISTORY_CAP`] appends the file re-compacts: a
    /// resident session must not grow it unboundedly until the next restart
    /// (Gate-2 МИНОР-3).
    fn record(&mut self, model: &mut Model, entry: HistoryEntry) {
        if !self.seen.insert(entry.id.clone()) {
            model.fill_history_detail(&entry);
            return;
        }
        if let Some(path) = &self.path {
            if append_history_line(path, &entry).is_err() {
                self.path = None;
                model.set_history_note(
                    "history is not being persisted — the activity log write failed".to_owned(),
                );
            } else {
                self.appended += 1;
                if self.appended >= HISTORY_CAP {
                    compact_history(path);
                    self.appended = 0;
                }
            }
        }
        model.push_history(entry);
    }
}

/// The local log's path cascade: an explicit override, else the wallet
/// image's persistent volume (`RUSTOK_DATA_DIR=/data`, Dockerfile.wallet),
/// else nowhere (session-only).
fn history_log_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("RUSTOK_CONSOLE_LOG") {
        return Some(PathBuf::from(explicit));
    }
    std::env::var("RUSTOK_DATA_DIR")
        .ok()
        .map(|dir| Path::new(&dir).join("console-activity.jsonl"))
}

/// What [`load_history`] found on disk.
struct LoadedHistory {
    /// Merged entries, one per id (file order first-wins; later duplicates
    /// only fill missing fields — Gate-2 BLOCKER-1).
    entries: Vec<HistoryEntry>,
    /// Corrupt (non-UTF8 / non-JSON) lines — counted, noted, never fatal.
    skipped: usize,
    /// Raw non-empty lines on disk, pre-dedup — the compaction trigger.
    raw_lines: usize,
    /// The file exists but could NOT be read (permissions, I/O) — distinct
    /// from missing (a normal empty history). BLOCKER-2: this surfaces as a
    /// note, never as a silently healthy empty view.
    unreadable: bool,
}

/// Load the JSONL log as BYTES — a torn multi-byte write from a killed
/// process must poison one line, not the whole read (BLOCKER-2). Corrupt
/// lines are skipped and counted; duplicate ids merge FIELD-BY-FIELD in
/// file order (the first record wins for present fields, later ones fill
/// gaps — the same [`HistoryEntry::fill_missing_from`] seam as every other
/// duplicate path). BLOCKER-1: the stamps of a rich decision record and a
/// poor server-window record come from different formulas — a timestamp
/// must never decide which record survives whole.
fn load_history(path: &Path) -> LoadedHistory {
    let empty = |unreadable| LoadedHistory {
        entries: Vec::new(),
        skipped: 0,
        raw_lines: 0,
        unreadable,
    };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return empty(false),
        Err(_) => return empty(true),
    };
    let mut skipped = 0usize;
    let mut raw_lines = 0usize;
    let mut entries: Vec<HistoryEntry> = Vec::new();
    let mut index_of: HashMap<String, usize> = HashMap::new();
    for raw in bytes.split(|b| *b == b'\n') {
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        raw_lines += 1;
        let parsed = std::str::from_utf8(raw)
            .ok()
            .and_then(|line| serde_json::from_str::<HistoryEntry>(line).ok());
        let Some(entry) = parsed else {
            skipped += 1;
            continue;
        };
        match index_of.entry(entry.id.clone()) {
            std::collections::hash_map::Entry::Occupied(slot) => {
                entries[*slot.get()].fill_missing_from(&entry);
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(entries.len());
                entries.push(entry);
            }
        }
    }
    LoadedHistory {
        entries,
        skipped,
        raw_lines,
        unreadable: false,
    }
}

/// Open (creating 0600) for writing. The mode applies only when the file is
/// created — the log carries the wallet's decision history, no reason to be
/// group/world-readable (Gate-2 МИНОР-5).
fn open_log_0600(path: &Path, append: bool) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .create(true)
        .append(append)
        .write(true)
        .truncate(!append)
        .mode(0o600)
        .open(path)
}

/// Serialize log WRITERS across processes: appends and compaction take an
/// exclusive lock on a sidecar lockfile. The sidecar — not the log itself —
/// because compaction replaces the log via rename: a writer blocked on the
/// old inode's lock would wake up owning an unlinked ghost file and write
/// into the void (Gate-2 МИНОР-1 — the TOCTOU race four reviewers hit).
/// Readers do not lock: appends are line-atomic and rename is atomic.
fn lock_log(path: &Path) -> io::Result<std::fs::File> {
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let file = open_log_0600(Path::new(&lock_path), true)?;
    file.lock()?;
    Ok(file) // released on drop
}

/// Append one entry as a JSONL line, under the writers' lock.
fn append_history_line(path: &Path, entry: &HistoryEntry) -> io::Result<()> {
    let mut line =
        serde_json::to_string(entry).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    let _lock = lock_log(path)?;
    open_log_0600(path, true)?.write_all(line.as_bytes())
}

/// Best-effort compaction: under the writers' lock, RE-READ the file fresh —
/// a snapshot taken before the lock could erase a concurrent append (Gate-2
/// МИНОР-1) — keep the newest [`HISTORY_CAP`] merged entries, and swap them
/// in atomically. The tmp name carries the pid (МИНОР-4: a shared name
/// would race two consoles starting over the same oversized file). Corrupt
/// and duplicate lines are dropped with the rest (the file heals); any
/// failure leaves the file exactly as it was.
fn compact_history(path: &Path) {
    let Ok(_lock) = lock_log(path) else {
        return;
    };
    let loaded = load_history(path);
    if loaded.unreadable {
        return;
    }
    let mut newest = loaded.entries;
    newest.sort_unstable_by(|a, b| b.unix.cmp(&a.unix).then_with(|| a.id.cmp(&b.id)));
    newest.truncate(HISTORY_CAP);
    let mut body = String::new();
    for entry in &newest {
        match serde_json::to_string(entry) {
            Ok(line) => {
                body.push_str(&line);
                body.push('\n');
            }
            Err(_) => return,
        }
    }
    let tmp = path.with_extension(format!("jsonl.tmp.{}", std::process::id()));
    let written = open_log_0600(&tmp, false)
        .and_then(|mut f| f.write_all(body.as_bytes()))
        .is_ok();
    if written && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The event loop: draw, read input, drain worker replies, tick the poll. The
/// resident console leaves only on quit or a dead connection; each terminal
/// decision is streamed to stdout as it happens (when stdout is not a TTY).
/// A `Fatal` phase is shown until a keypress, then exits.
fn run(mut terminal: Tui, transport: &Transport, stream_decisions: bool) -> u8 {
    let mut model = Model::new();
    let mut history = HistoryLog::open(&mut model);
    let mut last_tick = Instant::now();

    // Report the terminal size before anything else: the model approves only a
    // card it knows to be fully readable, so an unknown size (this call
    // failing) leaves approval gated off — fail closed.
    if let Ok(size) = terminal.size()
        && let Some(req) = model.update(Msg::Resize {
            width: size.width,
            height: size.height,
        })
    {
        transport.send(req);
    }

    loop {
        if terminal
            .draw(|f| ui::render(f, &model, now_unix()))
            .is_err()
        {
            return EXIT_FATAL;
        }

        if let Phase::Fatal(err) = model.phase() {
            let code = fatal_code(err);
            wait_for_key();
            return code;
        }

        // The deadline says no on its own (`AGENTS.md` #5). `Msg::Expire` denies the
        // card and reports `expired` rather than `rejected` — no human pressed a key.
        if deadline_passed(model.phase(), now_unix())
            && let Some(req) = model.update(Msg::Expire)
        {
            transport.send(req);
        }

        // Wake up at least every FRAME so the countdown's seconds actually tick,
        // and the deadline above is noticed within a frame of passing.
        let timeout = POLL.saturating_sub(last_tick.elapsed()).min(FRAME);
        match event::poll(timeout) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if let Some(msg) = map_key(&key, model.phase())
                        && let Some(req) = model.update(msg)
                    {
                        transport.send(req);
                    }
                }
                // The approve gate follows the terminal: shrink below the card's
                // priority fields and `y` dies, grow back and it re-arms.
                Ok(Event::Resize(width, height)) => {
                    if let Some(req) = model.update(Msg::Resize { width, height }) {
                        transport.send(req);
                    }
                }
                _ => {}
            },
            Ok(false) => {}
            Err(_) => return EXIT_FATAL,
        }

        // Drain everything the worker has answered since the last pass; stream
        // each terminal decision the moment its answer lands (invariant #7 —
        // one JSON line per decision, non-TTY stdout only).
        while let Some(reply) = transport.try_recv() {
            if let Some(req) = model.update(Msg::Reply(reply)) {
                transport.send(req);
            }
            if let Some(decision) = model.take_decision()
                && stream_decisions
            {
                let _ = writeln!(io::stdout(), "{}", decision_line(&decision));
            }
            // History pipeline (Stage 7): stamp → persist → push back, in the
            // SAME pass, before the next render (the order is normative). A
            // decision made here is stamped `now`; a server outcome is
            // stamped `arrival − age` (§3.9).
            if let Some(d) = model.take_decided_outcome() {
                history.record(
                    &mut model,
                    HistoryEntry {
                        unix: now_unix(),
                        id: d.id,
                        state: d.state,
                        to: Some(d.to),
                        amount_wei: Some(d.amount_wei),
                        chain_id: Some(d.chain_id),
                        tx_hash: d.tx_hash,
                        reason: d.reason,
                    },
                );
            }
            for outcome in model.take_server_outcomes() {
                history.record(
                    &mut model,
                    HistoryEntry {
                        unix: now_unix().saturating_sub(outcome.age_secs),
                        id: outcome.id,
                        state: outcome.state,
                        to: None,
                        amount_wei: None,
                        chain_id: None,
                        tx_hash: outcome.tx_hash,
                        reason: outcome.reason,
                    },
                );
            }
        }

        if last_tick.elapsed() >= POLL {
            if let Some(req) = model.update(Msg::Tick) {
                transport.send(req);
            }
            last_tick = Instant::now();
        }

        if model.should_quit() {
            return EXIT_ABORTED;
        }
    }
}

/// Whether the open card's deadline has passed and nothing is on the wire yet.
///
/// A decision already in flight is left alone: the server, not the clock, gets the
/// last word on an item we have already answered.
fn deadline_passed(phase: &Phase, now_unix: u64) -> bool {
    matches!(
        phase,
        Phase::Watching { confirm: Some(c), .. }
            if !c.is_resolving() && now_unix >= c.card().not_after_unix
    )
}

/// The one line a caller parses (invariant #7). It names **what happened to the
/// money**, and carries a detail only when we actually know it: an item another
/// session executed comes back as `approved` with no hash, because the hash was
/// never ours to report.
fn decision_line(decision: &Decision) -> String {
    let value = match decision {
        Decision {
            kind: DecisionKind::Approved,
            tx_hash: Some(tx_hash),
            ..
        } => json!({ "decision": "approved", "tx_hash": tx_hash }),
        Decision {
            kind: DecisionKind::Approved,
            ..
        } => json!({ "decision": "approved" }),
        Decision {
            kind: DecisionKind::Rejected,
            ..
        } => json!({ "decision": "rejected" }),
        Decision {
            kind: DecisionKind::Expired,
            ..
        } => json!({ "decision": "expired" }),
        Decision {
            kind: DecisionKind::Failed,
            reason: Some(reason),
            ..
        } => json!({ "decision": "failed", "reason": reason }),
        Decision {
            kind: DecisionKind::Failed,
            ..
        } => json!({ "decision": "failed" }),
    };
    value.to_string()
}

/// Wall-clock seconds since the Unix epoch, used to render the expiry countdown.
///
/// A clock that cannot be read reports `u64::MAX` — *past every deadline* — so the
/// countdown floors at zero rather than granting the approval unbounded time. The
/// broken clock fails closed, like everything else on this path (`AGENTS.md` #5).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(u64::MAX, |d| d.as_secs())
}

/// Block until the next key press (so a human can read a fatal message).
fn wait_for_key() {
    loop {
        if let Ok(Event::Key(k)) = event::read()
            && k.kind == KeyEventKind::Press
        {
            break;
        }
    }
}

fn fatal_code(err: &TransportError) -> u8 {
    match err {
        TransportError::UnsupportedProto(_) => EXIT_UPGRADE,
        _ => EXIT_FATAL,
    }
}

/// Map a key press to a message, given the current phase. Returns `None` for keys
/// with no meaning in that phase.
fn map_key(key: &KeyEvent, phase: &Phase) -> Option<Msg> {
    let confirm = match phase {
        Phase::Watching { confirm, .. } => confirm.as_deref(),
        _ => None,
    };
    // Ctrl-C quits — but never walks away from an open confirmation without
    // deciding: default-deny (AGENTS.md #5).
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Some(if confirm.is_some() {
            Msg::Reject
        } else {
            Msg::Quit
        });
    }
    match phase {
        // Waiting for the handshake / showing a fatal screen: no interactive keys
        // (both are handled by `wait_for_key`).
        Phase::Connecting | Phase::Fatal(_) => None,
        Phase::Authing { .. } => match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() => Some(Msg::PinDigit(c)),
            KeyCode::Backspace => Some(Msg::PinBackspace),
            KeyCode::Enter => Some(Msg::PinSubmit),
            KeyCode::Esc => Some(Msg::Quit),
            _ => None,
        },
        // The queue: no decision is pending, so quitting is free.
        Phase::Watching {
            confirm: None,
            view: View::Queue,
            ..
        } => match key.code {
            KeyCode::Up | KeyCode::Char('k') => Some(Msg::MoveUp),
            KeyCode::Down | KeyCode::Char('j') => Some(Msg::MoveDown),
            KeyCode::Enter => Some(Msg::Open),
            KeyCode::Char('d') => Some(Msg::View(View::Dashboard)),
            KeyCode::Char('r') => Some(Msg::View(View::Receive)),
            KeyCode::Char('h') => Some(Msg::View(View::Activity)),
            KeyCode::Char('q') => Some(Msg::Quit),
            _ => None,
        },
        // Receive is display-only: back to the queue, or quit. Everything
        // else — navigation, Enter, y/n — is dead here (and `Msg::Open` is
        // refused by the model too, the key map is not the boundary).
        Phase::Watching {
            confirm: None,
            view: View::Receive,
            ..
        } => match key.code {
            KeyCode::Char('a') | KeyCode::Esc => Some(Msg::View(View::Queue)),
            KeyCode::Char('d') => Some(Msg::View(View::Dashboard)),
            KeyCode::Char('h') => Some(Msg::View(View::Activity)),
            KeyCode::Char('q') => Some(Msg::Quit),
            _ => None,
        },
        // The Dashboard is display-only: navigation and quit, nothing else.
        Phase::Watching {
            confirm: None,
            view: View::Dashboard,
            ..
        } => match key.code {
            KeyCode::Char('a') | KeyCode::Esc => Some(Msg::View(View::Queue)),
            KeyCode::Char('r') => Some(Msg::View(View::Receive)),
            KeyCode::Char('h') => Some(Msg::View(View::Activity)),
            KeyCode::Char('q') => Some(Msg::Quit),
            _ => None,
        },
        // Activity is display-only: navigation, the outcome filter, quit.
        Phase::Watching {
            confirm: None,
            view: View::Activity,
            ..
        } => match key.code {
            KeyCode::Char('a') | KeyCode::Esc => Some(Msg::View(View::Queue)),
            KeyCode::Char('d') => Some(Msg::View(View::Dashboard)),
            KeyCode::Char('r') => Some(Msg::View(View::Receive)),
            KeyCode::Char('f') => Some(Msg::Filter),
            KeyCode::Char('q') => Some(Msg::Quit),
            _ => None,
        },
        // An open card IS the confirmation: it is left by deciding, never by
        // closing. A high-risk item takes the PIN on the same screen.
        Phase::Watching {
            confirm: Some(c), ..
        } => {
            if c.pin_len().is_some() {
                match key.code {
                    KeyCode::Char(d) if d.is_ascii_digit() => Some(Msg::PinDigit(d)),
                    KeyCode::Backspace => Some(Msg::PinBackspace),
                    KeyCode::Enter => Some(Msg::PinSubmit),
                    KeyCode::Esc => Some(Msg::Reject),
                    _ => None,
                }
            } else {
                match key.code {
                    KeyCode::Char('y' | 'Y') => Some(Msg::Approve),
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(Msg::Reject),
                    _ => None,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustok_console::app::Pin;
    use rustok_console::protocol::{
        AuthOutcome, Card, ContextOutcome, GetOutcome, Kind, Risk, Summary, WalletContext,
    };
    use rustok_console::transport::Reply;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn authing() -> Phase {
        Phase::Authing {
            pin: Pin::default(),
            error: None,
        }
    }

    fn watching() -> Phase {
        Phase::Watching {
            items: vec![],
            selected: 0,
            confirm: None,
            notice: None,
            view: View::Queue,
        }
    }

    fn receiving() -> Phase {
        Phase::Watching {
            items: vec![],
            selected: 0,
            confirm: None,
            notice: None,
            view: View::Receive,
        }
    }

    fn confirming(high_risk: bool) -> Model {
        confirming_at(high_risk, 1)
    }

    /// A model parked on an open confirmation — the only way to build one from
    /// outside the crate, and the same path the real loop takes.
    fn confirming_at(high_risk: bool, not_after_unix: u64) -> Model {
        let mut m = Model::new();
        // The size report main sends at startup — a standard 80×24 terminal.
        m.update(Msg::Resize {
            width: 80,
            height: 24,
        });
        m.update(Msg::Reply(Reply::Hello {
            server: "s".to_owned(),
        }));
        m.update(Msg::PinDigit('1'));
        m.update(Msg::PinSubmit);
        assert!(matches!(
            m.update(Msg::Reply(Reply::Auth(AuthOutcome::Ok))),
            Some(rustok_console::transport::Request::Context)
        ));
        m.update(Msg::Reply(Reply::Context(ContextOutcome::Ok(Box::new(
            WalletContext {
                address: "0x489Fe09Fbb489Fe09Fbb489Fe09Fbb489F9Fbbbb".to_owned(),
                balances: vec![],
                allowed_chains: vec![1],
            },
        )))));
        m.update(Msg::View(View::Queue)); // Stage-5 home is Dashboard
        m.update(Msg::Tick);
        m.update(Msg::Reply(Reply::List(vec![Summary {
            id: "a".to_owned(),
            kind: Kind::Send,
            chain_id: 1,
            to: "0xabc".to_owned(),
            amount_wei: "0".to_owned(),
            risk: Risk::Safe,
            high_risk,
            not_after_unix,
        }])));
        m.update(Msg::Open);
        m.update(Msg::Reply(Reply::Get(GetOutcome::Card(Box::new(Card {
            id: "a".to_owned(),
            chain_id: 1,
            to: "0xabc".to_owned(),
            amount_wei: "0".to_owned(),
            decoded_call: None,
            high_risk,
            high_risk_reasons: vec![],
            raw_data: "0x".to_owned(),
            not_after_unix,
        })))));
        m
    }

    #[test]
    fn ctrl_c_quits_when_no_decision_is_pending() {
        let k = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(map_key(&k, &Phase::Connecting), Some(Msg::Quit)));
        assert!(matches!(map_key(&k, &authing()), Some(Msg::Quit)));
        assert!(matches!(map_key(&k, &watching()), Some(Msg::Quit)));
    }

    #[test]
    fn ctrl_c_on_an_open_confirmation_rejects_it() {
        // Default-deny (AGENTS.md #5): walking away must never leave an item
        // pending for the next console to approve.
        let k = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let m = confirming(false);
        assert!(matches!(map_key(&k, m.phase()), Some(Msg::Reject)));
        let m = confirming(true);
        assert!(matches!(map_key(&k, m.phase()), Some(Msg::Reject)));
    }

    #[test]
    fn a_confirmation_maps_y_to_approve_and_n_or_esc_to_reject() {
        let m = confirming(false);
        assert!(matches!(
            map_key(&key(KeyCode::Char('y')), m.phase()),
            Some(Msg::Approve)
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Char('n')), m.phase()),
            Some(Msg::Reject)
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Esc), m.phase()),
            Some(Msg::Reject)
        ));
        // there is no "just close the card" key any more
        assert!(map_key(&key(KeyCode::Backspace), m.phase()).is_none());
        assert!(map_key(&key(KeyCode::Char('q')), m.phase()).is_none());
    }

    #[test]
    fn the_high_risk_pin_prompt_takes_digits_and_esc_still_rejects() {
        let mut m = confirming(true);
        m.update(Msg::Approve); // opens the PIN prompt
        assert!(matches!(
            map_key(&key(KeyCode::Char('7')), m.phase()),
            Some(Msg::PinDigit('7'))
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Backspace), m.phase()),
            Some(Msg::PinBackspace)
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Enter), m.phase()),
            Some(Msg::PinSubmit)
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Esc), m.phase()),
            Some(Msg::Reject)
        ));
        // 'y' is a no-op here: Enter submits the PIN
        assert!(map_key(&key(KeyCode::Char('y')), m.phase()).is_none());
    }

    #[test]
    fn session_exit_codes_stay_distinct_and_keep_their_v01_values() {
        // The four surviving codes are preserved, not renumbered (Gate-1):
        // a caller of v0.1 that only handled session ends keeps working.
        assert_eq!(EXIT_FATAL, 1);
        assert_eq!(EXIT_UPGRADE, 2);
        assert_eq!(EXIT_NO_TTY, 3);
        assert_eq!(EXIT_ABORTED, 6);
        let codes = [EXIT_FATAL, EXIT_UPGRADE, EXIT_NO_TTY, EXIT_ABORTED];
        for (i, a) in codes.iter().enumerate() {
            for b in &codes[i + 1..] {
                assert_ne!(a, b, "exit codes must never collide");
            }
        }
    }

    #[test]
    fn a_decision_is_drained_from_the_model_exactly_once() {
        // The run loop drains `take_decision` after every reply and streams
        // one JSON line per decision (non-TTY stdout only). The drain contract
        // lives on the model: exactly one Decision per terminal answer.
        let mut m = confirming(false);
        m.update(Msg::Approve);
        m.update(Msg::Reply(Reply::Resolve(
            rustok_console::protocol::ResolveOutcome::Executed {
                tx_hash: "0xabc".to_owned(),
            },
        )));
        let d = m.take_decision().expect("one decision per terminal answer");
        assert_eq!(
            decision_line(&d),
            r#"{"decision":"approved","tx_hash":"0xabc"}"#
        );
        assert!(
            m.take_decision().is_none(),
            "a second drain must not re-emit the same decision line"
        );
    }

    #[test]
    fn auth_phase_maps_digits_backspace_enter() {
        assert!(matches!(
            map_key(&key(KeyCode::Char('7')), &authing()),
            Some(Msg::PinDigit('7'))
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Backspace), &authing()),
            Some(Msg::PinBackspace)
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Enter), &authing()),
            Some(Msg::PinSubmit)
        ));
        // a non-digit letter is ignored on the PIN screen
        assert!(map_key(&key(KeyCode::Char('a')), &authing()).is_none());
    }

    #[test]
    fn watch_phase_maps_navigation_and_actions() {
        assert!(matches!(
            map_key(&key(KeyCode::Up), &watching()),
            Some(Msg::MoveUp)
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Down), &watching()),
            Some(Msg::MoveDown)
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Enter), &watching()),
            Some(Msg::Open)
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Char('q')), &watching()),
            Some(Msg::Quit)
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Char('r')), &watching()),
            Some(Msg::View(View::Receive))
        ));
    }

    #[test]
    fn the_receive_view_maps_only_back_and_quit() {
        // `a` and Esc return to the queue; `q` still quits.
        assert!(matches!(
            map_key(&key(KeyCode::Char('a')), &receiving()),
            Some(Msg::View(View::Queue))
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Esc), &receiving()),
            Some(Msg::View(View::Queue))
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Char('q')), &receiving()),
            Some(Msg::Quit)
        ));
        // A display-only view: nothing else may act — Enter must not open a
        // card behind the screen, y/n must not decide anything.
        for dead in [
            KeyCode::Enter,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Char('y'),
            KeyCode::Char('n'),
            KeyCode::Char('r'),
            KeyCode::Char('7'),
        ] {
            assert!(
                map_key(&key(dead), &receiving()).is_none(),
                "{dead:?} must be dead on the Receive view"
            );
        }
    }

    #[test]
    fn a_view_key_never_reaches_an_open_confirmation() {
        // With a card open the confirm arm owns the keys: `r` is not a tab
        // switch there (and the model refuses the switch besides).
        let m = confirming(false);
        assert!(map_key(&key(KeyCode::Char('r')), m.phase()).is_none());
        assert!(map_key(&key(KeyCode::Char('a')), m.phase()).is_none());
    }

    #[test]
    fn connecting_and_fatal_ignore_ordinary_keys() {
        assert!(map_key(&key(KeyCode::Enter), &Phase::Connecting).is_none());
        assert!(map_key(&key(KeyCode::Char('x')), &Phase::Connecting).is_none());
        assert!(
            map_key(
                &key(KeyCode::Enter),
                &Phase::Fatal(TransportError::ConnectionLost)
            )
            .is_none()
        );
    }

    #[test]
    fn the_deadline_denies_an_open_card_and_nothing_else() {
        // Nothing is open: the clock has nothing to say, however late it is.
        assert!(!deadline_passed(&watching(), u64::MAX));
        assert!(!deadline_passed(&authing(), u64::MAX));

        let m = confirming_at(false, 1_000);
        assert!(
            !deadline_passed(m.phase(), 999),
            "a second before the deadline the human still owns the decision"
        );
        assert!(
            deadline_passed(m.phase(), 1_000),
            "at the deadline the clock says no (AGENTS.md #5)"
        );
        assert!(deadline_passed(m.phase(), 1_001));
    }

    #[test]
    fn a_decision_on_the_wire_outranks_the_deadline() {
        let mut m = confirming_at(false, 1_000);
        m.update(Msg::Approve);

        assert!(
            !deadline_passed(m.phase(), u64::MAX),
            "an approve is already on the wire — the server, not the clock, has the \
             last word; expiring here would deny what we just approved"
        );
    }

    #[test]
    fn the_decision_line_names_what_happened_to_the_money() {
        assert_eq!(
            decision_line(&Decision {
                kind: DecisionKind::Approved,
                tx_hash: Some("0xabc".to_owned()),
                reason: None,
            }),
            r#"{"decision":"approved","tx_hash":"0xabc"}"#
        );
        assert_eq!(
            decision_line(&Decision {
                kind: DecisionKind::Rejected,
                tx_hash: None,
                reason: None,
            }),
            r#"{"decision":"rejected"}"#
        );
        // The same server answer, denied by the deadline rather than by a human.
        assert_eq!(
            decision_line(&Decision {
                kind: DecisionKind::Expired,
                tx_hash: None,
                reason: None,
            }),
            r#"{"decision":"expired"}"#
        );
        assert_eq!(
            decision_line(&Decision {
                kind: DecisionKind::Failed,
                tx_hash: None,
                reason: Some("nonce too low".to_owned()),
            }),
            r#"{"decision":"failed","reason":"nonce too low"}"#
        );
    }

    #[test]
    fn an_approval_by_another_session_reports_no_hash_we_never_saw() {
        let line = decision_line(&Decision {
            kind: DecisionKind::Approved,
            tx_hash: None,
            reason: None,
        });

        assert_eq!(line, r#"{"decision":"approved"}"#);
        assert!(
            !line.contains("tx_hash"),
            "we never invent a hash the server did not give us"
        );
    }

    #[test]
    fn the_decision_line_survives_a_hostile_server_reason() {
        // `reason` is server-controlled text landing on a caller's stdout.
        let line = decision_line(&Decision {
            kind: DecisionKind::Failed,
            tx_hash: None,
            reason: Some("\"}\n{\"decision\":\"approved\"".to_owned()),
        });

        assert!(
            !line.contains('\n'),
            "the decision is exactly one line — a reason cannot forge a second one"
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&line).expect("one parsable JSON object");
        assert_eq!(parsed["decision"], "failed");
        assert_eq!(parsed["reason"], "\"}\n{\"decision\":\"approved\"");
    }

    #[test]
    fn fatal_code_distinguishes_upgrade_from_the_rest() {
        assert_eq!(
            fatal_code(&TransportError::UnsupportedProto(vec![2])),
            EXIT_UPGRADE
        );
        assert_eq!(fatal_code(&TransportError::NotConnected), EXIT_FATAL);
        assert_eq!(fatal_code(&TransportError::ConnectionLost), EXIT_FATAL);
        assert_eq!(
            fatal_code(&TransportError::Protocol("x".to_owned())),
            EXIT_FATAL
        );
    }

    fn dashboarding() -> Phase {
        Phase::Watching {
            items: vec![],
            selected: 0,
            confirm: None,
            notice: None,
            view: View::Dashboard,
        }
    }

    #[test]
    fn the_dashboard_view_maps_navigation_and_quit_only() {
        assert!(matches!(
            map_key(&key(KeyCode::Char('a')), &dashboarding()),
            Some(Msg::View(View::Queue))
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Esc), &dashboarding()),
            Some(Msg::View(View::Queue))
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Char('r')), &dashboarding()),
            Some(Msg::View(View::Receive))
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Char('q')), &dashboarding()),
            Some(Msg::Quit)
        ));
        // Display-only: Enter must not open a card behind the screen.
        for dead in [
            KeyCode::Enter,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Char('y'),
            KeyCode::Char('n'),
            KeyCode::Char('d'),
            KeyCode::Char('7'),
        ] {
            assert!(
                map_key(&key(dead), &dashboarding()).is_none(),
                "{dead:?} must be dead on the Dashboard"
            );
        }
    }

    #[test]
    fn every_view_reaches_every_other_view() {
        assert!(matches!(
            map_key(&key(KeyCode::Char('d')), &watching()),
            Some(Msg::View(View::Dashboard))
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Char('d')), &receiving()),
            Some(Msg::View(View::Dashboard))
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Char('r')), &dashboarding()),
            Some(Msg::View(View::Receive))
        ));
    }

    // ── Stage 7: the Activity view's keys and the history log's file side ──

    fn activitying() -> Phase {
        Phase::Watching {
            items: vec![],
            selected: 0,
            confirm: None,
            notice: None,
            view: View::Activity,
        }
    }

    #[test]
    fn h_reaches_activity_from_every_display_view_and_f_cycles_there() {
        for phase in [watching(), receiving(), dashboarding()] {
            assert!(
                matches!(
                    map_key(&key(KeyCode::Char('h')), &phase),
                    Some(Msg::View(View::Activity))
                ),
                "h opens the Activity view"
            );
        }
        assert!(matches!(
            map_key(&key(KeyCode::Char('f')), &activitying()),
            Some(Msg::Filter)
        ));
        assert!(
            map_key(&key(KeyCode::Char('f')), &watching()).is_none(),
            "the filter key exists only on the Activity view"
        );
        assert!(matches!(
            map_key(&key(KeyCode::Char('a')), &activitying()),
            Some(Msg::View(View::Queue))
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Char('d')), &activitying()),
            Some(Msg::View(View::Dashboard))
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Char('r')), &activitying()),
            Some(Msg::View(View::Receive))
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Char('q')), &activitying()),
            Some(Msg::Quit)
        ));
    }

    use rustok_console::protocol::OutcomeState;

    fn tmp_log(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rustok_console_hist_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmp dir");
        dir.join("log.jsonl")
    }

    fn hist(id: &str, unix: u64) -> HistoryEntry {
        HistoryEntry {
            unix,
            id: id.to_owned(),
            state: OutcomeState::Denied,
            to: None,
            amount_wei: None,
            chain_id: None,
            tx_hash: None,
            reason: None,
        }
    }

    #[test]
    fn load_merges_duplicate_ids_field_by_field_never_by_timestamp() {
        // Gate-2 BLOCKER-1: console P1 decides a card (rich record, unix =
        // now); a second long-lived console P2 later sees the same id via
        // activity polling (poor record, unix = arrival − age — a DIFFERENT
        // formula, not guaranteed smaller). Both lines are in the file. A
        // whole-record replace by "bigger unix" trades the rich record for
        // the poor one; the load must merge field-by-field instead.
        let path = tmp_log("merge_rich_first");
        let mut rich = hist("a", 100);
        rich.to = Some("0xTO".to_owned());
        rich.amount_wei = Some("5".to_owned());
        rich.tx_hash = Some("0xfeed".to_owned());
        append_history_line(&path, &rich).expect("append rich");
        append_history_line(&path, &hist("a", 200)).expect("append poor, newer stamp");

        let loaded = load_history(&path);
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(
            loaded.entries[0].to.as_deref(),
            Some("0xTO"),
            "rich details survive a poorer duplicate with a newer stamp"
        );
        assert_eq!(loaded.entries[0].tx_hash.as_deref(), Some("0xfeed"));

        // The reverse file order merges too: a poor base gains the rich fields.
        let path = tmp_log("merge_poor_first");
        append_history_line(&path, &hist("b", 200)).expect("append poor");
        let mut rich = hist("b", 100);
        rich.to = Some("0xTO".to_owned());
        append_history_line(&path, &rich).expect("append rich");
        let loaded = load_history(&path);
        assert_eq!(
            loaded.entries[0].to.as_deref(),
            Some("0xTO"),
            "a later rich line fills the poor base's gaps"
        );
    }

    #[test]
    fn a_torn_non_utf8_tail_never_hides_the_healthy_lines() {
        // Gate-2 BLOCKER-2: a process killed mid-append (SIGKILL on the
        // container) leaves a torn, possibly non-UTF8 tail. That is line
        // corruption — counted and noted — never "the whole history is gone".
        let path = tmp_log("torn");
        append_history_line(&path, &hist("good", 10)).expect("append");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(b"\xff\xfe{torn"))
            .expect("torn tail");

        let loaded = load_history(&path);
        assert_eq!(
            loaded.entries.len(),
            1,
            "the good line must survive a torn tail"
        );
        assert_eq!(loaded.skipped, 1, "the torn tail is counted as corruption");
    }

    #[test]
    fn the_log_roundtrips_and_a_duplicate_id_keeps_the_newest_stamp() {
        let path = tmp_log("roundtrip");
        append_history_line(&path, &hist("a", 10)).expect("append");
        append_history_line(&path, &hist("b", 20)).expect("append");
        // The same id again — a second console appended the same server
        // outcome with a newer stamp (spec /check-3).
        append_history_line(&path, &hist("a", 30)).expect("append");

        let loaded = load_history(&path);
        assert_eq!(loaded.skipped, 0);
        assert_eq!(loaded.entries.len(), 2, "load dedups by id");
        assert_eq!(loaded.raw_lines, 3, "the raw line count sees every line");
        let a = loaded
            .entries
            .iter()
            .find(|e| e.id == "a")
            .expect("a survives");
        assert_eq!(
            a.unix, 10,
            "the FIRST record in file order stays the base (never a timestamp contest)"
        );
    }

    #[test]
    fn corrupt_lines_are_skipped_and_counted_never_fatal() {
        let path = tmp_log("corrupt");
        append_history_line(&path, &hist("good", 10)).expect("append");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(b"{not json\ntail-without-brace"))
            .expect("hostile bytes");

        let loaded = load_history(&path);
        assert_eq!(loaded.entries.len(), 1, "the good line survives");
        assert_eq!(
            loaded.skipped, 2,
            "both hostile lines are counted, not fatal"
        );
        // A missing file is a normal empty history — NOT the unreadable case.
        let missing = load_history(Path::new("/nonexistent/x.jsonl"));
        assert!(missing.entries.is_empty() && !missing.unreadable);
    }

    #[test]
    fn an_unreadable_file_is_flagged_never_a_healthy_empty_history() {
        // Gate-2 BLOCKER-2: permissions/I/O trouble is NOT "no history yet".
        let path = tmp_log("unreadable");
        append_history_line(&path, &hist("a", 10)).expect("append");
        let mut perms = std::fs::metadata(&path).expect("meta").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o000);
        std::fs::set_permissions(&path, perms).expect("chmod");

        let loaded = load_history(&path);
        assert!(loaded.unreadable, "a read failure must be flagged");

        let mut model = Model::new();
        let log = HistoryLog::open_at(Some(path.clone()), &mut model);
        assert!(
            model
                .history_note()
                .is_some_and(|n| n.contains("could not be read")),
            "open surfaces the unreadable log as a note"
        );
        assert!(log.path.is_some(), "appends stay best-effort");
    }

    #[test]
    fn open_at_none_is_session_only_and_the_log_file_is_private() {
        let mut model = Model::new();
        let log = HistoryLog::open_at(None, &mut model);
        assert!(log.path.is_none());
        assert!(
            model
                .history_note()
                .is_some_and(|n| n.contains("session-only")),
            "no path configured reads as an explicit note"
        );

        // МИНОР-5: a fresh log file is created 0600 — the wallet's decision
        // history is nobody else's business.
        let path = tmp_log("private");
        append_history_line(&path, &hist("a", 10)).expect("append");
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&path).expect("meta").permissions(),
        );
        assert_eq!(mode & 0o777, 0o600, "the log is owner-only");
    }

    #[test]
    fn compaction_keeps_the_newest_cap_and_heals_the_file() {
        let path = tmp_log("compact");
        let total = 2 * HISTORY_CAP + 1;
        let mut body = String::new();
        for i in 0..total {
            body.push_str(&serde_json::to_string(&hist(&format!("id{i:05}"), i as u64)).unwrap());
            body.push('\n');
        }
        body.push_str("{corrupt line\n");
        std::fs::write(&path, body).expect("seed file");

        let loaded = load_history(&path);
        assert_eq!(loaded.entries.len(), total);
        assert_eq!(loaded.skipped, 1);
        compact_history(&path);

        let after = load_history(&path);
        assert_eq!(
            after.entries.len(),
            HISTORY_CAP,
            "compaction keeps exactly the cap"
        );
        assert_eq!(
            after.skipped, 0,
            "the corrupt line is gone — the file healed"
        );
        assert!(
            after
                .entries
                .iter()
                .all(|e| e.unix > (total - 1 - HISTORY_CAP) as u64),
            "what was dropped is exactly the oldest tail"
        );
    }

    #[test]
    fn record_appends_new_ids_and_fills_missing_on_known_ones() {
        let path = tmp_log("record");
        let mut model = Model::new();
        let mut log = HistoryLog {
            path: Some(path.clone()),
            seen: HashSet::new(),
            appended: 0,
        };

        log.record(&mut model, hist("a", 10));
        assert_eq!(model.history().len(), 1);
        // The same id again, now carrying a tx hash: fill-missing in memory,
        // NO second file line (append-only keeps the first record).
        let mut richer = hist("a", 11);
        richer.tx_hash = Some("0xfeed".to_owned());
        log.record(&mut model, richer);
        assert_eq!(model.history().len(), 1, "no duplicate row in the model");
        assert_eq!(
            model.history()[0].tx_hash.as_deref(),
            Some("0xfeed"),
            "the missing detail filled in memory"
        );
        let on_disk = load_history(&path);
        assert_eq!(
            on_disk.entries.len(),
            1,
            "the file keeps its first record only"
        );
        assert_eq!(on_disk.entries[0].tx_hash, None);
    }

    #[test]
    fn a_resident_session_compacts_periodically_not_only_at_startup() {
        // Gate-2 МИНОР-3: seed a file bloated with duplicate lines of one id,
        // arm the append counter one short of the threshold — the very next
        // record() must fire a compaction that shrinks the file.
        let path = tmp_log("periodic");
        let mut body = String::new();
        for i in 0..600u64 {
            body.push_str(&serde_json::to_string(&hist("dup", i)).unwrap());
            body.push('\n');
        }
        std::fs::write(&path, body).expect("seed bloated file");

        let mut model = Model::new();
        let mut log = HistoryLog {
            path: Some(path.clone()),
            seen: std::iter::once("dup".to_owned()).collect(),
            appended: HISTORY_CAP - 1,
        };
        log.record(&mut model, hist("fresh", 1000));

        let after = load_history(&path);
        assert_eq!(
            after.raw_lines, 2,
            "the mid-session compaction collapsed the duplicate bloat"
        );
        assert_eq!(log.appended, 0, "the counter re-arms after compacting");
    }

    #[test]
    fn a_failed_write_switches_to_session_only_with_a_visible_note() {
        let mut model = Model::new();
        let mut log = HistoryLog {
            path: Some(PathBuf::from("/nonexistent-dir/x.jsonl")),
            seen: HashSet::new(),
            appended: 0,
        };
        log.record(&mut model, hist("a", 10));
        assert!(
            log.path.is_none(),
            "persistence switched off after the failure"
        );
        assert_eq!(model.history().len(), 1, "the session view still works");
        assert!(
            model
                .history_note()
                .is_some_and(|n| n.contains("not being persisted")),
            "the degradation is visible, not silent"
        );
    }
}
