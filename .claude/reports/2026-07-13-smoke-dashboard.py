#!/usr/bin/env python3
"""DoD smoke — Console v2 Этап 5 Dashboard.

Real binary in a pty against a proto-2 stub server:
  unlock → queue (tab bar) → r → Receive (address + QR) → a → queue → q.
Asserts on the raw stderr stream (UI) and the exit code.
"""

import fcntl
import json
import os
import pty
import socket
import struct
import subprocess
import sys
import termios
import threading
import time

ADDR = "0x489Fe09Fbb489Fe09Fbb489Fe09Fbb489F9Fbbbb"
SOCK = "/tmp/claude-1000/-home-temrjan-Dev/064de351-afaf-44e1-a4c0-aa64358663a5/scratchpad/approve.sock"
BIN = "/home/temrjan/Dev/projects/rustok/console/target/debug/rustok-console"


def serve(listener: socket.socket) -> None:
    conn, _ = listener.accept()
    f = conn.makefile("rw", encoding="utf-8")
    for line in f:
        req = json.loads(line)
        op = req.get("op")
        if op == "hello":
            resp = {"ok": True, "proto": req["proto"], "server": "stub/0.0"}
        elif op == "auth":
            resp = {"ok": True}
        elif op == "context":
            resp = {"ok": True, "address": ADDR,
                    "balances": [{"chain_id": 1, "symbol": "ETH", "balance": "10000000000000000"}],
                    "allowed_chains": [1]}
        elif op == "positions":
            resp = {"ok": True, "positions": [{
                "protocol": "aave_v3", "chain_id": 1,
                "asset_address": "0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2",
                "asset_symbol": "USD", "asset_name": "Aave v3 account",
                "asset_decimals": 8, "balance": "100000000000", "balance_formatted": "1000",
                "extra": {"health_factor": "∞", "ltv": "80%"}}]}
        elif op == "list":
            resp = {"ok": True, "pending": []}
        else:
            resp = {"ok": False, "error": "protocol_error"}
        f.write(json.dumps(resp) + "\n")
        f.flush()


def main() -> int:
    if os.path.exists(SOCK):
        os.unlink(SOCK)
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(SOCK)
    listener.listen(1)
    threading.Thread(target=serve, args=(listener,), daemon=True).start()

    # stdin + stderr on the pty (interactive UI), stdout piped (decision stream).
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    os.set_blocking(master, False)
    proc = subprocess.Popen(
        [BIN],
        stdin=slave,
        stdout=subprocess.PIPE,
        stderr=slave,
        env={**os.environ, "RUSTOK_APPROVE_SOCK": SOCK, "TERM": "xterm-256color"},
    )
    os.close(slave)

    captured = bytearray()

    def drain(seconds: float) -> None:
        end = time.time() + seconds
        while time.time() < end:
            try:
                captured.extend(os.read(master, 65536))
            except BlockingIOError:
                time.sleep(0.05)
            except OSError:
                break

    def press(keys: str, wait: float = 0.7) -> None:
        os.write(master, keys.encode())
        drain(wait)

    drain(1.0)                # connect + hello → unlock screen
    press("1\r", wait=1.0)    # PIN → auth ok → context → HOME = Dashboard
    drain(3.5)                # первый poll-цикл: list → positions
    dash_frame = bytes(captured)
    captured.clear()
    press("a", wait=0.7)      # → Queue
    queue_frame = bytes(captured)
    captured.clear()
    press("d", wait=0.7)      # → назад на Dashboard
    back_frame = bytes(captured)
    press("q", wait=0.5)      # quit
    proc.wait(timeout=5)
    os.close(master)

    text_dash = dash_frame.decode("utf-8", "replace")
    text_queue = queue_frame.decode("utf-8", "replace")
    text_back = back_frame.decode("utf-8", "replace")

    checks = [
        ("home = Dashboard (title after PIN)", "Dashboard" in text_dash),
        ("three tabs with keys", "Dashboard [d]" in text_dash and "Queue·0 [a]" in text_dash and "Receive [r]" in text_dash),
        ("waiting block", "Waiting for you" in text_dash),
        ("balance humanized per chain", "chain 1" in text_dash and "0.01 ETH" in text_dash),
        ("position row verbatim", "aave_v3" in text_dash and "1000 USD" in text_dash),
        ("extra display strings verbatim", "∞" in text_dash and "80%" in text_dash),
        ("'a' switches to the queue", "Queue" in text_queue),
        ("'d' returns to the dashboard", "Dashboard" in text_back),
        ("exit code = aborted (6)", proc.returncode == 6),
        ("stdout stayed clean (no decisions)", proc.stdout.read() == b""),
    ]


    ok = True
    for name, passed in checks:
        print(f"  {'PASS' if passed else 'FAIL'}  {name}")
        ok &= passed
    print(f"\nsmoke: {'PASS' if ok else 'FAIL'} ({sum(p for _, p in checks)}/{len(checks)})")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
