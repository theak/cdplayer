//! Router, settings, webhook, and track-list tests. None touch real hardware: the drive path
//! doesn't exist, so the player always sees `Disc::Missing`.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::post,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

use crate::config::{self, Config};
use crate::album;
use crate::datadisc;
use crate::iso9660;
use crate::drive::{Toc, Track};
use crate::{AppState, Settings, build_router, playback, player};

/// A fresh, empty data dir per test.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cdplayer-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn test_state(name: &str) -> AppState {
    AppState::new(&Settings {
        cd_device: "/nonexistent/sr0".into(),
        audio_device: "null".into(),
        data_dir: temp_dir(name),
        port: 0,
    })
}

async fn send(app: Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn serves_page_and_assets() {
    let app = build_router(test_state("assets"));
    for (uri, ct) in [
        ("/", "text/html"),
        ("/static/style.css", "text/css"),
        ("/static/script.js", "application/javascript"),
        ("/static/icon.svg", "image/svg+xml"),
    ] {
        let resp = app
            .clone()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        let got = resp.headers()["content-type"].to_str().unwrap();
        assert!(got.starts_with(ct), "{uri}: {got}");
    }
    let resp = app
        .oneshot(Request::get("/static/nope.js").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn status_without_drive() {
    let state = test_state("status");
    player::tick(&state).await;
    let (code, body) = send(build_router(state), "GET", "/api/status", None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["drive"], "missing");
    assert_eq!(body["state"], "idle");
    assert_eq!(body["track"], Value::Null);
}

#[tokio::test]
async fn controls_without_disc() {
    let app = build_router(test_state("controls"));
    let (code, body) = send(app.clone(), "POST", "/api/control/playpause", None).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["error"], "Nothing to play in the drive");

    let (code, _) = send(app.clone(), "POST", "/api/control/next", None).await;
    assert_eq!(code, StatusCode::CONFLICT);

    // Stop with nothing playing is a harmless no-op.
    let (code, _) = send(app.clone(), "POST", "/api/control/stop", None).await;
    assert_eq!(code, StatusCode::OK);

    let (code, _) = send(app, "POST", "/api/control/rewind", None).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn settings_round_trip() {
    let state = test_state("settings");
    let path = state.config_path.as_ref().clone();
    let app = build_router(state);

    let (_, body) = send(app.clone(), "GET", "/api/config", None).await;
    assert_eq!(body["start_webhook"], "");
    assert_eq!(body["eject_when_finished"], true);
    assert_eq!(body["stop_after_paused_minutes"], 5);

    let new = json!({
        "start_webhook": "  http://ha.local:8123/api/webhook/cd-start ",
        "stop_webhook": "https://ha.local/api/webhook/cd-stop",
        "eject_when_finished": false,
        "stop_after_paused_minutes": 0,
    });
    let (code, body) = send(app.clone(), "POST", "/api/config", Some(new)).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["start_webhook"], "http://ha.local:8123/api/webhook/cd-start");

    // Persisted, and served back.
    let saved = config::load(&path);
    assert_eq!(saved.start_webhook, "http://ha.local:8123/api/webhook/cd-start");
    assert!(!saved.eject_when_finished);
    assert_eq!(saved.stop_after_paused_minutes, 0);
    let (_, body) = send(app, "GET", "/api/config", None).await;
    assert_eq!(body["stop_webhook"], "https://ha.local/api/webhook/cd-stop");
}

#[tokio::test]
async fn volume_persists_and_survives_settings_saves() {
    let state = test_state("volume");
    let path = state.config_path.as_ref().clone();
    let app = build_router(state);

    let (_, body) = send(app.clone(), "GET", "/api/status", None).await;
    assert_eq!(body["volume"], 100);

    let (code, body) = send(app.clone(), "POST", "/api/volume", Some(json!({ "volume": 40 }))).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["volume"], 40);
    let (_, body) = send(app.clone(), "GET", "/api/status", None).await;
    assert_eq!(body["volume"], 40);
    assert_eq!(config::load(&path).volume, 40);

    // Saving the settings form, which has no volume field, keeps it.
    let form = json!({ "start_webhook": "", "stop_webhook": "", "eject_when_finished": true });
    send(app.clone(), "POST", "/api/config", Some(form)).await;
    assert_eq!(config::load(&path).volume, 40);

    // Out of range is clamped.
    let (_, body) = send(app, "POST", "/api/volume", Some(json!({ "volume": 250 }))).await;
    assert_eq!(body["volume"], 100);
}

#[tokio::test]
async fn seek_needs_playback() {
    let app = build_router(test_state("seek"));
    let (code, _) = send(app.clone(), "POST", "/api/seek", Some(json!({ "seconds": 30.0 }))).await;
    assert_eq!(code, StatusCode::CONFLICT);
    let (code, _) = send(app, "POST", "/api/seek", Some(json!({ "seconds": -1.0 }))).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn settings_reject_bad_urls() {
    let state = test_state("bad-settings");
    let path = state.config_path.as_ref().clone();
    let (code, body) = send(
        build_router(state),
        "POST",
        "/api/config",
        Some(json!({ "start_webhook": "ha.local/webhook" })),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "Start webhook must start with http:// or https://");
    assert!(!path.exists());
}

#[test]
fn config_load_falls_back_to_defaults() {
    let dir = temp_dir("load");
    assert_eq!(config::load(&dir.join("config.json")), Config::default());

    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("partial.json"), r#"{"stop_webhook": "http://x"}"#).unwrap();
    let cfg = config::load(&dir.join("partial.json"));
    assert_eq!(cfg.stop_webhook, "http://x");
    assert!(cfg.eject_when_finished);

    std::fs::write(dir.join("junk.json"), "not json").unwrap();
    assert_eq!(config::load(&dir.join("junk.json")), Config::default());
}

/// A local HTTP server that records the JSON bodies POSTed to it.
async fn webhook_receiver() -> (String, Arc<Mutex<Vec<Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let app = Router::new().route(
        "/hook",
        post(move |axum::Json(body): axum::Json<Value>| {
            let log = log.clone();
            async move { log.lock().unwrap().push(body) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

#[tokio::test]
async fn webhook_test_endpoint() {
    let (url, seen) = webhook_receiver().await;
    let app = build_router(test_state("webhook-test"));

    let (code, _) = send(
        app.clone(),
        "POST",
        "/api/webhook-test",
        Some(json!({ "event": "start", "url": url })),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(*seen.lock().unwrap(), vec![json!({ "event": "start" })]);

    let (code, _) = send(
        app.clone(),
        "POST",
        "/api/webhook-test",
        Some(json!({ "event": "stop", "url": "" })),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);

    // Nothing listening there.
    let (code, _) = send(
        app,
        "POST",
        "/api/webhook-test",
        Some(json!({ "event": "stop", "url": "http://127.0.0.1:1/hook" })),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn notify_uses_configured_url() {
    let (url, seen) = webhook_receiver().await;
    let state = test_state("notify");
    state.config.write().await.stop_webhook = url;

    crate::webhook::notify(&state, crate::webhook::Event::Start).await; // unset: skipped
    crate::webhook::notify(&state, crate::webhook::Event::Stop).await;
    assert_eq!(*seen.lock().unwrap(), vec![json!({ "event": "stop" })]);
}

fn toc(tracks: &[(u32, bool)], leadout: u32) -> Toc {
    Toc {
        first_track: 1,
        tracks: tracks.to_vec(),
        leadout,
    }
}

#[test]
fn audio_tracks_from_toc() {
    // Plain audio CD: each track runs to the next one; the last to the lead-out.
    let plain = toc(&[(0, false), (15000, false), (30000, false)], 40000);
    assert_eq!(
        plain.audio_tracks(),
        vec![
            Track { start: 0, end: 15000 },
            Track { start: 15000, end: 30000 },
            Track { start: 30000, end: 40000 },
        ]
    );

    // Mixed mode: a leading data track is skipped.
    let mixed = toc(&[(0, true), (20000, false)], 35000);
    assert_eq!(mixed.audio_tracks(), vec![Track { start: 20000, end: 35000 }]);

    // Enhanced CD: the last audio track stops short of the session gap before the data.
    let enhanced = toc(&[(0, false), (20000, false), (50000, true)], 60000);
    assert_eq!(
        enhanced.audio_tracks(),
        vec![
            Track { start: 0, end: 20000 },
            Track { start: 20000, end: 50000 - 11_400 },
        ]
    );

    assert!(toc(&[], 0).audio_tracks().is_empty());
}

#[test]
fn musicbrainz_disc_id() {
    // The worked example from https://musicbrainz.org/doc/Disc_ID_Calculation (its
    // offsets include the 150-sector lead-in; ours are raw sector numbers).
    let disc = toc(
        &[150, 15363, 32314, 46592, 63414, 80489].map(|o| (o - 150, false)),
        95462 - 150,
    );
    assert_eq!(disc.musicbrainz_id(), "49HHV7Eb8UKF3aQiNmu1GR8vKTY-");
    assert_eq!(disc.musicbrainz_toc(), "1+6+95462+150+15363+32314+46592+63414+80489");

    // Enhanced CD: the trailing data track is excluded, and the lead-out moves back.
    let enhanced = toc(&[(0, false), (20000, false), (50000, true)], 60000);
    assert_eq!(enhanced.musicbrainz_toc(), "1+2+38750+150+20150");
}

#[test]
fn musicbrainz_response_parsing() {
    let tracks = |titles: &[&str]| -> Value {
        titles
            .iter()
            .map(|t| json!({ "title": t, "artist-credit": [] }))
            .collect()
    };
    let response = json!({
        "releases": [
            {
                // Right medium (disc 2 has our ID) but no cover art.
                "id": "no-art", "title": "Greatest Hits",
                "artist-credit": [{ "name": "A", "joinphrase": " & " }, { "name": "B", "joinphrase": "" }],
                "cover-art-archive": { "front": false },
                "media": [
                    { "discs": [{ "id": "other" }], "tracks": tracks(&["x"]) },
                    { "discs": [{ "id": "DISC" }], "tracks": tracks(&["One", "Two"]) },
                ],
            },
            {
                "id": "with-art", "title": "Greatest Hits (Remaster)",
                "artist-credit": [{ "name": "A", "joinphrase": "" }],
                "cover-art-archive": { "front": true },
                "media": [{
                    "discs": [{ "id": "DISC" }],
                    "tracks": [
                        { "title": "One", "artist-credit": [] },
                        { "title": "Two", "artist-credit": [{ "name": "Guest", "joinphrase": "" }] },
                    ],
                }],
            },
        ],
    });
    let album = album::parse(&response, "DISC", 2).unwrap();
    assert_eq!(album.title, "Greatest Hits (Remaster)"); // the one with cover art wins
    assert_eq!(
        album.cover.as_deref(),
        Some("https://coverartarchive.org/release/with-art/front-500")
    );
    assert_eq!(album.tracks[0].artist, "A"); // falls back to the album artist
    assert_eq!(album.tracks[1].artist, "Guest");

    // Without art anywhere, the medium matching the disc ID is used.
    let no_art = json!({ "releases": [response["releases"][0].clone()] });
    let album = album::parse(&no_art, "DISC", 2).unwrap();
    assert_eq!(album.artist, "A & B");
    assert_eq!(album.cover, None);
    assert_eq!(album.tracks.len(), 2);

    // Fuzzy TOC matches carry no disc IDs; match the medium by track count.
    let fuzzy = json!({ "releases": [{
        "id": "f", "title": "Fuzzy", "artist-credit": [],
        "media": [{ "tracks": tracks(&["a", "b", "c"]) }],
    }]});
    assert_eq!(album::parse(&fuzzy, "DISC", 3).unwrap().title, "Fuzzy");
    assert!(album::parse(&fuzzy, "DISC", 5).is_none());
    assert!(album::parse(&json!({}), "DISC", 3).is_none());
}

#[test]
fn track_lookup() {
    let tracks = [
        Track { start: 150, end: 1000 },
        Track { start: 1000, end: 2000 },
        Track { start: 2000, end: 3000 },
    ];
    assert_eq!(playback::track_index(&tracks, 150), 0);
    assert_eq!(playback::track_index(&tracks, 999), 0);
    assert_eq!(playback::track_index(&tracks, 1000), 1);
    assert_eq!(playback::track_index(&tracks, 2999), 2);
    // Before the first track (the pregap) counts as track 1.
    assert_eq!(playback::track_index(&tracks, 0), 0);
}

/// One ISO 9660 directory record.
fn iso_record(extent: u32, len: u32, is_dir: bool, name: &[u8]) -> Vec<u8> {
    let mut r = vec![0u8; 33];
    r[2..6].copy_from_slice(&extent.to_le_bytes());
    r[6..10].copy_from_slice(&extent.to_be_bytes());
    r[10..14].copy_from_slice(&len.to_le_bytes());
    r[14..18].copy_from_slice(&len.to_be_bytes());
    r[25] = if is_dir { 0x02 } else { 0 };
    r[28] = 1;
    r[32] = name.len() as u8;
    r.extend_from_slice(name);
    if r.len() % 2 == 1 {
        r.push(0);
    }
    r[0] = r.len() as u8;
    r
}

fn ucs2(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_be_bytes).collect()
}

/// A tiny disc image: `B.MP3` and `SUB/A.FLAC` (plus `cover.jpg`), with an optional Joliet
/// tree naming them `Song B.mp3` and `Sub Folder/Track A.flac`.
fn iso_image(joliet: bool) -> Vec<u8> {
    const S: usize = 2048;
    let mut img = vec![0u8; 30 * S];
    let sector = |img: &mut Vec<u8>, n: usize, data: &[u8]| img[n * S..n * S + data.len()].copy_from_slice(data);
    let descriptor = |kind: u8, root: u32, label: &[u8], escape: &[u8]| {
        let mut d = vec![0u8; S];
        d[0] = kind;
        d[1..6].copy_from_slice(b"CD001");
        d[6] = 1;
        d[40..72].fill(b' ');
        d[40..40 + label.len()].copy_from_slice(label);
        d[88..88 + escape.len()].copy_from_slice(escape);
        d[156..190].copy_from_slice(&iso_record(root, S as u32, true, &[0]));
        d[813..829].copy_from_slice(b"2001020304050600");
        d
    };
    let dir = |own: u32, parent: u32, entries: &[Vec<u8>]| {
        let mut d = iso_record(own, S as u32, true, &[0]);
        d.extend(iso_record(parent, S as u32, true, &[1]));
        entries.iter().for_each(|e| d.extend(e));
        d
    };
    // Files' data.
    sector(&mut img, 24, b"mp3!");
    sector(&mut img, 26, b"flac");
    sector(&mut img, 27, b"jpeg");

    sector(&mut img, 16, &descriptor(1, 20, b"MY_DISC", b""));
    sector(&mut img, 20, &dir(20, 20, &[iso_record(24, 3000, false, b"B.MP3;1"), iso_record(21, S as u32, true, b"SUB")]));
    sector(&mut img, 21, &dir(21, 20, &[iso_record(26, 10, false, b"A.FLAC;1"), iso_record(27, 4, false, b"COVER.JPG;1")]));
    let terminator = if joliet {
        let mut label = ucs2("My Disc");
        label.resize(32, 0);
        sector(&mut img, 17, &descriptor(2, 22, &label, b"%/E"));
        sector(&mut img, 22, &dir(22, 22, &[iso_record(24, 3000, false, &ucs2("Song B.mp3;1")), iso_record(23, S as u32, true, &ucs2("Sub Folder"))]));
        sector(&mut img, 23, &dir(23, 22, &[iso_record(26, 10, false, &ucs2("Track A.flac;1")), iso_record(27, 4, false, &ucs2("cover.jpg"))]));
        18
    } else {
        17
    };
    let mut end = vec![0u8; S];
    end[0] = 255;
    end[1..6].copy_from_slice(b"CD001");
    sector(&mut img, terminator, &end);
    img
}

#[test]
fn iso9660_listing() {
    let plain = iso9660::read_volume(iso_image(false).as_slice()).unwrap();
    assert_eq!(plain.label, "MY_DISC");
    assert_eq!(plain.created, "2001020304050600");
    let paths: Vec<_> = plain.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["B.MP3", "SUB/A.FLAC", "SUB/COVER.JPG"]);
    assert_eq!((plain.files[0].start, plain.files[0].len), (24 * 2048, 3000));

    // With a Joliet tree, its long Unicode names win.
    let joliet = iso9660::read_volume(iso_image(true).as_slice()).unwrap();
    assert_eq!(joliet.label, "My Disc");
    let paths: Vec<_> = joliet.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["Song B.mp3", "Sub Folder/Track A.flac", "Sub Folder/cover.jpg"]);

    assert!(iso9660::read_volume(vec![0u8; 40 * 2048].as_slice()).is_err());
}

#[test]
fn data_disc_tracks() {
    let image = std::sync::Arc::new(iso_image(true));
    let volume = iso9660::read_volume(image.as_slice()).unwrap();
    let files = datadisc::audio_files(&volume);
    let paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["Song B.mp3", "Sub Folder/Track A.flac"]); // cover.jpg isn't audio

    let placeholder = datadisc::placeholder_album(&volume, &files);
    assert_eq!(placeholder.tracks[1].title, "Track A");

    let id = datadisc::disc_id(&volume, &files);
    assert!(id.starts_with("data-"));
    assert_eq!(id, datadisc::disc_id(&volume, &files));
    assert_ne!(id, datadisc::disc_id(&volume, &files[..1]));

    // The "files" aren't real audio, so tags fall back to file names; with no art in the
    // first file's folder, none is found.
    let (album, lengths, art) = datadisc::scan_files(&image, &volume, &files);
    assert_eq!(album.title, "My Disc");
    assert_eq!(album.tracks[0].title, "Song B");
    assert_eq!(lengths, vec![None, None]);
    assert!(art.is_none());
}

#[test]
#[ignore = "needs CDPLAYER_TEST_ISO pointing at a disc image holding audio files"]
fn decodes_disc_image() {
    let path = std::env::var("CDPLAYER_TEST_ISO").expect("CDPLAYER_TEST_ISO");
    let image = std::sync::Arc::new(std::fs::read(path).unwrap());
    let volume = iso9660::read_volume(image.as_slice()).unwrap();
    let files = datadisc::audio_files(&volume);
    assert!(!files.is_empty());

    let (album, lengths, art) = datadisc::scan_files(&image, &volume, &files);
    println!("album: {:?} by {:?}, art: {:?}", album.title, album.artist, art.map(|a| (a.mime, a.data.len())));
    for ((file, info), length) in files.iter().zip(&album.tracks).zip(&lengths) {
        let mut decoder = datadisc::Decoder::open(&image, file).unwrap();
        let (rate, channels) = (decoder.rate, decoder.channels as usize);
        let mut frames = 0;
        while let Some(samples) = decoder.next().unwrap() {
            frames += samples.len() / channels;
        }
        let decoded = frames as f64 / f64::from(rate);
        println!("{}: {:?} / {:?}, {rate} Hz x{channels}, decoded {decoded:.2}s, header says {length:?}",
            file.path, info.title, info.artist);
        assert!(frames > 0);
        if let Some(length) = length {
            assert!((decoded - length).abs() < 0.1, "{}: decoded {decoded} vs {length}", file.path);
        }

        // Seeking halfway leaves about half the file to decode.
        let mut decoder = datadisc::Decoder::open(&image, file).unwrap();
        let landed = decoder.seek(decoded / 2.0).unwrap();
        let mut rest = 0;
        while let Some(samples) = decoder.next().unwrap() {
            rest += samples.len() / channels;
        }
        let rest = rest as f64 / f64::from(rate);
        println!("  seek to {:.2}s landed at {landed:.2}s, {rest:.2}s left", decoded / 2.0);
        assert!((landed - decoded / 2.0).abs() < 0.1);
        assert!((landed + rest - decoded).abs() < 0.1);
    }
}

#[tokio::test]
#[ignore = "plays CDPLAYER_TEST_ISO's files out loud on CDPLAYER_TEST_DEVICE"]
async fn plays_disc_image() {
    use std::time::Duration;
    let path = std::env::var("CDPLAYER_TEST_ISO").expect("CDPLAYER_TEST_ISO");
    let device = std::env::var("CDPLAYER_TEST_DEVICE").unwrap_or("default".into());
    let image = std::fs::read(&path).unwrap();
    let files = datadisc::audio_files(&iso9660::read_volume(image.as_slice()).unwrap());
    // A disc image file reads just like a data disc.
    let reader = crate::drive::Drive::new(path).data_reader().unwrap();
    let mut playback = playback::Playback::start(playback::Source::Files { reader, files: files.clone() }, device, 0, playback::Gain::new(60));

    // Seek into the first track, then let it play on from there.
    tokio::time::sleep(Duration::from_millis(500)).await;
    playback.seek(0, 3.0);
    tokio::time::sleep(Duration::from_millis(700)).await;
    println!("track 1 after seeking to 3s: {:.2}s", playback.elapsed());
    assert!(playback.elapsed() > 3.0 && playback.elapsed() < 4.0);
    playback.seek(1, 0.0);

    // A second of each track, skipping forward through format changes (48 kHz, mono).
    for i in 1..files.len() {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(playback.outcome().is_none(), "playback ended early");
        println!("track {} at {:.2}s", playback.track() + 1, playback.elapsed());
        assert_eq!(playback.track(), i);
        assert!(playback.elapsed() > 0.5);
        if i + 1 < files.len() {
            playback.seek(i + 1, 0.0);
        }
    }
    // Pause holds the position; resume continues from it.
    playback.set_paused(true);
    tokio::time::sleep(Duration::from_millis(400)).await;
    let held = playback.elapsed();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(playback.elapsed(), held);
    playback.set_paused(false);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(playback.outcome().is_some(), "should finish after the last track");
}

#[test]
fn mqtt_broker_urls() {
    use crate::mqtt::Broker;
    let b = Broker::parse("mqtt://me:p@ss:word@10.0.0.2:1884").unwrap();
    assert_eq!(format!("{b:?}"), r#"Broker { host: "10.0.0.2", port: 1884, credentials: Some(("me", "p@ss:word")) }"#);
    let b = Broker::parse("mqtt://broker.local").unwrap();
    assert_eq!(format!("{b:?}"), r#"Broker { host: "broker.local", port: 1883, credentials: None }"#);
    assert!(Broker::parse("http://broker.local").is_none());
    assert!(Broker::parse("mqtt://host:notaport").is_none());
    assert!(Broker::parse("mqtt://").is_none());

    let bad = Config { mqtt_broker: "broker.local".into(), ..Config::default() };
    assert!(bad.normalized().is_err());
    let bad = Config { mqtt_topic: "shairport/#".into(), ..Config::default() };
    assert!(bad.normalized().is_err());
}

#[test]
fn mqtt_messages_follow_playback() {
    use crate::mqtt::{messages, needs_cover};
    use crate::player::{Activity, NowPlaying};
    let topics = |prev: &NowPlaying, now: &NowPlaying| -> Vec<&str> {
        messages(prev, now).into_iter().map(|(t, _)| t).collect()
    };
    let idle = NowPlaying::default();
    let track = |title: &str, activity| NowPlaying {
        activity,
        title: title.into(),
        artist: "Jamiroquai".into(),
        album: "The Return of the Space Cowboy".into(),
        cover: Some("https://example.com/front".into()),
    };
    let one = track("Just Another Story", Activity::Playing);
    let paused = track("Just Another Story", Activity::Paused);
    let two = track("Stillness in Time", Activity::Playing);

    // Track info first, then the state message that makes Home Assistant show it.
    assert_eq!(topics(&idle, &one), ["title", "artist", "album", "play_start"]);
    assert_eq!(messages(&idle, &one)[0].1, "Just Another Story");
    assert!(needs_cover(&idle, &one));
    // Nothing changed, nothing sent.
    assert!(topics(&one, &one).is_empty());
    assert!(!needs_cover(&one, &one));
    // Pause is a flush, not play_end (which automations treat as "done").
    assert_eq!(topics(&one, &paused), ["play_flush"]);
    assert_eq!(topics(&paused, &one), ["play_resume"]);
    // A new track: new info, then a state message to refresh it. Same cover, not resent.
    assert_eq!(topics(&one, &two), ["title", "artist", "album", "play_resume"]);
    assert!(!needs_cover(&one, &two));
    // Stopping ends the session, which also clears the info in Home Assistant.
    assert_eq!(topics(&two, &idle), ["play_end", "active_end"]);
    assert!(topics(&idle, &idle).is_empty());
    assert!(!needs_cover(&two, &idle));
}
