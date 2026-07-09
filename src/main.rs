//! Rustok Console — terminal approval screen (the human face of the wallet).
//!
//! v0.1 (C-PR-1a): connect → hello → PIN → **watch** the queue read-only. Keys are
//! mapped to [`Msg`] and folded into the [`Model`]; the socket worker runs on its
//! own thread so a slow core never freezes the UI. `approve`/`deny` land in
//! C-PR-1b. UI goes to stderr (ratatui's alternate screen); the exit code carries
//! the outcome.

use std::io::{self, Stderr};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use rustok_console::app::{ExitOutcome, Model, Msg, Phase};
use rustok_console::transport::{Transport, TransportError};
use rustok_console::ui;

/// The console's terminal — rendered to **stderr**, so stdout stays clean for a
/// machine-readable decision (invariant #7; the approval verdict lands on stdout
/// in C-PR-1b). `ratatui::init` would use stdout, so the setup is done by hand.
type Tui = Terminal<CrosstermBackend<Stderr>>;

/// Default approver socket path (overridable for tests / non-standard layouts).
const DEFAULT_SOCKET: &str = "/run/wallet/approve.sock";
/// How often the watch screen polls `list`.
const POLL: Duration = Duration::from_millis(2500);

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

    // No TTY → view-only is not possible for an interactive approver; fail clearly
    // rather than half-render. (The full no-TTY view-only gate is C-PR-1b.)
    let Ok(terminal) = try_init() else {
        eprintln!("rustok-console needs an interactive terminal (a TTY).");
        return ExitCode::from(EXIT_NO_TTY);
    };

    let transport = Transport::connect(&path);
    let code = run(terminal, &transport);
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

/// The event loop: draw, read input, drain worker replies, tick the poll. Returns
/// the exit code. A `Fatal` phase is shown until a keypress, then exits.
fn run(mut terminal: Tui, transport: &Transport) -> u8 {
    let mut model = Model::new();
    let mut last_tick = Instant::now();

    loop {
        if terminal
            .draw(|f| ui::render(f, &model, now_unix()))
            .is_err()
        {
            return EXIT_FATAL;
        }

        match model.phase() {
            Phase::Fatal(err) => {
                let code = fatal_code(err);
                wait_for_key();
                return code;
            }
            // The item is terminal: show the answer, then carry it in the exit code.
            Phase::Resolved { exit, .. } => {
                let code = exit_code(*exit);
                wait_for_key();
                return code;
            }
            _ => {}
        }

        let timeout = POLL.saturating_sub(last_tick.elapsed());
        match event::poll(timeout) {
            Ok(true) => {
                if let Ok(Event::Key(key)) = event::read()
                    && key.kind == KeyEventKind::Press
                    && let Some(msg) = map_key(&key, model.phase())
                    && let Some(req) = model.update(msg)
                {
                    transport.send(req);
                }
            }
            Ok(false) => {}
            Err(_) => return EXIT_FATAL,
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
            return EXIT_ABORTED;
        }
    }
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
    use rustok_console::protocol::{AuthOutcome, Card, GetOutcome, Kind, Risk, Summary};
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

    /// A model parked on an open confirmation — the only way to build one from
    /// outside the crate, and the same path the real loop takes.
    fn confirming(high_risk: bool) -> Model {
        let mut m = Model::new();
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
            not_after_unix: 1,
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
            not_after_unix: 1,
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
        // aborted and no-tty are distinct from approved (invariant #7)
        for code in [EXIT_ABORTED, EXIT_NO_TTY] {
            assert_ne!(code, EXIT_APPROVED);
        }
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
