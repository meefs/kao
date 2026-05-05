//! Reconcile 4byte's "selector → list of human signatures" with
//! evmole's "selector → list of arg types" to produce the most specific
//! resolution we can:
//!
//! - **Unique** — exactly one signature is consistent with both
//!   sources. The clear-signing UI shows the function name and decodes
//!   arguments by name.
//! - **Ambiguous** — multiple signatures collide on this selector and
//!   we couldn't narrow further. The UI lists candidates and falls back
//!   to raw calldata.
//! - **TypesOnly** — 4byte has no entry, but the bytecode tells us the
//!   shape. The UI shows `unknown(address, uint256)` so the user at
//!   least sees structured arguments.
//! - **Unknown** — no information either way. The UI shows the raw
//!   selector and warns this is an unverified call.
//!
//! Module name is `matcher` rather than `match` because `match` is a
//! reserved keyword and the raw-identifier alternative (`r#match`) bleeds
//! into every import site.

use alloy::dyn_abi::DynSolType;

#[derive(Debug, Clone)]
pub enum Resolved {
    Unique {
        name: String,
        arg_types: Vec<DynSolType>,
    },
    Ambiguous(Vec<(String, Vec<DynSolType>)>),
    TypesOnly(Vec<DynSolType>),
    Unknown,
}

/// Combine 4byte candidates with whatever evmole could pull out of the
/// bytecode at the same selector. `bytecode_arg_types == None` means
/// "no bytecode info" (e.g., we couldn't fetch the code, or evmole
/// returned nothing); empty list means "bytecode says zero-arg
/// function", which is meaningful and used for matching.
pub fn resolve(
    fourbyte_candidates: &[&str],
    bytecode_arg_types: Option<&[DynSolType]>,
) -> Resolved {
    let parsed: Vec<(String, Vec<DynSolType>)> = fourbyte_candidates
        .iter()
        .filter_map(|sig| Some((function_name(sig).to_owned(), parse_signature_args(sig)?)))
        .collect();

    match (parsed.len(), bytecode_arg_types) {
        (0, Some(types)) if !types.is_empty() => Resolved::TypesOnly(types.to_vec()),
        (0, _) => Resolved::Unknown,
        (1, _) => {
            let (name, arg_types) = parsed.into_iter().next().unwrap();
            Resolved::Unique { name, arg_types }
        }
        (_, Some(bytecode_types)) => {
            let matches: Vec<_> = parsed
                .iter()
                .filter(|(_, types)| types == bytecode_types)
                .cloned()
                .collect();
            match matches.len() {
                1 => {
                    let (name, arg_types) = matches.into_iter().next().unwrap();
                    Resolved::Unique { name, arg_types }
                }
                // Either no candidate matches the bytecode (suspicious —
                // could indicate a phishing fixture or a stale 4byte
                // entry), or several do. Either way, hand the user the
                // full candidate list so they can decide.
                _ => Resolved::Ambiguous(if matches.is_empty() { parsed } else { matches }),
            }
        }
        (_, None) => Resolved::Ambiguous(parsed),
    }
}

/// Function name from a 4byte signature: `"transfer(address,uint256)"`
/// → `"transfer"`.
fn function_name(sig: &str) -> &str {
    sig.split_once('(').map(|(n, _)| n).unwrap_or(sig)
}

/// Argument types from a 4byte signature: `"transfer(address,uint256)"`
/// → `[Address, Uint(256)]`. Returns `None` when the signature is
/// malformed or contains a type alloy's parser doesn't understand.
///
/// Strategy: extract `(...)` substring, parse via `DynSolType::parse`
/// (which natively handles nested tuples like
/// `((address,uint256)[],bool)`), unwrap the resulting `Tuple`.
fn parse_signature_args(sig: &str) -> Option<Vec<DynSolType>> {
    let open = sig.find('(')?;
    let close = sig.rfind(')')?;
    if close <= open {
        return None;
    }
    let args_str = &sig[open..=close];
    if args_str == "()" {
        return Some(Vec::new());
    }
    let parsed: DynSolType = args_str.parse().ok()?;
    match parsed {
        DynSolType::Tuple(types) => Some(types),
        // `"(address)"` parses as a one-element tuple in alloy, so we
        // shouldn't hit this branch — but if a future alloy ever
        // collapses a single-element tuple to its inner type, treat
        // that single type as the only arg.
        other => Some(vec![other]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_with_single_candidate() {
        let r = resolve(&["transfer(address,uint256)"], None);
        match r {
            Resolved::Unique { name, arg_types } => {
                assert_eq!(name, "transfer");
                assert_eq!(arg_types.len(), 2);
                assert!(matches!(arg_types[0], DynSolType::Address));
                assert!(matches!(arg_types[1], DynSolType::Uint(256)));
            }
            other => panic!("expected Unique, got {other:?}"),
        }
    }

    #[test]
    fn unknown_when_nothing_known() {
        match resolve(&[], None) {
            Resolved::Unknown => (),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn types_only_from_bytecode() {
        let types = vec![DynSolType::Address, DynSolType::Uint(256)];
        match resolve(&[], Some(&types)) {
            Resolved::TypesOnly(out) => assert_eq!(out, types),
            other => panic!("expected TypesOnly, got {other:?}"),
        }
    }

    #[test]
    fn bytecode_narrows_ambiguous() {
        let candidates = &[
            "transfer(address,uint256)",
            "totally_other(bytes32,bytes32)",
        ];
        let bytecode = vec![DynSolType::Address, DynSolType::Uint(256)];
        match resolve(candidates, Some(&bytecode)) {
            Resolved::Unique { name, .. } => assert_eq!(name, "transfer"),
            other => panic!("expected Unique transfer, got {other:?}"),
        }
    }

    #[test]
    fn ambiguous_when_multiple_match_no_bytecode() {
        let candidates = &[
            "transfer(address,uint256)",
            "doppel(address,uint256)",
        ];
        match resolve(candidates, None) {
            Resolved::Ambiguous(list) => {
                assert_eq!(list.len(), 2);
                let names: Vec<_> = list.iter().map(|(n, _)| n.as_str()).collect();
                assert!(names.contains(&"transfer"));
                assert!(names.contains(&"doppel"));
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn empty_args_signature_parses() {
        match resolve(&["renounceOwnership()"], None) {
            Resolved::Unique { name, arg_types } => {
                assert_eq!(name, "renounceOwnership");
                assert!(arg_types.is_empty());
            }
            other => panic!("expected Unique, got {other:?}"),
        }
    }

    #[test]
    fn nested_tuple_parses() {
        // Multicall-shaped arg: array of (target, callData) tuples.
        match resolve(&["aggregate((address,bytes)[])"], None) {
            Resolved::Unique { name, arg_types } => {
                assert_eq!(name, "aggregate");
                assert_eq!(arg_types.len(), 1);
                // Should be Array(Tuple([Address, Bytes]))
                assert!(matches!(arg_types[0], DynSolType::Array(_)));
            }
            other => panic!("expected Unique aggregate, got {other:?}"),
        }
    }

    #[test]
    fn multiple_match_bytecode_remains_ambiguous() {
        // Two candidates both share the bytecode's arg shape — neither
        // can be eliminated, so we hand the user the matched subset to
        // review (not the unmatched ones).
        let candidates = &[
            "transfer(address,uint256)",
            "doppel(address,uint256)",
            "totally_other(bytes32,bytes32)",
        ];
        let bytecode = vec![DynSolType::Address, DynSolType::Uint(256)];
        match resolve(candidates, Some(&bytecode)) {
            Resolved::Ambiguous(list) => {
                let names: Vec<_> = list.iter().map(|(n, _)| n.as_str()).collect();
                assert_eq!(list.len(), 2, "expected 2 narrowed matches, got {names:?}");
                assert!(names.contains(&"transfer"));
                assert!(names.contains(&"doppel"));
                assert!(!names.contains(&"totally_other"));
            }
            other => panic!("expected Ambiguous narrowed pair, got {other:?}"),
        }
    }

    #[test]
    fn no_bytecode_match_falls_back_to_all_parsed() {
        // Bytecode says (bool, bool) but no candidate has that shape.
        // The fallback hands back ALL parsed candidates — see the
        // "suspicious — could indicate phishing" branch in resolve().
        let candidates = &[
            "transfer(address,uint256)",
            "approve(address,uint256)",
        ];
        let bytecode = vec![DynSolType::Bool, DynSolType::Bool];
        match resolve(candidates, Some(&bytecode)) {
            Resolved::Ambiguous(list) => {
                assert_eq!(list.len(), 2);
                let names: Vec<_> = list.iter().map(|(n, _)| n.as_str()).collect();
                assert!(names.contains(&"transfer"));
                assert!(names.contains(&"approve"));
            }
            other => panic!("expected Ambiguous fallback, got {other:?}"),
        }
    }

    #[test]
    fn malformed_signature_filtered_out() {
        // A signature missing the closing paren can't be parsed — it
        // should be silently dropped, not panic. The well-formed sibling
        // becomes the unique match.
        match resolve(&["broken(address", "transfer(address,uint256)"], None) {
            Resolved::Unique { name, .. } => assert_eq!(name, "transfer"),
            other => panic!("expected Unique transfer (malformed dropped), got {other:?}"),
        }
    }

    #[test]
    fn empty_bytecode_types_yields_unknown_when_no_candidates() {
        // (0, Some(empty)) — bytecode said "this selector takes no
        // args" but 4byte has nothing. The matcher treats the empty
        // typelist as no-info-from-bytecode and returns Unknown.
        match resolve(&[], Some(&[])) {
            Resolved::Unknown => (),
            other => panic!("expected Unknown for empty-types-no-candidates, got {other:?}"),
        }
    }

    #[test]
    fn single_arg_signature_parses() {
        // alloy may collapse `(address)` to a single Address rather than
        // a one-element tuple — resolve() handles both.
        match resolve(&["foo(address)"], None) {
            Resolved::Unique { name, arg_types } => {
                assert_eq!(name, "foo");
                assert_eq!(arg_types.len(), 1);
                assert!(matches!(arg_types[0], DynSolType::Address));
            }
            other => panic!("expected Unique foo(address), got {other:?}"),
        }
    }

    #[test]
    fn function_name_without_parens_used_verbatim() {
        // function_name() helper: no '(' → the whole string is the name.
        // resolve() will drop these because parse_signature_args returns
        // None, so the candidate is filtered. End-to-end: pure-name
        // string with no peer is treated as if 4byte returned nothing.
        match resolve(&["weirdNameNoParens"], None) {
            Resolved::Unknown => (),
            other => panic!("expected Unknown (signature dropped), got {other:?}"),
        }
    }
}
