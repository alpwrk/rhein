//! Who is who, as the web client checks it (`web/src/lib/verify.js`, `identity-continuity.js`):
//!
//! - An account may have a cross-signing identity key (Ed25519). Its secret half is stored on
//!   the home server sealed under the recovery keyring (`recovery-v1:` prefix).
//! - Each device's MLS signature key is vouched for by a signature of that identity key over
//!   `telium-device-sig:v1|{user}|{device}|{mls_key}`.
//! - The identity key itself is anchored in a self-certifying continuity document: a genesis key
//!   (whose hash is the immutable `tid:`), then rotations each signed by the key before or by
//!   guardians. Clients pin what they have seen and refuse rollbacks and forks.
//!
//! Web peers accept a device of an account with an identity key only when all of this checks
//! out. Since Telium desktop 0.4.0 they also refuse an account with no identity key at all once
//! their own home advertises `identity.continuity`, so every account needs one: Rhein makes it
//! at sign-in when the account has none (`IdentityKey::generate`, published `create_only`).

use std::collections::BTreeMap;

use anyhow::Result;
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::vault::{Keyring, b64, seal, unb64};

pub const RECOVERY_SEAL: &str = "recovery-v1:";
pub const DEVICE_SEAL: &str = "device-v1:";
const ANCHOR_DOMAIN: &str = "telium-identity-anchor:v1";
const POLICY_DOMAIN: &str = "telium-identity-recovery-policy:v1";
const ROTATE_DOMAIN: &str = "telium-identity-rotate:v1";
const ID_DOMAIN: &[u8] = b"telium-identity-id:v1\0";

pub fn device_payload(user_id: &str, device_id: &str, mls_key_b64: &str) -> Vec<u8> {
    format!("telium-device-sig:v1|{user_id}|{device_id}|{mls_key_b64}").into_bytes()
}

/// What the account root signs to revoke one device's MLS key.
pub fn revoke_payload(user_id: &str, immutable_id: &str, device_id: &str, mls_key_b64: &str) -> Vec<u8> {
    format!("telium-device-revoke:v1|{user_id}|{immutable_id}|{device_id}|{mls_key_b64}").into_bytes()
}

pub fn key_binding_probe(user_id: &str) -> Vec<u8> {
    format!("telium-crosssign-key-binding:v1|{user_id}").into_bytes()
}

pub fn anchor_payload(user_id: &str, immutable_id: &str, identity_key_b64: &str) -> Vec<u8> {
    format!("{ANCHOR_DOMAIN}|{user_id}|{immutable_id}|{identity_key_b64}").into_bytes()
}

pub fn rotation_payload(immutable_id: &str, sequence: u64, previous: &str, next: &str) -> Vec<u8> {
    format!("{ROTATE_DOMAIN}|{immutable_id}|{sequence}|{previous}|{next}").into_bytes()
}

pub fn policy_payload(immutable_id: &str, version: u64, identity_sequence: u64, threshold: u64, guardians: &[Guardian]) -> Vec<u8> {
    let sorted = sorted_guardians(guardians);
    let rows: Vec<[&str; 3]> = sorted.iter().map(|g| [g.user_id.as_str(), g.immutable_id.as_str(), g.identity_key.as_str()]).collect();
    let body = serde_json::to_string(&(threshold, rows)).expect("serialisable");
    format!("{POLICY_DOMAIN}|{immutable_id}|{version}|{identity_sequence}|{body}").into_bytes()
}

/// `tid:` + hex SHA-256 of `telium-identity-id:v1\0 || raw key`.
pub fn immutable_id(identity_key_b64: &str) -> Result<String> {
    let raw = unb64(identity_key_b64)?;
    anyhow::ensure!(raw.len() == 32, "identity key must be 32 bytes");
    let digest = Sha256::new().chain_update(ID_DOMAIN).chain_update(&raw).finalize();
    Ok(format!("tid:{}", digest.iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

/// Strict Ed25519 verification of `signature` (base64 either alphabet) by `key`.
pub fn verifies(key_b64: &str, payload: &[u8], signature_b64: &str) -> bool {
    let Ok(key) = unb64(key_b64).map_err(|_| ()).and_then(|k| <[u8; 32]>::try_from(k).map_err(|_| ())) else { return false };
    let Ok(sig) = unb64(signature_b64).map_err(|_| ()).and_then(|s| <[u8; 64]>::try_from(s).map_err(|_| ())) else { return false };
    let Ok(key) = VerifyingKey::from_bytes(&key) else { return false };
    key.verify_strict(payload, &Signature::from_bytes(&sig)).is_ok()
}

/// The account's cross-signing key while this device holds its secret half.
pub struct IdentityKey {
    secret: SigningKey,
}

/// The PKCS#8 wrapping of an Ed25519 seed, as WebCrypto exports it: ts-mls stores keys this way
/// wherever the browser can do Ed25519 itself, and as the bare 32-byte seed elsewhere.
const PKCS8_ED25519: [u8; 16] = [0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20];

/// The 32-byte seed out of either form.
pub fn ed25519_seed(bytes: &[u8]) -> Result<[u8; 32]> {
    let seed = match bytes.len() {
        32 => bytes,
        48 if bytes[..16] == PKCS8_ED25519 => &bytes[16..],
        n => anyhow::bail!("not an Ed25519 private key ({n} bytes)"),
    };
    Ok(seed.try_into().expect("checked above"))
}

pub fn pkcs8(seed: &[u8; 32]) -> Vec<u8> {
    [PKCS8_ED25519.as_slice(), seed].concat()
}

impl IdentityKey {
    /// A new account key, for an account that has none yet.
    pub fn generate() -> IdentityKey {
        IdentityKey { secret: SigningKey::from_bytes(&crate::vault::rand_bytes()) }
    }

    pub fn from_secret(bytes: &[u8]) -> Result<IdentityKey> {
        Ok(IdentityKey { secret: SigningKey::from_bytes(&ed25519_seed(bytes)?) })
    }

    pub fn public_b64(&self) -> String {
        b64(self.secret.verifying_key().as_bytes())
    }

    pub fn secret(&self) -> &[u8; 32] {
        self.secret.as_bytes()
    }

    pub fn sign(&self, payload: &[u8]) -> Vec<u8> {
        self.secret.sign(payload).to_bytes().to_vec()
    }

    /// Whether this secret really belongs to the published identity key: a home server can
    /// replace the public row, but not make a held secret match it.
    pub fn binds(&self, user_id: &str, published_b64: &str) -> bool {
        let probe = key_binding_probe(user_id);
        verifies(published_b64, &probe, &b64(&self.sign(&probe)))
    }

    /// The root's revocation of one of our devices' MLS keys (base64url, as every non-MLS
    /// signature on the wire).
    pub fn revoke_device(&self, user_id: &str, immutable_id: &str, device_id: &str, mls_key_b64: &str) -> String {
        crate::vault::b64url(&self.sign(&revoke_payload(user_id, immutable_id, device_id, mls_key_b64)))
    }

    /// Our signature for one of our devices' MLS keys.
    pub fn vouch_for_device(&self, user_id: &str, device_id: &str, mls_key_b64: &str) -> String {
        b64(&self.sign(&device_payload(user_id, device_id, mls_key_b64)))
    }
}

/// Opens the cross-signing secret from `GET /crosssigning`'s `sealed_secret`. Only the
/// `recovery-v1:` form (and the original unprefixed one) opens with the recovery keyring; a
/// `device-v1:` secret lives on the devices that made it.
pub fn open_cross_signing_secret(sealed: &str, keyring: &Keyring) -> Option<IdentityKey> {
    if sealed.starts_with(DEVICE_SEAL) {
        return None;
    }
    let blob = sealed.strip_prefix(RECOVERY_SEAL).unwrap_or(sealed);
    keyring.open(blob).ok().and_then(|secret| IdentityKey::from_secret(&secret).ok())
}

/// Sealed as the web client seals it, in the PKCS#8 form WebCrypto imports.
pub fn seal_cross_signing_secret(key: &IdentityKey, keyring: &Keyring) -> String {
    format!("{RECOVERY_SEAL}{}", seal(&keyring.key, &pkcs8(key.secret())))
}

/// Whether a published device entry (`{device_id, mls_key, cross_sig, active}`) is vouched for
/// by `identity_key`.
pub fn device_is_vouched(user_id: &str, identity_key_b64: &str, device: &Value) -> bool {
    let (Some(device_id), Some(mls_key), Some(sig)) = (device["device_id"].as_str(), device["mls_key"].as_str(), device["cross_sig"].as_str()) else {
        return false;
    };
    device["active"].as_bool() != Some(false) && verifies(identity_key_b64, &device_payload(user_id, device_id, mls_key), sig)
}

/// The MLS keys an account revoked: statements (`revocations` on `GET /crosssigning` and
/// `GET /users/{id}/devices`) signed by a root of the account's verified chain over
/// `telium-device-revoke:v1|{user}|{tid}|{device}|{mls_key}`. A server can withhold a statement
/// but not forge one, so only verified ones count. Without a continuity document the account's
/// published identity key is its only root.
pub fn revoked_keys(user_id: &str, continuity: Option<&Continuity>, identity_key: Option<&str>, statements: &Value) -> std::collections::HashSet<String> {
    let roots: Vec<String> = match continuity {
        Some(c) => c.keys.values().cloned().collect(),
        None => identity_key.map(str::to_owned).into_iter().collect(),
    };
    let tid = match continuity {
        Some(c) => Some(c.immutable_id.clone()),
        None => identity_key.and_then(|k| immutable_id(k).ok()),
    };
    let Some(tid) = tid else { return Default::default() };
    statements
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let (device, key, sig) = (s["device_id"].as_str()?, s["mls_key"].as_str()?, s["signature"].as_str()?);
            let payload = revoke_payload(user_id, &tid, device, key);
            roots.iter().any(|root| verifies(root, &payload, sig)).then(|| key.to_owned())
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Guardian {
    pub user_id: String,
    pub immutable_id: String,
    pub identity_key: String,
}

fn sorted_guardians(guardians: &[Guardian]) -> Vec<Guardian> {
    let mut out = guardians.to_vec();
    out.sort_by(|a, b| {
        a.immutable_id.cmp(&b.immutable_id).then_with(|| a.user_id.cmp(&b.user_id)).then_with(|| a.identity_key.cmp(&b.identity_key))
    });
    out
}

/// A verified continuity document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Continuity {
    pub immutable_id: String,
    pub genesis_key: String,
    pub current_key: String,
    pub sequence: u64,
    /// Identity key by sequence number.
    pub keys: BTreeMap<u64, String>,
    /// Verified recovery policies by version: (identity sequence, signature).
    pub policies: BTreeMap<u64, (u64, String)>,
    pub current_policy_version: u64,
}

/// `verifyIdentityDocument`: `Ok(None)` for "no document", `Err(reason)` for a document that
/// doesn't verify.
pub fn verify_identity_document(doc: &Value, expected_user: Option<&str>) -> Result<Option<Continuity>, &'static str> {
    if !doc["exists"].as_bool().unwrap_or(false) {
        return Ok(None);
    }
    let user = doc["user_id"].as_str().filter(|u| !u.is_empty()).ok_or("missing-user")?;
    if expected_user.is_some_and(|e| e != user) {
        return Err("wrong-user");
    }
    let tid = doc["immutable_id"].as_str().unwrap_or_default();
    let genesis = doc["genesis_key"].as_str().unwrap_or_default();
    if tid.is_empty() || genesis.is_empty() || immutable_id(genesis).ok().as_deref() != Some(tid) {
        return Err("invalid-genesis");
    }
    let anchored = doc["anchor_signature"].as_str().is_some_and(|sig| verifies(genesis, &anchor_payload(user, tid, genesis), sig));
    if !anchored {
        return Err("invalid-anchor");
    }

    let policy_rows = doc["recovery_policies"].as_array().cloned().unwrap_or_default();
    let mut policies: BTreeMap<u64, Value> = BTreeMap::new();
    for p in &policy_rows {
        policies.insert(p["version"].as_u64().unwrap_or(0), p.clone());
    }
    if policies.len() != policy_rows.len() {
        return Err("duplicate-recovery-policy-version");
    }

    let mut keys = BTreeMap::from([(0u64, genesis.to_owned())]);
    let mut current = genesis.to_owned();
    let mut sequence = 0u64;
    let mut valid_policies: BTreeMap<u64, (u64, String, Vec<Guardian>, u64)> = BTreeMap::new();

    let verify_policy = |version: u64, keys: &BTreeMap<u64, String>, cache: &mut BTreeMap<u64, (u64, String, Vec<Guardian>, u64)>| -> bool {
        if cache.contains_key(&version) {
            return true;
        }
        let Some(policy) = policies.get(&version) else { return false };
        let Some(policy_sequence) = policy["identity_sequence"].as_u64() else { return false };
        if version < 1 {
            return false;
        }
        let Some(signer) = keys.get(&policy_sequence) else { return false };
        let Ok(guardians) = serde_json::from_value::<Vec<Guardian>>(policy["guardians"].clone()) else { return false };
        let guardians = sorted_guardians(&guardians);
        let Some(threshold) = policy["threshold"].as_u64() else { return false };
        if guardians.len() < 2 || guardians.len() > 5 || threshold < 2 || threshold as usize > guardians.len() {
            return false;
        }
        let mut seen_users = std::collections::HashSet::new();
        let mut seen_ids = std::collections::HashSet::new();
        let mut seen_keys = std::collections::HashSet::new();
        for g in &guardians {
            if g.user_id.is_empty() || g.immutable_id.is_empty() || g.identity_key.is_empty() || g.user_id == user || g.immutable_id == tid {
                return false;
            }
            let Ok(raw) = unb64(&g.identity_key) else { return false };
            if raw.len() != 32 || !seen_users.insert(g.user_id.clone()) || !seen_ids.insert(g.immutable_id.clone()) || !seen_keys.insert(raw) {
                return false;
            }
        }
        let payload = policy_payload(tid, version, policy_sequence, threshold, &guardians);
        let signature = policy["signature"].as_str().unwrap_or_default();
        if !verifies(signer, &payload, signature) {
            return false;
        }
        cache.insert(version, (policy_sequence, signature.to_owned(), guardians, threshold));
        true
    };

    let mut transitions = doc["transitions"].as_array().cloned().unwrap_or_default();
    transitions.sort_by_key(|t| t["sequence"].as_u64().unwrap_or(0));
    for t in &transitions {
        let next = sequence + 1;
        let new_key = t["new_key"].as_str().unwrap_or_default();
        if t["sequence"].as_u64() != Some(next) || t["previous_key"].as_str() != Some(current.as_str()) || new_key.is_empty() {
            return Err("broken-chain");
        }
        let payload = rotation_payload(tid, next, &current, new_key);
        let auth = if t["authorization"]["kind"].is_string() {
            t["authorization"].clone()
        } else {
            let mut a = t["authorization"].clone();
            if !a.is_object() {
                a = serde_json::json!({});
            }
            a["kind"] = t["authorization_kind"].clone();
            a
        };
        match auth["kind"].as_str() {
            Some("existing_root") => {
                if !verifies(&current, &payload, auth["signature"].as_str().unwrap_or_default()) {
                    return Err("bad-root-signature");
                }
            }
            Some("guardians") => {
                let active = policies
                    .values()
                    .filter(|p| p["identity_sequence"].as_u64().is_some_and(|s| s <= sequence))
                    .max_by_key(|p| p["version"].as_u64().unwrap_or(0));
                let wanted = auth["policy_version"].as_u64().unwrap_or(0);
                if active.and_then(|p| p["version"].as_u64()) != Some(wanted) {
                    return Err("superseded-recovery-policy");
                }
                if !verify_policy(wanted, &keys, &mut valid_policies) {
                    return Err("bad-recovery-policy");
                }
                let (_, _, guardians, threshold) = valid_policies[&wanted].clone();
                let mut approved = std::collections::HashSet::new();
                for approval in auth["approvals"].as_array().cloned().unwrap_or_default() {
                    let Some(g) = guardians.iter().find(|g| Some(g.immutable_id.as_str()) == approval["guardian_immutable_id"].as_str()) else { continue };
                    if !approved.contains(&g.immutable_id) && verifies(&g.identity_key, &payload, approval["signature"].as_str().unwrap_or_default()) {
                        approved.insert(g.immutable_id.clone());
                    }
                }
                if (approved.len() as u64) < threshold {
                    return Err("insufficient-guardians");
                }
            }
            _ => return Err("unknown-authorization"),
        }
        sequence = next;
        current = new_key.to_owned();
        keys.insert(sequence, current.clone());
    }

    if doc["sequence"].as_u64() != Some(sequence) || doc["current_key"].as_str() != Some(current.as_str()) {
        return Err("head-mismatch");
    }
    for version in policies.keys().copied().collect::<Vec<_>>() {
        if !verify_policy(version, &keys, &mut valid_policies) {
            return Err("bad-recovery-policy");
        }
    }
    let versions: Vec<u64> = valid_policies.keys().copied().collect();
    let current_policy = doc["current_policy_version"].as_u64().unwrap_or(0);
    if versions.iter().enumerate().any(|(i, v)| *v != i as u64 + 1) || current_policy != versions.last().copied().unwrap_or(0) {
        return Err("policy-head-mismatch");
    }
    Ok(Some(Continuity {
        immutable_id: tid.to_owned(),
        genesis_key: genesis.to_owned(),
        current_key: current,
        sequence,
        keys,
        policies: valid_policies.into_iter().map(|(v, (s, sig, _, _))| (v, (s, sig))).collect(),
        current_policy_version: current_policy,
    }))
}

/// What this client last saw of an account's identity, to refuse rollbacks and forks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pin {
    pub immutable_id: String,
    pub sequence: Option<u64>,
    pub current_key: Option<String>,
    pub policy_version: Option<u64>,
    pub policy_signature: Option<String>,
}

/// `pinIdentity`: checks a verified document against the pin and returns the new pin, or why
/// it is refused.
pub fn check_pin(pinned: Option<&Pin>, c: &Continuity) -> Result<Pin, &'static str> {
    let next = Pin {
        immutable_id: c.immutable_id.clone(),
        sequence: Some(c.sequence),
        current_key: Some(c.current_key.clone()),
        policy_version: Some(c.current_policy_version),
        policy_signature: c.policies.get(&c.current_policy_version).map(|(_, s)| s.clone()),
    };
    let Some(pinned) = pinned else { return Ok(next) };
    if pinned.immutable_id != next.immutable_id {
        return Err("immutable-id-changed");
    }
    if let (Some(old), Some(new)) = (pinned.sequence, next.sequence) {
        if new < old {
            return Err("identity-sequence-rollback");
        }
        if let Some(old_key) = &pinned.current_key {
            let fork = if new == old { next.current_key.as_ref() != Some(old_key) } else { c.keys.get(&old) != Some(old_key) };
            if fork {
                return Err("identity-chain-fork");
            }
        }
    }
    if let (Some(old), Some(new)) = (pinned.policy_version, next.policy_version) {
        if new < old {
            return Err("recovery-policy-rollback");
        }
        if old > 0
            && let Some(sig) = &pinned.policy_signature
            && c.policies.get(&old).map(|(_, s)| s) != Some(sig)
        {
            return Err("recovery-policy-fork");
        }
        if new > old
            && let Some(pinned_sequence) = pinned.sequence
            && c.policies.iter().any(|(v, (s, _))| *v > old && *s < pinned_sequence)
        {
            return Err("retired-root-recovery-policy");
        }
    }
    Ok(next)
}

#[cfg(test)]
mod revocation_tests {
    use super::*;

    #[test]
    fn only_root_signed_revocations_count() {
        let root = IdentityKey::from_secret(&[3; 32]).unwrap();
        let other = IdentityKey::from_secret(&[4; 32]).unwrap();
        let public = root.public_b64();
        let tid = immutable_id(&public).unwrap();
        let good = root.revoke_device("@a:x", &tid, "D1", "KEY1");
        let forged = other.revoke_device("@a:x", &tid, "D2", "KEY2");
        let statements = serde_json::json!([
            { "device_id": "D1", "mls_key": "KEY1", "signature": good },
            { "device_id": "D2", "mls_key": "KEY2", "signature": forged },
            { "device_id": "D3", "mls_key": "KEY3", "signature": good },
        ]);
        let revoked = revoked_keys("@a:x", None, Some(&public), &statements);
        assert_eq!(revoked.into_iter().collect::<Vec<_>>(), vec!["KEY1".to_owned()]);
        assert!(revoked_keys("@a:x", None, None, &statements).is_empty());
    }

    #[test]
    fn a_generated_key_binds_and_seals_as_the_web_reads_it() {
        let key = IdentityKey::generate();
        let public = key.public_b64();
        assert!(key.binds("@a:x", &public));
        assert!(!IdentityKey::generate().binds("@a:x", &public));
        let ring = Keyring::generate();
        let sealed = seal_cross_signing_secret(&key, &ring);
        assert!(sealed.starts_with(RECOVERY_SEAL));
        let opened = open_cross_signing_secret(&sealed, &ring).unwrap();
        assert_eq!(opened.public_b64(), public);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vectors() -> Option<Value> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.dev/telium/docs/spec/vectors/domain-separated.json");
        std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok())
    }

    /// Against Telium's published vectors, when the clone is present.
    #[test]
    fn spec_vectors() {
        let Some(doc) = vectors() else {
            eprintln!("no Telium clone; skipping vectors");
            return;
        };
        for v in doc["vectors"].as_array().unwrap() {
            match v["name"].as_str().unwrap() {
                "immutable-id" => {
                    assert_eq!(immutable_id(v["identity_key_b64u"].as_str().unwrap()).unwrap(), v["immutable_id"].as_str().unwrap());
                }
                "identity-anchor" | "identity-rotation" => {
                    let bytes = hex_decode(v["canonical"]["hex"].as_str().unwrap());
                    let f = &v["fields"];
                    let ours = if v["name"] == "identity-anchor" {
                        anchor_payload(f["user_id"].as_str().unwrap(), f["immutable_id"].as_str().unwrap(), f["identity_key"].as_str().unwrap())
                    } else {
                        rotation_payload(
                            f["immutable_id"].as_str().unwrap(),
                            f["sequence"].as_u64().unwrap(),
                            f["previous_key"].as_str().unwrap(),
                            f["new_key"].as_str().unwrap(),
                        )
                    };
                    assert_eq!(ours, bytes, "{}", v["name"]);
                    let key = v["key"]["public_b64u"].as_str().or_else(|| v["key"].as_str()).unwrap_or_default();
                    if !key.is_empty() {
                        assert!(verifies(key, &bytes, v["signature_b64u"].as_str().unwrap()), "{} signature", v["name"]);
                    }
                }
                _ => {}
            }
        }
    }

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn key(seed: u8) -> IdentityKey {
        IdentityKey::from_secret(&[seed; 32]).unwrap()
    }

    fn anchored(user: &str, root: &IdentityKey) -> Value {
        let genesis = root.public_b64();
        let tid = immutable_id(&genesis).unwrap();
        json!({
            "exists": true, "user_id": user, "immutable_id": tid, "genesis_key": genesis,
            "anchor_signature": b64(&root.sign(&anchor_payload(user, &tid, &genesis))),
            "transitions": [], "recovery_policies": [], "sequence": 0, "current_key": genesis,
            "current_policy_version": 0,
        })
    }

    #[test]
    fn continuity_chain_and_pins() {
        let (root, next) = (key(1), key(2));
        let mut doc = anchored("@a:x", &root);
        let c0 = verify_identity_document(&doc, Some("@a:x")).unwrap().unwrap();
        assert_eq!(c0.sequence, 0);
        assert_eq!(verify_identity_document(&doc, Some("@b:x")), Err("wrong-user"));

        // Rotate to a new root, signed by the old one.
        let tid = c0.immutable_id.clone();
        let payload = rotation_payload(&tid, 1, &root.public_b64(), &next.public_b64());
        doc["transitions"] = json!([{ "sequence": 1, "previous_key": root.public_b64(), "new_key": next.public_b64(),
            "authorization": { "kind": "existing_root", "signature": b64(&root.sign(&payload)) } }]);
        doc["sequence"] = json!(1);
        doc["current_key"] = json!(next.public_b64());
        let c1 = verify_identity_document(&doc, None).unwrap().unwrap();
        assert_eq!(c1.current_key, next.public_b64());

        let pin0 = check_pin(None, &c0).unwrap();
        let pin1 = check_pin(Some(&pin0), &c1).unwrap();
        assert_eq!(check_pin(Some(&pin1), &c0), Err("identity-sequence-rollback"));

        // A forged rotation (signed by the wrong key) is refused.
        let forged = rotation_payload(&tid, 1, &root.public_b64(), &key(3).public_b64());
        doc["transitions"][0]["new_key"] = json!(key(3).public_b64());
        doc["transitions"][0]["authorization"]["signature"] = json!(b64(&next.sign(&forged)));
        doc["current_key"] = json!(key(3).public_b64());
        assert_eq!(verify_identity_document(&doc, None), Err("bad-root-signature"));
    }

    #[test]
    fn both_private_key_forms() {
        let seed = [5u8; 32];
        assert_eq!(ed25519_seed(&seed).unwrap(), seed);
        assert_eq!(ed25519_seed(&pkcs8(&seed)).unwrap(), seed);
        assert!(ed25519_seed(&[0u8; 48]).is_err());
        assert_eq!(IdentityKey::from_secret(&pkcs8(&seed)).unwrap().public_b64(), IdentityKey::from_secret(&seed).unwrap().public_b64());
    }

    #[test]
    fn device_vouching_and_key_binding() {
        let root = key(9);
        let sig = root.vouch_for_device("@a:x", "d1", "bWxz");
        let device = json!({ "device_id": "d1", "mls_key": "bWxz", "cross_sig": sig, "active": true });
        assert!(device_is_vouched("@a:x", &root.public_b64(), &device));
        assert!(!device_is_vouched("@a:x", &key(8).public_b64(), &device));
        assert!(!device_is_vouched("@a:x", &root.public_b64(), &json!({ "device_id": "d2", "mls_key": "bWxz", "cross_sig": sig })));
        assert!(root.binds("@a:x", &root.public_b64()));
        assert!(!root.binds("@a:x", &key(8).public_b64()));

        let ring = Keyring::generate();
        let sealed = seal_cross_signing_secret(&root, &ring);
        assert_eq!(open_cross_signing_secret(&sealed, &ring).unwrap().public_b64(), root.public_b64());
        assert!(open_cross_signing_secret("device-v1:abc.def", &ring).is_none());
    }
}
