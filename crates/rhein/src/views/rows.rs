//! One row of the message list: optional day separator and "New messages" line, the reply
//! quote, header, rich text, attachments, link previews, reactions and hover actions.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui::{
    AnyElement, App, Entity, FontWeight, Hsla, IntoElement, ObjectFit, ParentElement as _, Pixels, SharedString,
    Styled as _, StyledImage as _, WeakEntity, Window, div, img, prelude::*, px,
};
use rhein_proto::{Attachment, Message};
use rhein_store::embed::{Embed, EmbedKind};
use rhein_store::client::GifShare;
use rhein_store::gif_links::MediaKind;
use rhein_store::MediaProbe;
use rhein_store::{Client, DownloadView, EmbedState, MediaSource, Poster, gif_links, markdown};
use rhein_ui::images::{self, Fitted, fitted};
use rhein_ui::theme::{FONT_EMOJI, FONT_MONO, radius, size, text};
use rhein_ui::{ActiveTheme as _, motion, Avatar, Button, Icon, IconButton, Theme, icon, icon_sized, tooltip};

use crate::views::chat::ChatView;
use crate::views::media::{self, Media};
use crate::views::message_text;

/// Room above and below every row's content, inside its background.
const ROW_PAD: Pixels = px(3.);
/// Space between groups of messages, outside the rows.
const GROUP_GAP: Pixels = px(18.);
const GROUP_GAP_COMPACT: Pixels = px(8.);
/// How much closer a message's text sits under its header than the body's gap (`gap_1`).
const HEADER_TEXT_PULL: Pixels = px(2.);
/// The line height of a group's header, shared by the name and the time.
const HEADER_LINE: Pixels = px(20.);
/// Messages from the same author this close together share one header.
const GROUP_WINDOW_MS: i64 = 7 * 60 * 1000;
/// How many emoji the hover bar offers to react with in one click: the most recently used,
/// topped up from `DEFAULT_REACTIONS` while there are fewer.
const QUICK_REACTIONS: usize = 4;
const DEFAULT_REACTIONS: [&str; QUICK_REACTIONS] = ["👍", "❤️", "😂", "🎉"];
/// Room kept free on the right of every message for the hover action bar, so the bar never
/// covers text: the quick reactions, add reaction, reply and "more", at 28 px each, plus padding.
const ACTION_BAR_W: f32 = (QUICK_REACTIONS + 3) as f32 * 28. + 14.;
/// Link previews and embed cards.
const IMAGE_MAX_W: f32 = 400.;
/// Every picture in the chat (an attachment, a GIF, a link to a picture, a preview's image) is
/// shown whole and at most this high: a larger one is scaled down to it, keeping its shape, a
/// smaller one keeps its own size.
const PICTURE_MAX_H: f32 = 300.;
/// A picture of unknown size, until it loads.
const PICTURE_UNKNOWN: (f32, f32) = (400., 300.);

pub struct RowCtx {
    pub client: Entity<Client>,
    pub view: WeakEntity<ChatView>,
    pub channel_id: String,
    /// The pixel size of each picture that has arrived (see `ChatView::prefetch`).
    pub picture_sizes: Rc<RefCell<HashMap<String, (u32, u32)>>>,
    pub scale_factor: f32,
    /// Each message's parsed text, kept between frames (see `parsed`).
    pub parsed: ParsedCache,
    /// The player of the video playing, shown in its row.
    pub movie: Entity<crate::views::live::LiveMovie>,
    /// The player is shown large over the window, not in its row.
    pub movie_big: bool,
    /// Where the "New messages" line goes (`Client::divider_index`), found once per frame.
    pub divider: Option<usize>,
}

/// What a row shows of the chat view's own state (not the client's): worked out by the chat
/// view for each row it lays out and handed to the row's view, so a row neither reads the chat
/// view (which would redraw every row whenever any of this changes for one) nor misses a change.
#[derive(Clone, Default, PartialEq)]
pub struct RowUi {
    /// Text selected in a block of this message, and the link under the pointer.
    pub selected: Option<(usize, std::ops::Range<usize>)>,
    pub hovered_link: Option<(usize, std::ops::Range<usize>)>,
    /// This message's spoilers the user opened.
    pub revealed: HashSet<(String, usize)>,
    pub highlighted: bool,
    /// `ChatView::fresh` for this message.
    pub fresh: Option<Option<f32>>,
    /// The pointer is over the row, and over which of its reactions.
    pub hovered: bool,
    pub hovered_reaction: Option<usize>,
    pub divider_here: bool,
    /// What the row depends on around it: the message before it (grouping, the day line) and
    /// whether it is the first.
    pub prev: Option<String>,
    pub first: bool,
    /// `ChatView::row_progress`: changes when a preview, a GIF page or a picture's size arrives.
    pub progress: Option<u64>,
    pub scale_factor: f32,
    pub movie_big: bool,
    /// The list is too narrow to keep `ACTION_BAR_W` free beside every message (a thread panel,
    /// a small window): the text gets the room and the hover bar lies over the row's edge.
    pub narrow: bool,
}

/// One message row of the history as a view of its own, drawn cached at the size it last had
/// (`keep_size`, Rhein's GPUI patch): it draws again only when something it shows changes (the
/// client, its `RowUi`, a picture it waits on), not on every frame of a scroll or a hover
/// elsewhere.
pub struct RowView {
    pub key: String,
    pub ctx: Rc<RowCtx>,
    pub ui: RowUi,
}

/// How many times a row was drawn afresh, for timing what a channel switch costs.
pub static ROWS_DRAWN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl gpui::Render for RowView {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        ROWS_DRAWN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ix = self.ctx.client.read(cx).logs.get(&self.ctx.channel_id).and_then(|log| log.messages.iter().position(|m| message_key(m) == self.key));
        match ix {
            Some(ix) => render_row(&self.ctx, ix, &self.ui, window, cx),
            None => div().into_any_element(),
        }
    }
}

/// What a message's text parses to, which depends on nothing but the text: worked out once,
/// not on every frame. Keyed by message id and checked against a hash of the text, so an edit
/// parses again.
pub struct Parsed {
    hash: u64,
    pub doc: markdown::Document,
    pub links: Vec<String>,
    /// `gif_links::gifs_in` of the text.
    pub gifs: Vec<String>,
    /// `gif_links::telium_media_in` of the text: Telium media links, GIF or picture once asked.
    pub telium_media: Vec<String>,
    /// The text is one GIF link and nothing else (`gif_links::is_gif_only`).
    pub gif_only: bool,
    /// The text is one Telium media link and nothing else.
    pub media_only: bool,
}

pub type ParsedCache = Rc<RefCell<HashMap<String, Rc<Parsed>>>>;

/// Past this many messages the cache starts over rather than growing with every channel read.
const PARSED_KEPT: usize = 4000;

pub fn parsed(cache: &ParsedCache, message: &Message) -> Rc<Parsed> {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    message.content.hash(&mut hasher);
    let hash = hasher.finish();
    if let Some(p) = cache.borrow().get(&message.id).filter(|p| p.hash == hash) {
        return p.clone();
    }
    let p = Rc::new(Parsed {
        hash,
        doc: markdown::parse(&message.content),
        links: markdown::links_in(&message.content),
        gifs: gif_links::gifs_in(&message.content),
        telium_media: gif_links::telium_media_in(&message.content),
        gif_only: gif_links::is_gif_only(&message.content),
        media_only: gif_links::is_telium_media(message.content.trim()),
    });
    let mut cache = cache.borrow_mut();
    if cache.len() >= PARSED_KEPT {
        cache.clear();
    }
    cache.insert(message.id.clone(), p.clone());
    p
}

/// `url` fitted whole into `max_w` by the picture height limit, smoothed when it is larger.
fn picture(ctx: &RowCtx, url: &str, max_w: f32) -> Fitted {
    let natural = ctx.picture_sizes.borrow().get(url).copied();
    fitted(url.to_owned(), natural, gpui::size(px(max_w), px(PICTURE_MAX_H)), ctx.scale_factor)
}

/// A box for a picture of `size`: that size, keeping its shape when the column is narrower.
/// Unknown yet: at most the height limit, the picture laying itself out once it has loaded.
/// Either way it hugs the picture instead of stretching across the message column.
fn picture_box<E: Styled>(el: E, size: Option<gpui::Size<Pixels>>) -> E {
    let el = el.self_start();
    match size {
        Some(s) => el.max_w_full().w(s.width).aspect_ratio(f32::from(s.width) / f32::from(s.height)),
        None => el.max_w_full().max_h(px(PICTURE_MAX_H)),
    }
}

pub fn render_row(ctx: &RowCtx, ix: usize, ui: &RowUi, _window: &mut Window, cx: &mut App) -> AnyElement {
    let t = cx.theme().clone();
    let client = ctx.client.read(cx);
    let channel_id = ctx.channel_id.as_str();
    let Some(log) = client.logs.get(channel_id) else { return div().into_any_element() };
    let Some(message) = log.messages.get(ix) else { return div().into_any_element() };
    let prev = ix.checked_sub(1).and_then(|p| log.messages.get(p));
    let divider_here = ui.divider_here;
    let new_day = prev.is_none_or(|p| day_of(p.created_at) != day_of(message.created_at));
    let grouped = !new_day
        && !divider_here
        && message.reply_to.is_none()
        && prev.is_some_and(|p| p.author == message.author && message.created_at - p.created_at < GROUP_WINDOW_MS);

    let guild_id = client.guild_of_channel(channel_id).map(|g| g.guild.id.clone());
    let author_name = client.display_name(guild_id.as_deref(), &message.author);
    let author_color: Option<gpui::Hsla> = guild_id.as_deref().and_then(|g| client.name_color(g, &message.author)).map(|c| gpui::rgb(c).into());
    let author_bot = guild_id.as_deref().and_then(|g| client.member(g, &message.author)).is_some_and(|m| m.is_bot);
    let me = client.user_id().map(str::to_owned);
    let mine = me.as_deref() == Some(message.author.as_str());
    let pending = message.id.starts_with("pending:");
    let failed = client.is_failed(channel_id, &message.id);
    let strip_embeds = client.channel(channel_id).is_some_and(|c| c.strip_embeds);
    let avatar_url = client.avatar_url(guild_id.as_deref(), &message.author);
    let prefs = &client.prefs;
    let avatar_size = if prefs.compact { 28. } else { 40. };
    let body_size = text::MD * (prefs.text_scale.clamp(80, 150) as f32 / 100.);

    let (revealed, highlighted, fresh, selected, hovered, row_hovered) =
        (&ui.revealed, ui.highlighted, ui.fresh, ui.selected.clone(), ui.hovered_link.clone(), ui.hovered);

    let parsed = parsed(&ctx.parsed, message);
    let doc = &parsed.doc;
    // Called on: by name, @everyone, a role we hold, or a reply to one of our messages.
    let mentions_me = me.as_deref().is_some_and(|me| {
        let roles = guild_id.as_deref().map(|g| client.my_roles(g)).unwrap_or_default();
        let replied = message.reply_author.as_deref().or_else(|| {
            message.reply_to.as_deref().and_then(|p| client.find_message(channel_id, p)).map(|p| p.author.as_str())
        }) == Some(me);
        !mine && (replied || markdown::mentions_me(doc, me, &roles))
    });

    // Previews: links outside code, looked up lazily.
    let links: &[String] = if strip_embeds || !client.prefs.link_previews { &[] } else { &parsed.links };
    let mut missing = Vec::new();
    let mut media_missing = Vec::new();
    let mut cards = Vec::new();
    for url in links {
        if is_invite(url) {
            cards.push(invite_card(ctx, ix, url, &t));
            continue;
        }
        // GIFs are shown below, straight from their host, and never unfurled.
        if gif_links::is_gif(url) {
            continue;
        }
        // Telium media says what it is only when asked: a GIF or a picture is shown below as
        // itself; anything else gets the usual preview.
        if gif_links::is_telium_media(url) {
            match client.media_kind(url) {
                Some(MediaProbe::Known(MediaKind::Other)) => {}
                Some(_) => continue,
                None => {
                    media_missing.push(url.clone());
                    continue;
                }
            }
        }
        match client.embed(channel_id, url) {
            Some(EmbedState::Ready(embed)) => cards.push(embed_card(ctx, ix, cards.len(), embed, &t, cx)),
            Some(EmbedState::Loading) => {}
            Some(EmbedState::Empty { retry_at }) if std::time::Instant::now() < *retry_at => {}
            _ => missing.push(url.clone()),
        }
    }

    let seen = client.dm_seen(channel_id) == Some(message.id.as_str());

    // What a bot attached: its cards, and the controls under them.
    let api_base = client.api_base_of_channel(channel_id).unwrap_or_default();
    let bot_cards: Vec<AnyElement> = message.cards.iter().take(5).enumerate().map(|(n, card)| bot_card(ix, n, card, &author_name, &api_base, &t)).collect();
    let controls = (!message.components.is_empty()).then(|| bot_controls(ctx, ix, message, client, &t));
    let deferred = client.deferred.contains(&message.id);

    // GIF links: the picture itself (a share page once its provider has named it).
    let mut shares_missing = Vec::new();
    let mut gifs = Vec::new();
    // As `Client::gif_links_in`, from the links parsed once.
    let is_gif = |u: &&String| client.media_kind(u) == Some(MediaProbe::Known(MediaKind::Gif));
    let gif_links = parsed.gifs.iter().chain(parsed.telium_media.iter().filter(is_gif)).cloned().collect::<Vec<_>>();
    for (n, url) in gif_links.into_iter().enumerate() {
        match gif_links::share(&url) {
            None => gifs.push(gif_tile(ctx, ix, n, url.clone(), url, true, &t, cx)),
            Some(share) => match client.gif_share(&url) {
                Some(GifShare::Ready(media)) => gifs.push(gif_tile(ctx, ix, n, media, url, true, &t, cx)),
                Some(GifShare::Loading) => {}
                Some(GifShare::Failed) => gifs.push(gif_share_link(ix, n, url, share.provider(), &t)),
                None => shares_missing.push(url),
            },
        }
    }
    // Telium media that proved to be a still picture: shown as one, without the GIF star.
    for url in parsed.telium_media.iter().cloned() {
        if client.media_kind(&url) == Some(MediaProbe::Known(MediaKind::Image)) {
            let n = gifs.len() + 100;
            gifs.push(gif_tile(ctx, ix, n, url.clone(), url, false, &t, cx));
        }
    }
    // As `Client::is_gif_only`.
    let gif_only = parsed.gif_only
        || parsed.media_only
            && parsed.telium_media.len() == 1
            && client.media_kind(message.content.trim()) == Some(MediaProbe::Known(MediaKind::Gif));

    let reply_quote = message.reply_to.as_ref().map(|parent_id| {
        let parent = client.find_message(channel_id, parent_id);
        let label = match parent {
            Some(p) => {
                let name = client.display_name(guild_id.as_deref(), &p.author);
                let first = p.content.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                let snippet: String = plain_text(&markdown::spans(first), client, guild_id.as_deref()).chars().take(100).collect();
                format!("{name}: {}", if snippet.is_empty() { "(attachment)".to_owned() } else { snippet })
            }
            None => "Replying to an older message".to_owned(),
        };
        let client = ctx.client.clone();
        let (chan, target) = (ctx.channel_id.clone(), parent_id.clone());
        div()
            .id(SharedString::from(format!("quote-{ix}")))
            .ml(px(20.))
            .mb_0p5()
            .flex()
            .items_center()
            .gap_1()
            .text_size(text::SM)
            .text_color(t.text_faint)
            .cursor_pointer()
            .hover(|s| s.text_color(t.text_muted))
            .on_click(move |_, _, cx| {
                let (chan, target) = (chan.clone(), target.clone());
                client.update(cx, |c, cx| c.jump_to(chan, target, cx));
            })
            .child(icon_sized(Icon::Reply, px(14.)).text_color(t.text_faint))
            .child(div().truncate().child(label))
    });

    // A message that started a thread links to it (not inside the thread itself).
    let thread_chip = message.thread_id.clone().filter(|t| t != channel_id).map(|thread_id| {
        let title = client.thread(&thread_id).map(|t| (t.title.clone(), t.message_count));
        let label = match title {
            Some((title, count)) => format!("{title} · {count} message{}", if count == 1 { "" } else { "s" }),
            None => "View thread".to_owned(),
        };
        let view = ctx.view.clone();
        div()
            .id(SharedString::from(format!("thread-chip-{ix}")))
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded(radius::MD)
            .bg(t.bg_panel)
            .text_size(text::SM)
            .text_color(t.accent)
            .cursor_pointer()
            .hover(|s| s.bg(t.bg_selected))
            .on_click(move |_, _, cx| {
                let id = thread_id.clone();
                view.update(cx, |v, cx| v.open_thread(id, cx)).ok();
            })
            .child(icon_sized(Icon::MessagesSquare, px(14.)).text_color(t.accent))
            .child(label)
    });

    let text_ctx = message_text::Context {
        client: &ctx.client,
        view: &ctx.view,
        message_id: &message.id,
        guild_id: guild_id.as_deref(),
        me: me.as_deref(),
        revealed,
        selected,
        hovered,
        pending: pending && !failed,
        // "(edited)" follows the last line when the message ends in one; else it gets its own.
        edited: message.edited_at.is_some() && message_text::ends_in_text(doc),
    };
    let body_text = (!message.content.trim().is_empty() && !gif_only).then(|| message_text::render(doc, &text_ctx, cx));
    let edited_line = message.edited_at.is_some() && !(body_text.is_some() && text_ctx.edited);

    // Indexed by position in the message, which is how an encrypted attachment is found again.
    type Indexed<'a> = Vec<(usize, &'a Attachment)>;
    let (images, files): (Indexed, Indexed) = message.attachments.iter().enumerate().partition(|(_, a)| a.is_image());
    let body = div()
        .flex()
        .flex_col()
        .gap_1()
        .min_w_0()
        .flex_1()
        .when(!grouped, |d| {
            // Name and time share one line height, so their baselines sit together: GPUI text has
            // no baseline to align by, and boxes of different heights put the time too low.
            d.child(
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .line_height(HEADER_LINE)
                    .child({
                        let (v1, v2, u1, u2) = (ctx.view.clone(), ctx.view.clone(), message.author.clone(), message.author.clone());
                        div()
                            .id(SharedString::from(format!("author-{ix}")))
                            .font_weight(FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .on_click(move |e, _, cx| {
                                let (u, at) = (u1.clone(), e.position());
                                v1.update(cx, |v, cx| v.show_profile(u, at, cx)).ok();
                            })
                            .on_mouse_down(gpui::MouseButton::Right, move |e, _, cx| {
                                cx.stop_propagation();
                                let (u, at) = (u2.clone(), e.position);
                                v2.update(cx, |v, cx| v.member_menu(u, at, cx)).ok();
                            })
                            .when_some(author_color, |d, c| d.text_color(c))
                            .child(author_name.clone())
                    })
                    // Centred on the name, not on its baseline: the tag's padding would drop it.
                    .when(author_bot, |d| d.child(div().self_center().flex().child(rhein_ui::Tag("BOT"))))
                    .child(
                        div()
                            .font_family(FONT_MONO)
                            .text_size(text::XS)
                            .line_height(HEADER_LINE)
                            .text_color(t.text_faint)
                            .child(format_time(message.created_at, prefs.clock_24h)),
                    ),
            )
        })
        // Text sits closer under the header than the gap between the body's parts (media keeps
        // the full gap).
        .children(body_text.map(|b| div().when(!grouped, |d| d.mt(-HEADER_TEXT_PULL)).text_size(body_size).child(b)))
        .when(edited_line, |d| d.child(div().font_family(FONT_MONO).text_size(text::XS).text_color(t.text_faint).child(message_text::EDITED)))
        .children(images.into_iter().map(|(i, a)| image_attachment(ctx, ix, i, &message.id, a, &t, cx)))
        .children(files.into_iter().map(|(i, a)| {
            if rhein_voice::movie::playable(&a.filename, &a.mime) {
                video_attachment(ctx, ix, i, &message.id, a, &t, cx)
            } else if rhein_voice::playback::playable(&a.filename, &a.mime) {
                sound_chip(ctx, ix, i, &message.id, a, &t, cx)
            } else {
                file_chip(ctx, ix, i, &message.id, a, &t, cx).into_any_element()
            }
        }))
        .when(!gifs.is_empty(), |d| d.child(div().flex().flex_wrap().gap_2().children(gifs)))
        .children(cards)
        .children(bot_cards)
        .children(controls)
        .when(deferred, |d| d.child(div().font_family(FONT_MONO).text_size(text::XS).text_color(t.text_faint).child(format!("{author_name} is thinking…"))))
        .when(message.ephemeral, |d| {
            d.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_size(text::XS)
                    .text_color(t.text_faint)
                    .child(icon_sized(Icon::Eye, px(12.)).text_color(t.text_faint))
                    .child("Only you can see this."),
            )
        })
        // Your newest message the other side has read (read receipts on).
        .when(seen, |d| d.child(div().font_family(FONT_MONO).text_size(text::XS).text_color(t.text_faint).child("seen")))
        .when(failed, |d| {
            let (c1, c2) = (ctx.client.clone(), ctx.client.clone());
            let (ch1, id1, ch2, id2) = (ctx.channel_id.clone(), message.id.clone(), ctx.channel_id.clone(), message.id.clone());
            d.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(text::SM)
                    .text_color(t.danger)
                    .child(icon(Icon::CircleAlert).text_color(t.danger))
                    .child("Not sent.")
                    .child(Button::new(SharedString::from(format!("retry-{ix}")), "Retry").small().ghost().on_click(move |_, _, cx| {
                        let (ch, id) = (ch1.clone(), id1.clone());
                        c1.update(cx, |c, cx| c.retry_failed(ch, id, cx));
                    }))
                    .child(Button::new(SharedString::from(format!("discard-{ix}")), "Discard").small().ghost().on_click(move |_, _, cx| {
                        let (ch, id) = (ch2.clone(), id2.clone());
                        c2.update(cx, |c, cx| c.discard_failed(ch, id, cx));
                    })),
            )
        })
        // A little room under the chips, which are taller than a line of text.
        .when(!message.reactions.is_empty(), |d| d.child(div().pb_0p5().child(reactions(ctx, ix, message, ui.hovered_reaction, client, guild_id.as_deref(), &t))))
        .children(thread_chip);

    let group = message_group(ix);
    let gutter = if grouped {
        div()
            .w(px(avatar_size))
            .flex_none()
            .pt_0p5()
            .text_size(text::XS)
            .text_color(t.text_faint)
            .opacity(0.)
            .group_hover(group.clone(), |s| s.opacity(1.))
            // Only the hovered row shapes its time (see `actions`).
            .when(row_hovered, |d| d.child(format_hm(message.created_at, prefs.clock_24h)))
            .into_any_element()
    } else {
        let (v1, u1) = (ctx.view.clone(), message.author.clone());
        div()
            .id(SharedString::from(format!("avatar-{ix}")))
            .w(px(avatar_size))
            .flex_none()
            .cursor_pointer()
            .on_click(move |e, _, cx| {
                let (u, at) = (u1.clone(), e.position());
                v1.update(cx, |v, cx| v.show_profile(u, at, cx)).ok();
            })
            .child(Avatar::new(message.author.clone(), author_name.clone()).size(px(avatar_size)).image(avatar_url.map(Into::into)))
            .into_any_element()
    };

    // Built for the row under the pointer only: hidden on every other row, they were laid out and
    // shaped on each of them in every frame of a scroll. They still show by `group_hover`, so a
    // row the pointer just left draws none even before the view hears of it.
    let actions = (!pending && row_hovered).then(|| hover_actions(ctx, ix, message, &group, client, guild_id.as_deref(), &t));

    let row_bg: Option<Hsla> = if highlighted {
        Some(t.accent_soft.into())
    } else if mentions_me {
        Some(Hsla::from(t.warning).opacity(0.10))
    } else {
        None
    };
    let hover_view = ctx.view.clone();
    let hover_key = message_key(message).to_owned();
    let row = div()
        .id(SharedString::from(format!("row-{hover_key}")))
        .on_hover(move |hovered, _, cx| {
            hover_view.update(cx, |v, cx| v.hover_row(&hover_key, *hovered, cx)).ok();
        })
        .relative()
        .w_full()
        .group(group)
        .flex()
        .flex_col()
        .px_4()
        // The same room above and below the content, so a highlight or the hover sits evenly
        // around it; the space between groups is outside the row (below).
        .py(ROW_PAD)
        .when_some(row_bg, |d, bg| d.bg(bg))
        // The bar lies over the row's edge rather than taking room, so the text stays in line
        // with its neighbours.
        .when(mentions_me, |d| d.child(div().absolute().left_0().top_0().bottom_0().w(px(2.)).bg(t.warning)))
        .when(row_bg.is_none(), |d| d.hover(|s| s.bg(t.bg_hover)))
        .when(mentions_me && !highlighted, |d| {
            let lit = Hsla::from(t.warning).opacity(0.16);
            d.hover(move |s| s.bg(lit))
        })
        .children(reply_quote)
        .child(div().flex().gap_3().pr(px(if ui.narrow { 0. } else { ACTION_BAR_W })).child(gutter).child(body))
        .children(actions)
        .when(!pending, |d| {
            // The message is looked up on the click, not cloned into every row of every frame.
            let (view, client, chan, id) = (ctx.view.clone(), ctx.client.clone(), ctx.channel_id.clone(), message.id.clone());
            d.on_mouse_down(gpui::MouseButton::Right, move |e, window, cx| {
                let Some(m) = client.read(cx).find_message(&chan, &id).cloned() else { return };
                view.update(cx, |v, cx| v.open_message_menu(m, None, e.position, window, cx)).ok();
            })
        });

    // As wide as the list: a highlight or the hover spans the row, and the actions sit at its
    // right edge rather than on the text. A new group starts after a gap the row's background
    // doesn't cover.
    let element = div()
        .w_full()
        .flex()
        .flex_col()
        .when(new_day, |d| d.child(day_separator(message.created_at, &t)))
        .when(divider_here, |d| d.child(new_messages_line(&t)))
        // A day line is the gap itself: the same room above it as below.
        .when(!grouped && !new_day, |d| d.pt(if prefs.compact { GROUP_GAP_COMPACT } else { GROUP_GAP }))
        .child(row);
    // A message that just arrived: it is first laid out unseen and without taking room (with its
    // group gap and any day line), to learn its height; then that space opens smoothly (the
    // history moves up) and the message fades in. History renders as it is.
    let element = match fresh {
        Some(None) => {
            let (view, key) = (ctx.view.clone(), message_key(message).to_owned());
            div()
                .w_full()
                .relative()
                // One pixel, not none: the list lays out (and so measures) only rows that reach
                // into its view.
                .h(px(1.))
                .overflow_hidden()
                .on_children_prepainted(move |bounds, _, cx| {
                    let Some(height) = bounds.first().map(|b| f32::from(b.size.height)) else { return };
                    let key = key.clone();
                    view.update(cx, |v, cx| v.fresh_measured(key, height, cx)).ok();
                })
                .child(div().absolute().top_0().left_0().right_0().opacity(0.).child(element))
                .into_any_element()
        }
        Some(Some(height)) => {
            let id = SharedString::from(format!("fresh-{}", message_key(message)));
            motion::expand_then_fade_in(div().w_full().overflow_hidden().child(element), id, height).into_any_element()
        }
        None => element.into_any_element(),
    };
    // Unknown previews are asked for after this frame, never during it.
    if !media_missing.is_empty() {
        let client = ctx.client.clone();
        cx.defer(move |cx| {
            client.update(cx, |c, cx| {
                for url in &media_missing {
                    c.request_media_kind(url, cx);
                }
            })
        });
    }
    if !shares_missing.is_empty() {
        let client = ctx.client.clone();
        cx.defer(move |cx| {
            client.update(cx, |c, cx| {
                for url in &shares_missing {
                    c.request_gif_share(url, cx);
                }
            })
        });
    }
    if !missing.is_empty() {
        let client = ctx.client.clone();
        let chan = ctx.channel_id.clone();
        cx.defer(move |cx| {
            client.update(cx, |c, cx| {
                for url in &missing {
                    c.request_embed(&chan, url, cx);
                }
            })
        });
    }
    element
}

/// The emoji the hover bar reacts with: the most recently used first.
fn quick_reactions(store: &Client) -> Vec<String> {
    let mut quick: Vec<String> = store.recent_emoji.iter().take(QUICK_REACTIONS).cloned().collect();
    for emoji in DEFAULT_REACTIONS {
        if quick.len() == QUICK_REACTIONS {
            break;
        }
        if !quick.iter().any(|e| e == emoji) {
            quick.push(emoji.to_owned());
        }
    }
    quick
}

#[allow(clippy::too_many_arguments)]
fn hover_actions(ctx: &RowCtx, ix: usize, message: &Message, group: &SharedString, store: &Client, guild_id: Option<&str>, t: &Theme) -> AnyElement {
    // Astride the row's top edge, as in Discord, so it covers a short message's neighbours
    // least; the first row keeps it inside, where the list would clip it.
    let mut bar = div()
        .absolute()
        .top(px(if ix == 0 { 2. } else { -16. }))
        .right(px(16.))
        .flex()
        .gap_0p5()
        .p_0p5()
        .rounded(radius::MD)
        .bg(t.bg_raised)
        .border_1()
        .border_color(t.border_strong)
        .shadow_md()
        .opacity(0.)
        .group_hover(group.clone(), |s| s.opacity(1.));
    for (i, emoji) in quick_reactions(store).into_iter().enumerate() {
        let client = ctx.client.clone();
        let (chan, mid) = (ctx.channel_id.clone(), message.id.clone());
        let label = match markdown::spans(&emoji).as_slice() {
            [markdown::Span::Emoji { name, .. }] => format!(":{name}:"),
            _ => emoji.clone(),
        };
        let picture = reaction_emoji(&emoji, (ix, 100 + i), store, guild_id);
        bar = bar.child(
            div()
                .id(SharedString::from(format!("qr-{ix}-{i}")))
                .size(size::ICON_BUTTON)
                .flex()
                .items_center()
                .justify_center()
                .rounded(radius::MD)
                .cursor_pointer()
                .hover(|s| s.bg(t.bg_hover))
                .tooltip(tooltip(format!("React with {label}")))
                .on_click(move |_, _, cx| {
                    let (chan, mid, emoji) = (chan.clone(), mid.clone(), emoji.clone());
                    client.update(cx, |c, cx| {
                        c.use_emoji(emoji.clone());
                        c.toggle_reaction(chan, mid, emoji, cx);
                    });
                })
                .child(picture),
        );
    }
    let view = ctx.view.clone();
    let react_id = message.id.clone();
    bar = bar.child(IconButton::new(SharedString::from(format!("react-pick-{ix}")), Icon::SmilePlus).tooltip("Add reaction").on_click(
        move |e, window, cx| {
            let (id, at) = (react_id.clone(), e.position());
            view.update(cx, |v, cx| v.react_picker(at, id, window, cx)).ok();
        },
    ));
    let view = ctx.view.clone();
    let reply_id = message.id.clone();
    bar = bar.child(IconButton::new(SharedString::from(format!("reply-{ix}")), Icon::Reply).tooltip("Reply").on_click(
        move |_, window, cx| {
            let id = reply_id.clone();
            view.update(cx, |v, cx| v.start_reply(id, window, cx)).ok();
        },
    ));
    let view = ctx.view.clone();
    let for_menu = message.clone();
    bar = bar.child(IconButton::new(SharedString::from(format!("more-{ix}")), Icon::Ellipsis).tooltip("More").on_click(
        move |e, window, cx| {
            let (m, at) = (for_menu.clone(), e.position());
            view.update(cx, |v, cx| {
                // A second press on the same button closes its menu.
                if !v.menu.toggle(&format!("more-{}", m.id), cx) {
                    v.open_message_menu(m, None, at, window, cx);
                }
            })
            .ok();
        },
    ));
    bar.into_any_element()
}

fn reactions(ctx: &RowCtx, ix: usize, message: &Message, hovered_reaction: Option<usize>, store: &Client, guild_id: Option<&str>, t: &Theme) -> AnyElement {
    div()
        .flex()
        .flex_wrap()
        .gap_1()
        .children(message.reactions.iter().enumerate().map(|(i, r)| {
            let client = ctx.client.clone();
            let (chan, mid, emoji) = (ctx.channel_id.clone(), message.id.clone(), r.emoji.clone());
            let hovered = hovered_reaction == Some(i);
            let who = hovered.then(|| reactors(r, store, guild_id, t)).flatten();
            let (view, hover_id) = (ctx.view.clone(), message.id.clone());
            div()
                .id(SharedString::from(format!("react-{ix}-{i}")))
                .relative()
                .on_hover(move |hovered, _, cx| {
                    let key = (hover_id.clone(), i);
                    view.update(cx, |v, cx| {
                        let before = v.hovered_reaction.clone();
                        if *hovered {
                            v.hovered_reaction = Some(key);
                        } else if v.hovered_reaction.as_ref() == Some(&key) {
                            v.hovered_reaction = None;
                        }
                        if v.hovered_reaction != before {
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .px_2()
                .py_0p5()
                .rounded(radius::MD)
                .border_1()
                .border_color(if r.me { t.accent } else { t.border_strong })
                .bg(if r.me { t.accent_soft } else { t.bg_raised })
                .text_size(text::SM)
                .cursor_pointer()
                .hover(|s| s.border_color(t.text_faint))
                .on_click(move |_, _, cx| {
                    let (chan, mid, emoji) = (chan.clone(), mid.clone(), emoji.clone());
                    client.update(cx, |c, cx| c.toggle_reaction(chan, mid, emoji, cx));
                })
                .flex()
                .items_center()
                .gap_1()
                .child(reaction_emoji(&r.emoji, (ix, i), store, guild_id))
                .child(div().text_color(if r.me { t.accent } else { t.text_muted }).child(r.count.to_string()))
                .children(who)
        }))
        .into_any_element()
}

/// Who reacted, shown to the left of the reaction while it is hovered: the first three, and how
/// many more under them.
fn reactors(r: &rhein_proto::Reaction, store: &Client, guild_id: Option<&str>, t: &Theme) -> Option<AnyElement> {
    const SHOWN: usize = 3;
    if r.users.is_empty() {
        return None;
    }
    let names: Vec<String> = r.users.iter().take(SHOWN).map(|u| store.display_name(guild_id, u)).collect();
    let others = (r.count.max(r.users.len() as i64) as usize).saturating_sub(names.len());
    let more = (others > 0).then(|| {
        let label = if others == 1 { "and 1 other".to_owned() } else { format!("and {others} others") };
        div().text_color(t.text_faint).child(label)
    });
    // Painted after the rows, so the messages below don't cover it. (Not with `group_hover`: a
    // deferred element is painted outside its group, so that never shows it.)
    Some(
        gpui::deferred(
            div()
                .absolute()
                .top_0()
                .right_full()
                .mr_1()
                .flex()
                .flex_col()
                .px_2()
                .py_1()
                .rounded(radius::MD)
                .bg(t.bg_raised)
                .border_1()
                .border_color(t.border_strong)
                .shadow_md()
                .text_size(text::XS)
                .text_color(t.text)
                .whitespace_nowrap()
                .children(names.into_iter().map(|n| div().child(n)))
                .children(more),
        )
        .into_any_element(),
    )
}

/// A line of a message as plain text, for a one-line quote: mentions by name, custom emoji as
/// `:name:`, formatting dropped and spoilers kept hidden.
fn plain_text(spans: &[markdown::Span], store: &Client, guild_id: Option<&str>) -> String {
    use markdown::Span;
    let mut out = String::new();
    for span in spans {
        match span {
            Span::Text(t) | Span::Code(t) | Span::Broadcast(t) | Span::Link(t) => out.push_str(t),
            Span::Bold(inner) | Span::Italic(inner) | Span::Strike(inner) => out.push_str(&plain_text(inner, store, guild_id)),
            Span::Spoiler(_) => out.push_str("▮▮▮"),
            Span::User(user) => out.push_str(&format!("@{}", store.display_name(guild_id, user))),
            Span::Role(id) => {
                let role = guild_id.and_then(|g| store.guild(g)).and_then(|g| g.guild.roles.iter().find(|r| &r.id == id).map(|r| r.name.clone()));
                out.push_str(&format!("@{}", role.as_deref().unwrap_or("unknown-role")));
            }
            Span::Channel(id) => out.push_str(&format!("#{}", store.channel(id).map_or("unknown-channel", |c| c.name.as_str()))),
            Span::Emoji { name, .. } => out.push_str(&format!(":{name}:")),
        }
    }
    out
}

/// A reaction's emoji: a custom one (`<name:ID>`) as its picture, a unicode one as text.
fn reaction_emoji(emoji: &str, id: (usize, usize), client: &Client, guild_id: Option<&str>) -> AnyElement {
    let text_emoji = || div().font_family(FONT_EMOJI).child(emoji.to_owned()).into_any_element();
    let spans = markdown::spans(emoji);
    let [markdown::Span::Emoji { name, id: emoji_id, origin }] = spans.as_slice() else { return text_emoji() };
    let Some(url) = client.emoji_url(guild_id, emoji_id, origin.as_deref()) else {
        return div().child(format!(":{name}:")).into_any_element();
    };
    let name = name.clone();
    img(images::source(url))
        .id(SharedString::from(format!("react-emoji-{}-{}", id.0, id.1)))
        .size(px(18.))
        .object_fit(ObjectFit::Contain)
        .with_fallback(move || div().child(format!(":{name}:")).into_any_element())
        .into_any_element()
}

/// The natural size of a picture, scaled down to the height limit; its aspect ratio when known.
fn fit(width: Option<i64>, height: Option<i64>) -> (Pixels, Option<f32>) {
    match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => {
            let (w, h) = (w as f32, h as f32);
            let scale = (PICTURE_MAX_H / h).min(1.0);
            (px((w * scale).max(16.)), Some(w / h))
        }
        _ => (px(PICTURE_UNKNOWN.0), None),
    }
}

/// Whether a link is a GIF (a .gif, or a GIF provider's).
/// A GIF from a link: `media` is the picture, `link` what the message said (a share page, or the
/// picture again).
/// A picture straight from its link: a GIF (with its favourite star) or a still picture.
#[allow(clippy::too_many_arguments)]
fn gif_tile(ctx: &RowCtx, ix: usize, n: usize, media: String, link: String, starred: bool, t: &Theme, cx: &App) -> AnyElement {
    let group = SharedString::from(format!("gif-group-{ix}-{n}"));
    let star = starred.then(|| gif_star(ctx, format!("gif-star-{ix}-{n}"), message_group(ix), media.clone(), t, cx));
    // A still picture (not a GIF) can be saved from its corner.
    let save = (!starred).then(|| download_button(ctx, format!("gif-save-{ix}-{n}"), group.clone(), Media::new(media.clone(), media.clone()), t));
    let menu_media = Media::new(media.clone(), media.clone());
    let view = ctx.view.clone();
    let (open, raised, faint) = (media.clone(), t.bg_raised, t.text_faint);
    let Fitted { source, size } = picture(ctx, &media, f32::MAX);
    picture_box(div(), size)
        .id(SharedString::from(format!("gif-{ix}-{n}")))
        .group(group)
        .relative()
        .rounded(radius::LG)
        .overflow_hidden()
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            let (open, name) = (open.clone(), link.clone());
            view.update(cx, |v, cx| v.open_lightbox(open, name, window, cx)).ok();
        })
        .child(
            img(source)
                .id("pic")
                .map(|i| if size.is_some() { i.size_full() } else { i.max_w_full().max_h(px(PICTURE_MAX_H)) })
                .object_fit(ObjectFit::Contain)
                .with_loading(move || {
                    let placeholder = div().bg(raised);
                    if size.is_some() { placeholder.size_full() } else { placeholder.w(px(PICTURE_UNKNOWN.0 / 2.)).h(px(PICTURE_UNKNOWN.1 / 2.)) }.into_any_element()
                })
                .with_fallback(move || {
                    div().p_3().text_size(text::SM).text_color(faint).child("This picture is no longer available").into_any_element()
                }),
        )
        .children(star)
        .children(save)
        .map(|d| media_menu(d, ctx, ix, menu_media))
        .into_any_element()
}

/// A share page whose provider had no GIF for it: a link to open it there.
fn gif_share_link(ix: usize, n: usize, url: String, provider: &str, t: &Theme) -> AnyElement {
    div()
        .id(SharedString::from(format!("gif-link-{ix}-{n}")))
        .text_size(text::SM)
        .text_color(t.link)
        .cursor_pointer()
        .hover(|s| s.text_color(t.link_hover).underline())
        .on_click(move |_, _, cx| cx.open_url(&url))
        .child(format!("Open GIF on {provider}"))
        .into_any_element()
}

/// The hover group of the message at `ix`.
fn message_group(ix: usize) -> SharedString {
    SharedString::from(format!("msg-{ix}"))
}

/// The star over a GIF (shown while the pointer is on its message, favourite or not): adds it
/// to the favourites or takes it out.
fn gif_star(ctx: &RowCtx, id: String, group: SharedString, url: String, t: &Theme, cx: &App) -> AnyElement {
    let starred = ctx.client.read(cx).is_gif_favorite(&url);
    let client = ctx.client.clone();
    div()
        .absolute()
        .top_1p5()
        .left_1p5()
        .rounded(radius::MD)
        .bg(t.bg_base)
        .invisible()
        .group_hover(group, |s| s.visible())
        .child(
            IconButton::new(SharedString::from(id), if starred { Icon::StarFilled } else { Icon::Star })
                .active(starred)
                .tooltip(if starred { "Remove from favourite GIFs" } else { "Add to favourite GIFs" })
                .on_click(move |_, _, cx| client.update(cx, |c, cx| c.toggle_gif_favorite(&url, cx))),
        )
        .into_any_element()
}

/// Right-clicking a picture opens its message's menu, starting with copying and saving it.
fn media_menu<E: InteractiveElement>(el: E, ctx: &RowCtx, ix: usize, media: Media) -> E {
    let view = ctx.view.clone();
    el.on_mouse_down(gpui::MouseButton::Right, move |e, window, cx| {
        cx.stop_propagation();
        let (media, at) = (media.clone(), e.position);
        view.update(cx, |v, cx| v.open_media_menu(ix, media, at, window, cx)).ok();
    })
}

/// The download button in a still picture's top right corner, shown while it is hovered.
fn download_button(ctx: &RowCtx, id: String, group: SharedString, media: Media, t: &Theme) -> AnyElement {
    let client = ctx.client.clone();
    div()
        .absolute()
        .top_1p5()
        .right_1p5()
        .rounded(radius::MD)
        .bg(t.bg_base)
        .invisible()
        .group_hover(group, |s| s.visible())
        .child(
            IconButton::new(SharedString::from(id), Icon::Download)
                .tooltip("Download")
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    media::download(&client, media.clone(), cx);
                }),
        )
        .into_any_element()
}

fn image_attachment(ctx: &RowCtx, ix: usize, i: usize, message_id: &str, a: &Attachment, t: &Theme, cx: &App) -> AnyElement {
    let Some(url) = a.url.clone() else { return file_chip(ctx, ix, i, message_id, a, t, cx).into_any_element() };
    // The size the attachment says it has, else the one it turned out to have once it arrived.
    let natural = match (a.width, a.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some((w as u32, h as u32)),
        _ => ctx.picture_sizes.borrow().get(&url).copied(),
    };
    let Fitted { source, size } = fitted(url.clone(), natural, gpui::size(px(f32::MAX), px(PICTURE_MAX_H)), ctx.scale_factor);
    let group = SharedString::from(format!("img-group-{ix}-{i}"));
    let star = (a.mime == "image/gif").then(|| gif_star(ctx, format!("img-star-{ix}-{i}"), message_group(ix), url.clone(), t, cx));
    let menu_media = Media::new(url.clone(), a.filename.clone());
    let save = (a.mime != "image/gif").then(|| download_button(ctx, format!("img-save-{ix}-{i}"), group.clone(), menu_media.clone(), t));
    let view = ctx.view.clone();
    let (open, name) = (url.clone(), a.filename.clone());
    let fallback_name = a.filename.clone();
    let faint = t.text_faint;
    let raised = t.bg_raised;
    picture_box(div(), size)
        .id(SharedString::from(format!("img-{ix}-{i}")))
        .group(group)
        .relative()
        .rounded(radius::LG)
        .overflow_hidden()
        .bg(t.bg_raised)
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            let (open, name) = (open.clone(), name.clone());
            view.update(cx, |v, cx| v.open_lightbox(open, name, window, cx)).ok();
        })
        .child(
            img(source)
                .id("pic")
                .map(|i| if size.is_some() { i.size_full() } else { i.max_w_full().max_h(px(PICTURE_MAX_H)) })
                .object_fit(ObjectFit::Contain)
                .with_loading(move || {
                    let placeholder = div().bg(raised);
                    if size.is_some() { placeholder.size_full() } else { placeholder.w(px(PICTURE_UNKNOWN.0)).h(px(PICTURE_UNKNOWN.1)) }.into_any_element()
                })
                .with_fallback(move || {
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(text::SM)
                        .text_color(faint)
                        .child(format!("Couldn't load {fallback_name}. Check your connection and try again."))
                        .into_any_element()
                }),
        )
        .children(star)
        .children(save)
        .map(|d| media_menu(d, ctx, ix, menu_media))
        .into_any_element()
}

/// A sound file: play and pause, where it is (click the bar to jump), how long it is.
fn sound_chip(ctx: &RowCtx, ix: usize, i: usize, message_id: &str, a: &Attachment, t: &Theme, cx: &App) -> AnyElement {
    let key = format!("{message_id}#{i}");
    let state = ctx.client.read(cx).sound(&key);
    let (playing, loading) = state.as_ref().map_or((false, false), |s| (s.playing, s.loading));
    let (position, duration) = state.as_ref().map_or((0., None), |s| (s.position, s.duration));
    let fraction = duration.filter(|d| *d > 0.).map_or(0., |d| (position / d).clamp(0., 1.)) as f32;
    let clock = |secs: f64| format!("{}:{:02}", (secs / 60.) as u64, secs as u64 % 60);
    let times = match (state.as_ref(), duration) {
        (Some(s), _) if s.error.is_some() => s.error.clone().unwrap_or_default(),
        (Some(_), Some(d)) => format!("{} / {}", clock(position), clock(d)),
        (Some(s), None) if s.loading => "Loading…".to_owned(),
        _ => format_size(a.size),
    };
    let source = attachment_source(ctx, message_id, i, a);
    let (client, toggle_key, name) = (ctx.client.clone(), key.clone(), a.filename.clone());
    let (seek_client, seek_key) = (ctx.client.clone(), key);
    // Where the bar is on screen, for turning a click into a position.
    let bar = std::rc::Rc::new(std::cell::Cell::new(None::<gpui::Bounds<Pixels>>));
    let measured = bar.clone();
    div()
        .id(SharedString::from(format!("sound-{ix}-{i}")))
        .w(px(420.))
        .max_w_full()
        .px_3()
        .py_2()
        .rounded(radius::MD)
        .bg(t.bg_raised)
        .border_1()
        .border_color(t.border)
        .flex()
        .gap_3()
        .items_center()
        .child(
            IconButton::new(SharedString::from(format!("sound-play-{ix}-{i}")), if playing { Icon::Pause } else { Icon::Play })
                .tooltip(if playing { "Pause" } else { "Play" })
                .on_click(move |_, _, cx| {
                    let (key, source, name) = (toggle_key.clone(), source.clone(), name.clone());
                    client.update(cx, |c, cx| c.toggle_sound(key, source, name, cx));
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(div().flex_1().min_w_0().truncate().text_size(text::SM).text_color(t.text).child(a.filename.clone()))
                        .child(div().flex_none().font_family(FONT_MONO).text_size(text::XS).text_color(t.text_faint).child(times)),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("sound-bar-{ix}-{i}")))
                        .relative()
                        .h(px(6.))
                        .w_full()
                        .rounded_full()
                        .bg(t.bg_selected)
                        .when(state.is_some() && !loading, |d| d.cursor_pointer())
                        .child(div().absolute().left_0().top_0().h_full().rounded_full().bg(t.accent).w(gpui::relative(fraction)))
                        .child(
                            gpui::canvas(move |bounds, _, _| measured.set(Some(bounds)), |_, _, _, _| {})
                                .absolute()
                                .inset_0(),
                        )
                        .on_mouse_down(gpui::MouseButton::Left, move |e, _, cx| {
                            let Some(bounds) = bar.get() else { return };
                            let fraction = ((e.position.x - bounds.left()) / bounds.size.width) as f64;
                            seek_client.update(cx, |c, cx| c.seek_sound(&seek_key, fraction, cx));
                        }),
                ),
        )
        .into_any_element()
}

/// Where an attachment's bytes come from: its link, or, encrypted in a DM, its message.
fn attachment_source(ctx: &RowCtx, message_id: &str, i: usize, a: &Attachment) -> MediaSource {
    match &a.url {
        Some(url) => MediaSource::Url(url.clone()),
        None => MediaSource::Dm { conversation: ctx.channel_id.clone(), message: message_id.to_owned(), index: i },
    }
}

/// How far a download has come, for showing in place of the file's size.
fn download_progress(download: DownloadView) -> String {
    match download.total {
        Some(total) => format!("Downloading… {}%", download.got * 100 / total.max(1)),
        None => format!("Downloading… {}", format_size(download.got as i64)),
    }
}

/// Clicking a file saves it into the downloads folder; while it downloads the chip says how far.
fn file_chip(ctx: &RowCtx, ix: usize, i: usize, message_id: &str, a: &Attachment, t: &Theme, cx: &App) -> impl IntoElement {
    let key = format!("{message_id}#{i}");
    let download = ctx.client.read(cx).download(&key);
    let (client, source, name) = (ctx.client.clone(), attachment_source(ctx, message_id, i, a), a.filename.clone());
    div()
        .id(SharedString::from(format!("att-{ix}-{i}")))
        .max_w(px(420.))
        .px_3()
        .py_2()
        .rounded(radius::MD)
        .bg(t.bg_raised)
        .border_1()
        .border_color(t.border)
        .flex()
        .gap_2()
        .items_center()
        .cursor_pointer()
        .hover(|s| s.bg(t.bg_selected))
        .tooltip(tooltip("Download"))
        .on_click(move |_, _, cx| {
            let (key, source, name) = (key.clone(), source.clone(), name.clone());
            client.update(cx, |c, cx| c.download_file(key, source, name, cx));
        })
        .child(icon(if a.is_image() { Icon::Image } else { Icon::File }).text_color(t.text_muted))
        .child(div().flex_1().truncate().text_color(t.accent).child(a.filename.clone()))
        .child(div().text_size(text::XS).text_color(t.text_faint).child(download.map_or_else(|| format_size(a.size), download_progress)))
        .child(icon(Icon::Download).text_color(t.text_muted))
}

/// The box a video takes in the chat: its own shape (16:9 until known), fitted into this.
const VIDEO_MAX: (f32, f32) = (560., 360.);

/// A video: a poster with a play button until it is played, then the chat's player, in a box of
/// the video's shape. Saving it sits in the top right corner, shown while it is hovered.
fn video_attachment(ctx: &RowCtx, ix: usize, i: usize, message_id: &str, a: &Attachment, t: &Theme, cx: &App) -> AnyElement {
    let key = format!("{message_id}#{i}");
    let source = attachment_source(ctx, message_id, i, a);
    let player = ctx.client.read(cx).movie_player.read(cx);
    let loaded = player.key() == Some(key.as_str());
    let poster = ctx.client.read(cx).movie_posters.read(cx).get(&key).cloned();
    // Its size as the message says, else its poster's, else the player's once it plays.
    let natural = match (a.width, a.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some((w as f32, h as f32)),
        _ => poster
            .as_ref()
            .and_then(Poster::size)
            .or_else(|| player.view().filter(|_| loaded).and_then(|v| v.size))
            .filter(|(w, h)| *w > 0 && *h > 0)
            .map(|(w, h)| (w as f32, h as f32)),
    };
    let poster = match poster {
        Some(Poster::Ready { path, .. }) => Some(path),
        _ => None,
    };
    let (w, h) = natural.unwrap_or((16., 9.));
    let scale = (VIDEO_MAX.0 / w).min(VIDEO_MAX.1 / h);
    let size = gpui::size(px(w * scale), px(h * scale));
    let group = SharedString::from(format!("video-group-{ix}-{i}"));
    let body = if loaded && ctx.movie_big {
        div().size_full().flex().items_center().justify_center().text_size(text::SM).text_color(gpui::white()).child("Playing enlarged").into_any_element()
    } else if loaded {
        ctx.movie.clone().into_any_element()
    } else {
        let (client, key, source) = (ctx.client.clone(), key.clone(), source.clone());
        let (resting, rest_key, rest_source) = (ctx.client.clone(), key.clone(), source.clone());
        let download = ctx.client.read(cx).download(&key);
        div()
            .id(SharedString::from(format!("video-poster-{ix}-{i}")))
            .size_full()
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            // Resting on it fetches its start, so a click plays at once.
            .on_hover(move |hovered, _, cx| {
                let source = hovered.then(|| rest_source.clone());
                resting.update(cx, |c, cx| c.rest_on_movie(rest_key.clone(), source, cx));
            })
            .on_click(move |_, _, cx| {
                let (key, source) = (key.clone(), source.clone());
                client.update(cx, |c, cx| c.toggle_movie(key, source, cx));
            })
            .children(poster.map(|path| img(rhein_ui::images::file(path)).absolute().inset_0().size_full().object_fit(ObjectFit::Contain)))
            .child(
                div()
                    .size(px(56.))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(gpui::Hsla::from(t.bg_base).opacity(0.85))
                    .child(icon_sized(Icon::Play, px(24.)).text_color(t.text)),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .px_3()
                    .py_1p5()
                    .flex()
                    .gap_2()
                    .text_size(text::XS)
                    .bg(gpui::Hsla::from(t.bg_base).opacity(0.85))
                    .child(div().flex_1().min_w_0().truncate().text_color(t.text).child(a.filename.clone()))
                    .child(div().flex_none().font_family(FONT_MONO).text_color(t.text_faint).child(download.map_or_else(|| format_size(a.size), download_progress))),
            )
            .into_any_element()
    };
    picture_box(div(), Some(size))
        .id(SharedString::from(format!("video-{ix}-{i}")))
        .group(group.clone())
        .relative()
        .rounded(radius::LG)
        .overflow_hidden()
        .bg(gpui::black())
        .child(body)
        .child(save_button(ctx, format!("video-save-{ix}-{i}"), group, key, source, a.filename.clone(), t))
        .into_any_element()
}

/// Saving an attachment into the downloads folder, from a button in its top right corner shown
/// while it is hovered.
fn save_button(ctx: &RowCtx, id: String, group: SharedString, key: String, source: MediaSource, name: String, t: &Theme) -> AnyElement {
    let client = ctx.client.clone();
    div()
        .absolute()
        .top_1p5()
        .right_1p5()
        .rounded(radius::MD)
        .bg(t.bg_base)
        .invisible()
        .group_hover(group, |s| s.visible())
        .child(IconButton::new(SharedString::from(id), Icon::Download).tooltip("Download").on_click(move |_, _, cx| {
            cx.stop_propagation();
            let (key, source, name) = (key.clone(), source.clone(), name.clone());
            client.update(cx, |c, cx| c.download_file(key, source, name, cx));
        }))
        .into_any_element()
}

/// Only `https:` links without credentials reach a bot's buttons and cards.
fn safe_link(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| u.scheme() == "https" && u.username().is_empty() && u.password().is_none())
}

/// A bot's action rows: buttons (a link button opens its address and never reaches the server)
/// and selects, which open a menu of their options.
fn bot_controls(ctx: &RowCtx, ix: usize, message: &Message, client: &Client, t: &Theme) -> AnyElement {
    let mut rows = div().flex().flex_col().gap_1p5().pt_1();
    for (r, row) in message.components.iter().filter(|c| c.kind == "action_row").take(5).enumerate() {
        let mut line = div().flex().flex_wrap().gap_1p5();
        for (n, c) in row.components.iter().take(5).enumerate() {
            let id = SharedString::from(format!("bot-{ix}-{r}-{n}"));
            let label = c.label.clone().unwrap_or_default();
            match c.kind.as_str() {
                "button" if c.style.as_deref() == Some("link") => {
                    let url = c.url.clone().filter(|u| safe_link(u));
                    line = line.child(
                        Button::new(id, label)
                            .small()
                            .icon(Icon::ExternalLink)
                            .disabled(c.disabled || url.is_none())
                            .on_click(move |_, _, cx| {
                                if let Some(url) = &url {
                                    cx.open_url(url);
                                }
                            }),
                    );
                }
                "button" => {
                    let Some(custom) = c.custom_id.clone() else { continue };
                    let busy = client.pressing.contains(&(message.id.clone(), custom.clone()));
                    let kind = match c.style.as_deref() {
                        Some("primary" | "success") => rhein_ui::ButtonKind::Primary,
                        Some("danger") => rhein_ui::ButtonKind::Danger,
                        _ => rhein_ui::ButtonKind::Secondary,
                    };
                    let (client, chan, mid) = (ctx.client.clone(), ctx.channel_id.clone(), message.id.clone());
                    line = line.child(Button::new(id, if busy { format!("{label}…") } else { label }).small().kind(kind).disabled(c.disabled || busy).on_click(
                        move |_, _, cx| {
                            let (chan, mid, custom) = (chan.clone(), mid.clone(), custom.clone());
                            client.update(cx, |c, cx| c.press_component(chan, mid, custom, Vec::new(), cx));
                        },
                    ));
                }
                "select" => {
                    if c.custom_id.is_none() {
                        continue;
                    }
                    let chosen: Vec<&str> = c.options.iter().filter(|o| o.default).map(|o| o.label.as_str()).collect();
                    let shown = if chosen.is_empty() { c.placeholder.clone().unwrap_or_else(|| "Choose…".into()) } else { chosen.join(", ") };
                    let (view, mid, select) = (ctx.view.clone(), message.id.clone(), c.clone());
                    line = line.child(
                        div()
                            .id(id)
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .h(px(28.))
                            .min_w(px(180.))
                            .rounded(radius::MD)
                            .border_1()
                            .border_color(t.border)
                            .bg(t.bg_panel)
                            .text_size(text::SM)
                            .when(c.disabled, |d| d.opacity(0.5))
                            .when(!c.disabled, |d| d.cursor_pointer().hover(|s| s.bg(t.bg_selected)))
                            .on_click(move |e, window, cx| {
                                if select.disabled {
                                    return;
                                }
                                let (mid, select, at) = (mid.clone(), select.clone(), e.position());
                                view.update(cx, |v, cx| v.open_select_menu(mid, select, at, window, cx)).ok();
                            })
                            .child(div().flex_1().truncate().text_color(if chosen.is_empty() { t.text_faint } else { t.text }).child(shown))
                            .child(icon_sized(Icon::ChevronDown, px(14.)).text_color(t.text_faint)),
                    );
                }
                _ => {}
            }
        }
        rows = rows.child(line);
    }
    rows.into_any_element()
}

/// A bot's card: drawn with the bot's name on it, apart from link previews.
fn bot_card(ix: usize, n: usize, card: &rhein_proto::Card, bot: &str, api_base: &str, t: &Theme) -> AnyElement {
    let stripe: Hsla = card.colour.map_or(t.accent.into(), |c| gpui::rgb(c & 0xff_ffff).into());
    let link = card.url.clone().filter(|u| safe_link(u));
    let image = card.image.as_ref().and_then(|i| i.source(api_base));
    let thumb = card.thumbnail.as_ref().and_then(|i| i.source(api_base));
    let clip = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    let mut fields = div().flex().flex_wrap().gap_x_4().gap_y_1();
    for f in card.fields.iter().take(25) {
        fields = fields.child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .when(f.inline, |d| d.max_w(px(180.)))
                .when(!f.inline, |d| d.w_full())
                .child(div().text_size(text::SM).font_weight(FontWeight::SEMIBOLD).child(clip(&f.name, 256)))
                .child(div().text_size(text::SM).text_color(t.text_muted).child(clip(&f.value, 1024))),
        );
    }
    let footer = [card.footer.as_ref().and_then(|f| f.text.clone().or(f.name.clone())), card.timestamp.map(|ms| format_time(ms, true))]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    div()
        .max_w(px(IMAGE_MAX_W + 32.))
        .min_w_0()
        .flex()
        .rounded(radius::MD)
        .bg(t.bg_panel)
        .overflow_hidden()
        .child(div().w(px(3.)).flex_none().bg(stripe))
        .child(
            div()
                .flex()
                .flex_1()
                .gap_3()
                .p_3()
                .min_w_0()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .gap_1()
                        .min_w_0()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_size(text::XS)
                                .text_color(t.text_faint)
                                .child(icon_sized(Icon::Bot, px(12.)).text_color(t.text_faint))
                                .child(card.author.as_ref().and_then(|a| a.name.clone()).map_or_else(|| bot.to_owned(), |a| format!("{} · {bot}", clip(&a, 256)))),
                        )
                        .children(card.title.clone().map(|title| {
                            let link = link.clone();
                            div()
                                .id(SharedString::from(format!("card-title-{ix}-{n}")))
                                .font_weight(FontWeight::SEMIBOLD)
                                .when(link.is_some(), |d| d.text_color(t.link).cursor_pointer().hover(|s| s.underline()))
                                .on_click(move |_, _, cx| {
                                    if let Some(url) = &link {
                                        cx.open_url(url);
                                    }
                                })
                                .child(clip(&title, 256))
                        }))
                        .children(card.description.clone().map(|d| div().text_size(text::SM).text_color(t.text_muted).child(clip(&d, 4096))))
                        .when(!card.fields.is_empty(), |d| d.child(fields))
                        .children(image.map(|src| img(images::source(src)).mt_1().max_w(px(IMAGE_MAX_W)).max_h(px(PICTURE_MAX_H)).object_fit(ObjectFit::Contain).rounded(radius::MD)))
                        .when(!footer.is_empty(), |d| d.child(div().text_size(text::XS).text_color(t.text_faint).child(clip(&footer, 2048)))),
                )
                .children(thumb.map(|src| img(images::source(src)).size(px(64.)).flex_none().rounded(radius::MD).object_fit(ObjectFit::Cover))),
        )
        .into_any_element()
}

fn embed_card(ctx: &RowCtx, ix: usize, n: usize, e: &Embed, t: &Theme, cx: &App) -> AnyElement {
    let view = ctx.view.clone();
    // Just a picture: show it like an attachment.
    if e.kind == EmbedKind::Image && e.title.is_none() && e.description.is_none() {
        let Some(image) = e.image.clone() else { return div().into_any_element() };
        let (open, name) = (image.clone(), e.url.clone());
        // A GIF is kept by its picture's own link, which draws anywhere.
        let group = SharedString::from(format!("embed-group-{ix}-{n}"));
        let is_gif = gif_links::is_gif(&image) || gif_links::is_gif(&e.url);
        let star = is_gif.then(|| gif_star(ctx, format!("embed-star-{ix}-{n}"), message_group(ix), image.clone(), t, cx));
        let menu_media = Media::new(image.clone(), image.clone());
        let save = (!is_gif).then(|| download_button(ctx, format!("embed-save-{ix}-{n}"), group.clone(), menu_media.clone(), t));
        let Fitted { source, size } = picture(ctx, &image, f32::MAX);
        return picture_box(div(), size)
            .id(SharedString::from(format!("embed-{ix}-{n}")))
            .group(group)
            .relative()
            .rounded(radius::LG)
            .overflow_hidden()
            .cursor_pointer()
            .on_click(move |_, window, cx| {
                let (open, name) = (open.clone(), name.clone());
                view.update(cx, |v, cx| v.open_lightbox(open, name, window, cx)).ok();
            })
            .child(img(source).id("pic").map(|i| if size.is_some() { i.size_full() } else { i.max_w_full().max_h(px(PICTURE_MAX_H)) }).object_fit(ObjectFit::Contain))
            .children(star)
            .children(save)
            .map(|d| media_menu(d, ctx, ix, menu_media))
            .into_any_element();
    }
    let link = e.url.clone();
    let title = e.title.clone().unwrap_or_else(|| e.url.clone());
    let video = e.video.clone();
    let media = e.image.clone().map(|image| {
        let is_video = video.is_some();
        let target = video.clone().unwrap_or_else(|| image.clone());
        let name = e.url.clone();
        let view = view.clone();
        let Fitted { source, size } = picture(ctx, &image, IMAGE_MAX_W);
        let group = SharedString::from(format!("embed-media-group-{ix}-{n}"));
        let menu_media = Media::new(image.clone(), image.clone());
        let save = (!is_video && !gif_links::is_gif(&image))
            .then(|| download_button(ctx, format!("embed-media-save-{ix}-{n}"), group.clone(), menu_media.clone(), t));
        div()
            .id(SharedString::from(format!("embed-media-{ix}-{n}")))
            .group(group)
            .relative()
            .self_start()
            .mt_2()
            .max_w(px(IMAGE_MAX_W))
            .rounded(radius::MD)
            .overflow_hidden()
            .cursor_pointer()
            .on_click(move |_, window, cx| {
                if is_video {
                    cx.open_url(&target);
                } else {
                    let (open, name) = (target.clone(), name.clone());
                    view.update(cx, |v, cx| v.open_lightbox(open, name, window, cx)).ok();
                }
            })
            .child(
                img(source)
                    .id("pic")
                    .max_w(px(IMAGE_MAX_W))
                    .max_h(px(PICTURE_MAX_H))
                    .when_some(size, |i, s| i.w(s.width).h(s.height))
                    .object_fit(ObjectFit::Contain),
            )
            .when(is_video, |d| {
                d.child(
                    div().absolute().inset_0().flex().items_center().justify_center().child(
                        div()
                            .size(px(48.))
                            .rounded_full()
                            .bg(t.scrim)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon_sized(Icon::Play, px(22.)).text_color(gpui::white())),
                    ),
                )
            })
            .children(save)
            .when(!is_video, |d| media_menu(d, ctx, ix, menu_media))
    });
    div()
        .max_w(px(IMAGE_MAX_W + 32.))
        .min_w_0()
        .flex()
        .rounded(radius::MD)
        .bg(t.bg_panel)
        .overflow_hidden()
        .child(div().w(px(3.)).flex_none().bg(t.accent))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .p_3()
                .min_w_0()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_size(text::XS)
                        .text_color(t.text_faint)
                        .child(icon_sized(Icon::Globe, px(12.)).text_color(t.text_faint))
                        .child(e.site.clone()),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("embed-title-{ix}-{n}")))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.link)
                        .cursor_pointer()
                        .hover(|s| s.text_color(t.link_hover).underline())
                        .on_click(move |_, _, cx| cx.open_url(&link))
                        .child(title),
                )
                .children(e.description.clone().map(|d| {
                    div().text_size(text::SM).text_color(t.text_muted).line_clamp(4).child(d)
                }))
                .children(media),
        )
        .into_any_element()
}

fn is_invite(url: &str) -> bool {
    url.contains("/invite/")
}

fn invite_card(ctx: &RowCtx, ix: usize, url: &str, t: &Theme) -> AnyElement {
    let client = ctx.client.clone();
    let invite = url.to_owned();
    let (server, code) = rhein_store::client::parse_invite(url, None).unwrap_or_default();
    div()
        .max_w(px(IMAGE_MAX_W))
        .min_w_0()
        .p_3()
        .rounded(radius::MD)
        .bg(t.bg_panel)
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .size(px(40.))
                .rounded(radius::LG)
                .bg(t.accent_soft)
                .flex()
                .items_center()
                .justify_center()
                .child(icon(Icon::UserPlus).text_color(t.accent)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().text_size(text::SM).text_color(t.text_faint).child("You've been invited to a guild"))
                .child(div().font_weight(FontWeight::SEMIBOLD).truncate().child(format!("{server} · {code}"))),
        )
        .child(Button::new(SharedString::from(format!("invite-join-{ix}")), "Join guild").primary().on_click(move |_, _, cx| {
            let invite = invite.clone();
            client.update(cx, |c, cx| c.join_invite(invite, None, cx));
        }))
        .into_any_element()
}

fn day_separator(ms: i64, t: &Theme) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_3()
        .px_4()
        .py_1()
        .child(div().flex_1().h(px(1.)).bg(t.border))
        .child(div().font_family(FONT_MONO).text_size(text::XS).text_color(t.text_faint).child(day_label(ms).to_lowercase()))
        .child(div().flex_1().h(px(1.)).bg(t.border))
        .into_any_element()
}

fn new_messages_line(t: &Theme) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .px_4()
        .pt_2()
        .child(div().flex_1().h(px(1.)).bg(t.danger))
        .child(
            div()
                .px_1p5()
                .rounded(px(3.))
                .font_family(FONT_MONO)
                .text_size(text::XS)
                .text_color(t.danger)
                .child("new"),
        )
        .into_any_element()
}

fn zoned(ms: i64) -> Option<jiff::Zoned> {
    jiff::Timestamp::from_millisecond(ms).ok().map(|ts| ts.to_zoned(jiff::tz::TimeZone::system()))
}

fn day_of(ms: i64) -> Option<jiff::civil::Date> {
    zoned(ms).map(|z| z.date())
}

fn day_label(ms: i64) -> String {
    let Some(z) = zoned(ms) else { return String::new() };
    let today = jiff::Zoned::now().date();
    if z.date() == today {
        "Today".into()
    } else if today.yesterday().ok() == Some(z.date()) {
        "Yesterday".into()
    } else {
        z.strftime("%B %-d, %Y").to_string()
    }
}

fn clock(z: &jiff::Zoned, h24: bool) -> String {
    if h24 { z.strftime("%H:%M").to_string() } else { z.strftime("%-I:%M %p").to_string() }
}

pub(crate) fn format_hm(ms: i64, h24: bool) -> String {
    zoned(ms).map(|z| clock(&z, h24)).unwrap_or_default()
}

fn format_time(ms: i64, h24: bool) -> String {
    let Some(z) = zoned(ms) else { return String::new() };
    let today = jiff::Zoned::now().date();
    if z.date() == today {
        format!("Today at {}", clock(&z, h24))
    } else if today.yesterday().ok() == Some(z.date()) {
        format!("Yesterday at {}", clock(&z, h24))
    } else {
        format!("{} {}", z.strftime("%d/%m/%Y"), clock(&z, h24))
    }
}

pub fn format_size(bytes: i64) -> String {
    let b = bytes as f64;
    if b < 1024.0 {
        format!("{bytes} B")
    } else if b < 1024.0 * 1024.0 {
        format!("{:.1} KB", b / 1024.0)
    } else {
        format!("{:.1} MB", b / 1024.0 / 1024.0)
    }
}

/// Identifies a message across the swap from its pending copy to the server's: the nonce when we
/// sent it, the id otherwise.
pub fn message_key(message: &Message) -> &str {
    message.nonce.as_deref().unwrap_or(&message.id)
}

/// What a row's height still waits on: previews and GIF share pages not asked for yet, the
/// pictures whose size is only known once loaded, and a fingerprint of how far the previews
/// have got (it changes whenever one arrives and the row grows).
#[derive(Default)]
pub struct RowMedia {
    pub embeds: Vec<String>,
    pub shares: Vec<String>,
    /// Telium media links whose kind is not known yet.
    pub probes: Vec<String>,
    pub pictures: Vec<String>,
    /// Videos whose posters are not asked for yet: key, content hash, size, source.
    pub posters: Vec<(String, String, i64, MediaSource)>,
    pub progress: u64,
}

/// A message's videos, with their indexes among its attachments.
fn videos(message: &Message) -> impl Iterator<Item = (usize, &Attachment)> {
    message.attachments.iter().enumerate().filter(|(_, a)| rhein_voice::movie::playable(&a.filename, &a.mime))
}

/// A message's guild videos, for fetching their first moments: key and source.
pub fn preloadable(message: &Message) -> impl Iterator<Item = (String, MediaSource)> + '_ {
    videos(message).filter_map(|(i, a)| Some((format!("{}#{i}", message.id), MediaSource::Url(a.url.clone()?))))
}

/// A message's videos whose posters are not asked for yet: key, content hash, size, source.
pub fn missing_posters(client: &Client, channel_id: &str, message: &Message, cx: &App) -> Vec<(String, String, i64, MediaSource)> {
    let posters = client.movie_posters.read(cx);
    videos(message)
        .map(|(i, a)| (i, a, format!("{}#{i}", message.id)))
        .filter(|(_, _, key)| posters.get(key).is_none())
        .map(|(i, a, key)| {
            let source = match &a.url {
                Some(url) => MediaSource::Url(url.clone()),
                None => MediaSource::Dm { conversation: channel_id.to_owned(), message: message.id.clone(), index: i },
            };
            (key, a.hash.clone(), a.size, source)
        })
        .collect()
}

pub fn row_media(client: &Client, channel_id: &str, message: &Message, cache: &ParsedCache, cx: &App) -> RowMedia {
    let parsed = parsed(cache, message);
    use std::hash::{Hash, Hasher};
    let mut media = RowMedia::default();
    let mut progress = std::collections::hash_map::DefaultHasher::new();
    for a in message.attachments.iter().filter(|a| a.is_image()) {
        if let (Some(url), (_, None)) = (&a.url, fit(a.width, a.height)) {
            media.pictures.push(url.clone());
        }
    }
    let posters = client.movie_posters.read(cx);
    for (i, _) in videos(message) {
        // A poster arriving can change the row's height.
        posters.get(&format!("{}#{i}", message.id)).map(Poster::size).hash(&mut progress);
    }
    media.posters = missing_posters(client, channel_id, message, cx);
    let strip_embeds = client.channel(channel_id).is_some_and(|c| c.strip_embeds);
    let links: &[String] = if strip_embeds { &[] } else { &parsed.links };
    for url in links.iter().filter(|u| !is_invite(u) && !gif_links::is_gif(u)) {
        if gif_links::is_telium_media(url) {
            match client.media_kind(url) {
                Some(MediaProbe::Known(MediaKind::Other)) => {}
                Some(MediaProbe::Known(_)) => {
                    5u8.hash(&mut progress);
                    media.pictures.push(url.clone());
                    continue;
                }
                Some(MediaProbe::Loading) => {
                    4u8.hash(&mut progress);
                    continue;
                }
                None => {
                    media.probes.push(url.clone());
                    continue;
                }
            }
        }
        match client.embed(channel_id, url) {
            Some(EmbedState::Ready(embed)) => {
                1u8.hash(&mut progress);
                media.pictures.extend(embed.image.clone());
            }
            Some(EmbedState::Loading) => 2u8.hash(&mut progress),
            Some(EmbedState::Empty { .. }) => 3u8.hash(&mut progress),
            None => media.embeds.push(url.clone()),
        }
    }
    for url in parsed.gifs.iter().cloned() {
        if gif_links::share(&url).is_none() {
            media.pictures.push(url);
            continue;
        }
        match client.gif_share(&url) {
            Some(GifShare::Ready(picture)) => {
                1u8.hash(&mut progress);
                media.pictures.push(picture);
            }
            Some(state) => (state == GifShare::Loading).hash(&mut progress),
            None => media.shares.push(url),
        }
    }
    media.progress = progress.finish();
    media
}
