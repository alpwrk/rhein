use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Attachment, Channel, Guild, Member, Message, Millis, Profile};

/// `GET /.well-known/telium/server`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Discovery {
    pub server_name: String,
    pub api: String,
    pub gateway: Option<String>,
    pub client: Option<String>,
    pub fallback: Option<String>,
    pub moved_to: Option<String>,
    pub relay: Option<Value>,
    pub protocol: Option<ProtocolWindow>,
    /// The domain that is the authority now (differs from `server_name` after a warm standby
    /// was promoted).
    pub authority: Option<String>,
    /// A planned shutdown announced by the operator.
    pub sunset: Option<Value>,
    pub relocation: Option<Value>,
    /// Where `moved_to` led, when discovery followed it: the name to keep from now on.
    #[serde(skip)]
    pub relocated_to: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProtocolWindow {
    pub version: u32,
    pub min_supported: u32,
    pub capabilities: Vec<String>,
}

/// `GET /_telium/v1/capabilities`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    pub server_name: String,
    pub gateway: Option<String>,
    pub roles: Value,
    pub registration_open: bool,
    /// Sign-up is by registration code only (Telium server 0.4.1+): `registration_open` is
    /// false, but someone holding a code may still register.
    pub registration_code_required: bool,
    pub registration_min_password_len: Option<u32>,
    pub auth: AuthCapabilities,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthCapabilities {
    pub opaque: bool,
    pub recovery_key_reset: bool,
    pub protocol: Option<String>,
}

/// The `{error, code, message}` body every non-2xx answer carries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ErrorBody {
    pub error: String,
    pub code: Option<String>,
    pub message: Option<String>,
    /// Seconds until a rate-limited request may be repeated.
    #[serde(default)]
    pub retry_after: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisterStart<'a> {
    pub localpart: &'a str,
    pub registration_request: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registration_code: Option<&'a str>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegisterStartResponse {
    pub registration_response: String,
    #[serde(default)]
    pub min_password_len: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisterFinish<'a> {
    pub localpart: &'a str,
    pub registration_record: String,
    pub device_name: &'a str,
    pub device_pubkey: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registration_code: Option<&'a str>,
    /// The key-stretching set the record was made with (`opaque_ksf_version`).
    pub ksf_version: i64,
    /// The account recovery key's public half, so a password reset can be proven later.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_public_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoginStart<'a> {
    pub user: &'a str,
    pub start_login_request: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginStartResponse {
    pub login_id: String,
    pub login_response: String,
    #[serde(default)]
    pub legacy_upgrade_required: bool,
    /// The key-stretching set the account's record was made with; absent means 1.
    #[serde(default)]
    pub ksf_version: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoginFinish<'a> {
    pub login_id: &'a str,
    pub finish_login_request: String,
    pub device_name: &'a str,
    pub device_pubkey: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mfa_code: Option<&'a str>,
}

/// What both `register/opaque/finish` and `login/opaque/finish` answer with.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HomeSession {
    pub user_id: String,
    pub device_id: String,
    pub access_token: String,
    pub expires_at: Option<Millis>,
    pub server_name: String,
    /// Registration only: the server kept the recovery key's public half.
    #[serde(default)]
    pub recovery_key_enrolled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WhoAmI {
    pub user_id: String,
    pub device_id: String,
    pub server_name: String,
    pub admin: bool,
    pub profile: Option<Profile>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssertionRequest<'a> {
    pub audience: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssertionResponse {
    pub assertion: String,
    #[serde(default)]
    pub expires_at: Option<Millis>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionRequest<'a> {
    pub assertion: &'a str,
}

/// `POST /_telium/v1/sessions` on a guild server.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GuildSession {
    pub session_token: String,
    pub user_id: String,
    pub expires_at: Option<Millis>,
    pub revalidate_at: Option<Millis>,
    pub gateway: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GuildList {
    pub guilds: Vec<Guild>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ChannelList {
    pub channels: Vec<Channel>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MessageList {
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MemberList {
    pub members: Vec<Member>,
    pub member_count: Option<i64>,
    pub online_count: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SendMessage {
    pub content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EditMessage<'a> {
    pub content: &'a str,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateGuild<'a> {
    pub name: &'a str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Ack<'a> {
    pub message_id: &'a str,
}

/// `GET /guilds/{id}/read-state`: per channel, how far this user has read.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ReadStateList {
    pub guild_id: String,
    pub channels: Vec<ChannelReadState>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ChannelReadState {
    pub channel_id: String,
    pub last_read_id: Option<String>,
    pub muted: bool,
    pub unread: u32,
    pub mentions: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PinList {
    pub pins: Vec<Message>,
}

/// A thread: a channel of type `thread` under a text channel.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Thread {
    pub id: String,
    pub parent_id: String,
    pub guild_id: Option<String>,
    pub starter_id: Option<String>,
    pub owner: Option<String>,
    pub title: String,
    pub archived: bool,
    pub locked: bool,
    pub message_count: i64,
    pub last_active: Option<Millis>,
    pub created_at: Option<Millis>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ThreadList {
    pub threads: Vec<Thread>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateThread<'a> {
    pub title: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<&'a str>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub q: Option<String>,
    pub channel_id: Option<String>,
    pub author: Option<String>,
    pub has: Option<String>,
    pub offset: u32,
    pub limit: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SearchResults {
    pub results: Vec<Message>,
    pub count: u32,
    pub total: u32,
    pub has_more: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomEmoji {
    pub id: String,
    pub name: String,
    pub animated: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Expressions {
    pub emojis: Vec<CustomEmoji>,
    /// Stickers carry the same fields; `animated` is a GIF.
    pub stickers: Vec<CustomEmoji>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Device {
    pub device_id: String,
    pub name: Option<String>,
    pub created_at: Option<Millis>,
    pub last_seen: Option<Millis>,
    pub active: bool,
    pub current: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DeviceList {
    pub devices: Vec<Device>,
}

/// One relationship as the home server keeps it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Friend {
    pub user_id: String,
    pub domain: Option<String>,
    /// `accepted`, `pending_in`, `pending_out` or `blocked`.
    pub state: String,
    pub presence: Option<serde_json::Value>,
    pub display_name: Option<String>,
    pub avatar_ref: Option<String>,
    pub created_at: Option<Millis>,
    pub updated_at: Option<Millis>,
}

impl Friend {
    pub fn status(&self) -> &str {
        self.presence
            .as_ref()
            .and_then(|p| p.get("status").and_then(|s| s.as_str()).or_else(|| p.as_str()))
            .unwrap_or("offline")
    }

    pub fn activities(&self) -> Vec<crate::Activity> {
        self.presence.as_ref().map(crate::activity::activities_of).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FriendList {
    pub friends: Vec<Friend>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConversationMember {
    pub user_id: String,
    pub home: Option<String>,
    pub state: Option<String>,
}

/// A private conversation's metadata; its messages are MLS ciphertext.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Conversation {
    pub id: String,
    /// `dm` or `gc`.
    pub kind: String,
    pub creator: Option<String>,
    pub members: Vec<ConversationMember>,
    pub last_seq: Option<i64>,
    pub created_at: Option<Millis>,
    pub last_activity_at: Option<Millis>,
    pub request_state: Option<String>,
    pub muted: bool,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ConversationList {
    pub conversations: Vec<Conversation>,
}

// ------------------------------------------------------------------ guild administration

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RoleList {
    pub roles: Vec<crate::model::Role>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Ban {
    pub user_id: String,
    pub reason: Option<String>,
    pub created_at: Option<Millis>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BanList {
    pub bans: Vec<Ban>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct InviteInfo {
    pub code: String,
    pub url: Option<String>,
    pub channel_id: Option<String>,
    pub creator: Option<String>,
    pub uses: i64,
    pub max_uses: Option<i64>,
    pub expires_at: Option<Millis>,
    pub created_at: Option<Millis>,
    pub expired: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct InviteList {
    pub invites: Vec<InviteInfo>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AuditEntry {
    pub id: String,
    pub actor: String,
    /// `member.ban`, `role.update`, `invite.revoke`, …
    pub action: String,
    pub target: Option<String>,
    pub data: Value,
    pub created_at: Millis,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AuditLog {
    pub entries: Vec<AuditEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct VoiceRegion {
    pub name: String,
    pub sfu: Option<String>,
    pub turn: Option<String>,
    /// Added by this guild rather than offered by the server.
    pub guild: bool,
    pub default: bool,
    pub healthy: bool,
    pub latency_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct VoiceRegionList {
    pub regions: Vec<VoiceRegion>,
}

