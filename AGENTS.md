# AGENTS.md — Rustok Console

> Overrides `meta/AGENTS.md` for the `console/` repo. Read `meta/AGENTS.md` first.

## What this is

The **human face** of the Rustok wallet: a terminal approval screen (`rustok-console`)
shipped inside the `rustok-wallet` image and opened by the человек via
`docker exec -it rustok-wallet rustok-console`. The agent face lives in `mcp/`;
the money lives in `core/` (private). Управляющая спека арки:
`core/.claude/specs/2026-07-05-slice1b-tui-approver.md` · ADR:
`core/.claude/decisions/2026-07-05-tui-approver-pin-core-executes.md`.

## Stack

- **Language:** Rust 2024, MSRV 1.95 (same toolchain line as core).
- **Planned deps (C-PR-1):** `ratatui` + `crossterm`, **no async runtime** —
  `crossterm::event::poll(timeout)` loop, Model-View-Update.
- **License discipline:** MIT repo; deps must pass `cargo deny check` (see `deny.toml`).

## Invariants (do not break in any PR)

1. **Display-only client.** The console renders the core's `DecodedCall` + raw
   calldata verbatim — it contains **no decoding/interpretation logic of its own**
   and never re-derives transaction meaning. Re-basing a value for display (a
   `0x`-hex `amount` shown in decimal) is formatting, not re-derivation, and is
   allowed. The `SimulationResult` on the card is deferred to v0.2 (decision A′);
   v0.1 renders `decoded_call` + `raw_data`.
2. **No key material.** No seeds, keys, keystores, or PIN hashes in this codebase,
   its logs, or its tests.
3. **Protocol canon lives here:** `docs/APPROVER-PROTOCOL.md`. Wire changes land in
   that doc first (with a version decision), then in code.
4. **Approve requires a real TTY.** No TTY (`docker exec -i`, pipes) → view-only +
   distinct exit code. Never accept an approval decision from non-interactive stdin.
5. **Default-deny.** Esc / Ctrl-C / timeout / lost TTY / panic → reject. The expiry
   countdown renders on the Reject button, not the Approve one.
6. **PIN is masked** (`●●●`) and never echoed, logged, or persisted client-side.
7. **UI → stderr, machine decisions → stdout** — one JSON line per decision,
   emitted only when stdout is **not** a TTY (interactively stdout shares the
   terminal with the alternate screen and must stay silent). Exit codes report
   the **session end** — aborted / no-tty / fatal / upgrade — keeping their
   v0.1 values. The one-shot "exit code = decision" contract ended with
   residency (Фаза 2 Этап 2); rationale:
   `.claude/decisions/2026-07-12-invariant-7-decision-stream.md`.

## CI

`fmt` · `clippy -D warnings` · `test` · `cargo deny` — all green before merge
(rule №0: nothing is done without a shown green run). Tags `v*` publish the
binary-carrier image `ghcr.io/rustok-org/rustok-console` consumed by
`mcp/Dockerfile.wallet` via `ARG CONSOLE_IMAGE`.
