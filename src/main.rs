//! Rustok Console — terminal approval screen (the human face of the wallet).
//!
//! v0.1: connect → hello → PIN → watch the queue → approve or deny. Keys are
//! mapped to [`Msg`] and folded into the [`Model`]; the socket worker runs on its
//! own thread so a slow core never freezes the UI. UI goes to stderr (ratatui's
//! alternate screen), the machine-readable decision goes to stdout, and the exit
//! code carries the outcome (invariant #7).

use std::io::{self, IsTerminal, Stderr};
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

use rustok_console::app::{ExitOutcome, Model, Msg, Phase};
use rustok_console::protocol::ResolveOutcome;
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

// Exit codes (AGENTS.md #7) — each decision outcome is distinguishable.
/// The transaction was signed and broadcast.
const EXIT_APPROVED: u8 = 0;
/// The connection broke, or an approved transaction failed to broadcast — no money
/// moved and the reason is on the screen.
const EXIT_FATAL: u8 = 1;
/// The server speaks a protocol major this build does not.
const EXIT_UPGRADE: u8 = 2;
/// No interactive terminal: an approval may never come from a pipe (invariant #4).
const EXIT_NO_TTY: u8 = 3;
/// A human said no.
const EXIT_REJECTED: u8 = 4;
/// The deadline passed before a decision.
const EXIT_EXPIRED: u8 = 5;
/// Quit from the queue without deciding anything.
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
    let (code, decision) = run(terminal, &transport);
    restore_then_announce(|| drop(restore()), decision.as_deref(), &mut io::stdout());
    ExitCode::from(code)
}

/// Leave the terminal, THEN speak on stdout — in that order and no other: the
/// decision line must never share stdout's airtime with alternate-screen escape
/// sequences a caller could capture (invariant #7). The order lives in this one
/// function so a test can pin it.
fn restore_then_announce(restore: impl FnOnce(), decision: Option<&str>, out: &mut impl io::Write) {
    restore();
    if let Some(decision) = decision {
        let _ = writeln!(out, "{decision}");
    }
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

/// The event loop: draw, read input, drain worker replies, tick the poll. Returns
/// the exit code and, once an item is terminal, the machine-readable decision.
/// A `Fatal` phase is shown until a keypress, then exits.
fn run(mut terminal: Tui, transport: &Transport) -> (u8, Option<String>) {
    let mut model = Model::new();
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
            return (EXIT_FATAL, None);
        }

        match model.phase() {
            Phase::Fatal(err) => {
                let code = fatal_code(err);
                wait_for_key();
                return (code, None);
            }
            // The item is terminal: show the answer, then carry it out in both the
            // exit code and the decision line.
            Phase::Resolved { outcome, exit } => {
                let code = exit_code(*exit);
                let decision = decision_line(outcome, *exit);
                wait_for_key();
                return (code, Some(decision));
            }
            _ => {}
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
            Err(_) => return (EXIT_FATAL, None),
        }

        // Drain everything the worker has answered since the last pass.
        while let Some(reply) = transport.try_recv() {
            if let Some(req) = model.update(Msg::Reply(reply)) {
                transport.send(req);
            }
        }

        if last_tick.elapsed() >= POLL {
            if let Some(req) = model.update(Msg::Tick) {
                transport.send(req);
            }
            last_tick = Instant::now();
        }

        if model.should_quit() {
            return (EXIT_ABORTED, None);
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
fn decision_line(outcome: &ResolveOutcome, exit: ExitOutcome) -> String {
    let value = match (exit, outcome) {
        (ExitOutcome::Approved, ResolveOutcome::Executed { tx_hash }) => {
            json!({ "decision": "approved", "tx_hash": tx_hash })
        }
        (ExitOutcome::Approved, _) => json!({ "decision": "approved" }),
        (ExitOutcome::Rejected, _) => json!({ "decision": "rejected" }),
        (ExitOutcome::Expired, _) => json!({ "decision": "expired" }),
        (ExitOutcome::Failed, ResolveOutcome::Failed { reason }) => {
            json!({ "decision": "failed", "reason": reason })
        }
        (ExitOutcome::Failed, _) => json!({ "decision": "failed" }),
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

/// The decision the process exits with. A `failed` execution is not an approval a
/// caller can act on — the transaction never made it out — so it exits fatal.
fn exit_code(exit: ExitOutcome) -> u8 {
    match exit {
        ExitOutcome::Approved => EXIT_APPROVED,
        ExitOutcome::Rejected => EXIT_REJECTED,
        ExitOutcome::Expired => EXIT_EXPIRED,
        ExitOutcome::Failed => EXIT_FATAL,
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
        // Waiting for the handshake / showing a terminal screen: no interactive keys
        // (both are handled by `wait_for_key`).
        Phase::Connecting | Phase::Fatal(_) | Phase::Resolved { .. } => None,
        Phase::Authing { .. } => match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() => Some(Msg::PinDigit(c)),
            KeyCode::Backspace => Some(Msg::PinBackspace),
            KeyCode::Enter => Some(Msg::PinSubmit),
            KeyCode::Esc => Some(Msg::Quit),
            _ => None,
        },
        // The queue: no decision is pending, so quitting is free.
        Phase::Watching { confirm: None, .. } => match key.code {
            KeyCode::Up | KeyCode::Char('k') => Some(Msg::MoveUp),
            KeyCode::Down | KeyCode::Char('j') => Some(Msg::MoveDown),
            KeyCode::Enter => Some(Msg::Open),
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
        AuthOutcome, Card, GetOutcome, Kind, Risk, Summary, TerminalState,
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
            note: None,
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
        m.update(Msg::Reply(Reply::Auth(AuthOutcome::Ok)));
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
    fn exit_code_distinguishes_every_decision() {
        assert_eq!(exit_code(ExitOutcome::Approved), EXIT_APPROVED);
        assert_eq!(exit_code(ExitOutcome::Rejected), EXIT_REJECTED);
        assert_eq!(exit_code(ExitOutcome::Expired), EXIT_EXPIRED);
        assert_eq!(exit_code(ExitOutcome::Failed), EXIT_FATAL);
        // Every process outcome a caller can see must be pairwise distinct
        // (invariant #7) — reusing a code would let one outcome impersonate
        // another, `approved` worst of all.
        let codes = [
            EXIT_APPROVED,
            EXIT_FATAL,
            EXIT_UPGRADE,
            EXIT_NO_TTY,
            EXIT_REJECTED,
            EXIT_EXPIRED,
            EXIT_ABORTED,
        ];
        for (i, a) in codes.iter().enumerate() {
            for b in &codes[i + 1..] {
                assert_ne!(a, b, "exit codes must never collide");
            }
        }
    }

    #[test]
    fn the_decision_speaks_only_after_the_terminal_is_restored() {
        use std::cell::Cell;

        struct OrderProbe<'a> {
            restored: &'a Cell<bool>,
            buf: Vec<u8>,
        }
        impl io::Write for OrderProbe<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                assert!(
                    self.restored.get(),
                    "the decision line must never race the alternate-screen \
                     escapes (invariant #7)"
                );
                self.buf.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let restored = Cell::new(false);
        let mut probe = OrderProbe {
            restored: &restored,
            buf: Vec::new(),
        };
        restore_then_announce(
            || restored.set(true),
            Some(r#"{"decision":"approved"}"#),
            &mut probe,
        );
        assert_eq!(probe.buf, b"{\"decision\":\"approved\"}\n");

        // And nothing is written at all when there is no decision.
        let mut probe = OrderProbe {
            restored: &restored,
            buf: Vec::new(),
        };
        restore_then_announce(|| restored.set(true), None, &mut probe);
        assert!(probe.buf.is_empty());
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
    }

    #[test]
    fn connecting_fatal_and_resolved_ignore_ordinary_keys() {
        assert!(map_key(&key(KeyCode::Enter), &Phase::Connecting).is_none());
        assert!(map_key(&key(KeyCode::Char('x')), &Phase::Connecting).is_none());
        assert!(
            map_key(
                &key(KeyCode::Enter),
                &Phase::Fatal(TransportError::ConnectionLost)
            )
            .is_none()
        );
        assert!(
            map_key(
                &key(KeyCode::Char('y')),
                &Phase::Resolved {
                    outcome: rustok_console::protocol::ResolveOutcome::Denied,
                    exit: ExitOutcome::Rejected,
                }
            )
            .is_none(),
            "a resolved item can no longer be approved"
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
            decision_line(
                &ResolveOutcome::Executed {
                    tx_hash: "0xabc".to_owned()
                },
                ExitOutcome::Approved
            ),
            r#"{"decision":"approved","tx_hash":"0xabc"}"#
        );
        assert_eq!(
            decision_line(&ResolveOutcome::Denied, ExitOutcome::Rejected),
            r#"{"decision":"rejected"}"#
        );
        // The same server answer, denied by the deadline rather than by a human.
        assert_eq!(
            decision_line(&ResolveOutcome::Denied, ExitOutcome::Expired),
            r#"{"decision":"expired"}"#
        );
        assert_eq!(
            decision_line(
                &ResolveOutcome::Failed {
                    reason: "nonce too low".to_owned()
                },
                ExitOutcome::Failed
            ),
            r#"{"decision":"failed","reason":"nonce too low"}"#
        );
    }

    #[test]
    fn an_approval_by_another_session_reports_no_hash_we_never_saw() {
        let line = decision_line(
            &ResolveOutcome::AlreadyResolved {
                state: TerminalState::Executed,
            },
            ExitOutcome::Approved,
        );

        assert_eq!(line, r#"{"decision":"approved"}"#);
        assert!(
            !line.contains("tx_hash"),
            "we never invent a hash the server did not give us"
        );
    }

    #[test]
    fn the_decision_line_survives_a_hostile_server_reason() {
        // `reason` is server-controlled text landing on a caller's stdout.
        let line = decision_line(
            &ResolveOutcome::Failed {
                reason: "\"}\n{\"decision\":\"approved\"".to_owned(),
            },
            ExitOutcome::Failed,
        );

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
}
