//! Telium's MLS profile on openmls, so Rhein can take part in the encrypted DMs of the web client
//! (`web/src/lib/mls.js`, ts-mls):
//!
//! - one ciphersuite, `MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519`;
//! - basic credentials whose identity is `"{user_id}|{device_id}"`;
//! - the group id is the conversation id as UTF-8;
//! - a JSON membership policy in GroupContext extension `0xF7A0`, which every leaf advertises;
//! - the ratchet tree travels inside Welcome and GroupInfo;
//! - commits and application messages go out as PrivateMessage, external commits as PublicMessage,
//!   and both come in.
//!
//! Frames are whole `MLSMessage`s; callers base64 them for the wire.

pub mod call_group;
pub mod payload;
pub mod sealed_sync;
pub mod policy;
pub mod recovery;
pub mod trust;
pub mod vault;

use anyhow::{Context as _, Result, anyhow, bail};
use openmls::prelude::tls_codec::{Deserialize as _, Serialize as _};
use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;

pub use policy::Policy;

pub const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;
/// The private-use GroupContext extension that carries the membership policy.
pub const POLICY_EXTENSION: u16 = 0xF7A0;
/// ts-mls keeps four past epochs' keys; so do we, so late messages across a commit still open.
const MAX_PAST_EPOCHS: usize = 4;
/// ts-mls: keys for 10 generations back, up to 200 ratchet steps forward.
const OUT_OF_ORDER_TOLERANCE: u32 = 10;
const MAXIMUM_FORWARD_DISTANCE: u32 = 200;

/// The wire-format policy: send ciphertext, accept both (an external commit is a PublicMessage).
fn wire_format() -> WireFormatPolicy {
    MIXED_CIPHERTEXT_WIRE_FORMAT_POLICY
}

/// What every leaf says it can do: this suite, basic credentials, and the policy extension.
pub fn capabilities() -> Capabilities {
    Capabilities::new(
        None,
        Some(&[CIPHERSUITE]),
        Some(&[ExtensionType::Unknown(POLICY_EXTENSION)]),
        None,
        Some(&[CredentialType::Basic]),
    )
}

pub fn join_config() -> MlsGroupJoinConfig {
    MlsGroupJoinConfig::builder()
        .wire_format_policy(wire_format())
        .max_past_epochs(MAX_PAST_EPOCHS)
        .sender_ratchet_configuration(SenderRatchetConfiguration::new(OUT_OF_ORDER_TOLERANCE, MAXIMUM_FORWARD_DISTANCE))
        .use_ratchet_tree_extension(true)
        .build()
}

fn create_config(policy: Option<&Policy>) -> Result<MlsGroupCreateConfig> {
    let mut builder = MlsGroupCreateConfig::builder()
        .ciphersuite(CIPHERSUITE)
        .wire_format_policy(wire_format())
        .max_past_epochs(MAX_PAST_EPOCHS)
        .sender_ratchet_configuration(SenderRatchetConfiguration::new(OUT_OF_ORDER_TOLERANCE, MAXIMUM_FORWARD_DISTANCE))
        .use_ratchet_tree_extension(true)
        .capabilities(capabilities());
    if let Some(policy) = policy {
        let ext = Extension::Unknown(POLICY_EXTENSION, UnknownExtension(policy.to_bytes()));
        builder = builder.with_group_context_extensions(Extensions::single(ext).map_err(|e| anyhow!("policy extension: {e:?}"))?);
    }
    Ok(builder.build())
}

/// `"{user_id}|{device_id}"` out of a basic credential.
pub fn identity_of(credential: &Credential) -> Option<(String, String)> {
    let basic = BasicCredential::try_from(credential.clone()).ok()?;
    let text = std::str::from_utf8(basic.identity()).ok()?;
    let (user, device) = text.split_once('|')?;
    (!user.is_empty() && !device.is_empty()).then(|| (user.to_owned(), device.to_owned()))
}

/// The membership policy a group carries, `Ok(None)` when it has none; an extension that doesn't
/// parse is an error, never "no policy".
pub fn policy_of(group: &MlsGroup) -> Result<Option<Policy>> {
    policy_in(group.extensions())
}

pub fn policy_in(extensions: &Extensions<GroupContext>) -> Result<Option<Policy>> {
    match extensions.unknown(POLICY_EXTENSION) {
        Some(UnknownExtension(bytes)) => Policy::from_bytes(bytes).map(Some),
        None => Ok(None),
    }
}

/// What processing a frame did.
#[derive(Debug)]
pub enum Processed {
    /// An application message: the plaintext and who sent it (`user_id`, `device_id`).
    Application { plaintext: Vec<u8>, sender: Option<(String, String)> },
    /// A commit was merged; the group is at a new epoch.
    Commit { sender: Option<(String, String)> },
    /// A standalone proposal, stored for the next commit.
    Proposal,
    /// One of our own frames coming back: it can't be decrypted (sending moved past its keys),
    /// so its plaintext is whatever was kept when it was sent.
    Own,
}

/// One device's MLS identity: its signature key and credential, over an openmls provider that
/// holds key packages and group states.
pub struct Device {
    pub provider: OpenMlsRustCrypto,
    pub(crate) signer: SignatureKeyPair,
    credential: CredentialWithKey,
}

impl Device {
    /// A new device with a fresh signature key. `identity` is `"{user_id}|{device_id}"`.
    pub fn new(identity: &str) -> Result<Device> {
        let provider = OpenMlsRustCrypto::default();
        let signer = SignatureKeyPair::new(CIPHERSUITE.signature_algorithm()).map_err(|e| anyhow!("signature key: {e:?}"))?;
        signer.store(provider.storage()).map_err(|e| anyhow!("storing signature key: {e:?}"))?;
        Self::with_signer(provider, signer, identity)
    }

    /// A device with an existing signature key (the web client keeps one per account and reuses
    /// it on every device session).
    pub fn with_signer(provider: OpenMlsRustCrypto, signer: SignatureKeyPair, identity: &str) -> Result<Device> {
        let credential = CredentialWithKey {
            credential: BasicCredential::new(identity.as_bytes().to_vec()).into(),
            signature_key: signer.public().into(),
        };
        Ok(Device { provider, signer, credential })
    }

    pub fn signature_public_key(&self) -> &[u8] {
        self.signer.public()
    }

    pub fn identity(&self) -> Option<(String, String)> {
        identity_of(&self.credential.credential)
    }

    /// A fresh key package, as an encoded `MLSMessage` ready to publish.
    pub fn key_package(&self) -> Result<Vec<u8>> {
        let bundle = KeyPackage::builder()
            .leaf_node_capabilities(capabilities())
            .build(CIPHERSUITE, &self.provider, &self.signer, self.credential.clone())
            .map_err(|e| anyhow!("key package: {e:?}"))?;
        let message: MlsMessageOut = bundle.key_package().clone().into();
        Ok(message.tls_serialize_detached()?)
    }

    /// Creates the group for a conversation and adds the given key packages in one commit.
    /// Returns the group and the Welcome for the new members (if any were added).
    pub fn create_group(&self, group_id: &str, policy: Option<&Policy>, key_packages: &[Vec<u8>]) -> Result<(MlsGroup, Option<Vec<u8>>)> {
        let config = create_config(policy)?;
        let mut group = MlsGroup::new_with_group_id(
            &self.provider,
            &self.signer,
            &config,
            GroupId::from_slice(group_id.as_bytes()),
            self.credential.clone(),
        )
        .map_err(|e| anyhow!("creating group: {e:?}"))?;
        if key_packages.is_empty() {
            return Ok((group, None));
        }
        let (_commit, welcome) = self.add(&mut group, key_packages)?;
        Ok((group, Some(welcome)))
    }

    /// Adds members; returns the commit for the existing members and the Welcome for the new.
    pub fn add(&self, group: &mut MlsGroup, key_packages: &[Vec<u8>]) -> Result<(Vec<u8>, Vec<u8>)> {
        let packages = key_packages.iter().map(|kp| self.validate_key_package(kp)).collect::<Result<Vec<_>>>()?;
        let (commit, welcome, _) = group.add_members(&self.provider, &self.signer, &packages).map_err(|e| anyhow!("adding members: {e:?}"))?;
        group.merge_pending_commit(&self.provider).map_err(|e| anyhow!("merging commit: {e:?}"))?;
        Ok((commit.tls_serialize_detached()?, welcome.tls_serialize_detached()?))
    }

    /// An empty commit: a fresh path and a new epoch.
    pub fn update(&self, group: &mut MlsGroup) -> Result<Vec<u8>> {
        let bundle = group
            .self_update(&self.provider, &self.signer, LeafNodeParameters::default())
            .map_err(|e| anyhow!("self update: {e:?}"))?;
        group.merge_pending_commit(&self.provider).map_err(|e| anyhow!("merging commit: {e:?}"))?;
        Ok(bundle.commit().tls_serialize_detached()?)
    }

    fn validate_key_package(&self, bytes: &[u8]) -> Result<KeyPackage> {
        let message = MlsMessageIn::tls_deserialize_exact(bytes).context("key package frame")?;
        let MlsMessageBodyIn::KeyPackage(kp) = message.extract() else { bail!("not a key package") };
        kp.validate(self.provider.crypto(), ProtocolVersion::Mls10).map_err(|e| anyhow!("invalid key package: {e:?}"))
    }

    /// Joins from a Welcome addressed to one of this device's key packages.
    pub fn join(&self, welcome: &[u8]) -> Result<MlsGroup> {
        let message = MlsMessageIn::tls_deserialize_exact(welcome).context("welcome frame")?;
        let MlsMessageBodyIn::Welcome(welcome) = message.extract() else { bail!("not a welcome") };
        let staged = StagedWelcome::new_from_welcome(&self.provider, &join_config(), welcome, None).map_err(|e| anyhow!("welcome: {e:?}"))?;
        staged.into_group(&self.provider).map_err(|e| anyhow!("joining: {e:?}"))
    }

    /// Encrypts an application message.
    pub fn send(&self, group: &mut MlsGroup, plaintext: &[u8]) -> Result<Vec<u8>> {
        let message = group.create_message(&self.provider, &self.signer, plaintext).map_err(|e| anyhow!("encrypting: {e:?}"))?;
        Ok(message.tls_serialize_detached()?)
    }

    /// Processes one frame, merging commits. `judge` decides whether a commit may be applied
    /// (the membership policy); a refused commit is an error and the group stays where it was.
    pub fn process(
        &self,
        group: &mut MlsGroup,
        frame: &[u8],
        judge: impl Fn(&MlsGroup, Option<&(String, String)>, &StagedCommit) -> Result<()>,
    ) -> Result<Processed> {
        let message = MlsMessageIn::tls_deserialize_exact(frame).context("frame")?;
        let protocol: ProtocolMessage = message.try_into_protocol_message().map_err(|e| anyhow!("not a protocol message: {e:?}"))?;
        let processed = group.process_message(&self.provider, protocol).map_err(|e| anyhow!("processing: {e:?}"))?;
        let sender = identity_of(processed.credential());
        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => Ok(Processed::Application { plaintext: app.into_bytes(), sender }),
            ProcessedMessageContent::StagedCommitMessage(staged) => {
                judge(group, sender.as_ref(), &staged)?;
                group.merge_staged_commit(&self.provider, *staged).map_err(|e| anyhow!("merging commit: {e:?}"))?;
                Ok(Processed::Commit { sender })
            }
            ProcessedMessageContent::ProposalMessage(proposal) => {
                group.store_pending_proposal(self.provider.storage(), *proposal).map_err(|e| anyhow!("storing proposal: {e:?}"))?;
                Ok(Processed::Proposal)
            }
            ProcessedMessageContent::ExternalJoinProposalMessage(proposal) => {
                group.store_pending_proposal(self.provider.storage(), *proposal).map_err(|e| anyhow!("storing proposal: {e:?}"))?;
                Ok(Processed::Proposal)
            }
            ProcessedMessageContent::OwnPendingCommit | ProcessedMessageContent::OwnPrivateMessage => Ok(Processed::Own),
        }
    }

    /// A GroupInfo with the ratchet tree and external-init key, for devices joining later.
    pub fn group_info(&self, group: &MlsGroup) -> Result<Vec<u8>> {
        let info = group.export_group_info(self.provider.crypto(), &self.signer, true).map_err(|e| anyhow!("group info: {e:?}"))?;
        Ok(info.tls_serialize_detached()?)
    }

    /// Commits this device into a group from its GroupInfo (the new-device path). Returns the
    /// group and the external commit to send.
    pub fn join_external(&self, group_info: &[u8]) -> Result<(MlsGroup, Vec<u8>)> {
        let message = MlsMessageIn::tls_deserialize_exact(group_info).context("group info frame")?;
        let MlsMessageBodyIn::GroupInfo(info) = message.extract() else { bail!("not a group info") };
        let leaf = LeafNodeParameters::builder().with_capabilities(capabilities()).build();
        let (mut group, bundle) = MlsGroup::external_commit_builder()
            .with_config(join_config())
            .build_group(&self.provider, info, self.credential.clone())
            .map_err(|e| anyhow!("external commit: {e:?}"))?
            .leaf_node_parameters(leaf)
            .load_psks(self.provider.storage())
            .map_err(|e| anyhow!("psks: {e:?}"))?
            .build(self.provider.rand(), self.provider.crypto(), &self.signer, |_| true)
            .map_err(|e| anyhow!("external commit: {e:?}"))?
            .finalize(&self.provider)
            .map_err(|e| anyhow!("external commit: {e:?}"))?;
        group.merge_pending_commit(&self.provider).map_err(|e| anyhow!("merging external commit: {e:?}"))?;
        let (commit, _, _) = bundle.into_contents();
        Ok((group, commit.tls_serialize_detached()?))
    }

    /// RFC 9420 exporter, as `mlsExporter(exporterSecret, label, context, length)` in ts-mls.
    pub fn export(&self, group: &MlsGroup, label: &str, context: &[u8], length: usize) -> Result<Vec<u8>> {
        group.export_secret(self.provider.crypto(), label, context, length).map_err(|e| anyhow!("exporter: {e:?}"))
    }
}

/// `judgeCommit`: whether a commit keeps to the group's membership policy. Adding a further
/// device of someone already in the group, removing yourself and groups without a policy are
/// always fine; everything else needs the right the policy names.
pub fn judge_commit(group: &MlsGroup, committer: Option<&(String, String)>, staged: &StagedCommit) -> Result<()> {
    let policy = policy_of(group).map_err(|_| anyhow!("this group's membership policy does not parse"))?;
    let who = committer.map(|(u, _)| u.as_str());
    let present: std::collections::HashSet<String> = group.members().filter_map(|m| identity_of(&m.credential)).map(|(u, _)| u).collect();
    let allowed = |rule: policy::Rule| policy.as_ref().is_none_or(|p| who.is_some_and(|w| p.allows(rule, w)));
    let committer_name = who.unwrap_or("somebody outside the group");
    for add in staged.add_proposals() {
        let added = identity_of(add.add_proposal().key_package().leaf_node().credential())
            .map(|(u, _)| u)
            .ok_or_else(|| anyhow!("an add that names nobody"))?;
        if !present.contains(&added) && !policy.as_ref().is_none_or(|p| allowed(p.add)) {
            bail!("{committer_name} may not add people to this group");
        }
    }
    // An external commit may take out only the joiner's own earlier leaf (a profile that lost its
    // group state rejoining under the same key), never anyone else's.
    let external = staged.queued_proposals().any(|p| matches!(p.proposal(), Proposal::ExternalInit(_)));
    if external {
        let joiner = staged.update_path_leaf_node().map(|leaf| leaf.signature_key().as_slice().to_vec());
        for remove in staged.remove_proposals() {
            let index = remove.remove_proposal().removed();
            let key = group.members().find(|m| m.index == index).map(|m| m.signature_key);
            if key.is_none() || key != joiner {
                bail!("{committer_name} joined from outside and tried to remove someone else's device");
            }
        }
    }
    for remove in staged.remove_proposals() {
        let removed = group.member(remove.remove_proposal().removed()).and_then(identity_of).map(|(u, _)| u);
        if removed.as_deref() != who && !policy.as_ref().is_none_or(|p| allowed(p.remove)) {
            bail!("{committer_name} may not remove people from this group");
        }
    }
    let next = policy_in(staged.group_context().extensions()).map_err(|_| anyhow!("a membership policy that does not parse"))?;
    match (&policy, &next) {
        (None, Some(n)) if Some(n.creator.as_str()) != who => bail!("a membership policy must name the member adopting it as creator"),
        (None, _) => {}
        (Some(_), None) => bail!("a group's membership policy cannot be taken away"),
        (Some(p), Some(n)) if n.creator != p.creator => bail!("the creator of a group cannot be changed"),
        (Some(p), Some(n)) if n != p && !allowed(p.policy) => bail!("{committer_name} may not change this group's policy"),
        _ => {}
    }
    Ok(())
}

/// Who a published key package says it is: its credential identity and signature key. `None`
/// when it isn't a valid key package of our suite.
pub fn key_package_identity(bytes: &[u8]) -> Option<(String, Vec<u8>)> {
    let message = MlsMessageIn::tls_deserialize_exact(bytes).ok()?;
    let MlsMessageBodyIn::KeyPackage(kp) = message.extract() else { return None };
    let crypto = openmls_rust_crypto::RustCrypto::default();
    let kp = kp.validate(&crypto, ProtocolVersion::Mls10).ok()?;
    if kp.ciphersuite() != CIPHERSUITE {
        return None;
    }
    let leaf = kp.leaf_node();
    let identity = BasicCredential::try_from(leaf.credential().clone()).ok()?;
    Some((String::from_utf8(identity.identity().to_vec()).ok()?, leaf.signature_key().as_slice().to_vec()))
}

/// A fresh Ed25519 MLS signature key: (private, public).
pub fn new_signature_key() -> Result<(Vec<u8>, Vec<u8>)> {
    // Ed25519: the 32-byte seed is the private half, as openmls' Rust crypto provider keeps it.
    let seed: [u8; 32] = vault::rand_bytes();
    let public = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key().to_bytes();
    Ok((seed.to_vec(), public.to_vec()))
}

/// A signature key kept from before, as `new_signature_key` made it.
pub fn signature_key(private: &[u8], public: &[u8]) -> SignatureKeyPair {
    SignatureKeyPair::from_raw(CIPHERSUITE.signature_algorithm(), private.to_vec(), public.to_vec())
}

/// Everyone in a group, as `"{user_id}|{device_id}"`, sorted.
pub fn members(group: &MlsGroup) -> Vec<String> {
    let mut out: Vec<String> = group
        .members()
        .filter_map(|m| BasicCredential::try_from(m.credential).ok())
        .filter_map(|c| String::from_utf8(c.identity().to_vec()).ok())
        .collect();
    out.sort();
    out
}
