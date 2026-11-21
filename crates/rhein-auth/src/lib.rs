//! Client-side account cryptography: OPAQUE (RFC 9807) and the per-install device key.
//!
//! OPAQUE must match `@serenity-kit/opaque` 1.1.0 bit for bit, because that is what the web client
//! registers accounts with: Ristretto255 OPRF, TripleDH over Ristretto255/SHA-512, and Argon2id with
//! the "memory-constrained" parameters (t=3, m=2^16 KiB, p=4) over a 16-byte all-zero salt, no
//! identifiers and no context. A different KSF looks exactly like a wrong password.

use anyhow::{Context as _, Result, anyhow};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::SigningKey;
use opaque_ke::errors::InternalError;
use opaque_ke::generic_array::{ArrayLength, GenericArray};
use opaque_ke::ksf::Ksf;
use opaque_ke::{
    CipherSuite, ClientLogin, ClientLoginFinishParameters, ClientRegistration,
    ClientRegistrationFinishParameters, CredentialResponse, Identifiers, RegistrationResponse,
};
use rand::rngs::OsRng;

pub mod signed;

pub fn b64u(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn unb64u(text: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).context("invalid base64url")
}

/// The key-stretching set `SerenityKsf::memory_constrained` is, as Telium numbers them
/// (`accounts.opaque_ksf_version`, the web client's `KSF_VERSIONS`). Sent at registration, and
/// checked against what the server says an account's record was made with before signing in.
pub const KSF_VERSION: i64 = 1;

/// serenity-kit's `CustomKsf`, with its default configuration.
pub struct SerenityKsf {
    argon: Argon2<'static>,
}

impl SerenityKsf {
    pub fn memory_constrained() -> Self {
        Self::argon2id(3, 1 << 16, 4)
    }

    /// Cheap parameters, for tests against a server that does not care which KSF a client used.
    pub fn argon2id(t_cost: u32, m_cost: u32, p_cost: u32) -> Self {
        let params = Params::new(m_cost, t_cost, p_cost, None).expect("valid argon2 params");
        Self { argon: Argon2::new(Algorithm::Argon2id, Version::V0x13, params) }
    }
}

impl Default for SerenityKsf {
    fn default() -> Self {
        Self::memory_constrained()
    }
}

impl Ksf for SerenityKsf {
    fn hash<L: ArrayLength<u8>>(
        &self,
        input: GenericArray<u8, L>,
    ) -> Result<GenericArray<u8, L>, InternalError> {
        let mut output = GenericArray::default();
        self.argon
            .hash_password_into(&input, &[0; argon2::RECOMMENDED_SALT_LEN], &mut output)
            .map_err(|_| InternalError::KsfError)?;
        Ok(output)
    }
}

pub struct Suite;

impl CipherSuite for Suite {
    type OprfCs = opaque_ke::Ristretto255;
    type KeyExchange = opaque_ke::TripleDh<opaque_ke::Ristretto255, sha2::Sha512>;
    type Ksf = SerenityKsf;
}

/// First half of registration: keep `state`, send `request` to `register/opaque/start`.
pub struct RegistrationStart {
    state: ClientRegistration<Suite>,
    pub request: String,
}

pub struct Finished {
    /// base64url message for the `/finish` call.
    pub message: String,
    /// OPAQUE's export key: the root the recovery vault is unwrapped from.
    pub export_key: Vec<u8>,
}

pub fn registration_start(password: &str) -> Result<RegistrationStart> {
    let started = ClientRegistration::<Suite>::start(&mut OsRng, password.as_bytes())
        .map_err(|e| anyhow!("opaque registration start: {e:?}"))?;
    Ok(RegistrationStart {
        request: b64u(started.message.serialize().as_slice()),
        state: started.state,
    })
}

impl RegistrationStart {
    pub fn finish(self, password: &str, response_b64: &str, ksf: &SerenityKsf) -> Result<Finished> {
        let response = RegistrationResponse::<Suite>::deserialize(&unb64u(response_b64)?)
            .map_err(|e| anyhow!("registration response: {e:?}"))?;
        let params = ClientRegistrationFinishParameters::new(Identifiers::default(), Some(ksf));
        let finished = self
            .state
            .finish(&mut OsRng, password.as_bytes(), response, params)
            .map_err(|e| anyhow!("opaque registration finish: {e:?}"))?;
        Ok(Finished {
            message: b64u(finished.message.serialize().as_slice()),
            export_key: finished.export_key.to_vec(),
        })
    }
}

pub struct LoginStart {
    state: ClientLogin<Suite>,
    pub request: String,
}

pub fn login_start(password: &str) -> Result<LoginStart> {
    let started = ClientLogin::<Suite>::start(&mut OsRng, password.as_bytes())
        .map_err(|e| anyhow!("opaque login start: {e:?}"))?;
    Ok(LoginStart {
        request: b64u(started.message.serialize().as_slice()),
        state: started.state,
    })
}

impl LoginStart {
    /// Fails on a wrong password (the server cannot tell the client apart from an attacker until
    /// this step, and neither side learns more than "no").
    pub fn finish(self, password: &str, response_b64: &str, ksf: &SerenityKsf) -> Result<Finished> {
        let response = CredentialResponse::<Suite>::deserialize(&unb64u(response_b64)?)
            .map_err(|e| anyhow!("credential response: {e:?}"))?;
        let params = ClientLoginFinishParameters::new(None, Identifiers::default(), Some(ksf));
        let finished = self
            .state
            .finish(&mut OsRng, password.as_bytes(), response, params)
            .map_err(|_| anyhow!("wrong username or password"))?;
        Ok(Finished {
            message: b64u(finished.message.serialize().as_slice()),
            export_key: finished.export_key.to_vec(),
        })
    }
}

/// The Ed25519 key this install presents as its device: the server binds it into every identity
/// assertion. Generated locally so the secret never travels.
pub struct DeviceKey(SigningKey);

impl DeviceKey {
    pub fn generate() -> Self {
        Self(SigningKey::generate(&mut OsRng))
    }

    pub fn from_seed(seed: &[u8]) -> Result<Self> {
        let seed: [u8; 32] = seed.try_into().map_err(|_| anyhow!("device seed must be 32 bytes"))?;
        Ok(Self(SigningKey::from_bytes(&seed)))
    }

    pub fn seed(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    pub fn public_b64u(&self) -> String {
        b64u(self.0.verifying_key().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opaque_ke::{
        RegistrationRequest, ServerLogin, ServerLoginParameters, ServerRegistration, ServerSetup,
    };

    /// A full round trip against opaque-ke's server half, with the same suite the Telium server
    /// runs (its KSF type never runs server-side).
    #[test]
    fn register_then_login() {
        let ksf = SerenityKsf::argon2id(1, 8, 1);
        let setup = ServerSetup::<Suite>::new(&mut OsRng);
        let id = b"@alice:example.org";

        let reg = registration_start("hunter22").unwrap();
        let request =
            RegistrationRequest::<Suite>::deserialize(&unb64u(&reg.request).unwrap()).unwrap();
        let response = ServerRegistration::<Suite>::start(&setup, request, id).unwrap();
        let finished = reg
            .finish("hunter22", &b64u(response.message.serialize().as_slice()), &ksf)
            .unwrap();
        let record = ServerRegistration::<Suite>::finish(
            opaque_ke::RegistrationUpload::deserialize(&unb64u(&finished.message).unwrap())
                .unwrap(),
        );

        let login = login_start("hunter22").unwrap();
        let credential_request =
            opaque_ke::CredentialRequest::deserialize(&unb64u(&login.request).unwrap()).unwrap();
        let server_login = ServerLogin::start(
            &mut OsRng,
            &setup,
            Some(record.clone()),
            credential_request,
            id,
            ServerLoginParameters::default(),
        )
        .unwrap();
        let done = login
            .finish("hunter22", &b64u(server_login.message.serialize().as_slice()), &ksf)
            .unwrap();
        assert_eq!(done.export_key, finished.export_key);

        let wrong = login_start("hunter23").unwrap();
        let credential_request =
            opaque_ke::CredentialRequest::deserialize(&unb64u(&wrong.request).unwrap()).unwrap();
        let server_login = ServerLogin::start(
            &mut OsRng,
            &setup,
            Some(record),
            credential_request,
            id,
            ServerLoginParameters::default(),
        )
        .unwrap();
        assert!(
            wrong
                .finish("hunter23", &b64u(server_login.message.serialize().as_slice()), &ksf)
                .is_err()
        );
    }

    #[test]
    fn device_key_roundtrip() {
        let key = DeviceKey::generate();
        let again = DeviceKey::from_seed(&key.seed()).unwrap();
        assert_eq!(key.public_b64u(), again.public_b64u());
    }
}
