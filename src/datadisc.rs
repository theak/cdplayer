//! MP3 and FLAC files on a data disc: finding them, decoding them with Symphonia, and
//! reading their tags and cover art to build the disc's track list.

use sha1::{Digest, Sha1};
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use symphonia::core::audio::{SampleBuffer, SignalSpec};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::{MetadataOptions, MetadataRevision, StandardTagKey};
use symphonia::core::probe::{Hint, ProbeResult};
use symphonia::core::units::{Time, TimeBase};

use crate::AppState;
use crate::album::{Album, TrackInfo};
use crate::iso9660::{IsoFile, ReadAt, Volume};

const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac"];
const COVER_NAMES: &[&str] = &["cover", "folder", "front", "albumart"];
const MAX_ART_BYTES: u64 = 10 << 20;

fn extension(path: &str) -> String {
    path.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

/// File name without directories or extension.
fn stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// The disc's playable files, in path order (so `01 …` sorts before `02 …`, and an
/// album's folder plays through before the next).
pub fn audio_files(volume: &Volume) -> Vec<IsoFile> {
    let mut files: Vec<IsoFile> = volume
        .files
        .iter()
        .filter(|f| f.len > 0 && AUDIO_EXTENSIONS.contains(&extension(&f.path).as_str()))
        .cloned()
        .collect();
    files.sort_by_key(|f| f.path.to_lowercase());
    files
}

/// An ID for a data disc (used to match background scan results to the disc still in the
/// drive): a hash of its label, creation time, and file layout.
pub fn disc_id(volume: &Volume, files: &[IsoFile]) -> String {
    let mut hash = Sha1::new();
    hash.update(&volume.label);
    hash.update(&volume.created);
    for f in files {
        hash.update(format!("{}:{}:{}", f.path, f.start, f.len));
    }
    let digest = hash.finalize();
    format!("data-{}", digest[..8].iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// Track info before the files are scanned: titles from file names.
pub fn placeholder_album(volume: &Volume, files: &[IsoFile]) -> Album {
    Album {
        title: volume.label.clone(),
        artist: String::new(),
        cover: None,
        tracks: files
            .iter()
            .map(|f| TrackInfo {
                title: stem(&f.path).to_string(),
                artist: String::new(),
            })
            .collect(),
    }
}

/// A file on the disc as a seekable byte stream, for Symphonia.
struct FileStream<R> {
    disc: Arc<R>,
    start: u64,
    len: u64,
    pos: u64,
}

impl<R: ReadAt> Read for FileStream<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = (self.len - self.pos.min(self.len)).min(buf.len() as u64) as usize;
        self.disc.read_exact_at(&mut buf[..n], self.start + self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R> Seek for FileStream<R> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => self.len as i64 + d,
        };
        self.pos = u64::try_from(pos).map_err(|_| io::ErrorKind::InvalidInput)?;
        Ok(self.pos)
    }
}

impl<R: ReadAt + Send + Sync> MediaSource for FileStream<R> {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

fn probe<R: ReadAt + Send + Sync + 'static>(disc: &Arc<R>, file: &IsoFile) -> Result<ProbeResult, String> {
    let stream = FileStream {
        disc: disc.clone(),
        start: file.start,
        len: file.len,
        pos: 0,
    };
    let mss = MediaSourceStream::new(Box::new(stream), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(&extension(&file.path));
    symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("{}: {e}", file.path))
}

/// A file being decoded to interleaved 16-bit samples.
pub struct Decoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    pub rate: u32,
    pub channels: u32,
    buf: Option<(SampleBuffer<i16>, u64, SignalSpec)>,
}

impl Decoder {
    pub fn open<R: ReadAt + Send + Sync + 'static>(disc: &Arc<R>, file: &IsoFile) -> Result<Self, String> {
        let format = probe(disc, file)?.format;
        let track = format
            .default_track()
            .ok_or_else(|| format!("{}: no audio track", file.path))?;
        let params = &track.codec_params;
        let (Some(rate), Some(channels)) = (params.sample_rate, params.channels) else {
            return Err(format!("{}: unknown sample format", file.path));
        };
        let decoder = symphonia::default::get_codecs()
            .make(params, &DecoderOptions::default())
            .map_err(|e| format!("{}: {e}", file.path))?;
        Ok(Decoder {
            track_id: track.id,
            time_base: params.time_base,
            rate,
            channels: channels.count() as u32,
            format,
            decoder,
            buf: None,
        })
    }

    /// Jump to `secs` into the file, returning where it actually landed (seconds), which
    /// can be a little earlier.
    pub fn seek(&mut self, secs: f64) -> Result<f64, String> {
        let time = Time::new(secs.trunc() as u64, secs.fract());
        let to = SeekTo::Time {
            time,
            track_id: Some(self.track_id),
        };
        let seeked = self
            .format
            .seek(SeekMode::Accurate, to)
            .map_err(|e| e.to_string())?;
        self.decoder.reset();
        Ok(self.time_base.map_or(secs, |tb| {
            let t = tb.calc_time(seeked.actual_ts);
            t.seconds as f64 + t.frac
        }))
    }

    /// The next chunk of samples, or `None` at the end of the file.
    pub fn next(&mut self) -> Result<Option<&[i16]>, String> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymphoniaError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(None);
                }
                Err(SymphoniaError::ResetRequired) => return Ok(None),
                Err(e) => return Err(e.to_string()),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let audio = match self.decoder.decode(&packet) {
                Ok(a) => a,
                Err(SymphoniaError::DecodeError(_)) => continue, // skip a corrupt frame
                Err(e) => return Err(e.to_string()),
            };
            let (frames, spec) = (audio.capacity() as u64, *audio.spec());
            if self
                .buf
                .as_ref()
                .is_none_or(|(_, cap, s)| *cap < frames || *s != spec)
            {
                self.buf = Some((SampleBuffer::new(frames, spec), frames, spec));
            }
            let (buf, _, _) = self.buf.as_mut().expect("buffer");
            buf.copy_interleaved_ref(audio);
            return Ok(Some(buf.samples()));
        }
    }
}

/// What a file's tags and headers say about it.
#[derive(Default)]
struct FileInfo {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    length: Option<f64>,
    picture: Option<Art>,
}

#[derive(Clone)]
pub struct Art {
    pub mime: String,
    pub data: Arc<[u8]>,
}

fn read_tags(info: &mut FileInfo, revision: &MetadataRevision) {
    for tag in revision.tags() {
        let slot = match tag.std_key {
            Some(StandardTagKey::TrackTitle) => &mut info.title,
            Some(StandardTagKey::Artist) => &mut info.artist,
            Some(StandardTagKey::Album) => &mut info.album,
            Some(StandardTagKey::AlbumArtist) => &mut info.album_artist,
            _ => continue,
        };
        let value = tag.value.to_string().trim().to_string();
        if slot.is_none() && !value.is_empty() {
            *slot = Some(value);
        }
    }
    if info.picture.is_none() {
        info.picture = revision.visuals().first().map(|v| Art {
            mime: v.media_type.clone(),
            data: v.data.clone().into(),
        });
    }
}

fn file_info<R: ReadAt + Send + Sync + 'static>(disc: &Arc<R>, file: &IsoFile) -> FileInfo {
    let mut info = FileInfo::default();
    let mut probed = match probe(disc, file) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cdplayer: skipping tags for {e}");
            return info;
        }
    };
    // Tags can come from in front of the stream (e.g. ID3v2) or from the container.
    if let Some(revision) = probed.metadata.get().as_ref().and_then(|m| m.current()) {
        read_tags(&mut info, revision);
    }
    if let Some(revision) = probed.format.metadata().current() {
        read_tags(&mut info, revision);
    }
    if let Some(track) = probed.format.default_track() {
        let p = &track.codec_params;
        if let (Some(frames), Some(rate)) = (p.n_frames, p.sample_rate) {
            info.length = Some(frames as f64 / f64::from(rate));
        }
    }
    info
}

/// `cover.jpg`, `folder.png`, etc. in the first audio file's folder.
fn cover_file<R: ReadAt>(disc: &R, volume: &Volume, first: &IsoFile) -> Option<Art> {
    let dir = first.path.rsplit_once('/').map_or("", |(d, _)| d);
    let image = volume.files.iter().find(|f| {
        let in_dir = f.path.rsplit_once('/').map_or("", |(d, _)| d) == dir;
        let ext = extension(&f.path);
        in_dir
            && matches!(ext.as_str(), "jpg" | "jpeg" | "png")
            && COVER_NAMES.contains(&stem(&f.path).to_lowercase().as_str())
            && f.len <= MAX_ART_BYTES
    })?;
    let mut data = vec![0u8; image.len as usize];
    disc.read_exact_at(&mut data, image.start).ok()?;
    let mime = if extension(&image.path) == "png" { "image/png" } else { "image/jpeg" };
    Some(Art {
        mime: mime.into(),
        data: data.into(),
    })
}

/// The value every file that has one agrees on (untagged files don't count against it).
fn common(values: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    let mut tagged = values.into_iter().flatten();
    let first = tagged.next()?;
    tagged.all(|v| v == first).then_some(first)
}

/// Read every file's tags and lengths, building the disc's album info and finding cover
/// art. Blocking: reads the whole disc's headers.
pub fn scan_files<R: ReadAt + Send + Sync + 'static>(
    disc: &Arc<R>,
    volume: &Volume,
    files: &[IsoFile],
) -> (Album, Vec<Option<f64>>, Option<Art>) {
    let infos: Vec<FileInfo> = files.iter().map(|f| file_info(disc, f)).collect();
    let art = files
        .first()
        .and_then(|first| cover_file(&**disc, volume, first))
        .or_else(|| infos.iter().find_map(|i| i.picture.clone()));

    let album_title = common(infos.iter().map(|i| i.album.clone()));
    let album_artist = common(infos.iter().map(|i| i.album_artist.clone()))
        .or_else(|| common(infos.iter().map(|i| i.artist.clone())));
    let any_artist = infos.iter().any(|i| i.artist.is_some());
    let album = Album {
        title: album_title.unwrap_or_else(|| volume.label.clone()),
        artist: album_artist
            .unwrap_or_else(|| if any_artist { "Various Artists".into() } else { String::new() }),
        cover: None,
        tracks: files
            .iter()
            .zip(&infos)
            .map(|(f, i)| TrackInfo {
                title: i.title.clone().unwrap_or_else(|| stem(&f.path).to_string()),
                artist: i.artist.clone().unwrap_or_default(),
            })
            .collect(),
    };
    let lengths = infos.iter().map(|i| i.length).collect();
    (album, lengths, art)
}

/// Scan a newly inserted data disc in the background and attach the results.
pub async fn scan<R: ReadAt + Send + Sync + 'static>(
    state: AppState,
    disc: Arc<R>,
    disc_id: String,
    volume: Volume,
    files: Vec<IsoFile>,
) {
    let result = tokio::task::spawn_blocking(move || scan_files(&disc, &volume, &files)).await;
    match result {
        Ok((album, lengths, art)) => {
            eprintln!(
                "cdplayer: data disc \"{}\": {} audio files",
                album.title,
                album.tracks.len()
            );
            state
                .player
                .lock()
                .await
                .set_scan(&disc_id, album, lengths, art);
        }
        Err(e) => eprintln!("cdplayer: data disc scan failed: {e}"),
    }
}
