//! Human-readable formatting for the card — pure string functions, no rendering.
//!
//! Everything here is bignum-safe by operating on the decimal/hex **strings** the
//! core sends (`AGENTS.md` #1: the console never re-derives a value, only
//! re-bases it for display). A wallet must never truncate an amount, so `u64` /
//! `u128` are deliberately avoided — a `U256` wei value can exceed both.

/// Wei in one ether (`10^18`).
const ETH_DECIMALS: usize = 18;

/// Format a native **decimal wei** string as ether, e.g. `"10000000000000000"`
/// → `"0.01 ETH"`. Trailing fractional zeros are trimmed. Non-numeric input is
/// returned verbatim (defensive — the card shows the truth rather than crashing).
#[must_use]
pub fn wei_to_eth(wei: &str) -> String {
    let digits = wei.trim();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return wei.to_owned();
    }

    // Drop leading zeros; keep one if the whole value is zero.
    let significant = digits.trim_start_matches('0');
    let significant = if significant.is_empty() {
        "0"
    } else {
        significant
    };

    let (int_part, frac_part) = if significant.len() > ETH_DECIMALS {
        let split = significant.len() - ETH_DECIMALS;
        (
            significant[..split].to_owned(),
            significant[split..].to_owned(),
        )
    } else {
        // Fewer than 18 significant digits: whole value is fractional.
        ("0".to_owned(), format!("{significant:0>ETH_DECIMALS$}"))
    };

    let frac = frac_part.trim_end_matches('0');
    if frac.is_empty() {
        format!("{int_part} ETH")
    } else {
        format!("{int_part}.{frac} ETH")
    }
}

/// Whether a native decimal wei string carries no value. A token transfer or an
/// `approve` sends `0` native wei with the real amount in `decoded_call`, so the
/// card must NOT headline `"0 ETH"` when this is true — the decoded call leads.
#[must_use]
pub fn is_zero_wei(wei: &str) -> bool {
    let digits = wei.trim();
    !digits.is_empty() && digits.bytes().all(|b| b == b'0')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wei_to_eth_formats_common_amounts() {
        assert_eq!(wei_to_eth("10000000000000000"), "0.01 ETH");
        assert_eq!(wei_to_eth("1000000000000000000"), "1 ETH");
        assert_eq!(wei_to_eth("0"), "0 ETH");
        assert_eq!(wei_to_eth("1"), "0.000000000000000001 ETH");
        assert_eq!(wei_to_eth("1500000000000000000"), "1.5 ETH");
    }

    #[test]
    fn wei_to_eth_is_bignum_safe_past_u128() {
        // U256::MAX (78 digits) — well past u128::MAX (39 digits); the string math
        // must format it exactly, digit-for-digit, with no truncation.
        assert_eq!(
            wei_to_eth(
                "115792089237316195423570985008687907853269984665640564039457584007913129639935"
            ),
            "115792089237316195423570985008687907853269984665640564039457.\
             584007913129639935 ETH"
        );
    }

    #[test]
    fn wei_to_eth_returns_non_numeric_verbatim() {
        assert_eq!(wei_to_eth("not-a-number"), "not-a-number");
        assert_eq!(wei_to_eth(""), "");
    }

    #[test]
    fn is_zero_wei_detects_a_token_op() {
        assert!(is_zero_wei("0"));
        assert!(is_zero_wei("000"));
        assert!(!is_zero_wei("1"));
        assert!(!is_zero_wei("10000000000000000"));
        assert!(!is_zero_wei(""));
    }
}
