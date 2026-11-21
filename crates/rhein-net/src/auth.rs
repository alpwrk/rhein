//! Sign-in against a home server and the assertion → guild-session exchange.

use anyhow::{Context as _, Result, bail};
use rhein_auth::{DeviceKey, SerenityKsf};
use rhein_proto::*;

use crate::api::Api;

pub struct SignedIn {
    pub session: HomeSession,
    /// OPAQUE export key; kept in memory only, it unwraps the recovery vault later.
    pub export_key: Vec<u8>,
}

/// Refuses servers that do not speak OPAQUE rather than falling back to a password that travels.
pub async fn require_opaque(home: &Api) -> Result<Capabilities> {
    let caps = home.capabilities().await.context("capabilities")?;
    if !caps.auth.opaque {
        bail!("{} does not offer OPAQUE sign-in; Rhein will not send a password", caps.server_name);
    }
    Ok(caps)
}

pub async fn login(
    home: &Api,
    user: &str,
    password: &str,
    device: &DeviceKey,
    device_name: &str,
    ksf: &SerenityKsf,
) -> Result<SignedIn> {
    login_with_code(home, user, password, device, device_name, ksf, None).await
}

/// Refuses a key-stretching set this build doesn't know before the password is tried with the
/// wrong one: that looks exactly like a wrong password. `ksf` is always set 1 (the web client's
/// `KSF_VERSIONS[1]`, memory-constrained Argon2id); the server names what the record was made with.
fn check_ksf(version: Option<i64>) -> Result<()> {
    match version.unwrap_or(rhein_auth::KSF_VERSION) {
        rhein_auth::KSF_VERSION => Ok(()),
        other => bail!(
            "this account's password uses key-stretching version {other}, which this Rhein does not know yet; update Rhein to sign in"
        ),
    }
}

/// Signs in, with the code from an authenticator app for an account with two-factor sign-in (the
/// server answers `mfa_required` without one).
pub async fn login_with_code(
    home: &Api,
    user: &str,
    password: &str,
    device: &DeviceKey,
    device_name: &str,
    ksf: &SerenityKsf,
    mfa_code: Option<&str>,
) -> Result<SignedIn> {
    require_opaque(home).await?;
    let start = rhein_auth::login_start(password)?;
    let answer: LoginStartResponse = home
        .post("/login/opaque/start", &LoginStart { user, start_login_request: start.request.clone() })
        .await?;
    if answer.legacy_upgrade_required {
        bail!("this account still uses a legacy password; sign in once with the web client to upgrade it");
    }
    check_ksf(answer.ksf_version)?;
    let finished = start.finish(password, &answer.login_response, ksf)?;
    let session: HomeSession = home
        .post(
            "/login/opaque/finish",
            &LoginFinish {
                login_id: &answer.login_id,
                finish_login_request: finished.message,
                device_name,
                device_pubkey: device.public_b64u(),
                mfa_code,
            },
        )
        .await?;
    home.set_token(session.access_token.clone()).await;
    Ok(SignedIn { session, export_key: finished.export_key })
}

/// What a sign-up may carry beyond the password.
#[derive(Debug, Default, Clone, Copy)]
pub struct RegisterOptions<'a> {
    /// For a home server that registers by code only.
    pub registration_code: Option<&'a str>,
    /// The recovery key's public half (`RecoveryKey::public_key`). Sent only to a server that
    /// offers recovery-key resets; `session.recovery_key_enrolled` says whether it kept it.
    pub recovery_public_key: Option<&'a str>,
}

pub async fn register(
    home: &Api,
    localpart: &str,
    password: &str,
    device: &DeviceKey,
    device_name: &str,
    ksf: &SerenityKsf,
) -> Result<SignedIn> {
    register_with(home, localpart, password, device, device_name, ksf, RegisterOptions::default()).await
}

pub async fn register_with(
    home: &Api,
    localpart: &str,
    password: &str,
    device: &DeviceKey,
    device_name: &str,
    ksf: &SerenityKsf,
    options: RegisterOptions<'_>,
) -> Result<SignedIn> {
    let caps = require_opaque(home).await?;
    let code = options.registration_code.map(str::trim).filter(|c| !c.is_empty());
    if !caps.registration_open {
        if !caps.registration_code_required {
            bail!("registration is closed on {}", caps.server_name);
        }
        if code.is_none() {
            bail!("{} registers new accounts by invitation: enter the registration code you were given", caps.server_name);
        }
    }
    let start = rhein_auth::registration_start(password)?;
    let answer: RegisterStartResponse = home
        .post(
            "/register/opaque/start",
            &RegisterStart { localpart, registration_request: start.request.clone(), registration_code: code },
        )
        .await?;
    let finished = start.finish(password, &answer.registration_response, ksf)?;
    let session: HomeSession = home
        .post(
            "/register/opaque/finish",
            &RegisterFinish {
                localpart,
                registration_record: finished.message,
                device_name,
                device_pubkey: device.public_b64u(),
                registration_code: code,
                ksf_version: rhein_auth::KSF_VERSION,
                recovery_public_key: options.recovery_public_key.filter(|_| caps.auth.recovery_key_reset).map(str::to_owned),
            },
        )
        .await?;
    home.set_token(session.access_token.clone()).await;
    Ok(SignedIn { session, export_key: finished.export_key })
}

/// Proves the password again inside a signed-in session (no new device session), and hands back
/// the OPAQUE export key: how a restored session unlocks the encrypted-DM vault.
pub async fn reauthenticate(home: &Api, password: &str, ksf: &SerenityKsf) -> Result<Vec<u8>> {
    Ok(reauth(home, password, ksf, None).await?.0)
}

/// Proves the password again for a sensitive change (two-factor, the recovery address, deleting
/// the account): the `{reauth_token}` those routes take.
pub async fn reauth_proof(home: &Api, password: &str, ksf: &SerenityKsf) -> Result<serde_json::Value> {
    let token = reauth(home, password, ksf, None).await?.1["reauth_token"].as_str().map(str::to_owned).context("the server issued no reauthentication token")?;
    Ok(serde_json::json!({ "reauth_token": token }))
}

/// Enrols a recovery key for an account that has none: its public half and the vault copy sealed
/// under it (`k1.`), written together behind a password proof, as the web client's
/// `reauthenticateOpaque` does. Returns the OPAQUE export key.
pub async fn enroll_recovery_key(
    home: &Api,
    password: &str,
    ksf: &SerenityKsf,
    recovery_public_key: &str,
    vault_copy: &str,
) -> Result<Vec<u8>> {
    let caps = home.capabilities().await.context("capabilities")?;
    if !caps.auth.recovery_key_reset {
        bail!("{} can't keep a recovery key", caps.server_name);
    }
    let extra = serde_json::json!({ "recovery_public_key": recovery_public_key, "recovery_vault_ciphertext": vault_copy });
    let (export_key, proof) = reauth(home, password, ksf, Some(extra)).await?;
    // A server that ignores the fields answers with a plain reauth token: nothing was kept.
    if proof["recovery_key_enrolled"].as_bool() != Some(true) {
        bail!("{} did not keep the recovery key", caps.server_name);
    }
    Ok(export_key)
}

async fn reauth(
    home: &Api,
    password: &str,
    ksf: &SerenityKsf,
    extra: Option<serde_json::Value>,
) -> Result<(Vec<u8>, serde_json::Value)> {
    let start = rhein_auth::login_start(password)?;
    let answer: serde_json::Value = home
        .post("/auth/opaque/reauth/start", &serde_json::json!({ "start_login_request": start.request.clone() }))
        .await
        .context("reauthentication")?;
    check_ksf(answer["ksf_version"].as_i64())?;
    let login_response = answer["login_response"].as_str().context("no login response")?;
    let finished = start.finish(password, login_response, ksf).context("That password was not accepted.")?;
    let mut body = serde_json::json!({ "login_id": answer["login_id"], "finish_login_request": finished.message });
    if let (Some(body), Some(serde_json::Value::Object(extra))) = (body.as_object_mut(), extra) {
        body.extend(extra);
    }
    let proof: serde_json::Value = home.post("/auth/opaque/reauth/finish", &body).await.map_err(|err| {
        // A refused enrolment (409: a key is already enrolled, or the vault is not ready) is not a
        // wrong password.
        if err.status() == Some(409) {
            anyhow::Error::new(err)
        } else {
            anyhow::Error::new(err).context("That password was not accepted.")
        }
    })?;
    Ok((finished.export_key, proof))
}

/// Trades a short-lived, audience-bound assertion from the home server for a session on the guild
/// server named `audience`. After this the home server is off the guild's hot path.
pub async fn guild_session(home: &Api, guild: &Api, audience: &str) -> Result<GuildSession> {
    let assertion = home.assertion(audience).await.context("identity assertion")?;
    let session = guild.create_session(&assertion.assertion).await.context("guild session")?;
    guild.set_token(session.session_token.clone()).await;
    guild.set_expires_at(session.expires_at);
    // From here on it renews itself when it runs out, as the web client's sessions do.
    guild.renew_from(home, audience);
    Ok(session)
}
