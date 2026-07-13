//! Rustok Console — library surface.
//!
//! The binary (`src/main.rs`) is a thin shell over this crate so the protocol,
//! transport, and render layers are unit-testable in isolation. The wire contract
//! is canonical in `docs/APPROVER-PROTOCOL.md`.

pub mod app;
pub mod format;
pub mod protocol;
pub mod qr;
pub mod theme;
pub mod transport;
pub mod ui;
