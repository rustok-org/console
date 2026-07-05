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
│ agent: "payment is parked │   │ 🔔 PENDING  swap 0.1 ETH → USDT │
│  — please approve in your │   │    to 0x7a25…c488 (full addr)   │
│  wallet window"           │   │    simulation: ok · risk: low   │
│                           │   │    [y] approve   [N] reject     │
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
docker exec -it rustok-wallet rustok-console          # watch mode (default)
docker exec -it rustok-wallet rustok-console approve  # one-shot: approve/deny and exit
```

Run this in **your own terminal window** — never through the agent session
(otherwise the PIN would land in the agent's context).

## Status

**v0.1 in development.** This repository currently pins the crate/CI skeleton and
the canonical core↔console protocol contract: [`docs/APPROVER-PROTOCOL.md`](docs/APPROVER-PROTOCOL.md).
The wallet core implements the server side of that contract; compatibility is
negotiated with a `hello` version handshake.

Planned for v0.1: single-screen watch mode (pending queue + card), one-shot approve,
session PIN (`auth` once per session, per-transaction PIN only for high-risk items),
clear-signing cards (full EIP-55 addresses, literal **UNLIMITED** for infinite
approvals, raw calldata behind a scroll).

Out of scope by design: balances, history, dashboards — that is the agent's job in
chat. The console does exactly one thing: it lets a human say no.

## Related

- [`rustok-org/mcp`](https://github.com/rustok-org/mcp) — the agent face: MCP server + `rustok-wallet` all-in-one image.
- Wallet core — proprietary; distributed as a binary image (`ghcr.io/rustok-org/rustok-core`).

## License

[MIT](LICENSE) © 2026 Rustok Org
