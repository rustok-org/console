//! Rustok Console — terminal approval screen (the human face of the wallet).
//!
//! v0.1 (C-PR-1a): connect → hello → PIN → **watch** the queue read-only. Keys are
//! mapped to [`Msg`] and folded into the [`Model`]; the socket worker runs on its
//! own thread so a slow core never freezes the UI. `approve`/`deny` land in
//! C-PR-1b. UI goes to stderr (ratatui's alternate screen); the exit code carries
//! the outcome.

use std::io::{self, Stderr};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use rustok_console::app::{Model, Msg, Phase};
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

// Exit codes (AGENTS.md #7). C-PR-1a covers the read-only subset; approve/deny
// outcomes (approved/rejected/expired) arrive with C-PR-1b.
const EXIT_ABORTED: u8 = 0;
const EXIT_FATAL: u8 = 1;
const EXIT_UPGRADE: u8 = 2;
const EXIT_NO_TTY: u8 = 3;

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
        if terminal.draw(|f| ui::render(f, &model)).is_err() {
            return EXIT_FATAL;
        }

        if let Phase::Fatal(err) = model.phase() {
            let code = fatal_code(err);
            wait_for_key();
            return code;
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
    // Ctrl-C always quits.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Some(Msg::Quit);
    }
    match phase {
        // Waiting for the handshake / showing a fatal reason: no interactive keys
        // (fatal is handled by `wait_for_key`).
        Phase::Connecting | Phase::Fatal(_) => None,
        Phase::Authing { .. } => match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() => Some(Msg::PinDigit(c)),
            KeyCode::Backspace => Some(Msg::PinBackspace),
            KeyCode::Enter => Some(Msg::PinSubmit),
            KeyCode::Esc => Some(Msg::Quit),
            _ => None,
        },
        Phase::Watching { .. } => match key.code {
            KeyCode::Up | KeyCode::Char('k') => Some(Msg::MoveUp),
            KeyCode::Down | KeyCode::Char('j') => Some(Msg::MoveDown),
            KeyCode::Enter => Some(Msg::Open),
            KeyCode::Esc | KeyCode::Backspace => Some(Msg::Back),
            KeyCode::Char('q') => Some(Msg::Quit),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustok_console::app::Pin;

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
            card: None,
            note: None,
        }
    }

    #[test]
    fn ctrl_c_quits_from_any_phase() {
        let k = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(map_key(&k, &Phase::Connecting), Some(Msg::Quit)));
        assert!(matches!(map_key(&k, &authing()), Some(Msg::Quit)));
        assert!(matches!(map_key(&k, &watching()), Some(Msg::Quit)));
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
            map_key(&key(KeyCode::Esc), &watching()),
            Some(Msg::Back)
        ));
        assert!(matches!(
            map_key(&key(KeyCode::Char('q')), &watching()),
            Some(Msg::Quit)
        ));
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
