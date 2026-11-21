//! What travels inside an application message: a type byte, then UTF-8 (`pack`/`unpack` in
//! `mls.js`). Unknown types are dropped, never drawn as text.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MESSAGE: u8 = 0x00;
pub const RECEIPT: u8 = 0x01;
pub const TYPING: u8 = 0x02;
pub const REACTION: u8 = 0x03;
pub const EDIT: u8 = 0x04;
pub const DELETE: u8 = 0x05;
pub const PIN: u8 = 0x06;
pub const CALL_SFU: u8 = 0x07;
pub const DELIVERED: u8 = 0x08;
pub const RENAME: u8 = 0x09;

/// A group name is at most this many characters (`GROUP_NAME_MAX` in `mls.js`).
pub const GROUP_NAME_MAX: usize = 100;

/// A name as it is sent: whitespace runs collapsed, trimmed, capped; empty means no name.
pub fn clean_group_name(text: &str) -> Option<String> {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped: String = collapsed.chars().take(GROUP_NAME_MAX).collect();
    Some(capped.trim().to_owned()).filter(|n| !n.is_empty())
}

/// What a received name must already be: trimmed, not empty, not too long.
pub fn valid_group_name(text: &str) -> bool {
    text.trim() == text && !text.is_empty() && text.chars().count() <= GROUP_NAME_MAX
}

/// The marker in front of a body that carries more than text (attachments, a reply).
pub const ENVELOPE: &str = "\u{0000}telium/dm-body/1\n";
/// How much of a pinned message rides inside the pin.
pub const SNIPPET_MAX: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    Message(String),
    Receipt(Value),
    Typing,
    Reaction { target: String, emoji: String, add: bool },
    Edit { target: String, text: String },
    Delete { target: String },
    Pin { target: String, add: bool, author: String, snippet: String },
    CallSfu { endpoint: Option<String> },
    Delivered(Value),
    /// A group's name, or `None` to clear it so the group shows its people again.
    Rename { name: Option<String> },
    Unknown,
}

pub fn pack(tag: u8, body: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 1);
    out.push(tag);
    out.extend_from_slice(body.as_bytes());
    out
}

impl Payload {
    pub fn encode(&self) -> Vec<u8> {
        use serde_json::json;
        match self {
            Payload::Message(text) => pack(MESSAGE, text),
            Payload::Receipt(v) => pack(RECEIPT, &v.to_string()),
            Payload::Typing => pack(TYPING, ""),
            Payload::Reaction { target, emoji, add } => {
                pack(REACTION, &json!({ "target": target, "emoji": emoji, "op": if *add { "add" } else { "remove" } }).to_string())
            }
            Payload::Edit { target, text } => pack(EDIT, &json!({ "target": target, "text": text }).to_string()),
            Payload::Delete { target } => pack(DELETE, &json!({ "target": target }).to_string()),
            Payload::Pin { target, add, author, snippet } => {
                let mut body = json!({ "target": target, "op": if *add { "add" } else { "remove" } });
                if *add {
                    body["author"] = json!(author);
                    body["snippet"] = json!(snippet.chars().take(SNIPPET_MAX).collect::<String>());
                }
                pack(PIN, &body.to_string())
            }
            Payload::CallSfu { endpoint } => pack(CALL_SFU, &json!({ "endpoint": endpoint }).to_string()),
            Payload::Delivered(v) => pack(DELIVERED, &v.to_string()),
            Payload::Rename { name } => pack(RENAME, &json!({ "name": name }).to_string()),
            Payload::Unknown => Vec::new(),
        }
    }

    pub fn decode(bytes: &[u8]) -> Payload {
        let Some((&tag, rest)) = bytes.split_first() else { return Payload::Unknown };
        let text = String::from_utf8_lossy(rest).into_owned();
        let json = || serde_json::from_str::<Value>(&text).ok();
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        match tag {
            MESSAGE => Payload::Message(text),
            RECEIPT => Payload::Receipt(json().unwrap_or(Value::Null)),
            TYPING => Payload::Typing,
            REACTION => json()
                .and_then(|v| Some(Payload::Reaction { target: s(&v, "target")?, emoji: s(&v, "emoji")?, add: v["op"] != "remove" }))
                .unwrap_or(Payload::Unknown),
            EDIT => json().and_then(|v| Some(Payload::Edit { target: s(&v, "target")?, text: s(&v, "text")? })).unwrap_or(Payload::Unknown),
            DELETE => json().and_then(|v| Some(Payload::Delete { target: s(&v, "target")? })).unwrap_or(Payload::Unknown),
            PIN => json()
                .and_then(|v| {
                    let add = match v["op"].as_str()? {
                        "add" => true,
                        "remove" => false,
                        _ => return None,
                    };
                    Some(Payload::Pin {
                        target: s(&v, "target")?,
                        add,
                        author: s(&v, "author").unwrap_or_default().chars().take(SNIPPET_MAX).collect(),
                        snippet: s(&v, "snippet").unwrap_or_default().chars().take(SNIPPET_MAX).collect(),
                    })
                })
                .unwrap_or(Payload::Unknown),
            CALL_SFU => json().map(|v| Payload::CallSfu { endpoint: s(&v, "endpoint") }).unwrap_or(Payload::Unknown),
            DELIVERED => Payload::Delivered(json().unwrap_or(Value::Null)),
            RENAME => json()
                .and_then(|v| match v.get("name")? {
                    Value::Null => Some(Payload::Rename { name: None }),
                    Value::String(n) if valid_group_name(n) => Some(Payload::Rename { name: Some(n.clone()) }),
                    _ => None,
                })
                .unwrap_or(Payload::Unknown),
            _ => Payload::Unknown,
        }
    }
}

/// An attachment as the body carries it: where the encrypted bytes are and the key to them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    #[serde(flatten)]
    pub fields: serde_json::Map<String, Value>,
}

/// A message body: the text, attachments and a reply snapshot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Body {
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<Value>,
    #[serde(default)]
    pub reply: Option<Value>,
}

impl Body {
    /// `encodeBody`: plain text stays plain text; anything more is the envelope.
    pub fn encode(&self) -> String {
        if self.attachments.is_empty() && self.reply.is_none() {
            return self.text.clone();
        }
        format!("{ENVELOPE}{}", serde_json::to_string(self).expect("serialisable"))
    }

    pub fn decode(raw: &str) -> Body {
        match raw.strip_prefix(ENVELOPE) {
            Some(json) => serde_json::from_str(json).unwrap_or_else(|_| Body { text: raw.to_owned(), ..Default::default() }),
            None => Body { text: raw.to_owned(), ..Default::default() },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn payloads_round_trip() {
        for p in [
            Payload::Message("hi".into()),
            Payload::Typing,
            Payload::Reaction { target: "01F".into(), emoji: "👍".into(), add: true },
            Payload::Reaction { target: "01F".into(), emoji: "👍".into(), add: false },
            Payload::Edit { target: "01F".into(), text: "fixed".into() },
            Payload::Delete { target: "01F".into() },
            Payload::Pin { target: "01F".into(), add: true, author: "@a:x".into(), snippet: "hello".into() },
            Payload::CallSfu { endpoint: Some("https://sfu.example".into()) },
        ] {
            assert_eq!(Payload::decode(&p.encode()), p);
        }
        assert_eq!(Payload::decode(&[0x42, b'x']), Payload::Unknown);
        // RENAME, as `setGroupName` packs it.
        assert_eq!(Payload::decode(&pack(RENAME, r#"{"name":"Book club"}"#)), Payload::Rename { name: Some("Book club".into()) });
        assert_eq!(Payload::decode(&pack(RENAME, r#"{"name":null}"#)), Payload::Rename { name: None });
        assert_eq!(Payload::decode(&pack(RENAME, r#"{"name":" padded "}"#)), Payload::Unknown);
        assert_eq!(Payload::decode(&pack(RENAME, &format!(r#"{{"name":"{}"}}"#, "x".repeat(101)))), Payload::Unknown);
        assert_eq!(Payload::decode(&Payload::Rename { name: Some("A".into()) }.encode()), Payload::Rename { name: Some("A".into()) });
        assert_eq!(clean_group_name("  Book \n  club "), Some("Book club".into()));
        assert_eq!(clean_group_name("   "), None);
        assert_eq!(Payload::decode(&pack(REACTION, "not json")), Payload::Unknown);
    }

    #[test]
    fn bodies_match_the_web_client() {
        let plain = Body { text: "hello".into(), ..Default::default() };
        assert_eq!(plain.encode(), "hello");
        let rich = Body { text: "look".into(), attachments: vec![json!({ "name": "a.png" })], reply: None };
        let encoded = rich.encode();
        assert!(encoded.starts_with("\u{0}telium/dm-body/1\n"));
        assert_eq!(Body::decode(&encoded), rich);
        assert_eq!(Body::decode("\u{0}telium/dm-body/1\n{broken").text, "\u{0}telium/dm-body/1\n{broken");
    }
}
