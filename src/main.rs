//! Rustok Console — the human face of the Rustok wallet.
//!
//! Bootstrap stub: the v0.1 client (watch + one-shot approve) lands in C-PR-1.
//! The wire contract it will implement is canonical in `docs/APPROVER-PROTOCOL.md`.
//! Until then this binary only identifies itself and exits non-zero, so nothing
//! can mistake the skeleton for a working approver (fail-closed from day one).

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!(
        "rustok-console {}: not implemented yet — the v0.1 approver client lands in C-PR-1.\n\
         Protocol contract: docs/APPROVER-PROTOCOL.md (proto 1).",
        env!("CARGO_PKG_VERSION")
    );
    ExitCode::FAILURE
}
