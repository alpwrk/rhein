//! The encrypted-DM engine: one MLS group per conversation, driven the way the web client's
//! `Mls` class drives its groups (`web/src/lib/mls.js`), so both clients take part in the same
//! conversations.
//!
//! - Frames are processed strictly in `seq` order, and a frame this device should be able to
//!   open but can't stops the walk rather than leaving a silent hole.
//! - A sender can't decrypt its own application messages, so our own frames are stepped over
//!   and their plaintext is kept when sending.
//! - Every epoch this device reaches, it publishes a GroupInfo sealed under the account's
//!   recovery keyring, so a later device of the account can commit itself in (`self_join`).
//! - Decrypted history is kept on this device: MLS deletes each key as it goes, so a message
//!   can be opened exactly once.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail};
use openmls::prelude::{GroupId, MlsGroup, OpenMlsProvider as _};
use openmls_rust_crypto::OpenMlsRustCrypto;
use rhein_mls::payload::{Body, Payload};
use rhein_mls::trust::{self, Pin};
use rhein_mls::vault::{self, Keyring};
use rhein_mls::{Device, Policy, Processed};
use rhein_net::Api;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Spare key packages kept on the home server.
pub const STOCK: u64 = 12;
const PAGE: u32 = 500;
const MAX_PAGES: usize = 40;

/// One decrypted message as this device keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DmMessage {
    pub id: String,
    pub seq: u64,
    pub sender: String,
    pub sender_device: String,
    pub created_at: i64,
    pub body: Body,
    #[serde(default)]
    pub edited: Option<String>,
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConversationState {
    /// The last frame processed.
    pub last_seq: u64,
    /// The newest epoch we published a GroupInfo for.
    pub published_epoch: u64,
    /// We asked to be let back in and haven't been yet.
    pub rejoin_requested: bool,
    pub messages: Vec<DmMessage>,
    /// frame id → emoji → users.
    pub reactions: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    /// frame id → (by, author, snippet).
    pub pins: BTreeMap<String, (String, String, String)>,
    pub call_sfu: Option<String>,
    /// The group's name, when someone the policy lets gave it one (`RENAME`).
    pub name: Option<String>,
    /// Who renamed the group and to what, newest last, at most `RENAME_LOG_MAX`.
    pub renames: Vec<Rename>,
    /// Why the walk stopped, if it did.
    pub error: Option<String>,
    /// How far each other member has read (their read receipts, kept only while ours are on).
    pub read_by: BTreeMap<String, String>,
    /// The last message our own read receipt named.
    pub sent_read: Option<String>,
}

/// One rename, as the web client's history mirror keeps it (`{id, by, name, at}`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rename {
    pub id: String,
    pub by: String,
    pub name: Option<String>,
    pub at: i64,
}

const RENAME_LOG_MAX: usize = 50;

impl ConversationState {
    /// Our newest message that someone else has read, for the "Seen" under it.
    pub fn seen_upto(&self, me: &str) -> Option<String> {
        let furthest = self.read_by.iter().filter(|(user, _)| *user != me).filter_map(|(_, id)| self.messages.iter().position(|m| m.id == *id)).max()?;
        self.messages[..=furthest].iter().rev().find(|m| m.sender == me && !m.deleted).map(|m| m.id.clone())
    }
}

/// What this account says about its reading in a conversation (the web client's
/// `store/signals.js`): read receipts are off unless turned on, delivery acknowledgements on
/// unless turned off. Each is reciprocal: turned off, the others' are not kept either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signals {
    pub read: bool,
    pub delivered: bool,
}

impl Default for Signals {
    fn default() -> Self {
        Signals { read: false, delivered: true }
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Stored {
    conversations: HashMap<String, ConversationState>,
    /// Device ids this account has used on this machine: frames from them are our own.
    own_devices: HashSet<String>,
    pins: BTreeMap<String, Pin>,
}

fn dir() -> PathBuf {
    crate::persist::path().with_file_name("dm")
}

fn safe(user_id: &str) -> String {
    user_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect()
}

/// Loads a provider's storage from disk. The file holds MLS private keys and group secrets,
/// so it is 0600 like the session.
fn load_provider(path: &std::path::Path) -> OpenMlsRustCrypto {
    let provider = OpenMlsRustCrypto::default();
    if let Some(values) = std::fs::read(path).ok().and_then(|b| serde_json::from_slice::<Vec<(String, String)>>(&b).ok()) {
        let mut map = provider.storage().values.write().unwrap();
        for (k, v) in values {
            if let (Ok(k), Ok(v)) = (vault::unb64(&k), vault::unb64(&v)) {
                map.insert(k, v);
            }
        }
    }
    provider
}

fn save_provider(provider: &OpenMlsRustCrypto, path: &std::path::Path) -> Result<()> {
    let values: Vec<(String, String)> = provider.storage().values.read().unwrap().iter().map(|(k, v)| (vault::b64(k), vault::b64(v))).collect();
    crate::persist::write_private(path, &serde_json::to_vec(&values)?)
}

pub struct Engine {
    pub user_id: String,
    pub device_id: String,
    device: Device,
    keyring: Option<Keyring>,
    stored: Stored,
    groups: HashMap<String, MlsGroup>,
    /// Conversations whose history mirror was read this session.
    mirrored: HashSet<String>,
    /// For conversations whose group this device is not in, the frames already looked through
    /// this session: what is in them did not let it in, so the next sync starts after them.
    scanned: HashMap<String, u64>,
    /// Mirror rows waiting to be sealed and stored: (conversation, item, value).
    mirror_out: Vec<(String, String, Value)>,
    provider_path: PathBuf,
    state_path: PathBuf,
    /// The account's receipt choices for the conversation being worked on (set before each use).
    pub signals: Signals,
}

/// Who could not be added when building or growing a group.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Claimed {
    /// Devices with no key package published (they have to sign in once).
    pub missing: Vec<String>,
    /// Devices whose package failed the checks (not vouched for by their identity, or mislabelled).
    pub refused: Vec<String>,
}

/// The result of a sync, for the UI.
#[derive(Debug, Default)]
pub struct Synced {
    pub opened: usize,
    /// Devices that asked to be let back in and were added.
    pub readmitted: usize,
}

impl Engine {
    /// Opens the engine for an account on this device. `signature` is this account's MLS key
    /// (private, public) from `dm::Secrets`.
    pub fn open(user_id: &str, device_id: &str, signature: (&[u8], &[u8]), keyring: Option<Keyring>) -> Result<Engine> {
        std::fs::create_dir_all(dir()).context("dm dir")?;
        let provider_path = dir().join(format!("{}.mls", safe(user_id)));
        let state_path = dir().join(format!("{}.history.json", safe(user_id)));
        let provider = load_provider(&provider_path);
        let signer = rhein_mls::signature_key(signature.0, signature.1);
        let device = Device::with_signer(provider, signer, &format!("{user_id}|{device_id}"))?;
        let mut stored: Stored = std::fs::read(&state_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        stored.own_devices.insert(device_id.to_owned());
        Ok(Engine {
            user_id: user_id.into(),
            device_id: device_id.into(),
            device,
            keyring,
            stored,
            groups: HashMap::new(),
            mirrored: HashSet::new(),
            scanned: HashMap::new(),
            mirror_out: Vec::new(),
            provider_path,
            state_path,
            signals: Signals::default(),
        })
    }

    /// Forgets everything this device kept for an account.
    pub fn remove(user_id: &str) {
        let _ = std::fs::remove_file(dir().join(format!("{}.mls", safe(user_id))));
        let _ = std::fs::remove_file(dir().join(format!("{}.history.json", safe(user_id))));
    }

    fn save(&self) -> Result<()> {
        save_provider(&self.device.provider, &self.provider_path)?;
        crate::persist::write_private(&self.state_path, &serde_json::to_vec(&self.stored)?)
    }

    /// Every group name this device knows (`RENAME`), for the conversation list.
    pub fn group_names(&self) -> Vec<(String, String)> {
        self.stored.conversations.iter().filter_map(|(id, c)| Some((id.clone(), c.name.clone()?))).collect()
    }

    pub fn conversation(&self, id: &str) -> Option<&ConversationState> {
        self.stored.conversations.get(id)
    }

    pub fn has_group(&mut self, id: &str) -> bool {
        self.group(id).is_some()
    }

    /// A call secret of a conversation's current epoch (`MLS-Exporter(label, conversation id,
    /// 32)`) and that epoch: the media key of an encrypted SFU call, or the hello key.
    pub fn call_secret(&mut self, conversation: &str, label: &str) -> Option<(Vec<u8>, u64)> {
        self.group(conversation)?;
        let group = self.groups.get(conversation)?;
        let key = self.device.export(group, label, conversation.as_bytes(), 32).ok()?;
        Some((key, group.epoch().as_u64()))
    }

    fn group(&mut self, id: &str) -> Option<&mut MlsGroup> {
        if !self.groups.contains_key(id) {
            let group = MlsGroup::load(self.device.provider.storage(), &GroupId::from_slice(id.as_bytes())).ok().flatten()?;
            self.groups.insert(id.to_owned(), group);
        }
        self.groups.get_mut(id)
    }

    fn is_mine(&self, frame: &Value) -> bool {
        frame["sender"] == self.user_id.as_str() && frame["sender_device"].as_str().is_some_and(|d| self.stored.own_devices.contains(d))
    }

    // ------------------------------------------------------------------ key packages

    /// Tops the server's stock of this device's key packages back up to [`STOCK`].
    pub async fn replenish(&mut self, api: &Api) -> Result<usize> {
        let status = api.key_packages().await?;
        let have = status["devices"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|d| d["device_id"] == self.device_id.as_str())
            .and_then(|d| d["available"].as_u64())
            .unwrap_or(0);
        if have >= STOCK {
            return Ok(0);
        }
        let fresh: Vec<String> = (have..STOCK).map(|_| self.device.key_package().map(|kp| vault::b64(&kp))).collect::<Result<_>>()?;
        api.publish_key_packages(&fresh).await?;
        self.save()?;
        Ok(fresh.len())
    }

    /// One key package per device of each user, checked the way the web client checks them:
    /// the credential inside the signed package must name the device the server labelled it
    /// with, and an account with a cross-signing identity must vouch for the device's key.
    async fn claim(&mut self, api: &Api, users: &[String]) -> Result<(Vec<Vec<u8>>, Claimed)> {
        let mut packages = Vec::new();
        let mut claimed = Claimed::default();
        let mut taken: HashSet<Vec<u8>> = HashSet::from([self.device.signature_public_key().to_vec()]);
        for user in users.iter().collect::<std::collections::BTreeSet<_>>() {
            let answer = api.claim_key_packages(user).await?;
            for entry in answer["key_packages"].as_array().into_iter().flatten() {
                let device = entry["device_id"].as_str().unwrap_or_default();
                if *user == self.user_id && device == self.device_id {
                    continue;
                }
                let Some(Ok(bytes)) = entry["package"].as_str().map(vault::unb64) else {
                    claimed.missing.push(format!("{user} ({device})"));
                    continue;
                };
                let Some((identity, key)) = rhein_mls::key_package_identity(&bytes) else {
                    claimed.refused.push(format!("{user} ({device})"));
                    continue;
                };
                if identity != format!("{user}|{device}") || !taken.insert(key.clone()) {
                    continue;
                }
                if self.verify_claimed(api, user, device, &vault::b64(&key)).await == Some(false) {
                    claimed.refused.push(format!("{user} ({device})"));
                    continue;
                }
                packages.push(bytes);
            }
        }
        Ok((packages, claimed))
    }

    /// `verifyClaimedPackage`: `Some(true/false)` for accounts with a cross-signing identity,
    /// `None` for accounts that have none (accepted as they are).
    pub async fn verify_claimed(&mut self, api: &Api, user: &str, device: &str, key_b64: &str) -> Option<bool> {
        let list = api.device_list(user).await.ok()?;
        let identity_key = list["identity_key"].as_str()?.to_owned();
        let mut chain = None;
        if let Ok(doc) = api.identity_document(user).await {
            match trust::verify_identity_document(&doc, Some(user)) {
                Ok(Some(c)) => match trust::check_pin(self.stored.pins.get(user), &c) {
                    Ok(pin) if c.current_key == identity_key => {
                        self.stored.pins.insert(user.to_owned(), pin);
                        chain = Some(c);
                    }
                    _ => return Some(false),
                },
                Ok(None) if self.stored.pins.contains_key(user) => return Some(false),
                Ok(None) => {}
                Err(_) => return Some(false),
            }
        }
        // A key the account's root revoked is never trusted again: no KeyPackage, no rejoin.
        let revoked = trust::revoked_keys(user, chain.as_ref(), Some(&identity_key), &list["revocations"]);
        if revoked.contains(key_b64) {
            log::warn!("{user}'s device {device} uses a revoked key");
            return Some(false);
        }
        let entry = list["devices"].as_array()?.iter().find(|d| d["device_id"] == device)?;
        Some(entry["mls_key"] == key_b64 && trust::device_is_vouched(user, &identity_key, entry))
    }

    // ------------------------------------------------------------------ groups

    /// Builds the group for a new conversation and posts the Welcome for the other devices.
    pub async fn establish(&mut self, api: &Api, conversation: &str, members: &[String], policy: Option<&Policy>) -> Result<Claimed> {
        let (packages, claimed) = self.claim(api, members).await?;
        let (group, welcome) = self.device.create_group(conversation, policy, &packages)?;
        let epoch = group.epoch().as_u64();
        self.groups.insert(conversation.to_owned(), group);
        let state = self.stored.conversations.entry(conversation.to_owned()).or_default();
        state.error = None;
        if let Some(welcome) = welcome {
            let sent = api.send_frame(conversation, &json!({ "kind": "welcome", "ciphertext": vault::b64(&welcome), "epoch": epoch })).await?;
            state.last_seq = state.last_seq.max(sent["seq"].as_u64().unwrap_or(0));
        }
        self.publish_group_info(api, conversation).await;
        self.save()?;
        Ok(claimed)
    }

    /// Publishes a GroupInfo for the group's current epoch, sealed to this account, so the next
    /// device of the account can let itself in.
    async fn publish_group_info(&mut self, api: &Api, conversation: &str) {
        let Some(keyring) = self.keyring.clone() else { return };
        let Some(group) = self.group(conversation) else { return };
        let epoch = group.epoch().as_u64();
        if self.stored.conversations.get(conversation).is_some_and(|c| c.published_epoch >= epoch) {
            return;
        }
        let Ok(info) = self.device.group_info(self.groups.get(conversation).expect("loaded above")) else { return };
        let frame = json!({ "kind": "groupinfo", "ciphertext": vault::seal(&keyring.key, &info), "epoch": epoch });
        match api.send_frame(conversation, &frame).await {
            Ok(_) => self.stored.conversations.entry(conversation.to_owned()).or_default().published_epoch = epoch,
            Err(err) => log::warn!("publishing group info for {conversation}: {err}"),
        }
    }

    /// The new-device path: commit ourselves in from the newest GroupInfo this account sealed.
    async fn self_join(&mut self, api: &Api, conversation: &str, frames: &[Value]) -> Result<bool> {
        let Some(keyring) = self.keyring.clone() else { return Ok(false) };
        // Only a GroupInfo of the current generation: one from before a start-over would fork the
        // group (`newestGroupInfo` in `mls.js`, Telium #66).
        let generation = newest_generation(frames);
        let mut ours: Vec<&Value> = frames
            .iter()
            .filter(|f| f["kind"] == "groupinfo" && f["sender"] == self.user_id.as_str() && of_generation(f, generation))
            .collect();
        ours.sort_by_key(|f| std::cmp::Reverse(f["seq"].as_u64().unwrap_or(0)));
        for frame in ours {
            let Ok(info) = keyring.open(frame["ciphertext"].as_str().unwrap_or_default()) else { continue };
            let Ok((group, commit)) = self.device.join_external(&info) else { continue };
            let epoch = group.epoch().as_u64();
            api.send_frame(conversation, &json!({ "kind": "commit", "ciphertext": vault::b64(&commit), "epoch": epoch })).await?;
            self.groups.insert(conversation.to_owned(), group);
            let through = frames.iter().filter_map(|f| f["seq"].as_u64()).max().unwrap_or(0);
            let state = self.stored.conversations.entry(conversation.to_owned()).or_default();
            state.last_seq = state.last_seq.max(through);
            state.rejoin_requested = false;
            state.error = None;
            self.publish_group_info(api, conversation).await;
            self.save()?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Commits other devices of an account back in, when they asked and are vouched for.
    async fn readmit(&mut self, api: &Api, conversation: &str, requests: &[(String, String)]) -> Result<usize> {
        if requests.is_empty() || self.group(conversation).is_none() {
            return Ok(0);
        }
        let present: HashSet<String> = rhein_mls::members(self.groups.get(conversation).expect("loaded")).into_iter().collect();
        let absent: Vec<String> =
            requests.iter().filter(|(u, d)| !present.contains(&format!("{u}|{d}"))).map(|(u, _)| u.clone()).collect::<HashSet<_>>().into_iter().collect();
        if absent.is_empty() {
            return Ok(0);
        }
        // Only people already in the group come back this way; rejoin never adds anyone new.
        let in_group: HashSet<String> = present.iter().filter_map(|p| p.split_once('|').map(|(u, _)| u.to_owned())).collect();
        let users: Vec<String> = absent.into_iter().filter(|u| in_group.contains(u)).collect();
        let (packages, _) = self.claim(api, &users).await?;
        let wanted: Vec<Vec<u8>> = packages
            .into_iter()
            .filter(|kp| rhein_mls::key_package_identity(kp).is_some_and(|(id, _)| requests.iter().any(|(u, d)| id == format!("{u}|{d}"))))
            .collect();
        if wanted.is_empty() {
            return Ok(0);
        }
        let group = self.groups.get_mut(conversation).expect("loaded");
        let (commit, welcome) = self.device.add(group, &wanted)?;
        let epoch = group.epoch().as_u64();
        let sent = api.send_frame(conversation, &json!({ "kind": "commit", "ciphertext": vault::b64(&commit), "epoch": epoch })).await;
        if let Err(err) = sent {
            // Somebody else readmitted them first (409): drop our copy and resync.
            self.groups.remove(conversation);
            return Err(anyhow!("readmitting: {err}"));
        }
        api.send_frame(conversation, &json!({ "kind": "welcome", "ciphertext": vault::b64(&welcome), "epoch": epoch })).await?;
        Ok(wanted.len())
    }

    // ------------------------------------------------------------------ history mirror
    //
    // The web client's `Backup`: what a device opened, sealed under the account's recovery
    // keyring and stored on the home server, so the account's next device (or the web client)
    // can show history from before it joined. One row per frame id holding its body (1 for a
    // signal, null for a deleted message), plus `rx` (reactions) and `cx` (edits, deletions,
    // pins, the call SFU) per conversation.

    fn queue_mirror(&mut self, conversation: &str, item: &str, value: Value) {
        if self.keyring.is_some() {
            self.mirror_out.push((conversation.to_owned(), item.to_owned(), value));
        }
    }

    /// Queues the fold rows for a conversation from its current state.
    fn queue_folds(&mut self, conversation: &str) {
        let Some(state) = self.stored.conversations.get(conversation) else { return };
        let mut rx = Vec::new();
        for (target, by_emoji) in &state.reactions {
            let mut per_user: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
            for (emoji, users) in by_emoji {
                for u in users {
                    per_user.entry(u).or_default().push(emoji);
                }
            }
            for (u, e) in per_user {
                rx.push(json!({ "t": target, "u": u, "e": e }));
            }
        }
        let cx = json!({
            "edits": state.messages.iter().filter_map(|m| Some(json!({ "t": m.id, "x": m.edited.as_ref()?, "by": m.sender }))).collect::<Vec<_>>(),
            "dead": state.messages.iter().filter(|m| m.deleted).map(|m| m.id.clone()).collect::<Vec<_>>(),
            "pins": state.pins.iter().map(|(t, (by, author, snippet))| json!({ "t": t, "by": by, "author": author, "snippet": snippet })).collect::<Vec<_>>(),
            "sfu": state.call_sfu,
            "name": state.name,
            "renames": state.renames,
        });
        self.queue_mirror(conversation, "rx", Value::Array(rx));
        self.queue_mirror(conversation, "cx", cx);
    }

    async fn flush_mirror(&mut self, api: &Api) {
        let Some(keyring) = self.keyring.clone() else { return };
        if self.mirror_out.is_empty() {
            return;
        }
        let rows: Vec<Value> = self
            .mirror_out
            .iter()
            .map(|(scope, item, value)| json!({ "scope": scope, "item": item, "ciphertext": vault::seal(&keyring.key, value.to_string().as_bytes()) }))
            .collect();
        for chunk in rows.chunks(512) {
            if let Err(err) = api.put_backup(chunk).await {
                log::warn!("history mirror: {err}");
                return;
            }
        }
        self.mirror_out.clear();
    }

    /// Reads a conversation's mirror and fills in what this device never opened itself.
    async fn pull_mirror(&mut self, api: &Api, conversation: &str, frames: &[Value]) -> Result<()> {
        let Some(keyring) = self.keyring.clone() else { return Ok(()) };
        if !self.mirrored.insert(conversation.to_owned()) {
            return Ok(());
        }
        let mut rows: HashMap<String, Value> = HashMap::new();
        let mut after: Option<String> = None;
        loop {
            let page = api.backup_page(conversation, after.as_deref()).await?;
            let items = page["items"].as_array().cloned().unwrap_or_default();
            for row in &items {
                let Ok(bytes) = keyring.open(row["ciphertext"].as_str().unwrap_or_default()) else { continue };
                let Ok(value) = serde_json::from_slice::<Value>(&bytes) else { continue };
                rows.insert(row["item"].as_str().unwrap_or_default().to_owned(), value);
            }
            if page["complete"].as_bool().unwrap_or(true) || items.is_empty() {
                break;
            }
            after = page["cursor"].as_str().map(str::to_owned);
            if after.is_none() {
                break;
            }
        }
        if rows.is_empty() {
            return Ok(());
        }
        let state = self.stored.conversations.entry(conversation.to_owned()).or_default();
        let known: HashSet<String> = state.messages.iter().map(|m| m.id.clone()).collect();
        for frame in frames.iter().filter(|f| f["kind"] == "application") {
            let id = frame["id"].as_str().unwrap_or_default();
            if known.contains(id) {
                continue;
            }
            if let Some(Value::String(raw)) = rows.get(id) {
                state.messages.push(DmMessage {
                    id: id.to_owned(),
                    seq: frame["seq"].as_u64().unwrap_or(0),
                    sender: frame["sender"].as_str().unwrap_or_default().to_owned(),
                    sender_device: frame["sender_device"].as_str().unwrap_or_default().to_owned(),
                    created_at: frame["created_at"].as_i64().unwrap_or(0),
                    body: Body::decode(raw),
                    edited: None,
                    deleted: false,
                });
            }
        }
        state.messages.sort_by_key(|m| m.seq);
        if let Some(Value::Array(rx)) = rows.get("rx") {
            for row in rx {
                let (Some(t), Some(u)) = (row["t"].as_str(), row["u"].as_str()) else { continue };
                for e in row["e"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                    let users = state.reactions.entry(t.to_owned()).or_default().entry(e.to_owned()).or_default();
                    if !users.iter().any(|x| x == u) {
                        users.push(u.to_owned());
                    }
                }
            }
        }
        if let Some(cx) = rows.get("cx") {
            for e in cx["edits"].as_array().into_iter().flatten() {
                if let (Some(t), Some(x)) = (e["t"].as_str(), e["x"].as_str())
                    && let Some(m) = state.messages.iter_mut().find(|m| m.id == t)
                    && m.edited.is_none()
                {
                    m.edited = Some(x.to_owned());
                }
            }
            for dead in cx["dead"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                if let Some(m) = state.messages.iter_mut().find(|m| m.id == dead) {
                    m.deleted = true;
                    m.body = Body::default();
                }
            }
            for p in cx["pins"].as_array().into_iter().flatten() {
                if let Some(t) = p["t"].as_str() {
                    let s = |k: &str| p[k].as_str().unwrap_or_default().to_owned();
                    state.pins.entry(t.to_owned()).or_insert((s("by"), s("author"), s("snippet")));
                }
            }
            if state.call_sfu.is_none() {
                state.call_sfu = cx["sfu"].as_str().map(str::to_owned);
            }
            if state.name.is_none() && state.renames.is_empty() {
                state.name = cx["name"].as_str().filter(|n| rhein_mls::payload::valid_group_name(n)).map(str::to_owned);
                state.renames = cx["renames"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|r| serde_json::from_value::<Rename>(r.clone()).ok())
                    .filter(|r| !r.id.is_empty() && !r.by.is_empty() && r.name.as_deref().is_none_or(rhein_mls::payload::valid_group_name))
                    .collect();
                let excess = state.renames.len().saturating_sub(RENAME_LOG_MAX);
                state.renames.drain(..excess);
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ frames

    async fn frames_after(api: &Api, conversation: &str, after: u64) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut through = after;
        for _ in 0..MAX_PAGES {
            let page = api.dm_frames(conversation, Some(through), PAGE).await?;
            let frames = page["messages"].as_array().cloned().unwrap_or_default();
            let n = frames.len();
            through = frames.iter().filter_map(|f| f["seq"].as_u64()).max().unwrap_or(through);
            out.extend(frames);
            // A member home still filling a gap from the conversation's sequencer answers up to
            // the gap; the rest comes on a later sync.
            if page["incomplete"].as_bool() == Some(true) {
                log::info!("{conversation}: frames from {} not here yet", page["missing_from"]);
                break;
            }
            if n < PAGE as usize {
                break;
            }
        }
        out.sort_by_key(|f| f["seq"].as_u64().unwrap_or(0));
        Ok(out)
    }

    /// Catches a conversation up: joins by Welcome or self-join if we aren't in its group yet,
    /// processes every new frame in order, readmits our other devices that asked, and publishes
    /// our GroupInfo for the epoch we reached.
    pub async fn sync(&mut self, api: &Api, conversation: &str) -> Result<Synced> {
        let last_seq = self.stored.conversations.get(conversation).map_or(0, |c| c.last_seq);
        // Not in the group, nothing advances `last_seq`: without this, every sync (every time
        // the conversation is opened) would fetch all of its frames again.
        let outside = self.group(conversation).is_none();
        let looked_through = outside && self.scanned.contains_key(conversation);
        let start = if outside { last_seq.max(self.scanned.get(conversation).copied().unwrap_or(0)) } else { last_seq };
        let frames = Self::frames_after(api, conversation, start).await?;
        if outside && let Some(through) = frames.iter().filter_map(|f| f["seq"].as_u64()).max() {
            self.scanned.insert(conversation.to_owned(), through);
        }
        // The mirror needs the keyring; locked, every sync fetched all frames for nothing.
        if self.keyring.is_some() && !self.mirrored.contains(conversation) {
            let everything = if start == 0 { frames.clone() } else { Self::frames_after(api, conversation, 0).await? };
            if let Err(err) = self.pull_mirror(api, conversation, &everything).await {
                log::info!("history mirror for {conversation}: {err:#}");
            }
        }
        let mut synced = Synced::default();
        let mut rejoins: Vec<(String, String)> = Vec::new();
        // The newest message from someone else we opened: acknowledged as delivered afterwards.
        let mut delivered: Option<(String, u64)> = None;

        for frame in &frames {
            let seq = frame["seq"].as_u64().unwrap_or(0);
            if seq <= self.stored.conversations.get(conversation).map_or(0, |c| c.last_seq) {
                continue;
            }
            let mine = self.is_mine(frame);
            let kind = frame["kind"].as_str().unwrap_or_default();
            let advance = |this: &mut Self| this.stored.conversations.entry(conversation.to_owned()).or_default().last_seq = seq;
            match kind {
                "groupinfo" => {
                    if self.group(conversation).is_some() {
                        advance(self);
                    }
                    continue;
                }
                "rejoin" => {
                    if !mine && let (Some(u), Some(d)) = (frame["sender"].as_str(), frame["sender_device"].as_str()) {
                        rejoins.push((u.to_owned(), d.to_owned()));
                    }
                    if self.group(conversation).is_some() {
                        advance(self);
                    }
                    continue;
                }
                "welcome" => {
                    if !mine
                        && let Ok(bytes) = vault::unb64(frame["ciphertext"].as_str().unwrap_or_default())
                        && let Ok(group) = self.device.join(&bytes)
                    {
                        self.groups.insert(conversation.to_owned(), group);
                        let state = self.stored.conversations.entry(conversation.to_owned()).or_default();
                        state.last_seq = seq;
                        state.error = None;
                        state.rejoin_requested = false;
                    }
                    continue;
                }
                _ => {}
            }
            if self.group(conversation).is_none() || mine {
                if self.group(conversation).is_some() {
                    advance(self);
                }
                continue;
            }
            let Ok(bytes) = vault::unb64(frame["ciphertext"].as_str().unwrap_or_default()) else {
                advance(self);
                continue;
            };
            let group = self.groups.get_mut(conversation).expect("loaded above");
            let ours = group.epoch().as_u64();
            match self.device.process(group, &bytes, rhein_mls::judge_commit) {
                Ok(Processed::Application { plaintext, sender }) => {
                    let (user, device) = sender.unwrap_or_default();
                    if self.absorb(conversation, frame, &user, &device, Payload::decode(&plaintext)) {
                        synced.opened += 1;
                        if user != self.user_id {
                            delivered = Some((frame["id"].as_str().unwrap_or_default().to_owned(), seq));
                        }
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    // A frame written at an epoch we have already moved past (it raced our
                    // join) is stepped over; anything else stops the walk.
                    let theirs = frame["epoch"].as_u64().unwrap_or(0);
                    if theirs > 0 && theirs < ours {
                        advance(self);
                        continue;
                    }
                    log::warn!("{conversation} stopped at seq {seq}: {err:#}");
                    self.stored.conversations.entry(conversation.to_owned()).or_default().error = Some(format!("{err:#}"));
                    break;
                }
            }
            advance(self);
        }

        if self.group(conversation).is_none() {
            // Not in the group: let ourselves in from our own GroupInfo, or ask to be let in. Only
            // frames not looked through this session can hold a new one, and without the keyring
            // none can be opened.
            let everything = match start {
                _ if self.keyring.is_none() => Vec::new(),
                0 => frames.clone(),
                _ if looked_through => frames.clone(),
                _ => Self::frames_after(api, conversation, 0).await?,
            };
            if !self.self_join(api, conversation, &everything).await? {
                let state = self.stored.conversations.entry(conversation.to_owned()).or_default();
                if !state.rejoin_requested {
                    // Which leaf is ours and how far we walked, so whoever answers can replace a
                    // leaf we hold no state for instead of taking it as already answered.
                    let seen = frames.iter().filter_map(|f| f["seq"].as_u64()).max().unwrap_or(0).max(state.last_seq);
                    let body = rejoin_body(&vault::b64(self.device.signature_public_key()), seen);
                    api.send_frame(conversation, &json!({ "kind": "rejoin", "ciphertext": body, "epoch": 0 })).await?;
                    state.rejoin_requested = true;
                }
            }
        } else {
            synced.readmitted = self.readmit(api, conversation, &rejoins).await.unwrap_or_else(|err| {
                log::info!("{err:#}");
                0
            });
            self.publish_group_info(api, conversation).await;
            // A delivery acknowledgement, as the web client sends one: this device has it.
            if let Some((id, seq)) = delivered.filter(|_| self.signals.delivered) {
                let ack = json!({ "device": self.device_id, "seq": seq, "id": id });
                if let Err(err) = self.send(api, conversation, Payload::Delivered(ack)).await {
                    log::info!("delivery acknowledgement: {err:#}");
                }
            }
        }
        self.flush_mirror(api).await;
        self.save()?;
        Ok(synced)
    }

    /// Tells the conversation we have read everything in it so far (a read receipt, as the web
    /// client sends one), when receipts are on and there is something newer than the last one.
    pub async fn mark_read(&mut self, api: &Api, conversation: &str) -> Result<()> {
        if !self.signals.read || self.group(conversation).is_none() {
            return Ok(());
        }
        let Some(state) = self.stored.conversations.get(conversation) else { return Ok(()) };
        let Some(last) = state.messages.iter().rev().find(|m| !m.deleted).map(|m| m.id.clone()) else { return Ok(()) };
        if state.sent_read.as_deref() == Some(last.as_str()) {
            return Ok(());
        }
        self.send(api, conversation, Payload::Receipt(json!({ "read": last }))).await?;
        self.stored.conversations.entry(conversation.to_owned()).or_default().sent_read = Some(last);
        self.save()?;
        Ok(())
    }

    /// Folds one decrypted payload into the conversation. Returns whether it was a message.
    fn absorb(&mut self, conversation: &str, frame: &Value, sender: &str, device: &str, payload: Payload) -> bool {
        let id = frame["id"].as_str().unwrap_or_default().to_owned();
        let (signals, me) = (self.signals, self.user_id.clone());
        // Renaming is the group policy's `policy` right, as `applyRename` checks it.
        let may_rename = self.may_rename(conversation, sender);
        let state = self.stored.conversations.entry(conversation.to_owned()).or_default();
        let author_of = |state: &ConversationState, target: &str| state.messages.iter().find(|m| m.id == target).map(|m| m.sender.clone());
        match payload {
            Payload::Message(raw) => {
                let row = (conversation.to_owned(), id.clone(), Value::String(raw.clone()));
                state.messages.push(DmMessage {
                    id,
                    seq: frame["seq"].as_u64().unwrap_or(0),
                    sender: sender.to_owned(),
                    sender_device: device.to_owned(),
                    created_at: frame["created_at"].as_i64().unwrap_or(0),
                    body: Body::decode(&raw),
                    edited: None,
                    deleted: false,
                });
                if self.keyring.is_some() {
                    self.mirror_out.push(row);
                }
                return true;
            }
            Payload::Reaction { target, emoji, add } => {
                let users = state.reactions.entry(target).or_default().entry(emoji).or_default();
                users.retain(|u| u != sender);
                if add {
                    users.push(sender.to_owned());
                }
            }
            // Edits and deletes are the author's alone.
            Payload::Edit { target, text } if !text.trim().is_empty() && author_of(state, &target).is_none_or(|a| a == sender) => {
                if let Some(m) = state.messages.iter_mut().find(|m| m.id == target) {
                    m.edited = Some(text);
                }
            }
            Payload::Delete { target } if author_of(state, &target).is_none_or(|a| a == sender) => {
                if let Some(m) = state.messages.iter_mut().find(|m| m.id == target) {
                    m.deleted = true;
                    m.body = Body::default();
                }
                state.reactions.remove(&target);
                state.pins.remove(&target);
                // The body leaves the mirror too.
                if self.keyring.is_some() {
                    self.mirror_out.push((conversation.to_owned(), target, Value::Null));
                }
            }
            Payload::Pin { target, add, author, snippet } => {
                if add {
                    state.pins.insert(target, (sender.to_owned(), author, snippet));
                } else {
                    state.pins.remove(&target);
                }
            }
            Payload::CallSfu { endpoint } => {
                state.call_sfu = endpoint.filter(|e| (e.starts_with("https://") || e.starts_with("http://")) && e.len() <= 512);
            }
            Payload::Rename { name } if may_rename => {
                state.name = name.clone();
                if !id.is_empty() && !state.renames.iter().any(|r| r.id == id) {
                    let at = frame["created_at"].as_i64().unwrap_or(0);
                    state.renames.push(Rename { id: id.clone(), by: sender.to_owned(), name, at });
                    let excess = state.renames.len().saturating_sub(RENAME_LOG_MAX);
                    state.renames.drain(..excess);
                }
            }
            // How far someone has read, kept only while we send our own, and only forward.
            Payload::Receipt(receipt) if signals.read && sender != me => {
                if let Some(read) = receipt["read"].as_str() {
                    let at = |id: &str| state.messages.iter().position(|m| m.id == id);
                    let ahead = match state.read_by.get(sender) {
                        Some(held) => at(read) > at(held),
                        None => true,
                    };
                    if ahead && at(read).is_some() {
                        state.read_by.insert(sender.to_owned(), read.to_owned());
                    }
                }
            }
            _ => {}
        }
        // A signal: the frame is marked as one, and the folds it changed are stored.
        self.queue_mirror(conversation, &id, json!(1));
        self.queue_folds(conversation);
        false
    }

    /// Sends a message with attachments (and optionally a reply snapshot): each file is sealed
    /// under its own key, uploaded as `attachment.bin`, and the key travels in the encrypted body.
    pub async fn send_message(&mut self, api: &Api, conversation: &str, text: &str, files: Vec<(String, String, Vec<u8>)>, reply: Option<Value>) -> Result<String> {
        let mut attachments = Vec::new();
        for (name, mime, bytes) in files {
            let size = bytes.len();
            let (sealed, key, iv) = vault::seal_attachment(&bytes);
            let media = api.upload("attachment.bin", "application/octet-stream", sealed).await?;
            attachments.push(json!({
                "ref": media.hash, "url": media.url, "capability": media.capability,
                "name": name, "mime": mime, "size": size, "expires_at": media.expires_at,
                "key": key, "iv": iv,
            }));
        }
        let body = Body { text: text.to_owned(), attachments, reply };
        self.send(api, conversation, Payload::Message(body.encode())).await
    }

    /// Downloads and opens one attachment from a message body.
    pub async fn fetch_attachment(api: &Api, attachment: &Value) -> Result<Vec<u8>> {
        let url = match attachment["url"].as_str() {
            Some(url) => url.to_owned(),
            None => format!(
                "{}/_telium/v1/media/{}?cap={}",
                api.base(),
                attachment["ref"].as_str().unwrap_or_default(),
                urlencoding::encode(attachment["capability"].as_str().unwrap_or_default())
            ),
        };
        let bytes = api.http().get(url).send().await?.error_for_status()?.bytes().await?;
        vault::open_attachment(&bytes, attachment["key"].as_str().unwrap_or_default(), attachment["iv"].as_str().unwrap_or_default())
    }

    /// Whether `user` may rename the group: anyone in a group without a policy, else whom the
    /// policy's `policy` rule names.
    pub fn may_rename(&self, conversation: &str, user: &str) -> bool {
        let Some(group) = self.groups.get(conversation) else { return false };
        match rhein_mls::policy_of(group) {
            Ok(None) => true,
            Ok(Some(policy)) => policy.allows(policy.policy, user),
            Err(_) => false,
        }
    }

    /// Names the group (`None` or a blank name clears it), for everyone in it.
    pub async fn rename(&mut self, api: &Api, conversation: &str, name: &str) -> Result<String> {
        if !self.may_rename(conversation, &self.user_id.clone()) {
            bail!("This group's rules do not let you rename it.");
        }
        self.send(api, conversation, Payload::Rename { name: rhein_mls::payload::clean_group_name(name) }).await
    }

    /// Encrypts and sends one payload; our own copy is kept right away (we can't decrypt it
    /// later). Returns the frame id.
    pub async fn send(&mut self, api: &Api, conversation: &str, payload: Payload) -> Result<String> {
        let group = self.group(conversation).ok_or_else(|| anyhow!("this device isn't in the conversation's group yet"))?;
        let epoch = group.epoch().as_u64();
        let ciphertext = self.device.send(self.groups.get_mut(conversation).expect("loaded"), &payload.encode())?;
        let notify = matches!(payload, Payload::Message(_));
        let notification_id = vault::b64url(&vault::rand_bytes::<16>());
        let sent = api
            .send_frame(
                conversation,
                &json!({ "kind": "application", "ciphertext": vault::b64(&ciphertext), "epoch": epoch, "notify": notify, "notification_id": notification_id }),
            )
            .await?;
        let frame = json!({ "id": sent["id"], "seq": sent["seq"], "created_at": sent["created_at"] });
        let (user, device) = (self.user_id.clone(), self.device_id.clone());
        self.absorb(conversation, &frame, &user, &device, payload);
        self.flush_mirror(api).await;
        self.save()?;
        Ok(sent["id"].as_str().unwrap_or_default().to_owned())
    }
}

/// The body of a `rejoin` frame (`rejoin.js`): `key:<leaf signature key>`, with `;seen:<seq>` when
/// this device has walked any frames. Older readers ignore the suffix and judge by device id.
pub fn rejoin_body(key_b64: &str, seen: u64) -> String {
    if seen > 0 { format!("key:{key_b64};seen:{seen}") } else { format!("key:{key_b64}") }
}

/// The newest start-over generation the frames carry, if any does.
fn newest_generation(frames: &[Value]) -> Option<u64> {
    frames.iter().filter_map(|f| f["generation"].as_u64()).max()
}

/// Whether a frame belongs to `generation` or later; frames that name none (or 0) always do.
fn of_generation(frame: &Value, generation: Option<u64>) -> bool {
    match (generation, frame["generation"].as_u64()) {
        (Some(current), Some(g)) if g > 0 => g >= current,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejoin_bodies_match_the_web_client() {
        assert_eq!(rejoin_body("AAAA", 0), "key:AAAA");
        assert_eq!(rejoin_body("AAAA", 42), "key:AAAA;seen:42");
    }

    #[test]
    fn group_info_is_taken_from_the_current_generation() {
        let frames = vec![json!({ "generation": 1 }), json!({ "generation": 3 }), json!({})];
        assert_eq!(newest_generation(&frames), Some(3));
        assert!(!of_generation(&json!({ "generation": 2 }), Some(3)));
        assert!(of_generation(&json!({ "generation": 3 }), Some(3)));
        assert!(of_generation(&json!({ "generation": 0 }), Some(3)));
        assert!(of_generation(&json!({}), Some(3)));
        assert!(of_generation(&json!({ "generation": 1 }), None));
    }
}
