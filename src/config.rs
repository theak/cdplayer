//! User-editable settings, edited from the web UI and persisted as JSON in the data
//! volume so they survive container restarts.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// POSTed once a disc starts playing (e.g. a Home Assistant webhook that powers on the
    /// receiver). Empty = disabled.
    pub start_webhook: String,
    /// POSTed when playback stops for any reason: stop, eject, end of disc, or error.
    pub stop_webhook: String,
    /// Eject the disc after its last track finishes (the drive has no eject button).
    pub eject_when_finished: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            start_webhook: String::new(),
            stop_webhook: String::new(),
            eject_when_finished: true,
        }
    }
}

impl Config {
    /// Trim the URLs and reject anything that isn't blank or http(s).
    pub fn normalized(mut self) -> Result<Self, String> {
        for (label, url) in [
            ("Start webhook", &mut self.start_webhook),
            ("Stop webhook", &mut self.stop_webhook),
        ] {
            *url = url.trim().to_string();
            if !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(format!("{label} must start with http:// or https://"));
            }
        }
        Ok(self)
    }
}

/// Load settings, falling back to defaults when the file is missing or unreadable.
pub fn load(path: &Path) -> Config {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            eprintln!("cdplayer: ignoring unreadable {}: {e}", path.display());
            Config::default()
        }),
        Err(_) => Config::default(),
    }
}

/// Write settings atomically (temp file + rename), creating the directory if needed.
pub fn save(path: &Path, cfg: &Config) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(cfg)?)?;
    std::fs::rename(&tmp, path)
}
