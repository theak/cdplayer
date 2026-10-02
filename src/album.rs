//! Album info (titles, artists, cover art) for an audio CD, looked up on MusicBrainz by
//! disc ID with cover art from the Cover Art Archive, and cached in the data volume so
//! each disc is only looked up once.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::AppState;
use crate::drive::Toc;

/// MusicBrainz asks clients to identify themselves.
const USER_AGENT: &str = concat!(
    "cdplayer/",
    env!("CARGO_PKG_VERSION"),
    " ( https://github.com/theak/cdplayer )"
);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Album {
    pub title: String,
    pub artist: String,
    /// Front cover image URL, if the release has one.
    pub cover: Option<String>,
    pub tracks: Vec<TrackInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrackInfo {
    pub title: String,
    pub artist: String,
}

/// Concatenate a MusicBrainz `artist-credit` list ("A feat. B").
fn credit(v: &Value) -> String {
    v.as_array()
        .map(|credits| {
            credits
                .iter()
                .map(|c| {
                    format!(
                        "{}{}",
                        c["name"].as_str().unwrap_or_default(),
                        c["joinphrase"].as_str().unwrap_or_default()
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Pick the best match from a MusicBrainz `discid` lookup response. A release can span
/// several discs, so find the medium that is this disc — by disc ID, else by track count
/// (for TOC-based fuzzy matches) — preferring releases that have cover art.
pub fn parse(response: &Value, disc_id: &str, track_count: usize) -> Option<Album> {
    let empty = Vec::new();
    let releases = response["releases"].as_array().unwrap_or(&empty);
    let medium_of = |release: &Value| -> Option<Value> {
        let media = release["media"].as_array()?;
        let has_disc = |m: &&Value| {
            m["discs"]
                .as_array()
                .is_some_and(|discs| discs.iter().any(|d| d["id"] == disc_id))
        };
        let tracks = |m: &&Value| m["tracks"].as_array().map_or(0, Vec::len);
        media
            .iter()
            .find(has_disc)
            .or_else(|| media.iter().find(|m| tracks(m) == track_count))
            .cloned()
    };
    let candidates: Vec<(&Value, Value)> = releases
        .iter()
        .filter_map(|r| Some((r, medium_of(r)?)))
        .collect();
    let has_cover = |r: &Value| r["cover-art-archive"]["front"] == true;
    let (release, medium) = candidates
        .iter()
        .find(|(r, _)| has_cover(r))
        .or_else(|| candidates.first())?;

    let artist = credit(&release["artist-credit"]);
    let tracks = medium["tracks"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .map(|t| TrackInfo {
            title: t["title"]
                .as_str()
                .or_else(|| t["recording"]["title"].as_str())
                .unwrap_or_default()
                .to_string(),
            artist: match credit(&t["artist-credit"]) {
                a if a.is_empty() => artist.clone(),
                a => a,
            },
        })
        .collect();
    Some(Album {
        title: release["title"].as_str().unwrap_or_default().to_string(),
        cover: has_cover(release).then(|| {
            format!(
                "https://coverartarchive.org/release/{}/front-500",
                release["id"].as_str().unwrap_or_default()
            )
        }),
        artist,
        tracks,
    })
}

/// Look the disc up on MusicBrainz. `Ok(None)` means it isn't in the database.
pub async fn fetch(client: &reqwest::Client, toc: &Toc) -> Result<Option<Album>, String> {
    let id = toc.musicbrainz_id();
    let url = format!(
        "https://musicbrainz.org/ws/2/discid/{id}?toc={}&inc=artist-credits+recordings&cdstubs=no&fmt=json",
        toc.musicbrainz_toc()
    );
    let resp = client
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let body: Value = resp
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    Ok(parse(&body, &id, toc.audio_tracks().len()))
}

fn cache_path(dir: &Path, disc_id: &str) -> PathBuf {
    dir.join(format!("{disc_id}.json"))
}

/// Fill in the album info for the disc now in the drive: from the cache, else from
/// MusicBrainz (caching a hit). Runs in the background after a disc is inserted.
pub async fn load(state: AppState, toc: Toc) {
    let id = toc.musicbrainz_id();
    let path = cache_path(&state.albums_dir, &id);
    let album = match std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Album>(&s).ok())
    {
        Some(album) => Some(album),
        None => match fetch(&state.client, &toc).await {
            Ok(Some(album)) => {
                eprintln!("cdplayer: disc {id} is \"{}\" by {}", album.title, album.artist);
                let saved = std::fs::create_dir_all(&*state.albums_dir).and_then(|()| {
                    std::fs::write(&path, serde_json::to_string_pretty(&album).unwrap())
                });
                if let Err(e) = saved {
                    eprintln!("cdplayer: couldn't cache album info: {e}");
                }
                Some(album)
            }
            Ok(None) => {
                eprintln!("cdplayer: disc {id} isn't in MusicBrainz");
                None
            }
            Err(e) => {
                eprintln!("cdplayer: MusicBrainz lookup failed: {e}");
                None
            }
        },
    };
    if let Some(album) = album {
        state.player.lock().await.set_album(&id, album);
    }
}
