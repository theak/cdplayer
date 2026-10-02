//! In-process playback on a dedicated thread: either raw audio streamed straight off an
//! audio CD (already 44.1 kHz 16-bit stereo, so it goes to the sound card as-is), or audio
//! files on a data disc decoded with Symphonia.
//!
//! The thread owns the drive handle and the sound card for as long as it runs; pause,
//! seek, and stop are flags it checks between chunks. It reports its position as a track
//! index plus time into that track.

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::datadisc::Decoder;
use crate::drive::{AudioReader, DataReader, SECTOR_BYTES, SECTORS_PER_SECOND, Track};
use crate::iso9660::IsoFile;

/// Sectors per read/write (1/5 s), which bounds how long pause/skip/stop take to land.
const CHUNK: u32 = 15;
const READ_RETRIES: usize = 3;
/// Stereo samples ("frames" in ALSA terms) per CD sector.
const FRAMES_PER_SECTOR: u64 = (SECTOR_BYTES / 4) as u64;
const PAUSE_POLL: Duration = Duration::from_millis(50);

/// What to play.
pub enum Source {
    /// An audio CD's tracks.
    Cdda { reader: AudioReader, tracks: Vec<Track> },
    /// Audio files on a data disc.
    Files { reader: DataReader, files: Vec<IsoFile> },
}

/// How a playback run ended on its own.
pub enum Outcome {
    Finished,
    Stopped,
    Failed(String),
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    paused: AtomicBool,
    /// Set once audio has reached the sound card.
    started: AtomicBool,
    /// A track to jump to.
    seek: Mutex<Option<usize>>,
    /// The track now audible, and how far into it (ms).
    track: AtomicUsize,
    elapsed_ms: AtomicU64,
}

impl Shared {
    fn report(&self, track: usize, elapsed_secs: f64) {
        self.track.store(track, Relaxed);
        self.elapsed_ms.store((elapsed_secs * 1000.0) as u64, Relaxed);
    }

    fn take_seek(&self) -> Option<usize> {
        self.seek.lock().unwrap().take()
    }
}

pub struct Playback {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<Outcome>>,
}

impl Playback {
    /// Start playing `source` from track `first` on ALSA PCM `device`.
    pub fn start(source: Source, device: String, first: usize) -> Self {
        let shared = Arc::new(Shared::default());
        shared.track.store(first, Relaxed);
        let s = shared.clone();
        let thread = std::thread::spawn(move || match source {
            Source::Cdda { reader, tracks } => play_cdda(&reader, &tracks, first, &device, &s),
            Source::Files { reader, files } => play_files(&Arc::new(reader), &files, first, &device, &s),
        });
        Playback {
            shared,
            thread: Some(thread),
        }
    }

    pub fn track(&self) -> usize {
        self.shared.track.load(Relaxed)
    }

    pub fn elapsed(&self) -> f64 {
        self.shared.elapsed_ms.load(Relaxed) as f64 / 1000.0
    }

    pub fn paused(&self) -> bool {
        self.shared.paused.load(Relaxed)
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Relaxed);
    }

    /// Jump to the start of `track`.
    pub fn seek(&self, track: usize) {
        *self.shared.seek.lock().unwrap() = Some(track);
        self.shared.report(track, 0.0);
    }

    pub fn started(&self) -> bool {
        self.shared.started.load(Relaxed)
    }

    /// How the run ended, once the thread has finished on its own.
    pub fn outcome(&mut self) -> Option<Outcome> {
        if !self.thread.as_ref()?.is_finished() {
            return None;
        }
        let thread = self.thread.take()?;
        Some(
            thread
                .join()
                .unwrap_or_else(|_| Outcome::Failed("Playback crashed".into())),
        )
    }

    /// Stop, waiting for the thread to let go of the drive and sound card.
    pub async fn stop(mut self) {
        self.shared.stop.store(true, Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.shared.stop.store(true, Relaxed);
    }
}

/// An open, configured ALSA output.
struct Output {
    pcm: PCM,
    rate: u32,
    channels: u32,
}

impl Output {
    fn open(device: &str, rate: u32, channels: u32) -> Result<Self, String> {
        let pcm = PCM::new(device, Direction::Playback, false).map_err(|e| {
            if e.errno() == libc::EBUSY {
                "The speakers are busy. Is AirPlay playing?".to_string()
            } else {
                format!("Couldn't open audio device {device}: {e}")
            }
        })?;
        let configure = || -> alsa::Result<()> {
            let hw = HwParams::any(&pcm)?;
            hw.set_access(Access::RWInterleaved)?;
            hw.set_format(Format::s16())?;
            hw.set_channels(channels)?;
            hw.set_rate(rate, ValueOr::Nearest)?;
            hw.set_buffer_time_near(500_000, ValueOr::Nearest)?;
            hw.set_period_time_near(100_000, ValueOr::Nearest)?;
            pcm.hw_params(&hw)
        };
        configure().map_err(|e| format!("Couldn't configure audio device {device}: {e}"))?;
        Ok(Output { pcm, rate, channels })
    }

    fn write(&self, mut samples: &[i16]) -> alsa::Result<()> {
        let io = self.pcm.io_i16()?;
        let channels = self.channels as usize;
        while !samples.is_empty() {
            match io.writei(samples) {
                Ok(frames) => samples = &samples[frames * channels..],
                Err(e) => self.pcm.try_recover(e, true)?, // underrun: re-prepare and carry on
            }
        }
        Ok(())
    }

    /// Frames written but not yet heard.
    fn buffered(&self) -> u64 {
        self.pcm.delay().unwrap_or(0).max(0) as u64
    }

    /// Discard buffered audio and get ready to play again.
    fn flush(&self) {
        let _ = self.pcm.drop();
        let _ = self.pcm.prepare();
    }
}

/// Index of the track containing `sector`.
pub(crate) fn track_index(tracks: &[Track], sector: u32) -> usize {
    tracks.iter().rposition(|t| sector >= t.start).unwrap_or(0)
}

fn play_cdda(reader: &AudioReader, tracks: &[Track], first: usize, device: &str, s: &Shared) -> Outcome {
    let out = match Output::open(device, 44_100, 2) {
        Ok(o) => o,
        Err(e) => return Outcome::Failed(e),
    };
    let end = tracks.last().map_or(0, |t| t.end);
    let mut next = tracks.get(first).map_or(end, |t| t.start); // next sector to read
    let mut bytes = vec![0u8; CHUNK as usize * SECTOR_BYTES];
    let mut samples = Vec::with_capacity(bytes.len() / 2);
    let mut was_paused = false;

    // The sector now audible: everything read so far, minus what's still buffered.
    let audible = |next: u32| next.saturating_sub((out.buffered() / FRAMES_PER_SECTOR) as u32);
    let report = |sector: u32| {
        let i = track_index(tracks, sector);
        let into = sector.saturating_sub(tracks[i].start);
        s.report(i, f64::from(into) / f64::from(SECTORS_PER_SECOND));
    };

    loop {
        if s.stop.load(Relaxed) {
            out.flush();
            return Outcome::Stopped;
        }
        if let Some(t) = s.take_seek().and_then(|i| tracks.get(i)) {
            next = t.start;
            out.flush();
        }
        if s.paused.load(Relaxed) {
            if !was_paused {
                // Rewind to what's actually audible and discard the buffer, so resuming
                // picks up exactly where the sound stopped.
                was_paused = true;
                next = audible(next);
                report(next);
                out.flush();
            }
            std::thread::sleep(PAUSE_POLL);
            continue;
        }
        was_paused = false;

        if next >= end {
            let _ = out.pcm.drain();
            return Outcome::Finished;
        }
        let sectors = CHUNK.min(end - next);
        let chunk = &mut bytes[..sectors as usize * SECTOR_BYTES];
        if let Err(e) = read_sectors(reader, next, chunk) {
            // Play an unreadable patch (scratch) as silence rather than stopping.
            eprintln!("cdplayer: couldn't read sectors {next}..{}: {e}", next + sectors);
            chunk.fill(0);
        }
        samples.clear();
        samples.extend(chunk.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
        if let Err(e) = out.write(&samples) {
            return Outcome::Failed(format!("Audio output failed: {e}"));
        }
        next += sectors;
        s.started.store(true, Relaxed);
        report(audible(next));
    }
}

fn read_sectors(reader: &AudioReader, sector: u32, buf: &mut [u8]) -> std::io::Result<()> {
    let mut result = Ok(());
    for _ in 0..READ_RETRIES {
        result = reader.read(sector, buf);
        if result.is_ok() {
            break;
        }
    }
    result
}

fn play_files(reader: &Arc<DataReader>, files: &[IsoFile], first: usize, device: &str, s: &Shared) -> Outcome {
    let mut out: Option<Output> = None;
    let mut i = first;

    'tracks: while i < files.len() {
        s.report(i, 0.0);
        let mut decoder = match Decoder::open(reader, &files[i]) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("cdplayer: skipping {e}");
                i += 1;
                continue;
            }
        };
        // Reopen the sound card only when the format changes, so same-format tracks run
        // together without a gap.
        if out
            .as_ref()
            .is_none_or(|o| o.rate != decoder.rate || o.channels != decoder.channels)
        {
            if let Some(previous) = out.take() {
                let _ = previous.pcm.drain();
            }
            match Output::open(device, decoder.rate, decoder.channels) {
                Ok(o) => out = Some(o),
                Err(e) => return Outcome::Failed(e),
            }
        }
        let o = out.as_ref().expect("output open");
        let mut written: u64 = 0; // frames of this track handed to ALSA
        let mut paused_in_hw = None; // Some(whether the hardware pause worked) while paused

        loop {
            if s.stop.load(Relaxed) {
                o.flush();
                return Outcome::Stopped;
            }
            if let Some(target) = s.take_seek().filter(|&t| t < files.len()) {
                o.flush();
                i = target;
                continue 'tracks;
            }
            if s.paused.load(Relaxed) {
                // Decoded audio can't be cheaply rewound, so pause the hardware in place;
                // if it can't pause, drop the buffer (losing a fraction of a second).
                if paused_in_hw.is_none() {
                    let ok = o.pcm.pause(true).is_ok();
                    if !ok {
                        o.flush();
                    }
                    paused_in_hw = Some(ok);
                }
                std::thread::sleep(PAUSE_POLL);
                continue;
            }
            if paused_in_hw.take() == Some(true) {
                let _ = o.pcm.pause(false);
            }

            let samples = match decoder.next() {
                Ok(Some(samples)) => samples,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("cdplayer: error decoding {}: {e}", files[i].path);
                    break;
                }
            };
            if let Err(e) = o.write(samples) {
                return Outcome::Failed(format!("Audio output failed: {e}"));
            }
            written += (samples.len() / o.channels as usize) as u64;
            s.started.store(true, Relaxed);
            let heard = written.saturating_sub(o.buffered());
            s.report(i, heard as f64 / f64::from(o.rate));
        }
        i += 1;
    }
    if let Some(o) = out {
        let _ = o.pcm.drain();
    }
    Outcome::Finished
}
