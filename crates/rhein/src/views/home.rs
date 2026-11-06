//! The home view's main area: the friends page, with adding friends and starting group chats.
//! An open private conversation is shown by the chat view instead.

use gpui::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, FontWeight, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Window, div, prelude::*, px,
};
use rhein_proto::Friend;
use rhein_store::{Client, FriendAction};
use rhein_ui::theme::{radius, size, text};
use rhein_ui::{ActiveTheme as _, Avatar, Button, Icon, IconButton, InputEvent, Presence, Tabs, TextInput, Theme, icon};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Online,
    All,
    Pending,
    Blocked,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Online, Tab::All, Tab::Pending, Tab::Blocked];

    fn label(self) -> &'static str {
        match self {
            Tab::Online => "Online",
            Tab::All => "All",
            Tab::Pending => "Pending",
            Tab::Blocked => "Blocked",
        }
    }

    fn shows(self, f: &Friend) -> bool {
        match self {
            Tab::Online => f.state == "accepted" && f.status() != "offline",
            Tab::All => f.state == "accepted",
            Tab::Pending => f.state == "pending_in" || f.state == "pending_out",
            Tab::Blocked => f.state == "blocked",
        }
    }
}

pub struct HomeView {
    client: Entity<Client>,
    list_scroll: gpui::ScrollHandle,
    tab: Tab,
    add: Entity<TextInput>,
    group: Entity<TextInput>,
    _subs: Vec<Subscription>,
}

impl HomeView {
    pub fn new(client: Entity<Client>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let add = cx.new(|cx| TextInput::new("@someone:example.org", cx));
        let group = cx.new(|cx| TextInput::new("@one:example.org, @two:example.org", cx));
        let subs = vec![
            cx.observe(&client, |_, _, cx| cx.notify()),
            cx.subscribe_in(&add, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Submit) {
                    this.send_request(cx);
                }
            }),
            cx.subscribe_in(&group, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Submit) {
                    this.start_group(cx);
                }
            }),
        ];
        Self { client, list_scroll: gpui::ScrollHandle::new(), tab: Tab::Online, add, group, _subs: subs }
    }

    /// An encrypted group chat with the addresses typed in, separated by commas or spaces.
    fn start_group(&mut self, cx: &mut Context<Self>) {
        let members: Vec<String> = self
            .group
            .read(cx)
            .text()
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(|m| if m.starts_with('@') { m.to_owned() } else { format!("@{m}") })
            .collect();
        if members.is_empty() || members.iter().any(|m| !m.contains(':')) {
            self.client.update(cx, |c, cx| {
                c.error = Some("Use full addresses, like @name:server, separated by commas.".into());
                cx.notify();
            });
            return;
        }
        self.group.update(cx, |i, cx| i.clear(cx));
        self.client.update(cx, |c, cx| c.new_group(members, cx));
    }

    fn send_request(&mut self, cx: &mut Context<Self>) {
        let mut id = self.add.read(cx).text().trim().to_owned();
        if id.is_empty() {
            return;
        }
        if !id.starts_with('@') {
            id.insert(0, '@');
        }
        if !id.contains(':') {
            self.client.update(cx, |c, cx| {
                c.error = Some("Use the full address, like @name:server.".into());
                cx.notify();
            });
            return;
        }
        self.add.update(cx, |i, cx| i.clear(cx));
        self.client.update(cx, |c, cx| c.friend_action(FriendAction::Request, id, cx));
    }

    /// Puts a friend's full address on the clipboard and says so.
    fn copy_address(&self, user_id: &str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(user_id.to_owned()));
        let text = format!("Copied {user_id}");
        self.client.update(cx, |c, cx| c.flash_notice(text, cx));
    }

    fn friend_row(&self, i: usize, f: &Friend, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let client = self.client.read(cx);
        let name = f.display_name.clone().unwrap_or_else(|| client.friend_or_local_name(&f.user_id));
        let avatar = client.friend_avatar(f);
        let presence = Presence::parse(f.status());
        let detail = match f.state.as_str() {
            "pending_in" => "Incoming friend request".to_owned(),
            "pending_out" => "Outgoing friend request".to_owned(),
            "blocked" => "Blocked".to_owned(),
            _ => f.activities().first().map(|a| a.summary()).unwrap_or_else(|| {
                match f.status() {
                    "online" => "Online",
                    "idle" => "Idle",
                    "dnd" => "Do not disturb",
                    _ => "Offline",
                }
                .to_owned()
            }),
        };
        // The address beside the name: a click copies it.
        let address = {
            let id = f.user_id.clone();
            div()
                .id(SharedString::from(format!("address-{i}")))
                .flex_shrink(1.)
                .min_w_0()
                .truncate()
                .text_size(text::SM)
                .text_color(t.text_faint)
                .cursor_pointer()
                .hover(|s| s.text_color(t.text_muted))
                .tooltip(rhein_ui::tooltip("Copy"))
                .on_click(cx.listener(move |this, _, _, cx| this.copy_address(&id, cx)))
                .child(f.user_id.clone())
        };
        let action = |label: &'static str, id: String, what: FriendAction, primary: bool, cx: &mut Context<Self>| {
            let b = Button::new(SharedString::from(format!("{label}-{i}")), label).small();
            let b = if primary { b.primary() } else { b.ghost() };
            b.on_click(cx.listener(move |this, _, _, cx| {
                let id = id.clone();
                this.client.update(cx, |c, cx| c.friend_action(what, id, cx));
            }))
        };
        let id = f.user_id.clone();
        // The full address, in every state: to pass on, or to add them elsewhere.
        let copy = {
            let id = id.clone();
            IconButton::new(SharedString::from(format!("copy-{i}")), Icon::Copy)
                .tooltip(format!("Copy {id}"))
                .on_click(cx.listener(move |this, _, _, cx| this.copy_address(&id, cx)))
        };
        let mut actions = div().flex().flex_none().items_center().gap_2().child(copy);
        actions = match f.state.as_str() {
            "pending_in" => actions
                .child(action("Accept", id.clone(), FriendAction::Accept, true, cx))
                .child(action("Decline", id, FriendAction::Remove, false, cx)),
            "pending_out" => actions.child(action("Cancel", id, FriendAction::Remove, false, cx)),
            "blocked" => actions.child(action("Unblock", id, FriendAction::Unblock, false, cx)),
            _ => actions
                .child(action("Remove", id.clone(), FriendAction::Remove, false, cx))
                .child(action("Block", id, FriendAction::Block, false, cx)),
        };
        let right_click = {
            let id = f.user_id.clone();
            cx.listener(move |this, _: &gpui::MouseDownEvent, _, cx| this.copy_address(&id, cx))
        };
        div()
            .px_3()
            .py_2()
            .rounded(radius::MD)
            .flex()
            .items_center()
            .gap_3()
            .hover(|s| s.bg(t.bg_hover))
            .on_mouse_down(gpui::MouseButton::Right, right_click)
            .child(Avatar::new(f.user_id.clone(), name.clone()).size(px(36.)).image(avatar.map(Into::into)).presence(Some(presence)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_2()
                            .min_w_0()
                            .child(div().flex_none().font_weight(FontWeight::MEDIUM).child(name))
                            .child(address),
                    )
                    .child(div().text_size(text::SM).text_color(t.text_faint).truncate().child(detail)),
            )
            .child(actions)
            .into_any_element()
    }

    fn friends_page(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let friends: Vec<Friend> = self.client.read(cx).friends.iter().filter(|f| self.tab.shows(f)).cloned().collect();
        let pending = self.client.read(cx).incoming_friend_requests();
        // Requests on screen count as looked at: the mark on Direct messages goes.
        if self.tab == Tab::Pending && self.client.read(cx).unseen_friend_requests() > 0 {
            let client = self.client.clone();
            cx.defer(move |cx| client.update(cx, |c, cx| c.mark_requests_seen(cx)));
        }
        let this = cx.weak_entity();
        let labels: Vec<String> = Tab::ALL
            .iter()
            .map(|tab| match tab {
                Tab::Pending if pending > 0 => format!("Pending ({pending})"),
                _ => tab.label().to_owned(),
            })
            .collect();
        let selected = Tab::ALL.iter().position(|t| *t == self.tab).unwrap_or(0);
        let tabs = Tabs::new("friend-tabs", labels).selected(selected).on_select(move |i, _, cx| {
            this.update(cx, |v, cx| {
                v.tab = Tab::ALL[i];
                cx.notify();
            })
            .ok();
        });
        let empty = match self.tab {
            Tab::Online => "No friends online right now.",
            Tab::All => "No friends yet. Add someone by their address below.",
            Tab::Pending => "No pending requests.",
            Tab::Blocked => "You haven't blocked anyone.",
        };
        let mut list = div().id("friends-list").flex_1().min_h_0().overflow_y_scroll().track_scroll(&self.list_scroll).px_4().flex().flex_col().gap_0p5();
        if friends.is_empty() {
            list = list.child(div().p_6().text_center().text_color(t.text_faint).child(empty));
        }
        for (i, f) in friends.iter().enumerate() {
            list = list.child(self.friend_row(i, f, t, cx));
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(size::HEADER)
                    .flex_none()
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_3()
                    .border_b_1()
                    .border_color(t.border)
                    .child(icon(Icon::Users).text_color(t.text_faint))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Friends"))
                    .child(tabs),
            )
            .child(
                div()
                    .p_4()
                    // A form of a readable width, not the whole window's.
                    .max_w(px(720.))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_size(text::SM).font_weight(FontWeight::SEMIBOLD).child("Add a friend"))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(div().flex_1().child(self.add.clone()))
                            .child(div().w(px(FORM_BUTTON_W)).flex_none().flex().child(Button::new("friend-send", "Send request").primary().full_width().on_click(cx.listener(|v, _, _, cx| v.send_request(cx))))),
                    )
                    .child(div().pt_2().text_size(text::SM).font_weight(FontWeight::SEMIBOLD).child("New group chat"))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(div().flex_1().child(self.group.clone()))
                            .child(div().w(px(FORM_BUTTON_W)).flex_none().flex().child(Button::new("group-start", "Start group").icon(Icon::Lock).full_width().on_click(cx.listener(|v, _, _, cx| v.start_group(cx))))),
                    ),
            )
            .child(rhein_ui::smooth_scroll("friends-glide", rhein_ui::ScrollTarget::Handle(self.list_scroll.clone()), list))
            .into_any_element()
    }

}

/// Both form rows end in a button of the same width, so their fields line up.
const FORM_BUTTON_W: f32 = 132.;

impl Render for HomeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        // An open conversation is drawn by the chat view; this is the friends page.
        let page = self.friends_page(&t, cx);
        div().flex_1().min_w_0().h_full().bg(t.bg_main).text_color(t.text).child(page)
    }
}
