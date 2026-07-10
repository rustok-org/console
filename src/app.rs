//! The Model and its update logic (MVU) — pure over messages, no rendering and no
//! I/O. Keys are mapped to [`Msg`] in the event loop and transport replies arrive
//! as [`Msg::Reply`]; `update` folds them into the [`Model`] and returns at most one
//! [`transport::Request`] to send. This keeps the decision logic unit-testable
//! without a terminal or a socket.
//!
//! **One request in flight** (protocol §1): while a request is outstanding the
//! periodic `list` poll is suppressed (we do not pile up stale polls), and a user
//! action taken meanwhile is parked in a single latest-wins slot so it is not lost.
//!
//! **Default-deny** (`AGENTS.md` #5): an open card *is* the confirmation. Only an
//! explicit `y` approves; `n`, Esc, Ctrl-C and the expiry deadline all send `deny`.
//! Leaving the confirmation without deciding is not offered.

use zeroize::Zeroizing;

use crate::protocol::{AuthOutcome, Card, GetOutcome, ResolveOutcome, Summary, TerminalState};
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
        const PREFIX: &str = r#"{"op":"auth","pin":""#;
        const SUFFIX: &str = r#""}"#;
        // Reserve exactly so the buffer never reallocates — a realloc would free
        // the old allocation (with the PIN in it) WITHOUT zeroizing. Named
        // constants keep the reservation correct if the format ever changes.
        let mut line = Zeroizing::new(String::with_capacity(
            PREFIX.len() + self.0.len() + SUFFIX.len(),
        ));
        line.push_str(PREFIX);
        line.push_str(&self.0);
        line.push_str(SUFFIX);
        line
    }

    /// Build the high-risk `approve` request line into a `Zeroizing` buffer.
    ///
    /// The `id` goes through serde (so it is quoted and escaped by the same code
    /// that would encode it on the normal path — a card id can never break out of
    /// its JSON string), while the PIN is appended by hand: routing the whole
    /// request through `serde_json::to_string` would leave an **un-zeroized**
    /// `String` copy of the PIN behind. Digits-only (see [`Self::push`]).
    ///
    /// Returns `None` if the id could not be serialized — a string always can, so
    /// this is a fail-closed seam rather than a panic in a money path.
    #[must_use]
    pub fn approve_line(&self, id: &str) -> Option<Zeroizing<String>> {
        const PREFIX: &str = r#"{"op":"approve","id":"#;
        const MID: &str = r#","pin":""#;
        const SUFFIX: &str = r#""}"#;
        // Quoted and escaped by serde; carries no secret, so a plain String is fine.
        let id_json = serde_json::to_string(id).ok()?;
        // Reserve exactly — a realloc would free the old buffer (with the PIN in it)
        // WITHOUT zeroizing it.
        let mut line = Zeroizing::new(String::with_capacity(
            PREFIX.len() + id_json.len() + MID.len() + self.0.len() + SUFFIX.len(),
        ));
        line.push_str(PREFIX);
        line.push_str(&id_json);
        line.push_str(MID);
        line.push_str(&self.0);
        line.push_str(SUFFIX);
        Some(line)
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
        /// The opened card and its confirmation state, if one is being decided.
        confirm: Option<Box<Confirm>>,
        /// A transient note (e.g. the selected item vanished).
        note: Option<String>,
    },
    /// The item reached a terminal state — render it, then exit with [`ExitOutcome`].
    Resolved {
        /// The server's terminal answer, rendered as received.
        outcome: ResolveOutcome,
        /// What the process exits with.
        exit: ExitOutcome,
    },
    /// The connection is finished — render the reason and exit.
    Fatal(TransportError),
}

/// An open card **is** the confirmation dialog (`AGENTS.md` #5): it is left by
/// approving or denying, never by simply closing.
#[derive(Debug)]
pub struct Confirm {
    card: Card,
    /// `Some` while the high-risk PIN prompt is up.
    pin: Option<Pin>,
    /// The last non-terminal failure to show.
    error: Option<ResolveError>,
    /// The decision on the wire, while one is — further key presses are ignored
    /// so a second press cannot become a second decision. *Which* decision it
    /// was matters too: a PIN-family answer is only ever legal for an approve
    /// (§3.6), and `apply_resolve` refuses the channel when it arrives for
    /// anything else — a "no" must never grow a PIN prompt.
    sent: Option<SentDecision>,
    /// The `deny` was sent by the expiry deadline, not by the human.
    timed_out: bool,
}

/// The decision the console put on the wire for the open card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SentDecision {
    Approve,
    Deny,
}

impl Confirm {
    fn new(card: Card) -> Self {
        Self {
            card,
            pin: None,
            error: None,
            sent: None,
            timed_out: false,
        }
    }

    /// The card being decided (rendered verbatim).
    #[must_use]
    pub fn card(&self) -> &Card {
        &self.card
    }

    /// Number of PIN digits entered, or `None` when the PIN prompt is not up.
    #[must_use]
    pub fn pin_len(&self) -> Option<usize> {
        self.pin.as_ref().map(Pin::len)
    }

    /// The last non-terminal failure, if any.
    #[must_use]
    pub fn error(&self) -> Option<&ResolveError> {
        self.error.as_ref()
    }

    /// Whether a decision is currently on the wire.
    #[must_use]
    pub fn is_resolving(&self) -> bool {
        self.sent.is_some()
    }
}

/// What the console exits with once an item is terminal. It reports **what happened
/// to the money**, not which key the human pressed: an item another connection
/// executed while we were denying it is still [`Self::Approved`]. A `deny` sent by
/// the expiry deadline reports [`Self::Expired`], not [`Self::Rejected`] — the
/// deadline, not a human, said no.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitOutcome {
    /// Signed and broadcast.
    Approved,
    /// A human said no.
    Rejected,
    /// The deadline passed before a decision.
    Expired,
    /// Approved, but signing/broadcast failed — no money moved.
    Failed,
}

/// A **non-terminal** failure of `approve`/`deny`: the item is still live and the
/// human can act again. Terminal answers become [`Phase::Resolved`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// The item is high-risk; a per-request PIN is needed (the entry is untouched).
    PinRequired,
    /// Wrong PIN; attempts before lockout.
    BadPin(u32),
    /// Locked out; seconds to wait.
    Locked(u64),
    /// The wallet has no PIN set.
    NotSet,
    /// Transient verifier failure.
    Unavailable,
    /// Another connection is executing this id right now (`already_resolved:pending`).
    Busy,
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
    /// Open the selected item's card — which opens the confirmation.
    Open,
    /// Approve the open card (`y`). A high-risk card asks for the PIN first.
    Approve,
    /// Reject the open card — `n`, Esc or Ctrl-C (`AGENTS.md` #5).
    Reject,
    /// The open card's deadline passed: deny it fail-closed, and report `expired`.
    Expire,
    /// Quit.
    Quit,
}

/// The application model.
#[derive(Debug)]
pub struct Model {
    phase: Phase,
    in_flight: bool,
    pending: Option<PendingIntent>,
    /// A `get` has been **sent** and its answer is still pending. While set,
    /// [`Self::on_open`] refuses new card requests: two card requests racing is
    /// how a confirmation could be rebound to a card the human never opened.
    awaiting_card: bool,
    quit: bool,
}

/// A parked user intent (no `List` — the poll is suppressed, not queued).
///
/// **Not `derive(Debug)`**: `ApprovePin` holds the serialized PIN, and `Zeroizing`
/// forwards `Debug` to the inner `String`. A derived `Debug` would print the PIN
/// through the `Model`'s own `Debug`.
enum PendingIntent {
    Get(String),
    Auth,
    Approve(String),
    ApprovePin(Zeroizing<String>),
    Deny(String),
}

impl std::fmt::Debug for PendingIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Get(id) => write!(f, "Get({id})"),
            Self::Auth => f.write_str("Auth"),
            Self::Approve(id) => write!(f, "Approve({id})"),
            // The line carries the PIN — never its contents.
            Self::ApprovePin(_) => f.write_str("ApprovePin(<redacted>)"),
            Self::Deny(id) => write!(f, "Deny({id})"),
        }
    }
}

impl Default for Model {
    fn default() -> Self {
        Self {
            phase: Phase::Connecting,
            in_flight: false,
            pending: None,
            awaiting_card: false,
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
                if let Some(pin) = self.pin_mut() {
                    pin.push(c);
                }
                None
            }
            Msg::PinBackspace => {
                if let Some(pin) = self.pin_mut() {
                    pin.pop();
                }
                None
            }
            Msg::PinSubmit => self.on_pin_submit(),
            Msg::Approve => self.on_approve(),
            Msg::Reject => self.on_reject(false),
            Msg::Expire => self.on_reject(true),
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
        }
    }

    /// The PIN buffer the keyboard is currently feeding: the unlock screen, or the
    /// high-risk prompt on an open confirmation. `None` while a decision is on the
    /// wire — a late keystroke must not edit a PIN that has already been sent.
    fn pin_mut(&mut self) -> Option<&mut Pin> {
        match &mut self.phase {
            Phase::Authing { pin, .. } => Some(pin),
            Phase::Watching {
                confirm: Some(c), ..
            } if c.sent.is_none() => c.pin.as_mut(),
            _ => None,
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
        if matches!(
            self.phase,
            Phase::Watching {
                confirm: Some(_),
                ..
            }
        ) {
            return self.on_confirm_pin_submit();
        }
        let Phase::Authing { pin, .. } = &self.phase else {
            return None;
        };
        if pin.is_empty() {
            return None;
        }
        let line = pin.auth_line();
        self.dispatch_user(PendingIntent::Auth, || transport::Request::Auth(line))
    }

    /// Submit the per-request PIN of a high-risk approval.
    fn on_confirm_pin_submit(&mut self) -> Option<transport::Request> {
        let Phase::Watching {
            confirm: Some(c), ..
        } = &mut self.phase
        else {
            return None;
        };
        if c.sent.is_some() {
            return None;
        }
        let Some(pin) = &c.pin else {
            return None; // the prompt is not up: `y` opens it
        };
        if pin.is_empty() {
            return None;
        }
        let Some(line) = pin.approve_line(&c.card.id) else {
            // A card id that will not serialize cannot be approved safely.
            self.phase = Phase::Fatal(TransportError::Protocol(
                "card id could not be encoded".to_owned(),
            ));
            return None;
        };
        c.sent = Some(SentDecision::Approve);
        c.error = None;
        self.dispatch_pin_approve(line)
    }

    /// `y` on the confirmation: a high-risk card asks for the PIN first, a normal
    /// one goes straight to `approve`.
    fn on_approve(&mut self) -> Option<transport::Request> {
        let Phase::Watching {
            confirm: Some(c), ..
        } = &mut self.phase
        else {
            return None;
        };
        if c.sent.is_some() || c.pin.is_some() {
            return None; // decided already, or the PIN prompt owns Enter
        }
        if c.card.high_risk {
            c.pin = Some(Pin::default());
            c.error = None;
            return None;
        }
        c.sent = Some(SentDecision::Approve);
        c.error = None;
        let id = c.card.id.clone();
        self.dispatch_user(PendingIntent::Approve(id.clone()), || {
            transport::Request::Approve(id)
        })
    }

    /// Default-deny (`AGENTS.md` #5). `by_timeout` records that the deadline said
    /// no, not the human — it changes only what we exit with, never the `deny`.
    fn on_reject(&mut self, by_timeout: bool) -> Option<transport::Request> {
        let Phase::Watching {
            confirm: Some(c), ..
        } = &mut self.phase
        else {
            return None;
        };
        if c.sent.is_some() {
            return None; // a decision is already on the wire; the server decides
        }
        c.sent = Some(SentDecision::Deny);
        c.timed_out = by_timeout;
        c.error = None;
        let id = c.card.id.clone();
        self.dispatch_user(PendingIntent::Deny(id.clone()), || {
            transport::Request::Deny(id)
        })
    }

    fn on_open(&mut self) -> Option<transport::Request> {
        let Phase::Watching {
            items,
            selected,
            confirm,
            ..
        } = &self.phase
        else {
            return None;
        };
        if confirm.is_some() {
            return None; // already deciding one
        }
        if self.awaiting_card {
            // A card request is already on the wire. Refused, NOT parked: with
            // two gets racing, the late answer would land after the first card
            // opened — and a confirmation must never be rebound (`apply_get`).
            return None;
        }
        let Some(id) = items.get(*selected).map(|s| s.id.clone()) else {
            return None; // empty queue
        };
        let request = self.dispatch_user(PendingIntent::Get(id.clone()), || {
            transport::Request::Get(id)
        });
        // Parked gets are covered too: `flush_pending` raises the flag when the
        // park is sent, and latest-wins can only swap one park for another.
        self.awaiting_card = request.is_some();
        request
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

    /// Same, for the high-risk approve line. It takes the `Zeroizing` buffer by
    /// value so the PIN is never cloned into a second allocation.
    fn dispatch_pin_approve(&mut self, line: Zeroizing<String>) -> Option<transport::Request> {
        if self.in_flight {
            self.pending = Some(PendingIntent::ApprovePin(line));
            None
        } else {
            self.in_flight = true;
            Some(transport::Request::ApprovePin(line))
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
                // A handshake resets the protocol: an outstanding get will never
                // be answered, and a stuck flag would refuse every future open.
                self.awaiting_card = false;
            }
            Reply::Auth(outcome) => self.apply_auth(outcome),
            Reply::List(items) => self.apply_list(items),
            Reply::Get(outcome) => self.apply_get(outcome),
            Reply::Resolve(outcome) => self.apply_resolve(outcome),
            Reply::Fatal(err) => {
                self.phase = Phase::Fatal(err);
                self.pending = None;
                return None;
            }
        }
        // A terminal answer ends the session: nothing parked may still be sent.
        if matches!(self.phase, Phase::Resolved { .. } | Phase::Fatal(_)) {
            self.pending = None;
            return None;
        }
        self.flush_pending()
    }

    fn apply_auth(&mut self, outcome: AuthOutcome) {
        match outcome {
            AuthOutcome::Ok => {
                self.phase = Phase::Watching {
                    items: Vec::new(),
                    selected: 0,
                    confirm: None,
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
        // Whatever this outcome says, it answers the one get that was allowed
        // on the wire — the console may ask for a card again.
        self.awaiting_card = false;
        if let Phase::Watching { confirm, note, .. } = &mut self.phase {
            if confirm.is_some() {
                // An open card *is* the confirmation (`AGENTS.md` #5): no reply
                // may replace it — least of all mid-resolve, where a swap would
                // re-aim the human's decision at a card they never opened. No
                // get is ever solicited while a card is open (`on_open`), so
                // this outcome is stale or unsolicited: dropped, fail closed.
                return;
            }
            match outcome {
                GetOutcome::Card(c) => {
                    // An open card *is* the confirmation (AGENTS.md #5).
                    *confirm = Some(Box::new(Confirm::new(*c)));
                    *note = None;
                }
                GetOutcome::UnknownId => {
                    // The selected item vanished between list and get — show a
                    // transient note instead of a dead card.
                    *note = Some("that request is no longer available".to_owned());
                }
            }
        }
    }

    /// Fold an `approve`/`deny` answer. Terminal answers end the session with the
    /// matching [`ExitOutcome`]; the rest leave the item live and the human in
    /// charge (`pin_required` opens the PIN prompt, `bad_pin` clears it, and so on).
    fn apply_resolve(&mut self, outcome: ResolveOutcome) {
        let (sent, timed_out) = match &mut self.phase {
            Phase::Watching {
                confirm: Some(c), ..
            } => (c.sent.take(), c.timed_out),
            // No confirmation is open — an answer to a decision we never made.
            _ => return,
        };

        if let Some(exit) = terminal_exit(&outcome, timed_out) {
            self.phase = Phase::Resolved { outcome, exit };
            return;
        }

        if matches!(outcome, ResolveOutcome::Unauthorized) {
            // We only send approve/deny after a successful auth on this very
            // connection; the server disagreeing means the channel is not what we
            // think it is. Fail closed rather than retry a money action.
            self.phase = Phase::Fatal(TransportError::Protocol(
                "server refused an authenticated session (unauthorized)".to_owned(),
            ));
            return;
        }

        let pin_flavoured = matches!(
            outcome,
            ResolveOutcome::PinRequired
                | ResolveOutcome::BadPin { .. }
                | ResolveOutcome::Locked { .. }
                | ResolveOutcome::PinNotSet
                | ResolveOutcome::PinUnavailable
        );
        if pin_flavoured && sent != Some(SentDecision::Approve) {
            // A PIN-family answer exists only for `approve` (§3.6: "deny never
            // requires a PIN beyond session auth"). Arriving for a deny — or for
            // nothing we sent — it would open the PIN prompt over a rejection,
            // and the prompt can only build an approve line: the human's "no"
            // would come back as a signature. Same fail-closed as Unauthorized;
            // `parse_deny` already refuses this at the wire, this is depth.
            self.phase = Phase::Fatal(TransportError::Protocol(
                "server asked for a PIN outside an approve (§3.6)".to_owned(),
            ));
            return;
        }

        let Phase::Watching { confirm, note, .. } = &mut self.phase else {
            return;
        };
        if matches!(outcome, ResolveOutcome::UnknownId) {
            *confirm = None;
            *note = Some("that request is no longer available".to_owned());
            return;
        }
        let Some(c) = confirm else {
            return;
        };
        let error = match outcome {
            // The entry is untouched and still approvable — with the PIN this time.
            ResolveOutcome::PinRequired => {
                c.pin.get_or_insert_with(Pin::default);
                ResolveError::PinRequired
            }
            ResolveOutcome::BadPin { attempts_left } => {
                c.pin.get_or_insert_with(Pin::default).clear();
                ResolveError::BadPin(attempts_left)
            }
            ResolveOutcome::Locked { retry_after_s } => {
                if let Some(pin) = &mut c.pin {
                    pin.clear();
                }
                ResolveError::Locked(retry_after_s)
            }
            ResolveOutcome::PinNotSet => ResolveError::NotSet,
            ResolveOutcome::PinUnavailable => ResolveError::Unavailable,
            // Only `already_resolved:pending` is non-terminal; the rest were taken
            // by `terminal_exit` above.
            ResolveOutcome::AlreadyResolved { .. } => ResolveError::Busy,
            ResolveOutcome::Executed { .. }
            | ResolveOutcome::Failed { .. }
            | ResolveOutcome::Denied
            | ResolveOutcome::Unauthorized
            | ResolveOutcome::UnknownId => return,
        };
        c.error = Some(error);
    }

    /// After a reply lands, send a parked user intent if one is waiting.
    fn flush_pending(&mut self) -> Option<transport::Request> {
        match self.pending.take() {
            Some(PendingIntent::Get(id)) => {
                self.in_flight = true;
                self.awaiting_card = true;
                Some(transport::Request::Get(id))
            }
            Some(PendingIntent::Approve(id)) => {
                self.in_flight = true;
                Some(transport::Request::Approve(id))
            }
            Some(PendingIntent::ApprovePin(line)) => {
                self.in_flight = true;
                Some(transport::Request::ApprovePin(line))
            }
            Some(PendingIntent::Deny(id)) => {
                self.in_flight = true;
                Some(transport::Request::Deny(id))
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

/// Is this answer the item's last word, and if so what do we exit with?
///
/// The exit reports **what happened to the money**, not which key was pressed: an
/// `already_resolved:executed` answer to our `deny` means another connection got
/// there first and the transaction went out — that is [`ExitOutcome::Approved`].
/// `timed_out` is the one place the cause matters: a `deny` the deadline sent
/// reports `expired`, so a caller can tell "the human said no" from "nobody did".
///
/// `already_resolved:pending` is **not** terminal — another connection is executing
/// this id right now (protocol §3.5); the human may retry.
fn terminal_exit(outcome: &ResolveOutcome, timed_out: bool) -> Option<ExitOutcome> {
    // Only *our own* deny can have been sent by the deadline. An `already_resolved`
    // deny was somebody else's decision, so it stays a rejection.
    let our_deny = if timed_out {
        ExitOutcome::Expired
    } else {
        ExitOutcome::Rejected
    };
    match outcome {
        ResolveOutcome::Executed { .. } => Some(ExitOutcome::Approved),
        ResolveOutcome::Failed { .. } => Some(ExitOutcome::Failed),
        ResolveOutcome::Denied => Some(our_deny),
        ResolveOutcome::AlreadyResolved { state } => match state {
            TerminalState::Executed => Some(ExitOutcome::Approved),
            TerminalState::Failed => Some(ExitOutcome::Failed),
            TerminalState::Denied => Some(ExitOutcome::Rejected),
            TerminalState::Expired => Some(ExitOutcome::Expired),
            TerminalState::Pending => None,
        },
        ResolveOutcome::Unauthorized
        | ResolveOutcome::PinRequired
        | ResolveOutcome::BadPin { .. }
        | ResolveOutcome::Locked { .. }
        | ResolveOutcome::PinNotSet
        | ResolveOutcome::PinUnavailable
        | ResolveOutcome::UnknownId => None,
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
        card_risk(id, false)
    }

    fn card_risk(id: &str, high_risk: bool) -> Box<Card> {
        Box::new(Card {
            id: id.to_owned(),
            chain_id: 1,
            to: "0xabc".to_owned(),
            amount_wei: "0".to_owned(),
            decoded_call: None,
            high_risk,
            high_risk_reasons: vec![],
            raw_data: "0x".to_owned(),
            not_after_unix: 1,
        })
    }

    /// Drive a model to an open confirmation on `id`.
    fn confirming(id: &str, high_risk: bool) -> Model {
        let mut m = watching(vec![summary(id)]);
        assert!(matches!(
            m.update(Msg::Open),
            Some(transport::Request::Get(_))
        ));
        m.update(Msg::Reply(Reply::Get(GetOutcome::Card(card_risk(
            id, high_risk,
        )))));
        m
    }

    fn confirm_of(m: &Model) -> &Confirm {
        let Phase::Watching {
            confirm: Some(c), ..
        } = m.phase()
        else {
            panic!("a confirmation must be open");
        };
        c
    }

    fn type_pin(m: &mut Model, digits: &str) {
        for c in digits.chars() {
            m.update(Msg::PinDigit(c));
        }
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

    #[test]
    fn pin_auth_line_reserves_exactly_so_the_buffer_never_reallocates() {
        // A realloc frees the old buffer (with the PIN) WITHOUT zeroizing it. The
        // reserved capacity must fit the assembled line exactly, for any PIN length.
        for n in 1..=12 {
            let mut pin = Pin::default();
            for _ in 0..n {
                pin.push('9');
            }
            let line = pin.auth_line();
            assert_eq!(
                line.capacity(),
                line.len(),
                "auth_line must reserve exactly (no realloc) for pin_len {n}"
            );
        }
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

    // ── one decision, one card: a confirmation can never be rebound ──

    #[test]
    fn a_decision_can_never_be_rebound_to_a_card_the_human_did_not_open() {
        // The incident, key press by key press — no timing involved.
        let mut m = watching(vec![summary("a"), summary("b")]);
        // Open(a): the get flies at once.
        assert!(matches!(
            m.update(Msg::Open),
            Some(transport::Request::Get(id)) if id == "a"
        ));
        // Open(b) while that get is on the wire: refused outright, NOT parked —
        // two card requests racing is how a decision gets rebound.
        m.update(Msg::MoveDown);
        assert!(m.update(Msg::Open).is_none());
        // Card a lands and binds the confirmation; nothing parked may chase it.
        let flushed = m.update(Msg::Reply(Reply::Get(GetOutcome::Card(card("a")))));
        assert!(
            flushed.is_none(),
            "a second get must never chase an open card"
        );
        // `y` — the approve goes out for a, the card on the screen.
        assert!(matches!(
            m.update(Msg::Approve),
            Some(transport::Request::Approve(id)) if id == "a"
        ));
        // A stale card b arrives while the decision is on the wire.
        assert!(
            m.update(Msg::Reply(Reply::Get(GetOutcome::Card(card("b")))))
                .is_none()
        );
        let c = confirm_of(&m);
        assert_eq!(
            c.card().id,
            "a",
            "the confirmation stays bound to the card the human saw"
        );
        assert!(
            c.is_resolving(),
            "the in-flight decision survives a stale card reply"
        );
        // A second `y` cannot become a second decision.
        assert!(
            m.update(Msg::Approve).is_none(),
            "one session, one decision"
        );
    }

    #[test]
    fn an_unsolicited_card_reply_never_replaces_an_open_confirmation() {
        let mut m = confirming("a", false);
        assert!(
            m.update(Msg::Reply(Reply::Get(GetOutcome::Card(card("b")))))
                .is_none()
        );
        assert_eq!(
            confirm_of(&m).card().id,
            "a",
            "an open card IS the confirmation — no reply may swap it out"
        );
    }

    #[test]
    fn an_unsolicited_unknown_id_never_clears_an_open_confirmation() {
        let mut m = confirming("a", false);
        m.update(Msg::Approve); // the decision goes on the wire
        assert!(
            m.update(Msg::Reply(Reply::Get(GetOutcome::UnknownId)))
                .is_none()
        );
        let c = confirm_of(&m);
        assert_eq!(c.card().id, "a");
        assert!(
            c.is_resolving(),
            "the resolve answer must still find the confirmation it belongs to"
        );
    }

    #[test]
    fn a_pin_answer_to_a_deny_can_never_turn_the_no_into_a_signature() {
        // The incident: the human said no, a skewed/hostile core answered the
        // deny with a PIN-family error — the console must refuse the channel,
        // never open the PIN prompt over a rejection. Every family member.
        for outcome in [
            ResolveOutcome::PinRequired,
            ResolveOutcome::BadPin { attempts_left: 2 },
            ResolveOutcome::Locked { retry_after_s: 30 },
            ResolveOutcome::PinNotSet,
            ResolveOutcome::PinUnavailable,
        ] {
            let mut m = confirming("a", false);
            assert!(matches!(
                m.update(Msg::Reject),
                Some(transport::Request::Deny(id)) if id == "a"
            ));
            assert!(m.update(Msg::Reply(Reply::Resolve(outcome))).is_none());
            assert!(
                matches!(m.phase(), Phase::Fatal(_)),
                "a PIN ask on a deny is not the channel we think it is (§3.6)"
            );
            // The rest of the incident's key presses must be dead ends.
            type_pin(&mut m, "42");
            assert!(
                m.update(Msg::PinSubmit).is_none(),
                "no approve line may ever follow a rejection"
            );
        }
    }

    #[test]
    fn a_get_sent_from_the_park_also_locks_out_further_opens() {
        let mut m = watching(vec![summary("a"), summary("b")]);
        assert!(matches!(
            m.update(Msg::Tick),
            Some(transport::Request::List)
        ));
        m.update(Msg::Open); // parked behind the poll
        let flushed = m.update(Msg::Reply(Reply::List(vec![summary("a"), summary("b")])));
        assert!(matches!(flushed, Some(transport::Request::Get(id)) if id == "a"));
        // The parked get is on the wire now — a second open is refused…
        m.update(Msg::MoveDown);
        assert!(m.update(Msg::Open).is_none());
        // …and nothing chases the card when it lands.
        assert!(
            m.update(Msg::Reply(Reply::Get(GetOutcome::Card(card("a")))))
                .is_none()
        );
        assert_eq!(confirm_of(&m).card().id, "a");
    }

    #[test]
    fn a_vanished_card_frees_the_console_to_open_another() {
        let mut m = watching(vec![summary("a"), summary("b")]);
        assert!(matches!(
            m.update(Msg::Open),
            Some(transport::Request::Get(id)) if id == "a"
        ));
        m.update(Msg::Reply(Reply::Get(GetOutcome::UnknownId)));
        m.update(Msg::MoveDown);
        assert!(
            matches!(
                m.update(Msg::Open),
                Some(transport::Request::Get(id)) if id == "b"
            ),
            "once the get is answered the console can open the next card"
        );
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
    fn opening_a_card_opens_the_confirmation() {
        let mut m = watching(vec![summary("a")]);
        assert!(matches!(
            m.update(Msg::Open),
            Some(transport::Request::Get(_))
        ));
        m.update(Msg::Reply(Reply::Get(GetOutcome::Card(card("a")))));
        let c = confirm_of(&m);
        assert_eq!(c.card().id, "a");
        assert!(c.pin_len().is_none(), "a normal card asks for no PIN");
        assert!(!c.is_resolving());
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
        let Phase::Watching { confirm, note, .. } = m.phase() else {
            panic!("watching");
        };
        assert!(confirm.is_none());
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

    #[test]
    fn pin_pop_on_empty_is_safe() {
        let mut pin = Pin::default();
        pin.pop(); // must not panic on an empty PIN
        assert!(pin.is_empty());
    }

    // ── confirmation: approve ──

    #[test]
    fn approving_a_normal_card_sends_approve_without_a_pin() {
        let mut m = confirming("a", false);
        let req = m.update(Msg::Approve);
        assert!(matches!(req, Some(transport::Request::Approve(id)) if id == "a"));
        assert!(confirm_of(&m).is_resolving());
    }

    #[test]
    fn approving_a_high_risk_card_asks_for_the_pin_before_sending_anything() {
        let mut m = confirming("a", true);
        assert!(
            m.update(Msg::Approve).is_none(),
            "a high-risk approve must not reach the wire without a PIN"
        );
        let c = confirm_of(&m);
        assert_eq!(c.pin_len(), Some(0), "the PIN prompt is up");
        assert!(!c.is_resolving());
    }

    #[test]
    fn high_risk_pin_submit_sends_the_approve_line_with_the_pin() {
        let mut m = confirming("a", true);
        m.update(Msg::Approve);
        type_pin(&mut m, "4839");
        let req = m.update(Msg::PinSubmit);
        let Some(transport::Request::ApprovePin(line)) = req else {
            panic!("expected a pin-carrying approve");
        };
        assert_eq!(&*line, r#"{"op":"approve","id":"a","pin":"4839"}"#);
        assert!(confirm_of(&m).is_resolving());
    }

    #[test]
    fn an_empty_pin_is_never_submitted() {
        let mut m = confirming("a", true);
        m.update(Msg::Approve);
        assert!(m.update(Msg::PinSubmit).is_none());
        assert!(!confirm_of(&m).is_resolving());
    }

    #[test]
    fn approve_line_reserves_exactly_so_the_buffer_never_reallocates() {
        // A realloc frees the old buffer (with the PIN) WITHOUT zeroizing it.
        for n in 1..=12 {
            let mut pin = Pin::default();
            for _ in 0..n {
                pin.push('9');
            }
            let line = pin
                .approve_line("2f1c9f3e-0000-4000-8000-0123456789ab")
                .unwrap();
            assert_eq!(
                line.capacity(),
                line.len(),
                "approve_line must reserve exactly (no realloc) for pin_len {n}"
            );
        }
    }

    #[test]
    fn approve_line_escapes_the_id_so_it_cannot_inject_json() {
        let mut pin = Pin::default();
        pin.push('1');
        let line = pin.approve_line(r#"a","pin":"0000"#).unwrap();
        // The hostile id stays one JSON string value: the real PIN is still the
        // last `pin` member, and the injected quotes are escaped.
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["id"], r#"a","pin":"0000"#);
        assert_eq!(parsed["pin"], "1");
    }

    #[test]
    fn a_parked_approve_pin_is_redacted_in_the_model_debug() {
        // Park a money intent behind an in-flight poll, then Debug the whole model:
        // `Zeroizing<String>` forwards Debug to the String, so a derived Debug on
        // the intent would print the PIN.
        let mut m = confirming("a", true);
        m.update(Msg::Approve);
        type_pin(&mut m, "483920");
        assert!(matches!(
            m.update(Msg::Tick),
            Some(transport::Request::List)
        ));
        assert!(m.update(Msg::PinSubmit).is_none(), "parked behind the poll");
        let dbg = format!("{m:?}");
        assert!(!dbg.contains("483920"), "the PIN must not appear in Debug");
        assert!(dbg.contains("redacted"));
    }

    #[test]
    fn a_parked_high_risk_approve_is_sent_after_the_reply_pin_and_all() {
        let mut m = confirming("a", true);
        m.update(Msg::Approve); // high risk: opens the PIN prompt
        type_pin(&mut m, "483920");
        assert!(matches!(
            m.update(Msg::Tick),
            Some(transport::Request::List)
        ));
        assert!(m.update(Msg::PinSubmit).is_none(), "parked behind the poll");

        let flushed = m.update(Msg::Reply(Reply::List(vec![summary("a")])));

        // Dropping this arm would silently lose an approval the human confirmed.
        let Some(transport::Request::ApprovePin(line)) = flushed else {
            panic!("the parked high-risk approve must be sent once the wire is free");
        };
        assert_eq!(&*line, r#"{"op":"approve","id":"a","pin":"483920"}"#);
    }

    #[test]
    fn a_money_intent_parked_behind_a_poll_is_sent_after_the_reply() {
        let mut m = confirming("a", false);
        assert!(matches!(
            m.update(Msg::Tick),
            Some(transport::Request::List)
        ));
        assert!(m.update(Msg::Approve).is_none(), "parked, not lost");
        let flushed = m.update(Msg::Reply(Reply::List(vec![summary("a")])));
        assert!(matches!(flushed, Some(transport::Request::Approve(id)) if id == "a"));
    }

    // ── confirmation: default-deny (AGENTS.md #5) ──

    #[test]
    fn rejecting_sends_deny() {
        let mut m = confirming("a", false);
        let req = m.update(Msg::Reject);
        assert!(matches!(req, Some(transport::Request::Deny(id)) if id == "a"));
    }

    #[test]
    fn rejecting_a_high_risk_card_needs_no_pin() {
        // Saying no must always be cheap (protocol §3.6).
        let mut m = confirming("a", true);
        m.update(Msg::Approve); // the PIN prompt is up
        let req = m.update(Msg::Reject);
        assert!(matches!(req, Some(transport::Request::Deny(id)) if id == "a"));
    }

    #[test]
    fn a_second_decision_while_one_is_on_the_wire_is_ignored_and_not_parked() {
        let mut m = confirming("a", true);
        m.update(Msg::Approve);
        type_pin(&mut m, "1234");
        assert!(
            m.update(Msg::PinSubmit).is_some(),
            "the approve is on the wire"
        );

        // Further keys send nothing…
        assert!(m.update(Msg::Reject).is_none());
        assert!(m.update(Msg::Approve).is_none());
        assert!(m.update(Msg::Expire).is_none());

        // …and are not *parked* either. A non-terminal answer flushes the parking
        // slot, which would resurrect a deny for an item the human just approved.
        let after = m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::BadPin {
            attempts_left: 2,
        })));
        assert!(
            after.is_none(),
            "an ignored keystroke must never be parked behind the decision"
        );
        assert!(matches!(
            m.phase(),
            Phase::Watching {
                confirm: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn an_approve_pressed_after_a_reject_is_never_parked_and_can_never_sign() {
        // The scary direction: the human said no, a stray `y` must not survive in
        // the parking slot and sign once the server answers something non-terminal.
        let mut m = confirming("a", false);
        assert!(m.update(Msg::Reject).is_some());
        assert!(m.update(Msg::Approve).is_none());
        let after = m.update(Msg::Reply(Reply::Resolve(
            ResolveOutcome::AlreadyResolved {
                state: TerminalState::Pending,
            },
        )));
        assert!(
            after.is_none(),
            "a parked approve would sign what the human refused"
        );
        assert_eq!(confirm_of(&m).error(), Some(&ResolveError::Busy));
    }

    #[test]
    fn a_keystroke_cannot_edit_a_pin_that_is_already_on_the_wire() {
        let mut m = confirming("a", true);
        m.update(Msg::Approve);
        type_pin(&mut m, "1234");
        m.update(Msg::PinSubmit); // sent
        type_pin(&mut m, "9");
        assert_eq!(confirm_of(&m).pin_len(), Some(4), "the sent PIN is frozen");
    }

    // ── confirmation: terminal answers ──

    #[test]
    fn an_executed_answer_exits_approved_and_keeps_the_tx_hash() {
        let mut m = confirming("a", false);
        m.update(Msg::Approve);
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::Executed {
            tx_hash: "0xfeed".to_owned(),
        })));
        let Phase::Resolved { outcome, exit } = m.phase() else {
            panic!("resolved");
        };
        assert_eq!(*exit, ExitOutcome::Approved);
        assert!(matches!(outcome, ResolveOutcome::Executed { tx_hash } if tx_hash == "0xfeed"));
    }

    #[test]
    fn a_failed_broadcast_exits_failed_not_approved() {
        let mut m = confirming("a", false);
        m.update(Msg::Approve);
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::Failed {
            reason: "nonce too low".to_owned(),
        })));
        let Phase::Resolved { exit, .. } = m.phase() else {
            panic!("resolved");
        };
        assert_eq!(*exit, ExitOutcome::Failed, "no money moved");
    }

    #[test]
    fn a_human_deny_exits_rejected() {
        let mut m = confirming("a", false);
        m.update(Msg::Reject);
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::Denied)));
        let Phase::Resolved { exit, .. } = m.phase() else {
            panic!("resolved");
        };
        assert_eq!(*exit, ExitOutcome::Rejected);
    }

    #[test]
    fn the_deadline_denies_fail_closed_and_exits_expired_not_rejected() {
        let mut m = confirming("a", false);
        let req = m.update(Msg::Expire);
        assert!(
            matches!(req, Some(transport::Request::Deny(id)) if id == "a"),
            "an expiring item is denied, never left pending"
        );
        // The server has not observed the expiry yet and simply denies it.
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::Denied)));
        let Phase::Resolved { exit, .. } = m.phase() else {
            panic!("resolved");
        };
        assert_eq!(
            *exit,
            ExitOutcome::Expired,
            "the deadline said no, not the human"
        );
    }

    #[test]
    fn an_item_executed_by_another_connection_exits_approved_even_if_we_denied() {
        // The exit reports what happened to the money, not which key was pressed.
        let mut m = confirming("a", false);
        m.update(Msg::Reject);
        m.update(Msg::Reply(Reply::Resolve(
            ResolveOutcome::AlreadyResolved {
                state: TerminalState::Executed,
            },
        )));
        let Phase::Resolved { exit, .. } = m.phase() else {
            panic!("resolved");
        };
        assert_eq!(*exit, ExitOutcome::Approved);
    }

    #[test]
    fn an_already_expired_item_exits_expired() {
        let mut m = confirming("a", false);
        m.update(Msg::Approve);
        m.update(Msg::Reply(Reply::Resolve(
            ResolveOutcome::AlreadyResolved {
                state: TerminalState::Expired,
            },
        )));
        assert!(matches!(
            m.phase(),
            Phase::Resolved {
                exit: ExitOutcome::Expired,
                ..
            }
        ));
    }

    #[test]
    fn terminal_exit_maps_every_answer() {
        use ResolveOutcome as R;
        assert_eq!(
            terminal_exit(
                &R::Executed {
                    tx_hash: String::new()
                },
                false
            ),
            Some(ExitOutcome::Approved)
        );
        assert_eq!(
            terminal_exit(
                &R::Failed {
                    reason: String::new()
                },
                true
            ),
            Some(ExitOutcome::Failed)
        );
        assert_eq!(
            terminal_exit(&R::Denied, false),
            Some(ExitOutcome::Rejected)
        );
        assert_eq!(terminal_exit(&R::Denied, true), Some(ExitOutcome::Expired));
        assert_eq!(
            terminal_exit(
                &R::AlreadyResolved {
                    state: TerminalState::Denied
                },
                true
            ),
            Some(ExitOutcome::Rejected),
            "somebody else's deny is a rejection, never our deadline"
        );
        assert_eq!(
            terminal_exit(
                &R::AlreadyResolved {
                    state: TerminalState::Failed
                },
                false
            ),
            Some(ExitOutcome::Failed),
            "somebody else's approval that failed to broadcast is a failure, not a rejection"
        );
        assert_eq!(
            terminal_exit(
                &R::AlreadyResolved {
                    state: TerminalState::Executed
                },
                true
            ),
            Some(ExitOutcome::Approved),
            "somebody else executed it while our deadline denied — the money moved"
        );
        assert_eq!(
            terminal_exit(
                &R::AlreadyResolved {
                    state: TerminalState::Expired
                },
                false
            ),
            Some(ExitOutcome::Expired)
        );
        assert_eq!(
            terminal_exit(
                &R::AlreadyResolved {
                    state: TerminalState::Pending
                },
                false
            ),
            None,
            "pending means another connection is executing — retry, do not exit"
        );
        for live in [
            R::PinRequired,
            R::BadPin { attempts_left: 1 },
            R::Locked { retry_after_s: 1 },
            R::PinNotSet,
            R::PinUnavailable,
            R::UnknownId,
            R::Unauthorized,
        ] {
            assert_eq!(
                terminal_exit(&live, false),
                None,
                "{live:?} is not terminal"
            );
        }
    }

    // ── confirmation: non-terminal answers keep the human in charge ──

    #[test]
    fn pin_required_opens_the_pin_prompt_and_keeps_the_item_live() {
        // The server considers the item high-risk even though the summary did not.
        let mut m = confirming("a", false);
        m.update(Msg::Approve);
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::PinRequired)));
        let c = confirm_of(&m);
        assert_eq!(c.pin_len(), Some(0));
        assert_eq!(c.error(), Some(&ResolveError::PinRequired));
        assert!(!c.is_resolving(), "the human may act again");
    }

    #[test]
    fn a_bad_pin_clears_the_digits_and_shows_the_attempts_left() {
        let mut m = confirming("a", true);
        m.update(Msg::Approve);
        type_pin(&mut m, "0000");
        m.update(Msg::PinSubmit);
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::BadPin {
            attempts_left: 2,
        })));
        let c = confirm_of(&m);
        assert_eq!(
            c.pin_len(),
            Some(0),
            "the wrong PIN is cleared for re-entry"
        );
        assert_eq!(c.error(), Some(&ResolveError::BadPin(2)));
        assert!(!c.is_resolving());
    }

    #[test]
    fn a_pending_race_is_transient_and_lets_the_human_retry() {
        let mut m = confirming("a", false);
        m.update(Msg::Reject);
        m.update(Msg::Reply(Reply::Resolve(
            ResolveOutcome::AlreadyResolved {
                state: TerminalState::Pending,
            },
        )));
        let c = confirm_of(&m);
        assert_eq!(c.error(), Some(&ResolveError::Busy));
        assert!(!c.is_resolving());
        // and a retry is actually sent
        assert!(matches!(
            m.update(Msg::Reject),
            Some(transport::Request::Deny(_))
        ));
    }

    #[test]
    fn a_locked_answer_shows_the_lockout_and_clears_the_pin() {
        let mut m = confirming("a", true);
        m.update(Msg::Approve);
        type_pin(&mut m, "1111");
        m.update(Msg::PinSubmit);
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::Locked {
            retry_after_s: 300,
        })));
        let c = confirm_of(&m);
        assert_eq!(c.error(), Some(&ResolveError::Locked(300)));
        assert_eq!(c.pin_len(), Some(0));
    }

    #[test]
    fn a_vanished_item_closes_the_confirmation_with_a_note() {
        let mut m = confirming("a", false);
        m.update(Msg::Approve);
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::UnknownId)));
        let Phase::Watching { confirm, note, .. } = m.phase() else {
            panic!("back to watching");
        };
        assert!(confirm.is_none());
        assert!(note.is_some());
    }

    #[test]
    fn an_unauthorized_answer_to_an_authed_session_is_fatal() {
        let mut m = confirming("a", false);
        m.update(Msg::Approve);
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::Unauthorized)));
        assert!(
            matches!(m.phase(), Phase::Fatal(TransportError::Protocol(_))),
            "fail closed: never retry a money action on a channel we misread"
        );
    }

    #[test]
    fn a_resolve_answer_without_an_open_confirmation_is_ignored() {
        let mut m = watching(vec![summary("a")]);
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::Denied)));
        assert!(matches!(m.phase(), Phase::Watching { .. }));
    }

    #[test]
    fn approve_and_reject_do_nothing_without_an_open_confirmation() {
        let mut m = watching(vec![summary("a")]);
        assert!(m.update(Msg::Approve).is_none());
        assert!(m.update(Msg::Reject).is_none());
        assert!(m.update(Msg::Expire).is_none());
    }

    #[test]
    fn navigation_saturates_at_the_edges() {
        let mut m = watching(vec![summary("a"), summary("b")]);
        m.update(Msg::MoveUp); // already at the top → stays at 0
        let Phase::Watching { selected, .. } = m.phase() else {
            panic!("watching");
        };
        assert_eq!(*selected, 0);
        for _ in 0..5 {
            m.update(Msg::MoveDown); // past the end → clamps to the last row
        }
        let Phase::Watching {
            selected, items, ..
        } = m.phase()
        else {
            panic!("watching");
        };
        assert_eq!(*selected, items.len() - 1);
    }
}
