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

/// Wire protocol major version this client speaks. Proto 3 adds `ack`
/// (protocol §3.10) — the operation by which a human confirms an autonomous
/// mode the core's volume-shape heuristic assigned on its own — together with
/// the `policy_mode` / `policy_origin` pair on `context` (§3.7). Proto 2 had
/// added the auth-gated read-ops, `context` among them.
///
/// **There is deliberately no fallback to an older proto against an older
/// server: the wallet image ships core and console as a pair, so a mismatch
/// means a hand-built setup — the honest answer is the upgrade hint, not a
/// silently poorer card (Gate-1 ratification, 2026-07-12).** Re-confirmed when
/// proto 3 landed (2026-08-07) by re-measuring the premise rather than
/// trusting it: `Dockerfile.wallet` copies this binary in from a pinned
/// console image tag and lays it beside `core-server`, so the two versions are
/// locked together by the image build. A reconnect-and-degrade branch would be
/// machinery for a case the deployment does not produce.
///
/// 4 carries `set_mode` (§3.13) — the human switches the wallet's mode from
/// the Dashboard, downgrades included; `ack` stays in the protocol for older
/// consoles, this one no longer sends it.
pub const PROTO_VERSION: u32 = 4;

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

/// One balance row from `context` (protocol §3.7) — the chain's native coin, or
/// a token the operator put in the registry. `balance` is a **decimal** string
/// in the asset's own raw units, kept verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ChainBalance {
    /// EVM chain id.
    pub chain_id: u64,
    /// The asset's symbol — `"ETH"` for the native coin, the registry's word for
    /// a token.
    ///
    /// Rendered. It used to be parsed and dropped, because every row was ETH and
    /// the amount formatter already said so — printing both is what made the
    /// panel read `0.01 ETH ETH`. With tokens in the list the unit is no longer
    /// one word for the whole panel, so the row carries its own and the
    /// formatter is told which one to state.
    pub symbol: String,
    /// The balance in the asset's raw integer units, decimal string.
    pub balance: String,
    /// How many places [`Self::balance`] is denominated in — 18 for the native
    /// coin, 6 for USDC.
    ///
    /// Mandatory on the wire, with no serde default: a missing field silently
    /// read as 18 would divide a USDC balance by 10¹² and show dust where there
    /// is money. A core too old to send it is a core this console refuses to
    /// draw for, not one it guesses for.
    pub decimals: u8,
    /// [`Self::balance`] with [`Self::decimals`] applied, trailing zeros
    /// trimmed — the string the panel prints.
    ///
    /// The core renders it once, exactly as it does for a position, so nothing
    /// downstream has to know an asset's decimals in order to print it
    /// (`AGENTS.md` #1: the console re-bases for display, never re-derives).
    pub balance_formatted: String,
    /// The token's contract, or empty for the native coin.
    ///
    /// A symbol is not unique: native USDC and bridged USDC.e sit side by side
    /// on Arbitrum and people call both "USDC". The contract is what tells them
    /// apart.
    pub token_address: String,
}

/// One asset that could not be read at all (protocol §3.7).
///
/// Not a balance of zero — a balance the wallet does not know. The distinction
/// is the point: before this existed, an unreachable chain simply had no row,
/// and a short panel read as "you have nothing".
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AssetUnavailable {
    /// EVM chain id.
    pub chain_id: u64,
    /// `"ETH"` for the native coin, else the token's symbol — which asset on
    /// that chain went unread.
    pub symbol: String,
    /// Why, in the core's words: `no_rpc_configured` | `rpc_call_failed` |
    /// `call_reverted`. Kept as the wire string and worded for the human in one
    /// place (`ui.rs`); an unknown word is shown verbatim rather than guessed at.
    pub reason: String,
    /// The token's contract, or empty for the native coin.
    ///
    /// Defaulted, unlike the contract on a balance row, and the difference is
    /// the point: that one is drawn, so a core that stopped sending it would
    /// leave a token looking like a native coin, and refusing the row is the
    /// cheaper failure. This one is not drawn at all. Making it mandatory buys
    /// nothing and costs a whole `context` reply — one missing field on one
    /// unread asset would fail the parse, surface as `Reply::Fatal`, and end the
    /// session over a string nobody reads (round-6 MINOR-3).
    #[serde(default)]
    pub token_address: String,
}

/// The wallet's own context from a successful `context` reply (protocol §3.7).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WalletContext {
    /// The wallet's (signer's) address, EIP-55 checksummed — same convention
    /// as the card's top-level `to`, so From→To renders both verbatim.
    pub address: String,
    /// Every asset the wallet holds and could read: each allowed chain's native
    /// coin first, then that chain's registry tokens in declaration order.
    ///
    /// An asset missing from here is either a zero balance or an unread one, and
    /// [`Self::unavailable`] is what separates the two.
    pub balances: Vec<ChainBalance>,
    /// Assets the wallet could NOT read, and why (§3.7).
    ///
    /// Empty means everything configured was queried — so a balance row absent
    /// from [`Self::balances`] while this list is empty means zero, and only
    /// zero. Absent on the wire reads as empty, like `balances`: the mandatory
    /// fields on a balance row are what refuse a core too old to have this at
    /// all.
    pub unavailable: Vec<AssetUnavailable>,
    /// The server's configured chain allow-list, in order.
    pub allowed_chains: Vec<u64>,
    /// The autonomy ceiling and how the core arrived at it (§3.7, proto 3+).
    ///
    /// Skipped by serde on purpose: the wire path goes through `parse_context`'s
    /// private `Raw`, which is the ONE place the pair is interpreted — including
    /// its safe readings for absent/unknown words. A second derive-driven path
    /// would be a second interpretation, free to drift from the first.
    #[serde(skip)]
    pub policy: Policy,
}

/// The wallet's autonomy as the core reports it — **mode and origin together**.
///
/// They are one statement, not two fields (§3.7): `Autonomous` +
/// `Provisioned` still parks every send, so anything rendering the mode alone
/// tells the human this wallet sends when it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Policy {
    /// The ceiling itself.
    pub mode: PolicyMode,
    /// How the core arrived at it.
    pub origin: PolicyOrigin,
}

/// The autonomy ceiling (§3.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PolicyMode {
    /// Writes denied outright.
    ReadOnly,
    /// Every write parks for the human.
    Supervised,
    /// Sends on its own — **only once [`PolicyOrigin::Acknowledged`]**.
    Autonomous,
    /// Absent or a word this build does not know.
    ///
    /// Reachable without any exotic scenario: a `proto:2` core carries no
    /// policy fields at all, which is exactly the degraded session §3.1's
    /// fallback lands in. The default is this rather than a real mode because
    /// a guess here is a claim about whether the wallet spends money by itself.
    #[default]
    Unknown,
}

/// How the mode was set (§3.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PolicyOrigin {
    /// Assigned by the core's volume-shape heuristic — nobody was asked.
    ///
    /// Normative default for an absent or unrecognised value: erring here costs
    /// one extra confirmation, erring the other way misinforms the human.
    #[default]
    Provisioned,
    /// A human confirmed it (`ack`, §3.10).
    Acknowledged,
}

impl Policy {
    /// Whether there is autonomy here awaiting a human's confirmation — the one
    /// state that asks something of the human.
    #[must_use]
    pub fn awaits_acknowledgment(self) -> bool {
        matches!(
            (self.mode, self.origin),
            (PolicyMode::Autonomous, PolicyOrigin::Provisioned)
        )
    }
}

impl PolicyMode {
    /// The wire word `set_mode` sends (§3.13). `Unknown` has none — it is a
    /// reading of a degraded session, not a mode a human can ask for, and a
    /// `None` here is what keeps the switcher from ever putting it on the wire.
    #[must_use]
    pub fn wire_word(self) -> Option<&'static str> {
        match self {
            Self::ReadOnly => Some("read_only"),
            Self::Supervised => Some("supervised"),
            Self::Autonomous => Some("autonomous"),
            Self::Unknown => None,
        }
    }
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
    /// The core's own keyring isn't unlocked (§3.12).
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
/// Outcome of `ack` (§3.10) — confirming this wallet's autonomous mode.
///
/// Deliberately a separate type from [`AuthOutcome`] even though the PIN-family
/// answers coincide: `ack` also answers `not_autonomous` and
/// `policy_store_failed`, and one shared type would let an `auth` handler
/// silently accept an outcome that only makes sense here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckOutcome {
    /// The mode is confirmed — and stays confirmed across restarts.
    Confirmed,
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
    /// Transient Argon2 backend failure — never an accept.
    PinUnavailable,
    /// Not an autonomous wallet: nothing to confirm. `ack` confirms an existing
    /// mode and never switches one.
    NotAutonomous,
    /// The core could not persist the policy and left it unchanged on purpose —
    /// a confirmation that lasted only until the next restart would be worse
    /// than a visible failure.
    StoreFailed,
}

/// Parse an `ack` reply (§3.10).
///
/// # Errors
/// [`ProtocolError::Unexpected`] for anything the canon does not list —
/// including `unauthorized` and `protocol_error`, which mean the channel is not
/// what we negotiated. This op lifts the parking gate for good, so an answer we
/// do not understand is never read as a confirmation.
pub fn parse_ack(line: &str) -> Result<AckOutcome, ProtocolError> {
    #[derive(Deserialize)]
    struct Raw {
        ok: bool,
        error: Option<String>,
        attempts_left: Option<u32>,
        retry_after_s: Option<u64>,
    }
    let raw: Raw = parse_line(line)?;
    if raw.ok {
        return Ok(AckOutcome::Confirmed);
    }
    match raw.error.as_deref() {
        Some("bad_pin") => Ok(AckOutcome::BadPin {
            attempts_left: raw.attempts_left.unwrap_or(0),
        }),
        Some("locked") => Ok(AckOutcome::Locked {
            retry_after_s: raw.retry_after_s.unwrap_or(0),
        }),
        Some("pin_not_set") => Ok(AckOutcome::PinNotSet),
        Some("pin_unavailable") => Ok(AckOutcome::PinUnavailable),
        Some("not_autonomous") => Ok(AckOutcome::NotAutonomous),
        Some("policy_store_failed") => Ok(AckOutcome::StoreFailed),
        other => Err(ProtocolError::Unexpected(
            other.unwrap_or("ack without ok or error").to_owned(),
        )),
    }
}

/// Outcome of `set_mode` (§3.13) — the human switches the wallet's mode.
///
/// A separate type from [`AckOutcome`] for the same reason that one is
/// separate from [`AuthOutcome`]: this op answers refusals none of the others
/// can (`unknown_mode`, `policy_newer_than_build`, `policy_unreadable`), and a
/// shared type would let a handler accept an outcome that only makes sense
/// elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetModeOutcome {
    /// The wallet now stands where the human asked — mode and origin as the
    /// core recorded them, so the header can change without a restart.
    Applied {
        /// The mode the core confirmed.
        mode: PolicyMode,
        /// Always [`PolicyOrigin::Acknowledged`] on today's core; parsed
        /// rather than assumed so the header shows what was said, not what
        /// was expected.
        origin: PolicyOrigin,
    },
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
    /// Transient Argon2 backend failure — never an accept.
    PinUnavailable,
    /// The core did not recognise the mode name. This console only sends the
    /// three it renders, so seeing this means the pair is not the pair.
    UnknownMode,
    /// `policy.json` was written by a newer build; the core refused to touch
    /// it. Rolling the image forward (or removing the file) is the way out.
    NewerBuild,
    /// `policy.json` cannot be read at all (not merely absent); the core
    /// refused to overwrite what it cannot see.
    Unreadable,
    /// The core could not persist the change and left everything as it was.
    StoreFailed,
}

/// Parse a `set_mode` reply (§3.13).
///
/// # Errors
/// [`ProtocolError::Unexpected`] for anything the canon does not list —
/// including `unauthorized` and `protocol_error`, which mean the channel is
/// not what we negotiated. This op moves the wallet between modes, so an
/// answer we do not understand is never read as applied.
pub fn parse_set_mode(line: &str) -> Result<SetModeOutcome, ProtocolError> {
    #[derive(Deserialize)]
    struct Raw {
        ok: bool,
        mode: Option<String>,
        origin: Option<String>,
        error: Option<String>,
        attempts_left: Option<u32>,
        retry_after_s: Option<u64>,
    }
    let raw: Raw = parse_line(line)?;
    if raw.ok {
        // The same word-maps the context parser uses (§3.7): an ok answer with
        // a word this build does not know falls to the safe reading rather
        // than an error — the switch DID land, and refusing to show it would
        // desynchronise the header from the wallet.
        return Ok(SetModeOutcome::Applied {
            mode: match raw.mode.as_deref() {
                Some("read_only") => PolicyMode::ReadOnly,
                Some("supervised") => PolicyMode::Supervised,
                Some("autonomous") => PolicyMode::Autonomous,
                _ => PolicyMode::Unknown,
            },
            origin: match raw.origin.as_deref() {
                Some("acknowledged") => PolicyOrigin::Acknowledged,
                _ => PolicyOrigin::Provisioned,
            },
        });
    }
    match raw.error.as_deref() {
        Some("bad_pin") => Ok(SetModeOutcome::BadPin {
            attempts_left: raw.attempts_left.unwrap_or(0),
        }),
        Some("locked") => Ok(SetModeOutcome::Locked {
            retry_after_s: raw.retry_after_s.unwrap_or(0),
        }),
        Some("pin_not_set") => Ok(SetModeOutcome::PinNotSet),
        Some("pin_unavailable") => Ok(SetModeOutcome::PinUnavailable),
        Some("unknown_mode") => Ok(SetModeOutcome::UnknownMode),
        Some("policy_newer_than_build") => Ok(SetModeOutcome::NewerBuild),
        Some("policy_unreadable") => Ok(SetModeOutcome::Unreadable),
        Some("policy_store_failed") => Ok(SetModeOutcome::StoreFailed),
        other => Err(ProtocolError::Unexpected(
            other.unwrap_or("set_mode without ok or error").to_owned(),
        )),
    }
}

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
        unavailable: Option<Vec<AssetUnavailable>>,
        allowed_chains: Option<Vec<u64>>,
        policy_mode: Option<String>,
        policy_origin: Option<String>,
        error: Option<String>,
    }
    let raw: Raw = parse_line(line)?;
    if raw.ok {
        let address = raw
            .address
            .ok_or_else(|| ProtocolError::Malformed("ok context without address".to_owned()))?;
        // Absent or unrecognised words fall to the safe reading rather than a
        // parse error: a proto-2 core sends neither field, and refusing the
        // whole context would cost the human the screen over a field that is
        // not signing-critical (§3.7).
        let policy = Policy {
            mode: match raw.policy_mode.as_deref() {
                Some("read_only") => PolicyMode::ReadOnly,
                Some("supervised") => PolicyMode::Supervised,
                Some("autonomous") => PolicyMode::Autonomous,
                _ => PolicyMode::Unknown,
            },
            origin: match raw.policy_origin.as_deref() {
                Some("acknowledged") => PolicyOrigin::Acknowledged,
                _ => PolicyOrigin::Provisioned,
            },
        };
        Ok(ContextOutcome::Ok(Box::new(WalletContext {
            address,
            balances: raw.balances.unwrap_or_default(),
            unavailable: raw.unavailable.unwrap_or_default(),
            allowed_chains: raw.allowed_chains.unwrap_or_default(),
            policy,
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
            r#"{"op":"hello","proto":4,"client":"rustok-console/0.0.1"}"#
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
            "balances":[{"chain_id":1,"symbol":"ETH","balance":"1000000000000000000",
            "decimals":18,"balance_formatted":"1","token_address":""}],
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

    /// Test 9 (spec §S2). A token row arrives whole — the symbol the operator
    /// registered, the raw units, the places they are counted in, the string to
    /// print, and the contract that tells USDC from USDC.e.
    #[test]
    fn parse_context_carries_a_token_row_whole() {
        let line = r#"{"ok":true,"address":"0xAbC","balances":[
            {"chain_id":42161,"symbol":"ETH","balance":"6700000000000000",
             "decimals":18,"balance_formatted":"0.0067","token_address":""},
            {"chain_id":42161,"symbol":"USDC","balance":"22820562","decimals":6,
             "balance_formatted":"22.820562",
             "token_address":"0xaf88d065e77c8cC2239327C5EDb3A432268e5831"}],
            "allowed_chains":[42161]}"#
            .replace('\n', "");
        let ContextOutcome::Ok(ctx) = parse_context(&line).unwrap() else {
            panic!("ok context");
        };
        assert_eq!(ctx.balances.len(), 2, "native first, then the registry");
        let token = &ctx.balances[1];
        assert_eq!(token.symbol, "USDC");
        assert_eq!(token.balance, "22820562");
        assert_eq!(token.decimals, 6);
        assert_eq!(token.balance_formatted, "22.820562");
        assert_eq!(
            token.token_address,
            "0xaf88d065e77c8cC2239327C5EDb3A432268e5831"
        );
        // The native row keeps the empty contract that marks it as native.
        assert!(ctx.balances[0].token_address.is_empty());
    }

    /// Test 9, the half that has teeth. A row without the token fields is a row
    /// from a core that predates them, and the console must refuse it rather
    /// than fill in 18 places — that default would turn 22.820562 USDC into
    /// 0.000000000000022820 and call it a balance.
    ///
    /// **One field at a time, on purpose** (round-6 MINOR-2). Dropping all three
    /// at once cannot say which one is guarded: a regression that put
    /// `serde(default)` back on `decimals` alone would still be refused by the
    /// other two, and this test would stay green while the guard that matters
    /// was gone.
    #[test]
    fn parse_context_refuses_a_balance_row_missing_any_one_token_field() {
        // The whole row, then the same row with exactly one field taken out.
        let whole = [
            (r#""chain_id""#, "1"),
            (r#""symbol""#, r#""ETH""#),
            (r#""balance""#, r#""1000000000000000000""#),
            (r#""decimals""#, "18"),
            (r#""balance_formatted""#, r#""1""#),
            (r#""token_address""#, r#""""#),
        ];
        for dropped in ["\"decimals\"", "\"balance_formatted\"", "\"token_address\""] {
            let row: Vec<String> = whole
                .iter()
                .filter(|(k, _)| *k != dropped)
                .map(|(k, v)| format!("{k}:{v}"))
                .collect();
            let line = format!(
                r#"{{"ok":true,"address":"0xAbC","balances":[{{{}}}],"allowed_chains":[1]}}"#,
                row.join(",")
            );
            assert!(
                matches!(parse_context(&line), Err(ProtocolError::Malformed(_))),
                "a row missing {dropped} must be malformed, not a row with a \
                 silent default: {line}"
            );
        }
    }

    /// The counterpart with the same shape: the row that has all three parses.
    /// Without it the test above would also pass on a parser that refuses every
    /// balance row there is.
    #[test]
    fn parse_context_accepts_the_row_those_fields_complete() {
        let line = r#"{"ok":true,"address":"0xAbC","balances":[{"chain_id":1,
            "symbol":"ETH","balance":"1000000000000000000","decimals":18,
            "balance_formatted":"1","token_address":""}],"allowed_chains":[1]}"#
            .replace('\n', "");
        let ContextOutcome::Ok(ctx) = parse_context(&line).unwrap() else {
            panic!("ok context");
        };
        assert_eq!(ctx.balances.len(), 1);
    }

    /// MINOR-3: an unread asset without a contract is an ordinary native one,
    /// and the field it does not need must not cost the whole reply. The core
    /// sends it today (`server.rs` puts an empty string on every native entry) —
    /// this pins that the console does not DEPEND on it doing so.
    #[test]
    fn parse_context_accepts_an_unread_asset_without_a_contract() {
        let line = r#"{"ok":true,"address":"0xAbC","balances":[],
            "unavailable":[{"chain_id":8453,"symbol":"ETH","reason":"no_rpc_configured"}],
            "allowed_chains":[8453]}"#
            .replace('\n', "");
        let ContextOutcome::Ok(ctx) = parse_context(&line).unwrap() else {
            panic!("an unread native asset carries no contract, and needs none");
        };
        assert_eq!(ctx.unavailable.len(), 1);
        assert!(ctx.unavailable[0].token_address.is_empty());
    }

    /// An unread asset is not a zero one. The list says which asset, on which
    /// chain, and in the core's own words why.
    #[test]
    fn parse_context_carries_the_assets_it_could_not_read() {
        let line = r#"{"ok":true,"address":"0xAbC","balances":[],"unavailable":[
            {"chain_id":8453,"symbol":"ETH","reason":"no_rpc_configured","token_address":""},
            {"chain_id":42161,"symbol":"USDT","reason":"call_reverted",
             "token_address":"0xdAC17F958D2ee523a2206206994597C13D831ec7"}],
            "allowed_chains":[8453,42161]}"#
            .replace('\n', "");
        let ContextOutcome::Ok(ctx) = parse_context(&line).unwrap() else {
            panic!("ok context");
        };
        assert_eq!(ctx.unavailable.len(), 2);
        assert_eq!(ctx.unavailable[0].chain_id, 8453);
        assert_eq!(ctx.unavailable[0].symbol, "ETH");
        assert_eq!(ctx.unavailable[0].reason, "no_rpc_configured");
        assert!(ctx.unavailable[0].token_address.is_empty());
        assert_eq!(ctx.unavailable[1].reason, "call_reverted");
        assert_eq!(
            ctx.unavailable[1].token_address,
            "0xdAC17F958D2ee523a2206206994597C13D831ec7"
        );
    }

    /// Nothing unavailable is the ordinary case, and it must not need the key:
    /// an absent list reads as empty, exactly as an absent `balances` does.
    #[test]
    fn parse_context_reads_an_absent_unavailable_list_as_empty() {
        let line = r#"{"ok":true,"address":"0xAbC","balances":[],"allowed_chains":[1]}"#;
        let ContextOutcome::Ok(ctx) = parse_context(line).unwrap() else {
            panic!("ok context");
        };
        assert!(ctx.unavailable.is_empty());
    }

    /// Every answer §3.10 lists, transcribed from the protocol canon rather
    /// than from whatever the parser happens to accept.
    #[test]
    fn parse_ack_covers_every_documented_answer() {
        for (line, expected) in [
            (
                r#"{"ok":true,"mode":"autonomous","origin":"acknowledged"}"#,
                AckOutcome::Confirmed,
            ),
            (
                r#"{"ok":false,"error":"bad_pin","attempts_left":2}"#,
                AckOutcome::BadPin { attempts_left: 2 },
            ),
            (
                r#"{"ok":false,"error":"locked","retry_after_s":287}"#,
                AckOutcome::Locked { retry_after_s: 287 },
            ),
            (
                r#"{"ok":false,"error":"pin_not_set"}"#,
                AckOutcome::PinNotSet,
            ),
            (
                r#"{"ok":false,"error":"pin_unavailable"}"#,
                AckOutcome::PinUnavailable,
            ),
            (
                r#"{"ok":false,"error":"not_autonomous"}"#,
                AckOutcome::NotAutonomous,
            ),
            (
                r#"{"ok":false,"error":"policy_store_failed"}"#,
                AckOutcome::StoreFailed,
            ),
        ] {
            assert_eq!(parse_ack(line).unwrap(), expected, "line: {line}");
        }
    }

    /// An answer the canon does not list is not silently read as success —
    /// this is the one op that lifts the parking gate for good.
    #[test]
    fn an_unknown_ack_answer_is_never_a_confirmation() {
        for line in [
            r#"{"ok":false,"error":"unauthorized"}"#,
            r#"{"ok":false,"error":"protocol_error"}"#,
            r#"{"ok":false}"#,
        ] {
            assert!(
                parse_ack(line).is_err(),
                "must not resolve to an outcome: {line}"
            );
        }
    }

    /// Proto 4 is what carries `set_mode`; a silent drift of this constant
    /// would strand the switcher behind a `protocol_error`. Same shape as the
    /// core's own version pins.
    #[test]
    fn the_protocol_version_is_pinned() {
        assert_eq!(PROTO_VERSION, 4);
    }

    #[test]
    fn set_mode_replies_parse_to_their_outcomes() {
        for (line, expected) in [
            (
                r#"{"ok":true,"mode":"autonomous","origin":"acknowledged"}"#,
                SetModeOutcome::Applied {
                    mode: PolicyMode::Autonomous,
                    origin: PolicyOrigin::Acknowledged,
                },
            ),
            (
                r#"{"ok":true,"mode":"read_only","origin":"acknowledged"}"#,
                SetModeOutcome::Applied {
                    mode: PolicyMode::ReadOnly,
                    origin: PolicyOrigin::Acknowledged,
                },
            ),
            (
                r#"{"ok":true,"mode":"supervised","origin":"acknowledged"}"#,
                SetModeOutcome::Applied {
                    mode: PolicyMode::Supervised,
                    origin: PolicyOrigin::Acknowledged,
                },
            ),
            (
                r#"{"ok":false,"error":"bad_pin","attempts_left":2}"#,
                SetModeOutcome::BadPin { attempts_left: 2 },
            ),
            (
                r#"{"ok":false,"error":"locked","retry_after_s":300}"#,
                SetModeOutcome::Locked { retry_after_s: 300 },
            ),
            (
                r#"{"ok":false,"error":"pin_not_set"}"#,
                SetModeOutcome::PinNotSet,
            ),
            (
                r#"{"ok":false,"error":"pin_unavailable"}"#,
                SetModeOutcome::PinUnavailable,
            ),
            (
                r#"{"ok":false,"error":"unknown_mode"}"#,
                SetModeOutcome::UnknownMode,
            ),
            (
                r#"{"ok":false,"error":"policy_newer_than_build"}"#,
                SetModeOutcome::NewerBuild,
            ),
            (
                r#"{"ok":false,"error":"policy_unreadable"}"#,
                SetModeOutcome::Unreadable,
            ),
            (
                r#"{"ok":false,"error":"policy_store_failed"}"#,
                SetModeOutcome::StoreFailed,
            ),
        ] {
            assert_eq!(parse_set_mode(line).unwrap(), expected, "line: {line}");
        }
    }

    /// The same fail-closed rule as `ack`: this op moves the wallet between
    /// modes, so an answer outside the canon never reads as applied.
    #[test]
    fn an_unknown_set_mode_answer_is_never_applied() {
        for line in [
            r#"{"ok":false,"error":"unauthorized"}"#,
            r#"{"ok":false,"error":"protocol_error"}"#,
            r#"{"ok":false}"#,
        ] {
            assert!(
                parse_set_mode(line).is_err(),
                "must not resolve to an outcome: {line}"
            );
        }
    }

    /// The wire words are the three the human can pick; `Unknown` deliberately
    /// has none — a degraded reading must never become a request.
    #[test]
    fn wire_words_cover_exactly_the_pickable_modes() {
        assert_eq!(PolicyMode::ReadOnly.wire_word(), Some("read_only"));
        assert_eq!(PolicyMode::Supervised.wire_word(), Some("supervised"));
        assert_eq!(PolicyMode::Autonomous.wire_word(), Some("autonomous"));
        assert_eq!(PolicyMode::Unknown.wire_word(), None);
    }

    /// §3.13 — the PIN rides on the operation itself, and the line is built in
    /// a zeroizing buffer like `auth`, never through the general Serialize
    /// path. One case per pickable mode: the word slot is the only thing that
    /// varies, and each is a static string this test pins verbatim.
    #[test]
    fn the_set_mode_line_carries_the_mode_and_the_pin_and_nothing_else() {
        let mut pin = crate::app::Pin::default();
        for c in "483920".chars() {
            pin.push(c);
        }
        for (mode, expected) in [
            (
                PolicyMode::ReadOnly,
                r#"{"op":"set_mode","mode":"read_only","pin":"483920"}"#,
            ),
            (
                PolicyMode::Supervised,
                r#"{"op":"set_mode","mode":"supervised","pin":"483920"}"#,
            ),
            (
                PolicyMode::Autonomous,
                r#"{"op":"set_mode","mode":"autonomous","pin":"483920"}"#,
            ),
        ] {
            let word = mode.wire_word().expect("pickable modes have wire words");
            assert_eq!(&*pin.set_mode_line(word), expected);
        }
    }

    /// §3.7: the mode and its origin are ONE statement. A wallet reported as
    /// `autonomous` + `provisioned` still parks every send.
    #[test]
    fn parse_context_carries_the_policy_pair() {
        for (mode_wire, origin_wire, mode, origin) in [
            (
                "autonomous",
                "acknowledged",
                PolicyMode::Autonomous,
                PolicyOrigin::Acknowledged,
            ),
            (
                "autonomous",
                "provisioned",
                PolicyMode::Autonomous,
                PolicyOrigin::Provisioned,
            ),
            (
                "supervised",
                "provisioned",
                PolicyMode::Supervised,
                PolicyOrigin::Provisioned,
            ),
            (
                "read_only",
                "provisioned",
                PolicyMode::ReadOnly,
                PolicyOrigin::Provisioned,
            ),
        ] {
            let line = format!(
                r#"{{"ok":true,"address":"0x1","balances":[],"allowed_chains":[1],"policy_mode":"{mode_wire}","policy_origin":"{origin_wire}"}}"#
            );
            let ContextOutcome::Ok(ctx) = parse_context(&line).unwrap() else {
                panic!("ok context");
            };
            assert_eq!(ctx.policy.mode, mode, "mode {mode_wire}");
            assert_eq!(ctx.policy.origin, origin, "origin {origin_wire}");
        }
    }

    /// Normative (§3.7): an absent or unrecognised origin reads as
    /// `provisioned`. Reachable for real — a proto-2 core carries no policy
    /// fields at all. Erring this way costs one extra confirmation; erring the
    /// other way tells the human the wallet sends when it does not.
    #[test]
    fn an_absent_or_unknown_policy_origin_reads_as_provisioned() {
        for line in [
            r#"{"ok":true,"address":"0x1","balances":[],"allowed_chains":[1],"policy_mode":"autonomous"}"#,
            r#"{"ok":true,"address":"0x1","balances":[],"allowed_chains":[1],"policy_mode":"autonomous","policy_origin":"something_new"}"#,
        ] {
            let ContextOutcome::Ok(ctx) = parse_context(line).unwrap() else {
                panic!("ok context");
            };
            assert_eq!(
                ctx.policy.origin,
                PolicyOrigin::Provisioned,
                "unconfirmed is the safe reading: {line}"
            );
        }
    }

    /// A mode word we do not know must never render as autonomy. Also reachable:
    /// against a proto-2 core the field is absent entirely.
    #[test]
    fn an_absent_or_unknown_policy_mode_is_not_autonomous() {
        for line in [
            r#"{"ok":true,"address":"0x1","balances":[],"allowed_chains":[1]}"#,
            r#"{"ok":true,"address":"0x1","balances":[],"allowed_chains":[1],"policy_mode":"turbo"}"#,
        ] {
            let ContextOutcome::Ok(ctx) = parse_context(line).unwrap() else {
                panic!("ok context");
            };
            assert_eq!(ctx.policy.mode, PolicyMode::Unknown, "line: {line}");
            assert_ne!(ctx.policy.mode, PolicyMode::Autonomous);
        }
    }

    #[test]
    fn parse_context_tolerates_empty_balances() {
        // An empty list is still an `ok` answer (protocol §3.7). What it MEANS
        // is no longer decided here: with `unavailable` empty too, this wallet
        // holds nothing — a chain that could not be read says so in that list.
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
