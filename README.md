# Rustok Console

**The human face of the [Rustok](https://rustokwallet.com) self-custody wallet.**

Rustok gives an AI agent a wallet; Rustok Console gives the *human* the final word.
It is a terminal approval screen you keep open in a second window next to your agent
session: when the agent asks the wallet to move money, the transaction is **parked**
inside the wallet core and shows up here as a clear-signing card — decoded by the
core itself, never by the agent. You read what is actually being signed and press
`y` or `n`. Silence means no.

```
┌ window 1 ─────────────────┐   ┌ window 2 ──────────────────────┐
│ you ↔ agent (chat / MCP)  │   │ $ docker exec -it rustok-wallet \
│                           │   │       rustok-console            │
│ agent: "payment is parked │   │ 🔔 PENDING  send 0.1 ETH        │
│  — please approve in your │   │    to 0x7a25…c488 (full addr)   │
│  wallet window"           │   │    From: your wallet → To: …    │
│                           │   │    [y] approve   [n] reject     │
└───────────────────────────┘   └────────────────────────────────┘
```

## Trust model, in three lines

1. **The card renders the core's decode, not the agent's words.** A lying agent
   cannot change what you see.
2. **Approval is unlocked by a PIN** printed once, in *your* terminal, when the
   wallet is created. The agent has never seen it and cannot type it.
3. **Fail-closed everywhere.** No console open, no PIN, no answer, expired card,
   piped stdin instead of a real TTY — everything resolves to *reject*.

## Usage

The console ships inside the `rustok-wallet` Docker image (it is copied from this
repo's published image at wallet-image build time). With the wallet container
running under its fixed name:

```bash
docker exec -it rustok-wallet rustok-console   # the resident wallet console
```

Run this in **your own terminal window** — never through the agent session
(otherwise the PIN would land in the agent's context).

## Status

**v0.2 — the resident wallet console.** PIN-unlock opens a Dashboard (per-chain
balances, DeFi positions, "waiting for you" count); the queue view carries
clear-signing cards with a full From→To block (complete EIP-55 addresses, literal
**UNLIMITED** for infinite approvals, raw calldata); Receive shows the wallet's
address with a QR of the exact same string; Activity keeps a decision history that
outlives the core's retention window (a local journal, written the moment you
decide). The console no longer exits per decision: outcomes stream as one JSON
line each to a **non-TTY stdout** for machine callers, and exit codes report only
how the *session* ended. The canonical core↔console contract lives in
[`docs/APPROVER-PROTOCOL.md`](docs/APPROVER-PROTOCOL.md) (proto 2), negotiated
with a `hello` version handshake — a v0.1 client keeps working unchanged.

Still out of scope by design: originating transactions. Every surface here is
display or approval — the console lets a human say no; it never lets anyone
(including the human) bypass the agent-proposed, core-decoded flow.

## Related

- [`rustok-org/mcp`](https://github.com/rustok-org/mcp) — the agent face: MCP server + `rustok-wallet` all-in-one image.
- Wallet core — proprietary; distributed as a binary image (`ghcr.io/rustok-org/rustok-core`).

## License

[MIT](LICENSE) © 2026 Rustok Org
