//! The recovery keyring R that makes encrypted history portable between an account's devices,
//! as the web client keeps it (`web/src/lib/vault.js`).
//!
//! The home server stores R sealed. In `opaque_custody`, the normal mode, the seal's key is
//! `HMAC-SHA256(exportKey, "telium/dm-recovery-from-opaque-export/v1")`, where `exportKey` is
//! OPAQUE's client-only export key from signing in: the server never sees it, so it can't open R.
//! R seals GroupInfo frames, the history mirror and the cross-signing secret.

use aes_gcm::aead::{Aead as _, KeyInit as _};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;

pub const KEY_BYTES: usize = 32;
const RECOVERY_LABEL: &[u8] = b"telium/dm-recovery-from-opaque-export/v1";
/// A keyring keeps at most this many retired keys (what older seals may still be under).
const MAX_RETIRED: usize = 8;

pub type Key = [u8; KEY_BYTES];

/// The key that seals R in `opaque_custody`, from the OPAQUE export key.
pub fn recovery_wrapper(export_key: &[u8]) -> Key {
    let mut mac = <Hmac<Sha256> as hmac::Mac>::new_from_slice(export_key).expect("HMAC takes any key length");
    mac.update(RECOVERY_LABEL);
    mac.finalize().into_bytes().into()
}

/// Base64 as the web client writes it (standard alphabet, padded); read in either alphabet.
pub fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

pub fn unb64(text: &str) -> Result<Vec<u8>> {
    let normal: String = text.trim_end_matches('=').replace('-', "+").replace('_', "/");
    base64::engine::general_purpose::STANDARD_NO_PAD.decode(normal).context("base64")
}

pub fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// `b64(iv) + "." + b64(AES-256-GCM(ciphertext || tag))`, the web client's `seal`.
pub fn seal(key: &Key, plaintext: &[u8]) -> String {
    let iv: [u8; 12] = rand_bytes();
    let cipher = Aes256Gcm::new(key.into());
    let sealed = cipher.encrypt(Nonce::from_slice(&iv), plaintext).expect("AES-GCM encrypts any length");
    format!("{}.{}", b64(&iv), b64(&sealed))
}

pub fn open(key: &Key, blob: &str) -> Result<Vec<u8>> {
    let (iv, ct) = blob.split_once('.').ok_or_else(|| anyhow!("malformed sealed blob"))?;
    let iv = unb64(iv)?;
    anyhow::ensure!(iv.len() == 12, "malformed sealed blob");
    let cipher = Aes256Gcm::new(key.into());
    cipher.decrypt(Nonce::from_slice(&iv), unb64(ct)?.as_slice()).map_err(|_| anyhow!("this key does not open it"))
}

/// An attachment sealed for a DM: AES-256-GCM under a fresh key, as the web client's
/// `uploadEncrypted` does. Returns the ciphertext and the key and IV (base64url) for the body.
pub fn seal_attachment(plain: &[u8]) -> (Vec<u8>, String, String) {
    let key: Key = rand_bytes();
    let iv: [u8; 12] = rand_bytes();
    let sealed = Aes256Gcm::new((&key).into()).encrypt(Nonce::from_slice(&iv), plain).expect("AES-GCM encrypts any length");
    (sealed, b64url(&key), b64url(&iv))
}

pub fn open_attachment(sealed: &[u8], key_b64: &str, iv_b64: &str) -> Result<Vec<u8>> {
    let key: Key = unb64(key_b64)?.try_into().map_err(|_| anyhow!("bad attachment key"))?;
    let iv = unb64(iv_b64)?;
    anyhow::ensure!(iv.len() == 12, "bad attachment iv");
    Aes256Gcm::new((&key).into()).decrypt(Nonce::from_slice(&iv), sealed).map_err(|_| anyhow!("this attachment does not open with its key"))
}

pub fn rand_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("the OS random source");
    out
}

/// R and the keys it replaced; `openers` tries them newest first.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keyring {
    pub key: Key,
    pub retired: Vec<Key>,
}

impl std::fmt::Debug for Keyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Keyring({} retired)", self.retired.len())
    }
}

impl Keyring {
    pub fn generate() -> Keyring {
        Keyring { key: rand_bytes(), retired: Vec::new() }
    }

    pub fn openers(&self) -> impl Iterator<Item = &Key> {
        std::iter::once(&self.key).chain(self.retired.iter())
    }

    /// Opens something sealed under R or any retired key.
    pub fn open(&self, blob: &str) -> Result<Vec<u8>> {
        self.openers().find_map(|k| open(k, blob).ok()).ok_or_else(|| anyhow!("no key of the recovery keyring opens it"))
    }

    /// `{v: 2, key, retired}` as JSON bytes, or a bare 32-byte key from older clients.
    pub fn decode(bytes: &[u8]) -> Result<Keyring> {
        if bytes.len() == KEY_BYTES {
            return Ok(Keyring { key: bytes.try_into()?, retired: Vec::new() });
        }
        let doc: Value = serde_json::from_slice(bytes).context("the vault holds something this build does not understand")?;
        let key: Key = unb64(doc["key"].as_str().unwrap_or_default())?.try_into().map_err(|_| anyhow!("bad recovery key"))?;
        let retired = doc["retired"]
            .as_array()
            .map(|list| list.iter().filter_map(|r| unb64(r.as_str()?).ok()?.try_into().ok()).take(MAX_RETIRED).collect())
            .unwrap_or_default();
        Ok(Keyring { key, retired })
    }

    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "v": 2,
            "key": b64(&self.key),
            "retired": self.retired.iter().map(|k| b64(k)).collect::<Vec<_>>(),
        }))
        .expect("serialisable")
    }
}

/// A vault kept in a custody mode Telium retired: only a device already holding the keyring,
/// or the web client converting it, opens it.
const RETIRED: &str = "this vault uses a custody mode Telium retired; open the Telium web client once to convert it";

/// What opening the vault came to.
#[derive(Debug)]
pub enum Opened {
    /// R is available.
    Ring { keyring: Keyring, mode: String },
    /// The vault exists but this session can't open it (another custody mode, or the wrapper
    /// doesn't fit); a trusted sibling device or the web client has to help.
    Locked { mode: String, reason: &'static str },
    /// No vault yet: the account has never set up encrypted DMs.
    Missing,
}

/// Opens the vault document the home server returned (`GET /vault`) with what this session
/// holds: the OPAQUE-derived wrapper (only right after a password sign-in) and R from this
/// device's own store. Nothing here writes to the server; migrations stay the web client's job.
pub fn open_vault(vault: &Value, wrapper: Option<&Key>, local: Option<&Keyring>) -> Result<Opened> {
    if !vault["exists"].as_bool().unwrap_or(false) {
        return Ok(Opened::Missing);
    }
    let mode = vault["mode"].as_str().unwrap_or_default().to_owned();
    let ciphertext = vault["ciphertext"].as_str().unwrap_or_default();
    let ring = |keyring: Keyring| Ok(Opened::Ring { keyring, mode: mode.clone() });
    match mode.as_str() {
        "opaque_custody" => {
            if let Some(wrapper) = wrapper
                && let Ok(bytes) = open(wrapper, ciphertext)
            {
                return ring(Keyring::decode(&bytes)?);
            }
            match local {
                Some(local) => ring(local.clone()),
                None if wrapper.is_some() => Ok(Opened::Locked { mode, reason: "the account password does not open this vault" }),
                None => Ok(Opened::Locked { mode, reason: "sign in with your password once on this device" }),
            }
        }
        // Retired by Telium (server 0.5.0): the server no longer hands out the escrow key, so
        // such a vault is locked until the web client converts it, not an error that stops the
        // rest of DM bring-up.
        "escrowed" => {
            let escrow: Option<Key> = vault["escrow_key"].as_str().and_then(|k| unb64(k).ok()).and_then(|k| k.try_into().ok());
            if let Some(escrow) = escrow
                && let Ok(bytes) = open(&escrow, ciphertext)
            {
                return ring(Keyring::decode(&bytes)?);
            }
            match local {
                Some(local) => ring(local.clone()),
                None => Ok(Opened::Locked { mode, reason: RETIRED }),
            }
        }
        "device_custody" | "self_custody" => match local {
            Some(local) => ring(local.clone()),
            None => Ok(Opened::Locked { mode, reason: RETIRED }),
        },
        _ => Ok(Opened::Locked { mode, reason: "this vault uses a custody mode this build does not know" }),
    }
}

/// The body for `PUT /vault` that creates a fresh `opaque_custody` vault, for an account that has
/// none yet (one registered with Rhein). `recovery` adds the recovery key's copy of the keyring,
/// as the web client's sign-up does, so a password reset can bring the history back.
pub fn new_vault_body(keyring: &Keyring, wrapper: &Key, recovery: Option<&crate::recovery::RecoveryKey>) -> Value {
    serde_json::json!({
        "mode": "opaque_custody",
        "ciphertext": seal(wrapper, &keyring.encode()),
        "independent_ciphertext": recovery.map(|r| r.seal_vault_copy(keyring)),
        // Compare-and-swap: written only if the account has no vault yet, so a vault another
        // device made meanwhile is never overwritten.
        "expected_version": 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn seal_open_round_trip_and_web_format() {
        let key = rand_bytes::<32>();
        let blob = seal(&key, b"hello");
        let (iv, ct) = blob.split_once('.').unwrap();
        assert_eq!(unb64(iv).unwrap().len(), 12);
        assert_eq!(unb64(ct).unwrap().len(), 5 + 16);
        assert_eq!(open(&key, &blob).unwrap(), b"hello");
        assert!(open(&rand_bytes::<32>(), &blob).is_err());
    }

    #[test]
    fn wrapper_is_hmac_of_the_label() {
        // HMAC-SHA256(key = 32 zero bytes, "telium/dm-recovery-from-opaque-export/v1"), computed
        // independently with Python's hmac module.
        let w = recovery_wrapper(&[0u8; 32]);
        assert_eq!(hex(&w), "4e1720e53a555804e576f6aba4bce0e99f39835ff827d421bccb141ae7a7e292");
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn attachments_round_trip() {
        let (sealed, key, iv) = seal_attachment(b"picture bytes");
        assert_eq!(sealed.len(), 13 + 16);
        assert_eq!(open_attachment(&sealed, &key, &iv).unwrap(), b"picture bytes");
        assert!(open_attachment(&sealed, &b64url(&[0; 32]), &iv).is_err());
    }

    #[test]
    fn keyring_payloads() {
        let ring = Keyring { key: [1; 32], retired: vec![[2; 32]] };
        assert_eq!(Keyring::decode(&ring.encode()).unwrap(), ring);
        assert_eq!(Keyring::decode(&[7u8; 32]).unwrap().key, [7; 32]);
        let blob = seal(&[2; 32], b"old");
        assert_eq!(ring.open(&blob).unwrap(), b"old");
    }

    #[test]
    fn opening_the_vault() {
        let wrapper = recovery_wrapper(b"export key");
        let ring = Keyring::generate();
        let vault = json!({ "exists": true, "mode": "opaque_custody", "ciphertext": seal(&wrapper, &ring.encode()) });
        assert!(matches!(open_vault(&vault, Some(&wrapper), None).unwrap(), Opened::Ring { keyring, .. } if keyring == ring));
        assert!(matches!(open_vault(&vault, None, None).unwrap(), Opened::Locked { .. }));
        assert!(matches!(open_vault(&vault, None, Some(&ring)).unwrap(), Opened::Ring { .. }));
        assert!(matches!(open_vault(&json!({ "exists": false }), Some(&wrapper), None).unwrap(), Opened::Missing));
        let escrow = rand_bytes::<32>();
        let escrowed = json!({ "exists": true, "mode": "escrowed", "ciphertext": seal(&escrow, &ring.encode()), "escrow_key": b64(&escrow) });
        assert!(matches!(open_vault(&escrowed, None, None).unwrap(), Opened::Ring { .. }));
        // Telium 0.5 no longer serves the escrow key: locked, not an error.
        let retired = json!({ "exists": true, "mode": "escrowed", "ciphertext": seal(&escrow, &ring.encode()) });
        assert!(matches!(open_vault(&retired, None, None).unwrap(), Opened::Locked { .. }));
        assert!(matches!(open_vault(&retired, None, Some(&ring)).unwrap(), Opened::Ring { .. }));
        let unknown = json!({ "exists": true, "mode": "something_new", "ciphertext": "" });
        assert!(matches!(open_vault(&unknown, Some(&wrapper), None).unwrap(), Opened::Locked { .. }));
    }
}
