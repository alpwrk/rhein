//! The keys of one guild call on an SFU, as `web/src/lib/guild-call-keys.js` keeps them: offer a
//! package with the join, found the call's group when the server says so, join it from the
//! welcome, follow its commits in epoch order, and, as the call's longest-standing member, add
//! whoever waits and remove whoever left. Every leaf must be a device its account vouched for;
//! a call with one that isn't is left with the reason. Each change yields the media key.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use rhein_mls::call_group::{self, CallGroup};
use rhein_mls::vault;
use rhein_net::Api;
use rhein_proto::GuildE2ee;
use serde_json::{Value, json};

/// How long a `check` waits before reading the group's state a second time: a commit on its way
/// may close the gap alone.
const BEHIND_GRACE: Duration = Duration::from_millis(3000);

/// What taking one `VOICE_E2EE` event in did to this device's hold on the call's encryption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heard {
    /// Nothing changed.
    Same,
    /// The media key moved (or arrived).
    Keyed(Key),
    /// This device is behind what the group became (it missed a reset or a commit): the caller
    /// asks the server where the group stands ([`GuildKeys::check`]), and follows it or gives
    /// the seat up.
    Behind,
    /// The committer refused this session's package, so no welcome will ever come for it: the
    /// seat is retaken with a fresh one.
    Left,
}

/// Where the server says the call's group stands, from one look.
struct Standing {
    group_id: String,
    epoch: u64,
    member: bool,
}

/// Whether an account vouches for a device's signature key (`user`, `device`, key base64).
pub type Verifier = Arc<dyn Fn(String, String, String) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// A verifier that takes every device (tests; accounts without identities).
pub fn trust_all() -> Verifier {
    Arc::new(|_, _, _| Box::pin(async { true }))
}

pub struct GuildKeys {
    group: CallGroup,
    channel: String,
    me: String,
    api: Api,
    verify: Verifier,
    /// Events that came before the join answered.
    early: Vec<Value>,
    begun: bool,
}

/// A media key and its epoch.
pub type Key = (Vec<u8>, u64);

impl GuildKeys {
    pub fn new(user: &str, device: &str, signature: (&[u8], &[u8]), channel: &str, api: Api, verify: Verifier) -> Result<GuildKeys> {
        let group = CallGroup::new(user, device, signature.0, signature.1)?;
        Ok(GuildKeys { group, channel: channel.to_owned(), me: user.to_owned(), api, verify, early: Vec::new(), begun: false })
    }

    /// The key package for the join request.
    pub fn offer(&mut self) -> Result<String> {
        self.group.offer()
    }

    pub fn key(&self) -> Option<Key> {
        self.group.media_key(&self.channel)
    }

    /// The join answered: this is the call's group. Returns the key if we hold one now.
    pub async fn begin(&mut self, info: &GuildE2ee) -> Result<Option<Key>> {
        self.group.begin(&info.group_id, info.member)?;
        self.begun = true;
        let mut key = self.key();
        for event in std::mem::take(&mut self.early) {
            if let Heard::Keyed(k) = self.apply(&event).await? {
                key = Some(k);
            }
        }
        Ok(key)
    }

    /// A `VOICE_E2EE` event.
    pub async fn hear(&mut self, event: &Value) -> Result<Heard> {
        if !self.begun {
            self.early.push(event.clone());
            return Ok(Heard::Same);
        }
        self.apply(event).await
    }

    async fn apply(&mut self, d: &Value) -> Result<Heard> {
        let group_id = d["group_id"].as_str().unwrap_or_default();
        match d["op"].as_str() {
            Some("reset") => {
                if Some(group_id) == self.group.group_id() && self.group.is_member() {
                    return Ok(Heard::Same);
                }
                let founder = d["founder"].as_str() == Some(self.me.as_str());
                self.group.begin(group_id, founder)?;
                return Ok(self.key().map_or(Heard::Same, Heard::Keyed));
            }
            // The committer's look happens through the roster; an announcement naming a group we
            // hold nothing from means we missed the move.
            Some("committer") if Some(group_id) != self.group.group_id() => Ok(Heard::Behind),
            Some("refused") if d["user_id"].as_str() == Some(self.me.as_str()) && !self.group.is_member() => Ok(Heard::Left),
            Some("commit") if Some(group_id) == self.group.group_id() => {
                let epoch = d["epoch"].as_u64().unwrap_or(0);
                if !self.group.is_member() {
                    let added = d["added"].as_array().is_some_and(|a| a.iter().any(|u| u.as_str() == Some(self.me.as_str())));
                    let Some(welcome) = d["welcome"].as_str().filter(|_| added) else { return Ok(Heard::Same) };
                    // A welcome for a package of ours that is not the one we hold is not ours.
                    if let Err(err) = self.group.join(welcome) {
                        log::warn!("guild call: the welcome did not open: {err:#}");
                        return Ok(Heard::Same);
                    }
                    self.admit().await?;
                    return Ok(self.key().map_or(Heard::Same, Heard::Keyed));
                }
                let now = self.group.epoch().unwrap_or(0);
                if epoch <= now {
                    return Ok(Heard::Same);
                }
                if epoch != now + 1 {
                    // A gateway reconnect does not replay the relay; the log read in `check`
                    // catches up, or the seat is retaken.
                    return Ok(Heard::Behind);
                }
                let Some(commit) = d["commit"].as_str() else { return Ok(Heard::Behind) };
                if !self.group.process(commit)? {
                    // Removed: silent until a later welcome adds us again.
                    return Ok(Heard::Same);
                }
                self.admit().await?;
                Ok(self.key().map_or(Heard::Same, Heard::Keyed))
            }
            Some("commit") => Ok(Heard::Behind),
            _ => Ok(Heard::Same),
        }
    }

    /// Asks the server where the call's group stands and follows it: reads the commit log when
    /// the group is ours and we are behind. Fails when the group moved on without this device —
    /// the caller then asks for a re-founded group and retakes the seat.
    pub async fn check(&mut self) -> Result<Option<Key>> {
        let Some(target) = self.behind().await else { return Ok(self.key()) };
        tokio::time::sleep(BEHIND_GRACE).await;
        if self.reached(&target) {
            return Ok(self.key());
        }
        if self.catch_up(&target).await && self.reached(&target) {
            Ok(self.key())
        } else {
            bail!("This device lost track of the call's encryption and cannot hear it. Rejoin the call.")
        }
    }

    /// Where the server says the group stands, when that is ahead of us.
    async fn behind(&self) -> Option<Standing> {
        if !self.begun {
            return None;
        }
        let view = self.api.voice_e2ee(&self.channel).await.ok()?;
        let target = Standing {
            group_id: view["group_id"].as_str()?.to_owned(),
            epoch: view["epoch"].as_u64().unwrap_or(0),
            member: view["participants"].as_array().is_some_and(|ps| {
                ps.iter().any(|p| p["user_id"].as_str() == Some(self.me.as_str()) && p["member"].as_bool() == Some(true))
            }),
        };
        (!self.reached(&target)).then_some(target)
    }

    /// Same group, held at least to `target` or, holding nothing, not a member by the server's
    /// account either.
    fn reached(&self, target: &Standing) -> bool {
        if Some(target.group_id.as_str()) != self.group.group_id() {
            return false;
        }
        match self.group.is_member() {
            true => self.group.epoch().unwrap_or(0) >= target.epoch,
            false => !target.member,
        }
    }

    /// Follows the server's commit log. False when the group is not ours or the log has a gap
    /// nothing after it can mend.
    async fn catch_up(&mut self, target: &Standing) -> bool {
        let since = self.group.is_member().then(|| self.group.epoch().unwrap_or(0)).unwrap_or(0);
        let Ok(log) = self.api.voice_e2ee_commits(&self.channel, since).await else { return false };
        if log["group_id"].as_str() != Some(target.group_id.as_str()) {
            return false;
        }
        for event in log["commits"].as_array().cloned().unwrap_or_default() {
            if event["group_id"].as_str().is_some_and(|g| g != target.group_id) {
                return false;
            }
            match self.apply(&event).await {
                Ok(Heard::Behind) | Ok(Heard::Left) | Err(_) => return false,
                Ok(_) => {}
            }
        }
        self.reached(target)
    }

    /// Every other leaf must be a device its account vouched for.
    async fn admit(&mut self) -> Result<()> {
        for leaf in self.group.leaves() {
            if leaf.user_id.is_empty() || leaf.device_id.is_empty() {
                bail!("This call's encryption group holds a device that does not say whose it is, so this device left.");
            }
            if leaf.user_id == self.me && leaf.device_id == self.group.own_device() {
                continue;
            }
            if !(self.verify)(leaf.user_id.clone(), leaf.device_id.clone(), vault::b64(&leaf.signature_key)).await {
                bail!("A device in this call claims to be {}, but their account never signed it, so this device left.", leaf.user_id);
            }
        }
        Ok(())
    }

    /// When we are the call's committer: adds whoever waits and removes whoever left, in one
    /// commit. `roster` is the channel's voice states as JSON. Returns the new key if it moved.
    pub async fn tend(&mut self, roster: &[Value]) -> Result<Option<Key>> {
        if !self.group.is_member() || call_group::committer_of(roster).as_deref() != Some(self.me.as_str()) {
            return Ok(None);
        }
        let view = self.api.voice_e2ee(&self.channel).await.map_err(|e| anyhow!("{e}"))?;
        if view["group_id"].as_str() != self.group.group_id() || view["epoch"].as_u64() != self.group.epoch() {
            return Ok(None);
        }
        let participants = view["participants"].as_array().cloned().unwrap_or_default();
        let (removes, waiting) = call_group::plan_change(&self.group.leaves(), &participants, &self.me);
        // A waiting package is taken only if it names the device the server filed it under and
        // its account vouches for it.
        let mut adds = Vec::new();
        for p in waiting {
            let package = p["key_package"].as_str().unwrap_or_default();
            let Some((user, device, key)) = call_group::package_identity(package) else { continue };
            if Some(user.as_str()) != p["user_id"].as_str() || Some(device.as_str()) != p["device_id"].as_str() {
                continue;
            }
            if !(self.verify)(user.clone(), device, vault::b64(&key)).await {
                continue;
            }
            adds.push((user, package.to_owned()));
        }
        if removes.is_empty() && adds.is_empty() {
            return Ok(None);
        }
        let epoch = self.group.epoch().unwrap_or(0);
        let packages: Vec<String> = adds.iter().map(|(_, p)| p.clone()).collect();
        let (commit, welcome) = self.group.change(&removes, &packages)?;
        let body = json!({
            "group_id": self.group.group_id(),
            "epoch": epoch,
            "commit": commit,
            "welcome": welcome,
            "added": adds.iter().map(|(u, p)| json!({ "user_id": u, "key_package": p })).collect::<Vec<_>>(),
        });
        match self.api.voice_e2ee_commit(&self.channel, &body).await {
            Ok(_) => {
                self.group.merge_pending()?;
                Ok(self.key())
            }
            // Someone else took this epoch; their commit arrives as an event.
            Err(err) if err.status() == Some(409) => {
                self.group.discard_pending();
                Ok(None)
            }
            Err(err) => {
                self.group.discard_pending();
                Err(anyhow!("{err}"))
            }
        }
    }
}
