//! The playback state machine. A background task calls `tick` once a second to reconcile
//! with the drive: wake it when it (re)appears, work out what's playable on a newly
//! inserted disc (audio CD tracks, or audio files on a data disc), start playing, announce
//! playback once audio is actually flowing, and notice when playback ends. The web
//! handlers act on the same state through the shared mutex.
//!
//! Webhooks: `start` fires only once audio reaches the sound card, so a failed start (e.g.
//! the card is busy with AirPlay) never powers the receiver on; `stop` fires whenever a
//! session that announced `start` ends, however it ends.

use serde_json::{Value, json};
use std::time::{Duration, Instant};

use crate::AppState;
use crate::config::Config;
use crate::album::{self, Album};
use crate::datadisc::{self, Art};
use crate::drive::{DataReader, Disc, Drive, SECTORS_PER_SECOND, Toc, Track};
use crate::iso9660::{self, IsoFile, Volume};
use crate::playback::{Gain, Outcome, Playback, Source};
use crate::webhook::{self, Event};

/// A disc found this soon after the drive appears was already inside (left in across a
/// reboot or replug), so it waits for Play instead of surprising anyone by auto-playing.
const SETTLE: Duration = Duration::from_secs(20);
/// "Previous" restarts the current track when we're further into it than this (seconds).
const RESTART_THRESHOLD: f64 = 3.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    PlayPause,
    Next,
    Previous,
    Stop,
    Eject,
    /// Jump to this track (0-based), starting playback if stopped.
    Track(usize),
    /// Jump to this far into the current track.
    Seek(Duration),
}

impl Action {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "playpause" => Action::PlayPause,
            "next" => Action::Next,
            "prev" => Action::Previous,
            "stop" => Action::Stop,
            "eject" => Action::Eject,
            _ => return None,
        })
    }
}

/// What's playable on the disc in the drive.
enum Program {
    /// An audio CD's tracks.
    Cdda(Vec<Track>),
    /// Audio files on a data disc.
    Files(Vec<IsoFile>),
}

/// Background work to fill in a newly inserted disc's track info.
pub enum Lookup {
    MusicBrainz(Toc),
    Scan {
        disc_id: String,
        reader: DataReader,
        volume: Volume,
        files: Vec<IsoFile>,
    },
}

/// One playback run over a disc.
struct Session {
    playback: Playback,
    /// Whether the start webhook has fired for this session.
    announced: bool,
}

pub struct Player {
    drive: Drive,
    audio_device: String,
    gain: Gain,
    session: Option<Session>,
    disc: Disc,
    program: Option<Program>,
    /// Each track's length in seconds, where known.
    lengths: Vec<Option<f64>>,
    /// Identifies the disc in the drive, so background lookups only apply to that disc.
    disc_id: Option<String>,
    album: Option<Album>,
    /// Cover art read off a data disc, served at `/api/art/{disc_id}`.
    art: Option<Art>,
    /// When the drive last went from missing to present.
    appeared_at: Option<Instant>,
    /// The disc is in but shouldn't auto-play: it was stopped, finished, failed, or was
    /// already in when the drive appeared. Cleared when the disc leaves.
    hold: bool,
    /// When playback was first seen paused, for stopping it after a while.
    paused_since: Option<Instant>,
    error: Option<String>,
}

impl Player {
    pub fn new(drive: Drive, audio_device: String, volume: u8) -> Self {
        Player {
            drive,
            audio_device,
            gain: Gain::new(volume),
            session: None,
            disc: Disc::Missing,
            program: None,
            lengths: Vec::new(),
            disc_id: None,
            album: None,
            art: None,
            appeared_at: None,
            hold: false,
            paused_since: None,
            error: None,
        }
    }

    /// Set the playback volume (0–100); takes effect immediately, mid-track included.
    pub fn set_volume(&self, percent: u8) {
        self.gain.set(percent);
    }

    fn check_drive(&self) -> Disc {
        // A loaded disc only needs a cheap presence check; re-identifying it every second
        // would mean re-reading the TOC under playback.
        if self.program.is_some() {
            return match self.drive.has_disc() {
                Some(true) => self.disc,
                Some(false) => Disc::Empty,
                None => Disc::Missing,
            };
        }
        self.drive.status()
    }

    /// Reconcile with the drive. Returns background work for a newly inserted disc.
    async fn poll(&mut self, cfg: &Config, events: &mut Vec<Event>) -> Option<Lookup> {
        let disc = self.check_drive();

        // A SuperDrive comes up asleep after every power-on (boot, replug); wake it so it
        // takes discs.
        if self.disc == Disc::Missing && disc != Disc::Missing {
            match self.drive.wake() {
                Ok(()) => eprintln!("cdplayer: drive {} connected; woke it", self.drive.path()),
                Err(e) => eprintln!(
                    "cdplayer: drive {} connected; wake failed ({e}), fine unless it's a SuperDrive",
                    self.drive.path()
                ),
            }
            self.appeared_at = Some(Instant::now());
        }

        let outcome = self.session.as_mut().and_then(|s| s.playback.outcome());
        if let Some(outcome) = outcome {
            let session = self.session.take().expect("session ended");
            if session.announced {
                events.push(Event::Stop);
            }
            self.hold = true;
            match outcome {
                Outcome::Finished if cfg.eject_when_finished && disc.has_media() => {
                    if let Err(e) = self.drive.eject() {
                        eprintln!("cdplayer: eject after finishing failed: {e}");
                    }
                }
                Outcome::Failed(e) => self.error = Some(e),
                Outcome::Finished | Outcome::Stopped => {}
            }
        } else if let Some(s) = &mut self.session {
            if !s.announced && s.playback.started() {
                s.announced = true;
                events.push(Event::Start);
            }
        }

        // Stop a disc left paused too long.
        if self.session.as_ref().is_some_and(|s| s.playback.paused()) {
            let since = *self.paused_since.get_or_insert_with(Instant::now);
            let limit = Duration::from_secs(u64::from(cfg.stop_after_paused_minutes) * 60);
            if cfg.stop_after_paused_minutes > 0 && since.elapsed() >= limit {
                self.end_session(events).await;
                self.hold = true;
            }
        } else {
            self.paused_since = None;
        }

        let mut lookup = None;
        if disc != self.disc {
            // The disc left, or a new one arrived: whatever was loaded no longer applies.
            self.end_session(events).await;
            self.program = None;
            self.lengths.clear();
            self.disc_id = None;
            self.album = None;
            self.art = None;
            self.error = None;
            self.hold = disc.has_media() && self.appeared_at.is_some_and(|t| t.elapsed() < SETTLE);
            lookup = self.load(disc);
        }
        self.disc = disc;

        if self.program.is_some() && self.session.is_none() && !self.hold {
            self.start(0);
        }
        lookup
    }

    /// Work out what's playable on a newly inserted disc.
    fn load(&mut self, disc: Disc) -> Option<Lookup> {
        match disc {
            Disc::Audio => {
                let toc = self
                    .drive
                    .toc()
                    .inspect_err(|e| self.error = Some(format!("Couldn't read the disc: {e}")))
                    .ok()?;
                let tracks = toc.audio_tracks();
                if tracks.is_empty() {
                    return None;
                }
                let seconds = |sectors: u32| f64::from(sectors) / f64::from(SECTORS_PER_SECOND);
                self.lengths = tracks.iter().map(|t| Some(seconds(t.end - t.start))).collect();
                self.disc_id = Some(toc.musicbrainz_id());
                self.program = Some(Program::Cdda(tracks));
                Some(Lookup::MusicBrainz(toc))
            }
            Disc::Data => {
                let read = self.drive.data_reader().and_then(|reader| {
                    let volume = iso9660::read_volume(&reader)?;
                    Ok((reader, volume))
                });
                let (reader, volume) = read
                    .inspect_err(|e| eprintln!("cdplayer: couldn't read the data disc: {e}"))
                    .ok()?;
                let files = datadisc::audio_files(&volume);
                if files.is_empty() {
                    eprintln!("cdplayer: data disc \"{}\" has no audio files", volume.label);
                    return None;
                }
                let disc_id = datadisc::disc_id(&volume, &files);
                self.album = Some(datadisc::placeholder_album(&volume, &files));
                self.lengths = vec![None; files.len()];
                self.disc_id = Some(disc_id.clone());
                self.program = Some(Program::Files(files.clone()));
                Some(Lookup::Scan {
                    disc_id,
                    reader,
                    volume,
                    files,
                })
            }
            Disc::Missing | Disc::Empty => None,
        }
    }

    /// Attach looked-up album info, if that disc is still the one in the drive.
    pub fn set_album(&mut self, disc_id: &str, album: Album) {
        if self.disc_id.as_deref() == Some(disc_id) {
            self.album = Some(album);
        }
    }

    /// Attach a data disc's scanned tags, lengths, and art.
    pub fn set_scan(&mut self, disc_id: &str, mut album: Album, lengths: Vec<Option<f64>>, art: Option<Art>) {
        if self.disc_id.as_deref() != Some(disc_id) {
            return;
        }
        album.cover = art.as_ref().map(|_| format!("/api/art/{disc_id}"));
        self.album = Some(album);
        self.lengths = lengths;
        self.art = art;
    }

    /// The data disc cover art for `disc_id`, if that's the disc in the drive.
    pub fn art(&self, disc_id: &str) -> Option<Art> {
        (self.disc_id.as_deref() == Some(disc_id))
            .then(|| self.art.clone())
            .flatten()
    }

    fn start(&mut self, first: usize) {
        let source = match &self.program {
            Some(Program::Cdda(tracks)) => self.drive.audio_reader().map(|reader| Source::Cdda {
                reader,
                tracks: tracks.clone(),
            }),
            Some(Program::Files(files)) => self.drive.data_reader().map(|reader| Source::Files {
                reader,
                files: files.clone(),
            }),
            None => {
                self.hold = true;
                self.error = Some("Nothing to play on this disc".into());
                return;
            }
        };
        match source {
            Ok(source) => {
                self.session = Some(Session {
                    playback: Playback::start(source, self.audio_device.clone(), first, self.gain.clone()),
                    announced: false,
                });
                self.error = None;
            }
            Err(e) => {
                self.hold = true;
                self.error = Some(format!("Couldn't open the drive: {e}"));
            }
        }
    }

    /// Stop playback (if running), waiting until the drive and sound card are free.
    async fn end_session(&mut self, events: &mut Vec<Event>) {
        if let Some(Session { playback, announced }) = self.session.take() {
            playback.stop().await;
            if announced {
                events.push(Event::Stop);
            }
        }
    }

    /// Start playing (if stopped) from track `first`, reporting any failure.
    fn play_from(&mut self, first: usize) -> Result<(), String> {
        if self.program.is_none() {
            return Err("Nothing to play in the drive".into());
        }
        self.hold = false;
        self.start(first);
        self.error.clone().map_or(Ok(()), Err)
    }

    async fn control(&mut self, action: Action, events: &mut Vec<Event>) -> Result<(), String> {
        // Any button press supersedes the last failure's message.
        self.error = None;
        match action {
            Action::PlayPause => match &self.session {
                Some(s) => s.playback.set_paused(!s.playback.paused()),
                None => self.play_from(0)?,
            },
            Action::Next | Action::Previous => {
                let Some(s) = &self.session else {
                    return Err("Not playing".into());
                };
                let current = s.playback.track();
                let target = match action {
                    Action::Next => current + 1,
                    _ if s.playback.elapsed() > RESTART_THRESHOLD => current,
                    _ => current.saturating_sub(1),
                };
                // Next on the last track is a no-op rather than ending the disc.
                if target < self.lengths.len() {
                    s.playback.seek(target, 0.0);
                }
            }
            Action::Track(i) => {
                if i >= self.lengths.len() {
                    return Err("No such track".into());
                }
                match &self.session {
                    Some(s) => {
                        s.playback.seek(i, 0.0);
                        s.playback.set_paused(false);
                    }
                    None => self.play_from(i)?,
                }
            }
            Action::Seek(to) => {
                let Some(s) = &self.session else {
                    return Err("Not playing".into());
                };
                let track = s.playback.track();
                let secs = to.as_secs_f64();
                if self.lengths.get(track).copied().flatten().is_some_and(|len| secs >= len) {
                    return Err("That's past the end of the track".into());
                }
                s.playback.seek(track, secs);
            }
            Action::Stop => {
                self.end_session(events).await;
                self.hold = true;
            }
            Action::Eject => {
                self.end_session(events).await;
                self.drive
                    .eject()
                    .map_err(|e| format!("Eject failed: {e}"))?;
            }
        }
        Ok(())
    }

    fn status(&self) -> Value {
        let info = |i: usize| self.album.as_ref().and_then(|a| a.tracks.get(i));
        let tracklist: Vec<Value> = self
            .lengths
            .iter()
            .enumerate()
            .map(|(i, length)| {
                json!({
                    "title": info(i).map(|t| &t.title),
                    "artist": info(i).map(|t| &t.artist),
                    "length": length,
                })
            })
            .collect();
        let playable = self.program.is_some();
        let mut status = json!({
            "drive": self.disc.as_str(),
            "state": if playable { "stopped" } else { "idle" },
            "tracks": if playable { json!(self.lengths.len()) } else { Value::Null },
            "track": null,
            "elapsed": null,
            "length": null,
            "error": self.error,
            "volume": self.gain.percent(),
            "album": self.album.as_ref().map(|a| json!({
                "title": a.title,
                "artist": a.artist,
                "cover": a.cover,
            })),
            "tracklist": tracklist,
        });
        let Some(s) = &self.session else {
            return status;
        };
        status["state"] = json!(if s.playback.paused() { "paused" } else { "playing" });
        if !s.playback.started() && !s.playback.paused() {
            return status; // still spinning up
        }
        let i = s.playback.track();
        status["track"] = json!(i + 1);
        status["elapsed"] = json!(s.playback.elapsed());
        status["length"] = json!(self.lengths.get(i).copied().flatten());
        status
    }
}

/// Whether a disc is playing, for reporting elsewhere (MQTT).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Activity {
    #[default]
    Idle,
    Playing,
    Paused,
}

/// What's playing, as reported to Home Assistant over MQTT.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NowPlaying {
    pub activity: Activity,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Cover art URL: remote, or `/api/art/...` for art read off a data disc.
    pub cover: Option<String>,
}

impl Player {
    pub fn now_playing(&self) -> NowPlaying {
        let Some(s) = self.session.as_ref().filter(|s| s.playback.started() || s.playback.paused()) else {
            return NowPlaying::default();
        };
        let i = s.playback.track();
        let album = self.album.as_ref();
        let track = album.and_then(|a| a.tracks.get(i));
        let nonempty = |s: &String| !s.is_empty();
        NowPlaying {
            activity: if s.playback.paused() { Activity::Paused } else { Activity::Playing },
            title: track
                .map(|t| t.title.clone())
                .filter(nonempty)
                .unwrap_or_else(|| format!("Track {}", i + 1)),
            artist: track
                .map(|t| t.artist.clone())
                .filter(nonempty)
                .or_else(|| album.map(|a| a.artist.clone()))
                .unwrap_or_default(),
            album: album.map(|a| a.title.clone()).unwrap_or_default(),
            cover: album.and_then(|a| a.cover.clone()),
        }
    }

    /// Whether Play would start the disc in the drive.
    pub fn can_start(&self) -> bool {
        self.program.is_some() && self.session.is_none()
    }

    pub fn volume(&self) -> u8 {
        self.gain.percent()
    }

    pub fn disc(&self) -> Disc {
        self.disc
    }
}

/// Set the playback volume now and for later discs.
pub async fn set_volume(state: &AppState, volume: u8) -> u8 {
    let volume = volume.min(100);
    state.player.lock().await.set_volume(volume);
    let mut cfg = state.config.write().await;
    cfg.volume = volume;
    if let Err(e) = crate::config::save(&state.config_path, &cfg) {
        eprintln!("cdplayer: couldn't save volume: {e}");
    }
    volume
}

/// One reconcile pass against the drive.
pub async fn tick(state: &AppState) {
    let cfg = state.config.read().await.clone();
    let mut events = Vec::new();
    let lookup = state.player.lock().await.poll(&cfg, &mut events).await;
    webhook::fire(state, events);
    match lookup {
        Some(Lookup::MusicBrainz(toc)) => {
            tokio::spawn(album::load(state.clone(), toc));
        }
        Some(Lookup::Scan {
            disc_id,
            reader,
            volume,
            files,
        }) => {
            tokio::spawn(datadisc::scan(
                state.clone(),
                std::sync::Arc::new(reader),
                disc_id,
                volume,
                files,
            ));
        }
        None => {}
    }
}

pub async fn control(state: &AppState, action: Action) -> Result<(), String> {
    let mut events = Vec::new();
    let result = state.player.lock().await.control(action, &mut events).await;
    webhook::fire(state, events);
    result
}

pub async fn status(state: &AppState) -> Value {
    state.player.lock().await.status()
}

/// On container stop: end playback and deliver the stop webhook before exiting, so the
/// receiver doesn't get left on.
pub async fn shutdown(state: &AppState) {
    let mut events = Vec::new();
    state.player.lock().await.end_session(&mut events).await;
    for event in events {
        webhook::notify(state, event).await;
    }
}
