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

/// Wire protocol major version this client speaks.
pub const PROTO_VERSION: u32 = 1;

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
            r#"{"op":"hello","proto":1,"client":"rustok-console/0.0.1"}"#
        );
    }

    #[test]
    fn encode_list_is_just_the_op() {
        assert_eq!(encode_request(&Request::List).unwrap(), r#"{"op":"list"}"#);
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
}
