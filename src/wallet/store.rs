//! Persistent wallet store backed by redb + XChaCha20-Poly1305 + Argon2id.
//!
//! Layout
//! ------
//! - `meta` table (`&str -> &[u8]`):
//!   - `header`: postcard of `Header` (salt, Argon2 params, authenticated
//!     check blob, monotonic epoch).
//!   - `active_index`: `u32` LE bytes; plaintext, since the index leaks
//!     nothing useful and we want `redb` range scans over
//!     account keys to remain cheap.
//! - `accounts` table (`u32 -> &[u8]`):
//!   - value = `nonce(24) || ciphertext || tag(16)`. AAD binds the record to
//!     its `redb` key (`b"accounts:" || key.to_le_bytes()`) so values can't
//!     be silently moved between rows.
//!
//! XChaCha20-Poly1305 is preferred over plain ChaCha20-Poly1305 / AES-GCM:
//! the 192-bit nonce makes random nonces safe indefinitely, removing the
//! reuse footgun that comes with 96-bit AEADs.
//!
//! The master key is derived from the user's passphrase via Argon2id and the
//! random salt stored in `header`. The header also contains a small
//! authenticated blob whose successful decryption doubles as the "is this the
//! right password?" check.
//!
//! Rollback protection
//! -------------------
//! Per-row AAD already prevents within-file row swaps. To prevent an
//! attacker with filesystem access from swapping the *whole* file with an
//! older valid snapshot, every save bumps a monotonic `epoch` in the
//! header AND mirrors it to the OS keyring (anchored outside the file).
//! On load we compare the file's epoch against the keyring's; a file that
//! lags the keyring is a rollback. See `enforce_rollback_policy` for the
//! full table including the legitimate restore-from-backup path.
//!
//! The header's `epoch` field is in plaintext postcard but bound into the
//! AAD of `auth_check` (see `header_aad`) and re-sealed on every save, so
//! an attacker can't restore an older snapshot AND rewrite the on-disk
//! epoch byte to slip past the keyring check — that tampering surfaces
//! as a decrypt failure on next load.

use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rand::{RngCore, rngs::OsRng};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tracing::warn;
use zeroize::Zeroizing;

use super::{
    AccountDescriptor, Contact, SafeDescriptor, WalletDescriptor, WalletError, keyring as kr,
};

const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const ACCOUNTS_TABLE: TableDefinition<u32, &[u8]> = TableDefinition::new("accounts");
const CONTACTS_TABLE: TableDefinition<u32, &[u8]> = TableDefinition::new("contacts");
const SAFES_TABLE: TableDefinition<u32, &[u8]> = TableDefinition::new("safes");

const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const SALT_LEN: usize = 16;

const HEADER_KEY: &str = "header";
const ACTIVE_INDEX_KEY: &str = "active_index";
const HEADER_AAD: &[u8] = b"header:auth_check";
const ACCOUNT_AAD_PREFIX: &[u8] = b"accounts:";
const CONTACT_AAD_PREFIX: &[u8] = b"contacts:";
const SAFE_AAD_PREFIX: &[u8] = b"safes:";

const AUTH_CONSTANT: &[u8] = b"KAO_AUTH";

/// Length, in bytes, of the rollback-protection epoch as stored in the OS
/// keyring. Plain little-endian `u64` — there's no MAC because an attacker
/// who can write to the keyring already owns the box; we only need the
/// keyring as a *separate* store from the wallet file.
const EPOCH_LEN: usize = 8;

/// AAD used when sealing / opening the header's `auth_check` blob. Binds
/// the AEAD to the current epoch so an attacker who restores an older
/// wallet snapshot AND rewrites its plaintext `epoch` field (to match the
/// keyring's current value and slip past `enforce_rollback_policy`) sees
/// the auth_check decrypt fail instead.
///
/// Other mutable header fields don't need this treatment: `salt` and the
/// Argon2 parameters all feed into key derivation, so any tamper produces
/// a different master key that can't open `auth_check` regardless of AAD.
/// `epoch` is the only field that's load-bearing for security but doesn't
/// participate in key derivation.
fn header_aad(epoch: u64) -> Vec<u8> {
    let mut aad = Vec::with_capacity(HEADER_AAD.len() + EPOCH_LEN);
    aad.extend_from_slice(HEADER_AAD);
    aad.extend_from_slice(&epoch.to_le_bytes());
    aad
}

// Argon2id parameters. Targeting ~250–500ms on a modern desktop CPU; tune
// these on the actual target hardware before shipping. The chosen values are
// persisted in the header on first save, so future tuning will not invalidate
// existing wallets.
#[cfg(not(test))]
const DEFAULT_ARGON2_M_COST_KIB: u32 = 48 * 1024;
#[cfg(not(test))]
const DEFAULT_ARGON2_T_COST: u32 = 3;
#[cfg(not(test))]
const DEFAULT_ARGON2_P_COST: u32 = 1;

// Tests use the smallest legal Argon2id parameters so the suite finishes in
// well under a second total. Production wallets are unaffected.
#[cfg(test)]
const DEFAULT_ARGON2_M_COST_KIB: u32 = 8;
#[cfg(test)]
const DEFAULT_ARGON2_T_COST: u32 = 1;
#[cfg(test)]
const DEFAULT_ARGON2_P_COST: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Header {
    salt: [u8; SALT_LEN],
    argon2_m_cost_kib: u32,
    argon2_t_cost: u32,
    argon2_p_cost: u32,
    auth_check: Vec<u8>,
    /// Monotonic write counter used as the rollback-protection anchor.
    /// Bumped by 1 on every save. Mirrored into the OS keyring so a
    /// whole-file swap with an older snapshot can be detected on next
    /// load. Starts at 1 on the first save of a fresh wallet.
    epoch: u64,
}

pub fn db_path() -> PathBuf {
    crate::paths::data_dir().join("wallet.redb")
}

pub fn db_exists() -> bool {
    db_path().exists()
}

/// Persist `desc` under `passphrase` and bump the rollback-protection epoch.
///
/// On success the wallet file is durable AND the keyring's epoch entry has
/// been updated to match. On failure the meaning of the returned variant
/// matters:
///
/// - [`WalletError::SavedKeyringSyncFailed`]: the file IS on disk; only the
///   post-commit keyring write failed. The user's data is safe and the
///   next load will auto-resync. Surface as a "saved with a warning",
///   not as a failed save — retrying would just bump the epoch again and
///   hit the same condition.
/// - Any other variant: the file was NOT modified.
pub fn save_descriptor(
    desc: &WalletDescriptor,
    passphrase: &SecretString,
) -> Result<(), WalletError> {
    let path = db_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        restrict_to_owner(parent, 0o700)?;
    }
    save_to(&path, desc, passphrase)
}

pub fn load_descriptor(passphrase: &SecretString) -> Result<WalletDescriptor, WalletError> {
    let path = db_path();
    if !path.exists() {
        return Err(WalletError::NotFound);
    }
    load_from(&path, passphrase)
}

// Awaiting the unlock-screen modal that calls this on the user's
// "Accept" click. Tests exercise it directly.
#[allow(dead_code)]
/// Load the wallet bypassing the "no keyring record on this machine"
/// check, seeding the keyring with the file's current epoch on success.
///
/// Call only after the user has explicitly acknowledged a prior
/// [`WalletError::KeyringMissingEntry`] — this is the "I'm restoring from
/// my own backup, this isn't tampering" path. The function still
/// enforces rollback (file < keyring → refuse) and unavailable-keyring
/// (refuse) checks; those should never trigger on this path because
/// `KeyringMissingEntry` is what got us here in the first place, but
/// keeping the checks in means a racy keyring write between the warning
/// and the accept doesn't accidentally weaken the guarantees.
pub fn load_descriptor_accepting_keyring_reset(
    passphrase: &SecretString,
) -> Result<WalletDescriptor, WalletError> {
    let path = db_path();
    if !path.exists() {
        return Err(WalletError::NotFound);
    }
    load_from_accepting_keyring_reset(&path, passphrase)
}

fn save_to(path: &Path, desc: &WalletDescriptor, pw: &SecretString) -> Result<(), WalletError> {
    let db = Database::create(path).map_err(redb_err)?;
    // Tighten the on-disk permissions so other local users can't read the
    // (encrypted) wallet blob. Redb's open fd is unaffected; this only
    // restricts who can open the file in the future.
    restrict_to_owner(path, 0o600)?;
    let txn = db.begin_write().map_err(redb_err)?;

    let existing = {
        let meta = txn.open_table(META_TABLE).map_err(redb_err)?;
        match meta.get(HEADER_KEY).map_err(redb_err)? {
            Some(v) => Some(deserialize_header(v.value())?),
            None => None,
        }
    };

    let header = match existing {
        Some(h) => {
            // Existing wallet — verify the password matches the stored params
            // before we overwrite anything. Verify against the OLD epoch's
            // AAD because that's what sealed `h.auth_check`.
            let key = derive_master_key(
                pw,
                &h.salt,
                h.argon2_m_cost_kib,
                h.argon2_t_cost,
                h.argon2_p_cost,
            )?;
            decrypt_blob(key.as_slice(), &header_aad(h.epoch), &h.auth_check)
                .map_err(|_| WalletError::Encryption("incorrect password".into()))?;
            // Bump epoch. `checked_add` is overkill — at one save per second
            // it'd take ~584 billion years to wrap — but the alternative is
            // silently emitting epoch=0 which would look like a rollback.
            let new_epoch = h
                .epoch
                .checked_add(1)
                .ok_or_else(|| WalletError::Encryption("epoch counter overflow".into()))?;
            // Re-seal auth_check with the NEW epoch in the AAD. Without
            // this re-encryption the AEAD would still be bound to the old
            // epoch and either (a) fail to open on next load — breaking
            // legitimate saves — or (b) leave a stale binding that an
            // attacker could exploit by rolling back the file and rewriting
            // the plaintext epoch byte to match.
            let auth_check = encrypt_blob(key.as_slice(), &header_aad(new_epoch), AUTH_CONSTANT)?;
            Header {
                epoch: new_epoch,
                auth_check,
                ..h
            }
        }
        None => {
            // Fresh wallet — mint a salt and seal a fresh auth-check blob.
            let mut salt = [0u8; SALT_LEN];
            OsRng.fill_bytes(&mut salt);
            let key = derive_master_key(
                pw,
                &salt,
                DEFAULT_ARGON2_M_COST_KIB,
                DEFAULT_ARGON2_T_COST,
                DEFAULT_ARGON2_P_COST,
            )?;
            // First-ever save uses epoch=1 (not 0) so a wiped keyring
            // entry is unambiguously distinguishable from "valid first
            // save not yet mirrored to keyring".
            let epoch: u64 = 1;
            let auth_check = encrypt_blob(key.as_slice(), &header_aad(epoch), AUTH_CONSTANT)?;
            Header {
                salt,
                argon2_m_cost_kib: DEFAULT_ARGON2_M_COST_KIB,
                argon2_t_cost: DEFAULT_ARGON2_T_COST,
                argon2_p_cost: DEFAULT_ARGON2_P_COST,
                auth_check,
                epoch,
            }
        }
    };

    let master_key = derive_master_key(
        pw,
        &header.salt,
        header.argon2_m_cost_kib,
        header.argon2_t_cost,
        header.argon2_p_cost,
    )?;

    let header_bytes = postcard::to_stdvec(&header)
        .map_err(|e| WalletError::Encryption(format!("serialize header: {e}")))?;

    {
        let mut meta = txn.open_table(META_TABLE).map_err(redb_err)?;
        meta.insert(HEADER_KEY, header_bytes.as_slice())
            .map_err(redb_err)?;
        let active = (desc.active_index as u32).to_le_bytes();
        meta.insert(ACTIVE_INDEX_KEY, active.as_slice())
            .map_err(redb_err)?;
    }

    {
        let mut accounts = txn.open_table(ACCOUNTS_TABLE).map_err(redb_err)?;
        let existing_keys: Vec<u32> = {
            let mut acc = Vec::new();
            for entry in accounts.iter().map_err(redb_err)? {
                let (k, _) = entry.map_err(redb_err)?;
                acc.push(k.value());
            }
            acc
        };
        for k in existing_keys {
            accounts.remove(k).map_err(redb_err)?;
        }

        for (i, account) in desc.accounts.iter().enumerate() {
            let idx = i as u32;
            // Holds the account's private-key bytes — scrub it on drop rather
            // than leaving the serialized key in a freed allocation.
            let plaintext = Zeroizing::new(
                postcard::to_stdvec(account)
                    .map_err(|e| WalletError::Encryption(format!("serialize account {i}: {e}")))?,
            );
            let aad = account_aad(idx);
            let blob = encrypt_blob(master_key.as_slice(), &aad, &plaintext)?;
            accounts.insert(idx, blob.as_slice()).map_err(redb_err)?;
        }
    }

    // Safes table: same shape as accounts — wipe and re-insert from
    // `desc.safes` so a removed Safe drops its row. The `safes:` AAD
    // prefix prevents a Safe ciphertext from being silently swapped into
    // the accounts table or vice versa. Bundled into the same write txn
    // so the file is internally consistent on every commit.
    {
        let mut safes = txn.open_table(SAFES_TABLE).map_err(redb_err)?;
        let existing_keys: Vec<u32> = {
            let mut acc = Vec::new();
            for entry in safes.iter().map_err(redb_err)? {
                let (k, _) = entry.map_err(redb_err)?;
                acc.push(k.value());
            }
            acc
        };
        for k in existing_keys {
            safes.remove(k).map_err(redb_err)?;
        }

        for (i, safe) in desc.safes.iter().enumerate() {
            let idx = i as u32;
            let plaintext = Zeroizing::new(
                postcard::to_stdvec(safe)
                    .map_err(|e| WalletError::Encryption(format!("serialize safe {i}: {e}")))?,
            );
            let aad = safe_aad(idx);
            let blob = encrypt_blob(master_key.as_slice(), &aad, &plaintext)?;
            safes.insert(idx, blob.as_slice()).map_err(redb_err)?;
        }
    }

    txn.commit().map_err(redb_err)?;

    // File is durable. Now mirror the new epoch to the keyring. If this
    // fails, the file is still safely committed; the next load will
    // auto-resync (file > keyring is the benign "we just bumped" path) or
    // surface `KeyringMissingEntry`. The dedicated `SavedKeyringSyncFailed`
    // variant lets the caller distinguish "save aborted" from "saved but
    // rollback anchor stale" — important UX-wise because retrying a
    // SavedKeyringSyncFailed save would just bump the epoch again and
    // re-hit the same condition.
    kr::write_secret(
        &keyring_entry_name(&header.salt),
        &header.epoch.to_le_bytes(),
    )
    .map_err(map_save_keyring_err)?;
    Ok(())
}

/// Per-wallet keyring entry name. Derived from the header salt so each
/// wallet on a machine has its own slot (supports the future "multiple
/// wallets" use case) and so test wallets — each with a fresh random
/// salt — never collide in the process-global test backend.
fn keyring_entry_name(salt: &[u8]) -> String {
    debug_assert!(
        salt.len() >= 8,
        "salt must be at least 8 bytes for keyring naming"
    );
    format!(
        "wallet-epoch-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        salt[0], salt[1], salt[2], salt[3], salt[4], salt[5], salt[6], salt[7],
    )
}

/// Mapper for keyring failures from the *post-commit* save path. Both
/// kinds of failure (`Unavailable` and `Backend`) collapse to the soft
/// `SavedKeyringSyncFailed` variant: the file was already committed, so
/// there's no useful distinction at the save layer between "the
/// environment is broken" and "the keyring returned something weird" —
/// in both cases the user's data is safe and rollback protection is
/// degraded until the next load resyncs.
fn map_save_keyring_err(e: kr::KeyringError) -> WalletError {
    match e {
        kr::KeyringError::Unavailable(msg) => WalletError::SavedKeyringSyncFailed(msg),
        kr::KeyringError::Backend(msg) => {
            WalletError::SavedKeyringSyncFailed(format!("keyring backend: {msg}"))
        }
    }
}

fn decode_epoch(bytes: &[u8]) -> Result<u64, WalletError> {
    if bytes.len() != EPOCH_LEN {
        return Err(WalletError::Encryption(format!(
            "keyring epoch wrong size: got {}, expected {EPOCH_LEN}",
            bytes.len()
        )));
    }
    let mut buf = [0u8; EPOCH_LEN];
    buf.copy_from_slice(bytes);
    Ok(u64::from_le_bytes(buf))
}

/// Verify the file's epoch against what the keyring last saw and decide
/// whether the load may proceed.
///
/// Policy table:
///
/// | keyring state          | file vs keyring | action                           |
/// |------------------------|-----------------|----------------------------------|
/// | Unavailable            | n/a             | refuse → `KeyringUnavailable`    |
/// | NotFound + accept=true | n/a             | seed keyring with file.epoch, OK |
/// | NotFound + accept=false| n/a             | refuse → `KeyringMissingEntry`   |
/// | Some(k)                | file < k        | refuse → `Rollback`              |
/// | Some(k)                | file == k       | OK                               |
/// | Some(k)                | file > k        | best-effort resync to file.epoch |
///
/// `accept_missing_keyring` corresponds to the explicit user "I'm
/// restoring from backup, accept it" path; surfaced in the public API as
/// [`load_descriptor_accepting_keyring_reset`]. The keyring writes for
/// "accept" and "auto-resync" are best-effort: if they fail we proceed
/// (data is intact) and the same path will be hit again on next load.
fn enforce_rollback_policy(
    header: &Header,
    accept_missing_keyring: bool,
) -> Result<(), WalletError> {
    let entry_name = keyring_entry_name(&header.salt);
    let stored = match kr::read_secret(&entry_name) {
        Ok(v) => v,
        Err(kr::KeyringError::Unavailable(msg)) => {
            return Err(WalletError::KeyringUnavailable(msg));
        }
        Err(kr::KeyringError::Backend(msg)) => {
            return Err(WalletError::Encryption(format!("keyring: {msg}")));
        }
    };

    match stored {
        None if accept_missing_keyring => {
            if let Err(e) = kr::write_secret(&entry_name, &header.epoch.to_le_bytes()) {
                warn!("seeding keyring after explicit accept failed: {e}");
            }
            Ok(())
        }
        None => Err(WalletError::KeyringMissingEntry {
            file_epoch: header.epoch,
        }),
        Some(bytes) => {
            let keyring_epoch = decode_epoch(&bytes)?;
            if header.epoch < keyring_epoch {
                Err(WalletError::Rollback {
                    file: header.epoch,
                    expected: keyring_epoch,
                })
            } else if header.epoch > keyring_epoch {
                if let Err(e) = kr::write_secret(&entry_name, &header.epoch.to_le_bytes()) {
                    warn!("auto-resync keyring write failed: {e}");
                }
                Ok(())
            } else {
                Ok(())
            }
        }
    }
}

fn load_from(path: &Path, pw: &SecretString) -> Result<WalletDescriptor, WalletError> {
    load_from_with_policy(path, pw, false)
}

// Reachable only via the public bypass loader; that one's
// `#[allow(dead_code)]` until the UI wires it.
#[allow(dead_code)]
/// Same as `load_from`, but bypasses the "no keyring entry → refuse"
/// check and seeds the keyring with the file's epoch on success. Only
/// call after the user has explicitly accepted the security warning that
/// surfaces from a prior `KeyringMissingEntry` error.
fn load_from_accepting_keyring_reset(
    path: &Path,
    pw: &SecretString,
) -> Result<WalletDescriptor, WalletError> {
    load_from_with_policy(path, pw, true)
}

fn load_from_with_policy(
    path: &Path,
    pw: &SecretString,
    accept_missing_keyring: bool,
) -> Result<WalletDescriptor, WalletError> {
    let db = match Database::open(path) {
        Ok(db) => db,
        Err(redb::DatabaseError::Storage(redb::StorageError::Io(e)))
            if e.kind() == std::io::ErrorKind::NotFound =>
        {
            return Err(WalletError::NotFound);
        }
        Err(e) => return Err(redb_err(e)),
    };
    let txn = db.begin_read().map_err(redb_err)?;

    let meta = txn.open_table(META_TABLE).map_err(redb_err)?;
    let header_guard = meta
        .get(HEADER_KEY)
        .map_err(redb_err)?
        .ok_or_else(|| WalletError::Encryption("missing header in store".into()))?;
    let header = deserialize_header(header_guard.value())?;

    let master_key = derive_master_key(
        pw,
        &header.salt,
        header.argon2_m_cost_kib,
        header.argon2_t_cost,
        header.argon2_p_cost,
    )?;

    // AAD binds the AEAD to `header.epoch`; a tampered epoch field flips
    // this to a decrypt failure that surfaces as "incorrect password".
    // Same surface as a real wrong-password — by design, we don't tell
    // the caller which check failed.
    decrypt_blob(
        master_key.as_slice(),
        &header_aad(header.epoch),
        &header.auth_check,
    )
    .map_err(|_| WalletError::Encryption("incorrect password".into()))?;

    // Password OK — now check rollback policy. We deliberately do this
    // *after* password verification so that a wrong-password attacker
    // can't probe rollback state ("does this machine know about a wallet
    // here at all?") without the passphrase.
    enforce_rollback_policy(&header, accept_missing_keyring)?;

    let active_index = match meta.get(ACTIVE_INDEX_KEY).map_err(redb_err)? {
        Some(v) => {
            let bytes = v.value();
            if bytes.len() == 4 {
                u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize
            } else {
                0
            }
        }
        None => 0,
    };

    let accounts_tbl = txn.open_table(ACCOUNTS_TABLE).map_err(redb_err)?;
    let mut accounts: Vec<(u32, AccountDescriptor)> = Vec::new();
    for entry in accounts_tbl.iter().map_err(redb_err)? {
        let (k, v) = entry.map_err(redb_err)?;
        let idx = k.value();
        let aad = account_aad(idx);
        let plaintext = decrypt_blob(master_key.as_slice(), &aad, v.value())?;
        let account: AccountDescriptor = postcard::from_bytes(&plaintext)
            .map_err(|e| WalletError::Encryption(format!("deserialize account {idx}: {e}")))?;
        accounts.push((idx, account));
    }
    accounts.sort_by_key(|(i, _)| *i);
    let accounts: Vec<AccountDescriptor> = accounts.into_iter().map(|(_, a)| a).collect();

    if accounts.is_empty() {
        return Err(WalletError::Encryption("no accounts in store".into()));
    }

    // Safes table: same shape as contacts loader — a wallet written
    // before the safes feature shipped has no `safes` table at all, and
    // `open_table` on a read txn returns `TableDoesNotExist` in that
    // case. Collapse to an empty vec so old wallets load cleanly; the
    // next save lazily creates the table.
    let safes = match txn.open_table(SAFES_TABLE) {
        Ok(safes_tbl) => {
            let mut entries: Vec<(u32, SafeDescriptor)> = Vec::new();
            for entry in safes_tbl.iter().map_err(redb_err)? {
                let (k, v) = entry.map_err(redb_err)?;
                let idx = k.value();
                let aad = safe_aad(idx);
                let plaintext = decrypt_blob(master_key.as_slice(), &aad, v.value())?;
                let safe = decode_safe(&plaintext, idx)?;
                entries.push((idx, safe));
            }
            entries.sort_by_key(|(i, _)| *i);
            entries.into_iter().map(|(_, s)| s).collect()
        }
        Err(redb::TableError::TableDoesNotExist(_)) => Vec::new(),
        Err(e) => return Err(redb_err(e)),
    };

    let active_index = active_index.min(accounts.len() - 1);
    Ok(WalletDescriptor {
        accounts,
        safes,
        active_index,
    })
}

/// Decode one decrypted Safe row, tolerating the pre-`tx_service_url`
/// layout.
///
/// Postcard isn't self-describing: appending a field to
/// `SafeDescriptor` makes blobs written before the field one byte
/// short, and a plain `from_bytes` would hard-fail the whole wallet
/// load. Try the current shape first; on failure re-read as the legacy
/// shape and default the new field. New fields MUST keep appending to
/// the end of `SafeDescriptor` and extend the legacy fallback here.
fn decode_safe(plaintext: &[u8], idx: u32) -> Result<SafeDescriptor, WalletError> {
    /// `SafeDescriptor` exactly as persisted before `tx_service_url`
    /// shipped. Field order/types must never change.
    #[derive(serde::Deserialize)]
    struct LegacySafeV1 {
        name: Option<String>,
        chain_id: u64,
        address: [u8; 20],
        version: String,
        trust: crate::wallet::SafeTrust,
        threshold: u32,
        owners: Vec<[u8; 20]>,
        modules: Vec<[u8; 20]>,
        guard: Option<[u8; 20]>,
        fallback_handler: Option<[u8; 20]>,
        linked_signer_indices: Vec<u32>,
        sibling_chains: Vec<u64>,
        cached_at: u64,
    }

    if let Ok(safe) = postcard::from_bytes::<SafeDescriptor>(plaintext) {
        return Ok(safe);
    }
    let legacy: LegacySafeV1 = postcard::from_bytes(plaintext)
        .map_err(|e| WalletError::Encryption(format!("deserialize safe {idx}: {e}")))?;
    Ok(SafeDescriptor {
        name: legacy.name,
        chain_id: legacy.chain_id,
        address: legacy.address,
        version: legacy.version,
        trust: legacy.trust,
        threshold: legacy.threshold,
        owners: legacy.owners,
        modules: legacy.modules,
        guard: legacy.guard,
        fallback_handler: legacy.fallback_handler,
        linked_signer_indices: legacy.linked_signer_indices,
        sibling_chains: legacy.sibling_chains,
        cached_at: legacy.cached_at,
        tx_service_url: None,
    })
}

fn deserialize_header(bytes: &[u8]) -> Result<Header, WalletError> {
    postcard::from_bytes::<Header>(bytes)
        .map_err(|e| WalletError::Encryption(format!("deserialize header: {e}")))
}

fn derive_master_key(
    pw: &SecretString,
    salt: &[u8],
    m_cost_kib: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<Zeroizing<[u8; 32]>, WalletError> {
    let params = Params::new(m_cost_kib, t_cost, p_cost, Some(32))
        .map_err(|e| WalletError::Encryption(format!("argon2 params: {e}")))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(pw.expose_secret().as_bytes(), salt, out.as_mut())
        .map_err(|e| WalletError::Encryption(format!("argon2 hash: {e}")))?;
    Ok(out)
}

fn encrypt_blob(key: &[u8], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, WalletError> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|e| WalletError::Encryption(format!("encrypt: {e}")))?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypt one record. The returned plaintext may contain private-key
/// material (account rows), so it is wrapped in `Zeroizing` to scrub the
/// buffer when the caller drops it rather than leaving it in freed memory.
fn decrypt_blob(key: &[u8], aad: &[u8], blob: &[u8]) -> Result<Zeroizing<Vec<u8>>, WalletError> {
    if blob.len() < NONCE_LEN + TAG_LEN {
        return Err(WalletError::Encryption("ciphertext too short".into()));
    }
    let nonce = XNonce::from_slice(&blob[..NONCE_LEN]);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: &blob[NONCE_LEN..],
                aad,
            },
        )
        .map(Zeroizing::new)
        .map_err(|e| WalletError::Encryption(format!("decrypt: {e}")))
}

fn account_aad(idx: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(ACCOUNT_AAD_PREFIX.len() + 4);
    out.extend_from_slice(ACCOUNT_AAD_PREFIX);
    out.extend_from_slice(&idx.to_le_bytes());
    out
}

fn contact_aad(idx: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(CONTACT_AAD_PREFIX.len() + 4);
    out.extend_from_slice(CONTACT_AAD_PREFIX);
    out.extend_from_slice(&idx.to_le_bytes());
    out
}

fn safe_aad(idx: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(SAFE_AAD_PREFIX.len() + 4);
    out.extend_from_slice(SAFE_AAD_PREFIX);
    out.extend_from_slice(&idx.to_le_bytes());
    out
}

/// Persist the contacts list under the existing wallet's master key.
///
/// Reuses the same per-row AEAD pattern as accounts but lives in its own
/// `contacts` table — a contact edit doesn't re-encrypt every account row
/// (and an account edit doesn't re-encrypt every contact). The save does
/// NOT bump the rollback-protection epoch: contact data isn't security-
/// load-bearing for rollback (an attacker rolling back contacts would
/// already need to roll back the whole file, which the existing accounts-
/// epoch covers), and bumping per contact-save would be needless wear on
/// the keyring write path. The `contacts:` AAD prefix prevents a contact
/// ciphertext from being silently swapped into the accounts table or vice
/// versa.
pub fn save_contacts(contacts: &[Contact], passphrase: &SecretString) -> Result<(), WalletError> {
    save_contacts_to(&db_path(), contacts, passphrase)
}

pub fn load_contacts(passphrase: &SecretString) -> Result<Vec<Contact>, WalletError> {
    load_contacts_from(&db_path(), passphrase)
}

fn save_contacts_to(
    path: &Path,
    contacts: &[Contact],
    pw: &SecretString,
) -> Result<(), WalletError> {
    let db = Database::create(path).map_err(redb_err)?;
    restrict_to_owner(path, 0o600)?;
    let txn = db.begin_write().map_err(redb_err)?;

    // Verify password against the existing header. Contacts are only
    // stored alongside an existing wallet — a contacts save without a
    // header is a logic error elsewhere (the App should never dispatch
    // this before a successful unlock).
    let header = {
        let meta = txn.open_table(META_TABLE).map_err(redb_err)?;
        match meta.get(HEADER_KEY).map_err(redb_err)? {
            Some(v) => deserialize_header(v.value())?,
            None => {
                return Err(WalletError::Encryption(
                    "save_contacts called before wallet exists".into(),
                ));
            }
        }
    };
    let master_key = derive_master_key(
        pw,
        &header.salt,
        header.argon2_m_cost_kib,
        header.argon2_t_cost,
        header.argon2_p_cost,
    )?;
    decrypt_blob(
        master_key.as_slice(),
        &header_aad(header.epoch),
        &header.auth_check,
    )
    .map_err(|_| WalletError::Encryption("incorrect password".into()))?;

    {
        let mut tbl = txn.open_table(CONTACTS_TABLE).map_err(redb_err)?;
        // Wipe the table, then re-insert. Same shape as the accounts save
        // path so a smaller list correctly drops removed rows.
        let existing_keys: Vec<u32> = {
            let mut acc = Vec::new();
            for entry in tbl.iter().map_err(redb_err)? {
                let (k, _) = entry.map_err(redb_err)?;
                acc.push(k.value());
            }
            acc
        };
        for k in existing_keys {
            tbl.remove(k).map_err(redb_err)?;
        }
        for (i, contact) in contacts.iter().enumerate() {
            let idx = i as u32;
            let plaintext = Zeroizing::new(
                postcard::to_stdvec(contact)
                    .map_err(|e| WalletError::Encryption(format!("serialize contact {i}: {e}")))?,
            );
            let aad = contact_aad(idx);
            let blob = encrypt_blob(master_key.as_slice(), &aad, &plaintext)?;
            tbl.insert(idx, blob.as_slice()).map_err(redb_err)?;
        }
    }

    txn.commit().map_err(redb_err)?;
    Ok(())
}

fn load_contacts_from(path: &Path, pw: &SecretString) -> Result<Vec<Contact>, WalletError> {
    let db = match Database::open(path) {
        Ok(db) => db,
        Err(redb::DatabaseError::Storage(redb::StorageError::Io(e)))
            if e.kind() == std::io::ErrorKind::NotFound =>
        {
            return Err(WalletError::NotFound);
        }
        Err(e) => return Err(redb_err(e)),
    };
    let txn = db.begin_read().map_err(redb_err)?;

    let meta = txn.open_table(META_TABLE).map_err(redb_err)?;
    let header_guard = meta
        .get(HEADER_KEY)
        .map_err(redb_err)?
        .ok_or_else(|| WalletError::Encryption("missing header in store".into()))?;
    let header = deserialize_header(header_guard.value())?;

    let master_key = derive_master_key(
        pw,
        &header.salt,
        header.argon2_m_cost_kib,
        header.argon2_t_cost,
        header.argon2_p_cost,
    )?;
    decrypt_blob(
        master_key.as_slice(),
        &header_aad(header.epoch),
        &header.auth_check,
    )
    .map_err(|_| WalletError::Encryption("incorrect password".into()))?;

    // Treat a missing contacts table as empty — fresh-since-feature
    // wallets have never saved contacts, and the table is created lazily
    // on first save. `open_table` on a read txn returns
    // `TableError::TableDoesNotExist` for missing tables; collapse to []
    // rather than bubbling that up as an error.
    let contacts_tbl = match txn.open_table(CONTACTS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(redb_err(e)),
    };
    let mut entries: Vec<(u32, Contact)> = Vec::new();
    for entry in contacts_tbl.iter().map_err(redb_err)? {
        let (k, v) = entry.map_err(redb_err)?;
        let idx = k.value();
        let aad = contact_aad(idx);
        let plaintext = decrypt_blob(master_key.as_slice(), &aad, v.value())?;
        let contact: Contact = postcard::from_bytes(&plaintext)
            .map_err(|e| WalletError::Encryption(format!("deserialize contact {idx}: {e}")))?;
        entries.push((idx, contact));
    }
    entries.sort_by_key(|(i, _)| *i);
    Ok(entries.into_iter().map(|(_, c)| c).collect())
}

fn redb_err<E: std::fmt::Display>(e: E) -> WalletError {
    WalletError::Encryption(format!("store: {e}"))
}

/// Restrict a path to owner-only access. Unix-only — Windows lacks POSIX
/// modes and a proper ACL story is out of scope for this fix.
fn restrict_to_owner(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wallet::{AccountDescriptor, LedgerHdPath, WalletDescriptor};
    use tempfile::tempdir;

    fn pw(s: &str) -> SecretString {
        // Install the in-memory keyring backend if it isn't already.
        // Every test that constructs a passphrase is, by definition,
        // about to touch the wallet store — and the store now reads /
        // writes the OS keyring on every save and load. The OnceLock
        // guard inside `install` makes this cheap on repeat calls and
        // safe across parallel tests.
        crate::wallet::keyring::test_support::install();
        SecretString::new(s.to_string().into_boxed_str())
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor {
            accounts: vec![
                AccountDescriptor::Local {
                    name: Some("Treasury".into()),
                    key_bytes: crate::wallet::SecretKeyBytes::new([0xab; 32]),
                },
                AccountDescriptor::Ledger {
                    name: None,
                    path: LedgerHdPath::LedgerLive(2),
                    address: [0x33; 20],
                },
                AccountDescriptor::Local {
                    name: None,
                    key_bytes: crate::wallet::SecretKeyBytes::new([0xcd; 32]),
                },
            ],
            safes: Vec::new(),
            active_index: 2,
        };
        save_to(&path, &desc, &pw("hunter2")).unwrap();
        let loaded = load_from(&path, &pw("hunter2")).unwrap();
        assert_eq!(loaded.accounts.len(), 3);
        assert_eq!(loaded.active_index, 2);
        match &loaded.accounts[0] {
            AccountDescriptor::Local { name, key_bytes } => {
                assert_eq!(name.as_deref(), Some("Treasury"));
                assert_eq!(key_bytes.as_array(), &[0xab; 32]);
            }
            _ => panic!("expected Local"),
        }
        match &loaded.accounts[1] {
            AccountDescriptor::Ledger {
                name,
                path,
                address,
            } => {
                assert!(name.is_none());
                assert!(matches!(path, LedgerHdPath::LedgerLive(2)));
                assert_eq!(*address, [0x33; 20]);
            }
            _ => panic!("expected Ledger"),
        }
        match &loaded.accounts[2] {
            AccountDescriptor::Local { key_bytes, .. } => {
                assert_eq!(key_bytes.as_array(), &[0xcd; 32])
            }
            _ => panic!("expected Local"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_wallet_file_with_owner_only_perms() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor::single(AccountDescriptor::Local {
            name: None,
            key_bytes: crate::wallet::SecretKeyBytes::new([0x42; 32]),
        });
        save_to(&path, &desc, &pw("pw")).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0o600, got 0o{mode:o}");
    }

    #[test]
    fn load_returns_not_found_for_missing_file() {
        // Loading a nonexistent wallet file must surface `WalletError::NotFound`
        // — not a generic "missing header" error after silently creating an
        // empty redb. This is the contract the unlock screen relies on to
        // distinguish "no wallet yet, send to setup" from "real corruption".
        let dir = tempdir().unwrap();
        let path = dir.path().join("does_not_exist.redb");
        let err = load_from(&path, &pw("anything")).unwrap_err();
        assert!(
            matches!(err, WalletError::NotFound),
            "expected NotFound, got {err:?}",
        );
        assert!(
            !path.exists(),
            "load_from must not create the file as a side effect",
        );
    }

    #[test]
    fn wrong_password_fails_to_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor::single(AccountDescriptor::Local {
            name: None,
            key_bytes: crate::wallet::SecretKeyBytes::new([0x42; 32]),
        });
        save_to(&path, &desc, &pw("correct")).unwrap();
        let err = load_from(&path, &pw("wrong")).unwrap_err();
        match err {
            WalletError::Encryption(msg) => assert!(msg.contains("incorrect password")),
            other => panic!("expected Encryption error, got {other:?}"),
        }
    }

    #[test]
    fn wrong_password_refused_on_save_over_existing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor::single(AccountDescriptor::Local {
            name: None,
            key_bytes: crate::wallet::SecretKeyBytes::new([0x11; 32]),
        });
        save_to(&path, &desc, &pw("right")).unwrap();
        let err = save_to(&path, &desc, &pw("wrong")).unwrap_err();
        match err {
            WalletError::Encryption(msg) => assert!(msg.contains("incorrect password")),
            other => panic!("expected Encryption error, got {other:?}"),
        }
    }

    #[test]
    fn argon2_params_survive_header_roundtrip() {
        // Header is the only place the Argon2 work factors are stored. Tuning
        // the defaults later must not invalidate existing wallets, so confirm
        // a saved header round-trips through postcard with the values intact.
        let original = Header {
            salt: [0x7e; SALT_LEN],
            argon2_m_cost_kib: 12_345,
            argon2_t_cost: 4,
            argon2_p_cost: 2,
            auth_check: vec![0xaa; 64],
            epoch: 42,
        };
        let bytes = postcard::to_stdvec(&original).unwrap();
        let parsed = deserialize_header(&bytes).unwrap();
        assert_eq!(parsed.salt, original.salt);
        assert_eq!(parsed.argon2_m_cost_kib, original.argon2_m_cost_kib);
        assert_eq!(parsed.argon2_t_cost, original.argon2_t_cost);
        assert_eq!(parsed.argon2_p_cost, original.argon2_p_cost);
        assert_eq!(parsed.auth_check, original.auth_check);
        assert_eq!(parsed.epoch, original.epoch);
    }

    #[test]
    fn tampered_account_ciphertext_fails_to_decrypt() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor::single(AccountDescriptor::Local {
            name: None,
            key_bytes: crate::wallet::SecretKeyBytes::new([0x55; 32]),
        });
        save_to(&path, &desc, &pw("pw")).unwrap();

        // Flip a single byte inside the ciphertext (skipping the nonce prefix).
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut accounts = txn.open_table(ACCOUNTS_TABLE).unwrap();
                let mut blob = accounts.get(0u32).unwrap().unwrap().value().to_vec();
                let target = NONCE_LEN + 1;
                blob[target] ^= 0x01;
                accounts.insert(0u32, blob.as_slice()).unwrap();
            }
            txn.commit().unwrap();
        }

        let err = load_from(&path, &pw("pw")).unwrap_err();
        match err {
            WalletError::Encryption(msg) => assert!(msg.contains("decrypt"), "got: {msg}"),
            other => panic!("expected Encryption error, got {other:?}"),
        }
    }

    #[test]
    fn save_overwrite_replaces_account_set() {
        // Saving a smaller account set must drop rows from the previous save —
        // not silently leave them as ghost accounts that re-appear on load.
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");

        let big = WalletDescriptor {
            accounts: vec![
                AccountDescriptor::Local {
                    name: None,
                    key_bytes: crate::wallet::SecretKeyBytes::new([0x01; 32]),
                },
                AccountDescriptor::Local {
                    name: None,
                    key_bytes: crate::wallet::SecretKeyBytes::new([0x02; 32]),
                },
                AccountDescriptor::Local {
                    name: None,
                    key_bytes: crate::wallet::SecretKeyBytes::new([0x03; 32]),
                },
            ],
            safes: Vec::new(),
            active_index: 2,
        };
        save_to(&path, &big, &pw("pw")).unwrap();

        let small = WalletDescriptor::single(AccountDescriptor::Local {
            name: None,
            key_bytes: crate::wallet::SecretKeyBytes::new([0x09; 32]),
        });
        save_to(&path, &small, &pw("pw")).unwrap();

        let loaded = load_from(&path, &pw("pw")).unwrap();
        assert_eq!(loaded.accounts.len(), 1);
        assert_eq!(loaded.active_index, 0);
        match &loaded.accounts[0] {
            AccountDescriptor::Local { key_bytes, .. } => {
                assert_eq!(key_bytes.as_array(), &[0x09; 32])
            }
            _ => panic!("expected Local"),
        }
    }

    // ── Rollback-protection tests ─────────────────────────────────────────

    /// Helper: open a wallet file just long enough to extract the salt,
    /// which tests need to derive the keyring entry name.
    fn read_salt_from_file(path: &Path) -> [u8; SALT_LEN] {
        let db = Database::open(path).unwrap();
        let txn = db.begin_read().unwrap();
        let meta = txn.open_table(META_TABLE).unwrap();
        let header_bytes = meta.get(HEADER_KEY).unwrap().unwrap().value().to_vec();
        deserialize_header(&header_bytes).unwrap().salt
    }

    fn epoch_from_keyring(entry: &str) -> u64 {
        let bytes = crate::wallet::keyring::read_secret(entry)
            .unwrap()
            .expect("keyring entry should exist");
        decode_epoch(&bytes).unwrap()
    }

    #[test]
    fn save_bumps_epoch_and_writes_keyring() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor::single(AccountDescriptor::Local {
            name: None,
            key_bytes: crate::wallet::SecretKeyBytes::new([0x42; 32]),
        });
        save_to(&path, &desc, &pw("pw")).unwrap();
        let salt = read_salt_from_file(&path);
        let entry = keyring_entry_name(&salt);
        assert_eq!(epoch_from_keyring(&entry), 1, "first save → epoch 1");

        save_to(&path, &desc, &pw("pw")).unwrap();
        assert_eq!(epoch_from_keyring(&entry), 2, "second save → epoch 2");

        save_to(&path, &desc, &pw("pw")).unwrap();
        assert_eq!(epoch_from_keyring(&entry), 3, "third save → epoch 3");
    }

    #[test]
    fn rolled_back_file_is_rejected() {
        // Capture the wallet bytes after one save, do another save, then
        // restore the older snapshot to disk. The file's epoch is now
        // strictly less than the keyring's last-seen epoch — that's the
        // canonical "attacker rolled back the file" signature, and load
        // must refuse rather than silently expose the older state.
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor::single(AccountDescriptor::Local {
            name: None,
            key_bytes: crate::wallet::SecretKeyBytes::new([0x42; 32]),
        });
        save_to(&path, &desc, &pw("pw")).unwrap();
        let snapshot = std::fs::read(&path).unwrap();
        save_to(&path, &desc, &pw("pw")).unwrap();
        std::fs::write(&path, &snapshot).unwrap();

        let err = load_from(&path, &pw("pw")).unwrap_err();
        match err {
            WalletError::Rollback { file, expected } => {
                assert_eq!(file, 1, "snapshot was taken at epoch 1");
                assert_eq!(expected, 2, "second save bumped keyring to 2");
            }
            other => panic!("expected Rollback, got {other:?}"),
        }
    }

    #[test]
    fn missing_keyring_entry_blocks_strict_load() {
        // Simulates a wallet file present but no keyring record on this
        // machine: legit "restore from backup" / "fresh OS user", OR an
        // attacker who copied the file. Strict load refuses; the explicit-
        // accept loader is the only path through.
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor::single(AccountDescriptor::Local {
            name: None,
            key_bytes: crate::wallet::SecretKeyBytes::new([0x42; 32]),
        });
        save_to(&path, &desc, &pw("pw")).unwrap();
        let salt = read_salt_from_file(&path);
        let entry = keyring_entry_name(&salt);
        crate::wallet::keyring::delete_secret(&entry).unwrap();

        let err = load_from(&path, &pw("pw")).unwrap_err();
        match err {
            WalletError::KeyringMissingEntry { file_epoch } => {
                assert_eq!(file_epoch, 1);
            }
            other => panic!("expected KeyringMissingEntry, got {other:?}"),
        }
    }

    #[test]
    fn accepting_loader_seeds_keyring_and_unblocks_strict_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor::single(AccountDescriptor::Local {
            name: None,
            key_bytes: crate::wallet::SecretKeyBytes::new([0x42; 32]),
        });
        save_to(&path, &desc, &pw("pw")).unwrap();
        save_to(&path, &desc, &pw("pw")).unwrap(); // file=2, keyring=2
        let salt = read_salt_from_file(&path);
        let entry = keyring_entry_name(&salt);
        crate::wallet::keyring::delete_secret(&entry).unwrap();

        // Bypass loader returns the descriptor and writes file.epoch
        // back to the keyring so subsequent strict loads are clean.
        let loaded = load_from_accepting_keyring_reset(&path, &pw("pw")).unwrap();
        assert_eq!(loaded.accounts.len(), 1);
        assert_eq!(epoch_from_keyring(&entry), 2);
        load_from(&path, &pw("pw")).unwrap();
    }

    #[test]
    fn load_auto_resyncs_when_file_ahead_of_keyring() {
        // Simulates a prior save whose keyring write failed (or was racily
        // observed mid-write): file has the new epoch, keyring still has
        // the old one. This is the benign "we just bumped" pattern, not
        // an attack — load must succeed and pull the keyring up to the
        // file's epoch so the next strict load is clean.
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor::single(AccountDescriptor::Local {
            name: None,
            key_bytes: crate::wallet::SecretKeyBytes::new([0x42; 32]),
        });
        save_to(&path, &desc, &pw("pw")).unwrap();
        save_to(&path, &desc, &pw("pw")).unwrap(); // file=2, keyring=2
        let salt = read_salt_from_file(&path);
        let entry = keyring_entry_name(&salt);
        crate::wallet::keyring::write_secret(&entry, &1u64.to_le_bytes()).unwrap();

        load_from(&path, &pw("pw")).unwrap();
        assert_eq!(
            epoch_from_keyring(&entry),
            2,
            "keyring resynced up to file epoch"
        );
    }

    #[test]
    fn save_keyring_failure_maps_to_saved_soft_variant() {
        // The post-commit keyring write doesn't get to abort the save —
        // the file is already durable. Both Unavailable and Backend
        // failures from the wrapper collapse to `SavedKeyringSyncFailed`
        // so the UI can treat the save as "succeeded with a warning"
        // rather than re-prompting the user (a retry would just bump
        // the epoch again and hit the same condition).
        use crate::wallet::keyring::KeyringError;
        let mapped = map_save_keyring_err(KeyringError::Unavailable("dbus down".into()));
        assert!(
            matches!(mapped, WalletError::SavedKeyringSyncFailed(ref m) if m.contains("dbus down")),
            "expected SavedKeyringSyncFailed carrying the unavailable detail, got {mapped:?}",
        );
        let mapped = map_save_keyring_err(KeyringError::Backend("oversize value".into()));
        assert!(
            matches!(mapped, WalletError::SavedKeyringSyncFailed(ref m) if m.contains("oversize value")),
            "expected SavedKeyringSyncFailed carrying the backend detail, got {mapped:?}",
        );
    }

    #[test]
    fn rollback_check_runs_after_password_verification() {
        // A wrong-password caller must not be able to probe rollback state
        // ("does this machine know about a wallet at this path?"), so the
        // rollback policy check must happen *after* the AEAD auth-check
        // verifies the passphrase. Concretely: even when the keyring has
        // been wiped (which would normally surface KeyringMissingEntry),
        // a wrong password must still surface Encryption("incorrect
        // password") — not the rollback variant.
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor::single(AccountDescriptor::Local {
            name: None,
            key_bytes: crate::wallet::SecretKeyBytes::new([0x42; 32]),
        });
        save_to(&path, &desc, &pw("pw")).unwrap();
        let salt = read_salt_from_file(&path);
        let entry = keyring_entry_name(&salt);
        crate::wallet::keyring::delete_secret(&entry).unwrap();

        let err = load_from(&path, &pw("WRONG")).unwrap_err();
        match err {
            WalletError::Encryption(msg) => assert!(msg.contains("incorrect password")),
            other => panic!("expected wrong-password error, got {other:?}"),
        }
    }

    /// Helper: read the header bytes from a wallet file, deserialize, hand
    /// to the caller for mutation, then re-serialize and write back.
    fn rewrite_header(path: &Path, mutate: impl FnOnce(&mut Header)) {
        let header_bytes = {
            let db = Database::open(path).unwrap();
            let txn = db.begin_read().unwrap();
            let meta = txn.open_table(META_TABLE).unwrap();
            meta.get(HEADER_KEY).unwrap().unwrap().value().to_vec()
        };
        let mut header: Header = postcard::from_bytes(&header_bytes).unwrap();
        mutate(&mut header);
        let new_bytes = postcard::to_stdvec(&header).unwrap();
        let db = Database::create(path).unwrap();
        let txn = db.begin_write().unwrap();
        {
            let mut meta = txn.open_table(META_TABLE).unwrap();
            meta.insert(HEADER_KEY, new_bytes.as_slice()).unwrap();
        }
        txn.commit().unwrap();
    }

    #[test]
    fn tampered_header_epoch_field_fails_to_load() {
        // The plaintext `epoch` byte in the postcard-encoded header is the lever an
        // attacker would pull to defeat rollback protection: restore an
        // older snapshot and rewrite its epoch to match (or exceed) the
        // keyring's last-seen value, so `enforce_rollback_policy` waves it
        // through. Binding `epoch` into the auth_check AAD turns that
        // tamper into an AEAD decrypt failure.
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor::single(AccountDescriptor::Local {
            name: None,
            key_bytes: crate::wallet::SecretKeyBytes::new([0x42; 32]),
        });
        save_to(&path, &desc, &pw("pw")).unwrap();

        rewrite_header(&path, |h| h.epoch = h.epoch.wrapping_add(99));

        let err = load_from(&path, &pw("pw")).unwrap_err();
        match err {
            // Same surface as wrong-password — we don't leak which check
            // failed. Crucially NOT `Rollback` (would mean we noticed the
            // mismatch via keyring comparison only) and NOT `Ok` (would
            // mean the tamper succeeded).
            WalletError::Encryption(msg) => {
                assert!(msg.contains("incorrect password"), "got: {msg}")
            }
            other => panic!("expected Encryption(\"incorrect password\"), got {other:?}"),
        }
    }

    #[test]
    fn rollback_with_epoch_rewrite_is_blocked() {
        // End-to-end version of the attack the AAD binding exists to
        // defeat: save twice (file/keyring at epoch=2), restore the
        // epoch=1 snapshot, then patch the postcard-encoded epoch byte from 1 to
        // 2 so the keyring check passes. Without the AAD binding this
        // would load the old account set silently. With the binding,
        // auth_check decrypt fails because it was sealed at epoch=1.
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor::single(AccountDescriptor::Local {
            name: None,
            key_bytes: crate::wallet::SecretKeyBytes::new([0x42; 32]),
        });
        save_to(&path, &desc, &pw("pw")).unwrap();
        let snapshot = std::fs::read(&path).unwrap();
        save_to(&path, &desc, &pw("pw")).unwrap();
        std::fs::write(&path, &snapshot).unwrap();
        rewrite_header(&path, |h| {
            assert_eq!(h.epoch, 1, "snapshot was taken after first save");
            h.epoch = 2;
        });

        let err = load_from(&path, &pw("pw")).unwrap_err();
        assert!(
            matches!(err, WalletError::Encryption(_)),
            "expected Encryption decrypt failure, got {err:?}",
        );
    }

    /// Bind-AAD test: take the ciphertext for account 0 and stuff it under
    /// key 1. Loading must reject — the AAD on decrypt won't match.
    #[test]
    fn aad_binding_rejects_swapped_account_record() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor {
            accounts: vec![
                AccountDescriptor::Local {
                    name: None,
                    key_bytes: crate::wallet::SecretKeyBytes::new([0x01; 32]),
                },
                AccountDescriptor::Local {
                    name: None,
                    key_bytes: crate::wallet::SecretKeyBytes::new([0x02; 32]),
                },
            ],
            safes: Vec::new(),
            active_index: 0,
        };
        save_to(&path, &desc, &pw("pw")).unwrap();

        // Swap rows 0 and 1 in the accounts table.
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut accounts = txn.open_table(ACCOUNTS_TABLE).unwrap();
                let a = accounts.get(0u32).unwrap().unwrap().value().to_vec();
                let b = accounts.get(1u32).unwrap().unwrap().value().to_vec();
                accounts.insert(0u32, b.as_slice()).unwrap();
                accounts.insert(1u32, a.as_slice()).unwrap();
            }
            txn.commit().unwrap();
        }

        let err = load_from(&path, &pw("pw")).unwrap_err();
        match err {
            WalletError::Encryption(msg) => assert!(msg.contains("decrypt")),
            other => panic!("expected Encryption error, got {other:?}"),
        }
    }

    // ── Contacts tests ───────────────────────────────────────────────────

    use crate::wallet::{Contact, ContactEns};

    fn sample_contact(seed: u8, name: &str) -> Contact {
        Contact {
            name: name.into(),
            address: [seed; 20],
            kaomoji: "(◕‿◕)".into(),
            notes: format!("seed {seed}"),
            ens: None,
        }
    }

    #[test]
    fn contacts_save_and_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor::single(AccountDescriptor::Local {
            name: None,
            key_bytes: crate::wallet::SecretKeyBytes::new([0x42; 32]),
        });
        save_to(&path, &desc, &pw("pw")).unwrap();

        let contacts = vec![
            sample_contact(0x01, "A"),
            Contact {
                name: "vitalik.eth".into(),
                address: [0xab; 20],
                kaomoji: "(◕‿◕✿)".into(),
                notes: String::new(),
                ens: Some(ContactEns {
                    name: "vitalik.eth".into(),
                    last_resolved_addr: [0xab; 20],
                }),
            },
        ];
        save_contacts_to(&path, &contacts, &pw("pw")).unwrap();
        let loaded = load_contacts_from(&path, &pw("pw")).unwrap();
        assert_eq!(loaded, contacts);
    }

    #[test]
    fn contacts_load_returns_empty_when_table_missing() {
        // A wallet saved before the contacts feature has no contacts
        // table at all. `load_contacts` must collapse that to an empty
        // vec rather than erroring — otherwise unlock would fail
        // post-feature on every pre-feature wallet.
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor::single(AccountDescriptor::Local {
            name: None,
            key_bytes: crate::wallet::SecretKeyBytes::new([0x42; 32]),
        });
        save_to(&path, &desc, &pw("pw")).unwrap();
        let loaded = load_contacts_from(&path, &pw("pw")).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn contacts_save_overwrite_replaces_set() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor::single(AccountDescriptor::Local {
            name: None,
            key_bytes: crate::wallet::SecretKeyBytes::new([0x42; 32]),
        });
        save_to(&path, &desc, &pw("pw")).unwrap();

        let big = vec![
            sample_contact(0x01, "A"),
            sample_contact(0x02, "B"),
            sample_contact(0x03, "C"),
        ];
        save_contacts_to(&path, &big, &pw("pw")).unwrap();
        let small = vec![sample_contact(0x09, "Solo")];
        save_contacts_to(&path, &small, &pw("pw")).unwrap();
        let loaded = load_contacts_from(&path, &pw("pw")).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "Solo");
    }

    #[test]
    fn contacts_save_does_not_bump_epoch() {
        // The accounts-epoch is the rollback-protection anchor; bumping
        // it on every contact save would burn the keyring write path
        // for no security benefit (a contact rollback already implies a
        // file-level rollback, which the existing accounts-epoch covers).
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor::single(AccountDescriptor::Local {
            name: None,
            key_bytes: crate::wallet::SecretKeyBytes::new([0x42; 32]),
        });
        save_to(&path, &desc, &pw("pw")).unwrap();
        let salt = read_salt_from_file(&path);
        let entry = keyring_entry_name(&salt);
        let before = epoch_from_keyring(&entry);
        save_contacts_to(&path, &[sample_contact(0x01, "A")], &pw("pw")).unwrap();
        save_contacts_to(&path, &[sample_contact(0x01, "A2")], &pw("pw")).unwrap();
        let after = epoch_from_keyring(&entry);
        assert_eq!(
            before, after,
            "contacts saves must not bump the accounts epoch"
        );
    }

    #[test]
    fn contacts_save_refuses_wrong_password() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = WalletDescriptor::single(AccountDescriptor::Local {
            name: None,
            key_bytes: crate::wallet::SecretKeyBytes::new([0x42; 32]),
        });
        save_to(&path, &desc, &pw("right")).unwrap();
        let err = save_contacts_to(&path, &[sample_contact(0x01, "A")], &pw("wrong")).unwrap_err();
        match err {
            WalletError::Encryption(msg) => assert!(msg.contains("incorrect password")),
            other => panic!("expected wrong-password error, got {other:?}"),
        }
    }

    #[test]
    fn contact_aad_namespaces_against_account_aad() {
        // A blob encrypted under the contacts AAD must not decrypt under
        // the accounts AAD (and vice versa). This is the mechanism that
        // stops a copy-paste attack across tables — the AEAD tag binds
        // the row to its table.
        let key = [0x77u8; 32];
        let plaintext = b"hello";
        let blob_c = encrypt_blob(&key, &contact_aad(0), plaintext).unwrap();
        let blob_a = encrypt_blob(&key, &account_aad(0), plaintext).unwrap();
        assert!(decrypt_blob(&key, &account_aad(0), &blob_c).is_err());
        assert!(decrypt_blob(&key, &contact_aad(0), &blob_a).is_err());
        // Sanity: same AAD on both sides round-trips.
        assert_eq!(
            decrypt_blob(&key, &contact_aad(0), &blob_c)
                .unwrap()
                .as_slice(),
            plaintext,
        );
        assert_eq!(
            decrypt_blob(&key, &account_aad(0), &blob_a)
                .unwrap()
                .as_slice(),
            plaintext,
        );
    }

    // ── Safes tests ──────────────────────────────────────────────────────

    use crate::wallet::{SafeDescriptor, SafeTrust};

    /// Minimal Canonical Safe — owner-of-one threshold-one. Tests that
    /// don't care about the contents use this and override fields as
    /// needed.
    fn minimal_safe(seed: u8, chain_id: u64) -> SafeDescriptor {
        SafeDescriptor {
            name: Some(format!("Safe seed {seed:02x}")),
            chain_id,
            address: [seed; 20],
            version: "1.4.1".into(),
            trust: SafeTrust::Canonical,
            threshold: 1,
            owners: vec![[seed.wrapping_add(1); 20]],
            modules: Vec::new(),
            guard: None,
            fallback_handler: None,
            linked_signer_indices: vec![0],
            sibling_chains: Vec::new(),
            cached_at: 1_700_000_000,
            tx_service_url: None,
        }
    }

    /// Safe with every optional field populated — exercises the
    /// "loud surfaces" (modules, guard, fallback handler), the
    /// unrecognized-implementation branch, multiple owners and signer
    /// links, and the sibling-chains list. The deserialized round-trip
    /// of this must equal the input bit-for-bit.
    fn rich_safe(seed: u8) -> SafeDescriptor {
        SafeDescriptor {
            name: None,   // exercise the unnamed path
            chain_id: 10, // Optimism
            address: [seed; 20],
            version: "1.3.0".into(),
            trust: SafeTrust::UnrecognizedImpl,
            threshold: 3,
            owners: vec![[0xa1; 20], [0xa2; 20], [0xa3; 20], [0xa4; 20]],
            modules: vec![[0xb1; 20], [0xb2; 20]],
            guard: Some([0xc1; 20]),
            fallback_handler: Some([0xd1; 20]),
            linked_signer_indices: vec![0, 2],
            sibling_chains: vec![1, 8453], // mainnet, base
            cached_at: 1_700_000_042,
            tx_service_url: Some("https://txs.example-dao.org".into()),
        }
    }

    #[test]
    fn decode_safe_falls_back_to_pre_service_url_layout() {
        // Serialize the legacy 13-field shape verbatim (everything up to
        // `cached_at`) and decode through the loader's fallback — older
        // wallets must load with `tx_service_url: None`, not error.
        let current = minimal_safe(0x07, 1);
        let legacy_bytes = postcard::to_stdvec(&(
            &current.name,
            current.chain_id,
            current.address,
            &current.version,
            &current.trust,
            current.threshold,
            &current.owners,
            &current.modules,
            current.guard,
            current.fallback_handler,
            &current.linked_signer_indices,
            &current.sibling_chains,
            current.cached_at,
        ))
        .unwrap();
        let decoded = decode_safe(&legacy_bytes, 0).unwrap();
        assert_eq!(decoded, current);
        assert!(decoded.tx_service_url.is_none());

        // And the current shape still round-trips, custom URL intact.
        let rich = rich_safe(0x42);
        let bytes = postcard::to_stdvec(&rich).unwrap();
        assert_eq!(decode_safe(&bytes, 1).unwrap(), rich);
    }

    fn wallet_with_safes(safes: Vec<SafeDescriptor>) -> WalletDescriptor {
        WalletDescriptor {
            accounts: vec![AccountDescriptor::Local {
                name: None,
                key_bytes: crate::wallet::SecretKeyBytes::new([0x42; 32]),
            }],
            safes,
            active_index: 0,
        }
    }

    #[test]
    fn safes_save_and_load_roundtrip_preserves_every_field() {
        // The rich variant exercises both SafeTrust branches, optional
        // guard/fallback (Some), multi-element owners/modules/linked-
        // signers/siblings, and the unnamed display-name fallback path.
        // Anything that gets lost between save and load shows up here.
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = wallet_with_safes(vec![minimal_safe(0x01, 1), rich_safe(0x55)]);
        save_to(&path, &desc, &pw("hunter2")).unwrap();
        let loaded = load_from(&path, &pw("hunter2")).unwrap();
        assert_eq!(loaded.safes, desc.safes);
        // Sanity: accounts didn't get scrambled by the new safes loop.
        assert_eq!(loaded.accounts.len(), 1);
    }

    #[test]
    fn safes_load_returns_empty_when_table_missing() {
        // A wallet saved before the safes feature has no `safes` table at
        // all — the redb file only contains `meta`, `accounts`, and
        // possibly `contacts`. `load_from` must collapse the missing
        // table to an empty vec rather than erroring, otherwise unlock
        // breaks for every pre-feature wallet on upgrade.
        //
        // We can't easily construct a "pre-feature" file in-test (the
        // current `save_to` always opens the safes table). So we save
        // normally, then drop the safes table out-of-band and reload.
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = wallet_with_safes(Vec::new());
        save_to(&path, &desc, &pw("pw")).unwrap();
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            txn.delete_table(SAFES_TABLE).unwrap();
            txn.commit().unwrap();
        }

        let loaded = load_from(&path, &pw("pw")).unwrap();
        assert!(loaded.safes.is_empty());
        assert_eq!(loaded.accounts.len(), 1);
    }

    #[test]
    fn safes_save_overwrite_drops_removed_rows() {
        // A second save with fewer safes must actually shrink the table —
        // not silently leave the deleted rows as ghosts that re-appear on
        // load. Same shape as `save_overwrite_replaces_account_set`.
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let big = wallet_with_safes(vec![
            minimal_safe(0x01, 1),
            minimal_safe(0x02, 10),
            minimal_safe(0x03, 8453),
        ]);
        save_to(&path, &big, &pw("pw")).unwrap();
        let small = wallet_with_safes(vec![minimal_safe(0x09, 1)]);
        save_to(&path, &small, &pw("pw")).unwrap();

        let loaded = load_from(&path, &pw("pw")).unwrap();
        assert_eq!(loaded.safes.len(), 1);
        assert_eq!(loaded.safes[0].address, [0x09; 20]);
    }

    #[test]
    fn safes_save_with_empty_vec_clears_existing_table() {
        // Onboarding flow inverse: a wallet that had safes, all removed.
        // The table must end up empty, not retain old entries.
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let with_safes = wallet_with_safes(vec![minimal_safe(0x01, 1), minimal_safe(0x02, 10)]);
        save_to(&path, &with_safes, &pw("pw")).unwrap();
        let without = wallet_with_safes(Vec::new());
        save_to(&path, &without, &pw("pw")).unwrap();

        let loaded = load_from(&path, &pw("pw")).unwrap();
        assert!(loaded.safes.is_empty());
    }

    #[test]
    fn safes_preserve_insertion_order_via_index() {
        // Iteration order from `load_from` must match the order in the
        // input vec, because the UI surfaces Safes in the user's
        // chronological add order (matches accounts behavior). The
        // explicit sort-by-key in load_from is what guarantees this.
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = wallet_with_safes(vec![
            minimal_safe(0xaa, 8453), // base
            minimal_safe(0xbb, 1),    // mainnet
            minimal_safe(0xcc, 10),   // optimism
        ]);
        save_to(&path, &desc, &pw("pw")).unwrap();
        let loaded = load_from(&path, &pw("pw")).unwrap();
        let chain_ids: Vec<u64> = loaded.safes.iter().map(|s| s.chain_id).collect();
        assert_eq!(chain_ids, vec![8453, 1, 10]);
    }

    #[test]
    fn safes_and_accounts_save_atomically_under_one_commit() {
        // The whole point of bundling safes into `save_to` (rather than
        // a contacts-style sidecar) is that a wallet snapshot is always
        // internally consistent: you cannot observe "new safe added but
        // accounts still old" or vice versa. This test does a save that
        // changes BOTH and asserts both are visible after a single
        // reload. (A regression that committed safes after accounts in
        // a separate txn would leave a window where this fails.)
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let initial = wallet_with_safes(Vec::new());
        save_to(&path, &initial, &pw("pw")).unwrap();

        let updated = WalletDescriptor {
            accounts: vec![
                AccountDescriptor::Local {
                    name: Some("primary".into()),
                    key_bytes: crate::wallet::SecretKeyBytes::new([0x42; 32]),
                },
                AccountDescriptor::Local {
                    name: Some("hardware-backup".into()),
                    key_bytes: crate::wallet::SecretKeyBytes::new([0x43; 32]),
                },
            ],
            safes: vec![minimal_safe(0x77, 1)],
            active_index: 1,
        };
        save_to(&path, &updated, &pw("pw")).unwrap();

        let loaded = load_from(&path, &pw("pw")).unwrap();
        assert_eq!(loaded.accounts.len(), 2);
        assert_eq!(loaded.safes.len(), 1);
        assert_eq!(loaded.active_index, 1);
    }

    #[test]
    fn safes_save_bumps_rollback_epoch() {
        // Safes ARE security-bearing for rollback (unlike contacts) —
        // an attacker who could roll back the safes table could
        // resurrect a kicked owner key or hide a freshly removed
        // malicious module. Bundling safes into `save_to` means every
        // save bumps the file's epoch and re-mirrors to the keyring,
        // so any rollback to a prior file snapshot trips the policy
        // check. This test pins that contract: a save that only changes
        // the safes list still bumps the epoch.
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let initial = wallet_with_safes(Vec::new());
        save_to(&path, &initial, &pw("pw")).unwrap();
        let salt = read_salt_from_file(&path);
        let entry = keyring_entry_name(&salt);
        let before = epoch_from_keyring(&entry);

        let with_safe = wallet_with_safes(vec![minimal_safe(0x01, 1)]);
        save_to(&path, &with_safe, &pw("pw")).unwrap();
        let after = epoch_from_keyring(&entry);

        assert!(
            after > before,
            "safes save must bump epoch (before={before}, after={after})",
        );
    }

    #[test]
    fn safe_aad_namespaces_against_account_and_contact_aads() {
        // Three-way AAD isolation: a safe ciphertext must not decrypt
        // under the accounts or contacts AAD (and vice versa). Without
        // this, an attacker with file-write access could copy an
        // encrypted safe row into the accounts table and have it
        // silently load as an account, or copy an account row into the
        // safes table to forge owner sets.
        let key = [0x77u8; 32];
        let plaintext = b"hello-safe";
        let blob_s = encrypt_blob(&key, &safe_aad(0), plaintext).unwrap();
        let blob_a = encrypt_blob(&key, &account_aad(0), plaintext).unwrap();
        let blob_c = encrypt_blob(&key, &contact_aad(0), plaintext).unwrap();

        // safe blob must not open under account or contact AAD.
        assert!(decrypt_blob(&key, &account_aad(0), &blob_s).is_err());
        assert!(decrypt_blob(&key, &contact_aad(0), &blob_s).is_err());
        // account / contact blobs must not open under safe AAD.
        assert!(decrypt_blob(&key, &safe_aad(0), &blob_a).is_err());
        assert!(decrypt_blob(&key, &safe_aad(0), &blob_c).is_err());

        // Sanity: the safe AAD round-trip works on its own blob.
        assert_eq!(
            decrypt_blob(&key, &safe_aad(0), &blob_s)
                .unwrap()
                .as_slice(),
            plaintext,
        );
    }

    #[test]
    fn aad_binding_rejects_swapped_safe_record() {
        // Mirror of `aad_binding_rejects_swapped_account_record` for the
        // safes table. Per-row AAD includes the row index, so swapping
        // the bytes between row 0 and row 1 must produce a decrypt
        // failure on load — otherwise an attacker with file-write
        // access could reorder safes (e.g. promote a watch-only Safe
        // into a signer slot by swapping it with one that has linked
        // signers, if the UI ever derived role from position).
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let desc = wallet_with_safes(vec![minimal_safe(0x01, 1), minimal_safe(0x02, 10)]);
        save_to(&path, &desc, &pw("pw")).unwrap();

        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut safes = txn.open_table(SAFES_TABLE).unwrap();
                let a = safes.get(0u32).unwrap().unwrap().value().to_vec();
                let b = safes.get(1u32).unwrap().unwrap().value().to_vec();
                safes.insert(0u32, b.as_slice()).unwrap();
                safes.insert(1u32, a.as_slice()).unwrap();
            }
            txn.commit().unwrap();
        }

        let err = load_from(&path, &pw("pw")).unwrap_err();
        match err {
            WalletError::Encryption(msg) => {
                assert!(msg.contains("decrypt"), "got: {msg}");
            }
            other => panic!("expected Encryption error, got {other:?}"),
        }
    }

    #[test]
    fn load_accepts_legacy_safe_row_written_before_tx_service_url() {
        // End-to-end guarantee for wallets saved before `tx_service_url`
        // shipped: a safes row whose plaintext uses the OLD 13-field
        // postcard layout — encrypted under the real per-row AAD — must
        // load as `tx_service_url: None`, not fail the whole wallet
        // open. Exercises the full path (decrypt → `decode_safe`
        // fallback), unlike the codec-only unit test.
        let dir = tempdir().unwrap();
        let path = dir.path().join("wallet.redb");
        let expected = minimal_safe(0x07, 1);
        let desc = wallet_with_safes(vec![expected.clone()]);
        save_to(&path, &desc, &pw("pw")).unwrap();

        // Overwrite row 0 with a legacy-encoded, properly-encrypted blob.
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let header = {
                    let meta = txn.open_table(META_TABLE).unwrap();
                    let raw = meta.get(HEADER_KEY).unwrap().unwrap().value().to_vec();
                    deserialize_header(&raw).unwrap()
                };
                let key = derive_master_key(
                    &pw("pw"),
                    &header.salt,
                    header.argon2_m_cost_kib,
                    header.argon2_t_cost,
                    header.argon2_p_cost,
                )
                .unwrap();
                let legacy_plaintext = postcard::to_stdvec(&(
                    &expected.name,
                    expected.chain_id,
                    expected.address,
                    &expected.version,
                    &expected.trust,
                    expected.threshold,
                    &expected.owners,
                    &expected.modules,
                    expected.guard,
                    expected.fallback_handler,
                    &expected.linked_signer_indices,
                    &expected.sibling_chains,
                    expected.cached_at,
                ))
                .unwrap();
                let blob = encrypt_blob(key.as_slice(), &safe_aad(0), &legacy_plaintext).unwrap();
                let mut safes = txn.open_table(SAFES_TABLE).unwrap();
                safes.insert(0u32, blob.as_slice()).unwrap();
            }
            txn.commit().unwrap();
        }

        let loaded = load_from(&path, &pw("pw")).unwrap();
        assert_eq!(loaded.safes, vec![expected]);
        assert!(loaded.safes[0].tx_service_url.is_none());
    }

    #[test]
    fn safe_descriptor_display_name_falls_back_to_indexed_default() {
        // Mirror of the account `display_name` behavior — unnamed safes
        // render as "Safe N" so the account list never has a blank row.
        let unnamed = SafeDescriptor {
            name: None,
            ..minimal_safe(0x01, 1)
        };
        assert_eq!(unnamed.display_name(0), "Safe 1");
        assert_eq!(unnamed.display_name(4), "Safe 5");

        let mut named = minimal_safe(0x01, 1);
        named.set_name(Some("  Treasury  ".into()));
        // set_name must trim whitespace; the rendered name is the trim.
        assert_eq!(named.display_name(0), "Treasury");

        // Empty-after-trim collapses to unnamed default.
        named.set_name(Some("   ".into()));
        assert_eq!(named.display_name(2), "Safe 3");
    }

    #[test]
    fn safe_descriptor_is_signer_reflects_linked_indices() {
        // The watch-only Safe path produces `linked_signer_indices: []`.
        // Anything non-empty marks the user as a signer. This is what
        // the UI checks to decide between the muted watch-only style
        // and the actuatable signer style.
        let mut s = minimal_safe(0x01, 1);
        assert!(s.is_signer(), "minimal_safe links signer 0");
        s.linked_signer_indices.clear();
        assert!(!s.is_signer());
        s.linked_signer_indices.push(7);
        assert!(s.is_signer());
    }

    #[test]
    fn contains_safe_distinguishes_address_and_chain() {
        // Dedup at onboarding is (address, chain_id) — the same address
        // on a different chain is a different Safe (separate owner set,
        // separate everything). This guard belongs in WalletDescriptor
        // rather than UI so every add path uses the same definition of
        // "already added".
        use alloy::primitives::Address;
        let desc = wallet_with_safes(vec![minimal_safe(0x42, 1)]);
        let same = Address::from([0x42; 20]);
        let other = Address::from([0x43; 20]);
        assert!(desc.contains_safe(same, 1));
        // Same address, different chain → not a duplicate.
        assert!(!desc.contains_safe(same, 10));
        // Different address, same chain → not a duplicate.
        assert!(!desc.contains_safe(other, 1));
    }
}
