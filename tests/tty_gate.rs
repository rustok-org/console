//! Invariant #4, end to end: an approval may never come from a pipe. The gate
//! is the first thing `main` runs — before the terminal, before the socket —
//! so this is testable without either.

use std::process::{Command, Stdio};

/// `EXIT_NO_TTY` in `main.rs` (the binary's constants are not importable).
const EXIT_NO_TTY: i32 = 3;

#[test]
fn a_piped_stdin_is_refused_before_anything_opens() {
    let out = Command::new(env!("CARGO_BIN_EXE_rustok-console"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("the binary runs");

    assert_eq!(
        out.status.code(),
        Some(EXIT_NO_TTY),
        "a non-interactive stdin exits with the distinct no-tty code"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Approval from a pipe is never accepted."),
        "the refusal names itself on stderr: {stderr}"
    );
    assert!(
        out.stdout.is_empty(),
        "stdout carries decisions only (invariant #7) — a refusal is not one"
    );
}
