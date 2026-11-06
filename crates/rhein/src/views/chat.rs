//! The message pane: history as a virtual list that sticks to the bottom, and the composer with
//! replies, edits and staged attachments.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    Anchor, AnyElement, App, AppContext as _, ClipboardEntry, ClipboardItem, Context, DismissEvent, Entity,
    EventEmitter, ExternalPaths, FocusHandle, Focusable, FollowMode, FontWeight, ImageFormat, IntoElement,
    KeyBinding, ListAlignment, ListState, ObjectFit, ParentElement as _, PathPromptOptions, Pixels, Point, Render,
    SharedString, Styled as _, StyledImage as _, Subscription, Window, anchored, deferred, div, img, list,
    prelude::*, px,
};
use rhein_proto::Message;
use rhein_store::autocomplete::{self, Trigger};
use rhein_store::{Client, ClientEvent, StagedState, perms};
use rhein_ui::theme::{FONT_MONO, radius, size, text};
use rhein_ui::{motion, 
    ActiveTheme as _, Avatar, Button, Dismiss, Icon, IconButton, InputEvent, MenuEntry, MenuHost, Modal, Nav,
    TextInput, icon,
};

use crate::views::emoji_picker::{EmojiPicker, Picked};
use crate::views::live::{LiveMovie, LiveVideo, SpeakingFace, ToggleBig};
use crate::views::part::{Part, part};
use crate::views::gif_picker::{GifPicker, PickedGif};
use crate::views::media::{self, Media};
use crate::views::rows::{RowCtx, RowUi, RowView, format_size, message_key};

const TYPING_EVERY: Duration = Duration::from_secs(6);
/// How long without a key before a conversation hears we stopped typing.
const TYPING_IDLE: Duration = Duration::from_secs(8);
const HIGHLIGHT_FOR: Duration = Duration::from_millis(1600);
const COMPOSER_LINES: usize = 10;
const LIGHTBOX_CONTEXT: &str = "Lightbox";
/// The typing line: its height, and the space it keeps from the last message's box above and
/// from the message bar below. The history ends that far above the composer's top.
const TYPING_H: Pixels = px(16.);
const TYPING_GAP: Pixels = px(2.);
/// The composer's padding above the message bar (`pt_2`).
const COMPOSER_TOP: Pixels = px(8.);
/// `TYPING_H + 2 × TYPING_GAP − COMPOSER_TOP` (pixel arithmetic isn't const).
const HISTORY_BOTTOM: Pixels = px(16. + 2. * 2. - 8.);
/// Space between the message bar and the chat's bottom, left and right edges.
const COMPOSER_MARGIN: Pixels = px(12.);

pub fn bindings() -> Vec<KeyBinding> {
    vec![KeyBinding::new("escape", Dismiss, Some(LIGHTBOX_CONTEXT))]
}

pub struct ChatView {
    client: Entity<Client>,
    list: ListState,
    /// The lists of channels shown before, latest last: coming back to one shows it as it was,
    /// without measuring its rows again.
    lists: Vec<(String, ListState)>,
    channel: Rc<RefCell<Option<String>>>,
    composer: Entity<TextInput>,
    editing: Option<String>,
    replying: Option<String>,
    last_typing: Option<Instant>,
    /// Says "stopped typing" once the keys have been still for a while.
    typing_idle: Option<gpui::Task<()>>,
    /// The composer's length at the last change, to tell typing from deleting.
    composer_len: usize,
    /// Which side panel the shell shows, so the header can highlight its button.
    active_panel: Option<PanelKind>,
    /// A thread shown in the side panel: this view is pinned to that channel.
    fixed: Option<String>,
    picker: Option<OpenPicker>,
    /// The GIF favourites menu, where it is anchored, its subscriptions.
    gif_picker: Option<(Entity<GifPicker>, Point<Pixels>, Vec<Subscription>)>,
    /// The GIF picker shows four columns instead of two; kept across openings.
    gif_picker_wide: Rc<Cell<bool>>,
    /// When a composer picker (GIF or emoji) last closed by a click outside it: the click on
    /// its own button that closed it must not open it again.
    picker_closed: Option<(String, std::time::Instant)>,
    suggest: Option<Suggest>,
    pub(crate) menu: MenuHost,
    /// A bot select's picks before they are sent: `(message id, custom id)` to values.
    select_draft: HashMap<(String, String), Vec<String>>,
    /// A forum's "new post" title.
    forum_title: Entity<TextInput>,
    confirm_delete: Option<Message>,
    modal_focus: FocusHandle,
    /// Spoilers opened by the user, per (message id, spoiler index).
    pub(crate) revealed: HashSet<(String, usize)>,
    /// Text selected in one message's block (`views::select`).
    pub(crate) selection: Option<crate::views::select::TextSelection>,
    /// The link (or channel, or spoiler) under the pointer: message, text block, byte range.
    pub(crate) hovered_link: Option<(String, usize, std::ops::Range<usize>)>,
    /// The message row under the pointer (its key, `rows::message_key`). Only that row builds
    /// its hover actions: every row building them, hidden, was most of a scroll frame's layout.
    pub(crate) hovered_row: Option<String>,
    /// Takes the keyboard while text is selected, so Ctrl+C reaches the selection and not the
    /// composer.
    pub(crate) select_focus: FocusHandle,
    /// The message bar's box, as last laid out.
    composer_bounds: Rc<Cell<gpui::Bounds<Pixels>>>,
    /// Messages that arrived while the channel was open, animating in (see `rows::message_key`).
    /// Messages that just arrived, by `rows::message_key`, and the height their row measured:
    /// `None` until the row has been laid out once, unseen, to learn it (see `rows`).
    pub(crate) fresh: HashMap<String, Option<f32>>,
    /// A message briefly highlighted after a jump.
    pub(crate) highlight: Option<(String, Instant)>,
    lightbox: Option<(String, String)>,
    lightbox_focus: FocusHandle,
    /// A running glide to the newest message; replaced by the next one.
    glide: Option<gpui::Task<()>>,
    /// A voice channel's own chat, in a column beside its call (closed to begin with).
    voice_chat: Option<(Entity<ChatView>, Subscription)>,
    /// The call's videos on the stage, each its own view: `(user, slot)` to it.
    live: HashMap<(String, rhein_store::Slot), Entity<LiveVideo>>,
    /// The player of the video playing from a message, shown in its row.
    movie: Entity<LiveMovie>,
    /// The player is shown large over the window instead (its button, like a picture's lightbox).
    movie_big: bool,
    /// Pictures asked for ahead of the view, and the ones that have arrived.
    prefetched: HashSet<String>,
    arrived: HashSet<String>,
    /// The reaction under the pointer (message ID, its index), which shows who reacted.
    pub(crate) hovered_reaction: Option<(String, usize)>,
    /// The pixel size of each picture that has arrived, for the rows to lay it out whole.
    picture_sizes: Rc<RefCell<HashMap<String, (u32, u32)>>>,
    parsed: crate::views::rows::ParsedCache,
    /// A repaint is scheduled for the typing dots' next step.
    /// The typing line's pulsing dots, a view that ticks by itself.
    dots: Entity<TypingDots>,
    /// The history, drawn as a cached view of its own (see `history`).
    history: Option<Entity<Part<ChatView>>>,
    /// Faces in a conversation's call bar, ringed while speaking.
    faces: HashMap<String, Entity<SpeakingFace>>,
    /// How far each nearby row's previews and pictures had got when last looked at; a row whose
    /// state moves on is measured again (see `prefetch`).
    row_progress: HashMap<String, u64>,
    /// The channel and the number of its messages when every video in it last had its poster
    /// asked for (see `prefetch`).
    posters_asked: Option<(String, usize)>,
    /// Videos whose first moments were asked for (`Client::preload_movie`).
    preloaded: HashSet<String>,
    /// When the channel shown last changed, until the frames after it are timed (`time_switch`).
    switched: Option<Instant>,
    /// Each history row's view (`rows::RowView`), by `rows::message_key`, of this channel and
    /// those shown before.
    rows: HashMap<String, Entity<RowView>>,
    _subs: Vec<Subscription>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelKind {
    Members,
    Pins,
    Threads,
    Search,
}

/// Requests for the shell, which owns the side panel.
pub enum ChatEvent {
    TogglePanel(PanelKind),
    OpenThread(String),
    ClosePanel,
    /// Show a user's profile card at a position.
    Profile { user_id: String, at: Point<Pixels> },
    /// Open the member context menu for a user.
    MemberMenu { user_id: String, at: Point<Pixels> },
}

impl EventEmitter<ChatEvent> for ChatView {}

/// An open emoji picker: the view, where it is anchored, what it is for, its subscriptions.
type OpenPicker = (Entity<EmojiPicker>, Point<Pixels>, PickerTarget, Vec<Subscription>);

#[derive(Clone)]
enum PickerTarget {
    Composer,
    React { channel_id: String, message_id: String },
}

/// One autocomplete row: what to show and what to insert.
#[derive(Clone)]
struct Suggestion {
    label: String,
    hint: String,
    insert: String,
    avatar: Option<(String, Option<String>)>,
    glyph: Option<String>,
    icon: Option<Icon>,
}

struct Suggest {
    start: usize,
    items: Vec<Suggestion>,
    selected: usize,
}

/// A channel's history list, following its newest message; near the top, older ones are asked for.
fn history_list(client: &Entity<Client>, channel: &Rc<RefCell<Option<String>>>) -> ListState {
    let list = ListState::new(0, ListAlignment::Bottom, px(600.));
    list.set_follow_mode(FollowMode::Tail);
    let (client, channel) = (client.downgrade(), channel.clone());
    list.set_scroll_handler(move |event, _window, cx| {
        if event.visible_range.start > 2 {
            return;
        }
        let Some(id) = channel.borrow().clone() else { return };
        client
            .update(cx, |client, cx| {
                let ready = client.logs.get(&id).is_some_and(|l| l.loaded && !l.reached_start);
                if ready {
                    client.load_history(id, true, cx);
                }
            })
            .ok();
    });
    list
}

/// Lists of channels left that are kept, with what their rows measured and where they were
/// scrolled.
const LISTS_KEPT: usize = 30;

impl ChatView {
    pub fn new(client: Entity<Client>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build(client, None, window, cx)
    }

    /// A view pinned to one channel (a thread in the side panel).
    pub fn fixed(client: Entity<Client>, channel_id: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build(client, Some(channel_id), window, cx)
    }

    fn build(client: Entity<Client>, fixed: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let channel: Rc<RefCell<Option<String>>> = Rc::default();
        let list = history_list(&client, &channel);
        let composer = cx.new(|cx| TextInput::new("message", cx).multiline(COMPOSER_LINES).block_cursor().bare());
        let speaking = client.read(cx).call_speaking.clone();
        let mut subs = vec![
            cx.subscribe_in(&composer, window, |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Submit => this.submit(window, cx),
                InputEvent::Changed => {
                    this.typed(cx);
                    this.update_suggestions(cx);
                }
                InputEvent::Navigate(nav) => this.navigate_suggestions(*nav, cx),
                InputEvent::UpAtStart => this.edit_last(window, cx),
                InputEvent::Cancel => this.cancel(cx),
                InputEvent::PasteFiles(item) => this.paste_files(item.clone(), cx),
            }),
            cx.subscribe(&client, |this, _, event: &ClientEvent, cx| this.on_client(event, cx)),
            cx.subscribe(&client, |this, _, event: &ClientEvent, cx| {
                if let ClientEvent::OpenThread { thread_id } = event
                    && this.fixed.is_none()
                {
                    cx.emit(ChatEvent::OpenThread(thread_id.clone()));
                }
            }),
            // The chat is a cached view: whatever it shows of the client, it repaints for.
            cx.observe(&client, |this, _, cx| {
                this.sync_channel(cx);
                cx.notify();
            }),
            // The voice stage rings whoever speaks (elsewhere the faces ring themselves).
            cx.observe(&speaking, |this, _, cx| {
                let client = this.client.read(cx);
                let stage = this.fixed.is_none() && this.current().and_then(|c| client.channel(&c)).is_some_and(|c| c.is_voice());
                if stage {
                    cx.notify();
                }
            }),
        ];
        let movie = cx.new(|cx| LiveMovie::new(client.clone(), cx));
        subs.push(cx.subscribe_in(&movie, window, |this, _, _: &ToggleBig, window, cx| this.set_movie_big(!this.movie_big, window, cx)));
        let mut this = Self {
            client,
            list,
            lists: Vec::new(),
            channel,
            composer,
            editing: None,
            replying: None,
            last_typing: None,
            typing_idle: None,
            composer_len: 0,
            active_panel: None,
            fixed,
            picker: None,
            gif_picker: None,
            gif_picker_wide: Rc::new(Cell::new(false)),
            picker_closed: None,
            suggest: None,
            menu: MenuHost::default(),
            select_draft: HashMap::new(),
            forum_title: cx.new(|cx| TextInput::new("Title of a new post", cx)),
            confirm_delete: None,
            modal_focus: cx.focus_handle(),
            revealed: HashSet::new(),
            fresh: HashMap::new(),
            highlight: None,
            lightbox: None,
            lightbox_focus: cx.focus_handle(),
            glide: None,
            voice_chat: None,
            selection: None,
            hovered_link: None,
            hovered_row: None,
            select_focus: cx.focus_handle(),
            composer_bounds: Rc::default(),
            prefetched: HashSet::new(),
            live: HashMap::new(),
            movie,
            movie_big: false,
            arrived: HashSet::new(),
            hovered_reaction: None,
            picture_sizes: Rc::default(),
            parsed: Rc::default(),
            dots: cx.new(|_| TypingDots::default()),
            history: None,
            faces: HashMap::new(),
            row_progress: HashMap::new(),
            posters_asked: None,
            preloaded: HashSet::new(),
            switched: None,
            rows: HashMap::new(),
            _subs: subs,
        };
        this.sync_channel(cx);
        this
    }

    pub fn set_active_panel(&mut self, panel: Option<PanelKind>, cx: &mut Context<Self>) {
        self.active_panel = panel;
        cx.notify();
    }

    /// Scrolls the history by `by` pixels, as the wheel does (debug builds' dev hooks).
    #[cfg(debug_assertions)]
    pub fn dev_scroll(&mut self, by: f32, cx: &mut Context<Self>) {
        self.list.scroll_by(px(by));
        if let Some(history) = &self.history {
            history.update(cx, |_, cx| cx.notify());
        }
        cx.notify();
    }

    pub fn composer_focus(&self, cx: &App) -> FocusHandle {
        self.composer.focus_handle(cx)
    }

    fn current(&self) -> Option<String> {
        self.channel.borrow().clone()
    }

    fn count(&self, cx: &App) -> usize {
        let Some(id) = self.current() else { return 0 };
        self.client.read(cx).logs.get(&id).map_or(0, |l| l.messages.len())
    }

    /// Follows the client's selected channel; a switch resets the list to that channel's history.
    fn sync_channel(&mut self, cx: &mut Context<Self>) {
        let selected = match &self.fixed {
            Some(id) => Some(id.clone()),
            None => self.client.read(cx).selected_channel().map(str::to_owned),
        };
        if *self.channel.borrow() == selected {
            return;
        }
        let left = self.channel.borrow().clone();
        if let Some(old) = self.channel.borrow().clone() {
            self.client.update(cx, |c, _| c.stop_typing(&old));
        }
        self.last_typing = None;
        self.typing_idle = None;
        self.composer_len = 0;
        *self.channel.borrow_mut() = selected.clone();
        self.switched = Some(Instant::now());
        self.voice_chat = None;
        self.editing = None;
        self.replying = None;
        let name = selected
            .as_deref()
            .and_then(|id| {
                let client = self.client.read(cx);
                match client.conversations.iter().find(|c| c.id == id) {
                    Some(c) => Some(format!("message @{}", client.conversation_title(c))),
                    None => client.channel(id).map(|c| format!("message #{}", c.name)),
                }
            })
            .unwrap_or_else(|| "message".into());
        self.composer.update(cx, |c, cx| {
            c.clear(cx);
            c.set_placeholder(name, cx);
        });
        // The list it had, unless its messages changed meanwhile (then it starts over).
        let count = self.count(cx);
        let kept = selected.as_ref().and_then(|c| self.lists.iter().position(|(k, _)| k == c)).map(|i| self.lists.remove(i).1);
        let list = match kept {
            Some(list) if count > 0 && list.item_count() == count => list,
            _ => {
                let list = history_list(&self.client, &self.channel);
                list.reset(count);
                list
            }
        };
        let left_list = std::mem::replace(&mut self.list, list);
        if let Some(left) = left {
            self.lists.push((left, left_list));
            if self.lists.len() > LISTS_KEPT {
                self.lists.remove(0);
            }
        }
        self.glide = None;
        // A thread in the side panel is not the selected channel, so nobody else loads it.
        if let Some(id) = self.fixed.clone() {
            let needs = self.client.read(cx).logs.get(&id).is_none_or(|l| !l.loaded && !l.loading);
            if needs {
                self.client.update(cx, |c, cx| c.load_history(id, false, cx));
            }
        }
        cx.notify();
    }

    fn on_client(&mut self, event: &ClientEvent, cx: &mut Context<Self>) {
        let current = self.current();
        let is_current = |id: &String| current.as_ref() == Some(id);
        match event {
            ClientEvent::MessagesReset { channel_id } if is_current(channel_id) => {
                self.list.reset(self.count(cx));
                self.list.set_follow_mode(FollowMode::Tail);
            }
            // A channel left whose messages started over keeps no list from before.
            ClientEvent::MessagesReset { channel_id } => self.lists.retain(|(c, _)| c != channel_id),
            ClientEvent::MessagesPrepended { channel_id, count } if is_current(channel_id) => {
                self.list.splice(0..0, *count);
            }
            ClientEvent::MessageAppended { channel_id } if is_current(channel_id) => {
                let n = self.list.item_count();
                self.list.splice(n..n, 1);
                let key = self.client.read(cx).logs.get(channel_id).and_then(|l| l.messages.last()).map(|m| message_key(m).to_owned());
                if let Some(key) = key
                    && !cx.reduce_motion()
                {
                    self.fresh.insert(key.clone(), None);
                    // A row not laid out soon (the list is scrolled away from the end) just shows.
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(Duration::from_millis(400)).await;
                        this.update(cx, |this, cx| {
                            if this.fresh.get(&key) == Some(&None) {
                                this.fresh.remove(&key);
                                cx.notify();
                            }
                        })
                        .ok();
                    })
                    .detach();
                }
            }
            ClientEvent::MessageChanged { channel_id, index } if is_current(channel_id) => {
                // The next row's header depends on this one, so re-measure both. Not `splice`:
                // that resets the scroll position, and the echo replacing a message just sent
                // made the list jump up and back to the end.
                let end = (*index + 2).min(self.list.item_count());
                if *index < end {
                    self.list.remeasure_items(*index..end);
                }
            }
            ClientEvent::MessageRemoved { channel_id, index } if is_current(channel_id) && *index < self.list.item_count() => {
                self.list.splice(*index..*index + 1, 0);
            }
            ClientEvent::Jump { channel_id, index } if is_current(channel_id) => {
                let id = self
                    .client
                    .read(cx)
                    .logs
                    .get(channel_id)
                    .and_then(|l| l.messages.get(*index))
                    .map(|m| m.id.clone());
                self.list.pause_following_tail();
                self.list.scroll_to_reveal_item(*index);
                if let Some(id) = id {
                    self.flash(id, cx);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// A new message's row has been laid out, unseen, at `height`: now its space opens and it
    /// fades in (`motion::expand_then_fade_in`), after which it is an ordinary row.
    pub(crate) fn fresh_measured(&mut self, key: String, height: f32, cx: &mut Context<Self>) {
        let Some(slot) = self.fresh.get_mut(&key) else { return };
        if slot.is_some() {
            return;
        }
        *slot = Some(height);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(motion::EXPAND + motion::APPEAR + Duration::from_millis(40)).await;
            this.update(cx, |this, cx| {
                this.fresh.remove(&key);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn flash(&mut self, message_id: String, cx: &mut Context<Self>) {
        self.highlight = Some((message_id, Instant::now()));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HIGHLIGHT_FOR).await;
            this.update(cx, |this, cx| {
                if this.highlight.as_ref().is_some_and(|(_, at)| at.elapsed() >= HIGHLIGHT_FOR) {
                    this.highlight = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// The channel on screen, if its message bar is open.
    fn writable(&self, cx: &App) -> Option<String> {
        self.current().filter(|c| self.client.read(cx).may_write(c))
    }

    fn submit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.writable(cx) else { return };
        let channel_for_stop = channel.clone();
        let text = self.composer.read(cx).text().to_owned();
        if let Some(message_id) = self.editing.take() {
            if !text.trim().is_empty() {
                self.client.update(cx, |c, cx| c.edit_message(channel, message_id, text, cx));
            }
        } else {
            let has_files = !self.client.read(cx).staged(&channel).is_empty();
            if text.trim().is_empty() && !has_files {
                return;
            }
            let reply = self.replying.take();
            self.client.update(cx, |c, cx| c.send_message(channel, text, reply, cx));
        }
        self.last_typing = None;
        self.typing_idle = None;
        self.client.update(cx, |c, _| c.stop_typing(&channel_for_stop));
        self.composer.update(cx, |c, cx| c.clear(cx));
        self.composer_len = 0;
        self.list.set_follow_mode(FollowMode::Tail);
        cx.notify();
    }

    /// The composer changed. Only text that grew is typing (a key or a paste); a deletion, an
    /// emptied composer or an edit is not, and an emptied one says we stopped.
    fn typed(&mut self, cx: &mut Context<Self>) {
        let len = self.composer.read(cx).text().len();
        let grew = len > self.composer_len;
        self.composer_len = len;
        let Some(channel) = self.current() else { return };
        if len == 0 {
            self.stop_typing(&channel, cx);
            return;
        }
        if self.editing.is_some() || !grew || !self.client.read(cx).may_write(&channel) || !self.client.read(cx).prefs.send_typing {
            return;
        }
        // In a conversation the others are told when we stop: after a while without a key.
        let idle = channel.clone();
        self.typing_idle = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TYPING_IDLE).await;
            this.update(cx, |this, cx| this.stop_typing(&idle, cx)).ok();
        }));
        if self.last_typing.is_some_and(|at| at.elapsed() < TYPING_EVERY) {
            return;
        }
        self.last_typing = Some(Instant::now());
        self.client.update(cx, |c, _| c.send_typing(&channel));
    }

    fn stop_typing(&mut self, channel: &str, cx: &mut Context<Self>) {
        self.last_typing = None;
        self.typing_idle = None;
        self.client.update(cx, |c, _| c.stop_typing(channel));
    }

    pub(crate) fn start_edit(&mut self, message: &Message, window: &mut Window, cx: &mut Context<Self>) {
        self.replying = None;
        self.editing = Some(message.id.clone());
        let content = message.content.clone();
        self.composer.update(cx, |c, cx| c.set_text(content, cx));
        let focus = self.composer_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Up on an empty composer edits your newest message, as in the web client.
    fn edit_last(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() || !self.composer.read(cx).text().is_empty() {
            return;
        }
        let Some(channel) = self.current() else { return };
        let Some(message) = self.client.read(cx).last_own_message(&channel).cloned() else { return };
        self.start_edit(&message, window, cx);
    }

    pub(crate) fn start_reply(&mut self, message_id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            self.editing = None;
            self.composer.update(cx, |c, cx| c.clear(cx));
        }
        self.replying = Some(message_id);
        let focus = self.composer_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            self.composer.update(cx, |c, cx| c.clear(cx));
        } else if self.replying.take().is_none() {
            // Nothing to cancel: Esc marks the channel read and returns to the present.
            if let Some(channel) = self.current() {
                self.client.update(cx, |c, cx| c.mark_read(channel, cx));
            }
            self.glide_to_end(cx);
        }
        cx.notify();
    }

    /// Gets the rows around the view ready before they come into it: their previews and GIF
    /// share pages asked for, their pictures loaded, and each one measured again whenever one of
    /// those arrives. The list measures a row above the view once and keeps that height; without
    /// this, a picture that loads later makes the row grow as it scrolls in, and everything below
    /// jumps.
    fn prefetch(&mut self, cx: &mut Context<Self>) {
        const ABOVE: usize = 40;
        const BELOW: usize = 20;
        let Some(channel) = self.current() else { return };
        let count = self.count(cx);
        let top = self.list.logical_scroll_top().item_ix.min(count);
        let range = top.saturating_sub(ABOVE)..(top + BELOW).min(count);
        let (mut embeds, mut shares, mut pictures, mut changed) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (mut probes, mut posters, mut preloads) = (Vec::new(), Vec::new(), Vec::new());
        {
            let client = self.client.read(cx);
            let Some(log) = client.logs.get(&channel) else { return };
            // The videos in view and just around it get their first moments fetched.
            // Not reading the player here: the history would draw again with each of its pictures.
            for message in log.messages.get(top.saturating_sub(2)..(top + 12).min(count)).unwrap_or_default() {
                preloads.extend(crate::views::rows::preloadable(message).filter(|(key, _)| !self.preloaded.contains(key)));
            }
            self.preloaded.extend(preloads.iter().map(|(key, _)| key.clone()));
            for ix in range {
                let Some(message) = log.messages.get(ix) else { break };
                let media = crate::views::rows::row_media(client, &channel, message, &self.parsed, cx);
                let arrived = media.pictures.iter().filter(|p| self.arrived.contains(*p)).count() as u64;
                let progress = media.progress ^ arrived.wrapping_mul(0x9e37_79b9_7f4a_7c15);
                if self.row_progress.insert(crate::views::rows::message_key(message).to_owned(), progress).is_some_and(|was| was != progress) {
                    changed.push(ix);
                }
                embeds.extend(media.embeds);
                shares.extend(media.shares);
                probes.extend(media.probes);
                posters.extend(media.posters);
                pictures.extend(media.pictures.into_iter().filter(|p| !self.prefetched.contains(p)));
            }
            // Every other video loaded in the channel too, newest first after those near the
            // view: each is only its index and first picture. Looked for when the messages
            // change, not on every frame of a scroll.
            if self.posters_asked.as_ref() != Some(&(channel.clone(), count)) {
                self.posters_asked = Some((channel.clone(), count));
                for message in log.messages.iter().rev() {
                    for poster in crate::views::rows::missing_posters(client, &channel, message, cx) {
                        if !posters.iter().any(|asked| asked.0 == poster.0) {
                            posters.push(poster);
                        }
                    }
                }
            }
        }
        for ix in changed {
            self.list.remeasure_items(ix..ix + 1);
        }
        for picture in pictures {
            self.prefetched.insert(picture.clone());
            // Only its size, from the header: the picture itself is decoded when drawn.
            let task = rhein_ui::images::probe(picture.clone(), cx);
            cx.spawn(async move |this, cx| {
                let size = task.await;
                this.update(cx, |this, cx| {
                    if let Some(size) = size {
                        this.picture_sizes.borrow_mut().insert(picture.clone(), size);
                    }
                    this.arrived.insert(picture);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        if !embeds.is_empty() || !shares.is_empty() || !probes.is_empty() || !posters.is_empty() || !preloads.is_empty() {
            let client = self.client.clone();
            cx.defer(move |cx| {
                client.update(cx, |client, cx| {
                    for url in &embeds {
                        client.request_embed(&channel, url, cx);
                    }
                    for url in &shares {
                        client.request_gif_share(url, cx);
                    }
                    for url in &probes {
                        client.request_media_kind(url, cx);
                    }
                    for (key, hash, size, source) in posters {
                        client.request_poster(key, &hash, size, source, cx);
                    }
                    for (key, source) in preloads {
                        client.preload_movie(key, source, cx);
                    }
                })
            });
        }
    }

    /// Scrolls to the newest message with an ease-out glide, then follows the tail again. From
    /// far back it first jumps to a viewport away, so the glide is always short.
    fn glide_to_end(&mut self, cx: &mut Context<Self>) {
        let finish = |this: &mut Self, cx: &mut Context<Self>| {
            this.list.scroll_to_end();
            this.list.set_follow_mode(FollowMode::Tail);
            this.glide = None;
            cx.notify();
        };
        let remaining = self.list.max_offset_for_scrollbar().y + self.list.scroll_px_offset_for_scrollbar().y;
        let viewport = self.list.viewport_bounds().size.height;
        if cx.reduce_motion() || remaining <= px(1.) || viewport <= px(0.) {
            finish(self, cx);
            return;
        }
        let mut total = remaining;
        if remaining > viewport * 3. {
            self.list.scroll_by(remaining - viewport);
            total = viewport;
        }
        let start = Instant::now();
        self.glide = Some(cx.spawn(async move |this, cx| {
            let mut done = px(0.);
            loop {
                cx.background_executor().timer(Duration::from_millis(8)).await;
                let t = start.elapsed().as_secs_f32() / motion::SCROLL.as_secs_f32();
                let keep_going = this
                    .update(cx, |this, cx| {
                        if t >= 1. {
                            finish(this, cx);
                            return false;
                        }
                        let target = total * motion::ease_out(t);
                        this.list.scroll_by(target - done);
                        done = target;
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Inserts text at the caret and focuses the composer (e.g. "Mention" from a menu).
    pub fn insert_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.update(cx, |c, cx| c.insert(text, cx));
        let focus = self.composer_focus(cx);
        window.focus(&focus, cx);
    }

    pub(crate) fn show_profile(&mut self, user_id: String, at: Point<Pixels>, cx: &mut Context<Self>) {
        cx.emit(ChatEvent::Profile { user_id, at });
    }

    pub(crate) fn member_menu(&mut self, user_id: String, at: Point<Pixels>, cx: &mut Context<Self>) {
        cx.emit(ChatEvent::MemberMenu { user_id, at });
    }

    pub(crate) fn set_selection(&mut self, selection: Option<crate::views::select::TextSelection>, cx: &mut Context<Self>) {
        if self.selection != selection {
            self.selection = selection;
            cx.notify();
        }
    }

    /// The pointer is over a clickable range of a text block (`Some`), or over none of that
    /// block's (`None`, which only clears the hover if it was this block's).
    pub(crate) fn hover_link(&mut self, message: &str, block: usize, range: Option<std::ops::Range<usize>>, cx: &mut Context<Self>) {
        let next = match range {
            Some(r) => Some((message.to_owned(), block, r)),
            None if self.hovered_link.as_ref().is_some_and(|(m, b, _)| m == message && *b == block) => None,
            None => return,
        };
        if self.hovered_link != next {
            self.hovered_link = next;
            cx.notify();
        }
    }

    /// The view of the history row at `ix`, made the first time it is drawn, and whether what it
    /// shows of this view (its `RowUi`) changed since it last drew.
    fn row_view(&mut self, ix: usize, ctx: &Rc<RowCtx>, cx: &mut Context<Self>) -> Option<(Entity<RowView>, bool)> {
        /// Past this many rows the views start over, like the parsed texts.
        const ROWS_KEPT: usize = 2000;
        let (key, ui) = {
            let log = self.client.read(cx).logs.get(&ctx.channel_id)?;
            let message = log.messages.get(ix)?;
            let key = message_key(message).to_owned();
            let ui = RowUi {
                selected: self.selection.as_ref().filter(|s| s.message == message.id).map(|s| (s.block, s.range())),
                hovered_link: self.hovered_link.as_ref().filter(|(m, _, _)| *m == message.id).map(|(_, b, r)| (*b, r.clone())),
                // Only this message's spoilers, not a copy of every one revealed.
                revealed: self.revealed.iter().filter(|(m, _)| *m == message.id).cloned().collect(),
                highlighted: self.highlight.as_ref().is_some_and(|(id, _)| *id == message.id),
                fresh: self.fresh.get(&key).copied(),
                hovered: self.hovered_row.as_deref() == Some(key.as_str()),
                hovered_reaction: self.hovered_reaction.as_ref().filter(|(m, _)| *m == message.id).map(|(_, n)| *n),
                divider_here: ctx.divider == Some(ix),
                prev: ix.checked_sub(1).and_then(|p| log.messages.get(p)).map(|p| message_key(p).to_owned()),
                first: ix == 0,
                progress: self.row_progress.get(&key).copied(),
                scale_factor: ctx.scale_factor,
                movie_big: ctx.movie_big,
                narrow: {
                    let width = self.list.viewport_bounds().size.width;
                    width > px(0.) && width < px(520.)
                },
            };
            (key, ui)
        };
        // Rows of channels left stay (by message, so they are not confused): coming back draws
        // them from what they drew.
        if self.rows.len() > ROWS_KEPT {
            self.rows.clear();
        }
        if let Some(row) = self.rows.get(&key).cloned() {
            let changed = row.update(cx, |row, _| {
                let changed = row.ui != ui;
                if changed {
                    row.ui = ui;
                    row.ctx = ctx.clone();
                }
                changed
            });
            return Some((row, changed));
        }
        let row = cx.new(|_| RowView { key: key.clone(), ctx: ctx.clone(), ui });
        self.rows.insert(key, row.clone());
        Some((row, false))
    }

    /// The pointer entered the row of the message under `key` (`true`) or left it.
    pub(crate) fn hover_row(&mut self, key: &str, hovered: bool, cx: &mut Context<Self>) {
        let next = match hovered {
            true => Some(key.to_owned()),
            false if self.hovered_row.as_deref() == Some(key) => None,
            false => return,
        };
        if self.hovered_row != next {
            self.hovered_row = next;
            cx.notify();
        }
    }

    /// The pointer moved with the button held: the selection it started follows it.
    pub(crate) fn drag_selection(&mut self, message: &str, block: usize, index: usize, cx: &mut Context<Self>) {
        if let Some(s) = self.selection.as_mut().filter(|s| s.dragging && s.message == message && s.block == block) {
            let index = index.min(s.text.len());
            if s.head != index {
                s.head = index;
                cx.notify();
            }
        }
    }

    pub(crate) fn end_selection(&mut self, _cx: &mut Context<Self>) {
        if let Some(s) = self.selection.as_mut() {
            s.dragging = false;
        }
    }

    /// Copies the selected text; false when nothing is selected.
    fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.selection.as_ref().and_then(|s| s.selected()).map(str::to_owned) else { return false };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        true
    }

    pub(crate) fn reveal_spoiler(&mut self, key: (String, usize), cx: &mut Context<Self>) {
        self.revealed.insert(key);
        cx.notify();
    }

    pub(crate) fn open_lightbox(&mut self, url: String, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.lightbox = Some((url, name));
        window.focus(&self.lightbox_focus, cx);
        cx.notify();
    }

    fn set_movie_big(&mut self, big: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.movie_big = big;
        self.movie.update(cx, |m, cx| m.set_big(big, cx));
        let focus = if big { self.lightbox_focus.clone() } else { self.composer_focus(cx) };
        window.focus(&focus, cx);
        cx.notify();
    }

    /// The playing video, large over the window: the chat's one player, taken out of its row.
    fn render_movie_big(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.movie_big {
            return None;
        }
        let Some(view) = self.client.read(cx).movie_player.read(cx).view() else {
            // The video was stopped meanwhile.
            self.movie_big = false;
            self.movie.update(cx, |m, cx| m.set_big(false, cx));
            return None;
        };
        let t = cx.theme().clone();
        let viewport = window.viewport_size();
        let (w, h) = view.size.filter(|(w, h)| *w > 0 && *h > 0).map_or((16., 9.), |(w, h)| (w as f32, h as f32));
        // Within the padding (`p_8`), as large as fits.
        let scale = (f32::from(viewport.width - px(64.)) / w).min(f32::from(viewport.height - px(64.)) / h);
        let overlay = div()
            .id("movie-big")
            .occlude()
            .track_focus(&self.lightbox_focus)
            .key_context(LIGHTBOX_CONTEXT)
            .on_action(cx.listener(|this, _: &Dismiss, window, cx| this.set_movie_big(false, window, cx)))
            .w(viewport.width)
            .h(viewport.height)
            .bg(t.overlay)
            .flex()
            .items_center()
            .justify_center()
            .p_8()
            .on_click(cx.listener(|this, _, window, cx| this.set_movie_big(false, window, cx)))
            .child(
                div()
                    .id("movie-big-box")
                    .w(px(w * scale))
                    .h(px(h * scale))
                    .rounded(rhein_ui::theme::radius::LG)
                    .overflow_hidden()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(self.movie.clone()),
            );
        Some(deferred(anchored().position(gpui::point(px(0.), px(0.))).child(overlay)).with_priority(3).into_any_element())
    }

    fn close_lightbox(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.lightbox = None;
        let focus = self.composer_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn attach(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = self.writable(cx) else { return };
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach files".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            this.update(cx, |this, cx| this.stage_paths(channel, paths, cx)).ok();
        })
        .detach();
    }

    fn stage_paths(&mut self, channel: String, paths: Vec<std::path::PathBuf>, cx: &mut Context<Self>) {
        if !self.client.read(cx).may_write(&channel) {
            return;
        }
        self.client.update(cx, |c, cx| {
            for path in paths.into_iter().filter(|p| p.is_file()) {
                c.stage_path(channel.clone(), path, cx);
            }
        });
    }

    fn paste_files(&mut self, item: ClipboardItem, cx: &mut Context<Self>) {
        let Some(channel) = self.writable(cx) else { return };
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(image) => {
                    let (mime, ext) = image_type(image.format());
                    let bytes = image.bytes().to_vec();
                    let preview = Some(Arc::new(image.clone()));
                    let channel = channel.clone();
                    self.client.update(cx, |c, cx| {
                        c.stage_bytes(channel, format!("pasted-image.{ext}"), mime.into(), bytes, preview, cx)
                    });
                }
                ClipboardEntry::ExternalPaths(paths) => {
                    self.stage_paths(channel.clone(), paths.paths().to_vec(), cx);
                }
                ClipboardEntry::String(_) => {}
            }
        }
    }

    // ---------------------------------------------------------------- emoji picker

    fn open_picker(&mut self, at: Point<Pixels>, target: PickerTarget, window: &mut Window, cx: &mut Context<Self>) {
        // Each emoji button toggles its own picker (the composer's, a message's reactions).
        let key = picker_key(&target);
        if self.picker.as_ref().is_some_and(|p| picker_key(&p.2) == key) {
            self.picker = None;
            cx.notify();
            return;
        }
        if self.just_closed(&key) {
            return;
        }
        let guild = self.current().and_then(|c| self.client.read(cx).guild_of_channel(&c).map(|g| g.guild.id.clone()));
        let client = self.client.clone();
        let picker = cx.new(|cx| EmojiPicker::new(client, guild, window, cx));
        let sub = cx.subscribe_in(&picker, window, |this, _, picked: &Picked, window, cx| {
            let Some((_, _, target, _)) = this.picker.take() else { return };
            match target {
                PickerTarget::Composer => {
                    this.composer.update(cx, |c, cx| c.insert(&picked.0, cx));
                    let focus = this.composer_focus(cx);
                    window.focus(&focus, cx);
                }
                PickerTarget::React { channel_id, message_id } => {
                    let emoji = picked.0.clone();
                    this.client.update(cx, |c, cx| c.toggle_reaction(channel_id, message_id, emoji, cx));
                }
            }
            cx.notify();
        });
        let dismiss = cx.subscribe_in(&picker, window, |this, _, _: &DismissEvent, window, cx| {
            if let Some(p) = this.picker.take() {
                this.picker_closed = Some((picker_key(&p.2), std::time::Instant::now()));
            }
            let focus = this.composer_focus(cx);
            window.focus(&focus, cx);
            cx.notify();
        });
        let at = if matches!(target, PickerTarget::Composer) { self.above_composer(at) } else { at };
        self.picker = Some((picker, at, target, vec![sub, dismiss]));
        cx.notify();
    }

    /// Whether a click on a picker's button only closed it (a second click on the button).
    fn just_closed(&mut self, key: &str) -> bool {
        self.picker_closed.take().is_some_and(|(k, at)| k == key && at.elapsed() < std::time::Duration::from_millis(400))
    }

    pub(crate) fn open_gif_picker(&mut self, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        if self.gif_picker.take().is_some() || self.just_closed("gif") {
            cx.notify();
            return;
        }
        let picker = cx.new(|cx| GifPicker::new(self.client.clone(), self.gif_picker_wide.clone(), window, cx));
        let pick = cx.subscribe_in(&picker, window, |this, _, picked: &PickedGif, window, cx| {
            this.gif_picker = None;
            if let Some(channel) = this.current() {
                let reply = this.replying.take();
                let url = picked.0.clone();
                this.client.update(cx, |c, cx| c.send_message(channel, url, reply, cx));
            }
            let focus = this.composer_focus(cx);
            window.focus(&focus, cx);
            cx.notify();
        });
        let dismiss = cx.subscribe_in(&picker, window, |this, _, _: &DismissEvent, window, cx| {
            this.gif_picker = None;
            this.picker_closed = Some(("gif".into(), std::time::Instant::now()));
            let focus = this.composer_focus(cx);
            window.focus(&focus, cx);
            cx.notify();
        });
        let at = self.above_composer(at);
        self.gif_picker = Some((picker, at, vec![pick, dismiss]));
        cx.notify();
    }

    /// Where a composer picker opens: above the message bar, flush with its right edge.
    fn above_composer(&self, fallback: Point<Pixels>) -> Point<Pixels> {
        let b = self.composer_bounds.get();
        if b.size.width > px(0.) { gpui::point(b.right(), b.top() - px(6.)) } else { fallback }
    }

    fn render_gif_picker(&self) -> Option<AnyElement> {
        let (picker, at, _) = self.gif_picker.as_ref()?;
        Some(
            deferred(anchored().position(*at).anchor(Anchor::BottomRight).snap_to_window_with_margin(px(8.)).child(motion::pop_in(div().relative().child(picker.clone()), "gif-picker-in")))
                .with_priority(2)
                .into_any_element(),
        )
    }

    pub(crate) fn react_picker(&mut self, at: Point<Pixels>, message_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel_id) = self.current() else { return };
        self.open_picker(at, PickerTarget::React { channel_id, message_id }, window, cx);
    }

    // ---------------------------------------------------------------- autocomplete

    fn update_suggestions(&mut self, cx: &mut Context<Self>) {
        let (text, caret) = {
            let c = self.composer.read(cx);
            (c.text().to_owned(), c.cursor())
        };
        let token = autocomplete::token_at(&text, caret);
        let Some(token) = token else {
            self.close_suggestions(cx);
            return;
        };
        let items = self.suggestions_for(token.trigger, &token.query, cx);
        if items.is_empty() {
            self.close_suggestions(cx);
            return;
        }
        let selected = self.suggest.as_ref().map_or(0, |s| s.selected.min(items.len() - 1));
        self.suggest = Some(Suggest { start: token.start, items, selected });
        self.composer.update(cx, |c, _| c.capture_navigation(true));
        cx.notify();
    }

    fn close_suggestions(&mut self, cx: &mut Context<Self>) {
        if self.suggest.take().is_some() {
            self.composer.update(cx, |c, _| c.capture_navigation(false));
            cx.notify();
        }
    }

    fn suggestions_for(&mut self, trigger: Trigger, query: &str, cx: &mut Context<Self>) -> Vec<Suggestion> {
        const LIMIT: usize = 8;
        let Some(channel) = self.current() else { return Vec::new() };
        let q = query.to_lowercase();
        let guild_id = self.client.read(cx).guild_of_channel(&channel).map(|g| g.guild.id.clone());
        let mut out = Vec::new();
        match trigger {
            Trigger::Mention => {
                let Some(gid) = guild_id else { return out };
                let members = self.client.update(cx, |c, cx| c.lookup_members(&gid, &q, cx));
                let client = self.client.read(cx);
                for m in members.iter().take(LIMIT) {
                    out.push(Suggestion {
                        label: m.shown_name().to_owned(),
                        hint: m.user_id.clone(),
                        insert: format!("<{}>", m.user_id),
                        avatar: Some((m.user_id.clone(), client.avatar_url(Some(&gid), &m.user_id))),
                        glyph: None,
                        icon: None,
                    });
                }
                if let Some(entry) = client.guild(&gid) {
                    for role in entry.guild.roles.iter().filter(|r| r.id != gid && r.name.to_lowercase().contains(&q)) {
                        out.push(Suggestion {
                            label: format!("@{}", role.name),
                            hint: "Role".into(),
                            insert: format!("<@&{}>", role.id),
                            avatar: None,
                            glyph: None,
                            icon: Some(Icon::Users),
                        });
                    }
                }
                if client.can(&channel, perms::MENTION_EVERYONE) {
                    for (word, hint) in [("everyone", "Notify everyone in the channel"), ("here", "Notify everyone online")] {
                        if word.starts_with(&q) {
                            out.push(Suggestion {
                                label: format!("@{word}"),
                                hint: hint.into(),
                                insert: format!("@{word}"),
                                avatar: None,
                                glyph: None,
                                icon: Some(Icon::AtSign),
                            });
                        }
                    }
                }
            }
            Trigger::Channel => {
                let client = self.client.read(cx);
                if let Some(entry) = guild_id.as_deref().and_then(|g| client.guild(g)) {
                    for ch in entry.ordered_channels().into_iter().filter(|c| c.is_textual() && c.name.to_lowercase().contains(&q)) {
                        out.push(Suggestion {
                            label: ch.name.clone(),
                            hint: ch.topic.clone().unwrap_or_default(),
                            insert: format!("<#{}>", ch.id),
                            avatar: None,
                            glyph: None,
                            icon: Some(if ch.kind == "announcement" { Icon::Megaphone } else { Icon::Hash }),
                        });
                    }
                }
            }
            Trigger::Emoji => {
                let client = self.client.read(cx);
                if let Some(gid) = guild_id.as_deref() {
                    for e in client.guild_emoji(gid).iter().filter(|e| e.name.to_lowercase().contains(&q)) {
                        out.push(Suggestion {
                            label: format!(":{}:", e.name),
                            hint: "Custom emoji".into(),
                            insert: format!("<{}:{}>", e.name, e.id),
                            avatar: None,
                            glyph: client.emoji_url(Some(gid), &e.id, None),
                            icon: None,
                        });
                    }
                }
                for e in rhein_store::emoji::search(&q, LIMIT) {
                    out.push(Suggestion {
                        label: e.shortcode().map(|s| format!(":{s}:")).unwrap_or_else(|| e.name().to_owned()),
                        hint: e.name().to_owned(),
                        insert: e.as_str().to_owned(),
                        avatar: None,
                        glyph: None,
                        icon: None,
                    });
                }
            }
        }
        out.truncate(LIMIT);
        out
    }

    fn navigate_suggestions(&mut self, nav: Nav, cx: &mut Context<Self>) {
        let Some(suggest) = self.suggest.as_mut() else { return };
        let n = suggest.items.len();
        match nav {
            Nav::Up => suggest.selected = (suggest.selected + n - 1) % n,
            Nav::Down => suggest.selected = (suggest.selected + 1) % n,
            Nav::Accept => {
                let ix = suggest.selected;
                self.accept_suggestion(ix, cx);
                return;
            }
            Nav::Close => {
                self.close_suggestions(cx);
                return;
            }
        }
        cx.notify();
    }

    fn accept_suggestion(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(suggest) = self.suggest.take() else { return };
        let Some(item) = suggest.items.get(ix).cloned() else { return };
        let caret = self.composer.read(cx).cursor();
        let insert = format!("{} ", item.insert);
        self.composer.update(cx, |c, cx| {
            c.capture_navigation(false);
            c.replace_range(suggest.start..caret, &insert, cx);
        });
        if item.glyph.is_some() || !item.insert.starts_with('<') && !item.insert.starts_with('@') {
            self.client.update(cx, |c, _| c.use_emoji(item.insert.clone()));
        }
        self.suggest = None;
        cx.notify();
    }

    fn render_suggestions(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let suggest = self.suggest.as_ref()?;
        let t = cx.theme().clone();
        // Anchored by its bottom edge, so it fades rather than rises.
        Some(motion::fade_in(
            div()
                .absolute()
                .bottom(px(64.))
                .left(px(16.))
                .right(px(16.))
                .p_1()
                .rounded(radius::LG)
                .bg(t.bg_raised)
                .border_1()
                .border_color(t.border_strong)
                .shadow_lg()
                .flex()
                .flex_col()
                .children(suggest.items.iter().enumerate().map(|(i, item)| {
                    let selected = i == suggest.selected;
                    let leading: AnyElement = match (&item.avatar, &item.glyph, item.icon) {
                        (Some((id, url)), _, _) => Avatar::new(id.clone(), item.label.clone())
                            .size(px(24.))
                            .image(url.clone().map(Into::into))
                            .into_any_element(),
                        (_, Some(url), _) => img(rhein_ui::images::source(url.clone())).size(px(22.)).into_any_element(),
                        (_, _, Some(i)) => icon(i).text_color(t.text_muted).into_any_element(),
                        _ => div().w(px(24.)).text_size(px(18.)).font_family(rhein_ui::theme::FONT_EMOJI).child(item.insert.clone()).into_any_element(),
                    };
                    div()
                        .id(("suggestion", i))
                        .px_2()
                        .py_1()
                        .rounded(radius::MD)
                        .flex()
                        .items_center()
                        .gap_2()
                        .cursor_pointer()
                        .when(selected, |d| d.bg(t.bg_selected))
                        .hover(|s| s.bg(t.bg_hover))
                        .on_click(cx.listener(move |this, _, _, cx| this.accept_suggestion(i, cx)))
                        .child(leading)
                        .child(div().text_size(text::MD).child(item.label.clone()))
                        .child(div().flex_1().min_w_0().truncate().text_size(text::SM).text_color(t.text_faint).child(item.hint.clone()))
                })),
                "suggest-in",
                motion::FAST,
            )
            .into_any_element(),
        )
    }

    // ---------------------------------------------------------------- context menu

    /// The menu of the message at `ix` in this channel, opened on one of its pictures.
    pub(crate) fn open_media_menu(&mut self, ix: usize, media: Media, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.current() else { return };
        let Some(message) = self.client.read(cx).logs.get(&channel).and_then(|log| log.messages.get(ix)).cloned() else { return };
        self.open_message_menu(message, Some(media), at, window, cx);
    }

    /// The menu of a message; opened on one of its pictures, it starts with copying and saving it.
    pub(crate) fn open_message_menu(&mut self, message: Message, media: Option<Media>, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.current() else { return };
        let client = self.client.read(cx);
        let mine = client.user_id() == Some(message.author.as_str());
        let moderate = client.can(&channel, perms::MANAGE_MESSAGES);
        let pinned = client.is_pinned(&channel, &message.id);
        let in_thread = client.channel(&channel).is_some_and(|c| c.is_thread());
        let this = cx.weak_entity();
        let mut entries = Vec::new();
        if let Some(media) = media {
            let (client, url) = (self.client.clone(), media.url.clone());
            entries.push(MenuEntry::item("Copy picture", move |_, cx| media::copy(&client, url.clone(), cx)).with_icon(Icon::Copy));
            let client = self.client.clone();
            entries.push(
                MenuEntry::item("Download picture", move |_, cx| media::download(&client, media.clone(), cx)).with_icon(Icon::Download),
            );
            entries.push(MenuEntry::Separator);
        }
        {
            let (this, id) = (this.clone(), message.id.clone());
            entries.push(
                MenuEntry::item("Add reaction", move |window, cx| {
                    let at = window.mouse_position();
                    let id = id.clone();
                    this.update(cx, |v, cx| v.react_picker(at, id, window, cx)).ok();
                })
                .with_icon(Icon::SmilePlus),
            );
        }
        {
            let (this, id) = (this.clone(), message.id.clone());
            entries.push(
                MenuEntry::item("Reply", move |window, cx| {
                    let id = id.clone();
                    this.update(cx, |v, cx| v.start_reply(id, window, cx)).ok();
                })
                .with_icon(Icon::Reply),
            );
        }
        if !in_thread {
            let (client, chan, id) = (self.client.clone(), channel.clone(), message.id.clone());
            let title: String = message.content.lines().next().unwrap_or("Thread").chars().take(60).collect();
            let title = if title.trim().is_empty() { "Thread".to_owned() } else { title };
            entries.push(
                MenuEntry::item("Start a thread", move |_, cx| {
                    let (chan, id, title) = (chan.clone(), id.clone(), title.clone());
                    client.update(cx, |c, cx| c.create_thread(chan, title, Some(id), cx));
                })
                .with_icon(Icon::MessagesSquare),
            );
        }
        if mine {
            let (this, m) = (this.clone(), message.clone());
            entries.push(
                MenuEntry::item("Edit message", move |window, cx| {
                    this.update(cx, |v, cx| v.start_edit(&m, window, cx)).ok();
                })
                .with_icon(Icon::Pencil),
            );
        }
        entries.push(MenuEntry::Separator);
        {
            // What is selected in this message, first.
            if let Some(part) = self.selection.as_ref().filter(|s| s.message == message.id).and_then(|s| s.selected()).map(str::to_owned) {
                entries.push(
                    MenuEntry::item("Copy selected text", move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(part.clone())))
                        .with_icon(Icon::Copy),
                );
            }
            let content = message.content.clone();
            entries.push(
                MenuEntry::item("Copy text", move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(content.clone())))
                    .with_icon(Icon::Copy),
            );
            let id = message.id.clone();
            entries.push(
                MenuEntry::item("Copy message ID", move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(id.clone())))
                    .with_icon(Icon::Copy),
            );
        }
        if moderate {
            let (client, chan, id) = (self.client.clone(), channel.clone(), message.id.clone());
            entries.push(
                MenuEntry::item(if pinned { "Unpin message" } else { "Pin message" }, move |_, cx| {
                    let (chan, id) = (chan.clone(), id.clone());
                    client.update(cx, |c, cx| {
                        c.set_pinned(chan.clone(), id, !pinned, cx);
                        c.load_pins(chan, cx);
                    });
                })
                .with_icon(Icon::Pin),
            );
        }
        if mine || moderate {
            entries.push(MenuEntry::Separator);
            let (this, m) = (this.clone(), message.clone());
            entries.push(
                MenuEntry::item("Delete message", move |window, cx| {
                    let m = m.clone();
                    this.update(cx, |v, cx| v.ask_delete(m, window, cx)).ok();
                })
                .with_icon(Icon::Trash)
                .danger(),
            );
        }
        // Know which messages are pinned before the menu is opened next time.
        if moderate && self.client.read(cx).pins(&channel).is_none() {
            let chan = channel.clone();
            self.client.update(cx, |c, cx| c.load_pins(chan, cx));
        }
        self.menu.show(at, entries, |v: &mut ChatView| &mut v.menu, window, cx);
    }

    /// A bot's select: its options as a menu. A single choice goes out at once; a select that
    /// takes several gathers them (each pick toggles one) until they are sent.
    pub(crate) fn open_select_menu(&mut self, message_id: String, select: rhein_proto::Component, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.borrow().clone() else { return };
        let Some(custom) = select.custom_id.clone() else { return };
        let (min, max) = (select.min.unwrap_or(1).max(1) as usize, select.max.unwrap_or(1).max(1) as usize);
        let key = (message_id.clone(), custom.clone());
        let picked = self.select_draft.get(&key).cloned().unwrap_or_default();
        let this = cx.weak_entity();
        let mut entries = Vec::new();
        for option in select.options.iter().take(25) {
            let value = option.value.clone();
            let on = picked.contains(&value);
            let label = match &option.description {
                Some(d) => format!("{} — {d}", option.label),
                None => option.label.clone(),
            };
            let (client, chan, mid, custom, key, this) = (self.client.clone(), channel.clone(), message_id.clone(), custom.clone(), key.clone(), this.clone());
            let mut entry = MenuEntry::item(label, move |_, cx| {
                if max == 1 {
                    let (chan, mid, custom, value) = (chan.clone(), mid.clone(), custom.clone(), value.clone());
                    client.update(cx, |c, cx| c.press_component(chan, mid, custom, vec![value], cx));
                    return;
                }
                let (key, value) = (key.clone(), value.clone());
                this.update(cx, |v, cx| {
                    let list = v.select_draft.entry(key).or_default();
                    match list.iter().position(|x| *x == value) {
                        Some(i) => {
                            list.remove(i);
                        }
                        None if list.len() < max => list.push(value),
                        None => {}
                    }
                    cx.notify();
                })
                .ok();
            });
            entry = entry.checked(max > 1 && on);
            entries.push(entry);
        }
        if max > 1 {
            entries.push(MenuEntry::Separator);
            let ready = (min..=max).contains(&picked.len());
            let (client, chan, mid, custom, key, this) = (self.client.clone(), channel.clone(), message_id.clone(), custom.clone(), key.clone(), this.clone());
            let label = if ready { format!("Send {} chosen", picked.len()) } else { format!("Choose {min} to {max}") };
            entries.push(
                MenuEntry::item(label, move |_, cx| {
                    if !ready {
                        return;
                    }
                    let values = this.update(cx, |v, _| v.select_draft.remove(&key).unwrap_or_default()).unwrap_or_default();
                    let (chan, mid, custom) = (chan.clone(), mid.clone(), custom.clone());
                    client.update(cx, |c, cx| c.press_component(chan, mid, custom, values, cx));
                })
                .with_icon(Icon::SendHorizontal),
            );
        }
        self.menu.show(at, entries, |v: &mut ChatView| &mut v.menu, window, cx);
    }

    /// A forum: its posts (threads), the most active first, and a new post by title. A post
    /// opens like any thread.
    fn render_forum(&mut self, channel_id: &str, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let client = self.client.read(cx);
        let name = client.channel(channel_id).map(|c| c.name.clone()).unwrap_or_default();
        let topic = client.channel(channel_id).and_then(|c| c.topic.clone()).filter(|s| !s.trim().is_empty());
        let can_post = client.can(channel_id, rhein_store::perms::SEND_MESSAGES);
        let mut posts: Vec<rhein_proto::Thread> = client.threads_of(channel_id).map(<[_]>::to_vec).unwrap_or_default();
        posts.sort_by_key(|p| std::cmp::Reverse(p.last_active.or(p.created_at).unwrap_or(0)));
        let chan = channel_id.to_owned();
        let mut list = div().flex().flex_col().gap_1();
        if posts.is_empty() {
            list = list.child(div().text_size(text::SM).text_color(t.text_faint).child("No posts yet."));
        }
        for (i, post) in posts.into_iter().enumerate() {
            let id = post.id.clone();
            let author = post.owner.as_deref().map(|o| client.display_name(client.guild_of_channel(channel_id).map(|g| g.guild.id.as_str()), o));
            let meta = [
                author,
                Some(if post.message_count == 1 { "1 message".to_owned() } else { format!("{} messages", post.message_count) }),
                post.archived.then(|| "archived".to_owned()),
                post.locked.then(|| "locked".to_owned()),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
            list = list.child(
                div()
                    .id(("forum-post", i))
                    .p_3()
                    .rounded(radius::MD)
                    .bg(t.bg_panel)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.bg_selected))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(ChatEvent::OpenThread(id.clone()))))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(post.title.clone()))
                    .child(div().text_size(text::XS).text_color(t.text_faint).child(meta)),
            );
        }
        let new_post = can_post.then(|| {
            div()
                .flex()
                .gap_2()
                .child(div().flex_1().child(self.forum_title.clone()))
                .child(Button::new("forum-new-post", "New post").primary().on_click(cx.listener(move |v, _, _, cx| {
                    let title = v.forum_title.read(cx).text().trim().to_owned();
                    if title.is_empty() {
                        return;
                    }
                    v.forum_title.update(cx, |i, cx| i.clear(cx));
                    let chan = chan.clone();
                    v.client.update(cx, |c, cx| c.create_thread(chan, title, None, cx));
                })))
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(48.))
                    .flex_none()
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(t.border)
                    .child(icon(Icon::MessagesSquare).text_color(t.text_faint))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(name))
                    .children(topic.map(|topic| div().min_w_0().truncate().text_size(text::SM).text_color(t.text_faint).child(topic))),
            )
            .child(
                div()
                    .id("forum-posts")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(div().text_size(text::SM).text_color(t.text_muted).child("Every post here is a thread of its own."))
                    .children(new_post)
                    .child(list),
            )
            .into_any_element()
    }

    fn ask_delete(&mut self, message: Message, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_delete = Some(message);
        window.focus(&self.modal_focus, cx);
        cx.notify();
    }

    fn close_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_delete = None;
        let focus = self.composer_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn render_delete_confirm(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let message = self.confirm_delete.clone()?;
        let t = cx.theme().clone();
        let this = cx.weak_entity();
        let (client, chan, id) = (self.client.clone(), message.channel_id.clone(), message.id.clone());
        let preview: String = message.content.chars().take(200).collect();
        Some(
            Modal::new("delete-message")
                .title("Delete message?")
                .width(px(420.))
                .focus(&self.modal_focus)
                .on_dismiss(move |window, cx| {
                    this.update(cx, |v, cx| v.close_delete(window, cx)).ok();
                })
                .child(
                    div()
                        .p_3()
                        .rounded(radius::MD)
                        .bg(t.bg_main)
                        .text_color(t.text_muted)
                        .child(if preview.is_empty() { "(attachment)".to_owned() } else { preview }),
                )
                .footer(Button::new("delete-cancel", "Cancel").ghost().on_click(cx.listener(|this, _, window, cx| this.close_delete(window, cx))))
                .footer(Button::new("delete-confirm", "Delete").danger().icon(Icon::Trash).on_click(cx.listener(
                    move |this, _, window, cx| {
                        let (chan, id) = (chan.clone(), id.clone());
                        client.update(cx, |c, cx| c.delete_message(chan, id, cx));
                        this.close_delete(window, cx);
                    },
                )))
                .into_any_element(),
        )
    }

    pub(crate) fn open_thread(&mut self, thread_id: String, cx: &mut Context<Self>) {
        cx.emit(ChatEvent::OpenThread(thread_id));
    }

    /// A private conversation's call, while anyone is in it: who, who is speaking, how private
    /// it is, and joining or leaving.
    fn render_dm_call(&mut self, conversation: &str, cx: &mut Context<Self>) -> Option<AnyElement> {
        let controls = self.call_controls("dm-call", cx);
        let t = cx.theme().clone();
        // Each face is a view of its own that rings while that person speaks.
        let looks: Vec<(String, String, Option<String>)> = {
            let client = self.client.read(cx);
            client
                .voice_members(conversation, conversation)
                .iter()
                .map(|m| (m.user_id.clone(), client.display_name(None, &m.user_id), client.avatar_url(None, &m.user_id)))
                .collect()
        };
        self.faces.retain(|user, _| looks.iter().any(|(u, _, _)| u == user));
        let speaking = self.client.read(cx).call_speaking.clone();
        let faces: Vec<AnyElement> = looks
            .into_iter()
            .map(|(user, name, avatar)| {
                let face = self
                    .faces
                    .entry(user.clone())
                    .or_insert_with(|| cx.new(|cx| SpeakingFace::new(speaking.clone(), user, px(28.), cx)))
                    .clone();
                face.update(cx, |f, cx| f.set_look(name, avatar, cx));
                face.cached(gpui::StyleRefinement::default().size(px(32.)).flex_none()).into_any_element()
            })
            .collect();
        let client = self.client.read(cx);
        let members = client.voice_members(conversation, conversation);
        let call = client.call().filter(|c| c.channel_id == conversation);
        if members.is_empty() && call.is_none() {
            return None;
        }
        let names: Vec<String> = members.iter().map(|m| client.display_name(None, &m.user_id)).collect();
        let line = match call {
            Some(call) => match &call.status {
                rhein_store::CallStatus::Connecting => "Connecting…".to_owned(),
                rhein_store::CallStatus::Failed(err) => err.clone(),
                rhein_store::CallStatus::Connected => {
                    let how = match (&call.mode, call.posture) {
                        (Some(rhein_store::CallMode::Sfu { .. }), _) => "through an SFU, end-to-end encrypted",
                        (_, Some(rhein_voice::posture::Posture::Relay | rhein_voice::posture::Posture::Sfu)) => "peer to peer through relays",
                        _ => "peer to peer",
                    };
                    format!("In the call {how}")
                }
            },
            None => format!("{} in a call", names.join(", ")),
        };
        let failed = call.is_some_and(|c| matches!(c.status, rhein_store::CallStatus::Failed(_)));
        let in_call = call.is_some();
        let button = if in_call {
            controls
        } else {
            let (c, id) = (self.client.clone(), conversation.to_owned());
            Button::new("dm-call-join", "Join").primary().icon(Icon::Phone).on_click(move |_, _, cx| c.update(cx, |c, cx| c.join_dm_call(id.clone(), cx))).into_any_element()
        };
        Some(
            div()
                .mx_4()
                .mt_2()
                .p_2()
                .rounded(radius::MD)
                .bg(t.bg_panel)
                .flex()
                .items_center()
                .gap_3()
                .child(div().flex().gap_1().children(faces))
                .child(div().flex_1().min_w_0().truncate().text_size(text::SM).text_color(if failed { t.danger } else { t.text_muted }).child(line))
                .child(button)
                .into_any_element(),
        )
    }

    /// Opens or closes a voice channel's chat column.
    fn toggle_voice_chat(&mut self, channel_id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.voice_chat.take().is_some() {
            self.client.update(cx, |c, cx| c.set_voice_chat(None, cx));
            cx.notify();
            return;
        }
        let chat = cx.new(|cx| ChatView::fixed(self.client.clone(), channel_id.clone(), window, cx));
        // Its close button closes the column; a profile or a menu opened in it goes on up.
        let sub = cx.subscribe(&chat, |this, _, event: &ChatEvent, cx| match event {
            ChatEvent::ClosePanel => {
                this.voice_chat = None;
                this.client.update(cx, |c, cx| c.set_voice_chat(None, cx));
                cx.notify();
            }
            ChatEvent::Profile { user_id, at } => cx.emit(ChatEvent::Profile { user_id: user_id.clone(), at: *at }),
            ChatEvent::MemberMenu { user_id, at } => cx.emit(ChatEvent::MemberMenu { user_id: user_id.clone(), at: *at }),
            ChatEvent::OpenThread(id) => cx.emit(ChatEvent::OpenThread(id.clone())),
            _ => {}
        });
        self.voice_chat = Some((chat, sub));
        self.client.update(cx, |c, cx| c.set_voice_chat(Some(channel_id), cx));
        cx.notify();
    }

    /// The call's controls, where the call is shown: microphone, sound, screen share, leave.
    fn call_controls(&self, prefix: &'static str, cx: &mut Context<Self>) -> AnyElement {
        let client = self.client.read(cx);
        let (muted, deafened) = (client.voice_muted(), client.voice_deafened());
        let sharing = client.call().is_some_and(|c| c.sharing_screen);
        let hidden = client.own_screen_hidden();
        let (c1, c2, c3, c4, c5) = (self.client.clone(), self.client.clone(), self.client.clone(), self.client.clone(), self.client.clone());
        let id = |what: &str| SharedString::from(format!("{prefix}-{what}"));
        div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                IconButton::new(id("mute"), if muted { Icon::MicOff } else { Icon::Mic })
                    .danger(muted)
                    .tooltip(if muted { "Unmute" } else { "Mute" })
                    .on_click(move |_, _, cx| c1.update(cx, |c, cx| c.set_voice_muted(!muted, cx))),
            )
            .child(
                IconButton::new(id("deafen"), if deafened { Icon::HeadphoneOff } else { Icon::Headphones })
                    .danger(deafened)
                    .tooltip(if deafened { "Undeafen" } else { "Deafen" })
                    .on_click(move |_, _, cx| c2.update(cx, |c, cx| c.set_voice_deafened(!deafened, cx))),
            )
            .child(
                // While sharing, the crossed-out screen: a press stops it.
                IconButton::new(id("screen"), if sharing { Icon::ScreenShareOff } else { Icon::ScreenShare })
                    .danger(sharing)
                    .tooltip(if sharing { "Stop sharing your screen" } else { "Share your screen" })
                    .on_click(move |_, _, cx| c3.update(cx, |c, cx| if sharing { c.stop_screen_share(cx) } else { c.start_screen_share(cx) })),
            )
            // Only while sharing: our own preview off (the share goes on) or back on.
            .when(sharing, |d| {
                d.child(
                    IconButton::new(id("preview"), if hidden { Icon::EyeOff } else { Icon::Eye })
                        .active(hidden)
                        .tooltip(if hidden { "Show your screen to yourself again" } else { "Hide your screen from yourself (others still see it)" })
                        .on_click(move |_, _, cx| c5.update(cx, |c, cx| c.set_own_screen_hidden(!hidden, cx))),
                )
            })
            .child(
                IconButton::new(id("leave"), Icon::PhoneOff)
                    .danger(true)
                    .tooltip("Leave the call")
                    .on_click(move |_, _, cx| c4.update(cx, |c, cx| c.leave_voice(cx))),
            )
            .into_any_element()
    }

    /// A voice channel: who is in it, who is speaking, and joining or leaving it.
    fn render_voice_stage(&mut self, channel_id: &str, cx: &mut Context<Self>) -> AnyElement {
        use rhein_store::Slot;
        let mut controls = Some(self.call_controls("voice", cx));
        let t = cx.theme().clone();
        // The screen on the stage (someone else's we watch, else our own while we share) and the
        // cameras, each drawn by a view of its own.
        let (screen_user, camera_users) = {
            let client = self.client.read(cx);
            let me = client.user_id();
            let members = client
                .guild_of_channel(channel_id)
                .map(|g| client.voice_members(&g.guild.id, channel_id))
                .unwrap_or_default();
            let call = client.call().filter(|c| c.channel_id == channel_id);
            let has = |user: &str, slot| call.is_some_and(|c| c.has_video(user, slot));
            let screen = members
                .iter()
                .filter(|m| me != Some(m.user_id.as_str()))
                .chain(members.iter().filter(|m| me == Some(m.user_id.as_str())))
                .find(|m| has(&m.user_id, Slot::Screen))
                .map(|m| m.user_id.clone());
            let cameras: Vec<String> =
                members.iter().filter(|m| m.self_video && has(&m.user_id, Slot::Camera)).map(|m| m.user_id.clone()).collect();
            (screen, cameras)
        };
        let mut wanted: HashSet<(String, Slot)> = camera_users.iter().map(|u| (u.clone(), Slot::Camera)).collect();
        wanted.extend(screen_user.iter().map(|u| (u.clone(), Slot::Screen)));
        self.live.retain(|key, _| wanted.contains(key));
        for (user, slot) in wanted {
            let video = self.client.read(cx).call_video.clone();
            let cover = slot == Slot::Camera;
            self.live.entry((user.clone(), slot)).or_insert_with(|| cx.new(|cx| LiveVideo::new(video, user, slot, cover, cx)));
        }
        let live = |user: &str, slot| {
            self.live.get(&(user.to_owned(), slot)).map(|view| view.clone().cached(gpui::StyleRefinement::default().size_full()))
        };
        let screen = screen_user.as_deref().and_then(|user| live(user, Slot::Screen).map(|view| (user.to_owned(), view)));
        let client = self.client.read(cx);
        let speaking = client.call_speaking.read(cx);
        let name = client.channel(channel_id).map(|c| c.name.clone()).unwrap_or_default();
        let guild_id = client.guild_of_channel(channel_id).map(|g| g.guild.id.clone());
        let members = guild_id.as_deref().map(|g| client.voice_members(g, channel_id)).unwrap_or_default();
        let call = client.call().filter(|c| c.channel_id == channel_id);
        let in_call = call.is_some();
        let note = call.map(|call| match &call.status {
            rhein_store::CallStatus::Connecting => ("Connecting…".to_owned(), t.text_faint),
            rhein_store::CallStatus::Failed(err) => (err.clone(), t.danger),
            rhein_store::CallStatus::Connected => (
                match &call.mode {
                    Some(rhein_store::CallMode::Sfu { .. }) => "Connected through the server's SFU.".to_owned(),
                    _ => format!("Connected peer to peer with {} of {}.", call.linked.len(), members.len().saturating_sub(1)),
                },
                t.text_faint,
            ),
        });
        let me = client.user_id().map(str::to_owned);
        let sfu = call.is_some_and(|c| matches!(c.mode, Some(rhein_store::CallMode::Sfu { .. })));
        let tiles = members.iter().map(|state| {
            let camera = live(&state.user_id, Slot::Camera);
            let mine = me.as_deref() == Some(state.user_id.as_str());
            let watched = call.is_some_and(|c| c.watching.contains(&state.user_id));
            // On an SFU a screen share has to be asked for; a mesh sends it anyway.
            let watch = (in_call && sfu && state.streaming && !mine).then(|| {
                let (c, user) = (self.client.clone(), state.user_id.clone());
                Button::new(SharedString::from(format!("watch-{}", state.user_id)), if watched { "Stop watching" } else { "Watch" })
                    .small()
                    .icon(Icon::ScreenShare)
                    .on_click(move |_, _, cx| c.update(cx, |c, cx| c.watch_screen(&user, !watched, cx)))
            });
            let who = client.display_name(guild_id.as_deref(), &state.user_id);
            let avatar = client.avatar_url(guild_id.as_deref(), &state.user_id);
            let badge = |glyph: Icon, color: Option<gpui::Rgba>| {
                div()
                    .size(px(24.))
                    .rounded_full()
                    .bg(t.bg_base)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(rhein_ui::icon_sized(glyph, px(14.)).text_color(color.unwrap_or(t.text_muted)))
            };
            let speaking = speaking.is_speaking(&state.user_id);
            div()
                .w(px(200.))
                .h(px(if watch.is_some() { 180. } else { 150. }))
                .rounded(radius::LG)
                .bg(t.bg_panel)
                .border_2()
                .border_color(if speaking { t.success } else { gpui::transparent_black().into() })
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .overflow_hidden()
                .relative()
                .child(match camera {
                    Some(view) => div().absolute().inset_0().size_full().child(view).into_any_element(),
                    None => Avatar::new(state.user_id.clone(), who.clone()).size(px(64.)).image(avatar.map(Into::into)).into_any_element(),
                })
                .child(div().font_weight(FontWeight::MEDIUM).child(who))
                .children(watch)
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .when(state.self_mute || state.server_mute, |d| d.child(badge(Icon::MicOff, state.server_mute.then_some(t.danger))))
                        .when(state.self_deaf || state.server_deaf, |d| d.child(badge(Icon::HeadphoneOff, state.server_deaf.then_some(t.danger))))
                        .when(state.self_video, |d| d.child(badge(Icon::Video, None)))
                        .when(state.streaming, |d| d.child(badge(Icon::ScreenShare, Some(t.live)))),
                )
        });
        // The channel's own chat, beside the call; unread while it is closed.
        let chat_open = self.voice_chat.is_some();
        let unread = !chat_open && client.read_mark(channel_id).is_unread();
        let chat_toggle = {
            let id = channel_id.to_owned();
            div()
                .relative()
                .child(
                    IconButton::new("voice-chat", Icon::MessageSquare)
                        .large()
                        .active(chat_open)
                        .tooltip(if chat_open { "Close the chat" } else { "Chat" })
                        .on_click(cx.listener(move |this, _, window, cx| this.toggle_voice_chat(id.clone(), window, cx))),
                )
                .when(unread, |d| d.child(div().absolute().top_1().right_1().size(px(8.)).rounded_full().bg(t.danger)))
        };
        let column = self.voice_chat.as_ref().map(|(chat, _)| {
            div()
                .w(px(380.))
                .flex_none()
                .h_full()
                .border_l_1()
                .border_color(t.border)
                .child(chat.clone().cached(gpui::StyleRefinement::default().size_full()))
        });
        let stage = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(t.bg_base)
            .text_color(t.text)
            .child(
                div()
                    .h(size::HEADER)
                    .flex_none()
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(t.border)
                    .child(icon(Icon::Volume).text_color(t.text_faint))
                    .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child(name))
                    .child(chat_toggle),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_6()
                    .p_8()
                    .children(screen.map(|(user, picture)| {
                        let label = if me.as_deref() == Some(user.as_str()) {
                            "Your screen".to_owned()
                        } else {
                            format!("{}'s screen", client.display_name(guild_id.as_deref(), &user))
                        };

                        div()
                            .w_full()
                            .flex_1()
                            .min_h(px(240.))
                            .rounded(radius::LG)
                            .bg(t.bg_panel)
                            .overflow_hidden()
                            .relative()
                            .child(picture)
                            .child(
                                div()
                                    .absolute()
                                    .left_2()
                                    .bottom_2()
                                    .px_2()
                                    .py_0p5()
                                    .rounded(radius::SM)
                                    .bg(t.bg_base)
                                    .text_size(text::SM)
                                    .child(label),
                            )
                    }))
                    .child(if members.is_empty() {
                        div().text_color(t.text_faint).child("Nobody is here right now.").into_any_element()
                    } else {
                        div().flex().flex_wrap().justify_center().gap_3().children(tiles).into_any_element()
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_2()
                            .child(if in_call {
                                div().p_1().rounded(radius::LG).bg(t.bg_panel).border_1().border_color(t.border).child(controls.take().unwrap_or_else(|| div().into_any_element())).into_any_element()
                            } else {
                                let (c, id) = (self.client.clone(), channel_id.to_owned());
                                Button::new("voice-join", "Join voice")
                                    .primary()
                                    .icon(Icon::Phone)
                                    .on_click(move |_, _, cx| c.update(cx, |c, cx| c.join_voice(id.clone(), cx)))
                                    .into_any_element()
                            })
                            .children(note.map(|(line, color)| div().text_size(text::SM).text_color(color).child(line))),
                    ),
            )
;
        div().flex_1().min_w_0().size_full().flex().child(stage).children(column).into_any_element()
    }

    /// The history's part, made the first time it is drawn.
    fn history_part(&mut self, cx: &mut Context<Self>) -> Entity<Part<ChatView>> {
        if let Some(history) = &self.history {
            return history.clone();
        }
        let this = cx.entity();
        let client = self.client.clone();
        let sound = client.read(cx).media_tick.clone();
        let posters = client.read(cx).movie_posters.clone();
        let history = part(&this, |chat, window, cx| chat.history(window, cx), cx);
        history.update(cx, |p, cx| {
            p.watch(&client, cx);
            // A sound file playing moves its bar.
            p.watch(&sound, cx);
            // A video's poster arriving can change its row's height.
            p.watch(&posters, cx);
        });
        self.history = Some(history.clone());
        history
    }

    /// The messages, with the jump to the present over them: everything that moves as the
    /// history scrolls. A cached part of its own (see `part`), so typing in the message bar or a
    /// pulse of the typing line does not draw the history again.
    fn history(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(channel_id) = self.current() else { return div().into_any_element() };
        self.prefetch(cx);

        // Wheel glides move the list without its scroll handler, so older messages are also
        // asked for here once the top comes near.
        if !self.list.is_following_tail() && self.list.logical_scroll_top().item_ix <= 2
            && let Some(id) = self.current()
        {
            let client = self.client.clone();
            cx.defer(move |cx| {
                client.update(cx, |client, cx| {
                    if client.logs.get(&id).is_some_and(|l| l.loaded && !l.loading && !l.reached_start && l.error.is_none()) {
                        client.load_history(id, true, cx);
                    }
                })
            });
        }

        let row_ctx = Rc::new(RowCtx {
            client: self.client.clone(),
            view: cx.weak_entity(),
            channel_id: channel_id.clone(),
            picture_sizes: self.picture_sizes.clone(),
            scale_factor: window.scale_factor(),
            parsed: self.parsed.clone(),
            movie: self.movie.clone(),
            movie_big: self.movie_big,
            divider: self.client.read(cx).divider_index(&channel_id),
        });
        // Each row is a view of its own, drawn cached at the size it last had: a scroll moves the
        // rows without drawing them again, and a hover or a picture arriving draws only its row.
        // A row whose `RowUi` changed draws afresh in this frame (uncached), so it shows at once.
        let view = cx.weak_entity();
        let messages = rhein_ui::smooth_scroll(
            "history-glide",
            rhein_ui::ScrollTarget::List(self.list.clone()),
            list(self.list.clone(), move |ix, _, cx| {
                let row = view.upgrade().and_then(|view| view.update(cx, |this, cx| this.row_view(ix, &row_ctx, cx)));
                match row {
                    Some((row, false)) => row.cached(gpui::StyleRefinement::default().w_full()).keep_size().into_any_element(),
                    Some((row, true)) => row.into_any_element(),
                    None => div().into_any_element(),
                }
            })
            .size_full(),
        );

        let behind = !self.list.is_following_tail() && self.count(cx) > 0;
        let jump = behind.then(|| {
            div().absolute().bottom(px(8.)).right(px(16.)).child(
                Button::new("jump-present", "Jump to present")
                    .icon(Icon::ArrowDown)
                    .primary()
                    .on_click(cx.listener(|this, _, _, cx| this.glide_to_end(cx))),
            )
        });

        div()
            .relative()
            .size_full()
            // Room for the typing line between the last message and the message bar, with as
            // much space above it as below.
            .pb(HISTORY_BOTTOM)
            // Switching channels fades the new history in instead of swapping it hard.
            .child(motion::fade_in(div().size_full().child(messages), SharedString::from(format!("history-{channel_id}")), motion::FAST))
            .children(jump)
            .into_any_element()
    }

    fn render_picker(&self) -> Option<AnyElement> {
        let (picker, at, target, _) = self.picker.as_ref()?;
        let anchor = match target {
            PickerTarget::Composer => Anchor::BottomRight,
            PickerTarget::React { .. } => Anchor::TopRight,
        };
        Some(
            deferred(anchored().position(*at).anchor(anchor).snap_to_window_with_margin(px(8.)).child(motion::pop_in(div().relative().child(picker.clone()), "picker-in")))
                .with_priority(2)
                .into_any_element(),
        )
    }

    fn render_composer(&self, channel_id: &str, window: &Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = cx.theme().clone();
        let focused = self.composer.read(cx).is_focused(window);
        let client = self.client.read(cx);
        let locked = !client.may_write(channel_id);
        let guild_id = client.guild_of_channel(channel_id).map(|g| g.guild.id.clone());
        let typing: Vec<String> = client.typing_in(channel_id).iter().map(|u| client.display_name(guild_id.as_deref(), u)).collect();
        // Who is typing, just above the message bar, as in Discord: floating over the end of the
        // list, so the chat does not move when it comes and goes.
        let typing_line = typing_label(&typing).map(|label| {
            div()
                .absolute()
                .left(px(20.))
                // Its bottom `TYPING_GAP` above the message bar.
                .top(COMPOSER_TOP - TYPING_GAP - TYPING_H)
                .h(TYPING_H)
                .px_1()
                .rounded(radius::SM)
                .bg(t.bg_base)
                .flex()
                .items_center()
                .gap_1p5()
                .text_size(text::SM)
                .text_color(t.text_muted)
                .child(self.dots.clone().cached(gpui::StyleRefinement::default().w(px(21.)).h(px(5.))))
                .child(label)
        });

        let context_bar = if self.editing.is_some() {
            Some(("Editing message".to_owned(), None))
        } else if let Some(reply) = &self.replying {
            client.find_message(channel_id, reply).map(|m| {
                let name = client.display_name(guild_id.as_deref(), &m.author);
                (format!("Replying to {name}"), Some(snippet(&m.content)))
            })
        } else {
            None
        };
        let context_bar = context_bar.map(|(label, detail)| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1p5()
                .rounded_t(radius::MD)
                .bg(t.bg_panel)
                .text_size(text::SM)
                .text_color(t.text_muted)
                .child(icon(if self.editing.is_some() { Icon::Pencil } else { Icon::Reply }).text_color(t.text_faint))
                .child(div().font_weight(FontWeight::MEDIUM).child(label))
                .children(detail.map(|d| div().flex_1().min_w_0().truncate().text_color(t.text_faint).child(d)))
                .child(div().flex_1())
                .child(
                    IconButton::new("composer-cancel", Icon::X)
                        .tooltip("Cancel (Esc)")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                )
        });

        let staged = client.staged(channel_id);
        let chips = (!staged.is_empty()).then(|| {
            div().flex().flex_wrap().gap_2().pb_2().children(staged.iter().map(|s| {
                let (status, color) = match &s.state {
                    StagedState::Uploading => ("Uploading…".to_owned(), t.text_faint),
                    StagedState::Ready(_) => (format_size(s.size as i64), t.text_faint),
                    StagedState::Local(_) => (format!("{} · encrypted when sent", format_size(s.size as i64)), t.text_faint),
                    StagedState::Failed(err) => (format!("Upload failed: {err}. Check your connection and try again."), t.danger),
                };
                let thumb: Option<gpui::AnyElement> = match (&s.preview, &s.state) {
                    (Some(image), _) => Some(img(rhein_ui::images::local(image.clone(), px(40.), window)).size(px(40.)).rounded(radius::SM).object_fit(ObjectFit::Cover).into_any_element()),
                    (None, StagedState::Ready(a)) if a.is_image() => a.url.clone().map(|u| {
                        img(rhein_ui::images::source(u)).size(px(40.)).rounded(radius::SM).object_fit(ObjectFit::Cover).into_any_element()
                    }),
                    _ => None,
                };
                let client = self.client.clone();
                let (chan, id) = (channel_id.to_owned(), s.id);
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_1()
                    .pr_2()
                    .rounded(radius::MD)
                    .bg(t.bg_raised)
                    .border_1()
                    .border_color(t.border)
                    .child(thumb.unwrap_or_else(|| {
                        div()
                            .size(px(40.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon(Icon::File).text_color(t.text_muted))
                            .into_any_element()
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .max_w(px(180.))
                            .child(div().text_size(text::SM).truncate().child(s.name.clone()))
                            .child(div().text_size(text::SM).text_color(color).truncate().child(status)),
                    )
                    .child(
                        IconButton::new(SharedString::from(format!("unstage-{}", s.id)), Icon::X)
                            .tooltip("Remove")
                            .on_click(move |_, _, cx| client.update(cx, |c, cx| c.unstage(&chan, id, cx))),
                    )
            }))
        });

        // The bar keeps the same distance from the bottom as from the sides.
        div()
            .relative()
            .flex_none()
            .px(COMPOSER_MARGIN)
            .pt(COMPOSER_TOP)
            .pb(COMPOSER_MARGIN)
            .flex()
            .flex_col()
            .children(typing_line)
            .children(chips)
            .children(context_bar)
            // A prompt line: `›`, the text with its block cursor, and the attach and emoji buttons;
            // where we may not write, a closed bar saying so.
            .child(if locked {
                div()
                    .h(px(48.))
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded(radius::MD)
                    .bg(t.bg_raised)
                    .border_1()
                    .border_color(t.border)
                    .opacity(0.7)
                    .cursor(gpui::CursorStyle::OperationNotAllowed)
                    .text_size(text::MD)
                    .text_color(t.text_faint)
                    .child(icon(Icon::Lock).text_color(t.text_faint))
                    .child("You don't have permission to send messages in this channel.")
                    .into_any_element()
            } else {
                div()
                    .relative()
                    .flex()
                    .items_start()
                    .gap_1()
                    .pl_2()
                    .pr(px(6.))
                    .py(px(6.))
                    // Where the box is, for the pickers to open just above it.
                    .child({
                        let bounds = self.composer_bounds.clone();
                        gpui::canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {}).absolute().size_full()
                    })
                    .rounded(radius::LG)
                    .bg(t.bg_raised)
                    .border_1()
                    .border_color(if focused { t.text_faint } else { t.border_strong })
                    .child(
                        div()
                            .h(size::ICON_BUTTON_LG)
                            .w(px(20.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .font_family(FONT_MONO)
                            .text_color(if focused { t.text } else { t.text_faint })
                            .child("›"),
                    )
                    .child(div().flex_1().min_w_0().py(px(5.)).child(self.composer.clone()))
                    .child(
                        IconButton::new("attach", Icon::Paperclip)
                            .large()
                            .tooltip("Attach files")
                            .on_click(cx.listener(|this, _, _, cx| this.attach(cx))),
                    )
                    // In a slot as tall as the icon buttons beside it, so all three share one
                    // middle.
                    .child(
                        div().h(size::ICON_BUTTON_LG).flex().items_center().child(
                            div()
                                .id("composer-gif")
                                .h(px(28.))
                                .px_1p5()
                                .flex()
                                .items_center()
                                .rounded(radius::MD)
                                .border_1()
                                .border_color(t.text_faint)
                                .text_color(t.text_muted)
                                .text_size(text::SM)
                                .font_weight(FontWeight::BOLD)
                                .cursor_pointer()
                                .hover(|s| s.text_color(t.text).border_color(t.text_muted))
                                .tooltip(rhein_ui::tooltip("Favourite GIFs"))
                                .on_click(cx.listener(|this, e: &gpui::ClickEvent, window, cx| this.open_gif_picker(e.position(), window, cx)))
                                .child("GIF"),
                        ),
                    )
                    .child(
                        IconButton::new("composer-emoji", Icon::Smile)
                            .large()
                            .tooltip("Emoji")
                            .on_click(cx.listener(|this, e: &gpui::ClickEvent, window, cx| {
                                this.open_picker(e.position(), PickerTarget::Composer, window, cx)
                            })),
                    )
                    .into_any_element()
            })
    }

    /// The picture over the whole window, as large as fits and smoothed when shrunk. A click
    /// anywhere but on the picture or its link closes it.
    fn render_lightbox(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (url, name) = self.lightbox.clone()?;
        let t = cx.theme().clone();
        let (open, save, save_name) = (url.clone(), url.clone(), name.clone());
        let viewport = window.viewport_size();
        // Room left by the padding (`p_8`), the caption row and the gap above it.
        let bound = gpui::size(viewport.width - px(64.), (viewport.height - px(64.) - px(48.)) * 0.95);
        let picture = rhein_ui::images::fit(url, bound, window, cx);
        let overlay = div()
            .id("lightbox")
            .occlude()
            .track_focus(&self.lightbox_focus)
            .key_context(LIGHTBOX_CONTEXT)
            .on_action(cx.listener(|this, _: &Dismiss, window, cx| this.close_lightbox(window, cx)))
            .w(viewport.width)
            .h(viewport.height)
            .bg(t.overlay)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .p_8()
            .on_click(cx.listener(|this, _, window, cx| this.close_lightbox(window, cx)))
            .child(
                img(picture.source)
                    .id("pic")
                    .map(|i| match picture.size {
                        Some(s) => i.w(s.width).h(s.height),
                        None => i.max_w_full().max_h(gpui::relative(0.85)),
                    })
                    .object_fit(ObjectFit::Contain)
                    .on_click(|_, _, cx| cx.stop_propagation()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_size(text::SM)
                    .text_color(t.text_muted)
                    .child(name)
                    .child(
                        Button::new("lightbox-open", "Open in browser")
                            .ghost()
                            .icon(Icon::ExternalLink)
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                cx.open_url(&open);
                            }),
                    )
                    .child({
                        let (client, media) = (self.client.clone(), Media::new(save, save_name));
                        Button::new("lightbox-download", "Download")
                            .ghost()
                            .icon(Icon::Download)
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                media::download(&client, media.clone(), cx);
                            })
                    }),
            );
        // Over the whole window, not just the chat, so the sidebars close it too.
        Some(deferred(anchored().position(gpui::point(px(0.), px(0.))).child(overlay)).with_priority(3).into_any_element())
    }
}

fn snippet(content: &str) -> String {
    let line = content.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut out: String = line.chars().take(120).collect();
    if line.chars().count() > 120 {
        out.push('…');
    }
    out
}

fn image_type(format: ImageFormat) -> (&'static str, &'static str) {
    match format {
        ImageFormat::Png => ("image/png", "png"),
        ImageFormat::Jpeg => ("image/jpeg", "jpg"),
        ImageFormat::Webp => ("image/webp", "webp"),
        ImageFormat::Gif => ("image/gif", "gif"),
        ImageFormat::Svg => ("image/svg+xml", "svg"),
        ImageFormat::Bmp => ("image/bmp", "bmp"),
        ImageFormat::Tiff => ("image/tiff", "tiff"),
        _ => ("application/octet-stream", "bin"),
    }
}

/// Logs (at debug, target `rhein::switch`) how the frames after a channel switch went: the
/// first one, every slow one and how many rows each drew, for three seconds.
fn time_switch(channel: String, since: Instant, window: &Window) {
    fn next(channel: String, since: Instant, last: Instant, rows: usize, window: &Window) {
        window.on_next_frame(move |window, _| {
            let now = Instant::now();
            let drawn = crate::views::rows::ROWS_DRAWN.load(std::sync::atomic::Ordering::Relaxed);
            let gap = now - last;
            if gap > Duration::from_millis(25) || last == since {
                log::debug!(target: "rhein::switch", "{channel}: frame at {:?}, {:?} after the last, {} row(s) drawn", now - since, gap, drawn - rows);
            }
            if now - since < Duration::from_secs(3) {
                next(channel, since, now, drawn, window);
            }
        });
    }
    let rows = crate::views::rows::ROWS_DRAWN.load(std::sync::atomic::Ordering::Relaxed);
    next(channel, since, since, rows, window);
}

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let (Some(since), Some(channel)) = (self.switched.take(), self.current())
            && log::log_enabled!(target: "rhein::switch", log::Level::Debug)
        {
            log::debug!(target: "rhein::switch", "{channel}: drawing {:?} after the switch", since.elapsed());
            time_switch(channel, since, window);
        }
        let t = cx.theme().clone();
        let channel_id = self.current();
        let client = self.client.read(cx);
        let Some(channel_id) = channel_id else {
            return div()
                .flex_1()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(t.bg_main)
                .text_color(t.text_muted)
                .flex_col()
                .gap_2()
                .child(icon(if client.showing_home { Icon::MessageSquare } else { Icon::Hash }).text_color(t.text_faint))
                .child(if client.showing_home {
                    "Pick a conversation."
                } else if client.guilds.is_empty() {
                    "No guilds yet. Create one or join with an invite (+ on the left)."
                } else {
                    "Pick a channel."
                })
                .into_any_element();
        };
        let channel = client.channel(&channel_id).cloned();
        if self.fixed.is_none() && channel.as_ref().is_some_and(|c| c.is_voice()) {
            return self.render_voice_stage(&channel_id, cx);
        }
        if self.fixed.is_none() && channel.as_ref().is_some_and(|c| c.is_forum()) {
            return self.render_forum(&channel_id, cx);
        }
        let client = self.client.read(cx);
        let log = client.logs.get(&channel_id);
        let loading = log.is_some_and(|l| l.loading);
        let reached_start = log.is_some_and(|l| l.reached_start);
        let history_error = log.and_then(|l| l.error.clone());
        // A private conversation is shown here like a channel: its title, a lock, no guild tools.
        let conversation = client.conversations.iter().find(|c| c.id == channel_id).cloned();
        let is_dm = conversation.is_some();
        let name = match &conversation {
            Some(c) => client.conversation_title(c),
            None => channel.as_ref().map(|c| c.name.clone()).unwrap_or_default(),
        };
        let topic = match &conversation {
            Some(_) => Some("end-to-end encrypted".to_owned()),
            None => channel.as_ref().and_then(|c| c.topic.clone()),
        };

        let guild_face = (!is_dm && self.fixed.is_none())
            .then(|| client.guild_of_channel(&channel_id))
            .flatten()
            .map(|g| {
                // A circle before the channel's name.
                let side = px(28.);
                let tile = div().size(side).flex_none().mr_1().rounded_full().overflow_hidden().bg(t.bg_selected).flex().items_center().justify_center();
                match client.guild_icon_url(&g.guild.id) {
                    // Shrunk properly to the pixels it covers: GPUI's own sampling is jagged.
                    Some(url) => tile.child(gpui::img(rhein_ui::images::scaled(url, side, window)).size_full().rounded_full().object_fit(gpui::ObjectFit::Cover)).into_any_element(),
                    None => tile
                        .text_size(text::LG)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.text)
                        .child(g.guild.name.chars().next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default())
                        .into_any_element(),
                }
            });
        let header = div()
            .h(size::HEADER)
            .flex_none()
            .px_4()
            .flex()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(t.border)
            // The guild's face, in its circle, before the channel.
            .children(guild_face)
            .child(
                icon(match (&conversation, self.fixed.is_some()) {
                    (Some(c), _) if c.kind == "gc" => Icon::Users,
                    (Some(_), _) => Icon::AtSign,
                    (None, true) => Icon::MessagesSquare,
                    (None, false) => Icon::Hash,
                })
                .text_color(t.text_faint),
            )
            .child(div().font_weight(FontWeight::SEMIBOLD).truncate().child(name.clone()))
            .children(topic.map(|topic| {
                div()
                    .ml_2()
                    .pl_3()
                    .border_l_1()
                    .border_color(t.bg_selected)
                    .text_size(text::SM)
                    .text_color(t.text_muted)
                    .truncate()
                    .child(topic)
            }))
            .child(div().flex_1())
            .when(is_dm, |d| d.child(icon(Icon::Lock).text_color(t.text_faint)))
            .map(|header| {
                if is_dm {
                    let (c, id) = (self.client.clone(), channel_id.clone());
                    let in_call = self.client.read(cx).in_call(&channel_id);
                    return header.child(
                        IconButton::new("dm-call", if in_call { Icon::PhoneOff } else { Icon::Phone })
                            .large()
                            .danger(in_call)
                            .tooltip(if in_call { "Leave the call" } else { "Start a call" })
                            .on_click(move |_, _, cx| {
                                c.update(cx, |c, cx| if in_call { c.leave_voice(cx) } else { c.join_dm_call(id.clone(), cx) })
                            }),
                    );
                }
                if self.fixed.is_some() {
                    return header.child(
                        IconButton::new("close-thread", Icon::X)
                            .large()
                            .tooltip("Close thread")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ChatEvent::ClosePanel))),
                    );
                }
                let active = self.active_panel;
                let button = |id: &'static str, glyph: Icon, tip: &'static str, kind: PanelKind, cx: &mut Context<Self>| {
                    IconButton::new(id, glyph)
                        .large()
                        .tooltip(tip)
                        .active(active == Some(kind))
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(ChatEvent::TogglePanel(kind))))
                };
                header
                    .child(button("panel-threads", Icon::MessagesSquare, "Threads", PanelKind::Threads, cx))
                    .child(button("panel-pins", Icon::Pin, "Pinned messages", PanelKind::Pins, cx))
                    .child(button("panel-search", Icon::Search, "Search", PanelKind::Search, cx))
                    .child(button("panel-members", Icon::Users, "Members", PanelKind::Members, cx))
            });

        let top_note = if loading {
            Some("Loading messages…".to_owned())
        } else if let Some(err) = history_error {
            Some(format!("Couldn't load history: {err}. Check your connection and try again."))
        } else if reached_start && is_dm {
            Some(format!("This is the start of your encrypted conversation with {name}."))
        } else if reached_start {
            Some(format!("This is the start of #{name}."))
        } else {
            None
        };

        // A message request: someone we don't know wrote first. Accept or decline before replying.
        let request = self.client.read(cx).conversations.iter().find(|c| c.id == channel_id).filter(|c| {
            c.request_state.as_deref() == Some("pending") && c.creator.as_deref() != self.client.read(cx).user_id()
        }).map(|c| c.id.clone());
        let request_bar = request.map(|id| {
            let (accept, decline) = (self.client.clone(), self.client.clone());
            let (a, d) = (id.clone(), id);
            div()
                .mx_4()
                .mb_2()
                .p_3()
                .rounded(radius::MD)
                .border_1()
                .border_color(t.border_strong)
                .flex()
                .items_center()
                .gap_3()
                .child(icon(Icon::Mail).text_color(t.text_faint))
                .child(div().flex_1().text_size(text::SM).text_color(t.text_muted).child("This is a message request. Accept it to reply; declining hides the conversation."))
                .child(Button::new("request-decline", "Decline").ghost().on_click(move |_, _, cx| {
                    let d = d.clone();
                    decline.update(cx, |c, cx| c.answer_request(d, false, cx));
                }))
                .child(Button::new("request-accept", "Accept").primary().on_click(move |_, _, cx| {
                    let a = a.clone();
                    accept.update(cx, |c, cx| c.answer_request(a, true, cx));
                }))
        });
        let dm_call = if is_dm { self.render_dm_call(&channel_id, cx) } else { None };
        let composer = self.render_composer(&channel_id, window, cx);
        let lightbox = self.render_lightbox(window, cx);
        let movie_big = self.render_movie_big(window, cx);
        let suggestions = self.render_suggestions(cx);
        let delete_confirm = self.render_delete_confirm(cx);
        let picker = self.render_picker();
        let gif_picker = self.render_gif_picker();
        let drop_channel = channel_id.clone();

        div()
            .id("chat")
            .track_focus(&self.select_focus)
            // A press anywhere ends the old selection first; a press on message text then
            // starts a new one.
            .capture_any_mouse_down(cx.listener(|this, e: &gpui::MouseDownEvent, window, cx| {
                if e.button == gpui::MouseButton::Left {
                    this.set_selection(None, cx);
                    // A click into the chat focuses it (an input clicked takes the focus back as
                    // the event reaches it), so typing afterwards goes to the message bar. An
                    // open overlay keeps the focus it has.
                    if this.lightbox.is_none() && !this.movie_big && this.picker.is_none() && this.gif_picker.is_none() {
                        window.focus(&this.select_focus, cx);
                    }
                }
            }))
            .capture_key_down(cx.listener(|this, e: &gpui::KeyDownEvent, _, cx| {
                let k = &e.keystroke;
                if k.key == "c" && (k.modifiers.control || k.modifiers.platform) && !k.modifiers.shift && this.copy_selection(cx) {
                    cx.stop_propagation();
                }
            }))
            // Typing while the chat itself has focus (after clicking into the history) writes
            // into the message bar, as in Discord: the key goes there and so does the focus.
            .on_key_down(cx.listener(|this, e: &gpui::KeyDownEvent, window, cx| {
                if !this.select_focus.is_focused(window) {
                    return;
                }
                let k = &e.keystroke;
                if k.modifiers.control || k.modifiers.alt || k.modifiers.platform || k.modifiers.function {
                    return;
                }
                // Enter sends what the message bar holds (text, or files waiting to go) without
                // going there first.
                if k.key == "enter" && !k.modifiers.shift {
                    let Some(channel) = this.writable(cx) else { return };
                    let has_text = !this.composer.read(cx).text().trim().is_empty();
                    if has_text || !this.client.read(cx).staged(&channel).is_empty() {
                        this.submit(window, cx);
                        cx.stop_propagation();
                    }
                    return;
                }
                let Some(text) = k.key_char.clone().filter(|c| !c.is_empty() && !c.chars().any(char::is_control)) else { return };
                if this.current().is_none_or(|channel| !this.client.read(cx).may_write(&channel)) {
                    return;
                }
                let focus = this.composer_focus(cx);
                window.focus(&focus, cx);
                this.composer.update(cx, |c, cx| c.insert(&text, cx));
                cx.stop_propagation();
            }))
            .relative()
            .flex_1()
            .min_w_0()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .bg(t.bg_main)
            .text_color(t.text)
            .drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(t.bg_panel))
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                this.stage_paths(drop_channel.clone(), paths.paths().to_vec(), cx);
            }))
            .child(header)
            .children(dm_call)
            .children(top_note.map(|note| div().px_4().pt_3().text_size(text::SM).text_color(t.text_faint).child(note)))
            .child(self.history_part(cx).cached(gpui::StyleRefinement::default().flex_1().min_h_0()))
            .children(request_bar)
            .child(composer)
            .children(suggestions)
            .children(lightbox)
            .children(movie_big)
            .children(delete_confirm)
            .children(picker)
            .children(gif_picker)
            .children(self.menu.element())
            .into_any_element()
    }
}

/// "anna is typing…" and friends, or `None` when nobody is.
pub fn typing_label(names: &[String]) -> Option<String> {
    match names {
        [] => None,
        [one] => Some(format!("{one} is typing…")),
        [a, b] => Some(format!("{a} and {b} are typing…")),
        _ => Some("Several people are typing…".into()),
    }
}

/// Which button a picker belongs to, to tell a second press on it.
fn picker_key(target: &PickerTarget) -> String {
    match target {
        PickerTarget::Composer => "composer".into(),
        PickerTarget::React { message_id, .. } => format!("react-{message_id}"),
    }
}

/// The three dots of the typing line. They pulse in steps of `STEP` rather than every display
/// frame, and only themselves: someone may type for a long while.
#[derive(Default)]
struct TypingDots {
    ticking: Option<gpui::Task<()>>,
    drawn: Option<std::time::Instant>,
}

impl Render for TypingDots {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        const STEP: Duration = Duration::from_millis(120);
        self.drawn = Some(std::time::Instant::now());
        if self.ticking.is_none() {
            self.ticking = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(STEP).await;
                    // Off screen once the typing line has gone: stop until drawn again.
                    let going = this
                        .update(cx, |this, cx| {
                            let shown = this.drawn.is_some_and(|at| at.elapsed() < STEP * 3);
                            if shown {
                                cx.notify();
                            } else {
                                this.ticking = None;
                            }
                            shown
                        })
                        .unwrap_or(false);
                    if !going {
                        return;
                    }
                }
            }));
        }
        let t = cx.theme();
        let delta = (typing_epoch().elapsed().as_secs_f32() / 1.2).fract();
        let dot = |i: usize| {
            let phase = (delta * std::f32::consts::TAU - i as f32 * 0.9).sin() * 0.5 + 0.5;
            div().size(px(5.)).rounded_full().bg(t.text_muted).opacity(0.3 + 0.7 * phase)
        };
        div().flex().items_center().gap(px(3.)).child(dot(0)).child(dot(1)).child(dot(2))
    }
}

/// Where the typing dots' pulse is measured from.
fn typing_epoch() -> std::time::Instant {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(std::time::Instant::now)
}
