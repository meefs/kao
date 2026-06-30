//! Decoded function call panel — read-only view that slots into the
//! send review screen between the gas row and the warning band.
//!
//! Dispatches on `DecodeResult`:
//! - **ClearSigned** — ERC-7730 descriptor matched. Shows intent header
//!   + labeled entries from the `DisplayModel`.
//! - **Fallback** — partial descriptor match. Shows `DisplayModel` with
//!   a fallback hint.
//! - **Heuristic** — no descriptor. Existing heuristic panel
//!   (function name + per-arg rows from evmole/4byte).
//! - **Empty** — native ETH transfer, no calldata. Returns `None`.
//!
//! Warnings render as tinted strips. `AmbiguousSignature` rides *above*
//! the header — when several signatures collide on the same selector,
//! the title we show is provisional, so the warning has to land before
//! the user reads the name (and the title gets muted to match).
//! `InfiniteApproval` / `UnverifiedBytecode` qualify the call without
//! contradicting the title and stay in the strip below the args.
//!
//! Loading state shows a small "decoding…" line; sized so the review
//! card doesn't pop when the result lands.

use alloy::primitives::Address;
use iced::border::Radius;
use iced::widget::text::Wrapping;
use iced::widget::{Space, column, container, row, text};
use iced::{Background, Border, Element, Length, Padding};

use clear_signing::{
    DiagnosticSeverity, DisplayEntry, DisplayItem, DisplayModel, FormatDiagnostic,
};

use crate::decode::clear_sign::DecodeResult;
use crate::decode::render::{ArgDisplay, DecodedArg, DecodedCall, ResolutionState, Warning};
use crate::ui::kao_theme::{KaoTheme, with_alpha};
use crate::ui::kao_widgets::{bold, mono};

/// Build the panel. Returns `None` when there's nothing to render
/// (native ETH transfer); the caller should also omit the surrounding
/// divider so the review card layout stays clean.
pub fn view<'a, M: 'a>(
    t: KaoTheme,
    decoded: Option<&'a DecodeResult>,
    loading: bool,
) -> Option<Element<'a, M>> {
    if loading {
        return Some(loading_view(t));
    }
    match decoded? {
        DecodeResult::ClearSigned {
            model,
            diagnostics,
            proxy_hops,
            all_verified,
        } => Some(clear_signed_panel(
            t,
            model,
            diagnostics,
            proxy_hops,
            *all_verified,
            &[],
        )),
        DecodeResult::Fallback {
            model,
            diagnostics,
            all_verified,
            heuristic,
            ..
        } => Some(clear_signed_panel(
            t,
            model,
            diagnostics,
            &heuristic.proxy_hops,
            *all_verified && heuristic.all_verified,
            // The heuristic ran alongside the partial descriptor; don't drop
            // its spoof/ambiguity signals just because we're showing the
            // descriptor's intent.
            &heuristic.warnings,
        )),
        DecodeResult::Heuristic(decoded) => {
            if matches!(decoded.state, ResolutionState::Empty) {
                None
            } else {
                Some(heuristic_panel(t, decoded))
            }
        }
        DecodeResult::Empty => None,
    }
}

fn loading_view<'a, M: 'a>(t: KaoTheme) -> Element<'a, M> {
    let mut col = column![].spacing(6);
    col = col.push(text("Intent").size(11).color(t.sub).font(bold()));
    col = col.push(Space::new().height(2));
    col = col.push(
        text("(・_・;) resolving…")
            .size(13)
            .color(t.sub)
            .font(mono()),
    );
    // Placeholder rows so the card doesn't jump when the result lands.
    for label in ["· ···", "· ···"] {
        col = col.push(
            row![
                text(label)
                    .size(11)
                    .color(with_alpha(t.sub, 0.4))
                    .font(mono()),
                Space::new().width(Length::Fill),
                text("···")
                    .size(12)
                    .color(with_alpha(t.sub, 0.4))
                    .font(mono()),
            ]
            .width(Length::Fill),
        );
    }
    col.width(Length::Fill).into()
}

// ---------------------------------------------------------------------------
// ERC-7730 clear-signed panel

fn clear_signed_panel<'a, M: 'a>(
    t: KaoTheme,
    model: &'a DisplayModel,
    diagnostics: &'a [FormatDiagnostic],
    proxy_hops: &'a [Address],
    all_verified: bool,
    heuristic_warnings: &'a [Warning],
) -> Element<'a, M> {
    let mut col = column![].spacing(6);

    // Spoof / ambiguity signals from the cross-referenced heuristic decode
    // (Fallback path) ride above the intent — they cast doubt on the title
    // itself, so the user must see them before reading the name.
    for w in heuristic_warnings {
        if matches!(
            w,
            Warning::BytecodeMismatch { .. } | Warning::AmbiguousSignature { .. }
        ) {
            col = col.push(warning_strip(t, w));
        }
    }

    // Intent header.
    let intent = model
        .interpolated_intent
        .as_deref()
        .unwrap_or(&model.intent);
    // When some on-chain read fell back to unverified RPC, the intent text,
    // contract name and amounts below are no longer fully trustworthy — mute
    // the header so it doesn't read as authoritative, and raise a prominent
    // caution band rather than the easy-to-miss one-line note it used to be.
    let intent_color = if all_verified { t.text } else { t.sub };
    col = col.push(text("Intent").size(11).color(t.sub).font(bold()));
    col = col.push(Space::new().height(2));
    col = col.push(text(intent).size(13).color(intent_color).font(bold()));

    if let Some(name) = &model.contract_name {
        col = col.push(text(name).size(11).color(t.sub).font(mono()));
    }
    if !proxy_hops.is_empty() {
        col = col.push(text("(via proxy)").size(10).color(t.sub).font(mono()));
    }
    if !all_verified {
        col = col.push(Space::new().height(3));
        col = col.push(caution_strip(
            t,
            "⚠ Some on-chain reads fell back to unverified RPC — the intent, names and amounts below may be spoofed.".into(),
        ));
    }

    col = col.push(Space::new().height(4));

    // Entries.
    for entry in &model.entries {
        col = push_display_entry(&mut col, t, entry, 0);
    }

    // Diagnostics (warning-severity only).
    let warnings: Vec<&FormatDiagnostic> = diagnostics
        .iter()
        .filter(|d| matches!(d.severity, DiagnosticSeverity::Warning))
        .collect();
    if !warnings.is_empty() {
        col = col.push(Space::new().height(4));
        for diag in warnings {
            col = col.push(diagnostic_strip(t, diag));
        }
    }

    col.width(Length::Fill).into()
}

/// Recursively push a `DisplayEntry` into the column. `depth` controls
/// indentation for nested entries.
fn push_display_entry<'a, M: 'a>(
    col: &mut iced::widget::Column<'a, M>,
    t: KaoTheme,
    entry: &'a DisplayEntry,
    depth: u16,
) -> iced::widget::Column<'a, M> {
    let indent = (depth as f32) * 12.0;
    let col_taken = std::mem::replace(col, column![]);
    let mut col = col_taken;
    match entry {
        DisplayEntry::Item(item) => {
            col = col.push(display_item_row(t, item, indent));
        }
        DisplayEntry::Group { label, items, .. } => {
            // Group label as sub-header.
            let label_el = row![
                Space::new().width(Length::Fixed(indent)),
                text(label).size(11).color(t.sub).font(bold()),
            ];
            col = col.push(label_el);
            for item in items {
                col = col.push(display_item_row(t, item, indent + 8.0));
            }
        }
        DisplayEntry::Nested {
            label,
            intent,
            entries,
            ..
        } => {
            // Nested card: label + intent as a sub-header, entries indented.
            let nested_header = row![
                Space::new().width(Length::Fixed(indent)),
                column![
                    text(label).size(11).color(t.sub).font(bold()),
                    text(intent).size(12).color(t.text).font(bold()),
                ]
                .spacing(2),
            ];
            col = col.push(nested_header);
            for sub_entry in entries {
                col = push_display_entry(&mut col, t, sub_entry, depth + 1);
            }
        }
    }
    col
}

fn display_item_row<'a, M: 'a>(t: KaoTheme, item: &'a DisplayItem, indent: f32) -> Element<'a, M> {
    let value_display = truncate(&item.value, 48);
    labeled_value(
        t,
        indent,
        format!("· {}", item.label),
        value_display.into_owned(),
        t.text,
    )
}

/// A tinted caution band — the shared styling for diagnostics, heuristic
/// warnings, and the unverified-reads notice.
fn caution_strip<'a, M: 'a>(t: KaoTheme, line: String) -> Element<'a, M> {
    container(text(line).size(11).color(t.down).font(bold()))
        .padding(Padding::from([6, 8]))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(with_alpha(t.down, 0.12))),
            border: Border {
                color: with_alpha(t.down, 0.4),
                width: 1.0,
                radius: Radius::from(8),
            },
            text_color: Some(t.down),
            ..container::Style::default()
        })
        .into()
}

fn diagnostic_strip<'a, M: 'a>(t: KaoTheme, diag: &'a FormatDiagnostic) -> Element<'a, M> {
    caution_strip(t, format!("⚠ {}", diag.message))
}

// ---------------------------------------------------------------------------
// Heuristic panel (existing renderer, extracted from the old `panel`)

fn heuristic_panel<'a, M: 'a>(t: KaoTheme, d: &'a DecodedCall) -> Element<'a, M> {
    // AmbiguousSignature warnings precede the header — they cast doubt
    // on the title itself, so the user must see them first. The other
    // warning kinds qualify the call without undermining the name and
    // ride below the arg rows in the foot strip.
    let mut col = column![].spacing(6);
    for w in &d.warnings {
        if matches!(
            w,
            Warning::AmbiguousSignature { .. } | Warning::BytecodeMismatch { .. }
        ) {
            col = col.push(warning_strip(t, w));
        }
    }
    col = col.push(header(t, d));

    for arg in &d.args {
        col = col.push(arg_row(t, arg));
    }

    if d.args.is_empty() && matches!(d.state, ResolutionState::Unknown) {
        // No types at all — tell the user we couldn't decode and show
        // the raw calldata footprint so they can at least eyeball it.
        col = col.push(unknown_call_body(t, d));
    }

    let mut foot_warnings = d
        .warnings
        .iter()
        .filter(|w| {
            !matches!(
                w,
                Warning::AmbiguousSignature { .. } | Warning::BytecodeMismatch { .. }
            )
        })
        .peekable();
    if foot_warnings.peek().is_some() {
        col = col.push(Space::new().height(4));
        for w in foot_warnings {
            col = col.push(warning_strip(t, w));
        }
    }

    col.width(Length::Fill).into()
}

fn header<'a, M: 'a>(t: KaoTheme, d: &'a DecodedCall) -> Element<'a, M> {
    let label = text("Function").size(11).color(t.sub).font(bold());
    let title = match &d.function_name {
        Some(name) => format!("{}(…)", name),
        None => format!(
            "0x{:02x}{:02x}{:02x}{:02x}",
            d.selector[0], d.selector[1], d.selector[2], d.selector[3]
        ),
    };
    // For Ambiguous the title is just one of several plausible names —
    // mute it so the chunk doesn't visually claim authority. The banner
    // above carries the full candidate list. Resolved/TypesOnly/Unknown
    // keep the strong text color.
    let title_color = if matches!(d.state, ResolutionState::Ambiguous) {
        t.sub
    } else {
        t.text
    };
    let subtitle: Option<String> = match d.state {
        ResolutionState::Resolved => None,
        // Banner above the header conveys this; a duplicate subtitle
        // would just split the user's attention.
        ResolutionState::Ambiguous => None,
        ResolutionState::TypesOnly => Some("decoded from bytecode · no name".into()),
        ResolutionState::Unknown => Some("unverified call".into()),
        ResolutionState::Empty => None,
    };

    let mut col = column![
        label,
        Space::new().height(2),
        text(title).size(13).color(title_color).font(bold()),
    ]
    .spacing(0);
    if let Some(s) = subtitle {
        col = col.push(text(s).size(11).color(t.sub).font(mono()));
    }
    if !d.proxy_hops.is_empty() {
        col = col.push(text("(via proxy)").size(10).color(t.sub).font(mono()));
    }
    col.into()
}

fn arg_row<'a, M: 'a>(t: KaoTheme, arg: &'a DecodedArg) -> Element<'a, M> {
    let name = arg.name.as_deref().unwrap_or(""); // bytecode introspection rarely has names
    let ty_label = ty_short(&arg.ty);
    let label = if name.is_empty() {
        format!("· {ty_label}")
    } else {
        format!("· {name}: {ty_label}")
    };
    let value = match &arg.display {
        ArgDisplay::Address { addr, ens } => match ens {
            Some(name) => format!("{name}  {}", short(*addr)),
            None => short(*addr),
        },
        ArgDisplay::Uint { formatted, .. } => formatted.clone(),
        ArgDisplay::Int { formatted, .. } => formatted.clone(),
        ArgDisplay::Bool(b) => b.to_string(),
        ArgDisplay::String(s) => format!("\"{}\"", truncate(s, 48)),
        ArgDisplay::Bytes(b) => {
            let hex = alloy::hex::encode(b);
            format!("0x{}", truncate(&hex, 32))
        }
        ArgDisplay::Raw(s) => truncate(s, 48).into_owned(),
    };

    labeled_value(t, 0.0, label, value, t.text)
}

fn unknown_call_body<'a, M: 'a>(t: KaoTheme, d: &'a DecodedCall) -> Element<'a, M> {
    // Show truncated raw calldata so the user has _something_ to
    // eyeball when no decoder applied.
    let hex = alloy::hex::encode(&d.raw_calldata);
    let display = format!("0x{}", truncate(&hex, 64));
    labeled_value(t, 0.0, "· raw".to_string(), display, t.sub)
}

/// A `label … value` arg row. Short values sit on the same line as the label,
/// right-aligned; values too long for that (big `uint256`s, long hex) drop onto
/// their own full-width line and **wrap by glyph** so a 77-digit number breaks
/// across rows instead of overflowing the panel edge.
fn labeled_value<'a, M: 'a>(
    t: KaoTheme,
    indent: f32,
    label: String,
    value: String,
    value_color: iced::Color,
) -> Element<'a, M> {
    let label_el = text(label).size(11).color(t.sub).font(mono());
    if value.chars().count() <= VALUE_INLINE_MAX {
        row![
            Space::new().width(Length::Fixed(indent)),
            label_el,
            Space::new().width(Length::Fill),
            text(value).size(12).color(value_color).font(mono()),
        ]
        .width(Length::Fill)
        .into()
    } else {
        // Stacked: label, then the value wrapping across the full width. The
        // value row must be `Fill` (a Row defaults to Shrink) so the text has a
        // width bound to glyph-wrap within instead of overflowing.
        column![
            row![Space::new().width(Length::Fixed(indent)), label_el],
            row![
                Space::new().width(Length::Fixed(indent + 8.0)),
                text(value)
                    .size(12)
                    .color(value_color)
                    .font(mono())
                    .wrapping(Wrapping::Glyph)
                    .width(Length::Fill),
            ]
            .width(Length::Fill),
        ]
        .width(Length::Fill)
        .spacing(2)
        .into()
    }
}

/// Values longer than this won't fit on the shared label row (the panel is ~45
/// mono chars wide once the label and gap are subtracted), so they wrap onto
/// their own line instead.
const VALUE_INLINE_MAX: usize = 20;

fn warning_strip<'a, M: 'a>(t: KaoTheme, w: &'a Warning) -> Element<'a, M> {
    let line: String = match w {
        Warning::InfiniteApproval { spender, .. } => {
            format!("⚠ infinite approval to {}", short(*spender))
        }
        Warning::UnverifiedBytecode => "⚠ bytecode read fell back to unverified RPC".into(),
        Warning::AmbiguousSignature { candidates } => {
            let names: Vec<&str> = candidates.iter().map(String::as_str).collect();
            format!("⚠ ambiguous: {}", truncate(&names.join(", "), 60))
        }
        Warning::BytecodeMismatch { candidates } => {
            let names: Vec<&str> = candidates.iter().map(String::as_str).collect();
            format!(
                "⚠ possible spoof — on-chain code matches no known signature (claimed: {})",
                truncate(&names.join(", "), 48)
            )
        }
    };
    caution_strip(t, line)
}

// ---------------------------------------------------------------------------
// Formatting helpers

fn short(addr: Address) -> String {
    let s = format!("{addr:#x}");
    // Six leading + four trailing — enough to spot-check, narrow enough
    // to fit in the value column without crowding.
    let len = s.len();
    if len <= 12 {
        return s;
    }
    format!("{}…{}", &s[..6], &s[len - 4..])
}

/// Clamp `s` to `max` characters (appending `…`) **and** strip unsafe display
/// code points — bidi controls, zero-width / invisible characters, control
/// chars. Arg values and clear-signed labels can carry attacker-controlled
/// strings (decoded calldata, contract-supplied text); stripping here keeps a
/// hostile contract from reordering or hiding the review surface. See
/// [`crate::sanitize`].
fn truncate(s: &str, max: usize) -> std::borrow::Cow<'_, str> {
    crate::sanitize::sanitize_display(s, max)
}

/// Compact canonical-string label for an alloy `DynSolType` — used on
/// arg rows so the reader sees `address` / `uint256` / `(address,uint256)[]`
/// instead of evmole's debug repr.
fn ty_short(ty: &alloy::dyn_abi::DynSolType) -> String {
    use alloy::dyn_abi::DynSolType;
    match ty {
        DynSolType::Address => "address".into(),
        DynSolType::Bool => "bool".into(),
        DynSolType::String => "string".into(),
        DynSolType::Bytes => "bytes".into(),
        DynSolType::FixedBytes(n) => format!("bytes{n}"),
        DynSolType::Uint(n) => format!("uint{n}"),
        DynSolType::Int(n) => format!("int{n}"),
        DynSolType::Tuple(items) => {
            let inner: Vec<String> = items.iter().map(ty_short).collect();
            format!("({})", inner.join(","))
        }
        DynSolType::Array(inner) => format!("{}[]", ty_short(inner)),
        DynSolType::FixedArray(inner, n) => format!("{}[{}]", ty_short(inner), n),
        DynSolType::Function => "function".into(),
        // `CustomStruct` only materializes from EIP-712 typed-data
        // decoding, which the function panel (evmole-derived calldata
        // types) never produces — render its name rather than its fields.
        DynSolType::CustomStruct { name, .. } => name.clone(),
    }
}
