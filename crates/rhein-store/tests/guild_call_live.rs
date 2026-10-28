//! An end-to-end encrypted guild call on the dev SFU (`RHEIN_LIVE`, `scripts/dev-sfu.sh`,
//! `RHEIN_SFU=1 scripts/dev-server.sh a`), as Telium #330 has it. Two Rhein accounts join a
//! guild voice channel, each offering a key package:
//! - the first founds the call's MLS group, the second waits;
//! - the first, as committer, adds the second, which joins from the welcome in `VOICE_E2EE`;
//! - both hold the same media key, and a tone crosses the SFU sealed with it;
//! - when the second leaves, the committer removes it and the key moves on.
//!
//! Also: without a key package the server refuses the join.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rhein_auth::{DeviceKey, SerenityKsf};
use rhein_mls::vault;
use rhein_net::gateway::{self, GatewayConfig, GatewayHandle};
use rhein_net::{Api, GatewayEvent, auth, discover};
use rhein_proto::{Event, PresenceState, USER_INTENTS, VoiceServer};
use rhein_store::conversations::Engine;
use rhein_store::dm::{Secrets, bring_up};
use rhein_store::guild_keys::{GuildKeys, Heard, Verifier};
use rhein_voice::audio::AudioEngine;
use rhein_voice::livekit::webrtc::media_stream_track::MediaStreamTrack;
use rhein_voice::sfu::{self, SfuCall};
use rhein_voice::{CallEvent, Slot, e2ee};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc::UnboundedReceiver};

const PASSWORD: &str = "correct horse battery";

async fn register(home: &Api, localpart: &str) -> auth::SignedIn {
    let ksf = SerenityKsf::argon2id(1, 8, 1);
    for _ in 0..20 {
        match auth::register(home, localpart, PASSWORD, &DeviceKey::generate(), "rhein guild call test", &ksf).await {
            Ok(signed) => return signed,
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
    panic!("could not register {localpart}");
}

/// One account: its home, its DM engine (for the cross-signing check) and its MLS key.
struct Person {
    user: String,
    device: String,
    home: Api,
    engine: Arc<Mutex<Engine>>,
    signature: (Vec<u8>, Vec<u8>),
}

async fn person(http: &reqwest::Client, doc: &rhein_proto::Discovery, localpart: &str) -> Person {
    let home = Api::new(http.clone(), &doc.api, None);
    let signed = register(&home, localpart).await;
    let (user, device) = (signed.session.user_id.clone(), signed.session.device_id.clone());
    bring_up(&home, &user, &device, Some(vault::recovery_wrapper(&signed.export_key))).await.expect("keys");
    let secrets = Secrets::load(&user);
    let (private, public) = secrets.mls_signature.clone().unwrap();
    let signature = (vault::unb64(&private).unwrap(), vault::unb64(&public).unwrap());
    let engine = Engine::open(&user, &device, (&signature.0, &signature.1), secrets.keyring.clone()).unwrap();
    Person { user, device, home, engine: Arc::new(Mutex::new(engine)), signature }
}

impl Person {
    fn verifier(&self) -> Verifier {
        let (engine, api) = (self.engine.clone(), self.home.clone());
        Arc::new(move |user, device, key| {
            let (engine, api) = (engine.clone(), api.clone());
            Box::pin(async move { engine.lock().await.verify_claimed(&api, &user, &device, &key).await != Some(false) })
        })
    }

    async fn guild(&self, http: &reqwest::Client, doc: &rhein_proto::Discovery) -> (Api, GatewayHandle, UnboundedReceiver<GatewayEvent>) {
        let api = Api::new(http.clone(), &doc.api, None);
        let session = auth::guild_session(&self.home, &api, &doc.server_name).await.expect("guild session");
        let (handle, mut events) = gateway::connect(GatewayConfig {
            url: session.gateway.clone().or(doc.gateway.clone()).expect("gateway"),
            token: session.session_token.clone(),
            intents: USER_INTENTS,
            presence: PresenceState { status: "online".into(), ..Default::default() },
            voice: Vec::new(),
            proxy: None,
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = events.recv().await {
                if matches!(event, GatewayEvent::Ready(_)) {
                    return;
                }
            }
        })
        .await
        .expect("gateway ready");
        (api, handle, events)
    }
}

/// The next `VOICE_E2EE` event on a gateway.
async fn next_e2ee(events: &mut UnboundedReceiver<GatewayEvent>) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(event) = events.recv().await {
            if let GatewayEvent::Dispatch(event) = event
                && let Event::VoiceE2ee { data, .. } = *event
            {
                return data;
            }
        }
        panic!("the gateway ended");
    })
    .await
    .expect("a VOICE_E2EE event")
}

fn is_tone(samples: &[i16], hz: f64) -> bool {
    let (mut re, mut im, mut energy) = (0f64, 0f64, 0f64);
    for (i, &x) in samples.iter().enumerate() {
        let t = 2. * std::f64::consts::PI * hz * i as f64 / 48_000.;
        re += x as f64 * t.cos();
        im += x as f64 * t.sin();
        energy += (x as f64).powi(2);
    }
    let peak = samples.iter().map(|s| s.saturating_abs()).max().unwrap_or(0);
    peak > 2000 && 2. * (re * re + im * im) / (energy.max(1.) * samples.len() as f64) > 0.9
}

/// Two accounts in an encrypted guild call on the SFU, keys settled: a founded the group and
/// committed b in, b joined from the welcome. `None` without `RHEIN_LIVE` or an SFU region.
struct Encrypted {
    a: Person,
    b: Person,
    voice: String,
    a_api: Api,
    b_api: Api,
    ka: GuildKeys,
    key: (Vec<u8>, u64),
    b_key: (Vec<u8>, u64),
    a_room: rhein_proto::SfuSession,
    b_room: rhein_proto::SfuSession,
    _gateways: (GatewayHandle, GatewayHandle),
}

static CONFIG: std::sync::Once = std::sync::Once::new();

async fn encrypted_call(prefix: &str) -> Option<Encrypted> {
    let Ok(server) = std::env::var("RHEIN_LIVE") else {
        eprintln!("RHEIN_LIVE not set; skipping");
        return None;
    };
    CONFIG.call_once(|| {
        let dir = std::env::temp_dir().join(format!("rhein-guild-call-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: set once, before any test of this binary reads it.
        unsafe { std::env::set_var("RHEIN_CONFIG_DIR", &dir) };
    });
    let http = reqwest::Client::new();
    let doc = discover(&http, &server).await.expect("discovery");
    let tag = std::process::id();
    let (a, b) = (person(&http, &doc, &format!("{prefix}a{tag}")).await, person(&http, &doc, &format!("{prefix}b{tag}")).await);

    // A guild with a voice channel, both in it.
    let (setup, _, _) = a.guild(&http, &doc).await;
    let guild = setup.create_guild("Rhein encrypted voice").await.unwrap();
    let voice = setup.create_channel(&guild.id, "Lounge", "voice", None).await.unwrap();
    let invite = setup.create_invite(&guild.id, None).await.unwrap();
    let (b_setup, _, _) = b.guild(&http, &doc).await;
    b_setup.join_invite(invite["code"].as_str().unwrap()).await.unwrap();
    let (a_api, a_gw, mut a_events) = a.guild(&http, &doc).await;
    let (b_api, b_gw, mut b_events) = b.guild(&http, &doc).await;

    // Without a key package an SFU channel refuses the join.
    match b_api.voice_join(&voice.id, false, false, false, None, None).await {
        Err(err) if err.status() == Some(400) => {}
        Ok(joined) if matches!(joined.voice_server, VoiceServer::Mesh { .. }) => {
            eprintln!("the server has no SFU region; skipping");
            return None;
        }
        other => panic!("expected a 400 without a key package, got {:?}", other.map(|j| j.voice_state)),
    }

    // a founds the call's group.
    let mut ka = GuildKeys::new(&a.user, &a.device, (&a.signature.0, &a.signature.1), &voice.id, a_api.clone(), a.verifier()).unwrap();
    let joined = a_api.voice_join(&voice.id, false, false, false, None, Some(&ka.offer().unwrap())).await.expect("a joins");
    let info = joined.e2ee.clone().expect("an SFU call names its group");
    assert!(info.member, "the first one in founds the group");
    assert_eq!(ka.begin(&info).await.unwrap().expect("the founder holds a key").1, 0);
    let VoiceServer::Sfu { endpoint, token, e2ee: required, .. } = joined.voice_server else { panic!("an SFU call") };
    assert_eq!(required.as_deref(), Some("required"));
    let a_room = sfu::session(&http, &endpoint, &token).await.unwrap();

    // b waits with its package.
    let mut kb = GuildKeys::new(&b.user, &b.device, (&b.signature.0, &b.signature.1), &voice.id, b_api.clone(), b.verifier()).unwrap();
    let joined = b_api.voice_join(&voice.id, false, false, false, None, Some(&kb.offer().unwrap())).await.expect("b joins");
    let info = joined.e2ee.clone().unwrap();
    assert!(!info.member);
    assert!(kb.begin(&info).await.unwrap().is_none(), "no key before the welcome");
    let VoiceServer::Sfu { endpoint, token, .. } = joined.voice_server else { panic!("an SFU call") };
    let b_room = sfu::session(&http, &endpoint, &token).await.unwrap();

    // a is the committer: it adds b; b joins from the welcome the server relays.
    let roster: Vec<Value> = a_api.voice_channel(&voice.id).await.unwrap()["voice_states"].as_array().unwrap().clone();
    let key = ka.tend(&roster).await.unwrap().expect("a commits b in");
    assert_eq!(key.1, 1);
    let event = next_e2ee(&mut b_events).await;
    assert_eq!(event["op"], "commit");
    let Heard::Keyed(b_key) = kb.hear(&event).await.unwrap() else { panic!("b joins from the welcome") };
    assert_eq!(b_key, key, "one key for both");
    // a's own commit comes back to it too, and changes nothing.
    let echo = next_e2ee(&mut a_events).await;
    assert_eq!(ka.hear(&echo).await.unwrap(), Heard::Same);
    Some(Encrypted { a, b, voice: voice.id, a_api, b_api, ka, key, b_key, a_room, b_room, _gateways: (a_gw, b_gw) })
}

/// Feeds a 440 Hz tone into an engine's microphone in real time until the flag drops.
fn speak(engine: Arc<AudioEngine>, speaking: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let tone: Vec<i16> = (0..480 * 100).map(|i| ((i as f32 * 2. * std::f32::consts::PI * 440. / 48_000.).sin() * 8000.) as i16).collect();
        let mut chunks = tone.chunks(480).cycle();
        while speaking.load(Ordering::Relaxed) {
            engine.feed(chunks.next().unwrap());
            std::thread::sleep(Duration::from_millis(10));
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_guild_call_on_the_sfu_is_encrypted_end_to_end() {
    let Some(mut call) = encrypted_call("ga").await else { return };

    // Both in the room, sealed with the group's key: b hears a's tone.
    let engines = [Arc::new(AudioEngine::headless().unwrap()), Arc::new(AudioEngine::headless().unwrap())];
    let (a_call, _a_media) = SfuCall::connect(call.a_room.clone(), engines[0].microphone(), Some(e2ee::key_provider(&call.key.0, call.key.1))).await.unwrap();
    // As the client does it when the welcome comes after the room: b joins with a key nobody
    // has, and the group's key goes in once it is known.
    let b_provider = e2ee::key_provider(&rhein_mls::vault::rand_bytes::<32>(), 0);
    let (mut b_call, mut b_media) = SfuCall::connect(call.b_room.clone(), engines[1].microphone(), Some(b_provider.clone())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    b_call.rekey(&b_provider, &call.b_key.0, call.b_key.1);
    let speaking = Arc::new(AtomicBool::new(true));
    let feeder = speak(engines[0].clone(), speaking.clone());
    let track = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(event) = b_media.recv().await {
            if let CallEvent::Track { slot: Slot::Mic, track: MediaStreamTrack::Audio(track), .. } = event {
                return track;
            }
        }
        panic!("the call ended");
    })
    .await
    .expect("b gets a's microphone");
    engines[1].add_voice(&call.a.user, track);
    let tap = engines[1].tap();
    let heard = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match tap.try_recv() {
                Ok(frame) if is_tone(&frame, 440.) => return true,
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(heard, "b hears a through the SFU, decrypted with the call group's key");
    speaking.store(false, Ordering::Relaxed);
    feeder.join().unwrap();

    // b leaves: the committer removes it, and the key moves on without b.
    b_call.close().await;
    call.b_api.voice_leave(&call.voice).await.unwrap();
    let roster: Vec<Value> = call.a_api.voice_channel(&call.voice).await.unwrap()["voice_states"].as_array().unwrap().clone();
    let next = call.ka.tend(&roster).await.unwrap().expect("a removes b");
    assert_eq!(next.1, 2);
    assert_ne!(next.0, call.key.0);
    a_call.close().await;
    call.a_api.voice_leave(&call.voice).await.ok();
}

/// What b saw of a's screen: the widest picture, and whether any showed the pattern (bright,
/// with its white bar) rather than black.
struct Seen {
    widest: (u32, u32),
    patterned: bool,
    black: usize,
    pictures: usize,
    /// Pictures after the SFU's own check of what everyone sends (every 10 s): a track over
    /// its ceiling is muted there, and the viewer is left with black.
    late: usize,
}

fn looks_like_the_pattern(picture: &rhein_voice::video::Picture) -> (bool, bool) {
    let (pixels, _) = picture.bgra.as_chunks::<4>();
    let n = (picture.bgra.len() / 4).max(1);
    let (mut sum, mut white) = (0u64, 0usize);
    for p in pixels {
        let luma = (p[0] as u64 + p[1] as u64 + p[2] as u64) / 3;
        sum += luma;
        if p[0] > 220 && p[1] > 220 && p[2] > 220 {
            white += 1;
        }
    }
    let mean = sum / n as u64;
    (mean < 8, mean > 40 && white * 100 / n >= 2)
}

/// a shares a `width`×`height` test pattern in an encrypted guild call; b watches, for a while.
async fn share_and_look(tag: &str, width: u32, height: u32, check_early: bool) -> Option<Seen> {
    use rhein_voice::video::{TestPattern, VideoFeed, VideoOut};
    let call = encrypted_call(tag).await?;
    let engines = [AudioEngine::headless().unwrap(), AudioEngine::headless().unwrap()];
    let (mut a_call, _a_media) = SfuCall::connect(call.a_room.clone(), engines[0].microphone(), Some(e2ee::key_provider(&call.key.0, call.key.1))).await.unwrap();
    let (mut b_call, mut b_media) = SfuCall::connect(call.b_room.clone(), engines[1].microphone(), Some(e2ee::key_provider(&call.b_key.0, call.b_key.1))).await.unwrap();
    let out = VideoOut::new(width, height, true);
    let _pattern = TestPattern::start(out.clone(), width, height);
    call.a_api.voice_stream(&call.voice, true, Some("detail")).await.expect("stream state");
    a_call.publish_video(Slot::Screen, &out).await.expect("publish the screen");

    if check_early {
        let early = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(event) = b_media.recv().await {
                if matches!(event, CallEvent::Track { slot: Slot::Screen, .. }) {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(!early, "a screen share is not forwarded before Watch");
    }

    call.b_api.voice_watch(&call.voice, &call.a.user, true, None).await.expect("watch");
    b_call.watch(&call.a.user, true);
    let track = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(event) = b_media.recv().await {
            if let CallEvent::Track { slot: Slot::Screen, track: MediaStreamTrack::Video(track), .. } = event {
                return track;
            }
        }
        panic!("the call ended");
    })
    .await
    .expect("the screen arrives once watched");
    let feed = VideoFeed::start(track, 4096, 30);
    let mut seen = Seen { widest: (0, 0), patterned: false, black: 0, pictures: 0, late: 0 };
    let mut last = 0;
    let began = tokio::time::Instant::now();
    let until = began + Duration::from_secs(20);
    while tokio::time::Instant::now() < until {
        if let Some((n, picture)) = feed.latest()
            && n != last
        {
            last = n;
            seen.pictures += 1;
            if began.elapsed() > Duration::from_secs(14) {
                seen.late += 1;
            }
            if picture.width > seen.widest.0 {
                seen.widest = (picture.width, picture.height);
            }
            let (black, patterned) = looks_like_the_pattern(&picture);
            seen.black += black as usize;
            seen.patterned |= patterned;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    let _ = &call.b;
    eprintln!("{width}x{height}: {} pictures ({} late), widest {:?}, {} black, pattern seen: {}", seen.pictures, seen.late, seen.widest, seen.black, seen.patterned);
    a_call.close().await;
    b_call.close().await;
    Some(seen)
}

/// A screen share in an encrypted guild call: not forwarded until b watches, then the picture.
#[tokio::test(flavor = "multi_thread")]
async fn a_screen_share_waits_for_watch() {
    let Some(seen) = share_and_look("gw", 640, 360, true).await else { return };
    assert!(seen.patterned, "the shared screen shows");
}

/// A whole monitor's worth: full size, and the picture, not black.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_hd_screen_share_shows() {
    let Some(seen) = share_and_look("hd", 1920, 1080, true).await else { return };
    assert!(seen.patterned, "the shared screen shows, not black");
    assert_eq!(seen.widest, (1920, 1080), "the viewer gets the full picture");
    assert!(seen.late > 5, "still showing after the SFU checked the ceiling ({} late pictures)", seen.late);
}
