//! Wire protocol types + codec — a faithful mirror of `docs/APPROVER-PROTOCOL.md`.
//!
//! This layer is **pure**: `encode_request` and the `parse_*` functions do no I/O,
//! so they are unit-tested directly (the socket worker thread is a separate layer).
//!
//! Numeric wire strings (`amount_wei`, a decoded `amount`, `tx_hash`) are kept as
//! `String` — the console renders the core's values **verbatim** (`AGENTS.md` #1)
//! and never re-derives meaning. A display may re-base a value for reading, but the
//! type carries the ground truth exactly as received; a decoded `amount` in
//! particular is a `0x`-hex string, not an integer, so a truncating parse cannot
//! silently mis-state an unlimited approval.
//!
//! Unknown fields are ignored (not `deny_unknown_fields`): additive fields are
//! allowed within a major version (protocol §6).

use serde::{Deserialize, Serialize};

/// Wire protocol major version this client speaks. Proto 2 adds the auth-gated
/// `context` read-op (protocol §3.7) — the source of the wallet's own address
/// for the card's From→To block. There is deliberately no fallback to proto 1
/// against an older server: the wallet image ships core and console as a pair,
/// so a mismatch means a hand-built setup — the honest answer is the upgrade
/// hint, not a silently poorer card (Gate-1 ratification, 2026-07-12).
pub const PROTO_VERSION: u32 = 2;

// ─────────────────────────── Requests (client → server) ───────────────────────────

/// A request line the client sends. Serializes to one JSON object, e.g.
/// `{"op":"list"}`. **`auth` is intentionally absent here** — it carries the PIN,
/// whose serialized form must live in a `Zeroizing` buffer, so it is built in the
/// transport layer rather than through this general `Serialize` path (which would
/// leave an un-zeroized `String` copy of the PIN).
#[derive(Debug, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request<'a> {
    /// Version handshake; must be the first line on a connection.
    Hello {
        /// Major protocol version ([`PROTO_VERSION`]).
        proto: u32,
        /// Informational client id (the server does not validate it).
        client: &'a str,
    },
    /// Ask for the pending/executing queue summaries.
    List,
    /// Ask for the full clear-signing card of one item.
    Get {
        /// The item's preview-uuid, as received in a summary.
        id: &'a str,
    },
    /// Approve an item — the core signs and broadcasts. This is the normal path;
    /// a **high-risk** item needs a per-request PIN, which is built separately in
    /// the transport layer so the PIN stays in a `Zeroizing` buffer (never through
    /// this general `Serialize` path).
    Approve {
        /// The item's preview-uuid.
        id: &'a str,
    },
    /// Deny an item — cheap, no PIN beyond the session `auth`.
    Deny {
        /// The item's preview-uuid.
        id: &'a str,
    },
    /// Ask for the wallet's own address / balances / allowed chains
    /// (proto 2+, auth-gated — protocol §3.7). Sent once after `auth`; the
    /// address feeds the card's From→To block.
    Context,
    /// Ask for the wallet's own DeFi positions (proto 2+, auth-gated —
    /// protocol §3.8). Dispatched by the read-op scheduler only right after
    /// a `list` reply, never ahead of one (the §3.8 client rule).
    Positions,
    /// Ask for the recent terminal outcomes (proto 2+, auth-gated — protocol
    /// §3.9): newest first, server-capped at 100. Same scheduler discipline
    /// as [`Request::Positions`] — only right after a `list` reply.
    Activity,
}

/// Serialize a request to a single JSON line (no trailing `\n`; the transport adds
/// it).
///
/// # Errors
/// [`ProtocolError::Encode`] if serialization fails — not expected for these
/// shapes, but the seam is kept rather than panicking in a library path.
pub fn encode_request(req: &Request<'_>) -> Result<String, ProtocolError> {
    serde_json::to_string(req).map_err(|e| ProtocolError::Encode(e.to_string()))
}

// ─────────────────────────── Domain types (card / summary) ───────────────────────────

/// A pending item's kind, from a `list` summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A bare native transfer (no calldata).
    Send,
    /// A contract call (has calldata).
    Call,
}

/// The txguard risk level — two-valued, mirroring the core `RiskLevel` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// No txguard warning.
    Safe,
    /// txguard flagged a warning.
    Warning,
}

/// One `list` summary line. `to` is EIP-55 checksummed and `amount_wei` is a
/// decimal string (both top-level, via the core's `Display`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Summary {
    /// Preview-uuid (hyphenated lowercase).
    pub id: String,
    /// Send vs call.
    pub kind: Kind,
    /// EVM chain id.
    pub chain_id: u64,
    /// Recipient / target, EIP-55 checksummed `0x`-hex.
    pub to: String,
    /// Native value, **decimal** wei string.
    pub amount_wei: String,
    /// txguard risk level.
    pub risk: Risk,
    /// Whether approving needs a per-request PIN.
    pub high_risk: bool,
    /// Absolute expiry, unix seconds.
    pub not_after_unix: u64,
}

/// The core's decode of a recognised drain-vector call. **Every field is optional**
/// — an absent field is `null`, not a misleading zero. Addresses here are
/// serde-encoded **lowercase** `0x`-hex (unlike the top-level checksummed `to`),
/// and `amount`/`deadline` are `0x`-hex strings (unlike the decimal `amount_wei`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DecodedCall {
    /// `approve` | `transfer` | `transfer_from` | `set_approval_for_all` |
    /// `permit` | `permit2_approve` | `increase_allowance`.
    pub method: String,
    /// Authorized spender (`approve`/`permit`/`permit2_approve`).
    pub spender: Option<String>,
    /// Operator (`set_approval_for_all`).
    pub operator: Option<String>,
    /// Source (`transfer_from`) / owner (`permit`).
    pub from: Option<String>,
    /// Recipient (`transfer`/`transfer_from`).
    pub to: Option<String>,
    /// Approved token (`permit2_approve`; the tx `to` is the Permit2 contract).
    pub token: Option<String>,
    /// Raw token amount, `0x`-hex string (kept as text — bignum-safe, verbatim).
    pub amount: Option<String>,
    /// `permit` deadline / Permit2 expiration (unix), `0x`-hex string.
    pub deadline: Option<String>,
    /// `set_approval_for_all`: `true` = grant, `false` = revoke.
    pub approved: Option<bool>,
    /// `amount == U256::MAX` — an infinite (unlimited) approval.
    pub is_unlimited: Option<bool>,
}

/// The full clear-signing card for one item (`get`). `decoded_call` may be `null`
/// (a bare transfer or an unrecognised selector) — render from `to` / `amount_wei`
/// / `raw_data` in that case, do not assume an object.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Card {
    /// Preview-uuid.
    pub id: String,
    /// EVM chain id.
    pub chain_id: u64,
    /// Recipient / target, EIP-55 checksummed.
    pub to: String,
    /// Native value, decimal wei string.
    pub amount_wei: String,
    /// The core's decode, or `null`.
    pub decoded_call: Option<DecodedCall>,
    /// Whether approving needs a per-request PIN.
    pub high_risk: bool,
    /// Closed set: `unlimited_approval` and/or `txguard_warning`.
    pub high_risk_reasons: Vec<String>,
    /// Exact call input as `0x`-lowercase-hex; `"0x"` if empty.
    pub raw_data: String,
    /// Absolute expiry, unix seconds.
    pub not_after_unix: u64,
}

// ─────────────────────────── Responses (server → client) ───────────────────────────

/// Outcome of the `hello` handshake.
#[derive(Debug, PartialEq, Eq)]
pub enum HelloOutcome {
    /// Handshake accepted; carries the informational server id.
    Ok {
        /// e.g. `"core-server/0.1.0"` — informational, never a compat gate.
        server: String,
    },
    /// Major version mismatch — fatal; the client must upgrade. Carries the
    /// versions the server supports.
    Unsupported {
        /// Protocol majors the server accepts.
        supported: Vec<u32>,
    },
}

/// Outcome of `auth`.
#[derive(Debug, PartialEq, Eq)]
pub enum AuthOutcome {
    /// Session authorized.
    Ok,
    /// Wrong PIN; `attempts_left == 0` means the lockout is now armed.
    BadPin {
        /// Attempts before the lockout trips.
        attempts_left: u32,
    },
    /// Lockout active; retry after this many seconds.
    Locked {
        /// Seconds until the channel accepts a PIN again.
        retry_after_s: u64,
    },
    /// The wallet has no PIN record (created before the PIN era).
    PinNotSet,
    /// Transient Argon2 backend failure — never an accept.
    PinUnavailable,
}

/// Outcome of `get`.
#[derive(Debug, PartialEq, Eq)]
pub enum GetOutcome {
    /// The card.
    Card(Box<Card>),
    /// The id is not a live item (resolved, expired+swept, or never known).
    UnknownId,
}

/// One chain's native balance from `context` (protocol §3.7). `balance` is a
/// **decimal** wei string (same convention as `amount_wei`), kept verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ChainBalance {
    /// EVM chain id.
    pub chain_id: u64,
    /// Native token symbol (`"ETH"` for every chain in the allowed set).
    pub symbol: String,
    /// Native balance, decimal wei string.
    pub balance: String,
}

/// The wallet's own context from a successful `context` reply (protocol §3.7).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WalletContext {
    /// The wallet's (signer's) address, EIP-55 checksummed — same convention
    /// as the card's top-level `to`, so From→To renders both verbatim.
    pub address: String,
    /// Per-chain native balances; a chain whose provider was unreachable is
    /// omitted, not zeroed (best-effort, §3.7).
    pub balances: Vec<ChainBalance>,
    /// The server's configured chain allow-list, in order.
    pub allowed_chains: Vec<u64>,
}

/// Outcome of `context`. Both non-`Ok` variants degrade the UI (the card falls
/// back to its To-only layout) — they never gate approve: the From block is
/// display-only, the signing-critical surface (`to`/amount/decode) does not
/// depend on it.
#[derive(Debug, PartialEq, Eq)]
pub enum ContextOutcome {
    /// The wallet's context.
    Ok(Box<WalletContext>),
    /// The core's own keyring isn't unlocked (distinct from PIN auth, §3.9).
    WalletLocked,
}

/// One DeFi position from `positions` (protocol §3.8), kept **verbatim** — the
/// dashboard renders these strings and never parses them: `extra` values are
/// display strings by canon (`health_factor` may be the literal `"∞"`, `ltv`
/// carries a trailing `%`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Position {
    /// Protocol wire form: `"aave_v3"` | `"erc4626"`. Kept as a string — an
    /// unknown future protocol renders as-is instead of failing the parse.
    pub protocol: String,
    /// Chain the position lives on.
    pub chain_id: u64,
    /// Asset address (EIP-55) — the Aave Pool contract or the vault's
    /// underlying token. Not rendered on the dashboard (Gate-1 decision №4);
    /// carried so the client type mirrors §3.8 whole.
    pub asset_address: String,
    /// Human-readable asset symbol (`"USD"` for the Aave account).
    pub asset_symbol: String,
    /// Human-readable asset name.
    pub asset_name: String,
    /// Decimal places `balance` is denominated in.
    pub asset_decimals: u8,
    /// Raw integer balance, decimal string (no point).
    pub balance: String,
    /// `balance` at `asset_decimals` places, trailing zeros trimmed.
    pub balance_formatted: String,
    /// Per-protocol extras — display strings, keys sorted (§3.8).
    #[serde(default)]
    pub extra: std::collections::BTreeMap<String, String>,
}

/// Outcome of `positions` (§3.8). `WalletLocked` degrades the dashboard's
/// positions block only — it never gates anything.
#[derive(Debug, PartialEq, Eq)]
pub enum PositionsOutcome {
    /// The wallet's positions — an empty list is a valid answer (best-effort:
    /// no positions, or every source skipped on RPC failure; §3.8).
    Ok(Vec<Position>),
    /// The core's own keyring isn't unlocked (§3.11).
    WalletLocked,
}

/// The four terminal words an `activity` outcome may carry (protocol §3.9).
/// Deliberately NOT [`TerminalState`]: that enum includes `Pending` for
/// `already_resolved` (§3.5), which §3.9 promises never to send — a reply
/// carrying `"pending"` here must FAIL the parse (default-deny), not slip
/// through. `Serialize` is derived so the console's local activity log stores
/// exactly these protocol words (one vocabulary end to end).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeState {
    /// Signed and broadcast.
    Executed,
    /// Rejected by the human.
    Denied,
    /// Expired before a decision.
    Expired,
    /// Approved, but signing/broadcast failed.
    Failed,
}

/// One retained terminal outcome from `activity` (protocol §3.9), verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OutcomeEntry {
    /// The resolved item's preview id — stable across polls; the dedup key
    /// for the console's local log (§3.9).
    pub id: String,
    /// Terminal state word (§3.9 — never `"pending"`; see [`OutcomeState`]).
    pub state: OutcomeState,
    /// The executed transaction's hash (`0x…`) — executed only; on every
    /// other state the field is absent on the wire (§3.9) and `None` here.
    #[serde(default)]
    pub tx_hash: Option<String>,
    /// Operator-masked failure reason — failed only; absent otherwise.
    #[serde(default)]
    pub reason: Option<String>,
    /// Seconds since the resolution — a relative age, not a timestamp; the
    /// console derives an absolute time locally at arrival (§3.9).
    pub age_secs: u64,
}

/// The terminal state carried by an `already_resolved` reply (protocol §3.5).
/// Includes `Pending` (I4): another connection is executing this id right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    /// Signed and broadcast.
    Executed,
    /// Rejected by the human.
    Denied,
    /// Expired before a decision.
    Expired,
    /// Approved, but signing/broadcast failed.
    Failed,
    /// Another connection is executing this id right now (retry / wait).
    Pending,
}

/// Outcome of `approve` or `deny`. Note there is **no** `expired` error code:
/// an item that expired resolves to `AlreadyResolved { Expired }` (or `UnknownId`
/// after retention), never a top-level `expired`.
#[derive(Debug, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// Approved: signed and broadcast; carries the tx hash.
    Executed {
        /// `0x`-hex transaction hash.
        tx_hash: String,
    },
    /// Approved, but signing/broadcast failed (still resolved — not retryable).
    Failed {
        /// Operator-masked failure reason.
        reason: String,
    },
    /// Denied.
    Denied,
    /// Already terminal (or in-flight) — carries the state.
    AlreadyResolved {
        /// The existing terminal/in-flight state.
        state: TerminalState,
    },
    /// No successful `auth` on this connection.
    Unauthorized,
    /// A high-risk item was approved without a `pin`.
    PinRequired,
    /// Wrong PIN; `attempts_left == 0` means the lockout is now armed.
    BadPin {
        /// Attempts before the lockout trips.
        attempts_left: u32,
    },
    /// Lockout active; retry after this many seconds.
    Locked {
        /// Seconds until the channel accepts a PIN again.
        retry_after_s: u64,
    },
    /// The wallet has no PIN record.
    PinNotSet,
    /// Transient Argon2 backend failure.
    PinUnavailable,
    /// The id is not a live item.
    UnknownId,
}

/// A parse or encode failure in the protocol layer.
#[derive(Debug, PartialEq, Eq)]
pub enum ProtocolError {
    /// The line was not valid JSON, or lacked a field the shape requires.
    Malformed(String),
    /// A request could not be serialized (not expected for our shapes).
    Encode(String),
    /// A server error code this response path does not model.
    Unexpected(String),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(m) => write!(f, "malformed response: {m}"),
            Self::Encode(m) => write!(f, "request encode failed: {m}"),
            Self::Unexpected(code) => write!(f, "unexpected server response: {code}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

fn parse_line<T>(line: &str) -> Result<T, ProtocolError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(line).map_err(|e| ProtocolError::Malformed(e.to_string()))
}

/// Parse a response to `hello`.
///
/// # Errors
/// [`ProtocolError::Malformed`] on non-JSON / wrong shape; [`ProtocolError::Unexpected`]
/// on an error code other than `unsupported_proto`.
pub fn parse_hello(line: &str) -> Result<HelloOutcome, ProtocolError> {
    #[derive(Deserialize)]
    struct Raw {
        ok: bool,
        server: Option<String>,
        error: Option<String>,
        supported: Option<Vec<u32>>,
    }
    let raw: Raw = parse_line(line)?;
    if raw.ok {
        Ok(HelloOutcome::Ok {
            server: raw.server.unwrap_or_default(),
        })
    } else if raw.error.as_deref() == Some("unsupported_proto") {
        Ok(HelloOutcome::Unsupported {
            supported: raw.supported.unwrap_or_default(),
        })
    } else {
        Err(ProtocolError::Unexpected(
            raw.error
                .unwrap_or_else(|| "hello without ok or error".to_owned()),
        ))
    }
}

/// Parse a response to `auth`.
///
/// # Errors
/// [`ProtocolError::Malformed`] on non-JSON / wrong shape; [`ProtocolError::Unexpected`]
/// on an unmodeled error code.
pub fn parse_auth(line: &str) -> Result<AuthOutcome, ProtocolError> {
    #[derive(Deserialize)]
    struct Raw {
        ok: bool,
        error: Option<String>,
        attempts_left: Option<u32>,
        retry_after_s: Option<u64>,
    }
    let raw: Raw = parse_line(line)?;
    if raw.ok {
        return Ok(AuthOutcome::Ok);
    }
    match raw.error.as_deref() {
        Some("bad_pin") => Ok(AuthOutcome::BadPin {
            attempts_left: raw.attempts_left.unwrap_or(0),
        }),
        Some("locked") => Ok(AuthOutcome::Locked {
            retry_after_s: raw.retry_after_s.unwrap_or(0),
        }),
        Some("pin_not_set") => Ok(AuthOutcome::PinNotSet),
        Some("pin_unavailable") => Ok(AuthOutcome::PinUnavailable),
        other => Err(ProtocolError::Unexpected(
            other.unwrap_or("auth without ok or error").to_owned(),
        )),
    }
}

/// Parse a response to `list` into the queue summaries.
///
/// # Errors
/// [`ProtocolError::Malformed`] on non-JSON / wrong shape; [`ProtocolError::Unexpected`]
/// on an error response.
pub fn parse_list(line: &str) -> Result<Vec<Summary>, ProtocolError> {
    #[derive(Deserialize)]
    struct Raw {
        ok: bool,
        pending: Option<Vec<Summary>>,
        error: Option<String>,
    }
    let raw: Raw = parse_line(line)?;
    if raw.ok {
        Ok(raw.pending.unwrap_or_default())
    } else {
        Err(ProtocolError::Unexpected(
            raw.error
                .unwrap_or_else(|| "list without ok or error".to_owned()),
        ))
    }
}

/// Parse a response to `get`.
///
/// # Errors
/// [`ProtocolError::Malformed`] on non-JSON / wrong shape / `ok` without a card;
/// [`ProtocolError::Unexpected`] on an error code other than `unknown_id`.
pub fn parse_get(line: &str) -> Result<GetOutcome, ProtocolError> {
    #[derive(Deserialize)]
    struct Raw {
        ok: bool,
        card: Option<Card>,
        error: Option<String>,
    }
    let raw: Raw = parse_line(line)?;
    if raw.ok {
        raw.card
            .map(|c| GetOutcome::Card(Box::new(c)))
            .ok_or_else(|| ProtocolError::Malformed("ok get without a card".to_owned()))
    } else if raw.error.as_deref() == Some("unknown_id") {
        Ok(GetOutcome::UnknownId)
    } else {
        Err(ProtocolError::Unexpected(
            raw.error
                .unwrap_or_else(|| "get without ok or error".to_owned()),
        ))
    }
}

/// Parse a response to `context` (proto 2+, protocol §3.7).
///
/// # Errors
/// [`ProtocolError::Malformed`] on non-JSON / wrong shape / `ok` without the
/// context fields; [`ProtocolError::Unexpected`] on an error code other than
/// `wallet_locked` — `unauthorized`/`protocol_error` here mean the channel is
/// not what we negotiated (we only send `context` post-auth on a proto-2
/// session), the same fail-closed class as an unexpected resolve code.
pub fn parse_context(line: &str) -> Result<ContextOutcome, ProtocolError> {
    #[derive(Deserialize)]
    struct Raw {
        ok: bool,
        address: Option<String>,
        balances: Option<Vec<ChainBalance>>,
        allowed_chains: Option<Vec<u64>>,
        error: Option<String>,
    }
    let raw: Raw = parse_line(line)?;
    if raw.ok {
        let address = raw
            .address
            .ok_or_else(|| ProtocolError::Malformed("ok context without address".to_owned()))?;
        Ok(ContextOutcome::Ok(Box::new(WalletContext {
            address,
            balances: raw.balances.unwrap_or_default(),
            allowed_chains: raw.allowed_chains.unwrap_or_default(),
        })))
    } else if raw.error.as_deref() == Some("wallet_locked") {
        Ok(ContextOutcome::WalletLocked)
    } else {
        Err(ProtocolError::Unexpected(raw.error.unwrap_or_else(|| {
            "context without ok or error".to_owned()
        })))
    }
}

/// Parse a `positions` reply (§3.8). Mirrors [`parse_context`]'s error
/// surface: `wallet_locked` is the one degradable answer;
/// `unauthorized`/`protocol_error` mean the channel is not what we negotiated
/// (we only send `positions` post-auth on a proto-2 session) — the same
/// fail-closed class as an unexpected resolve code.
///
/// # Errors
/// [`ProtocolError::Malformed`]/[`ProtocolError::Unexpected`] as above.
pub fn parse_positions(line: &str) -> Result<PositionsOutcome, ProtocolError> {
    #[derive(Deserialize)]
    struct Raw {
        ok: bool,
        positions: Option<Vec<Position>>,
        error: Option<String>,
    }
    let raw: Raw = parse_line(line)?;
    if raw.ok {
        let positions = raw.positions.ok_or_else(|| {
            ProtocolError::Malformed("ok positions without a positions array".to_owned())
        })?;
        Ok(PositionsOutcome::Ok(positions))
    } else if raw.error.as_deref() == Some("wallet_locked") {
        Ok(PositionsOutcome::WalletLocked)
    } else {
        Err(ProtocolError::Unexpected(raw.error.unwrap_or_else(|| {
            "positions without ok or error".to_owned()
        })))
    }
}

/// Parse an `activity` reply (§3.9). Unlike `context`/`positions` there is NO
/// degradable answer: the op reads only the outcome store — `wallet_locked`
/// is not in its vocabulary (§3.9). We only send `activity` post-auth on a
/// proto-2 session, so `unauthorized`/`protocol_error` (or any other code)
/// mean the channel is not what we negotiated — the same fail-closed class
/// as an unexpected resolve code.
///
/// # Errors
/// [`ProtocolError::Malformed`] on a wrong shape (including a `"pending"`
/// state — see [`OutcomeState`]); [`ProtocolError::Unexpected`] on `ok:false`.
pub fn parse_activity(line: &str) -> Result<Vec<OutcomeEntry>, ProtocolError> {
    #[derive(Deserialize)]
    struct Raw {
        ok: bool,
        outcomes: Option<Vec<OutcomeEntry>>,
        error: Option<String>,
    }
    let raw: Raw = parse_line(line)?;
    if raw.ok {
        raw.outcomes.ok_or_else(|| {
            ProtocolError::Malformed("ok activity without an outcomes array".to_owned())
        })
    } else {
        Err(ProtocolError::Unexpected(raw.error.unwrap_or_else(|| {
            "activity without ok or error".to_owned()
        })))
    }
}

/// The fields an `approve` / `deny` reply may carry. `state` means the outcome
/// (`executed`/`failed`/`denied`) on an `ok` reply, or the `already_resolved`
/// state on an error reply.
#[derive(Deserialize)]
struct ResolveRaw {
    ok: bool,
    state: Option<String>,
    tx_hash: Option<String>,
    reason: Option<String>,
    error: Option<String>,
    attempts_left: Option<u32>,
    retry_after_s: Option<u64>,
}

/// Map an error reply (shared by `approve` and `deny`) to a [`ResolveOutcome`].
fn resolve_error(raw: &ResolveRaw) -> Result<ResolveOutcome, ProtocolError> {
    match raw.error.as_deref() {
        Some("unauthorized") => Ok(ResolveOutcome::Unauthorized),
        Some("pin_required") => Ok(ResolveOutcome::PinRequired),
        Some("bad_pin") => Ok(ResolveOutcome::BadPin {
            attempts_left: raw.attempts_left.unwrap_or(0),
        }),
        Some("locked") => Ok(ResolveOutcome::Locked {
            retry_after_s: raw.retry_after_s.unwrap_or(0),
        }),
        Some("pin_not_set") => Ok(ResolveOutcome::PinNotSet),
        Some("pin_unavailable") => Ok(ResolveOutcome::PinUnavailable),
        Some("unknown_id") => Ok(ResolveOutcome::UnknownId),
        Some("already_resolved") => Ok(ResolveOutcome::AlreadyResolved {
            state: parse_terminal_state(raw.state.as_deref())?,
        }),
        other => Err(ProtocolError::Unexpected(
            other.unwrap_or("resolve without ok or error").to_owned(),
        )),
    }
}

fn parse_terminal_state(s: Option<&str>) -> Result<TerminalState, ProtocolError> {
    match s {
        Some("executed") => Ok(TerminalState::Executed),
        Some("denied") => Ok(TerminalState::Denied),
        Some("expired") => Ok(TerminalState::Expired),
        Some("pending") => Ok(TerminalState::Pending),
        Some("failed") => Ok(TerminalState::Failed),
        other => Err(ProtocolError::Unexpected(format!(
            "already_resolved with unknown state {other:?}"
        ))),
    }
}

/// Parse a response to `approve`.
///
/// # Errors
/// [`ProtocolError::Malformed`] on non-JSON; [`ProtocolError::Unexpected`] on an
/// `ok` reply with an unexpected `state`, or an unmodeled error code.
pub fn parse_approve(line: &str) -> Result<ResolveOutcome, ProtocolError> {
    let raw: ResolveRaw = parse_line(line)?;
    if raw.ok {
        match raw.state.as_deref() {
            Some("executed") => Ok(ResolveOutcome::Executed {
                tx_hash: raw.tx_hash.unwrap_or_default(),
            }),
            Some("failed") => Ok(ResolveOutcome::Failed {
                reason: raw.reason.unwrap_or_default(),
            }),
            other => Err(ProtocolError::Unexpected(format!(
                "approve ok with unexpected state {other:?}"
            ))),
        }
    } else {
        resolve_error(&raw)
    }
}

/// Parse a response to `deny`.
///
/// # Errors
/// [`ProtocolError::Malformed`] on non-JSON; [`ProtocolError::Unexpected`] on an
/// `ok` reply that is not `denied`, or any error outside `deny`'s documented
/// surface (§3.6): `unauthorized` / `unknown_id` / `already_resolved`. The PIN
/// family in particular is refused — "deny never requires a PIN beyond session
/// auth" — because accepted, it would open the PIN prompt over a rejection and
/// the prompt can only build an approve line.
pub fn parse_deny(line: &str) -> Result<ResolveOutcome, ProtocolError> {
    let raw: ResolveRaw = parse_line(line)?;
    if raw.ok {
        match raw.state.as_deref() {
            Some("denied") => Ok(ResolveOutcome::Denied),
            other => Err(ProtocolError::Unexpected(format!(
                "deny ok with unexpected state {other:?}"
            ))),
        }
    } else {
        match raw.error.as_deref() {
            Some("unauthorized" | "unknown_id" | "already_resolved") => resolve_error(&raw),
            other => Err(ProtocolError::Unexpected(format!(
                "deny answered with error {other:?} — its only errors are \
                 unauthorized/unknown_id/already_resolved (§3.6)"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── requests ──

    #[test]
    fn encode_hello_is_a_single_tagged_line() {
        let line = encode_request(&Request::Hello {
            proto: PROTO_VERSION,
            client: "rustok-console/0.0.1",
        })
        .unwrap();
        assert_eq!(
            line,
            r#"{"op":"hello","proto":2,"client":"rustok-console/0.0.1"}"#
        );
    }

    #[test]
    fn encode_list_is_just_the_op() {
        assert_eq!(encode_request(&Request::List).unwrap(), r#"{"op":"list"}"#);
    }

    #[test]
    fn encode_context_is_just_the_op() {
        assert_eq!(
            encode_request(&Request::Context).unwrap(),
            r#"{"op":"context"}"#
        );
    }

    #[test]
    fn parse_context_carries_address_balances_and_chains() {
        let line = r#"{"ok":true,"address":"0x742d35Cc6634C0532925a3b844Bc9e7595f2bD4e",
            "balances":[{"chain_id":1,"symbol":"ETH","balance":"1000000000000000000"}],
            "allowed_chains":[1,8453]}"#
            .replace('\n', "");
        let ContextOutcome::Ok(ctx) = parse_context(&line).unwrap() else {
            panic!("ok context");
        };
        assert_eq!(ctx.address, "0x742d35Cc6634C0532925a3b844Bc9e7595f2bD4e");
        assert_eq!(ctx.balances.len(), 1);
        assert_eq!(ctx.balances[0].chain_id, 1);
        assert_eq!(ctx.balances[0].symbol, "ETH");
        // decimal wei string, verbatim — never re-based here
        assert_eq!(ctx.balances[0].balance, "1000000000000000000");
        assert_eq!(ctx.allowed_chains, vec![1, 8453]);
    }

    #[test]
    fn parse_context_tolerates_empty_balances() {
        // Every chain's provider was unreachable: balances are omitted, not
        // zeroed or errored (protocol §3.7) — the answer is still ok.
        let line = r#"{"ok":true,"address":"0xAbC","balances":[],"allowed_chains":[1]}"#;
        let ContextOutcome::Ok(ctx) = parse_context(line).unwrap() else {
            panic!("ok context");
        };
        assert!(ctx.balances.is_empty());
    }

    #[test]
    fn parse_context_wallet_locked() {
        let line = r#"{"ok":false,"error":"wallet_locked"}"#;
        assert_eq!(parse_context(line).unwrap(), ContextOutcome::WalletLocked);
    }

    #[test]
    fn parse_context_ok_without_address_is_malformed() {
        // An `ok` that cannot feed the From block is a protocol violation,
        // not a silent degradation.
        let line = r#"{"ok":true,"balances":[],"allowed_chains":[1]}"#;
        assert!(matches!(
            parse_context(line),
            Err(ProtocolError::Malformed(_))
        ));
    }

    #[test]
    fn parse_context_unauthorized_is_unexpected() {
        // We only send `context` post-auth on a proto-2 session; the server
        // disagreeing means the channel is not what we negotiated — the same
        // fail-closed class as an unexpected resolve code (→ Fatal upstream).
        let line = r#"{"ok":false,"error":"unauthorized"}"#;
        assert!(matches!(
            parse_context(line),
            Err(ProtocolError::Unexpected(_))
        ));
    }

    #[test]
    fn encode_get_carries_the_id() {
        let line = encode_request(&Request::Get { id: "abc-123" }).unwrap();
        assert_eq!(line, r#"{"op":"get","id":"abc-123"}"#);
    }

    // ── hello ──

    #[test]
    fn parse_hello_ok_keeps_the_server_id() {
        let r = parse_hello(r#"{"ok":true,"proto":1,"server":"core-server/0.1.0"}"#).unwrap();
        assert_eq!(
            r,
            HelloOutcome::Ok {
                server: "core-server/0.1.0".to_owned()
            }
        );
    }

    #[test]
    fn parse_hello_unsupported_carries_supported_versions() {
        let r = parse_hello(r#"{"ok":false,"error":"unsupported_proto","supported":[1]}"#).unwrap();
        assert_eq!(r, HelloOutcome::Unsupported { supported: vec![1] });
    }

    #[test]
    fn parse_hello_rejects_a_bogus_error() {
        assert!(matches!(
            parse_hello(r#"{"ok":false,"error":"weird"}"#),
            Err(ProtocolError::Unexpected(_))
        ));
    }

    // ── auth ──

    #[test]
    fn parse_auth_ok() {
        assert_eq!(parse_auth(r#"{"ok":true}"#).unwrap(), AuthOutcome::Ok);
    }

    #[test]
    fn parse_auth_bad_pin_carries_attempts_left_including_zero() {
        assert_eq!(
            parse_auth(r#"{"ok":false,"error":"bad_pin","attempts_left":2}"#).unwrap(),
            AuthOutcome::BadPin { attempts_left: 2 }
        );
        // attempts_left:0 is the arming response — must round-trip as 0, not drop.
        assert_eq!(
            parse_auth(r#"{"ok":false,"error":"bad_pin","attempts_left":0}"#).unwrap(),
            AuthOutcome::BadPin { attempts_left: 0 }
        );
    }

    #[test]
    fn parse_auth_locked_and_pin_states() {
        assert_eq!(
            parse_auth(r#"{"ok":false,"error":"locked","retry_after_s":287}"#).unwrap(),
            AuthOutcome::Locked { retry_after_s: 287 }
        );
        assert_eq!(
            parse_auth(r#"{"ok":false,"error":"pin_not_set"}"#).unwrap(),
            AuthOutcome::PinNotSet
        );
        assert_eq!(
            parse_auth(r#"{"ok":false,"error":"pin_unavailable"}"#).unwrap(),
            AuthOutcome::PinUnavailable
        );
    }

    // ── list ──

    #[test]
    fn parse_list_empty_queue() {
        assert_eq!(parse_list(r#"{"ok":true,"pending":[]}"#).unwrap(), vec![]);
    }

    #[test]
    fn parse_list_one_summary_all_fields() {
        let summaries = parse_list(
            r#"{"ok":true,"pending":[
                {"id":"a1","kind":"call","chain_id":1,"to":"0x742d35Cc6634C0532925a3b844Bc454e4438f44e",
                 "amount_wei":"100000000000000000","risk":"warning","high_risk":true,
                 "not_after_unix":1783100000}]}"#,
        )
        .unwrap();
        assert_eq!(summaries.len(), 1);
        let s = &summaries[0];
        assert_eq!(s.kind, Kind::Call);
        assert_eq!(s.risk, Risk::Warning);
        assert_eq!(s.amount_wei, "100000000000000000"); // decimal, verbatim
        assert!(s.high_risk);
    }

    #[test]
    fn parse_list_ignores_unknown_additive_fields() {
        // §6: additive fields within a major version are ignored, not rejected.
        let summaries = parse_list(
            r#"{"ok":true,"pending":[
                {"id":"a1","kind":"send","chain_id":1,"to":"0xabc","amount_wei":"0",
                 "risk":"safe","high_risk":false,"not_after_unix":1,"future_field":42}],
              "server_note":"ignored"}"#,
        )
        .unwrap();
        assert_eq!(summaries[0].kind, Kind::Send);
    }

    // ── get ──

    #[test]
    fn parse_get_card_with_decoded_call() {
        let out = parse_get(
            r#"{"ok":true,"card":{"id":"a1","chain_id":1,
                "to":"0x742d35Cc6634C0532925a3b844Bc454e4438f44e","amount_wei":"0",
                "decoded_call":{"method":"approve",
                    "spender":"0x742d35cc6634c0532925a3b844bc454e4438f44e",
                    "amount":"0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                    "is_unlimited":true},
                "high_risk":true,"high_risk_reasons":["unlimited_approval"],
                "raw_data":"0x095ea7b3","not_after_unix":1783100000}}"#,
        )
        .unwrap();
        let GetOutcome::Card(card) = out else {
            panic!("expected a card");
        };
        let decoded = card.decoded_call.expect("decoded_call present");
        assert_eq!(decoded.method, "approve");
        assert_eq!(decoded.is_unlimited, Some(true));
        // amount is a 0x-hex STRING (bignum-safe), not an integer.
        assert_eq!(
            decoded.amount.as_deref(),
            Some("0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff")
        );
        // absent sub-fields are None, not a misleading zero.
        assert_eq!(decoded.operator, None);
        assert_eq!(card.raw_data, "0x095ea7b3");
    }

    #[test]
    fn parse_get_card_with_null_decoded_call() {
        // A bare transfer: decoded_call is null, raw_data is "0x".
        let out = parse_get(
            r#"{"ok":true,"card":{"id":"a1","chain_id":1,"to":"0xabc","amount_wei":"1000",
                "decoded_call":null,"high_risk":false,"high_risk_reasons":[],
                "raw_data":"0x","not_after_unix":1}}"#,
        )
        .unwrap();
        let GetOutcome::Card(card) = out else {
            panic!("expected a card");
        };
        assert_eq!(card.decoded_call, None);
        assert_eq!(card.raw_data, "0x");
    }

    #[test]
    fn parse_get_unknown_id() {
        assert_eq!(
            parse_get(r#"{"ok":false,"error":"unknown_id"}"#).unwrap(),
            GetOutcome::UnknownId
        );
    }

    #[test]
    fn parse_get_ok_without_card_is_malformed() {
        assert!(matches!(
            parse_get(r#"{"ok":true}"#),
            Err(ProtocolError::Malformed(_))
        ));
    }

    // ── codec robustness ──

    #[test]
    fn parse_rejects_non_json() {
        assert!(matches!(
            parse_hello("not json at all"),
            Err(ProtocolError::Malformed(_))
        ));
    }

    #[test]
    fn parse_rejects_a_truncated_line() {
        assert!(matches!(
            parse_get(r#"{"ok":true,"card":{"id":"#),
            Err(ProtocolError::Malformed(_))
        ));
    }

    // ── approve / deny requests ──

    #[test]
    fn encode_approve_and_deny_carry_the_id() {
        assert_eq!(
            encode_request(&Request::Approve { id: "a1" }).unwrap(),
            r#"{"op":"approve","id":"a1"}"#
        );
        assert_eq!(
            encode_request(&Request::Deny { id: "a1" }).unwrap(),
            r#"{"op":"deny","id":"a1"}"#
        );
    }

    // ── approve outcomes ──

    #[test]
    fn parse_approve_executed_carries_the_tx_hash() {
        assert_eq!(
            parse_approve(r#"{"ok":true,"state":"executed","tx_hash":"0xabc"}"#).unwrap(),
            ResolveOutcome::Executed {
                tx_hash: "0xabc".to_owned()
            }
        );
    }

    #[test]
    fn parse_approve_failed_carries_the_reason() {
        assert_eq!(
            parse_approve(r#"{"ok":true,"state":"failed","reason":"broadcast error"}"#).unwrap(),
            ResolveOutcome::Failed {
                reason: "broadcast error".to_owned()
            }
        );
    }

    #[test]
    fn parse_approve_error_codes() {
        assert_eq!(
            parse_approve(r#"{"ok":false,"error":"pin_required"}"#).unwrap(),
            ResolveOutcome::PinRequired
        );
        assert_eq!(
            parse_approve(r#"{"ok":false,"error":"bad_pin","attempts_left":1}"#).unwrap(),
            ResolveOutcome::BadPin { attempts_left: 1 }
        );
        assert_eq!(
            parse_approve(r#"{"ok":false,"error":"locked","retry_after_s":300}"#).unwrap(),
            ResolveOutcome::Locked { retry_after_s: 300 }
        );
        assert_eq!(
            parse_approve(r#"{"ok":false,"error":"unauthorized"}"#).unwrap(),
            ResolveOutcome::Unauthorized
        );
        assert_eq!(
            parse_approve(r#"{"ok":false,"error":"unknown_id"}"#).unwrap(),
            ResolveOutcome::UnknownId
        );
    }

    #[test]
    fn parse_approve_already_resolved_accepts_every_state_including_pending() {
        for (word, state) in [
            ("executed", TerminalState::Executed),
            ("denied", TerminalState::Denied),
            ("expired", TerminalState::Expired),
            ("failed", TerminalState::Failed),
            ("pending", TerminalState::Pending), // I4 — must not panic
        ] {
            let line = format!(r#"{{"ok":false,"error":"already_resolved","state":"{word}"}}"#);
            assert_eq!(
                parse_approve(&line).unwrap(),
                ResolveOutcome::AlreadyResolved { state }
            );
        }
    }

    #[test]
    fn parse_approve_rejects_an_unknown_already_resolved_state() {
        assert!(matches!(
            parse_approve(r#"{"ok":false,"error":"already_resolved","state":"weird"}"#),
            Err(ProtocolError::Unexpected(_))
        ));
    }

    // ── deny outcomes ──

    #[test]
    fn parse_deny_denied() {
        assert_eq!(
            parse_deny(r#"{"ok":true,"state":"denied"}"#).unwrap(),
            ResolveOutcome::Denied
        );
    }

    #[test]
    fn parse_deny_accepts_exactly_its_documented_errors() {
        // §3.6: unauthorized / unknown_id / already_resolved — and nothing else.
        assert_eq!(
            parse_deny(r#"{"ok":false,"error":"unauthorized"}"#).unwrap(),
            ResolveOutcome::Unauthorized
        );
        assert_eq!(
            parse_deny(r#"{"ok":false,"error":"unknown_id"}"#).unwrap(),
            ResolveOutcome::UnknownId
        );
        assert_eq!(
            parse_deny(r#"{"ok":false,"error":"already_resolved","state":"executed"}"#).unwrap(),
            ResolveOutcome::AlreadyResolved {
                state: TerminalState::Executed
            }
        );
    }

    #[test]
    fn parse_deny_refuses_the_whole_pin_family() {
        // §3.6: "deny never requires a PIN beyond session auth". A PIN-family
        // answer to a deny would flow into the PIN prompt and turn the human's
        // "no" into an approve line — it must kill the channel instead.
        for line in [
            r#"{"ok":false,"error":"pin_required"}"#,
            r#"{"ok":false,"error":"bad_pin","attempts_left":2}"#,
            r#"{"ok":false,"error":"locked","retry_after_s":30}"#,
            r#"{"ok":false,"error":"pin_not_set"}"#,
            r#"{"ok":false,"error":"pin_unavailable"}"#,
        ] {
            assert!(
                matches!(parse_deny(line), Err(ProtocolError::Unexpected(_))),
                "deny must never accept: {line}"
            );
        }
    }

    // ── positions (§3.8) ──

    #[test]
    fn parse_positions_keeps_every_field_and_extra_verbatim() {
        // The canonical §3.8 example: display strings ("∞", "80%") must cross
        // untouched — the dashboard renders them, it never parses them.
        let line = r#"{"ok":true,"positions":[
            {"protocol":"aave_v3","chain_id":1,
             "asset_address":"0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2",
             "asset_symbol":"USD","asset_name":"Aave v3 account",
             "asset_decimals":8,"balance":"100000000000","balance_formatted":"1000",
             "extra":{"available_borrows_usd":"250","health_factor":"∞","ltv":"80%","total_debt_usd":"0"}}]}"#
            .replace('\n', "");
        let PositionsOutcome::Ok(positions) = parse_positions(&line).unwrap() else {
            panic!("ok positions");
        };
        assert_eq!(positions.len(), 1);
        let p = &positions[0];
        assert_eq!(p.protocol, "aave_v3");
        assert_eq!(p.chain_id, 1);
        assert_eq!(p.asset_symbol, "USD");
        assert_eq!(p.asset_name, "Aave v3 account");
        assert_eq!(p.asset_decimals, 8);
        assert_eq!(p.balance, "100000000000");
        assert_eq!(p.balance_formatted, "1000");
        assert_eq!(p.extra["health_factor"], "∞");
        assert_eq!(p.extra["ltv"], "80%");
        assert_eq!(p.extra.len(), 4);
    }

    #[test]
    fn parse_positions_accepts_an_empty_list_as_success() {
        // Best-effort canon: no positions, or every source skipped — still ok.
        let line = r#"{"ok":true,"positions":[]}"#;
        assert_eq!(parse_positions(line).unwrap(), PositionsOutcome::Ok(vec![]));
    }

    #[test]
    fn parse_positions_wallet_locked_degrades() {
        let line = r#"{"ok":false,"error":"wallet_locked"}"#;
        assert_eq!(
            parse_positions(line).unwrap(),
            PositionsOutcome::WalletLocked
        );
    }

    #[test]
    fn parse_positions_unexpected_errors_fail_closed() {
        // unauthorized/protocol_error mean the channel is not what we
        // negotiated — an error, not a degradation.
        for code in ["unauthorized", "protocol_error"] {
            let line = format!(r#"{{"ok":false,"error":"{code}"}}"#);
            assert!(matches!(
                parse_positions(&line),
                Err(ProtocolError::Unexpected(_))
            ));
        }
    }

    #[test]
    fn parse_positions_ok_without_array_is_malformed() {
        let line = r#"{"ok":true}"#;
        assert!(matches!(
            parse_positions(line),
            Err(ProtocolError::Malformed(_))
        ));
        assert!(matches!(
            parse_positions("not json"),
            Err(ProtocolError::Malformed(_))
        ));
    }

    #[test]
    fn positions_request_encodes_the_documented_op() {
        assert_eq!(
            encode_request(&Request::Positions).unwrap(),
            r#"{"op":"positions"}"#
        );
    }

    // ── activity (§3.9, Stage 7) — mirrors the positions parse suite ──

    #[test]
    fn parse_activity_carries_all_four_states_verbatim() {
        let line = r#"{"ok":true,"outcomes":[
            {"id":"e1","state":"executed","tx_hash":"0xfeed","age_secs":42},
            {"id":"d1","state":"denied","age_secs":120},
            {"id":"x1","state":"expired","age_secs":1800},
            {"id":"f1","state":"failed","reason":"broadcast failed","age_secs":3599}]}"#
            .replace('\n', "");
        let outcomes = parse_activity(&line).unwrap();
        assert_eq!(outcomes.len(), 4);
        assert_eq!(outcomes[0].id, "e1");
        assert_eq!(outcomes[0].state, OutcomeState::Executed);
        assert_eq!(outcomes[0].tx_hash.as_deref(), Some("0xfeed"));
        assert_eq!(outcomes[0].reason, None);
        assert_eq!(outcomes[0].age_secs, 42);
        assert_eq!(outcomes[1].state, OutcomeState::Denied);
        assert_eq!(
            (
                outcomes[1].tx_hash.as_deref(),
                outcomes[1].reason.as_deref()
            ),
            (None, None),
            "absent wire fields read as None, never a fabricated value"
        );
        assert_eq!(outcomes[2].state, OutcomeState::Expired);
        assert_eq!(outcomes[3].state, OutcomeState::Failed);
        assert_eq!(outcomes[3].reason.as_deref(), Some("broadcast failed"));
        assert_eq!(outcomes[3].tx_hash, None);
    }

    #[test]
    fn parse_activity_accepts_an_empty_history() {
        let line = r#"{"ok":true,"outcomes":[]}"#;
        assert_eq!(parse_activity(line).unwrap(), vec![]);
    }

    #[test]
    fn parse_activity_never_accepts_a_pending_state() {
        // §3.9: only terminal words; "pending" is not in the vocabulary and
        // must fail the parse (default-deny), not slip through as data.
        let line = r#"{"ok":true,"outcomes":[{"id":"p1","state":"pending","age_secs":1}]}"#;
        assert!(matches!(
            parse_activity(line),
            Err(ProtocolError::Malformed(_))
        ));
    }

    #[test]
    fn parse_activity_unexpected_errors_fail_closed() {
        // §3.9 has no degradable answer (no wallet_locked): any error code
        // means the channel is not what we negotiated.
        for code in ["unauthorized", "protocol_error", "wallet_locked"] {
            let line = format!(r#"{{"ok":false,"error":"{code}"}}"#);
            assert!(matches!(
                parse_activity(&line),
                Err(ProtocolError::Unexpected(_))
            ));
        }
    }

    #[test]
    fn parse_activity_ok_without_array_is_malformed() {
        assert!(matches!(
            parse_activity(r#"{"ok":true}"#),
            Err(ProtocolError::Malformed(_))
        ));
        assert!(matches!(
            parse_activity("not json"),
            Err(ProtocolError::Malformed(_))
        ));
    }

    #[test]
    fn activity_request_encodes_the_documented_op() {
        assert_eq!(
            encode_request(&Request::Activity).unwrap(),
            r#"{"op":"activity"}"#
        );
    }
}
