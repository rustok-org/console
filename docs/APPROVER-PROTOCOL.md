# Approver Protocol — core ↔ console contract

> **Canonical.** This document is the source of truth for the wire protocol between
> the Rustok wallet core (server) and Rustok Console (client). The core implements
> the server side; any change lands HERE first, then in code on both sides.
>
> **proto 1 — DRAFT** until the first core (PR-2) + console (C-PR-1) implementations
> ship; frozen on the `console v0.1.0` / `core v0.2.0` release pair.

## 1. Transport

- **UNIX stream socket** at `/run/wallet/approve.sock` inside the `rustok-wallet`
  container. The directory `/run/wallet/` is mode `0700`.
- **JSON Lines**: every message is a single JSON object on one line, UTF-8,
  terminated by `\n`. Client sends a request line; server answers with exactly one
  response line. No pipelining: one request in flight per connection.
- The client is `rustok-console` (launched via `docker exec`); the server is
  `core-server`. **No key material, seed words, or PIN hashes ever cross this
  socket.** The PIN itself crosses it only inside `auth` / high-risk `approve`
  requests, plaintext over the local socket (same-host, fs-permission-guarded);
  it is never logged on either side.

## 2. Session

```
connect → hello → auth → { list | get | approve | deny }* → disconnect
```

- A **session is one connection**. `auth` authorizes that connection only;
  disconnect requires a fresh `auth`. There is no token, cookie, or resume.
- `hello` is mandatory first. Requests before a successful `hello` → `protocol_error`.
- `approve` / `deny` before a successful `auth` → `unauthorized`.
- **Default-deny**: malformed line, unknown `op`, unknown field type, oversized
  message (> 64 KiB) → single `error` response, no state change. The server never
  executes anything as a result of an ambiguous message.

## 3. Messages

### 3.1 `hello` — version handshake

```json
→ {"op":"hello","proto":1,"client":"rustok-console/0.1.0"}
← {"ok":true,"proto":1,"server":"core-server/0.2.0"}
← {"ok":false,"error":"unsupported_proto","supported":[1]}   // then server closes
```

Major version mismatch is fatal: the client must print an upgrade hint and exit.

### 3.2 `auth` — unlock this session with the wallet PIN

```json
→ {"op":"auth","pin":"483920"}
← {"ok":true}
← {"ok":false,"error":"bad_pin","attempts_left":2}
← {"ok":false,"error":"locked","retry_after_s":287}
← {"ok":false,"error":"pin_not_set"}    // wallet created before PIN era: run set-pin
```

### 3.3 `list` — pending queue summaries

```json
→ {"op":"list"}
← {"ok":true,"pending":[
     {"id":"<preview-uuid>","kind":"send|call","chain_id":1,
      "to":"0x…full-eip55…","amount_wei":"100000000000000000",
      "risk":"low|medium|high","high_risk":false,"not_after_unix":1783100000}
  ]}
```

`list` is allowed pre-`auth` (view is not a privilege; deciding is). Console watch
mode polls `list` every 2–3 s.

### 3.4 `get` — full clear-signing card for one item

```json
→ {"op":"get","id":"<preview-uuid>"}
← {"ok":true,"card":{
     "id":"…","chain_id":1,"to":"0x…","amount_wei":"…",
     "decoded_call":{ …core DecodedCall: method/spender/amount/is_unlimited/deadline… },
     "simulation":{ …core SimulationResult: revert tri-state, value delta, gas… },
     "high_risk":true,"high_risk_reasons":["unlimited_approval"],
     "raw_data":"0x…calldata…","not_after_unix":1783100000}}
← {"ok":false,"error":"unknown_id"}
```

The card content is **the core's decode, re-derived from the exact bytes to be
signed** — the console renders it verbatim and adds no interpretation of its own.

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
← {"ok":false,"error":"expired"}
← {"ok":false,"error":"unknown_id"}
← {"ok":false,"error":"already_resolved","state":"executed|denied|expired|failed"}
```

On approve the **core executes synchronously** (take → sign → broadcast under the
execute lock) and answers with the terminal state. Single-use: the first resolution
wins; a second `approve`/`deny` for the same id → `already_resolved`.

### 3.6 `deny` — resolve as denied

```json
→ {"op":"deny","id":"<preview-uuid>"}
← {"ok":true,"state":"denied"}
```

Same `unauthorized` / `unknown_id` / `already_resolved` errors as `approve`.
`deny` never requires a PIN beyond session `auth` — saying no must always be cheap.

## 4. PIN & lockout semantics (server-side, normative)

- PIN failures are counted **cumulatively across `auth` and `approve.pin`** for the
  wallet, not per connection. **3 consecutive failures** → `locked`: all pending
  items are dropped (fail-closed), the channel refuses `auth`/`approve` for
  **5 minutes**, an audit event is written. A successful PIN entry resets the counter.
- Verification uses the wallet's Argon2id parameters; comparison is constant-time.
- The server never echoes the PIN back in any response or log.

## 5. Expiry & retention (server-side, normative)

- A parked item expires at `not_after` (default: parked-at + 15 min). Expired items
  resolve to `expired` and are announced to the agent's status polling.
- Terminal states (`executed` / `denied` / `expired` / `failed`) are queryable by the
  agent-side status RPC for 60 min after resolution.

## 6. Compatibility

| proto | core (server) | console (client) |
|-------|---------------|------------------|
| 1     | ≥ v0.2.0      | ≥ v0.1.0         |

Additive fields are allowed within a major version (clients must ignore unknown
fields). Anything that changes the meaning of an existing field, an error code, or
the auth/lockout semantics bumps the major and this table.

## 7. Non-goals

- No server push — the client polls (`list`). Push may come as a proto-2 extension.
- No approval tokens leaving the core, no re-entry of `execute` with a token: the
  core executes on approve; the agent learns the outcome via its own status RPC.
- This socket is not an agent surface. The MCP layer must have **no code path** to
  it (enforced by review + grep in the wallet-image acceptance).
