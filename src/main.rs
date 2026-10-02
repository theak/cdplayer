//! CD Player — auto-plays an audio CD inserted into the attached drive, with a small web
//! remote (play/pause, previous/next, stop, eject) and webhooks fired when playback starts
//! and stops (e.g. to power a receiver on and off via Home Assistant).
//!
//! It talks to the hardware directly: the drive through ioctls on its block device (see
//! `drive`), and the sound card through ALSA, fed by a thread that streams an audio CD's
//! raw audio or decodes the audio files on a data disc (see `playback`). The container only
//! needs the drive and `/dev/snd` passed in.

mod album;
mod config;
mod datadisc;
mod drive;
mod handlers;
mod iso9660;
mod playback;
mod player;
#[cfg(test)]
mod tests;
mod webhook;

use axum::{
    Router,
    routing::{get, post},
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};

use config::Config;
use drive::Drive;
use player::Player;

const DEFAULT_PORT: u16 = 42781;

/// Where the hardware and data live, read from the environment once at boot. These are
/// fixed per deployment, unlike the UI-editable `Config`.
pub struct Settings {
    pub cd_device: String,
    /// An ALSA PCM name, e.g. `plughw:CARD=PCH,DEV=0`.
    pub audio_device: String,
    pub data_dir: PathBuf,
    pub port: u16,
}

impl Settings {
    pub fn from_env() -> Self {
        fn var(name: &str, default: &str) -> String {
            std::env::var(name)
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| default.to_string())
        }
        Settings {
            cd_device: var("CD_DEVICE", "/dev/sr0"),
            audio_device: var("AUDIO_DEVICE", "default"),
            data_dir: var("DATA_DIR", "/data").into(),
            port: var("PORT", "").parse().unwrap_or(DEFAULT_PORT),
        }
    }
}

/// Shared, cheaply-cloneable application state. `reqwest::Client` is internally `Arc`.
#[derive(Clone)]
pub struct AppState {
    pub player: Arc<Mutex<Player>>,
    pub config: Arc<RwLock<Config>>,
    pub config_path: Arc<PathBuf>,
    /// Cached album info, one JSON file per disc ID.
    pub albums_dir: Arc<PathBuf>,
    pub client: reqwest::Client,
}

impl AppState {
    pub fn new(settings: &Settings) -> Self {
        let config_path = settings.data_dir.join("config.json");
        let config = config::load(&config_path);
        let player = Player::new(
            Drive::new(settings.cd_device.clone()),
            settings.audio_device.clone(),
            config.volume,
        );
        AppState {
            player: Arc::new(Mutex::new(player)),
            config: Arc::new(RwLock::new(config)),
            config_path: Arc::new(config_path),
            albums_dir: Arc::new(settings.data_dir.join("albums")),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("failed to build HTTP client"),
        }
    }
}

/// Build the app router. Kept public and layer-free so tests can drive it via `oneshot`.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(handlers::index))
        .route("/static/{file}", get(handlers::serve_static))
        .route("/api/status", get(handlers::status))
        .route("/api/control/{action}", post(handlers::control))
        .route("/api/track/{number}", post(handlers::play_track))
        .route("/api/art/{disc_id}", get(handlers::art))
        .route("/api/config", get(handlers::get_config).post(handlers::save_config))
        .route("/api/seek", post(handlers::seek))
        .route("/api/volume", post(handlers::set_volume))
        .route("/api/webhook-test", post(handlers::test_webhook))
        .with_state(state)
}

/// Resolve on SIGTERM (`docker stop`) or Ctrl-C.
async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[tokio::main]
async fn main() {
    let settings = Settings::from_env();

    // `healthcheck` subcommand: TCP-connect to our own port, exit 0/1 (the image has no curl).
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        let ok = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::TcpStream::connect(("127.0.0.1", settings.port)),
        )
        .await
        .is_ok_and(|r| r.is_ok());
        std::process::exit(if ok { 0 } else { 1 });
    }

    let state = AppState::new(&settings);
    eprintln!(
        "cdplayer: drive {}, audio device {}, settings in {}",
        settings.cd_device,
        settings.audio_device,
        state.config_path.display()
    );

    // Reconcile with the drive once a second.
    let poller = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            player::tick(&poller).await;
        }
    });

    let addr = SocketAddr::from(([0, 0, 0, 0], settings.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    eprintln!("cdplayer: listening on http://{addr}");

    axum::serve(listener, build_router(state.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
    player::shutdown(&state).await;
}
