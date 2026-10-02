//! The playback state machine. A background task calls `tick` once a second to reconcile
//! with the drive: wake it when it (re)appears, start playing when an audio disc goes in,
//! announce playback once audio is actually flowing, and notice when playback ends. The
//! web handlers act on the same state through the shared mutex.
//!
//! Webhooks: `start` fires only once audio reaches the sound card, so a failed start (e.g.
//! the card is busy with AirPlay) never powers the receiver on; `stop` fires whenever a
//! session that announced `start` ends, however it ends.

use serde_json::{Value, json};
use std::time::{Duration, Instant};

use crate::AppState;
use crate::album::{self, Album};
use crate::drive::{Disc, Drive, SECTORS_PER_SECOND, Toc, Track};
use crate::playback::{Outcome, Playback};
use crate::webhook::{self, Event};

/// A disc found this soon after the drive appears was already inside (left in across a
/// reboot or replug), so it waits for Play instead of surprising anyone by auto-playing.
const SETTLE: Duration = Duration::from_secs(20);
/// "Previous" restarts the current track when we're further into it than this.
const RESTART_THRESHOLD: u32 = 3 * SECTORS_PER_SECOND;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    PlayPause,
    Next,
    Previous,
    Stop,
    Eject,
    /// Jump to this track (0-based), starting playback if stopped.
    Track(usize),
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

/// One playback run over a disc.
struct Session {
    playback: Playback,
    /// Whether the start webhook has fired for this session.
    announced: bool,
}

pub struct Player {
    drive: Drive,
    audio_device: String,
    session: Option<Session>,
    disc: Disc,
    tracks: Vec<Track>,
    /// MusicBrainz disc ID of the disc in the drive, and its album info once looked up.
    disc_id: Option<String>,
    album: Option<Album>,
    /// When the drive last went from missing to present.
    appeared_at: Option<Instant>,
    /// The disc is in but shouldn't auto-play: it was stopped, finished, failed, or was
    /// already in when the drive appeared. Cleared when the disc leaves.
    hold: bool,
    error: Option<String>,
}

/// Index of the track containing `sector`.
pub(crate) fn track_index(tracks: &[Track], sector: u32) -> usize {
    tracks.iter().rposition(|t| sector >= t.start).unwrap_or(0)
}

impl Player {
    pub fn new(drive: Drive, audio_device: String) -> Self {
        Player {
            drive,
            audio_device,
            session: None,
            disc: Disc::Missing,
            tracks: Vec::new(),
            disc_id: None,
            album: None,
            appeared_at: None,
            hold: false,
            error: None,
        }
    }

    fn check_drive(&self) -> Disc {
        // A known audio disc only needs a cheap presence check; re-identifying it every
        // second would mean re-reading the TOC under playback.
        if self.disc.is_audio() {
            return match self.drive.has_disc() {
                Some(true) => Disc::Audio,
                Some(false) => Disc::Empty,
                None => Disc::Missing,
            };
        }
        self.drive.status()
    }

    /// Reconcile with the drive. Returns the TOC of a newly inserted disc, to look up.
    async fn poll(&mut self, eject_when_finished: bool, events: &mut Vec<Event>) -> Option<Toc> {
        let disc = self.check_drive();
        let mut inserted = None;

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
                Outcome::Finished if eject_when_finished && disc.is_audio() => {
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

        if !disc.is_audio() {
            // Disc gone (or drive unplugged) mid-play.
            self.end_session(events).await;
            self.hold = false;
            self.tracks.clear();
            self.disc_id = None;
            self.album = None;
        } else if !self.disc.is_audio() {
            self.error = None;
            self.hold = self.appeared_at.is_some_and(|t| t.elapsed() < SETTLE);
            inserted = self.read_toc();
        }
        self.disc = disc;

        if disc.is_audio() && self.session.is_none() && !self.hold {
            self.start();
        }
        inserted
    }

    /// Read the disc's TOC into `tracks`/`disc_id`, returning it if it has audio tracks.
    fn read_toc(&mut self) -> Option<Toc> {
        let toc = self
            .drive
            .toc()
            .inspect_err(|e| eprintln!("cdplayer: couldn't read the track list: {e}"))
            .ok()?;
        self.tracks = toc.audio_tracks();
        if self.tracks.is_empty() {
            return None;
        }
        self.disc_id = Some(toc.musicbrainz_id());
        Some(toc)
    }

    /// Attach looked-up album info, if that disc is still the one in the drive.
    pub fn set_album(&mut self, disc_id: &str, album: Album) {
        if self.disc_id.as_deref() == Some(disc_id) {
            self.album = Some(album);
        }
    }

    fn start(&mut self) {
        if self.tracks.is_empty() {
            self.read_toc();
        }
        let (Some(first), Some(last)) = (self.tracks.first(), self.tracks.last()) else {
            self.hold = true;
            self.error = Some("Couldn't read the disc's track list".into());
            return;
        };
        let (start, end) = (first.start, last.end);
        match self.drive.audio_reader() {
            Ok(reader) => {
                self.session = Some(Session {
                    playback: Playback::start(reader, self.audio_device.clone(), start, end),
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

    async fn control(&mut self, action: Action, events: &mut Vec<Event>) -> Result<(), String> {
        // Any button press supersedes the last failure's message.
        self.error = None;
        match action {
            Action::PlayPause => {
                if let Some(s) = &self.session {
                    s.playback.set_paused(!s.playback.paused());
                } else if self.disc.is_audio() {
                    self.hold = false;
                    self.start();
                    if let Some(e) = &self.error {
                        return Err(e.clone());
                    }
                } else {
                    return Err("No audio CD in the drive".into());
                }
            }
            Action::Next | Action::Previous => {
                let Some(s) = &self.session else {
                    return Err("Not playing".into());
                };
                let pos = s.playback.position();
                let current = track_index(&self.tracks, pos);
                let into_track = pos.saturating_sub(self.tracks[current].start);
                let target = match action {
                    Action::Next => current + 1,
                    _ if into_track > RESTART_THRESHOLD => current,
                    _ => current.saturating_sub(1),
                };
                // Next on the last track is a no-op rather than ending the disc.
                if let Some(t) = self.tracks.get(target) {
                    s.playback.seek(t.start);
                }
            }
            Action::Track(i) => {
                let Some(track) = self.tracks.get(i).copied() else {
                    return Err("No such track".into());
                };
                if self.session.is_none() {
                    if !self.disc.is_audio() {
                        return Err("No audio CD in the drive".into());
                    }
                    self.hold = false;
                    self.start();
                    if let Some(e) = &self.error {
                        return Err(e.clone());
                    }
                }
                if let Some(s) = &self.session {
                    s.playback.seek(track.start);
                    s.playback.set_paused(false);
                }
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
        let seconds = |sectors: u32| f64::from(sectors) / f64::from(SECTORS_PER_SECOND);
        let info = |i: usize| self.album.as_ref().and_then(|a| a.tracks.get(i));
        let tracklist: Vec<Value> = self
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                json!({
                    "title": info(i).map(|t| &t.title),
                    "artist": info(i).map(|t| &t.artist),
                    "length": seconds(t.end - t.start),
                })
            })
            .collect();
        let mut status = json!({
            "drive": self.disc.as_str(),
            "state": if self.disc.is_audio() { "stopped" } else { "idle" },
            "tracks": if self.disc.is_audio() { json!(self.tracks.len()) } else { Value::Null },
            "track": null,
            "elapsed": null,
            "length": null,
            "error": self.error,
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
        let pos = s.playback.position();
        let i = track_index(&self.tracks, pos);
        let track = self.tracks[i];
        status["track"] = json!(i + 1);
        status["elapsed"] = json!(seconds(pos.saturating_sub(track.start)));
        status["length"] = json!(seconds(track.end - track.start));
        status
    }
}

/// One reconcile pass against the drive.
pub async fn tick(state: &AppState) {
    let eject_when_finished = state.config.read().await.eject_when_finished;
    let mut events = Vec::new();
    let inserted = state
        .player
        .lock()
        .await
        .poll(eject_when_finished, &mut events)
        .await;
    webhook::fire(state, events);
    if let Some(toc) = inserted {
        tokio::spawn(album::load(state.clone(), toc));
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
