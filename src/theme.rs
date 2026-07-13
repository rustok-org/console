//! Brand palette — one place to change a console color.
//!
//! Values are the Rustok website tokens (rustok-landing), so the terminal reads
//! as the same product as the site. Colors are addressed by **role**
//! (`approve`, `high_risk`, `frame`, …), never by raw `Color::Rgb` scattered
//! through the renderer: a brand change lives here alone.
//!
//! `NO_COLOR` (https://no-color.org) is honored — when it is set, every role
//! degrades to the terminal's own default (`Color::Reset`), keeping the layout
//! and text identical, just uncolored.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

/// Whether to emit color at all. Read once: `NO_COLOR` does not change mid-run,
/// and the renderer asks for a role on every frame.
fn color_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("NO_COLOR").is_none())
}

/// A brand color, or the terminal default under `NO_COLOR`. Split from the env
/// read so the degradation itself is a pure, testable function.
fn resolve(enabled: bool, r: u8, g: u8, b: u8) -> Color {
    if enabled {
        Color::Rgb(r, g, b)
    } else {
        Color::Reset
    }
}

fn role(r: u8, g: u8, b: u8) -> Color {
    resolve(color_enabled(), r, g, b)
}

// --- roles ---------------------------------------------------------------

/// Primary readable text.
pub fn ink() -> Color {
    role(0xED, 0xE7, 0xFF)
}

/// Secondary text — field labels, captions.
pub fn muted() -> Color {
    role(0x8E, 0x7A, 0xC4)
}

/// Dimmest text — hints, disabled affordances.
pub fn faint() -> Color {
    role(0x5E, 0x4F, 0x86)
}

/// Panel borders / frames — the soft border canon (`#2A1C4D`). The bright accent
/// (`accent`, `#9D5CFF`) is reserved for an emphasized / focused frame.
pub fn frame() -> Color {
    role(0x2A, 0x1C, 0x4D)
}

/// Brand accent — headings, emphasis.
pub fn accent() -> Color {
    role(0x9D, 0x5C, 0xFF)
}

/// Bright accent — addresses, the one thing the eye should land on.
pub fn accent_bright() -> Color {
    role(0xC9, 0xA2, 0xFF)
}

/// Semantic: an approved / executed outcome (not the same as the brand accent).
pub fn approve() -> Color {
    role(0x16, 0xE0, 0xC3)
}

/// Semantic: read-this-carefully — high-risk, warnings.
pub fn high_risk() -> Color {
    role(0xFF, 0xB4, 0x54)
}

/// Semantic: a rejected / denied outcome.
pub fn reject() -> Color {
    role(0xFF, 0x6B, 0x6B)
}

// --- style helpers -------------------------------------------------------

/// A field label — muted, understated.
pub fn label_style() -> Style {
    Style::new().fg(muted())
}

/// A value the user reads — primary ink.
pub fn value_style() -> Style {
    Style::new().fg(ink())
}

/// A section heading / panel title — accent, bold.
pub fn heading_style() -> Style {
    Style::new().fg(accent()).add_modifier(Modifier::BOLD)
}

/// A high-risk line — amber, bold, so it cannot be skimmed past.
pub fn high_risk_style() -> Style {
    Style::new().fg(high_risk()).add_modifier(Modifier::BOLD)
}

/// QR modules — true black on true white, whatever the brand palette or the
/// terminal theme: a scanner's contrast floor is a function, not branding.
/// Under `NO_COLOR` both drop to the terminal default and the half-block
/// characters alone carry the polarity (dark module = block) — correct on a
/// light terminal, inverted on a dark one, which most scanners still read.
pub fn qr_style() -> Style {
    Style::new()
        .fg(role(0x00, 0x00, 0x00))
        .bg(role(0xFF, 0xFF, 0xFF))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_degrades_every_role_to_the_terminal_default() {
        // Colored: the exact brand RGB. Uncolored (NO_COLOR): the terminal default,
        // so the layout and text are untouched — only the color is dropped.
        assert_eq!(
            resolve(true, 0x16, 0xE0, 0xC3),
            Color::Rgb(0x16, 0xE0, 0xC3)
        );
        assert_eq!(resolve(false, 0x16, 0xE0, 0xC3), Color::Reset);
    }
}
