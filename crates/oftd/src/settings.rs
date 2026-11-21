//! Saved settings: the power-on config, as JSON in one file (`--settings`).
//!
//! The file is `{"config": {...}}`, the shape the RP2350 Timer keeps in flash. It is read
//! once at start-up and written only on request (`POST`/`DELETE /config/save`). A write
//! goes to a temporary file first and is renamed into place, so a power cut leaves either
//! the old settings or the new ones, never half a file.

use crate::protocol::Config;
use std::path::{Path, PathBuf};

/// Where `oftd` keeps its settings when not told: `$XDG_CONFIG_HOME/oftd/settings.json`,
/// or `~/.config/oftd/settings.json`.
pub fn default_path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()).map(|h| PathBuf::from(h).join(".config")))?;
    Some(dir.join("oftd").join("settings.json"))
}

/// Everything in the file, including keys this version does not know (kept on a save).
fn read(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// The saved power-on config, if there is one and it is usable. A file that is missing,
/// damaged or holds a config this version refuses means none: the built-in defaults.
pub fn load(path: &Path) -> Option<Config> {
    let config: Config = serde_json::from_value(read(path).remove("config")?).ok()?;
    config.check().ok()?;
    Some(config)
}

/// Store `config` as the power-on config, or forget it (`None`).
pub fn save(path: &Path, config: Option<&Config>) -> std::io::Result<()> {
    let mut all = read(path);
    match config {
        Some(c) => all.insert("config".into(), serde_json::to_value(c).map_err(std::io::Error::other)?),
        None => all.remove("config"),
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut text = serde_json::to_vec_pretty(&all).map_err(std::io::Error::other)?;
    text.push(b'\n');
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&text)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}
