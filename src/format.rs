//! Human-readable formatting for the card — pure string functions, no rendering.
//!
//! Everything here is bignum-safe by operating on the decimal/hex **strings** the
//! core sends (`AGENTS.md` #1: the console never re-derives a value, only
//! re-bases it for display). A wallet must never truncate an amount, so `u64` /
//! `u128` are deliberately avoided — a `U256` wei value can exceed both.

/// Wei in one ether (`10^18`).
const ETH_DECIMALS: usize = 18;

/// The wei string as ASCII decimal digits, or `None` when the core sent something
/// this module will not re-derive.
///
/// The ONE place "is this a number" is decided, so the exact form ([`wei_to_eth`])
/// and the shortened one ([`short_eth`]) cannot come to different answers about
/// the same wire value.
fn decimal_wei(wei: &str) -> Option<&str> {
    let digits = wei.trim();
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some(digits)
}

/// Format a native **decimal wei** string as ether, e.g. `"10000000000000000"`
/// → `"0.01 ETH"`. Trailing fractional zeros are trimmed. Non-numeric input is
/// returned verbatim (defensive — the card shows the truth rather than crashing).
#[must_use]
pub fn wei_to_eth(wei: &str) -> String {
    let Some(digits) = decimal_wei(wei) else {
        return wei.to_owned();
    };

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

/// Human name for an EVM chain id, or the id itself when we do not know it.
///
/// Display formatting, not re-derivation (`AGENTS.md` #1): the core's value is
/// the id, and an unknown one is shown as the number rather than guessed at —
/// a wrong chain name on a payment screen is worse than a bare number.
///
/// The names live HERE, in code, not in configuration: which networks a wallet
/// speaks of is a product decision, and letting an operator label the network on
/// the screen where money moves is the wrong kind of flexibility. A name is added
/// **together with support for the chain**, never ahead of it — `Arbitrum` on a
/// wallet that cannot reach Arbitrum would read as "it works" (Captain, 2026-08-08).
#[must_use]
pub fn network_name(chain_id: u64) -> String {
    match chain_id {
        1 => "Ethereum".to_owned(),
        8453 => "Base".to_owned(),
        42161 => "Arbitrum".to_owned(),
        other => format!("chain {other}"),
    }
}

/// Fractional digits a SCAN surface keeps. Six is roughly a micro-ether — enough
/// to tell two rows apart while triaging, and short enough that the column holds.
const SCAN_FRAC_DIGITS: usize = 6;

/// Shorten an ether amount for a SCAN surface — the queue, the balance panel and
/// Activity: `0.00549906802239073 ETH` → `0.005499… ETH`.
///
/// **Never on the card.** The card is where the human decides how much leaves the
/// wallet, and it renders the exact value ([`wei_to_eth`]) — the same boundary
/// [`short_addr`] keeps for addresses.
///
/// What it does, in order:
/// - digits of the whole part are grouped: `1,234.5 ETH`;
/// - a value with no fraction at all — a real zero among them — is done there
///   and never meets the floor below;
/// - the fraction is cut to [`SCAN_FRAC_DIGITS`] — **truncated, never rounded**,
///   because a wallet must not display more than there is — and a `…` marks that
///   digits were dropped;
/// - a value too small to survive that cut reads `<0.000001 ETH` rather than
///   `0.000000…`, so dust never looks like nothing.
#[must_use]
pub fn short_eth(wei: &str) -> String {
    let exact = wei_to_eth(wei);
    if decimal_wei(wei).is_none() {
        return exact;
    }
    let number = exact.strip_suffix(" ETH").unwrap_or(&exact);
    short_amount(number, "ETH")
}

/// The same shortening, over an amount the core has **already** rendered — a
/// token's `balance_formatted` — with the unit that amount is in.
///
/// This is the half of [`short_eth`] that never knew anything about ether: it
/// takes a plain decimal string and returns a scan-sized one. Tokens go through
/// it directly, so a USDC row and an ETH row speak the same language of digits
/// — same grouping, same six-digit cut, same `…`, same dust floor — without the
/// console ever re-deriving an amount from raw units (`AGENTS.md` #1).
///
/// Input that is not a plain decimal is returned verbatim, unit and all left
/// off: display shows the truth rather than dressing up something it could not
/// read.
#[must_use]
pub fn short_amount(number: &str, unit: &str) -> String {
    let (int_part, frac) = number.split_once('.').unwrap_or((number, ""));
    let plain = !int_part.is_empty()
        && int_part.bytes().all(|b| b.is_ascii_digit())
        && frac.bytes().all(|b| b.is_ascii_digit());
    if !plain {
        return number.to_owned();
    }
    let whole = group_thousands(int_part);
    // Nothing to shorten — and this is also what keeps a REAL zero a zero: an
    // exact `0 ETH` has no fraction at all, so it never reaches the dust floor
    // below (`short_eth_keeps_a_real_zero_a_zero` fails if this return goes).
    if frac.is_empty() {
        return format!("{whole} {unit}");
    }

    // Truncate, never round: a wallet must not display more than there is.
    let kept: String = frac.chars().take(SCAN_FRAC_DIGITS).collect();
    let dropped = frac.chars().count() > SCAN_FRAC_DIGITS;
    // Everything that survived the cut is zero, yet digits were dropped — so the
    // value is small, not absent. Say that with a floor rather than render
    // `0.000000…`, which reads as none. `dropped` is what makes the test
    // "all kept digits are zero" mean something: on an empty fraction it would
    // hold vacuously, and an exact zero would come out as `<0.000001 ETH`.
    if dropped && int_part == "0" && kept.bytes().all(|b| b == b'0') {
        return format!("<0.{:0>SCAN_FRAC_DIGITS$} {unit}", 1);
    }
    let marker = if dropped { "…" } else { "" };
    format!("{whole}.{kept}{marker} {unit}")
}

/// Separate a run of ASCII digits into groups of three: `120000000` →
/// `120,000,000`. A comma, not a space — the interface is English, and a space
/// inside a table cell reads as two numbers.
fn group_thousands(digits: &str) -> String {
    let n = digits.len();
    let mut out = String::with_capacity(n + n / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (n - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Shorten an address for a DISPLAY-LIST row: `0x489Fe0…bbbb` (first 6 + last 4
/// hex digits, EIP-55 casing preserved verbatim). **Never on a signing or
/// approving surface** — the card, From→To and Receive render addresses in full
/// (address poisoning; ТЗ §4.1).
///
/// Scan surfaces, as of design §4 (2026-08-07): Activity, the queue, and the
/// Dashboard's identity panel — which shows WHICH wallet is loaded, not where
/// money is going. The boundary did not move: every one of them is read, and
/// the decision is made on the card, which still shows the address in full.
/// What changed is that the list of scan surfaces grew, not that a decision
/// surface started shortening.
/// Input without a `0x` prefix, non-ASCII, or too short to save space is
/// returned verbatim (display never crashes).
#[must_use]
pub fn short_addr(addr: &str) -> String {
    match addr.strip_prefix("0x") {
        Some(hex) if hex.is_ascii() && hex.len() > 12 => {
            format!("0x{}…{}", &hex[..6], &hex[hex.len() - 4..])
        }
        _ => addr.to_owned(),
    }
}

#[cfg(test)]
mod tests {

    /// The mapping and the fallback: an unknown chain shows its number rather
    /// than a guessed name — a wrong network on a payment screen is worse than
    /// a bare id.
    #[test]
    fn network_name_maps_what_it_knows_and_shows_the_rest() {
        assert_eq!(network_name(1), "Ethereum");
        assert_eq!(network_name(8453), "Base");
        assert_eq!(network_name(42161), "Arbitrum");
        assert_eq!(network_name(42), "chain 42");
    }

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

    /// A scan row has a column, not a page: the whole part gets separators, the
    /// fraction is cut to six digits, and the `…` says digits were dropped.
    #[test]
    fn short_eth_groups_digits_and_marks_what_it_dropped() {
        // The amount from the live acceptance run — 19 characters before the unit.
        assert_eq!(short_eth("5499068022390730"), "0.005499… ETH");
        assert_eq!(short_eth("1234500000000000000000"), "1,234.5 ETH");
        assert_eq!(
            short_eth("120000000123456789000000000"),
            "120,000,000.123456… ETH"
        );
        // Nothing to drop: a short amount is left exactly as it is, no marker.
        assert_eq!(short_eth("1500000000000000000"), "1.5 ETH");
    }

    /// Dust is not nothing. Cutting at six digits would render one wei as
    /// `0.000000…`, which reads as zero at a glance — so it gets a floor mark.
    #[test]
    fn short_eth_never_rounds_dust_to_zero() {
        assert_eq!(short_eth("1"), "<0.000001 ETH");
        assert_eq!(short_eth("999999999999"), "<0.000001 ETH");
    }

    /// …and the floor must not swallow a REAL zero: an empty balance on an
    /// allowed chain is an ordinary state, and `<0.000001 ETH` would be a lie
    /// about it. The zero is recognised on the raw wei, before any shortening.
    #[test]
    fn short_eth_keeps_a_real_zero_a_zero() {
        assert_eq!(short_eth("0"), "0 ETH");
        assert_eq!(short_eth("000"), "0 ETH");
    }

    /// Same defensive contract as `wei_to_eth`: display never crashes and never
    /// invents a number it could not read.
    #[test]
    fn short_eth_returns_non_numeric_verbatim() {
        assert_eq!(short_eth("not-a-number"), "not-a-number");
        assert_eq!(short_eth(""), "");
    }

    /// A token amount arrives already rendered, in its own unit, and comes out
    /// speaking the same language of digits as an ether one: grouped whole part,
    /// six fractional digits, `…` when digits were dropped.
    #[test]
    fn short_amount_shortens_a_token_the_way_it_shortens_ether() {
        // The live figure this whole arc exists for.
        assert_eq!(short_amount("22.820562", "USDC"), "22.820562 USDC");
        assert_eq!(short_amount("1234567.5", "USDT"), "1,234,567.5 USDT");
        assert_eq!(short_amount("0.1234567", "USDC"), "0.123456… USDC");
        assert_eq!(
            short_amount("0", "USDC"),
            "0 USDC",
            "a real zero stays zero"
        );
    }

    /// The dust floor belongs to the amount, not to ether: six places is a
    /// hundredth of a cent in USDC, and `0.000000…` there reads as nothing just
    /// as it does in ETH.
    #[test]
    fn short_amount_gives_token_dust_the_same_floor() {
        assert_eq!(short_amount("0.0000001", "USDC"), "<0.000001 USDC");
    }

    /// Same defensive contract as the ether path: an amount this module cannot
    /// read is shown as it came, without a unit dressed onto it.
    #[test]
    fn short_amount_returns_what_it_cannot_read_verbatim() {
        assert_eq!(short_amount("not-a-number", "USDC"), "not-a-number");
        assert_eq!(short_amount("", "USDC"), "");
        assert_eq!(short_amount("1.2.3", "USDC"), "1.2.3");
    }

    #[test]
    fn is_zero_wei_detects_a_token_op() {
        assert!(is_zero_wei("0"));
        assert!(is_zero_wei("000"));
        assert!(!is_zero_wei("1"));
        assert!(!is_zero_wei("10000000000000000"));
        assert!(!is_zero_wei(""));
    }

    #[test]
    fn short_addr_keeps_head_tail_and_eip55_casing() {
        assert_eq!(
            short_addr("0x489Fe09Fbb489Fe09Fbb489Fe09Fbb489F9Fbbbb"),
            "0x489Fe0…bbbb",
            "first 6 + last 4, casing verbatim"
        );
    }

    #[test]
    fn short_addr_returns_unshortenable_input_verbatim() {
        assert_eq!(
            short_addr("0x1234567890ab"),
            "0x1234567890ab",
            "12 hex: nothing saved"
        );
        assert_eq!(
            short_addr("not-an-address"),
            "not-an-address",
            "no 0x prefix"
        );
        assert_eq!(short_addr(""), "");
    }

    #[test]
    fn short_addr_never_panics_on_non_ascii() {
        let hostile = "0xдлинная-не-ascii-строка-длиннее-двенадцати";
        assert_eq!(
            short_addr(hostile),
            hostile,
            "non-ASCII input is returned verbatim"
        );
    }
}
