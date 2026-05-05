//! Generate `assets/4byte.bin` from the ethereum-lists/4bytes corpus.
//!
//! Pulls the master tarball, walks `signatures/<8 hex>`, verifies each
//! `(selector, signature)` pair via `keccak256(signature)[..4] == selector`,
//! and emits a sorted binary blob the runtime loader binary-searches.
//!
//! ## File format (`4BYT` v1)
//!
//! ```text
//! header (16 bytes):
//!   [0..4]   magic = "4BYT"
//!   [4..8]   version u32 LE = 1
//!   [8..12]  count u32 LE      // number of unique selectors
//!   [12..16] reserved u32 LE   // 0
//!
//! index (count * 8 bytes), sorted by selector ascending (lex / big-endian):
//!   [u8; 4]  selector
//!   [u32 LE] strings_offset    // relative to start of strings region
//!
//! strings region, at file offset 16 + count*8:
//!   per selector:
//!     [u16 LE] sig_count
//!     for each sig:
//!       [u16 LE] len
//!       [u8; len] utf8
//! ```

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use sha3::{Digest, Keccak256};

const TARBALL_URL: &str =
    "https://codeload.github.com/ethereum-lists/4bytes/tar.gz/refs/heads/master";

const MAGIC: &[u8; 4] = b"4BYT";
const VERSION: u32 = 1;

pub async fn sync_4byte_db() -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent("kao-xtask/sync-4byte-db")
        .build()?;

    println!("fetching {TARBALL_URL}");
    let bytes = client
        .get(TARBALL_URL)
        .send()
        .await
        .map_err(|e| anyhow!("fetch tarball: {}", e.without_url()))?
        .error_for_status()
        .map_err(|e| anyhow!("fetch tarball: {}", e.without_url()))?
        .bytes()
        .await
        .map_err(|e| anyhow!("fetch tarball: {}", e.without_url()))?;
    println!("tarball: {} bytes compressed", bytes.len());

    // BTreeMap gives us sorted-by-selector ordering for free, which is
    // exactly what the binary-search loader needs.
    let mut entries: BTreeMap<[u8; 4], Vec<String>> = BTreeMap::new();
    let mut total: u64 = 0;
    let mut dropped: u64 = 0;
    let mut malformed: u64 = 0;

    let cursor = std::io::Cursor::new(&bytes[..]);
    let gz = flate2::read::GzDecoder::new(cursor);
    let mut tar = tar::Archive::new(gz);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();

        // Only accept `<root>/signatures/<8hex>` files.
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let in_signatures = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            == Some("signatures");
        if !in_signatures {
            continue;
        }
        if filename.len() != 8 {
            continue;
        }
        let Some(selector) = parse_hex4(filename) else {
            continue;
        };

        let mut content = String::new();
        if entry.read_to_string(&mut content).is_err() {
            malformed += 1;
            continue;
        }

        // Sigs in a single file are `;`-separated; trailing newlines exist on some.
        for sig in content.split(';') {
            let sig = sig.trim();
            if sig.is_empty() {
                continue;
            }
            total += 1;
            let mut h = Keccak256::new();
            h.update(sig.as_bytes());
            let digest = h.finalize();
            if digest[..4] == selector {
                entries.entry(selector).or_default().push(sig.to_owned());
            } else {
                dropped += 1;
            }
        }
    }

    let verified = total - dropped;
    println!(
        "parsed {total} entries: {verified} verified, {dropped} keccak mismatch, {malformed} read err"
    );
    println!("{} unique selectors", entries.len());

    // De-duplicate within each selector group while preserving order
    // (the dataset has the occasional repeat).
    for sigs in entries.values_mut() {
        let mut seen = std::collections::HashSet::new();
        sigs.retain(|s| seen.insert(s.clone()));
    }

    let blob = encode(&entries)?;
    let path = output_path()?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&path, &blob)
        .await
        .with_context(|| format!("write {}", path.display()))?;
    println!("wrote {} ({} bytes)", path.display(), blob.len());

    Ok(())
}

fn encode(entries: &BTreeMap<[u8; 4], Vec<String>>) -> Result<Vec<u8>> {
    let count: u32 = entries.len().try_into().context("too many selectors")?;

    // Build strings region first so we know each entry's offset.
    let mut strings: Vec<u8> = Vec::with_capacity(40 * 1024 * 1024);
    let mut offsets: Vec<u32> = Vec::with_capacity(entries.len());
    for sigs in entries.values() {
        let offset: u32 = strings
            .len()
            .try_into()
            .context("strings region overflowed u32")?;
        offsets.push(offset);
        let sig_count: u16 = sigs
            .len()
            .try_into()
            .context("too many sigs for one selector (>u16)")?;
        strings.extend_from_slice(&sig_count.to_le_bytes());
        for s in sigs {
            let len: u16 = s
                .len()
                .try_into()
                .context("signature too long (>u16 bytes)")?;
            strings.extend_from_slice(&len.to_le_bytes());
            strings.extend_from_slice(s.as_bytes());
        }
    }

    let mut out = Vec::with_capacity(16 + entries.len() * 8 + strings.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved

    for ((selector, _), offset) in entries.iter().zip(offsets.iter()) {
        out.extend_from_slice(selector);
        out.extend_from_slice(&offset.to_le_bytes());
    }
    out.extend_from_slice(&strings);
    Ok(out)
}

fn parse_hex4(s: &str) -> Option<[u8; 4]> {
    if s.len() != 8 {
        return None;
    }
    let mut out = [0u8; 4];
    for i in 0..4 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn output_path() -> Result<PathBuf> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR not set")?;
    let root = std::path::Path::new(&manifest)
        .parent()
        .context("xtask must live one level below the workspace root")?;
    Ok(root.join("assets").join("4byte.bin"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_layout() {
        let mut entries: BTreeMap<[u8; 4], Vec<String>> = BTreeMap::new();
        entries.insert([0xa9, 0x05, 0x9c, 0xbb], vec!["transfer(address,uint256)".into()]);
        let blob = encode(&entries).unwrap();
        assert_eq!(&blob[0..4], MAGIC);
        assert_eq!(u32::from_le_bytes(blob[4..8].try_into().unwrap()), VERSION);
        assert_eq!(u32::from_le_bytes(blob[8..12].try_into().unwrap()), 1);
        // index record at offset 16
        assert_eq!(&blob[16..20], &[0xa9, 0x05, 0x9c, 0xbb]);
        // strings region begins at 16 + 8 = 24
        // sig_count (u16) = 1, len (u16) = 25, then 25 utf8 bytes
        assert_eq!(u16::from_le_bytes(blob[24..26].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(blob[26..28].try_into().unwrap()), 25);
        assert_eq!(&blob[28..28 + 25], b"transfer(address,uint256)");
    }

    #[test]
    fn parse_hex4_round_trip() {
        assert_eq!(parse_hex4("a9059cbb"), Some([0xa9, 0x05, 0x9c, 0xbb]));
        assert_eq!(parse_hex4("00000000"), Some([0, 0, 0, 0]));
        assert_eq!(parse_hex4("a9059c"), None);
        assert_eq!(parse_hex4("zzzzzzzz"), None);
    }
}
