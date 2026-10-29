//! `Client`: the one entity that holds everything the UI shows — the home session, every connected
//! guild server, their guilds, channel histories and member lists — and owns all network work.
//!
//! Views read fields and call methods; every method that talks to a server hands its future to
//! [`Io`] and applies the answer in `this.update`, ending with `cx.notify()`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, SharedString, Task, WeakEntity};
use rhein_auth::{DeviceKey, SerenityKsf};
use rhein_net::gateway::{self, GatewayConfig};
use rhein_net::{Api, GatewayEvent, GatewayHandle, GatewayStatus, auth, discover};
use rhein_proto::{
    Attachment, Channel, Discovery, Event, Guild, Member, Message, PresenceState, Ready, SendMessage,
    USER_INTENTS, close, localpart,
};

use crate::io::{Io, join};

pub mod banners;
mod call;
mod retake;
mod dm_view;
mod people;
mod account;
mod account_prefs;
mod covers;
mod gifs;
mod notify;
mod signals;
mod media;
mod movie;
mod posters;
mod preload;
mod sound;
pub mod update;
pub use gifs::{GifShare, MediaProbe};
pub use media::{DownloadView, MediaSource, MediaTick, free_download_path, safe_file_name};
pub use movie::{MoviePlayer, MovieView};
pub use posters::{MoviePosters, Poster};
pub use sound::SoundView;
pub use call::{ActiveCall, CallMode, CallSpeaking, CallStatus, CallVideo, VoicePrefs};
pub use rhein_voice::Slot;
use crate::persist::{self, Saved};

const PAGE: u32 = 50;
const TYPING_SHOWN_FOR: Duration = Duration::from_secs(8);
/// How long a passing notice (a friend request, say) stays up.
const NOTICE_SHOWN_FOR: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Looking for a saved session at startup.
    Restoring,
    SignedOut,
    SigningIn,
    SignedIn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerStatus {
    Connecting,
    Ready,
    Reconnecting,
    Renewing,
    Incompatible,
    Failed,
}

impl ServerStatus {
    pub fn label(self) -> &'static str {
        match self {
            ServerStatus::Connecting => "Connecting…",
            ServerStatus::Ready => "Connected",
            ServerStatus::Reconnecting => "Connection lost, retrying…",
            ServerStatus::Renewing => "Renewing session…",
            ServerStatus::Incompatible => "Incompatible protocol version",
            ServerStatus::Failed => "Unreachable",
        }
    }
}

/// How long before a guild session ends it is renewed.
const RENEW_BEFORE: Duration = Duration::from_secs(5 * 60);

pub struct Home {
    pub input: String,
    pub doc: Discovery,
    pub api: Api,
    pub user_id: String,
    pub device_id: String,
    pub display_name: Option<String>,
    pub avatar_ref: Option<String>,
    device_seed: String,
    expires_at: Option<i64>,
    /// The OPAQUE-derived key that opens the DM recovery vault; only after a password sign-in in
    /// this session, never stored.
    dm_wrapper: Option<rhein_mls::vault::Key>,
}

pub struct Server {
    /// The logical server name (the audience of its assertions).
    pub name: String,
    /// What it was added as, for persistence.
    pub input: String,
    pub doc: Option<Discovery>,
    pub api: Option<Api>,
    pub status: ServerStatus,
    pub error: Option<String>,
    /// Round trip of the last gateway heartbeat; cleared while disconnected.
    pub latency: Option<std::time::Duration>,
    /// The last close code the gateway got, and how many closes there were (diagnostics).
    pub last_close: Option<u16>,
    pub closes: u32,
    gateway: Option<GatewayHandle>,
    connect: Option<Task<()>>,
}

pub struct GuildEntry {
    pub server: String,
    pub guild: Guild,
}

impl GuildEntry {
    /// Channels in display order: uncategorised first, then each category followed by its
    /// children, everything by `position`.
    pub fn ordered_channels(&self) -> Vec<&Channel> {
        let mut chans: Vec<&Channel> = self.guild.channels.iter().filter(|c| !c.is_thread()).collect();
        chans.sort_by(|a, b| (a.position, &a.id).cmp(&(b.position, &b.id)));
        let mut out = Vec::with_capacity(chans.len());
        for c in chans.iter().filter(|c| c.parent_id.is_none() && !c.is_category()) {
            out.push(*c);
        }
        for cat in chans.iter().filter(|c| c.is_category()) {
            out.push(*cat);
            for c in chans.iter().filter(|c| c.parent_id.as_deref() == Some(cat.id.as_str())) {
                out.push(*c);
            }
        }
        // Children whose category is not visible to us.
        let placed: HashSet<&str> = out.iter().map(|c| c.id.as_str()).collect();
        let orphans: Vec<&Channel> = chans.iter().filter(|c| !placed.contains(c.id.as_str())).copied().collect();
        out.extend(orphans);
        out
    }
}

#[derive(Default)]
pub struct ChannelLog {
    pub messages: Vec<Message>,
    pub loaded: bool,
    pub loading: bool,
    pub reached_start: bool,
    pub error: Option<String>,
    /// Pending messages the server refused; they stay visible with Retry / Discard.
    pub failed: std::collections::HashSet<String>,
    task: Option<Task<()>>,
}

/// How much of a channel this user has not read yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadMark {
    pub unread: u32,
    pub mentions: u32,
    pub muted: bool,
    /// The newest message the server says we read.
    pub last_read: Option<String>,
    /// The first message counted unread live, when the server's cursor is unknown.
    pub first_unread: Option<String>,
}

/// Where the "New messages" line goes in a channel just opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Divider {
    /// Before the first message newer than this id.
    After(String),
    /// Before this message.
    At(String),
}

/// A file waiting to go out with the next message.
pub struct Staged {
    pub id: u64,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub state: StagedState,
    /// For pasted images: the bytes to preview before the upload has a URL.
    pub preview: Option<std::sync::Arc<gpui::Image>>,
}

pub enum StagedState {
    Uploading,
    Ready(Box<Attachment>),
    /// For an encrypted DM: the bytes stay on this device until they are sealed and sent.
    Local(std::sync::Arc<Vec<u8>>),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FriendAction {
    Request,
    Accept,
    /// Remove a friend, decline an incoming request or cancel an outgoing one.
    Remove,
    Block,
    Unblock,
}

#[derive(Debug, Clone, Default)]
pub struct SearchState {
    pub guild_id: String,
    pub query: rhein_proto::SearchQuery,
    pub results: Vec<Message>,
    pub total: u32,
    pub has_more: bool,
    pub loading: bool,
    pub error: Option<String>,
}

pub enum EmbedState {
    Loading,
    Ready(crate::embed::Embed),
    /// Nothing to show; asked again after `retry_at`.
    Empty { retry_at: Instant },
}

impl ReadMark {
    pub fn is_unread(&self) -> bool {
        self.unread > 0 && !self.muted
    }
}

/// What changed in a channel's message list, so the view can keep its scroll position.
#[derive(Debug, Clone)]
pub enum ClientEvent {
    MessagesReset { channel_id: String },
    MessagesPrepended { channel_id: String, count: usize },
    MessageAppended { channel_id: String },
    MessageChanged { channel_id: String, index: usize },
    MessageRemoved { channel_id: String, index: usize },
    /// Scroll to and highlight a message.
    Jump { channel_id: String, index: usize },
    /// A thread was created from this client and should open.
    OpenThread { thread_id: String },
    Notice(SharedString),
}

/// A form a bot opened for us (`MODAL_OPEN`): text inputs, answered once.
#[derive(Debug, Clone, PartialEq)]
pub struct BotModal {
    pub interaction_id: String,
    pub channel_id: String,
    pub title: String,
    pub fields: Vec<ModalField>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModalField {
    pub custom_id: String,
    pub label: String,
    pub value: String,
    pub required: bool,
    pub min: Option<u32>,
    pub max: Option<u32>,
}

impl BotModal {
    /// Reads `MODAL_OPEN`: the interaction it answers and the form, given either flat or under
    /// `data` as the bot sent it.
    pub fn parse(d: &serde_json::Value) -> Option<BotModal> {
        let form = if d.get("components").is_some() { d } else { d.get("data")? };
        let interaction_id = d["interaction_id"].as_str().or(d["id"].as_str())?.to_owned();
        let rows: Vec<rhein_proto::Component> = serde_json::from_value(form["components"].clone()).ok()?;
        let fields: Vec<ModalField> = rows
            .iter()
            .flat_map(|row| if row.kind == "action_row" { row.components.clone() } else { vec![row.clone()] })
            .filter(|c| c.kind == "text_input")
            .filter_map(|c| {
                Some(ModalField {
                    custom_id: c.custom_id.clone()?,
                    label: c.label.clone().unwrap_or_default(),
                    value: c.value.clone().unwrap_or_default(),
                    required: c.required,
                    min: c.min,
                    max: c.max,
                })
            })
            .take(5)
            .collect();
        (!fields.is_empty()).then(|| BotModal {
            interaction_id,
            channel_id: d["channel_id"].as_str().unwrap_or_default().to_owned(),
            title: form["title"].as_str().unwrap_or("A form").to_owned(),
            fields,
        })
    }
}

pub struct Client {
    io: Io,
    http: reqwest::Client,
    pub phase: Phase,
    pub error: Option<SharedString>,
    pub notice: Option<SharedString>,
    /// Something missing that the person can fix themselves (a package to install).
    pub warning: Option<SharedString>,
    pub home: Option<Home>,
    pub servers: Vec<Server>,
    pub guilds: Vec<GuildEntry>,
    pub selected_guild: Option<String>,
    /// The home view (direct messages) is showing instead of a guild.
    pub showing_home: bool,
    read: HashMap<String, ReadMark>,
    dividers: HashMap<String, Divider>,
    staged: HashMap<String, Vec<Staged>>,
    embeds: HashMap<String, EmbedState>,
    /// The activities this client shares: `None` while sharing is off, so another session's
    /// (the Telium desktop app's, say) stay untouched.
    sharing: Option<Vec<rhein_proto::Activity>>,
    /// The local activity sources while sharing is on, and the task that follows them.
    rpc: Option<(rhein_rpc::Handle, Task<()>)>,
    /// How far encrypted-DM keys and trust came up for this account.
    pub dm: Option<crate::dm::DmStatus>,
    pub dm_error: Option<String>,
    dm_task: Option<Task<()>>,
    dm_engine: Option<dm_view::SharedEngine>,
    /// While adding an account: the one we came from (user id, name), to go back to.
    pub returning_to: Option<(String, String)>,
    /// Everything the sources found, before the sharing preferences filter it.
    pub local_activities: Vec<rhein_proto::Activity>,
    pub rpc_status: rhein_rpc::Status,
    /// Activity artwork re-hosted by one of our servers' camo caches, by original URL.
    artwork: HashMap<String, EmbedState>,
    /// The voice call we are in.
    call: Option<call::ActiveCall>,
    /// The account's shared preferences (`prefs.v1`), as last read or written.
    account_prefs: serde_json::Value,
    /// Whether the window is in front (alerts come only while it is not).
    window_active: bool,
    /// Sounds, the ringing call and their pacing.
    alerts: notify::Alerts,
    /// Conversations we told we are typing in, until we say we stopped.
    typing_sent: std::collections::HashSet<String>,
    /// The latest `MIGRATION_ALERT`: a move of this account or an owned guild is scheduled.
    pub migration_alert: Option<serde_json::Value>,
    /// When we last sent in each channel, for slowmode.
    last_sent: HashMap<String, Instant>,
    /// Bot controls we pressed and wait to hear back about: `(message id, custom id)`.
    pub pressing: HashSet<(String, String)>,
    /// Messages whose bot took a press and answers later.
    pub deferred: HashSet<String>,
    /// A form a bot asked us to fill in, until the shell shows it.
    pub modal: Option<BotModal>,
    /// Banners closed in this session (by key).
    dismissed_banners: HashSet<String>,
    /// A newer release of Rhein, from looking for one to installing it.
    pub update: update::Update,
    /// Where this copy may install updates; `None` when it can only announce them.
    update_target: Option<rhein_update::Target>,
    /// The voice channel whose chat column is open (its messages count as read only then).
    voice_chat_open: Option<String>,
    /// Per conversation, our newest message someone else has read.
    dm_seen: HashMap<String, String>,
    /// The read receipt waiting to go out.
    read_flush: Option<Task<()>>,
    /// Local covers of shared activities, uploaded to the home server.
    covers: covers::Covers,
    /// No input to the window for a while: shown as idle (see `effective_status`).
    auto_idle: bool,
    /// The account's favourite GIFs (from the sealed sync store), newest first.
    gif_favorites: Vec<String>,
    /// GIF share pages (Tenor, Giphy, Klipy) and the media URL each turned out to be.
    gif_shares: HashMap<String, gifs::GifShare>,
    /// What Telium media links serve, as their origins said.
    media_kinds: HashMap<String, gifs::MediaProbe>,
    /// Mutual friends by person, as their home server answered (`None` while asking).
    mutual_friends: HashMap<String, Option<Vec<String>>>,
    /// The relay proxies of servers reached through "Protect my IP", by server key.
    relays: HashMap<String, Arc<rhein_net::tunnel::RelayProxy>>,
    /// Servers the person chose to reach directly this session although protected, after the
    /// relay failed.
    direct_once: HashSet<String>,
    /// Servers whose last connection through the relay failed (they may be offered directly).
    pub relay_failed: HashSet<String>,
    /// The mic test, while it runs.
    mic_test: Option<call::MicTest>,
    /// A call to start once the conversation being opened is there (`call_user`).
    call_when_open: Option<String>,
    /// Guild refreshes under way, and whether another change came in meanwhile.
    guild_refreshes: HashMap<String, bool>,
    sync: Option<std::sync::Arc<tokio::sync::Mutex<crate::sync_store::SyncStore>>>,
    /// The sound file loaded from a message.
    sound: Option<sound::Sound>,
    /// Attachments being saved to the downloads folder, by message id and attachment index.
    downloads: HashMap<String, media::Download>,
    /// `RHEIN_AUDIO=null`: calls open ALSA's null devices.
    null_audio: bool,
    /// Whether the microphone was already muted when deafening muted it, so undeafening leaves
    /// it as it was.
    muted_before_deafen: bool,
    speaking_task: Option<Task<()>>,
    /// The call's pictures and who in it is speaking, each its own entity: they change many
    /// times a second, and only the views showing them should redraw.
    pub call_video: Entity<CallVideo>,
    pub call_speaking: Entity<CallSpeaking>,
    pub media_tick: Entity<MediaTick>,
    /// The video playing from a message, apart from the client for the same reason.
    pub movie_player: Entity<MoviePlayer>,
    /// The videos' posters, apart from the client so one arriving redraws only video rows.
    pub movie_posters: Entity<MoviePosters>,
    /// Renews guild sessions before they run out (every minute, see `freshen_sessions`).
    renew_ticker: Option<Task<()>>,
    /// guild id → user id → their voice state (only users in a channel).
    voice: HashMap<String, HashMap<String, rhein_proto::VoiceState>>,
    selected_channel: HashMap<String, String>,
    pub logs: HashMap<String, ChannelLog>,
    pub members: HashMap<String, Vec<Member>>,
    /// guild id → user id → index in `members`, kept with it (see `set_members`).
    member_ix: HashMap<String, HashMap<String, usize>>,
    pins: HashMap<String, Vec<Message>>,
    threads: HashMap<String, Vec<rhein_proto::Thread>>,
    emoji: HashMap<String, Vec<rhein_proto::CustomEmoji>>,
    pub search: Option<SearchState>,
    member_lookup: HashMap<String, Vec<Member>>,
    pub recent_emoji: Vec<String>,
    pub prefs: crate::prefs::Prefs,
    pub friends: Vec<rhein_proto::Friend>,
    pub conversations: Vec<rhein_proto::Conversation>,
    /// Conversations with a frame newer than the last time they were opened.
    unread_conversations: std::collections::HashSet<String>,
    /// What the home view shows: `None` = friends, `Some(id)` = a conversation.
    pub home_conversation: Option<String>,
    profiles: HashMap<String, Option<rhein_proto::Profile>>,
    /// Channels in the order they were opened, newest first (for the quick switcher).
    pub visited: Vec<String>,
    /// Channels' newest pages fetched before they are opened.
    preload: preload::Preload,
    typing: HashMap<String, Vec<(String, Instant)>>,
    /// Drops expired typing entries and repaints once they are gone; one for all of them.
    typing_sweep: Option<Task<()>>,
    signin: Option<Task<()>>,
    /// The last sign-in stopped for a two-factor code.
    pub mfa_required: bool,
}

impl EventEmitter<ClientEvent> for Client {}

impl Client {
    pub fn new(io: Io, cx: &mut Context<Self>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("Rhein/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("http client");
        let mut this = Self {
            io,
            http,
            phase: Phase::Restoring,
            error: None,
            notice: None,
            warning: None,
            home: None,
            servers: Vec::new(),
            guilds: Vec::new(),
            selected_guild: None,
            selected_channel: HashMap::new(),
            logs: HashMap::new(),
            members: HashMap::new(),
            member_ix: HashMap::new(),
            pins: HashMap::new(),
            threads: HashMap::new(),
            emoji: HashMap::new(),
            search: None,
            member_lookup: HashMap::new(),
            recent_emoji: crate::prefs::load().recent_emoji,
            prefs: crate::prefs::load(),
            friends: Vec::new(),
            conversations: Vec::new(),
            unread_conversations: Default::default(),
            home_conversation: None,
            profiles: HashMap::new(),
            visited: Vec::new(),
            preload: Default::default(),
            typing: HashMap::new(),
            typing_sweep: None,
            showing_home: false,
            read: HashMap::new(),
            dividers: HashMap::new(),
            staged: HashMap::new(),
            embeds: HashMap::new(),
            sharing: None,
            rpc: None,
            returning_to: None,
            dm: None,
            dm_error: None,
            dm_task: None,
            dm_engine: None,
            local_activities: Vec::new(),
            rpc_status: rhein_rpc::Status::default(),
            artwork: HashMap::new(),
            voice: HashMap::new(),
            call: None,
            sound: None,
            auto_idle: false,
            covers: covers::Covers::load(),
            window_active: true,
            alerts: notify::Alerts::default(),
            dm_seen: HashMap::new(),
            voice_chat_open: None,
            typing_sent: std::collections::HashSet::new(),
            migration_alert: None,
            last_sent: HashMap::new(),
            pressing: HashSet::new(),
            deferred: HashSet::new(),
            modal: None,
            dismissed_banners: HashSet::new(),
            update: update::Update::Idle,
            update_target: update::target(),
            read_flush: None,
            account_prefs: serde_json::Value::Object(Default::default()),
            gif_favorites: Vec::new(),
            gif_shares: HashMap::new(),
            media_kinds: HashMap::new(),
            guild_refreshes: HashMap::new(),
            mutual_friends: HashMap::new(),
            relays: HashMap::new(),
            direct_once: HashSet::new(),
            relay_failed: HashSet::new(),
            call_when_open: None,
            mic_test: None,
            sync: None,
            null_audio: cfg!(debug_assertions) && std::env::var("RHEIN_AUDIO").is_ok_and(|v| v == "null"),
            muted_before_deafen: false,
            speaking_task: None,
            call_video: cx.new(|_| CallVideo::default()),
            call_speaking: cx.new(|_| CallSpeaking::default()),
            media_tick: cx.new(|_| MediaTick),
            movie_player: cx.new(|_| MoviePlayer::default()),
            movie_posters: cx.new(|_| MoviePosters::default()),
            downloads: HashMap::new(),
            renew_ticker: None,
            signin: None,
            mfa_required: false,
        };
        this.restore(cx);
        if this.prefs.check_updates && rhein_update::enabled() {
            this.check_for_updates(true, cx);
        }
        this
    }

    // ---------------------------------------------------------------- sign-in

    fn restore(&mut self, cx: &mut Context<Self>) {
        let Some(saved) = persist::load() else {
            log::info!("no saved session at {}", persist::path().display());
            self.phase = Phase::SignedOut;
            return;
        };
        log::info!("restoring session for {} on {}", saved.user_id, saved.home);
        if saved.expires_at.is_some_and(|at| at <= now_ms()) {
            persist::clear();
            self.phase = Phase::SignedOut;
            self.error = Some("Your session has expired. Please sign in again.".into());
            return;
        }
        let io = self.io.clone();
        let http = self.http.clone();
        let input = saved.home.clone();
        let token = saved.access_token.clone();
        self.signin = Some(cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move {
                let doc = discover(&http, &input).await?;
                let api = Api::new(http, &doc.api, Some(token));
                let me = api.whoami().await.context("whoami")?;
                Ok((doc, api, me))
            }))
            .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok((doc, api, me)) => {
                        let avatar_ref = me.profile.as_ref().and_then(|p| p.avatar_ref.clone());
                        let display_name = me.profile.and_then(|p| p.display_name);
                        // A relocated home is remembered under its new name once the session
                        // proved still good there.
                        if let Some(new) = &doc.relocated_to {
                            log::info!("home server moved: {} is now {new}", saved.home);
                        }
                        let home = Home {
                            avatar_ref,
                            input: doc.relocated_to.clone().unwrap_or_else(|| saved.home.clone()),
                            doc,
                            api,
                            user_id: saved.user_id.clone(),
                            device_id: saved.device_id.clone(),
                            display_name,
                            device_seed: saved.device_seed.clone(),
                            expires_at: saved.expires_at,
                            dm_wrapper: None,
                        };
                        this.signed_in(home, saved.servers.clone(), cx);
                    }
                    Err(err) => {
                        log::warn!("restoring session failed: {err:#}");
                        let unauthorized = err
                            .downcast_ref::<rhein_net::ApiError>()
                            .and_then(|e| e.status())
                            .is_some_and(|s| s == 401);
                        if unauthorized {
                            persist::clear();
                            this.error = Some("Your session was revoked. Please sign in again.".into());
                        } else {
                            this.error = Some(format!("Can't reach your home server: {err:#}").into());
                        }
                        this.phase = Phase::SignedOut;
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Signs in (or registers) with OPAQUE. `server` is a domain such as `chat.example.com` or
    /// `localhost:8448`; `user` is a localpart or a full `@user:server`.
    pub fn sign_in(&mut self, server: String, user: String, password: String, register: bool, mfa_code: Option<String>, cx: &mut Context<Self>) {
        if self.phase == Phase::SigningIn {
            return;
        }
        self.phase = Phase::SigningIn;
        self.error = None;
        self.mfa_required = false;
        cx.notify();
        let io = self.io.clone();
        let http = self.http.clone();
        let device_name = device_name();
        self.signin = Some(cx.spawn(async move |this, cx| {
            let input = server.trim().to_owned();
            let result = join(io.spawn(async move {
                let doc = discover(&http, &input).await?;
                let api = Api::new(http, &doc.api, None);
                let device = DeviceKey::generate();
                let ksf = SerenityKsf::memory_constrained();
                let user = user.trim();
                let signed = if register {
                    auth::register(&api, localpart(user), &password, &device, &device_name, &ksf).await?
                } else {
                    auth::login_with_code(&api, user, &password, &device, &device_name, &ksf, mfa_code.as_deref().map(str::trim)).await?
                };
                let me = api.whoami().await.ok();
                Ok((input, doc, api, signed, device, me))
            }))
            .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok((input, doc, api, signed, device, me)) => {
                        let home = Home {
                            input,
                            doc,
                            api,
                            user_id: signed.session.user_id.clone(),
                            device_id: signed.session.device_id.clone(),
                            avatar_ref: me.as_ref().and_then(|m| m.profile.as_ref()).and_then(|p| p.avatar_ref.clone()),
                            display_name: me.and_then(|m| m.profile).and_then(|p| p.display_name),
                            device_seed: rhein_auth::b64u(&device.seed()),
                            expires_at: signed.session.expires_at,
                            dm_wrapper: Some(rhein_mls::vault::recovery_wrapper(&signed.export_key)),
                        };
                        this.signed_in(home, Vec::new(), cx);
                    }
                    Err(err) => {
                        this.phase = Phase::SignedOut;
                        // Two-factor sign-in: the same credentials again, with a code.
                        match err.downcast_ref::<rhein_net::ApiError>().and_then(|e| e.code()) {
                            Some("mfa_required") => {
                                this.mfa_required = true;
                                this.error = Some("Enter the code from your authenticator app, or a recovery code.".into());
                            }
                            Some("invalid_mfa_code") => {
                                this.mfa_required = true;
                                this.error = Some("That code didn't work. Try the current one.".into());
                            }
                            _ => this.error = Some(friendly(&err).into()),
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Renews every guild session that is about to run out (they last an hour), and gives each
    /// gateway the current token for its next connection; a session refused in between renews
    /// itself on the spot (`Api`). As the web client does, a minute apart and on waking.
    pub fn freshen_sessions(&mut self, cx: &mut Context<Self>) {
        // RHEIN_RENEW_MARGIN_SECS forces early renewals in development builds, for testing.
        let margin = std::env::var("RHEIN_RENEW_MARGIN_SECS")
            .ok()
            .filter(|_| cfg!(debug_assertions))
            .and_then(|s| s.parse().ok())
            .map_or(RENEW_BEFORE, Duration::from_secs);
        let live: Vec<(String, Api)> = self.servers.iter().filter_map(|s| s.api.clone().map(|api| (s.name.clone(), api))).collect();
        let io = self.io.clone();
        cx.spawn(async move |this, cx| {
            for (name, api) in live {
                let check = api.clone();
                let renewed = join(io.spawn(async move { Ok(check.renew_if_expiring(margin).await?) })).await;
                if let Err(err) = &renewed {
                    log::info!("renewing {name}: {err:#}");
                }
                let token = join(io.spawn(async move { Ok(api.token().await) })).await.ok().flatten();
                this.update(cx, |this, _| {
                    if let (Some(token), Some(gw)) = (token, this.server(&name).and_then(|s| s.gateway.as_ref())) {
                        gw.set_token(token);
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    fn signed_in(&mut self, home: Home, extra: Vec<String>, cx: &mut Context<Self>) {
        self.renew_ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(60)).await;
                if this.update(cx, |this, cx| this.freshen_sessions(cx)).is_err() {
                    return;
                }
            }
        }));
        let home_name = home.doc.server_name.clone();
        let home_input = home.input.clone();
        let me = home.user_id.clone();
        self.home = Some(home);
        self.phase = Phase::SignedIn;
        self.error = None;
        self.persist();
        // Our own profile at once: the one saved last time, then the server's.
        if let Some(saved) = own_profile_load(&me) {
            self.profiles.insert(me.clone(), Some(saved));
        }
        self.refresh_profile(&me, cx);
        self.sync_rpc(cx);
        self.bring_up_dm(cx);
        self.sync_rpc(cx);
        // The home server may host guilds too; it gets a guild session like any other server.
        self.add_server_named(home_input, Some(home_name), cx);
        for input in extra {
            self.add_server_named(input, None, cx);
        }
    }

    /// Signs out of the active account and forgets it on this device. Another saved account, if
    /// there is one, takes over.
    pub fn sign_out(&mut self, cx: &mut Context<Self>) {
        log::info!("signing out");
        let me = self.home.as_ref().map(|h| h.user_id.clone());
        // The server ends this session too; signing out locally does not wait for it.
        if let Some(api) = self.home.as_ref().map(|h| h.api.clone()) {
            self.io.spawn(async move {
                if let Err(err) = api.logout().await {
                    log::debug!("logout: {err}");
                }
            });
        }
        if let Some(me) = &me {
            crate::dm::Secrets::remove(me);
            crate::conversations::Engine::remove(me);
        }
        let next = me.and_then(|id| persist::remove(&id));
        self.reset();
        self.stop_movie(cx);
        self.returning_to = None;
        match next {
            Some(_) => {
                self.phase = Phase::Restoring;
                self.restore(cx);
            }
            None => self.phase = Phase::SignedOut,
        }
        cx.notify();
    }

    /// Accounts saved on this device, the active one included.
    pub fn saved_accounts(&self) -> Vec<Saved> {
        persist::accounts()
    }

    /// Switches to another saved account without a password.
    pub fn switch_account(&mut self, user_id: &str, cx: &mut Context<Self>) {
        if self.user_id() == Some(user_id) {
            return;
        }
        if let Err(err) = persist::set_active(user_id) {
            self.error = Some(format!("{err:#}").into());
            cx.notify();
            return;
        }
        log::info!("switching to {user_id}");
        self.reset();
        self.stop_movie(cx);
        self.returning_to = None;
        self.phase = Phase::Restoring;
        self.restore(cx);
        cx.notify();
    }

    /// Opens the sign-in form for another account; the current one stays saved, and the form
    /// offers the way back to it.
    pub fn add_account(&mut self, cx: &mut Context<Self>) {
        let back = self.home.as_ref().map(|h| (h.user_id.clone(), h.display_name.clone().unwrap_or_else(|| localpart(&h.user_id).to_owned())));
        self.reset();
        self.stop_movie(cx);
        self.returning_to = back;
        self.phase = Phase::SignedOut;
        cx.notify();
    }

    /// Unlocks encrypted DMs in a session restored without the password: proves the password to
    /// the home server again (no new device session) and opens the vault with what that yields.
    pub fn unlock_dm(&mut self, password: String, cx: &mut Context<Self>) {
        let Some(api) = self.home.as_ref().map(|h| h.api.clone()) else { return };
        let io = self.io.clone();
        self.dm_error = None;
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move {
                let ksf = SerenityKsf::memory_constrained();
                auth::reauthenticate(&api, &password, &ksf).await
            }))
            .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(export_key) => {
                        if let Some(home) = this.home.as_mut() {
                            home.dm_wrapper = Some(rhein_mls::vault::recovery_wrapper(&export_key));
                        }
                        this.bring_up_dm(cx);
                    }
                    Err(err) => this.dm_error = Some(friendly(&err)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Opens the DM recovery keyring and brings cross-signing up, in the background.
    fn bring_up_dm(&mut self, cx: &mut Context<Self>) {
        let Some(home) = &self.home else { return };
        let (api, user, device, wrapper) = (home.api.clone(), home.user_id.clone(), home.device_id.clone(), home.dm_wrapper);
        let io = self.io.clone();
        self.dm_task = Some(cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move { crate::dm::bring_up(&api, &user, &device, wrapper).await })).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(status) => {
                        log::info!("DM keys: vault {}, cross-signed {}", status.vault_mode, status.cross_signed);
                        this.dm_error = None;
                        this.open_dm_engine(&status, cx);
                        this.load_gif_favorites(cx);
                        this.dm = Some(status);
                    }
                    Err(err) => {
                        log::warn!("DM keys unavailable: {err:#}");
                        this.dm_error = Some(format!("{err:#}"));
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Drops everything the signed-in account brought with it: connections, guilds, history,
    /// what we share. Saved accounts on disk are not touched.
    fn reset(&mut self) {
        self.signin = None;
        self.call = None;
        self.sound = None;
        self.downloads.clear();
        self.gif_favorites.clear();
        self.gif_shares.clear();
        self.media_kinds.clear();
        self.alerts = notify::Alerts::default();
        self.dm_seen.clear();
        self.read_flush = None;
        self.account_prefs = serde_json::Value::Object(Default::default());
        self.migration_alert = None;
        self.dismissed_banners.clear();
        self.sync = None;
        self.speaking_task = None;
        self.renew_ticker = None;
        self.rpc = None;
        self.dm = None;
        self.dm_error = None;
        self.dm_task = None;
        self.dm_engine = None;
        self.sharing = None;
        self.local_activities.clear();
        self.servers.clear();
        self.guilds.clear();
        self.logs.clear();
        self.members.clear();
        self.member_ix.clear();
        self.pins.clear();
        self.threads.clear();
        self.emoji.clear();
        self.search = None;
        self.member_lookup.clear();
        self.friends.clear();
        self.conversations.clear();
        self.unread_conversations.clear();
        self.home_conversation = None;
        self.typing.clear();
        self.read.clear();
        self.dividers.clear();
        self.staged.clear();
        self.embeds.clear();
        self.voice.clear();
        self.showing_home = false;
        self.selected_guild = None;
        self.selected_channel.clear();
        self.visited.clear();
        self.home = None;
        self.error = None;
    }

    fn persist(&self) {
        let Some(home) = &self.home else { return };
        let saved = Saved {
            home: home.input.clone(),
            user_id: home.user_id.clone(),
            device_id: home.device_id.clone(),
            access_token: String::new(),
            expires_at: home.expires_at,
            device_seed: home.device_seed.clone(),
            servers: self
                .servers
                .iter()
                .filter(|s| s.name != home.doc.server_name)
                .map(|s| s.input.clone())
                .collect(),
            display_name: home.display_name.clone(),
        };
        let io = self.io.clone();
        let api = home.api.clone();
        // The token lives inside `Api`; read it on the runtime and write the file there too.
        io.spawn(async move {
            let mut saved = saved;
            saved.access_token = api.token().await.unwrap_or_default();
            if let Err(err) = persist::store(&saved) {
                log::error!("saving session: {err:#}");
            }
        });
    }

    pub fn user_id(&self) -> Option<&str> {
        self.home.as_ref().map(|h| h.user_id.as_str())
    }

    // ---------------------------------------------------------------- servers

    /// Connects to another guild server under the signed-in account.
    pub fn add_server(&mut self, input: String, cx: &mut Context<Self>) {
        let input = input.trim().to_owned();
        if input.is_empty() || self.servers.iter().any(|s| s.input == input || s.name == input) {
            return;
        }
        self.add_server_named(input, None, cx);
        self.persist();
    }

    // ---------------------------------------------------------------- presence & prefs

    /// Sets the status on every server now and for later reconnects.
    pub fn set_presence(&mut self, status: String, custom_status: Option<String>, cx: &mut Context<Self>) {
        let custom_status = custom_status.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
        self.prefs.status = status;
        self.prefs.custom_status = custom_status;
        self.save_prefs();
        self.push_presence();
        cx.notify();
    }

    /// The presence others see: the chosen one, except that "online" reads "idle" while this
    /// window has gone without input for a while (`set_auto_idle`).
    pub fn effective_status(&self) -> &str {
        if self.auto_idle && self.prefs.status == "online" { "idle" } else { &self.prefs.status }
    }

    /// Marks us idle after a stretch without input, or back when input comes again. The chosen
    /// status is left as it is, so "do not disturb", "invisible" and a chosen "idle" stand.
    pub fn set_auto_idle(&mut self, idle: bool, cx: &mut Context<Self>) {
        if self.auto_idle == idle {
            return;
        }
        self.auto_idle = idle;
        if self.prefs.status == "online" {
            self.push_presence();
            cx.notify();
        }
    }

    pub fn is_auto_idle(&self) -> bool {
        self.auto_idle
    }

    /// What every connection should say about us right now.
    pub fn presence_state(&self) -> PresenceState {
        PresenceState {
            status: self.effective_status().to_owned(),
            custom_status: self.prefs.custom_status.clone(),
            activities: self.sharing.clone(),
        }
    }

    fn push_presence(&self) {
        let presence = self.presence_state();
        for server in &self.servers {
            if let Some(gw) = &server.gateway {
                gw.set_presence(presence.clone());
            }
        }
    }

    pub fn update_prefs(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut crate::prefs::Prefs)) {
        change(&mut self.prefs);
        self.save_prefs();
        self.sync_rpc(cx);
        self.share_local(cx);
        cx.notify();
    }

    // ---------------------------------------------------------------- rich presence

    /// Puts the activity of `slot` first in what we share, or goes back to the usual order.
    pub fn set_primary_activity(&mut self, slot: Option<rhein_proto::activity::Slot>, cx: &mut Context<Self>) {
        self.update_prefs(cx, |p| p.primary_activity = slot.map(|s| s.key().to_owned()));
    }

    /// What this client shares right now (after the sharing preferences), if anything.
    pub fn shared_activities(&self) -> &[rhein_proto::Activity] {
        self.sharing.as_deref().unwrap_or_default()
    }

    /// Starts the local activity sources while signed in with sharing on, and stops them
    /// otherwise. Stopping clears what we shared, once.
    fn sync_rpc(&mut self, cx: &mut Context<Self>) {
        let want = self.phase == Phase::SignedIn && self.prefs.share_activity;
        if want && self.rpc.is_none() {
            let config = rhein_rpc::Config::new(persist::cache_dir());
            let handle = {
                let _runtime = self.io.handle().enter();
                rhein_rpc::start(config)
            };
            let (activities, status) = (handle.activities.clone(), handle.status.clone());
            // The sources change at human speed; a glance once a second is plenty and costs
            // nothing while idle.
            let follow = cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let (now, state) = (activities.borrow().clone(), status.borrow().clone());
                    let alive = this
                        .update(cx, |this, cx| {
                            let changed = this.local_activities != now || this.rpc_status != state;
                            this.local_activities = now;
                            this.rpc_status = state;
                            if changed {
                                this.share_local(cx);
                                cx.notify();
                            }
                        })
                        .is_ok();
                    if !alive {
                        break;
                    }
                }
            });
            self.rpc = Some((handle, follow));
        } else if !want && self.rpc.is_some() {
            self.rpc = None;
            self.local_activities.clear();
            self.rpc_status = rhein_rpc::Status::default();
            if self.sharing.is_some() {
                self.sharing = Some(Vec::new());
                self.push_presence();
                self.sharing = None;
            }
        }
    }

    /// Applies the sharing preferences to what the sources found and tells the servers when that
    /// changes.
    pub(super) fn share_local(&mut self, cx: &mut Context<Self>) {
        if self.rpc.is_none() {
            return;
        }
        let prefs = &self.prefs;
        let shared: Vec<rhein_proto::Activity> = self
            .local_activities
            .iter()
            .filter(|a| match a.slot() {
                rhein_proto::activity::Slot::Game => prefs.share_games,
                rhein_proto::activity::Slot::Music => prefs.share_music,
                rhein_proto::activity::Slot::Video => prefs.share_videos,
            })
            .filter(|a| !prefs.hidden_activities.contains(&a.name))
            .cloned()
            .collect();
        let primary = prefs.primary_activity.as_deref().and_then(rhein_proto::activity::Slot::from_key);
        let shared = rhein_proto::activity::primary_first(shared, primary);
        let shared = self.with_shared_covers(shared, cx);
        // With nothing of our own we stay out of it, so another session's activity (the Telium
        // desktop app's, say) isn't erased; when ours ends, it is cleared once.
        if shared.is_empty() {
            if self.sharing.as_ref().is_some_and(|s| !s.is_empty()) {
                self.sharing = Some(Vec::new());
                self.push_presence();
                self.sharing = None;
                cx.notify();
            }
        } else if self.sharing.as_ref() != Some(&shared) {
            self.sharing = Some(shared);
            self.push_presence();
            cx.notify();
        }
    }

    fn save_prefs(&self) {
        let mut on_disk = crate::prefs::load();
        let recent = on_disk.recent_emoji.clone();
        on_disk = self.prefs.clone();
        on_disk.recent_emoji = if self.recent_emoji.is_empty() { recent } else { self.recent_emoji.clone() };
        crate::prefs::store(&on_disk);
    }

    // ---------------------------------------------------------------- profile & devices

    /// A user's public profile, fetched once through the home server and cached.
    pub fn profile(&mut self, user_id: &str, cx: &mut Context<Self>) -> Option<rhein_proto::Profile> {
        if let Some(cached) = self.profiles.get(user_id) {
            return cached.clone();
        }
        self.refresh_profile(user_id, cx);
        None
    }

    /// Asks the home server for a profile (again), keeping what is known meanwhile. A failed
    /// request is forgotten, so the next look asks again instead of waiting forever.
    pub fn refresh_profile(&mut self, user_id: &str, cx: &mut Context<Self>) {
        let Some(api) = self.home.as_ref().map(|h| h.api.clone()) else { return };
        self.profiles.entry(user_id.to_owned()).or_insert(None);
        let io = self.io.clone();
        let uid = user_id.to_owned();
        cx.spawn(async move |this, cx| {
            let asked = uid.clone();
            let result = join(io.spawn(async move { Ok(api.profile(&asked).await?) })).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(profile) => {
                        if this.user_id() == Some(uid.as_str()) {
                            own_profile_save(&uid, &profile);
                        }
                        this.profiles.insert(uid, Some(profile));
                    }
                    Err(err) => {
                        log::debug!("profile {uid}: {err:#}");
                        if this.profiles.get(&uid).is_some_and(Option::is_none) {
                            this.profiles.remove(&uid);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A profile already fetched, without asking for it.
    pub fn cached_profile(&self, user_id: &str) -> Option<&rhein_proto::Profile> {
        self.profiles.get(user_id).and_then(Option::as_ref)
    }

    /// Saves profile fields (only those given) and refreshes what this client shows.
    pub fn save_profile(&mut self, patch: serde_json::Value, done: impl FnOnce(Result<(), String>, &mut Context<Self>) + 'static, cx: &mut Context<Self>) {
        let Some(api) = self.home.as_ref().map(|h| h.api.clone()) else { return };
        let io = self.io.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move {
                api.patch_profile(&patch).await?;
                Ok(api.whoami().await?)
            }))
            .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(me) => {
                        if let (Some(home), Some(profile)) = (this.home.as_mut(), me.profile) {
                            home.display_name = profile.display_name.clone();
                            home.avatar_ref = profile.avatar_ref.clone();
                            this.profiles.insert(home.user_id.clone(), Some(profile));
                        }
                        // Guild servers learn the new essentials with a fresh session.
                        let names: Vec<String> = this.servers.iter().map(|s| s.name.clone()).collect();
                        for name in names {
                            this.renew_server(&name, cx);
                        }
                        done(Ok(()), cx);
                    }
                    Err(err) => done(Err(friendly(&err)), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Uploads a picture as the profile avatar; `done` receives the new `avatar_ref`.
    /// Uploads a picture for the profile (avatar or banner) and hands back its media reference.
    pub fn upload_profile_media(&mut self, path: std::path::PathBuf, done: impl FnOnce(Result<String, String>, &mut Context<Self>) + 'static, cx: &mut Context<Self>) {
        let Some(api) = self.home.as_ref().map(|h| h.api.clone()) else { return };
        let io = self.io.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("avatar").to_owned();
                let bytes = tokio::task::spawn_blocking(move || std::fs::read(path)).await??;
                let media = api.upload_profile_media(&name, guess_mime(&name), bytes).await?;
                let cap = media.capability.clone().unwrap_or_default();
                Ok(format!("{}?cap={cap}", media.hash))
            }))
            .await;
            this.update(cx, |_, cx| done(result.map_err(|e| friendly(&e)), cx)).ok();
        })
        .detach();
    }

    /// Uploads an avatar or banner already prepared in memory (a crop), like `upload_profile_media`.
    pub fn upload_profile_bytes(
        &mut self,
        bytes: Vec<u8>,
        filename: String,
        mime: &'static str,
        done: impl FnOnce(Result<String, String>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(api) = self.home.as_ref().map(|h| h.api.clone()) else { return };
        let io = self.io.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move {
                let media = api.upload_profile_media(&filename, mime, bytes).await?;
                let cap = media.capability.clone().unwrap_or_default();
                Ok(format!("{}?cap={cap}", media.hash))
            }))
            .await;
            this.update(cx, |_, cx| done(result.map_err(|e| friendly(&e)), cx)).ok();
        })
        .detach();
    }

    /// A guild's icon, from its own server's media origin.
    pub fn guild_icon_url(&self, guild_id: &str) -> Option<String> {
        let entry = self.guild(guild_id)?;
        let reference = entry.guild.icon_ref.as_deref().filter(|r| !r.is_empty())?;
        let doc = self.server(&entry.server)?.doc.as_ref()?;
        Some(crate::media::media_url(&doc.api, reference, None))
    }

    /// The API base (`https://…`) of the server a guild lives on.
    pub fn guild_api_base(&self, guild_id: &str) -> Option<String> {
        let entry = self.guild(guild_id)?;
        Some(self.server(&entry.server)?.doc.as_ref()?.api.trim_end_matches('/').to_owned())
    }

    /// The name of the server a guild lives on.
    pub fn guild_server(&self, guild_id: &str) -> Option<&str> {
        self.guild(guild_id).map(|g| g.server.as_str())
    }

    /// The media URL for an `avatar_ref` on the home server (for previews in settings).
    pub fn home_media_url(&self, reference: &str) -> Option<String> {
        let home = self.home.as_ref()?;
        Some(crate::media::media_url(&home.doc.api, reference, None))
    }

    pub fn load_devices(&mut self, done: impl FnOnce(Result<Vec<rhein_proto::Device>, String>, &mut Context<Self>) + 'static, cx: &mut Context<Self>) {
        let Some(api) = self.home.as_ref().map(|h| h.api.clone()) else { return };
        let io = self.io.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move { Ok(api.devices().await?) })).await;
            this.update(cx, |_, cx| done(result.map_err(|e| friendly(&e)), cx)).ok();
        })
        .detach();
    }

    /// Signs a device out. Holding the account root, this device also revokes the device's MLS
    /// key with a statement the root signs, so every client stops trusting it; without the root
    /// (or for a device that never had a signed key) only its sessions end.
    pub fn revoke_device(&mut self, device_id: String, cx: &mut Context<Self>) {
        let Some(home) = &self.home else { return };
        let (api, user) = (home.api.clone(), home.user_id.clone());
        let root = crate::dm::Secrets::load(&user)
            .cross_signing
            .and_then(|s| rhein_mls::vault::unb64(&s).ok())
            .and_then(|b| rhein_mls::trust::IdentityKey::from_secret(&b).ok());
        let tid = self.dm.as_ref().and_then(|d| d.immutable_id.clone());
        self.fire(cx, async move {
            let signed = match (root, tid) {
                (Some(root), Some(tid)) => {
                    let rows = api.cross_signing().await?;
                    let key = rows["devices"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|d| d["device_id"] == device_id.as_str())
                        .and_then(|d| d["mls_key"].as_str())
                        .map(str::to_owned);
                    key.map(|key| {
                        let signature = root.revoke_device(&user, &tid, &device_id, &key);
                        serde_json::json!({ "mls_key": key, "signature": signature })
                    })
                }
                _ => None,
            };
            match api.revoke_device(&device_id, signed.as_ref()).await {
                // No identity anchor on the account: the statement means nothing to it yet.
                Err(err) if signed.is_some() && err.status() == Some(409) => api.revoke_device(&device_id, None).await.map(drop),
                other => other.map(drop),
            }
        });
    }

    // ---------------------------------------------------------------- guild, channel & member actions

    pub fn guild_can(&self, guild_id: &str, bit: u64) -> bool {
        let Some(entry) = self.guild(guild_id) else { return false };
        // A timeout outranks everything, ownership included: reading is all that is left.
        let me = entry.guild.me.as_ref();
        if me.and_then(|m| m.get("timeout_until")).and_then(|v| v.as_i64()).is_some_and(|until| until > now_ms()) {
            return bit & (crate::perms::VIEW_CHANNEL | crate::perms::READ_MESSAGE_HISTORY) == bit;
        }
        if entry.guild.owner.as_deref() == self.user_id() {
            return true;
        }
        let bits = entry
            .guild
            .me
            .as_ref()
            .and_then(|m| m.get("permissions"))
            .and_then(|p| p.as_str())
            .map(crate::perms::parse_bits)
            .unwrap_or(0);
        bits & bit != 0 || bits & crate::perms::ADMINISTRATOR != 0
    }

    pub fn is_owner(&self, guild_id: &str) -> bool {
        self.guild(guild_id).is_some_and(|g| g.guild.owner.as_deref() == self.user_id())
    }

    pub fn rename_guild(&mut self, guild_id: String, name: String, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_guild(&guild_id) else { return };
        if let Some(entry) = self.guild_mut(&guild_id) {
            entry.guild.name = name.clone();
        }
        cx.notify();
        self.fire(cx, async move { api.patch_guild(&guild_id, &serde_json::json!({ "name": name })).await.map(drop) });
    }

    pub fn leave_guild(&mut self, guild_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_guild(&guild_id) else { return };
        self.guilds.retain(|g| g.guild.id != guild_id);
        if self.selected_guild.as_deref() == Some(guild_id.as_str()) {
            self.selected_guild = self.guilds.first().map(|g| g.guild.id.clone());
        }
        cx.notify();
        self.fire(cx, async move { api.leave_guild(&guild_id).await.map(drop) });
    }

    pub fn create_channel(&mut self, guild_id: String, name: String, kind: String, parent_id: Option<String>, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_guild(&guild_id) else { return };
        let io = self.io.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move {
                Ok(api.create_channel(&guild_id, name.trim(), &kind, parent_id.as_deref()).await?)
            }))
            .await;
            this.update(cx, |this, cx| {
                match result {
                    // CHANNEL_CREATE adds it to the list; open it once it is there.
                    Ok(channel) => {
                        if channel.is_textual() {
                            let id = channel.id.clone();
                            if this.channel(&id).is_none()
                                && let Some(entry) = channel.guild_id.clone().and_then(|g| this.guilds.iter_mut().find(|e| e.guild.id == g))
                            {
                                entry.guild.channels.push(channel);
                            }
                            this.select_channel(id, cx);
                        }
                    }
                    Err(err) => this.error = Some(format!("Couldn't create channel: {}", friendly(&err)).into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Changes a channel's settings (`name`, `topic`, `slowmode`, `bitrate`, `user_limit`,
    /// `voice_region`).
    pub fn edit_channel(&mut self, channel_id: String, patch: serde_json::Value, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_channel(&channel_id) else { return };
        self.fire(cx, async move { api.patch_channel(&channel_id, &patch).await.map(drop) });
    }

    pub fn delete_channel(&mut self, channel_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_channel(&channel_id) else { return };
        self.fire(cx, async move { api.delete_channel(&channel_id).await.map(drop) });
    }

    pub fn set_nickname(&mut self, guild_id: String, user_id: String, nickname: String, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_guild(&guild_id) else { return };
        // Nicknames hold at most 32 characters.
        let nickname: String = nickname.trim().chars().take(32).collect();
        let patch = serde_json::json!({ "nickname": nickname });
        self.fire(cx, async move { api.patch_member(&guild_id, &user_id, &patch).await.map(drop) });
    }

    /// Gives a member a role, or takes it away.
    pub fn set_member_role(&mut self, guild_id: String, user_id: String, role_id: String, give: bool, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_guild(&guild_id) else { return };
        let patch = if give { serde_json::json!({ "add_roles": [role_id] }) } else { serde_json::json!({ "remove_roles": [role_id] }) };
        self.fire(cx, async move { api.patch_member(&guild_id, &user_id, &patch).await.map(drop) });
    }

    /// Moves a channel one place up (`-1`) or down (`1`) among the channels beside it (same
    /// category), by swapping places with its neighbour.
    pub fn move_channel(&mut self, channel_id: &str, by: i64, cx: &mut Context<Self>) {
        let Some(entry) = self.guild_of_channel(channel_id) else { return };
        let Some(me) = entry.guild.channels.iter().find(|c| c.id == channel_id) else { return };
        let (gid, parent, category) = (entry.guild.id.clone(), me.parent_id.clone(), me.is_category());
        let mut siblings: Vec<&rhein_proto::Channel> = entry
            .ordered_channels()
            .into_iter()
            .filter(|c| c.parent_id == parent && c.is_category() == category)
            .collect();
        siblings.sort_by_key(|c| (c.position, c.id.clone()));
        let Some(at) = siblings.iter().position(|c| c.id == channel_id) else { return };
        let to = at as i64 + by;
        if to < 0 || to as usize >= siblings.len() {
            return;
        }
        let other = siblings[to as usize];
        let (mine, theirs) = (siblings[at].position, other.position);
        // Equal positions (never reordered) still need two different places.
        let (mine, theirs) = if mine == theirs { if by < 0 { (theirs, theirs + 1) } else { (theirs + 1, theirs) } } else { (theirs, mine) };
        let places = vec![(channel_id.to_owned(), mine, parent.clone()), (other.id.clone(), theirs, parent)];
        let Some(api) = self.api_for_guild(&gid) else { return };
        self.fire(cx, async move { api.reorder_channels(&gid, &places).await.map(drop) });
    }

    /// Times a member out for `minutes`; 0 ends a timeout (a time in the past clears it).
    pub fn timeout_member(&mut self, guild_id: String, user_id: String, minutes: i64, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_guild(&guild_id) else { return };
        let until = if minutes > 0 { now_ms() + minutes * 60_000 } else { now_ms() - 1 };
        let patch = serde_json::json!({ "timeout_until": until });
        self.fire(cx, async move { api.patch_member(&guild_id, &user_id, &patch).await.map(drop) });
    }

    pub fn kick_member(&mut self, guild_id: String, user_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_guild(&guild_id) else { return };
        self.fire(cx, async move { api.kick_member(&guild_id, &user_id).await.map(drop) });
    }

    pub fn ban_member(&mut self, guild_id: String, user_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_guild(&guild_id) else { return };
        self.fire(cx, async move { api.ban_member(&guild_id, &user_id, None).await.map(drop) });
    }

    // ---------------------------------------------------------------- mark read

    /// Acknowledges the newest message of a channel, fetching it when the history isn't loaded.
    /// Opens (with the channel) or closes a voice channel's chat; opening reads it.
    pub fn set_voice_chat(&mut self, channel_id: Option<String>, cx: &mut Context<Self>) {
        self.voice_chat_open = channel_id.clone();
        if let Some(id) = channel_id {
            let needs_load = self.logs.get(&id).is_none_or(|l| !l.loaded && !l.loading);
            if needs_load {
                self.load_history(id.clone(), false, cx);
            }
            self.mark_read(id, cx);
        }
        cx.notify();
    }

    pub fn voice_chat_open(&self) -> Option<&str> {
        self.voice_chat_open.as_deref()
    }

    pub fn mark_read(&mut self, channel_id: String, cx: &mut Context<Self>) {
        if self.is_dm(&channel_id) {
            // Read state of an encrypted conversation travels inside it (receipts), not to a server.
            self.unread_conversations.remove(&channel_id);
            return;
        }
        if let Some(mark) = self.read.get_mut(&channel_id) {
            mark.unread = 0;
            mark.mentions = 0;
            mark.first_unread = None;
        }
        self.dividers.remove(&channel_id);
        cx.notify();
        let Some(api) = self.api_for_channel(&channel_id) else { return };
        let newest = self
            .logs
            .get(&channel_id)
            .and_then(|l| l.messages.iter().rev().find(|m| !m.id.starts_with("pending:")))
            .map(|m| m.id.clone());
        self.io.spawn(async move {
            let id = match newest {
                Some(id) => Some(id),
                None => api.messages(&channel_id, None, 1).await.ok().and_then(|m| m.last().map(|m| m.id.clone())),
            };
            if let Some(id) = id {
                let _ = api.ack(&channel_id, &id).await;
            }
        });
    }

    pub fn mark_guild_read(&mut self, guild_id: &str, cx: &mut Context<Self>) {
        let Some(entry) = self.guild(guild_id) else { return };
        let unread: Vec<String> = entry
            .guild
            .channels
            .iter()
            .filter(|c| self.read.get(&c.id).is_some_and(|m| m.unread > 0 || m.mentions > 0))
            .map(|c| c.id.clone())
            .collect();
        for channel in unread {
            self.mark_read(channel, cx);
        }
    }

    // ---------------------------------------------------------------- navigation

    /// Text channels of the selected guild in sidebar order.
    fn sidebar_channels(&self) -> Vec<String> {
        let Some(entry) = self.selected_guild.as_deref().and_then(|g| self.guild(g)) else { return Vec::new() };
        entry.ordered_channels().into_iter().filter(|c| c.is_textual()).map(|c| c.id.clone()).collect()
    }

    /// Moves to the previous (`-1`) or next (`+1`) channel, optionally only unread ones; wraps.
    pub fn step_channel(&mut self, direction: i32, unread_only: bool, cx: &mut Context<Self>) {
        let order = self.sidebar_channels();
        let current = self.selected_channel().map(str::to_owned);
        if let Some(target) = crate::nav::step(&order, current.as_deref(), direction, |c| {
            !unread_only || self.read.get(c).is_some_and(|m| m.is_unread() || m.mentions > 0)
        }) {
            self.select_channel(target, cx);
        }
    }

    pub fn remove_server(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.home.as_ref().is_some_and(|h| h.doc.server_name == name) {
            return;
        }
        self.servers.retain(|s| s.name != name);
        self.guilds.retain(|g| g.server != name);
        if self.selected_guild.as_ref().is_some_and(|g| self.guild(g).is_none()) {
            self.selected_guild = None;
        }
        self.persist();
        cx.notify();
    }

    fn add_server_named(&mut self, input: String, name: Option<String>, cx: &mut Context<Self>) {
        let placeholder = name.clone().unwrap_or_else(|| input.clone());
        self.servers.push(Server {
            name: placeholder.clone(),
            input: input.clone(),
            doc: None,
            api: None,
            status: ServerStatus::Connecting,
            error: None,
            latency: None,
            last_close: None,
            closes: 0,
            gateway: None,
            connect: None,
        });
        self.connect_server(placeholder, cx);
        cx.notify();
    }

    fn server_mut(&mut self, name: &str) -> Option<&mut Server> {
        self.servers.iter_mut().find(|s| s.name == name)
    }

    pub fn server(&self, name: &str) -> Option<&Server> {
        self.servers.iter().find(|s| s.name == name)
    }

    fn ready_servers(&self) -> impl Iterator<Item = &Server> {
        self.servers.iter().filter(|s| s.status == ServerStatus::Ready)
    }

    /// Whether "Protect my IP" covers a server: for every server, or per guild (then the
    /// strictest of its guilds, since one connection serves them all).
    pub fn protected(&self, key: &str) -> bool {
        match self.account_pref(&["network", "mode"]).and_then(|v| v.as_str()) {
            Some("protected") => true,
            Some("per-guild") => self.guilds.iter().filter(|g| g.server == key || self.server(&g.server).is_some_and(|s| s.input == key)).any(|g| {
                self.account_pref(&["network", "guilds", &g.guild.id]).and_then(|v| v.as_str()) == Some("protected")
            }),
            _ => false,
        }
    }

    /// Whether the home server runs a relay for its users.
    pub fn relay_available(&self) -> bool {
        self.home.as_ref().is_some_and(|h| h.doc.relay.as_ref().is_some_and(|r| !r.is_null()))
    }

    /// Reconnects every server, after the network privacy choice changed.
    pub fn reconnect_all(&mut self, cx: &mut Context<Self>) {
        self.relays.clear();
        let keys: Vec<String> = self.servers.iter().map(|s| s.input.clone()).collect();
        for key in keys {
            self.connect_server(key, cx);
        }
    }

    /// Connects a protected server directly this once, after its relay failed.
    pub fn connect_directly(&mut self, key: &str, cx: &mut Context<Self>) {
        self.direct_once.insert(key.to_owned());
        self.relay_failed.remove(key);
        self.connect_server(key.to_owned(), cx);
    }

    /// Discovery, assertion → guild session, then the gateway.
    fn connect_server(&mut self, key: String, cx: &mut Context<Self>) {
        if self.home.is_none() {
            return;
        }
        // Protected: through the home's relay, or not at all (unless allowed to fall back).
        let fallback = self.account_flag(&["network", "autoFallback"], false);
        let relayed = self.protected(&key) && !self.direct_once.contains(&key);
        if relayed && !self.relay_available() && !fallback {
            if let Some(server) = self.server_mut(&key) {
                server.status = ServerStatus::Failed;
                server.error = Some("\"Protect my IP\" is on, but your home server offers no relay.".into());
            }
            cx.notify();
            return;
        }
        let relayed = relayed && self.relay_available();
        let presence = self.presence_state();
        let Some(server) = self.server_mut(&key) else { return };
        server.status = ServerStatus::Connecting;
        server.error = None;
        server.gateway = None;
        let input = server.input.clone();
        let io = self.io.clone();
        let http = self.http.clone();
        let home_api = self.home.as_ref().map(|h| h.api.clone()).unwrap();
        let task_key = key.clone();
        let task = cx.spawn(async move |this, cx| {
            let key = task_key;
            let result = join(io.spawn(async move {
                // Through the relay, discovery included: the guild never sees our address.
                let (http, proxy) = if relayed {
                    let host = rhein_net::discovery::normalise(&input).split(':').next().unwrap_or_default().to_owned();
                    let proxy = Arc::new(rhein_net::tunnel::RelayProxy::start(home_api.clone(), host).await?);
                    let http = reqwest::Client::builder()
                        .user_agent(concat!("Rhein/", env!("CARGO_PKG_VERSION")))
                        .connect_timeout(Duration::from_secs(20))
                        .proxy(proxy.reqwest_proxy()?)
                        .build()?;
                    (http, Some(proxy))
                } else {
                    (http, None)
                };
                let doc = discover(&http, &input).await?;
                let api = Api::new(http, &doc.api, None);
                let session = auth::guild_session(&home_api, &api, &doc.server_name).await?;
                let url = session
                    .gateway
                    .clone()
                    .or_else(|| doc.gateway.clone())
                    .ok_or_else(|| anyhow!("{} publishes no gateway", doc.server_name))?;
                let (handle, events) = gateway::connect(GatewayConfig {
                    url,
                    token: session.session_token,
                    intents: USER_INTENTS,
                    presence,
                    voice: Vec::new(),
                    proxy: proxy.as_ref().map(|p| p.addr()),
                });
                Ok((doc, api, handle, events, proxy))
            }))
            .await;
            let Ok(Some(mut events)) = this.update(cx, |this, cx| {
                match &result {
                    Ok((.., Some(proxy))) => {
                        this.relays.insert(key.clone(), proxy.clone());
                        this.relay_failed.remove(&key);
                    }
                    Ok(_) => {
                        this.relays.remove(&key);
                    }
                    Err(_) if relayed => {
                        this.relay_failed.insert(key.clone());
                    }
                    Err(_) => {}
                }
                let server = this.server_mut(&key)?;
                match result {
                    Ok((doc, api, handle, events, _)) => {
                        let real = doc.server_name.clone();
                        server.name = real.clone();
                        if let Some(new) = doc.relocated_to.clone() {
                            server.input = new;
                        }
                        server.doc = Some(doc);
                        server.api = Some(api);
                        server.gateway = Some(handle);
                        cx.notify();
                        // Two inputs can name the same server; keep the one connected first.
                        let duplicate = this.servers.iter().filter(|s| s.name == real).count() > 1;
                        if duplicate {
                            if let Some(i) = this.servers.iter().rposition(|s| s.name == real) {
                                this.servers.remove(i);
                            }
                            return None;
                        }
                        Some(events)
                    }
                    Err(err) => {
                        log::warn!("connecting {key}: {err:#}");
                        server.status = ServerStatus::Failed;
                        server.error = Some(friendly(&err));
                        cx.notify();
                        None
                    }
                }
            }) else {
                return;
            };
            let name = this
                .read_with(cx, |this, _| {
                    this.servers
                        .iter()
                        .find(|s| s.input == key || s.name == key)
                        .map(|s| s.name.clone())
                })
                .ok()
                .flatten();
            let Some(name) = name else { return };
            // Pump gateway events into the entity until the connection is dropped: whatever
            // arrived together is applied in one update, with one repaint.
            while let Some(event) = events.recv().await {
                let mut batch = vec![event];
                while batch.len() < 256
                    && let Ok(more) = events.try_recv()
                {
                    batch.push(more);
                }
                let applied = this.update(cx, |this, cx| {
                    let mut changed = false;
                    for event in batch {
                        changed |= this.on_gateway(&name, event, cx);
                    }
                    if changed {
                        cx.notify();
                    }
                });
                if applied.is_err() {
                    break;
                }
            }
        });
        if let Some(server) = self.server_mut(&key) {
            server.connect = Some(task);
        }
    }

    /// Mints a fresh guild session and hands its token to the live gateway task.
    fn renew_server(&mut self, name: &str, cx: &mut Context<Self>) {
        let (Some(home), Some(server)) = (&self.home, self.servers.iter().find(|s| s.name == name)) else {
            return;
        };
        let (Some(api), Some(doc)) = (server.api.clone(), server.doc.clone()) else { return };
        let home_api = home.api.clone();
        let io = self.io.clone();
        let name = name.to_owned();
        let task = cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move {
                let session = auth::guild_session(&home_api, &api, &doc.server_name).await?;
                Ok(session.session_token)
            }))
            .await;
            this.update(cx, |this, cx| {
                let Some(server) = this.server_mut(&name) else { return };
                match result {
                    Ok(token) => {
                        if let Some(gw) = &server.gateway {
                            gw.reconnect(token);
                        }
                    }
                    Err(err) => {
                        server.status = ServerStatus::Failed;
                        server.error = Some(friendly(&err));
                    }
                }
                cx.notify();
            })
            .ok();
        });
        task.detach();
    }

    /// Re-identifies so a guild created or joined since the last READY is streamed too; the
    /// gateway does not add guilds to a live session.
    fn refresh_server(&mut self, name: &str, cx: &mut Context<Self>) {
        self.renew_server(name, cx);
    }

    /// Applies one gateway event; true when something on screen may have changed.
    fn on_gateway(&mut self, server: &str, event: GatewayEvent, cx: &mut Context<Self>) -> bool {
        match event {
            GatewayEvent::Status(status) => {
                let renew = matches!(status, GatewayStatus::Rejected);
                if let Some(s) = self.server_mut(server) {
                    if !matches!(status, GatewayStatus::Ready) {
                        s.latency = None;
                    }
                    s.status = match status {
                        GatewayStatus::Connecting => ServerStatus::Connecting,
                        GatewayStatus::Ready => ServerStatus::Ready,
                        GatewayStatus::Backoff => ServerStatus::Reconnecting,
                        GatewayStatus::Rejected => ServerStatus::Renewing,
                        GatewayStatus::Incompatible => ServerStatus::Incompatible,
                    };
                }
                if renew {
                    self.renew_server(server, cx);
                }
            }
            GatewayEvent::Ready(ready) => self.on_ready(server, *ready, cx),
            GatewayEvent::Dispatch(event) => return self.on_dispatch(server, *event, cx),
            GatewayEvent::Latency(rtt) => {
                if let Some(s) = self.server_mut(server) {
                    s.latency = Some(rtt);
                }
            }
            GatewayEvent::Closed { code, reason } => {
                log::info!("gateway {server} closed {code}: {reason}");
                if let Some(s) = self.server_mut(server) {
                    s.last_close = Some(code);
                    s.closes = s.closes.saturating_add(1);
                    // A refusal says what to do about it.
                    let refusal = matches!(code, close::PROTOCOL_INCOMPATIBLE | close::MALFORMED | close::AUTH_FAILED | close::IDENTIFY_EXPECTED);
                    if refusal && !reason.is_empty() {
                        s.error = Some(reason);
                    }
                }
            }
        }
        true
    }

    fn on_ready(&mut self, server: &str, ready: Ready, cx: &mut Context<Self>) {
        log::info!("READY from {server}: {} guild(s)", ready.guilds.len());
        if self.home.as_ref().is_some_and(|h| h.doc.server_name == server) {
            self.load_friends(cx);
            self.load_conversations(cx);
            self.load_pending_moves(cx);
        }
        // READY replaces everything this server said before: there is no resume.
        let insert_at = self
            .guilds
            .iter()
            .position(|g| g.server == server)
            .unwrap_or(self.guilds.len());
        self.guilds.retain(|g| g.server != server);
        let incoming: Vec<GuildEntry> = ready
            .guilds
            .into_iter()
            .filter(|g| g.moved_to.is_none() || !g.channels.is_empty())
            .map(|guild| GuildEntry { server: server.to_owned(), guild })
            .collect();
        let at = insert_at.min(self.guilds.len());
        for entry in &incoming {
            let roster = entry
                .guild
                .voice_states
                .iter()
                .filter(|v| v.channel_id.is_some())
                .map(|v| (v.user_id.clone(), v.clone()))
                .collect();
            self.voice.insert(entry.guild.id.clone(), roster);
        }
        let ids: Vec<String> = incoming.iter().map(|g| g.guild.id.clone()).collect();
        self.guilds.splice(at..at, incoming);
        for id in ids {
            // Members may have come and gone while we were away: fetched again when shown.
            self.members.remove(&id);
            self.load_read_state(id, cx);
        }

        if self.selected_guild.as_ref().is_none_or(|g| self.guild(g).is_none()) {
            self.selected_guild = self.guilds.first().map(|g| g.guild.id.clone());
        }
        // History may have moved on while we were away: what is here stays shown and is fetched
        // again, when opened or ahead of that.
        for (channel, log) in self.logs.iter_mut() {
            if !log.loading && self.guilds.iter().any(|g| g.server == server && g.guild.channels.iter().any(|c| c.id == *channel)) {
                log.loaded = false;
            }
        }
        self.preload_server(server, cx);
        if let Some(guild) = self.selected_guild.clone()
            && self.guild(&guild).is_some_and(|g| g.server == server) {
                self.open_guild(guild, cx);
            }
    }

    /// Applies one dispatch; true when something on screen may have changed.
    fn on_dispatch(&mut self, _server: &str, event: Event, cx: &mut Context<Self>) -> bool {
        match event {
            Event::MessageCreate(message) => {
                log::debug!("message {} in {}", message.id, message.channel_id);
                let channel_id = message.channel_id.clone();
                if let Some(typing) = self.typing.get_mut(&channel_id) {
                    typing.retain(|(u, _)| *u != message.author);
                }
                self.count_unread(&message, cx);
                let Some(log) = self.logs.get_mut(&channel_id) else { return true };
                if !log.loaded {
                    return true;
                }
                if log.messages.iter().any(|m| m.id == message.id) {
                    return true;
                }
                // Our own send may already be here under its nonce.
                if let Some(nonce) = &message.nonce
                    && let Some(i) = log.messages.iter().position(|m| m.nonce.as_ref() == Some(nonce) && m.id.starts_with("pending:")) {
                        log.messages[i] = message;
                        cx.emit(ClientEvent::MessageChanged { channel_id, index: i });
                        return true;
                    }
                log.messages.push(message);
                cx.emit(ClientEvent::MessageAppended { channel_id });
            }
            Event::MessageUpdate { id, channel_id, content, edited_at, components, cards } => {
                self.pressing.retain(|(m, _)| *m != id);
                self.deferred.remove(&id);
                if let Some(log) = self.logs.get_mut(&channel_id)
                    && let Some(i) = log.messages.iter().position(|m| m.id == id) {
                        log.messages[i].content = content;
                        log.messages[i].edited_at = edited_at;
                        if let Some(components) = components {
                            log.messages[i].components = components;
                        }
                        if let Some(cards) = cards {
                            log.messages[i].cards = cards;
                        }
                        cx.emit(ClientEvent::MessageChanged { channel_id, index: i });
                    }
            }
            Event::InteractionDeferred { message_id, .. } => {
                self.pressing.retain(|(m, _)| *m != message_id);
                self.deferred.insert(message_id);
            }
            Event::ModalOpen(d) => self.modal = BotModal::parse(&d),
            Event::EphemeralUpdate(message) => {
                self.pressing.retain(|(m, _)| *m != message.id);
                let channel_id = message.channel_id.clone();
                if let Some(log) = self.logs.get_mut(&channel_id) {
                    match log.messages.iter().position(|m| m.id == message.id) {
                        Some(i) => {
                            log.messages[i] = message;
                            cx.emit(ClientEvent::MessageChanged { channel_id, index: i });
                        }
                        None => {
                            log.messages.push(message);
                            cx.emit(ClientEvent::MessageAppended { channel_id });
                        }
                    }
                }
            }
            Event::MessageDelete { id, channel_id } => {
                if let Some(log) = self.logs.get_mut(&channel_id)
                    && let Some(i) = log.messages.iter().position(|m| m.id == id) {
                        log.messages.remove(i);
                        cx.emit(ClientEvent::MessageRemoved { channel_id, index: i });
                    }
            }
            Event::ReactionAdd { message_id, channel_id, emoji, user_id } => {
                self.reaction(channel_id, message_id, emoji, user_id, true, cx);
            }
            Event::ReactionRemove { message_id, channel_id, emoji, user_id } => {
                self.reaction(channel_id, message_id, emoji, user_id, false, cx);
            }
            Event::TypingStop { channel_id, user_id } => {
                if let Some(list) = self.typing.get_mut(&channel_id) {
                    list.retain(|(u, _)| *u != user_id);
                }
            }
            Event::TypingStart { channel_id, user_id } => {
                if Some(user_id.as_str()) == self.user_id() || self.is_request(&channel_id) {
                    return false;
                }
                let entry = self.typing.entry(channel_id).or_default();
                entry.retain(|(u, _)| *u != user_id);
                entry.push((user_id, Instant::now()));
                self.sweep_typing(cx);
            }
            Event::ChannelCreate(channel) | Event::ChannelUpdate(channel) => {
                let Some(gid) = channel.guild_id.clone() else { return false };
                if let Some(entry) = self.guild_mut(&gid) {
                    match entry.guild.channels.iter().position(|c| c.id == channel.id) {
                        Some(i) => entry.guild.channels[i] = channel,
                        None => entry.guild.channels.push(channel),
                    }
                }
            }
            Event::ChannelDelete { id, guild_id } => {
                if let Some(entry) = guild_id.as_deref().and_then(|g| self.guild_mut(g)) {
                    entry.guild.channels.retain(|c| c.id != id);
                }
            }
            Event::GuildUpdate(d) => {
                let id = d.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_owned();
                if let Some(entry) = self.guild_mut(&id)
                    && let Some(name) = d.get("name").and_then(|v| v.as_str()) {
                        entry.guild.name = name.to_owned();
                    }
                // The icon, the alias, the owner: whatever changed, as the server has it now.
                self.refresh_guild(&id, cx);
            }
            Event::RoleChange { guild_id } => self.refresh_guild(&guild_id, cx),
            Event::GuildDelete { id } => {
                self.guilds.retain(|g| g.guild.id != id);
                if self.selected_guild.as_deref() == Some(id.as_str()) {
                    self.selected_guild = self.guilds.first().map(|g| g.guild.id.clone());
                }
            }
            Event::MemberAdd(_) | Event::MemberUpdate(_) | Event::MemberRemove(_) => {
                // Cheap enough to refetch the visible roster.
                let gid = match &event {
                    Event::MemberAdd(d) | Event::MemberUpdate(d) | Event::MemberRemove(d) => {
                        d.get("guild_id").and_then(|v| v.as_str()).map(str::to_owned)
                    }
                    _ => None,
                };
                // Our own roles or timeout changed: so did what we may do.
                let mine = matches!(&event, Event::MemberUpdate(d) if d.get("user_id").and_then(|v| v.as_str()) == self.user_id());
                if mine && let Some(gid) = gid.as_deref() {
                    self.refresh_guild(gid, cx);
                }
                if let Some(gid) = gid
                    && self.members.contains_key(&gid) {
                        self.load_members(gid, cx);
                    }
            }
            Event::PresenceUpdate { user_id, presence } => {
                // The whole object is kept: status, custom status and activities.
                let mut known = false;
                for (gid, ix) in &self.member_ix {
                    if let Some(&i) = ix.get(&user_id)
                        && let Some(m) = self.members.get_mut(gid).and_then(|l| l.get_mut(i))
                    {
                        m.presence = Some(presence.clone());
                        known = true;
                    }
                }
                for f in self.friends.iter_mut().filter(|f| f.user_id == user_id) {
                    f.presence = Some(presence.clone());
                    known = true;
                }
                // Nobody we show: nothing to repaint.
                return known;
            }
            Event::VoiceStateUpdate(state) => {
                // A guild's roster, or a conversation's call (on the home gateway).
                let Some(guild) = state.guild_id.clone().or_else(|| state.conversation_id.clone()) else { return false };
                let roster = self.voice.entry(guild).or_default();
                let before = roster.len();
                if state.channel_id.is_some() {
                    roster.insert(state.user_id.clone(), state.clone());
                } else {
                    roster.remove(&state.user_id);
                }
                let after = roster.len();
                self.ring_on_roster(&state, before, after, cx);
                self.on_voice_roster(&state, cx);
            }
            Event::PinsUpdate { channel_id, .. } => {
                if self.pins.contains_key(&channel_id) {
                    self.load_pins(channel_id, cx);
                }
            }
            Event::ThreadUpsert(thread) => self.remember_thread(thread),
            Event::ThreadDelete { id, parent_id } => {
                if let Some(parent) = parent_id
                    && let Some(list) = self.threads.get_mut(&parent)
                {
                    list.retain(|t| t.id != id);
                }
                for g in &mut self.guilds {
                    g.guild.channels.retain(|c| c.id != id);
                }
            }
            Event::EmojiUpdate { guild_id } => {
                if !guild_id.is_empty() {
                    self.load_emoji(guild_id, cx);
                }
            }
            Event::FriendUpdate(friend) => {
                let before = self.friend_state(&friend.user_id).map(str::to_owned);
                let news = crate::social::friend_news(before.as_deref(), &friend.state);
                let user_id = friend.user_id.clone();
                crate::social::upsert_friend(&mut self.friends, friend);
                if let Some(news) = news {
                    let name = self.display_name(None, &user_id);
                    let text = match news {
                        crate::social::FriendNews::Requested => format!("{name} sent you a friend request."),
                        crate::social::FriendNews::Accepted => format!("{name} accepted your friend request."),
                    };
                    self.flash_notice(text, cx);
                }
            }
            Event::FriendRemove { user_id } => crate::social::remove_friend(&mut self.friends, &user_id),
            Event::ConversationActivity { conversation_id, message } => {
                if message && self.home_conversation.as_deref() != Some(conversation_id.as_str()) && !self.is_request(&conversation_id) {
                    self.unread_conversations.insert(conversation_id.clone());
                }
                // Keep every conversation's group current, not only the one on screen: its
                // epochs move whether or not someone is looking.
                if message && self.dm_engine.is_some() && self.is_dm(&conversation_id) {
                    self.sync_dm(conversation_id, cx);
                }
                self.load_conversations(cx);
            }
            Event::VoiceSignal { channel_id, from, payload } => self.on_voice_signal(&channel_id, from, payload),
            Event::VoiceE2ee { channel_id, data } => self.on_voice_e2ee(&channel_id, data),
            Event::VoiceServerUpdate(data) => self.on_voice_server(data),
            // Screen watchers matter with M4.2.
            Event::VoiceWatchersUpdate { .. } => return false,
            Event::ChannelVisibility { guild_id } => self.reload_channels(&guild_id, cx),
            Event::SessionRevoked(_) => self.on_session_revoked(_server, cx),
            Event::AutomodAction(d) => {
                let rule = d["rule_name"].as_str().or(d["rule"]["name"].as_str()).unwrap_or("a rule");
                let action = d["action"].as_str().unwrap_or("acted");
                self.flash_notice(format!("AutoMod ({action}) caught a message under {rule}."), cx);
            }
            Event::MigrationAlert(d) => {
                let to = d["destination"].as_str().or(d["to"].as_str()).unwrap_or("another server");
                self.flash_notice(format!("A move to {to} is scheduled. If you did not ask for it, cancel it before it runs."), cx);
                self.migration_alert = Some(d);
            }
            Event::RecoveryEmailUpdate(d) => {
                if d["state"].as_str() == Some("bounced") {
                    self.flash_notice("Your recovery email bounced and was retired. Set another in Account & security.", cx);
                }
            }
            Event::MemberListUpdate(_) | Event::Other { .. } => return false,
        }
        true
    }

    /// Presses a bot's control. Link buttons never come here: they only open their address.
    pub fn press_component(&mut self, channel_id: String, message_id: String, custom_id: String, values: Vec<String>, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_channel(&channel_id) else { return };
        let ephemeral = self.find_message(&channel_id, &message_id).is_some_and(|m| m.ephemeral);
        let key = (message_id.clone(), custom_id.clone());
        if !self.pressing.insert(key.clone()) {
            return;
        }
        cx.notify();
        let io = self.io.clone();
        cx.spawn(async move |this, cx| {
            let result = io.spawn(async move { api.interact(&channel_id, &message_id, ephemeral, &custom_id, &values).await }).await;
            // The answer arrives as its own event; a bot that never answers lets go after a while.
            let failed = match result {
                Ok(Ok(_)) => None,
                Ok(Err(err)) => Some(friendly(&anyhow::Error::new(err))),
                Err(err) => Some(err.to_string()),
            };
            this.update(cx, |this, cx| {
                if let Some(err) = failed {
                    this.pressing.remove(&key);
                    this.flash_notice(format!("The bot didn't take that: {err}"), cx);
                }
                cx.notify();
            })
            .ok();
            cx.background_executor().timer(Duration::from_secs(10)).await;
            this.update(cx, |this, cx| {
                if this.pressing.remove(&key) {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Sends what was typed into a bot's form.
    pub fn submit_modal(&mut self, modal: BotModal, values: Vec<String>, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_channel(&modal.channel_id).or_else(|| self.home.as_ref().map(|h| h.api.clone())) else { return };
        let fields: Vec<(String, String)> = modal.fields.iter().zip(values).map(|(f, v)| (f.custom_id.clone(), v)).collect();
        for (field, (_, value)) in modal.fields.iter().zip(&fields) {
            let len = value.chars().count() as u32;
            if field.required && value.trim().is_empty() {
                self.flash_notice(format!("{} is required.", field.label), cx);
                return;
            }
            if field.min.is_some_and(|m| len < m && !value.is_empty()) || field.max.is_some_and(|m| len > m) {
                self.flash_notice(format!("{} has the wrong length.", field.label), cx);
                return;
            }
        }
        let id = modal.interaction_id.clone();
        self.fire(cx, async move { api.submit_modal(&id, &fields).await.map(drop) });
    }

    /// Fetches a guild's channels again (what this session may see changed).
    fn reload_channels(&mut self, guild_id: &str, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_guild(guild_id) else { return };
        let (io, gid) = (self.io.clone(), guild_id.to_owned());
        cx.spawn(async move |this, cx| {
            let id = gid.clone();
            let fetched = io.spawn(async move { api.channels(&id).await }).await;
            this.update(cx, |this, cx| match fetched {
                Ok(Ok(channels)) => {
                    if let Some(entry) = this.guild_mut(&gid) {
                        entry.guild.channels = channels;
                    }
                    cx.notify();
                }
                Ok(Err(err)) => log::debug!("channels of {gid}: {err}"),
                Err(err) => log::debug!("channels of {gid}: {err}"),
            })
            .ok();
        })
        .detach();
    }

    /// A server ended one of our sessions. A guild server's is minted again on the close that
    /// follows; if the home server no longer knows this device, it was signed out from elsewhere.
    fn on_session_revoked(&mut self, server: &str, cx: &mut Context<Self>) {
        let Some(home) = &self.home else { return };
        if home.doc.server_name != server {
            return;
        }
        let (api, io) = (home.api.clone(), self.io.clone());
        cx.spawn(async move |this, cx| {
            let check = io.spawn(async move { api.whoami().await }).await;
            if let Ok(Err(err)) = check
                && err.status() == Some(401)
            {
                this.update(cx, |this, cx| {
                    this.sign_out(cx);
                    this.flash_notice("This device was signed out from another device.", cx);
                })
                .ok();
            }
        })
        .detach();
    }

    /// Keeps one task that drops expired typing entries and repaints when some expired, for as
    /// long as anyone is typing anywhere.
    fn sweep_typing(&mut self, cx: &mut Context<Self>) {
        if self.typing_sweep.is_some() {
            return;
        }
        self.typing_sweep = Some(cx.spawn(async move |this, cx| {
            loop {
                // The slot is cleared in the same update that finds nothing left, so an entry
                // added afterwards starts a new sweep.
                let next = this.update(cx, |this, _| {
                    let next = this.typing.values().flatten().map(|(_, at)| *at + TYPING_SHOWN_FOR).min();
                    if next.is_none() {
                        this.typing_sweep = None;
                    }
                    next
                });
                let Ok(Some(next)) = next else { break };
                let wait = next.saturating_duration_since(Instant::now());
                cx.background_executor().timer(wait + Duration::from_millis(10)).await;
                let more = this.update(cx, |this, cx| {
                    let now = Instant::now();
                    for list in this.typing.values_mut() {
                        list.retain(|(_, at)| now.duration_since(*at) < TYPING_SHOWN_FOR);
                    }
                    this.typing.retain(|_, list| !list.is_empty());
                    cx.notify();
                    if this.typing.is_empty() {
                        this.typing_sweep = None;
                    }
                    !this.typing.is_empty()
                });
                if !matches!(more, Ok(true)) {
                    break;
                }
            }
        }));
    }

    fn reaction(&mut self, channel_id: String, message_id: String, emoji: String, user_id: String, add: bool, cx: &mut Context<Self>) {
        let me = self.user_id().map(str::to_owned);
        let Some(log) = self.logs.get_mut(&channel_id) else { return };
        let Some(i) = log.messages.iter().position(|m| m.id == message_id) else { return };
        apply_reaction(&mut log.messages[i].reactions, &emoji, &user_id, add, me.as_deref());
        cx.emit(ClientEvent::MessageChanged { channel_id, index: i });
    }

    // ---------------------------------------------------------------- guilds & channels

    pub fn guild(&self, id: &str) -> Option<&GuildEntry> {
        self.guilds.iter().find(|g| g.guild.id == id)
    }

    fn guild_mut(&mut self, id: &str) -> Option<&mut GuildEntry> {
        self.guilds.iter_mut().find(|g| g.guild.id == id)
    }

    pub fn guild_of_channel(&self, channel_id: &str) -> Option<&GuildEntry> {
        self.guilds.iter().find(|g| g.guild.channels.iter().any(|c| c.id == channel_id))
    }

    pub fn channel(&self, channel_id: &str) -> Option<&Channel> {
        self.guild_of_channel(channel_id)
            .and_then(|g| g.guild.channels.iter().find(|c| c.id == channel_id))
    }

    fn server_of_channel(&self, channel_id: &str) -> Option<&str> {
        self.guild_of_channel(channel_id).map(|g| g.server.as_str())
    }

    /// A guild's server connection and the runtime to call it on, for views that talk to it
    /// directly (the guild settings, whose answers only they need).
    pub fn guild_api(&self, guild_id: &str) -> Option<(Api, Io)> {
        Some((self.api_for_guild(guild_id)?, self.io.clone()))
    }

    /// Fetches a guild again (its roles, our member with our permissions, name, icon, alias)
    /// after something changed it. One request at a time per guild; a change during one runs
    /// it once more when it lands.
    pub fn refresh_guild(&mut self, guild_id: &str, cx: &mut Context<Self>) {
        if guild_id.is_empty() || self.guild(guild_id).is_none() {
            return;
        }
        if let Some(again) = self.guild_refreshes.get_mut(guild_id) {
            *again = true;
            return;
        }
        let Some(api) = self.api_for_guild(guild_id) else { return };
        self.guild_refreshes.insert(guild_id.to_owned(), false);
        let (io, gid) = (self.io.clone(), guild_id.to_owned());
        cx.spawn(async move |this, cx| {
            let id = gid.clone();
            let fetched = io.spawn(async move { api.guild(&id).await }).await;
            this.update(cx, |this, cx| {
                let again = this.guild_refreshes.remove(&gid).unwrap_or(false);
                match fetched {
                    Ok(Ok(fresh)) => {
                        if let Some(entry) = this.guild_mut(&gid) {
                            let g = &mut entry.guild;
                            g.name = fresh.name;
                            g.icon_ref = fresh.icon_ref;
                            g.owner = fresh.owner.or(g.owner.take());
                            g.alias = fresh.alias;
                            g.media_expiry_limit_secs = fresh.media_expiry_limit_secs;
                            g.external_emoji = fresh.external_emoji;
                            g.emoji_export_policy = fresh.emoji_export_policy;
                            g.emoji_export_role_id = fresh.emoji_export_role_id;
                            g.created_at = fresh.created_at.or(g.created_at);
                            g.roles = fresh.roles;
                            if fresh.me.is_some() {
                                g.me = fresh.me;
                            }
                            if fresh.member_count.is_some() {
                                g.member_count = fresh.member_count;
                            }
                        }
                        cx.notify();
                    }
                    Ok(Err(err)) => log::debug!("refreshing guild {gid}: {err}"),
                    Err(err) => log::debug!("refreshing guild {gid}: {err}"),
                }
                if again {
                    this.refresh_guild(&gid, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn api_for_guild(&self, guild_id: &str) -> Option<Api> {
        let server = &self.guild(guild_id)?.server;
        self.server(server)?.api.clone()
    }

    fn api_for_channel(&self, channel_id: &str) -> Option<Api> {
        let server = self.server_of_channel(channel_id)?;
        self.server(server)?.api.clone()
    }

    /// The API origin of the server that holds a channel (for pictures it serves, such as a bot
    /// card's camo image).
    pub fn api_base_of_channel(&self, channel_id: &str) -> Option<String> {
        let server = self.server_of_channel(channel_id)?;
        self.server(server)?.api.as_ref().map(|a| a.base().to_owned())
    }

    pub fn selected_channel(&self) -> Option<&str> {
        if self.showing_home {
            // An open private conversation is shown in the chat view like a channel.
            return self.home_conversation.as_deref();
        }
        let guild = self.selected_guild.as_ref()?;
        self.selected_channel.get(guild).map(String::as_str)
    }

    pub fn show_home(&mut self, cx: &mut Context<Self>) {
        self.showing_home = true;
        self.load_friends(cx);
        self.load_conversations(cx);
        cx.notify();
    }

    /// Friends page (`None`) or a conversation in the home view.
    pub fn open_conversation(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        self.showing_home = true;
        if let Some(id) = &id {
            self.unread_conversations.remove(id);
        }
        self.home_conversation = id.clone();
        if let Some(id) = id {
            self.sync_dm(id, cx);
        }
        cx.notify();
    }

    pub fn conversation_unread(&self, id: &str) -> bool {
        self.unread_conversations.contains(id)
    }

    pub fn incoming_friend_requests(&self) -> usize {
        crate::social::incoming_requests(&self.friends)
    }

    /// Incoming requests not looked at yet, for the mark on Direct messages.
    pub fn unseen_friend_requests(&self) -> usize {
        let Some(me) = self.user_id() else { return 0 };
        crate::social::unseen_requests(&self.friends, me, &self.prefs.seen_requests)
    }

    /// Remembers the incoming requests as looked at (the Pending tab is on screen). Requests
    /// that are gone are forgotten, other accounts' kept.
    pub fn mark_requests_seen(&mut self, cx: &mut Context<Self>) {
        if self.unseen_friend_requests() == 0 {
            return;
        }
        let Some(me) = self.user_id().map(str::to_owned) else { return };
        let mine = format!("{me} ");
        let mut seen: Vec<String> = self.prefs.seen_requests.iter().filter(|k| !k.starts_with(&mine)).cloned().collect();
        seen.extend(self.friends.iter().filter(|f| f.state == "pending_in").map(|f| crate::social::seen_key(&me, &f.user_id)));
        self.prefs.seen_requests = seen;
        self.save_prefs();
        cx.notify();
    }

    pub fn friend_state(&self, user_id: &str) -> Option<&str> {
        self.friends.iter().find(|f| f.user_id == user_id).map(|f| f.state.as_str())
    }

    pub fn conversation_title(&self, c: &rhein_proto::Conversation) -> String {
        let me = self.user_id().unwrap_or_default();
        crate::social::conversation_title(c, me, |u| self.friend_or_local_name(u))
    }

    /// A name for someone outside any guild context: their friend entry, else the id's localpart.
    pub fn friend_or_local_name(&self, user_id: &str) -> String {
        self.friends
            .iter()
            .find(|f| f.user_id == user_id)
            .and_then(|f| f.display_name.clone())
            .unwrap_or_else(|| self.display_name(None, user_id))
    }

    /// A friend's picture through the home server (their media is proxied when remote).
    /// Anyone's picture, from wherever it is known: a guild's member list, the friends list,
    /// or their fetched profile (through the home server's federation proxy).
    pub fn person_avatar(&self, user_id: &str) -> Option<String> {
        if let Some(url) = self.guilds.iter().find_map(|g| self.avatar_url(Some(&g.guild.id), user_id)) {
            return Some(url);
        }
        if let Some(url) = self.friends.iter().find(|f| f.user_id == user_id).and_then(|f| self.friend_avatar(f)) {
            return Some(url);
        }
        if let (Some(home), Some(reference)) = (self.home.as_ref(), self.cached_profile(user_id).and_then(|p| p.avatar_ref.clone()).filter(|r| !r.is_empty())) {
            let origin = crate::media::remote_origin(user_id, &home.doc.server_name);
            return Some(crate::media::media_url(&home.doc.api, &reference, origin));
        }
        self.avatar_url(None, user_id)
    }

    pub fn friend_avatar(&self, friend: &rhein_proto::Friend) -> Option<String> {
        let home = self.home.as_ref()?;
        let reference = friend.avatar_ref.as_deref().filter(|r| !r.is_empty())?;
        let origin = crate::media::remote_origin(&friend.user_id, &home.doc.server_name);
        Some(crate::media::media_url(&home.doc.api, reference, origin))
    }

    pub fn load_friends(&mut self, cx: &mut Context<Self>) {
        let Some(api) = self.home.as_ref().map(|h| h.api.clone()) else { return };
        let io = self.io.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move { Ok(api.friends().await?) })).await;
            this.update(cx, |this, cx| {
                if let Ok(list) = result {
                    this.friends = list;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn load_conversations(&mut self, cx: &mut Context<Self>) {
        let Some(api) = self.home.as_ref().map(|h| h.api.clone()) else { return };
        let io = self.io.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move { Ok(api.conversations().await?) })).await;
            this.update(cx, |this, cx| {
                if let Ok(list) = result {
                    this.conversations = list;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Friend actions; the gateway's FRIEND_UPDATE / FRIEND_REMOVE brings the result.
    pub fn friend_action(&mut self, action: FriendAction, user_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.home.as_ref().map(|h| h.api.clone()) else { return };
        let io = self.io.clone();
        cx.spawn(async move |this, cx| {
            let uid = user_id.clone();
            let result = join(io.spawn(async move {
                match action {
                    FriendAction::Request => api.request_friend(&uid).await?,
                    FriendAction::Accept => api.accept_friend(&uid).await?,
                    FriendAction::Remove => api.remove_friend(&uid).await?,
                    FriendAction::Block => api.set_blocked(&uid, true).await?,
                    FriendAction::Unblock => api.set_blocked(&uid, false).await?,
                };
                Ok(api.friends().await?)
            }))
            .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(list) => {
                        this.friends = list;
                        if action == FriendAction::Request {
                            this.notice = Some(format!("Friend request sent to {user_id}.").into());
                        }
                    }
                    Err(err) => this.error = Some(format!("Couldn't update friends: {}", friendly(&err)).into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn select_guild(&mut self, id: String, cx: &mut Context<Self>) {
        self.showing_home = false;
        self.selected_guild = Some(id.clone());
        self.open_guild(id, cx);
        cx.notify();
    }

    fn open_guild(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(entry) = self.guild(&id) else { return };
        let current = self.selected_channel.get(&id).cloned();
        let valid = current.filter(|c| entry.guild.channels.iter().any(|ch| &ch.id == c));
        let channel = valid.or_else(|| {
            entry.ordered_channels().into_iter().find(|c| c.is_textual()).map(|c| c.id.clone())
        });
        if let Some(channel) = channel {
            self.select_channel(channel, cx);
        }
        if !self.emoji.contains_key(&id) {
            self.load_emoji(id.clone(), cx);
        }
        if !self.members.contains_key(&id) {
            self.load_members(id, cx);
        }
    }

    pub fn select_channel(&mut self, channel_id: String, cx: &mut Context<Self>) {
        let Some(guild) = self.guild_of_channel(&channel_id).map(|g| g.guild.id.clone()) else { return };
        // The channel left keeps its newest messages on disk, as it was last seen.
        if let Some(left) = self.selected_channel().filter(|c| *c != channel_id).map(str::to_owned) {
            self.keep_on_disk(&left);
        }
        self.showing_home = false;
        self.selected_guild = Some(guild.clone());
        self.selected_channel.insert(guild, channel_id.clone());
        self.visited.retain(|c| *c != channel_id);
        self.visited.insert(0, channel_id.clone());
        self.visited.truncate(50);
        // A voice channel's chat starts closed; what is said there stays unread until it opens.
        self.voice_chat_open = None;
        if self.channel(&channel_id).is_some_and(|c| c.is_voice()) {
            cx.notify();
            return;
        }
        let had_unread = self.read.get(&channel_id).is_some_and(|m| m.unread > 0 || m.mentions > 0);
        let divider = self.read.get(&channel_id).filter(|m| m.unread > 0).and_then(|m| {
            m.first_unread.clone().map(Divider::At).or_else(|| m.last_read.clone().map(Divider::After))
        });
        match divider {
            Some(d) => {
                self.dividers.insert(channel_id.clone(), d);
            }
            None if had_unread => {}
            None => {
                self.dividers.remove(&channel_id);
            }
        }
        if let Some(mark) = self.read.get_mut(&channel_id) {
            mark.unread = 0;
            mark.mentions = 0;
            mark.first_unread = None;
        }
        // Neither a voice channel nor a forum (whose posts are threads) has history of its own.
        let voice = self.channel(&channel_id).is_some_and(|c| c.is_voice() || c.is_forum());
        // Thread chips on messages need the channel's threads (title and message count); a
        // forum lists them as its posts.
        let has_threads = self.channel(&channel_id).is_some_and(|c| matches!(c.kind.as_str(), "text" | "announcement" | "forum"));
        if has_threads && !self.threads.contains_key(&channel_id) {
            self.threads.insert(channel_id.clone(), Vec::new());
            self.load_threads(channel_id.clone(), cx);
        }
        let needs_load = !voice && self.logs.get(&channel_id).is_none_or(|l| !l.loaded && !l.loading);
        if needs_load {
            self.load_history(channel_id, false, cx);
        } else if had_unread && !voice {
            self.ack_latest(&channel_id, cx);
        }
        cx.notify();
    }

    /// Loads the newest page, or with `older` the page before the oldest message we hold.
    pub fn load_history(&mut self, channel_id: String, older: bool, cx: &mut Context<Self>) {
        self.load_page(channel_id, older, false, cx);
    }

    /// As [`Client::load_history`]; `ahead` for a channel not opened (see `preload`), which is
    /// not marked read.
    fn load_page(&mut self, channel_id: String, older: bool, ahead: bool, cx: &mut Context<Self>) {
        if self.is_dm(&channel_id) {
            // An encrypted conversation's whole history is local; there is nothing older to page.
            if !older {
                self.sync_dm(channel_id, cx);
            }
            return;
        }
        let Some(api) = self.api_for_channel(&channel_id) else { return };
        let log = self.logs.entry(channel_id.clone()).or_default();
        // What was kept from last time shows until the server answers.
        if !older && log.messages.is_empty() && !log.loading {
            log.messages = preload::read_kept(&channel_id);
            if !log.messages.is_empty() {
                cx.emit(ClientEvent::MessagesReset { channel_id: channel_id.clone() });
            }
        }
        let log = self.logs.entry(channel_id.clone()).or_default();
        if log.loading || (older && log.reached_start) {
            return;
        }
        let before = if older {
            log.messages.iter().find(|m| !m.id.starts_with("pending:")).map(|m| m.id.clone())
        } else {
            None
        };
        if older && before.is_none() {
            return;
        }
        log.loading = true;
        log.error = None;
        let io = self.io.clone();
        let chan = channel_id.clone();
        let since = Instant::now();
        log.task = Some(cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move {
                Ok(api.messages(&chan, before.as_deref(), PAGE).await?)
            }))
            .await;
            this.update(cx, |this, cx| {
                let Some(log) = this.logs.get_mut(&channel_id) else { return };
                log.loading = false;
                match result {
                    Ok(page) => {
                        log::debug!(target: "rhein::switch", "{channel_id}: history of {} message(s) in {:?}", page.len(), since.elapsed());
                        let full = page.len() as u32 >= PAGE;
                        if older {
                            let count = page.len();
                            log.messages.splice(0..0, page);
                            log.reached_start = !full;
                            cx.emit(ClientEvent::MessagesPrepended { channel_id: channel_id.clone(), count });
                        } else {
                            // Fetched again (after a reconnect, or over what was kept on disk)
                            // and nothing changed: the list stays as it is, scrolled where it was.
                            let same = log.messages.len() == page.len()
                                && log.messages.iter().zip(&page).all(|(a, b)| a.id == b.id && a.content == b.content && a.edited_at == b.edited_at);
                            log.messages = page;
                            log.loaded = true;
                            log.reached_start = !full;
                            if !same {
                                cx.emit(ClientEvent::MessagesReset { channel_id: channel_id.clone() });
                            }
                            if !ahead {
                                this.ack_latest(&channel_id, cx);
                            }
                            this.keep_on_disk(&channel_id);
                        }
                    }
                    Err(err) => log.error = Some(friendly(&err)),
                }
                this.preloaded(&channel_id, cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn ack_latest(&mut self, channel_id: &str, _cx: &mut Context<Self>) {
        let Some(api) = self.api_for_channel(channel_id) else { return };
        let Some(last) = self.logs.get(channel_id).and_then(|l| l.messages.last()) else { return };
        if last.id.starts_with("pending:") {
            return;
        }
        let (chan, id) = (channel_id.to_owned(), last.id.clone());
        self.io.spawn(async move {
            let _ = api.ack(&chan, &id).await;
        });
    }

    fn load_read_state(&mut self, guild_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_guild(&guild_id) else { return };
        let io = self.io.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move { Ok(api.read_state(&guild_id).await?) })).await;
            this.update(cx, |this, cx| {
                let Ok(list) = result else { return };
                let open = this.selected_channel().map(str::to_owned);
                for ch in list.channels {
                    // What is on screen right now is read, whatever the server counted.
                    let on_screen = open.as_deref() == Some(ch.channel_id.as_str());
                    this.read.insert(
                        ch.channel_id,
                        ReadMark {
                            unread: if on_screen { 0 } else { ch.unread },
                            mentions: if on_screen { 0 } else { ch.mentions },
                            muted: ch.muted,
                            last_read: ch.last_read_id,
                            first_unread: None,
                        },
                    );
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A message arrived: count it as unread unless it is ours or on screen, and acknowledge it
    /// when it is on screen.
    fn count_unread(&mut self, message: &Message, cx: &mut Context<Self>) {
        let Some(me) = self.user_id().map(str::to_owned) else { return };
        if message.author == me {
            return;
        }
        // As the server notifies: a mention of you or a role you hold, @everyone or @here (not
        // inside code), or a reply to one of your messages.
        let roles = self.guild_of_channel(&message.channel_id).map(|g| self.my_roles(&g.guild.id)).unwrap_or_default();
        let replied = message.reply_author.as_deref().or_else(|| {
            message.reply_to.as_deref().and_then(|p| self.find_message(&message.channel_id, p)).map(|p| p.author.as_str())
        }) == Some(me.as_str());
        let mention = replied || crate::markdown::mentions_me(&crate::markdown::parse(&message.content), &me, &roles);
        // A voice channel is in front of you only with its chat open.
        let voice = self.channel(&message.channel_id).is_some_and(|c| c.is_voice());
        let here = self.selected_channel() == Some(message.channel_id.as_str()) && (!voice || self.voice_chat_open.as_deref() == Some(message.channel_id.as_str()));
        if !(here && self.window_active) && !self.read_mark(&message.channel_id).muted {
            if mention {
                let guild = self.guild_of_channel(&message.channel_id).map(|g| g.guild.id.clone());
                let sender = self.display_name(guild.as_deref(), &message.author);
                let place = self.channel(&message.channel_id).map(|c| format!("#{}", c.name));
                self.alert(notify::Alert::Mention, &sender, place, &message.content, cx);
            } else {
                self.message_sound(cx);
            }
        }
        if here {
            if let Some(api) = self.api_for_channel(&message.channel_id) {
                let (chan, id) = (message.channel_id.clone(), message.id.clone());
                self.io.spawn(async move {
                    let _ = api.ack(&chan, &id).await;
                });
            }
            return;
        }
        let mark = self.read.entry(message.channel_id.clone()).or_default();
        mark.first_unread.get_or_insert_with(|| message.id.clone());
        mark.unread += 1;
        if mention {
            mark.mentions += 1;
        }
        cx.notify();
    }

    pub fn read_mark(&self, channel_id: &str) -> ReadMark {
        let entry = self.guild_of_channel(channel_id);
        let parent = entry
            .and_then(|g| g.guild.channels.iter().find(|c| c.id == channel_id))
            .and_then(|c| c.parent_id.as_deref());
        self.mark_with(channel_id, parent, entry.map(|g| g.guild.id.as_str()))
    }

    /// `read_mark` for a channel whose guild is already at hand, without looking either up.
    pub fn read_mark_in(&self, entry: &GuildEntry, channel: &Channel) -> ReadMark {
        self.mark_with(&channel.id, channel.parent_id.as_deref(), Some(&entry.guild.id))
    }

    fn mark_with(&self, channel_id: &str, parent: Option<&str>, guild: Option<&str>) -> ReadMark {
        let mut mark = self.read.get(channel_id).cloned().unwrap_or_default();
        // Muted here, or its category or its guild muted, in the account's preferences.
        if !mark.muted {
            mark.muted = [Some(channel_id), parent, guild].into_iter().flatten().any(|id| self.is_muted(id));
        }
        mark
    }

    /// Whether a guild, category or channel is muted in the account's preferences (`muted` in
    /// `prefs.v1`, shared with the web client): for good (`true`) or until a moment.
    pub fn is_muted(&self, id: &str) -> bool {
        match self.account_pref(&["muted", id]) {
            Some(serde_json::Value::Bool(b)) => *b,
            Some(v) => v.as_i64().is_some_and(|until| until > now_ms()),
            None => false,
        }
    }

    /// When a mute ends: `Some(None)` for "until turned back on", `None` when not muted.
    pub fn muted_until(&self, id: &str) -> Option<Option<i64>> {
        match self.account_pref(&["muted", id]) {
            Some(serde_json::Value::Bool(true)) => Some(None),
            Some(v) => v.as_i64().filter(|u| *u > now_ms()).map(Some),
            None => None,
        }
    }

    /// Mutes for `for_ms` (`None`: until turned back on), or unmutes with `unmute`.
    pub fn set_muted(&mut self, id: &str, for_ms: Option<i64>, unmute: bool, cx: &mut Context<Self>) {
        let value = if unmute {
            None
        } else {
            Some(match for_ms {
                Some(ms) => serde_json::json!(now_ms() + ms),
                None => serde_json::json!(true),
            })
        };
        self.set_account_pref(&["muted", id], value, cx);
        // Draw again when a timed mute runs out.
        if let (false, Some(ms)) = (unmute, for_ms) {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(ms.max(0) as u64 + 500)).await;
                this.update(cx, |_, cx| cx.notify()).ok();
            })
            .detach();
        }
    }

    /// Whether anything in the guild is unread, and how many mentions wait there.
    pub fn guild_unread(&self, guild_id: &str) -> (bool, u32) {
        let Some(entry) = self.guild(guild_id) else { return (false, 0) };
        entry.guild.channels.iter().fold((false, 0), |(unread, mentions), c| {
            let mark = self.read_mark_in(entry, c);
            (unread || mark.is_unread(), mentions + mark.mentions)
        })
    }

    /// Mentions across every guild, for the window title and the home button.
    pub fn total_mentions(&self) -> u32 {
        self.read.values().map(|m| m.mentions).sum()
    }

    /// Who is in a voice channel right now, and in what state.
    pub fn voice_members(&self, guild_id: &str, channel_id: &str) -> Vec<rhein_proto::VoiceState> {
        let mut users: Vec<rhein_proto::VoiceState> = self
            .voice
            .get(guild_id)
            .map(|roster| roster.values().filter(|v| v.channel_id.as_deref() == Some(channel_id)).cloned().collect())
            .unwrap_or_default();
        users.sort_by(|a, b| a.user_id.cmp(&b.user_id));
        users
    }

    // ---------------------------------------------------------------- permissions

    /// Whether the message bar in a channel is open. A guild channel whose permissions are known
    /// and lack SEND_MESSAGES is closed; one not loaded yet stays open (no flicker while READY
    /// arrives), and a conversation always is.
    pub fn may_write(&self, channel_id: &str) -> bool {
        match self.guild_of_channel(channel_id) {
            Some(entry) if entry.guild.me.is_some() => self.can(channel_id, crate::perms::SEND_MESSAGES),
            _ => true,
        }
    }

    /// The colour a member's name is drawn in: that of their highest role with a colour, as
    /// the web client and Discord draw it (0xRRGGBB).
    pub fn name_color(&self, guild_id: &str, user_id: &str) -> Option<u32> {
        let entry = self.guild(guild_id)?;
        let held: Vec<String> = if self.user_id() == Some(user_id) {
            self.my_roles(guild_id)
        } else {
            self.member(guild_id, user_id)?.roles.clone()
        };
        entry.guild.roles.iter().filter(|r| r.rgb().is_some() && held.contains(&r.id)).max_by_key(|r| r.position).and_then(|r| r.rgb())
    }

    /// The roles this user holds in a guild, from READY.
    pub fn my_roles(&self, guild_id: &str) -> Vec<String> {
        self.guild(guild_id).and_then(|g| g.guild.me.as_ref()).map(roles_of).unwrap_or_default()
    }

    /// This user's permissions in a channel, from READY's roles and overwrites. Threads inherit
    /// their parent's.
    pub fn permissions(&self, channel_id: &str) -> u64 {
        let Some(entry) = self.guild_of_channel(channel_id) else { return 0 };
        let Some(me) = entry.guild.me.as_ref() else { return 0 };
        let Some(user_id) = self.user_id() else { return 0 };
        let mut channel = entry.guild.channels.iter().find(|c| c.id == channel_id);
        if let Some(c) = channel.filter(|c| c.is_thread())
            && let Some(parent) = c.parent_id.as_deref()
        {
            channel = entry.guild.channels.iter().find(|p| p.id == parent).or(channel);
        }
        let str_field = |key: &str| me.get(key).and_then(|v| v.as_str()).unwrap_or_default();
        let roles = roles_of(me);
        let timed_out = me.get("timeout_until").and_then(|v| v.as_i64()).is_some_and(|until| until > now_ms());
        let owner = entry.guild.owner.as_deref() == Some(user_id)
            || me.get("is_owner").and_then(|v| v.as_bool()).unwrap_or(false);
        let member = crate::perms::Member {
            user_id,
            guild_id: &entry.guild.id,
            guild_permissions: if owner { u64::MAX } else { crate::perms::parse_bits(str_field("permissions")) },
            role_ids: &roles,
            timed_out,
        };
        let overwrites = channel.map(|c| c.permission_overwrites.as_slice()).unwrap_or_default();
        crate::perms::channel_permissions(&member, overwrites)
    }

    pub fn can(&self, channel_id: &str, bit: u64) -> bool {
        self.permissions(channel_id) & bit != 0
    }

    // ---------------------------------------------------------------- pins

    pub fn pins(&self, channel_id: &str) -> Option<&[Message]> {
        self.pins.get(channel_id).map(Vec::as_slice)
    }

    pub fn is_pinned(&self, channel_id: &str, message_id: &str) -> bool {
        self.pins.get(channel_id).is_some_and(|p| p.iter().any(|m| m.id == message_id))
    }

    pub fn load_pins(&mut self, channel_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_channel(&channel_id) else { return };
        let io = self.io.clone();
        let chan = channel_id.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move { Ok(api.pins(&chan).await?) })).await;
            this.update(cx, |this, cx| {
                if let Ok(pins) = result {
                    this.pins.insert(channel_id, pins);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn set_pinned(&mut self, channel_id: String, message_id: String, pinned: bool, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_channel(&channel_id) else { return };
        self.fire(cx, async move { api.set_pinned(&channel_id, &message_id, pinned).await.map(drop) });
    }

    // ---------------------------------------------------------------- threads

    pub fn threads_of(&self, channel_id: &str) -> Option<&[rhein_proto::Thread]> {
        self.threads.get(channel_id).map(Vec::as_slice)
    }

    pub fn thread(&self, thread_id: &str) -> Option<&rhein_proto::Thread> {
        self.threads.values().flatten().find(|t| t.id == thread_id)
    }

    /// Keeps a thread in its parent's list and makes it addressable as a channel, so history,
    /// sending and permissions work for it like for any other channel.
    fn remember_thread(&mut self, thread: rhein_proto::Thread) {
        let parent = thread.parent_id.clone();
        if let Some(entry) = self.guilds.iter_mut().find(|g| g.guild.channels.iter().any(|c| c.id == parent)) {
            match entry.guild.channels.iter_mut().find(|c| c.id == thread.id) {
                Some(c) => c.name = thread.title.clone(),
                None => entry.guild.channels.push(Channel {
                    id: thread.id.clone(),
                    guild_id: Some(entry.guild.id.clone()),
                    kind: "thread".into(),
                    name: thread.title.clone(),
                    parent_id: Some(parent.clone()),
                    ..Default::default()
                }),
            }
        }
        let list = self.threads.entry(parent).or_default();
        match list.iter_mut().find(|t| t.id == thread.id) {
            Some(existing) => *existing = thread,
            None => list.insert(0, thread),
        }
    }

    pub fn load_threads(&mut self, channel_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_channel(&channel_id) else { return };
        let io = self.io.clone();
        let chan = channel_id.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move {
                let mut all = api.threads(&chan, false).await?;
                all.extend(api.threads(&chan, true).await?);
                Ok(all)
            }))
            .await;
            this.update(cx, |this, cx| {
                if let Ok(threads) = result {
                    this.threads.insert(channel_id.clone(), Vec::new());
                    for t in threads {
                        this.remember_thread(t);
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Creates a thread (optionally from a message) and reports its id through `ClientEvent::OpenThread`.
    pub fn create_thread(&mut self, channel_id: String, title: String, message_id: Option<String>, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_channel(&channel_id) else { return };
        let io = self.io.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move {
                Ok(api.create_thread(&channel_id, title.trim(), message_id.as_deref()).await?)
            }))
            .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(thread) => {
                        let id = thread.id.clone();
                        this.remember_thread(thread);
                        cx.emit(ClientEvent::OpenThread { thread_id: id });
                    }
                    Err(err) => this.error = Some(format!("Couldn't create thread: {}", friendly(&err)).into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---------------------------------------------------------------- custom emoji

    pub fn guild_emoji(&self, guild_id: &str) -> &[rhein_proto::CustomEmoji] {
        self.emoji.get(guild_id).map(Vec::as_slice).unwrap_or_default()
    }

    fn load_emoji(&mut self, guild_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_guild(&guild_id) else { return };
        let io = self.io.clone();
        let gid = guild_id.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move { Ok(api.expressions(&gid).await?) })).await;
            this.update(cx, |this, cx| {
                this.emoji.insert(guild_id, result.unwrap_or_default());
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Remembers an emoji as recently used (unicode character or `<name:id>` token).
    pub fn use_emoji(&mut self, emoji: String) {
        self.recent_emoji.retain(|e| *e != emoji);
        self.recent_emoji.insert(0, emoji);
        self.recent_emoji.truncate(32);
        let mut prefs = crate::prefs::load();
        prefs.recent_emoji = self.recent_emoji.clone();
        crate::prefs::store(&prefs);
    }

    // ---------------------------------------------------------------- search

    pub fn run_search(&mut self, guild_id: String, query: rhein_proto::SearchQuery, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_guild(&guild_id) else { return };
        let append = query.offset > 0;
        if !append {
            self.search = Some(SearchState { guild_id: guild_id.clone(), query: query.clone(), ..Default::default() });
        }
        if let Some(state) = self.search.as_mut() {
            state.loading = true;
            state.error = None;
        }
        cx.notify();
        let io = self.io.clone();
        let asked = query.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move { Ok(api.search(&guild_id, &asked).await?) })).await;
            this.update(cx, |this, cx| {
                let Some(state) = this.search.as_mut() else { return };
                if state.query.q != query.q || state.query.channel_id != query.channel_id || state.query.author != query.author || state.query.has != query.has {
                    return; // a newer search replaced this one
                }
                state.loading = false;
                match result {
                    Ok(page) => {
                        state.total = page.total;
                        state.has_more = page.has_more;
                        state.query.offset = query.offset + page.results.len() as u32;
                        state.results.extend(page.results);
                    }
                    Err(err) => state.error = Some(friendly(&err)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn search_more(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.search.as_ref().filter(|s| s.has_more && !s.loading) else { return };
        let (guild, query) = (state.guild_id.clone(), state.query.clone());
        self.run_search(guild, query, cx);
    }

    pub fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.search = None;
        cx.notify();
    }

    /// Opens the channel a message lives in and scrolls to it.
    pub fn open_message(&mut self, channel_id: String, message_id: String, cx: &mut Context<Self>) {
        if self.selected_channel() != Some(channel_id.as_str()) {
            self.select_channel(channel_id.clone(), cx);
            // History for the channel is loading; jump once it is there.
            cx.spawn(async move |this, cx| {
                for _ in 0..50 {
                    cx.background_executor().timer(Duration::from_millis(100)).await;
                    let ready = this
                        .read_with(cx, |this, _| this.logs.get(&channel_id).is_some_and(|l| l.loaded))
                        .unwrap_or(false);
                    if ready {
                        break;
                    }
                }
                this.update(cx, |this, cx| this.jump_to(channel_id, message_id, cx)).ok();
            })
            .detach();
        } else {
            self.jump_to(channel_id, message_id, cx);
        }
    }

    // ---------------------------------------------------------------- member lookup

    /// Members matching a name: the loaded roster immediately, the server's answer once it arrives.
    pub fn lookup_members(&mut self, guild_id: &str, query: &str, cx: &mut Context<Self>) -> Vec<Member> {
        let q = query.to_lowercase();
        let mut found: Vec<Member> = self
            .members
            .get(guild_id)
            .map(|list| {
                list.iter()
                    .filter(|m| m.user_id.to_lowercase().contains(&q) || m.shown_name().to_lowercase().contains(&q))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let key = format!("{guild_id}\0{q}");
        match self.member_lookup.get(&key) {
            Some(remote) => {
                for m in remote {
                    if !found.iter().any(|f| f.user_id == m.user_id) {
                        found.push(m.clone());
                    }
                }
            }
            None if !q.is_empty() => {
                self.member_lookup.insert(key.clone(), Vec::new());
                if let Some(api) = self.api_for_guild(guild_id) {
                    let io = self.io.clone();
                    let (gid, query) = (guild_id.to_owned(), q.clone());
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(Duration::from_millis(200)).await;
                        let result = join(io.spawn(async move { Ok(api.search_members(&gid, &query, 25).await?) })).await;
                        this.update(cx, |this, cx| {
                            if let Ok(list) = result {
                                this.member_lookup.insert(key, list.members);
                                cx.notify();
                            }
                        })
                        .ok();
                    })
                    .detach();
                }
            }
            None => {}
        }
        found
    }

    /// Replaces a guild's roster and its index.
    fn set_members(&mut self, guild_id: String, members: Vec<Member>) {
        let ix = members.iter().enumerate().map(|(i, m)| (m.user_id.clone(), i)).collect();
        self.member_ix.insert(guild_id.clone(), ix);
        self.members.insert(guild_id, members);
    }

    /// A member of a guild's loaded roster.
    pub fn member(&self, guild_id: &str, user_id: &str) -> Option<&Member> {
        let i = *self.member_ix.get(guild_id)?.get(user_id)?;
        self.members.get(guild_id)?.get(i)
    }

    /// The same person in any loaded roster.
    fn any_member(&self, user_id: &str) -> Option<&Member> {
        self.member_ix.keys().find_map(|g| self.member(g, user_id))
    }

    pub fn load_members(&mut self, guild_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_guild(&guild_id) else { return };
        let io = self.io.clone();
        let gid = guild_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move { Ok(api.members(&gid, 0, 200).await?) })).await;
            this.update(cx, |this, cx| {
                if let Ok(list) = result {
                    this.set_members(guild_id, list.members);
                    cx.notify();
                }
            })
            .ok();
        });
        task.detach();
    }

    /// Who to show a user as in a guild: nickname, display name, then the localpart.
    pub fn display_name(&self, guild_id: Option<&str>, user_id: &str) -> String {
        if let Some(m) = guild_id.and_then(|g| self.member(g, user_id)) {
            return m.shown_name().to_owned();
        }
        if self.user_id() == Some(user_id)
            && let Some(name) = self.home.as_ref().and_then(|h| h.display_name.clone()) {
                return name;
            }
        // Outside a guild (a DM, say): what a friend entry, a fetched profile or any shared
        // guild says, before falling back to the bare localpart.
        let known = self
            .friends
            .iter()
            .find(|f| f.user_id == user_id)
            .and_then(|f| f.display_name.clone())
            .or_else(|| self.profiles.get(user_id).and_then(|p| p.as_ref()).and_then(|p| p.display_name.clone()))
            .or_else(|| self.any_member(user_id).map(|m| m.shown_name().to_owned()));
        known.filter(|n| !n.is_empty()).unwrap_or_else(|| localpart(user_id).to_owned())
    }

    /// A user's picture as seen in a guild: the member record's `avatar_ref` through the guild
    /// server, or for ourselves the home profile through the home server.
    pub fn avatar_url(&self, guild_id: Option<&str>, user_id: &str) -> Option<String> {
        if let Some(gid) = guild_id
            && let Some(member) = self.member(gid, user_id)
            && let Some(reference) = member.avatar_ref.as_deref().filter(|r| !r.is_empty())
            && let Some(entry) = self.guild(gid)
            && let Some(server) = self.server(&entry.server)
            && let Some(doc) = &server.doc
        {
            let origin = crate::media::remote_origin(user_id, &doc.server_name);
            return Some(crate::media::media_url(&doc.api, reference, origin));
        }
        let home = self.home.as_ref()?;
        if home.user_id != user_id {
            return None;
        }
        let reference = home.avatar_ref.as_deref().filter(|r| !r.is_empty())?;
        Some(crate::media::media_url(&home.doc.api, reference, None))
    }

    /// The banner picture of a profile fetched from the home server; a remote user's media is
    /// served through the home server's federation proxy.
    pub fn banner_url(&self, user_id: &str, banner_ref: &str) -> Option<String> {
        let home = self.home.as_ref()?;
        let reference = Some(banner_ref).filter(|r| !r.is_empty())?;
        let origin = crate::media::remote_origin(user_id, &home.doc.server_name);
        Some(crate::media::media_url(&home.doc.api, reference, origin))
    }

    pub fn typing_in(&self, channel_id: &str) -> Vec<String> {
        let now = Instant::now();
        self.typing
            .get(channel_id)
            .map(|list| {
                list.iter()
                    .filter(|(_, at)| now.duration_since(*at) < TYPING_SHOWN_FOR)
                    .map(|(u, _)| u.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    // ---------------------------------------------------------------- actions

    /// Sends the composer's text with every finished staged upload, optionally as a reply.
    pub fn send_message(&mut self, channel_id: String, content: String, reply_to: Option<String>, cx: &mut Context<Self>) {
        let content = content.trim().to_owned();
        if self.is_dm(&channel_id) {
            if self.staged.get(&channel_id).is_some_and(|l| l.iter().any(|s| matches!(s.state, StagedState::Uploading))) {
                self.error = Some("Wait until the files are ready.".into());
                cx.notify();
                return;
            }
            self.send_dm(channel_id, content, reply_to, cx);
            return;
        }
        // Slowmode: the server refuses early sends anyway; saying so here keeps the text.
        let slowmode = self.channel(&channel_id).and_then(|c| c.slowmode).filter(|s| *s > 0);
        if let Some(secs) = slowmode
            && !self.can(&channel_id, crate::perms::MANAGE_MESSAGES)
            && let Some(last) = self.last_sent.get(&channel_id)
        {
            let left = Duration::from_secs(secs).saturating_sub(last.elapsed());
            if !left.is_zero() {
                self.error = Some(format!("Slowmode is on here: you can send again in {} s.", left.as_secs().max(1)).into());
                cx.notify();
                return;
            }
        }
        let staged = self.staged.get(&channel_id);
        if staged.is_some_and(|list| list.iter().any(|s| matches!(s.state, StagedState::Uploading))) {
            self.error = Some("Wait until the uploads have finished.".into());
            cx.notify();
            return;
        }
        self.last_sent.insert(channel_id.clone(), Instant::now());
        let attachments: Vec<Attachment> = self
            .staged
            .remove(&channel_id)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|s| match s.state {
                StagedState::Ready(a) => Some(*a),
                _ => None,
            })
            .collect();
        if content.is_empty() && attachments.is_empty() {
            return;
        }
        let Some(me) = self.user_id().map(str::to_owned) else { return };
        let nonce = format!("rhein-{}-{}", now_ms(), next_nonce());
        let guild_id = self.guild_of_channel(&channel_id).map(|g| g.guild.id.clone());
        let pending = Message {
            id: format!("pending:{nonce}"),
            channel_id: channel_id.clone(),
            guild_id,
            author: me,
            content,
            attachments,
            reply_to,
            created_at: now_ms(),
            nonce: Some(nonce),
            ..Default::default()
        };
        // Show it immediately; the echo or the response replaces it.
        let id = pending.id.clone();
        if let Some(log) = self.logs.get_mut(&channel_id) {
            log.messages.push(pending);
            cx.emit(ClientEvent::MessageAppended { channel_id: channel_id.clone() });
        }
        self.post(channel_id, id, cx);
        cx.notify();
    }

    /// Sends (or re-sends) a pending message already in the log.
    fn post(&mut self, channel_id: String, pending_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_channel(&channel_id) else { return };
        let Some(message) = self.logs.get(&channel_id).and_then(|l| l.messages.iter().find(|m| m.id == pending_id)) else {
            return;
        };
        let body = SendMessage {
            content: message.content.clone(),
            attachments: message.attachments.clone(),
            reply_to: message.reply_to.clone(),
            nonce: message.nonce.clone(),
        };
        let io = self.io.clone();
        let chan = channel_id.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move { Ok(api.send_message(&chan, &body).await?) })).await;
            this.update(cx, |this, cx| {
                let Some(log) = this.logs.get_mut(&channel_id) else { return };
                let Some(i) = log.messages.iter().position(|m| m.id == pending_id) else { return };
                match result {
                    Ok(sent) => {
                        log.failed.remove(&pending_id);
                        if log.messages.iter().any(|m| m.id == sent.id) {
                            log.messages.remove(i);
                            cx.emit(ClientEvent::MessageRemoved { channel_id: channel_id.clone(), index: i });
                        } else {
                            log.messages[i] = sent;
                            cx.emit(ClientEvent::MessageChanged { channel_id: channel_id.clone(), index: i });
                        }
                        this.ack_latest(&channel_id, cx);
                    }
                    Err(err) => {
                        log.failed.insert(pending_id.clone());
                        cx.emit(ClientEvent::MessageChanged { channel_id: channel_id.clone(), index: i });
                        this.error = Some(format!("Couldn't send message: {}", friendly(&err)).into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn retry_failed(&mut self, channel_id: String, pending_id: String, cx: &mut Context<Self>) {
        if let Some(log) = self.logs.get_mut(&channel_id) {
            log.failed.remove(&pending_id);
        }
        self.post(channel_id, pending_id, cx);
        cx.notify();
    }

    pub fn discard_failed(&mut self, channel_id: String, pending_id: String, cx: &mut Context<Self>) {
        let Some(log) = self.logs.get_mut(&channel_id) else { return };
        log.failed.remove(&pending_id);
        if let Some(i) = log.messages.iter().position(|m| m.id == pending_id) {
            log.messages.remove(i);
            cx.emit(ClientEvent::MessageRemoved { channel_id, index: i });
        }
        cx.notify();
    }

    pub fn is_failed(&self, channel_id: &str, message_id: &str) -> bool {
        self.logs.get(channel_id).is_some_and(|l| l.failed.contains(message_id))
    }

    // ---------------------------------------------------------------- staged uploads

    pub fn staged(&self, channel_id: &str) -> &[Staged] {
        self.staged.get(channel_id).map(Vec::as_slice).unwrap_or_default()
    }

    /// Reads a file from disk and uploads it right away; it goes out with the next message.
    pub fn stage_path(&mut self, channel_id: String, path: std::path::PathBuf, cx: &mut Context<Self>) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_owned();
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mime = guess_mime(&name).to_owned();
        let id = self.push_staged(&channel_id, name.clone(), mime.clone(), size, None);
        self.upload_staged(channel_id, id, name, mime, Upload::Path(path), cx);
    }

    /// Uploads bytes (a pasted image); `preview` lets the chip show it before the upload is done.
    pub fn stage_bytes(
        &mut self,
        channel_id: String,
        name: String,
        mime: String,
        bytes: Vec<u8>,
        preview: Option<std::sync::Arc<gpui::Image>>,
        cx: &mut Context<Self>,
    ) {
        let id = self.push_staged(&channel_id, name.clone(), mime.clone(), bytes.len() as u64, preview);
        self.upload_staged(channel_id, id, name, mime, Upload::Bytes(bytes), cx);
    }

    pub fn unstage(&mut self, channel_id: &str, id: u64, cx: &mut Context<Self>) {
        if let Some(list) = self.staged.get_mut(channel_id) {
            list.retain(|s| s.id != id);
        }
        cx.notify();
    }

    fn push_staged(&mut self, channel_id: &str, name: String, mime: String, size: u64, preview: Option<std::sync::Arc<gpui::Image>>) -> u64 {
        let id = next_nonce();
        self.staged.entry(channel_id.to_owned()).or_default().push(Staged {
            id,
            name,
            mime,
            size,
            state: StagedState::Uploading,
            preview,
        });
        id
    }

    fn upload_staged(&mut self, channel_id: String, id: u64, name: String, mime: String, source: Upload, cx: &mut Context<Self>) {
        if self.is_dm(&channel_id) {
            // Never uploaded as is: the bytes wait here and are sealed when the message is sent.
            let io = self.io.clone();
            cx.spawn(async move |this, cx| {
                let result = join(io.spawn(async move {
                    Ok(match source {
                        Upload::Bytes(bytes) => bytes,
                        Upload::Path(path) => tokio::task::spawn_blocking(move || std::fs::read(path)).await??,
                    })
                }))
                .await;
                this.update(cx, |this, cx| {
                    let Some(entry) = this.staged.get_mut(&channel_id).and_then(|l| l.iter_mut().find(|s| s.id == id)) else { return };
                    entry.state = match result {
                        Ok(bytes) => StagedState::Local(std::sync::Arc::new(bytes)),
                        Err(err) => StagedState::Failed(friendly(&err)),
                    };
                    cx.notify();
                })
                .ok();
            })
            .detach();
            return;
        }
        let Some(home) = self.home.as_ref().map(|h| h.api.clone()) else { return };
        let guild_id = self.guild_of_channel(&channel_id).map(|g| g.guild.id.clone()).unwrap_or_default();
        let io = self.io.clone();
        let chan = channel_id.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move {
                let bytes = match source {
                    Upload::Bytes(bytes) => bytes,
                    Upload::Path(path) => tokio::task::spawn_blocking(move || std::fs::read(path)).await??,
                };
                Ok(home.upload_attachment(&name, &mime, bytes, &chan, &guild_id).await?)
            }))
            .await;
            this.update(cx, |this, cx| {
                let Some(entry) = this.staged.get_mut(&channel_id).and_then(|l| l.iter_mut().find(|s| s.id == id)) else {
                    return;
                };
                entry.state = match result {
                    Ok(attachment) => StagedState::Ready(Box::new(attachment)),
                    Err(err) => StagedState::Failed(friendly(&err)),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---------------------------------------------------------------- replies & jumping

    /// The newest message this user wrote in a channel, for "edit last message".
    pub fn last_own_message(&self, channel_id: &str) -> Option<&Message> {
        let me = self.user_id()?;
        self.logs.get(channel_id)?.messages.iter().rev().find(|m| m.author == me && !m.id.starts_with("pending:"))
    }

    pub fn find_message(&self, channel_id: &str, message_id: &str) -> Option<&Message> {
        self.logs.get(channel_id)?.messages.iter().find(|m| m.id == message_id)
    }

    /// Where the "New messages" line goes, if anywhere.
    pub fn divider_index(&self, channel_id: &str) -> Option<usize> {
        let divider = self.dividers.get(channel_id)?;
        let log = self.logs.get(channel_id)?;
        log.messages.iter().position(|m| {
            !m.id.starts_with("pending:")
                && match divider {
                    Divider::After(id) => m.id.as_str() > id.as_str(),
                    Divider::At(id) => m.id.as_str() >= id.as_str(),
                }
        })
    }

    /// Scrolls to a message, paging older history (up to five pages) when it isn't loaded yet.
    pub fn jump_to(&mut self, channel_id: String, message_id: String, cx: &mut Context<Self>) {
        if let Some(index) = self.logs.get(&channel_id).and_then(|l| l.messages.iter().position(|m| m.id == message_id)) {
            cx.emit(ClientEvent::Jump { channel_id, index });
            return;
        }
        let Some(api) = self.api_for_channel(&channel_id) else { return };
        let Some(oldest) = self.logs.get(&channel_id).and_then(|l| l.messages.iter().find(|m| !m.id.starts_with("pending:"))).map(|m| m.id.clone()) else {
            return;
        };
        let io = self.io.clone();
        let chan = channel_id.clone();
        let target = message_id.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move {
                let mut older: Vec<Message> = Vec::new();
                let mut before = oldest;
                for _ in 0..5 {
                    let page = api.messages(&chan, Some(&before), PAGE).await?;
                    let done = page.is_empty() || page.iter().any(|m| m.id == target) || (page.len() as u32) < PAGE;
                    if let Some(first) = page.first() {
                        before = first.id.clone();
                    }
                    older.splice(0..0, page);
                    if done {
                        break;
                    }
                }
                Ok(older)
            }))
            .await;
            this.update(cx, |this, cx| {
                let Ok(older) = result else { return };
                let Some(log) = this.logs.get_mut(&channel_id) else { return };
                let count = older.len();
                let full_history = count < (PAGE as usize);
                log.messages.splice(0..0, older);
                if full_history {
                    log.reached_start = true;
                }
                cx.emit(ClientEvent::MessagesPrepended { channel_id: channel_id.clone(), count });
                match log.messages.iter().position(|m| m.id == message_id) {
                    Some(index) => cx.emit(ClientEvent::Jump { channel_id, index }),
                    None => this.error = Some("The message this replies to is not available any more.".into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---------------------------------------------------------------- embeds

    fn embed_key(&self, channel_id: &str, url: &str) -> Option<String> {
        Some(format!("{}\0{url}", self.server_of_channel(channel_id)?))
    }

    pub fn embed(&self, channel_id: &str, url: &str) -> Option<&EmbedState> {
        self.embeds.get(&self.embed_key(channel_id, url)?)
    }

    /// Asks the channel's guild server to unfurl a link, unless that is already known or pending.
    pub fn request_embed(&mut self, channel_id: &str, url: &str, cx: &mut Context<Self>) {
        let Some(key) = self.embed_key(channel_id, url) else { return };
        match self.embeds.get(&key) {
            Some(EmbedState::Loading | EmbedState::Ready(_)) => return,
            Some(EmbedState::Empty { retry_at }) if Instant::now() < *retry_at => return,
            _ => {}
        }
        let Some(api) = self.api_for_channel(channel_id) else { return };
        self.embeds.insert(key.clone(), EmbedState::Loading);
        let io = self.io.clone();
        let url = url.to_owned();
        cx.spawn(async move |this, cx| {
            let base = api.base().to_owned();
            let asked = url.clone();
            let result = join(io.spawn(async move { Ok(api.embed(&asked).await?) })).await;
            this.update(cx, |this, cx| {
                let state = match &result {
                    Ok(payload) => match crate::embed::read(payload, &base, &url) {
                        Some(embed) => EmbedState::Ready(embed),
                        None => EmbedState::Empty { retry_at: Instant::now() + Duration::from_secs(300) },
                    },
                    // Refused (403) or failed upstream: quiet; a network error is retried sooner.
                    Err(err) => {
                        let wait = match err.downcast_ref::<rhein_net::ApiError>().and_then(|e| e.status()) {
                            Some(_) => 300,
                            None => 15,
                        };
                        EmbedState::Empty { retry_at: Instant::now() + Duration::from_secs(wait) }
                    }
                };
                this.embeds.insert(key, state);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Someone else's activity artwork, safe to show: never the original host (that would tell it
    /// who is looking), always one of our own servers. A picture already on one of them (a cover
    /// uploaded from someone's machine) is loaded as it is, once it is known to be one of a
    /// bounded size; anything else is a copy re-hosted by a camo cache, as the web client does.
    /// `None` until that is known. Called on every frame for every activity shown, so all of it is
    /// worked out once, in the background, and kept.
    pub fn artwork_url(&mut self, url: &str, cx: &mut Context<Self>) -> Option<String> {
        match self.artwork.get(url) {
            Some(EmbedState::Ready(embed)) => return embed.image.clone(),
            Some(EmbedState::Loading) => return None,
            Some(EmbedState::Empty { retry_at }) if Instant::now() < *retry_at => return None,
            _ => {}
        }
        let api = self.ready_servers().find_map(|s| s.api.clone())?;
        let own = self.ready_servers().filter_map(|s| s.doc.as_ref()).any(|d| crate::embed::is_own_media_file(url, &d.api));
        self.artwork.insert(url.to_owned(), EmbedState::Loading);
        let io = self.io.clone();
        let url = url.to_owned();
        cx.spawn(async move |this, cx| {
            let base = api.base().to_owned();
            let asked = url.clone();
            // `None`: the picture as it is; else the camo cache's answer.
            let result = join(io.spawn(async move {
                if own && small_picture(api.http(), &asked).await {
                    return Ok(None);
                }
                Ok(Some(api.embed(&asked).await?))
            }))
            .await;
            this.update(cx, |this, cx| {
                let state = match result {
                    Ok(None) => EmbedState::Ready(crate::embed::Embed {
                        url: url.clone(),
                        site: String::new(),
                        kind: crate::embed::EmbedKind::Image,
                        title: None,
                        description: None,
                        image: Some(url.clone()),
                        video: None,
                        width: None,
                        height: None,
                    }),
                    Ok(Some(payload)) => match crate::embed::read(&payload, &base, &url).filter(|e| e.image.is_some()) {
                        Some(embed) => EmbedState::Ready(embed),
                        None => EmbedState::Empty { retry_at: Instant::now() + Duration::from_secs(300) },
                    },
                    Err(_) => EmbedState::Empty { retry_at: Instant::now() + Duration::from_secs(300) },
                };
                this.artwork.insert(url, state);
                cx.notify();
            })
            .ok();
        })
        .detach();
        None
    }

    /// A custom emoji's picture, from the server that owns it when we are connected there,
    /// otherwise through the guild's server.
    pub fn emoji_url(&self, guild_id: Option<&str>, emoji_id: &str, origin: Option<&str>) -> Option<String> {
        let base = origin
            .and_then(|o| self.server(o))
            .and_then(|s| s.doc.as_ref())
            .map(|d| d.api.clone())
            .or_else(|| {
                let server = self.guild(guild_id?)?.server.clone();
                self.server(&server)?.doc.as_ref().map(|d| d.api.clone())
            })?;
        Some(format!("{base}/_telium/v1/emoji/{emoji_id}"))
    }

    pub fn edit_message(&mut self, channel_id: String, message_id: String, content: String, cx: &mut Context<Self>) {
        if self.is_dm(&channel_id) {
            return self.dm_payload(channel_id, rhein_mls::payload::Payload::Edit { target: message_id, text: content }, cx);
        }
        let Some(api) = self.api_for_channel(&channel_id) else { return };
        self.fire(cx, async move { api.edit_message(&channel_id, &message_id, &content).await.map(drop) });
    }

    pub fn delete_message(&mut self, channel_id: String, message_id: String, cx: &mut Context<Self>) {
        if self.is_dm(&channel_id) {
            return self.dm_payload(channel_id, rhein_mls::payload::Payload::Delete { target: message_id }, cx);
        }
        let Some(api) = self.api_for_channel(&channel_id) else { return };
        self.fire(cx, async move { api.delete_message(&channel_id, &message_id).await.map(drop) });
    }

    pub fn toggle_reaction(&mut self, channel_id: String, message_id: String, emoji: String, cx: &mut Context<Self>) {
        let mine = self
            .logs
            .get(&channel_id)
            .and_then(|l| l.messages.iter().find(|m| m.id == message_id))
            .and_then(|m| m.reactions.iter().find(|r| r.emoji == emoji))
            .is_some_and(|r| r.me);
        if self.is_dm(&channel_id) {
            return self.dm_payload(channel_id, rhein_mls::payload::Payload::Reaction { target: message_id, emoji, add: !mine }, cx);
        }
        let Some(api) = self.api_for_channel(&channel_id) else { return };
        let mine = self
            .logs
            .get(&channel_id)
            .and_then(|l| l.messages.iter().find(|m| m.id == message_id))
            .and_then(|m| m.reactions.iter().find(|r| r.emoji == emoji))
            .is_some_and(|r| r.me);
        self.fire(cx, async move { api.react(&message_id, &emoji, !mine).await.map(drop) });
    }

    /// Says we stopped typing where we said we were typing (a message sent, the draft cleared).
    pub fn stop_typing(&mut self, channel_id: &str) {
        if !self.typing_sent.remove(channel_id) {
            return;
        }
        log::debug!("typing stopped in {channel_id}");
        if self.is_dm(channel_id) {
            let home = self.home.as_ref().map(|h| h.doc.server_name.clone());
            if let Some(gw) = home.and_then(|h| self.server(&h)).and_then(|s| s.gateway.as_ref()) {
                gw.send(rhein_proto::dm_typing(channel_id, false));
            }
            return;
        }
        let Some(server) = self.server_of_channel(channel_id) else { return };
        if let Some(gw) = self.server(server).and_then(|s| s.gateway.as_ref()) {
            gw.send(rhein_proto::typing(channel_id, false));
        }
    }

    pub fn send_typing(&mut self, channel_id: &str) {
        log::debug!("typing in {channel_id}");
        if self.is_dm(channel_id) {
            if self.is_request(channel_id) || !self.signal("typing", Some(channel_id), true) {
                return;
            }
            self.typing_sent.insert(channel_id.to_owned());
            // Through the home server's gateway, which relays it to the conversation.
            let home = self.home.as_ref().map(|h| h.doc.server_name.clone());
            if let Some(gw) = home.and_then(|h| self.server(&h)).and_then(|s| s.gateway.as_ref()) {
                gw.send(rhein_proto::dm_typing(channel_id, true));
            }
            return;
        }
        let Some(server) = self.server_of_channel(channel_id) else { return };
        if let Some(gw) = self.server(server).and_then(|s| s.gateway.as_ref()) {
            gw.send(rhein_proto::typing(channel_id, true));
        }
        self.typing_sent.insert(channel_id.to_owned());
    }

    pub fn create_guild(&mut self, server: String, name: String, cx: &mut Context<Self>) {
        let Some(api) = self.server(&server).and_then(|s| s.api.clone()) else { return };
        let io = self.io.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move { Ok(api.create_guild(name.trim()).await?) })).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(guild) => {
                        this.selected_guild = Some(guild.id);
                        this.refresh_server(&server, cx);
                    }
                    Err(err) => this.error = Some(format!("Couldn't create guild: {}", friendly(&err)).into()),
                }
                cx.notify();
            })
            .ok();
        });
        task.detach();
    }

    /// Accepts `CODE` (on `default_server`), `server/CODE` or an invite URL
    /// (`https://server/invite/CODE`). Connects to the server first if needed.
    pub fn join_invite(&mut self, text: String, default_server: Option<String>, cx: &mut Context<Self>) {
        let Some((server_input, code)) = parse_invite(&text, default_server.as_deref()) else {
            self.error = Some("That doesn't look like an invite".into());
            cx.notify();
            return;
        };
        let known = self
            .servers
            .iter()
            .find(|s| s.name == server_input || s.input == server_input)
            .map(|s| s.name.clone());
        let name = match known {
            Some(name) => name,
            None => {
                self.add_server(server_input.clone(), cx);
                server_input.clone()
            }
        };
        let task = cx.spawn(async move |this, cx| {
            // Wait (bounded) for the server's session if we just added it.
            let mut api = None;
            for _ in 0..100 {
                api = this
                    .read_with(cx, |this, _| {
                        this.servers
                            .iter()
                            .find(|s| s.name == name || s.input == name)
                            .and_then(|s| s.api.clone().map(|a| (s.name.clone(), a)))
                    })
                    .ok()
                    .flatten();
                if api.is_some() {
                    break;
                }
                cx.background_executor().timer(Duration::from_millis(100)).await;
            }
            let Some((server, api)) = api else {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("Can't reach {name}").into());
                    cx.notify();
                })
                .ok();
                return;
            };
            let io = this.read_with(cx, |this, _| this.io.clone()).ok();
            let Some(io) = io else { return };
            let host = server.clone();
            let result = join(io.spawn(async move {
                match api.join_invite(&code).await {
                    // Not an invite: a vanity name. Generated codes start with `~` and are never one.
                    Err(err) if err.status() == Some(404) && !code.starts_with('~') => {
                        let answer = api.resolve_guild_address(&format!("#{code}:{host}")).await?;
                        let guild = answer["guild_id"].as_str().unwrap_or_default().to_owned();
                        let lands = answer["host"].as_str().unwrap_or(host.as_str());
                        if guild.is_empty() {
                            return Err(anyhow!("No guild answers to #{code}:{host}"));
                        }
                        if lands != host {
                            return Err(anyhow!("That guild moved: join it as #{code}:{lands}"));
                        }
                        Ok(api.join_guild(&guild).await.map(|mut joined| {
                            if joined.get("id").is_none() {
                                joined["id"] = serde_json::Value::String(guild);
                            }
                            joined
                        })?)
                    }
                    other => Ok(other?),
                }
            }))
            .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(answer) => {
                        if answer.get("pending").and_then(|v| v.as_bool()) == Some(true) {
                            this.notice = Some("Join requested – a moderator has to approve it.".into());
                        } else if let Some(id) = answer.get("id").and_then(|v| v.as_str()) {
                            this.selected_guild = Some(id.to_owned());
                        }
                        this.refresh_server(&server, cx);
                    }
                    Err(err) => this.error = Some(format!("Couldn't join: {}", friendly(&err)).into()),
                }
                cx.notify();
            })
            .ok();
        });
        task.detach();
    }

    /// Creates an invite for the guild and puts a shareable form of it in `notice`.
    pub fn create_invite(&mut self, guild_id: String, cx: &mut Context<Self>) {
        let Some(api) = self.api_for_guild(&guild_id) else { return };
        let server = self.guild(&guild_id).map(|g| g.server.clone()).unwrap_or_default();
        let io = self.io.clone();
        let channel = self.selected_channel.get(&guild_id).cloned();
        let task = cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move { Ok(api.create_invite(&guild_id, channel.as_deref()).await?) })).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(invite) => {
                        let code = invite.get("code").and_then(|v| v.as_str()).unwrap_or_default();
                        let text = format!("{server}/{code}");
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
                        this.notice = Some(format!("Invite copied: {text}").into());
                    }
                    Err(err) => this.error = Some(format!("Couldn't create invite: {}", friendly(&err)).into()),
                }
                cx.notify();
            })
            .ok();
        });
        task.detach();
    }

    /// A notice that goes away by itself after a while, unless another has taken its place.
    pub fn flash_notice(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        let text: SharedString = text.into();
        self.notice = Some(text.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE_SHOWN_FOR).await;
            this.update(cx, |this, cx| {
                if this.notice.as_ref() == Some(&text) {
                    this.notice = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.notice = None;
        self.warning = None;
        cx.notify();
    }

    /// Runs a request whose result arrives over the gateway anyway; only failures are reported.
    fn fire<F>(&mut self, cx: &mut Context<Self>, fut: F)
    where
        F: Future<Output = Result<(), rhein_net::ApiError>> + Send + 'static,
    {
        let io = self.io.clone();
        let task = cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let result = io.spawn(fut).await;
            let failed = match result {
                Ok(Ok(())) => None,
                Ok(Err(err)) => Some(err.to_string()),
                Err(err) => Some(err.to_string()),
            };
            if let Some(message) = failed {
                this.update(cx, |this, cx| {
                    this.error = Some(message.into());
                    cx.notify();
                })
                .ok();
            }
        });
        task.detach();
    }
}

fn apply_reaction(
    reactions: &mut Vec<rhein_proto::Reaction>,
    emoji: &str,
    user_id: &str,
    add: bool,
    me: Option<&str>,
) {
    let mine = me == Some(user_id);
    match reactions.iter().position(|r| r.emoji == emoji) {
        Some(i) => {
            let r = &mut reactions[i];
            if add {
                if !r.users.iter().any(|u| u == user_id) {
                    r.users.push(user_id.to_owned());
                    r.count += 1;
                }
                if mine {
                    r.me = true;
                }
            } else {
                if let Some(p) = r.users.iter().position(|u| u == user_id) {
                    r.users.remove(p);
                }
                r.count = (r.count - 1).max(0);
                if mine {
                    r.me = false;
                }
                if r.count == 0 {
                    reactions.remove(i);
                }
            }
        }
        None if add => reactions.push(rhein_proto::Reaction {
            emoji: emoji.to_owned(),
            count: 1,
            me: mine,
            users: vec![user_id.to_owned()],
        }),
        None => {}
    }
}

/// Mirrors the server's mention rule: a literal `@user:server`, or `@everyone` / `@here`.
enum Upload {
    Bytes(Vec<u8>),
    Path(std::path::PathBuf),
}

pub fn parse_invite(text: &str, default_server: Option<&str>) -> Option<(String, String)> {
    let text = text.trim().trim_end_matches('/');
    if text.is_empty() {
        return None;
    }
    // A guild address, `#name:host`: the same as `https://host/invite/name`.
    if let Some((name, host)) = text.strip_prefix('#').and_then(|a| a.split_once(':')) {
        return (!name.is_empty() && !host.is_empty()).then(|| (host.to_owned(), name.to_ascii_lowercase()));
    }
    let bare = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"))
        .unwrap_or(text);
    if let Some((server, code)) = bare.split_once("/invite/") {
        return Some((server.to_owned(), code.to_owned()));
    }
    if let Some((server, code)) = bare.rsplit_once('/') {
        return Some((server.to_owned(), code.to_owned()));
    }
    Some((default_server?.to_owned(), bare.to_owned()))
}

fn guess_mime(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" | "opus" => "audio/ogg",
        "pdf" => "application/pdf",
        "txt" | "md" | "log" => "text/plain",
        "json" => "application/json",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

fn device_name() -> String {
    let host = std::fs::read_to_string("/etc/hostname")
        .map(|h| h.trim().to_owned())
        .unwrap_or_default();
    if host.is_empty() { "Rhein".into() } else { format!("Rhein on {host}") }
}

fn next_nonce() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// A one-line explanation for the UI, preferring the server's own message.
/// Artwork loaded as it is from one of our servers is at most this large; a larger one comes
/// through the camo cache, which bounds what is fetched and decoded.
const ARTWORK_MAX: u64 = 4 << 20;

/// Whether `url` is a picture of at most `ARTWORK_MAX` bytes, by what the server says of it
/// (without fetching it). Not knowing counts as no.
async fn small_picture(http: &reqwest::Client, url: &str) -> bool {
    let Ok(response) = http.head(url).send().await else { return false };
    let picture = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("image/"));
    response.status().is_success() && picture && response.content_length().is_some_and(|n| n <= ARTWORK_MAX)
}

fn friendly(err: &anyhow::Error) -> String {
    if let Some(api) = err.downcast_ref::<rhein_net::ApiError>() {
        let text = |code| api.is(code);
        return if text("invalid_credentials") {
            "Wrong username or password".into()
        } else if text("username_taken") {
            "That username is taken".into()
        } else if text("registration_closed") {
            "Registration is closed on this server".into()
        } else if text("account_deactivated") {
            "This account is deactivated".into()
        } else if text("standby_read_only") {
            "This server is running from a read-only copy right now; nothing can be changed until it is back.".into()
        } else if text("demoted") {
            "This server has handed over to another one and no longer accepts changes.".into()
        } else if text("rate_limited") {
            "Too many requests; try again in a moment.".into()
        } else if text("incompatible_protocol") {
            "This server speaks a protocol version Rhein does not; one of them needs an update.".into()
        } else if api.status().is_some_and(|s| (502..=504).contains(&s)) {
            "A server on the way did not answer; try again later.".into()
        } else {
            api.to_string()
        };
    }
    format!("{err:#}")
}

/// Our own profile as last fetched, kept beside the session so settings show it at once.
fn own_profile_path(user_id: &str) -> std::path::PathBuf {
    let safe: String = user_id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    crate::persist::path().with_file_name(format!("profile-{safe}.json"))
}

fn own_profile_load(user_id: &str) -> Option<rhein_proto::Profile> {
    serde_json::from_str(&std::fs::read_to_string(own_profile_path(user_id)).ok()?).ok()
}

fn own_profile_save(user_id: &str, profile: &rhein_proto::Profile) {
    if let Ok(text) = serde_json::to_string(profile) {
        let _ = std::fs::write(own_profile_path(user_id), text);
    }
}

/// The role ids in a READY `me` object.
fn roles_of(me: &serde_json::Value) -> Vec<String> {
    me.get("roles")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invites() {
        assert_eq!(parse_invite("abc", Some("a.x")), Some(("a.x".into(), "abc".into())));
        assert_eq!(parse_invite("b.x/abc", None), Some(("b.x".into(), "abc".into())));
        assert_eq!(
            parse_invite("https://b.x/invite/abc", None),
            Some(("b.x".into(), "abc".into()))
        );
        assert_eq!(parse_invite("abc", None), None);
        let modal = BotModal::parse(&serde_json::json!({
            "id": "i1", "channel_id": "c",
            "data": { "custom_id": "report", "title": "Report this", "components": [{
                "type": "action_row",
                "components": [{ "type": "text_input", "custom_id": "why", "label": "Why?", "style": "paragraph", "required": true, "max": 500 }]
            }]}
        }))
        .unwrap();
        assert_eq!((modal.interaction_id.as_str(), modal.title.as_str(), modal.fields.len()), ("i1", "Report this", 1));
        assert!(modal.fields[0].required && modal.fields[0].max == Some(500));
        assert_eq!(parse_invite("#Gaming:guilds.example", None), Some(("guilds.example".into(), "gaming".into())));
        assert_eq!(parse_invite("#:x", None), None);
    }

    #[test]
    fn reactions() {
        let mut r = Vec::new();
        apply_reaction(&mut r, "👍", "@a:x", true, Some("@a:x"));
        apply_reaction(&mut r, "👍", "@b:x", true, Some("@a:x"));
        assert_eq!(r[0].count, 2);
        assert!(r[0].me);
        apply_reaction(&mut r, "👍", "@a:x", false, Some("@a:x"));
        assert_eq!(r[0].count, 1);
        assert!(!r[0].me);
        apply_reaction(&mut r, "👍", "@b:x", false, Some("@a:x"));
        assert!(r.is_empty());
    }
}
