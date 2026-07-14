#!/usr/bin/env python3
"""DoD smoke — Console v2 Этап 7 Activity.

Real binary in a pty against a proto-2 stub, TWO runs over one log file:
  run 1: unlock → h (server outcomes land as poor rows) → f (filter) →
         a → Enter → y (approve executes) → h (rich row is there INSTANTLY)
         → q. Decision stream: exactly one JSON line on piped stdout.
  run 2: fresh process, stub answers activity with [] — every row must come
         back from the JSONL log on disk (persistence proven).
"""

import fcntl
import json
import os
import pty
import re
import socket
import struct
import subprocess
import sys
import termios
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
SCRATCH = "/tmp/claude-1000/-home-temrjan-Dev/0252c58e-cae7-4976-84d5-5dfa0d509cf8/scratchpad"
SOCK = os.path.join(SCRATCH, "approve7.sock")
LOG = os.path.join(SCRATCH, "console-activity.jsonl")
BIN = os.path.join(HERE, "../../target/debug/rustok-console")

ITEM_ID = "aaaaaaaa-0000-0000-0000-000000000001"
TO = "0x1111222233334444555566667777888899990000"
TX = "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"

state = {"connections": 0, "approved": False}


def serve(listener: socket.socket) -> None:
    while True:
        conn, _ = listener.accept()
        state["connections"] += 1
        first_run = state["connections"] == 1
        f = conn.makefile("rw", encoding="utf-8")
        for line in f:
            req = json.loads(line)
            op = req.get("op")
            if op == "hello":
                resp = {"ok": True, "proto": req["proto"], "server": "stub/0.0"}
            elif op == "auth":
                resp = {"ok": True}
            elif op == "context":
                resp = {"ok": True, "address": TO, "balances": [], "allowed_chains": [1]}
            elif op == "positions":
                resp = {"ok": True, "positions": []}
            elif op == "activity":
                if first_run:
                    resp = {"ok": True, "outcomes": [
                        {"id": "bbbbbbbb-0000-0000-0000-000000000002",
                         "state": "denied", "age_secs": 120},
                        {"id": "cccccccc-0000-0000-0000-000000000003",
                         "state": "expired", "age_secs": 1800}]}
                else:
                    # Run 2: the server window is empty — whatever the view
                    # shows must have come from the file.
                    resp = {"ok": True, "outcomes": []}
            elif op == "list":
                pending = [] if (state["approved"] or not first_run) else [{
                    "id": ITEM_ID, "kind": "send", "chain_id": 1, "to": TO,
                    "amount_wei": "1000000000000000", "risk": "safe",
                    "high_risk": False, "not_after_unix": int(time.time()) + 600}]
                resp = {"ok": True, "pending": pending}
            elif op == "get":
                resp = {"ok": True, "card": {
                    "id": ITEM_ID, "chain_id": 1, "to": TO,
                    "amount_wei": "1000000000000000", "decoded_call": None,
                    "raw_data": "0x", "high_risk": False, "high_risk_reasons": [],
                    "not_after_unix": int(time.time()) + 600}}
            elif op == "approve":
                state["approved"] = True
                resp = {"ok": True, "state": "executed", "tx_hash": TX}
            else:
                resp = {"ok": False, "error": "protocol_error"}
            f.write(json.dumps(resp) + "\n")
            f.flush()


CSI = re.compile(r"\x1b\[([0-9;?]*)([A-Za-z])")


def screen_of(stream: str, rows: int = 24, cols: int = 80) -> list[str]:
    """Rebuild the final screen from a raw ANSI stream: ratatui DIFFS frames
    (a settled screen emits nothing; words split across cursor jumps), so
    substring asserts must run on the reconstructed grid, not the bytes."""
    grid = [[" "] * cols for _ in range(rows)]
    r = c = 0
    i = 0
    while i < len(stream):
        ch = stream[i]
        if ch == "\x1b":
            m = CSI.match(stream, i)
            if m:
                params, final = m.group(1), m.group(2)
                if final == "H":
                    parts = params.split(";") if params else []
                    r = int(parts[0]) - 1 if parts and parts[0] else 0
                    c = int(parts[1]) - 1 if len(parts) > 1 and parts[1] else 0
                    r = min(max(r, 0), rows - 1)
                    c = min(max(c, 0), cols - 1)
                elif final == "J":
                    grid = [[" "] * cols for _ in range(rows)]
                i = m.end()
                continue
            i += 1
            continue
        if ch == "\r":
            c = 0
        elif ch == "\n":
            r = min(r + 1, rows - 1)
        elif ch >= " ":
            grid[r][c] = ch
            c = min(c + 1, cols - 1)
        i += 1
    return ["".join(row) for row in grid]


def on_screen(screen: list[str], *fragments: str) -> bool:
    return all(any(f in row for row in screen) for f in fragments)


def launch():
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    os.set_blocking(master, False)
    proc = subprocess.Popen(
        [BIN], stdin=slave, stdout=subprocess.PIPE, stderr=slave,
        env={**os.environ, "RUSTOK_APPROVE_SOCK": SOCK,
             "RUSTOK_CONSOLE_LOG": LOG, "TERM": "xterm-256color"},
    )
    os.close(slave)
    return proc, master


def main() -> int:
    for p in (SOCK, LOG):
        if os.path.exists(p):
            os.unlink(p)
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(SOCK)
    listener.listen(2)
    threading.Thread(target=serve, args=(listener,), daemon=True).start()

    captured = bytearray()

    def drain(master, seconds: float) -> None:
        end = time.time() + seconds
        while time.time() < end:
            try:
                captured.extend(os.read(master, 65536))
            except BlockingIOError:
                time.sleep(0.05)
            except OSError:
                break

    def press(master, keys: str, wait: float = 0.7) -> None:
        os.write(master, keys.encode())
        drain(master, wait)

    def snap() -> list[str]:
        return screen_of(bytes(captured).decode("utf-8", "replace"))

    # ── run 1 ──
    proc, master = launch()
    drain(master, 1.0)
    press(master, "1\r", wait=1.0)     # PIN → Dashboard
    press(master, "h", wait=0.3)       # → Activity (marks stale)
    drain(master, 3.5)                 # poll → list → activity dispatch
    scr_act = snap()
    press(master, "f", wait=0.6)       # filter → executed (nothing matches)
    scr_filt = snap()
    for _ in range(4):                 # cycle back to All
        press(master, "f", wait=0.2)
    press(master, "a", wait=0.5)       # → Queue
    drain(master, 3.0)                 # a list poll delivers the pending item
    press(master, "\r", wait=1.0)      # open the card
    press(master, "y", wait=1.0)       # approve → executed
    press(master, "h", wait=0.8)       # → Activity: the rich row is INSTANT
    scr_rich = snap()
    press(master, "q", wait=0.5)
    proc.wait(timeout=5)
    stdout_1 = proc.stdout.read().decode()
    os.close(master)

    # ── run 2: fresh process, empty server window — the file must speak ──
    captured.clear()
    proc2, master2 = launch()
    drain(master2, 1.0)
    press(master2, "1\r", wait=1.0)
    press(master2, "h", wait=0.8)
    scr_reload = snap()
    press(master2, "q", wait=0.5)
    proc2.wait(timeout=5)
    os.close(master2)

    log_lines = []
    if os.path.exists(LOG):
        with open(LOG, encoding="utf-8") as fh:
            log_lines = [json.loads(line) for line in fh if line.strip()]

    decision_lines = [line for line in stdout_1.splitlines() if line.strip()]

    checks = [
        ("Activity tab renders with the filter header",
         on_screen(scr_act, "Activity [h]", "filter: all")),
        ("server outcomes render as honest poor rows",
         on_screen(scr_act, "rejected", "expired", "(details not recorded)")),
        ("filter hides what does not match",
         on_screen(scr_filt, "no executed outcomes under this filter")
         and not on_screen(scr_filt, "rejected")),
        ("the decision births a rich row instantly",
         on_screen(scr_rich, "approved", "0.001 ETH")),
        ("addresses are shortened on the list",
         on_screen(scr_rich, "0x111122…0000")
         and not on_screen(scr_rich, TO)),
        ("the tx hash rides the rich row",
         on_screen(scr_rich, "tx 0xdeadbe…beef")),
        ("exactly one decision line on piped stdout (ADR #7)",
         len(decision_lines) == 1 and "approved" in decision_lines[0]
         and TX in decision_lines[0]),
        ("the log file holds all three outcomes",
         len(log_lines) == 3 and {e["id"] for e in log_lines}
         == {ITEM_ID, "bbbbbbbb-0000-0000-0000-000000000002",
             "cccccccc-0000-0000-0000-000000000003"}),
        ("the rich record persisted with card data",
         any(e.get("to") == TO and e.get("tx_hash") == TX for e in log_lines)),
        ("run 2 reloads history from the file (server window empty)",
         on_screen(scr_reload, "approved", "0.001 ETH", "rejected")),
        ("both runs exit aborted (6)",
         proc.returncode == 6 and proc2.returncode == 6),
    ]

    for tag, screen in [("act", scr_act), ("filt", scr_filt),
                        ("rich", scr_rich), ("reload", scr_reload)]:
        with open(os.path.join(SCRATCH, f"screen-{tag}.txt"), "w") as out:
            out.write("\n".join(screen))
    ok = True
    for name, passed in checks:
        print(f"  {'PASS' if passed else 'FAIL'}  {name}")
        ok &= passed
    print(f"\nsmoke: {'PASS' if ok else 'FAIL'} ({sum(p for _, p in checks)}/{len(checks)})")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
