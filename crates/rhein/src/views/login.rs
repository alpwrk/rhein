use gpui::{
    AppContext as _, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement, ParentElement as _, Render,
    Styled as _, Subscription, Window, div, prelude::*, px,
};
use rhein_store::{Client, Phase};
use rhein_ui::theme::{radius, text};
use rhein_ui::{ActiveTheme as _, Button, Icon, InputEvent, TextInput, Toast, ToastKind, icon};

pub struct LoginView {
    client: Entity<Client>,
    server: Entity<TextInput>,
    user: Entity<TextInput>,
    password: Entity<TextInput>,
    /// The two-factor code, asked for when the server wants one.
    code: Entity<TextInput>,
    /// What was signed in with, kept while the code is asked for.
    pending: Option<(String, String, String)>,
    code_focused: bool,
    register: bool,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl LoginView {
    pub fn new(client: Entity<Client>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Development builds point at the local dev server; a release starts empty.
        let default_server = if cfg!(debug_assertions) {
            std::env::var("RHEIN_SERVER").unwrap_or_else(|_| "localhost:8448".into())
        } else {
            String::new()
        };
        let server = cx.new(|cx| TextInput::new("chat.example.com", cx).with_text(default_server));
        let user = cx.new(|cx| TextInput::new("Username", cx));
        let password = cx.new(|cx| TextInput::new("Password", cx).masked());
        let code = cx.new(|cx| TextInput::new("123456", cx));
        let subs = [&server, &user, &password, &code]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(input, window, |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Submit) {
                        this.submit(window, cx);
                    }
                })
            })
            .collect();
        let focus = cx.focus_handle();
        // The first empty field: the server in a release, the username with the dev default.
        let first = if server.read(cx).text().is_empty() { &server } else { &user };
        window.focus(&first.focus_handle(cx), cx);
        Self { client, server, user, password, code, pending: None, code_focused: false, register: false, focus, _subs: subs }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.client.read(cx).mfa_required
            && let Some((server, user, password)) = self.pending.clone()
        {
            let code = self.code.read(cx).text().trim().to_owned();
            if code.is_empty() {
                window.focus(&self.code.focus_handle(cx), cx);
                return;
            }
            self.code.update(cx, |c, cx| c.clear(cx));
            self.client.update(cx, |client, cx| client.sign_in(server, user, password, false, Some(code), cx));
            return;
        }
        let server = self.server.read(cx).text().to_owned();
        let user = self.user.read(cx).text().to_owned();
        let password = self.password.read(cx).text().to_owned();
        let empty = [
            (server.trim().is_empty(), &self.server),
            (user.trim().is_empty(), &self.user),
            (password.is_empty(), &self.password),
        ];
        if let Some((_, input)) = empty.iter().find(|(missing, _)| *missing) {
            window.focus(&input.focus_handle(cx), cx);
            return;
        }
        let register = self.register;
        self.password.update(cx, |p, cx| p.clear(cx));
        self.pending = Some((server.clone(), user.clone(), password.clone()));
        self.client.update(cx, |client, cx| client.sign_in(server, user, password, register, None, cx));
    }
}

impl Focusable for LoginView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for LoginView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let client = self.client.read(cx);
        let busy = client.phase == Phase::SigningIn;
        let error = client.error.clone();
        let register = self.register;
        let asking_code = client.mfa_required && self.pending.is_some();
        // The code field takes the keyboard when it appears.
        if asking_code && !self.code.focus_handle(cx).contains_focused(window, cx) && !self.code_focused {
            self.code_focused = true;
            window.focus(&self.code.focus_handle(cx), cx);
        }
        if !asking_code {
            self.code_focused = false;
        }

        let field = |label: &'static str, input: &Entity<TextInput>| {
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(div().text_size(text::SM).text_color(t.text_muted).child(label))
                .child(input.clone())
        };

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(t.bg_base)
            .child(
                div()
                    .w(px(380.))
                    .flex()
                    .flex_col()
                    .gap_5()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .size(px(40.))
                                    .mb_3()
                                    .rounded(radius::LG)
                                    .bg(t.accent)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(icon(Icon::MessagesSquare).text_color(t.text_on_accent)),
                            )
                            .child(
                                div().text_size(text::XXL).font_weight(FontWeight::SEMIBOLD).child(if register {
                                    "Create account"
                                } else {
                                    "Welcome back"
                                }),
                            )
                            .child(div().text_color(t.text_muted).child(if register {
                                "Register on a Telium server of your choice."
                            } else {
                                "Sign in with your Telium account."
                            })),
                    )
                    .children(error.map(|e| Toast::new(e, ToastKind::Error)))
                    .when(!asking_code, |d| {
                        d.child(field("Home server", &self.server)).child(field("Username", &self.user)).child(field("Password", &self.password))
                    })
                    .when(asking_code, |d| d.child(field("Two-factor code", &self.code)))
                    .child(
                        Button::new(
                            "login-submit",
                            match (busy, register) {
                                (true, true) => "Creating account…",
                                (true, false) => "Signing in…",
                                (false, true) => "Create account",
                                (false, false) => "Sign in",
                            },
                        )
                        .primary()
                        .full_width()
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    )
                    // Adding an account: the one we came from is a click away.
                    .children(self.client.read(cx).returning_to.clone().map(|(id, name)| {
                        let client = self.client.clone();
                        Button::new("login-back", format!("Back to {name}"))
                            .ghost()
                            .full_width()
                            .on_click(move |_, _, cx| client.update(cx, |c, cx| c.switch_account(&id, cx)))
                    }))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .text_size(text::SM)
                            .child(
                                div()
                                    .id("login-toggle")
                                    .text_color(t.accent)
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(t.accent_hover))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.register = !this.register;
                                        cx.notify();
                                    }))
                                    .child(if register { "I already have an account" } else { "Create an account" }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .text_color(t.text_faint)
                                    .child(icon(Icon::Lock).text_color(t.text_faint))
                                    .child("Password stays on this device"),
                            ),
                    ),
            )
    }
}
