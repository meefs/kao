//! The dedicated Privacy Pools account — derived from the 24-word pool mnemonic
//! (a separate secret from the wallet's own seed; see `wallet::store` `pp`
//! table). One mnemonic derives every pool account, byte-compatible with the
//! 0xbow TS SDK, so deposits made in the official app are recoverable here.

use secrecy::{ExposeSecret, SecretString};

use privacy_pools::Account;

use super::PoolError;

/// Derive the [`Account`] from the pool mnemonic phrase.
///
/// `Account` holds only two field-element master keys (it is `Copy` and carries
/// no allocation), so it is safe to hold in memory for the unlocked session; the
/// phrase itself stays in the keyring and is only exposed transiently here.
pub fn account_from_phrase(phrase: &SecretString) -> Result<Account, PoolError> {
    Account::from_mnemonic(phrase.expose_secret())
        .map_err(|e| PoolError::Input(format!("invalid pool mnemonic: {e}")))
}
