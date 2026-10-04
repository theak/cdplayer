//! Shows up in Home Assistant as the Shairport Sync player on the same MQTT topic, via the
//! hass-shairport-sync integration: it publishes the messages shairport-sync would (track
//! info, cover, play/pause/stop) and takes the integration's remote commands.
//!
//! It also announces a "CD Player" device through Home Assistant's MQTT discovery, on its
//! own `cdplayer/` topics: a Disc sensor (on while a disc is in the drive) and an Eject
//! button.
//!
//! shairport-sync hears those commands too. That's fine because the two can't play at
//! once: while a disc is playing or paused every command is ours (AirPlay has no session),
//! and when stopped we only take Play, and only while AirPlay isn't active.

use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, Packet, QoS};
use serde_json::json;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::AppState;
use crate::drive::Disc;
use crate::player::{self, Action, Activity, NowPlaying};

const POLL: Duration = Duration::from_millis(500);
const RETRY: Duration = Duration::from_secs(5);
/// Cover art can be a few hundred KB.
const MAX_PACKET: usize = 8 << 20;
const VOLUME_STEP: u8 = 5;

/// Topics for the discovered device, apart from the Shairport Sync ones.
const AVAILABILITY: &str = "cdplayer/availability";
const DISC: &str = "cdplayer/disc";
const EJECT: &str = "cdplayer/eject";
const DISCOVERY_PREFIX: &str = "homeassistant";
/// Home Assistant says "online" here when it starts, which is the cue to announce again.
const HA_STATUS: &str = "homeassistant/status";

/// Where to connect, parsed from `mqtt://user:password@host:port`.
#[derive(Debug, PartialEq, Eq)]
pub struct Broker {
    host: String,
    port: u16,
    credentials: Option<(String, String)>,
}

impl Broker {
    pub fn parse(url: &str) -> Option<Self> {
        let rest = url.strip_prefix("mqtt://")?.trim_end_matches('/');
        // The password may contain '@', so split at the last one.
        let (credentials, address) = match rest.rsplit_once('@') {
            Some((creds, address)) => {
                let (user, password) = creds.split_once(':').unwrap_or((creds, ""));
                (Some((user.to_string(), password.to_string())), address)
            }
            None => (None, rest),
        };
        let (host, port) = match address.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().ok()?),
            None => (address, 1883),
        };
        (!host.is_empty()).then(|| Broker {
            host: host.to_string(),
            port,
            credentials,
        })
    }
}

/// Keep the MQTT link up with the current settings, reconnecting when they change.
pub async fn run(state: AppState) {
    loop {
        let (broker, topic) = {
            let cfg = state.config.read().await;
            (Broker::parse(&cfg.mqtt_broker), cfg.mqtt_topic.clone())
        };
        match broker {
            Some(broker) => link(&state, broker, &topic).await,
            None => state.mqtt_reload.notified().await,
        }
    }
}

/// On shutdown, give the link a moment to report that playback stopped.
pub async fn settle(state: &AppState) {
    if !state.config.read().await.mqtt_broker.is_empty() {
        tokio::time::sleep(POLL * 3).await;
    }
}

enum Incoming {
    Connected,
    Message(String, Vec<u8>),
}

/// One connection's lifetime: until the settings change.
async fn link(state: &AppState, broker: Broker, topic: &str) {
    eprintln!("cdplayer: MQTT: connecting to {}:{} as {topic}", broker.host, broker.port);
    let mut options = MqttOptions::new("cdplayer", broker.host, broker.port);
    options.set_keep_alive(Duration::from_secs(30));
    options.set_max_packet_size(MAX_PACKET, MAX_PACKET);
    // If we drop off without saying goodbye, the broker marks the device offline.
    options.set_last_will(LastWill::new(AVAILABILITY, "offline", QoS::AtLeastOnce, true));
    if let Some((user, password)) = broker.credentials {
        options.set_credentials(user, password);
    }
    let (client, mut eventloop) = AsyncClient::new(options, 64);

    // Drive the connection on its own task, so publishing never waits on it.
    let (tx, mut rx) = mpsc::channel(64);
    let driver = tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    eprintln!("cdplayer: MQTT: connected");
                    let _ = tx.send(Incoming::Connected).await;
                }
                Ok(Event::Incoming(Packet::Publish(p))) => {
                    let _ = tx.send(Incoming::Message(p.topic, p.payload.to_vec())).await;
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("cdplayer: MQTT: {e}; retrying in {}s", RETRY.as_secs());
                    tokio::time::sleep(RETRY).await;
                }
            }
        }
    });

    let remote = format!("{topic}/remote");
    let (active_start, active_end) = (format!("{topic}/active_start"), format!("{topic}/active_end"));
    let mut reported = NowPlaying::default();
    let mut reported_disc = None;
    let mut airplay_active = false;
    let mut poll = tokio::time::interval(POLL);
    loop {
        tokio::select! {
            _ = state.mqtt_reload.notified() => break,
            Some(incoming) = rx.recv() => match incoming {
                Incoming::Connected => {
                    for t in [remote.as_str(), &active_start, &active_end, EJECT, HA_STATUS] {
                        let _ = client.subscribe(t, QoS::AtMostOnce).await;
                    }
                    announce(&client).await;
                    // A fresh session: report everything again.
                    reported = NowPlaying::default();
                    reported_disc = None;
                }
                Incoming::Message(t, payload) if t == HA_STATUS => {
                    // Home Assistant restarted: announce again and resend the disc state.
                    if payload == b"online" {
                        announce(&client).await;
                        reported_disc = None;
                        poll.reset_immediately();
                    }
                }
                Incoming::Message(t, _) if t == EJECT => {
                    if let Err(e) = player::control(state, Action::Eject).await {
                        eprintln!("cdplayer: MQTT: eject: {e}");
                    }
                    poll.reset_immediately();
                }
                // Only shairport-sync sends active_start; both of us send active_end.
                Incoming::Message(t, _) if t == active_start => airplay_active = true,
                Incoming::Message(t, _) if t == active_end => airplay_active = false,
                Incoming::Message(_, payload) => {
                    command(state, &String::from_utf8_lossy(&payload), airplay_active).await;
                    poll.reset_immediately();
                }
            },
            _ = poll.tick() => {
                let (now, disc) = {
                    let p = state.player.lock().await;
                    (p.now_playing(), p.disc())
                };
                let disc = disc_payload(disc);
                if reported_disc != Some(disc) {
                    publish_retained(&client, DISC, disc).await;
                    reported_disc = Some(disc);
                }
                for (subtopic, payload) in messages(&reported, &now) {
                    publish(&client, topic, subtopic, payload.into_bytes()).await;
                }
                if needs_cover(&reported, &now) {
                    tokio::spawn(send_cover(state.clone(), client.clone(), topic.to_string(), now.cover.clone()));
                }
                reported = now;
            }
        }
    }
    // A clean disconnect doesn't trigger the last will, so say it ourselves.
    publish_retained(&client, AVAILABILITY, "offline").await;
    driver.abort();
    let _ = client.disconnect().await;
}

/// Tell Home Assistant about the device's entities (retained, so it still knows them after
/// a restart), and that it's online.
async fn announce(client: &AsyncClient) {
    for (topic, config) in discovery() {
        publish_retained(client, &topic, &config).await;
    }
    publish_retained(client, AVAILABILITY, "online").await;
}

/// The discovery config topics and payloads for the Disc sensor and the Eject button.
pub(crate) fn discovery() -> Vec<(String, String)> {
    let device = json!({
        "identifiers": ["cdplayer"],
        "name": "CD Player",
        "sw_version": env!("CARGO_PKG_VERSION"),
    });
    let disc = json!({
        "name": "Disc",
        "unique_id": "cdplayer_disc",
        "icon": "mdi:disc",
        "state_topic": DISC,
        "availability_topic": AVAILABILITY,
        "device": device,
    });
    let eject = json!({
        "name": "Eject",
        "unique_id": "cdplayer_eject",
        "icon": "mdi:eject",
        "command_topic": EJECT,
        "availability_topic": AVAILABILITY,
        "device": device,
    });
    vec![
        (format!("{DISCOVERY_PREFIX}/binary_sensor/cdplayer/disc/config"), disc.to_string()),
        (format!("{DISCOVERY_PREFIX}/button/cdplayer/eject/config"), eject.to_string()),
    ]
}

/// The Disc sensor's state: on for any disc, audio or data.
pub(crate) fn disc_payload(disc: Disc) -> &'static str {
    if disc.has_media() { "ON" } else { "OFF" }
}

async fn publish_retained(client: &AsyncClient, topic: &str, payload: &str) {
    if let Err(e) = client.publish(topic, QoS::AtLeastOnce, true, payload.as_bytes().to_vec()).await {
        eprintln!("cdplayer: MQTT: couldn't publish {topic}: {e}");
    }
}

async fn publish(client: &AsyncClient, topic: &str, subtopic: &str, payload: Vec<u8>) {
    if let Err(e) = client.publish(format!("{topic}/{subtopic}"), QoS::AtMostOnce, false, payload).await {
        eprintln!("cdplayer: MQTT: couldn't publish {subtopic}: {e}");
    }
}

/// Act on a Home Assistant remote command (see the module docs for whose it is).
async fn command(state: &AppState, command: &str, airplay_active: bool) {
    let (activity, can_start, volume) = {
        let p = state.player.lock().await;
        (p.now_playing().activity, p.can_start(), p.volume())
    };
    let action = match (command.trim(), activity) {
        ("play", Activity::Idle) if can_start && !airplay_active => Action::PlayPause,
        (_, Activity::Idle) => return,
        ("play", Activity::Paused) | ("pause", Activity::Playing) => Action::PlayPause,
        ("stop", _) => Action::Stop,
        ("nextitem", _) => Action::Next,
        ("previtem", _) => Action::Previous,
        ("volumeup", _) => {
            player::set_volume(state, volume.saturating_add(VOLUME_STEP)).await;
            return;
        }
        ("volumedown", _) => {
            player::set_volume(state, volume.saturating_sub(VOLUME_STEP)).await;
            return;
        }
        _ => return,
    };
    if let Err(e) = player::control(state, action).await {
        eprintln!("cdplayer: MQTT: {command}: {e}");
    }
}

/// What to publish, in order, to move Home Assistant from `prev` to `now`. The
/// integration only refreshes on state messages, so track info goes first and is followed
/// by one. Pausing sends `play_flush` rather than `play_end`, which automations take to
/// mean playback is over.
pub(crate) fn messages(prev: &NowPlaying, now: &NowPlaying) -> Vec<(&'static str, String)> {
    use Activity::*;
    let mut out = Vec::new();
    if now.activity == Idle {
        if prev.activity != Idle {
            out.push(("play_end", String::new()));
            out.push(("active_end", String::new()));
        }
        return out;
    }
    let info_changed =
        prev.activity == Idle || (&prev.title, &prev.artist, &prev.album) != (&now.title, &now.artist, &now.album);
    if info_changed {
        out.push(("title", now.title.clone()));
        out.push(("artist", now.artist.clone()));
        out.push(("album", now.album.clone()));
    }
    let state = match (prev.activity, now.activity) {
        (Idle, Playing) => Some("play_start"),
        (_, Playing) if prev.activity != Playing || info_changed => Some("play_resume"),
        (_, Paused) if prev.activity != Paused || info_changed => Some("play_flush"),
        _ => None,
    };
    out.extend(state.map(|s| (s, String::new())));
    out
}

/// Whether to (re)send the cover: Home Assistant drops it whenever the player goes idle.
pub(crate) fn needs_cover(prev: &NowPlaying, now: &NowPlaying) -> bool {
    now.activity != Activity::Idle && (prev.activity == Activity::Idle || prev.cover != now.cover)
}

/// Publish the cover's image bytes (an empty message clears it).
async fn send_cover(state: AppState, client: AsyncClient, topic: String, cover: Option<String>) {
    let image = match cover.as_deref() {
        None => Vec::new(),
        Some(url) => match url.strip_prefix("/api/art/") {
            Some(disc_id) => state
                .player
                .lock()
                .await
                .art(disc_id)
                .map(|art| art.data.to_vec())
                .unwrap_or_default(),
            None => match fetch(&state.client, url).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    eprintln!("cdplayer: MQTT: couldn't fetch cover {url}: {e}");
                    Vec::new()
                }
            },
        },
    };
    publish(&client, &topic, "cover", image).await;
}

async fn fetch(client: &reqwest::Client, url: &str) -> reqwest::Result<Vec<u8>> {
    let response = client.get(url).send().await?.error_for_status()?;
    Ok(response.bytes().await?.to_vec())
}
