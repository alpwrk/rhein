//! DM keys and trust against a running dev server (`RHEIN_LIVE=localhost:8448`), in one test
//! because the secrets location comes from a process-wide environment variable.

use std::time::Duration;

use rhein_auth::{DeviceKey, SerenityKsf};
use rhein_mls::recovery::RecoveryKey;
use rhein_mls::trust::{self, IdentityKey};
use rhein_mls::vault::{self, Keyring};
use rhein_net::{Api, auth, discover};
use rhein_store::dm::{Secrets, bring_up, enroll_recovery_key, keep_new_recovery_key, recovery_key_saved};

const PASSWORD: &str = "correct horse battery";

async fn register(home: &Api, localpart: &str, recovery: Option<&RecoveryKey>) -> auth::SignedIn {
    let ksf = SerenityKsf::argon2id(1, 8, 1);
    let public = recovery.map(RecoveryKey::public_key);
    let options = auth::RegisterOptions { registration_code: None, recovery_public_key: public.as_deref() };
    for _ in 0..20 {
        match auth::register_with(home, localpart, PASSWORD, &DeviceKey::generate(), "rhein dm test", &ksf, options).await {
            Ok(signed) => return signed,
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
    panic!("could not register {localpart}");
}

/// What a web peer checks before letting a device into a group (`verify.js`): an identity key,
/// anchored in a continuity document whose head it is, that vouches for the device's MLS key.
async fn web_peer_accepts(home: &Api, user: &str, device: &str, mls_key: &str, identity_key: &str) {
    let list = home.device_list(user).await.unwrap();
    assert_eq!(list["identity_key"], identity_key);
    let ours = list["devices"].as_array().unwrap().iter().find(|d| d["device_id"] == device).expect("our device");
    assert_eq!(ours["mls_key"], mls_key);
    assert!(trust::device_is_vouched(user, identity_key, ours));
    let published = home.identity_document(user).await.unwrap();
    let continuity = trust::verify_identity_document(&published, Some(user)).unwrap().expect("a continuity document");
    assert_eq!(continuity.current_key, identity_key);
}

#[tokio::test]
async fn vault_cross_signing_and_continuity() {
    let Ok(server) = std::env::var("RHEIN_LIVE") else {
        eprintln!("RHEIN_LIVE not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("rhein-dm-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // SAFETY: the only test in this binary.
    unsafe { std::env::set_var("RHEIN_CONFIG_DIR", &dir) };

    let http = reqwest::Client::new();
    let doc = discover(&http, &server).await.expect("discovery");
    let pid = std::process::id();

    // 1. A sign-up as Rhein makes it now: a recovery key from the start. The first bring-up
    //    creates the vault with the recovery key's copy, and the account key, anchored and
    //    vouching for this device, so web peers let it into DMs.
    {
        let home = Api::new(http.clone(), &doc.api, None);
        let recovery = RecoveryKey::generate();
        let signed = register(&home, &format!("dm{pid}"), Some(&recovery)).await;
        let (user, device) = (signed.session.user_id.clone(), signed.session.device_id.clone());
        assert!(signed.session.recovery_key_enrolled, "the server kept the recovery key");
        keep_new_recovery_key(&user, &recovery).unwrap();
        let wrapper = vault::recovery_wrapper(&signed.export_key);

        let first = bring_up(&home, &user, &device, Some(wrapper)).await.expect("first bring-up");
        assert_eq!(first.vault_mode, "opaque_custody");
        assert!(first.locked.is_none(), "{first:?}");
        assert!(first.cross_signed && first.identity_problem.is_none(), "{first:?}");
        assert!(first.recovery_key_enrolled && first.recovery_copy, "{first:?}");
        assert_eq!(first.unsaved_recovery_key.as_deref(), Some(recovery.text().as_str()));
        let identity_key = first.identity_key.clone().expect("an identity key");
        assert_eq!(first.immutable_id.as_deref(), Some(trust::immutable_id(&identity_key).unwrap().as_str()));

        let ring = Secrets::load(&user).keyring.expect("the keyring is kept on this device");
        let stored = home.vault().await.unwrap();
        assert_eq!(vault::open_vault(&stored, Some(&wrapper), None).unwrap_as_ring(), ring);
        // The recovery key's copy opens to the same ring: a password reset keeps the history.
        assert_eq!(recovery.open_vault_copy(stored["independent_ciphertext"].as_str().unwrap()).unwrap(), ring);
        // The account key is sealed under R alone, as `ensureRoot` seals it.
        let cs = home.cross_signing().await.unwrap();
        assert!(cs["sealed_secret"].as_str().unwrap().starts_with(trust::RECOVERY_SEAL));
        assert_eq!(trust::open_cross_signing_secret(cs["sealed_secret"].as_str().unwrap(), &ring).unwrap().public_b64(), identity_key);
        web_peer_accepts(&home, &user, &device, &first.mls_key, &identity_key).await;

        // A restored session (no password) changes nothing; saying the key is saved forgets it.
        recovery_key_saved(&user).unwrap();
        let again = bring_up(&home, &user, &device, None).await.expect("second bring-up");
        assert_eq!(again, rhein_store::dm::DmStatus { unsaved_recovery_key: None, ..first });
    }

    // 2. An account an older Rhein made: no recovery key, and no account key until now. The
    //    bring-up makes the account key; Settings then makes a recovery key behind the password.
    {
        let home = Api::new(http.clone(), &doc.api, None);
        let signed = register(&home, &format!("dmold{pid}"), None).await;
        let (user, device) = (signed.session.user_id.clone(), signed.session.device_id.clone());
        let wrapper = vault::recovery_wrapper(&signed.export_key);
        let first = bring_up(&home, &user, &device, Some(wrapper)).await.expect("bring-up");
        assert!(first.cross_signed && first.identity_key.is_some(), "{first:?}");
        assert!(!first.recovery_key_enrolled && !first.recovery_copy, "{first:?}");
        web_peer_accepts(&home, &user, &device, &first.mls_key, first.identity_key.as_deref().unwrap()).await;

        let text = enroll_recovery_key(&home, &user, PASSWORD, &SerenityKsf::argon2id(1, 8, 1)).await.expect("enrolling a recovery key");
        let after = bring_up(&home, &user, &device, None).await.unwrap();
        assert!(after.recovery_key_enrolled && after.recovery_copy, "{after:?}");
        let ring = Secrets::load(&user).keyring.unwrap();
        let copy = home.vault().await.unwrap()["independent_ciphertext"].as_str().unwrap().to_owned();
        assert_eq!(RecoveryKey::parse(&text).unwrap().open_vault_copy(&copy).unwrap(), ring);
        // A second enrolment is refused, not taken as a wrong password.
        let again = enroll_recovery_key(&home, &user, PASSWORD, &SerenityKsf::argon2id(1, 8, 1)).await.unwrap_err();
        assert!(format!("{again:#}").contains("already has a recovery key"), "{again:#}");
    }

    // 3. An account whose key the web client made first: Rhein adopts it rather than making
    //    another (and `create_only` would refuse one anyway).
    {
        let home = Api::new(http.clone(), &doc.api, None);
        let signed = register(&home, &format!("dmweb{pid}"), None).await;
        let (user, device) = (signed.session.user_id.clone(), signed.session.device_id.clone());
        let wrapper = vault::recovery_wrapper(&signed.export_key);
        let ring = Keyring::generate();
        let _: serde_json::Value = home.put_vault(&vault::new_vault_body(&ring, &wrapper, None)).await.expect("vault");
        let identity = IdentityKey::generate();
        let body = serde_json::json!({
            "identity_key": identity.public_b64(),
            "sealed_secret": trust::seal_cross_signing_secret(&identity, &ring),
            "create_only": true,
        });
        let _: serde_json::Value = home.put("/crosssigning", &body).await.expect("publish identity");
        let status = bring_up(&home, &user, &device, Some(wrapper)).await.expect("bring-up");
        assert_eq!(status.identity_key.as_deref(), Some(identity.public_b64().as_str()));
        assert!(status.cross_signed && status.identity_problem.is_none(), "{status:?}");
        web_peer_accepts(&home, &user, &device, &status.mls_key, &identity.public_b64()).await;
        // And the server keeps refusing a second first key.
        let other = IdentityKey::generate();
        let refused = home.create_cross_signing(&other.public_b64(), &trust::seal_cross_signing_secret(&other, &ring)).await.unwrap_err();
        assert_eq!(refused.status(), Some(409));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

trait RingOf {
    fn unwrap_as_ring(self) -> Keyring;
}

impl RingOf for anyhow::Result<vault::Opened> {
    fn unwrap_as_ring(self) -> Keyring {
        match self.unwrap() {
            vault::Opened::Ring { keyring, .. } => keyring,
            other => panic!("expected the keyring, got {other:?}"),
        }
    }
}
