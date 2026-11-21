//! The account recovery key, as the web client makes it (`web/src/lib/account-recovery.js`).
//!
//! A random 32-byte seed, shown to the person once as `telium-recovery-v1-<base64url>`. Two
//! HMAC-SHA256 subkeys come out of it: an Ed25519 signing key, whose public half the home server
//! keeps (`recovery_public_key`) to check password resets, and a vault wrapper that seals the
//! second copy of the DM recovery keyring (`independent_ciphertext`, `k1.` + a sealed blob). With
//! that copy, a forgotten password no longer means losing every encrypted conversation.

use anyhow::{Result, anyhow, bail};
use ed25519_dalek::{Signer as _, SigningKey};
use hmac::{Hmac, Mac as _};
use sha2::Sha256;

use crate::vault::{Key, Keyring, b64url, open, rand_bytes, seal, unb64};

const PREFIX: &str = "telium-recovery-v1-";
const SIGN_LABEL: &[u8] = b"telium/account-recovery-sign/v1";
const VAULT_LABEL: &[u8] = b"telium/account-recovery-vault/v1";
/// The prefix of a vault copy sealed under the recovery key; the server refuses any other.
pub const COPY_FORMAT: &str = "k1.";

pub struct RecoveryKey {
    seed: [u8; 32],
}

impl std::fmt::Debug for RecoveryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RecoveryKey(…)")
    }
}

fn subkey(seed: &[u8; 32], label: &[u8]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256>>::new_from_slice(seed).expect("HMAC takes any key length");
    mac.update(label);
    mac.finalize().into_bytes().into()
}

impl RecoveryKey {
    pub fn generate() -> RecoveryKey {
        RecoveryKey { seed: rand_bytes() }
    }

    /// Reads a pasted key, also out of a whole saved note or with line breaks flattened.
    pub fn parse(text: &str) -> Result<RecoveryKey> {
        let compact: String = text.split_whitespace().collect();
        let Some(at) = compact.find(PREFIX) else { bail!("That is not a Telium recovery key.") };
        let body: String = compact[at + PREFIX.len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        let seed: [u8; 32] = unb64(&body)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| anyhow!("That recovery key is malformed."))?;
        Ok(RecoveryKey { seed })
    }

    /// What the person writes down.
    pub fn text(&self) -> String {
        format!("{PREFIX}{}", b64url(&self.seed))
    }

    fn signing(&self) -> SigningKey {
        SigningKey::from_bytes(&subkey(&self.seed, SIGN_LABEL))
    }

    /// `recovery_public_key`: base64url, unpadded.
    pub fn public_key(&self) -> String {
        b64url(self.signing().verifying_key().as_bytes())
    }

    pub fn sign(&self, payload: &[u8]) -> String {
        b64url(&self.signing().sign(payload).to_bytes())
    }

    pub fn vault_wrapper(&self) -> Key {
        subkey(&self.seed, VAULT_LABEL)
    }

    /// The second copy of the keyring, for `independent_ciphertext`.
    pub fn seal_vault_copy(&self, keyring: &Keyring) -> String {
        format!("{COPY_FORMAT}{}", seal(&self.vault_wrapper(), &keyring.encode()))
    }

    pub fn open_vault_copy(&self, blob: &str) -> Result<Keyring> {
        let sealed = blob.strip_prefix(COPY_FORMAT).ok_or_else(|| anyhow!("this vault copy is not sealed by a recovery key"))?;
        Keyring::decode(&open(&self.vault_wrapper(), sealed)?)
    }
}

pub fn is_vault_copy(blob: Option<&str>) -> bool {
    blob.is_some_and(|b| b.starts_with(COPY_FORMAT))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// From the web client's own `account-recovery.js`, seed 1..=32.
    const TEXT: &str = "telium-recovery-v1-AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA";
    const PUBLIC: &str = "xFNL4p-54R0iXjk1Dv-2dZwAaNzCzZE7hbK91iYe_qo";
    const WRAPPER: &str = "f791d08ed538f18f24e89f52c26fcae7855c9d530501313b404232d887ebfba8";
    /// `signPasswordResetStart(TEXT, '@a:x', 'req')`.
    const RESET_START_SIG: &str = "WGee4MX-xuyrEJ_5nQ4OP5ByARa2_lNwmIawUdGTJh_hefm_hGjUD9YG63Fq6AqH5_lw1yxLouXe6f-pVRhqCg";

    #[test]
    fn matches_the_web_client() {
        let key = RecoveryKey::parse(TEXT).unwrap();
        assert_eq!(key.seed, std::array::from_fn::<u8, 32, _>(|i| i as u8 + 1));
        assert_eq!(key.text(), TEXT);
        assert_eq!(key.public_key(), PUBLIC);
        let hex: String = key.vault_wrapper().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, WRAPPER);
        assert_eq!(key.sign(b"telium-password-reset-start:v1|@a:x|req"), RESET_START_SIG);
    }

    #[test]
    fn parses_out_of_a_saved_note() {
        let note = format!("Telium recovery key\n\n{}\n{}\n", &TEXT[..30], &TEXT[30..]);
        assert_eq!(RecoveryKey::parse(&note).unwrap().text(), TEXT);
        assert!(RecoveryKey::parse("telium-recovery-v1-short").is_err());
        assert!(RecoveryKey::parse("hello").is_err());
    }

    #[test]
    fn vault_copy_round_trip() {
        let key = RecoveryKey::generate();
        let ring = Keyring::generate();
        let copy = key.seal_vault_copy(&ring);
        assert!(is_vault_copy(Some(&copy)));
        assert_eq!(key.open_vault_copy(&copy).unwrap(), ring);
        assert!(RecoveryKey::generate().open_vault_copy(&copy).is_err());
    }
}
