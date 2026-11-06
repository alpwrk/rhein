//! The signed-in window: sidebar tree with your profile | chat over a status line | side panel,
//! plus its modals.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Instant;

use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable as _, FontWeight, IntoElement, KeyBinding, MouseButton, ParentElement as _, Pixels, Point, Render,
    SharedString, Styled as _, Subscription, Window, actions, anchored, deferred, div, prelude::*, px,
};
use rhein_store::{CallMode, CallStatus, Client, ServerStatus, perms};
use rhein_ui::theme::{FONT_MONO, radius, size, text};
use rhein_ui::{
    ActiveTheme as _, motion, Avatar, Button, CountBadge, Icon, IconButton, InputEvent, ListRow, MenuEntry, MenuHost, Modal,
    Presence, Tabs, Tag, TextInput, Toast, ToastKind, UnreadDot, icon, icon_sized, section_label, tooltip,
};

use crate::views::chat::{ChatEvent, ChatView, PanelKind};
use crate::views::home::HomeView;
use crate::views::part::{Clock, Part, part};
use crate::views::{activity, profile};
use crate::views::panels::{PanelEvent, PinsPanel, SearchPanel, ThreadsPanel};
use crate::views::guild_settings::{GUILD_SETTINGS_CONTEXT, GuildSettingsEvent, GuildSettingsView, Pane as GuildPane};
use crate::views::settings::{SETTINGS_CONTEXT, SettingsEvent, SettingsView};
use crate::views::status::StatusLine;
use crate::views::switcher::QuickSwitcher;

actions!(rhein_shell, [OpenSwitcher, OpenSettings, PrevChannel, NextChannel, PrevUnread, NextUnread]);

const SHELL_CONTEXT: &str = "Shell";

/// Who you are, at the foot of the sidebar; an activity you share gets a box as tall below it.
const PROFILE_ROW_H: Pixels = px(54.);

pub fn bindings() -> Vec<KeyBinding> {
    let ctx = Some(SHELL_CONTEXT);
    let mut out = vec![
        KeyBinding::new("alt-up", PrevChannel, ctx),
        KeyBinding::new("alt-down", NextChannel, ctx),
        KeyBinding::new("alt-shift-up", PrevUnread, ctx),
        KeyBinding::new("alt-shift-down", NextUnread, ctx),
        KeyBinding::new("escape", rhein_ui::Dismiss, Some(SETTINGS_CONTEXT)),
        KeyBinding::new("escape", rhein_ui::Dismiss, Some(GUILD_SETTINGS_CONTEXT)),
    ];
    for [k, comma] in [["ctrl-k", "ctrl-,"], ["cmd-k", "cmd-,"]] {
        out.push(KeyBinding::new(k, OpenSwitcher, ctx));
        out.push(KeyBinding::new(comma, OpenSettings, ctx));
    }
    out
}

type Action = Rc<dyn Fn(&mut App)>;
type PromptAction = Rc<dyn Fn(Vec<String>, &mut App)>;

/// The shell's own dialogs (confirmations and small forms).
enum ShellModal {
    Confirm { title: String, body: String, label: String, danger: bool, action: Action },
    Prompt { title: String, fields: Vec<(String, Entity<TextInput>)>, label: String, action: PromptAction },
    CreateChannel { guild_id: String, name: Entity<TextInput>, kind: usize, category: Option<String> },
}

const CHANNEL_KINDS: [(&str, &str); 3] = [("text", "Text"), ("voice", "Voice"), ("announcement", "Announcement")];

#[derive(Clone, Copy, PartialEq, Eq)]
enum DialogTab {
    Join,
    Create,
    Server,
}

impl DialogTab {
    const ALL: [DialogTab; 3] = [DialogTab::Join, DialogTab::Create, DialogTab::Server];

    fn label(self) -> &'static str {
        match self {
            DialogTab::Join => "Join",
            DialogTab::Create => "Create",
            DialogTab::Server => "Add server",
        }
    }
}

/// What occupies the right-hand slot. Members is the default and stays remembered via
/// `show_members`; the others replace it while open.
enum Panel {
    None,
    Pins { channel: String, view: Entity<PinsPanel> },
    Threads { channel: String, view: Entity<ThreadsPanel> },
    Search { guild: String, view: Entity<SearchPanel> },
    Thread { view: Entity<ChatView> },
}

pub struct Shell {
    client: Entity<Client>,
    /// The sidebar tree's and the member list's scrolling, for smooth wheel steps.
    tree_scroll: gpui::ScrollHandle,
    members_list: gpui::ListState,
    /// The member list's rows as last laid out (see `members`).
    member_rows: Rc<RefCell<Vec<MemberRow>>>,
    /// The sidebar and the member list, each drawn as a cached view of its own (see `part`):
    /// made at the first render, which has the shell's own entity to hand them.
    parts: Option<ShellParts>,
    /// Ticks once a second while a profile card is open, for its activity clocks: redrawing the
    /// card, not the whole shell.
    card_clock: Entity<Clock>,
    chat: Entity<ChatView>,
    home: Entity<HomeView>,
    dialog: Option<DialogTab>,
    dialog_input: Entity<TextInput>,
    create_on: Option<String>,
    show_members: bool,
    panel: Panel,
    panel_subs: Vec<Subscription>,
    /// Categories the user folded away.
    collapsed: HashSet<String>,
    /// Guilds folded or unfolded by hand in the sidebar tree.
    folded_guilds: HashSet<String>,
    unfolded_guilds: HashSet<String>,
    /// The guild open when the client last changed, to keep it unfolded once another opens.
    open_guild: Option<String>,
    /// The sign-out question, with how many other saved accounts stay signed in (read once when
    /// it opens, not from disk on every frame).
    confirm_sign_out: Option<usize>,
    modal_focus: FocusHandle,
    menu: MenuHost,
    modal: Option<ShellModal>,
    settings: Option<(Entity<SettingsView>, Subscription)>,
    guild_settings: Option<(Entity<GuildSettingsView>, Subscription)>,
    /// Whose profile card a press outside it just closed, and when.
    card_closed: Option<(String, std::time::Instant)>,
    /// The whole profile over the window: whose, from which guild, which tab.
    full_profile: Option<(String, Option<String>, usize)>,
    switcher: Option<(Entity<QuickSwitcher>, Subscription)>,
    profile_card: Option<(String, Option<String>, Point<Pixels>)>,
    card_ticker: Option<gpui::Task<()>>,
    /// When this window last had input from you (auto-idle, `IDLE_AFTER`).
    last_input: Rc<Cell<Instant>>,
    _idle_watch: gpui::Task<()>,
    _subs: Vec<Subscription>,
}

struct ShellParts {
    sidebar: Entity<Part<Shell>>,
    members: Entity<Part<Shell>>,
}

/// How long without input to this window before you show as idle.
const IDLE_AFTER: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Input to the window: note the time, and wake from auto-idle.
fn note_input(last: &Cell<Instant>, client: &Entity<Client>, cx: &mut App) {
    last.set(Instant::now());
    if client.read(cx).is_auto_idle() {
        client.update(cx, |c, cx| c.set_auto_idle(false, cx));
    }
}

impl Shell {
    pub fn new(client: Entity<Client>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let chat = cx.new(|cx| ChatView::new(client.clone(), window, cx));
        let dialog_input = cx.new(|cx| TextInput::new("", cx));
        let home = cx.new(|cx| HomeView::new(client.clone(), window, cx));
        let last_input = Rc::new(Cell::new(Instant::now()));
        // RHEIN_IDLE_SECS shortens the wait in development builds, to watch it happen.
        let idle_after = std::env::var("RHEIN_IDLE_SECS")
            .ok()
            .filter(|_| cfg!(debug_assertions))
            .and_then(|s| s.parse().ok())
            .map_or(IDLE_AFTER, std::time::Duration::from_secs);
        let idle_watch = {
            let (last, client) = (last_input.clone(), client.clone());
            cx.spawn(async move |_, cx| {
                loop {
                    cx.background_executor().timer((idle_after / 20).min(std::time::Duration::from_secs(30))).await;
                    if last.get().elapsed() >= idle_after {
                        client.update(cx, |c, cx| c.set_auto_idle(true, cx));
                    }
                }
            })
        };
        let active = window.is_window_active();
        client.update(cx, |c, cx| c.set_window_active(active, cx));
        let subs = vec![
            // Alerts come only while the window is in the background.
            cx.observe_window_activation(window, |this, window, cx| {
                let active = window.is_window_active();
                this.client.update(cx, |c, cx| c.set_window_active(active, cx));
            }),
            {
                let (last, client) = (last_input.clone(), client.clone());
                cx.observe_keystrokes(move |_, _, _, cx| note_input(&last, &client, cx))
            },
            cx.observe_in(&client, window, |this, _, window, cx| {
                this.keep_previous_guild_unfolded(cx);
                this.follow_channel(window, cx);
                if let Some(modal) = this.client.update(cx, |c, _| c.modal.take()) {
                    this.open_bot_modal(modal, window, cx);
                }
                cx.notify();
            }),
            cx.subscribe_in(&chat, window, |this, _, event: &ChatEvent, window, cx| match event {
                ChatEvent::TogglePanel(kind) => this.toggle_panel(*kind, window, cx),
                ChatEvent::OpenThread(id) => this.open_thread(id.clone(), window, cx),
                ChatEvent::ClosePanel => this.close_panel(cx),
                ChatEvent::Profile { user_id, at } => this.show_profile(user_id.clone(), *at, cx),
                ChatEvent::MemberMenu { user_id, at } => this.member_menu(user_id.clone(), *at, window, cx),
            }),
            cx.subscribe_in(&dialog_input, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Submit) {
                    this.confirm_dialog(window, cx);
                }
            }),
        ];
        let focus = chat.read(cx).composer_focus(cx);
        window.focus(&focus, cx);
        // Debug builds can start on a given screen, for screenshots without input:
        // RHEIN_DEV_OPEN=home|settings|settings=Section|join|switcher|signout|account|switch=@user|profile=@user|voice.
        // RHEIN_DEV_UNLOCK=<password> unlocks encrypted DMs of a restored session at start.
        #[cfg(debug_assertions)]
        if let Ok(password) = std::env::var("RHEIN_DEV_UNLOCK") {
            let client = client.clone();
            cx.defer(move |cx| client.update(cx, |c, cx| c.unlock_dm(password, cx)));
        }
        #[cfg(debug_assertions)]
        if let Ok(what) = std::env::var("RHEIN_DEV_OPEN") {
            cx.defer_in(window, move |this: &mut Self, window, cx| match what.as_str() {
                // After READY, which selects a guild of its own.
                "home" => {
                    let client = this.client.clone();
                    cx.spawn(async move |_, cx| {
                        cx.background_executor().timer(std::time::Duration::from_secs(3)).await;
                        client.update(cx, |c, cx| c.show_home(cx));
                    })
                    .detach();
                }
                "settings" => this.open_settings(window, cx),
                // RHEIN_DEV_OPEN=guild-settings[=pane]: the selected guild's settings, once READY is in.
                w if w.starts_with("guild-settings") || w.starts_with("guild-icon=") => {
                    let what = w.to_owned();
                    cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor().timer(std::time::Duration::from_secs(3)).await;
                        this.update_in(cx, |this, window, cx| {
                            let Some(gid) = this.client.read(cx).selected_guild.clone() else { return };
                            let pane = what.strip_prefix("guild-settings=").and_then(GuildPane::from_key).unwrap_or(GuildPane::Overview);
                            this.open_guild_settings(gid, pane, window, cx);
                            if let Some(path) = what.strip_prefix("guild-icon=")
                                && let Some((view, _)) = &this.guild_settings
                            {
                                view.update(cx, |v, cx| v.dev_icon(path.into(), cx));
                            }
                        })
                        .ok();
                    })
                    .detach();
                }
                "join" => this.open_dialog(DialogTab::Join, window, cx),
                "switcher" => this.open_switcher(window, cx),
                // cycle[=n]: once READY is in, opens the first n text channels (6) one after the
                // other, twice round, three seconds each, then quits; for timing switches
                // (RUST_LOG=rhein::switch=debug). cycle-dm[=n] does the same with conversations
                // (an account seeded with `rhein-store`'s `dm_bench_seed` example).
                w if w.starts_with("cycle") => {
                    let dms = w.starts_with("cycle-dm");
                    let n = w.split_once('=').and_then(|(_, n)| n.parse().ok()).unwrap_or(6usize);
                    cx.spawn(async move |this, cx| {
                        let mut channels = Vec::new();
                        for _ in 0..60 {
                            cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                            channels = this
                                .update(cx, |v, cx| {
                                    let client = v.client.read(cx);
                                    if dms {
                                        client.conversations.iter().map(|c| c.id.clone()).take(n).collect::<Vec<_>>()
                                    } else {
                                        client.guilds.iter().flat_map(|g| g.ordered_channels().into_iter().filter(|c| c.is_textual()).map(|c| c.id.clone())).take(n).collect::<Vec<_>>()
                                    }
                                })
                                .unwrap_or_default();
                            if !channels.is_empty() {
                                break;
                            }
                        }
                        cx.background_executor().timer(std::time::Duration::from_secs(5)).await;
                        for round in 0..2 {
                            for id in &channels {
                                log::debug!(target: "rhein::switch", "cycle round {round}: opening {id}");
                                this.update(cx, |v, cx| {
                                    v.client.update(cx, |c, cx| match dms {
                                        true => c.open_conversation(Some(id.clone()), cx),
                                        false => c.select_channel(id.clone(), cx),
                                    })
                                })
                                .ok();
                                cx.background_executor().timer(std::time::Duration::from_secs(3)).await;
                            }
                        }
                        cx.update(|cx| cx.quit());
                    })
                    .detach();
                }
                "signout" => this.ask_sign_out(window, cx),
                // Someone's profile card over the chat, once READY is in.
                "card" => {
                    cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor().timer(std::time::Duration::from_secs(4)).await;
                        this.update_in(cx, |v, _, cx| {
                            let other = v.client.read(cx).selected_guild.clone().and_then(|g| {
                                let c = v.client.read(cx);
                                let me = c.user_id().map(str::to_owned);
                                c.members.get(&g).and_then(|l| l.iter().find(|m| Some(&m.user_id) != me.as_ref()).map(|m| m.user_id.clone()))
                            });
                            if let Some(user) = other {
                                v.show_profile(user, gpui::point(px(420.), px(200.)), cx);
                            }
                        })
                        .ok();
                    })
                    .detach();
                }
                // The account menu, once the profile is in.
                "account" => {
                    cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor().timer(std::time::Duration::from_secs(3)).await;
                        this.update_in(cx, |v, window, cx| v.account_menu(window, cx)).ok();
                    })
                    .detach();
                }
                // profile=@user:server opens that card once members are in; settings=Activity
                // opens a settings section.
                // Joins the selected guild's first voice channel (run with RHEIN_AUDIO=null).
                "voice" => {
                    cx.spawn(async move |this, cx| {
                        // Once READY has brought the guild and its channels.
                        for _ in 0..40 {
                            cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                            let joined = this
                                .update(cx, |v, cx| {
                                    let client = v.client.read(cx);
                                    let guild = client.selected_guild.clone().and_then(|g| client.guild(&g).map(|g| g.guild.clone()));
                                    let voice = guild.and_then(|g| g.channels.iter().find(|c| c.is_voice()).map(|c| c.id.clone()));
                                    let Some(id) = voice else { return false };
                                    v.client.update(cx, |c, cx| {
                                        c.select_channel(id.clone(), cx);
                                        c.join_voice(id, cx);
                                    });
                                    true
                                })
                                .unwrap_or(true);
                            if joined {
                                break;
                            }
                        }
                        // RHEIN_DEV_SHARE=1: then presses the screen share button, once connected.
                        // (=pattern shares a test pattern instead of asking the desktop.)
                        if let Ok(how) = std::env::var("RHEIN_DEV_SHARE") {
                            for _ in 0..40 {
                                let up = this
                                    .update(cx, |v, cx| v.client.read(cx).call().is_some_and(|c| c.status == CallStatus::Connected))
                                    .unwrap_or(true);
                                if up {
                                    break;
                                }
                                cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                            }
                            this.update(cx, |v, cx| {
                                v.client.update(cx, |c, cx| if how == "pattern" { c.share_test_pattern(cx) } else { c.start_screen_share(cx) })
                            })
                            .ok();
                        }
                        // Then watches whoever shares a screen.
                        cx.background_executor().timer(std::time::Duration::from_secs(4)).await;
                        this.update(cx, |v, cx| {
                            v.client.update(cx, |c, cx| {
                                let Some(call) = c.call() else { return };
                                let (guild, channel) = (call.room_id.clone(), call.channel_id.clone());
                                let streaming: Vec<String> = c.voice_members(&guild, &channel).into_iter().filter(|s| s.streaming).map(|s| s.user_id).collect();
                                for user in streaming {
                                    c.watch_screen(&user, true, cx);
                                }
                            });
                        })
                        .ok();
                    })
                    .detach();
                }
                // Favourites the demo's GIF once the keys are open, then opens the GIF menu
                // (with RHEIN_DEV_UNLOCK for a restored session).
                "gifs" => {
                    cx.spawn_in(window, async move |this, cx| {
                        for _ in 0..60 {
                            cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                            let done = this
                                .update_in(cx, |v, window, cx| {
                                    if !v.client.read(cx).gif_favorites_ready() {
                                        return false;
                                    }
                                    let gif = "https://upload.wikimedia.org/wikipedia/commons/2/2c/Rotating_earth_%28large%29.gif";
                                    if !v.client.read(cx).is_gif_favorite(gif) {
                                        v.client.update(cx, |c, cx| c.toggle_gif_favorite(gif, cx));
                                    }
                                    let at = gpui::point(window.viewport_size().width - px(120.), window.viewport_size().height - px(70.));
                                    v.chat.update(cx, |chat, cx| chat.open_gif_picker(at, window, cx));
                                    true
                                })
                                .unwrap_or(true);
                            if done {
                                break;
                            }
                        }
                    })
                    .detach();
                }
                // Plays the first sound file in the open channel (run with RHEIN_AUDIO=null).
                "play" => {
                    cx.spawn(async move |this, cx| {
                        for _ in 0..40 {
                            cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                            let started = this
                                .update(cx, |v, cx| {
                                    let client = v.client.read(cx);
                                    let Some(channel) = client.selected_channel().map(str::to_owned) else { return false };
                                    let found = client.logs.get(&channel).and_then(|log| {
                                        log.messages.iter().find_map(|m| {
                                            m.attachments.iter().enumerate().find(|(_, a)| rhein_voice::playback::playable(&a.filename, &a.mime)).map(|(i, a)| (m.id.clone(), i, a.clone()))
                                        })
                                    });
                                    let Some((message, i, a)) = found else { return false };
                                    let Some(url) = a.url.clone() else { return false };
                                    v.client.update(cx, |c, cx| c.toggle_sound(format!("{message}#{i}"), rhein_store::MediaSource::Url(url), a.filename.clone(), cx));
                                    true
                                })
                                .unwrap_or(true);
                            if started {
                                break;
                            }
                        }
                    })
                    .detach();
                }
                // The first private conversation, and its call (run with RHEIN_AUDIO=null).
                "dmcall" => {
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(std::time::Duration::from_secs(5)).await;
                        this.update(cx, |v, cx| {
                            let first = v.client.read(cx).conversations.first().map(|c| c.id.clone());
                            v.client.update(cx, |c, cx| {
                                c.open_conversation(first.clone(), cx);
                                if let Some(id) = first {
                                    c.join_dm_call(id, cx);
                                }
                            });
                        })
                        .ok();
                    })
                    .detach();
                }
                // The first private conversation, once the list is in.
                "dm" => {
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(std::time::Duration::from_secs(4)).await;
                        this.update(cx, |v, cx| {
                            let first = v.client.read(cx).conversations.first().map(|c| c.id.clone());
                            v.client.update(cx, |c, cx| c.open_conversation(first, cx));
                        })
                        .ok();
                    })
                    .detach();
                }
                other if other.starts_with("switch=") => {
                    let user = other.trim_start_matches("switch=").to_owned();
                    this.client.update(cx, |c, cx| c.switch_account(&user, cx));
                }
                "mfa-preview" => {
                    this.open_settings(window, cx);
                    if let Some((view, _)) = &this.settings {
                        view.update(cx, |v, cx| v.dev_mfa_preview(cx));
                    }
                }
                // The crop dialog on a file: `crop=banner:/path` or `crop=avatar:/path`.
                other if other.starts_with("crop=") => {
                    this.open_settings(window, cx);
                    let (kind, path) = other.trim_start_matches("crop=").split_once(':').unwrap_or(("avatar", ""));
                    let (banner, path) = (kind == "banner", std::path::PathBuf::from(path));
                    if let Some((view, _)) = &this.settings {
                        view.update(cx, |v, cx| v.dev_crop(path, banner, cx));
                    }
                }
                // Panics on purpose, for checking that crash.log keeps it.
                "panic" => panic!("RHEIN_DEV_OPEN=panic"),
                // Once READY and the history are in, scrolls the history up and down, then opens
                // the settings; for chasing what scrolling leaves behind.
                "scroll-settings" => {
                    cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor().timer(std::time::Duration::from_secs(6)).await;
                        for n in 0..24 {
                            let by = if n % 12 < 6 { -60. } else { 60. };
                            this.update(cx, |v, cx| v.chat.update(cx, |c, cx| c.dev_scroll(by, cx))).ok();
                            cx.background_executor().timer(std::time::Duration::from_millis(40)).await;
                        }
                        cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                        log::warn!("dev: opening the settings");
                        this.update_in(cx, |v, window, cx| v.open_settings(window, cx)).ok();
                    })
                    .detach();
                }
                other if other.starts_with("settings=") => {
                    this.open_settings(window, cx);
                    let section = other.trim_start_matches("settings=").to_owned();
                    if let Some((view, _)) = &this.settings {
                        view.update(cx, |v, cx| v.open_named(&section, cx));
                    }
                }
                other => {
                    if let Some(user) = other.strip_prefix("profile=").map(str::to_owned) {
                        cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(std::time::Duration::from_secs(3)).await;
                            this.update(cx, |v, cx| v.show_profile(user, gpui::point(px(420.), px(90.)), cx)).ok();
                        })
                        .detach();
                    }
                }
            });
        }
        Self {
            client,
            tree_scroll: gpui::ScrollHandle::new(),
            members_list: gpui::ListState::new(0, gpui::ListAlignment::Top, px(400.)),
            member_rows: Rc::default(),
            parts: None,
            card_clock: cx.new(|_| Clock),
            chat,
            home,
            dialog: None,
            dialog_input,
            create_on: None,
            show_members: true,
            panel: Panel::None,
            panel_subs: Vec::new(),
            collapsed: HashSet::new(),
            folded_guilds: HashSet::new(),
            unfolded_guilds: HashSet::new(),
            open_guild: None,
            confirm_sign_out: None,
            modal_focus: cx.focus_handle(),
            menu: MenuHost::default(),
            modal: None,
            settings: None,
            guild_settings: None,
            card_closed: None,
            full_profile: None,
            switcher: None,
            profile_card: None,
            card_ticker: None,
            last_input,
            _idle_watch: idle_watch,
            _subs: subs,
        }
    }

    fn parts(&mut self, cx: &mut Context<Self>) -> &ShellParts {
        if self.parts.is_none() {
            let shell = cx.entity();
            let client = self.client.clone();
            let sidebar = part(&shell, |s, _, cx| s.sidebar(cx), cx);
            let members = part(&shell, |s, window, cx| s.members(window, cx), cx);
            for p in [&sidebar, &members] {
                p.update(cx, |p, cx| p.watch(&client, cx));
            }
            // The sidebar's call bar shows who is speaking.
            let speaking = client.read(cx).call_speaking.clone();
            sidebar.update(cx, |p, cx| p.watch(&speaking, cx));
            self.parts = Some(ShellParts { sidebar, members });
        }
        self.parts.as_ref().expect("made above")
    }

    fn active_kind(&self) -> Option<PanelKind> {
        match self.panel {
            Panel::None if self.show_members => Some(PanelKind::Members),
            Panel::Pins { .. } => Some(PanelKind::Pins),
            Panel::Threads { .. } => Some(PanelKind::Threads),
            Panel::Search { .. } => Some(PanelKind::Search),
            _ => None,
        }
    }

    fn sync_header(&self, cx: &mut Context<Self>) {
        let kind = self.active_kind();
        self.chat.update(cx, |c, cx| c.set_active_panel(kind, cx));
    }

    fn toggle_panel(&mut self, kind: PanelKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_kind() == Some(kind) {
            if kind == PanelKind::Members {
                self.show_members = false;
            }
            self.close_panel(cx);
            return;
        }
        self.open_panel(kind, window, cx);
    }

    fn open_panel(&mut self, kind: PanelKind, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.read(cx);
        let Some(channel) = client.selected_channel().map(str::to_owned) else { return };
        let guild = client.guild_of_channel(&channel).map(|g| g.guild.id.clone());
        let chat_view = self.chat.downgrade();
        self.panel_subs.clear();
        self.panel = match kind {
            PanelKind::Members => {
                self.show_members = true;
                Panel::None
            }
            PanelKind::Pins => {
                let client = self.client.clone();
                let ch = channel.clone();
                let view = cx.new(|cx| PinsPanel::new(client, chat_view, ch, cx));
                self.panel_subs.push(self.panel_events(&view, window, cx));
                Panel::Pins { channel, view }
            }
            PanelKind::Threads => {
                let client = self.client.clone();
                let ch = channel.clone();
                let view = cx.new(|cx| ThreadsPanel::new(client, ch, window, cx));
                self.panel_subs.push(self.panel_events(&view, window, cx));
                Panel::Threads { channel, view }
            }
            PanelKind::Search => {
                let Some(guild) = guild else { return };
                let client = self.client.clone();
                let (g, ch) = (guild.clone(), channel.clone());
                let view = cx.new(|cx| SearchPanel::new(client, chat_view, g, ch, window, cx));
                self.panel_subs.push(self.panel_events(&view, window, cx));
                Panel::Search { guild, view }
            }
        };
        self.sync_header(cx);
        cx.notify();
    }

    fn panel_events<P: EventEmitter<PanelEvent>>(&self, view: &Entity<P>, window: &mut Window, cx: &mut Context<Self>) -> Subscription {
        cx.subscribe_in(view, window, |this, _, event: &PanelEvent, window, cx| match event {
            PanelEvent::Close => this.close_panel(cx),
            PanelEvent::OpenThread(id) => this.open_thread(id.clone(), window, cx),
        })
    }

    fn open_thread(&mut self, thread_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let view = cx.new(|cx| ChatView::fixed(client, thread_id, window, cx));
        self.panel_subs.clear();
        self.panel_subs.push(cx.subscribe_in(&view, window, |this, _, event: &ChatEvent, window, cx| match event {
            ChatEvent::ClosePanel => this.close_panel(cx),
            ChatEvent::OpenThread(id) => this.open_thread(id.clone(), window, cx),
            ChatEvent::TogglePanel(kind) => this.toggle_panel(*kind, window, cx),
            ChatEvent::Profile { user_id, at } => this.show_profile(user_id.clone(), *at, cx),
            ChatEvent::MemberMenu { user_id, at } => this.member_menu(user_id.clone(), *at, window, cx),
        }));
        let focus = view.read(cx).composer_focus(cx);
        window.focus(&focus, cx);
        self.panel = Panel::Thread { view };
        self.sync_header(cx);
        cx.notify();
    }

    fn close_panel(&mut self, cx: &mut Context<Self>) {
        self.panel = Panel::None;
        self.panel_subs.clear();
        self.sync_header(cx);
        cx.notify();
    }

    /// Pins and threads belong to a channel, search to a guild: follow the selection, and close a
    /// thread that belongs to another guild.
    fn follow_channel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.read(cx);
        let channel = client.selected_channel().map(str::to_owned);
        let guild = channel.as_deref().and_then(|c| client.guild_of_channel(c)).map(|g| g.guild.id.clone());
        let reopen = match &self.panel {
            Panel::Pins { channel: c, .. } if Some(c) != channel.as_ref() => Some(PanelKind::Pins),
            Panel::Threads { channel: c, .. } if Some(c) != channel.as_ref() => Some(PanelKind::Threads),
            Panel::Search { guild: g, .. } if Some(g) != guild.as_ref() => Some(PanelKind::Search),
            _ => None,
        };
        match reopen {
            Some(_) if channel.is_none() => self.close_panel(cx),
            Some(kind) => self.open_panel(kind, window, cx),
            None => {}
        }
    }

    fn render_panel(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = cx.theme().clone();
        let (content, width): (AnyElement, gpui::Pixels) = match &self.panel {
            Panel::None => return self.members_shown(cx).then(|| self.parts(cx).members.clone().cached(gpui::StyleRefinement::default().w(size::PANEL).h_full().flex_none()).into_any_element()),
            // Cached: each panel draws again when it or the client changes.
            Panel::Pins { view, .. } => (view.clone().cached(fill()).into_any_element(), px(320.)),
            Panel::Threads { view, .. } => (view.clone().cached(fill()).into_any_element(), px(320.)),
            Panel::Search { view, .. } => (view.clone().cached(fill()).into_any_element(), px(380.)),
            Panel::Thread { view } => (view.clone().cached(fill()).into_any_element(), px(420.)),
        };
        Some(
            div()
                .w(width)
                .h_full()
                .flex_none()
                .flex()
                .flex_col()
                .bg(t.bg_panel)
                .border_l_1()
                .border_color(t.border)
                .child(content)
                .into_any_element(),
        )
    }

    fn open_dialog(&mut self, tab: DialogTab, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog = Some(tab);
        let placeholder = match tab {
            DialogTab::Join => "server/CODE or https://server/invite/CODE",
            DialogTab::Create => "Guild name",
            DialogTab::Server => "chat.example.com",
        };
        self.dialog_input.update(cx, |i, cx| {
            i.clear(cx);
            i.set_placeholder(placeholder, cx);
        });
        if self.create_on.is_none() {
            self.create_on = self.client.read(cx).home.as_ref().map(|h| h.doc.server_name.clone());
        }
        window.focus(&self.dialog_input.focus_handle(cx), cx);
        cx.notify();
    }

    fn ask_sign_out(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog = None;
        let client = self.client.read(cx);
        let me = client.user_id().unwrap_or_default().to_owned();
        self.confirm_sign_out = Some(client.saved_accounts().iter().filter(|a| a.user_id != me).count());
        window.focus(&self.modal_focus, cx);
        cx.notify();
    }

    fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.confirm_sign_out = None;
        self.client.update(cx, |c, cx| c.sign_out(cx));
    }

    fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog = None;
        self.confirm_sign_out = None;
        let focus = self.chat.read(cx).composer_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn confirm_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.dialog else { return };
        let text = self.dialog_input.read(cx).text().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let client = self.client.read(cx);
        let default_server = client
            .selected_guild
            .as_ref()
            .and_then(|g| client.guild(g))
            .map(|g| g.server.clone())
            .or_else(|| client.home.as_ref().map(|h| h.doc.server_name.clone()));
        let create_on = self.create_on.clone();
        self.client.update(cx, |c, cx| match tab {
            DialogTab::Join => c.join_invite(text, default_server, cx),
            DialogTab::Create => {
                if let Some(server) = create_on {
                    c.create_guild(server, text, cx)
                }
            }
            DialogTab::Server => c.add_server(text, cx),
        });
        self.close_dialog(window, cx);
    }

    /// Everything you can do with a guild; its `…` button and a right-click on its row.
    fn show_guild_menu(&mut self, gid: String, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        // Developer mode (on by default) shows the "Copy … ID" entries.
        let dev = self.client.read(cx).prefs.developer_mode;
        let client = self.client.read(cx);
        let Some(entry) = client.guild(&gid) else { return };
        let name = entry.guild.name.clone();
        let owner = client.is_owner(&gid);
        let manage_channels = client.guild_can(&gid, perms::MANAGE_CHANNELS);
        let manage_guild = client.guild_can(&gid, perms::MANAGE_GUILD);
        let this = cx.weak_entity();
        let mut entries = Vec::new();
        {
            let (v, g) = (this.clone(), gid.clone());
            entries.push(MenuEntry::item("Guild settings", move |window, cx| {
                let g = g.clone();
                v.update(cx, |v, cx| v.open_guild_settings(g, GuildPane::Overview, window, cx)).ok();
            }).with_icon(Icon::Settings));
        }
        {
            let (c, g) = (self.client.clone(), gid.clone());
            entries.push(MenuEntry::item("Invite people", move |_, cx| {
                let g = g.clone();
                c.update(cx, |c, cx| c.create_invite(g, cx));
            }).with_icon(Icon::UserPlus));
            let (c, g) = (self.client.clone(), gid.clone());
            entries.push(MenuEntry::item("Mark as read", move |_, cx| c.update(cx, |c, cx| c.mark_guild_read(&g, cx))).with_icon(Icon::Check));
        }
        if manage_channels {
            let (v, g) = (this.clone(), gid.clone());
            entries.push(MenuEntry::item("Create channel", move |window, cx| {
                let g = g.clone();
                v.update(cx, |v, cx| v.open_create_channel(g, None, window, cx)).ok();
            }).with_icon(Icon::Plus));
        }
        if manage_channels {
            let (v, g) = (this.clone(), gid.clone());
            entries.push(MenuEntry::item("Create category", move |window, cx| {
                let g = g.clone();
                v.update(cx, |v, cx| {
                    let client = v.client.clone();
                    v.prompt("Create category", vec![("Name".into(), String::new())], "Create", move |values, cx| {
                        let name = values[0].trim().to_owned();
                        if !name.is_empty() {
                            client.update(cx, |c, cx| c.create_channel(g.clone(), name, "category".into(), None, cx));
                        }
                    }, window, cx)
                }).ok();
            }).with_icon(Icon::Plus));
        }
        entries.push(self.mute_entry(gid.clone(), "guild", cx));
        if manage_guild {
            let (v, g, n) = (this.clone(), gid.clone(), name.clone());
            entries.push(MenuEntry::item("Rename guild", move |window, cx| {
                let (g, n) = (g.clone(), n.clone());
                v.update(cx, |v, cx| {
                    let client = v.client.clone();
                    v.prompt("Rename guild", vec![("Name".into(), n)], "Save", move |values, cx| {
                        let name = values[0].trim().to_owned();
                        if !name.is_empty() {
                            client.update(cx, |c, cx| c.rename_guild(g.clone(), name, cx));
                        }
                    }, window, cx)
                }).ok();
            }).with_icon(Icon::Pencil));
        }
        entries.push(MenuEntry::Separator);
        {
            let g = gid.clone();
            if dev { entries.push(MenuEntry::item("Copy guild ID", move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(g.clone()))).with_icon(Icon::Copy)); }
        }
        {
            let v = this.clone();
            entries.push(MenuEntry::item("Join a guild", move |window, cx| {
                v.update(cx, |v, cx| v.open_dialog(DialogTab::Join, window, cx)).ok();
            }).with_icon(Icon::LogIn));
            let v = this.clone();
            entries.push(MenuEntry::item("Create a guild", move |window, cx| {
                v.update(cx, |v, cx| v.open_dialog(DialogTab::Create, window, cx)).ok();
            }).with_icon(Icon::Plus));
        }
        if owner {
            // The owner cannot leave; deleting, with its safeguard, lives in the overview.
            entries.push(MenuEntry::Separator);
            let (v, g) = (this.clone(), gid.clone());
            entries.push(MenuEntry::item("Delete guild…", move |window, cx| {
                let g = g.clone();
                v.update(cx, |v, cx| v.open_guild_settings(g, GuildPane::Overview, window, cx)).ok();
            }).with_icon(Icon::Trash).danger());
        }
        if !owner {
            entries.push(MenuEntry::Separator);
            let (v, g) = (this.clone(), gid.clone());
            entries.push(MenuEntry::item("Leave guild", move |window, cx| {
                let (g, n) = (g.clone(), name.clone());
                v.update(cx, |v, cx| {
                    let client = v.client.clone();
                    v.confirm(
                        format!("Leave {n}?"),
                        "You can come back with a new invite.".into(),
                        "Leave".into(),
                        true,
                        move |cx| client.update(cx, |c, cx| c.leave_guild(g.clone(), cx)),
                        window,
                        cx,
                    )
                }).ok();
            }).with_icon(Icon::LogOut).danger());
        }
        self.menu.show(at, entries, |v: &mut Shell| &mut v.menu, window, cx);
    }

    /// "Mute …" with how long, or "Unmute …" when it already is (muted in the account's
    /// preferences, shared with the web client).
    fn mute_entry(&self, id: String, noun: &str, cx: &App) -> MenuEntry {
        const DURATIONS: [(&str, Option<i64>); 6] = [
            ("For 15 minutes", Some(15 * 60_000)),
            ("For 1 hour", Some(60 * 60_000)),
            ("For 3 hours", Some(3 * 60 * 60_000)),
            ("For 8 hours", Some(8 * 60 * 60_000)),
            ("For 24 hours", Some(24 * 60 * 60_000)),
            ("Until I turn it back on", None),
        ];
        let client = self.client.clone();
        match self.client.read(cx).muted_until(&id) {
            Some(until) => {
                let label = match until {
                    Some(ms) => format!("Unmute {noun} (muted until {})", crate::views::rows::format_hm(ms, true)),
                    None => format!("Unmute {noun}"),
                };
                MenuEntry::item(label, move |_, cx| client.update(cx, |c, cx| c.set_muted(&id, None, true, cx))).with_icon(Icon::Bell)
            }
            None => {
                let choices = DURATIONS
                    .iter()
                    .map(|&(label, ms)| {
                        let (client, id) = (client.clone(), id.clone());
                        MenuEntry::item(label, move |_, cx| client.update(cx, |c, cx| c.set_muted(&id, ms, false, cx)))
                    })
                    .collect();
                MenuEntry::expand(format!("Mute {noun}"), choices)
                    .with_leading(|_, cx| rhein_ui::icon_sized(Icon::Bell, px(16.)).text_color(cx.theme().text).into_any_element())
            }
        }
    }

    fn channel_menu(&mut self, channel_id: String, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        // Developer mode (on by default) shows the "Copy … ID" entries.
        let dev = self.client.read(cx).prefs.developer_mode;
        let client = self.client.read(cx);
        let Some(channel) = client.channel(&channel_id).cloned() else { return };
        let manage = client.can(&channel_id, perms::MANAGE_CHANNELS);
        let manage_roles = client.can(&channel_id, perms::MANAGE_ROLES);
        let guild_id = client.guild_of_channel(&channel_id).map(|g| g.guild.id.clone());
        let category = channel.is_category();
        let this = cx.weak_entity();
        let mut entries = Vec::new();
        if category && manage && let Some(g) = guild_id.clone() {
            let (v, cat) = (this.clone(), channel_id.clone());
            entries.push(MenuEntry::item("Create channel here", move |window, cx| {
                let (g, cat) = (g.clone(), cat.clone());
                v.update(cx, |v, cx| v.open_create_channel(g, Some(cat), window, cx)).ok();
            }).with_icon(Icon::Plus));
        }
        if channel.is_textual() {
            let (c, id) = (self.client.clone(), channel_id.clone());
            entries.push(MenuEntry::item("Mark as read", move |_, cx| {
                let id = id.clone();
                c.update(cx, |c, cx| c.mark_read(id, cx));
            }).with_icon(Icon::Check));
        }
        entries.push(self.mute_entry(channel_id.clone(), if category { "category" } else { "channel" }, cx));
        {
            let id = channel_id.clone();
            let label = if category { "Copy category ID" } else { "Copy channel ID" };
            if dev { entries.push(MenuEntry::item(label, move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(id.clone()))).with_icon(Icon::Copy)); }
        }
        if manage {
            entries.push(MenuEntry::Separator);
            for (label, by, glyph) in [("Move up", -1, Icon::ChevronUp), ("Move down", 1, Icon::ChevronDown)] {
                let (c, id) = (self.client.clone(), channel_id.clone());
                entries.push(MenuEntry::item(label, move |_, cx| c.update(cx, |c, cx| c.move_channel(&id, by, cx))).with_icon(glyph));
            }
        }
        if manage_roles && !category && let Some(g) = guild_id.clone() {
            let (v, id) = (this.clone(), channel_id.clone());
            entries.push(MenuEntry::item("Permissions", move |window, cx| {
                let (g, id) = (g.clone(), id.clone());
                v.update(cx, |v, cx| {
                    v.open_guild_settings(g, GuildPane::Channels, window, cx);
                    if let Some((view, _)) = &v.guild_settings {
                        view.update(cx, |s, cx| s.focus_channel(id, cx));
                    }
                }).ok();
            }).with_icon(Icon::Lock));
        }
        if manage {
            entries.push(MenuEntry::Separator);
            let (v, id, edited) = (this.clone(), channel_id.clone(), channel.clone());
            entries.push(MenuEntry::item(if category { "Edit category" } else { "Edit channel" }, move |window, cx| {
                let (id, channel) = (id.clone(), edited.clone());
                v.update(cx, |v, cx| {
                    let client = v.client.clone();
                    let number = |n: Option<u64>| n.filter(|n| *n > 0).map(|n| n.to_string()).unwrap_or_default();
                    let mut fields = vec![("Name".to_owned(), channel.name.clone())];
                    if channel.is_voice() {
                        fields.push(("Bitrate in bit/s (empty: the server's)".into(), number(channel.bitrate)));
                        fields.push(("User limit (empty: none)".into(), number(channel.user_limit)));
                        fields.push(("Region (auto, or a region's name)".into(), channel.voice_region.clone().unwrap_or_default()));
                    } else if !channel.is_category() {
                        fields.push(("Topic".into(), channel.topic.clone().unwrap_or_default()));
                        fields.push(("Slowmode in seconds (empty: off)".into(), number(channel.slowmode)));
                    }
                    let (voice, category) = (channel.is_voice(), channel.is_category());
                    v.prompt("Edit channel", fields, "Save", move |values, cx| {
                        let name = values[0].trim();
                        if name.is_empty() {
                            return;
                        }
                        let count = |text: &str| text.trim().parse::<u64>().unwrap_or(0);
                        let patch = if voice {
                            let region = values[3].trim();
                            serde_json::json!({
                                "name": name,
                                "bitrate": (count(&values[1]) > 0).then(|| count(&values[1])),
                                "user_limit": count(&values[2]),
                                "voice_region": if region.is_empty() { "auto" } else { region },
                            })
                        } else if category {
                            serde_json::json!({ "name": name })
                        } else {
                            serde_json::json!({ "name": name, "topic": values[1].trim(), "slowmode": count(&values[2]) })
                        };
                        client.update(cx, |c, cx| c.edit_channel(id.clone(), patch, cx));
                    }, window, cx)
                }).ok();
            }).with_icon(Icon::Pencil));
            let (v, id, name) = (this.clone(), channel_id.clone(), channel.name.clone());
            entries.push(MenuEntry::item(if category { "Delete category" } else { "Delete channel" }, move |window, cx| {
                let (id, name) = (id.clone(), name.clone());
                v.update(cx, |v, cx| {
                    let client = v.client.clone();
                    v.confirm(
                        if category { format!("Delete the {name} category?") } else { format!("Delete #{name}?") },
                        if category { "The channels in it stay, without a category.".into() } else { "The channel and its messages are removed for everyone.".into() },
                        "Delete".into(),
                        true,
                        move |cx| client.update(cx, |c, cx| c.delete_channel(id.clone(), cx)),
                        window,
                        cx,
                    )
                }).ok();
            }).with_icon(Icon::Trash).danger());
        }
        self.menu.show(at, entries, |v: &mut Shell| &mut v.menu, window, cx);
    }

    fn member_menu(&mut self, user_id: String, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        // Developer mode (on by default) shows the "Copy … ID" entries.
        let dev = self.client.read(cx).prefs.developer_mode;
        let client = self.client.read(cx);
        let Some(gid) = client.selected_guild.clone() else { return };
        let me = client.user_id().map(str::to_owned);
        let myself = me.as_deref() == Some(user_id.as_str());
        let name = client.display_name(Some(&gid), &user_id);
        let target_owner = client.guild(&gid).is_some_and(|g| g.guild.owner.as_deref() == Some(user_id.as_str()));
        let moderate = !myself && !target_owner && client.guild_can(&gid, perms::MODERATE_MEMBERS);
        let kick = !myself && !target_owner && client.guild_can(&gid, perms::KICK_MEMBERS);
        let ban = !myself && !target_owner && client.guild_can(&gid, perms::BAN_MEMBERS);
        // Roles to give or take: those below our own top role, not held by a bot.
        let role_choices: Vec<(String, String, bool)> = if client.guild_can(&gid, perms::MANAGE_ROLES) {
            let guild = client.guild(&gid).map(|g| g.guild.clone()).unwrap_or_default();
            let top = guild.me.as_ref().and_then(|m| m.get("top_role_position")).and_then(|v| v.as_i64()).unwrap_or(0);
            let owner = client.is_owner(&gid);
            let held: Vec<String> = client.member(&gid, &user_id).map(|m| m.roles.clone()).unwrap_or_default();
            let mut roles: Vec<_> = guild.roles.iter().filter(|r| r.id != gid && !r.managed && (owner || r.position < top)).cloned().collect();
            roles.sort_by_key(|r| std::cmp::Reverse(r.position));
            roles.into_iter().map(|r| { let has = held.contains(&r.id); (r.id, r.name, has) }).collect()
        } else {
            Vec::new()
        };
        let this = cx.weak_entity();
        let mut entries = Vec::new();
        {
            let (v, u) = (this.clone(), user_id.clone());
            entries.push(MenuEntry::item("View profile", move |window, cx| {
                let at = window.mouse_position();
                let u = u.clone();
                v.update(cx, |v, cx| v.show_profile(u, at, cx)).ok();
            }).with_icon(Icon::User));
            let (v, u) = (this.clone(), user_id.clone());
            entries.push(MenuEntry::item("Mention", move |window, cx| {
                let u = u.clone();
                v.update(cx, |v, cx| v.chat.update(cx, |c, cx| c.insert_text(&format!("<{u}> "), window, cx))).ok();
            }).with_icon(Icon::AtSign));
            let u = user_id.clone();
            if dev { entries.push(MenuEntry::item("Copy user ID", move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(u.clone()))).with_icon(Icon::Copy)); }
        }
        if !myself {
            let (c, u) = (self.client.clone(), user_id.clone());
            entries.insert(1, MenuEntry::item("Message", move |_, cx| {
                let u = u.clone();
                c.update(cx, |c, cx| c.message_user(u, cx));
            }).with_icon(Icon::MessageSquare));
        }
        // In a call: how loud this person (and their screen's sound) is for us alone.
        if !myself && client.call().is_some() {
            for (label, key) in [("Voice volume", user_id.clone()), ("Screen volume", format!("{user_id}#screen"))] {
                let current = client.user_volume(&key);
                let choices = [0u32, 25, 50, 100, 150, 200]
                    .into_iter()
                    .map(|percent| {
                        let (c, key) = (self.client.clone(), key.clone());
                        MenuEntry::item(if percent == 0 { "Off".to_owned() } else { format!("{percent} %") }, move |_, cx| {
                            c.update(cx, |c, cx| {
                                c.set_user_volume(&key, percent);
                                cx.notify();
                            })
                        })
                        .checked(current == percent)
                    })
                    .collect();
                entries.push(MenuEntry::expand(format!("{label}: {current} %"), choices));
            }
        }
        if !myself && client.friend_state(&user_id).is_none() {
            let (c, u) = (self.client.clone(), user_id.clone());
            entries.push(MenuEntry::item("Add friend", move |_, cx| {
                let u = u.clone();
                c.update(cx, |c, cx| c.friend_action(rhein_store::FriendAction::Request, u, cx));
            }).with_icon(Icon::UserPlus));
        }
        // Your own nickname needs nothing; someone else's needs MODERATE_MEMBERS (and outranking
        // them, which the server checks).
        if myself || moderate {
            let (v, g, u) = (this.clone(), gid.clone(), user_id.clone());
            let current = client.member(&gid, &user_id).and_then(|m| m.nickname.clone()).unwrap_or_default();
            entries.push(MenuEntry::item("Change nickname", move |window, cx| {
                let (g, u, current) = (g.clone(), u.clone(), current.clone());
                v.update(cx, |v, cx| {
                    let client = v.client.clone();
                    v.prompt("Nickname in this guild", vec![("Nickname".into(), current)], "Save", move |values, cx| {
                        let nick = values[0].clone();
                        client.update(cx, |c, cx| {
                            c.set_nickname(g.clone(), u.clone(), nick, cx);
                            c.load_members(g.clone(), cx);
                        });
                    }, window, cx)
                }).ok();
            }).with_icon(Icon::Pencil));
        }
        if moderate || kick || ban {
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::Label("Moderation".into()));
        }
        let timed_out = client.member(&gid, &user_id).and_then(|m| m.timeout_until).is_some_and(|t| t > rhein_store::now_ms());
        if moderate && timed_out {
            let (c, g, u) = (self.client.clone(), gid.clone(), user_id.clone());
            entries.push(MenuEntry::item("Remove timeout", move |_, cx| c.update(cx, |c, cx| c.timeout_member(g.clone(), u.clone(), 0, cx))));
        }
        if moderate && !timed_out {
            for (label, minutes) in [("Time out for 10 minutes", 10), ("Time out for 1 hour", 60), ("Time out for 1 day", 1440)] {
                let (v, g, u, n) = (this.clone(), gid.clone(), user_id.clone(), name.clone());
                entries.push(MenuEntry::item(label, move |window, cx| {
                    let (g, u, n) = (g.clone(), u.clone(), n.clone());
                    v.update(cx, |v, cx| {
                        let client = v.client.clone();
                        v.confirm(
                            format!("Time out {n}?"),
                            "They can read but not write, react or talk until it ends.".into(),
                            "Time out".into(),
                            true,
                            move |cx| client.update(cx, |c, cx| c.timeout_member(g.clone(), u.clone(), minutes, cx)),
                            window,
                            cx,
                        )
                    }).ok();
                }));
            }
        }
        if !role_choices.is_empty() {
            let choices = role_choices
                .into_iter()
                .map(|(role, label, has)| {
                    let (c, g, u) = (self.client.clone(), gid.clone(), user_id.clone());
                    MenuEntry::item(label, move |_, cx| {
                        let (g, u, role) = (g.clone(), u.clone(), role.clone());
                        c.update(cx, |c, cx| {
                            c.set_member_role(g.clone(), u, role, !has, cx);
                            c.load_members(g, cx);
                        })
                    })
                    .checked(has)
                })
                .collect();
            entries.push(MenuEntry::expand("Roles", choices).with_leading(|_, cx| rhein_ui::icon_sized(Icon::Users, px(16.)).text_color(cx.theme().text).into_any_element()));
        }
        for (enabled, label, verb, ban_it) in [(kick, "Kick", "kick", false), (ban, "Ban", "ban", true)] {
            if !enabled {
                continue;
            }
            let (v, g, u, n) = (this.clone(), gid.clone(), user_id.clone(), name.clone());
            entries.push(MenuEntry::item(format!("{label} {name}"), move |window, cx| {
                let (g, u, n) = (g.clone(), u.clone(), n.clone());
                v.update(cx, |v, cx| {
                    let client = v.client.clone();
                    let body = if ban_it { "They are removed and cannot rejoin until unbanned." } else { "They are removed and can rejoin with an invite." };
                    v.confirm(
                        format!("{} {n}?", if ban_it { "Ban" } else { "Kick" }),
                        body.into(),
                        verb[..1].to_uppercase() + &verb[1..],
                        true,
                        move |cx| client.update(cx, |c, cx| {
                            if ban_it { c.ban_member(g.clone(), u.clone(), cx) } else { c.kick_member(g.clone(), u.clone(), cx) }
                            c.load_members(g.clone(), cx);
                        }),
                        window,
                        cx,
                    )
                }).ok();
            }).danger());
        }
        self.menu.show(at, entries, |v: &mut Shell| &mut v.menu, window, cx);
    }

    /// Your account, opened from the profile strip: your card, presence and custom status, the
    /// saved accounts to switch between, and signing out.
    fn account_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.read(cx);
        let Some(me) = client.user_id().map(str::to_owned) else { return };
        let prefs = client.prefs.clone();
        let accounts = client.saved_accounts();
        let this = cx.weak_entity();
        let mut entries = Vec::new();

        // Your whole card, as others see it: banner, face, name, address, pronouns, status line,
        // about me, and what you are sharing.
        {
            let client = self.client.clone();
            let me = me.clone();
            entries.push(MenuEntry::custom(move |window, cx| {
                let t = cx.theme().clone();
                let profile = client.update(cx, |c, cx| c.profile(&me, cx));
                let c = client.read(cx);
                let name = c.display_name(None, &me);
                let look = profile::Look {
                    banner: profile.as_ref().and_then(|p| p.banner_ref.as_deref()).and_then(|r| c.banner_url(&me, r)),
                    accent: profile.as_ref().and_then(|p| p.accent_color),
                    theme: profile.as_ref().and_then(|p| p.theme_colors),
                };
                let presence = presence_of(c.effective_status());
                let face = Avatar::new(me.clone(), name.clone()).size(px(profile::FACE_SIZE)).image(c.avatar_url(None, &me).map(Into::into)).presence(Some(presence));
                let pronouns = profile.as_ref().and_then(|p| p.pronouns.clone()).filter(|p| !p.is_empty());
                let bio = profile.as_ref().and_then(|p| p.bio.clone()).filter(|b| !b.is_empty());
                let now_ms = rhein_store::client::now_ms().max(0) as u64;
                let activities: Vec<AnyElement> = c
                    .shared_activities()
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        // Our own artwork loads directly: it came from this machine or its home server.
                        let assets = a.assets.clone().unwrap_or_default();
                        activity::card(&format!("me-activity-{i}"), a, assets.large_image, assets.small_image, now_ms, &t, window)
                    })
                    .collect();
                let body = div()
                    .px_4()
                    .pb_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(div().text_size(text::LG).font_weight(FontWeight::SEMIBOLD).truncate().child(name))
                            .child(div().text_size(text::SM).text_color(t.text_faint).truncate().child(me.clone()))
                            .children(pronouns.map(|p| div().text_size(text::SM).text_color(t.text_muted).child(p)))
                            .children(c.prefs.custom_status.clone().map(|s| div().pt_1().text_size(text::SM).child(s))),
                    )
                    .children(bio.map(|bio| {
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().font_family(FONT_MONO).text_size(text::XS).text_color(t.text_faint).child("about me"))
                            .child(div().text_size(text::SM).child(bio))
                    }))
                    .children(activities);
                div()
                    .mb_1()
                    .child(profile::frame(&look, &t, div().flex().flex_col().child(profile::header(&look, face.into_any_element(), &t)).child(body)))
                    .into_any_element()
            }));
        }

        // Presence: the current one, opening to all four with it checked.
        {
            const CHOICES: [(&str, &str); 4] = [("online", "Online"), ("idle", "Idle"), ("dnd", "Do not disturb"), ("invisible", "Invisible")];
            let choices = CHOICES
                .iter()
                .map(|&(status, label)| {
                    let (c, keep) = (self.client.clone(), prefs.custom_status.clone());
                    MenuEntry::item(label, move |_, cx| c.update(cx, |c, cx| c.set_presence(status.into(), keep.clone(), cx)))
                        .with_leading(move |_, cx| presence_dot(presence_of(status), cx))
                        .checked(prefs.status == status)
                })
                .collect();
            let current = prefs.status.clone();
            let label = CHOICES.iter().find(|(s, _)| *s == current).map_or("Online", |(_, l)| *l);
            entries.push(MenuEntry::expand(label, choices).with_leading(move |_, cx| presence_dot(presence_of(&current), cx)));
        }
        {
            let v = this.clone();
            let label = if prefs.custom_status.is_some() { "Edit custom status…" } else { "Set a custom status…" };
            entries.push(MenuEntry::item(label, move |window, cx| {
                v.update(cx, |v, cx| {
                    let client = v.client.clone();
                    let current = client.read(cx).prefs.custom_status.clone().unwrap_or_default();
                    v.prompt("Custom status", vec![("What are you up to?".into(), current)], "Save", move |values, cx| {
                        let text = values[0].clone();
                        client.update(cx, |c, cx| {
                            let status = c.prefs.status.clone();
                            c.set_presence(status, Some(text), cx)
                        });
                    }, window, cx)
                }).ok();
            }).with_icon(Icon::Smile));
        }
        // With more than one activity going, which one shows first: in the member list, the
        // strip down here and for everyone else.
        let running = self.client.read(cx).shared_activities().to_vec();
        if running.len() > 1 {
            use rhein_proto::activity::Slot;
            let chosen = prefs.primary_activity.as_deref().and_then(Slot::from_key);
            let mut choices = Vec::new();
            {
                let c = self.client.clone();
                choices.push(
                    MenuEntry::item("Automatic", move |_, cx| c.update(cx, |c, cx| c.set_primary_activity(None, cx)))
                        .checked(chosen.is_none()),
                );
            }
            for activity in &running {
                let (c, slot) = (self.client.clone(), activity.slot());
                let label = format!("{} {}", activity.verb(), activity.name);
                choices.push(
                    MenuEntry::item(label, move |_, cx| c.update(cx, |c, cx| c.set_primary_activity(Some(slot), cx)))
                        .checked(chosen == Some(slot)),
                );
            }
            entries.push(
                MenuEntry::expand("Primary activity", choices)
                    .with_leading(|_, cx| rhein_ui::icon_sized(Icon::Gamepad, px(16.)).text_color(cx.theme().text).into_any_element()),
            );
        }
        if prefs.custom_status.is_some() {
            let c = self.client.clone();
            entries.push(MenuEntry::item("Clear custom status", move |_, cx| c.update(cx, |c, cx| {
                let status = c.prefs.status.clone();
                c.set_presence(status, None, cx)
            })).with_icon(Icon::X));
        }
        {
            let v = this.clone();
            entries.push(MenuEntry::item("Edit profile", move |window, cx| {
                v.update(cx, |v, cx| v.open_settings(window, cx)).ok();
            }).with_icon(Icon::Pencil));
        }
        {
            let id = me.clone();
            entries.push(MenuEntry::item("Copy user ID", move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(id.clone()))).with_icon(Icon::Copy));
        }

        // Saved accounts, folded away: switch without a password, or add another.
        entries.push(MenuEntry::Separator);
        let mut switch = Vec::new();
        for account in accounts {
            let id = account.user_id.clone();
            let name = account.display_name.clone().unwrap_or_else(|| rhein_proto::localpart(&id).to_owned());
            let label = format!("{name} · {}", rhein_proto::domain_of(&id).unwrap_or_default());
            let (c, target) = (self.client.clone(), id.clone());
            let (face_id, face_name) = (id.clone(), name.clone());
            switch.push(
                MenuEntry::item(label, move |_, cx| c.update(cx, |c, cx| c.switch_account(&target, cx)))
                    .with_leading(move |_, _| Avatar::new(face_id.clone(), face_name.clone()).size(px(20.)).into_any_element())
                    .checked(id == me),
            );
        }
        {
            let c = self.client.clone();
            switch.push(MenuEntry::item("Add an account", move |_, cx| c.update(cx, |c, cx| c.add_account(cx))).with_icon(Icon::UserPlus));
        }
        entries.push(
            MenuEntry::expand("Switch account", switch)
                .with_leading(|_, cx| rhein_ui::icon_sized(Icon::Users, px(16.)).text_color(cx.theme().text).into_any_element()),
        );
        entries.push(MenuEntry::Separator);
        {
            let v = this.clone();
            entries.push(MenuEntry::item("Sign out", move |window, cx| {
                v.update(cx, |v, cx| v.ask_sign_out(window, cx)).ok();
            }).with_icon(Icon::LogOut).danger());
        }

        // Opens upward from the strip's top edge, flush with the sidebar's left.
        let strip = if self.client.read(cx).shared_activities().is_empty() { PROFILE_ROW_H } else { PROFILE_ROW_H * 2. };
        let at = gpui::point(px(8.), window.viewport_size().height - strip - px(4.));
        // The card's own width, and the menu's padding around it.
        let width = px(profile::CARD_W + 10.);
        self.menu.show_anchored(at, gpui::Anchor::BottomLeft, width, entries, |v: &mut Shell| &mut v.menu, window, cx);
    }

    // ---------------------------------------------------------------- modals

    // A confirmation is its title, text, button and consequence; bundling them would only move
    // the same arguments into a struct literal at every call site.
    #[allow(clippy::too_many_arguments)]
    fn confirm(&mut self, title: String, body: String, label: String, danger: bool, action: impl Fn(&mut App) + 'static, window: &mut Window, cx: &mut Context<Self>) {
        self.modal = Some(ShellModal::Confirm { title, body, label, danger, action: Rc::new(action) });
        window.focus(&self.modal_focus, cx);
        cx.notify();
    }

    /// A bot's form: its text inputs, sent back once.
    fn open_bot_modal(&mut self, modal: rhein_store::BotModal, window: &mut Window, cx: &mut Context<Self>) {
        let fields = modal
            .fields
            .iter()
            .map(|f| (if f.required { format!("{} (required)", f.label) } else { f.label.clone() }, f.value.clone()))
            .collect();
        let (client, title) = (self.client.clone(), modal.title.clone());
        self.prompt(&title, fields, "Submit", move |values, cx| client.update(cx, |c, cx| c.submit_modal(modal.clone(), values, cx)), window, cx);
    }

    fn prompt(&mut self, title: &str, fields: Vec<(String, String)>, label: &str, action: impl Fn(Vec<String>, &mut App) + 'static, window: &mut Window, cx: &mut Context<Self>) {
        let fields: Vec<(String, Entity<TextInput>)> = fields
            .into_iter()
            .map(|(label, value)| {
                let input = cx.new(|cx| TextInput::new(label.clone(), cx).with_text(value));
                (label, input)
            })
            .collect();
        for (_, input) in &fields {
            let sub = cx.subscribe_in(input, window, |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Submit => this.submit_modal(window, cx),
                InputEvent::Cancel => this.close_modal(window, cx),
                _ => {}
            });
            self._subs.push(sub);
        }
        if let Some((_, first)) = fields.first() {
            window.focus(&first.focus_handle(cx), cx);
        }
        self.modal = Some(ShellModal::Prompt { title: title.into(), fields, label: label.into(), action: Rc::new(action) });
        cx.notify();
    }

    fn open_create_channel(&mut self, guild_id: String, category: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| TextInput::new("new-channel", cx));
        let sub = cx.subscribe_in(&name, window, |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::Submit => this.submit_modal(window, cx),
            InputEvent::Cancel => this.close_modal(window, cx),
            _ => {}
        });
        self._subs.push(sub);
        window.focus(&name.focus_handle(cx), cx);
        self.modal = Some(ShellModal::CreateChannel { guild_id, name, kind: 0, category });
        cx.notify();
    }

    fn submit_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.modal.take() {
            Some(ShellModal::Confirm { action, .. }) => action(cx),
            Some(ShellModal::Prompt { fields, action, .. }) => {
                let values = fields.iter().map(|(_, i)| i.read(cx).text().to_owned()).collect();
                action(values, cx);
            }
            Some(ShellModal::CreateChannel { guild_id, name, kind, category }) => {
                let name = name.read(cx).text().trim().to_owned();
                if name.is_empty() {
                    return;
                }
                let kind = CHANNEL_KINDS[kind].0.to_owned();
                self.client.update(cx, |c, cx| c.create_channel(guild_id, name, kind, category, cx));
            }
            None => {}
        }
        self.close_modal(window, cx);
    }

    fn close_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.modal = None;
        let focus = self.chat.read(cx).composer_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn render_modal(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let modal = self.modal.as_ref()?;
        let t = cx.theme().clone();
        let this = cx.weak_entity();
        let dismiss = move |window: &mut Window, cx: &mut App| {
            this.update(cx, |v, cx| v.close_modal(window, cx)).ok();
        };
        let cancel = Button::new("modal-cancel", "Cancel").ghost().on_click(cx.listener(|v, _, window, cx| v.close_modal(window, cx)));
        let element = match modal {
            ShellModal::Confirm { title, body, label, danger, .. } => Modal::new("shell-confirm")
                .title(title.clone())
                .width(px(420.))
                .focus(&self.modal_focus)
                .on_dismiss(dismiss)
                .child(div().text_color(t.text_muted).child(body.clone()))
                .footer(cancel)
                .footer(Button::new("modal-ok", label.clone()).kind(if *danger { rhein_ui::ButtonKind::Danger } else { rhein_ui::ButtonKind::Primary })
                    .on_click(cx.listener(|v, _, window, cx| v.submit_modal(window, cx)))),
            ShellModal::Prompt { title, fields, label, .. } => Modal::new("shell-prompt")
                .title(title.clone())
                .width(px(420.))
                .on_dismiss(dismiss)
                .children(fields.iter().map(|(label, input)| {
                    div().flex().flex_col().gap_1p5()
                        .child(div().text_size(text::SM).text_color(t.text_muted).child(label.clone()))
                        .child(input.clone())
                }))
                .footer(cancel)
                .footer(Button::new("modal-ok", label.clone()).primary().on_click(cx.listener(|v, _, window, cx| v.submit_modal(window, cx)))),
            ShellModal::CreateChannel { guild_id, name, kind, category } => {
                let client = self.client.read(cx);
                let categories: Vec<(String, String)> = client
                    .guild(guild_id)
                    .map(|g| g.guild.channels.iter().filter(|c| c.is_category()).map(|c| (c.id.clone(), c.name.clone())).collect())
                    .unwrap_or_default();
                let chip = |id: SharedString, label: String, active: bool| {
                    div().id(id).px_2().py_1().rounded(radius::MD).border_1()
                        .border_color(if active { t.accent } else { t.border_strong })
                        .text_size(text::SM).cursor_pointer().child(label)
                };
                let kinds = div().flex().gap_1().children(CHANNEL_KINDS.iter().enumerate().map(|(i, (_, label))| {
                    chip(SharedString::from(format!("kind-{i}")), (*label).into(), *kind == i).on_click(cx.listener(move |v, _, _, cx| {
                        if let Some(ShellModal::CreateChannel { kind, .. }) = v.modal.as_mut() { *kind = i; }
                        cx.notify();
                    }))
                }));
                let mut cats = div().flex().flex_wrap().gap_1().child(
                    chip("cat-none".into(), "No category".into(), category.is_none()).on_click(cx.listener(|v, _, _, cx| {
                        if let Some(ShellModal::CreateChannel { category, .. }) = v.modal.as_mut() { *category = None; }
                        cx.notify();
                    })),
                );
                for (id, label) in categories {
                    let active = category.as_deref() == Some(id.as_str());
                    cats = cats.child(chip(SharedString::from(format!("cat-{id}")), label, active).on_click(cx.listener(move |v, _, _, cx| {
                        if let Some(ShellModal::CreateChannel { category, .. }) = v.modal.as_mut() { *category = Some(id.clone()); }
                        cx.notify();
                    })));
                }
                Modal::new("create-channel")
                    .title("Create channel")
                    .width(px(440.))
                    .on_dismiss(dismiss)
                    .child(div().flex().flex_col().gap_1p5().child(div().text_size(text::SM).text_color(t.text_muted).child("Type")).child(kinds))
                    .child(div().flex().flex_col().gap_1p5().child(div().text_size(text::SM).text_color(t.text_muted).child("Name")).child(name.clone()))
                    .child(div().flex().flex_col().gap_1p5().child(div().text_size(text::SM).text_color(t.text_muted).child("Category")).child(cats))
                    .footer(cancel)
                    .footer(Button::new("modal-ok", "Create").primary().on_click(cx.listener(|v, _, window, cx| v.submit_modal(window, cx))))
            }
        };
        Some(element.into_any_element())
    }

    // ---------------------------------------------------------------- profile card

    fn show_profile(&mut self, user_id: String, at: Point<Pixels>, cx: &mut Context<Self>) {
        // The press that closed this person's card (outside it, on their name) opens nothing.
        if self.card_closed.take().is_some_and(|(u, at)| u == user_id && at.elapsed() < std::time::Duration::from_millis(400)) {
            return;
        }
        let guild = self.client.read(cx).selected_guild.clone();
        let uid = user_id.clone();
        self.client.update(cx, |c, cx| {
            c.profile(&uid, cx);
        });
        self.profile_card = Some((user_id, guild, at));
        // Activity clocks tick once a second, only while a card is open.
        let clock = self.card_clock.clone();
        self.card_ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                clock.update(cx, |_, cx| cx.notify());
                let open = this.read_with(cx, |v, _| v.profile_card.is_some()).unwrap_or(false);
                if !open {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn render_profile_card(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (user_id, guild_id, at) = self.profile_card.clone()?;
        // Read so that its ticks draw the shell again (the shell itself is not cached).
        self.card_clock.read(cx);
        let t = cx.theme().clone();
        let profile = self.client.update(cx, |c, cx| c.profile(&user_id, cx));
        // What they are doing, from the guild's member list or the friends list.
        let activities = {
            let client = self.client.read(cx);
            guild_id
                .as_deref()
                .and_then(|g| client.member(g, &user_id))
                .map(|m| m.activities())
                .filter(|a| !a.is_empty())
                .or_else(|| client.friends.iter().find(|f| f.user_id == user_id).map(|f| f.activities()))
                .unwrap_or_default()
        };
        let now_ms = rhein_store::client::now_ms().max(0) as u64;
        let activity_cards: Vec<AnyElement> = activities
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let assets = a.assets.clone().unwrap_or_default();
                let (large, small) = self.client.update(cx, |c, cx| {
                    (
                        assets.large_image.as_deref().and_then(|u| c.artwork_url(u, cx)),
                        assets.small_image.as_deref().and_then(|u| c.artwork_url(u, cx)),
                    )
                });
                activity::card(&format!("card-activity-{i}"), a, large, small, now_ms, &t, window)
            })
            .collect();
        let client = self.client.read(cx);
        let name = client.display_name(guild_id.as_deref(), &user_id);
        let avatar = client.avatar_url(guild_id.as_deref(), &user_id);
        let member = guild_id.as_deref().and_then(|g| client.member(g, &user_id)).cloned();
        let roles: Vec<String> = match (&member, guild_id.as_deref().and_then(|g| client.guild(g))) {
            (Some(m), Some(g)) => g.guild.roles.iter().filter(|r| m.roles.contains(&r.id)).map(|r| r.name.clone()).collect(),
            _ => Vec::new(),
        };
        let presence = member.as_ref().map(|m| rhein_ui::Presence::parse(m.status()));
        let is_bot = member.as_ref().is_some_and(|m| m.is_bot);
        let myself = client.user_id() == Some(user_id.as_str());
        let blocked = client.friend_state(&user_id) == Some("blocked");
        let mutual_guilds = if myself { Vec::new() } else { client.mutual_guilds(&user_id) };
        let link = client.profile_link(&user_id);
        let look = profile::Look {
            banner: profile.as_ref().and_then(|p| p.banner_ref.as_deref()).and_then(|r| client.banner_url(&user_id, r)),
            accent: profile.as_ref().and_then(|p| p.accent_color),
            theme: profile.as_ref().and_then(|p| p.theme_colors),
        };
        let face = Avatar::new(user_id.clone(), name.clone())
            .size(px(profile::FACE_SIZE))
            .image(avatar.map(Into::into))
            .presence(presence)
            .into_any_element();
        let body = div()
            .px_4()
            .pb_4()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div().flex().flex_col()
                    .child(div().flex().items_center().gap_2().child(div().text_size(text::LG).font_weight(FontWeight::SEMIBOLD).child(name)).when(is_bot, |d| d.child(Tag("BOT"))))
                    // The address: a click copies it.
                    .child({
                        let (id, c) = (user_id.clone(), self.client.clone());
                        div()
                            .id("profile-address")
                            .text_size(text::SM)
                            .text_color(t.text_faint)
                            .cursor_pointer()
                            .hover(|s| s.text_color(t.text_muted))
                            .tooltip(tooltip("Copy"))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(id.clone()));
                                let text = format!("Copied {id}");
                                c.update(cx, |c, cx| c.flash_notice(text, cx));
                            })
                            .child(user_id.clone())
                    })
                    .children(profile.as_ref().and_then(|p| p.pronouns.clone()).filter(|p| !p.is_empty()).map(|p| div().text_size(text::SM).text_color(t.text_muted).child(p))),
            )
            .children(profile.as_ref().and_then(|p| p.bio.clone()).filter(|b| !b.is_empty()).map(|bio| {
                div().flex().flex_col().gap_1()
                    .child(div().font_family(FONT_MONO).text_size(text::XS).text_color(t.text_faint).child("about me"))
                    .child(div().text_size(text::SM).child(bio))
            }))
            .children({
                let me = client.user_id().map(str::to_owned);
                let state = client.friend_state(&user_id).map(str::to_owned);
                (me.as_deref() != Some(user_id.as_str())).then(|| {
                    let (c, u) = (self.client.clone(), user_id.clone());
                    let label = match state.as_deref() {
                        Some("accepted") => "Friends",
                        Some("pending_out") => "Request sent",
                        Some("pending_in") => "Accept friend request",
                        Some("blocked") => "Blocked",
                        _ => "Add friend",
                    };
                    let action = match state.as_deref() {
                        None => Some(rhein_store::FriendAction::Request),
                        Some("pending_in") => Some(rhein_store::FriendAction::Accept),
                        _ => None,
                    };
                    let (c2, u2, v) = (self.client.clone(), user_id.clone(), cx.weak_entity());
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .child(
                            Button::new("profile-message", "Message")
                                .primary()
                                .small()
                                .icon(Icon::MessageSquare)
                                .on_click(move |_, _, cx| {
                                    let u = u2.clone();
                                    v.update(cx, |v, cx| {
                                        v.profile_card = None;
                                        cx.notify();
                                    })
                                    .ok();
                                    c2.update(cx, |c, cx| c.message_user(u, cx));
                                }),
                        )
                        .child({
                            let (c3, u3, v3) = (self.client.clone(), user_id.clone(), cx.weak_entity());
                            IconButton::new("profile-call", Icon::Phone).tooltip("Call").on_click(move |_, _, cx| {
                                let u = u3.clone();
                                v3.update(cx, |v, cx| {
                                    v.profile_card = None;
                                    cx.notify();
                                })
                                .ok();
                                c3.update(cx, |c, cx| c.call_user(u, cx));
                            })
                        })
                        .child(
                            IconButton::new(
                                "profile-friend",
                                match state.as_deref() {
                                    Some("accepted") => Icon::UserCheck,
                                    Some("pending_out") => Icon::Clock,
                                    Some("pending_in") => Icon::Check,
                                    Some("blocked") => Icon::Ban,
                                    _ => Icon::UserPlus,
                                },
                            )
                                .tooltip(label)
                                .active(matches!(state.as_deref(), Some("accepted") | Some("pending_in")))
                                // Only sending or accepting a request does anything here.
                                .disabled(action.is_none())
                                .on_click(move |_, _, cx| {
                                    if let Some(a) = action {
                                        let u = u.clone();
                                        c.update(cx, |c, cx| c.friend_action(a, u, cx));
                                    }
                                }),
                        )
                })
            })
            .when(!roles.is_empty(), |d| {
                d.child(div().flex().flex_col().gap_1()
                    .child(div().font_family(FONT_MONO).text_size(text::XS).text_color(t.text_faint).child("roles"))
                    .child(div().flex().flex_wrap().gap_1().children(roles.into_iter().map(|r| {
                        div().px_2().py_0p5().rounded(radius::SM).bg(t.bg_raised).text_size(text::XS).child(r)
                    }))))
            });
        let mutual_friends = if myself { None } else { self.client.update(cx, |c, cx| c.mutual_friends(&user_id, cx)) };
        let mutual_names: Vec<String> = {
            let client = self.client.read(cx);
            mutual_friends.clone().unwrap_or_default().iter().map(|u| client.display_name(None, u)).collect()
        };
        self.client.update(cx, |c, cx| {
            for u in mutual_friends.clone().unwrap_or_default().iter().take(3) {
                c.profile(u, cx);
            }
        });
        let mutual_faces: Vec<(String, String, Option<String>)> = {
            let client = self.client.read(cx);
            mutual_friends.clone().unwrap_or_default().iter().take(3).map(|u| (u.clone(), client.display_name(None, u), client.person_avatar(u))).collect()
        };
        let body = body
            // As Discord shows it: a few faces, "10 Mutual Friends • 4 Mutual Servers"; a click
            // opens the whole profile on that list.
            .when(!mutual_guilds.is_empty() || !mutual_names.is_empty(), |d| {
                let (nf, ns) = (mutual_names.len(), mutual_guilds.len());
                let mut faces = div().flex().flex_none();
                for (i, (id, name, url)) in mutual_faces.iter().enumerate() {
                    faces = faces.child(
                        div().when(i > 0, |d| d.ml(px(-6.))).rounded_full().border_2().border_color(t.bg_panel).child(Avatar::new(id.clone(), name.clone()).size(px(18.)).image(url.clone().map(Into::into))),
                    );
                }
                let part = |label: String, tab: usize, cx: &mut Context<Self>| {
                    let (v, u, g) = (cx.weak_entity(), user_id.clone(), guild_id.clone());
                    div()
                        .id(SharedString::from(format!("mutual-{tab}")))
                        .cursor_pointer()
                        .hover(|s| s.text_color(t.text).underline())
                        .on_click(move |_, _, cx| {
                            let (u, g) = (u.clone(), g.clone());
                            v.update(cx, |v, cx| v.open_full_profile(u, g, tab, cx)).ok();
                        })
                        .child(label)
                };
                let mut line = div().flex().items_center().gap_1p5().text_size(text::SM).text_color(t.text_muted).child(faces);
                if nf > 0 {
                    line = line.child(part(format!("{nf} Mutual {}", if nf == 1 { "Friend" } else { "Friends" }), 1, cx));
                }
                if nf > 0 && ns > 0 {
                    line = line.child(div().text_color(t.text_faint).child("•"));
                }
                if ns > 0 {
                    line = line.child(part(format!("{ns} Mutual {}", if ns == 1 { "Server" } else { "Servers" }), 2, cx));
                }
                d.child(line)
            })
            // Activities sit close together, one block.
            .when(!activity_cards.is_empty(), |d| d.child(div().flex().flex_col().gap_1p5().children(activity_cards)));
        let more = {
            let (v, uid, link) = (cx.weak_entity(), user_id.clone(), link.clone());
            div().absolute().top_2().right_2().rounded(radius::MD).bg(gpui::Hsla::from(t.bg_main).opacity(0.7)).child(
                IconButton::new("profile-more", Icon::Ellipsis).tooltip("More").on_click(move |e, window, cx| {
                    let uid = uid.clone();
                    let at = e.position();
                    let link = link.clone();
                    v.update(cx, |v, cx| {
                        if v.menu.toggle(&format!("profile-more-{uid}"), cx) {
                            return;
                        }
                        let mut entries = Vec::new();
                        let id = uid.clone();
                        entries.push(MenuEntry::item("Copy user ID", move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(id.clone()))).with_icon(Icon::Copy));
                        if let Some(link) = link {
                            entries.push(MenuEntry::item("Copy profile link", move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(link.clone()))).with_icon(Icon::Link));
                        }
                        if !myself {
                            entries.push(MenuEntry::Separator);
                            let (c, u) = (v.client.clone(), uid.clone());
                            if blocked {
                                entries.push(MenuEntry::item("Unblock", move |_, cx| {
                                    let u = u.clone();
                                    c.update(cx, |c, cx| c.friend_action(rhein_store::FriendAction::Unblock, u, cx));
                                }).with_icon(Icon::Ban));
                            } else {
                                let this = cx.weak_entity();
                                entries.push(MenuEntry::item("Block", move |window, cx| {
                                    let (c, u) = (c.clone(), u.clone());
                                    this.update(cx, |v, cx| {
                                        v.profile_card = None;
                                        v.confirm(
                                            "Block them?".into(),
                                            "They can't message you or send you a friend request, and a friendship ends.".into(),
                                            "Block".into(),
                                            true,
                                            move |cx| c.update(cx, |c, cx| c.friend_action(rhein_store::FriendAction::Block, u.clone(), cx)),
                                            window,
                                            cx,
                                        )
                                    })
                                    .ok();
                                }).with_icon(Icon::Ban).danger());
                            }
                        }
                        v.menu.show(at, entries, |v: &mut Shell| &mut v.menu, window, cx);
                    })
                    .ok();
                }),
            )
        };
        // Right beside the card, level with its top: the whole profile. Outside the card so it covers nothing.
        let expand = {
            let (v, u, g) = (cx.weak_entity(), user_id.clone(), guild_id.clone());
            div().flex().items_start().child(
                div().ml_1().rounded(radius::MD).bg(t.bg_panel).border_1().border_color(t.border_strong).shadow_lg().child(
                    IconButton::new("profile-expand", Icon::Maximize).tooltip("View full profile").on_click(move |_, _, cx| {
                        let (u, g) = (u.clone(), g.clone());
                        v.update(cx, |v, cx| v.open_full_profile(u, g, 0, cx)).ok();
                    }),
                ),
            )
        };
        let card = div()
            .flex()
            .items_start()
            .occlude()
            .on_mouse_down_out(cx.listener(|v, _, _, cx| {
                if let Some((user, _, _)) = v.profile_card.take() {
                    v.card_closed = Some((user, std::time::Instant::now()));
                }
                cx.notify();
            }))
            .child(
                profile::frame(&look, &t, div().flex().flex_col().child(profile::header(&look, face, &t)).child(body))
                    .relative()
                    .child(more)
                    .shadow_lg(),
            )
            .child(expand);
        // Over the whole window, behind the card: whatever is under it neither scrolls nor
        // answers the pointer while the card is open (a click outside only closes the card).
        let backdrop = div().id("profile-card-backdrop").absolute().inset_0().occlude().on_scroll(|_, _, cx| cx.stop_propagation());
        Some(
            deferred(backdrop.child(anchored().position(at).snap_to_window_with_margin(px(8.)).child(motion::pop_in(card, "profile-card-in"))))
                .with_priority(2)
                .into_any_element(),
        )
    }

    /// Opens someone's whole profile, on a tab: 0 activity, 1 mutual friends, 2 mutual servers.
    fn open_full_profile(&mut self, user_id: String, guild_id: Option<String>, tab: usize, cx: &mut Context<Self>) {
        self.profile_card = None;
        self.full_profile = Some((user_id, guild_id, tab));
        cx.notify();
    }

    /// The whole profile, as Discord shows it: the card on the left, what they are doing and
    /// what you share on the right.
    fn render_full_profile(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (user_id, guild_id, tab) = self.full_profile.clone()?;
        let t = cx.theme().clone();
        let profile = self.client.update(cx, |c, cx| c.profile(&user_id, cx));
        let mutual_friends = self.client.update(cx, |c, cx| c.mutual_friends(&user_id, cx)).unwrap_or_default();
        // Their pictures and names come with their profiles.
        self.client.update(cx, |c, cx| {
            for u in &mutual_friends {
                c.profile(u, cx);
            }
        });
        let activities = {
            let client = self.client.read(cx);
            guild_id
                .as_deref()
                .and_then(|g| client.member(g, &user_id))
                .map(|m| m.activities())
                .filter(|a| !a.is_empty())
                .or_else(|| client.friends.iter().find(|f| f.user_id == user_id).map(|f| f.activities()))
                .unwrap_or_default()
        };
        let now_ms = rhein_store::client::now_ms().max(0) as u64;
        let activity_cards: Vec<AnyElement> = activities
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let assets = a.assets.clone().unwrap_or_default();
                let (large, small) = self.client.update(cx, |c, cx| {
                    (assets.large_image.as_deref().and_then(|u| c.artwork_url(u, cx)), assets.small_image.as_deref().and_then(|u| c.artwork_url(u, cx)))
                });
                activity::card(&format!("full-activity-{i}"), a, large, small, now_ms, &t, window)
            })
            .collect();
        let client = self.client.read(cx);
        let name = client.display_name(guild_id.as_deref(), &user_id);
        let avatar = client.avatar_url(guild_id.as_deref(), &user_id);
        let member = guild_id.as_deref().and_then(|g| client.member(g, &user_id)).cloned();
        let is_bot = member.as_ref().is_some_and(|m| m.is_bot);
        let roles: Vec<(String, Option<u32>)> = match (&member, guild_id.as_deref().and_then(|g| client.guild(g))) {
            (Some(m), Some(g)) => g.guild.roles.iter().filter(|r| m.roles.contains(&r.id)).map(|r| (r.name.clone(), r.rgb())).collect(),
            _ => Vec::new(),
        };
        let friend_since = client.friends.iter().find(|f| f.user_id == user_id && f.state == "accepted").and_then(|f| f.updated_at.or(f.created_at));
        let mutual_guilds = client.mutual_guilds(&user_id);
        let friends: Vec<(String, String, Option<String>)> =
            mutual_friends.iter().map(|u| (u.clone(), client.display_name(None, u), client.person_avatar(u))).collect();
        let guild_faces: Vec<(String, String, Option<String>)> =
            mutual_guilds.iter().map(|(id, n)| (id.clone(), n.clone(), client.guild_icon_url(id))).collect();
        let look = profile::Look {
            banner: profile.as_ref().and_then(|p| p.banner_ref.as_deref()).and_then(|r| client.banner_url(&user_id, r)),
            accent: profile.as_ref().and_then(|p| p.accent_color),
            theme: profile.as_ref().and_then(|p| p.theme_colors),
        };
        let presence = member.as_ref().map(|m| rhein_ui::Presence::parse(m.status()));
        let face = Avatar::new(user_id.clone(), name.clone()).size(px(profile::FACE_SIZE)).image(avatar.map(Into::into)).presence(presence).into_any_element();
        let date = |ms: i64| {
            jiff::Timestamp::from_millisecond(ms).ok().map(|ts| ts.to_zoned(jiff::tz::TimeZone::system()).strftime("%-d %b %Y").to_string()).unwrap_or_default()
        };
        let small_heading = |label: &'static str| div().font_family(FONT_MONO).text_size(text::XS).text_color(t.text_faint).child(label);

        // Left: the card, larger.
        let left = div()
            .w(px(360.))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .child(profile::frame(
                &look,
                &t,
                div().h_full().flex().flex_col().child(profile::header(&look, face, &t)).child(
                    div()
                        .flex_1()
                        .px_5()
                        .pb_5()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .child(div().flex().items_center().gap_2().child(div().text_size(text::XL).font_weight(FontWeight::SEMIBOLD).child(name.clone())).when(is_bot, |d| d.child(Tag("BOT"))))
                                // The address: a click copies it, as on the card.
                                .child({
                                    let (id, c) = (user_id.clone(), self.client.clone());
                                    div()
                                        .id("full-profile-address")
                                        .text_size(text::SM)
                                        .text_color(t.text_faint)
                                        .cursor_pointer()
                                        .hover(|s| s.text_color(t.text_muted))
                                        .tooltip(tooltip("Copy"))
                                        .on_click(move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(id.clone()));
                                            let text = format!("Copied {id}");
                                            c.update(cx, |c, cx| c.flash_notice(text, cx));
                                        })
                                        .child(user_id.clone())
                                })
                                .children(profile.as_ref().and_then(|p| p.pronouns.clone()).filter(|p| !p.is_empty()).map(|p| div().text_size(text::SM).text_color(t.text_muted).child(p))),
                        )
                        .children(profile.as_ref().and_then(|p| p.bio.clone()).filter(|b| !b.is_empty()).map(|bio| {
                            div().flex().flex_col().gap_1().child(small_heading("about me")).child(div().text_size(text::SM).child(bio))
                        }))
                        // The dates and roles at the bottom of the card, the space above them.
                        .child(div().flex_1())
                        .children(member.as_ref().and_then(|m| m.joined_at).map(|at| div().flex().flex_col().gap_1().child(small_heading("member since")).child(div().text_size(text::SM).child(date(at)))))
                        .children(friend_since.map(|at| div().flex().flex_col().gap_1().child(small_heading("friends since")).child(div().text_size(text::SM).child(date(at)))))
                        .when(!roles.is_empty(), |d| {
                            d.child(div().flex().flex_col().gap_1().child(small_heading("roles")).child(div().flex().flex_wrap().gap_1().children(roles.into_iter().map(|(r, colour)| {
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .rounded(radius::SM)
                                    .bg(t.bg_raised)
                                    .text_size(text::XS)
                                    .child(div().size(px(8.)).rounded_full().bg(colour.map(|c| gpui::rgb(c).into()).unwrap_or(gpui::Hsla::from(t.text_faint))))
                                    .child(r)
                            }))))
                        }),
                ),
            ).w_full().flex_1());

        // Right: tabs.
        let myself = self.client.read(cx).user_id() == Some(user_id.as_str());
        let tab = if myself { 0 } else { tab };
        let mut labels = vec![
            "Activity".to_owned(),
            format!("{} Mutual {}", friends.len(), if friends.len() == 1 { "Friend" } else { "Friends" }),
            format!("{} Mutual {}", guild_faces.len(), if guild_faces.len() == 1 { "Server" } else { "Servers" }),
        ];
        // Nothing is mutual with yourself.
        if myself {
            labels.truncate(1);
        }
        let this = cx.weak_entity();
        let tabs = Tabs::new("full-profile-tabs", labels).selected(tab).on_select(move |i, _, cx| {
            this.update(cx, |v, cx| {
                if let Some(fp) = v.full_profile.as_mut() {
                    fp.2 = i;
                }
                cx.notify();
            })
            .ok();
        });
        let row = |id: String, label: String, face: AnyElement, cx: &mut Context<Self>, action: RowAction| {
            div()
                .id(SharedString::from(format!("full-row-{id}")))
                .px_2()
                .h(px(44.))
                .flex()
                .items_center()
                .gap_3()
                .rounded(radius::MD)
                .cursor_pointer()
                .hover(|s| s.bg(t.bg_hover))
                .on_click(cx.listener(move |v, _, _, cx| action(v, cx)))
                .child(face)
                .child(div().flex_1().min_w_0().truncate().child(label))
        };
        let content: AnyElement = match tab {
            1 if friends.is_empty() => div().text_color(t.text_faint).child("No friends in common.").into_any_element(),
            1 => {
                let mut list = div().flex().flex_col();
                for (id, n, url) in friends {
                    let target = id.clone();
                    let g = guild_id.clone();
                    list = list.child(row(id.clone(), n.clone(), Avatar::new(id, n).size(px(32.)).image(url.map(Into::into)).into_any_element(), cx, Box::new(move |v, cx| v.open_full_profile(target.clone(), g.clone(), 0, cx))));
                }
                list.into_any_element()
            }
            2 if guild_faces.is_empty() => div().text_color(t.text_faint).child("No servers in common.").into_any_element(),
            2 => {
                let mut list = div().flex().flex_col();
                for (id, n, url) in guild_faces {
                    let target = id.clone();
                    list = list.child(row(
                        id,
                        n.clone(),
                        crate::views::guild_settings::guild_face(&n, url, 32., &t, window),
                        cx,
                        Box::new(move |v, cx| {
                            v.full_profile = None;
                            let g = target.clone();
                            v.client.update(cx, |c, cx| c.select_guild(g, cx));
                        }),
                    ));
                }
                list.into_any_element()
            }
            _ if activity_cards.is_empty() => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(t.text_faint)
                .child(format!("{name} isn't sharing any activity right now."))
                .into_any_element(),
            _ => div().flex().flex_col().gap_4().children(activity_cards).into_any_element(),
        };
        let right = div().flex_1().min_w_0().flex().flex_col().gap_4().child(tabs).child(div().id("full-profile-list").flex_1().overflow_y_scroll().child(content));

        let close = cx.listener(|v, _, _, cx| {
            v.full_profile = None;
            cx.notify();
        });
        let dialog = div()
            .id("full-profile")
            .relative()
            .w(px(920.))
            .h(px(620.))
            .p_6()
            .flex()
            .gap_6()
            .rounded(radius::LG)
            .bg(t.bg_panel)
            .border_1()
            .border_color(t.border_strong)
            .shadow_lg()
            .occlude()
            .on_mouse_down_out(close)
            .child(left)
            .child(right)
            .child(
                div().absolute().top_3().right_3().child(IconButton::new("full-profile-close", Icon::X).tooltip("Close").on_click(cx.listener(|v, _, _, cx| {
                    v.full_profile = None;
                    cx.notify();
                }))),
            );
        Some(
            deferred(
                div()
                    .id("full-profile-backdrop")
                    .absolute()
                    .inset_0()
                    .bg(t.overlay)
                    .occlude()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(motion::pop_in(dialog, "full-profile-in")),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    // ---------------------------------------------------------------- settings & switcher

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_some() {
            return;
        }
        let client = self.client.clone();
        let view = cx.new(|cx| SettingsView::new(client, window, cx));
        let sub = cx.subscribe_in(&view, window, |this, _, event: &SettingsEvent, window, cx| match event {
            SettingsEvent::Close => this.close_settings(window, cx),
            SettingsEvent::SignOut => {
                this.close_settings(window, cx);
                this.ask_sign_out(window, cx);
            }
        });
        self.settings = Some((view, sub));
        cx.notify();
    }

    /// A guild's settings over everything, on one of its pages.
    pub(crate) fn open_guild_settings(&mut self, guild_id: String, pane: GuildPane, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let view = cx.new(|cx| GuildSettingsView::new(client, guild_id, pane, window, cx));
        let sub = cx.subscribe_in(&view, window, |this, _, event: &GuildSettingsEvent, window, cx| match event {
            GuildSettingsEvent::Close => this.close_guild_settings(window, cx),
        });
        self.guild_settings = Some((view, sub));
        cx.notify();
    }

    fn close_guild_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.guild_settings = None;
        let focus = self.chat.read(cx).composer_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings = None;
        self.client.update(cx, |c, cx| c.stop_mic_test(cx));
        let focus = self.chat.read(cx).composer_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn open_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let view = cx.new(|cx| QuickSwitcher::new(client, window, cx));
        let sub = cx.subscribe_in(&view, window, |this, _, _: &DismissEvent, window, cx| {
            this.switcher = None;
            let focus = this.chat.read(cx).composer_focus(cx);
            window.focus(&focus, cx);
            cx.notify();
        });
        self.switcher = Some((view, sub));
        cx.notify();
    }

    /// Whether a guild's channels are showing in the sidebar tree: the open guild unless folded
    /// away, any other guild once unfolded.
    fn guild_expanded(&self, gid: &str, cx: &App) -> bool {
        let client = self.client.read(cx);
        let open = !client.showing_home && client.selected_guild.as_deref() == Some(gid);
        !self.folded_guilds.contains(gid) && (open || self.unfolded_guilds.contains(gid))
    }

    /// Opening another guild, however (its name, one of its text or voice channels, the
    /// switcher), leaves the one open before unfolded: only its arrow folds a guild.
    fn keep_previous_guild_unfolded(&mut self, cx: &App) {
        let client = self.client.read(cx);
        let open = if client.showing_home { None } else { client.selected_guild.clone() };
        if open == self.open_guild {
            return;
        }
        if let Some(previous) = self.open_guild.take()
            && !self.folded_guilds.contains(&previous)
        {
            self.unfolded_guilds.insert(previous);
        }
        self.open_guild = open;
    }

    fn toggle_guild(&mut self, gid: &str, cx: &mut Context<Self>) {
        if self.guild_expanded(gid, cx) {
            self.unfolded_guilds.remove(gid);
            self.folded_guilds.insert(gid.to_owned());
        } else {
            self.folded_guilds.remove(gid);
            self.unfolded_guilds.insert(gid.to_owned());
        }
        cx.notify();
    }

    /// The one sidebar: direct messages on top, then every server with its guilds, each guild
    /// unfolding into its channels.
    fn sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let menu_open = self.menu.is_open();
        let t = cx.theme().clone();
        let client = self.client.read(cx);
        let home = client.showing_home;
        let selected = client.selected_guild.clone();

        let mut tree = div().id("sidebar-tree").flex_1().overflow_y_scroll().track_scroll(&self.tree_scroll).px_2().pb_2().flex().flex_col();

        // Direct messages: the row sits in a strip as tall as the chat header, so both share one
        // line underneath; its conversations open below it.
        let mut top = div()
            .h(size::HEADER)
            .flex_none()
            .px_2()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(t.border);
        {
            let requests = client.unseen_friend_requests() as u32;
            let dm_unread = client.conversations.iter().any(|c| client.conversation_unread(&c.id));
            let c = self.client.clone();
            top = top.child(div().flex_1().child(
                ListRow::new("tree-home", "Direct messages")
                    .icon(Icon::MessagesSquare)
                    // Friends and conversations below carry the selection; this row just marks home.
                    .strong(dm_unread || home)
                    .when(requests > 0, |r| r.trailing(CountBadge(requests)))
                    .on_click(move |_, _, cx| c.update(cx, |c, cx| c.show_home(cx))),
            ));
            if home {
                tree = tree.child(div().h_2());
                for row in self.conversation_rows(cx) {
                    tree = tree.child(row);
                }
            }
        }

        // Servers and their guilds.
        for server in &client.servers {
            let dot = match server.status {
                ServerStatus::Ready => t.success,
                ServerStatus::Connecting | ServerStatus::Reconnecting | ServerStatus::Renewing => t.warning,
                ServerStatus::Incompatible | ServerStatus::Failed => t.danger,
            };
            tree = tree.child(
                div()
                    .px_2()
                    .pt_4()
                    .pb_1()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .font_family(FONT_MONO)
                    .text_size(text::XS)
                    .text_color(t.text_faint)
                    .when(client.prefs.server_dots, |d| d.child(div().size(px(6.)).flex_none().rounded_full().bg(dot)))
                    .child(div().flex_1().min_w_0().truncate().child(server.name.clone()))
                    // A server that can't be reached can be taken off the list.
                    .when(matches!(server.status, ServerStatus::Failed | ServerStatus::Incompatible), |d| {
                        let (v, name) = (cx.weak_entity(), server.name.clone());
                        d.child(IconButton::new(SharedString::from(format!("server-remove-{name}")), Icon::X).tooltip("Remove this server").on_click(move |_, window, cx| {
                            let name = name.clone();
                            v.update(cx, |v, cx| {
                                let client = v.client.clone();
                                v.confirm(
                                    format!("Remove {name}?"),
                                    "Its guilds leave the sidebar. Add it again in Settings › Servers.".into(),
                                    "Remove".into(),
                                    true,
                                    move |cx| client.update(cx, |c, cx| c.remove_server(&name, cx)),
                                    window,
                                    cx,
                                )
                            })
                            .ok();
                        }))
                    }),
            );
            if server.status != ServerStatus::Ready {
                tree = tree.child(
                    div()
                        .px_2()
                        .pb_1()
                        .font_family(FONT_MONO)
                        .text_size(text::XS)
                        .text_color(if matches!(server.status, ServerStatus::Failed | ServerStatus::Incompatible) { t.danger } else { t.warning })
                        .child(server.status.label().to_lowercase())
                        .children(server.error.clone().map(|e| div().text_color(t.danger).child(e)))
                        // Protected, and the relay failed: offer the direct way, this once.
                        .when(client.relay_failed.contains(&server.input) && server.status == ServerStatus::Failed, |d| {
                            let (c, key) = (self.client.clone(), server.input.clone());
                            d.child(div().pt_1().child(Button::new(SharedString::from(format!("direct-{key}")), "Connect directly this time").small().on_click(move |_, _, cx| {
                                c.update(cx, |c, cx| c.connect_directly(&key, cx));
                            })))
                        }),
                );
            }
            for entry in client.guilds.iter().filter(|g| g.server == server.name) {
                let gid = entry.guild.id.clone();
                let active = !home && selected.as_deref() == Some(gid.as_str());
                let expanded = self.guild_expanded(&gid, cx);
                let (unread, mentions) = client.guild_unread(&gid);
                let (select_id, chevron_id, menu_id, more_id) = (gid.clone(), gid.clone(), gid.clone(), gid.clone());
                tree = tree.child(
                    div()
                        .id(SharedString::from(format!("tree-guild-{gid}")))
                        .group("guild-row")
                        .h(px(30.))
                        .pl_1()
                        .pr_1()
                        .rounded(radius::MD)
                        .flex()
                        .items_center()
                        .gap_1()
                        .cursor_pointer()
                        .text_color(if active || unread { t.text } else { t.text_muted })
                        .when(unread, |d| d.font_weight(FontWeight::SEMIBOLD))
                        .when(active && !expanded, |d| d.bg(t.bg_selected))
                        .hover(|s| s.bg(t.bg_hover).text_color(t.text))
                        // Not while a menu is open: it would sit on the menu's first entry.
                        .when(!menu_open, |d| d.tooltip(tooltip(format!("{} · {}", entry.guild.name, entry.server))))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let open = {
                                let c = this.client.read(cx);
                                !c.showing_home && c.selected_guild.as_deref() == Some(select_id.as_str())
                            };
                            if open {
                                this.toggle_guild(&select_id, cx);
                            } else {
                                // The guild open before stays unfolded (see
                                // `keep_previous_guild_unfolded`).
                                this.folded_guilds.remove(&select_id);
                                let id = select_id.clone();
                                this.client.update(cx, |c, cx| c.select_guild(id, cx));
                            }
                        }))
                        .on_mouse_down(MouseButton::Right, cx.listener(move |this, e: &gpui::MouseDownEvent, window, cx| {
                            this.show_guild_menu(menu_id.clone(), e.position, window, cx)
                        }))
                        .child(
                            div()
                                .id(SharedString::from(format!("tree-chevron-{gid}")))
                                .size(px(20.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(radius::SM)
                                .text_color(t.text_faint)
                                .hover(|s| s.text_color(t.text))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.toggle_guild(&chevron_id, cx);
                                }))
                                .child(icon_sized(if expanded { Icon::ChevronDown } else { Icon::ChevronRight }, px(12.)).text_color(t.text_faint)),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(entry.guild.name.clone()))
                        .when(mentions > 0, |d| d.child(CountBadge(mentions)))
                        // The unread dot and the menu button share the row's end: the dot until
                        // the pointer is on the row, then the button in its place.
                        .child(
                            div()
                                .relative()
                                .size(px(20.))
                                .flex_none()
                                .when(unread && mentions == 0 && !expanded, |d| {
                                    d.child(
                                        div()
                                            .absolute()
                                            .inset_0()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .group_hover("guild-row", |s| s.invisible())
                                            .child(UnreadDot),
                                    )
                                })
                                .child(
                            div()
                                .id(SharedString::from(format!("tree-more-{gid}")))
                                .absolute()
                                .inset_0()
                                .invisible()
                                .group_hover("guild-row", |s| s.visible())
                                .size(px(20.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(radius::SM)
                                .text_color(t.text_faint)
                                .hover(|s| s.text_color(t.text).bg(t.bg_selected))
                                .on_click(cx.listener(move |this, e: &gpui::ClickEvent, window, cx| {
                                    cx.stop_propagation();
                                    if !this.menu.toggle(&format!("guild-more-{more_id}"), cx) {
                                        this.show_guild_menu(more_id.clone(), e.position(), window, cx)
                                    }
                                }))
                                .child(icon_sized(Icon::Ellipsis, px(14.)).text_color(t.text_muted)),
                                ),
                        ),
                );
                if expanded {
                    tree = tree.child(div().pl_3().flex().flex_col().children(self.channel_rows(&gid, cx)));
                }
            }
        }

        tree = tree.child(
            div().mt_2().pt_2().border_t_1().border_color(t.border).child(
                ListRow::new("tree-add", "Join or create")
                    .icon(Icon::Plus)
                    .muted(true)
                    .on_click(cx.listener(|this, _, window, cx| this.open_dialog(DialogTab::Join, window, cx))),
            ),
        );

        div()
            .w(size::SIDEBAR)
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(t.bg_panel)
            .border_r_1()
            .border_color(t.border)
            .child(top)
            .child(rhein_ui::smooth_scroll("sidebar-glide", rhein_ui::ScrollTarget::Handle(self.tree_scroll.clone()), tree))
            .children(self.call_bar(cx))
            .child(self.profile_strip(cx))
            .into_any_element()
    }

    fn push_to_talk_live(&self, cx: &Context<Self>) -> bool {
        let client = self.client.read(cx);
        client.call().is_some() && client.voice_prefs().push_to_talk
    }

    /// While in a call: how it is going, where it is, and the way out.
    /// Where the call is, as one line; a click goes back to it (its controls are there).
    fn call_bar(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let t = cx.theme().clone();
        let client = self.client.read(cx);
        let call = client.call()?;
        let (label, color) = match &call.status {
            CallStatus::Connecting => ("Connecting…".to_owned(), t.warning),
            CallStatus::Connected => ("Voice connected".to_owned(), t.success),
            CallStatus::Failed(_) => ("Call failed".to_owned(), t.danger),
        };
        let place = if call.conversation {
            client.conversations.iter().find(|c| c.id == call.room_id).map(|c| client.conversation_title(c)).unwrap_or_default()
        } else {
            let channel = client.channel(&call.channel_id).map(|c| c.name.clone()).unwrap_or_default();
            let guild = client.guild(&call.room_id).map(|g| g.guild.name.clone()).unwrap_or_default();
            format!("{channel} / {guild}")
        };
        let detail = match (&call.status, &call.mode) {
            (CallStatus::Failed(err), _) => err.clone(),
            (_, Some(CallMode::Sfu { region })) if call.conversation => format!("{place} · SFU, encrypted{}", region.as_ref().map(|r| format!(" ({r})")).unwrap_or_default()),
            (_, Some(CallMode::Sfu { region })) => format!("{place} · SFU{}", region.as_ref().map(|r| format!(" {r}")).unwrap_or_default()),
            (_, Some(CallMode::Mesh)) => format!("{place} · peer to peer"),
            _ => place,
        };
        let (open, conversation) = (call.channel_id.clone(), call.conversation);
        let open_client = self.client.clone();
        Some(
            div()
                .flex_none()
                .px_2()
                .py_1p5()
                .flex()
                .items_center()
                .gap_1()
                .border_t_1()
                .border_color(t.border)
                .child(
                    div()
                        .id("call-bar")
                        .flex_1()
                        .min_w_0()
                        .px_1()
                        .rounded(radius::SM)
                        .cursor_pointer()
                        .hover(|d| d.bg(t.bg_hover))
                        .on_click(move |_, _, cx| {
                            open_client.update(cx, |c, cx| {
                                if conversation {
                                    c.show_home(cx);
                                    c.open_conversation(Some(open.clone()), cx);
                                } else {
                                    c.select_channel(open.clone(), cx);
                                }
                            })
                        })
                        .child(div().flex().items_center().gap_1p5().text_size(text::SM).font_weight(FontWeight::SEMIBOLD).text_color(color).child(icon_sized(Icon::Volume, px(14.)).text_color(color)).child(label))
                        .child(div().text_size(text::XS).text_color(t.text_faint).truncate().child(detail)),
                )
                .into_any_element(),
        )
    }

    /// You, at the foot of the sidebar: who you are on top (a click opens the account menu), what
    /// you are sharing below, and the way to settings.
    fn profile_strip(&self, cx: &Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let client = self.client.read(cx);
        let me = client.user_id().unwrap_or_default().to_owned();
        let name = client.display_name(None, &me);
        let avatar = client.avatar_url(None, &me);
        let prefs = &client.prefs;
        let presence = presence_of(client.effective_status());
        let status = prefs.custom_status.clone().unwrap_or_else(|| match client.effective_status() {
            "idle" => "idle".into(),
            "dnd" => "do not disturb".into(),
            "invisible" => "invisible".into(),
            _ => "online".into(),
        });
        let activity = client.shared_activities().first().cloned();

        let who = div()
            .id("profile-strip")
            .flex_1()
            .min_w_0()
            .h(px(40.))
            .px_1()
            .rounded(radius::MD)
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .hover(|s| s.bg(t.bg_hover))
            // A second click closes it: the press already has (as a click outside the menu), so
            // the menu is on its way out and is not opened again.
            .on_click(cx.listener(|this, _, window, cx| {
                if !this.menu.toggle("account", cx) {
                    this.account_menu(window, cx);
                }
            }))
            .child(Avatar::new(me.clone(), name.clone()).size(px(32.)).image(avatar.map(Into::into)).presence(Some(presence)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().text_size(text::MD).line_height(px(18.)).font_weight(FontWeight::SEMIBOLD).truncate().child(name))
                    .child(div().font_family(FONT_MONO).text_size(text::XS).line_height(px(15.)).text_color(t.text_faint).truncate().child(status)),
            );

        // Your own activity: its picture may load directly, since it came from this machine.
        let shared = activity.map(|a| {
            let glyph = match a.kind {
                2 => Icon::Music,
                1 | 3 => Icon::Monitor,
                _ => Icon::Gamepad,
            };
            let art = a.assets.as_ref().and_then(|x| x.large_image.clone());
            let what = match a.kind {
                2 | 3 => a.details.clone().unwrap_or_else(|| a.name.clone()),
                _ => a.name.clone(),
            };
            let detail = match a.kind {
                2 | 3 => a.state.clone(),
                _ => a.details.clone(),
            };
            div()
                .h(PROFILE_ROW_H)
                .flex_none()
                .px_3()
                .flex()
                .items_center()
                .gap_2p5()
                .border_t_1()
                .border_color(t.border)
                .child(
                    div()
                        .size(px(40.))
                        .flex_none()
                        .rounded(radius::MD)
                        .bg(t.bg_selected)
                        .overflow_hidden()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(match art {
                            Some(url) => gpui::img(rhein_ui::images::source(url)).size(px(40.)).object_fit(gpui::ObjectFit::Cover).into_any_element(),
                            None => icon_sized(glyph, px(18.)).text_color(t.text_faint).into_any_element(),
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .font_family(FONT_MONO)
                                .text_size(text::XS)
                                .line_height(px(15.))
                                .text_color(t.text_faint)
                                .truncate()
                                .child(format!("{} {}", a.verb().to_lowercase(), if matches!(a.kind, 2 | 3) { a.name.clone() } else { String::new() })),
                        )
                        .child(div().text_size(text::SM).line_height(px(17.)).font_weight(FontWeight::MEDIUM).truncate().child(what))
                        .children(detail.map(|d| div().text_size(text::XS).line_height(px(15.)).text_color(t.text_muted).truncate().child(d))),
                )
        });

        // Who you are sits in the bottom corner; an activity pushes it up and takes a box of the
        // same height beneath it.
        div()
            .flex_none()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(t.border)
            .child(
                div()
                    .h(PROFILE_ROW_H)
                    .flex_none()
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(who)
                    .child({
                        let (muted, c) = (self.client.read(cx).voice_muted(), self.client.clone());
                        IconButton::new("voice-mute", if muted { Icon::MicOff } else { Icon::Mic })
                            .danger(muted)
                            .tooltip(if muted { "Unmute" } else { "Mute" })
                            .on_click(move |_, _, cx| c.update(cx, |c, cx| c.set_voice_muted(!muted, cx)))
                    })
                    .child({
                        let (deafened, c) = (self.client.read(cx).voice_deafened(), self.client.clone());
                        IconButton::new("voice-deafen", if deafened { Icon::HeadphoneOff } else { Icon::Headphones })
                            .danger(deafened)
                            .tooltip(if deafened { "Undeafen" } else { "Deafen" })
                            .on_click(move |_, _, cx| c.update(cx, |c, cx| c.set_voice_deafened(!deafened, cx)))
                    })
                    .child(
                        IconButton::new("open-settings", Icon::Settings)
                            .tooltip("Settings (Ctrl+,)")
                            .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
                    ),
            )
            .children(shared)
            .into_any_element()
    }

    /// Friends and the private conversations, shown under "Direct messages".
    fn conversation_rows(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let t = cx.theme().clone();
        let client = self.client.read(cx);
        let mut rows = Vec::new();
        let pending = client.unseen_friend_requests();
        let c = self.client.clone();
        rows.push(
            ListRow::new("home-friends", "Friends")
                .icon(Icon::Users)
                .selected(client.home_conversation.is_none())
                .when(pending > 0, |r| r.trailing(CountBadge(pending as u32)))
                .on_click(move |_, _, cx| c.update(cx, |c, cx| c.open_conversation(None, cx)))
                .into_any_element(),
        );
        if client.conversations.is_empty() {
            rows.push(
                div().px_2().py_1().font_family(FONT_MONO).text_size(text::XS).text_color(t.text_faint).child("no conversations yet").into_any_element(),
            );
        }
        for conversation in &client.conversations {
            // A declined request stays out of the list, as the web client keeps it.
            if conversation.request_state.as_deref() == Some("declined") && conversation.creator.as_deref() != client.user_id() {
                continue;
            }
            let title = client.conversation_title(conversation);
            let me = client.user_id().unwrap_or_default();
            let other = conversation.members.iter().find(|m| m.user_id != me).map(|m| m.user_id.clone()).unwrap_or_default();
            let selected = client.home_conversation.as_deref() == Some(conversation.id.as_str());
            let unread = client.conversation_unread(&conversation.id);
            let request = conversation.request_state.as_deref() == Some("pending");
            let (c, id) = (self.client.clone(), conversation.id.clone());
            let leading = if conversation.kind == "gc" {
                div().size(px(20.)).rounded_full().bg(t.bg_selected).flex().items_center().justify_center().child(icon_sized(Icon::Users, px(12.))).into_any_element()
            } else {
                Avatar::new(other.clone(), title.clone()).size(px(20.)).into_any_element()
            };
            rows.push(
                ListRow::new(SharedString::from(format!("conv-{}", conversation.id)), title)
                    .leading(leading)
                    .selected(selected)
                    .strong(unread)
                    .when(request, |r| r.trailing(Tag("REQUEST")))
                    .on_click(move |_, _, cx| {
                        let id = id.clone();
                        c.update(cx, |c, cx| c.open_conversation(Some(id), cx));
                    })
                    .into_any_element(),
            );
        }
        rows.into_iter().map(|r| div().pl_3().child(r).into_any_element()).collect()
    }

    /// One guild's channels in order, with categories, voice occupants and unread state.
    fn channel_rows(&self, gid: &str, cx: &Context<Self>) -> Vec<AnyElement> {
        let t = cx.theme().clone();
        let client = self.client.read(cx);
        let Some(entry) = client.guild(gid) else { return Vec::new() };
        let selected_channel = if client.showing_home { None } else { client.selected_channel().map(str::to_owned) };
        let mut rows = Vec::new();
        for ch in entry.ordered_channels() {
            if ch.is_category() {
                let collapsed = self.collapsed.contains(&ch.id);
                let cat = ch.id.clone();
                rows.push(
                    div()
                        .id(SharedString::from(format!("cat-{}", ch.id)))
                        .flex()
                        .items_center()
                        .gap_1()
                        .pl_1()
                        .pt_3()
                        .pb_1()
                        .font_family(FONT_MONO)
                        .text_size(text::XS)
                        .text_color(t.text_faint)
                        .cursor_pointer()
                        .hover(|s| s.text_color(t.text_muted))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.collapsed.remove(&cat) {
                                this.collapsed.insert(cat.clone());
                            }
                            cx.notify();
                        }))
                        .on_mouse_down(gpui::MouseButton::Right, {
                            let id = ch.id.clone();
                            cx.listener(move |this, e: &gpui::MouseDownEvent, window, cx| this.channel_menu(id.clone(), e.position, window, cx))
                        })
                        .child(icon_sized(if collapsed { Icon::ChevronRight } else { Icon::ChevronDown }, px(12.)).text_color(t.text_faint))
                        .child(ch.name.to_lowercase())
                        .into_any_element(),
                );
                continue;
            }
            let active = selected_channel.as_deref() == Some(ch.id.as_str());
            let mark = client.read_mark_in(entry, ch);
            // A collapsed category still shows its open channel and anything with news.
            let hidden = ch.parent_id.as_ref().is_some_and(|p| self.collapsed.contains(p));
            if hidden && !active && !mark.is_unread() && mark.mentions == 0 {
                continue;
            }
            if client.prefs.hide_muted && mark.muted && !active && mark.mentions == 0 {
                continue;
            }
            let glyph = match ch.kind.as_str() {
                "voice" => Icon::Volume,
                "announcement" => Icon::Megaphone,
                "forum" => Icon::MessagesSquare,
                _ => Icon::Hash,
            };
            let mut row = ListRow::new(SharedString::from(format!("chan-{}", ch.id)), ch.name.clone())
                .icon(glyph)
                .selected(active)
                .strong(mark.is_unread())
                .muted(mark.muted && !active)
                .when(mark.mentions > 0, |r| r.trailing(CountBadge(mark.mentions)));
            if ch.is_forum() {
                let (id, client) = (ch.id.clone(), self.client.clone());
                row = row.on_click(move |_, _, cx| {
                    let id = id.clone();
                    client.update(cx, |c, cx| c.select_channel(id, cx));
                });
            }
            if ch.is_textual() || ch.is_voice() {
                let (id, voice) = (ch.id.clone(), ch.is_voice());
                let client = self.client.clone();
                // A click opens a voice channel's window; a double click joins its call.
                let name = ch.name.clone();
                row = row.on_click(move |e, _, cx| {
                    let id = id.clone();
                    let join = voice && e.click_count() >= 2;
                    let hint = voice && e.click_count() == 1;
                    let name = name.clone();
                    client.update(cx, |c, cx| {
                        c.select_channel(id.clone(), cx);
                        if join {
                            c.join_voice(id, cx);
                        } else if hint && !c.in_voice_channel(&id) {
                            // A single click only opens the channel: say how to join.
                            c.flash_notice(format!("Double-click {name} to join the call."), cx);
                        }
                    });
                });
            }
            let menu_id = ch.id.clone();
            row = row.on_secondary(cx.listener(move |this, e: &gpui::MouseDownEvent, window, cx| {
                this.channel_menu(menu_id.clone(), e.position, window, cx)
            }));
            // A little air above each channel; the people in a voice channel stay close to it.
            rows.push(div().pt(px(CHANNEL_GAP)).child(row).into_any_element());
            if ch.is_voice() {
                let members = client.voice_members(gid, &ch.id);
                let occupied = !members.is_empty();
                for state in members {
                    let name = client.display_name(Some(gid), &state.user_id);
                    let avatar = client.avatar_url(Some(gid), &state.user_id);
                    let muted = state.self_mute || state.server_mute;
                    let deaf = state.self_deaf || state.server_deaf;
                    rows.push(
                        div()
                            .pl(px(28.))
                            .pr_2()
                            .py(px(3.))
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_size(text::MD)
                            .text_color(t.text_muted)
                            .child(
                                div()
                                    .rounded_full()
                                    .border_2()
                                    .border_color(if client.call_speaking.read(cx).is_speaking(&state.user_id) { t.success } else { gpui::transparent_black().into() })
                                    .child(Avatar::new(state.user_id.clone(), name.clone()).size(px(24.)).image(avatar.map(Into::into))),
                            )
                            .child(div().flex_1().min_w_0().truncate().child(name))
                            .when(state.streaming, |d| d.child(icon_sized(Icon::ScreenShare, px(14.)).text_color(t.live)))
                            .when(state.self_video, |d| d.child(icon_sized(Icon::Video, px(14.)).text_color(t.text_faint)))
                            .when(muted, |d| d.child(icon_sized(Icon::MicOff, px(14.)).text_color(if state.server_mute { t.danger } else { t.text_faint })))
                            .when(deaf, |d| d.child(icon_sized(Icon::HeadphoneOff, px(14.)).text_color(if state.server_deaf { t.danger } else { t.text_faint })))
                            .into_any_element(),
                    );
                }
                // Room before the next channel, so the people in a call read as its own.
                if occupied {
                    rows.push(div().h(px(6.)).into_any_element());
                }
            }
        }
        rows
    }

    /// Whether the member list shows: no other panel open, a guild selected and its roster in.
    fn members_shown(&self, cx: &App) -> bool {
        let client = self.client.read(cx);
        matches!(self.panel, Panel::None)
            && self.show_members
            && !client.showing_home
            && client.selected_guild.as_ref().is_some_and(|gid| client.members.contains_key(gid))
    }

    /// The member list, as a virtual list: only the rows in view are drawn, however large the
    /// guild. The order is worked out here; each row reads its member when it is drawn.
    fn members(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let client = self.client.read(cx);
        let Some(gid) = client.selected_guild.clone() else { return div().into_any_element() };
        let Some(members) = client.members.get(&gid) else { return div().into_any_element() };
        let (online, offline): (Vec<_>, Vec<_>) = members.iter().partition(|m| m.status() != "offline");
        // Roles shown separately each get a heading, highest first, and an online member stands
        // under exactly one: the highest of them they hold (as the web client groups them).
        // Everyone else online goes under "Online"; offline members are never grouped.
        let mut hoisted: Vec<&rhein_proto::Role> = client.guild(&gid).map(|g| g.guild.roles.iter().filter(|r| r.hoist).collect()).unwrap_or_default();
        hoisted.sort_by_key(|r| std::cmp::Reverse(r.position));
        let mut groups: Vec<Vec<&rhein_proto::Member>> = vec![Vec::new(); hoisted.len()];
        let mut ungrouped = Vec::new();
        for m in online {
            let held = if client.user_id() == Some(m.user_id.as_str()) { client.my_roles(&gid) } else { m.roles.clone() };
            match hoisted.iter().position(|r| held.contains(&r.id)) {
                Some(best) => groups[best].push(m),
                None => ungrouped.push(m),
            }
        }
        let mut rows = Vec::new();
        let named = hoisted.iter().map(|r| r.name.clone()).zip(groups);
        for (label, list) in named.chain([("Online".to_owned(), ungrouped), ("Offline".to_owned(), offline)]) {
            if list.is_empty() {
                continue;
            }
            rows.push(MemberRow::Heading { label: format!("{label} — {}", list.len()), first: rows.is_empty() });
            rows.extend(list.into_iter().map(|m| MemberRow::Member { user_id: m.user_id.clone(), two_lines: member_subtitle(m).is_some() }));
        }
        // Measured again only when the rows or their heights change.
        if *self.member_rows.borrow() != rows {
            let old = self.members_list.item_count();
            self.members_list.splice(0..old, rows.len());
            *self.member_rows.borrow_mut() = rows;
        }
        let (rows, client, this) = (self.member_rows.clone(), self.client.clone(), cx.weak_entity());
        let row = move |ix: usize, _: &mut Window, cx: &mut App| -> AnyElement {
            let Some(row) = rows.borrow().get(ix).cloned() else { return div().into_any_element() };
            let user_id = match row {
                MemberRow::Heading { label, first } => {
                    let heading = section_label(label, cx);
                    return if first { heading.pt_2() } else { heading }.into_any_element();
                }
                MemberRow::Member { user_id, .. } => user_id,
            };
            let client = client.read(cx);
            let Some(m) = client.member(&gid, &user_id) else { return div().into_any_element() };
            let name = m.shown_name().to_owned();
            let presence = Presence::parse(m.status());
            let (uid, uid2) = (m.user_id.clone(), m.user_id.clone());
            let (weak, weak2) = (this.clone(), this.clone());
            ListRow::new(SharedString::from(format!("member-{}", m.user_id)), name.clone())
                .label_color(client.name_color(&gid, &m.user_id).map(|c| gpui::rgb(c).into()))
                .subtitle(member_subtitle(m))
                .leading(Avatar::new(m.user_id.clone(), name).size(px(28.)).image(client.avatar_url(Some(&gid), &m.user_id).map(Into::into)).presence(Some(presence)))
                .muted(presence == Presence::Offline)
                .when(m.is_bot, |r| r.trailing(Tag("BOT")))
                .on_click(move |e, _, cx| {
                    let (u, at) = (uid.clone(), e.position());
                    weak.update(cx, |v, cx| v.show_profile(u, at, cx)).ok();
                })
                .on_secondary(move |e, window, cx| {
                    let (u, at) = (uid2.clone(), e.position);
                    weak2.update(cx, |v, cx| v.member_menu(u, at, window, cx)).ok();
                })
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg_panel)
            .border_l_1()
            .border_color(t.border)
            .child(
                div()
                    .h(size::HEADER)
                    .flex_none()
                    .px_4()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(t.border)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Members"),
            )
            .child(
                div().flex_1().min_h_0().px_2().child(rhein_ui::smooth_scroll(
                    "members-glide",
                    rhein_ui::ScrollTarget::List(self.members_list.clone()),
                    gpui::list(self.members_list.clone(), row).size_full(),
                )),
            )
            .into_any_element()
    }

    fn dialog(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tab = self.dialog?;
        let t = cx.theme().clone();
        let servers: Vec<String> =
            self.client.read(cx).servers.iter().filter(|s| s.api.is_some()).map(|s| s.name.clone()).collect();
        let explanation = match tab {
            DialogTab::Join => {
                "Join a guild with an invite. If it lives on another server, Rhein signs you in there with your account; you don't need a new one."
            }
            DialogTab::Create => {
                "A guild belongs entirely to the server it lives on: channels, roles, messages and moderation. Choose the server deliberately."
            }
            DialogTab::Server => "Connect another Telium server to see its guilds. Your account stays on your home server.",
        };
        let server_picker = (tab == DialogTab::Create).then(|| {
            let current = self.create_on.clone();
            div().flex().flex_wrap().gap_1().children(servers.into_iter().map(|name| {
                let active = current.as_deref() == Some(name.as_str());
                let pick = name.clone();
                div()
                    .id(SharedString::from(format!("pick-{name}")))
                    .px_2()
                    .py_1()
                    .rounded(radius::MD)
                    .flex()
                    .items_center()
                    .gap_1()
                    .border_1()
                    .border_color(if active { t.accent } else { t.border_strong })
                    .text_size(text::SM)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.create_on = Some(pick.clone());
                        cx.notify();
                    }))
                    .child(icon(Icon::Server).text_color(t.text_muted))
                    .child(name)
            }))
        });
        let selected = DialogTab::ALL.iter().position(|d| *d == tab).unwrap_or(0);
        let this = cx.weak_entity();
        let tabs = Tabs::new("dialog-tabs", DialogTab::ALL.map(DialogTab::label)).selected(selected).on_select(
            move |i, window, cx| {
                this.update(cx, |v, cx| v.open_dialog(DialogTab::ALL[i], window, cx)).ok();
            },
        );
        let this = cx.weak_entity();
        Some(
            Modal::new("add-dialog")
                .on_dismiss(move |window, cx| {
                    this.update(cx, |v, cx| v.close_dialog(window, cx)).ok();
                })
                .child(tabs)
                .child(div().text_color(t.text_muted).child(explanation))
                .children(server_picker)
                .child(self.dialog_input.clone())
                .footer(
                    Button::new("dialog-cancel", "Cancel")
                        .ghost()
                        .on_click(cx.listener(|this, _, window, cx| this.close_dialog(window, cx))),
                )
                .footer(
                    Button::new("dialog-ok", tab.label())
                        .primary()
                        .on_click(cx.listener(|this, _, window, cx| this.confirm_dialog(window, cx))),
                )
                .into_any_element(),
        )
    }

    /// Signing out drops this device's session, so it asks first.
    fn sign_out_dialog(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let others = self.confirm_sign_out?;
        let t = cx.theme().clone();
        let me = self.client.read(cx).user_id().unwrap_or_default().to_owned();
        let rest = match others {
            0 => String::new(),
            1 => " Your other saved account stays signed in.".into(),
            n => format!(" Your {n} other saved accounts stay signed in."),
        };
        let this = cx.weak_entity();
        Some(
            Modal::new("sign-out")
                .title("Sign out?")
                .width(px(400.))
                .focus(&self.modal_focus)
                .on_dismiss(move |window, cx| {
                    this.update(cx, |v, cx| v.close_dialog(window, cx)).ok();
                })
                .child(div().text_color(t.text_muted).child(format!(
                    "You'll be signed out of {me} on this device. Your account, guilds and \
                     messages stay on your servers; you can sign in again at any time.{rest}"
                )))
                .footer(
                    Button::new("sign-out-cancel", "Cancel")
                        .ghost()
                        .on_click(cx.listener(|this, _, window, cx| this.close_dialog(window, cx))),
                )
                .footer(
                    Button::new("sign-out-confirm", "Sign out")
                        .danger()
                        .icon(Icon::LogOut)
                        .on_click(cx.listener(|this, _, _, cx| this.sign_out(cx))),
                )
                .into_any_element(),
        )
    }

    /// Someone calling you in a conversation: who, with Answer and Decline.
    fn ringing_card(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let client = self.client.read(cx);
        let (conversation, from) = client.ringing().map(|(c, f)| (c.to_owned(), f.to_owned()))?;
        let t = cx.theme().clone();
        let name = client.display_name(None, &from);
        let face = Avatar::new(from.clone(), name.clone()).size(px(40.)).image(client.avatar_url(None, &from).map(Into::into));
        let (answer, decline) = (self.client.clone(), self.client.clone());
        Some(
            div()
                .absolute()
                .top(px(16.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(motion::pop_in(
                    div()
                        .id("ringing")
                        .occlude()
                        .w(px(340.))
                        .p_3()
                        .rounded(radius::LG)
                        .bg(t.bg_raised)
                        .border_1()
                        .border_color(t.border_strong)
                        .shadow_lg()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(face)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(div().font_weight(FontWeight::SEMIBOLD).truncate().child(name))
                                .child(div().text_size(text::SM).text_color(t.text_muted).child("is calling you")),
                        )
                        .child(Button::new("ring-decline", "Decline").ghost().on_click(move |_, _, cx| decline.update(cx, |c, cx| c.decline_ring(cx))))
                        .child(Button::new("ring-answer", "Answer").primary().icon(Icon::Phone).on_click(move |_, _, cx| {
                            let conversation = conversation.clone();
                            answer.update(cx, |c, cx| {
                                c.decline_ring(cx);
                                c.join_dm_call(conversation, cx);
                            })
                        })),
                    "ringing-in",
                ))
                .into_any_element(),
        )
    }

    /// Notices about the servers themselves (a scheduled move, a shutdown, a server this build
    /// can't talk to), one strip each above the chat until closed.
    fn banners(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = cx.theme().clone();
        self.client
            .read(cx)
            .banners()
            .into_iter()
            .map(|banner| {
                let color = if banner.urgent { t.danger } else { t.accent };
                let (c1, c2, key) = (self.client.clone(), self.client.clone(), banner.key.clone());
                div()
                    .flex_none()
                    .px_3()
                    .py_1p5()
                    .flex()
                    .items_center()
                    .gap_3()
                    .border_b_1()
                    .border_color(t.border)
                    .bg(gpui::Hsla::from(color).opacity(0.12))
                    .text_size(text::SM)
                    .child(icon_sized(if banner.urgent { Icon::CircleAlert } else { Icon::Info }, px(16.)).text_color(color))
                    .child(div().flex_1().min_w_0().child(banner.text))
                    .children(banner.action.map(|(label, action)| {
                        Button::new(SharedString::from(format!("banner-act-{}", banner.key)), label)
                            .small()
                            .kind(if banner.urgent { rhein_ui::ButtonKind::Danger } else { rhein_ui::ButtonKind::Secondary })
                            .on_click(move |_, _, cx| {
                                let action = action.clone();
                                c1.update(cx, |c, cx| c.banner_action(action, cx));
                            })
                    }))
                    .child(
                        IconButton::new(SharedString::from(format!("banner-close-{}", banner.key)), Icon::X)
                            .tooltip("Hide until next time")
                            .on_click(move |_, _, cx| c2.update(cx, |c, cx| c.dismiss_banner(&key, cx))),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn toast(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let client = self.client.read(cx);
        let (text, kind) = match (&client.error, &client.warning, &client.notice) {
            (Some(e), _, _) => (e.clone(), ToastKind::Error),
            (None, Some(w), _) => (w.clone(), ToastKind::Warning),
            (None, None, Some(n)) => (n.clone(), ToastKind::Info),
            _ => return None,
        };
        let client = self.client.clone();
        Some(
            div()
                .absolute()
                .bottom(px(84.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(Toast::new(text, kind).on_dismiss(move |_, _, cx| client.update(cx, |c, cx| c.dismiss(cx))))
                .into_any_element(),
        )
    }
}

/// The space between two channels in the sidebar.
const CHANNEL_GAP: f32 = 3.;

/// What a row of the full profile does when clicked.
type RowAction = Box<dyn Fn(&mut Shell, &mut Context<Shell>)>;

/// The style of a cached view filling a panel's column below its edge.
fn fill() -> gpui::StyleRefinement {
    gpui::StyleRefinement::default().flex_1().min_h_0().w_full()
}

/// One row of the member list: a heading, or a member (with whether their row has a second
/// line, which changes its height).
#[derive(Clone, PartialEq)]
enum MemberRow {
    Heading { label: String, first: bool },
    Member { user_id: String, two_lines: bool },
}

/// A member's second line: a custom status is what the person chose to say, so it comes first;
/// without one, what they are playing or listening to.
fn member_subtitle(m: &rhein_proto::Member) -> Option<String> {
    m.presence
        .as_ref()
        .and_then(|p| p.get("custom_status"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| m.activities().first().map(|a| a.summary()))
}

fn presence_of(status: &str) -> Presence {
    match status {
        "idle" => Presence::Idle,
        "dnd" => Presence::Dnd,
        "invisible" => Presence::Offline,
        _ => Presence::Online,
    }
}

/// A presence colour as a small dot, for menus.
fn presence_dot(presence: Presence, cx: &App) -> AnyElement {
    let t = cx.theme();
    let color = match presence {
        Presence::Online => t.success,
        Presence::Idle => t.warning,
        Presence::Dnd => t.danger,
        Presence::Offline => t.text_faint,
    };
    div().size(px(16.)).flex().items_center().justify_center().child(div().size(px(9.)).rounded_full().bg(color)).into_any_element()
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar = self.parts(cx).sidebar.clone().cached(gpui::StyleRefinement::default().w(size::SIDEBAR).h_full().flex_none());
        let status = StatusLine::new(self.client.clone());
        let members = self.render_panel(cx);
        let dialog = self.dialog(cx);
        let sign_out = self.sign_out_dialog(cx);
        let toast = self.toast(cx);
        let banners = self.banners(cx);
        let ringing = self.ringing_card(cx);
        let modal = self.render_modal(cx);
        let card = self.render_profile_card(window, cx);
        let full_profile = self.render_full_profile(window, cx);
        let switcher = self.switcher.as_ref().map(|(view, _)| {
            motion::fade_in(
                div()
                    .absolute()
                    .inset_0()
                    .bg(cx.theme().overlay)
                    .flex()
                    .justify_center()
                    .pt(px(120.))
                    .child(motion::pop_in(div().relative().child(view.clone()), "switcher-in")),
                "switcher-fade",
                motion::FAST,
            )
        });
        let settings = self
            .settings
            .as_ref()
            .map(|(view, _)| motion::fade_in(div().absolute().inset_0().child(view.clone()), "settings-in", motion::FAST));
        let guild_settings = self
            .guild_settings
            .as_ref()
            .map(|(view, _)| motion::fade_in(div().absolute().inset_0().child(view.clone()), "guild-settings-in", motion::FAST));
        // Pointer input anywhere in the window counts as being here (keys are observed in `new`).
        let watch_pointer = {
            let (last, client) = (self.last_input.clone(), self.client.clone());
            gpui::canvas(|_, _, _| (), move |_, _, window, _| {
                let (l1, c1, l2, c2, l3, c3) = (last.clone(), client.clone(), last.clone(), client.clone(), last.clone(), client.clone());
                window.on_mouse_event(move |_: &gpui::MouseMoveEvent, phase, _, cx| {
                    if phase == gpui::DispatchPhase::Capture {
                        note_input(&l1, &c1, cx);
                    }
                });
                window.on_mouse_event(move |_: &gpui::MouseDownEvent, phase, _, cx| {
                    if phase == gpui::DispatchPhase::Capture {
                        note_input(&l2, &c2, cx);
                    }
                });
                window.on_mouse_event(move |_: &gpui::ScrollEvent, phase, _, cx| {
                    if phase == gpui::DispatchPhase::Capture {
                        note_input(&l3, &c3, cx);
                    }
                });
            })
            .absolute()
            .size_0()
        };
        div()
            .child(watch_pointer)
            .key_context(SHELL_CONTEXT)
            // Push-to-talk, while this window has focus: backtick held means talking, and is not
            // typed. (A global shortcut through the desktop portal comes later.)
            .capture_key_down(cx.listener(|this, e: &gpui::KeyDownEvent, _, cx| {
                if e.keystroke.key == "`" && this.push_to_talk_live(cx) {
                    this.client.read(cx).set_talking(true);
                    cx.stop_propagation();
                }
            }))
            .capture_key_up(cx.listener(|this, e: &gpui::KeyUpEvent, _, cx| {
                if e.keystroke.key == "`" && this.push_to_talk_live(cx) {
                    this.client.read(cx).set_talking(false);
                    cx.stop_propagation();
                }
            }))
            .on_action(cx.listener(|this, _: &OpenSwitcher, window, cx| this.open_switcher(window, cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| this.open_settings(window, cx)))
            .on_action(cx.listener(|this, _: &PrevChannel, _, cx| this.client.update(cx, |c, cx| c.step_channel(-1, false, cx))))
            .on_action(cx.listener(|this, _: &NextChannel, _, cx| this.client.update(cx, |c, cx| c.step_channel(1, false, cx))))
            .on_action(cx.listener(|this, _: &PrevUnread, _, cx| this.client.update(cx, |c, cx| c.step_channel(-1, true, cx))))
            .on_action(cx.listener(|this, _: &NextUnread, _, cx| this.client.update(cx, |c, cx| c.step_channel(1, true, cx))))
            .relative()
            .size_full()
            .flex()
            .child(sidebar)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .children(banners)
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .map(|d| {
                                // An open conversation reads like a channel; the friends page is home's own view.
                                let client = self.client.read(cx);
                                // Cached: the chat draws again when it or the client changes, not whenever the
                                // sidebar or member list around it does.
                                if client.showing_home && client.home_conversation.is_none() {
                                    d.child(self.home.clone().cached(gpui::StyleRefinement::default().flex_1().min_w_0().h_full()))
                                } else {
                                    d.child(self.chat.clone().cached(gpui::StyleRefinement::default().flex_1().min_w_0().h_full()))
                                }
                            }),
                    )
                    .child(status),
            )
            // Side panels run the full height; the status line belongs to the middle column.
            .children(members)
            .children(toast)
            .children(ringing)
            .children(dialog)
            .children(sign_out)
            .children(settings)
            .children(guild_settings)
            .children(switcher)
            .children(modal)
            .children(card)
            .children(full_profile)
            .children(self.menu.element())
    }
}
