# Approver Protocol — core ↔ console contract

> **Canonical.** This document is the source of truth for the wire protocol between
> the Rustok wallet core (server) and Rustok Console (client). The core implements
> the server side; any change lands HERE first, then in code on both sides.
>
> **proto 1 — DRAFT** until the first console (C-PR-1) implementation ships; frozen
> on the `console v0.1.0` / `core v0.2.0` release pair. The **server half already
> shipped** (`core` PR #86 + T0a #87). This doc was reconciled field-by-field
> against that shipped server on **2026-07-09** (Console step T0b): every message,
> field, error code, and wire format below is what the running server actually
> emits — not an aspiration. Where the shipped behaviour has a known rough edge, it
> is documented as-is and cross-referenced to a tracked follow-up rather than
> silently "corrected" here.

## 1. Transport

- **UNIX stream socket**, default path `/run/wallet/approve.sock` inside the
  `rustok-wallet` container; overridable via the `RUSTOK_APPROVE_SOCK` env var
  (the console targets the default; tests point it at a temp path). The socket's
  parent directory is created mode `0700` by both the server and the wallet-image
  entrypoint.
- **JSON Lines**: every message is a single JSON object on one line, UTF-8,
  terminated by `\n`. Client sends a request line; server answers with exactly one
  response line. No pipelining: one request in flight per connection.
- The client is `rustok-console` (launched via `docker exec`); the server is
  `core-server`. **No key material, seed words, or PIN hashes ever cross this
  socket.** The PIN itself crosses it only inside `auth` / high-risk `approve`
  requests, plaintext over the local socket (same-host, fs-permission-guarded);
  it is never logged on either side.
- **Trust perimeter is the socket's filesystem, not this handshake.** Anyone able
  to open the socket already shares the wallet's uid — and therefore already has
  the keystore file and (under `docker exec`, which inherits the container env) the
  `RUSTOK_KEYRING_PASSWORD`. `auth` gates *deciding*, not *reading* (see §2); it is
  not, and cannot be, a defence against a same-uid local process.

## 2. Session

```
connect → hello → { list | get }* → auth → { list | get | approve | deny }* → disconnect
```

- A **session is one connection**. `auth` authorizes that connection only;
  disconnect requires a fresh `auth`. There is no token, cookie, or resume.
- `hello` is mandatory and exactly once. Any request before a successful `hello`,
  and a **second** `hello` after the first, → `protocol_error`.
- **`list` and `get` are allowed pre-`auth`.** Viewing the queue and reading a
  clear-signing card are deliberately *not* privileged: the card carries only the
  agent's own proposal (nothing secret), and the perimeter is already the socket
  fs (§1). Only `approve` / `deny` require `auth`; before it they → `unauthorized`.
  A future reviewer must not "fix" this into an auth-gate — it would defend nothing
  and break the console's watch-before-unlock flow.
- **Default-deny**: a malformed line, unknown `op`, a missing or mistyped field, or
  an oversized message (> 64 KiB, code `oversize`) yields a single `error` response
  and **no state change**. The server never executes anything as a result of an
  ambiguous message. (An *extra* unknown field is silently ignored, not rejected —
  the request structs are not `deny_unknown_fields`; see `client` in §3.1.)
  - On `oversize` the server answers once and then **closes the connection**
    (parity with `unsupported_proto`, §3.1). The capped read leaves the over-long
    line's tail unread; closing drops it rather than parsing it as subsequent
    request lines — so a crafted padding cannot smuggle a well-formed request past
    the size guard. A client must reconnect (fresh `hello`, then `auth`) after an
    `oversize`. (The console never sends > 64 KiB lines, so this does not arise
    from the intended client.)

## 3. Messages

Field wire-formats are normative and listed in **§3.7** — the console's serde types
must mirror them exactly (e.g. `amount_wei` is a decimal string while a nested
`decoded_call.amount` is a `0x`-hex string; they are **not** interchangeable).

### 3.1 `hello` — version handshake

```json
→ {"op":"hello","proto":1,"client":"rustok-console/0.1.0"}
← {"ok":true,"proto":1,"server":"core-server/0.1.0"}
← {"ok":false,"error":"unsupported_proto","supported":[1]}   // then server closes
```

- `client` is **informational**: the server does not read or validate it (the
  request struct is not `deny_unknown_fields`, so any extra field is ignored). The
  console may send it for logs; the server is not obliged to.
- `server` is **informational too**: it reflects the server binary's
  `CARGO_PKG_VERSION` (`core-server/<version>`; the shipped build answers
  `0.1.0`). **The client gates compatibility on `proto` alone** — never by parsing
  the `server` string. A major `proto` mismatch is fatal: the server replies
  `unsupported_proto` and closes; the client prints an upgrade hint and exits.

### 3.2 `auth` — unlock this session with the wallet PIN

```json
→ {"op":"auth","pin":"483920"}
← {"ok":true}
← {"ok":false,"error":"bad_pin","attempts_left":2}
← {"ok":false,"error":"locked","retry_after_s":287}
← {"ok":false,"error":"pin_not_set"}    // wallet created before PIN era: run set-pin
← {"ok":false,"error":"pin_unavailable"} // Argon2 backend failure (never a false accept)
```

- The lockout **arms on the response that carries `attempts_left: 0`** — i.e. the
  third consecutive failure answers `bad_pin` with `attempts_left: 0`, *not*
  `locked`. The **next** attempt (auth or high-risk approve) is the first to answer
  `locked`. A client must treat `bad_pin` + `attempts_left: 0` as "now locked".
- `pin_unavailable` is a rare internal fault (the Argon2 hash backend failed); it
  is never a wrong-PIN and never an accept. Surface it as a transient error.

### 3.3 `list` — pending queue summaries

```json
→ {"op":"list"}
← {"ok":true,"pending":[
     {"id":"<preview-uuid>","kind":"send|call","chain_id":1,
      "to":"0x…full-EIP55…","amount_wei":"100000000000000000",
      "risk":"safe|warning","high_risk":false,"not_after_unix":1783100000}
  ]}
```

- Allowed pre-`auth` (§2). Console watch mode polls `list` every 2–3 s.
- `risk` is **two-valued** (`"safe" | "warning"`) — it mirrors the core
  `RiskLevel` enum, which has exactly those variants. There is no `low/medium/high`.
- The list contains **pending *and* executing** items. An item whose `approve` is
  mid-flight (signing/broadcasting) still appears here, indistinguishable from a
  waiting one — the protocol has no interim "executing" state; the terminal answer
  arrives on the in-flight `approve` call, not via `list`.

### 3.4 `get` — full clear-signing card for one item

```json
→ {"op":"get","id":"<preview-uuid>"}
← {"ok":true,"card":{
     "id":"…","chain_id":1,"to":"0x…full-EIP55…","amount_wei":"…decimal…",
     "decoded_call":{ …core DecodedCall, or null… },
     "high_risk":true,"high_risk_reasons":["unlimited_approval"],
     "raw_data":"0x…calldata…","not_after_unix":1783100000}}
← {"ok":false,"error":"unknown_id"}
```

- **No `simulation` field** (deferred by decision A′, 2026-07-09; the card is
  `decoded_call` + `raw_data` in v0.1). The server computes no `SimulationResult`
  here; a future `simulation` field would be additive under proto 1.
- **`decoded_call` may be `null`.** It is the core's decode of a recognised
  drain-vector call (`approve`/`transfer`/`transfer_from`/`set_approval_for_all`/
  `permit`/`permit2_approve`/`increase_allowance`). A bare native transfer or an
  unrecognised selector yields `null` — the console must render the card from `to`
  / `amount_wei` / `raw_data` alone in that case, not assume an object.
- **`raw_data`** is the exact call input (`TransactionBuilder`'s calldata) as
  `0x`-lowercase-hex — the ground truth the decode is derived from. A bare transfer
  with no calldata renders as `"0x"` (never `null`).
- **`high_risk_reasons`** is a closed set: `"unlimited_approval"` (the decoded
  approval is `amount == U256::MAX`) and/or `"txguard_warning"` (the preview's
  `RiskLevel == Warning`). `high_risk` is `true` iff the list is non-empty.
- **`get` on a resolved, expired, or unknown id → `unknown_id`** (never
  `already_resolved`). Once an item leaves the live set — denied, executed, expired,
  or swept after retention — the card is simply gone. The console handles a
  disappeared selection by dropping it, not by expecting a terminal card.
- The card content is **the core's decode, re-derived from the exact bytes to be
  signed** — the console renders it verbatim and adds **no** interpretation of its
  own. Re-basing a value for display (a `0x`-hex `amount` shown in decimal) is
  formatting, not re-derivation, and is allowed; deciding `is_unlimited` from the
  server's flag (never by re-comparing to MAX) keeps invariant #1 intact.

### 3.5 `approve` — resolve as approved (core signs & broadcasts)

```json
→ {"op":"approve","id":"<preview-uuid>"}                  // normal item, authed session
→ {"op":"approve","id":"<preview-uuid>","pin":"483920"}   // high-risk: per-request PIN
← {"ok":true,"state":"executed","tx_hash":"0x…"}
← {"ok":true,"state":"failed","reason":"…broadcast/sign error…"}   // approved, execution failed
← {"ok":false,"error":"unauthorized"}       // no auth on this connection
← {"ok":false,"error":"pin_required"}       // high_risk item without "pin"
← {"ok":false,"error":"bad_pin","attempts_left":1}
← {"ok":false,"error":"locked","retry_after_s":300}
← {"ok":false,"error":"pin_not_set"}        // high-risk approve on a wallet with no PIN
← {"ok":false,"error":"pin_unavailable"}    // Argon2 backend failure
← {"ok":false,"error":"unknown_id"}
← {"ok":false,"error":"already_resolved","state":"executed|denied|expired|failed|pending"}
```

- On approve the **core executes synchronously** (take → sign → broadcast under the
  execute lock) and answers with the terminal state. Single-use: the first
  resolution wins; a second `approve`/`deny` for the same id → `already_resolved`.
- **There is no `error:"expired"` response.** An item that expired before approval
  is no longer live: within the 60-min retention window it answers
  `already_resolved` with `state:"expired"`; after retention it answers
  `unknown_id`. Do not expect a top-level `expired` error here.
- **`already_resolved.state` includes `"pending"`.** `"pending"` means *another
  connection is executing this id right now* (retry or wait) — it arises from the
  real `deny ∥ approve` race: `deny` deliberately runs **outside** the execute lock
  (saying no must stay cheap), so a `deny` arriving while an `approve` is
  mid-execution finds the entry `Executing` and answers `already_resolved` +
  `state:"pending"`. *Implementation note:* the two `approve`-side sources of
  `"pending"` (a racing take, and `TakeOutcome::InFlight`) are **unreachable** under
  the shared execute lock — the take is serialized by that lock — and exist only as
  defensive branches; the deny-vs-approve path is the one a client actually
  observes. The client's serde type for `state` **must** accept `"pending"` or it
  panics on a real everyday response.
- **`pin_not_set`** is reachable here: a high-risk item on a wallet that never set a
  PIN answers `pin_not_set` (the per-request PIN cannot be verified). Documented in
  `auth` too; listed here because high-risk approve is the other entry point.
- High-risk items (`high_risk: true`) require a per-request `pin`; omitting it →
  `pin_required` (the entry is untouched and still approvable with the PIN).

### 3.6 `deny` — resolve as denied

```json
→ {"op":"deny","id":"<preview-uuid>"}
← {"ok":true,"state":"denied"}
← {"ok":false,"error":"unauthorized"}       // no auth on this connection
← {"ok":false,"error":"unknown_id"}
← {"ok":false,"error":"already_resolved","state":"executed|denied|expired|failed|pending"}
```

- Same `unauthorized` / `unknown_id` / `already_resolved` errors as `approve`.
  `deny` never requires a PIN beyond session `auth` — saying no must always be cheap.
- A `deny` that lands while an `approve` for the same id is mid-execution answers
  `already_resolved` + `state:"pending"` (the human cannot interrupt a signature
  already in flight) — this is the everyday origin of the `"pending"` state (§3.5).

### 3.7 Field wire-formats (normative — the console's serde types mirror these)

| Field | Where | Wire form | Example |
|---|---|---|---|
| `ok` | every response | bool | `true` |
| `error` | error responses | string (see §3.8) | `"bad_pin"` |
| `proto` | `hello` | number | `1` |
| `supported` | `unsupported_proto` | array of number | `[1]` |
| `server` | `hello` | string, informational | `"core-server/0.1.0"` |
| `id` | list/card | UUID string, hyphenated lowercase | `"a1b2…-…"` |
| `kind` | list | string enum | `"send"` \| `"call"` |
| `chain_id` | list/card | number (u64) | `1` |
| `to` | list/card (top-level) | **Address via Display → EIP-55 mixed-case**, `0x`+40 hex | `"0x742d35Cc…f44e"` |
| `amount_wei` | list/card (top-level) | **U256 via Display → decimal string** | `"100000000000000000"` |
| `risk` | list | string enum | `"safe"` \| `"warning"` |
| `high_risk` | list/card | bool | `false` |
| `high_risk_reasons` | card | array of string, closed set | `["unlimited_approval","txguard_warning"]` |
| `not_after_unix` | list/card | number (u64 unix seconds) | `1783100000` |
| `raw_data` | card | `0x`+lowercase hex of calldata; `"0x"` if empty | `"0x095ea7b3…"` |
| `decoded_call` | card | object **or `null`** (see below) | — |
| `state` | approve/deny terminal | string enum | `"executed"`\|`"denied"`\|`"expired"`\|`"failed"`\|`"pending"` |
| `tx_hash` | approve executed | `0x`+lowercase hex, 32 bytes (Display `{:#x}`) | `"0x…64 hex…"` |
| `reason` | approve failed | string (operator-masked) | `"…"` |
| `attempts_left` | bad_pin | number | `2` |
| `retry_after_s` | locked | number (seconds) | `287` |

**`decoded_call` sub-fields** — note the encoding differs from the top-level `to`
/ `amount_wei` above: these go through **serde**, not `Display`.

| Sub-field | Wire form | Notes |
|---|---|---|
| `method` | string | `approve`\|`transfer`\|`transfer_from`\|`set_approval_for_all`\|`permit`\|`permit2_approve`\|`increase_allowance` |
| `spender` / `operator` / `from` / `to` / `token` | **Address via serde → `0x`+40 hex LOWERCASE** (no checksum), or `null` | ⚠️ lowercase, unlike top-level `to` |
| `amount` / `deadline` | **U256 via serde → `0x`-prefixed minimal hex string** (`"0x0"`, `"0x16345785d8a0000"`, MAX `"0xff…ff"`), or `null` | ⚠️ hex, unlike top-level `amount_wei` (decimal) |
| `approved` / `is_unlimited` | bool, or `null` | `is_unlimited == true` ⇔ `amount == U256::MAX` |

> Each sub-field is `Option` — an absent field is `null`, **not** a misleading zero
> (e.g. `set_approval_for_all` has no `amount`). A client parsing `amount` must use a
> bignum-safe type (not `u64`/`usize`): a truncated unlimited-approval would defeat
> the very reason the console exists.

### 3.8 Error-code vocabulary (every code the server emits)

| Code | Op(s) | Meaning |
|---|---|---|
| `protocol_error` | any | malformed line, unknown op, wrong field type, request before/after `hello` |
| `oversize` | any | request line > 64 KiB; the connection is then closed (§2) |
| `unsupported_proto` | hello | major `proto` mismatch; server then closes |
| `unauthorized` | approve, deny | no successful `auth` on this connection |
| `bad_pin` | auth, approve | wrong PIN; carries `attempts_left` (0 ⇒ now locked) |
| `locked` | auth, approve | lockout active; carries `retry_after_s` |
| `pin_not_set` | auth, approve | wallet has no PIN record; run `set-pin` |
| `pin_unavailable` | auth, approve | Argon2 backend failure (transient; never an accept) |
| `pin_required` | approve | high-risk item approved without a `pin` |
| `unknown_id` | get, approve, deny | id is not a live item (never parked, resolved+swept, or bad UUID) |
| `already_resolved` | approve, deny | id already terminal (or in-flight); carries `state` |
| `internal` | approve | unreachable post-execute inconsistency (defensive) |

## 4. PIN & lockout semantics (server-side, normative)

- PIN failures are counted **cumulatively across `auth` and `approve.pin`** for the
  wallet, not per connection. **3 consecutive failures** trip the lockout: the
  third failure answers `bad_pin` + `attempts_left: 0` (the arming response), and
  the channel then refuses `auth` / high-risk `approve` with `locked` for
  **5 minutes**. A successful PIN entry before the third resets the counter.
- On lockout the pending queue is **failed closed**: every *pending* item is
  resolved to `denied` (fail-closed — nothing signable is left behind). Note this is
  a *resolution*, not a deletion: the items stay queryable as `denied` for the
  retention window (§5), and any item already `Executing` (the human already
  approved it, signature in flight) is **left untouched**.
- A lockout tripped on **either** path — `auth` or high-risk `approve` — drops the
  queue; both documented lockout paths fail closed identically.
- Verification uses the wallet's Argon2id parameters; comparison is constant-time.
- The server never echoes the PIN back in any response or log.
- **Audit gap (shipped):** a PIN lockout currently emits only a `tracing::warn!`,
  **not** a persisted audit event. The lifecycle audit events
  (`ApprovalDenied` / `Expired` / `PinLockout`) are the deferred work tracked as
  **carry-over #26** (the money path — sign/broadcast — is already audited in
  `router::execute`). This canon states the shipped reality so the observability
  debt is visible, not implied-away.

## 5. Expiry & retention (server-side, normative)

- A parked item expires at `not_after` (default: parked-at + 15 min). Expired items
  resolve to `expired`; the expiry is observed lazily (on the next store access) but
  dated from the real deadline, and is announced to the agent's status polling.
- Terminal states (`executed` / `denied` / `expired` / `failed`) are queryable by
  the agent-side status RPC — and surface as `already_resolved` on the socket — for
  60 min after resolution. After that the entry is swept and reads as `unknown_id`.
- An `Executing` item (approved, signature in flight) does **not** expire: the human
  already decided, the clock stops until the terminal answer lands.

## 6. Compatibility

| proto | core (server) | console (client) |
|-------|---------------|------------------|
| 1     | shipped as `0.1.0`; freezes at `v0.2.0` | ≥ `v0.1.0` |

- **`proto` is the only compatibility gate.** The `server` version string is
  informational (§3.1) — a client must never gate on it. The shipped server reports
  `core-server/0.1.0`; the `v0.2.0` in the freeze column is the release target that
  will freeze proto 1 alongside `console v0.1.0`, **not** a literal the server emits
  today.
- Additive fields are allowed within a major version (clients must ignore unknown
  fields). Anything that changes the meaning of an existing field, an error code, or
  the auth/lockout semantics bumps the major and this table.

## 7. Non-goals

- No server push — the client polls (`list`). Push may come as a proto-2 extension.
- No approval tokens leaving the core, no re-entry of `execute` with a token: the
  core executes on approve; the agent learns the outcome via its own status RPC.
- This socket is not an agent surface. The MCP layer must have **no code path** to
  it (enforced by review + grep in the wallet-image acceptance).
