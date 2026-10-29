//! Settings: a full-window overlay with a section list on the left.

use gpui::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    ParentElement as _, PathPromptOptions, Render, SharedString, Styled as _, Subscription, Window, div, prelude::*,
    px,
};
use rhein_proto::Device;
use rhein_store::client::update::Update;
use rhein_store::{Client, ServerStatus, VoicePrefs};

use crate::views::crop::{self, CropDialog, CropEvent};
use crate::views::profile;
use rhein_ui::theme::{FONT_MONO, radius, text};
use rhein_ui::{ActiveTheme as _, Avatar, Button, Dismiss, Icon, IconButton, InputEvent, ListRow, Switch, Tabs, TextInput, Theme, icon};

pub const SETTINGS_CONTEXT: &str = "Settings";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Account,
    Devices,
    Servers,
    Appearance,
    Notifications,
    Privacy,
    Security,
    Extras,
    Activity,
    Encryption,
    Voice,
    Shortcuts,
    About,
}

impl Section {
    const ALL: [Section; 13] = [
        Section::Account,
        Section::Devices,
        Section::Servers,
        Section::Appearance,
        Section::Notifications,
        Section::Privacy,
        Section::Security,
        Section::Extras,
        Section::Activity,
        Section::Encryption,
        Section::Voice,
        Section::Shortcuts,
        Section::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::Account => "My account",
            Section::Devices => "Devices",
            Section::Servers => "Servers",
            Section::Appearance => "Appearance",
            Section::Notifications => "Notifications",
            Section::Privacy => "Messages & privacy",
            Section::Security => "Account & security",
            Section::Extras => "Extras",
            Section::Activity => "Activity",
            Section::Encryption => "Encryption",
            Section::Voice => "Voice",
            Section::Shortcuts => "Keyboard shortcuts",
            Section::About => "About Rhein",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Section::Account => Icon::User,
            Section::Devices => Icon::Monitor,
            Section::Servers => Icon::Server,
            Section::Appearance => Icon::Palette,
            Section::Notifications => Icon::Bell,
            Section::Privacy => Icon::Eye,
            Section::Security => Icon::Shield,
            Section::Extras => Icon::Star,
            Section::Activity => Icon::Gamepad,
            Section::Encryption => Icon::Lock,
            Section::Voice => Icon::Mic,
            Section::Shortcuts => Icon::Keyboard,
            Section::About => Icon::Info,
        }
    }
}

pub enum SettingsEvent {
    Close,
    SignOut,
}

/// Every shortcut, for the read-only list.
pub const SHORTCUTS: [(&str, &str); 16] = [
    ("Ctrl+K", "Quick switcher"),
    ("Ctrl+,", "Settings"),
    ("Alt+↑ / Alt+↓", "Previous / next channel"),
    ("Alt+Shift+↑ / ↓", "Previous / next unread channel"),
    ("Esc", "Mark channel read and jump to the present"),
    ("Enter", "Send message"),
    ("Shift+Enter", "New line"),
    ("↑ (empty message)", "Edit your last message"),
    ("Tab / Enter", "Accept a suggestion"),
    ("Ctrl+V", "Paste text or an image"),
    ("Home / End", "Start / end of the line (with Ctrl: of the whole text)"),
    ("Shift + arrows, Home, End", "Select"),
    ("Ctrl+← / Ctrl+→", "Previous / next word (with Shift: select)"),
    ("Ctrl+Backspace / Ctrl+Delete", "Delete the word before / after the cursor"),
    ("Esc (while replying or editing)", "Cancel"),
    ("Ctrl+Q", "Quit"),
];

pub struct SettingsView {
    client: Entity<Client>,
    body_scroll: gpui::ScrollHandle,
    section: Section,
    display_name: Entity<TextInput>,
    pronouns: Entity<TextInput>,
    bio: Entity<TextInput>,
    avatar_ref: Option<String>,
    avatar_dirty: bool,
    banner_ref: Option<String>,
    banner_dirty: bool,
    accent: Option<u32>,
    theme: Option<[u32; 2]>,
    /// Accent or theme changed since the last save.
    look_dirty: bool,
    /// The profile's banner, accent and theme have been read in (they arrive with the profile).
    look_loaded: bool,
    saving: bool,
    status: Option<(bool, String)>,
    security: SecurityForm,
    /// What the Extras page last did.
    extras_status: Option<(bool, String)>,
    /// A first click on removing this device's keys.
    extras_confirm: bool,
    /// The crop dialog for a picked avatar or banner.
    crop: Option<(Entity<CropDialog>, Subscription)>,
    crop_focus: bool,
    devices: Option<Result<Vec<Device>, String>>,
    /// Microphones and speakers, once listed.
    audio_devices: Option<(Vec<rhein_voice::audio::AudioDevice>, Vec<rhein_voice::audio::AudioDevice>)>,
    /// Redraws the level meter while the voice section is open.
    meter: Option<gpui::Task<()>>,
    add_server: Entity<TextInput>,
    unlock_password: Entity<TextInput>,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl SettingsView {
    pub fn new(client: Entity<Client>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (name, avatar_ref, profile) = {
            let c = client.read(cx);
            let home = c.home.as_ref();
            (
                home.and_then(|h| h.display_name.clone()).unwrap_or_default(),
                home.and_then(|h| h.avatar_ref.clone()),
                home.map(|h| h.user_id.clone()),
            )
        };
        let cached = profile.and_then(|id| client.update(cx, |c, cx| c.profile(&id, cx)));
        let display_name = cx.new(|cx| TextInput::new("Display name", cx).with_text(name));
        let pronouns = cx.new(|cx| {
            TextInput::new("Pronouns", cx).with_text(cached.as_ref().and_then(|p| p.pronouns.clone()).unwrap_or_default())
        });
        let bio = cx.new(|cx| {
            TextInput::new("About me", cx).multiline(5).with_text(cached.as_ref().and_then(|p| p.bio.clone()).unwrap_or_default())
        });
        let add_server = cx.new(|cx| TextInput::new("chat.example.com", cx));
        let unlock_password = cx.new(|cx| TextInput::new("Your account password", cx).masked());
        let subs = vec![
            cx.observe(&client, |this, _, cx| {
                this.load_look(cx);
                cx.notify();
            }),
            cx.subscribe_in(&add_server, window, |this, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Submit) {
                    let text = input.read(cx).text().trim().to_owned();
                    if !text.is_empty() {
                        this.client.update(cx, |c, cx| c.add_server(text, cx));
                        input.update(cx, |i, cx| i.clear(cx));
                    }
                }
            }),
        ];
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let mut view = Self {
            client,
            body_scroll: gpui::ScrollHandle::new(),
            section: Section::Account,
            display_name,
            pronouns,
            bio,
            avatar_ref,
            avatar_dirty: false,
            banner_ref: None,
            banner_dirty: false,
            accent: None,
            theme: None,
            look_dirty: false,
            look_loaded: false,
            saving: false,
            status: None,
            crop: None,
            crop_focus: false,
            security: SecurityForm::new(cx),
            extras_status: None,
            extras_confirm: false,
            devices: None,
            audio_devices: None,
            meter: None,
            add_server,
            unlock_password,
            focus,
            _subs: subs,
        };
        // The profile is usually there already (fetched at sign-in, kept on disk): show it now,
        // not at the client's next change.
        view.load_look(cx);
        view
    }

    /// Takes the banner, accent and theme from the profile once it has arrived, and whatever the
    /// profile had for pronouns and bio if those fields are still untouched.
    fn load_look(&mut self, cx: &mut Context<Self>) {
        if self.look_loaded {
            return;
        }
        let client = self.client.read(cx);
        let Some(me) = client.user_id().map(str::to_owned) else { return };
        let Some(profile) = client.cached_profile(&me).cloned() else { return };
        self.look_loaded = true;
        self.banner_ref = profile.banner_ref.filter(|r| !r.is_empty());
        self.accent = profile.accent_color;
        self.theme = profile.theme_colors;
        if self.pronouns.read(cx).text().is_empty() {
            let text = profile.pronouns.unwrap_or_default();
            self.pronouns.update(cx, |i, cx| i.set_text(text, cx));
        }
        if self.bio.read(cx).text().is_empty() {
            let text = profile.bio.unwrap_or_default();
            self.bio.update(cx, |i, cx| i.set_text(text, cx));
        }
    }

    /// Opens a section by its label ("Activity"), for debug start screens.
    #[cfg(debug_assertions)]
    pub fn open_named(&mut self, label: &str, cx: &mut Context<Self>) {
        if let Some(section) = Section::ALL.into_iter().find(|s| s.label().eq_ignore_ascii_case(label)) {
            self.open(section, cx);
        }
    }

    fn open(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        self.status = None;
        if section == Section::Devices {
            self.reload_devices(cx);
        }
        if section == Section::Security {
            self.reload_security(cx);
        }
        self.meter = None;
        // Leaving the voice settings ends the mic test.
        if section != Section::Voice {
            self.client.update(cx, |c, cx| c.stop_mic_test(cx));
        }
        if section == Section::Voice {
            self.load_audio_devices(cx);
            // The level meter moves while a call is up.
            self.meter = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(std::time::Duration::from_millis(50)).await;
                    if this.update(cx, |v, cx| if v.client.read(cx).call().is_some() || v.client.read(cx).mic_testing() { cx.notify() }).is_err() {
                        return;
                    }
                }
            }));
        }
        cx.notify();
    }

    fn load_audio_devices(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async { (rhein_voice::audio::input_devices(), rhein_voice::audio::output_devices()) })
                .await;
            this.update(cx, |v, cx| {
                v.audio_devices = Some(found);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Calls: which microphone and speakers, how sensitive, push-to-talk, processing.
    fn voice_section(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let client = self.client.read(cx);
        let voice = client.voice_prefs();
        let level = client.input_level();
        let row = |title: &'static str, detail: &'static str, control: AnyElement| {
            div()
                .p_3()
                .rounded(radius::MD)
                .border_1()
                .border_color(t.border)
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(div().font_weight(FontWeight::MEDIUM).child(title))
                                .child(div().text_size(text::SM).text_color(t.text_faint).child(detail)),
                        ),
                )
                .child(control)
        };
        let pill = |id: SharedString, label: String, active: bool, on_click: Box<dyn Fn(&mut gpui::App)>| {
            div()
                .id(id)
                .px_2()
                .py_1()
                .rounded(radius::MD)
                .border_1()
                .border_color(if active { t.accent } else { t.border_strong })
                .text_size(text::SM)
                .cursor_pointer()
                .on_click(move |_, _, cx| on_click(cx))
                .child(label)
        };
        let device_pills = |input: bool| -> AnyElement {
            let current = if input { voice.input.clone() } else { voice.output.clone() };
            let Some((inputs, outputs)) = &self.audio_devices else {
                return div().text_size(text::SM).text_color(t.text_faint).child("Looking for devices…").into_any_element();
            };
            let list = if input { inputs } else { outputs };
            let mut choices = vec![(None, "System default".to_owned())];
            choices.extend(list.iter().filter(|d| !d.id.ends_with(":default")).map(|d| (Some(d.id.clone()), d.name.clone())));
            div()
                .flex()
                .flex_wrap()
                .gap_1()
                .children(choices.into_iter().enumerate().map(|(i, (id, name))| {
                    let client = self.client.clone();
                    let active = id == current;
                    pill(
                        SharedString::from(format!("{}-{i}", if input { "in" } else { "out" })),
                        name,
                        active,
                        Box::new(move |cx| {
                            let id = id.clone();
                            client.update(cx, |c, cx| {
                                c.update_voice_prefs(cx, |v| if input { v.input = id } else { v.output = id })
                            })
                        }),
                    )
                }))
                .into_any_element()
        };
        let gates: [(Option<f32>, &str); 5] = [(None, "Off"), (Some(-60.), "-60 dB"), (Some(-50.), "-50 dB"), (Some(-40.), "-40 dB"), (Some(-30.), "-30 dB")];
        let gate_pills = div().flex().gap_1().children(gates.into_iter().enumerate().map(|(i, (gate, label))| {
            let client = self.client.clone();
            pill(
                SharedString::from(format!("gate-{i}")),
                label.to_owned(),
                voice.gate == gate,
                Box::new(move |cx| client.update(cx, |c, cx| c.update_voice_prefs(cx, |v| v.gate = gate))),
            )
        }));
        // The meter: how loud the microphone is, and where the gate cuts in (-70..0 dBFS).
        let meter = level.map(|level| {
            let db = 20. * level.max(1e-4).log10();
            let at = |db: f32| ((db + 70.) / 70.).clamp(0., 1.);
            let open = voice.gate.is_none_or(|g| db >= g);
            div()
                .relative()
                .h(px(8.))
                .w_full()
                .rounded_full()
                .bg(t.bg_selected)
                .child(div().absolute().left_0().top_0().h_full().rounded_full().w(gpui::relative(at(db))).bg(if open { t.success } else { t.text_faint }))
                .children(voice.gate.map(|g| div().absolute().top_0().h_full().w(px(2.)).left(gpui::relative(at(g))).bg(t.text)))
        });
        // Hear yourself as the others would; in a call it deafens you meanwhile.
        let testing = client.mic_testing();
        let in_call = client.call().is_some();
        let mic_test = div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                Button::new("mic-test", if testing { "Stop" } else { "Let me hear myself" })
                    .kind(if testing { rhein_ui::ButtonKind::Danger } else { rhein_ui::ButtonKind::Secondary })
                    .icon(if testing { Icon::MicOff } else { Icon::Mic })
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.client.update(cx, |c, cx| if testing { c.stop_mic_test(cx) } else { c.start_mic_test(cx) });
                    })),
            )
            .child(div().text_size(text::SM).text_color(t.text_faint).child(if testing {
                if in_call { "You hear your microphone; the call is deafened until you stop." } else { "You hear your microphone as the others would." }
            } else if in_call {
                "You'll be deafened in the call while you listen to yourself."
            } else {
                "Plays your microphone back, with noise suppression and the gate."
            }));
        let sensitivity = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(gate_pills)
            .child(match meter {
                Some(meter) => meter.into_any_element(),
                None => div().text_size(text::SM).text_color(t.text_faint).child("Start the mic test or join a call to see your microphone's level here.").into_any_element(),
            })
            .child(mic_test);
        let (c1, c2) = (self.client.clone(), self.client.clone());
        // Call privacy in conversations: the copy is the web client's (`POSTURE_COPY`).
        let postures = [
            ("direct", "Direct", "Connect straight to the others when the network allows. The quickest path, but in a peer-to-peer call everyone in it can see your IP address."),
            ("relay", "Relay only", "Every connection goes through a TURN relay, so nobody in the call learns your address. Needs a relay on your server, or an SFU."),
            ("sfu", "Through an SFU", "Media goes through an SFU and is encrypted end to end, so the SFU cannot hear or see it. The SFU still sees your address and when you talk."),
        ];
        let current = if voice.posture.is_empty() { "direct" } else { voice.posture.as_str() };
        let explained = postures.iter().find(|(id, ..)| *id == current).map(|(.., copy)| *copy).unwrap_or_default();
        let posture_control = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().flex().gap_1().children(postures.iter().map(|(id, label, _)| {
                let (client, id) = (self.client.clone(), id.to_string());
                pill(
                    SharedString::from(format!("posture-{id}")),
                    label.to_string(),
                    current == id,
                    Box::new(move |cx| {
                        let id = id.clone();
                        client.update(cx, |c, cx| c.update_voice_prefs(cx, |v| v.posture = id))
                    }),
                )
            })))
            .child(div().text_size(text::SM).text_color(t.text_faint).child(explained));
        // Screen shares: what they are for, how tall and how fast (`features/voice-and-video`).
        let share_pills = |items: Vec<SharePill>| {
            div().flex().gap_1().children(items.into_iter().map(|(id, label, on, set)| {
                let client = self.client.clone();
                let set = std::rc::Rc::new(set);
                pill(SharedString::from(id), label, on, Box::new(move |cx| {
                    let set = set.clone();
                    client.update(cx, |c, cx| c.update_voice_prefs(cx, move |v| set(v)))
                }))
            }))
        };
        let motion = voice.share_motion();
        let kinds: Vec<SharePill> = vec![
            // Choosing what it is for resets the rate to that kind's own.
            ("share-detail".into(), "A document or code".into(), !motion, Box::new(|v: &mut VoicePrefs| {
                v.share_kind = "detail".into();
                v.share_fps = 0;
            })),
            ("share-motion".into(), "A game or video".into(), motion, Box::new(|v: &mut VoicePrefs| {
                v.share_kind = "motion".into();
                v.share_fps = 0;
            })),
        ];
        let heights: Vec<SharePill> = [0u32, 720, 1080, 1440, 2160]
            .into_iter()
            .map(|h| {
                let label = if h == 0 { "As it is".to_owned() } else { format!("{h}p") };
                (format!("share-h-{h}"), label, voice.share_height == h, Box::new(move |v: &mut VoicePrefs| v.share_height = h) as Box<dyn Fn(&mut VoicePrefs)>)
            })
            .collect();
        let rates: Vec<SharePill> = [15u32, 30, 60]
            .into_iter()
            .map(|f| (format!("share-fps-{f}"), format!("{f} fps"), voice.share_fps() == f, Box::new(move |v: &mut VoicePrefs| v.share_fps = f) as Box<dyn Fn(&mut VoicePrefs)>))
            .collect();
        let share_control = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(share_pills(kinds))
            .child(share_pills(heights))
            .child(share_pills(rates))
            .child(div().text_size(text::SM).text_color(t.text_faint).child(if motion {
                "Keeps the motion smooth and lets sharpness give way when the connection is tight."
            } else {
                "Keeps text sharp and lets the frame rate give way when the connection is tight."
            }));
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(row("Call privacy", "For calls in direct messages and group chats. A call is as private as its most cautious participant asks; no server learns your choice.", posture_control.into_any_element()))
            .child(row("Screen sharing", "What your shares are usually for. Both spend the same bandwidth, split differently.", share_control.into_any_element()))
            .child(row("Microphone", "Where your voice comes from.", device_pills(true)))
            .child(row("Speakers", "Where calls are played.", device_pills(false)))
            .child(row("Input sensitivity", "Below this level the microphone sends silence, so keyboards and fans stay out of the call.", sensitivity.into_any_element()))
            .child(row(
                "Push to talk",
                "Only send your voice while holding the backtick key (`) in Rhein.",
                Switch::new("ptt", voice.push_to_talk)
                    .on_toggle(move |on, _, cx| c1.update(cx, |c, cx| c.update_voice_prefs(cx, |v| v.push_to_talk = on)))
                    .into_any_element(),
            ))
            .child(row(
                "Voice processing",
                "Echo cancellation, noise suppression and automatic gain. Turn off for a studio microphone or music.",
                Switch::new("processing", !voice.raw_input)
                    .on_toggle(move |on, _, cx| c2.update(cx, |c, cx| c.update_voice_prefs(cx, |v| v.raw_input = !on)))
                    .into_any_element(),
            ))
            .into_any_element()
    }

    fn reload_devices(&mut self, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        self.client.update(cx, |c, cx| {
            c.load_devices(
                move |result, cx| {
                    this.update(cx, |v, cx| {
                        v.devices = Some(result);
                        cx.notify();
                    })
                    .ok();
                },
                cx,
            )
        });
    }

    fn save_profile(&mut self, cx: &mut Context<Self>) {
        let mut patch = serde_json::json!({
            "display_name": self.display_name.read(cx).text().trim(),
            "pronouns": self.pronouns.read(cx).text().trim(),
            "bio": self.bio.read(cx).text().trim(),
        });
        if self.avatar_dirty {
            patch["avatar_ref"] = serde_json::json!(self.avatar_ref);
        }
        if self.banner_dirty {
            patch["banner_ref"] = serde_json::json!(self.banner_ref);
        }
        if self.look_dirty {
            patch["accent_color"] = serde_json::json!(self.accent);
            patch["theme_colors"] = serde_json::json!(self.theme);
        }
        self.saving = true;
        self.status = None;
        let this = cx.weak_entity();
        self.client.update(cx, |c, cx| {
            c.save_profile(
                patch,
                move |result, cx| {
                    this.update(cx, |v, cx| {
                        v.saving = false;
                        if result.is_ok() {
                            v.avatar_dirty = false;
                            v.banner_dirty = false;
                            v.look_dirty = false;
                        }
                        v.status = Some(match result {
                            Ok(()) => (true, "Saved.".into()),
                            Err(e) => (false, format!("Couldn't save: {e}")),
                        });
                        cx.notify();
                    })
                    .ok();
                },
                cx,
            )
        });
        cx.notify();
    }

    fn pick_avatar(&mut self, cx: &mut Context<Self>) {
        self.pick_media(false, cx);
    }

    fn pick_banner(&mut self, cx: &mut Context<Self>) {
        self.pick_media(true, cx);
    }

    /// Picks a picture for the avatar or the banner and opens it in the crop dialog; the crop
    /// is uploaded, and applies on save.
    fn pick_media(&mut self, banner: bool, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(if banner { "Choose a banner".into() } else { "Choose a picture".into() }),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("picture").to_owned();
            let prepared = cx
                .background_spawn(async move { std::fs::read(&path).map_err(anyhow::Error::from).and_then(|bytes| crop::prepare(bytes, &name)) })
                .await;
            this.update(cx, |v, cx| match prepared {
                Ok(prepared) => v.open_crop(prepared, banner, cx),
                Err(e) => {
                    v.status = Some((false, format!("Couldn't open that picture: {e}")));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Opens the crop dialog on a file, without the file picker (`RHEIN_DEV_OPEN=crop=…`).
    #[cfg(debug_assertions)]
    pub fn dev_crop(&mut self, path: std::path::PathBuf, banner: bool, cx: &mut Context<Self>) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("picture").to_owned();
        match std::fs::read(&path).map_err(anyhow::Error::from).and_then(|bytes| crop::prepare(bytes, &name)) {
            Ok(prepared) => self.open_crop(prepared, banner, cx),
            Err(e) => log::warn!("dev crop: {e:#}"),
        }
    }

    fn open_crop(&mut self, prepared: crop::Prepared, banner: bool, cx: &mut Context<Self>) {
        let preset = if banner { rhein_store::crop::BANNER } else { rhein_store::crop::AVATAR };
        let dialog = cx.new(|cx| CropDialog::new(prepared, preset, cx));
        let sub = cx.subscribe(&dialog, move |v, _, event: &CropEvent, cx| {
            v.crop = None;
            if let CropEvent::Done(encoded) = event {
                v.upload_media(encoded.clone(), banner, cx);
            }
            cx.notify();
        });
        self.crop = Some((dialog, sub));
        self.crop_focus = true;
        cx.notify();
    }

    fn upload_media(&mut self, encoded: rhein_store::crop::Encoded, banner: bool, cx: &mut Context<Self>) {
        self.status = Some((true, "Uploading…".into()));
        let weak = cx.weak_entity();
        self.client.update(cx, |c, cx| {
            c.upload_profile_bytes(
                encoded.bytes,
                encoded.filename,
                encoded.mime,
                move |result, cx| {
                    weak.update(cx, |v, cx| {
                        match result {
                            Ok(reference) if banner => {
                                v.banner_ref = Some(reference);
                                v.banner_dirty = true;
                                v.status = Some((true, "Banner uploaded – save to apply.".into()));
                            }
                            Ok(reference) => {
                                v.avatar_ref = Some(reference);
                                v.avatar_dirty = true;
                                v.status = Some((true, "Picture uploaded – save to apply.".into()));
                            }
                            Err(e) => v.status = Some((false, format!("Upload failed: {e}"))),
                        }
                        cx.notify();
                    })
                    .ok();
                },
                cx,
            )
        });
        cx.notify();
    }

    fn account(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let client = self.client.read(cx);
        let me = client.user_id().unwrap_or_default().to_owned();
        let name = self.display_name.read(cx).text().to_owned();
        let preview = self.avatar_ref.as_deref().and_then(|r| client.home_media_url(r));
        let field = |label: &'static str, input: &Entity<TextInput>| {
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(div().text_size(text::SM).text_color(t.text_muted).child(label))
                .child(input.clone())
        };
        let copy_id = me.clone();
        let fields = div()
            .flex_1()
            .min_w(px(340.))
            .max_w(px(480.))
            .flex()
            .flex_col()
            .gap_5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(Avatar::new(me.clone(), if name.is_empty() { me.clone() } else { name }).size(px(72.)).image(preview.map(Into::into)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .text_color(t.text_muted)
                                    .child(me.clone())
                                    .child(
                                        IconButton::new("copy-user-id", Icon::Copy)
                                            .tooltip("Copy user ID")
                                            .on_click(move |_, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy_id.clone()))),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(Button::new("avatar-change", "Change picture").icon(Icon::Image).on_click(cx.listener(|v, _, _, cx| v.pick_avatar(cx))))
                                    .when(self.avatar_ref.is_some(), |d| {
                                        d.child(Button::new("avatar-remove", "Remove").ghost().on_click(cx.listener(|v, _, _, cx| {
                                            v.avatar_ref = None;
                                            v.avatar_dirty = true;
                                            cx.notify();
                                        })))
                                    }),
                            ),
                    ),
            )
            .child(field("Display name", &self.display_name))
            .child(field("Pronouns", &self.pronouns))
            .child(field("About me", &self.bio))
            .child(self.look_editor(t, cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        Button::new("profile-save", if self.saving { "Saving…" } else { "Save changes" })
                            .primary()
                            // Saving before the profile arrived would wipe its banner and colours.
                            .disabled(self.saving || !self.look_loaded)
                            .on_click(cx.listener(|v, _, _, cx| v.save_profile(cx))),
                    )
                    .children(self.status.clone().map(|(ok, msg)| {
                        div().text_size(text::SM).text_color(if ok { t.success } else { t.danger }).child(msg)
                    })),
            );
        // The fields on the left, the card as others will see it on the right (below them when
        // the window is too narrow for both).
        div()
            .flex()
            .flex_wrap()
            .items_start()
            .gap_10()
            .child(fields)
            .child(
                div()
                    .flex_none()
                    .w(px(profile::CARD_W))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().font_family(FONT_MONO).text_size(text::XS).text_color(t.text_faint).child("preview"))
                    // Until the profile has arrived, a placeholder: the fields would otherwise
                    // show a card without its banner and colours.
                    .child(if self.look_loaded { self.preview_card(t, cx) } else { preview_placeholder(t) }),
            )
            .into_any_element()
    }

    /// Your card as others will see it, with what is typed so far.
    fn preview_card(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let client = self.client.read(cx);
        let me = client.user_id().unwrap_or_default().to_owned();
        let name = Some(self.display_name.read(cx).text().trim().to_owned()).filter(|n| !n.is_empty()).unwrap_or_else(|| me.clone());
        let face_url = self.avatar_ref.as_deref().and_then(|r| client.home_media_url(r));
        let look = profile::Look {
            banner: self.banner_ref.as_deref().and_then(|r| client.home_media_url(r)),
            accent: self.accent,
            theme: self.theme,
        };
        let pronouns = self.pronouns.read(cx).text().trim().to_owned();
        let bio = self.bio.read(cx).text().trim().to_owned();
        let face = Avatar::new(me.clone(), name.clone()).size(px(profile::FACE_SIZE)).image(face_url.map(Into::into)).into_any_element();
        profile::frame(
            &look,
            t,
            div().flex().flex_col().child(profile::header(&look, face, t)).child(
                div()
                    .px_4()
                    .pb_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(div().text_size(text::LG).font_weight(FontWeight::SEMIBOLD).child(name))
                            .child(div().text_size(text::SM).text_color(t.text_faint).child(me))
                            .when(!pronouns.is_empty(), |d| d.child(div().text_size(text::SM).text_color(t.text_muted).child(pronouns))),
                    )
                    .when(!bio.is_empty(), |d| {
                        d.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(div().font_family(FONT_MONO).text_size(text::XS).text_color(t.text_faint).child("about me"))
                                .child(div().text_size(text::SM).child(bio)),
                        )
                    }),
            ),
        )
        .into_any_element()
    }

    /// Banner, accent and theme (the preview beside the fields shows them).
    fn look_editor(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        // A row of colour swatches with "none" first; `pick` applies a choice.
        let swatches = |id: &'static str, current: Option<u32>, pick: fn(&mut Self, Option<u32>), cx: &mut Context<Self>| {
            let mut row = div().flex().flex_wrap().gap_2();
            let choices = std::iter::once(None).chain(profile::ACCENTS.iter().map(|(_, c)| Some(*c)));
            for (i, choice) in choices.enumerate() {
                let selected = current == choice;
                let label = choice.map_or("None", |c| profile::ACCENTS.iter().find(|(_, x)| *x == c).map_or("", |(n, _)| *n));
                row = row.child(
                    div()
                        .id((id, i))
                        .size(px(28.))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .tooltip(rhein_ui::tooltip(label))
                        .map(|d| match choice {
                            Some(c) => d.bg(profile::rgb(c)),
                            None => d.border_1().border_color(t.border_strong).child(icon(Icon::X).text_color(t.text_faint)),
                        })
                        .when(selected, |d| d.border_2().border_color(t.text))
                        .when(selected && choice.is_some(), |d| d.child(icon(Icon::Check).text_color(gpui::white())))
                        .on_click(cx.listener(move |v, _, _, cx| {
                            pick(v, choice);
                            v.look_dirty = true;
                            cx.notify();
                        })),
                );
            }
            row
        };
        let label = |title: &'static str, hint: &'static str| {
            div()
                .flex()
                .flex_col()
                .child(div().text_size(text::SM).text_color(t.text_muted).child(title))
                .child(div().text_size(text::XS).text_color(t.text_faint).child(hint))
        };
        let top = self.theme.map(|[a, _]| a);
        let bottom = self.theme.map(|[_, b]| b);
        let banner = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(label("Banner", "A wide picture across the top of your card."))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(Button::new("banner-change", "Change banner").icon(Icon::Image).on_click(cx.listener(|v, _, _, cx| v.pick_banner(cx))))
                    .when(self.banner_ref.is_some(), |d| {
                        d.child(Button::new("banner-remove", "Remove").ghost().on_click(cx.listener(|v, _, _, cx| {
                            v.banner_ref = None;
                            v.banner_dirty = true;
                            cx.notify();
                        })))
                    }),
            );
        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(banner)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(label("Accent", "Behind your name, wherever your card is drawn without a banner."))
                    .child(swatches("accent", self.accent, |v, c| v.accent = c, cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(label("Theme", "The border and background of your card, from top to bottom."))
                    // Picking one end fills the other if it was empty; "none" on either clears both.
                    .child(swatches("theme-top", top, |v, c| v.theme = c.map(|c| [c, v.theme.map_or(c, |[_, b]| b)]), cx))
                    .child(swatches("theme-bottom", bottom, |v, c| v.theme = c.map(|c| [v.theme.map_or(c, |[a, _]| a), c]), cx)),
            )
            .into_any_element()
    }

    fn devices_section(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut list = div().flex().flex_col().gap_2();
        match &self.devices {
            None => list = list.child(div().text_color(t.text_faint).child("Loading…")),
            Some(Err(e)) => list = list.child(div().text_color(t.danger).child(format!("Couldn't load devices: {e}"))),
            Some(Ok(devices)) => {
                for (i, d) in devices.iter().filter(|d| d.active).enumerate() {
                    let id = d.device_id.clone();
                    let seen = d.last_seen.map(|ms| format!("Last active {}", date(ms))).unwrap_or_default();
                    list = list.child(
                        div()
                            .p_3()
                            .rounded(radius::MD)
                            .bg(t.bg_panel)
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(icon(Icon::Monitor).text_color(t.text_muted))
                            .child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .flex_col()
                                    .child(div().font_weight(FontWeight::MEDIUM).child(d.name.clone().unwrap_or_else(|| "Unnamed device".into())))
                                    .child(div().text_size(text::XS).text_color(t.text_faint).child(format!("{} · {seen}", d.device_id))),
                            )
                            .child(if d.current {
                                div().text_size(text::SM).text_color(t.success).child("This device").into_any_element()
                            } else {
                                Button::new(("revoke", i), "Sign out").small()
                                    .ghost()
                                    .on_click(cx.listener(move |v, _, _, cx| {
                                        let id = id.clone();
                                        v.client.update(cx, |c, cx| c.revoke_device(id, cx));
                                        v.reload_devices(cx);
                                    }))
                                    .into_any_element()
                            }),
                    );
                }
            }
        }
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_color(t.text_muted).child("Everywhere this account is signed in. Signing a device out ends its session immediately."))
            .child(list)
            .into_any_element()
    }

    fn servers_section(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let client = self.client.read(cx);
        let home = client.home.as_ref().map(|h| h.doc.server_name.clone());
        let mut list = div().flex().flex_col().gap_2();
        for (i, s) in client.servers.iter().enumerate() {
            let is_home = home.as_deref() == Some(s.name.as_str());
            let guilds = client.guilds.iter().filter(|g| g.server == s.name).count();
            let color = if s.status == ServerStatus::Ready { t.success } else { t.warning };
            let name = s.name.clone();
            list = list.child(
                div()
                    .p_3()
                    .rounded(radius::MD)
                    .bg(t.bg_panel)
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().size(px(8.)).rounded_full().bg(color))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(div().font_weight(FontWeight::MEDIUM).child(s.name.clone()))
                                    .when(is_home, |d| d.child(div().text_size(text::XS).text_color(t.accent).child("HOME"))),
                            )
                            .child(div().text_size(text::XS).text_color(t.text_faint).child(format!(
                                "{} · {} · {guilds} guild{}",
                                s.doc.as_ref().map(|d| d.api.clone()).unwrap_or_else(|| s.input.clone()),
                                s.status.label(),
                                if guilds == 1 { "" } else { "s" }
                            )))
                            .children(client.server_advice(&s.name).map(|a| div().text_size(text::XS).text_color(t.text_muted).child(a)))
                            .children(s.error.clone().filter(|_| s.status != ServerStatus::Ready).map(|e| div().text_size(text::XS).text_color(t.danger).child(e))),
                    )
                    .when(!is_home, |d| {
                        d.child(Button::new(("remove-server", i), "Remove").small().ghost().on_click(cx.listener(move |v, _, _, cx| {
                            let name = name.clone();
                            v.client.update(cx, |c, cx| c.remove_server(&name, cx));
                        })))
                    }),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_color(t.text_muted).child("Your account lives on your home server. Other servers host guilds you joined; Rhein signs in to them with your account."))
            .child(list)
            .child(
                div().flex().items_center().gap_2().child(
                    Button::new("copy-diagnostics", "Copy diagnostics").small().icon(Icon::Copy).on_click(cx.listener(|v, _, _, cx| {
                        let report = v.client.read(cx).diagnostics();
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(report));
                        v.client.update(cx, |c, cx| c.flash_notice("Diagnostics copied: versions and connection states, no messages or names.", cx));
                    })),
                ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(div().flex_1().child(self.add_server.clone()))
                    .child(Button::new("add-server", "Add server").on_click(cx.listener(|v, _, _, cx| {
                        let text = v.add_server.read(cx).text().trim().to_owned();
                        if !text.is_empty() {
                            v.client.update(cx, |c, cx| c.add_server(text, cx));
                            v.add_server.update(cx, |i, cx| i.clear(cx));
                        }
                    }))),
            )
            .into_any_element()
    }

    fn appearance(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let prefs = self.client.read(cx).prefs.clone();
        let row = |title: &'static str, detail: &'static str, control: AnyElement| {
            div()
                .p_3()
                .rounded(radius::MD)
                .border_1()
                .border_color(t.border)
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().font_weight(FontWeight::MEDIUM).child(title))
                        .child(div().text_size(text::SM).text_color(t.text_faint).child(detail)),
                )
                .child(control)
        };
        let client = self.client.clone();
        let size_control = div()
            .flex()
            .gap_1()
            .children([80u16, 90, 100, 115, 130].into_iter().map(|scale| {
                let active = prefs.text_scale == scale;
                let client = client.clone();
                div()
                    .id(("scale", scale as usize))
                    .px_2()
                    .py_1()
                    .rounded(radius::MD)
                    .border_1()
                    .border_color(if active { t.accent } else { t.border_strong })
                    .text_size(text::SM)
                    .cursor_pointer()
                    .on_click(move |_, _, cx| client.update(cx, |c, cx| c.update_prefs(cx, |p| p.text_scale = scale)))
                    .child(format!("{scale}%"))
            }))
            .into_any_element();
        let (c1, c2, c3) = (self.client.clone(), self.client.clone(), self.client.clone());
        // A switch for one of this device's preferences.
        let toggle = |id: &'static str, on: bool, set: fn(&mut rhein_store::prefs::Prefs, bool)| {
            let client = self.client.clone();
            Switch::new(id, on).on_toggle(move |on, _, cx| client.update(cx, |c, cx| c.update_prefs(cx, |p| set(p, on)))).into_any_element()
        };
        let more = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(heading("chat", t))
            .child(row("Link previews", "Unfold links into a card with their title and picture.", toggle("link-previews", prefs.link_previews, |p, on| p.link_previews = on)))
            .child(row("Show when I'm typing", "Others see \"… is typing\" while you write to them.", toggle("send-typing", prefs.send_typing, |p, on| p.send_typing = on)))
            .child(heading("sidebar", t))
            .child(row("Hide muted channels", "Muted channels leave the sidebar, unless open or mentioning you.", toggle("hide-muted", prefs.hide_muted, |p, on| p.hide_muted = on)))
            .child(row("Server status dots", "A dot beside each server: green when connected, yellow while connecting, red when it can't be reached.", toggle("server-dots", prefs.server_dots, |p, on| p.server_dots = on)))
            .child(heading("advanced", t))
            .child(row("Developer mode", "\"Copy ID\" entries in the guild, channel and member menus.", toggle("developer-mode", prefs.developer_mode, |p, on| p.developer_mode = on)));
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(heading("display", t))
            .child(row("Light theme", "Light surfaces and dark text instead of the dark look.", toggle("light-theme", prefs.light_theme, |p, on| p.light_theme = on)))
            .child(row("Message text size", "How large message text is drawn.", size_control))
            .child(row(
                "Compact messages",
                "Smaller avatars and less space between messages.",
                Switch::new("compact", prefs.compact)
                    .on_toggle(move |on, _, cx| c1.update(cx, |c, cx| c.update_prefs(cx, |p| p.compact = on)))
                    .into_any_element(),
            ))
            .child(row(
                "24-hour clock",
                "Show times as 14:05 instead of 2:05 PM.",
                Switch::new("clock24", prefs.clock_24h)
                    .on_toggle(move |on, _, cx| c2.update(cx, |c, cx| c.update_prefs(cx, |p| p.clock_24h = on)))
                    .into_any_element(),
            ))
            .child(row(
                "Reduce motion",
                "Show changes instantly: no fades, glides or blinking cursor.",
                Switch::new("reduce-motion", prefs.reduce_motion)
                    .on_toggle(move |on, _, cx| c3.update(cx, |c, cx| c.update_prefs(cx, |p| p.reduce_motion = on)))
                    .into_any_element(),
            ))
            .child(more)
            .into_any_element()
    }

    /// Alerts, as the web client offers them; the choices are the account's, on every device.
    fn notifications(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let client = self.client.read(cx);
        let preview = client.account_pref(&["notificationPreview"]).and_then(|v| v.as_str()).unwrap_or("full").to_owned();
        let sounds = client.account_flag(&["notifySounds"], false);
        let c = self.client.clone();
        let preview_control = div()
            .flex()
            .gap_1()
            .children([("full", "Message"), ("sender", "Sender only"), ("hidden", "Nothing")].into_iter().enumerate().map(|(i, (value, label))| {
                let active = preview == value;
                let c = c.clone();
                div()
                    .id(("notify-preview", i))
                    .px_2()
                    .py_1()
                    .rounded(radius::MD)
                    .border_1()
                    .border_color(if active { t.accent } else { t.border_strong })
                    .text_size(text::SM)
                    .cursor_pointer()
                    .on_click(move |_, _, cx| c.update(cx, |c, cx| c.set_account_pref(&["notificationPreview"], Some(value.into()), cx)))
                    .child(label)
            }))
            .into_any_element();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(heading("Alerts", t))
            .child(setting_row(t, "What an alert shows", "The message itself, only who sent it, or neither.", preview_control))
            .child(setting_row(t, "Mentions", "When someone mentions you while Rhein is in the background.", account_switch(&self.client, "notify-mentions", &["notifyMentions"], true, cx)))
            .child(setting_row(t, "Private messages", "Every message in your conversations while Rhein is in the background.", account_switch(&self.client, "notify-dms", &["notifyDms"], true, cx)))
            .child(setting_row(t, "Sounds", "A ping with mentions and private messages.", account_switch(&self.client, "notify-sounds", &["notifySounds"], false, cx)))
            .child(
                div().when(!sounds, |d| d.opacity(0.5)).child(setting_row(
                    t,
                    "Sound for every message",
                    "A softer sound for any other message, in channels you are not looking at.",
                    if sounds { account_switch(&self.client, "notify-sound-messages", &["notifySoundMessages"], false, cx) } else { Switch::new("notify-sound-messages", false).into_any_element() },
                )),
            )
            .child(setting_row(t, "Ring for calls", "The ringtone when someone calls you in a conversation.", account_switch(&self.client, "notify-ring", &["notifyCallRing"], true, cx)))
            .child(div().pt_2().text_size(text::SM).text_color(t.text_faint).child("Nothing alerts while you are set to Do not disturb. These choices follow your account to your other devices."))
            .into_any_element()
    }

    /// What conversations are told about your reading, and what your public profile page shows.
    fn privacy(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let me = self.client.read(cx).user_id().unwrap_or_default().to_owned();
        let profile = self.client.update(cx, |c, cx| c.profile(&me, cx));
        let public = |field: &'static str, id: &'static str, on: bool| {
            let client = self.client.clone();
            Switch::new(id, on)
                .on_toggle(move |on, _, cx| {
                    client.update(cx, |c, cx| c.save_profile(serde_json::json!({ field: on }), |result, _| {
                        if let Err(e) = result {
                            log::warn!("profile privacy: {e}");
                        }
                    }, cx))
                })
                .into_any_element()
        };
        let page = profile.as_ref().and_then(|p| p.public_page_enabled).unwrap_or(true);
        let status = profile.as_ref().and_then(|p| p.public_status_enabled).unwrap_or(false);
        let activity = profile.as_ref().and_then(|p| p.public_activity_enabled).unwrap_or(false);
        let link = format!("https://app.telium.chat/u/{me}");
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(heading("In conversations", t))
            .child(setting_row(
                t,
                "Send and receive read receipts",
                "Tell a conversation when you have read it, and see when others have. Off, you see no one's either.",
                account_switch(&self.client, "signal-read", &["signals", "read"], false, cx),
            ))
            .child(setting_row(
                t,
                "Acknowledge delivery",
                "Let senders know this device has their message. Off, you don't see theirs either.",
                account_switch(&self.client, "signal-delivered", &["signals", "delivered"], true, cx),
            ))
            .child(setting_row(t, "Show when I am typing", "Others in a conversation see that you are writing.", account_switch(&self.client, "signal-typing", &["signals", "typing"], true, cx)))
            .child(heading("Public profile link", t))
            .child(setting_row(t, "Enable public profile link", "Anyone with your profile URL can see your public profile without signing in.", public("public_page_enabled", "public-page", page)))
            .child(div().when(!page, |d| d.opacity(0.5)).child(setting_row(
                t,
                "Show status",
                "Publish your presence and custom status. Anyone with the link can refresh it to see when you are at your computer.",
                public("public_status_enabled", "public-status", status),
            )))
            .child(div().when(!page, |d| d.opacity(0.5)).child(setting_row(
                t,
                "Show activity",
                "Publish what you are doing to anyone with the link, including whatever the reporting app sends.",
                public("public_activity_enabled", "public-activity", activity),
            )))
            .child(
                div().pt_1().flex().child(Button::new("copy-public-link", "Copy public profile link").icon(Icon::Copy).on_click(move |_, _, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(link.clone()));
                })),
            )
            .child(div().pt_2().text_size(text::SM).text_color(t.text_faint).child("Public profile pages are never indexed by search engines. The conversation choices follow your account to your other devices."))
            .into_any_element()
    }

    /// Shows two-factor setup with a made-up key, to look at (`RHEIN_DEV_OPEN=mfa-preview`).
    #[cfg(debug_assertions)]
    pub fn dev_mfa_preview(&mut self, cx: &mut Context<Self>) {
        self.open(Section::Security, cx);
        self.security.setup = Some(serde_json::json!({
            "secret": "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP",
            "otpauth_uri": "otpauth://totp/Telium:%40anna%3Alocalhost?secret=JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP&issuer=Telium&digits=6&period=30",
        }));
        cx.notify();
    }

    fn reload_security(&mut self, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        self.client.update(cx, |c, cx| {
            c.load_security(
                move |result, cx| {
                    weak.update(cx, |v, cx| {
                        v.security.state = Some(result);
                        cx.notify();
                    })
                    .ok();
                },
                cx,
            )
        });
    }

    /// Runs one change of the Account & security page; what it answers is shown on the page.
    fn security_action(&mut self, action: SecurityAction, cx: &mut Context<Self>) {
        let password = self.security.password.read(cx).text().to_owned();
        let code = self.security.code.read(cx).text().trim().to_owned();
        let email = self.security.email.read(cx).text().trim().to_owned();
        let needs_password = !matches!(action, SecurityAction::EnableMfa | SecurityAction::SignOutAll | SecurityAction::Export);
        if needs_password && password.is_empty() {
            self.security.status = Some((false, "Enter your password first.".into()));
            cx.notify();
            return;
        }
        if matches!(action, SecurityAction::EnableMfa | SecurityAction::DisableMfa) && code.is_empty() {
            self.security.status = Some((false, "Enter the code from your authenticator app.".into()));
            cx.notify();
            return;
        }
        // What can't be taken back takes a second click.
        let second = match action {
            SecurityAction::Close { delete: true } => Some(("delete", "Click Delete account again to delete it for good.")),
            SecurityAction::Close { delete: false } => Some(("deactivate", "Click Deactivate again to confirm.")),
            SecurityAction::SignOutAll => Some(("sign-out", "Click Sign out everywhere again to sign out every device.")),
            _ => None,
        };
        if let Some((key, ask)) = second
            && self.security.confirm != Some(key)
        {
            self.security.confirm = Some(key);
            self.security.status = Some((false, ask.into()));
            cx.notify();
            return;
        }
        if matches!(action, SecurityAction::SignOutAll) {
            self.client.update(cx, |c, cx| c.sign_out_everywhere(cx));
            return;
        }
        self.security.busy = true;
        self.security.status = None;
        let weak = cx.weak_entity();
        let finish = move |result: Result<serde_json::Value, String>, cx: &mut gpui::App, ok: &'static str| {
            weak.update(cx, |v, cx| {
                v.security.busy = false;
                v.security.confirm = None;
                match result {
                    Ok(answer) => {
                        v.security.status = Some((true, ok.into()));
                        if answer.get("otpauth_uri").is_some() {
                            v.security.setup = Some(answer.clone());
                        }
                        if let Some(codes) = answer["recovery_codes"].as_array() {
                            v.security.setup = None;
                            v.security.codes = Some(codes.iter().filter_map(|c| c.as_str().map(str::to_owned)).collect());
                        }
                        v.security.password.update(cx, |i, cx| i.clear(cx));
                        v.security.code.update(cx, |i, cx| i.clear(cx));
                        v.reload_security(cx);
                    }
                    Err(e) => v.security.status = Some((false, e)),
                }
                cx.notify();
            })
            .ok();
        };
        self.client.update(cx, |c, cx| match action {
            SecurityAction::SetupMfa => c.mfa_setup(password, move |r, cx| finish(r, cx, "Scan the code with your authenticator app, then enter the code it shows."), cx),
            SecurityAction::EnableMfa => c.mfa_enable(code, move |r, cx| finish(r, cx, "Two-factor sign-in is on. Keep the recovery codes somewhere safe."), cx),
            SecurityAction::DisableMfa => c.mfa_disable(password, code, move |r, cx| finish(r, cx, "Two-factor sign-in is off."), cx),
            SecurityAction::SetEmail => c.set_recovery_email(password, Some(email), move |r, cx| finish(r, cx, "Saved. Confirm the address from the mail sent to it."), cx),
            SecurityAction::RemoveEmail => c.set_recovery_email(password, None, move |r, cx| finish(r, cx, "The recovery address is removed."), cx),
            SecurityAction::Close { delete } => c.close_account(password, delete, move |r, cx| finish(r, cx, "Done."), cx),
            SecurityAction::Export => c.save_account_data(None, move |r, cx| finish(r, cx, "Saved to your Downloads folder."), cx),
            SecurityAction::Bundle => c.save_account_data(Some(password), move |r, cx| finish(r, cx, "The signed bundle is in your Downloads folder."), cx),
            SecurityAction::SignOutAll => {}
        });
        cx.notify();
    }

    /// The recovery address, two-factor sign-in, signing out everywhere and closing the account,
    /// as the web client's Account and Security panes have them.
    fn security_section(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let form = &self.security;
        let state = match &form.state {
            None => return div().text_color(t.text_faint).child("Loading…").into_any_element(),
            Some(Err(e)) => return div().text_color(t.danger).child(format!("Couldn't load your account's security: {e}")).into_any_element(),
            Some(Ok(state)) => state.clone(),
        };
        let (mfa, recovery) = (&state["mfa"], &state["recovery"]);
        let enabled = mfa["enabled"].as_bool().unwrap_or(false);
        let field = |label: &'static str, input: &Entity<TextInput>| {
            div().flex().flex_col().gap_1p5().child(div().text_size(text::SM).text_color(t.text_muted).child(label)).child(input.clone())
        };
        let action = |id: &'static str, label: &'static str, what: SecurityAction, cx: &mut Context<Self>| {
            Button::new(id, label).disabled(form.busy).on_click(cx.listener(move |v, _, _, cx| v.security_action(what, cx)))
        };

        // The recovery address.
        let mailer = recovery["mailer"].as_bool().unwrap_or(false);
        let address = match (recovery["verified_email"].as_str(), recovery["pending_email"].as_str()) {
            (Some(v), _) => format!("{v} (confirmed)"),
            (None, Some(p)) => format!("{p} (waiting for confirmation)"),
            _ => "None set".into(),
        };
        let recovery_block = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(heading("Recovery email", t))
            .child(div().text_size(text::SM).text_color(t.text_muted).child("Where a warning goes if someone tries to move your account elsewhere."))
            .child(div().child(address))
            .when(!mailer, |d| d.child(div().text_size(text::SM).text_color(t.text_faint).child("Your home server can't send mail, so it keeps no recovery address.")))
            .when(mailer, |d| {
                d.child(field("New address", &form.email)).child(
                    div()
                        .flex()
                        .gap_2()
                        .child(action("email-save", "Save address", SecurityAction::SetEmail, cx))
                        .when(recovery["verified_email"].is_string() || recovery["pending_email"].is_string(), |d| {
                            d.child(action("email-remove", "Remove", SecurityAction::RemoveEmail, cx).ghost())
                        }),
                )
            });

        // Two-factor sign-in.
        let mut mfa_block = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(heading("Two-factor sign-in", t))
            .child(div().text_size(text::SM).text_color(t.text_muted).child(if enabled {
                format!("On. {} recovery codes left.", mfa["recovery_codes_remaining"].as_u64().unwrap_or(0))
            } else {
                "Off. With it on, signing in also takes a code from an authenticator app.".into()
            }));
        if let Some(setup) = &form.setup {
            let secret = setup["secret"].as_str().unwrap_or_default().to_owned();
            let uri = setup["otpauth_uri"].as_str().unwrap_or_default().to_owned();
            let copy = secret.clone();
            mfa_block = mfa_block
                .child(div().flex().gap_4().items_center().child(qr_code(&uri)).child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().text_size(text::SM).text_color(t.text_muted).child("Or enter this key:"))
                        .child(div().font_family(FONT_MONO).text_size(text::SM).child(secret))
                        .child(div().flex().child(Button::new("mfa-copy", "Copy key").ghost().icon(Icon::Copy).on_click(move |_, _, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone()))
                        }))),
                ))
                .child(field("Code from the app", &form.code))
                .child(div().flex().child(action("mfa-enable", "Turn on", SecurityAction::EnableMfa, cx).primary()));
        } else if let Some(codes) = &form.codes {
            let all = codes.join("\n");
            mfa_block = mfa_block
                .child(div().text_size(text::SM).text_color(t.warning).child("Recovery codes: each signs you in once if your app is lost. They are shown only now."))
                .child(div().flex().flex_wrap().gap_2().children(codes.iter().map(|c| {
                    div().px_2().py_1().rounded(radius::SM).bg(t.bg_raised).font_family(FONT_MONO).text_size(text::SM).child(c.clone())
                })))
                .child(div().flex().child(Button::new("mfa-codes-copy", "Copy codes").icon(Icon::Copy).on_click(move |_, _, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(all.clone()))
                })));
        } else if enabled {
            mfa_block = mfa_block
                .child(field("Code from the app (or a recovery code)", &form.code))
                .child(div().flex().child(action("mfa-disable", "Turn off", SecurityAction::DisableMfa, cx).danger()));
        } else {
            mfa_block = mfa_block.child(div().flex().child(action("mfa-setup", "Set up", SecurityAction::SetupMfa, cx)));
        }

        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(field("Your password", &form.password))
            .child(div().text_size(text::XS).text_color(t.text_faint).mt(px(-8.)).child("Every change on this page asks for it again."))
            .children(form.status.clone().map(|(ok, msg)| div().text_size(text::SM).text_color(if ok { t.success } else { t.danger }).child(msg)))
            .child(recovery_block)
            .child(mfa_block)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(heading("Sessions", t))
                    .child(div().text_size(text::SM).text_color(t.text_muted).child("Signs out every device of this account, this one too. The Devices page lists them one by one."))
                    .child(div().flex().child(action("sign-out-all", "Sign out everywhere", SecurityAction::SignOutAll, cx).danger())),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(heading("Your data", t))
                    .child(div().text_size(text::SM).text_color(t.text_muted).child(
                        "A copy of what your home server holds about the account. Guild history on other servers is not in it. The signed bundle (it asks for your password) is the record to keep when a server shuts down.",
                    ))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(action("account-export", "Download your data", SecurityAction::Export, cx))
                            .child(action("account-bundle", "Download signed bundle", SecurityAction::Bundle, cx)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(heading("Close your account", t))
                    .child(div().text_size(text::SM).text_color(t.text_muted).child("Deactivating hides the account until you sign in again. Deleting removes it for good."))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(action("account-deactivate", "Deactivate", SecurityAction::Close { delete: false }, cx).danger())
                            .child(action("account-delete", "Delete account", SecurityAction::Close { delete: true }, cx).danger()),
                    ),
            )
            .into_any_element()
    }

    fn import_gifs(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Import GIF favourites".into()) });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let read = std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).map_err(|_| "That file is not valid JSON.".to_owned()));
            this.update(cx, |v, cx| {
                v.extras_status = Some(match read {
                    Ok(doc) => {
                        let urls = rhein_store::gif_links::favourites_in_export(&doc);
                        if urls.is_empty() {
                            (false, "No https GIF links were found in that file.".into())
                        } else {
                            let (added, dropped) = v.client.update(cx, |c, cx| c.import_gif_favorites(urls, cx));
                            let over = if dropped > 0 { format!(" {dropped} did not fit (500 at most).") } else { String::new() };
                            (true, format!("Imported {added} GIF favourite{}.{over}", if added == 1 { "" } else { "s" }))
                        }
                    }
                    Err(e) => (false, e),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn upload_gifs(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Upload GIFs as favourites".into()) });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            this.update(cx, |v, cx| {
                v.extras_status = Some((true, format!("Uploading {} file{}…", paths.len(), if paths.len() == 1 { "" } else { "s" })));
                let weak = cx.weak_entity();
                v.client.update(cx, |c, cx| {
                    c.upload_gif_favorites(
                        paths,
                        move |added, failed, cx| {
                            weak.update(cx, |v, cx| {
                                let tail = if failed.is_empty() { String::new() } else { format!(" Not added: {}", failed.join("; ")) };
                                v.extras_status = Some((failed.is_empty(), format!("Added {added} GIF favourite{}.{tail}", if added == 1 { "" } else { "s" })));
                                cx.notify();
                            })
                            .ok();
                        },
                        cx,
                    )
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// GIF favourites in bulk, what this device keeps, and how connections are made.
    fn extras(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let client = self.client.read(cx);
        let (count, ready) = (client.gif_favorites().len(), client.gif_favorites_ready());
        let c1 = self.client.clone();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .children(self.extras_status.clone().map(|(ok, msg)| div().text_size(text::SM).text_color(if ok { t.success } else { t.danger }).child(msg)))
            .child(heading("GIF favourites", t))
            .child(setting_row(
                t,
                "Import favourites",
                "From a JSON export, such as Discord's; added after the ones you have.",
                Button::new("gif-import", "Choose file…").disabled(!ready).on_click(cx.listener(|v, _, _, cx| v.import_gifs(cx))).into_any_element(),
            ))
            .child(setting_row(
                t,
                "Upload GIFs",
                "GIF or MP4 files from this computer, each kept as a favourite.",
                Button::new("gif-upload", "Choose files…").disabled(!ready).on_click(cx.listener(|v, _, _, cx| v.upload_gifs(cx))).into_any_element(),
            ))
            .child(div().text_size(text::SM).text_color(t.text_faint).child(if ready {
                format!("{count} of 500 favourites. Remove one with its star in the GIF menu.")
            } else {
                "Favourites open once this device has your account's keys.".to_owned()
            }))
            .child(heading("Local data", t))
            .child(setting_row(
                t,
                "Clear cached data",
                "The game list, app names, link previews and resolved GIF links. It all comes back when needed.",
                Button::new("clear-cache", "Clear")
                    .on_click(cx.listener(move |v, _, _, cx| {
                        c1.update(cx, |c, cx| c.clear_cache(cx));
                        v.extras_status = Some((true, "Cached data cleared.".into()));
                        cx.notify();
                    }))
                    .into_any_element(),
            ))
            .child(setting_row(
                t,
                "Remove this device's keys",
                "Signs out here and deletes this device's copy of your encryption keys and messages. Your account and other devices are not affected.",
                Button::new("wipe-device", if self.extras_confirm { "Click again to remove" } else { "Sign out and remove" })
                    .danger()
                    .on_click(cx.listener(|v, _, _, cx| {
                        if v.extras_confirm {
                            v.client.update(cx, |c, cx| c.sign_out(cx));
                        } else {
                            v.extras_confirm = true;
                            cx.notify();
                        }
                    }))
                    .into_any_element(),
            ))
            .child(heading("Network privacy", t))
            .child(self.network_privacy(t, cx))
            .into_any_element()
    }

    /// How connections to guild servers are made: directly, or through the home's relay so
    /// guild servers don't learn this address (`prefs.network`, shared with the web client).
    fn network_privacy(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        const MODES: [(&str, &str); 3] = [("direct", "Direct"), ("protected", "Protect my IP"), ("per-guild", "Choose per guild")];
        let client = self.client.read(cx);
        let mode = client.account_pref(&["network", "mode"]).and_then(|v| v.as_str()).unwrap_or("direct").to_owned();
        let fallback = client.account_flag(&["network", "autoFallback"], false);
        let relay = client.relay_available();
        let guilds: Vec<(String, String, bool)> = client
            .guilds
            .iter()
            .map(|g| {
                let on = client.account_pref(&["network", "guilds", &g.guild.id]).and_then(|v| v.as_str()) == Some("protected");
                (g.guild.id.clone(), format!("{} · {}", g.guild.name, g.server), on)
            })
            .collect();
        let chosen = MODES.iter().position(|(m, _)| *m == mode).unwrap_or(0);
        let this = self.client.clone();
        let set = move |path: Vec<String>, value: serde_json::Value, cx: &mut gpui::App| {
            this.update(cx, |c, cx| {
                let path: Vec<&str> = path.iter().map(String::as_str).collect();
                c.set_account_pref(&path, Some(value), cx);
                // Every connection is made again the way now chosen.
                c.reconnect_all(cx);
            });
        };
        let set_mode = set.clone();
        let mut block = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(setting_row(
                t,
                "How connections are made",
                "\"Protect my IP\" sends guild traffic through your account server's relay, so guild servers see its address instead of yours. The relay only passes encrypted bytes; it can't read them. Calls are not covered.",
                Tabs::new("network-mode", MODES.iter().map(|(_, l)| *l))
                    .selected(chosen)
                    .on_select(move |i, _, cx| set_mode(vec!["network".into(), "mode".into()], MODES[i].0.into(), cx))
                    .into_any_element(),
            ));
        if !relay {
            block = block.child(div().text_size(text::SM).text_color(t.warning).child(
                "Your account server doesn't run a relay, so protected guilds can't be reached (unless connecting directly is allowed below).",
            ));
        }
        if mode == "per-guild" {
            for (id, label, on) in guilds {
                let set = set.clone();
                block = block.child(setting_row_owned(
                    t,
                    label,
                    if on { "Through the relay." } else { "Direct." },
                    Switch::new(SharedString::from(format!("net-{id}")), on)
                        .on_toggle(move |on, _, cx| set(vec!["network".into(), "guilds".into(), id.clone()], if on { "protected" } else { "direct" }.into(), cx))
                        .into_any_element(),
                ));
            }
        }
        let set_fallback = set.clone();
        block = block.child(setting_row(
            t,
            "Connect directly if the relay is down",
            "Off, a protected guild waits until the relay works (or you choose to connect directly that once). On, it connects directly without asking.",
            Switch::new("net-fallback", fallback)
                .on_toggle(move |on, _, cx| set_fallback(vec!["network".into(), "autoFallback".into()], on.into(), cx))
                .into_any_element(),
        ));
        block.into_any_element()
    }

    /// Rich presence: what is shared, from which sources, and what is being shared right now.
    fn activity_section(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let client = self.client.read(cx);
        let prefs = client.prefs.clone();
        let status = client.rpc_status.clone();
        let found = client.local_activities.clone();
        let shared = client.shared_activities().to_vec();
        let row = |title: &'static str, detail: &'static str, control: AnyElement| {
            div()
                .p_3()
                .rounded(radius::MD)
                .border_1()
                .border_color(t.border)
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().font_weight(FontWeight::MEDIUM).child(title))
                        .child(div().text_size(text::SM).text_color(t.text_faint).child(detail)),
                )
                .child(control)
        };
        let toggle = |id: &'static str, on: bool, set: fn(&mut rhein_store::prefs::Prefs, bool)| {
            let client = self.client.clone();
            Switch::new(id, on).on_toggle(move |on, _, cx| client.update(cx, |c, cx| c.update_prefs(cx, |p| set(p, on)))).into_any_element()
        };
        let mono = |s: String| div().font_family(FONT_MONO).text_size(text::XS).text_color(t.text_faint).child(s);

        let sources = if !prefs.share_activity {
            div().child(mono("sharing is off".into()))
        } else {
            let socket = status.socket.as_ref().map_or("not listening (another app holds every slot)".to_owned(), |p| p.display().to_string());
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(mono(format!("discord rpc   {socket}")))
                .child(mono(format!("websocket     {}", status.websocket_port.map_or("not listening".to_owned(), |p| format!("127.0.0.1:{p}")))))
                .child(mono(format!("rpc apps      {}", if status.clients.is_empty() { "none connected".to_owned() } else { status.clients.len().to_string() })))
                .child(mono(format!("games         {}", if status.detected.is_empty() { "none running".to_owned() } else { status.detected.join(", ") })))
                .child(mono(format!("media players {}", if status.players.is_empty() { "none".to_owned() } else { status.players.join(", ") })))
                .child(mono(format!("arrpc bridge  {}", if status.bridge { "connected (127.0.0.1:1337)" } else { "not running" })))
        };

        // Everything found, with whether it goes out; any of it can be hidden by name.
        let mut list = div().flex().flex_col().gap_2();
        if found.is_empty() && prefs.hidden_activities.is_empty() {
            list = list.child(div().text_size(text::SM).text_color(t.text_faint).child("Nothing detected right now."));
        }
        for (i, a) in found.iter().enumerate() {
            let is_shared = shared.contains(a);
            let name = a.name.clone();
            let client = self.client.clone();
            list = list.child(
                div()
                    .p_3()
                    .rounded(radius::MD)
                    .border_1()
                    .border_color(t.border)
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(icon(match a.kind { 2 => Icon::Music, 1 | 3 => Icon::Monitor, _ => Icon::Gamepad }).text_color(t.text_faint))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().truncate().child(a.summary()))
                            .child(mono(if is_shared { "shared".into() } else { "not shared".into() })),
                    )
                    .child(Button::new(("hide-activity", i), "Don't share").small().ghost().on_click(move |_, _, cx| {
                        let name = name.clone();
                        client.update(cx, |c, cx| {
                            c.update_prefs(cx, |p| {
                                if !p.hidden_activities.contains(&name) {
                                    p.hidden_activities.push(name);
                                }
                            })
                        });
                    })),
            );
        }
        for (i, name) in prefs.hidden_activities.iter().enumerate() {
            let (client, name) = (self.client.clone(), name.clone());
            list = list.child(
                div()
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().flex_1().text_size(text::SM).text_color(t.text_muted).child(format!("{name} is never shared")))
                    .child(Button::new(("unhide-activity", i), "Share again").small().ghost().on_click(move |_, _, cx| {
                        let name = name.clone();
                        client.update(cx, |c, cx| c.update_prefs(cx, |p| p.hidden_activities.retain(|n| *n != name)));
                    })),
            );
        }

        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(row(
                "Share my activity",
                "Show what you are playing, listening to or watching, like Discord. Apps using Discord Rich Presence report to Rhein directly.",
                toggle("share-activity", prefs.share_activity, |p, on| p.share_activity = on),
            ))
            .child(row("Games", "Games found running, and apps that report over Discord RPC.", toggle("share-games", prefs.share_games, |p, on| p.share_games = on)))
            .child(row("Music", "The song a media player is playing.", toggle("share-music", prefs.share_music, |p, on| p.share_music = on)))
            .child(row("Videos", "What a browser or video player is showing.", toggle("share-videos", prefs.share_videos, |p, on| p.share_videos = on)))
            .child(div().pt_4().pb_1().font_weight(FontWeight::SEMIBOLD).child("Sources"))
            .child(sources)
            .child(div().pt_4().pb_1().font_weight(FontWeight::SEMIBOLD).child("Right now"))
            .child(list)
            .into_any_element()
    }

    /// Encrypted DMs on this device: whether the recovery keyring is here, this device's key
    /// fingerprint, and whether the account's identity vouches for it.
    fn encryption_section(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let client = self.client.read(cx);
        let status = client.dm.clone();
        let error = client.dm_error.clone();
        let line = |label: &'static str, value: String, good: Option<bool>| {
            div()
                .flex()
                .gap_4()
                .py_1()
                .child(div().w(px(180.)).flex_none().text_color(t.text_muted).child(label))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_family(FONT_MONO)
                        .text_size(text::SM)
                        .text_color(match good {
                            Some(true) => t.success,
                            Some(false) => t.warning,
                            None => t.text,
                        })
                        .child(value),
                )
        };
        let mut body = div().flex().flex_col().gap_1().child(
            div()
                .pb_3()
                .text_size(text::SM)
                .text_color(t.text_faint)
                .child("Direct messages and group chats are end-to-end encrypted with MLS. Their keys never leave your devices; your home server keeps only a copy sealed with a key derived from your password."),
        );
        match &status {
            None => {
                body = body.child(line("Status", error.clone().unwrap_or_else(|| "starting…".into()), Some(false)));
            }
            Some(s) => {
                body = body
                    .child(line("Key recovery", s.vault_mode.replace('_', " "), None))
                    .child(line(
                        "Recovery key on this device",
                        s.locked.clone().map_or("available".into(), |why| format!("locked: {why}")),
                        Some(s.locked.is_none()),
                    ))
                    .child(line("This device's key", s.fingerprint(), None))
                    .child(line(
                        "Cross-signing",
                        match (&s.identity_key, s.cross_signed, &s.identity_problem) {
                            (_, _, Some(problem)) => format!("problem: {problem}"),
                            (None, _, _) => "no identity yet (set up by the Telium web client)".into(),
                            (Some(_), true, _) => "this device is vouched for".into(),
                            (Some(_), false, _) => "not vouched for yet".into(),
                        },
                        Some(s.identity_problem.is_none() && (s.identity_key.is_none() || s.cross_signed)),
                    ))
                    .children(s.immutable_id.clone().map(|id| line("Identity", id, None)));
                if let Some(err) = &error {
                    body = body.child(div().pt_2().text_size(text::SM).text_color(t.danger).child(err.clone()));
                }
            }
        }
        // A session restored without the password can't open the vault; proving it again does.
        if status.as_ref().is_none_or(|s| s.locked.is_some()) {
            let client = self.client.clone();
            let input = self.unlock_password.clone();
            body = body.child(
                div()
                    .pt_4()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().font_weight(FontWeight::MEDIUM).child("Unlock on this device"))
                    .child(div().text_size(text::SM).text_color(t.text_faint).child("Enter your password once. It is checked with OPAQUE, so it never leaves this device."))
                    .child(
                        div().flex().gap_2().child(div().flex_1().child(self.unlock_password.clone())).child(
                            Button::new("dm-unlock", "Unlock").primary().on_click(move |_, _, cx| {
                                let password = input.read(cx).text().to_owned();
                                if password.is_empty() {
                                    return;
                                }
                                input.update(cx, |i, cx| i.clear(cx));
                                client.update(cx, |c, cx| c.unlock_dm(password, cx));
                            }),
                        ),
                    ),
            );
        }
        body.into_any_element()
    }

    fn shortcuts(&self, t: &Theme) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .children(SHORTCUTS.iter().map(|(keys, what)| {
                div()
                    .px_3()
                    .py_2()
                    .rounded(radius::MD)
                    .bg(t.bg_panel)
                    .flex()
                    .justify_between()
                    .child(div().text_color(t.text_muted).child(*what))
                    .child(
                        div()
                            .px_2()
                            .rounded(radius::SM)
                            .bg(t.bg_raised)
                            .font_family(rhein_ui::theme::FONT_MONO)
                            .text_size(text::SM)
                            .child(*keys),
                    )
            }))
            .into_any_element()
    }

    fn about(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let client = self.client.read(cx);
        let check_updates = client.prefs.check_updates;
        let (status, button) = match &client.update {
            Update::Idle => ("Updates".to_owned(), Button::new("update-check", "Check now")),
            Update::Checking => ("Looking for updates…".to_owned(), Button::new("update-check", "Checking…").disabled(true)),
            Update::UpToDate => ("Rhein is up to date".to_owned(), Button::new("update-check", "Check again")),
            Update::Failed(err) => (err.clone(), Button::new("update-check", "Try again")),
            Update::Available(r) if client.can_install(r) => (format!("Rhein {} is out", r.version), Button::new("update-install", "Install").primary()),
            Update::Available(r) => (format!("Rhein {} is out; update it the way you installed it", r.version), Button::new("update-notes", "What's new")),
            Update::Installing(r) => (format!("Installing Rhein {}…", r.version), Button::new("update-install", "Installing…").disabled(true)),
            Update::Installed(r) => (format!("Rhein {} is installed", r.version), Button::new("update-restart", "Restart").primary()),
        };
        let c1 = self.client.clone();
        let button = button
            .on_click(move |_, _, cx| {
                c1.update(cx, |c, cx| match &c.update {
                    Update::Available(r) if c.can_install(r) => c.install_update(cx),
                    Update::Available(r) => cx.open_url(&r.page_url),
                    Update::Installed(_) => c.restart_for_update(cx),
                    _ => c.check_for_updates(false, cx),
                })
            })
            .into_any_element();
        let c2 = self.client.clone();
        let startup = Switch::new("check-updates", check_updates)
            .on_toggle(move |on, _, cx| c2.update(cx, |c, cx| c.update_prefs(cx, |p| p.check_updates = on)))
            .into_any_element();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .text_color(t.text_muted)
            .child(div().text_size(text::XL).font_weight(FontWeight::SEMIBOLD).text_color(t.text).child("Rhein"))
            .child(format!("Version {} – a native client for Telium, written in Rust with GPUI.", env!("CARGO_PKG_VERSION")))
            .child("Licensed under the Apache License 2.0.")
            .child("Bundled: Geist and Geist Mono (SIL Open Font License 1.1), Lucide icons (ISC licence), Unicode emoji data via the emojis crate.")
            .child(heading("updates", t))
            .child(setting_row_owned(t, status, "New releases come from GitHub and are checked against Rhein's release key. Nothing is installed until you say so, and your settings and data stay as they are.", button))
            .child(setting_row(t, "Look for updates at startup", "Shows a notice when a new version is out.", startup))
            .into_any_element()
    }
}

/// The Account & security page's fields and what it last heard.
struct SecurityForm {
    password: Entity<TextInput>,
    code: Entity<TextInput>,
    email: Entity<TextInput>,
    state: Option<Result<serde_json::Value, String>>,
    /// Two-factor setup under way: `{secret, otpauth_uri}`.
    setup: Option<serde_json::Value>,
    /// The recovery codes, shown once after turning two-factor on.
    codes: Option<Vec<String>>,
    status: Option<(bool, String)>,
    busy: bool,
    /// A first click on something that can't be taken back, waiting for the second.
    confirm: Option<&'static str>,
}

impl SecurityForm {
    fn new(cx: &mut Context<SettingsView>) -> Self {
        SecurityForm {
            password: cx.new(|cx| TextInput::new("Your account password", cx).masked()),
            code: cx.new(|cx| TextInput::new("123456", cx)),
            email: cx.new(|cx| TextInput::new("you@example.com", cx)),
            state: None,
            setup: None,
            codes: None,
            status: None,
            busy: false,
            confirm: None,
        }
    }
}

/// One choice of the screen-sharing rows: id, label, chosen, and what it sets.
type SharePill = (String, String, bool, Box<dyn Fn(&mut VoicePrefs)>);

#[derive(Clone, Copy)]
enum SecurityAction {
    SetupMfa,
    EnableMfa,
    DisableMfa,
    SetEmail,
    RemoveEmail,
    SignOutAll,
    Close { delete: bool },
    /// Saves what the home server holds about the account.
    Export,
    /// Saves the signed account bundle (password needed), for a server that shuts down.
    Bundle,
}

/// A QR code for an authenticator app, drawn module by module on white.
fn qr_code(data: &str) -> AnyElement {
    let Ok(code) = qrcode::QrCode::new(data.as_bytes()) else { return div().into_any_element() };
    let width = code.width();
    let colors = code.to_colors();
    const MODULE: f32 = 4.;
    div()
        .p(px(MODULE * 2.))
        .bg(gpui::white())
        .rounded(radius::SM)
        .flex()
        .flex_col()
        .children(colors.chunks(width).map(|row| {
            div().flex().children(row.iter().map(|c| {
                div().size(px(MODULE)).when(*c == qrcode::Color::Dark, |d| d.bg(gpui::black()))
            }))
        }))
        .into_any_element()
}

/// One setting: its name and what it does on the left, its control on the right.
fn setting_row(t: &Theme, title: &'static str, detail: &'static str, control: AnyElement) -> gpui::Div {
    setting_row_owned(t, title.to_owned(), detail, control)
}

fn setting_row_owned(t: &Theme, title: String, detail: &'static str, control: AnyElement) -> gpui::Div {
    div()
        .p_3()
        .rounded(radius::MD)
        .border_1()
        .border_color(t.border)
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().font_weight(FontWeight::MEDIUM).child(title))
                .child(div().text_size(text::SM).text_color(t.text_faint).child(detail)),
        )
        .child(control)
}

/// A group heading inside a section.
fn heading(title: &'static str, t: &Theme) -> gpui::Div {
    div().pt_2().pb_1().font_family(FONT_MONO).text_size(text::XS).text_color(t.text_faint).child(title.to_lowercase())
}

/// A switch for a yes-or-no preference shared with the account (`prefs.v1`).
fn account_switch(client: &Entity<Client>, id: &'static str, path: &'static [&'static str], default: bool, cx: &gpui::App) -> AnyElement {
    let on = client.read(cx).account_flag(path, default);
    let client = client.clone();
    Switch::new(id, on).on_toggle(move |on, _, cx| client.update(cx, |c, cx| c.set_account_pref(path, Some(on.into()), cx))).into_any_element()
}

fn date(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .ok()
        .map(|ts| ts.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let mut nav = div().w(px(220.)).flex_none().flex().flex_col().gap_0p5().p_3().bg(t.bg_panel);
        nav = nav.child(div().px_2().pb_2().font_family(rhein_ui::theme::FONT_MONO).text_size(text::XS).text_color(t.text_faint).child("settings"));
        for section in Section::ALL {
            nav = nav.child(
                ListRow::new(SharedString::from(format!("settings-{}", section.label())), section.label())
                    .icon(section.icon())
                    .selected(self.section == section)
                    .on_click(cx.listener(move |v, _, _, cx| v.open(section, cx))),
            );
        }
        nav = nav.child(div().flex_1()).child(
            ListRow::new("settings-sign-out", "Sign out")
                .icon(Icon::LogOut)
                .on_click(cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::SignOut))),
        );
        let body = match self.section {
            Section::Account => self.account(&t, cx),
            Section::Devices => self.devices_section(&t, cx),
            Section::Servers => self.servers_section(&t, cx),
            Section::Appearance => self.appearance(&t, cx),
            Section::Notifications => self.notifications(&t, cx),
            Section::Privacy => self.privacy(&t, cx),
            Section::Security => self.security_section(&t, cx),
            Section::Extras => self.extras(&t, cx),
            Section::Activity => self.activity_section(&t, cx),
            Section::Encryption => self.encryption_section(&t, cx),
            Section::Voice => self.voice_section(&t, cx),
            Section::Shortcuts => self.shortcuts(&t),
            Section::About => self.about(&t, cx),
        };
        // A new crop dialog takes the keyboard, so Escape closes it rather than the settings.
        if let Some((dialog, _)) = &self.crop
            && std::mem::take(&mut self.crop_focus)
        {
            window.focus(&dialog.focus_handle(cx), cx);
        }
        div()
            .id("settings")
            .track_focus(&self.focus)
            .key_context(SETTINGS_CONTEXT)
            .on_action(cx.listener(|_, _: &Dismiss, _, cx| cx.emit(SettingsEvent::Close)))
            .absolute()
            .inset_0()
            // What is behind (the chat) neither hovers nor scrolls while settings are open.
            .occlude()
            .bg(t.bg_main)
            .flex()
            .text_color(t.text)
            .child(nav)
            .child(rhein_ui::smooth_scroll(
                "settings-glide",
                rhein_ui::ScrollTarget::Handle(self.body_scroll.clone()),
                div()
                    .id("settings-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.body_scroll)
                    .px_10()
                    .py_8()
                    .child(
                        div()
                            // In the middle of the window, not against its left edge.
                            .mx_auto()
                            // The profile's fields and its preview side by side need more room.
                            .max_w(px(if self.section == Section::Account { 860. } else { 640. }))
                            .flex()
                            .flex_col()
                            .gap_5()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .child(div().flex_1().text_size(text::XL).font_weight(FontWeight::SEMIBOLD).child(self.section.label()))
                                    .child(
                                        IconButton::new("settings-close", Icon::X)
                                            .tooltip("Close (Esc)")
                                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::Close))),
                                    ),
                            )
                            .child(body),
                    ),
            ))
            .children(self.crop.as_ref().map(|(dialog, _)| dialog.clone()))
    }
}

/// The profile card's shape, breathing, while the profile loads.
fn preview_placeholder(t: &Theme) -> AnyElement {
    let bar = |w: f32, h: f32| div().w(px(w)).h(px(h)).rounded(radius::SM).bg(t.bg_selected);
    let look = profile::Look::default();
    let face = div().size(px(profile::FACE_SIZE)).rounded_full().bg(t.bg_raised).into_any_element();
    let card = profile::frame(
        &look,
        t,
        div().flex().flex_col().child(profile::header(&look, face, t)).child(
            div()
                .px_4()
                .pt_1()
                .pb_4()
                .flex()
                .flex_col()
                .gap_2()
                .child(bar(120., 16.))
                .child(bar(170., 12.))
                .child(div().h_2())
                .child(bar(56., 10.))
                .child(bar(210., 12.)),
        ),
    );
    rhein_ui::motion::pulse(card, "profile-placeholder").into_any_element()
}
