//! In-process playback: a thread reads raw audio straight off the disc and writes it to
//! ALSA. CD audio is already 44.1 kHz 16-bit stereo, so it goes to the sound card as-is.
//!
//! The thread owns the drive handle and the sound card for as long as it runs; pause,
//! seek, and stop are flags it checks between chunks.

use alsa::pcm::{Access, Format, HwParams, IO, PCM};
use alsa::{Direction, ValueOr};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::drive::{AudioReader, SECTOR_BYTES};

/// Sectors per read/write (1/5 s), which bounds how long pause/skip/stop take to land.
const CHUNK: u32 = 15;
const READ_RETRIES: usize = 3;
/// Stereo 16-bit samples ("frames" in ALSA terms) per CD sector.
const FRAMES_PER_SECTOR: i64 = (SECTOR_BYTES / 4) as i64;

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
    seek: Mutex<Option<u32>>,
    /// The sector currently coming out of the speakers.
    position: AtomicU32,
}

pub struct Playback {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<Outcome>>,
}

impl Playback {
    /// Play sectors `start..end` on ALSA PCM `device`.
    pub fn start(reader: AudioReader, device: String, start: u32, end: u32) -> Self {
        let shared = Arc::new(Shared::default());
        shared.position.store(start, Relaxed);
        let s = shared.clone();
        let thread = std::thread::spawn(move || run(&reader, &device, start, end, &s));
        Playback {
            shared,
            thread: Some(thread),
        }
    }

    pub fn position(&self) -> u32 {
        self.shared.position.load(Relaxed)
    }

    pub fn paused(&self) -> bool {
        self.shared.paused.load(Relaxed)
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Relaxed);
    }

    pub fn seek(&self, sector: u32) {
        *self.shared.seek.lock().unwrap() = Some(sector);
        self.shared.position.store(sector, Relaxed);
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

fn open_pcm(device: &str) -> Result<PCM, String> {
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
        hw.set_format(Format::S16LE)?;
        hw.set_channels(2)?;
        hw.set_rate(44_100, ValueOr::Nearest)?;
        hw.set_buffer_time_near(500_000, ValueOr::Nearest)?;
        hw.set_period_time_near(100_000, ValueOr::Nearest)?;
        pcm.hw_params(&hw)
    };
    configure().map_err(|e| format!("Couldn't configure audio device {device}: {e}"))?;
    Ok(pcm)
}

fn run(reader: &AudioReader, device: &str, start: u32, end: u32, s: &Shared) -> Outcome {
    let pcm = match open_pcm(device) {
        Ok(p) => p,
        Err(e) => return Outcome::Failed(e),
    };
    let io = pcm.io_bytes();
    let mut buf = vec![0u8; CHUNK as usize * SECTOR_BYTES];
    let mut next = start; // next sector to read
    let mut was_paused = false;

    loop {
        if s.stop.load(Relaxed) {
            let _ = pcm.drop();
            return Outcome::Stopped;
        }
        if let Some(target) = s.seek.lock().unwrap().take() {
            next = target.clamp(start, end);
            flush(&pcm);
        }
        if s.paused.load(Relaxed) {
            if !was_paused {
                // Rewind to what's actually audible and discard the buffer, so resuming
                // picks up exactly where the sound stopped.
                was_paused = true;
                next = playing(&pcm, next);
                s.position.store(next, Relaxed);
                flush(&pcm);
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        was_paused = false;

        if next >= end {
            let _ = pcm.drain();
            return Outcome::Finished;
        }
        let sectors = CHUNK.min(end - next);
        let chunk = &mut buf[..sectors as usize * SECTOR_BYTES];
        if let Err(e) = read(reader, next, chunk) {
            // Play an unreadable patch (scratch) as silence rather than stopping.
            eprintln!("cdplayer: couldn't read sectors {next}..{}: {e}", next + sectors);
            chunk.fill(0);
        }
        if let Err(e) = write(&pcm, &io, chunk) {
            return Outcome::Failed(format!("Audio output failed: {e}"));
        }
        next += sectors;
        s.started.store(true, Relaxed);
        s.position.store(playing(&pcm, next), Relaxed);
    }
}

fn read(reader: &AudioReader, sector: u32, buf: &mut [u8]) -> std::io::Result<()> {
    let mut result = Ok(());
    for _ in 0..READ_RETRIES {
        result = reader.read(sector, buf);
        if result.is_ok() {
            break;
        }
    }
    result
}

fn write(pcm: &PCM, io: &IO<u8>, mut data: &[u8]) -> alsa::Result<()> {
    while !data.is_empty() {
        match io.writei(data) {
            Ok(frames) => data = &data[frames * 4..],
            Err(e) => pcm.try_recover(e, true)?, // underrun: re-prepare and carry on
        }
    }
    Ok(())
}

/// Discard buffered audio and get ready to play again.
fn flush(pcm: &PCM) {
    let _ = pcm.drop();
    let _ = pcm.prepare();
}

/// The sector now audible: everything up to `written`, minus what's still buffered.
fn playing(pcm: &PCM, written: u32) -> u32 {
    let buffered = pcm.delay().unwrap_or(0).max(0) / FRAMES_PER_SECTOR;
    written.saturating_sub(buffered as u32)
}
