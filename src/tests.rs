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
use crate::drive::{Track, audio_tracks};
use crate::{AppState, Settings, build_router, player};

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
    assert_eq!(body["error"], "No audio CD in the drive");

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

    let new = json!({
        "start_webhook": "  http://ha.local:8123/api/webhook/cd-start ",
        "stop_webhook": "https://ha.local/api/webhook/cd-stop",
        "eject_when_finished": false,
    });
    let (code, body) = send(app.clone(), "POST", "/api/config", Some(new)).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["start_webhook"], "http://ha.local:8123/api/webhook/cd-start");

    // Persisted, and served back.
    let saved = config::load(&path);
    assert_eq!(saved.start_webhook, "http://ha.local:8123/api/webhook/cd-start");
    assert!(!saved.eject_when_finished);
    let (_, body) = send(app, "GET", "/api/config", None).await;
    assert_eq!(body["stop_webhook"], "https://ha.local/api/webhook/cd-stop");
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

#[test]
fn audio_tracks_from_toc() {
    // Plain audio CD: each track runs to the next one; the last to the lead-out.
    let toc = [(0, false), (15000, false), (30000, false), (40000, false)];
    assert_eq!(
        audio_tracks(&toc),
        vec![
            Track { start: 0, end: 15000 },
            Track { start: 15000, end: 30000 },
            Track { start: 30000, end: 40000 },
        ]
    );

    // Mixed mode: a leading data track is skipped.
    let toc = [(0, true), (20000, false), (35000, false)];
    assert_eq!(audio_tracks(&toc), vec![Track { start: 20000, end: 35000 }]);

    // Enhanced CD: the last audio track stops short of the session gap before the data.
    let toc = [(0, false), (20000, false), (50000, true), (60000, false)];
    assert_eq!(
        audio_tracks(&toc),
        vec![
            Track { start: 0, end: 20000 },
            Track { start: 20000, end: 50000 - 11_400 },
        ]
    );

    assert!(audio_tracks(&[]).is_empty());
}

#[test]
fn track_lookup() {
    let tracks = [
        Track { start: 150, end: 1000 },
        Track { start: 1000, end: 2000 },
        Track { start: 2000, end: 3000 },
    ];
    assert_eq!(player::track_index(&tracks, 150), 0);
    assert_eq!(player::track_index(&tracks, 999), 0);
    assert_eq!(player::track_index(&tracks, 1000), 1);
    assert_eq!(player::track_index(&tracks, 2999), 2);
    // Before the first track (the pregap) counts as track 1.
    assert_eq!(player::track_index(&tracks, 0), 0);
}
