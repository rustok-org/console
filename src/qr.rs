//! The Receive view's QR code — the wallet address as terminal text.
//!
//! The module matrix comes from `qrcodegen` (Project Nayuki's reference
//! implementation); everything visual is owned here: the half-block packing
//! (two module rows per text row), the quiet zone, and nothing else. The text
//! encoded is **exactly the string the screen shows** — the bare EIP-55
//! address, no URI scheme, no re-casing (`AGENTS.md` #1: verbatim; Gate-1
//! ratification 2026-07-13).

use qrcodegen::{QrCode, QrCodeEcc};

/// Quiet-zone width in modules (ISO/IEC 18004 §9.1). The terminal theme
/// around the code cannot be trusted to be light, so the light border is
/// drawn explicitly as part of these rows.
const QUIET_ZONE: i32 = 4;

/// Render `text` as a QR code in half-block characters, two module rows per
/// text row: `█` both dark, `▀` top dark, `▄` bottom dark, space both light.
/// Every row is exactly `size + 2 × QUIET_ZONE` characters — the caller
/// checks the rows against its viewport and shows an explicit marker instead
/// when they do not fit; a wrapped or clipped QR must never render (it would
/// still look scannable).
///
/// `None` when there is nothing to encode — an empty text (a scannable QR of
/// an empty string would be a lie on a receive surface) or a payload no QR
/// version can hold. ECC level M: same 29×29 version 3 as level L for a
/// 42-character address, with more damage tolerance.
#[must_use]
pub fn half_block_rows(text: &str) -> Option<Vec<String>> {
    if text.is_empty() {
        return None;
    }
    let qr = QrCode::encode_text(text, QrCodeEcc::Medium).ok()?;
    let size = qr.size();

    let cols = size + 2 * QUIET_ZONE;
    let mut rows = Vec::new();
    // Module rows come in pairs; `get_module` answers light (false) outside
    // the code, which paints the quiet zone and the odd final half-row alike.
    let mut y = -QUIET_ZONE;
    while y < size + QUIET_ZONE {
        let mut row = String::with_capacity(usize::try_from(cols).ok()? * '█'.len_utf8());
        for x in -QUIET_ZONE..size + QUIET_ZONE {
            row.push(match (qr.get_module(x, y), qr.get_module(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        rows.push(row);
        y += 2;
    }
    Some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The full-length EIP-55 address the rest of the suite uses — the real
    /// payload shape (42 bytes → version 3), never a shortened stand-in.
    const ADDR: &str = "0x489Fe09Fbb489Fe09Fbb489Fe09Fbb489F9Fbbbb";

    /// A rendered character, back into its (top, bottom) module pair.
    fn unpack(c: char) -> (bool, bool) {
        match c {
            '█' => (true, true),
            '▀' => (true, false),
            '▄' => (false, true),
            ' ' => (false, false),
            other => panic!("a QR row may only carry half-blocks, got {other:?}"),
        }
    }

    #[test]
    fn the_rendered_qr_round_trips_to_the_exact_module_matrix() {
        // Polarity, row pairing, and off-by-one all die here: every character
        // is unpacked and compared to the encoder's own matrix, module by
        // module (out-of-range reads answer light — the quiet zone included).
        let qr = QrCode::encode_text(ADDR, QrCodeEcc::Medium).unwrap();
        let rows = half_block_rows(ADDR).unwrap();
        for (r, row) in rows.iter().enumerate() {
            for (c, ch) in row.chars().enumerate() {
                let (top, bottom) = unpack(ch);
                let x = i32::try_from(c).unwrap() - QUIET_ZONE;
                let y = i32::try_from(r).unwrap() * 2 - QUIET_ZONE;
                assert_eq!(top, qr.get_module(x, y), "top module at ({x},{y})");
                assert_eq!(
                    bottom,
                    qr.get_module(x, y + 1),
                    "bottom module at ({x},{})",
                    y + 1
                );
            }
        }
    }

    #[test]
    fn a_four_module_quiet_zone_surrounds_the_code() {
        // Asserted in MODULE space: 29 code rows + 8 quiet make 37 — odd — so
        // the final text row pairs the last quiet module row with a padding
        // half-row that must read light too. A dark cell in any of these
        // regions would sit inside the scanner's required light border.
        let rows = half_block_rows(ADDR).unwrap();
        let cols = rows[0].chars().count();
        for (r, row) in rows.iter().enumerate() {
            for (c, ch) in row.chars().enumerate() {
                let (top, bottom) = unpack(ch);
                // Horizontal quiet zone: the first and last 4 columns.
                if c < 4 || c >= cols - 4 {
                    assert!(!top && !bottom, "dark module in the side quiet zone");
                }
                // Vertical quiet zone: text rows 0–1 (module rows -4..0) and
                // the last two (module rows 30..34, incl. the padding row).
                if r < 2 || r >= rows.len() - 2 {
                    assert!(!top && !bottom, "dark module in the vertical quiet zone");
                }
            }
        }
    }

    #[test]
    fn the_test_address_renders_as_version_3_with_its_quiet_zone() {
        // 42 bytes at ECC M → version 3, a 29×29 matrix: 37 columns and
        // ceil(37/2) = 19 text rows once the quiet zone is on. The Receive
        // view's fit checks are sized against exactly these numbers.
        let rows = half_block_rows(ADDR).unwrap();
        assert_eq!(rows.len(), 19, "19 half-block rows");
        for row in &rows {
            assert_eq!(row.chars().count(), 37, "37 columns, every row");
        }
    }

    #[test]
    fn encoding_is_deterministic() {
        // Snapshot tests downstream rely on the same input always rendering
        // the same rows.
        assert_eq!(half_block_rows(ADDR), half_block_rows(ADDR));
    }

    #[test]
    fn an_empty_text_yields_no_qr() {
        // A scannable QR of an empty string would be a lie on a receive
        // surface (/check-4): the degraded screen shows no code at all.
        assert_eq!(half_block_rows(""), None);
    }
}
