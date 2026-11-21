//! Keys and trust for end-to-end encrypted DMs (milestone M3), brought up at sign-in the way the
//! web client does it (`store/lifecycle.js` → `vault.load` → `Trust.start`):
//!
//! 1. open the recovery keyring R from the home server's vault, with the OPAQUE-derived wrapper
//!    when this session came from a password sign-in, else with the copy this device kept;
//! 2. keep one MLS signature key per account on this device (the web client does the same, so a
//!    re-login is the same correspondent rather than a stranger);
//! 3. open the account's cross-signing secret with R (or make one, for an account that has none),
//!    check it really belongs to the published identity key, vouch for this device's MLS key, and
//!    make sure the identity is anchored in its continuity document, pinning what we saw.
//!
//! Secrets live in a 0600 file per account in the config directory; nothing here is ever written
//! to the server except what the web client writes too (a fresh vault for an account that has
//! none, with the recovery key's copy when sign-up made one; the account's first cross-signing
//! key; the device cross-signature and the identity anchor).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use rhein_mls::recovery::{self, RecoveryKey};
use rhein_mls::trust::{self, IdentityKey, Pin};
use rhein_mls::vault::{self, Key, Keyring, Opened};
use rhein_net::Api;
use serde::{Deserialize, Serialize};

/// What this device keeps for one account.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Secrets {
    pub keyring: Option<Keyring>,
    /// The account's cross-signing secret, once opened (base64).
    pub cross_signing: Option<String>,
    /// This account's MLS signature key on this device (base64 private, public).
    pub mls_signature: Option<(String, String)>,
    /// Identity pins by user id.
    pub pins: BTreeMap<String, Pin>,
    /// A cross-signing key this device sent to the server but never heard back about (public,
    /// secret, base64): if the server kept it, the next bring-up takes it instead of a new one.
    pub pending_cross_signing: Option<(String, String)>,
    /// A recovery key made here that the person has not yet said they saved. Kept (in this 0600
    /// file, as the web client keeps it in its own storage) until they do, so it can be shown
    /// again rather than lost.
    pub unsaved_recovery_key: Option<String>,
}

fn secrets_path(user_id: &str) -> PathBuf {
    let safe: String = user_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect();
    crate::persist::path().with_file_name("dm").join(format!("{safe}.json"))
}

impl Secrets {
    pub fn load(user_id: &str) -> Secrets {
        std::fs::read(secrets_path(user_id)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn store(&self, user_id: &str) -> Result<()> {
        let path = secrets_path(user_id);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).context("dm secrets dir")?;
        }
        crate::persist::write_private(&path, &serde_json::to_vec_pretty(self)?)
    }

    /// Forgets everything kept for an account (it signed out on this device).
    pub fn remove(user_id: &str) {
        let _ = std::fs::remove_file(secrets_path(user_id));
    }
}

/// How far DM encryption came up, for the settings page and the DM view.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DmStatus {
    /// `opaque_custody`, `device_custody`, …, or `none`.
    pub vault_mode: String,
    /// Why the recovery keyring isn't available, if it isn't.
    pub locked: Option<String>,
    /// The account's cross-signing identity key, if it has one.
    pub identity_key: Option<String>,
    pub immutable_id: Option<String>,
    /// This device's MLS key is vouched for by the identity key.
    pub cross_signed: bool,
    /// Something is wrong with the published identity (a replaced key, a broken chain).
    pub identity_problem: Option<String>,
    /// The account has a recovery key (`recovery_public_key` on the server).
    pub recovery_key_enrolled: bool,
    /// The vault carries the recovery key's copy of the keyring.
    pub recovery_copy: bool,
    /// A recovery key made on this device that still has to be written down.
    pub unsaved_recovery_key: Option<String>,
    /// This device's MLS signature key (base64), as the ratchet tree and cross-signature carry it.
    pub mls_key: String,
}

impl DmStatus {
    /// A short fingerprint of this device's MLS key: 8 groups of 4 hex digits of its SHA-256.
    pub fn fingerprint(&self) -> String {
        use sha2::Digest as _;
        let Ok(raw) = vault::unb64(&self.mls_key) else { return String::new() };
        let digest = sha2::Sha256::digest(raw);
        digest[..16].chunks(2).map(|c| format!("{:02x}{:02x}", c[0], c[1])).collect::<Vec<_>>().join(" ")
    }
}

/// Brings DM keys and trust up for an account. `wrapper` is the OPAQUE-derived vault key from a
/// password sign-in in this session, if there was one.
pub async fn bring_up(api: &Api, user_id: &str, device_id: &str, wrapper: Option<Key>) -> Result<DmStatus> {
    let mut secrets = Secrets::load(user_id);
    let mut status = DmStatus::default();
    let recovery_key = secrets.unsaved_recovery_key.as_deref().and_then(|t| RecoveryKey::parse(t).ok());

    // 1. The recovery keyring.
    let doc = api.vault().await.context("vault")?;
    status.vault_mode = doc["mode"].as_str().unwrap_or("none").to_owned();
    match vault::open_vault(&doc, wrapper.as_ref(), secrets.keyring.as_ref())? {
        Opened::Ring { keyring, mode } => {
            status.vault_mode = mode;
            secrets.keyring = Some(keyring);
        }
        Opened::Locked { reason, .. } => status.locked = Some(reason.to_owned()),
        Opened::Missing => match &wrapper {
            // An account without DMs set up yet (one registered with Rhein): create the vault the
            // way the web client does on its first sign-in. Not over an account key, though: a
            // new ring opens nothing that key's account already sealed (the web's `vault_missing`).
            Some(wrapper) => {
                let root = api.cross_signing().await.context("cross-signing")?;
                if root["exists"].as_bool().unwrap_or(false) && secrets.keyring.is_none() {
                    status.locked = Some("this account has an identity key but no recovery vault; open the Telium web client to repair it".into());
                } else {
                    let keyring = secrets.keyring.clone().unwrap_or_else(Keyring::generate);
                    // Kept before the write goes out, so a lost answer loses nothing.
                    secrets.keyring = Some(keyring.clone());
                    secrets.store(user_id)?;
                    api.put_vault(&vault::new_vault_body(&keyring, wrapper, recovery_key.as_ref()))
                        .await
                        .context("creating the vault")?;
                    status.vault_mode = "opaque_custody".into();
                }
            }
            None => status.locked = Some("sign in with your password once on this device".into()),
        },
    }
    let vault_now = if doc["exists"].as_bool().unwrap_or(false) { doc } else { api.vault().await.unwrap_or(doc) };
    status.recovery_key_enrolled = vault_now["recovery_key_enrolled"].as_bool().unwrap_or(false);
    status.recovery_copy = recovery::is_vault_copy(vault_now["independent_ciphertext"].as_str());
    status.unsaved_recovery_key = secrets.unsaved_recovery_key.clone();

    // 2. This account's MLS signature key on this device.
    let (_, public) = match &secrets.mls_signature {
        Some(pair) => pair.clone(),
        None => {
            let pair = rhein_mls::new_signature_key()?;
            let pair = (vault::b64(&pair.0), vault::b64(&pair.1));
            secrets.mls_signature = Some(pair.clone());
            pair
        }
    };
    status.mls_key = public.clone();

    // 3. Cross-signing and continuity.
    let mut cs = api.cross_signing().await.context("cross-signing")?;
    // An account with no identity key (one registered with Rhein, before this): make it, sealed
    // under R alone, as the web client's `ensureRoot` does. Since Telium desktop 0.4.0 web peers
    // refuse an account without one wherever their home advertises identity continuity, so
    // without this a Rhein-made account is left out of every DM a browser starts.
    if !cs["exists"].as_bool().unwrap_or(false)
        && let Some(ring) = secrets.keyring.clone()
        && status.locked.is_none()
    {
        cs = create_identity(api, user_id, &mut secrets, &ring).await?;
    }
    if cs["exists"].as_bool().unwrap_or(false) {
        let published = cs["identity_key"].as_str().unwrap_or_default().to_owned();
        status.identity_key = Some(published.clone());
        let from_b64 = |s: &str| vault::unb64(s).ok().and_then(|b| IdentityKey::from_secret(&b).ok());
        let held = secrets.cross_signing.as_deref().and_then(from_b64).filter(|k| k.binds(user_id, &published));
        // A create this device made but never heard back about: the server may have kept it.
        let pending = secrets
            .pending_cross_signing
            .as_ref()
            .filter(|(public, _)| *public == published)
            .and_then(|(_, secret)| from_b64(secret));
        let opened = held.or(pending).or_else(|| {
            let ring = secrets.keyring.as_ref()?;
            trust::open_cross_signing_secret(cs["sealed_secret"].as_str().unwrap_or_default(), ring)
        });
        secrets.pending_cross_signing = None;
        match opened {
            Some(key) if key.binds(user_id, &published) => {
                secrets.cross_signing = Some(vault::b64(key.secret()));
                // Continuity: anchor the identity if it never was, verify it, pin it.
                let mut identity = api.own_identity().await.ok();
                if identity.as_ref().is_some_and(|d| !d["exists"].as_bool().unwrap_or(false)) {
                    let tid = trust::immutable_id(&published)?;
                    let signature = vault::b64url(&key.sign(&trust::anchor_payload(user_id, &tid, &published)));
                    identity = api.anchor_identity(&tid, &published, &signature).await.ok();
                }
                match identity.as_ref().map(|d| trust::verify_identity_document(d, Some(user_id))) {
                    Some(Ok(Some(c))) => match trust::check_pin(secrets.pins.get(user_id), &c) {
                        Ok(pin) if c.current_key == published => {
                            status.immutable_id = Some(c.immutable_id.clone());
                            secrets.pins.insert(user_id.to_owned(), pin);
                        }
                        Ok(_) => status.identity_problem = Some("the published identity key is not the head of its continuity chain".into()),
                        Err(reason) => status.identity_problem = Some(reason.into()),
                    },
                    Some(Err(reason)) => status.identity_problem = Some(reason.into()),
                    // An older home server without continuity documents.
                    Some(Ok(None)) | None => {}
                }
                if status.identity_problem.is_none() {
                    let already = cs["devices"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|d| d["device_id"] == device_id && d["mls_key"] == public.as_str() && d["cross_sig"].is_string());
                    if !already {
                        let signature = key.vouch_for_device(user_id, device_id, &public);
                        api.sign_device(device_id, &public, &signature).await.context("vouching for this device")?;
                    }
                    status.cross_signed = true;
                }
            }
            Some(_) => status.identity_problem = Some("the cross-signing secret does not match the published identity key".into()),
            None => {}
        }
    }

    secrets.store(user_id)?;
    Ok(status)
}

/// Publishes a new account key with `create_only`, keeping it aside first so a lost answer loses
/// nothing; another device that made one first wins, and its row comes back instead.
async fn create_identity(api: &Api, user_id: &str, secrets: &mut Secrets, ring: &Keyring) -> Result<serde_json::Value> {
    let key = IdentityKey::generate();
    let public = key.public_b64();
    secrets.pending_cross_signing = Some((public.clone(), vault::b64(key.secret())));
    secrets.store(user_id)?;
    match api.create_cross_signing(&public, &trust::seal_cross_signing_secret(&key, ring)).await {
        Ok(_) => {
            log::info!("made this account's identity key");
            secrets.cross_signing = Some(vault::b64(key.secret()));
            secrets.pending_cross_signing = None;
        }
        // Another device made the account's key first: take that one as it stands.
        Err(err) if err.status() == Some(409) => secrets.pending_cross_signing = None,
        // Anything else may have reached the server; the pending copy covers that.
        Err(err) => return Err(anyhow::Error::new(err).context("creating the account's identity key")),
    }
    secrets.store(user_id)?;
    Ok(api.cross_signing().await.context("cross-signing")?)
}

/// Makes a recovery key for an account that has none, proving the password: its public half and
/// the vault copy are written together (`reauth/finish`), as the web client's Settings does. The
/// key is kept here as unsaved until the person says they wrote it down.
pub async fn enroll_recovery_key(api: &Api, user_id: &str, password: &str, ksf: &rhein_auth::SerenityKsf) -> Result<String> {
    let mut secrets = Secrets::load(user_id);
    let ring = secrets.keyring.clone().context("unlock encrypted messages on this device first")?;
    let key = RecoveryKey::generate();
    rhein_net::auth::enroll_recovery_key(api, password, ksf, &key.public_key(), &key.seal_vault_copy(&ring))
        .await
        .map_err(|err| match err.downcast_ref::<rhein_net::ApiError>().and_then(|e| e.status()) {
            Some(409) => anyhow::anyhow!("this account already has a recovery key"),
            _ => err,
        })?;
    let text = key.text();
    secrets.unsaved_recovery_key = Some(text.clone());
    secrets.store(user_id)?;
    Ok(text)
}

/// The person wrote the recovery key down: stop keeping it.
pub fn recovery_key_saved(user_id: &str) -> Result<()> {
    let mut secrets = Secrets::load(user_id);
    secrets.unsaved_recovery_key = None;
    secrets.store(user_id)
}

/// Keeps a recovery key made for a sign-up, before the account exists, so the vault bring-up
/// after it can seal the copy and the person can be shown it.
pub fn keep_new_recovery_key(user_id: &str, key: &RecoveryKey) -> Result<()> {
    let mut secrets = Secrets::load(user_id);
    secrets.unsaved_recovery_key = Some(key.text());
    secrets.store(user_id)
}
