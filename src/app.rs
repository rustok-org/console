//! The Model and its update logic (MVU) — pure over messages, no rendering and no
//! I/O. Keys are mapped to [`Msg`] in the event loop and transport replies arrive
//! as [`Msg::Reply`]; `update` folds them into the [`Model`] and returns at most one
//! [`transport::Request`] to send. This keeps the decision logic unit-testable
//! without a terminal or a socket.
//!
//! **One request in flight** (protocol §1): while a request is outstanding the
//! periodic `list` poll is suppressed (we do not pile up stale polls), and a user
//! action taken meanwhile is parked in a single latest-wins slot so it is not lost.

use zeroize::Zeroizing;

use crate::protocol::{AuthOutcome, Card, GetOutcome, Summary};
use crate::transport::{self, Reply, TransportError};

/// The approval PIN as it is typed. Zeroized on drop (via [`Zeroizing`]) and
/// **redacted in `Debug`** so it never lands in a log line, a panic message, or a
/// derived `Debug` of the `Model`. Only ASCII digits are accepted, which also keeps
/// the hand-built auth JSON injection-free.
#[derive(Default, Clone)]
pub struct Pin(Zeroizing<String>);

impl std::fmt::Debug for Pin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the digits — length only, so a Debug of the Model cannot leak it.
        write!(f, "Pin(<{} digits>)", self.0.len())
    }
}

impl Pin {
    /// Append a digit; non-digits are ignored (keeps the auth JSON injection-free).
    pub fn push(&mut self, c: char) {
        if c.is_ascii_digit() {
            self.0.push(c);
        }
    }

    /// Remove the last digit.
    pub fn pop(&mut self) {
        self.0.pop();
    }

    /// Number of digits entered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no digits have been entered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Clear the PIN, zeroizing the current buffer (the old `Zeroizing` is dropped).
    pub fn clear(&mut self) {
        self.0 = Zeroizing::new(String::new());
    }

    /// Build the `auth` request line into a `Zeroizing` buffer. Assembled by hand
    /// rather than via `serde_json::to_string`, which would allocate an
    /// **un-zeroized** `String` copy of the PIN. Digits-only (see [`Self::push`])
    /// so no JSON escaping is needed.
    #[must_use]
    pub fn auth_line(&self) -> Zeroizing<String> {
        let mut line = Zeroizing::new(String::with_capacity(21 + self.0.len()));
        line.push_str(r#"{"op":"auth","pin":""#);
        line.push_str(&self.0);
        line.push_str(r#""}"#);
        line
    }
}

/// Where the session is.
#[derive(Debug)]
pub enum Phase {
    /// Waiting for the `hello` handshake.
    Connecting,
    /// Handshake done; entering the PIN.
    Authing {
        /// The digits entered so far.
        pin: Pin,
        /// The last auth failure to show, if any.
        error: Option<AuthError>,
    },
    /// Authenticated; watching the queue.
    Watching {
        /// Current queue summaries.
        items: Vec<Summary>,
        /// Selected row (clamped into `items`).
        selected: usize,
        /// The opened card, if one is being viewed.
        card: Option<Box<Card>>,
        /// A transient note (e.g. the selected item vanished).
        note: Option<String>,
    },
    /// The connection is finished — render the reason and exit.
    Fatal(TransportError),
}

/// A human-facing auth failure shown on the PIN screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// Wrong PIN; attempts before lockout.
    BadPin(u32),
    /// Locked out; seconds to wait.
    Locked(u64),
    /// The wallet has no PIN set.
    NotSet,
    /// Transient verifier failure.
    Unavailable,
}

/// A message the update loop folds into the model.
#[derive(Debug)]
pub enum Msg {
    /// Periodic timer — drives `list` polling in the watch phase.
    Tick,
    /// A reply from the transport worker.
    Reply(Reply),
    /// A digit was typed on the PIN screen.
    PinDigit(char),
    /// Backspace on the PIN screen.
    PinBackspace,
    /// Submit the PIN.
    PinSubmit,
    /// Move the queue selection up.
    MoveUp,
    /// Move the queue selection down.
    MoveDown,
    /// Open the selected item's card.
    Open,
    /// Close the open card.
    Back,
    /// Quit.
    Quit,
}

/// The application model.
#[derive(Debug)]
pub struct Model {
    phase: Phase,
    in_flight: bool,
    pending: Option<PendingIntent>,
    quit: bool,
}

/// A parked user intent (no `List` — the poll is suppressed, not queued).
#[derive(Debug)]
enum PendingIntent {
    Get(String),
    Auth,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            phase: Phase::Connecting,
            in_flight: false,
            pending: None,
            quit: false,
        }
    }
}

impl Model {
    /// A fresh model in the `Connecting` phase.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read-only view of the current phase (for the renderer).
    #[must_use]
    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    /// Whether the app has been asked to quit.
    #[must_use]
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Fold one message into the model, returning at most one request to send.
    pub fn update(&mut self, msg: Msg) -> Option<transport::Request> {
        match msg {
            Msg::Quit => {
                self.quit = true;
                None
            }
            Msg::Tick => self.on_tick(),
            Msg::Reply(reply) => self.on_reply(reply),
            Msg::PinDigit(c) => {
                if let Phase::Authing { pin, .. } = &mut self.phase {
                    pin.push(c);
                }
                None
            }
            Msg::PinBackspace => {
                if let Phase::Authing { pin, .. } = &mut self.phase {
                    pin.pop();
                }
                None
            }
            Msg::PinSubmit => self.on_pin_submit(),
            Msg::MoveUp => {
                if let Phase::Watching { selected, .. } = &mut self.phase {
                    *selected = selected.saturating_sub(1);
                }
                None
            }
            Msg::MoveDown => {
                if let Phase::Watching {
                    selected, items, ..
                } = &mut self.phase
                {
                    *selected = (*selected + 1).min(items.len().saturating_sub(1));
                }
                None
            }
            Msg::Open => self.on_open(),
            Msg::Back => {
                if let Phase::Watching { card, .. } = &mut self.phase {
                    *card = None;
                }
                None
            }
        }
    }

    /// Periodic tick: poll `list` in the watch phase — but only when nothing is in
    /// flight (the poll is suppressed, never queued, so stale polls do not pile up).
    fn on_tick(&mut self) -> Option<transport::Request> {
        if matches!(self.phase, Phase::Watching { .. }) && !self.in_flight {
            self.in_flight = true;
            Some(transport::Request::List)
        } else {
            None
        }
    }

    fn on_pin_submit(&mut self) -> Option<transport::Request> {
        let Phase::Authing { pin, .. } = &self.phase else {
            return None;
        };
        if pin.is_empty() {
            return None;
        }
        let line = pin.auth_line();
        self.dispatch_user(PendingIntent::Auth, || transport::Request::Auth(line))
    }

    fn on_open(&mut self) -> Option<transport::Request> {
        let Phase::Watching {
            items,
            selected,
            card,
            ..
        } = &self.phase
        else {
            return None;
        };
        if card.is_some() {
            return None; // already viewing one
        }
        let Some(id) = items.get(*selected).map(|s| s.id.clone()) else {
            return None; // empty queue
        };
        self.dispatch_user(PendingIntent::Get(id.clone()), || {
            transport::Request::Get(id)
        })
    }

    /// Send a user intent now if idle, else park it (latest-wins) so it is not lost.
    fn dispatch_user(
        &mut self,
        park: PendingIntent,
        make: impl FnOnce() -> transport::Request,
    ) -> Option<transport::Request> {
        if self.in_flight {
            self.pending = Some(park);
            None
        } else {
            self.in_flight = true;
            Some(make())
        }
    }

    fn on_reply(&mut self, reply: Reply) -> Option<transport::Request> {
        self.in_flight = false;
        match reply {
            Reply::Hello { .. } => {
                self.phase = Phase::Authing {
                    pin: Pin::default(),
                    error: None,
                };
            }
            Reply::Auth(outcome) => self.apply_auth(outcome),
            Reply::List(items) => self.apply_list(items),
            Reply::Get(outcome) => self.apply_get(outcome),
            Reply::Fatal(err) => {
                self.phase = Phase::Fatal(err);
                self.pending = None;
                return None;
            }
        }
        self.flush_pending()
    }

    fn apply_auth(&mut self, outcome: AuthOutcome) {
        match outcome {
            AuthOutcome::Ok => {
                self.phase = Phase::Watching {
                    items: Vec::new(),
                    selected: 0,
                    card: None,
                    note: None,
                };
            }
            other => {
                if let Phase::Authing { pin, error } = &mut self.phase {
                    pin.clear();
                    *error = Some(match other {
                        AuthOutcome::BadPin { attempts_left } => AuthError::BadPin(attempts_left),
                        AuthOutcome::Locked { retry_after_s } => AuthError::Locked(retry_after_s),
                        AuthOutcome::PinNotSet => AuthError::NotSet,
                        AuthOutcome::PinUnavailable | AuthOutcome::Ok => AuthError::Unavailable,
                    });
                }
            }
        }
    }

    fn apply_list(&mut self, new_items: Vec<Summary>) {
        if let Phase::Watching {
            items, selected, ..
        } = &mut self.phase
        {
            // Preserve the selection across a refresh by id, not by index. An open
            // card is intentionally left as-is: if its item has left the live set,
            // the card stays visible and a re-open surfaces unknown_id (apply_get).
            let selected_id = items.get(*selected).map(|s| s.id.clone());
            *items = new_items;
            *selected = selected_id
                .and_then(|id| items.iter().position(|s| s.id == id))
                .unwrap_or(0)
                .min(items.len().saturating_sub(1));
        }
    }

    fn apply_get(&mut self, outcome: GetOutcome) {
        if let Phase::Watching { card, note, .. } = &mut self.phase {
            match outcome {
                GetOutcome::Card(c) => {
                    *card = Some(c);
                    *note = None;
                }
                GetOutcome::UnknownId => {
                    // The selected item vanished between list and get — drop any
                    // stale card and show a transient note instead of a dead card.
                    *card = None;
                    *note = Some("that request is no longer available".to_owned());
                }
            }
        }
    }

    /// After a reply lands, send a parked user intent if one is waiting.
    fn flush_pending(&mut self) -> Option<transport::Request> {
        match self.pending.take() {
            Some(PendingIntent::Get(id)) => {
                self.in_flight = true;
                Some(transport::Request::Get(id))
            }
            Some(PendingIntent::Auth) => {
                // Re-derive the auth line from the (now cleared-on-failure) pin only
                // if we are still on the auth screen with digits; otherwise drop it.
                if let Phase::Authing { pin, .. } = &self.phase
                    && !pin.is_empty()
                {
                    let line = pin.auth_line();
                    self.in_flight = true;
                    Some(transport::Request::Auth(line))
                } else {
                    None
                }
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{GetOutcome, Kind, Risk};

    fn summary(id: &str) -> Summary {
        Summary {
            id: id.to_owned(),
            kind: Kind::Send,
            chain_id: 1,
            to: "0xabc".to_owned(),
            amount_wei: "0".to_owned(),
            risk: Risk::Safe,
            high_risk: false,
            not_after_unix: 1,
        }
    }

    fn card(id: &str) -> Box<Card> {
        Box::new(Card {
            id: id.to_owned(),
            chain_id: 1,
            to: "0xabc".to_owned(),
            amount_wei: "0".to_owned(),
            decoded_call: None,
            high_risk: false,
            high_risk_reasons: vec![],
            raw_data: "0x".to_owned(),
            not_after_unix: 1,
        })
    }

    /// Drive a model to the watch phase with the given items.
    fn watching(items: Vec<Summary>) -> Model {
        let mut m = Model::new();
        assert!(
            m.update(Msg::Reply(Reply::Hello {
                server: "s".to_owned()
            }))
            .is_none()
        );
        m.update(Msg::PinSubmit); // empty — no-op, but exercises the guard
        // fake a successful auth
        let _ = m.update(Msg::PinDigit('1'));
        let req = m.update(Msg::PinSubmit);
        assert!(matches!(req, Some(transport::Request::Auth(_))));
        assert!(m.update(Msg::Reply(Reply::Auth(AuthOutcome::Ok))).is_none());
        // deliver the first list
        assert!(matches!(
            m.update(Msg::Tick),
            Some(transport::Request::List)
        ));
        m.update(Msg::Reply(Reply::List(items)));
        m
    }

    // ── PIN hygiene ──

    #[test]
    fn pin_debug_never_shows_the_digits() {
        let mut pin = Pin::default();
        for c in "483920".chars() {
            pin.push(c);
        }
        let dbg = format!("{pin:?}");
        assert!(
            !dbg.contains("483920"),
            "PIN digits must not appear in Debug"
        );
        assert!(dbg.contains("6 digits"));
    }

    #[test]
    fn model_debug_never_shows_the_pin() {
        let mut m = Model::new();
        m.update(Msg::Reply(Reply::Hello {
            server: "s".to_owned(),
        }));
        for c in "483920".chars() {
            m.update(Msg::PinDigit(c));
        }
        assert!(!format!("{m:?}").contains("483920"));
    }

    #[test]
    fn pin_ignores_non_digits_keeping_auth_json_safe() {
        let mut pin = Pin::default();
        for c in "1\"2}3".chars() {
            pin.push(c);
        }
        assert_eq!(pin.len(), 3);
        let line = pin.auth_line();
        assert_eq!(&*line, r#"{"op":"auth","pin":"123"}"#);
    }

    // ── one request in flight ──

    #[test]
    fn tick_polls_list_only_when_idle() {
        let mut m = watching(vec![summary("a")]);
        // idle → tick polls
        assert!(matches!(
            m.update(Msg::Tick),
            Some(transport::Request::List)
        ));
        // now in flight → a second tick is suppressed, not queued
        assert!(m.update(Msg::Tick).is_none());
        // reply clears the flight; the next tick polls again
        m.update(Msg::Reply(Reply::List(vec![summary("a")])));
        assert!(matches!(
            m.update(Msg::Tick),
            Some(transport::Request::List)
        ));
    }

    #[test]
    fn a_user_open_while_in_flight_is_parked_and_sent_after_the_reply() {
        let mut m = watching(vec![summary("a"), summary("b")]);
        // start a list poll → in flight
        assert!(matches!(
            m.update(Msg::Tick),
            Some(transport::Request::List)
        ));
        // user opens item 0 while the poll is outstanding → parked, nothing sent now
        assert!(m.update(Msg::Open).is_none());
        // the poll reply lands → the parked get is flushed
        let flushed = m.update(Msg::Reply(Reply::List(vec![summary("a"), summary("b")])));
        assert!(matches!(flushed, Some(transport::Request::Get(id)) if id == "a"));
    }

    #[test]
    fn parked_user_intent_is_latest_wins() {
        let mut m = watching(vec![summary("a"), summary("b")]);
        assert!(matches!(
            m.update(Msg::Tick),
            Some(transport::Request::List)
        ));
        m.update(Msg::Open); // parks get("a")
        m.update(Msg::MoveDown);
        m.update(Msg::Open); // replaces with get("b") — latest wins
        let flushed = m.update(Msg::Reply(Reply::List(vec![summary("a"), summary("b")])));
        assert!(matches!(flushed, Some(transport::Request::Get(id)) if id == "b"));
    }

    // ── watch behaviour ──

    #[test]
    fn selection_survives_a_refresh_by_id() {
        let mut m = watching(vec![summary("a"), summary("b"), summary("c")]);
        m.update(Msg::MoveDown);
        m.update(Msg::MoveDown); // selected = "c"
        // refresh reorders: "c" is now first
        m.update(Msg::Reply(Reply::List(vec![
            summary("c"),
            summary("a"),
            summary("b"),
        ])));
        let Phase::Watching {
            selected, items, ..
        } = m.phase()
        else {
            panic!("watching");
        };
        assert_eq!(
            items[*selected].id, "c",
            "selection follows the id, not the index"
        );
    }

    #[test]
    fn opening_shows_the_card_and_back_closes_it() {
        let mut m = watching(vec![summary("a")]);
        assert!(matches!(
            m.update(Msg::Open),
            Some(transport::Request::Get(_))
        ));
        m.update(Msg::Reply(Reply::Get(GetOutcome::Card(card("a")))));
        assert!(matches!(m.phase(), Phase::Watching { card: Some(_), .. }));
        m.update(Msg::Back);
        assert!(matches!(m.phase(), Phase::Watching { card: None, .. }));
    }

    #[test]
    fn a_vanished_selection_on_open_shows_a_note_not_a_dead_card() {
        let mut m = watching(vec![summary("a")]);
        assert!(matches!(
            m.update(Msg::Open),
            Some(transport::Request::Get(_))
        ));
        // the item vanished between list and get
        m.update(Msg::Reply(Reply::Get(GetOutcome::UnknownId)));
        let Phase::Watching { card, note, .. } = m.phase() else {
            panic!("watching");
        };
        assert!(card.is_none());
        assert!(note.is_some());
    }

    // ── auth failure ──

    #[test]
    fn bad_pin_clears_the_pin_and_records_the_error() {
        let mut m = Model::new();
        m.update(Msg::Reply(Reply::Hello {
            server: "s".to_owned(),
        }));
        m.update(Msg::PinDigit('9'));
        m.update(Msg::PinSubmit);
        m.update(Msg::Reply(Reply::Auth(AuthOutcome::BadPin {
            attempts_left: 2,
        })));
        let Phase::Authing { pin, error } = m.phase() else {
            panic!("still authing");
        };
        assert!(pin.is_empty(), "a bad PIN is cleared for re-entry");
        assert_eq!(error, &Some(AuthError::BadPin(2)));
    }

    #[test]
    fn a_fatal_reply_moves_to_the_fatal_phase() {
        let mut m = Model::new();
        assert!(
            m.update(Msg::Reply(Reply::Fatal(TransportError::ConnectionLost)))
                .is_none()
        );
        assert!(matches!(
            m.phase(),
            Phase::Fatal(TransportError::ConnectionLost)
        ));
    }
}
