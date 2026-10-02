//! Webhooks fired when playback starts and stops — e.g. Home Assistant automations that
//! power the receiver on and off.

use serde::Deserialize;
use serde_json::json;

use crate::AppState;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Event {
    Start,
    Stop,
}

impl Event {
    pub fn name(self) -> &'static str {
        match self {
            Event::Start => "start",
            Event::Stop => "stop",
        }
    }
}

/// POST `{"event": "start"|"stop"}` to `url`.
pub async fn send(client: &reqwest::Client, url: &str, event: Event) -> Result<(), String> {
    client
        .post(url)
        .json(&json!({ "event": event.name() }))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Send `event` to its configured URL, if one is set, and log the outcome.
pub async fn notify(state: &AppState, event: Event) {
    let url = {
        let cfg = state.config.read().await;
        match event {
            Event::Start => cfg.start_webhook.clone(),
            Event::Stop => cfg.stop_webhook.clone(),
        }
    };
    if url.is_empty() {
        return;
    }
    match send(&state.client, &url, event).await {
        Ok(()) => eprintln!("cdplayer: sent {} webhook", event.name()),
        Err(e) => eprintln!("cdplayer: {} webhook failed: {e}", event.name()),
    }
}

/// Deliver `events` in order on a background task, so playback never waits on HA.
pub fn fire(state: &AppState, events: Vec<Event>) {
    if events.is_empty() {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        for event in events {
            notify(&state, event).await;
        }
    });
}
