//! Clear-signing pipeline: turn raw `(to, calldata)` into a human-readable
//! "this transaction calls `transfer(0x…, 1.23 USDC)`" the user can
//! review before signing.
//!
//! Pipeline stages (one module each):
//!
//! 1. [`fourbyte`] — selector → human signatures (sorted binary blob,
//!    embedded at compile time).
//! 2. `proxy` — EIP-1967 / beacon / ZeppelinOS implementation walker
//!    over verified storage slot reads.
//! 3. `bytecode` — selector + arg-type extraction from contract code
//!    (evmole). Catches what 4byte doesn't, plus narrows ambiguity.
//! 4. `matcher` — intersect 4byte candidates with bytecode types,
//!    yielding `Resolved::{Unique, Ambiguous, TypesOnly, Unknown}`.
//! 5. `render` — decode arguments via `alloy::dyn_abi`, humanize
//!    addresses (reverse ENS) and amounts (token symbol/decimals),
//!    surface heuristic warnings (infinite approval).
//!
//! Stages 2–5 land in the phases that follow Phase 2; the module entries
//! appear here as they're added.

pub mod bytecode;
pub mod fourbyte;
pub mod matcher;
pub mod proxy;
pub mod render;
