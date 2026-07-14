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

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::protocol::{
    AuthOutcome, Card, ContextOutcome, GetOutcome, OutcomeEntry, OutcomeState, PositionsOutcome,
    ResolveOutcome, Summary, TerminalState, WalletContext,
};
use crate::transport::{self, Reply, TransportError};
use crate::ui;

/// The approval PIN as it is typed. Zeroized on drop (via [`Zeroizing`]) and
/// **redacted in `Debug`** so it never lands in a log line, a panic message, or a
/// derived `Debug` of the `Model`. Only ASCII digits are accepted, which also keeps
/// the hand-built auth JSON injection-free.
/// Hard cap on typed PIN digits — far beyond any real PIN. The buffer is
/// pre-reserved to exactly this length so it can never reallocate: a realloc
/// frees the old allocation (with the digits in it) **without** zeroizing.
const MAX_PIN_DIGITS: usize = 64;

// No `Clone`: a `String::clone` allocates `capacity == len`, so a cloned PIN
// would silently lose the reallocation-proof reserve below — the first push on
// it would free the digits un-zeroized. Nobody clones a PIN; nobody gets to.
pub struct Pin(Zeroizing<String>);

impl Default for Pin {
    fn default() -> Self {
        // Reserved to the cap up front — `push` never grows the buffer.
        Self(Zeroizing::new(String::with_capacity(MAX_PIN_DIGITS)))
    }
}

impl std::fmt::Debug for Pin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the digits — length only, so a Debug of the Model cannot leak it.
        write!(f, "Pin(<{} digits>)", self.0.len())
    }
}

impl Pin {
    /// Append a digit; non-digits are ignored (keeps the auth JSON injection-free),
    /// and so is anything past [`MAX_PIN_DIGITS`] — the pre-reserved buffer must
    /// never reallocate (a realloc frees the digits un-zeroized).
    pub fn push(&mut self, c: char) {
        if c.is_ascii_digit() && self.0.len() < MAX_PIN_DIGITS {
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

    /// Clear the PIN, zeroizing the current buffer (the old `Zeroizing` is
    /// dropped). The fresh buffer is reserved to the cap again, like
    /// [`Self::default`] — cleared-and-retyped digits must not reallocate either.
    pub fn clear(&mut self) {
        self.0 = Zeroizing::new(String::with_capacity(MAX_PIN_DIGITS));
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
        /// The transient notice line (decision outcome, PIN lockout, or an
        /// informational note) — one slot, latest event wins.
        notice: Option<Notice>,
        /// Which screen is on top (nav-shell). Lives here, not on the
        /// [`Model`]: a view before auth is unrepresentable.
        view: View,
    },
    /// The connection is finished — render the reason and exit.
    Fatal(TransportError),
}

/// The resident console's screens (nav-shell). All are real views — no
/// placeholder tabs are registered (Gate-1, Stage 2 ratification).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Balance, DeFi positions, and the "waiting for you" count — the home
    /// view after auth (Gate-1 Stage 5, plan Фазы 2: PIN-unlock → Dashboard).
    Dashboard,
    /// The queue and its card — the approve surface, and the only view that
    /// may open one.
    Queue,
    /// The wallet's own address + QR. Pure display: signs nothing, sends
    /// nothing, adds no socket op.
    Receive,
    /// Terminal outcomes — the local log merged with the server's retained
    /// window (Stage 7). Pure display with a state filter; addresses here
    /// are SHORTENED (`format::short_addr`) — a display list, not a signing
    /// surface (ТЗ §4.1).
    Activity,
}

/// The dashboard's positions block — a tri-state, so "still loading" never
/// reads as "unavailable" (spec /check-4).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Positions {
    /// No `positions` reply has landed yet this session.
    #[default]
    NotYet,
    /// The wallet's positions, verbatim from the last reply.
    Loaded(Vec<crate::protocol::Position>),
    /// The read degraded (`wallet_locked`) — distinct from an empty list.
    Unavailable,
}

/// Which read-op the scheduler sends next — `context` (balance refresh) and
/// `positions` alternate on the same list-first slot (Gate-1 decision №2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ReadOp {
    #[default]
    Positions,
    Context,
}

/// Read-data staleness threshold, in `Msg::Tick` units (~2.5 s each): ~30 s.
/// Positions/balances are live on-chain reads — not something to hammer on
/// every poll tick (Gate-1 decision №3).
const STALE_TICKS: u32 = 12;

/// The one transient notice slot of the watch screen. A resident console keeps
/// living after a decision — the notice is how the human sees what just
/// happened without the process ending. Latest event wins the slot; the
/// informational [`Self::Note`] never overwrites a decision or a lockout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// The PIN lockout tripped ([protocol §4]: every *pending* item was denied
    /// fail-closed; an item already executing is untouched). `retry_after_s`
    /// is the server's answer when it said `locked`; the arming `bad_pin
    /// {attempts_left: 0}` response does not carry one. Advisory: the ladder
    /// is server-side — a later `locked` answer re-arms this notice.
    Locked {
        /// Seconds until the server accepts a PIN again, when known.
        retry_after_s: Option<u64>,
    },
    /// A decision reached its terminal state (the resident replacement for the
    /// old exit-with-outcome).
    Outcome {
        /// What happened to the money.
        kind: DecisionKind,
        /// The tx hash (executed) or failure reason (failed), when carried.
        detail: Option<String>,
    },
    /// An informational note (e.g. the selected item vanished).
    Note(String),
}

/// What happened to the money — the classification behind the outcome notice
/// and the machine decision line. It reports the item's fate, not which key
/// the human pressed: an item another connection executed while we were
/// denying it is still [`Self::Approved`]; a `deny` sent by the expiry
/// deadline reports [`Self::Expired`], not [`Self::Rejected`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionKind {
    /// Signed and broadcast.
    Approved,
    /// A human said no.
    Rejected,
    /// The deadline passed before a decision.
    Expired,
    /// Approved, but signing/broadcast failed — no money moved.
    Failed,
}

/// One terminal decision, recorded for the machine decision line (one JSON
/// line per decision on a non-TTY stdout — AGENTS.md #7, ADR
/// `2026-07-12-invariant-7-decision-stream`). Drained by the run loop via
/// [`Model::take_decision`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// What happened to the money.
    pub kind: DecisionKind,
    /// The executed transaction's hash, when it was ours to report.
    pub tx_hash: Option<String>,
    /// The failure reason, on a failed execution.
    pub reason: Option<String>,
}

/// One row of the console's activity history — the local log's stored form
/// (one JSONL line) and the Activity view's display form, one representation
/// end to end. `to`/`amount_wei`/`chain_id` are `None` on a server-only
/// record: the retained window drops the preview on resolve, so a record
/// first seen via `activity` is honestly poorer than one written at decision
/// time. Unknown fields are ignored on load (soft evolution, no version field).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Resolution time, unix seconds — stamped by the run loop (the model has
    /// no clock): `now` for a decision made here, `arrival − age_secs` for a
    /// server outcome (§3.9).
    pub unix: u64,
    /// The item's preview id — the dedup key (§3.9).
    pub id: String,
    /// Terminal state, protocol vocabulary (§3.5/§3.9 words, one end to end).
    pub state: OutcomeState,
    /// Recipient (EIP-55, stored verbatim) — rich records only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Native value, decimal wei string — rich records only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount_wei: Option<String>,
    /// Chain id — rich records only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<u64>,
    /// Executed transaction hash, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<String>,
    /// Operator-masked failure reason, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A decision made on THIS console, drained by the run loop
/// ([`Model::take_decided_outcome`]) to be stamped (unix = now), appended to
/// the local log, and pushed back via [`Model::push_history`]. The timestamp
/// is deliberately absent — the model has no clock (Stage-2 lesson). Born in
/// `apply_resolve` while the card is still open: the ONLY moment to/amount
/// are known (the server's retained outcome drops the preview).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecidedOutcome {
    /// The item's preview id.
    pub id: String,
    /// Terminal state, protocol vocabulary.
    pub state: OutcomeState,
    /// Recipient, EIP-55 verbatim (from the open card).
    pub to: String,
    /// Native value, decimal wei string (from the open card).
    pub amount_wei: String,
    /// Chain id (from the open card).
    pub chain_id: u64,
    /// Executed transaction hash, when this console saw it.
    pub tx_hash: Option<String>,
    /// Failure reason, when this console saw it.
    pub reason: Option<String>,
}

/// The Activity view's outcome filter, cycled by `f`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HistoryFilter {
    /// Every outcome.
    #[default]
    All,
    /// Executed only.
    Executed,
    /// Denied only.
    Denied,
    /// Expired only.
    Expired,
    /// Failed only.
    Failed,
}

impl HistoryFilter {
    /// The next filter in the `f`-cycle.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Executed,
            Self::Executed => Self::Denied,
            Self::Denied => Self::Expired,
            Self::Expired => Self::Failed,
            Self::Failed => Self::All,
        }
    }

    /// Whether an entry in this state passes the filter.
    #[must_use]
    pub fn admits(self, state: OutcomeState) -> bool {
        match self {
            Self::All => true,
            Self::Executed => state == OutcomeState::Executed,
            Self::Denied => state == OutcomeState::Denied,
            Self::Expired => state == OutcomeState::Expired,
            Self::Failed => state == OutcomeState::Failed,
        }
    }

    /// The view-header label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Executed => "executed",
            Self::Denied => "denied",
            Self::Expired => "expired",
            Self::Failed => "failed",
        }
    }
}

/// The protocol word for a decision made here — the ONE mapping between
/// [`DecisionKind`] and the stored/wire vocabulary (a named seam: a swapped
/// arm must fail a test, not pass every gate green).
fn outcome_state(kind: DecisionKind) -> OutcomeState {
    match kind {
        DecisionKind::Approved => OutcomeState::Executed,
        DecisionKind::Rejected => OutcomeState::Denied,
        DecisionKind::Expired => OutcomeState::Expired,
        DecisionKind::Failed => OutcomeState::Failed,
    }
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

/// A **non-terminal** failure of `approve`/`deny`: the item is still live and the
/// human can act again. Terminal answers close the card and raise a
/// [`Notice::Outcome`] instead — the resident console keeps running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// The item is high-risk; a per-request PIN is needed (the entry is untouched).
    PinRequired,
    /// Wrong PIN; attempts before lockout. An armed `attempts_left: 0` never
    /// lands here — the lockout closes the card and raises [`Notice::Locked`].
    BadPin(u32),
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
    /// The terminal was resized (or its size first learned). The model gates
    /// approval on the card being fully readable at this size.
    Resize {
        /// Terminal width, in columns.
        width: u16,
        /// Terminal height, in rows.
        height: u16,
    },
    /// Switch to a view (nav-shell tab). Refused while a card is open or a
    /// card request exists — see [`Model::on_view`].
    View(View),
    /// Cycle the Activity view's outcome filter (`f`). A no-op outside the
    /// Activity view — the key is only mapped there, but the model is the
    /// boundary, not the key map.
    Filter,
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
    /// Last known terminal size, fed by [`Msg::Resize`]. Starts at zero — until
    /// the size is known, nothing fits and approval is refused (fail closed).
    viewport: (u16, u16),
    quit: bool,
    /// The wallet's own context (`context`, proto 2), fetched once right after
    /// a successful `auth`. `None` until it lands — or for the whole session
    /// when the op degrades (`wallet_locked`): the card then falls back to its
    /// To-only layout, and approve is never gated on it (the From block is
    /// display-only; the signing-critical surface does not depend on it).
    wallet: Option<WalletContext>,
    /// The last terminal decision, waiting for the run loop to drain it into
    /// the machine decision line ([`Self::take_decision`]).
    decision: Option<Decision>,
    /// The dashboard's positions block (tri-state; see [`Positions`]).
    positions: Positions,
    /// Ticks since the last completed read-op — the scheduler's staleness
    /// clock (the pure model has no wall clock; Stage-2 lesson).
    read_age: u32,
    /// Which read-op goes out next (context/positions alternate).
    next_read: ReadOp,
    /// The last `context` REFRESH answered `wallet_locked`: the old data is
    /// kept (the card's From→To keeps working) but the dashboard's balance
    /// block is flagged stale (Gate-1: named mechanism, not derived from
    /// `read_age`). Cleared by a successful refresh.
    context_stale: bool,
    /// Activity history, newest first (unix desc, id asc) — fed ONLY by the
    /// run loop (log load + stamped pushes); the model never stamps a time
    /// (it has no clock). Capped at [`HISTORY_CAP`].
    history: Vec<HistoryEntry>,
    /// The Activity view's outcome filter (`f` cycles it).
    history_filter: HistoryFilter,
    /// A decision made here, waiting for the run loop to stamp and persist
    /// it ([`Self::take_decided_outcome`]).
    decided: Option<DecidedOutcome>,
    /// Server outcomes from `activity` replies, waiting for the run loop's
    /// dedup/stamp pass ([`Self::take_server_outcomes`]).
    server_outcomes: Vec<OutcomeEntry>,
    /// A static log-degradation note for the Activity footer, set once at
    /// startup by the run loop (persistence off / log partially unreadable).
    /// `None` when the log is healthy.
    history_note: Option<String>,
}

/// In-memory/display cap on the activity history (the file compacts at 2×
/// this on load — run-loop side, which is why it is `pub`).
pub const HISTORY_CAP: usize = 500;

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
            viewport: (0, 0),
            quit: false,
            wallet: None,
            decision: None,
            positions: Positions::default(),
            // Born stale: the first list reply after auth dispatches the
            // first positions read immediately, not ~30 s later.
            read_age: STALE_TICKS,
            next_read: ReadOp::default(),
            context_stale: false,
            history: Vec::new(),
            history_filter: HistoryFilter::default(),
            decided: None,
            server_outcomes: Vec::new(),
            history_note: None,
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

    /// The wallet's own address (EIP-55, verbatim from `context`), when the
    /// read-op has landed — feeds the card's From block. `None` degrades the
    /// card to its To-only layout, never gating approve.
    #[must_use]
    pub fn wallet_address(&self) -> Option<&str> {
        self.wallet.as_ref().map(|w| w.address.as_str())
    }

    /// The wallet's full context (address, balances, allowed chains), when
    /// the read-op has landed — feeds the dashboard's balance block.
    #[must_use]
    pub fn wallet_context(&self) -> Option<&WalletContext> {
        self.wallet.as_ref()
    }

    /// The dashboard's positions block (tri-state).
    #[must_use]
    pub fn positions(&self) -> &Positions {
        &self.positions
    }

    /// Whether the last balance refresh failed while old data is still shown
    /// (the dashboard says "may be stale" instead of lying by omission).
    #[must_use]
    pub fn context_stale(&self) -> bool {
        self.context_stale
    }

    /// Drain the last terminal decision (for the machine decision line — one
    /// JSON line per decision on a non-TTY stdout, AGENTS.md #7).
    pub fn take_decision(&mut self) -> Option<Decision> {
        self.decision.take()
    }

    /// The activity history, newest first (unix desc, id asc — display order).
    #[must_use]
    pub fn history(&self) -> &[HistoryEntry] {
        &self.history
    }

    /// The Activity view's current outcome filter.
    #[must_use]
    pub fn history_filter(&self) -> HistoryFilter {
        self.history_filter
    }

    /// The static log-degradation note for the Activity footer, if any.
    #[must_use]
    pub fn history_note(&self) -> Option<&str> {
        self.history_note.as_deref()
    }

    /// Set the log-degradation footer note (run-loop startup, at most once).
    pub fn set_history_note(&mut self, note: String) {
        self.history_note = Some(note);
    }

    /// Seed the history from the local log (run-loop startup; the caller
    /// dedups by id — it owns the dedup set). Sorted and capped here so the
    /// display invariant never depends on file order.
    pub fn set_history(&mut self, entries: Vec<HistoryEntry>) {
        self.history = entries;
        self.sort_and_cap_history();
    }

    /// Add one stamped entry (run loop only; id uniqueness is the caller's
    /// contract — it owns the dedup set).
    pub fn push_history(&mut self, entry: HistoryEntry) {
        self.history.push(entry);
        self.sort_and_cap_history();
    }

    /// Fill MISSING detail on a known entry (spec /check-2), both ways: an
    /// item resolved by another connection leaves our rich record without
    /// tx_hash/reason (the server window carries them); a server-only record
    /// gains to/amount/chain when the same id is later decided here. Present
    /// fields are never overwritten; the file keeps its first record
    /// (append-only) — this enriches the session view only.
    pub fn fill_history_detail(
        &mut self,
        id: &str,
        to: Option<&str>,
        amount_wei: Option<&str>,
        chain_id: Option<u64>,
        tx_hash: Option<&str>,
        reason: Option<&str>,
    ) {
        let Some(entry) = self.history.iter_mut().find(|e| e.id == id) else {
            return;
        };
        if entry.to.is_none() {
            entry.to = to.map(str::to_owned);
        }
        if entry.amount_wei.is_none() {
            entry.amount_wei = amount_wei.map(str::to_owned);
        }
        if entry.chain_id.is_none() {
            entry.chain_id = chain_id;
        }
        if entry.tx_hash.is_none() {
            entry.tx_hash = tx_hash.map(str::to_owned);
        }
        if entry.reason.is_none() {
            entry.reason = reason.map(str::to_owned);
        }
    }

    /// Drain the decision made here, for the run loop to stamp (unix = now),
    /// persist, and push back via [`Self::push_history`] — in the same loop
    /// pass, before the next render (the order is normative).
    pub fn take_decided_outcome(&mut self) -> Option<DecidedOutcome> {
        self.decided.take()
    }

    /// Drain the server outcomes queued by `activity` replies, for the run
    /// loop's dedup/stamp pass (unix = arrival − age, §3.9).
    pub fn take_server_outcomes(&mut self) -> Vec<OutcomeEntry> {
        std::mem::take(&mut self.server_outcomes)
    }

    /// Newest first: unix desc, id asc on ties (a lockout batch shares one
    /// second — determinism mirrors the server's own tiebreak). Two RICH
    /// decisions inside one second tie-break by id too — a documented
    /// consequence of whole-second stamps, not chronology (Gate-1 finding).
    fn sort_and_cap_history(&mut self) {
        self.history
            .sort_unstable_by(|a, b| b.unix.cmp(&a.unix).then_with(|| a.id.cmp(&b.id)));
        self.history.truncate(HISTORY_CAP);
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
            Msg::Resize { width, height } => {
                self.viewport = (width, height);
                None
            }
            Msg::View(view) => {
                self.on_view(view);
                None
            }
            Msg::Filter => {
                if let Phase::Watching {
                    view: View::Activity,
                    confirm: None,
                    ..
                } = &self.phase
                {
                    self.history_filter = self.history_filter.next();
                }
                None
            }
            Msg::Open => self.on_open(),
        }
    }

    /// Switch the watch screen's view (nav-shell). Refused while ANY card
    /// request exists — open, on the wire, or parked: an open card is the
    /// decision surface, left by deciding and never by a tab (`AGENTS.md`
    /// #5); a `get` on the wire (`awaiting_card`) or parked behind the poll
    /// (`PendingIntent::Get` — `awaiting_card` is deliberately not raised
    /// until `flush_pending` sends it) is a card about to open, and switching
    /// in that gap would open it behind the Receive screen (Gate-1 finding).
    /// The check only reads `pending` — the latest-wins park semantics of a
    /// repeated Enter are untouched.
    fn on_view(&mut self, view: View) {
        if self.awaiting_card || matches!(self.pending, Some(PendingIntent::Get(_))) {
            return;
        }
        if let Phase::Watching {
            confirm: None,
            view: current,
            ..
        } = &mut self.phase
        {
            *current = view;
            if matches!(view, View::Dashboard | View::Activity) {
                // Entering a read-op view marks the read data stale: the next
                // list reply refreshes it. Bounded structurally — at most one
                // read-op per list reply (spec /check-5).
                self.read_age = self.read_age.max(STALE_TICKS);
            }
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
        if matches!(self.phase, Phase::Watching { .. }) {
            // The staleness clock of the read-op scheduler — ticks, not a
            // wall clock (the model stays pure).
            self.read_age = self.read_age.saturating_add(1);
        }
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
        if !ui::priority_fields_fit(
            c,
            self.wallet.as_ref().map(|w| w.address.as_str()),
            self.viewport.0,
            self.viewport.1,
        ) {
            // The prompt (or a warning above it) is off-screen: a PIN typed
            // into a card the human cannot read must not sign (the card shows
            // the TOO SMALL banner). Esc still rejects.
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
        if !ui::priority_fields_fit(
            c,
            self.wallet.as_ref().map(|w| w.address.as_str()),
            self.viewport.0,
            self.viewport.1,
        ) {
            // A "yes" to a card the human could not read is not a decision:
            // while the priority fields do not fit the terminal, `y` is dead —
            // it neither approves nor opens the PIN prompt. The card shows the
            // TOO SMALL banner; reject stays available (AGENTS.md #5).
            return None;
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
            view,
            ..
        } = &self.phase
        else {
            return None;
        };
        if *view != View::Queue {
            // The queue owns card opening. The key map never sends Open from
            // Receive, but the model is the security boundary, not the key
            // map: a get solicited here would open a card behind the Receive
            // screen (/check-3).
            return None;
        }
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
        // Only a fresh `list` answer may be followed by a read-op (the
        // list-first policy) — remembered before the reply is consumed.
        let was_list = matches!(reply, Reply::List(_));
        match reply {
            Reply::Hello { .. } => {
                self.phase = Phase::Authing {
                    pin: Pin::default(),
                    error: None,
                };
                // A handshake resets the protocol: an outstanding get will never
                // be answered (a stuck flag would refuse every future open), and
                // a parked intent — an approve first of all — belongs to the
                // session that parked it, not to the one being born.
                self.awaiting_card = false;
                self.pending = None;
            }
            Reply::Auth(outcome) => {
                let unlocked = matches!(outcome, AuthOutcome::Ok);
                self.apply_auth(outcome);
                if unlocked && self.wallet.is_none() {
                    // Fetch the wallet's own context exactly once, right after
                    // auth — before the first poll, while no card can be open
                    // (the read-op policy: display reads never contend with an
                    // open confirmation for the single in-flight slot).
                    self.in_flight = true;
                    return Some(transport::Request::Context);
                }
            }
            Reply::Context(outcome) => self.apply_context(outcome),
            Reply::Positions(outcome) => self.apply_positions(outcome),
            Reply::Activity(outcomes) => self.apply_activity(outcomes),
            Reply::List(items) => self.apply_list(items),
            Reply::Get(outcome) => self.apply_get(outcome),
            Reply::Resolve(outcome) => self.apply_resolve(outcome),
            Reply::Fatal(err) => {
                self.phase = Phase::Fatal(err);
                self.pending = None;
                return None;
            }
        }
        // A dead connection ends the session: nothing parked may still be sent.
        if matches!(self.phase, Phase::Fatal(_)) {
            self.pending = None;
            return None;
        }
        // Dispatch order is normative (spec /check-1): the parked USER intent
        // first — a read-op is considered only when nothing human is waiting.
        if let Some(req) = self.flush_pending() {
            return Some(req);
        }
        if was_list {
            return self.dispatch_read_op();
        }
        None
    }

    /// The read-op scheduler (list-first policy, canon §3.8 + the Reviewer's
    /// Gate-1 flag): a read-op goes out ONLY right after a fresh `list`
    /// reply — never from a tick (the tick belongs to `list`), never ahead
    /// of a parked user intent (`flush_pending` ran first), never with a
    /// card open or a card request anywhere in flight, and only when the
    /// read data has gone stale. WHICH op is the view's choice: the
    /// Dashboard alternates `positions`/`context` (the two-valued
    /// [`ReadOp`] state is untouched by Stage 7); the Activity view sends
    /// `activity` — it never joins the alternation, so leaving and
    /// re-entering the Dashboard resumes exactly where it left off.
    ///
    /// Guaranteed and tested: the ORDER OF ORIGINATION — a read-op never cuts
    /// ahead of a due `list`. Honest limit (Gate-1): once a read-op is on the
    /// wire and the server does not answer (no server-side timeout by canon),
    /// list polling stalls with it — the same risk class as the post-auth
    /// `context` fetch accepted since Stage 2, occurring more often here.
    fn dispatch_read_op(&mut self) -> Option<transport::Request> {
        let view = match &self.phase {
            Phase::Watching {
                confirm: None,
                view: view @ (View::Dashboard | View::Activity),
                ..
            } => *view,
            _ => return None,
        };
        // `pending` is None here (flush_pending ran first and found nothing)
        // and `awaiting_card` cannot coexist with a just-answered list on the
        // single slot — both checked anyway: the model is the boundary.
        if self.awaiting_card || self.pending.is_some() || self.read_age < STALE_TICKS {
            return None;
        }
        self.read_age = 0; // the cadence counts from dispatch
        self.in_flight = true;
        if view == View::Activity {
            return Some(transport::Request::Activity);
        }
        let req = match self.next_read {
            ReadOp::Positions => transport::Request::Positions,
            ReadOp::Context => transport::Request::Context,
        };
        self.next_read = match self.next_read {
            ReadOp::Positions => ReadOp::Context,
            ReadOp::Context => ReadOp::Positions,
        };
        Some(req)
    }

    /// Fold an `activity` answer: queue the server outcomes for the run
    /// loop's dedup/stamp pass ([`Self::take_server_outcomes`]). Deliberately
    /// view-independent — history is global state, an in-flight answer
    /// landing after a tab switch still counts.
    fn apply_activity(&mut self, outcomes: Vec<OutcomeEntry>) {
        self.server_outcomes.extend(outcomes);
    }

    /// Fold a `positions` answer — feeds the dashboard block only, gates
    /// nothing. Deliberately simpler than [`Self::apply_context`]: a
    /// `wallet_locked` here REPLACES `Loaded` with `Unavailable` instead of
    /// keeping-and-flagging, because unlike the balance block nothing else
    /// feeds off old positions (the card's From→To feeds off `wallet`) —
    /// and the state is unreachable today anyway (the shipped core never
    /// re-locks at runtime, canon §3.7). Revisit if positions grow a second
    /// consumer (Gate-2 NIT).
    fn apply_positions(&mut self, outcome: PositionsOutcome) {
        self.positions = match outcome {
            PositionsOutcome::Ok(list) => Positions::Loaded(list),
            PositionsOutcome::WalletLocked => Positions::Unavailable,
        };
    }

    fn apply_auth(&mut self, outcome: AuthOutcome) {
        match outcome {
            AuthOutcome::Ok => {
                self.phase = Phase::Watching {
                    items: Vec::new(),
                    selected: 0,
                    confirm: None,
                    notice: None,
                    // Home is the Dashboard (Gate-1 Stage 5: PIN-unlock →
                    // Dashboard, the letter of the Phase-2 plan).
                    view: View::Dashboard,
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

    /// Fold a `context` answer — the post-auth fetch or a scheduler refresh.
    /// Both failure shapes only degrade the display — approve is never gated
    /// on them. A REFRESH that answers `wallet_locked` keeps the old data
    /// (the card's From→To keeps working) but flags the dashboard's balance
    /// block stale (Gate-1: `context_stale`, a named mechanism).
    fn apply_context(&mut self, outcome: ContextOutcome) {
        match outcome {
            ContextOutcome::Ok(ctx) => {
                self.wallet = Some(*ctx);
                self.context_stale = false;
            }
            ContextOutcome::WalletLocked if self.wallet.is_some() => {
                // A refresh failed: keep what we have, say it may be stale.
                self.context_stale = true;
            }
            ContextOutcome::WalletLocked => {
                if let Phase::Watching { notice, .. } = &mut self.phase
                    && notice.is_none()
                {
                    // Informational only — never overwrite a decision/lockout.
                    *notice = Some(Notice::Note(
                        "wallet context unavailable — cards show the recipient only".to_owned(),
                    ));
                }
            }
        }
    }

    fn apply_get(&mut self, outcome: GetOutcome) {
        // Whatever this outcome says, it answers the one get that was allowed
        // on the wire — the console may ask for a card again.
        self.awaiting_card = false;
        if let Phase::Watching {
            confirm, notice, ..
        } = &mut self.phase
        {
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
                    *notice = None;
                }
                GetOutcome::UnknownId => {
                    // The selected item vanished between list and get — show a
                    // transient note instead of a dead card.
                    *notice = Some(Notice::Note(
                        "that request is no longer available".to_owned(),
                    ));
                }
            }
        }
    }

    /// Fold an `approve`/`deny` answer. A terminal answer closes the card,
    /// raises the outcome notice and records the machine decision line — the
    /// resident console returns to the queue instead of exiting. The rest
    /// leave the item live and the human in charge (`pin_required` opens the
    /// PIN prompt, `bad_pin` clears it, and so on).
    fn apply_resolve(&mut self, outcome: ResolveOutcome) {
        let (sent, timed_out) = match &mut self.phase {
            Phase::Watching {
                confirm: Some(c), ..
            } => (c.sent.take(), c.timed_out),
            // No confirmation is open — an answer to a decision we never made.
            _ => return,
        };

        if let Some(kind) = decision_kind(&outcome, timed_out) {
            let (tx_hash, reason) = match &outcome {
                ResolveOutcome::Executed { tx_hash } => (Some(tx_hash.clone()), None),
                ResolveOutcome::Failed { reason } => (None, Some(reason.clone())),
                _ => (None, None),
            };
            let detail = tx_hash.clone().or_else(|| reason.clone());
            self.decision = Some(Decision {
                kind,
                tx_hash: tx_hash.clone(),
                reason: reason.clone(),
            });
            if let Phase::Watching {
                confirm, notice, ..
            } = &mut self.phase
            {
                if let Some(c) = confirm.as_deref() {
                    // The card is still open — the ONLY moment to/amount are
                    // known (the server's retained outcome drops the preview):
                    // the rich history record is born here, stamped and
                    // persisted by the run loop (Stage 7).
                    self.decided = Some(DecidedOutcome {
                        id: c.card.id.clone(),
                        state: outcome_state(kind),
                        to: c.card.to.clone(),
                        amount_wei: c.card.amount_wei.clone(),
                        chain_id: c.card.chain_id,
                        tx_hash,
                        reason,
                    });
                }
                *confirm = None;
                *notice = Some(Notice::Outcome { kind, detail });
            }
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

        let Phase::Watching {
            confirm, notice, ..
        } = &mut self.phase
        else {
            return;
        };
        if matches!(outcome, ResolveOutcome::UnknownId) {
            *confirm = None;
            *notice = Some(Notice::Note(
                "that request is no longer available".to_owned(),
            ));
            return;
        }
        // The lockout tripped (an armed `bad_pin {attempts_left: 0}`) or was
        // already active (`locked`). The server has failed the queue closed —
        // every pending item is now denied (protocol §4) — so keeping this
        // card open as "still approvable" would be a lie: close it and raise
        // the lockout notice. NOT client-gated beyond that: a locked session
        // may still deny anything and approve non-high-risk items (the ladder
        // guards PIN paths only), so submits stay live and a repeat `locked`
        // answer simply re-arms this notice with a fresh countdown.
        let lockout = match outcome {
            ResolveOutcome::BadPin { attempts_left: 0 } => {
                Some(Notice::Locked {
                    retry_after_s: None, // the arming response carries no delay
                })
            }
            ResolveOutcome::Locked { retry_after_s } => Some(Notice::Locked {
                retry_after_s: Some(retry_after_s),
            }),
            _ => None,
        };
        if let Some(locked) = lockout {
            *confirm = None;
            *notice = Some(locked);
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
            ResolveOutcome::PinNotSet => ResolveError::NotSet,
            ResolveOutcome::PinUnavailable => ResolveError::Unavailable,
            // Only `already_resolved:pending` is non-terminal; the rest were taken
            // by `decision_kind` above.
            ResolveOutcome::AlreadyResolved { .. } => ResolveError::Busy,
            ResolveOutcome::Executed { .. }
            | ResolveOutcome::Failed { .. }
            | ResolveOutcome::Denied
            | ResolveOutcome::Locked { .. }
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

/// Is this answer the item's last word, and if so what happened to the money?
///
/// The kind reports **what happened to the money**, not which key was pressed:
/// an `already_resolved:executed` answer to our `deny` means another connection
/// got there first and the transaction went out — that is
/// [`DecisionKind::Approved`]. `timed_out` is the one place the cause matters:
/// a `deny` the deadline sent reports `expired`, so a caller can tell "the
/// human said no" from "nobody did".
///
/// `already_resolved:pending` is **not** terminal — another connection is executing
/// this id right now (protocol §3.5); the human may retry.
fn decision_kind(outcome: &ResolveOutcome, timed_out: bool) -> Option<DecisionKind> {
    // Only *our own* deny can have been sent by the deadline. An `already_resolved`
    // deny was somebody else's decision, so it stays a rejection.
    let our_deny = if timed_out {
        DecisionKind::Expired
    } else {
        DecisionKind::Rejected
    };
    match outcome {
        ResolveOutcome::Executed { .. } => Some(DecisionKind::Approved),
        ResolveOutcome::Failed { .. } => Some(DecisionKind::Failed),
        ResolveOutcome::Denied => Some(our_deny),
        ResolveOutcome::AlreadyResolved { state } => match state {
            TerminalState::Executed => Some(DecisionKind::Approved),
            TerminalState::Failed => Some(DecisionKind::Failed),
            TerminalState::Denied => Some(DecisionKind::Rejected),
            TerminalState::Expired => Some(DecisionKind::Expired),
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
    /// The wallet's own address in tests — full-length, so the From block
    /// exercises real wrapping (never a shortened stand-in).
    const WALLET: &str = "0x489Fe09Fbb489Fe09Fbb489Fe09Fbb489F9Fbbbb";

    fn watching(items: Vec<Summary>) -> Model {
        let mut m = Model::new();
        // The size report main sends at startup — a standard 80×24 terminal.
        m.update(Msg::Resize {
            width: 80,
            height: 24,
        });
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
        // The everyday session fetches the wallet context right after auth.
        assert!(matches!(
            m.update(Msg::Reply(Reply::Auth(AuthOutcome::Ok))),
            Some(transport::Request::Context)
        ));
        m.update(Msg::Reply(Reply::Context(ContextOutcome::Ok(Box::new(
            WalletContext {
                address: WALLET.to_owned(),
                balances: vec![],
                allowed_chains: vec![1],
            },
        )))));
        // Home is the Dashboard since Stage 5 — these tests exercise the
        // queue, so they step onto it explicitly (spec: 5 helpers, 0
        // rewritten tests).
        m.update(Msg::View(View::Queue));
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
    fn the_pin_buffer_never_reallocates_however_much_is_typed() {
        // A realloc frees the old buffer — with the digits in it — WITHOUT
        // zeroizing. The buffer is pre-reserved and capped, so it never moves.
        let mut pin = Pin::default();
        let capacity = pin.0.capacity();
        for _ in 0..100 {
            pin.push('7');
        }
        assert_eq!(pin.len(), MAX_PIN_DIGITS, "input is capped, not grown into");
        assert_eq!(
            pin.0.capacity(),
            capacity,
            "the PIN buffer must never reallocate"
        );

        // clear() hands out a fresh buffer — it must be just as realloc-proof.
        pin.clear();
        let capacity = pin.0.capacity();
        for _ in 0..100 {
            pin.push('7');
        }
        assert_eq!(pin.0.capacity(), capacity, "the cleared buffer too");
    }

    #[test]
    fn a_handshake_reset_drops_any_parked_intent() {
        // A parked money intent belongs to the session that parked it. If the
        // server ever restarts the handshake, flushing that intent afterwards
        // would fire an approve into a phase that cannot even show the card.
        let mut m = confirming("a", false);
        assert!(matches!(
            m.update(Msg::Tick),
            Some(transport::Request::List)
        ));
        m.update(Msg::Approve); // parked behind the poll
        assert!(
            m.update(Msg::Reply(Reply::Hello {
                server: "s".to_owned(),
            }))
            .is_none(),
            "a parked approve must die with the session, not follow the reset"
        );
    }

    #[test]
    fn auth_failures_map_each_code_to_its_own_message() {
        // A swapped arm here would tell a locked-out human to set a PIN — the
        // mappings must bite, not merely compile (несёт и retry-данные).
        for (outcome, expected) in [
            (
                AuthOutcome::BadPin { attempts_left: 2 },
                AuthError::BadPin(2),
            ),
            (
                AuthOutcome::Locked { retry_after_s: 30 },
                AuthError::Locked(30),
            ),
            (AuthOutcome::PinNotSet, AuthError::NotSet),
            (AuthOutcome::PinUnavailable, AuthError::Unavailable),
        ] {
            let mut m = Model::new();
            m.update(Msg::Reply(Reply::Hello {
                server: "s".to_owned(),
            }));
            m.update(Msg::PinDigit('1'));
            m.update(Msg::PinSubmit);
            m.update(Msg::Reply(Reply::Auth(outcome)));
            let Phase::Authing { error, .. } = m.phase() else {
                panic!("an auth failure keeps the unlock screen");
            };
            assert_eq!(error.as_ref(), Some(&expected));
        }
    }

    #[test]
    fn resolve_failures_map_each_code_to_its_own_error() {
        for (outcome, expected) in [
            (
                ResolveOutcome::BadPin { attempts_left: 1 },
                ResolveError::BadPin(1),
            ),
            (ResolveOutcome::PinNotSet, ResolveError::NotSet),
            (ResolveOutcome::PinUnavailable, ResolveError::Unavailable),
        ] {
            let mut m = confirming("a", false);
            m.update(Msg::Approve);
            m.update(Msg::Reply(Reply::Resolve(outcome)));
            assert_eq!(confirm_of(&m).error(), Some(&expected));
        }
    }

    #[test]
    fn a_locked_answer_closes_the_card_and_raises_the_lockout_notice() {
        // The lockout has failed the queue closed server-side (protocol §4):
        // every pending item is already denied, so a card left open as "still
        // approvable" would be a lie. The notice carries the server's delay.
        let mut m = confirming("a", false);
        m.update(Msg::Approve);
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::Locked {
            retry_after_s: 60,
        })));
        let Phase::Watching {
            confirm, notice, ..
        } = m.phase()
        else {
            panic!("a lockout keeps the resident console on the queue");
        };
        assert!(confirm.is_none(), "the dead card must close");
        assert_eq!(
            notice.as_ref(),
            Some(&Notice::Locked {
                retry_after_s: Some(60)
            })
        );
    }

    #[test]
    fn an_arming_bad_pin_closes_the_card_and_raises_the_lockout_notice() {
        // `bad_pin {attempts_left: 0}` IS the lockout arming (protocol §3.2):
        // the queue is dropped on that very answer; it carries no delay.
        let mut m = confirming("a", true);
        m.update(Msg::Approve); // opens the PIN prompt
        m.update(Msg::PinDigit('1'));
        m.update(Msg::PinSubmit);
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::BadPin {
            attempts_left: 0,
        })));
        let Phase::Watching {
            confirm, notice, ..
        } = m.phase()
        else {
            panic!("the arming response keeps the resident console on the queue");
        };
        assert!(confirm.is_none(), "the dead card must close");
        assert_eq!(
            notice.as_ref(),
            Some(&Notice::Locked {
                retry_after_s: None
            })
        );
    }

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

    // ── the approve gate: no "yes" to a card the human cannot read ──

    /// A card whose priority fields need more rows than an 80×14 terminal's
    /// card area can show (8 priority lines vs 7 inner rows).
    fn tall_card(id: &str) -> Box<Card> {
        Box::new(Card {
            id: id.to_owned(),
            chain_id: 1,
            to: "0xabc".to_owned(),
            amount_wei: "0".to_owned(),
            decoded_call: Some(crate::protocol::DecodedCall {
                method: "approve".to_owned(),
                spender: Some("0xdeadbeef".to_owned()),
                operator: None,
                from: None,
                to: None,
                token: None,
                amount: Some("0xffffffffffffffff".to_owned()),
                deadline: None,
                approved: None,
                is_unlimited: Some(true),
            }),
            high_risk: true,
            high_risk_reasons: vec!["unlimited_approval".to_owned()],
            raw_data: "0x".to_owned(),
            not_after_unix: 1,
        })
    }

    fn confirming_tall(id: &str) -> Model {
        let mut m = watching(vec![summary(id)]);
        assert!(matches!(
            m.update(Msg::Open),
            Some(transport::Request::Get(_))
        ));
        m.update(Msg::Reply(Reply::Get(GetOutcome::Card(tall_card(id)))));
        m
    }

    #[test]
    fn approve_is_dead_while_the_priority_fields_cannot_fit_the_terminal() {
        let mut m = confirming_tall("a");
        // 13 rows: too short for the card since v2 dropped the zero native-value row.
        m.update(Msg::Resize {
            width: 80,
            height: 13,
        });

        assert!(m.update(Msg::Approve).is_none());
        let c = confirm_of(&m);
        assert!(
            !c.is_resolving(),
            "no decision may leave a card the human cannot read"
        );
        assert!(
            c.pin_len().is_none(),
            "the PIN prompt must not open over a mutilated card"
        );
        // Default-deny survives at any size (AGENTS.md #5).
        assert!(matches!(
            m.update(Msg::Reject),
            Some(transport::Request::Deny(id)) if id == "a"
        ));
    }

    #[test]
    fn a_taller_terminal_re_arms_approve() {
        let mut m = confirming_tall("a");
        // 13 rows: too short for the card (see approve_is_dead… for the boundary).
        m.update(Msg::Resize {
            width: 80,
            height: 13,
        });
        assert!(m.update(Msg::Approve).is_none());
        assert!(confirm_of(&m).pin_len().is_none());

        m.update(Msg::Resize {
            width: 80,
            height: 24,
        });
        // High-risk: `y` now opens the PIN prompt again.
        assert!(m.update(Msg::Approve).is_none());
        assert!(confirm_of(&m).pin_len().is_some());
    }

    #[test]
    fn pin_submit_is_dead_while_the_prompt_is_off_screen() {
        let mut m = confirming_tall("a");
        m.update(Msg::Approve); // 80×24: opens the PIN prompt
        assert!(confirm_of(&m).pin_len().is_some());
        type_pin(&mut m, "42");

        // The terminal shrinks mid-prompt: the submit must die with the view.
        m.update(Msg::Resize {
            width: 80,
            height: 14,
        });
        assert!(
            m.update(Msg::PinSubmit).is_none(),
            "a PIN typed into an unreadable card must not sign"
        );
        assert!(!confirm_of(&m).is_resolving());
        // Esc still rejects (AGENTS.md #5).
        assert!(matches!(
            m.update(Msg::Reject),
            Some(transport::Request::Deny(_))
        ));
    }

    #[test]
    fn an_unknown_terminal_size_fails_closed_for_approve_only() {
        // No Msg::Resize ever arrived (terminal.size() failed at startup).
        let mut m = Model::new();
        m.update(Msg::Reply(Reply::Hello {
            server: "s".to_owned(),
        }));
        m.update(Msg::PinDigit('1'));
        m.update(Msg::PinSubmit);
        m.update(Msg::Reply(Reply::Auth(AuthOutcome::Ok)));
        m.update(Msg::Tick);
        m.update(Msg::Reply(Reply::List(vec![summary("a")])));
        m.update(Msg::Open);
        m.update(Msg::Reply(Reply::Get(GetOutcome::Card(card("a")))));

        assert!(m.update(Msg::Approve).is_none());
        assert!(!confirm_of(&m).is_resolving());
        assert!(matches!(
            m.update(Msg::Reject),
            Some(transport::Request::Deny(_))
        ));
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
        let Phase::Watching {
            confirm, notice, ..
        } = m.phase()
        else {
            panic!("watching");
        };
        assert!(confirm.is_none());
        assert!(matches!(notice, Some(Notice::Note(_))));
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

    // ── confirmation: terminal answers (the resident console stays) ──

    /// The residency red-mutation guard (ТЗ §5): a terminal answer must leave
    /// the model WATCHING — card closed, outcome notice up, polling alive. If
    /// `apply_resolve` ever ends the session again, this fails first.
    fn assert_resident_outcome(m: &mut Model, kind: DecisionKind, detail: Option<&str>) {
        let Phase::Watching {
            confirm, notice, ..
        } = m.phase()
        else {
            panic!("a terminal answer must keep the resident console watching");
        };
        assert!(confirm.is_none(), "the decided card must close");
        assert_eq!(
            notice.as_ref(),
            Some(&Notice::Outcome {
                kind,
                detail: detail.map(str::to_owned)
            })
        );
        // The queue keeps living: the very next tick polls `list` again.
        assert!(matches!(
            m.update(Msg::Tick),
            Some(transport::Request::List)
        ));
    }

    #[test]
    fn an_executed_answer_stays_resident_and_reports_approved_with_the_tx_hash() {
        let mut m = confirming("a", false);
        m.update(Msg::Approve);
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::Executed {
            tx_hash: "0xfeed".to_owned(),
        })));
        assert_resident_outcome(&mut m, DecisionKind::Approved, Some("0xfeed"));
        assert_eq!(
            m.take_decision(),
            Some(Decision {
                kind: DecisionKind::Approved,
                tx_hash: Some("0xfeed".to_owned()),
                reason: None,
            })
        );
        assert_eq!(
            m.take_decision(),
            None,
            "a decision is drained exactly once"
        );
    }

    #[test]
    fn a_failed_broadcast_reports_failed_not_approved() {
        let mut m = confirming("a", false);
        m.update(Msg::Approve);
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::Failed {
            reason: "nonce too low".to_owned(),
        })));
        assert_resident_outcome(&mut m, DecisionKind::Failed, Some("nonce too low"));
        let d = m
            .take_decision()
            .expect("a terminal answer records a decision");
        assert_eq!(d.kind, DecisionKind::Failed, "no money moved");
        assert_eq!(d.reason.as_deref(), Some("nonce too low"));
        assert_eq!(d.tx_hash, None);
    }

    #[test]
    fn a_human_deny_reports_rejected() {
        let mut m = confirming("a", false);
        m.update(Msg::Reject);
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::Denied)));
        assert_resident_outcome(&mut m, DecisionKind::Rejected, None);
        assert_eq!(
            m.take_decision().expect("recorded").kind,
            DecisionKind::Rejected
        );
    }

    #[test]
    fn the_deadline_denies_fail_closed_and_reports_expired_not_rejected() {
        let mut m = confirming("a", false);
        let req = m.update(Msg::Expire);
        assert!(
            matches!(req, Some(transport::Request::Deny(id)) if id == "a"),
            "an expiring item is denied, never left pending"
        );
        // The server has not observed the expiry yet and simply denies it.
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::Denied)));
        assert_resident_outcome(&mut m, DecisionKind::Expired, None);
        assert_eq!(
            m.take_decision().expect("recorded").kind,
            DecisionKind::Expired,
            "the deadline said no, not the human"
        );
    }

    #[test]
    fn an_item_executed_by_another_connection_reports_approved_even_if_we_denied() {
        // The kind reports what happened to the money, not which key was
        // pressed — and the hash was never ours to report.
        let mut m = confirming("a", false);
        m.update(Msg::Reject);
        m.update(Msg::Reply(Reply::Resolve(
            ResolveOutcome::AlreadyResolved {
                state: TerminalState::Executed,
            },
        )));
        assert_resident_outcome(&mut m, DecisionKind::Approved, None);
        let d = m.take_decision().expect("recorded");
        assert_eq!(d.kind, DecisionKind::Approved);
        assert_eq!(d.tx_hash, None, "the hash was never ours to report");
    }

    #[test]
    fn an_already_expired_item_reports_expired() {
        let mut m = confirming("a", false);
        m.update(Msg::Approve);
        m.update(Msg::Reply(Reply::Resolve(
            ResolveOutcome::AlreadyResolved {
                state: TerminalState::Expired,
            },
        )));
        assert_resident_outcome(&mut m, DecisionKind::Expired, None);
    }

    #[test]
    fn a_second_item_can_be_decided_after_the_first_resolves() {
        // The point of residency: the session survives a decision. Decide one
        // item, receive a fresh queue, open and decide the next — one session.
        let mut m = confirming("a", false);
        m.update(Msg::Approve);
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::Executed {
            tx_hash: "0x01".to_owned(),
        })));
        let _ = m.take_decision();
        // next poll delivers a new item
        assert!(matches!(
            m.update(Msg::Tick),
            Some(transport::Request::List)
        ));
        m.update(Msg::Reply(Reply::List(vec![summary("b")])));
        assert!(matches!(
            m.update(Msg::Open),
            Some(transport::Request::Get(_))
        ));
        m.update(Msg::Reply(Reply::Get(GetOutcome::Card(card("b")))));
        assert!(m.update(Msg::Reject).is_some());
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::Denied)));
        assert_resident_outcome(&mut m, DecisionKind::Rejected, None);
        assert_eq!(
            m.take_decision().expect("second decision recorded").kind,
            DecisionKind::Rejected
        );
    }

    #[test]
    fn decision_kind_maps_every_answer() {
        use ResolveOutcome as R;
        assert_eq!(
            decision_kind(
                &R::Executed {
                    tx_hash: String::new()
                },
                false
            ),
            Some(DecisionKind::Approved)
        );
        assert_eq!(
            decision_kind(
                &R::Failed {
                    reason: String::new()
                },
                true
            ),
            Some(DecisionKind::Failed)
        );
        assert_eq!(
            decision_kind(&R::Denied, false),
            Some(DecisionKind::Rejected)
        );
        assert_eq!(decision_kind(&R::Denied, true), Some(DecisionKind::Expired));
        assert_eq!(
            decision_kind(
                &R::AlreadyResolved {
                    state: TerminalState::Denied
                },
                true
            ),
            Some(DecisionKind::Rejected),
            "somebody else's deny is a rejection, never our deadline"
        );
        assert_eq!(
            decision_kind(
                &R::AlreadyResolved {
                    state: TerminalState::Failed
                },
                false
            ),
            Some(DecisionKind::Failed),
            "somebody else's approval that failed to broadcast is a failure, not a rejection"
        );
        assert_eq!(
            decision_kind(
                &R::AlreadyResolved {
                    state: TerminalState::Executed
                },
                true
            ),
            Some(DecisionKind::Approved),
            "somebody else executed it while our deadline denied — the money moved"
        );
        assert_eq!(
            decision_kind(
                &R::AlreadyResolved {
                    state: TerminalState::Expired
                },
                false
            ),
            Some(DecisionKind::Expired)
        );
        assert_eq!(
            decision_kind(
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
                decision_kind(&live, false),
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
    fn a_locked_answer_on_the_pin_path_closes_the_card_too() {
        // The high-risk PIN flow is the everyday origin of a lockout: the card
        // must close (its item is already denied server-side, protocol §4),
        // not linger with a cleared prompt.
        let mut m = confirming("a", true);
        m.update(Msg::Approve);
        type_pin(&mut m, "1111");
        m.update(Msg::PinSubmit);
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::Locked {
            retry_after_s: 300,
        })));
        let Phase::Watching {
            confirm, notice, ..
        } = m.phase()
        else {
            panic!("resident: back to the queue");
        };
        assert!(confirm.is_none());
        assert_eq!(
            notice.as_ref(),
            Some(&Notice::Locked {
                retry_after_s: Some(300)
            })
        );
    }

    #[test]
    fn a_vanished_item_closes_the_confirmation_with_a_note() {
        let mut m = confirming("a", false);
        m.update(Msg::Approve);
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::UnknownId)));
        let Phase::Watching {
            confirm, notice, ..
        } = m.phase()
        else {
            panic!("back to watching");
        };
        assert!(confirm.is_none());
        assert!(matches!(notice, Some(Notice::Note(_))));
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

    // ── nav-shell: the Receive view ──

    fn view_of(m: &Model) -> View {
        let Phase::Watching { view, .. } = m.phase() else {
            panic!("watching");
        };
        *view
    }

    #[test]
    fn the_view_switches_to_receive_and_back() {
        let mut m = watching(vec![summary("a")]);
        assert_eq!(view_of(&m), View::Queue, "the queue is the home view");
        assert!(m.update(Msg::View(View::Receive)).is_none());
        assert_eq!(view_of(&m), View::Receive);
        assert!(m.update(Msg::View(View::Queue)).is_none());
        assert_eq!(view_of(&m), View::Queue);
    }

    #[test]
    fn a_view_switch_is_refused_while_a_card_is_open() {
        // An open card is the decision surface: it is left by deciding,
        // never by a tab (AGENTS.md #5).
        let mut m = confirming("a", false);
        m.update(Msg::View(View::Receive));
        assert_eq!(view_of(&m), View::Queue, "the card pins the queue view");
    }

    #[test]
    fn a_view_switch_is_refused_while_a_get_is_on_the_wire() {
        // The card is about to open — switching now would open it behind the
        // Receive screen.
        let mut m = watching(vec![summary("a")]);
        assert!(matches!(
            m.update(Msg::Open),
            Some(transport::Request::Get(_))
        ));
        m.update(Msg::View(View::Receive));
        assert_eq!(view_of(&m), View::Queue, "awaiting_card pins the queue");
    }

    #[test]
    fn a_view_switch_is_refused_while_a_get_is_parked() {
        // The Gate-1 finding, step by step: Enter lands while the list poll
        // is in flight, so the get is PARKED (awaiting_card deliberately not
        // raised until flush_pending sends it). The switch must see the
        // parked get too — and the card must open on the Queue view, never
        // behind Receive.
        let mut m = watching(vec![summary("a")]);
        assert!(matches!(
            m.update(Msg::Tick),
            Some(transport::Request::List)
        )); // a poll is on the wire
        assert!(m.update(Msg::Open).is_none(), "the get parks behind it");
        m.update(Msg::View(View::Receive));
        assert_eq!(view_of(&m), View::Queue, "a parked get pins the queue");
        // The poll answers; the parked get goes out; the card opens — in Queue.
        assert!(matches!(
            m.update(Msg::Reply(Reply::List(vec![summary("a")]))),
            Some(transport::Request::Get(_))
        ));
        m.update(Msg::Reply(Reply::Get(GetOutcome::Card(card("a")))));
        assert!(
            matches!(
                m.phase(),
                Phase::Watching {
                    confirm: Some(_),
                    view: View::Queue,
                    ..
                }
            ),
            "the card opened on the queue view, not behind Receive"
        );
    }

    #[test]
    fn open_is_dead_on_the_receive_view() {
        // The key map never sends Open from Receive, but the model is the
        // security boundary, not the key map (/check-3).
        let mut m = watching(vec![summary("a")]);
        m.update(Msg::View(View::Receive));
        assert!(m.update(Msg::Open).is_none(), "no get is solicited");
        // And nothing was left armed: returning to the queue, the next open
        // works normally.
        m.update(Msg::View(View::Queue));
        assert!(matches!(
            m.update(Msg::Open),
            Some(transport::Request::Get(_))
        ));
    }

    #[test]
    fn the_receive_view_keeps_polling_the_queue() {
        // The resident console stays live on the queue whichever view is on
        // top: the tab bar's pending count follows reality.
        let mut m = watching(vec![]);
        m.update(Msg::View(View::Receive));
        assert!(matches!(
            m.update(Msg::Tick),
            Some(transport::Request::List)
        ));
        m.update(Msg::Reply(Reply::List(vec![summary("a")])));
        let Phase::Watching { items, .. } = m.phase() else {
            panic!("watching");
        };
        assert_eq!(items.len(), 1, "the list lands while Receive is on top");
        assert_eq!(view_of(&m), View::Receive, "and the view stays put");
    }

    #[test]
    fn a_view_switch_outside_the_watch_phase_is_ignored() {
        let mut m = Model::new();
        assert!(m.update(Msg::View(View::Receive)).is_none());
        assert!(matches!(m.phase(), Phase::Connecting));
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

    // ── Stage 5: the Dashboard + the read-op scheduler (list-first) ──

    /// Drive a fresh session to the watch phase WITHOUT leaving the home
    /// view — the dashboard scheduler flows live here.
    fn on_dashboard() -> Model {
        let mut m = Model::new();
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
            Some(transport::Request::Context)
        ));
        m.update(Msg::Reply(Reply::Context(ContextOutcome::Ok(Box::new(
            WalletContext {
                address: WALLET.to_owned(),
                balances: vec![],
                allowed_chains: vec![1],
            },
        )))));
        m
    }

    /// One poll round: tick (must yield `List` — the tick belongs to list)
    /// and the reply; returns what the model wants to send AFTER the reply.
    fn poll_round(m: &mut Model) -> Option<transport::Request> {
        assert!(
            matches!(m.update(Msg::Tick), Some(transport::Request::List)),
            "a tick with a free slot always originates a List, never a read-op"
        );
        m.update(Msg::Reply(Reply::List(vec![])))
    }

    #[test]
    fn the_home_view_after_auth_is_the_dashboard() {
        let m = on_dashboard();
        assert_eq!(view_of(&m), View::Dashboard, "PIN-unlock lands on the pult");
    }

    #[test]
    fn the_first_list_reply_on_the_dashboard_dispatches_positions() {
        // Born stale: the human sees positions right away, not ~30 s later.
        let mut m = on_dashboard();
        assert!(
            matches!(poll_round(&mut m), Some(transport::Request::Positions)),
            "the first list reply hands the slot to the first positions read"
        );
    }

    #[test]
    fn read_ops_wait_out_the_staleness_window() {
        let mut m = on_dashboard();
        assert!(matches!(
            poll_round(&mut m),
            Some(transport::Request::Positions)
        ));
        m.update(Msg::Reply(Reply::Positions(PositionsOutcome::Ok(vec![]))));
        // Fresh data: the following rounds are pure list polling…
        for _ in 0..(STALE_TICKS - 1) {
            assert!(
                poll_round(&mut m).is_none(),
                "no read-op before the staleness window elapses"
            );
        }
        // …until the window elapses — then the ALTERNATE op goes out.
        assert!(
            matches!(poll_round(&mut m), Some(transport::Request::Context)),
            "context (balance refresh) alternates with positions"
        );
    }

    #[test]
    fn a_read_op_is_refused_off_the_dashboard() {
        let mut m = on_dashboard();
        m.update(Msg::View(View::Queue));
        assert!(
            poll_round(&mut m).is_none(),
            "stale data does not fetch while the dashboard is not on top"
        );
    }

    #[test]
    fn a_parked_user_intent_outranks_everything_after_a_list_reply() {
        // Pins ONLY that a parked get goes out right after the list reply on
        // the queue view. It does NOT witness the flush-vs-read-op order:
        // here dispatch_read_op is already cut by its view check, and the
        // state "parked intent + active dashboard" is unreachable through
        // the public API (Gate-2 МИНОР — the dispatcher's own guard is
        // pinned separately in `the_dispatcher_itself_refuses_a_parked_intent`).
        let mut m = on_dashboard();
        m.update(Msg::View(View::Queue));
        assert!(matches!(
            m.update(Msg::Tick),
            Some(transport::Request::List)
        ));
        m.update(Msg::Reply(Reply::List(vec![summary("a")])));
        assert!(matches!(
            m.update(Msg::Tick),
            Some(transport::Request::List)
        ));
        assert!(
            m.update(Msg::Open).is_none(),
            "the get parks behind the poll"
        );
        assert!(
            matches!(
                m.update(Msg::Reply(Reply::List(vec![summary("a")]))),
                Some(transport::Request::Get(_))
            ),
            "after the list reply the parked get goes out — nothing else"
        );
    }

    #[test]
    fn entering_the_dashboard_marks_the_data_stale() {
        let mut m = on_dashboard();
        assert!(matches!(
            poll_round(&mut m),
            Some(transport::Request::Positions)
        ));
        m.update(Msg::Reply(Reply::Positions(PositionsOutcome::Ok(vec![]))));
        assert!(poll_round(&mut m).is_none(), "fresh right after the reply");
        // Leave and come back: the re-entry marks the read data stale.
        m.update(Msg::View(View::Receive));
        m.update(Msg::View(View::Dashboard));
        assert!(
            poll_round(&mut m).is_some(),
            "re-entering the dashboard refreshes at the next list reply"
        );
    }

    #[test]
    fn a_positions_reply_feeds_the_tristate() {
        let mut m = on_dashboard();
        assert!(matches!(m.positions(), Positions::NotYet));
        assert!(matches!(
            poll_round(&mut m),
            Some(transport::Request::Positions)
        ));
        m.update(Msg::Reply(Reply::Positions(PositionsOutcome::WalletLocked)));
        assert!(
            matches!(m.positions(), Positions::Unavailable),
            "wallet_locked degrades, distinct from an empty list"
        );
    }

    #[test]
    fn a_failed_balance_refresh_keeps_the_data_and_flags_it() {
        let mut m = on_dashboard();
        assert!(m.wallet_context().is_some());
        assert!(!m.context_stale());
        // A later refresh answers wallet_locked: the data survives, flagged.
        m.update(Msg::Reply(Reply::Context(ContextOutcome::WalletLocked)));
        assert!(
            m.wallet_context().is_some(),
            "old data is kept — the card's From→To keeps working"
        );
        assert!(m.context_stale(), "…but the dashboard says it may be stale");
        // A successful refresh clears the flag.
        m.update(Msg::Reply(Reply::Context(ContextOutcome::Ok(Box::new(
            WalletContext {
                address: WALLET.to_owned(),
                balances: vec![],
                allowed_chains: vec![1],
            },
        )))));
        assert!(!m.context_stale());
    }

    #[test]
    fn the_dispatcher_itself_refuses_a_parked_intent() {
        // "Parked intent + active dashboard" is unreachable through the
        // public API today (the switch gate refuses on a parked get) — build
        // the state by hand to pin dispatch_read_op's OWN guard, so a future
        // view that can park while the dashboard is up inherits the refusal
        // (Gate-2 МИНОР: the model is the boundary, not the reachability of
        // today's key map).
        let mut m = on_dashboard();
        m.pending = Some(PendingIntent::Get("x".to_owned()));
        assert!(
            m.dispatch_read_op().is_none(),
            "a parked human intent starves the read-op, never the reverse"
        );
        m.pending = None;
        assert!(
            m.dispatch_read_op().is_some(),
            "…and with the intent gone the read-op flows"
        );
    }

    #[test]
    fn a_dashboard_switch_is_refused_while_a_get_is_parked() {
        // The direct twin of the Receive-era test, for the third view.
        let mut m = on_dashboard();
        m.update(Msg::View(View::Queue));
        m.update(Msg::Tick);
        m.update(Msg::Reply(Reply::List(vec![summary("a")])));
        assert!(matches!(
            m.update(Msg::Tick),
            Some(transport::Request::List)
        ));
        assert!(m.update(Msg::Open).is_none(), "the get parks");
        m.update(Msg::View(View::Dashboard));
        assert_eq!(view_of(&m), View::Queue, "a parked get pins the queue");
    }

    #[test]
    fn the_dashboard_switch_obeys_the_same_triple_gate() {
        // The switch gate is one code path for every view — pin Dashboard to
        // it explicitly: an open card refuses the tab.
        let mut m = confirming("a", false);
        m.update(Msg::View(View::Dashboard));
        assert_eq!(view_of(&m), View::Queue, "the card pins the queue view");
    }

    // ── Stage 7: the Activity view, its scheduler slot, the history model ──

    fn history_entry(id: &str, unix: u64, state: OutcomeState) -> HistoryEntry {
        HistoryEntry {
            unix,
            id: id.to_owned(),
            state,
            to: None,
            amount_wei: None,
            chain_id: None,
            tx_hash: None,
            reason: None,
        }
    }

    #[test]
    fn the_activity_view_dispatches_the_activity_read_op() {
        let mut m = on_dashboard();
        m.update(Msg::View(View::Activity));
        assert!(
            matches!(poll_round(&mut m), Some(transport::Request::Activity)),
            "on the Activity view the list-first slot carries the activity op"
        );
    }

    #[test]
    fn the_tick_belongs_to_list_on_the_activity_view_too() {
        let mut m = on_dashboard();
        m.update(Msg::View(View::Activity));
        assert!(
            matches!(m.update(Msg::Tick), Some(transport::Request::List)),
            "a tick never births a read-op — the activity op waits for the list reply"
        );
    }

    #[test]
    fn entering_activity_marks_the_read_data_stale() {
        let mut m = on_dashboard();
        // Consume the born-stale dispatch and hand its answer back: fresh data.
        assert!(matches!(
            poll_round(&mut m),
            Some(transport::Request::Positions)
        ));
        m.update(Msg::Reply(Reply::Positions(PositionsOutcome::Ok(vec![]))));
        assert!(poll_round(&mut m).is_none(), "fresh data: no read-op due");
        m.update(Msg::View(View::Activity));
        assert!(
            matches!(poll_round(&mut m), Some(transport::Request::Activity)),
            "entering the Activity view marks the data stale — the next list reply fetches"
        );
    }

    #[test]
    fn activity_never_joins_the_dashboard_alternation() {
        let mut m = on_dashboard();
        assert!(matches!(
            poll_round(&mut m),
            Some(transport::Request::Positions)
        ));
        m.update(Msg::Reply(Reply::Positions(PositionsOutcome::Ok(vec![]))));
        // Visit Activity: its op rides the slot without touching next_read.
        m.update(Msg::View(View::Activity));
        assert!(matches!(
            poll_round(&mut m),
            Some(transport::Request::Activity)
        ));
        m.update(Msg::Reply(Reply::Activity(vec![])));
        // Back on the Dashboard the alternation resumes exactly where it left.
        m.update(Msg::View(View::Dashboard));
        assert!(
            matches!(poll_round(&mut m), Some(transport::Request::Context)),
            "the two-valued alternation is untouched by the activity slot"
        );
    }

    #[test]
    fn a_decision_here_births_a_rich_history_record() {
        let mut m = watching(vec![summary("a1")]);
        assert!(matches!(
            m.update(Msg::Open),
            Some(transport::Request::Get(_))
        ));
        m.update(Msg::Reply(Reply::Get(GetOutcome::Card(card("a1")))));
        assert!(matches!(
            m.update(Msg::Approve),
            Some(transport::Request::Approve(_))
        ));
        m.update(Msg::Reply(Reply::Resolve(ResolveOutcome::Executed {
            tx_hash: "0xfeed".to_owned(),
        })));

        let d = m
            .take_decided_outcome()
            .expect("a rich record is born at decision time, while the card is open");
        assert_eq!(d.id, "a1");
        assert_eq!(d.state, OutcomeState::Executed);
        assert_eq!(d.to, "0xabc", "the card's recipient, verbatim");
        assert_eq!(d.amount_wei, "0", "the card's amount");
        assert_eq!(d.chain_id, 1);
        assert_eq!(d.tx_hash.as_deref(), Some("0xfeed"));
        assert_eq!(d.reason, None);
        assert!(m.take_decided_outcome().is_none(), "drained exactly once");
        // The ADR-#7 decision-line drain is a separate, untouched channel.
        assert!(m.take_decision().is_some());
    }

    #[test]
    fn server_outcomes_queue_for_the_run_loop_and_drain_once() {
        let mut m = on_dashboard();
        m.update(Msg::Reply(Reply::Activity(vec![OutcomeEntry {
            id: "s1".to_owned(),
            state: OutcomeState::Denied,
            tx_hash: None,
            reason: None,
            age_secs: 42,
        }])));
        let drained = m.take_server_outcomes();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, "s1");
        assert!(
            m.take_server_outcomes().is_empty(),
            "the queue drains exactly once"
        );
    }

    #[test]
    fn history_sorts_newest_first_ties_by_id_and_caps() {
        let mut m = Model::new();
        m.set_history(vec![
            history_entry("b", 100, OutcomeState::Denied),
            history_entry("c", 200, OutcomeState::Executed),
            history_entry("a", 100, OutcomeState::Denied),
        ]);
        let ids: Vec<&str> = m.history().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["c", "a", "b"],
            "newest first; a shared second falls back to id order"
        );

        let many: Vec<HistoryEntry> = (0..=HISTORY_CAP)
            .map(|i| history_entry(&format!("id{i:04}"), i as u64, OutcomeState::Denied))
            .collect();
        m.set_history(many);
        assert_eq!(
            m.history().len(),
            HISTORY_CAP,
            "capped at the display limit"
        );
        assert!(
            m.history().iter().all(|e| e.unix >= 1),
            "what fell off the cap is exactly the oldest entry"
        );
    }

    #[test]
    fn two_rich_decisions_in_one_second_order_by_id_not_chronology() {
        // Gate-1 finding, documented on purpose: unix is whole seconds — two
        // decisions inside one second CANNOT be ordered by time here; the id
        // tiebreak (asc) makes the order deterministic, not chronological.
        let mut ids = [uuid_like("f"), uuid_like("0")];
        ids.sort();
        let (small, big) = (ids[0].clone(), ids[1].clone());
        let mut m = Model::new();
        // Decided big-id first, small-id second — same second.
        m.push_history(history_entry(&big, 77, OutcomeState::Denied));
        m.push_history(history_entry(&small, 77, OutcomeState::Denied));
        let got: Vec<&str> = m.history().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            got,
            vec![small.as_str(), big.as_str()],
            "id order wins inside one second — regardless of decision order"
        );
    }

    fn uuid_like(prefix: &str) -> String {
        format!("{prefix}1b2c3d-0000-0000-0000-000000000000")
    }

    #[test]
    fn fill_history_detail_fills_only_the_missing_fields() {
        let mut m = Model::new();
        let mut rich = history_entry("a", 10, OutcomeState::Executed);
        rich.to = Some("0xTO".to_owned());
        m.set_history(vec![rich]);

        m.fill_history_detail(
            "a",
            Some("0xEVIL"),
            Some("5"),
            Some(1),
            Some("0xfeed"),
            None,
        );
        let e = &m.history()[0];
        assert_eq!(
            e.to.as_deref(),
            Some("0xTO"),
            "a present field is never overwritten"
        );
        assert_eq!(e.amount_wei.as_deref(), Some("5"), "a missing field fills");
        assert_eq!(e.chain_id, Some(1));
        assert_eq!(e.tx_hash.as_deref(), Some("0xfeed"));
        assert_eq!(e.reason, None);
    }

    #[test]
    fn the_filter_cycles_only_on_the_activity_view() {
        let mut m = on_dashboard();
        m.update(Msg::Filter);
        assert_eq!(
            m.history_filter(),
            HistoryFilter::All,
            "no cycling off the Activity view — the model is the boundary"
        );
        m.update(Msg::View(View::Activity));
        m.update(Msg::Filter);
        assert_eq!(m.history_filter(), HistoryFilter::Executed);
        for _ in 0..4 {
            m.update(Msg::Filter);
        }
        assert_eq!(
            m.history_filter(),
            HistoryFilter::All,
            "the cycle closes back on All"
        );
    }
}
