//! Display sanitization for attacker-controlled strings.
//!
//! Token symbols (`symbol()`), decoded calldata string arguments, and ABI
//! string returns all originate from contracts an attacker can deploy, yet
//! they are rendered onto the transaction-review / signing surface. Without
//! sanitization a hostile token can smuggle in:
//!   - **bidi controls** (`U+202E` RIGHT-TO-LEFT OVERRIDE, the isolates) that
//!     visually reorder the line, so a spoofed "✓ verified — safe to sign"
//!     or a flipped amount/recipient renders as something the user trusts;
//!   - **zero-width / invisible** code points (ZWSP, ZWJ, word joiner, BOM,
//!     soft hyphen) that hide or splice characters;
//!   - **control characters / line separators** (`\n`, `\r`, `U+2028`) that
//!     break the label across rows or push content off-screen;
//!   - **unbounded length** that pushes the real values out of view.
//!
//! "Verified" (Helios) only attests that the RPC answer is the genuine
//! contract return — the contract itself is still attacker-deployed, so the
//! bytes must be sanitized before they are shown. This mirrors the ENSIP-15
//! normalization `crate::names::ens` applies to ENS names; arbitrary display strings
//! are not ENS names, so they get this lighter-weight strip-and-clamp instead.

use std::borrow::Cow;

/// Upper bound on a rendered token symbol. Real ERC-20 symbols are a handful
/// of characters ("USDC", "wstETH"); anything longer is junk or a padding
/// attack, so clamp it on the amount / review line.
pub const MAX_TOKEN_SYMBOL_CHARS: usize = 24;

/// Upper bound on a rendered token name ("USD Coin", "Wrapped BTC"). Longer
/// than a symbol but still bounded so a padded name can't dominate a row.
pub const MAX_TOKEN_NAME_CHARS: usize = 40;

/// True for code points that must never reach a display surface: control
/// characters, bidirectional formatting / override / isolate controls,
/// zero-width and other invisible formatting code points, and line /
/// paragraph separators.
fn is_unsafe_display_char(c: char) -> bool {
    c.is_control() // C0, C1, DEL, and \t \n \r
        || matches!(
            c,
            '\u{00AD}'                 // SOFT HYPHEN (invisible)
            | '\u{061C}'               // ARABIC LETTER MARK (bidi)
            | '\u{200B}'..='\u{200F}'  // ZWSP, ZWNJ, ZWJ, LRM, RLM
            | '\u{2028}'               // LINE SEPARATOR
            | '\u{2029}'               // PARAGRAPH SEPARATOR
            | '\u{202A}'..='\u{202E}'  // LRE, RLE, PDF, LRO, RLO (bidi embedding/override)
            | '\u{2060}'..='\u{2064}'  // WORD JOINER + invisible math operators
            | '\u{2066}'..='\u{2069}'  // LRI, RLI, FSI, PDI (bidi isolates)
            | '\u{FEFF}'               // ZERO WIDTH NO-BREAK SPACE / BOM
            | '\u{FFF9}'..='\u{FFFB}'  // interlinear annotation anchors
        )
}

/// Strip unsafe code points (see [`is_unsafe_display_char`]) from `s` and
/// clamp the result to `max_chars` (appending `…` when truncated).
///
/// Returns the input borrowed unchanged on the common clean, short path, so
/// the render loop does not allocate for ordinary labels. A string that is
/// entirely unsafe code points sanitizes to an empty string — callers treat
/// that as "no usable value" (e.g. a token symbol falls back to no symbol).
pub fn sanitize_display(s: &str, max_chars: usize) -> Cow<'_, str> {
    // Fast path: scan only until the first unsafe char or one past the cap.
    let mut count = 0usize;
    let mut clean = true;
    for c in s.chars() {
        if is_unsafe_display_char(c) {
            clean = false;
            break;
        }
        count += 1;
        if count > max_chars {
            break;
        }
    }
    if clean && count <= max_chars {
        return Cow::Borrowed(s);
    }
    // Slow path: rebuild, dropping unsafe chars and clamping length.
    let mut out = String::new();
    let mut kept = 0usize;
    for c in s.chars() {
        if is_unsafe_display_char(c) {
            continue;
        }
        if kept == max_chars {
            out.push('…');
            break;
        }
        out.push(c);
        kept += 1;
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_short_string_is_borrowed_unchanged() {
        let out = sanitize_display("USDC", MAX_TOKEN_SYMBOL_CHARS);
        assert!(matches!(out, Cow::Borrowed("USDC")));
    }

    #[test]
    fn strips_rtl_override() {
        // U+202E reorders the rest of the line — the classic signing-surface
        // spoof. It must be removed, not rendered.
        let out = sanitize_display("USD\u{202E}C", MAX_TOKEN_SYMBOL_CHARS);
        assert_eq!(out, "USDC");
    }

    #[test]
    fn strips_zero_width_and_bom() {
        let out = sanitize_display("U\u{200B}S\u{200D}D\u{FEFF}C", MAX_TOKEN_SYMBOL_CHARS);
        assert_eq!(out, "USDC");
    }

    #[test]
    fn strips_newlines_and_controls() {
        let out = sanitize_display("USD\nC\tX\r", MAX_TOKEN_SYMBOL_CHARS);
        assert_eq!(out, "USDCX");
    }

    #[test]
    fn strips_bidi_isolates() {
        let out = sanitize_display("\u{2066}evil\u{2069}", MAX_TOKEN_SYMBOL_CHARS);
        assert_eq!(out, "evil");
    }

    #[test]
    fn clamps_overlong_input_with_ellipsis() {
        let out = sanitize_display("ABCDEFGHIJ", 4);
        assert_eq!(out, "ABCD…");
    }

    #[test]
    fn all_unsafe_input_sanitizes_to_empty() {
        // A symbol made entirely of invisibles becomes empty → "no symbol".
        let out = sanitize_display("\u{200B}\u{202E}\u{FEFF}", MAX_TOKEN_SYMBOL_CHARS);
        assert!(out.is_empty());
    }

    #[test]
    fn keeps_ordinary_unicode_and_spaces() {
        // Spaces and printable non-ASCII (e.g. a euro sign) are legitimate.
        let out = sanitize_display("Wrapped €", MAX_TOKEN_SYMBOL_CHARS);
        assert_eq!(out, "Wrapped €");
    }

    #[test]
    fn injection_phrase_length_capped() {
        // A token trying to render a reassurance string into the amount line
        // gets clamped to the symbol bound, so it can't push real values away.
        let evil = "USDC ✓ verified — safe to sign, ignore the address";
        let out = sanitize_display(evil, MAX_TOKEN_SYMBOL_CHARS);
        assert_eq!(out.chars().count(), MAX_TOKEN_SYMBOL_CHARS + 1); // + the ellipsis
        assert!(out.ends_with('…'));
    }
}
