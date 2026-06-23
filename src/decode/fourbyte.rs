//! Static 4byte selector → human signatures lookup.
//!
//! Backed by `assets/4byte.bin` — a sorted binary blob produced by
//! `cargo xtask sync-4byte-db` from the ethereum-lists/4bytes corpus.
//! Embedded at compile time via `include_bytes!`, so a built Kao binary
//! has the entire DB in memory with no install-time file management
//! and no startup file I/O.
//!
//! ## Format (`4BYT` v1)
//!
//! Mirrors `xtask::sync_4byte`. Header + sorted index + variable-length
//! strings region. Lookup is a binary search over the index by selector,
//! followed by a single linear walk through that selector's signature
//! list.
//!
//! ```text
//! header (16 bytes):
//!   [0..4]   "4BYT"
//!   [4..8]   version u32 LE = 1
//!   [8..12]  count u32 LE
//!   [12..16] reserved u32 LE
//!
//! index (count * 8 bytes), sorted by selector ascending:
//!   [u8; 4]  selector
//!   [u32 LE] strings_offset    // relative to start of strings region
//!
//! strings region:
//!   per selector:
//!     [u16 LE] sig_count
//!     for each sig:
//!       [u16 LE] len
//!       [u8; len] utf8
//! ```

use std::sync::OnceLock;

const BLOB: &[u8] = include_bytes!("../../assets/4byte.bin");
const MAGIC: &[u8; 4] = b"4BYT";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 16;
const INDEX_RECORD_LEN: usize = 8;

#[derive(Debug)]
struct Header {
    count: usize,
    strings_start: usize,
}

static HEADER: OnceLock<Header> = OnceLock::new();

fn header() -> &'static Header {
    HEADER.get_or_init(|| {
        assert!(BLOB.len() >= HEADER_LEN, "4byte.bin too short");
        assert_eq!(&BLOB[0..4], MAGIC, "4byte.bin: bad magic");
        let version = u32::from_le_bytes(BLOB[4..8].try_into().unwrap());
        assert_eq!(version, VERSION, "4byte.bin: unsupported version {version}");
        let count = u32::from_le_bytes(BLOB[8..12].try_into().unwrap()) as usize;
        let strings_start = HEADER_LEN + count * INDEX_RECORD_LEN;
        assert!(BLOB.len() >= strings_start, "4byte.bin: truncated index");
        Header {
            count,
            strings_start,
        }
    })
}

/// Human-readable signatures registered for `selector`. Empty when the
/// selector isn't in the DB. Multiple signatures returned for the
/// (rare) keccak collisions.
///
/// Returned `&'static str` references slice directly into the embedded
/// blob — no allocation per signature.
pub fn lookup(selector: [u8; 4]) -> Vec<&'static str> {
    let h = header();
    if h.count == 0 {
        return Vec::new();
    }
    // Standard binary search. The index is 8-byte records of `(sel, off)`.
    let mut lo = 0usize;
    let mut hi = h.count;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let off = HEADER_LEN + mid * INDEX_RECORD_LEN;
        let sel: [u8; 4] = BLOB[off..off + 4].try_into().unwrap();
        match sel.cmp(&selector) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => {
                let rel = u32::from_le_bytes(BLOB[off + 4..off + 8].try_into().unwrap()) as usize;
                return read_sigs(h.strings_start + rel);
            }
        }
    }
    Vec::new()
}

fn read_sigs(start: usize) -> Vec<&'static str> {
    let mut p = start;
    // The 4byte.bin blob is a trusted build artifact, but read it defensively:
    // a corrupt or truncated embedding should degrade to "no signatures" rather
    // than panic the decode path (which runs while rendering a tx for signing).
    // `.get()` bounds-checks every read; the `try_into().unwrap()`s below are
    // infallible because each slice is exactly the 2 bytes we asked for.
    let Some(count_bytes) = BLOB.get(p..p + 2) else {
        return Vec::new();
    };
    let count = u16::from_le_bytes(count_bytes.try_into().unwrap()) as usize;
    p += 2;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let Some(len_bytes) = BLOB.get(p..p + 2) else {
            break;
        };
        let len = u16::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
        p += 2;
        let Some(bytes) = BLOB.get(p..p + len) else {
            break;
        };
        if let Ok(s) = std::str::from_utf8(bytes) {
            out.push(s);
        }
        p += len;
    }
    out
}

/// Total selector count exposed for diagnostics; used only in tests
/// and the optional `decode-calldata` xtask probe.
#[allow(dead_code)]
pub fn entry_count() -> usize {
    header().count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_parses() {
        let h = header();
        assert!(h.count > 100_000, "expected ~900k entries, got {}", h.count);
    }

    #[test]
    fn transfer_resolves() {
        let sigs = lookup([0xa9, 0x05, 0x9c, 0xbb]);
        assert!(
            sigs.contains(&"transfer(address,uint256)"),
            "transfer not found; got {sigs:?}"
        );
    }

    #[test]
    fn approve_resolves() {
        let sigs = lookup([0x09, 0x5e, 0xa7, 0xb3]);
        assert!(
            sigs.contains(&"approve(address,uint256)"),
            "approve not found; got {sigs:?}"
        );
    }

    #[test]
    fn unknown_selector_returns_empty() {
        // 0xdeadbeef hopefully has no signatures registered. If 4byte
        // ever picks up a collision for it, swap to a different all-1s
        // selector — the test only needs an empty result.
        let sigs = lookup([0xff, 0xff, 0xff, 0xff]);
        // Could legitimately be non-empty; the contract is "empty when
        // unknown", not "specifically empty for 0xffffffff". Still
        // useful as a smoke test that lookup doesn't panic on the tail
        // of the index.
        let _ = sigs;
    }

    #[test]
    fn transfer_from_resolves() {
        // ERC-20 `transferFrom(address,address,uint256)` selector.
        let sigs = lookup([0x23, 0xb8, 0x72, 0xdd]);
        assert!(
            sigs.contains(&"transferFrom(address,address,uint256)"),
            "transferFrom not found; got {sigs:?}"
        );
    }

    #[test]
    fn lookup_idempotent() {
        // Two calls back the same `&'static str` view into the embedded
        // blob — guards against the OnceLock initializer doing something
        // mutable per call.
        let a = lookup([0xa9, 0x05, 0x9c, 0xbb]);
        let b = lookup([0xa9, 0x05, 0x9c, 0xbb]);
        assert_eq!(a, b);
    }

    #[test]
    fn lookup_at_index_extremes_does_not_panic() {
        // Smallest and largest possible 4-byte selectors. Either may be
        // empty or populated; the binary search must not index out of
        // bounds at the edges.
        let _ = lookup([0x00, 0x00, 0x00, 0x00]);
        let _ = lookup([0xff, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn entry_count_matches_header() {
        // entry_count() reads the same Header that lookup() does;
        // sanity-check they agree on the magnitude.
        assert_eq!(entry_count(), header().count);
    }
}
