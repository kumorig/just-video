//! Saved servers (`~/.config/just-video/servers.json`) and their passwords
//! (`credentials.json`, mode 0600 like a mount.cifs credentials file).
//! SteamOS's game-mode session has no reachable Secret Service, so there is no
//! encrypted keystore to use; the file is readable only by its owner.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Server {
    pub name: String,
    /// `smb://[domain;]user@host[:port]`, no password.
    pub url: String,
}

pub fn dir() -> anyhow::Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("No home directory")?;
    Ok(base.join("just-video"))
}

fn read_json<T: for<'de> Deserialize<'de> + Default>(name: &str) -> anyhow::Result<T> {
    let path = dir()?.join(name);
    match std::fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("Parse {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("Read {}", path.display())),
    }
}

/// Writes atomically; `private` files are created with mode 0600.
fn write_json<T: Serialize>(name: &str, value: &T, private: bool) -> anyhow::Result<()> {
    let dir = dir()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let tmp = dir.join(format!(".{name}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(if private { 0o600 } else { 0o644 })
        .open(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    std::fs::rename(&tmp, dir.join(name))?;
    Ok(())
}

pub fn servers() -> anyhow::Result<Vec<Server>> {
    read_json("servers.json")
}

pub fn password(url: &str) -> anyhow::Result<Option<String>> {
    let credentials: BTreeMap<String, String> = read_json("credentials.json")?;
    Ok(credentials.get(url).cloned())
}

/// Adds or replaces a server (matched by URL) and stores its password.
pub fn save_server(server: Server, password: &str) -> anyhow::Result<()> {
    let mut list = servers()?;
    list.retain(|s| s.url != server.url);
    let url = server.url.clone();
    list.push(server);
    write_json("servers.json", &list, false)?;
    let mut credentials: BTreeMap<String, String> = read_json("credentials.json")?;
    credentials.insert(url, password.to_string());
    write_json("credentials.json", &credentials, true)
}

/// Removes a server by name or URL, with its password. Returns whether it existed.
pub fn remove_server(name_or_url: &str) -> anyhow::Result<bool> {
    let mut list = servers()?;
    let Some(index) = list
        .iter()
        .position(|s| s.name == name_or_url || s.url == name_or_url)
    else {
        return Ok(false);
    };
    let removed = list.remove(index);
    write_json("servers.json", &list, false)?;
    let mut credentials: BTreeMap<String, String> = read_json("credentials.json")?;
    credentials.remove(&removed.url);
    write_json("credentials.json", &credentials, true)?;
    Ok(true)
}

/// A user's choice of how to show one file (when metadata and name are wrong).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct LayoutOverride {
    pub projection: crate::vr::Projection,
    pub stereo: crate::vr::Stereo,
    pub swap_eyes: bool,
    /// Picture corrections (older saves have none).
    #[serde(default)]
    pub image: ImageAdjust,
}

/// Picture corrections for one file.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct ImageAdjust {
    /// Added to every channel (-0.5..0.5; 0 = unchanged).
    pub brightness: f32,
    /// Around mid grey (0.5..2; 1 = unchanged).
    pub contrast: f32,
    /// 0 = grey, 1 = unchanged, 2 = double.
    pub saturation: f32,
    /// Quarter turns clockwise (0..=3).
    pub rotation: u8,
}

impl Default for ImageAdjust {
    fn default() -> Self {
        Self {
            brightness: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            rotation: 0,
        }
    }
}

pub fn layout_override(key: &str) -> anyhow::Result<Option<LayoutOverride>> {
    let all: BTreeMap<String, LayoutOverride> = read_json("layouts.json")?;
    Ok(all.get(key).copied())
}

/// Saves (or with `None`, forgets) the override for a file.
pub fn save_layout_override(key: &str, layout: Option<LayoutOverride>) -> anyhow::Result<()> {
    let mut all: BTreeMap<String, LayoutOverride> = read_json("layouts.json")?;
    let changed = match layout {
        Some(l) => all.insert(key.to_string(), l) != Some(l),
        None => all.remove(key).is_some(),
    };
    if changed {
        write_json("layouts.json", &all, false)?;
    }
    Ok(())
}

pub fn move_layout_override(from: &str, to: &str) -> anyhow::Result<()> {
    if let Some(l) = layout_override(from)? {
        save_layout_override(from, None)?;
        save_layout_override(to, Some(l))?;
    }
    Ok(())
}

/// Where a video was left, to continue from there.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
struct Resume {
    seconds: f64,
    /// Unix time of the save; the oldest are forgotten first.
    saved: u64,
    /// The video's length, for showing how much was watched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    duration: Option<f64>,
}

/// Videos whose place is remembered; older ones are forgotten.
const MAX_RESUMES: usize = 1000;

/// Serialises writes of `resume.json` (saved from background threads).
static RESUME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Where to continue a file from, if it was left part way.
pub fn resume_position(key: &str) -> Option<f64> {
    let all: BTreeMap<String, Resume> = read_json("resume.json").ok()?;
    all.get(key).map(|r| r.seconds)
}

/// Every remembered place: file key to (seconds, video length if known).
pub fn resume_positions() -> BTreeMap<String, (f64, Option<f64>)> {
    read_json::<BTreeMap<String, Resume>>("resume.json")
        .map(|all| {
            all.into_iter()
                .map(|(k, r)| (k, (r.seconds, r.duration)))
                .collect()
        })
        .unwrap_or_default()
}

/// Remembers (or with `None`, forgets) where a file was left, with the
/// video's length when known (so the browser can show how much was watched).
pub fn save_resume_position(key: &str, point: Option<(f64, Option<f64>)>) -> anyhow::Result<()> {
    let _guard = RESUME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut all: BTreeMap<String, Resume> = read_json("resume.json")?;
    match point {
        Some((seconds, duration)) => {
            let saved = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let duration = duration.filter(|d| *d > 0.0);
            all.insert(
                key.to_string(),
                Resume {
                    seconds,
                    saved,
                    duration,
                },
            );
            while all.len() > MAX_RESUMES {
                let oldest = all
                    .iter()
                    .min_by_key(|(_, r)| r.saved)
                    .map(|(k, _)| k.clone())
                    .expect("not empty");
                all.remove(&oldest);
            }
        }
        None if all.remove(key).is_none() => return Ok(()),
        None => {}
    }
    write_json("resume.json", &all, false)
}

pub fn move_resume_position(from: &str, to: &str) -> anyhow::Result<()> {
    if let Some(point) = resume_positions().remove(from) {
        save_resume_position(from, None)?;
        save_resume_position(to, Some(point))?;
    }
    Ok(())
}

/// Carries saved video settings with a moved file or an entire folder tree.
pub fn move_file_state(from: &str, to: &str) -> anyhow::Result<()> {
    let mut layouts: BTreeMap<String, LayoutOverride> = read_json("layouts.json")?;
    if remap_file_keys(&mut layouts, from, to) {
        write_json("layouts.json", &layouts, false)?;
    }
    let _guard = RESUME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut resumes: BTreeMap<String, Resume> = read_json("resume.json")?;
    if remap_file_keys(&mut resumes, from, to) {
        write_json("resume.json", &resumes, false)?;
    }
    Ok(())
}

fn remap_file_keys<T>(entries: &mut BTreeMap<String, T>, from: &str, to: &str) -> bool {
    if from == to {
        return false;
    }
    let keys: Vec<_> = entries
        .keys()
        .filter_map(|key| {
            let suffix = key.strip_prefix(from)?;
            (suffix.is_empty() || suffix.starts_with('/'))
                .then(|| (key.clone(), format!("{to}{suffix}")))
        })
        .collect();
    let changed = !keys.is_empty();
    for (old, new) in keys {
        if let Some(value) = entries.remove(&old) {
            entries.insert(new, value);
        }
    }
    changed
}

/// The place worth remembering for a video stopped at `position`: none near
/// its start (nothing to skip) or its end (watched).
pub fn resume_point(position: f64, duration: f64) -> Option<f64> {
    let near_end = duration > 0.0 && (duration - position < 30.0 || position > duration * 0.95);
    (position >= 10.0 && !near_end).then_some(position)
}

/// A video format: projection and stereo layout.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Format {
    pub projection: crate::vr::Projection,
    pub stereo: crate::vr::Stereo,
}

#[derive(Default, Serialize, Deserialize)]
struct Settings {
    /// None: never changed (use the defaults).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    favourite_formats: Option<Vec<Format>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    captions: Option<CaptionSettings>,
    /// Flat screens: the surroundings show around the picture (passthrough
    /// or the runtime's environment) rather than black.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    see_through: Option<bool>,
    /// Most the reader buffers ahead while paused, in MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reserve_mib: Option<u64>,
}

/// Choices for how much to buffer ahead while paused (bytes).
pub const RESERVE_PRESETS: [u64; 5] = [256 << 20, 512 << 20, 1 << 30, 2 << 30, 4 << 30];

pub fn reserve_bytes() -> u64 {
    read_json::<Settings>("settings.json")
        .ok()
        .and_then(|s| s.reserve_mib)
        .map_or(crate::readahead::RESERVE_BYTES, |mib| mib << 20)
}

pub fn save_reserve_bytes(bytes: u64) -> anyhow::Result<()> {
    let mut settings: Settings = read_json("settings.json")?;
    settings.reserve_mib = Some(bytes >> 20);
    write_json("settings.json", &settings, false)
}

/// Subtitle size and position, the same for every video.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct CaptionSettings {
    /// Size multiplier (1 = default).
    pub scale: f32,
    /// Raised by this share of the screen's height (0 = default, negative = lower).
    pub raise: f32,
}

impl Default for CaptionSettings {
    fn default() -> Self {
        Self {
            scale: 1.0,
            raise: 0.0,
        }
    }
}

pub fn caption_settings() -> CaptionSettings {
    read_json::<Settings>("settings.json")
        .ok()
        .and_then(|s| s.captions)
        .unwrap_or_default()
}

pub fn see_through() -> bool {
    read_json::<Settings>("settings.json")
        .ok()
        .and_then(|s| s.see_through)
        .unwrap_or(true)
}

pub fn save_see_through(see_through: bool) -> anyhow::Result<()> {
    let mut settings: Settings = read_json("settings.json")?;
    settings.see_through = Some(see_through);
    write_json("settings.json", &settings, false)
}

pub fn save_caption_settings(captions: CaptionSettings) -> anyhow::Result<()> {
    let mut settings: Settings = read_json("settings.json")?;
    settings.captions = Some(captions);
    write_json("settings.json", &settings, false)
}

/// Formats the format button cycles through. Default: flat 2D and VR180 3D.
pub fn favourite_formats() -> Vec<Format> {
    use crate::vr::{Projection, Stereo};
    let saved = read_json::<Settings>("settings.json")
        .ok()
        .and_then(|s| s.favourite_formats);
    saved.filter(|f| !f.is_empty()).unwrap_or_else(|| {
        vec![
            Format {
                projection: Projection::Flat,
                stereo: Stereo::Mono,
            },
            Format {
                projection: Projection::Equirect180,
                stereo: Stereo::SideBySide,
            },
        ]
    })
}

pub fn save_favourite_formats(formats: &[Format]) -> anyhow::Result<()> {
    let mut settings: Settings = read_json("settings.json")?;
    settings.favourite_formats = Some(formats.to_vec());
    write_json("settings.json", &settings, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moved_folder_keeps_descendant_settings_without_matching_sibling_prefixes() {
        let mut entries = BTreeMap::from([
            ("server/share/old".into(), 1),
            ("server/share/old/nested/movie.mp4".into(), 2),
            ("server/share/older/movie.mp4".into(), 3),
            ("server/other/old/movie.mp4".into(), 4),
        ]);
        assert!(remap_file_keys(
            &mut entries,
            "server/share/old",
            "server/share/new/old"
        ));
        assert_eq!(entries["server/share/new/old"], 1);
        assert_eq!(entries["server/share/new/old/nested/movie.mp4"], 2);
        assert_eq!(entries["server/share/older/movie.mp4"], 3);
        assert_eq!(entries["server/other/old/movie.mp4"], 4);
        assert!(!entries.contains_key("server/share/old"));
        assert!(!remap_file_keys(&mut entries, "missing", "new"));
    }

    #[test]
    fn resume_skips_start_and_end() {
        assert_eq!(resume_point(5.0, 3600.0), None);
        assert_eq!(resume_point(600.0, 3600.0), Some(600.0));
        assert_eq!(resume_point(3580.0, 3600.0), None, "credits");
        assert_eq!(resume_point(110.0, 120.0), None);
        assert_eq!(resume_point(600.0, 0.0), Some(600.0), "unknown length");
    }

    #[test]
    fn round_trip_with_private_credentials() {
        let dir = std::env::temp_dir().join(format!("jv-config-{}", std::process::id()));
        // SAFETY: single-threaded within this test; other tests don't read XDG_CONFIG_HOME.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        let server = Server {
            name: "PC".into(),
            url: "smb://alice@192.168.1.10".into(),
        };
        save_server(server.clone(), "secret").unwrap();
        assert_eq!(servers().unwrap(), vec![server.clone()]);
        assert_eq!(password(&server.url).unwrap().as_deref(), Some("secret"));
        let mode = std::fs::metadata(dir.join("just-video/credentials.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let key = "smb://alice@192.168.1.10/media/VR/clip.mp4";
        assert_eq!(layout_override(key).unwrap(), None);
        let l = LayoutOverride {
            projection: crate::vr::Projection::Equirect180,
            stereo: crate::vr::Stereo::SideBySide,
            swap_eyes: false,
            image: ImageAdjust::default(),
        };
        save_layout_override(key, Some(l)).unwrap();
        move_layout_override(key, "smb://alice@192.168.1.10/media/VR/renamed.mp4").unwrap();
        assert_eq!(layout_override(key).unwrap(), None);
        assert_eq!(
            layout_override("smb://alice@192.168.1.10/media/VR/renamed.mp4").unwrap(),
            Some(l)
        );
        assert_eq!(resume_position(key), None);
        save_resume_position(key, Some((754.0, Some(5400.0)))).unwrap();
        move_resume_position(key, "smb://alice@192.168.1.10/media/VR/renamed.mp4").unwrap();
        assert_eq!(resume_position(key), None);
        assert_eq!(
            resume_position("smb://alice@192.168.1.10/media/VR/renamed.mp4"),
            Some(754.0)
        );
        assert!(remove_server("PC").unwrap());
        assert!(servers().unwrap().is_empty());
        assert_eq!(password(&server.url).unwrap(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
