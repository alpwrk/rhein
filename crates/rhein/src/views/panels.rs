//! Side panels next to the chat: pinned messages, threads, search. (Members and an open thread are
//! handled by the shell directly.)

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, Focusable as _, FontWeight, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, WeakEntity, Window, div, prelude::*, px,
};
use rhein_proto::{Message, SearchQuery};
use rhein_store::{Client, markdown, perms};
use rhein_ui::theme::{radius, size, text};
use rhein_ui::{ActiveTheme as _, Avatar, Button, Icon, IconButton, InputEvent, Tabs, TextInput, Theme, icon};

use crate::views::chat::ChatView;
use crate::views::message_text;

pub enum PanelEvent {
    Close,
    OpenThread(String),
}

fn panel_header(title: &str, t: &Theme, on_close: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static) -> gpui::Div {
    div()
        .h(size::HEADER)
        .flex_none()
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(t.border)
        .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child(title.to_owned()))
        .child(IconButton::new("panel-close", Icon::X).large().tooltip("Close").on_click(on_close))
}

/// A compact message: avatar, author, time, rendered text; click jumps to it.
fn compact_message(
    client: &Entity<Client>,
    view: &WeakEntity<ChatView>,
    m: &Message,
    id: SharedString,
    trailing: Option<AnyElement>,
    cx: &App,
) -> AnyElement {
    let t = cx.theme().clone();
    let c = client.read(cx);
    let guild_id = c.guild_of_channel(&m.channel_id).map(|g| g.guild.id.clone());
    let name = c.display_name(guild_id.as_deref(), &m.author);
    let avatar = c.avatar_url(guild_id.as_deref(), &m.author);
    let channel_name = c.channel(&m.channel_id).map(|ch| ch.name.clone()).unwrap_or_default();
    let revealed = Default::default();
    let doc = markdown::parse(&m.content);
    let text_ctx = message_text::Context {
        client,
        view,
        message_id: &m.id,
        guild_id: guild_id.as_deref(),
        me: c.user_id(),
        revealed: &revealed,
        pending: false,
        selected: None,
        hovered: None,
        edited: false,
    };
    let body = message_text::render(&doc, &text_ctx, cx);
    let (jump_client, chan, mid) = (client.clone(), m.channel_id.clone(), m.id.clone());
    div()
        .id(id)
        .p_2()
        .rounded(radius::MD)
        .bg(t.bg_main)
        .flex()
        .gap_2()
        .cursor_pointer()
        .hover(|s| s.bg(t.bg_hover))
        .on_click(move |_, _, cx| {
            let (chan, mid) = (chan.clone(), mid.clone());
            jump_client.update(cx, |c, cx| c.open_message(chan, mid, cx));
        })
        .child(Avatar::new(m.author.clone(), name.clone()).size(px(28.)).image(avatar.map(Into::into)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap_2()
                        .child(div().text_size(text::SM).font_weight(FontWeight::SEMIBOLD).child(name))
                        .child(div().text_size(text::XS).text_color(t.text_faint).child(format!("#{channel_name}"))),
                )
                .child(div().text_size(text::SM).max_h(px(120.)).overflow_hidden().child(body)),
        )
        .children(trailing)
        .into_any_element()
}

// ------------------------------------------------------------------------------------ pins

pub struct PinsPanel {
    client: Entity<Client>,
    view: WeakEntity<ChatView>,
    channel_id: String,
    _sub: Subscription,
}

impl EventEmitter<PanelEvent> for PinsPanel {}

impl PinsPanel {
    pub fn new(client: Entity<Client>, view: WeakEntity<ChatView>, channel_id: String, cx: &mut Context<Self>) -> Self {
        let chan = channel_id.clone();
        client.update(cx, |c, cx| c.load_pins(chan, cx));
        let sub = cx.observe(&client, |_, _, cx| cx.notify());
        Self { client, view, channel_id, _sub: sub }
    }
}

impl Render for PinsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let client = self.client.read(cx);
        let moderate = client.can(&self.channel_id, perms::MANAGE_MESSAGES);
        let pins: Option<Vec<Message>> = client.pins(&self.channel_id).map(<[Message]>::to_vec);
        let mut list = div().id("pins-list").flex_1().min_h_0().overflow_y_scroll().p_2().flex().flex_col().gap_2();
        match pins {
            None => list = list.child(div().p_2().text_color(t.text_faint).child("Loading…")),
            Some(pins) if pins.is_empty() => {
                list = list.child(
                    div()
                        .p_4()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_2()
                        .text_center()
                        .text_size(text::SM)
                        .text_color(t.text_faint)
                        .child(icon(Icon::Pin).text_color(t.text_faint))
                        .child("No pinned messages in this channel yet."),
                )
            }
            Some(pins) => {
                for (i, m) in pins.iter().enumerate() {
                    let trailing = moderate.then(|| {
                        let (client, chan, mid) = (self.client.clone(), self.channel_id.clone(), m.id.clone());
                        IconButton::new(("unpin", i), Icon::X)
                            .tooltip("Unpin")
                            .on_click(move |_, _, cx| {
                                let (chan, mid) = (chan.clone(), mid.clone());
                                client.update(cx, |c, cx| {
                                    c.set_pinned(chan.clone(), mid, false, cx);
                                    c.load_pins(chan, cx);
                                });
                            })
                            .into_any_element()
                    });
                    list = list.child(compact_message(&self.client, &self.view, m, SharedString::from(format!("pin-{i}")), trailing, cx));
                }
            }
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(panel_header("Pinned messages", &t, cx.listener(|_, _, _, cx| cx.emit(PanelEvent::Close))))
            .child(list)
    }
}

// ------------------------------------------------------------------------------------ threads

pub struct ThreadsPanel {
    client: Entity<Client>,
    channel_id: String,
    title: Entity<TextInput>,
    archived: bool,
    _subs: Vec<Subscription>,
}

impl EventEmitter<PanelEvent> for ThreadsPanel {}

impl ThreadsPanel {
    pub fn new(client: Entity<Client>, channel_id: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let chan = channel_id.clone();
        client.update(cx, |c, cx| c.load_threads(chan, cx));
        let title = cx.new(|cx| TextInput::new("New thread title", cx));
        let subs = vec![
            cx.observe(&client, |_, _, cx| cx.notify()),
            cx.subscribe_in(&title, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Submit) {
                    this.create(cx);
                }
            }),
        ];
        Self { client, channel_id, title, archived: false, _subs: subs }
    }

    fn create(&mut self, cx: &mut Context<Self>) {
        let title = self.title.read(cx).text().trim().to_owned();
        if title.is_empty() {
            return;
        }
        self.title.update(cx, |t, cx| t.clear(cx));
        let chan = self.channel_id.clone();
        self.client.update(cx, |c, cx| c.create_thread(chan, title, None, cx));
    }
}

impl Render for ThreadsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let client = self.client.read(cx);
        let threads: Vec<rhein_proto::Thread> = client
            .threads_of(&self.channel_id)
            .map(|list| list.iter().filter(|th| th.archived == self.archived).cloned().collect())
            .unwrap_or_default();
        let can_create = client.can(&self.channel_id, perms::SEND_MESSAGES);
        let this = cx.weak_entity();
        let tabs = Tabs::new("thread-tabs", ["Active", "Archived"]).selected(usize::from(self.archived)).on_select(
            move |i, _, cx| {
                this.update(cx, |p, cx| {
                    p.archived = i == 1;
                    cx.notify();
                })
                .ok();
            },
        );
        let mut list = div().id("threads-list").flex_1().min_h_0().overflow_y_scroll().p_2().flex().flex_col().gap_1();
        if threads.is_empty() {
            list = list.child(div().p_4().text_center().text_size(text::SM).text_color(t.text_faint).child(if self.archived {
                "No archived threads."
            } else {
                "No threads yet. Start one from a message's menu or below."
            }));
        }
        for (i, th) in threads.iter().enumerate() {
            let id = th.id.clone();
            let owner = th.owner.as_deref().map(|o| client.display_name(None, o)).unwrap_or_default();
            list = list.child(
                div()
                    .id(("thread", i))
                    .p_2()
                    .rounded(radius::MD)
                    .bg(t.bg_main)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.bg_hover))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(PanelEvent::OpenThread(id.clone()))))
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(icon(Icon::MessagesSquare).text_color(t.text_faint))
                            .child(div().flex_1().min_w_0().truncate().font_weight(FontWeight::MEDIUM).child(th.title.clone()))
                            .when(th.locked, |d| d.child(icon(Icon::Lock).text_color(t.text_faint))),
                    )
                    .child(
                        div()
                            .text_size(text::XS)
                            .text_color(t.text_faint)
                            .child(format!("{} message{} · started by {owner}", th.message_count, if th.message_count == 1 { "" } else { "s" })),
                    ),
            );
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(panel_header("Threads", &t, cx.listener(|_, _, _, cx| cx.emit(PanelEvent::Close))))
            .child(div().p_2().child(tabs))
            .child(list)
            .when(can_create && !self.archived, |d| {
                d.child(
                    div()
                        .p_2()
                        .flex()
                        .gap_2()
                        .border_t_1()
                        .border_color(t.border)
                        .child(div().flex_1().child(self.title.clone()))
                        .child(Button::new("thread-create", "Start thread").primary().on_click(cx.listener(|this, _, _, cx| this.create(cx)))),
                )
            })
    }
}

// ------------------------------------------------------------------------------------ search

const HAS: [(&str, &str); 4] = [("", "Anything"), ("file", "Files"), ("image", "Images"), ("link", "Links")];

pub struct SearchPanel {
    client: Entity<Client>,
    view: WeakEntity<ChatView>,
    guild_id: String,
    channel_id: String,
    input: Entity<TextInput>,
    only_channel: bool,
    has: usize,
    _subs: Vec<Subscription>,
}

impl EventEmitter<PanelEvent> for SearchPanel {}

impl SearchPanel {
    pub fn new(
        client: Entity<Client>,
        view: WeakEntity<ChatView>,
        guild_id: String,
        channel_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| TextInput::new("Search messages", cx));
        let subs = vec![
            cx.observe(&client, |_, _, cx| cx.notify()),
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Submit) {
                    this.run(cx);
                }
            }),
        ];
        window.focus(&input.focus_handle(cx), cx);
        Self { client, view, guild_id, channel_id, input, only_channel: false, has: 0, _subs: subs }
    }

    fn run(&mut self, cx: &mut Context<Self>) {
        let raw = self.input.read(cx).text().trim().to_owned();
        // `from:@user:server` narrows to an author, like the web client's filter chips.
        let mut author = None;
        let mut words = Vec::new();
        for word in raw.split_whitespace() {
            match word.strip_prefix("from:") {
                Some(who) if !who.is_empty() => author = Some(who.to_owned()),
                _ => words.push(word),
            }
        }
        let q = words.join(" ");
        let has = HAS[self.has].0;
        if q.is_empty() && author.is_none() && has.is_empty() {
            self.client.update(cx, |c, cx| c.clear_search(cx));
            return;
        }
        let query = SearchQuery {
            q: (!q.is_empty()).then_some(q),
            channel_id: self.only_channel.then(|| self.channel_id.clone()),
            author,
            has: (!has.is_empty()).then(|| has.to_owned()),
            offset: 0,
            limit: 25,
        };
        let gid = self.guild_id.clone();
        self.client.update(cx, |c, cx| c.run_search(gid, query, cx));
    }
}

impl Render for SearchPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let state = self.client.read(cx).search.clone().filter(|s| s.guild_id == self.guild_id);
        let filter = |id: &'static str, label: String, active: bool, cx: &mut Context<Self>, f: fn(&mut Self)| {
            div()
                .id(id)
                .h(rhein_ui::theme::size::CONTROL_SM)
                .px_2()
                .rounded(radius::MD)
                .border_1()
                .border_color(if active { t.accent } else { t.border_strong })
                .text_size(text::SM)
                .text_color(if active { t.accent } else { t.text_muted })
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    f(this);
                    this.run(cx);
                }))
                .child(label)
        };
        let filters = div()
            .flex()
            .flex_wrap()
            .gap_1()
            .child(filter("search-in", "This channel only".into(), self.only_channel, cx, |p| p.only_channel = !p.only_channel))
            .child(filter("search-has", format!("Has: {}", HAS[self.has].1), self.has != 0, cx, |p| p.has = (p.has + 1) % HAS.len()));

        let mut results = div().id("search-results").flex_1().min_h_0().overflow_y_scroll().p_2().flex().flex_col().gap_2();
        match &state {
            None => {
                results = results.child(
                    div()
                        .p_4()
                        .text_size(text::SM)
                        .text_color(t.text_faint)
                        .child("Type and press Enter. Use from:@user:server to filter by author."),
                )
            }
            Some(s) => {
                results = results.child(div().px_1().text_size(text::XS).text_color(t.text_faint).child(if s.loading && s.results.is_empty() {
                    "Searching…".to_owned()
                } else if let Some(err) = &s.error {
                    format!("Search failed: {err}. Check your connection and try again.")
                } else {
                    format!("{} result{}", s.total, if s.total == 1 { "" } else { "s" })
                }));
                for (i, m) in s.results.iter().enumerate() {
                    results = results.child(compact_message(&self.client, &self.view, m, SharedString::from(format!("hit-{i}")), None, cx));
                }
                if s.has_more {
                    results = results.child(
                        Button::new("search-more", if s.loading { "Loading…" } else { "Load more" })
                            .ghost()
                            .full_width()
                            .on_click(cx.listener(|this, _, _, cx| this.client.update(cx, |c, cx| c.search_more(cx)))),
                    );
                }
            }
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(panel_header("Search", &t, cx.listener(|_, _, _, cx| cx.emit(PanelEvent::Close))))
            .child(div().p_2().flex().flex_col().gap_2().child(self.input.clone()).child(filters))
            .child(results)
    }
}
