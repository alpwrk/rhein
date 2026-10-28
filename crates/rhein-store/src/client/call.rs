//! Calls in the client: guild voice channels and calls in private conversations. Joining asks
//! the server where to dial, then a call task on the tokio runtime owns the media: a mesh
//! (signals out through `/voice/signal`, in through `VOICE_SIGNAL`) or an SFU room. Remote
//! voices go into the audio engine, links and failures come back here. The roster itself stays
//! what `VOICE_STATE_UPDATE` says.
//!
//! A conversation call adds privacy (`docs/calls.md`): our posture goes to the others in hellos
//! sealed with the group's call signal key, the mesh pins itself to TURN when the call's
//! strictest posture asks for it, and an SFU room is end-to-end encrypted with the group's media
//! key, rekeyed on every epoch.

use std::collections::HashSet;
use std::sync::Arc;

use rhein_voice::livekit::e2ee::key_provider::KeyProvider;
use rhein_voice::livekit::webrtc::media_stream_track::MediaStreamTrack;
use rhein_voice::livekit::webrtc::peer_connection_factory::PeerConnectionFactory;
use rhein_voice::livekit::webrtc::peer_connection_factory::native::PeerConnectionFactoryExt as _;
use rhein_proto::{VoiceClaim, VoiceServer};
use rhein_voice::audio::{AudioConfig, AudioEngine};
use rhein_voice::e2ee;
use crate::guild_keys::{GuildKeys, Verifier, Heard};
use rhein_voice::mesh::{self, Mesh};
use rhein_voice::posture::{self, Hello, Posture};
use rhein_voice::screen::{ScreenShare, ShareError};
use rhein_voice::sfu::{self, SfuCall};
use rhein_voice::video::{VideoFeed, VideoOut};
use rhein_voice::{CallEvent, Slot};
use serde_json::Value;
use tokio::sync::mpsc;

use super::*;
use super::retake;

/// How a call's media travels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallMode {
    Mesh,
    Sfu { region: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallStatus {
    /// Asking the server, opening devices, reaching the SFU.
    Connecting,
    Connected,
    Failed(String),
}

/// The call this client is in (one at a time).
pub struct ActiveCall {
    pub server: String,
    /// The guild of a voice channel, or the conversation itself.
    pub room_id: String,
    /// The voice channel, or for a conversation call the conversation id again.
    pub channel_id: String,
    /// A call in a private conversation (on the home server, with posture and E2EE).
    pub conversation: bool,
    pub session_id: Option<String>,
    pub mode: Option<CallMode>,
    pub status: CallStatus,
    /// Participants whose media reaches us.
    pub linked: HashSet<String>,
    pub muted: bool,
    pub deafened: bool,
    /// Our own screen share, while it runs.
    pub sharing_screen: bool,
    /// A conversation call's effective posture, once hellos have been weighed.
    pub posture: Option<Posture>,
    /// The MLS epoch a conversation call is keyed with.
    epoch: Option<u64>,
    /// Whose screens we asked to see (an SFU forwards them only then).
    pub watching: HashSet<String>,
    engine: Option<Arc<AudioEngine>>,
    commands: Option<mpsc::UnboundedSender<Command>>,
    screen: Option<ScreenShare>,
    /// A test pattern shared in place of a screen (`RHEIN_DEV_SHARE=pattern`).
    test_pattern: Option<rhein_voice::video::TestPattern>,
    /// What we share, for our own eyes (paused while hidden, see `set_own_screen_hidden`).
    own_preview: Option<VideoFeed>,
    videos: HashMap<(String, Slot), VideoFeed>,
    _task: Option<Task<()>>,
}

impl ActiveCall {
    pub fn engine(&self) -> Option<&Arc<AudioEngine>> {
        self.engine.as_ref()
    }

    /// Whether someone's camera or screen reaches us (its first picture may still be coming).
    pub fn has_video(&self, user_id: &str, slot: Slot) -> bool {
        self.videos.contains_key(&(user_id.to_owned(), slot))
    }
}

/// The latest picture of each video in the call. Kept apart from the client: a new picture
/// thirty times a second redraws only the views that draw it, not everything that reads the
/// client.
#[derive(Default)]
pub struct CallVideo {
    /// The picture drawn for each video, and which frame it is.
    images: HashMap<(String, Slot), (u64, Arc<gpui::RenderImage>)>,
}

impl CallVideo {
    pub fn picture(&self, user_id: &str, slot: Slot) -> Option<Arc<gpui::RenderImage>> {
        self.images.get(&(user_id.to_owned(), slot)).map(|(_, image)| image.clone())
    }

    fn drop_picture(&mut self, key: &(String, Slot), cx: &mut App) -> bool {
        let Some((_, image)) = self.images.remove(key) else { return false };
        cx.drop_image(image, None);
        true
    }

    fn clear(&mut self, cx: &mut App) {
        for (_, (_, image)) in self.images.drain() {
            cx.drop_image(image, None);
        }
    }
}

/// Who in the call is audible right now (our own user included). Apart from the client for the
/// same reason as [`CallVideo`].
#[derive(Default)]
pub struct CallSpeaking(HashSet<String>);

impl CallSpeaking {
    pub fn is_speaking(&self, user_id: &str) -> bool {
        self.0.contains(user_id)
    }
}

enum Command {
    Signal { from: String, payload: Value },
    /// A conversation's call secrets for a (new) epoch: the hello key and the media key.
    Keys { hello: Option<(Vec<u8>, u64)>, media: Option<(Vec<u8>, u64)> },
    /// A guild call's `VOICE_E2EE` event.
    GuildE2ee(Value),
    /// A guild call's voice states, for its committer.
    GuildRoster(Vec<Value>),
    Roster(Vec<String>),
    Server(VoiceServer),
    Watch { user: String, on: bool },
    Publish { slot: Slot, video: VideoOut },
    Unpublish(Slot),
    /// This seat's current self flags, so a retake joins with them.
    Flags { muted: bool, deafened: bool },
    Leave,
}

enum Update {
    Connected(CallMode),
    /// A conversation call's effective posture changed.
    Posture(Posture),
    Link { user: String, up: bool },
    Video { user: String, slot: Slot, feed: Option<VideoFeed> },
    Failed(String),
    /// A live seat is being retaken: connecting again, not connected.
    Connecting,
    /// The call cannot go on from here: the client ends it and, when a reason is carried, says
    /// why.
    Ended(Option<String>),
}

pub use crate::prefs::VoicePrefs;

/// The settings' mic test: its own audio when not in a call, and whether it deafened you.
pub(crate) struct MicTest {
    own: Option<Arc<AudioEngine>>,
    deafened_here: bool,
}

trait AudioConfigOf {
    fn audio_config(&self) -> AudioConfig;
}

impl AudioConfigOf for VoicePrefs {
    fn audio_config(&self) -> AudioConfig {
        let null = || Some("alsa:null".to_owned());
        AudioConfig {
            input: if self.null_devices { null() } else { self.input.clone() },
            output: if self.null_devices { null() } else { self.output.clone() },
            echo_cancellation: !self.raw_input,
            noise_suppression: !self.raw_input,
            auto_gain: !self.raw_input,
        }
    }
}

impl Client {
    pub fn call(&self) -> Option<&ActiveCall> {
        self.call.as_ref()
    }

    pub fn in_call(&self, channel_id: &str) -> bool {
        self.call.as_ref().is_some_and(|c| c.channel_id == channel_id)
    }

    /// Joins a guild voice channel, leaving any other call first.
    pub fn join_voice(&mut self, channel_id: String, cx: &mut Context<Self>) {
        if self.in_call(&channel_id) {
            return;
        }
        self.leave_voice(cx);
        let (Some(api), Some(guild)) = (self.api_for_channel(&channel_id), self.guild_of_channel(&channel_id)) else { return };
        let (server, guild_id) = (guild.server.clone(), guild.guild.id.clone());
        let (muted, deafened) = (self.prefs.voice.muted, self.prefs.voice.deafened);
        let (http, channel) = (self.http.clone(), channel_id.clone());
        let route = Route::Channel { api: api.clone(), channel: channel_id.clone() };
        let mut keys = self.guild_keys(&channel_id, &api);
        // A channel set to a region by hand is never probed: the server uses that region.
        let fixed = self.channel(&channel_id).and_then(|c| c.voice_region.clone()).is_some_and(|r| !r.is_empty() && r != "auto");
        let dial = async move {
            let region = if fixed {
                None
            } else {
                let published = api.voice_regions().await.unwrap_or(Value::Null);
                rhein_voice::region::choose(&http, &published, None).await
            };
            // Offered with every join: only the server knows whether the channel is on an SFU,
            // where the call is end-to-end encrypted and a join without one is refused.
            let package = keys.as_mut().and_then(|k| k.offer().ok());
            let joined = api.voice_join(&channel, muted, deafened, false, region.as_deref(), package.as_deref()).await?;
            let mut guild = None;
            if let (Some(mut keys), Some(info)) = (keys, joined.e2ee.clone()) {
                keys.begin(&info).await?;
                guild = Some(keys);
            }
            let here = api.voice_channel(&channel).await.ok();
            Ok(Dialed { joined, privacy: None, guild, here })
        };
        self.begin(server, guild_id, channel_id, false, route, dial, cx);
    }

    /// Joins (or starts) the call in a private conversation.
    pub fn join_dm_call(&mut self, conversation: String, cx: &mut Context<Self>) {
        // No call signalling into a request before it is accepted.
        if self.in_call(&conversation) || self.is_request(&conversation) {
            return;
        }
        self.leave_voice(cx);
        let Some(home) = &self.home else { return };
        let (api, server) = (home.api.clone(), home.doc.server_name.clone());
        let (muted, deafened) = (self.prefs.voice.muted, self.prefs.voice.deafened);
        let own = Posture::parse(&self.prefs.voice.posture);
        let engine = self.dm_engine.clone();
        let (id, route) = (conversation.clone(), Route::Conversation { api: api.clone(), id: conversation.clone() });
        let dial = async move {
            let keys = match &engine {
                Some(engine) => {
                    let mut engine = engine.lock().await;
                    (engine.call_secret(&id, posture::SIGNAL_LABEL), engine.call_secret(&id, e2ee::MEDIA_EXPORTER_LABEL))
                }
                None => (None, None),
            };
            // Whether our home has a relay is only known from its answer; until then a relay
            // posture asks for an SFU, the strict way round.
            let joined = api.conversation_voice_join(&id, muted, deafened, posture::wants_sfu(own, false), false).await?;
            let here = api.conversation_voice(&id).await.ok();
            Ok(Dialed { joined, privacy: Some(Privacy::new(id, own, keys.0, keys.1)), guild: None, here })
        };
        self.begin(server, conversation.clone(), conversation, true, route, dial, cx);
    }

    /// Starts a call: dials (off the UI thread), opens the devices and hands the media to a call
    /// task.
    #[allow(clippy::too_many_arguments)]
    fn begin(
        &mut self,
        server: String,
        room_id: String,
        channel_id: String,
        conversation: bool,
        route: Route,
        // The join answer, a conversation's privacy state, and who is already there.
        dial: impl std::future::Future<Output = Result<Dialed>> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(me) = self.home.as_ref().map(|h| h.user_id.clone()) else { return };
        let prefs = self.voice_prefs();
        let (io, http) = (self.io.clone(), self.http.clone());
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (updates, mut update_rx) = mpsc::unbounded_channel();

        let task = cx.spawn(async move |this, cx| {
            let started = join(io.spawn(async move {
                let dialed = dial.await?;
                let engine = tokio::task::spawn_blocking(move || AudioEngine::start(prefs.audio_config())).await??;
                Ok((dialed, Arc::new(engine)))
            }))
            .await;
            let Ok(Some((dialed, engine))) = this.update(cx, |this, cx| {
                if let Ok((Dialed { here: Some(here), .. }, _)) = &started {
                    let states: Vec<rhein_proto::VoiceState> = here["voice_states"].as_array().into_iter().flatten().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect();
                    let room = this.call.as_ref()?.room_id.clone();
                    let roster = this.voice.entry(room).or_default();
                    for state in states.into_iter().filter(|s| s.channel_id.is_some()) {
                        roster.insert(state.user_id.clone(), state);
                    }
                }
                let call = this.call.as_mut()?;
                match started {
                    Ok(ready) => {
                        call.session_id = ready.0.joined.voice_state.session_id.clone();
                        call.engine = Some(ready.1.clone());
                        call.epoch = ready.0.privacy.as_ref().and_then(|p| p.hello.as_ref().map(|(_, e)| *e));
                        this.apply_call_audio();
                        this.claim_voice(cx);
                        cx.notify();
                        Some(ready)
                    }
                    Err(err) => {
                        log::warn!("joining a call: {err:#}");
                        call.status = CallStatus::Failed(friendly(&err));
                        cx.notify();
                        None
                    }
                }
            }) else {
                return;
            };
            for error in engine.errors() {
                log::warn!("call audio: {error}");
            }
            let roster = this.read_with(cx, |this, _| this.call_roster()).unwrap_or_default();
            let Dialed { joined, privacy, guild, .. } = dialed;
            let flags = this.read_with(cx, |this, _| (this.prefs.voice.muted, this.prefs.voice.deafened)).unwrap_or((false, false));
            io.spawn(drive(me, route, http, joined.voice_server, engine, roster, privacy, guild, command_rx, updates, flags));
            // The committer's first look at who waits.
            this.update(cx, |this, _| this.tend_guild_keys()).ok();

            while let Some(update) = update_rx.recv().await {
                let alive = this.update(cx, |this, cx| {
                    // The call is over from the call task's side (the server removed the seat,
                    // or the seat could not be retaken): end it as a hang-up would.
                    if matches!(update, Update::Ended(_)) {
                        let reason = match update {
                            Update::Ended(reason) => reason,
                            _ => None,
                        };
                        this.leave_voice(cx);
                        if let Some(reason) = reason {
                            this.error = Some(reason.into());
                        }
                        cx.notify();
                        return true;
                    }
                    let Some(call) = this.call.as_mut() else { return false };
                    match update {
                        Update::Connected(mode) => {
                            call.mode = Some(mode);
                            call.status = CallStatus::Connected;
                        }
                        Update::Posture(posture) => call.posture = Some(posture),
                        Update::Link { user, up } => {
                            if up {
                                call.linked.insert(user);
                            } else {
                                call.linked.remove(&user);
                            }
                        }
                        Update::Video { user, slot, feed } => match feed {
                            Some(feed) => {
                                call.videos.insert((user, slot), feed);
                            }
                            None => {
                                call.videos.remove(&(user.clone(), slot));
                                this.call_video.update(cx, |v, cx| {
                                    if v.drop_picture(&(user, slot), cx) {
                                        cx.notify();
                                    }
                                });
                            }
                        },
                        Update::Failed(err) => call.status = CallStatus::Failed(err),
                        Update::Connecting => call.status = CallStatus::Connecting,
                        // Handled above, before the call is borrowed.
                        Update::Ended(_) => {}
                    }
                    cx.notify();
                    true
                });
                if !matches!(alive, Ok(true)) {
                    break;
                }
            }
        });
        self.call = Some(ActiveCall {
            server,
            room_id,
            channel_id,
            conversation,
            session_id: None,
            mode: None,
            posture: None,
            epoch: None,
            status: CallStatus::Connecting,
            linked: HashSet::new(),
            muted: self.prefs.voice.muted,
            deafened: self.prefs.voice.deafened,
            sharing_screen: false,
            watching: HashSet::new(),
            engine: None,
            commands: Some(commands),
            screen: None,
            test_pattern: None,
            own_preview: None,
            videos: HashMap::new(),
            _task: Some(task),
        });
        self.watch_speaking(cx);
        cx.notify();
    }

    /// The keys of a guild call: this device's MLS signature key, and the account trust check a
    /// conversation uses. `None` before this device has DM keys.
    fn guild_keys(&self, channel: &str, api: &Api) -> Option<GuildKeys> {
        let home = self.home.as_ref()?;
        let (private, public) = crate::dm::Secrets::load(&home.user_id).mls_signature?;
        let (private, public) = (rhein_mls::vault::unb64(&private).ok()?, rhein_mls::vault::unb64(&public).ok()?);
        let (engine, home_api) = (self.dm_engine.clone(), home.api.clone());
        let verify: Verifier = Arc::new(move |user, device, key| {
            let (engine, api) = (engine.clone(), home_api.clone());
            Box::pin(async move {
                match engine {
                    Some(engine) => engine.lock().await.verify_claimed(&api, &user, &device, &key).await != Some(false),
                    None => true,
                }
            })
        });
        GuildKeys::new(&home.user_id, &home.device_id, (&private, &public), channel, api.clone(), verify)
            .map_err(|err| log::warn!("guild call keys: {err:#}"))
            .ok()
    }

    /// Hands a guild call's roster (with who holds the key and since when) to its keys.
    fn tend_guild_keys(&self) {
        let Some(call) = self.call.as_ref().filter(|c| !c.conversation) else { return };
        let states: Vec<Value> = self
            .voice_members(&call.room_id, &call.channel_id)
            .into_iter()
            .filter_map(|s| serde_json::to_value(s).ok())
            .collect();
        if let Some(commands) = &call.commands {
            let _ = commands.send(Command::GuildRoster(states));
        }
    }

    pub(super) fn on_voice_e2ee(&mut self, channel_id: &str, data: Value) {
        if let Some(call) = self.call.as_ref().filter(|c| c.channel_id == channel_id && !c.conversation)
            && let Some(commands) = &call.commands
        {
            let _ = commands.send(Command::GuildE2ee(data));
        }
    }

    /// The route (endpoints) of the call we are in.
    fn route(&self) -> Option<Route> {
        let call = self.call.as_ref()?;
        Some(if call.conversation {
            Route::Conversation { api: self.home.as_ref()?.api.clone(), id: call.room_id.clone() }
        } else {
            Route::Channel { api: self.api_for_guild(&call.room_id)?, channel: call.channel_id.clone() }
        })
    }

    /// Hangs up.
    pub fn leave_voice(&mut self, cx: &mut Context<Self>) {
        let Some(call) = self.call.take() else { return };
        if let Some(commands) = &call.commands {
            let _ = commands.send(Command::Leave);
        }
        self.call_video.update(cx, |v, cx| {
            v.clear(cx);
            cx.notify();
        });
        self.call_speaking.update(cx, |s, cx| {
            s.0.clear();
            cx.notify();
        });
        if let Some(route) = self.route_of(&call) {
            self.io.spawn(async move { route.leave().await });
        }
        // The engine goes with the last handle (the call task drops its own on Leave).
        drop(call);
        self.claim_voice(cx);
        cx.notify();
    }

    pub fn set_voice_muted(&mut self, muted: bool, cx: &mut Context<Self>) {
        self.prefs.voice.muted = muted;
        // Unmuting while deafened undeafens too, as in the web client.
        if !muted {
            self.prefs.voice.deafened = false;
        }
        self.save_prefs();
        self.push_voice_state(cx);
    }

    pub fn set_voice_deafened(&mut self, deafened: bool, cx: &mut Context<Self>) {
        // Deafening mutes too; undeafening gives the microphone back only if it was on before.
        if deafened && !self.prefs.voice.deafened {
            self.muted_before_deafen = self.prefs.voice.muted;
            self.prefs.voice.muted = true;
        } else if !deafened && self.prefs.voice.deafened {
            self.prefs.voice.muted = self.muted_before_deafen;
        }
        self.prefs.voice.deafened = deafened;
        self.save_prefs();
        self.push_voice_state(cx);
    }

    /// Whether we are in the call of this voice channel.
    pub fn in_voice_channel(&self, channel_id: &str) -> bool {
        self.call.as_ref().is_some_and(|c| !c.conversation && c.channel_id == channel_id)
    }

    pub fn voice_muted(&self) -> bool {
        self.prefs.voice.muted
    }

    pub fn voice_deafened(&self) -> bool {
        self.prefs.voice.deafened
    }

    /// The saved voice settings, with this run's device override.
    pub fn voice_prefs(&self) -> VoicePrefs {
        VoicePrefs { null_devices: self.null_audio, ..self.prefs.voice.clone() }
    }

    /// Changes the voice settings and hands them to a running call.
    pub fn update_voice_prefs(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut VoicePrefs)) {
        let before = self.voice_prefs();
        change(&mut self.prefs.voice);
        self.save_prefs();
        let after = self.voice_prefs();
        if (before.input != after.input || before.output != after.output || before.raw_input != after.raw_input)
            && let Some(engine) = self.call.as_ref().and_then(|c| c.engine.clone())
        {
            let config = after.audio_config();
            engine.set_devices(config.input, config.output);
        }
        self.apply_call_audio();
        cx.notify();
    }

    /// Push-to-talk key held or released.
    pub fn set_talking(&self, held: bool) {
        if let Some(engine) = self.call.as_ref().and_then(|c| c.engine.as_ref()) {
            engine.set_talking(held);
        }
    }

    /// A local volume for one participant, 0..200.
    pub fn set_user_volume(&mut self, user_id: &str, percent: u32) {
        self.prefs.voice.volumes.insert(user_id.to_owned(), percent);
        self.save_prefs();
        if let Some(engine) = self.call.as_ref().and_then(|c| c.engine.as_ref()) {
            engine.set_volume(user_id, percent);
        }
    }

    pub fn user_volume(&self, user_id: &str) -> u32 {
        self.prefs.voice.volumes.get(user_id).copied().unwrap_or(100)
    }

    /// The microphone's level, 0..1, while in a call.
    pub fn input_level(&self) -> Option<f32> {
        self.call.as_ref().and_then(|c| c.engine.as_ref()).or(self.mic_test.as_ref().and_then(|t| t.own.as_ref())).map(|e| e.input_level())
    }

    /// Whether the mic test is on (you hear yourself as the others would).
    pub fn mic_testing(&self) -> bool {
        self.mic_test.is_some()
    }

    /// Starts hearing yourself. In a call this uses the call's audio and deafens you meanwhile,
    /// so the call doesn't drown it out; outside one it opens the microphone for the test alone.
    pub fn start_mic_test(&mut self, cx: &mut Context<Self>) {
        if self.mic_test.is_some() {
            return;
        }
        if let Some(engine) = self.call.as_ref().and_then(|c| c.engine.clone()) {
            let deafened_here = !self.prefs.voice.deafened;
            if deafened_here {
                self.set_voice_deafened(true, cx);
            }
            engine.set_loopback(true);
            self.mic_test = Some(MicTest { own: None, deafened_here });
            cx.notify();
            return;
        }
        let voice = self.voice_prefs();
        let io = self.io.clone();
        self.mic_test = Some(MicTest { own: None, deafened_here: false });
        cx.spawn(async move |this, cx| {
            let config = voice.audio_config();
            let started = join(io.spawn(async move { tokio::task::spawn_blocking(move || AudioEngine::start(config)).await? })).await;
            this.update(cx, |this, cx| {
                match (started, this.mic_test.as_mut()) {
                    (Ok(engine), Some(test)) => {
                        engine.set_gate(voice.gate);
                        engine.set_input_gain(voice.input_gain.unwrap_or(100));
                        engine.set_loopback(true);
                        test.own = Some(Arc::new(engine));
                    }
                    (Err(err), _) => {
                        this.mic_test = None;
                        this.error = Some(format!("Couldn't open the microphone: {err:#}").into());
                    }
                    // Stopped while it was opening.
                    (Ok(_), None) => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Stops hearing yourself, and undeafens if the test deafened you.
    pub fn stop_mic_test(&mut self, cx: &mut Context<Self>) {
        let Some(test) = self.mic_test.take() else { return };
        if let Some(engine) = self.call.as_ref().and_then(|c| c.engine.as_ref()) {
            engine.set_loopback(false);
        }
        if test.deafened_here && self.prefs.voice.deafened {
            self.set_voice_deafened(false, cx);
        }
        cx.notify();
    }

    fn push_voice_state(&mut self, cx: &mut Context<Self>) {
        let (muted, deafened) = (self.prefs.voice.muted, self.prefs.voice.deafened);
        if let Some(call) = self.call.as_mut() {
            call.muted = muted;
            call.deafened = deafened;
        }
        if let Some(commands) = self.call.as_ref().and_then(|c| c.commands.as_ref()) {
            let _ = commands.send(Command::Flags { muted, deafened });
        }
        if let Some(route) = self.route() {
            self.io.spawn(async move { route.flags(muted, deafened).await });
        }
        self.apply_call_audio();
        cx.notify();
    }

    /// Hands mute, deafen and the settings to the running engine.
    fn apply_call_audio(&self) {
        let Some(engine) = self.call.as_ref().and_then(|c| c.engine.as_ref()) else { return };
        let voice = &self.prefs.voice;
        engine.set_muted(voice.muted);
        engine.set_deafened(voice.deafened);
        engine.set_push_to_talk(voice.push_to_talk);
        engine.set_gate(voice.gate);
        engine.set_input_gain(voice.input_gain.unwrap_or(100));
        for (user, volume) in &voice.volumes {
            engine.set_volume(user, *volume);
        }
    }

    /// Names the call we are in to the guild server's gateway, so a restart keeps our seat.
    fn claim_voice(&self, _cx: &mut Context<Self>) {
        for server in &self.servers {
            let claims = self
                .call
                .as_ref()
                .filter(|c| c.server == server.name)
                .and_then(|c| Some(VoiceClaim { room_id: c.room_id.clone(), session_id: c.session_id.clone()? }))
                .into_iter()
                .collect();
            if let Some(gateway) = &server.gateway {
                gateway.set_voice_claims(claims);
            }
        }
    }

    /// Who the server says is in our call's channel.
    fn call_roster(&self) -> Vec<String> {
        let Some(call) = &self.call else { return Vec::new() };
        self.voice_members(&call.room_id, &call.channel_id).into_iter().map(|s| s.user_id).collect()
    }

    /// Called for every voice state change of a guild or conversation: the mesh follows the
    /// roster, and a call the server moved or ended follows the server.
    pub(super) fn on_voice_roster(&mut self, state: &rhein_proto::VoiceState, cx: &mut Context<Self>) {
        let Some(call) = &self.call else { return };
        let room = state.guild_id.as_deref().or(state.conversation_id.as_deref());
        if room != Some(call.room_id.as_str()) {
            return;
        }
        let me = self.home.as_ref().map(|h| h.user_id.as_str());
        if Some(state.user_id.as_str()) == me {
            match state.channel_id.as_deref() {
                // Disconnected by the server (kicked, swept) — but a sweep that raced a
                // reconnect looks like a kick, and ending the call is the one outcome nobody
                // wants twice. For a guild call the server's own answer decides: gone, the call
                // ends with the seat; still there, it goes on. A conversation's call follows the
                // frame.
                None if !call.conversation => {
                    let Some(route) = self.route_of(&call) else {
                        self.call = None;
                        self.claim_voice(cx);
                        cx.notify();
                        return;
                    };
                    let (io, me, channel) = (self.io.clone(), me.map(str::to_owned).unwrap_or_default(), call.channel_id.clone());
                    cx.spawn(async move |this, cx| {
                        if !io.spawn(async move { route.seat_held(&me).await }).await.unwrap_or(false) {
                            return;
                        }
                        this.update(cx, |this, cx| {
                            if this.call.as_ref().is_some_and(|c| c.channel_id == channel) {
                                this.leave_voice(cx);
                            }
                            cx.notify();
                        })
                        .ok();
                    })
                    .detach();
                    return;
                }
                // Disconnected by the server (kicked, swept): the call is over.
                None => {
                    self.call = None;
                    self.claim_voice(cx);
                    cx.notify();
                    return;
                }
                // Moved by a moderator: dial the new channel ourselves.
                Some(channel) if channel != call.channel_id && !call.conversation => {
                    let channel = channel.to_owned();
                    self.call = None;
                    self.join_voice(channel, cx);
                    return;
                }
                _ => {}
            }
        }
        let roster = self.call_roster();
        if let Some(commands) = self.call.as_ref().and_then(|c| c.commands.as_ref()) {
            let _ = commands.send(Command::Roster(roster));
        }
        self.tend_guild_keys();
    }

    pub(super) fn on_voice_signal(&mut self, channel_id: &str, from: String, payload: Value) {
        if let Some(call) = self.call.as_ref().filter(|c| c.channel_id == channel_id)
            && let Some(commands) = &call.commands
        {
            let _ = commands.send(Command::Signal { from, payload });
        }
    }

    pub(super) fn on_voice_server(&mut self, data: Value) {
        let Some(call) = &self.call else { return };
        if data["channel_id"].as_str().is_some_and(|c| c != call.channel_id) {
            return;
        }
        if let (Ok(server), Some(commands)) = (serde_json::from_value::<VoiceServer>(data), &call.commands) {
            let _ = commands.send(Command::Server(server));
        }
    }

    /// Starts (or stops) watching someone's screen share. The server records it and has the SFU
    /// forward it; a mesh sends screens to everyone anyway.
    pub fn watch_screen(&mut self, user_id: &str, on: bool, cx: &mut Context<Self>) {
        let Some(call) = self.call.as_mut() else { return };
        if on {
            call.watching.insert(user_id.to_owned());
        } else {
            call.watching.remove(user_id);
        }
        if let Some(commands) = &call.commands {
            let _ = commands.send(Command::Watch { user: user_id.to_owned(), on });
        }
        let user = user_id.to_owned();
        if let Some(route) = self.route() {
            self.io.spawn(async move { route.watch(&user, on).await });
        }
        cx.notify();
    }

    /// Asks the desktop for a screen or window and shares it into the call.
    pub fn start_screen_share(&mut self, cx: &mut Context<Self>) {
        let Some(call) = self.call.as_ref() else { return };
        if call.sharing_screen {
            return;
        }
        let io = self.io.clone();
        let (fps, height) = (self.prefs.voice.share_fps(), self.prefs.voice.share_height);
        let preset = self.prefs.voice.share_preset();
        cx.spawn(async move |this, cx| {
            let started = io.spawn(async move { tokio::task::spawn_blocking(move || ScreenShare::start(fps, height, true)).await }).await;
            let started = match started {
                Ok(Ok(result)) => result,
                _ => Err(ShareError::Other("the screen capture stopped unexpectedly".into())),
            };
            this.update(cx, |this, cx| {
                let Some(call) = this.call.as_mut() else { return };
                match started {
                    Ok(share) => {
                        if let Some(commands) = &call.commands {
                            let _ = commands.send(Command::Publish { slot: Slot::Screen, video: share.video().clone() });
                        }
                        call.own_preview = Some(share.preview());
                        call.screen = Some(share);
                        call.sharing_screen = true;
                        this.place_own_preview(cx);
                        if let Some(route) = this.route() {
                            this.io.spawn(async move { route.stream(Some(preset)).await });
                        }
                    }
                    // A closed picker is the person's answer; anything else is worth saying.
                    Err(ShareError::Cancelled) => log::info!("screen share cancelled"),
                    // A missing portal is a package away: a warning, not an error.
                    Err(err @ ShareError::NoPortal) => {
                        log::warn!("screen share not started: {err}");
                        this.warning = Some(err.to_string().into());
                    }
                    Err(err) => {
                        log::warn!("screen share not started: {err}");
                        this.error = Some(err.to_string().into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Shares a moving test pattern as if it were a screen, without asking the desktop: the
    /// same publishing and preview, for screenshots in a headless session.
    pub fn share_test_pattern(&mut self, cx: &mut Context<Self>) {
        let Some(call) = self.call.as_mut() else { return };
        let _inside = self.io.handle().enter();
        let (preview, sink) = rhein_voice::video::VideoFeed::local(1280, 10);
        let video = VideoOut::new(1280, 720, true).with_preview(sink);
        call.test_pattern = Some(rhein_voice::video::TestPattern::start(video.clone(), 1280, 720));
        if let Some(commands) = &call.commands {
            let _ = commands.send(Command::Publish { slot: Slot::Screen, video });
        }
        call.own_preview = Some(preview);
        call.sharing_screen = true;
        self.place_own_preview(cx);
        if let Some(route) = self.route() {
            self.io.spawn(async move { route.stream(Some("detail 720p@15".into())).await });
        }
        cx.notify();
    }

    /// Whether our own screen stays off our own stage (a choice kept on this device).
    pub fn own_screen_hidden(&self) -> bool {
        self.prefs.hide_own_screen
    }

    /// Hides (or shows again) the preview of what we share; the share itself goes on.
    pub fn set_own_screen_hidden(&mut self, hidden: bool, cx: &mut Context<Self>) {
        self.prefs.hide_own_screen = hidden;
        self.save_prefs();
        self.place_own_preview(cx);
        cx.notify();
    }

    /// Puts our preview on our stage, or takes it off and stops making its pictures.
    fn place_own_preview(&mut self, cx: &mut Context<Self>) {
        let hidden = self.prefs.hide_own_screen;
        let Some(me) = self.home.as_ref().map(|h| h.user_id.clone()) else { return };
        let Some(call) = self.call.as_mut() else { return };
        let Some(preview) = call.own_preview.clone() else { return };
        preview.set_paused(hidden);
        let key = (me, Slot::Screen);
        if hidden {
            call.videos.remove(&key);
            self.call_video.update(cx, |v, cx| {
                if v.drop_picture(&key, cx) {
                    cx.notify();
                }
            });
        } else {
            call.videos.insert(key, preview);
        }
    }

    pub fn stop_screen_share(&mut self, cx: &mut Context<Self>) {
        let Some(call) = self.call.as_mut() else { return };
        if !call.sharing_screen {
            return;
        }
        call.sharing_screen = false;
        call.screen = None;
        call.test_pattern = None;
        call.own_preview = None;
        if let Some(me) = self.home.as_ref().map(|h| h.user_id.clone()) {
            call.videos.remove(&(me.clone(), Slot::Screen));
            self.call_video.update(cx, |v, cx| {
                if v.drop_picture(&(me, Slot::Screen), cx) {
                    cx.notify();
                }
            });
        }
        if let Some(commands) = &call.commands {
            let _ = commands.send(Command::Unpublish(Slot::Screen));
        }
        if let Some(route) = self.route() {
            self.io.spawn(async move { route.stream(None).await });
        }
        cx.notify();
    }

    /// Polls the engine for who is audible and the videos for new pictures. Each goes to its own
    /// entity, so only the views drawing it redraw. Without videos the poll slows down to the
    /// pace of who is speaking.
    fn watch_speaking(&mut self, cx: &mut Context<Self>) {
        const FRAME: Duration = Duration::from_millis(33);
        const VOICE: Duration = Duration::from_millis(100);
        self.speaking_task = Some(cx.spawn(async move |this, cx| {
            let (mut wait, mut last_voice, mut last_rekey) = (FRAME, Instant::now(), Instant::now());
            loop {
                cx.background_executor().timer(wait).await;
                let Ok(alive) = this.update(cx, |this, cx| {
                    let me = this.home.as_ref().map(|h| h.user_id.clone()).unwrap_or_default();
                    let Some(call) = this.call.as_mut() else { return false };
                    wait = if call.videos.is_empty() { VOICE } else { FRAME };
                    if last_voice.elapsed() >= VOICE - Duration::from_millis(5) {
                        last_voice = Instant::now();
                        let now: HashSet<String> = call
                            .engine
                            .as_ref()
                            .map(|e| e.speaking().into_iter().map(|k| if k.is_empty() { me.clone() } else { k }).collect())
                            .unwrap_or_default();
                        this.call_speaking.update(cx, |speaking, cx| {
                            if speaking.0 != now {
                                speaking.0 = now;
                                cx.notify();
                            }
                        });
                    }
                    let Some(call) = this.call.as_mut() else { return false };
                    if !call.videos.is_empty() {
                        let fresh: Vec<_> = call
                            .videos
                            .iter()
                            // Taken, not cloned: the next one is a new picture with a new number.
                            .filter_map(|(key, feed)| feed.take_latest().map(|(seq, picture)| (key.clone(), seq, picture)))
                            .collect();
                        if !fresh.is_empty() {
                            this.call_video.update(cx, |video, cx| {
                                let mut changed = false;
                                for (key, seq, picture) in fresh {
                                    if video.images.get(&key).is_some_and(|(have, _)| *have == seq) {
                                        continue;
                                    }
                                    let Some(image) = render_image(picture) else { continue };
                                    if let Some((_, old)) = video.images.insert(key, (seq, image)) {
                                        cx.drop_image(old, None);
                                    }
                                    changed = true;
                                }
                                if changed {
                                    cx.notify();
                                }
                            });
                        }
                    }
                    let Some(call) = this.call.as_mut() else { return false };
                    // A conversation call follows the group's epoch: new keys for hellos and media.
                    let rekey = (call.conversation && last_rekey.elapsed() >= Duration::from_secs(1)).then(|| {
                        last_rekey = Instant::now();
                        (call.room_id.clone(), call.epoch)
                    });
                    // The desktop's own "stop sharing" ends a share without us.
                    if call.screen.as_ref().is_some_and(|s| s.ended()) {
                        this.stop_screen_share(cx);
                    }
                    if let Some((conversation, epoch)) = rekey {
                        this.follow_epoch(&conversation, epoch);
                    }
                    true
                }) else {
                    return;
                };
                if !alive {
                    return;
                }
            }
        }));
    }
}

impl Client {
    /// Hands a conversation call the keys of the group's current epoch when it moved on.
    fn follow_epoch(&mut self, conversation: &str, epoch: Option<u64>) {
        let Some(engine) = &self.dm_engine else { return };
        // The engine may be busy syncing; the next tick tries again.
        let Ok(mut engine) = engine.try_lock() else { return };
        let hello = engine.call_secret(conversation, posture::SIGNAL_LABEL);
        let now = hello.as_ref().map(|(_, e)| *e);
        if now.is_none() || now == epoch {
            return;
        }
        let media = engine.call_secret(conversation, e2ee::MEDIA_EXPORTER_LABEL);
        drop(engine);
        if let Some(call) = self.call.as_mut() {
            call.epoch = now;
            if let Some(commands) = &call.commands {
                let _ = commands.send(Command::Keys { hello, media });
            }
        }
    }

    fn route_of(&self, call: &ActiveCall) -> Option<Route> {
        Some(if call.conversation {
            Route::Conversation { api: self.home.as_ref()?.api.clone(), id: call.room_id.clone() }
        } else {
            Route::Channel { api: self.api_for_guild(&call.room_id)?, channel: call.channel_id.clone() }
        })
    }
}

/// How often a guild call asks the server where its encryption group stands, so a device that
/// missed a commit without its gateway noticing does not stay a commit behind for the rest of
/// the call.
const AUDIT_PERIOD: Duration = Duration::from_secs(45);

/// How long a failed track subscription holds its tongue before it asks about the encryption
/// group again: media that fails to open is a symptom, not a question to hammer the server with.
const KEY_TROUBLE_THROTTLE: Duration = Duration::from_secs(5);

/// A failed renewal is retried after this long, so the grant the call resumes with never
/// expires.
const RENEW_RETRY: Duration = Duration::from_secs(30);

/// The grant is renewed at half the join token's lifetime, never less than a minute apart.
fn renew_after(ttl: u64) -> Duration {
    Duration::from_secs((ttl / 2).max(60))
}

/// A dial the call already knows cannot succeed — a guard said no: the retake ladder takes the
/// short way, since the answer will not change.
#[derive(Debug)]
struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

fn refused(reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow::Error::new(Refused(reason.to_string()))
}

/// What the call task carries across seats and arms.
struct SeatState {
    roster: Vec<String>,
    privacy: Option<Privacy>,
    /// A guild call's keys (on an SFU the media key comes from them).
    guild: Option<GuildKeys>,
    /// This seat's self flags, so a retake joins with them.
    flags: (bool, bool),
}

/// Where a seat ended and the call goes on: `next` is where to dial next, or the call is over.
struct Seat {
    next: Option<VoiceServer>,
    state: SeatState,
}

/// Why the seat's media session ended, and what the seat does about it.
enum SeatEnd {
    /// The server pointed the call somewhere else: dial there.
    Moved(VoiceServer),
    /// The caller left (or the client dropped the call).
    Left,
    /// The server removed the seat and the roster no longer holds it: the call is over.
    Gone,
    /// The media session ended while the seat stands: retake it.
    Dropped(String),
    /// This device can no longer follow the call's encryption: a re-founded group is asked for,
    /// then the seat is retaken.
    KeysLost,
}

/// What waiting out a recovery rung came to.
enum Wait {
    /// The next attempt may run.
    Rung,
    /// The server moved the call while the ladder waited.
    Moved(VoiceServer),
    /// The caller left.
    Left,
    /// The rungs are spent: the seat is given up (the client has been told).
    Spent,
}

/// What dialling a call brought back.
struct Dialed {
    joined: rhein_proto::VoiceJoin,
    /// A conversation call's privacy state.
    privacy: Option<Privacy>,
    /// A guild call's MLS keys (on an SFU the key comes from them).
    guild: Option<GuildKeys>,
    /// Who is already there.
    here: Option<Value>,
}

/// Where a call's requests go: a guild channel's routes, or a conversation's on the home server.
#[derive(Clone)]
enum Route {
    Channel { api: Api, channel: String },
    Conversation { api: Api, id: String },
}

impl Route {
    async fn signal(&self, target: &str, payload: &Value) -> Result<Value, rhein_net::ApiError> {
        match self {
            Route::Channel { api, channel } => api.voice_signal(channel, target, payload).await,
            Route::Conversation { api, id } => api.conversation_voice_signal(id, target, payload).await,
        }
    }

    async fn leave(&self) {
        let _ = match self {
            Route::Channel { api, channel } => api.voice_leave(channel).await,
            Route::Conversation { api, id } => api.conversation_voice_leave(id).await,
        };
    }

    async fn flags(&self, muted: bool, deafened: bool) {
        let result = match self {
            Route::Channel { api, channel } => api.voice_state(channel, Some(muted), Some(deafened), None).await.map(|_| Value::Null),
            Route::Conversation { api, id } => api.conversation_voice_state(id, &serde_json::json!({ "self_mute": muted, "self_deaf": deafened })).await,
        };
        if let Err(err) = result {
            log::warn!("voice state: {err}");
        }
    }

    /// Starts (with the share's preset) or stops (`None`) streaming.
    async fn stream(&self, preset: Option<String>) {
        let on = preset.is_some();
        let _ = match self {
            Route::Channel { api, channel } => api.voice_stream(channel, on, preset.as_deref()).await,
            Route::Conversation { api, id } => api.conversation_voice_state(id, &serde_json::json!({ "streaming": on })).await,
        };
    }

    /// Guild calls record watching with the server; a conversation's has no watcher rows.
    async fn watch(&self, user: &str, on: bool) {
        if let Route::Channel { api, channel } = self
            && let Err(err) = api.voice_watch(channel, user, on, None).await
        {
            log::warn!("watch {user}: {err}");
        }
    }

    /// Makes the join again for a seat the call lost: the answer says where the call lives now
    /// (the server may have moved it, or answer with a mesh). `wants_sfu` is a conversation's
    /// posture bit; `key_package` is a guild call's fresh offer.
    async fn join(&self, flags: (bool, bool), wants_sfu: bool, key_package: Option<&str>) -> Result<rhein_proto::VoiceJoin, rhein_net::ApiError> {
        let (muted, deafened) = flags;
        match self {
            Route::Channel { api, channel } => api.voice_join(channel, muted, deafened, false, None, key_package).await,
            Route::Conversation { api, id } => api.conversation_voice_join(id, muted, deafened, wants_sfu, false).await,
        }
    }

    /// Whether the server still holds this user's seat: a removal that raced a reconnect looks
    /// like a kick until the roster says otherwise.
    async fn seat_held(&self, me: &str) -> bool {
        let answer = match self {
            Route::Channel { api, channel } => api.voice_channel(channel).await,
            Route::Conversation { api, id } => api.conversation_voice(id).await,
        };
        match answer {
            Ok(view) => view["voice_states"].as_array().is_some_and(|ss| ss.iter().any(|s| s["user_id"].as_str() == Some(me))),
            // Unreadable is not gone: the seat is retaken, and the roster settles it either way.
            Err(err) => {
                log::warn!("reading the call's roster: {err}");
                true
            }
        }
    }

    /// Asks for the call's encryption group to be re-founded, for when this device can no longer
    /// follow it (guild calls only; best effort — the server refuses while the group is re-keying
    /// happily).
    async fn reset_keys(&self) {
        if let Route::Channel { api, channel } = self
            && let Err(err) = api.voice_e2ee_reset(channel).await
        {
            log::debug!("guild call: asking for a fresh encryption group: {err}");
        }
    }

    /// Mints a fresh join token for the seat this call already holds. Only a guild channel can
    /// re-mint mid-call; a conversation's SFU session cannot be renewed.
    async fn fresh_token(&self) -> Option<String> {
        match self {
            Route::Channel { api, channel } => match api.voice_token(channel).await.ok()? {
                VoiceServer::Sfu { token, .. } => (!token.is_empty()).then_some(token),
                _ => None,
            },
            Route::Conversation { .. } => None,
        }
    }
}

/// A conversation call's privacy state, inside the call task.
struct Privacy {
    conversation: String,
    own: Posture,
    /// The hello key and its epoch.
    hello: Option<(Vec<u8>, u64)>,
    /// The media key and its epoch.
    media: Option<(Vec<u8>, u64)>,
    hellos: HashMap<String, Hello>,
    /// Who has this epoch's hello from us.
    greeted: HashSet<String>,
    counter: u64,
}

impl Privacy {
    fn new(conversation: String, own: Posture, hello: Option<(Vec<u8>, u64)>, media: Option<(Vec<u8>, u64)>) -> Privacy {
        Privacy { conversation, own, hello, media, hellos: HashMap::new(), greeted: HashSet::new(), counter: 0 }
    }

    fn epoch(&self) -> Option<u64> {
        self.hello.as_ref().map(|(_, e)| *e)
    }

    fn effective(&self, me: &str, roster: &[String]) -> Posture {
        posture::effective(self.own, me, roster, &self.hellos, self.epoch())
    }

    /// Sealed hellos for everyone in the roster who has not had this epoch's from us.
    fn greetings(&mut self, me: &str, roster: &[String]) -> Vec<(String, Value)> {
        let Some((key, epoch)) = self.hello.clone() else { return Vec::new() };
        let mut out = Vec::new();
        for user in roster {
            if user == me || !self.greeted.insert(user.clone()) {
                continue;
            }
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
            self.counter = (self.counter + 1).max(now);
            let hello = posture::seal_hello(&key, &self.conversation, me, self.own, self.counter, epoch);
            out.push((user.clone(), serde_json::json!({ "hello": hello })));
        }
        out
    }

    /// Takes in a hello; whether it changed anything.
    fn hear(&mut self, from: &str, sealed: &Value) -> bool {
        let Some((key, epoch)) = &self.hello else { return false };
        let opened = posture::open_hello(key, &self.conversation, from, Some(*epoch), sealed);
        posture::note_hello(&mut self.hellos, from, opened)
    }

    /// New keys: every hello in hand is from the last epoch now, and everyone is owed ours.
    fn rekey(&mut self, hello: Option<(Vec<u8>, u64)>, media: Option<(Vec<u8>, u64)>) {
        self.hello = hello;
        self.media = media;
        self.greeted.clear();
    }
}

fn render_image(picture: rhein_voice::video::Picture) -> Option<Arc<gpui::RenderImage>> {
    let buffer = image::RgbaImage::from_raw(picture.width, picture.height, picture.bgra)?;
    Some(Arc::new(gpui::RenderImage::new(smallvec::smallvec![image::Frame::new(buffer)])))
}

/// The media side of one call, on the tokio runtime. Reconnects when the server points the call
/// somewhere else (`VOICE_SERVER_UPDATE`).
#[allow(clippy::too_many_arguments)]
async fn drive(
    me: String,
    route: Route,
    http: reqwest::Client,
    mut server: VoiceServer,
    engine: Arc<AudioEngine>,
    mut roster: Vec<String>,
    mut privacy: Option<Privacy>,
    mut guild: Option<GuildKeys>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    updates: mpsc::UnboundedSender<Update>,
    mut flags: (bool, bool),
) {
    // Whether a call's media ever came up: only then is a failed dial a seat to retake rather
    // than the person's own join failing.
    let mut begun = false;
    // Signals leave one at a time and in order: an answer overtaking its offer is a failed call.
    let (signal_tx, mut signal_rx) = mpsc::unbounded_channel::<(String, Value)>();
    let signal_route = route.clone();
    tokio::spawn(async move {
        while let Some((target, payload)) = signal_rx.recv().await {
            if let Err(err) = signal_route.signal(&target, &payload).await {
                log::debug!("call signal to {target}: {err}");
            }
        }
    });
    // Hellos to whoever has not had one this epoch, and the posture that follows.
    let greet = |privacy: &mut Option<Privacy>, roster: &[String]| -> Option<Posture> {
        let privacy = privacy.as_mut()?;
        for (target, payload) in privacy.greetings(&me, roster) {
            let _ = signal_tx.send((target, payload));
        }
        Some(privacy.effective(&me, roster))
    };
    loop {
        let next = match server.clone() {
            VoiceServer::Mesh { ice_servers, relay, .. } => {
                let factory = PeerConnectionFactory::default();
                let microphone: MediaStreamTrack = factory.create_audio_track("microphone", engine.microphone()).into();
                // Our own strict posture pins every connection to TURN from the first offer.
                let pinned = |privacy: &Option<Privacy>, effective: Option<Posture>| {
                    privacy.as_ref().is_some_and(|p| posture::relay_only(p.own, effective.unwrap_or(p.own), relay))
                };
                // A strict posture without TURN refuses the mesh rather than expose the address; the
                // join asked for an SFU (`wants_sfu`), so the server may still point us at one.
                if privacy.as_ref().is_some_and(|p| p.own >= Posture::Relay) && !relay {
                    let _ = updates.send(Update::Failed("Your privacy setting needs a relay, and your server has none: set a custom SFU for this conversation, or lower the setting.".into()));
                    match stand_by(&mut commands, &mut roster, &mut privacy).await {
                        Some(new) => {
                            server = new;
                            continue;
                        }
                        None => return,
                    }
                }
                let effective = greet(&mut privacy, &roster);
                let (mut call, mut events) = Mesh::new(&me, factory, mesh::ice_servers(&ice_servers), pinned(&privacy, effective));
                call.set_source(Slot::Mic, Some(microphone));
                call.sync(roster.iter().map(String::as_str));
                let _ = updates.send(Update::Connected(CallMode::Mesh));
                if let Some(effective) = effective {
                    let _ = updates.send(Update::Posture(effective));
                }
                loop {
                    tokio::select! {
                        command = commands.recv() => match command {
                            Some(Command::Signal { from, payload }) => {
                                if let Some(sealed) = payload.get("hello") {
                                    // A hello rides the same relay as an offer, and is not one.
                                    if privacy.as_mut().is_some_and(|p| p.hear(&from, sealed)) {
                                        let effective = greet(&mut privacy, &roster);
                                        call.set_relay(pinned(&privacy, effective));
                                        if let Some(effective) = effective {
                                            let _ = updates.send(Update::Posture(effective));
                                        }
                                    }
                                } else {
                                    call.accept(&from, payload);
                                }
                            }
                            Some(Command::Roster(users)) => {
                                call.sync(users.iter().map(String::as_str));
                                roster = users;
                                let effective = greet(&mut privacy, &roster);
                                call.set_relay(pinned(&privacy, effective));
                                if let Some(effective) = effective {
                                    let _ = updates.send(Update::Posture(effective));
                                }
                            }
                            Some(Command::Keys { hello, media }) => {
                                if let Some(p) = privacy.as_mut() {
                                    p.rekey(hello, media);
                                }
                                let effective = greet(&mut privacy, &roster);
                                call.set_relay(pinned(&privacy, effective));
                            }
                            // The server repeats the join's answer over the gateway: not a move.
                            Some(Command::Server(new)) if same_place(&server, &new) => {}
                            Some(Command::Server(new)) => break Some(new),
                            // A mesh guild call is DTLS peer to peer; its group matters on an SFU.
                            Some(Command::Watch { .. } | Command::GuildE2ee(_) | Command::GuildRoster(_)) => {}
                            Some(Command::Flags { muted, deafened }) => flags = (muted, deafened),
                            Some(Command::Publish { slot, video }) => {
                                let track: MediaStreamTrack = call.factory().create_video_track(if slot == Slot::Screen { "screen" } else { "camera" }, video.source()).into();
                                call.set_source(slot, Some(track));
                            }
                            Some(Command::Unpublish(slot)) => call.set_source(slot, None),
                            Some(Command::Leave) | None => break None,
                        },
                        Some(event) = events.recv() => match event {
                            CallEvent::Signal { target, payload } => { let _ = signal_tx.send((target, payload)); }
                            event => media(&engine, &updates, event),
                        },
                    }
                }
            }
            VoiceServer::Sfu { .. } => {
                let seat = sfu_seat(
                    &me,
                    &route,
                    &http,
                    server.clone(),
                    &engine,
                    SeatState {
                        roster: std::mem::take(&mut roster),
                        privacy: privacy.take(),
                        guild: guild.take(),
                        flags,
                    },
                    signal_tx.clone(),
                    &mut commands,
                    &updates,
                    &mut begun,
                )
                .await;
                roster = seat.state.roster;
                privacy = seat.state.privacy;
                guild = seat.state.guild;
                flags = seat.state.flags;
                seat.next
            }
        };
        match next {
            Some(new) => server = new,
            None => return,
        }
    }
}

/// A call that cannot connect where it is waits for the server to point it elsewhere, or for the
/// hang-up, keeping its roster and keys current meanwhile.
async fn stand_by(
    commands: &mut mpsc::UnboundedReceiver<Command>,
    roster: &mut Vec<String>,
    privacy: &mut Option<Privacy>,
) -> Option<VoiceServer> {
    loop {
        match commands.recv().await {
            Some(Command::Server(new)) => return Some(new),
            Some(Command::Leave) | None => return None,
            Some(Command::Roster(users)) => *roster = users,
            Some(Command::Keys { hello, media }) => {
                if let Some(p) = privacy.as_mut() {
                    p.rekey(hello, media);
                }
            }
            _ => {}
        }
    }
}

/// The guards and the dial of one SFU seat: who may join, and the room they join.
async fn dial(
    http: &reqwest::Client,
    endpoint: &str,
    token: &str,
    custom: bool,
    required: Option<&str>,
    privacy: &Option<Privacy>,
    guild: &Option<GuildKeys>,
    engine: &Arc<AudioEngine>,
) -> Result<(SfuCall, mpsc::UnboundedReceiver<CallEvent>, Option<KeyProvider>)> {
    // A conversation's SFU never gets plaintext: no media key, no call.
    let keys = match privacy {
        Some(p) => match &p.media {
            Some((key, epoch)) => Ok(Some(e2ee::key_provider(key, *epoch))),
            None => Err(refused("This device has no keys for this conversation yet, so it cannot join an encrypted call.")),
        },
        // A guild call's key comes from its own group. Until the welcome arrives the provider
        // holds a key nobody has, so nothing is heard and nothing goes out readable.
        None if guild.is_some() => {
            let (key, epoch) = guild
                .as_ref()
                .and_then(|g| g.key())
                .unwrap_or_else(|| (rhein_mls::vault::rand_bytes::<32>().to_vec(), 0));
            Ok(Some(e2ee::key_provider(&key, epoch)))
        }
        None if required.is_some() => Err(refused("This call must be end-to-end encrypted, and this device has no key to offer it yet.")),
        // Every guild call on an SFU is end-to-end encrypted under its own group. An answer
        // without one would carry the call in the clear: refused, as the web client does.
        None => Err(refused("This server offered the call through an SFU without end-to-end encryption, so Rhein does not join it.")),
    };
    match keys {
        Err(err) => Err(err),
        Ok(_) if custom && endpoint.is_empty() => Err(refused("This conversation uses its own SFU, whose address this device does not know yet.")),
        Ok(provider) => {
            let session = sfu::session(http, endpoint, token).await?;
            SfuCall::connect(session, engine.microphone(), provider.clone()).await.map(|(call, events)| (call, events, provider))
        }
    }
}

/// Whether a failed dial can hope for a different answer: guards that already know the call
/// cannot be dialled said so, and an SFU that answered no did too.
fn dial_answered_no(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Refused>().is_some()
        || err.downcast_ref::<sfu::SessionError>().is_some_and(|e| e.answered_no())
        || err.downcast_ref::<rhein_net::ApiError>().is_some_and(|e| !retake::join_unanswered(e))
}

/// Waits out the next rung of the retake ladder — `attempt` is the number of failed attempts of
/// this recovery so far — while the caller may still escape (another channel, a hang-up) or the
/// server may move the call.
async fn wait_out(
    rungs: &'static [Duration],
    attempt: usize,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    state: &mut SeatState,
) -> Wait {
    let Some(wait) = attempt.checked_sub(1).and_then(|i| rungs.get(i)) else {
        return Wait::Spent;
    };
    let deadline = tokio::time::Instant::now() + *wait;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return Wait::Rung,
            command = commands.recv() => match command {
                Some(Command::Server(new)) => return Wait::Moved(new),
                Some(Command::Leave) | None => return Wait::Left,
                Some(Command::Roster(users)) => state.roster = users,
                Some(Command::Keys { hello, media }) => {
                    if let Some(p) = state.privacy.as_mut() {
                        p.rekey(hello, media);
                    }
                }
                _ => {}
            },
        }
    }
}

/// Mints a fresh join token for the seat and installs its grant on the live room, so the first
/// network blip after the old grant's expiry is a resume, not a full reconnect.
async fn renew_token(route: &Route, http: &reqwest::Client, endpoint: &str, call: &mut SfuCall) -> Result<()> {
    let Some(join_token) = route.fresh_token().await else {
        anyhow::bail!("the server would not mint a fresh join token for this session");
    };
    let session = sfu::session(http, endpoint, &join_token).await?;
    call.install_session(session);
    Ok(())
}

/// The media side of one seat on an SFU, from its first dial to the seat's end. A standing seat
/// that loses its media is retaken: the join is remade on the retake ladder, so an SFU that is
/// restarting is outlived instead of ejecting everyone from the channel — and a server that
/// answered no gets the short ladder, since the answer will not change.
#[allow(clippy::too_many_arguments)]
async fn sfu_seat(
    me: &str,
    route: &Route,
    http: &reqwest::Client,
    mut server: VoiceServer,
    engine: &Arc<AudioEngine>,
    mut state: SeatState,
    signal_tx: mpsc::UnboundedSender<(String, Value)>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    updates: &mpsc::UnboundedSender<Update>,
    begun: &mut bool,
) -> Seat {
    // Hellos to whoever has not had one this epoch, and the posture that follows.
    let greet = |state: &mut SeatState| -> Option<Posture> {
        let privacy = state.privacy.as_mut()?;
        for (target, payload) in privacy.greetings(me, &state.roster) {
            let _ = signal_tx.send((target, payload));
        }
        Some(privacy.effective(me, &state.roster))
    };
    // Whether the seat is made by the join in hand (`server` is its answer) or has to be remade
    // first: from the first connection on, a standing seat is always remade.
    let mut needs_join = false;
    // Failed attempts of the recovery in progress; a recovery that connected starts fresh.
    let mut attempt = 0usize;
    loop {
        if needs_join {
            let _ = updates.send(Update::Connecting);
            server = 'join: loop {
                let package = state.guild.as_mut().and_then(|k| k.offer().ok());
                let wants_sfu = state.privacy.as_ref().is_some_and(|p| posture::wants_sfu(p.own, false));
                match route.join(state.flags, wants_sfu, package.as_deref()).await {
                    Ok(joined) => {
                        // The group may have moved on while the seat was gone.
                        if let (Some(keys), Some(info)) = (state.guild.as_mut(), joined.e2ee.clone())
                            && let Err(err) = keys.begin(&info).await
                        {
                            // A device nobody vouched for: leave, and say why.
                            let _ = updates.send(Update::Ended(Some(format!("{err:#}"))));
                            return Seat { next: None, state };
                        }
                        if !matches!(joined.voice_server, VoiceServer::Sfu { .. }) {
                            // The call does not live on an SFU any more (a conversation's
                            // sequencer may answer with a mesh): the drive takes that arm.
                            return Seat { next: Some(joined.voice_server), state };
                        }
                        break 'join joined.voice_server;
                    }
                    Err(err) => {
                        log::warn!("remaking the seat: {err}");
                        attempt += 1;
                        match wait_out(retake::ladder(&err), attempt, commands, &mut state).await {
                            Wait::Rung => continue 'join,
                            Wait::Moved(new) => return Seat { next: Some(new), state },
                            Wait::Left => return Seat { next: None, state },
                            Wait::Spent => {
                                let _ = updates.send(Update::Ended(Some(format!("Could not rejoin the call: {err}"))));
                                return Seat { next: None, state };
                            }
                        }
                    }
                }
            };
        }
        let VoiceServer::Sfu { endpoint, token, region, custom, e2ee: required, token_ttl, .. } = server.clone() else {
            return Seat { next: Some(server), state };
        };
        let connected = dial(http, &endpoint, &token, custom, required.as_deref(), &state.privacy, &state.guild, engine).await;
        let (mut call, mut events, provider) = match connected {
            Ok(connected) => {
                *begun = true;
                let _ = updates.send(Update::Connected(CallMode::Sfu { region }));
                if let Some(effective) = greet(&mut state) {
                    let _ = updates.send(Update::Posture(effective));
                }
                connected
            }
            Err(err) => {
                if !*begun {
                    // The first dial of the call is the person's own attempt: say it failed and
                    // wait for the server to point the call elsewhere.
                    log::warn!("joining the SFU call: {err:#}");
                    let _ = updates.send(Update::Failed(format!("{err:#}")));
                    let next = stand_by(commands, &mut state.roster, &mut state.privacy).await;
                    return Seat { next, state };
                }
                log::warn!("dialling the SFU call: {err:#}");
                attempt += 1;
                let rungs = if dial_answered_no(&err) { &retake::RETAKE_BACKOFF[..] } else { &retake::RETAKE_UNREACHABLE_BACKOFF[..] };
                match wait_out(rungs, attempt, commands, &mut state).await {
                    Wait::Rung => continue,
                    Wait::Moved(new) => return Seat { next: Some(new), state },
                    Wait::Left => return Seat { next: None, state },
                    Wait::Spent => {
                        let _ = updates.send(Update::Ended(Some(format!("Could not rejoin the call: {err:#}"))));
                        return Seat { next: None, state };
                    }
                }
            }
        };

        // A guild call asks its encryption group where it stands now and then, and renews its
        // join token at half its lifetime; a conversation's session can do neither.
        let mut audit_at = state.guild.as_ref().map(|_| tokio::time::Instant::now() + AUDIT_PERIOD);
        let mut renew_at = token_ttl.filter(|_| state.guild.is_some()).map(|ttl| tokio::time::Instant::now() + renew_after(ttl));
        let mut key_check_at = tokio::time::Instant::now();
        // Every way out of the loop breaks with a reason.
        let end = loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(Command::Watch { user, on }) => call.watch(&user, on),
                    Some(Command::Publish { slot, video }) => {
                        if let Err(err) = call.publish_video(slot, &video).await {
                            let _ = updates.send(Update::Failed(format!("{err:#}")));
                        }
                    }
                    Some(Command::Unpublish(slot)) => call.unpublish(slot).await,
                    Some(Command::Keys { hello, media }) => {
                        // An epoch change rekeys; the ring keeps the last keys for a peer a
                        // commit behind.
                        if let (Some(provider), Some((key, epoch))) = (&provider, &media) {
                            call.rekey(provider, key, *epoch);
                        }
                        if let Some(p) = state.privacy.as_mut() {
                            p.rekey(hello, media);
                        }
                        greet(&mut state);
                    }
                    Some(Command::Flags { muted, deafened }) => state.flags = (muted, deafened),
                    Some(Command::Server(new)) if same_place(&server, &new) => {}
                    Some(Command::Server(new)) => break SeatEnd::Moved(new),
                    Some(Command::Signal { from, payload }) => {
                        if let Some(sealed) = payload.get("hello")
                            && state.privacy.as_mut().is_some_and(|p| p.hear(&from, sealed))
                            && let Some(effective) = greet(&mut state)
                        {
                            let _ = updates.send(Update::Posture(effective));
                        }
                    }
                    Some(Command::Roster(users)) => {
                        state.roster = users;
                        greet(&mut state);
                    }
                    Some(Command::GuildE2ee(event)) => {
                        log::debug!("guild call: {} for epoch {}", event["op"], event["epoch"]);
                        let heard = match state.guild.as_mut() {
                            Some(keys) => match keys.hear(&event).await {
                                // Behind the group (a missed reset or commit): ask where it
                                // stands now, not at the next roster move.
                                Ok(Heard::Behind) => keys.check().await.map(|key| key.map_or(Heard::Same, Heard::Keyed)),
                                other => other,
                            },
                            None => Ok(Heard::Same),
                        };
                        match heard {
                            Ok(Heard::Keyed((key, epoch))) => {
                                log::debug!("guild call: key for epoch {epoch}");
                                if let Some(provider) = &provider {
                                    call.rekey(provider, &key, epoch);
                                }
                            }
                            // The committer will never add this session's package: a fresh group
                            // is asked for, and the seat retaken with a fresh one.
                            Ok(Heard::Left) => break SeatEnd::KeysLost,
                            Ok(_) => {}
                            // This device cannot follow the group any more: a fresh one is asked
                            // for, and the seat retaken.
                            Err(err) => {
                                log::warn!("guild call keys: {err:#}");
                                break SeatEnd::KeysLost;
                            }
                        }
                    }
                    Some(Command::GuildRoster(states)) => {
                        if let Some(keys) = state.guild.as_mut() {
                            match keys.tend(&states).await {
                                Ok(Some((key, epoch))) => {
                                    if let Some(provider) = &provider {
                                        call.rekey(provider, &key, epoch);
                                    }
                                }
                                Ok(None) => {}
                                Err(err) => log::warn!("guild call keys: {err:#}"),
                            }
                        }
                    }
                    Some(Command::Leave) | None => break SeatEnd::Left,
                },
                Some(event) = events.recv() => match event {
                    CallEvent::Removed => {
                        // The server tore this seat down: a moderator, or the sweeper racing a
                        // reconnect. The roster decides between rejoining and leaving; staying
                        // connected to a dead room is the one wrong reaction.
                        break if route.seat_held(me).await {
                            SeatEnd::Dropped("the server ended this call's media session".into())
                        } else {
                            SeatEnd::Gone
                        };
                    }
                    CallEvent::Dropped(reason) => break SeatEnd::Dropped(reason),
                    CallEvent::KeyTrouble { .. } => {
                        // This device is opening nobody's media: it is probably a commit behind.
                        // Ask the group where it stands, but not on every failed track.
                        if tokio::time::Instant::now() >= key_check_at
                            && let Some(keys) = state.guild.as_mut()
                        {
                            key_check_at = tokio::time::Instant::now() + KEY_TROUBLE_THROTTLE;
                            match keys.check().await {
                                Ok(Some((key, epoch))) => {
                                    if let Some(provider) = &provider {
                                        call.rekey(provider, &key, epoch);
                                    }
                                }
                                Ok(None) => {}
                                Err(err) => {
                                    log::warn!("guild call keys: {err:#}");
                                    break SeatEnd::KeysLost;
                                }
                            }
                        }
                    }
                    event => media(engine, updates, event),
                },
                // A missed commit does not wait for a roster move to be noticed.
                _ = tokio::time::sleep_until(audit_at.unwrap_or_else(tokio::time::Instant::now)), if audit_at.is_some() => {
                    audit_at = Some(tokio::time::Instant::now() + AUDIT_PERIOD);
                    if let Some(keys) = state.guild.as_mut() {
                        match keys.check().await {
                            Ok(Some((key, epoch))) => {
                                if let Some(provider) = &provider {
                                    call.rekey(provider, &key, epoch);
                                }
                            }
                            Ok(None) => {}
                            Err(err) => {
                                log::warn!("guild call keys: {err:#}");
                                break SeatEnd::KeysLost;
                            }
                        }
                    }
                }
                // The grant is kept fresh, so the first blip after its expiry is a resume.
                _ = tokio::time::sleep_until(renew_at.unwrap_or_else(tokio::time::Instant::now)), if renew_at.is_some() => {
                    renew_at = Some(tokio::time::Instant::now() + RENEW_RETRY);
                    match renew_token(route, http, &endpoint, &mut call).await {
                        Ok(()) => {
                            if let Some(ttl) = token_ttl {
                                renew_at = Some(tokio::time::Instant::now() + renew_after(ttl));
                            }
                        }
                        Err(err) => log::warn!("renewing the call's join token: {err:#}"),
                    }
                }
            }
        };
        call.close().await;
        match end {
            SeatEnd::Moved(new) => return Seat { next: Some(new), state },
            SeatEnd::Left => return Seat { next: None, state },
            SeatEnd::Gone => {
                let _ = updates.send(Update::Ended(Some("You were removed from the voice channel.".into())));
                return Seat { next: None, state };
            }
            SeatEnd::KeysLost => {
                // This device can no longer follow the group, and the group used to rebuild only
                // once its last member had left — so one stuck device stranded everyone's media.
                // Ask for a re-founded one; the server refuses while the call is re-keying
                // happily, and the retake still repairs this device alone.
                route.reset_keys().await;
                attempt = 0;
                needs_join = true;
            }
            SeatEnd::Dropped(reason) => {
                log::warn!("the call's media session ended: {reason}");
                attempt = 0;
                needs_join = true;
            }
        }
    }
}

/// Whether a server update points where the call already is (same mode, same SFU).
fn same_place(now: &VoiceServer, new: &VoiceServer) -> bool {
    match (now, new) {
        (VoiceServer::Mesh { .. }, VoiceServer::Mesh { .. }) => true,
        (VoiceServer::Sfu { endpoint: a, .. }, VoiceServer::Sfu { endpoint: b, .. }) => a == b,
        _ => false,
    }
}

/// What a call's media events mean for the engine and the UI.
fn media(engine: &AudioEngine, updates: &mpsc::UnboundedSender<Update>, event: CallEvent) {
    match event {
        CallEvent::Track { from, slot: Slot::Mic, track: MediaStreamTrack::Audio(track) } => engine.add_voice(&from, track),
        CallEvent::Track { from, slot: Slot::ScreenAudio, track: MediaStreamTrack::Audio(track) } => engine.add_voice(&format!("{from}#screen"), track),
        CallEvent::TrackEnded { from, slot: Slot::Mic } => engine.remove_voice(&from),
        CallEvent::TrackEnded { from, slot: Slot::ScreenAudio } => engine.remove_voice(&format!("{from}#screen")),
        CallEvent::Track { from, slot: slot @ (Slot::Camera | Slot::Screen), track: MediaStreamTrack::Video(track) } => {
            let feed = VideoFeed::start(track, if slot == Slot::Screen { 1920 } else { 640 }, 30);
            let _ = updates.send(Update::Video { user: from, slot, feed: Some(feed) });
        }
        CallEvent::TrackEnded { from, slot: slot @ (Slot::Camera | Slot::Screen) } => {
            let _ = updates.send(Update::Video { user: from, slot, feed: None });
        }
        CallEvent::Link { user, up } => {
            let _ = updates.send(Update::Link { user, up });
        }
        CallEvent::Error(err) => log::warn!("call: {err}"),
        _ => {}
    }
}
