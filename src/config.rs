//! Saved servers (`~/.config/just-video/servers.json`) and their passwords
//! (`credentials.json`, mode 0600 like a mount.cifs credentials file).
//! SteamOS's game-mode session has no reachable Secret Service, so there is no
//! encrypted keystore to use; the file is readable only by its owner.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Server {
    pub name: String,
    /// `smb://[domain;]user@host[:port]`, no password.
    pub url: String,
    /// Renaming, moving and deleting files on it are allowed (off: read only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub writable: bool,
}

#[cfg(test)]
thread_local! {
    /// Per-test config directory, so tests never touch the real one (and
    /// don't race each other through `XDG_CONFIG_HOME`).
    static TEST_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Points this test thread's config at a fresh temporary directory.
#[cfg(test)]
pub(crate) fn temp_config(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jv-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    TEST_DIR.with(|d| *d.borrow_mut() = Some(dir.clone()));
    dir
}

pub fn dir() -> anyhow::Result<PathBuf> {
    // A test that never chose a directory still gets one of its own (per
    // thread), never the real one.
    #[cfg(test)]
    return Ok(TEST_DIR.with(|d| d.borrow().clone()).unwrap_or_else(|| {
        temp_config(&format!("default-{:?}", std::thread::current().id()).replace(['(', ')'], ""))
    }));
    #[cfg(not(test))]
    dir_from_env()
}

#[cfg(not(test))]
fn dir_from_env() -> anyhow::Result<PathBuf> {
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
    // A name of its own: two writers of one file (a background save and the
    // UI's) must not delete each other's half-written file or fail.
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.{}.{n}.tmp", std::process::id()));
    let write = || -> anyhow::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(if private { 0o600 } else { 0o644 })
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.sync_all()?;
        std::fs::rename(&tmp, dir.join(name))?;
        Ok(())
    };
    write().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
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
    /// Played to the end (`seconds` is then unused).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    watched: bool,
}

/// What is remembered about a video's progress.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Place {
    /// Left part way: at this many seconds, in a video of this length if known.
    Resume(f64, Option<f64>),
    /// Played to (near) the end.
    Watched,
}

impl Resume {
    fn place(&self) -> Place {
        if self.watched {
            Place::Watched
        } else {
            Place::Resume(self.seconds, self.duration)
        }
    }
}

/// Videos whose place is remembered; older ones are forgotten.
const MAX_RESUMES: usize = 1000;

/// Serialises writes of `resume.json` (saved from background threads).
static RESUME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Where to continue a file from, if it was left part way.
pub fn resume_position(key: &str) -> Option<f64> {
    match places().get(key) {
        Some(Place::Resume(seconds, _)) => Some(*seconds),
        _ => None,
    }
}

/// Progress of every remembered video, by file key (one read of a small file,
/// for marking a whole folder).
pub fn places() -> BTreeMap<String, Place> {
    read_json::<BTreeMap<String, Resume>>("resume.json")
        .map(|all| all.into_iter().map(|(k, r)| (k, r.place())).collect())
        .unwrap_or_default()
}

/// Remembers (or with `None`, forgets) a video's progress.
pub fn save_resume_position(key: &str, place: Option<Place>) -> anyhow::Result<()> {
    update_place(key, |_| place)
}

/// Records a video stopped at `position`: where to continue, or watched.
pub fn save_stop(key: &str, position: f64, duration: f64) -> anyhow::Result<()> {
    update_place(key, |previous| stop_place(position, duration, previous))
}

/// Forgets or sets the progress of `key`, from what is saved now.
fn update_place(key: &str, f: impl FnOnce(Option<Place>) -> Option<Place>) -> anyhow::Result<()> {
    let _guard = RESUME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut all: BTreeMap<String, Resume> = read_json("resume.json")?;
    let previous = all.get(key).map(Resume::place);
    match f(previous) {
        Some(Place::Watched) if previous == Some(Place::Watched) => return Ok(()),
        Some(place) => {
            let saved = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let (seconds, duration, watched) = match place {
                Place::Resume(seconds, duration) => (seconds, duration.filter(|d| *d > 0.0), false),
                Place::Watched => (0.0, None, true),
            };
            all.insert(
                key.to_string(),
                Resume {
                    seconds,
                    saved,
                    duration,
                    watched,
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

/// What to remember for a video stopped at `position`. A watched video that is
/// only opened briefly stays watched.
pub fn stop_place(position: f64, duration: f64, previous: Option<Place>) -> Option<Place> {
    if let Some(seconds) = resume_point(position, duration) {
        return Some(Place::Resume(seconds, Some(duration).filter(|d| *d > 0.0)));
    }
    if position >= 10.0 {
        return Some(Place::Watched); // past the point worth resuming: near the end
    }
    (previous == Some(Place::Watched)).then_some(Place::Watched)
}

pub fn move_resume_position(from: &str, to: &str) -> anyhow::Result<()> {
    if let Some(point) = places().remove(from) {
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
    (position >= 10.0 && !near_end(position, duration)).then_some(position)
}

/// Whether `position` is in the last moments of a video of `duration`
/// (unknown: 0): it counts as watched.
pub fn near_end(position: f64, duration: f64) -> bool {
    duration > 0.0 && (duration - position < 30.0 || position > duration * 0.95)
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
    /// Most the reader buffers ahead while paused, in MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reserve_mib: Option<u64>,
    /// Whether the control bar shows the buffer-ahead button.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    buffer_button: Option<bool>,
    /// How much a curved screen bends: 1 wraps around the viewer, less is gentler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    curvature: Option<f32>,
    /// The browser panel's width, as a multiple of its usual width.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    browser_width: Option<f32>,
    /// Jumps land exactly on their target (decoding up to it) rather than
    /// on the keyframe before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seek_exact: Option<bool>,
    /// Player preferences (jump lengths, volume step, continuing videos).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    player: Option<Preferences>,
    /// SMB connections a playing video is read over (see [`video_connections`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    video_connections: Option<usize>,
    /// How folders are ordered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sort: Option<SortMode>,
    /// Start the next video in the folder when one ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    autoplay_next: Option<bool>,
    /// Ids of the tips already learned or dismissed (see [`crate::tips`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    tips_done: BTreeSet<String>,
}

/// How a folder's files are ordered (folders always come first, by name).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum SortMode {
    /// A-Z.
    #[default]
    Name,
    /// Largest first.
    Size,
    /// Started videos, then unwatched ones, then watched ones.
    Unwatched,
}

impl SortMode {
    pub fn next(self) -> Self {
        match self {
            SortMode::Name => SortMode::Size,
            SortMode::Size => SortMode::Unwatched,
            SortMode::Unwatched => SortMode::Name,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SortMode::Name => "A-Z",
            SortMode::Size => "Size",
            SortMode::Unwatched => "Unwatched",
        }
    }
}

pub fn autoplay_next() -> bool {
    read_json::<Settings>("settings.json")
        .ok()
        .and_then(|s| s.autoplay_next)
        .unwrap_or(false)
}

pub fn save_autoplay_next(on: bool) -> anyhow::Result<()> {
    let mut settings: Settings = read_json("settings.json")?;
    settings.autoplay_next = Some(on);
    write_json("settings.json", &settings, false)
}

pub fn sort_mode() -> SortMode {
    read_json::<Settings>("settings.json")
        .ok()
        .and_then(|s| s.sort)
        .unwrap_or_default()
}

pub fn save_sort_mode(mode: SortMode) -> anyhow::Result<()> {
    let mut settings: Settings = read_json("settings.json")?;
    settings.sort = Some(mode);
    write_json("settings.json", &settings, false)
}

/// Writes `settings.json`, keeping keys this build doesn't know (top level and
/// inside `player`) that another version saved, so rolling back to an older
/// build and forward again doesn't lose settings the older one lacks.
fn write_settings(settings: &Settings) -> anyhow::Result<()> {
    let mut new = serde_json::to_value(settings)?;
    if let Ok(serde_json::Value::Object(old)) = read_json::<serde_json::Value>("settings.json")
        && let Some(map) = new.as_object_mut()
    {
        for (key, value) in old {
            match (map.get_mut(&key), value) {
                (None, value) => {
                    map.insert(key, value);
                }
                (Some(serde_json::Value::Object(mine)), serde_json::Value::Object(theirs))
                    if key == "player" =>
                {
                    for (k, v) in theirs {
                        mine.entry(k).or_insert(v);
                    }
                }
                _ => {}
            }
        }
    }
    write_json("settings.json", &new, false)
}

/// Player choices made on the Settings page.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Preferences {
    /// D-pad left/right (and a sideways stick flick), seconds.
    pub short_jump: u32,
    /// The same with the grip held, seconds.
    pub long_jump: u32,
    /// D-pad up/down step of the headset's volume, percent.
    pub volume_step: u32,
    /// Continue videos where they were left.
    pub resume: bool,
    /// The D-pad moves through menus (focus, A clicks, B goes back).
    pub dpad_menus: bool,
    /// Thumbnails in video lists.
    pub thumbnails: bool,
    /// The A-B loop button on the player bar.
    pub ab_loop: bool,
    /// Short tips about hidden gestures in the browser.
    pub tips: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            short_jump: 5,
            long_jump: 60,
            volume_step: 10,
            resume: true,
            dpad_menus: false,
            thumbnails: false,
            ab_loop: false,
            tips: true,
        }
    }
}

/// Choices for the jump lengths and the volume step.
pub const SHORT_JUMP_PRESETS: [u32; 4] = [5, 10, 15, 30];
pub const LONG_JUMP_PRESETS: [u32; 5] = [30, 60, 120, 300, 600];
pub const VOLUME_STEP_PRESETS: [u32; 3] = [5, 10, 20];

impl Preferences {
    /// Seconds a D-pad press jumps (`direction` -1 or +1); the grip makes it long.
    pub fn jump(&self, direction: i32, long: bool) -> f64 {
        let seconds = if long {
            self.long_jump
        } else {
            self.short_jump
        };
        seconds.max(1) as f64 * direction.signum() as f64
    }
}

/// "5 s", "1 min", "1 min 30 s".
pub fn format_jump(seconds: u32) -> String {
    match (seconds / 60, seconds % 60) {
        (0, s) => format!("{s} s"),
        (m, 0) => format!("{m} min"),
        (m, s) => format!("{m} min {s} s"),
    }
}

pub fn preferences() -> Preferences {
    read_json::<Settings>("settings.json")
        .ok()
        .and_then(|s| s.player)
        .unwrap_or_default()
}

/// Changes the saved preferences: read, `change`, write, so other settings
/// are kept. Returns the result.
pub fn update_preferences(change: impl FnOnce(&mut Preferences)) -> anyhow::Result<Preferences> {
    let mut settings: Settings = read_json("settings.json")?;
    let mut prefs = settings.player.unwrap_or_default();
    change(&mut prefs);
    settings.player = Some(prefs);
    write_settings(&settings)?;
    Ok(prefs)
}

/// Ids of the tips already learned or dismissed.
pub fn tips_done() -> BTreeSet<String> {
    read_json::<Settings>("settings.json")
        .map(|s| s.tips_done)
        .unwrap_or_default()
}

/// Remembers a tip as done.
pub fn mark_tip_done(id: &str) -> anyhow::Result<()> {
    let mut settings: Settings = read_json("settings.json")?;
    if settings.tips_done.insert(id.to_string()) {
        write_json("settings.json", &settings, false)?;
    }
    Ok(())
}

/// Forgets every learned and dismissed tip, so they all show again.
pub fn reset_tips() -> anyhow::Result<()> {
    let mut settings: Settings = read_json("settings.json")?;
    settings.tips_done.clear();
    write_json("settings.json", &settings, false)
}

/// Choices for how many SMB connections a playing video is read over.
pub const VIDEO_CONNECTION_PRESETS: [usize; 2] = [1, 2];

/// SMB connections a playing video is read over: one by default. With two,
/// each read goes to the connection with the least data still to come, so
/// a jump's first read gets past reads left from before it, and over Wi-Fi
/// two TCP connections can carry more than one and ride out a stall of
/// either. Experimental, chosen on the Settings page.
pub fn video_connections() -> usize {
    read_json::<Settings>("settings.json")
        .ok()
        .and_then(|s| s.video_connections)
        .unwrap_or(1)
        .clamp(1, 4)
}

pub fn save_video_connections(connections: usize) -> anyhow::Result<()> {
    let mut settings: Settings = read_json("settings.json")?;
    settings.video_connections = Some(connections.clamp(1, 4));
    write_settings(&settings)
}

/// Where a jump lands: name and whether it is exact (see [`seek_exact`]).
pub const SEEK_LANDING_PRESETS: [(&str, bool); 2] = [("Keyframe", false), ("Exact", true)];

/// Whether jumps decode on to their exact target. Off, a jump shows the
/// keyframe before the target at once, which can be seconds early with
/// long keyframe intervals but is as quick as a seek gets.
pub fn seek_exact() -> bool {
    read_json::<Settings>("settings.json")
        .ok()
        .and_then(|s| s.seek_exact)
        .unwrap_or(false)
}

pub fn save_seek_exact(exact: bool) -> anyhow::Result<()> {
    let mut settings: Settings = read_json("settings.json")?;
    settings.seek_exact = Some(exact);
    write_settings(&settings)
}

/// Choices for how much a curved screen bends: name and the fraction of
/// the full curve (a cylinder around the viewer).
pub const CURVATURE_PRESETS: [(&str, f32); 4] = [
    ("Slight", 0.25),
    ("Medium", 0.5),
    ("Strong", 0.75),
    ("Full", 1.0),
];

pub fn curvature() -> f32 {
    read_json::<Settings>("settings.json")
        .ok()
        .and_then(|s| s.curvature)
        .map_or(1.0, |c| c.clamp(0.05, 1.0))
}

pub fn save_curvature(curvature: f32) -> anyhow::Result<()> {
    let mut settings: Settings = read_json("settings.json")?;
    settings.curvature = Some(curvature);
    write_settings(&settings)
}

/// The preset nearest to `curvature` (index into [`CURVATURE_PRESETS`]).
pub fn curvature_preset(curvature: f32) -> usize {
    CURVATURE_PRESETS
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| (a.1 - curvature).abs().total_cmp(&(b.1 - curvature).abs()))
        .map_or(0, |(i, _)| i)
}

/// Choices for the browser panel's width (multiples of the usual width).
pub const BROWSER_WIDTH_PRESETS: [f32; 4] = [0.5, 0.75, 1.0, 1.25];

pub fn browser_width() -> f32 {
    read_json::<Settings>("settings.json")
        .ok()
        .and_then(|s| s.browser_width)
        .map_or(1.0, |w| w.clamp(0.5, 1.25))
}

pub fn save_browser_width(width: f32) -> anyhow::Result<()> {
    let mut settings: Settings = read_json("settings.json")?;
    settings.browser_width = Some(width);
    write_settings(&settings)
}

/// "75%".
pub fn percent(fraction: f32) -> String {
    format!("{}%", (fraction * 100.0).round())
}

pub fn buffer_button() -> bool {
    read_json::<Settings>("settings.json")
        .ok()
        .and_then(|s| s.buffer_button)
        .unwrap_or(true)
}

pub fn save_buffer_button(shown: bool) -> anyhow::Result<()> {
    let mut settings: Settings = read_json("settings.json")?;
    settings.buffer_button = Some(shown);
    write_settings(&settings)
}

/// Choices for how much to buffer ahead while paused (bytes).
pub const RESERVE_PRESETS: [u64; 4] = [512 << 20, 1 << 30, 2 << 30, 4 << 30];

pub fn reserve_bytes() -> u64 {
    read_json::<Settings>("settings.json")
        .ok()
        .and_then(|s| s.reserve_mib)
        .map_or(crate::readahead::RESERVE_BYTES, |mib| mib << 20)
}

pub fn save_reserve_bytes(bytes: u64) -> anyhow::Result<()> {
    let mut settings: Settings = read_json("settings.json")?;
    settings.reserve_mib = Some(bytes >> 20);
    write_settings(&settings)
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

pub fn save_caption_settings(captions: CaptionSettings) -> anyhow::Result<()> {
    let mut settings: Settings = read_json("settings.json")?;
    settings.captions = Some(captions);
    write_settings(&settings)
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
    write_settings(&settings)
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
    fn old_settings_files_still_parse() {
        let s: Settings =
            serde_json::from_str(r#"{"captions":{"scale":1.2,"raise":0.1}}"#).unwrap();
        assert_eq!(s.player, None);
        // A partial object (fields added later) fills in defaults; fields no
        // longer used are ignored.
        let s: Settings =
            serde_json::from_str(r#"{"player":{"volume":70,"volume_step":5}}"#).unwrap();
        let p = s.player.unwrap();
        assert_eq!((p.volume_step, p.short_jump), (5, 5));
        assert!(p.resume);
        assert!(!p.thumbnails);
        let s: Settings = serde_json::from_str(r#"{"player":{"thumbnails":true}}"#).unwrap();
        assert!(s.player.unwrap().thumbnails);
    }

    #[test]
    fn preferences_round_trip_keeping_other_settings() {
        let dir = temp_config("prefs");
        assert_eq!(preferences(), Preferences::default());
        save_curvature(0.5).unwrap();
        update_preferences(|p| p.long_jump = 600).unwrap();
        update_preferences(|p| p.volume_step = 20).unwrap();
        let p = preferences();
        assert_eq!((p.long_jump, p.volume_step), (600, 20));
        assert_eq!(curvature(), 0.5);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sort_mode_is_saved_and_keeps_other_settings() {
        let dir = temp_config("sort");
        assert_eq!(sort_mode(), SortMode::Name);
        save_curvature(0.5).unwrap();
        save_sort_mode(SortMode::Unwatched).unwrap();
        assert_eq!(sort_mode(), SortMode::Unwatched);
        assert!(!autoplay_next(), "off unless chosen");
        save_autoplay_next(true).unwrap();
        assert!(autoplay_next());
        assert_eq!(sort_mode(), SortMode::Unwatched);
        assert_eq!(curvature(), 0.5);
        assert_eq!(SortMode::Name.next().next().next(), SortMode::Name);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_ab_loop_is_off_by_default_and_persists() {
        let dir = temp_config("ab-loop-pref");
        assert!(!Preferences::default().ab_loop);
        // Settings saved before the option existed have it off.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.json"), r#"{"player":{"short_jump":10}}"#).unwrap();
        assert!(!preferences().ab_loop);
        update_preferences(|p| p.ab_loop = true).unwrap();
        assert!(preferences().ab_loop);
        assert_eq!(preferences().short_jump, 10);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tips_default_on_and_round_trip() {
        let dir = temp_config("tips");
        assert!(preferences().tips);
        assert!(tips_done().is_empty());
        let old: Settings = serde_json::from_str(r#"{"player":{"short_jump":7}}"#).unwrap();
        assert!(old.player.unwrap().tips, "older saves have tips on");
        mark_tip_done("hold_server").unwrap();
        mark_tip_done("hold_server").unwrap();
        update_preferences(|p| p.tips = false).unwrap();
        assert_eq!(
            tips_done().iter().map(String::as_str).collect::<Vec<_>>(),
            ["hold_server"]
        );
        assert!(!preferences().tips);
        reset_tips().unwrap();
        assert!(tips_done().is_empty());
        assert!(!preferences().tips, "reset leaves the on/off choice");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn jump_lengths() {
        let p = Preferences::default();
        assert_eq!(p.jump(-1, false), -5.0);
        assert_eq!(p.jump(1, true), 60.0);
        assert_eq!(format_jump(5), "5 s");
        assert_eq!(format_jump(60), "1 min");
        assert_eq!(format_jump(90), "1 min 30 s");
    }

    #[test]
    fn stopping_near_the_end_marks_watched() {
        let (d, none) = (3600.0, None);
        let part = |s| Some(Place::Resume(s, Some(d)));
        assert_eq!(stop_place(5.0, d, none), None);
        assert_eq!(stop_place(754.0, d, none), part(754.0));
        assert_eq!(stop_place(3590.0, d, none), Some(Place::Watched));
        assert_eq!(stop_place(3500.0, d, none), Some(Place::Watched)); // past 95%
        assert_eq!(
            stop_place(600.0, 0.0, none),
            Some(Place::Resume(600.0, None))
        );
        // A watched video peeked at for a moment stays watched; a resume point is dropped.
        assert_eq!(
            stop_place(2.0, d, Some(Place::Watched)),
            Some(Place::Watched)
        );
        assert_eq!(stop_place(2.0, d, part(900.0)), None);
        // Watching it again part way replaces the mark.
        assert_eq!(stop_place(600.0, d, Some(Place::Watched)), part(600.0));
    }

    #[test]
    fn resume_skips_start_and_end() {
        assert_eq!(resume_point(5.0, 3600.0), None);
        assert_eq!(resume_point(600.0, 3600.0), Some(600.0));
        assert_eq!(resume_point(3580.0, 3600.0), None, "credits");
        assert_eq!(resume_point(110.0, 120.0), None);
        assert_eq!(resume_point(600.0, 0.0), Some(600.0), "unknown length");
        assert!(near_end(3580.0, 3600.0) && !near_end(600.0, 3600.0) && !near_end(600.0, 0.0));
    }

    #[test]
    fn round_trip_with_private_credentials() {
        // Not through XDG_CONFIG_HOME: tests on other threads read it.
        let dir = temp_config("config");
        let server = Server {
            name: "PC".into(),
            url: "smb://alice@192.168.1.10".into(),
            writable: true,
        };
        save_server(server.clone(), "secret").unwrap();
        assert_eq!(servers().unwrap(), vec![server.clone()]);
        assert_eq!(password(&server.url).unwrap().as_deref(), Some("secret"));
        let mode = std::fs::metadata(dir.join("credentials.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let key = "smb://alice@192.168.1.10/media/VR/clip.mp4";
        assert_eq!(layout_override(key).unwrap(), None);
        let l = LayoutOverride {
            projection: crate::vr::Projection::Equirect180,
            stereo: crate::vr::Stereo::SideBySide,
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
        save_resume_position(key, Some(Place::Resume(754.0, Some(5400.0)))).unwrap();
        move_resume_position(key, "smb://alice@192.168.1.10/media/VR/renamed.mp4").unwrap();
        assert_eq!(resume_position(key), None);
        assert_eq!(
            resume_position("smb://alice@192.168.1.10/media/VR/renamed.mp4"),
            Some(754.0)
        );
        let done = "smb://alice@192.168.1.10/media/VR/done.mp4";
        save_stop(done, 3590.0, 3600.0).unwrap();
        assert_eq!(places().get(done), Some(&Place::Watched));
        assert_eq!(resume_position(done), None);
        save_stop(done, 3.0, 3600.0).unwrap();
        assert_eq!(places().get(done), Some(&Place::Watched));
        save_stop(done, 900.0, 3600.0).unwrap();
        assert_eq!(
            places().get(done),
            Some(&Place::Resume(900.0, Some(3600.0)))
        );
        assert!(remove_server("PC").unwrap());
        assert!(servers().unwrap().is_empty());
        assert_eq!(password(&server.url).unwrap(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn saving_keeps_settings_from_other_versions() {
        let dir = temp_config("unknown-keys");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("settings.json"),
            r#"{"future":{"a":1},"player":{"short_jump":7,"future_pref":true}}"#,
        )
        .unwrap();
        update_preferences(|p| p.long_jump = 90).unwrap();
        save_curvature(0.5).unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("settings.json")).unwrap()).unwrap();
        assert_eq!(v["future"]["a"], 1);
        assert_eq!(v["player"]["future_pref"], true);
        assert_eq!(v["player"]["short_jump"], 7);
        assert_eq!(v["player"]["long_jump"], 90);
        assert_eq!(v["curvature"], 0.5);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn saving_leaves_only_the_file() {
        let dir = temp_config("write-clean");
        for n in 0..3 {
            update_preferences(|p| p.long_jump = 30 + n).unwrap();
        }
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["settings.json"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
