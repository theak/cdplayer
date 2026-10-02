//! HTTP handlers: the web remote's page and assets, playback status/controls, and the
//! settings API.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::config::{self, Config};
use crate::player::{self, Action};
use crate::webhook::{self, Event};

// Frontend assets are baked into the binary. Paths are relative to this source file.
const INDEX_HTML: &str = include_str!("../templates/index.html");
const STYLE_CSS: &str = include_str!("../static/style.css");
const SCRIPT_JS: &str = include_str!("../static/script.js");
const ICON_SVG: &str = include_str!("../static/icon.svg");

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

/// GET `/`
pub async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

/// GET `/static/{file}`
pub async fn serve_static(Path(file): Path<String>) -> Response {
    let (ct, body) = match file.as_str() {
        "style.css" => ("text/css; charset=UTF-8", STYLE_CSS),
        "script.js" => ("application/javascript; charset=UTF-8", SCRIPT_JS),
        "icon.svg" => ("image/svg+xml", ICON_SVG),
        _ => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    ([("content-type", ct)], body).into_response()
}

/// GET `/api/status` — drive, playback state, and current track position.
pub async fn status(State(state): State<AppState>) -> Json<Value> {
    Json(player::status(&state).await)
}

/// POST `/api/control/{playpause|next|prev|stop|eject}`. 409 when the action can't apply
/// right now (no disc, not playing, eject failed).
pub async fn control(State(state): State<AppState>, Path(action): Path<String>) -> Response {
    let Some(action) = Action::parse(&action) else {
        return error(StatusCode::NOT_FOUND, "Unknown action");
    };
    match player::control(&state, action).await {
        Ok(()) => Json(json!({ "success": true })).into_response(),
        Err(e) => error(StatusCode::CONFLICT, e),
    }
}

/// POST `/api/track/{number}` — jump to track `number` (1-based), starting playback if
/// stopped.
pub async fn play_track(State(state): State<AppState>, Path(number): Path<usize>) -> Response {
    let Some(index) = number.checked_sub(1) else {
        return error(StatusCode::NOT_FOUND, "No such track");
    };
    match player::control(&state, Action::Track(index)).await {
        Ok(()) => Json(json!({ "success": true })).into_response(),
        Err(e) => error(StatusCode::CONFLICT, e),
    }
}

/// GET `/api/art/{disc_id}` — cover art found on the data disc in the drive.
pub async fn art(State(state): State<AppState>, Path(disc_id): Path<String>) -> Response {
    match state.player.lock().await.art(&disc_id) {
        Some(art) => (
            [
                ("content-type", art.mime),
                // The URL names the disc, so its art never changes.
                ("cache-control", "max-age=86400".to_string()),
            ],
            axum::body::Bytes::from_owner(art.data),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// GET `/api/config`
pub async fn get_config(State(state): State<AppState>) -> Json<Config> {
    Json(state.config.read().await.clone())
}

/// POST `/api/config` — validate, persist, and apply. Returns the saved settings.
pub async fn save_config(State(state): State<AppState>, Json(cfg): Json<Config>) -> Response {
    let cfg = match cfg.normalized() {
        Ok(c) => c,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    if let Err(e) = config::save(&state.config_path, &cfg) {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Couldn't save settings: {e}"),
        );
    }
    *state.config.write().await = cfg.clone();
    Json(cfg).into_response()
}

#[derive(Deserialize)]
pub struct WebhookTest {
    event: Event,
    url: String,
}

/// POST `/api/webhook-test` `{"event": "start"|"stop", "url": ...}` — send one webhook
/// now, so a URL can be checked from the settings form before saving it.
pub async fn test_webhook(State(state): State<AppState>, Json(req): Json<WebhookTest>) -> Response {
    let url = req.url.trim();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return error(StatusCode::BAD_REQUEST, "Enter an http:// or https:// URL first");
    }
    match webhook::send(&state.client, url, req.event).await {
        Ok(()) => Json(json!({ "success": true })).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, e),
    }
}
