//! Mesh calls, wire-compatible with the web client's `MeshCall` (`web/src/lib/voice.js`).
//!
//! One peer connection per participant, perfect negotiation (`polite = me > peer`), and exactly
//! four transceivers laid down in a fixed order: microphone, camera, screen video, screen audio.
//! A source goes on or off by swapping the sender's track and turning the direction around, never
//! by adding m-lines. Signals are the relay payloads `{description: {type, sdp}}` and
//! `{candidate: {candidate, sdpMid, sdpMLineIndex}}`.

use std::collections::{HashMap, HashSet};

use livekit::webrtc::ice_candidate::IceCandidate;
use livekit::webrtc::media_stream_track::MediaStreamTrack;
use livekit::webrtc::peer_connection::{AnswerOptions, OfferOptions, PeerConnection, PeerConnectionState, SignalingState};
use livekit::webrtc::peer_connection_factory::{IceServer, IceTransportsType, PeerConnectionFactory, RtcConfiguration};
use livekit::webrtc::rtp_transceiver::{RtpTransceiver, RtpTransceiverDirection, RtpTransceiverInit};
use livekit::webrtc::session_description::{SdpType, SessionDescription};
use livekit::webrtc::MediaType;
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// A server's ICE list (`voice_server.ice_servers`) as libwebrtc takes it.
pub fn ice_servers(servers: &[rhein_proto::IceServer]) -> Vec<IceServer> {
    servers
        .iter()
        .map(|s| IceServer {
            urls: s.urls(),
            username: s.username.clone().unwrap_or_default(),
            password: s.credential.clone().unwrap_or_default(),
        })
        .filter(|s| !s.urls.is_empty())
        .collect()
}

/// The four m-lines, in the order both sides lay them down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Slot {
    Mic = 0,
    Camera = 1,
    Screen = 2,
    ScreenAudio = 3,
}

impl Slot {
    pub const ALL: [Slot; 4] = [Slot::Mic, Slot::Camera, Slot::Screen, Slot::ScreenAudio];

    fn media(self) -> MediaType {
        match self {
            Slot::Mic | Slot::ScreenAudio => MediaType::Audio,
            Slot::Camera | Slot::Screen => MediaType::Video,
        }
    }
}

/// What a call (mesh or SFU) tells its owner.
pub enum CallEvent {
    /// A relay payload for one peer, to go out through `/voice/signal`.
    Signal { target: String, payload: Value },
    /// A peer's media connection came up or went down.
    Link { user: String, up: bool },
    /// A peer started sending on a slot.
    Track { from: String, slot: Slot, track: MediaStreamTrack },
    /// A peer stopped sending on a slot.
    TrackEnded { from: String, slot: Slot },
    /// Something worth surfacing; the call goes on.
    Error(String),
    /// The server ended this seat's media session (a moderator, or a sweep that raced a
    /// reconnect). The caller asks the roster which, then rejoins or leaves.
    Removed,
    /// The media session ended while the seat stands: recoverable by retaking.
    Dropped(String),
    /// A peer's media failed to open: on an encrypted call probably a commit behind, so the
    /// caller asks the encryption group where it stands.
    KeyTrouble { from: String },
}

enum Input {
    Remote(Value),
    Negotiate,
    State(PeerConnectionState),
}

struct Peer {
    pc: PeerConnection,
    transceivers: Vec<RtpTransceiver>,
    input: mpsc::UnboundedSender<Input>,
}

/// A mesh call: the set of peer connections of one participant.
pub struct Mesh {
    me: String,
    factory: PeerConnectionFactory,
    ice: Vec<IceServer>,
    relay: bool,
    sources: [Option<MediaStreamTrack>; 4],
    peers: HashMap<String, Peer>,
    events: mpsc::UnboundedSender<CallEvent>,
}

impl Mesh {
    pub fn new(me: &str, factory: PeerConnectionFactory, ice: Vec<IceServer>, relay: bool) -> (Mesh, mpsc::UnboundedReceiver<CallEvent>) {
        let (events, rx) = mpsc::unbounded_channel();
        let mesh = Mesh { me: me.to_owned(), factory, ice, relay, sources: Default::default(), peers: HashMap::new(), events };
        (mesh, rx)
    }

    pub fn factory(&self) -> &PeerConnectionFactory {
        &self.factory
    }

    fn config(&self) -> RtcConfiguration {
        let mut config = RtcConfiguration::default();
        config.ice_servers = self.ice.clone();
        config.ice_transport_type = if self.relay { IceTransportsType::Relay } else { IceTransportsType::All };
        config
    }

    /// Reconciles the connections with who the server says is in the call.
    pub fn sync<'a>(&mut self, users: impl IntoIterator<Item = &'a str>) {
        let want: HashSet<&str> = users.into_iter().filter(|u| *u != self.me).collect();
        for user in &want {
            self.ensure(user);
        }
        let gone: Vec<String> = self.peers.keys().filter(|u| !want.contains(u.as_str())).cloned().collect();
        for user in gone {
            self.drop_peer(&user);
        }
    }

    /// One inbound relay payload. Unknown senders are connected to: their offer routinely
    /// beats the voice state that announces them.
    pub fn accept(&mut self, from: &str, payload: Value) {
        if from == self.me {
            return;
        }
        if let Some(peer) = self.ensure(from) {
            let _ = peer.input.send(Input::Remote(payload));
        }
    }

    /// Puts a local source on every connection (or takes it off with `None`).
    pub fn set_source(&mut self, slot: Slot, track: Option<MediaStreamTrack>) {
        for (user, peer) in &self.peers {
            if let Err(err) = put_source(&peer.transceivers[slot as usize], track.clone()) {
                let _ = self.events.send(CallEvent::Error(format!("{user}: {err}")));
            }
        }
        self.sources[slot as usize] = track;
    }

    /// Switches between direct and relayed ICE for every connection.
    pub fn set_relay(&mut self, relay: bool) {
        if relay == self.relay {
            return;
        }
        self.relay = relay;
        let config = self.config();
        for peer in self.peers.values() {
            let _ = peer.pc.set_configuration(config.clone());
            if relay {
                peer.pc.restart_ice();
            }
        }
    }

    pub fn peers(&self) -> impl Iterator<Item = &str> {
        self.peers.keys().map(String::as_str)
    }

    pub fn close(&mut self) {
        let users: Vec<String> = self.peers.keys().cloned().collect();
        for user in users {
            self.drop_peer(&user);
        }
    }

    fn ensure(&mut self, user: &str) -> Option<&Peer> {
        if !self.peers.contains_key(user) {
            match self.connect(user) {
                Ok(peer) => {
                    self.peers.insert(user.to_owned(), peer);
                }
                Err(err) => {
                    let _ = self.events.send(CallEvent::Error(format!("{user}: {err}")));
                    return None;
                }
            }
        }
        self.peers.get(user)
    }

    fn connect(&self, user: &str) -> anyhow::Result<Peer> {
        let pc = self.factory.create_peer_connection(self.config())?;
        let mut transceivers = Vec::with_capacity(4);
        for slot in Slot::ALL {
            let transceiver = match &self.sources[slot as usize] {
                Some(track) => pc.add_transceiver(
                    track.clone(),
                    RtpTransceiverInit { direction: RtpTransceiverDirection::SendRecv, stream_ids: vec![format!("{}-{slot:?}", self.me)], send_encodings: vec![] },
                )?,
                None => pc.add_transceiver_for_media(
                    slot.media(),
                    RtpTransceiverInit { direction: RtpTransceiverDirection::RecvOnly, stream_ids: vec![], send_encodings: vec![] },
                )?,
            };
            transceivers.push(transceiver);
        }

        let (input, rx) = mpsc::unbounded_channel();
        let negotiate = input.clone();
        pc.on_negotiation_needed(Some(Box::new(move |_| {
            let _ = negotiate.send(Input::Negotiate);
        })));
        let state = input.clone();
        pc.on_connection_state_change(Some(Box::new(move |s| {
            let _ = state.send(Input::State(s));
        })));
        let (events, target) = (self.events.clone(), user.to_owned());
        pc.on_ice_candidate(Some(Box::new(move |candidate: IceCandidate| {
            let payload = json!({ "candidate": {
                "candidate": candidate.candidate(),
                "sdpMid": candidate.sdp_mid(),
                "sdpMLineIndex": candidate.sdp_mline_index(),
            }});
            let _ = events.send(CallEvent::Signal { target: target.clone(), payload });
        })));

        let actor = Actor {
            user: user.to_owned(),
            polite: self.me.as_str() > user,
            pc: pc.clone(),
            events: self.events.clone(),
            pending: Vec::new(),
            has_remote: false,
            remote_sdp: None,
            receiving: [false; 4],
            restarted: false,
            connected: false,
        };
        // libwebrtc asked for negotiation while the transceivers went in, before anyone listened.
        let _ = input.send(Input::Negotiate);
        tokio::spawn(actor.run(rx));
        Ok(Peer { pc, transceivers, input })
    }

    fn drop_peer(&mut self, user: &str) {
        if let Some(peer) = self.peers.remove(user) {
            peer.pc.on_negotiation_needed(None);
            peer.pc.on_connection_state_change(None);
            peer.pc.on_ice_candidate(None);
            peer.pc.close();
            let _ = peer.input.send(Input::State(PeerConnectionState::Closed));
        }
    }
}

impl Drop for Mesh {
    fn drop(&mut self) {
        self.close();
    }
}

fn put_source(transceiver: &RtpTransceiver, track: Option<MediaStreamTrack>) -> anyhow::Result<()> {
    match track {
        Some(track) => {
            transceiver.sender().set_track(Some(track))?;
            transceiver.set_direction(RtpTransceiverDirection::SendRecv)?;
        }
        // The native `set_track(None)` dereferences the missing track. A recvonly m-line sends
        // nothing, so the old track stays on the sender until the next one replaces it; its
        // capture is stopped by whoever owns it.
        None => transceiver.set_direction(RtpTransceiverDirection::RecvOnly)?,
    }
    Ok(())
}

/// One peer's negotiation, one payload at a time: applying a description takes several steps,
/// and two of them interleaved leave the connection in a state neither describes.
struct Actor {
    user: String,
    polite: bool,
    pc: PeerConnection,
    events: mpsc::UnboundedSender<CallEvent>,
    pending: Vec<Value>,
    has_remote: bool,
    /// The remote description of the last completed negotiation.
    remote_sdp: Option<String>,
    receiving: [bool; 4],
    restarted: bool,
    connected: bool,
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Input>) {
        while let Some(input) = rx.recv().await {
            let result = match input {
                Input::Negotiate => self.offer().await,
                Input::Remote(payload) => self.apply(payload).await,
                Input::State(PeerConnectionState::Closed) => break,
                Input::State(state) => {
                    self.state(state);
                    Ok(())
                }
            };
            if let Err(err) = result {
                let _ = self.events.send(CallEvent::Error(format!("{}: {err}", self.user)));
            }
            self.announce_tracks();
        }
        for slot in Slot::ALL {
            if self.receiving[slot as usize] {
                let _ = self.events.send(CallEvent::TrackEnded { from: self.user.clone(), slot });
            }
        }
        if self.connected {
            let _ = self.events.send(CallEvent::Link { user: self.user.clone(), up: false });
        }
    }

    fn send(&self, payload: Value) {
        let _ = self.events.send(CallEvent::Signal { target: self.user.clone(), payload });
    }

    async fn offer(&mut self) -> anyhow::Result<()> {
        // Browsers only fire negotiation-needed while stable; a request queued behind an offer
        // that has since been answered is picked up again once the connection settles.
        if self.pc.signaling_state() != SignalingState::Stable {
            return Ok(());
        }
        // Left false, these are taken as the legacy "do not receive" and turn every recvonly
        // m-line inactive; browsers leave them unset.
        let options = OfferOptions { offer_to_receive_audio: true, offer_to_receive_video: true, ..Default::default() };
        let offer = self.pc.create_offer(options).await?;
        self.pc.set_local_description(offer.clone()).await?;
        self.send(json!({ "description": { "type": "offer", "sdp": offer.to_string() } }));
        Ok(())
    }

    async fn apply(&mut self, payload: Value) -> anyhow::Result<()> {
        if let Some(description) = payload.get("description") {
            let kind = description["type"].as_str().unwrap_or_default();
            let sdp = description["sdp"].as_str().unwrap_or_default();
            let offer = kind == "offer";
            let collision = offer && self.pc.signaling_state() != SignalingState::Stable;
            // A collision is two offers in flight: the impolite side ignores the other one, the
            // polite side rolls its own back (the browser does that implicitly, libwebrtc not).
            if collision && !self.polite {
                return Ok(());
            }
            if collision {
                self.pc.set_local_description(SessionDescription::parse("", SdpType::Rollback)?).await?;
            }
            if !offer && self.pc.signaling_state() != SignalingState::HaveLocalOffer {
                return Ok(());
            }
            let sdp_type = if offer { SdpType::Offer } else { SdpType::Answer };
            self.pc.set_remote_description(SessionDescription::parse(sdp, sdp_type)?).await?;
            self.has_remote = true;
            self.remote_sdp = Some(sdp.to_owned());
            for candidate in std::mem::take(&mut self.pending) {
                // One candidate the agent will not take is not worth the description.
                let _ = self.add_candidate(&candidate).await;
            }
            if offer {
                let answer = self.pc.create_answer(AnswerOptions::default()).await?;
                self.pc.set_local_description(answer.clone()).await?;
                self.send(json!({ "description": { "type": "answer", "sdp": answer.to_string() } }));
            }
        } else if let Some(candidate) = payload.get("candidate") {
            if !self.has_remote {
                self.pending.push(candidate.clone());
            } else {
                self.add_candidate(candidate).await?;
            }
        }
        Ok(())
    }

    async fn add_candidate(&self, candidate: &Value) -> anyhow::Result<()> {
        let line = candidate["candidate"].as_str().unwrap_or_default();
        if line.is_empty() {
            return Ok(()); // end of candidates
        }
        let mid = candidate["sdpMid"].as_str().unwrap_or_default();
        let index = candidate["sdpMLineIndex"].as_i64().unwrap_or(0) as i32;
        self.pc.add_ice_candidate(IceCandidate::parse(mid, index, line)?).await?;
        Ok(())
    }

    fn state(&mut self, state: PeerConnectionState) {
        let up = state == PeerConnectionState::Connected;
        if up != self.connected {
            self.connected = up;
            let _ = self.events.send(CallEvent::Link { user: self.user.clone(), up });
        }
        if state == PeerConnectionState::Failed {
            // The first failure is usually one lost candidate; the second is two networks with
            // no path between them.
            if !self.restarted {
                self.restarted = true;
                self.pc.restart_ice();
            } else {
                let _ = self.events.send(CallEvent::Error(format!("no media path to {}; this call may need a TURN server", self.user)));
            }
        }
    }

    /// Which slots the peer sends on follows from the negotiated descriptions: a started source
    /// turns its m-line to sendrecv, a stopped one back to recvonly. Each side lays its m-lines
    /// down in groups of four in slot order, so the position tells the slot; after glare there are
    /// two groups (the polite side's own transceivers are not reused for the other's offer).
    /// Read from the SDP because the native `current_direction` aborts on a transceiver that
    /// was never negotiated.
    fn announce_tracks(&mut self) {
        if self.pc.signaling_state() != SignalingState::Stable {
            return;
        }
        let Some(sdp) = &self.remote_sdp else { return };
        let lines = remote_sending(sdp);
        let transceivers = self.pc.transceivers();
        for slot in Slot::ALL {
            let sending = lines.iter().enumerate().find(|(i, (_, sends))| i % 4 == slot as usize && *sends).map(|(_, (mid, _))| mid.clone());
            let now = sending.is_some();
            if now == self.receiving[slot as usize] {
                continue;
            }
            let from = self.user.clone();
            if let Some(mid) = sending {
                let Some(track) = transceivers.iter().find(|t| t.mid().as_deref() == Some(mid.as_str())).and_then(|t| t.receiver().track()) else {
                    continue;
                };
                let _ = self.events.send(CallEvent::Track { from, slot, track });
            } else {
                let _ = self.events.send(CallEvent::TrackEnded { from, slot });
            }
            self.receiving[slot as usize] = now;
        }
    }
}

/// The m-lines of a description in order: their mid and whether their author sends on them.
fn remote_sending(sdp: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut section: Option<(Option<String>, bool, bool)> = None; // (mid, rejected, sends)
    let mut finish = |section: Option<(Option<String>, bool, bool)>| {
        if let Some((mid, rejected, sends)) = section {
            out.push((mid.unwrap_or_default(), sends && !rejected));
        }
    };
    for line in sdp.lines().map(str::trim_end) {
        if let Some(m) = line.strip_prefix("m=") {
            finish(section.take());
            let rejected = m.split_whitespace().nth(1) == Some("0");
            section = Some((None, rejected, true));
        } else if let Some((mid, _, sends)) = section.as_mut() {
            if let Some(value) = line.strip_prefix("a=mid:") {
                *mid = Some(value.to_owned());
            } else if matches!(line, "a=recvonly" | "a=inactive") {
                *sends = false;
            } else if matches!(line, "a=sendrecv" | "a=sendonly") {
                *sends = true;
            }
        }
    }
    finish(section);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn remote_sending_reads_directions_per_mid() {
        let sdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\na=sendrecv\r\nm=video 9 UDP 96\r\na=mid:1\r\na=recvonly\r\nm=video 0 UDP 96\r\na=mid:2\r\na=sendrecv\r\nm=audio 9 UDP 111\r\na=mid:3\r\n";
        let sending = super::remote_sending(sdp);
        let sends: Vec<bool> = sending.iter().map(|(_, s)| *s).collect();
        assert_eq!(sending[1].0, "1");
        assert_eq!(sends, [true, false, false, true], "a rejected m-line does not send, sendrecv is the default");
    }
}
