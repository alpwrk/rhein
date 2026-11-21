//! Encrypted DMs in the client: the engine (`crate::conversations`) runs on the tokio runtime,
//! and each conversation's decrypted history is mirrored into the same logs guild channels use, so
//! the chat list, rows and composer work on DMs unchanged. Sending, reacting, editing and deleting
//! in a DM go to the engine instead of a guild server.

use std::sync::Arc;

use rhein_mls::payload::Payload;
use rhein_proto::{Attachment, Message, Reaction};
use tokio::sync::Mutex;

use super::*;
use crate::conversations::{ConversationState, Engine};
use crate::dm::{DmStatus, Secrets};

pub type SharedEngine = Arc<Mutex<Engine>>;

impl Client {
    /// Whether an id names one of our private conversations.
    pub fn is_dm(&self, id: &str) -> bool {
        self.conversations.iter().any(|c| c.id == id)
    }

    /// Whether a conversation is someone else's message request we have not accepted (pending or
    /// declined). It raises no unread mark, alert, typing or ringing, and we send no typing or
    /// call signalling into it.
    pub fn is_request(&self, id: &str) -> bool {
        let me = self.user_id();
        self.conversations.iter().any(|c| {
            c.id == id && matches!(c.request_state.as_deref(), Some("pending" | "declined")) && c.creator.as_deref() != me
        })
    }

    /// Opens the engine once DM keys are up, and tops up this device's key packages.
    pub(super) fn open_dm_engine(&mut self, status: &DmStatus, cx: &mut Context<Self>) {
        let Some(home) = &self.home else { return };
        let (user, device, api) = (home.user_id.clone(), home.device_id.clone(), home.api.clone());
        let secrets = Secrets::load(&user);
        let Some((private, public)) = secrets.mls_signature.clone() else { return };
        let (Ok(private), Ok(public)) = (rhein_mls::vault::unb64(&private), rhein_mls::vault::unb64(&public)) else { return };
        let engine = match Engine::open(&user, &device, (&private, &public), secrets.keyring.clone()) {
            Ok(engine) => {
                // Group names this device already opened, for the list before anything syncs.
                self.dm_names.extend(engine.group_names());
                Arc::new(Mutex::new(engine))
            }
            Err(err) => {
                self.dm_error = Some(format!("{err:#}"));
                return;
            }
        };
        if status.locked.is_some() {
            log::info!("DM keyring locked: {:?}", status.locked);
        }
        self.dm_engine = Some(engine.clone());
        let io = self.io.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move { engine.lock().await.replenish(&api).await })).await;
            if let Err(err) = result {
                log::warn!("key packages: {err:#}");
            }
            this.update(cx, |this, cx| {
                // Catch up the conversation on screen, if any.
                if let Some(id) = this.home_conversation.clone() {
                    this.sync_dm(id, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Catches a conversation up through the engine and refreshes its log.
    pub fn sync_dm(&mut self, id: String, cx: &mut Context<Self>) {
        let (Some(engine), Some(api)) = (self.dm_engine.clone(), self.home.as_ref().map(|h| h.api.clone())) else {
            let log = self.logs.entry(id).or_default();
            log.loaded = true;
            log.reached_start = true;
            log.error = Some("Encryption isn't available on this device yet.".into());
            cx.notify();
            return;
        };
        let log = self.logs.entry(id.clone()).or_default();
        if log.loading {
            return;
        }
        log.loading = true;
        let io = self.io.clone();
        let conv = id.clone();
        let signals = self.signals_for(&id);
        let log = self.logs.entry(id.clone()).or_default();
        let since = std::time::Instant::now();
        let shown = !log.messages.is_empty();
        log.task = Some(cx.spawn(async move |this, cx| {
            // What this device holds shows at once, before the server is asked what came since.
            if !shown {
                let (engine, conv) = (engine.clone(), conv.clone());
                let local = join(io.spawn(async move { Ok(engine.lock().await.conversation(&conv).cloned()) })).await;
                if let Ok(Some(state)) = local
                    && !state.messages.is_empty()
                {
                    this.update(cx, |this, cx| this.apply_dm(&id, state, cx)).ok();
                }
            }
            let id = id.clone();
            let result = join(io.spawn(async move {
                let mut engine = engine.lock().await;
                let locked = since.elapsed();
                engine.signals = signals;
                engine.sync(&api, &conv).await?;
                log::debug!(target: "rhein::switch", "{conv}: synced in {:?} ({locked:?} waiting for the engine)", since.elapsed());
                Ok(engine.conversation(&conv).cloned().unwrap_or_default())
            }))
            .await;
            this.update(cx, |this, cx| {
                let log = this.logs.entry(id.clone()).or_default();
                log.loading = false;
                log.task = None;
                match result {
                    Ok(state) => this.apply_dm(&id, state, cx),
                    Err(err) => {
                        log.error = Some(friendly(&err));
                        log.loaded = true;
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// Mirrors a conversation's decrypted state into its log.
    fn apply_dm(&mut self, id: &str, state: ConversationState, cx: &mut Context<Self>) {
        match &state.name {
            Some(name) => self.dm_names.insert(id.to_owned(), name.clone()),
            None => self.dm_names.remove(id),
        };
        let me = self.user_id().unwrap_or_default().to_owned();
        match state.seen_upto(&me).filter(|_| self.signal("read", Some(id), false)) {
            Some(seen) => self.dm_seen.insert(id.to_owned(), seen),
            None => self.dm_seen.remove(id),
        };
        let messages: Vec<Message> = state
            .messages
            .iter()
            .filter(|m| !m.deleted)
            .map(|m| Message {
                id: m.id.clone(),
                channel_id: id.to_owned(),
                author: m.sender.clone(),
                content: m.edited.clone().unwrap_or_else(|| m.body.text.clone()),
                attachments: m
                    .body
                    .attachments
                    .iter()
                    .map(|a| Attachment {
                        filename: a["name"].as_str().unwrap_or("file").to_owned(),
                        mime: a["mime"].as_str().unwrap_or("application/octet-stream").to_owned(),
                        size: a["size"].as_i64().unwrap_or(0),
                        // Encrypted on the server: shown as a file, opened through the engine.
                        ..Default::default()
                    })
                    .collect(),
                reply_to: m.body.reply.as_ref().and_then(|r| r["id"].as_str()).map(str::to_owned),
                created_at: m.created_at,
                edited_at: m.edited.as_ref().map(|_| m.created_at),
                reactions: state
                    .reactions
                    .get(&m.id)
                    .map(|by_emoji| {
                        by_emoji
                            .iter()
                            .filter(|(_, users)| !users.is_empty())
                            .map(|(emoji, users)| Reaction { emoji: emoji.clone(), count: users.len() as i64, me: users.contains(&me), users: users.clone() })
                            .collect()
                    })
                    .unwrap_or_default(),
                ..Default::default()
            })
            .collect();
        let log = self.logs.entry(id.to_owned()).or_default();
        let grew = messages.len() > log.messages.len() && log.messages.iter().zip(&messages).all(|(a, b)| a.id == b.id);
        // Opening a conversation syncs it again; most often nothing changed, and starting its
        // list over would measure every row again and lose where it was scrolled to.
        let same = log.loaded && messages.len() == log.messages.len() && log.messages.iter().zip(&messages).all(|(a, b)| shown_alike(a, b));
        let before = log.messages.len();
        log.messages = messages;
        log.loaded = true;
        log.reached_start = true;
        log.error = state.error.clone();
        if grew && before > 0 {
            let fresh: Vec<(String, String)> =
                log.messages[before..].iter().filter(|m| m.author != me).map(|m| (m.author.clone(), m.content.clone())).collect();
            for _ in before..log.messages.len() {
                cx.emit(ClientEvent::MessageAppended { channel_id: id.to_owned() });
            }
            // New words from someone else, in a conversation not in front of you.
            let open = self.home_conversation.as_deref() == Some(id) && self.window_active;
            // A conversation muted here or in the web client (the shared `muted` preference, or
            // the home server's own mute) stays quiet.
            let muted = self.is_muted(id) || self.conversations.iter().any(|c| c.id == id && c.muted);
            if !open && !self.is_request(id) && !muted {
                for (author, text) in fresh {
                    let sender = self.display_name(None, &author);
                    self.alert(notify::Alert::Direct, &sender, None, &text, cx);
                }
            }
        } else if !same {
            cx.emit(ClientEvent::MessagesReset { channel_id: id.to_owned() });
        }
        self.note_dm_read(id, cx);
        cx.notify();
    }

    /// Sends a message in a DM, with the files staged for it (kept on this device and sealed
    /// before they leave it).
    pub(super) fn send_dm(&mut self, id: String, content: String, reply_to: Option<String>, cx: &mut Context<Self>) {
        let files: Vec<(String, String, Vec<u8>)> = self
            .staged
            .remove(&id)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|s| match s.state {
                StagedState::Local(bytes) => Some((s.name, s.mime, (*bytes).clone())),
                _ => None,
            })
            .collect();
        if content.is_empty() && files.is_empty() {
            return;
        }
        let reply = reply_to.and_then(|r| {
            let m = self.find_message(&id, &r)?;
            Some(serde_json::json!({ "id": m.id, "author": m.author, "text": m.content.chars().take(200).collect::<String>() }))
        });
        self.dm_run(id, cx, move |engine, api, conv| Box::pin(async move { engine.send_message(&api, &conv, &content, files, reply).await.map(drop) }));
    }

    /// Sends a signal payload (reaction, edit, delete) in a DM.
    pub(super) fn dm_payload(&mut self, id: String, payload: Payload, cx: &mut Context<Self>) {
        self.dm_run(id, cx, move |engine, api, conv| Box::pin(async move { engine.send(&api, &conv, payload).await.map(drop) }));
    }

    /// Names a group conversation for everyone in it; a blank name clears it.
    pub fn rename_dm(&mut self, id: String, name: String, cx: &mut Context<Self>) {
        self.dm_run(id, cx, move |engine, api, conv| Box::pin(async move { engine.rename(&api, &conv, &name).await.map(drop) }));
    }

    fn dm_run(
        &mut self,
        id: String,
        cx: &mut Context<Self>,
        op: impl for<'a> FnOnce(&'a mut Engine, Api, String) -> std::pin::Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> + Send + 'static,
    ) {
        let (Some(engine), Some(api)) = (self.dm_engine.clone(), self.home.as_ref().map(|h| h.api.clone())) else {
            self.error = Some("Encryption isn't available on this device yet.".into());
            cx.notify();
            return;
        };
        let io = self.io.clone();
        let conv = id.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move {
                let mut engine = engine.lock().await;
                op(&mut engine, api, conv.clone()).await?;
                Ok(engine.conversation(&conv).cloned().unwrap_or_default())
            }))
            .await;
            this.update(cx, |this, cx| match result {
                Ok(state) => this.apply_dm(&id, state, cx),
                Err(err) => {
                    this.error = Some(friendly(&err).into());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}

impl Client {
    /// Opens the one-to-one conversation with someone, starting it (and its group) if there is
    /// none yet.
    pub fn message_user(&mut self, user_id: String, cx: &mut Context<Self>) {
        let me = self.user_id().unwrap_or_default().to_owned();
        let existing = self
            .conversations
            .iter()
            .find(|c| c.kind == "dm" && c.members.iter().any(|m| m.user_id == user_id) && c.members.iter().any(|m| m.user_id == me))
            .map(|c| c.id.clone());
        if let Some(id) = existing {
            self.open_conversation(Some(id), cx);
            return;
        }
        self.start_conversation("dm", vec![user_id], cx);
    }

    /// Starts a group chat with these people.
    pub fn new_group(&mut self, members: Vec<String>, cx: &mut Context<Self>) {
        self.start_conversation("gc", members, cx);
    }

    fn start_conversation(&mut self, kind: &'static str, members: Vec<String>, cx: &mut Context<Self>) {
        let (Some(engine), Some(api)) = (self.dm_engine.clone(), self.home.as_ref().map(|h| h.api.clone())) else {
            self.error = Some("Encryption isn't available on this device yet. Unlock it in Settings › Encryption.".into());
            cx.notify();
            return;
        };
        let me = self.user_id().unwrap_or_default().to_owned();
        let known: std::collections::HashSet<String> = self.conversations.iter().map(|c| c.id.clone()).collect();
        let io = self.io.clone();
        cx.spawn(async move |this, cx| {
            let result = join(io.spawn(async move {
                let convo = api.create_conversation(kind, &members, None).await?;
                let id = convo["id"].as_str().unwrap_or_default().to_owned();
                // A one-to-one we already have comes back as that one; its group exists.
                let mut claimed = None;
                if !known.contains(&id) {
                    let mut all = members.clone();
                    all.push(me);
                    claimed = Some(engine.lock().await.establish(&api, &id, &all, None).await?);
                }
                Ok((id, claimed))
            }))
            .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok((id, claimed)) => {
                        if let Some(c) = claimed.filter(|c| !c.missing.is_empty() || !c.refused.is_empty()) {
                            let mut parts = Vec::new();
                            if !c.missing.is_empty() {
                                parts.push(format!("{} haven't signed in since keys were needed", c.missing.len()));
                            }
                            if !c.refused.is_empty() {
                                parts.push(format!("{} weren't vouched for by their identity", c.refused.len()));
                            }
                            this.notice = Some(format!("Some devices couldn't be added: {}.", parts.join("; ")).into());
                        }
                        this.load_conversations(cx);
                        this.open_conversation(Some(id.clone()), cx);
                        // Opened to call them (`call_user`): ring now.
                        if this.call_when_open.take().is_some() {
                            this.join_dm_call(id, cx);
                        }
                    }
                    Err(err) => this.error = Some(friendly(&err).into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Accepts or declines a message request.
    pub fn answer_request(&mut self, id: String, accept: bool, cx: &mut Context<Self>) {
        let Some(api) = self.home.as_ref().map(|h| h.api.clone()) else { return };
        let io = self.io.clone();
        cx.spawn(async move |this, cx| {
            let conv = id.clone();
            let result = join(io.spawn(async move {
                if accept { api.accept_message_request(&conv).await } else { api.decline_message_request(&conv).await }
                    .map_err(anyhow::Error::from)
            }))
            .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(_) if !accept => {
                        this.home_conversation = None;
                    }
                    Ok(_) => this.sync_dm(id, cx),
                    Err(err) => this.error = Some(friendly(&err).into()),
                }
                this.load_conversations(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// Whether a message of a conversation shows the same as it did.
fn shown_alike(a: &Message, b: &Message) -> bool {
    let reactions = |m: &Message| m.reactions.iter().map(|r| (r.emoji.clone(), r.count, r.me)).collect::<Vec<_>>();
    a.id == b.id
        && a.content == b.content
        && a.edited_at == b.edited_at
        && a.reply_to == b.reply_to
        && a.attachments.len() == b.attachments.len()
        && reactions(a) == reactions(b)
}
