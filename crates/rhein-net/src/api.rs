use std::sync::Arc;

use rhein_proto::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::RwLock;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{status}: {}", .body.message.as_deref().unwrap_or(&.body.error))]
    Status { status: u16, body: ErrorBody },
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

impl ApiError {
    pub fn status(&self) -> Option<u16> {
        match self {
            ApiError::Status { status, .. } => Some(*status),
            ApiError::Http(e) => e.status().map(|s| s.as_u16()),
        }
    }

    pub fn code(&self) -> Option<&str> {
        match self {
            ApiError::Status { body, .. } => body.code.as_deref(),
            ApiError::Http(_) => None,
        }
    }

    /// Whether the server answered with this error code (codes are stable; the English message
    /// is not, so branching goes by code, falling back to the error class).
    pub fn is(&self, code: &str) -> bool {
        match self {
            ApiError::Status { body, .. } => {
                body.code.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(code)) || body.error.eq_ignore_ascii_case(code)
            }
            ApiError::Http(_) => false,
        }
    }

    /// A failure that says nothing about the request itself: the network, or an upstream
    /// server that did not answer (502, 503, 504). Safe requests may be repeated.
    pub fn is_transient(&self) -> bool {
        match self {
            ApiError::Http(e) => e.is_connect() || e.is_timeout() || e.is_request(),
            ApiError::Status { status, .. } => matches!(status, 502 | 504) || (*status == 503 && !self.is("standby_read_only")),
        }
    }

    /// For a rate-limited request, how long to wait before repeating it.
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            ApiError::Status { status: 429, body } => Some(std::time::Duration::from_secs_f64(body.retry_after.unwrap_or(1.).clamp(0.1, 10.))),
            _ => None,
        }
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// One server's REST surface under one bearer token. Cheap to clone; the token can be swapped in
/// place when a guild session is renewed, so every clone sees the new one.
#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    base: Arc<str>,
    token: Arc<RwLock<Option<String>>>,
    /// How a guild session is renewed when it runs out (shared by every clone).
    renewal: Arc<Renewal>,
}

/// A guild session's renewal, as the web client does it: the same handshake as the first time
/// (an assertion from the home server, then `POST /sessions`), never a new sign-in.
#[derive(Default)]
struct Renewal {
    /// The home server and the audience its assertions name.
    from: std::sync::Mutex<Option<(Api, String)>>,
    /// One renewal at a time; the others wait for it and use its token.
    running: tokio::sync::Mutex<()>,
    /// Counts renewals, so a request refused on an older token only retries.
    generation: std::sync::atomic::AtomicU64,
    /// When the current session ends (ms since the epoch), if known.
    expires_at: std::sync::atomic::AtomicI64,
}

impl Api {
    pub fn new(http: reqwest::Client, base: &str, token: Option<String>) -> Self {
        Self {
            http,
            base: base.trim_end_matches('/').into(),
            token: Arc::new(RwLock::new(token)),
            renewal: Arc::default(),
        }
    }

    /// Makes this a guild session that renews itself from `home` when the server refuses it
    /// (and ahead of time, `renew_if_expiring`).
    pub fn renew_from(&self, home: &Api, audience: &str) {
        *self.renewal.from.lock().unwrap_or_else(|e| e.into_inner()) = Some((home.clone(), audience.to_owned()));
    }

    /// Records when the current session ends.
    pub fn set_expires_at(&self, at: Option<i64>) {
        self.renewal.expires_at.store(at.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn expires_at(&self) -> Option<i64> {
        Some(self.renewal.expires_at.load(std::sync::atomic::Ordering::Relaxed)).filter(|at| *at > 0)
    }

    /// Renews the session if it ends within `margin`. Returns the new token when it did.
    pub async fn renew_if_expiring(&self, margin: std::time::Duration) -> ApiResult<Option<String>> {
        let Some(at) = self.expires_at() else { return Ok(None) };
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        if at - now > margin.as_millis() as i64 {
            return Ok(None);
        }
        let seen = self.renewal.generation.load(std::sync::atomic::Ordering::Acquire);
        self.renew_after(seen).await
    }

    fn renews(&self) -> bool {
        self.renewal.from.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    /// Renews the session unless another renewal already did since `seen`. Returns the new
    /// token when this call renewed.
    async fn renew_after(&self, seen: u64) -> ApiResult<Option<String>> {
        use std::sync::atomic::Ordering;
        let _one = self.renewal.running.lock().await;
        if self.renewal.generation.load(Ordering::Acquire) != seen {
            return Ok(None);
        }
        let Some((home, audience)) = self.renewal.from.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return Ok(None) };
        // Both straight to the servers (the home session is not a renewable one either way).
        let ask = home.http.post(home.url("/assertions")).json(&AssertionRequest { audience: &audience });
        let assertion: AssertionResponse = home.send_once(ask).await?;
        let req = self.http.post(self.url("/sessions")).json(&SessionRequest { assertion: &assertion.assertion });
        let session: GuildSession = self.send_once(req).await?;
        self.set_token(session.session_token.clone()).await;
        self.set_expires_at(session.expires_at);
        self.renewal.generation.fetch_add(1, Ordering::Release);
        log::info!("renewed the session on {}", self.base);
        Ok(Some(session.session_token))
    }

    /// The bearer token this API sends, if signed in.
    pub async fn bearer(&self) -> Option<String> {
        self.token.read().await.clone()
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub async fn set_token(&self, token: String) {
        *self.token.write().await = Some(token);
    }

    pub async fn token(&self) -> Option<String> {
        self.token.read().await.clone()
    }

    fn url(&self, path: &str) -> String {
        format!("{}/_telium/v1{}", self.base, path)
    }

    /// Sends, and when a renewable session is refused (it ran out), renews it once and sends
    /// the same request again.
    ///
    /// Safe requests (GET, PUT, DELETE) are also repeated after a network failure or a 502, 503
    /// or 504, and any request after `409 try_again` (the server did nothing with it). A POST is
    /// never repeated otherwise: channel messages carry a nonce, and DM frames go out once.
    async fn send<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> ApiResult<T> {
        let safe = req
            .try_clone()
            .and_then(|r| r.build().ok())
            .is_some_and(|r| matches!(*r.method(), reqwest::Method::GET | reqwest::Method::PUT | reqwest::Method::DELETE));
        let (mut req, mut tries) = (req, 0u32);
        loop {
            let retry = req.try_clone();
            let result = self.send_renewing(req).await;
            let Err(err) = &result else { return result };
            let again = (safe && err.is_transient() && tries < 2) || (err.is("try_again") && tries < 1);
            match retry {
                Some(retry) if again => {
                    tries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(500 * u64::from(tries))).await;
                    req = retry;
                }
                _ => return result,
            }
        }
    }

    async fn send_renewing<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> ApiResult<T> {
        let again = self.renews().then(|| req.try_clone()).flatten();
        let seen = self.renewal.generation.load(std::sync::atomic::Ordering::Acquire);
        let result = self.send_once(req).await;
        match (result, again) {
            (Err(err), Some(again)) if err.status() == Some(401) => {
                if self.renew_after(seen).await.is_err() {
                    return Err(err);
                }
                self.send_once(again).await
            }
            (result, _) => result,
        }
    }

    async fn send_once<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> ApiResult<T> {
        let req = match self.token.read().await.as_deref() {
            Some(token) => req.bearer_auth(token),
            None => req,
        };
        let resp = req.send().await?;
        let status = resp.status();
        if status.is_success() {
            // Some routes answer 204 or an empty body; decode that as JSON null.
            let bytes = resp.bytes().await?;
            let text = if bytes.is_empty() { &b"null"[..] } else { &bytes[..] };
            return serde_json::from_slice(text).map_err(|e| ApiError::Status {
                status: status.as_u16(),
                body: ErrorBody { error: "decode".into(), code: None, message: Some(e.to_string()), retry_after: None },
            });
        }
        let body = resp.json::<ErrorBody>().await.unwrap_or_else(|_| ErrorBody {
            error: status.canonical_reason().unwrap_or("error").into(),
            ..Default::default()
        });
        Err(ApiError::Status { status: status.as_u16(), body })
    }

    /// A GET whose answer is kept as it came (a download): the bytes and their content type.
    pub async fn get_raw(&self, path: &str) -> ApiResult<(Vec<u8>, Option<String>)> {
        let mut req = self.http.get(self.url(path));
        if let Some(token) = self.token.read().await.as_deref() {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.json::<ErrorBody>().await.unwrap_or_else(|_| ErrorBody { error: status.canonical_reason().unwrap_or("error").into(), ..Default::default() });
            return Err(ApiError::Status { status: status.as_u16(), body });
        }
        let kind = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_owned);
        Ok((resp.bytes().await?.to_vec(), kind))
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> ApiResult<T> {
        self.send(self.http.get(self.url(path))).await
    }

    pub async fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> ApiResult<T> {
        self.send(self.http.post(self.url(path)).json(body)).await
    }

    pub async fn patch<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> ApiResult<T> {
        self.send(self.http.patch(self.url(path)).json(body)).await
    }

    pub async fn put_empty(&self, path: &str) -> ApiResult<serde_json::Value> {
        self.send(self.http.put(self.url(path))).await
    }

    pub async fn delete(&self, path: &str) -> ApiResult<serde_json::Value> {
        self.send(self.http.delete(self.url(path))).await
    }

    pub async fn put<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> ApiResult<T> {
        self.send(self.http.put(self.url(path)).json(body)).await
    }

    pub async fn delete_json<B: Serialize>(&self, path: &str, body: &B) -> ApiResult<serde_json::Value> {
        self.send(self.http.delete(self.url(path)).json(body)).await
    }

    // ---- the account's security (home server); `proof` is `{reauth_token}` from a fresh
    // OPAQUE reauthentication (`auth::reauth_token`) ----

    /// `{enabled, setup_pending, recovery_codes_remaining, reenrol_required}`.
    pub async fn mfa_status(&self) -> ApiResult<serde_json::Value> {
        self.get("/mfa").await
    }

    /// Starts two-factor setup: `{secret, otpauth_uri}`.
    pub async fn mfa_setup(&self, proof: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.post("/mfa/setup", proof).await
    }

    /// Turns two-factor sign-in on with a first code: `{ok, recovery_codes}`.
    pub async fn mfa_enable(&self, code: &str) -> ApiResult<serde_json::Value> {
        self.post("/mfa/enable", &serde_json::json!({ "code": code })).await
    }

    pub async fn mfa_disable(&self, proof: &serde_json::Value, code: &str) -> ApiResult<serde_json::Value> {
        let mut body = proof.clone();
        body["code"] = code.into();
        self.post("/mfa/disable", &body).await
    }

    /// `{state, verified_email, pending_email, mailer, …}`.
    pub async fn recovery_email(&self) -> ApiResult<serde_json::Value> {
        self.get("/account/recovery-email").await
    }

    /// Sets (or with `None` removes) the recovery address; a new one waits for confirmation.
    pub async fn set_recovery_email(&self, proof: &serde_json::Value, email: Option<&str>) -> ApiResult<serde_json::Value> {
        let mut body = proof.clone();
        body["email"] = email.map_or(serde_json::Value::Null, Into::into);
        self.put("/account/recovery-email", &body).await
    }

    /// Ends every session of the account, this one included.
    pub async fn logout_all(&self) -> ApiResult<serde_json::Value> {
        self.post("/logout-all", &serde_json::json!({})).await
    }

    pub async fn deactivate_account(&self, proof: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.post("/account/deactivate", proof).await
    }

    pub async fn delete_account(&self, proof: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.delete_json("/account", proof).await
    }

    // ---- encrypted DMs: keys, trust and frames (home server) ----

    /// The recovery-keyring vault, sealed (`{exists, mode, ciphertext, …}`).
    pub async fn vault(&self) -> ApiResult<serde_json::Value> {
        self.get("/vault").await
    }

    pub async fn put_vault(&self, body: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.put("/vault", body).await
    }

    /// This account's cross-signing row: identity key, sealed secret, vouched devices.
    pub async fn cross_signing(&self) -> ApiResult<serde_json::Value> {
        self.get("/crosssigning").await
    }

    pub async fn sign_device(&self, device_id: &str, mls_key: &str, signature: &str) -> ApiResult<serde_json::Value> {
        let body = serde_json::json!({ "mls_key": mls_key, "signature": signature });
        self.put(&format!("/crosssigning/devices/{}", urlencoding::encode(device_id)), &body).await
    }

    /// This account's own identity-continuity document.
    pub async fn own_identity(&self) -> ApiResult<serde_json::Value> {
        self.get("/identity").await
    }

    pub async fn anchor_identity(&self, immutable_id: &str, identity_key: &str, signature: &str) -> ApiResult<serde_json::Value> {
        let body = serde_json::json!({ "immutable_id": immutable_id, "identity_key": identity_key, "signature": signature });
        self.put("/identity/anchor", &body).await
    }

    /// Anyone's identity-continuity document, as their home server publishes it.
    pub async fn identity_document(&self, user_id: &str) -> ApiResult<serde_json::Value> {
        self.get(&format!("/users/{}/identity", urlencoding::encode(user_id))).await
    }

    /// Anyone's device list with their cross-signatures.
    pub async fn device_list(&self, user_id: &str) -> ApiResult<serde_json::Value> {
        self.get(&format!("/users/{}/devices", urlencoding::encode(user_id))).await
    }

    /// How many key packages each of this account's devices has left.
    pub async fn key_packages(&self) -> ApiResult<serde_json::Value> {
        self.get("/keypackages").await
    }

    pub async fn publish_key_packages(&self, packages: &[String]) -> ApiResult<serde_json::Value> {
        self.post("/keypackages", &serde_json::json!({ "packages": packages })).await
    }

    /// One key package for each of a user's devices (consumed on the server).
    pub async fn claim_key_packages(&self, user_id: &str) -> ApiResult<serde_json::Value> {
        self.post(&format!("/users/{}/keypackages/claim", urlencoding::encode(user_id)), &serde_json::json!({})).await
    }

    /// Starts a conversation (`kind` is `dm` or `gc`); a DM with someone we already have one with
    /// comes back as that one.
    pub async fn create_conversation(&self, kind: &str, members: &[String], policy: Option<&serde_json::Value>) -> ApiResult<serde_json::Value> {
        let mut body = serde_json::json!({ "kind": kind, "members": members });
        if let Some(policy) = policy {
            body["policy"] = policy.clone();
        }
        self.post("/conversations", &body).await
    }

    pub async fn accept_message_request(&self, conversation_id: &str) -> ApiResult<serde_json::Value> {
        self.post(&format!("/conversations/{}/request/accept", urlencoding::encode(conversation_id)), &serde_json::json!({})).await
    }

    pub async fn decline_message_request(&self, conversation_id: &str) -> ApiResult<serde_json::Value> {
        self.post(&format!("/conversations/{}/request/decline", urlencoding::encode(conversation_id)), &serde_json::json!({})).await
    }

    /// One page of the sealed history mirror for a conversation (`scope`).
    pub async fn backup_page(&self, scope: &str, after: Option<&str>) -> ApiResult<serde_json::Value> {
        let mut path = format!("/vault/backup?scope={}", urlencoding::encode(scope));
        if let Some(after) = after {
            path.push_str(&format!("&after={}", urlencoding::encode(after)));
        }
        self.get(&path).await
    }

    /// Stores sealed mirror rows `{scope, item, ciphertext}`.
    pub async fn put_backup(&self, items: &[serde_json::Value]) -> ApiResult<serde_json::Value> {
        self.put("/vault/backup", &serde_json::json!({ "items": items })).await
    }

    /// Frames of a conversation after `after` (seq), oldest first.
    pub async fn dm_frames(&self, conversation_id: &str, after: Option<u64>, limit: u32) -> ApiResult<serde_json::Value> {
        let mut path = format!("/conversations/{}/messages?limit={limit}", urlencoding::encode(conversation_id));
        if let Some(after) = after {
            path.push_str(&format!("&after={after}"));
        }
        self.get(&path).await
    }

    pub async fn send_frame(&self, conversation_id: &str, frame: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.post(&format!("/conversations/{}/messages", urlencoding::encode(conversation_id)), frame).await
    }

    // ---- home server ----

    // --- guild voice (`server/src/routes/voice.rs`) ---

    /// Joins a guild voice channel. `key_package` (base64) is this device's offer for the call's
    /// MLS group; an SFU channel refuses a join without one.
    pub async fn voice_join(&self, channel_id: &str, self_mute: bool, self_deaf: bool, self_video: bool, region: Option<&str>, key_package: Option<&str>) -> ApiResult<VoiceJoin> {
        let mut body = serde_json::json!({ "self_mute": self_mute, "self_deaf": self_deaf, "self_video": self_video });
        if let Some(region) = region {
            body["region"] = region.into();
        }
        if let Some(package) = key_package {
            body["key_package"] = package.into();
        }
        self.post(&format!("/channels/{}/voice/join", urlencoding::encode(channel_id)), &body).await
    }

    pub async fn voice_leave(&self, channel_id: &str) -> ApiResult<serde_json::Value> {
        self.post(&format!("/channels/{}/voice/leave", urlencoding::encode(channel_id)), &serde_json::json!({})).await
    }

    /// Changes this user's self flags; `None` leaves one as it is.
    pub async fn voice_state(&self, channel_id: &str, self_mute: Option<bool>, self_deaf: Option<bool>, self_video: Option<bool>) -> ApiResult<VoiceState> {
        let mut body = serde_json::json!({});
        for (key, value) in [("self_mute", self_mute), ("self_deaf", self_deaf), ("self_video", self_video)] {
            if let Some(value) = value {
                body[key] = value.into();
            }
        }
        self.post(&format!("/channels/{}/voice/state", urlencoding::encode(channel_id)), &body).await
    }

    /// A fresh `voice_server` for a call that is still up (the SFU token lasts minutes).
    pub async fn voice_token(&self, channel_id: &str) -> ApiResult<VoiceServer> {
        self.post(&format!("/channels/{}/voice/token", urlencoding::encode(channel_id)), &serde_json::json!({})).await
    }

    /// One mesh relay payload for another participant. A burst of ICE candidates runs into the
    /// write rate limit, and a dropped candidate can cost the connection, so a rate-limited
    /// signal waits and goes again.
    pub async fn voice_signal(&self, channel_id: &str, target: &str, payload: &serde_json::Value) -> ApiResult<serde_json::Value> {
        let body = serde_json::json!({ "target": target, "payload": payload });
        let path = format!("/channels/{}/voice/signal", urlencoding::encode(channel_id));
        self.post_patiently(&path, &body).await
    }

    /// `post`, repeated while the server says to wait (a few times at most).
    pub async fn post_patiently<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> ApiResult<T> {
        let mut tries = 0;
        loop {
            match self.post(path, body).await {
                Err(err) if tries < 8 => match err.retry_after() {
                    Some(wait) => {
                        tries += 1;
                        tokio::time::sleep(wait).await;
                    }
                    None => return Err(err),
                },
                other => return other,
            }
        }
    }

    /// Starts or stops a screen share; `preset` describes it ("detail 1080p@15").
    pub async fn voice_stream(&self, channel_id: &str, enabled: bool, preset: Option<&str>) -> ApiResult<serde_json::Value> {
        let body = serde_json::json!({ "enabled": enabled, "preset": preset });
        self.post(&format!("/channels/{}/voice/stream", urlencoding::encode(channel_id)), &body).await
    }

    /// Starts or stops watching someone's screen share (on an SFU the server has it forwarded).
    pub async fn voice_watch(&self, channel_id: &str, streamer: &str, watching: bool, quality: Option<&str>) -> ApiResult<serde_json::Value> {
        let body = serde_json::json!({ "streamer": streamer, "watching": watching, "quality": quality });
        self.post(&format!("/channels/{}/voice/watch", urlencoding::encode(channel_id)), &body).await
    }

    /// A guild call's MLS group for its committer: `{group_id, epoch, participants: [{user_id,
    /// device_id, member, key_package}]}`.
    pub async fn voice_e2ee(&self, channel_id: &str) -> ApiResult<serde_json::Value> {
        self.get(&format!("/channels/{}/voice/e2ee", urlencoding::encode(channel_id))).await
    }

    /// Posts a commit for the call's group; a 409 means another commit took this epoch.
    pub async fn voice_e2ee_commit(&self, channel_id: &str, body: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.post(&format!("/channels/{}/voice/e2ee", urlencoding::encode(channel_id)), body).await
    }

    /// The call group's commit log since `since`: `{group_id, epoch, commits}`, for a device that
    /// fell behind and is catching up.
    pub async fn voice_e2ee_commits(&self, channel_id: &str, since: u64) -> ApiResult<serde_json::Value> {
        self.get(&format!("/channels/{}/voice/e2ee/commits?since={since}", urlencoding::encode(channel_id))).await
    }

    /// A member asking for the call's encryption group to be re-founded: every device drops back
    /// to waiting and the founding member rebuilds from the packages on file. The server refuses
    /// (409) while the group is re-keying happily.
    pub async fn voice_e2ee_reset(&self, channel_id: &str) -> ApiResult<serde_json::Value> {
        self.post(&format!("/channels/{}/voice/e2ee/reset", urlencoding::encode(channel_id)), &serde_json::json!({})).await
    }

    /// Who is in a voice channel: `{channel_id, voice_states, watchers}`.
    pub async fn voice_channel(&self, channel_id: &str) -> ApiResult<serde_json::Value> {
        self.get(&format!("/channels/{}/voice", urlencoding::encode(channel_id))).await
    }

    // --- the account's sync store (`docs/sync-encryption.md`) ---

    /// Every key of the sync store, sealed values included: `{key: {value, updated_at}}`.
    pub async fn sync_all(&self) -> ApiResult<serde_json::Value> {
        self.get("/sync?sealed=1").await
    }

    /// Writes one key (a sealed envelope).
    pub async fn sync_put(&self, key: &str, value: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.put(&format!("/sync/{}", urlencoding::encode(key)), value).await
    }

    // --- conversation calls, on the home server (`server/src/routes/dmvoice.rs`) ---

    /// Joins a conversation's call. `wants_sfu` is the one bit the sequencer learns about our
    /// posture; `custom_sfu` says the group names its own SFU.
    pub async fn conversation_voice_join(&self, id: &str, self_mute: bool, self_deaf: bool, wants_sfu: bool, custom_sfu: bool) -> ApiResult<VoiceJoin> {
        let body = serde_json::json!({
            "self_mute": self_mute, "self_deaf": self_deaf, "self_video": false,
            "wants_sfu": wants_sfu, "custom_sfu": custom_sfu,
        });
        self.post(&format!("/conversations/{}/voice/join", urlencoding::encode(id)), &body).await
    }

    pub async fn conversation_voice_leave(&self, id: &str) -> ApiResult<serde_json::Value> {
        self.post(&format!("/conversations/{}/voice/leave", urlencoding::encode(id)), &serde_json::json!({})).await
    }

    /// Changes flags (`self_mute`, `self_deaf`, `self_video`, `streaming`, `wants_sfu`).
    pub async fn conversation_voice_state(&self, id: &str, patch: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.post(&format!("/conversations/{}/voice/state", urlencoding::encode(id)), patch).await
    }

    /// One relay payload (a mesh signal or a hello) for another participant.
    pub async fn conversation_voice_signal(&self, id: &str, target: &str, payload: &serde_json::Value) -> ApiResult<serde_json::Value> {
        let body = serde_json::json!({ "target": target, "payload": payload });
        self.post_patiently(&format!("/conversations/{}/voice/signal", urlencoding::encode(id)), &body).await
    }

    /// Where the call is now (after a switch to an SFU): `{voice_server}`.
    pub async fn conversation_voice_server(&self, id: &str) -> ApiResult<serde_json::Value> {
        self.post(&format!("/conversations/{}/voice/server", urlencoding::encode(id)), &serde_json::json!({})).await
    }

    /// Who is in a conversation's call.
    pub async fn conversation_voice(&self, id: &str) -> ApiResult<serde_json::Value> {
        self.get(&format!("/conversations/{}/voice", urlencoding::encode(id))).await
    }

    /// The server's SFU regions: `{default, regions: [{name, sfu, healthy, ...}], ...}`.
    pub async fn voice_regions(&self) -> ApiResult<serde_json::Value> {
        self.get("/voice/regions").await
    }

    pub async fn capabilities(&self) -> ApiResult<Capabilities> {
        self.get("/capabilities").await
    }

    pub async fn whoami(&self) -> ApiResult<WhoAmI> {
        self.get("/whoami").await
    }

    pub async fn assertion(&self, audience: &str) -> ApiResult<AssertionResponse> {
        self.post("/assertions", &AssertionRequest { audience }).await
    }

    /// The friends you share with someone (asked on your home server).
    pub async fn mutual_friends(&self, user_id: &str) -> ApiResult<serde_json::Value> {
        self.get(&format!("/friends/mutual/{}", urlencoding::encode(user_id))).await
    }

    pub async fn profile(&self, user_id: &str) -> ApiResult<Profile> {
        self.get(&format!("/users/{}/profile", urlencoding::encode(user_id))).await
    }

    /// Uploads to this (home) server's media origin.
    pub async fn upload(&self, filename: &str, mime: &str, bytes: Vec<u8>) -> ApiResult<Attachment> {
        let req = self
            .http
            .post(self.url("/media"))
            .header("Content-Type", mime)
            .header("X-Telium-Filename", urlencoding::encode(filename).into_owned())
            .header("X-Telium-Purpose", "attachment")
            .body(bytes);
        self.send(req).await
    }

    /// Uploads a profile picture; the result's `hash?cap=capability` becomes the `avatar_ref`.
    pub async fn upload_profile_media(&self, filename: &str, mime: &str, bytes: Vec<u8>) -> ApiResult<Attachment> {
        let req = self
            .http
            .post(self.url("/media"))
            .header("Content-Type", mime)
            .header("X-Telium-Filename", urlencoding::encode(filename).into_owned())
            .header("X-Telium-Purpose", "profile")
            .body(bytes);
        self.send(req).await
    }

    /// Uploads a guild's own picture (its icon) to that guild's server: a voucher from the guild
    /// for `origin` (the guild's server name), then the bytes with it. The result's
    /// `hash?cap=capability` becomes the `icon_ref`.
    pub async fn upload_guild_media(&self, guild_id: &str, origin: &str, filename: &str, mime: &str, bytes: Vec<u8>) -> ApiResult<Attachment> {
        use base64::Engine as _;
        let answer: serde_json::Value = self
            .post("/media/vouchers", &serde_json::json!({ "guild_id": guild_id, "origin": origin, "max_bytes": bytes.len() }))
            .await?;
        let voucher = serde_json::to_vec(&answer["voucher"]).unwrap_or_default();
        let req = self
            .http
            .post(self.url("/media/voucher-upload"))
            .header("Content-Type", mime)
            .header("X-Telium-Filename", urlencoding::encode(filename).into_owned())
            .header("X-Telium-Voucher", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(voucher))
            .header("X-Telium-Purpose", "profile")
            .body(bytes);
        self.send(req).await
    }

    pub async fn patch_profile(&self, patch: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.patch("/profile", patch).await
    }

    pub async fn devices(&self) -> ApiResult<Vec<Device>> {
        Ok(self.get::<DeviceList>("/devices").await?.devices)
    }

    /// Ends a device's sessions. With `signed` (`{mls_key, signature}` by the account root) the
    /// server also retires that MLS key everywhere and publishes the statement.
    pub async fn revoke_device(&self, device_id: &str, signed: Option<&serde_json::Value>) -> ApiResult<serde_json::Value> {
        let path = format!("/devices/{}", urlencoding::encode(device_id));
        match signed {
            Some(body) => self.delete_json(&path, body).await,
            None => self.delete(&path).await,
        }
    }

    /// Ends this session on the server.
    pub async fn logout(&self) -> ApiResult<serde_json::Value> {
        self.post("/logout", &serde_json::json!({})).await
    }

    pub async fn friends(&self) -> ApiResult<Vec<Friend>> {
        Ok(self.get::<FriendList>("/friends").await?.friends)
    }

    pub async fn request_friend(&self, user_id: &str) -> ApiResult<serde_json::Value> {
        self.post("/friends", &serde_json::json!({ "user_id": user_id })).await
    }

    pub async fn accept_friend(&self, user_id: &str) -> ApiResult<serde_json::Value> {
        self.post(&format!("/friends/{}/accept", urlencoding::encode(user_id)), &serde_json::json!({})).await
    }

    /// Removes a friend, declines an incoming request or cancels an outgoing one.
    pub async fn remove_friend(&self, user_id: &str) -> ApiResult<serde_json::Value> {
        self.delete(&format!("/friends/{}", urlencoding::encode(user_id))).await
    }

    pub async fn set_blocked(&self, user_id: &str, blocked: bool) -> ApiResult<serde_json::Value> {
        let path = format!("/friends/{}/block", urlencoding::encode(user_id));
        if blocked { self.put_empty(&path).await } else { self.delete(&path).await }
    }

    pub async fn conversations(&self) -> ApiResult<Vec<Conversation>> {
        Ok(self.get::<ConversationList>("/conversations").await?.conversations)
    }

    // ---- guild server ----

    pub async fn create_session(&self, assertion: &str) -> ApiResult<GuildSession> {
        self.post("/sessions", &SessionRequest { assertion }).await
    }

    pub async fn local_session(&self) -> ApiResult<GuildSession> {
        self.post("/sessions/local", &serde_json::json!({})).await
    }

    pub async fn guilds(&self) -> ApiResult<Vec<Guild>> {
        Ok(self.get::<GuildList>("/guilds").await?.guilds)
    }

    pub async fn create_guild(&self, name: &str) -> ApiResult<Guild> {
        self.post("/guilds", &CreateGuild { name }).await
    }

    pub async fn channels(&self, guild_id: &str) -> ApiResult<Vec<Channel>> {
        Ok(self.get::<ChannelList>(&format!("/guilds/{guild_id}/channels")).await?.channels)
    }

    pub async fn messages(&self, channel_id: &str, before: Option<&str>, limit: u32) -> ApiResult<Vec<Message>> {
        let mut path = format!("/channels/{channel_id}/messages?limit={limit}");
        if let Some(before) = before {
            path.push_str("&before=");
            path.push_str(before);
        }
        Ok(self.get::<MessageList>(&path).await?.messages)
    }

    /// Uploads raw bytes (a pasted image, for example) to the home media origin.
    pub async fn upload_attachment(&self, filename: &str, mime: &str, bytes: Vec<u8>, channel_id: &str, guild_id: &str) -> ApiResult<Attachment> {
        let req = self
            .http
            .post(self.url("/media"))
            .header("Content-Type", mime)
            .header("X-Telium-Filename", urlencoding::encode(filename).into_owned())
            .header("X-Telium-Purpose", "attachment")
            .header("X-Telium-Channel-Id", channel_id)
            .header("X-Telium-Guild-Id", guild_id)
            .body(bytes);
        self.send(req).await
    }

    pub async fn send_message(&self, channel_id: &str, body: &SendMessage) -> ApiResult<Message> {
        self.post(&format!("/channels/{channel_id}/messages"), body).await
    }

    pub async fn edit_message(&self, channel_id: &str, message_id: &str, content: &str) -> ApiResult<serde_json::Value> {
        self.patch(&format!("/channels/{channel_id}/messages/{message_id}"), &EditMessage { content }).await
    }

    pub async fn delete_message(&self, channel_id: &str, message_id: &str) -> ApiResult<serde_json::Value> {
        self.delete(&format!("/channels/{channel_id}/messages/{message_id}")).await
    }

    pub async fn react(&self, message_id: &str, emoji: &str, add: bool) -> ApiResult<serde_json::Value> {
        let path = format!("/messages/{message_id}/reactions/{}/@me", urlencoding::encode(emoji));
        if add { self.put_empty(&path).await } else { self.delete(&path).await }
    }

    pub async fn ack(&self, channel_id: &str, message_id: &str) -> ApiResult<serde_json::Value> {
        self.post(&format!("/channels/{channel_id}/ack"), &Ack { message_id }).await
    }

    /// The guild server's unfurl of a link (`GET /embed?url=`), as raw JSON.
    pub async fn embed(&self, url: &str) -> ApiResult<serde_json::Value> {
        self.get(&format!("/embed?url={}", urlencoding::encode(url))).await
    }

    pub async fn pins(&self, channel_id: &str) -> ApiResult<Vec<Message>> {
        Ok(self.get::<PinList>(&format!("/channels/{channel_id}/pins")).await?.pins)
    }

    pub async fn set_pinned(&self, channel_id: &str, message_id: &str, pinned: bool) -> ApiResult<serde_json::Value> {
        let path = format!("/channels/{channel_id}/pins/{message_id}");
        if pinned { self.put_empty(&path).await } else { self.delete(&path).await }
    }

    pub async fn threads(&self, channel_id: &str, archived: bool) -> ApiResult<Vec<Thread>> {
        Ok(self.get::<ThreadList>(&format!("/channels/{channel_id}/threads?archived={archived}")).await?.threads)
    }

    pub async fn create_thread(&self, channel_id: &str, title: &str, message_id: Option<&str>) -> ApiResult<Thread> {
        self.post(&format!("/channels/{channel_id}/threads"), &CreateThread { title, message_id }).await
    }

    pub async fn search(&self, guild_id: &str, query: &SearchQuery) -> ApiResult<SearchResults> {
        let mut path = format!("/guilds/{guild_id}/search?limit={}&offset={}", query.limit.clamp(1, 100), query.offset);
        for (key, value) in [("q", &query.q), ("channel_id", &query.channel_id), ("author", &query.author), ("has", &query.has)] {
            if let Some(value) = value.as_deref().filter(|v| !v.is_empty()) {
                path.push_str(&format!("&{key}={}", urlencoding::encode(value)));
            }
        }
        self.get(&path).await
    }

    /// A guild's emoji and stickers.
    pub async fn expression_pack(&self, guild_id: &str) -> ApiResult<Expressions> {
        self.get(&format!("/guilds/{guild_id}/expressions")).await
    }

    pub async fn expressions(&self, guild_id: &str) -> ApiResult<Vec<CustomEmoji>> {
        Ok(self.get::<Expressions>(&format!("/guilds/{guild_id}/expressions")).await?.emojis)
    }

    /// Server-side member lookup by name, for autocomplete beyond the loaded roster.
    pub async fn search_members(&self, guild_id: &str, q: &str, limit: u32) -> ApiResult<MemberList> {
        self.get(&format!("/guilds/{guild_id}/members?q={}&limit={limit}", urlencoding::encode(q))).await
    }

    pub async fn patch_guild(&self, guild_id: &str, patch: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.patch(&format!("/guilds/{guild_id}"), patch).await
    }

    pub async fn leave_guild(&self, guild_id: &str) -> ApiResult<serde_json::Value> {
        self.post(&format!("/guilds/{guild_id}/leave"), &serde_json::json!({})).await
    }

    pub async fn create_channel(&self, guild_id: &str, name: &str, kind: &str, parent_id: Option<&str>) -> ApiResult<Channel> {
        self.post(
            &format!("/guilds/{guild_id}/channels"),
            &serde_json::json!({ "name": name, "type": kind, "parent_id": parent_id }),
        )
        .await
    }

    pub async fn patch_channel(&self, channel_id: &str, patch: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.patch(&format!("/channels/{channel_id}"), patch).await
    }

    pub async fn delete_channel(&self, channel_id: &str) -> ApiResult<serde_json::Value> {
        self.delete(&format!("/channels/{channel_id}")).await
    }

    pub async fn patch_member(&self, guild_id: &str, user_id: &str, patch: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.patch(&format!("/guilds/{guild_id}/members/{}", urlencoding::encode(user_id)), patch).await
    }

    pub async fn kick_member(&self, guild_id: &str, user_id: &str) -> ApiResult<serde_json::Value> {
        self.delete(&format!("/guilds/{guild_id}/members/{}", urlencoding::encode(user_id))).await
    }

    pub async fn ban_member(&self, guild_id: &str, user_id: &str, reason: Option<&str>) -> ApiResult<serde_json::Value> {
        let req = self
            .http
            .put(self.url(&format!("/guilds/{guild_id}/bans/{}", urlencoding::encode(user_id))))
            .json(&serde_json::json!({ "reason": reason }));
        self.send(req).await
    }

    pub async fn read_state(&self, guild_id: &str) -> ApiResult<ReadStateList> {
        self.get(&format!("/guilds/{guild_id}/read-state")).await
    }

    /// The members holding a role.
    pub async fn role_members(&self, guild_id: &str, role_id: &str, limit: u32) -> ApiResult<MemberList> {
        self.get(&format!("/guilds/{guild_id}/members?role={}&limit={limit}", urlencoding::encode(role_id))).await
    }

    pub async fn members(&self, guild_id: &str, offset: u32, limit: u32) -> ApiResult<MemberList> {
        self.get(&format!("/guilds/{guild_id}/members?offset={offset}&limit={limit}")).await
    }

    pub async fn join_invite(&self, code: &str) -> ApiResult<serde_json::Value> {
        self.post(&format!("/invites/{}/join", urlencoding::encode(code)), &serde_json::json!({})).await
    }

    /// Presses a bot's button or picks from its select (`values`) on a channel message, or on
    /// a message only we see (`ephemeral`). The bot answers with a callback of its own.
    pub async fn interact(&self, channel_id: &str, message_id: &str, ephemeral: bool, custom_id: &str, values: &[String]) -> ApiResult<serde_json::Value> {
        let kind = if ephemeral { "ephemeral" } else { "messages" };
        let path = format!("/channels/{}/{kind}/{}/interact", urlencoding::encode(channel_id), urlencoding::encode(message_id));
        let body = if ephemeral { serde_json::json!({ "custom_id": custom_id }) } else { serde_json::json!({ "custom_id": custom_id, "values": values }) };
        self.post(&path, &body).await
    }

    /// Submits the form a bot opened (`MODAL_OPEN`): `{custom_id, value}` per field.
    pub async fn submit_modal(&self, interaction_id: &str, fields: &[(String, String)]) -> ApiResult<serde_json::Value> {
        let components: Vec<serde_json::Value> = fields.iter().map(|(id, value)| serde_json::json!({ "custom_id": id, "value": value })).collect();
        self.post(&format!("/interactions/{}/modal", urlencoding::encode(interaction_id)), &serde_json::json!({ "components": components })).await
    }

    /// Follows a guild address (`#name:host`) to the guild that holds it now: the final `active`
    /// answer, with its `host` and `guild_id`.
    pub async fn resolve_guild_address(&self, address: &str) -> ApiResult<serde_json::Value> {
        self.get(&format!("/guild-addresses/resolve?address={}", urlencoding::encode(address))).await
    }

    /// Joins a guild by its live address (the same bans, gates and screening as an invite).
    pub async fn join_guild(&self, guild_id: &str) -> ApiResult<serde_json::Value> {
        self.post(&format!("/guilds/{}/join", urlencoding::encode(guild_id)), &serde_json::json!({})).await
    }

    /// Answers the invite object; its `code` is what `join_invite` takes.
    pub async fn create_invite(&self, guild_id: &str, channel_id: Option<&str>) -> ApiResult<serde_json::Value> {
        self.post(&format!("/guilds/{guild_id}/invites"), &serde_json::json!({ "channel_id": channel_id })).await
    }

    // ---------------------------------------------------------------- guild administration
    // One call to one server each: a guild has a single authority (web `guild-settings/*`).

    pub async fn guild(&self, guild_id: &str) -> ApiResult<Guild> {
        self.get(&format!("/guilds/{guild_id}")).await
    }

    pub async fn delete_guild(&self, guild_id: &str) -> ApiResult<serde_json::Value> {
        self.delete(&format!("/guilds/{guild_id}")).await
    }

    pub async fn set_guild_alias(&self, guild_id: &str, alias: &str) -> ApiResult<serde_json::Value> {
        self.put(&format!("/guilds/{guild_id}/alias"), &serde_json::json!({ "alias": alias })).await
    }

    pub async fn clear_guild_alias(&self, guild_id: &str) -> ApiResult<serde_json::Value> {
        self.delete(&format!("/guilds/{guild_id}/alias")).await
    }

    pub async fn transfer_ownership(&self, guild_id: &str, user_id: &str) -> ApiResult<serde_json::Value> {
        self.post(&format!("/guilds/{guild_id}/transfer-ownership"), &serde_json::json!({ "user_id": user_id })).await
    }

    pub async fn roles(&self, guild_id: &str) -> ApiResult<RoleList> {
        self.get(&format!("/guilds/{guild_id}/roles")).await
    }

    /// `body`: `name`, and optionally `color` (0xRRGGBB), `permissions` (names), `hoist`,
    /// `mentionable`, `position`.
    pub async fn create_role(&self, guild_id: &str, body: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.post(&format!("/guilds/{guild_id}/roles"), body).await
    }

    pub async fn patch_role(&self, guild_id: &str, role_id: &str, body: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.patch(&format!("/guilds/{guild_id}/roles/{role_id}"), body).await
    }

    pub async fn delete_role(&self, guild_id: &str, role_id: &str) -> ApiResult<serde_json::Value> {
        self.delete(&format!("/guilds/{guild_id}/roles/{role_id}")).await
    }

    /// New positions for some roles, `(id, position)`.
    pub async fn reorder_roles(&self, guild_id: &str, places: &[(String, i64)]) -> ApiResult<serde_json::Value> {
        let roles: Vec<serde_json::Value> = places.iter().map(|(id, position)| serde_json::json!({ "id": id, "position": position })).collect();
        self.patch(&format!("/guilds/{guild_id}/roles"), &serde_json::json!({ "roles": roles })).await
    }

    /// New positions (and parents) for some channels, `(id, position, parent)`.
    pub async fn reorder_channels(&self, guild_id: &str, places: &[(String, i64, Option<String>)]) -> ApiResult<serde_json::Value> {
        let channels: Vec<serde_json::Value> = places
            .iter()
            .map(|(id, position, parent)| serde_json::json!({ "id": id, "position": position, "parent_id": parent }))
            .collect();
        self.patch(&format!("/guilds/{guild_id}/channels"), &serde_json::json!({ "channels": channels })).await
    }

    /// Sets (or, with both zero, clears) a channel overwrite for a `role` or a `member`.
    pub async fn set_overwrite(&self, channel_id: &str, target_type: &str, target_id: &str, allow: u64, deny: u64) -> ApiResult<serde_json::Value> {
        self.put(
            &format!("/channels/{channel_id}/overwrites/{target_type}/{}", urlencoding::encode(target_id)),
            &serde_json::json!({ "allow": allow.to_string(), "deny": deny.to_string() }),
        )
        .await
    }

    pub async fn bans(&self, guild_id: &str) -> ApiResult<BanList> {
        self.get(&format!("/guilds/{guild_id}/bans")).await
    }

    pub async fn unban(&self, guild_id: &str, user_id: &str) -> ApiResult<serde_json::Value> {
        self.delete(&format!("/guilds/{guild_id}/bans/{}", urlencoding::encode(user_id))).await
    }

    pub async fn invites(&self, guild_id: &str) -> ApiResult<InviteList> {
        self.get(&format!("/guilds/{guild_id}/invites")).await
    }

    /// An invite that stops after `max_uses` joins or `expires_in_secs`, if given.
    pub async fn create_invite_with(&self, guild_id: &str, channel_id: Option<&str>, max_uses: Option<i64>, expires_in_secs: Option<i64>) -> ApiResult<InviteInfo> {
        self.post(
            &format!("/guilds/{guild_id}/invites"),
            &serde_json::json!({ "channel_id": channel_id, "max_uses": max_uses, "expires_in_secs": expires_in_secs }),
        )
        .await
    }

    pub async fn revoke_invite(&self, guild_id: &str, code: &str) -> ApiResult<serde_json::Value> {
        self.delete(&format!("/guilds/{guild_id}/invites/{}", urlencoding::encode(code))).await
    }

    /// AutoMod rules (`MANAGE_GUILD`).
    pub async fn automod_rules(&self, guild_id: &str) -> ApiResult<serde_json::Value> {
        self.get(&format!("/guilds/{guild_id}/automod")).await
    }

    pub async fn create_automod_rule(&self, guild_id: &str, rule: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.post(&format!("/guilds/{guild_id}/automod"), rule).await
    }

    pub async fn delete_automod_rule(&self, guild_id: &str, rule_id: &str) -> ApiResult<serde_json::Value> {
        self.delete(&format!("/guilds/{guild_id}/automod/{}", urlencoding::encode(rule_id))).await
    }

    /// What AutoMod caught, with a short excerpt each (`MANAGE_MESSAGES`).
    pub async fn automod_hits(&self, guild_id: &str) -> ApiResult<serde_json::Value> {
        self.get(&format!("/guilds/{guild_id}/automod-hits")).await
    }

    pub async fn audit_log(&self, guild_id: &str) -> ApiResult<AuditLog> {
        self.get(&format!("/guilds/{guild_id}/audit-log")).await
    }

    /// `data` is the file, base64; `animated`/`format`/`mime` as the web client sends them.
    pub async fn create_emoji(&self, guild_id: &str, body: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.post(&format!("/guilds/{guild_id}/emojis"), body).await
    }

    pub async fn create_sticker(&self, guild_id: &str, body: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.post(&format!("/guilds/{guild_id}/stickers"), body).await
    }

    pub async fn delete_emoji(&self, guild_id: &str, emoji_id: &str) -> ApiResult<serde_json::Value> {
        self.delete(&format!("/guilds/{guild_id}/emojis/{emoji_id}")).await
    }

    /// Deletes one member's recent messages, in one channel or all, at most `limit`.
    pub async fn purge_messages(&self, guild_id: &str, user_id: &str, channel_id: Option<&str>, limit: Option<i64>) -> ApiResult<serde_json::Value> {
        self.post(
            &format!("/guilds/{guild_id}/moderation/purge"),
            &serde_json::json!({ "user_id": user_id, "channel_id": channel_id, "limit": limit }),
        )
        .await
    }

    pub async fn guild_voice_regions(&self, guild_id: &str) -> ApiResult<VoiceRegionList> {
        self.get(&format!("/guilds/{guild_id}/voice/regions")).await
    }

    pub async fn add_guild_voice_region(&self, guild_id: &str, body: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.post(&format!("/guilds/{guild_id}/voice/regions"), body).await
    }

    pub async fn delete_guild_voice_region(&self, guild_id: &str, name: &str) -> ApiResult<serde_json::Value> {
        self.delete(&format!("/guilds/{guild_id}/voice/regions/{}", urlencoding::encode(name))).await
    }

    pub async fn set_guild_voice_region_default(&self, guild_id: &str, name: &str) -> ApiResult<serde_json::Value> {
        self.patch(&format!("/guilds/{guild_id}/voice/regions/default"), &serde_json::json!({ "name": name })).await
    }

    /// The join gate: account age, domain and origin policies, screening questions, held first
    /// messages. Kept as the server's JSON, which the gate page reads and writes whole.
    pub async fn gates(&self, guild_id: &str) -> ApiResult<serde_json::Value> {
        self.get(&format!("/guilds/{guild_id}/gates")).await
    }

    pub async fn set_gates(&self, guild_id: &str, body: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.put(&format!("/guilds/{guild_id}/gates"), body).await
    }

    pub async fn gate_curators(&self, guild_id: &str) -> ApiResult<serde_json::Value> {
        self.get(&format!("/guilds/{guild_id}/gates/curators")).await
    }

    pub async fn refresh_gate_feeds(&self, guild_id: &str) -> ApiResult<serde_json::Value> {
        self.post(&format!("/guilds/{guild_id}/gates/refresh-feeds"), &serde_json::json!({})).await
    }

    pub async fn applicants(&self, guild_id: &str) -> ApiResult<serde_json::Value> {
        self.get(&format!("/guilds/{guild_id}/applicants")).await
    }

    /// `decision`: `approve` or `reject`.
    pub async fn decide_applicant(&self, guild_id: &str, user_id: &str, decision: &str) -> ApiResult<serde_json::Value> {
        self.post(&format!("/guilds/{guild_id}/applicants/{}", urlencoding::encode(user_id)), &serde_json::json!({ "decision": decision })).await
    }

    pub async fn held_messages(&self, guild_id: &str) -> ApiResult<serde_json::Value> {
        self.get(&format!("/guilds/{guild_id}/held")).await
    }

    pub async fn decide_held(&self, guild_id: &str, held_id: &str, decision: &str) -> ApiResult<serde_json::Value> {
        self.post(&format!("/guilds/{guild_id}/held/{held_id}"), &serde_json::json!({ "decision": decision })).await
    }

    pub async fn guild_bots(&self, guild_id: &str) -> ApiResult<serde_json::Value> {
        self.get(&format!("/guilds/{guild_id}/bots")).await
    }

    pub async fn preview_bot(&self, guild_id: &str, body: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.post(&format!("/guilds/{guild_id}/bots/preview"), body).await
    }

    pub async fn install_bot(&self, guild_id: &str, body: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.post(&format!("/guilds/{guild_id}/bots"), body).await
    }

    pub async fn patch_bot(&self, guild_id: &str, bot_user_id: &str, body: &serde_json::Value) -> ApiResult<serde_json::Value> {
        self.patch(&format!("/guilds/{guild_id}/bots/{}", urlencoding::encode(bot_user_id)), body).await
    }

    pub async fn uninstall_bot(&self, guild_id: &str, bot_user_id: &str) -> ApiResult<serde_json::Value> {
        self.delete(&format!("/guilds/{guild_id}/bots/{}", urlencoding::encode(bot_user_id))).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::Api;

    /// A guild server whose sessions have all run out but one (`Bearer new`), and a home server
    /// that hands out assertions; counts the sessions minted.
    async fn servers() -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let minted = Arc::new(AtomicUsize::new(0));
        let count = minted.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let count = count.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = request.split_whitespace().nth(1).unwrap_or_default().to_owned();
                    let fresh = request.lines().any(|l| l.eq_ignore_ascii_case("authorization: bearer new"));
                    let (status, body) = match path.as_str() {
                        "/_telium/v1/assertions" => ("200 OK", r#"{"assertion":"a"}"#.to_owned()),
                        "/_telium/v1/sessions" => {
                            count.fetch_add(1, Ordering::SeqCst);
                            // A moment's work, so the refused requests pile up behind it.
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            ("200 OK", r#"{"session_token":"new","user_id":"@u:x","expires_at":4102444800000}"#.to_owned())
                        }
                        _ if fresh => ("200 OK", r#"{"ok":true}"#.to_owned()),
                        _ => ("401 Unauthorized", r#"{"error":"unauthorized","message":"guild session expired"}"#.to_owned()),
                    };
                    let reply = format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                    let _ = socket.write_all(reply.as_bytes()).await;
                });
            }
        });
        (base, minted)
    }

    #[tokio::test]
    async fn a_run_out_session_renews_itself_once() {
        let (base, minted) = servers().await;
        let http = reqwest::Client::new();
        let home = Api::new(http.clone(), &base, Some("home".into()));
        let guild = Api::new(http, &base, Some("old".into()));
        // Without a way to renew, the refusal stands.
        assert_eq!(guild.get::<serde_json::Value>("/thing").await.unwrap_err().status(), Some(401));
        guild.renew_from(&home, "x");
        let (a, b, c) = tokio::join!(guild.get::<serde_json::Value>("/a"), guild.get::<serde_json::Value>("/b"), guild.get::<serde_json::Value>("/c"));
        for answer in [a, b, c] {
            assert_eq!(answer.unwrap()["ok"], true);
        }
        assert_eq!(minted.load(Ordering::SeqCst), 1, "three refusals, one renewal");
        assert_eq!(guild.token().await.as_deref(), Some("new"));
        assert_eq!(guild.expires_at(), Some(4102444800000));
        // Far from its end: nothing to do ahead of time.
        assert_eq!(guild.renew_if_expiring(std::time::Duration::from_secs(300)).await.unwrap(), None);
    }
}
