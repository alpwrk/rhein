//! A participant for trying calls by hand: joins a guild's first voice channel through the SFU,
//! end-to-end encrypted like any guild call there, and "speaks" a tone, one second on and one
//! off, until stopped.
//!
//!   cargo run -p rhein-store --example voice_bot -- localhost:8448 @ben…:localhost 'rhein demo password'
//!
//! With `--screen` after the password it also shares a moving test pattern as its screen.
//!
//! Signs in as that account (a new device each time, its keys kept in a temporary directory),
//! follows the call's MLS group over the gateway and, when it is the call's longest-standing
//! member, adds whoever joins. It trusts every device: it is a test participant, not a client.
//! Needs a server with an SFU region (`./rheinrun`).

use std::time::Duration;

use rhein_auth::{DeviceKey, SerenityKsf};
use rhein_mls::vault;
use rhein_net::gateway::{self, GatewayConfig};
use rhein_net::{Api, GatewayEvent, auth, discover};
use rhein_proto::{Event, PresenceState, USER_INTENTS, VoiceServer};
use rhein_store::dm::{Secrets, bring_up};
use rhein_store::guild_keys::{GuildKeys, Heard, trust_all};
use rhein_voice::audio::AudioEngine;
use rhein_voice::e2ee;
use rhein_voice::sfu::{self, SfuCall};
use serde_json::Value;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(server), Some(user), Some(password), screen) = (args.next(), args.next(), args.next(), args.next().is_some_and(|a| a == "--screen")) else {
        anyhow::bail!("usage: voice_bot <server> <user id> <password> [--screen]");
    };
    // The bot's device keys stay out of the real client's configuration.
    let dir = std::env::temp_dir().join(format!("rhein-voice-bot-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    // SAFETY: set before anything reads it; the bot is single-purpose.
    unsafe { std::env::set_var("RHEIN_CONFIG_DIR", &dir) };

    let http = reqwest::Client::new();
    let doc = discover(&http, &server).await?;
    let home = Api::new(http.clone(), &doc.api, None);
    let signed = auth::login(&home, &user, &password, &DeviceKey::generate(), "rhein voice bot", &SerenityKsf::memory_constrained()).await?;
    let (me, device) = (signed.session.user_id.clone(), signed.session.device_id.clone());
    bring_up(&home, &me, &device, Some(vault::recovery_wrapper(&signed.export_key))).await?;
    let (private, public) = Secrets::load(&me).mls_signature.ok_or_else(|| anyhow::anyhow!("no MLS key for this device"))?;
    let (private, public) = (vault::unb64(&private)?, vault::unb64(&public)?);

    let api = Api::new(http.clone(), &doc.api, None);
    let session = auth::guild_session(&home, &api, &doc.server_name).await?;
    let (_gateway, mut gateway_events) = gateway::connect(GatewayConfig {
        url: session.gateway.clone().or(doc.gateway.clone()).ok_or_else(|| anyhow::anyhow!("no gateway"))?,
        token: session.session_token.clone(),
        intents: USER_INTENTS,
        presence: PresenceState { status: "online".into(), ..Default::default() },
        voice: Vec::new(),
        proxy: None,
    });
    let guild = api.guilds().await?.into_iter().next().ok_or_else(|| anyhow::anyhow!("in no guild"))?;
    let channel = api.channels(&guild.id).await?.into_iter().find(|c| c.is_voice()).ok_or_else(|| anyhow::anyhow!("no voice channel"))?;

    let mut keys = GuildKeys::new(&me, &device, (&private, &public), &channel.id, api.clone(), trust_all())?;
    let joined = api.voice_join(&channel.id, false, false, false, None, Some(&keys.offer()?)).await?;
    let VoiceServer::Sfu { endpoint, token, .. } = joined.voice_server else {
        api.voice_leave(&channel.id).await.ok();
        anyhow::bail!("the server answered with a mesh; the bot needs an SFU region");
    };
    let info = joined.e2ee.ok_or_else(|| anyhow::anyhow!("the join named no encryption group"))?;
    let key = keys.begin(&info).await?.unwrap_or_else(|| (vault::rand_bytes::<32>().to_vec(), 0));
    let provider = e2ee::key_provider(&key.0, key.1);

    let engine = AudioEngine::headless()?;
    let room = sfu::session(&http, &endpoint, &token).await?;
    let (mut call, mut events) = SfuCall::connect(room, engine.microphone(), Some(provider.clone())).await?;
    let _pattern = if screen {
        let out = rhein_voice::video::VideoOut::new(1280, 720, true);
        let pattern = rhein_voice::video::TestPattern::start(out.clone(), 1280, 720);
        api.voice_stream(&channel.id, true, Some("detail 720p@15")).await?;
        call.publish_video(rhein_voice::Slot::Screen, &out).await?;
        Some(pattern)
    } else {
        None
    };
    println!("in #{} of {} (encrypted, group {}); Ctrl-C to leave", channel.name, guild.name, info.group_id);
    tokio::spawn(async move { while events.recv().await.is_some() {} });

    let mut ticker = tokio::time::interval(Duration::from_millis(10));
    let mut n: u64 = 0;
    loop {
        let moved = tokio::select! {
            _ = ticker.tick() => {
                let on = (n / 100).is_multiple_of(2);
                let frame: Vec<i16> = (0..480u64)
                    .map(|i| if on { ((((n * 480 + i) as f32) * 2. * std::f32::consts::PI * 330. / 48_000.).sin() * 6000.) as i16 } else { 0 })
                    .collect();
                engine.feed(&frame);
                n += 1;
                None
            }
            Some(event) = gateway_events.recv() => match event {
                GatewayEvent::Dispatch(event) => match *event {
                    Event::VoiceE2ee { data, .. } => match keys.hear(&data).await? {
                        Heard::Keyed(key) => Some(key),
                        _ => None,
                    },
                    // Someone came or went: the committer changes the group.
                    Event::VoiceStateUpdate(_) => {
                        let here = api.voice_channel(&channel.id).await?;
                        let roster: Vec<Value> = here["voice_states"].as_array().cloned().unwrap_or_default();
                        keys.tend(&roster).await?
                    }
                    _ => None,
                },
                _ => None,
            },
            _ = tokio::signal::ctrl_c() => break,
        };
        if let Some((key, epoch)) = moved {
            println!("key for epoch {epoch}");
            call.rekey(&provider, &key, epoch);
        }
    }
    call.close().await;
    api.voice_leave(&channel.id).await.ok();
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
