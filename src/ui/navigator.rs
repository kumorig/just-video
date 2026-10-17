//! Browser state: servers → shares → folders → videos. Talks to the
//! [`Library`] workers and turns their results into a [`View`].
//!
//! Holding select on an entry selects it: the header then offers Delete,
//! Move and (for one entry) Rename or Start over in place of the
//! breadcrumbs, and other entries can be ticked. Holding select on a server
//! reveals its Settings and Remove buttons.
//!
//! Changing files is opt-in per server: "Allow changing files" in its
//! settings. Without it, holding an entry still selects it (for Start over)
//! but offers no Delete, Move or Rename, and a notice says where to allow them.

use super::browser::{Action, Dialog, Hit, Icon, Row, Tool, View, Watch, format_size};
use super::controls::format_time;
use super::form::{self, Field, Form, Key};
use crate::config::{self, Place, Server, SortMode};
use crate::library::{
    Library, Listing, MoveSelection, Opened, Path, Request, Response, ThumbVideo, file_key,
};
use crate::local;
use crate::media::Thumb;
use crate::playability::{Assessment, Verdict};
use crate::smb::SmbUrl;
use crate::vr::Layout;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

const VIDEO_EXTENSIONS: &[&str] = &["mp4", "m4v", "mkv", "mov", "webm", "avi", "ts", "m2ts"];

/// Shown when an entry is held on a server that doesn't allow changing files.
const READ_ONLY_NOTICE: &str =
    "To change files, allow it in this server's settings (hold the server on the server list).";

pub fn is_video(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| VIDEO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// A row's detail line with where the viewer left off: "Resume 12:34 · title".
fn with_progress(detail: &str, place: Option<Place>) -> String {
    match place {
        Some(Place::Resume(seconds, _)) => format!("Resume {} · {detail}", format_time(seconds)),
        Some(Place::Watched) => format!("Watched · {detail}"),
        None => detail.to_string(),
    }
}

/// Names from the top down to a place: server URL, share, folders.
fn trail(location: &Location) -> Vec<String> {
    match location {
        Location::Servers => Vec::new(),
        // The Settings row's name (never a server URL): marks the way back in.
        Location::Settings => vec!["Settings".into()],
        Location::Shares { server } => vec![server.url.clone()],
        Location::Folder {
            server,
            share,
            path,
        } => [server.url.clone(), share.clone()]
            .into_iter()
            .chain(path.iter().cloned())
            .collect(),
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Location {
    Servers,
    /// The Settings page, off the server list.
    Settings,
    Shares {
        server: Server,
    },
    Folder {
        server: Server,
        share: String,
        path: Path,
    },
}

enum Item {
    Server(Server),
    AddServer,
    /// Opens the Settings page.
    Settings,
    /// How much to buffer ahead while paused; opens a dialog of presets.
    BufferSize,
    /// How much a curved video screen bends; a dialog of presets.
    Curvature,
    /// The browser panel's width; a dialog of presets.
    BrowserWidth,
    /// Where a jump lands: the keyframe before the target, or exactly on
    /// it. Two-way: a click switches.
    SeekLanding,
    /// How far D-pad left/right jump; a dialog of presets.
    ShortJump,
    /// The same with the grip held.
    LongJump,
    /// How much D-pad up/down change the headset's volume.
    VolumeStep,
    /// Continue videos where they were left. Two-way.
    Resume,
    /// The A-B loop button on the player bar. Two-way.
    AbLoop,
    /// The D-pad moves through menus. Two-way.
    DpadMenus,
    /// SMB connections a playing video is read over: one or two. Two-way.
    VideoConnections,
    /// Pictures of the videos in folder lists. Two-way, like the header's
    /// button in a folder.
    Thumbnails,
    Share(String),
    Dir(String),
    Video {
        name: String,
        size: u64,
        /// Last write time from the listing (keys the thumbnail cache).
        modified: u64,
        assessment: Option<Assessment>,
        /// How it will be shown (once probed).
        layout: Option<Layout>,
        broken: Option<String>,
        /// How far it was played, if remembered.
        resume: Option<Place>,
    },
    /// A file that isn't a video: shown dimmed, can be renamed or deleted.
    File {
        name: String,
        size: u64,
    },
}

/// The rows of the Settings page, in order.
const SETTINGS: [Item; 12] = [
    Item::ShortJump,
    Item::LongJump,
    Item::SeekLanding,
    Item::VolumeStep,
    Item::Resume,
    Item::AbLoop,
    Item::DpadMenus,
    Item::Thumbnails,
    Item::BufferSize,
    Item::Curvature,
    Item::BrowserWidth,
    Item::VideoConnections,
];

impl Item {
    fn name(&self) -> &str {
        match self {
            Item::Server(s) => &s.name,
            Item::AddServer
            | Item::BufferSize
            | Item::Curvature
            | Item::BrowserWidth
            | Item::SeekLanding
            | Item::ShortJump
            | Item::LongJump
            | Item::VolumeStep
            | Item::Resume
            | Item::AbLoop
            | Item::DpadMenus
            | Item::VideoConnections
            | Item::Thumbnails => "",
            Item::Settings => "Settings",
            Item::Share(n)
            | Item::Dir(n)
            | Item::Video { name: n, .. }
            | Item::File { name: n, .. } => n,
        }
    }

    /// What [`trail`] calls this item.
    fn trail_name(&self) -> &str {
        match self {
            Item::Server(s) => &s.url,
            other => other.name(),
        }
    }

    fn is_entry(&self) -> bool {
        matches!(self, Item::Dir(_) | Item::Video { .. } | Item::File { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ToolAction {
    /// Next sort order.
    CycleSort,
    /// Start the next video when one ends, or stop.
    ToggleAutoplay,
    CancelSelect,
    /// Forget where the selected video was left.
    ForgetResume,
    DeleteSelected,
    MoveSelected,
    RenameSelected,
    CancelMove,
    /// Stop loading this folder and go back up.
    CancelLoad,
    MoveHere,
    /// Closes the app (top right of the top page).
    Exit,
    /// Thumbnails in this folder on or off (a setting).
    ToggleThumbnails,
}

/// Work in progress behind the browser, for a progress bar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Progress {
    /// Connecting, listing or opening: no known end.
    Busy,
    /// Videos checked so far, as a fraction.
    Fraction(f32),
}

/// What the open form or dialog is for.
enum Purpose {
    AddServer,
    /// Editing the server with this URL.
    EditServer(String),
    Rename(usize),
    RemoveServer(usize),
    /// Deleting these entries.
    Delete(Vec<usize>),
    /// Choosing a buffer size; the buttons are [`config::RESERVE_PRESETS`].
    BufferSize,
    /// Choosing from [`config::CURVATURE_PRESETS`] / [`config::BROWSER_WIDTH_PRESETS`].
    Curvature,
    BrowserWidth,
    /// Choosing from the jump length and volume step presets.
    ShortJump,
    LongJump,
    VolumeStep,
    Info,
}

/// What the open settings dialog shows, to be previewed as the pointer
/// moves over its choices.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Preview {
    Curvature(f32),
    BrowserWidth(f32),
}

pub struct Navigator {
    library: Library,
    location: Location,
    items: Vec<Item>,
    view: View,
    purpose: Option<Purpose>,
    /// Entries held and ticked, to move, delete, rename or start over
    /// (the header shows the tools). On the server list, the one server
    /// showing its Settings and Remove buttons.
    selecting: Option<HashSet<usize>>,
    /// Sources survive navigation until a destination is chosen or cancelled.
    moving: Option<MoveSelection>,
    move_request: Option<u64>,
    folder_loaded: bool,
    /// What each header tool does.
    tool_actions: Vec<ToolAction>,
    /// The item being played (for previous/next).
    playing: Option<usize>,
    /// How folders are ordered.
    sort: SortMode,
    /// Start the next video when one ends.
    autoplay: bool,
    /// Navigation request whose answer we wait for.
    pending: Option<u64>,
    /// The folder listing in progress, and the count last shown.
    listing: Option<(Listing, usize)>,
    /// Playability probes answered and sent for this folder.
    probes: (usize, usize),
    /// Rename/delete/add requests whose answers we wait for.
    pending_changes: HashSet<u64>,
    change_errors: Vec<String>,
    /// Scroll position of each place left, by its [`trail`].
    scrolls: std::collections::HashMap<Vec<String>, f32>,
    /// After going up: the entry we came out of (outlined) and the scroll
    /// to return to once the list has loaded.
    came_from: Option<String>,
    return_scroll: Option<f32>,
    next_id: u64,
    generation: u64,
    dirty: bool,
    /// Exit was chosen on the top page; the app reads it with [`Navigator::take_exit`].
    exit_requested: bool,
    /// Text copied in a form, kept for pasting into the next.
    clipboard: String,
    /// Settings read once and kept up to date here.
    curvature: f32,
    browser_width: f32,
    seek_exact: bool,
    prefs: config::Preferences,
    /// Where the playing video is, for fetching its subtitle files.
    playing_source: Option<(Server, String)>,
    /// The subtitle fetch awaited (request id, track) and its answer.
    subtitle_request: Option<(u64, usize)>,
    subtitle_result: Option<(usize, Result<Vec<crate::subtitles::Cue>, String>)>,
    /// Which folder's thumbnails the worker is making (see
    /// [`Library::set_thumbnail_generation`]); bumped on leaving and toggling off.
    thumb_generation: u64,
    /// Ready thumbnails of this folder, by video name.
    thumbs: std::collections::HashMap<String, Arc<Thumb>>,
    /// Videos already handed to the worker (it is told as probes land).
    thumbs_sent: HashSet<String>,
    /// The first visible video last told to the worker.
    thumb_focus: usize,
}

impl Navigator {
    pub fn new(library: Library) -> Self {
        let mut nav = Self {
            library,
            location: Location::Servers,
            items: Vec::new(),
            view: View::default(),
            purpose: None,
            selecting: None,
            moving: None,
            move_request: None,
            folder_loaded: false,
            tool_actions: Vec::new(),
            sort: config::sort_mode(),
            autoplay: config::autoplay_next(),
            playing: None,
            pending: None,
            listing: None,
            probes: (0, 0),
            pending_changes: HashSet::new(),
            change_errors: Vec::new(),
            scrolls: Default::default(),
            came_from: None,
            return_scroll: None,
            next_id: 1,
            generation: 0,
            dirty: true,
            exit_requested: false,
            clipboard: String::new(),
            curvature: config::curvature(),
            browser_width: config::browser_width(),
            seek_exact: config::seek_exact(),
            prefs: config::preferences(),
            playing_source: None,
            subtitle_request: None,
            subtitle_result: None,
            thumb_generation: 0,
            thumbs: Default::default(),
            thumbs_sent: HashSet::new(),
            thumb_focus: 0,
        };
        nav.show_servers();
        nav
    }

    pub fn view(&self) -> &View {
        &self.view
    }

    /// True (once) when the view changed and must be redrawn.
    /// Whether Exit was chosen since the last call.
    pub fn take_exit(&mut self) -> bool {
        std::mem::take(&mut self.exit_requested)
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub fn redraw(&mut self) {
        self.dirty = true;
    }

    pub fn scroll(&self) -> f32 {
        self.view.scroll
    }

    pub fn set_scroll(&mut self, scroll: f32) {
        if self.view.form.is_some() || self.view.dialog.is_some() {
            return;
        }
        let before = self.view.scroll;
        self.view.scroll = scroll;
        self.view.clamp_scroll();
        if self.view.scroll != before {
            self.dirty = true;
            self.focus_thumbnails();
        }
    }

    pub fn scroll_by(&mut self, rows: f32) {
        self.set_scroll(self.view.scroll + rows);
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Files can be changed here: inside a share of a server that allows it
    /// (see [`Server::writable`]).
    fn editing(&self) -> bool {
        matches!(&self.location, Location::Folder { server, .. } if server.writable)
    }

    fn crumbs(&self) -> Vec<String> {
        let mut crumbs = vec!["Just Video".to_string()];
        match &self.location {
            Location::Servers => {}
            Location::Settings => crumbs.push("Settings".into()),
            Location::Shares { server } => crumbs.push(server.name.clone()),
            Location::Folder {
                server,
                share,
                path,
            } => {
                crumbs.push(server.name.clone());
                crumbs.push(share.clone());
                crumbs.extend(path.iter().cloned());
            }
        }
        crumbs
    }

    fn reset_view(&mut self) {
        self.view = View {
            crumbs: self.crumbs(),
            ..Default::default()
        };
        self.purpose = None;
        self.selecting = None;
        self.dirty = true;
    }

    fn set_status(&mut self, status: impl Into<String>) {
        self.view.status = Some(status.into());
        self.view.rows.clear();
        self.dirty = true;
    }

    /// Before moving to `to`: remembers this list's scroll and, when going
    /// up, which entry we are leaving and where that list was scrolled.
    fn leave(&mut self, to: &Location) {
        if *to == self.location {
            return;
        }
        let (from, to) = (trail(&self.location), trail(to));
        if !self.items.is_empty() {
            self.scrolls.insert(from.clone(), self.view.scroll);
        }
        if to.len() < from.len() && from.starts_with(&to) {
            self.came_from = Some(from[to.len()].clone());
            self.return_scroll = self.scrolls.get(&to).copied();
        } else {
            self.came_from = None;
            self.return_scroll = None;
        }
    }

    /// Once a list has loaded: back to its old scroll, with the entry we
    /// came out of in view.
    fn restore_scroll(&mut self) {
        // The row height decides what fits.
        self.view.thumbnails = self.thumbnails_on();
        if let Some(scroll) = self.return_scroll.take() {
            self.view.scroll = scroll;
        }
        let Some(name) = &self.came_from else { return };
        if let Some(i) = self.items.iter().position(|it| it.trail_name() == name) {
            let (i, visible) = (i as f32, self.view.visible_rows());
            if i < self.view.scroll || i + 1.0 > self.view.scroll + visible {
                self.view.scroll = i - (visible / 2.0).floor();
            }
        }
    }

    /// Stops a folder listing we no longer wait for.
    fn cancel_listing(&mut self) {
        if let Some((listing, _)) = self.listing.take() {
            listing.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// What is going on in the background, if anything.
    pub fn progress(&self) -> Option<Progress> {
        if self.pending.is_some() {
            return Some(Progress::Busy);
        }
        let (done, total) = self.probes;
        (done < total).then(|| Progress::Fraction(done as f32 / total as f32))
    }

    fn show_servers(&mut self) {
        self.leave(&Location::Servers);
        self.location = Location::Servers;
        self.folder_loaded = false;
        self.pending = None;
        self.cancel_listing();
        self.probes = (0, 0);
        self.reset_view();
        let servers = config::servers().unwrap_or_else(|e| {
            eprintln!("Can't read saved servers: {e:#}");
            Vec::new()
        });
        self.items = std::iter::once(local::server())
            .chain(servers)
            .map(Item::Server)
            .collect();
        self.items.push(Item::AddServer);
        self.items.push(Item::Settings);
        self.restore_scroll();
        self.rebuild_rows();
    }

    /// The Settings page: one row per setting, its value on the right.
    fn show_settings(&mut self) {
        self.leave(&Location::Settings);
        self.location = Location::Settings;
        self.folder_loaded = false;
        self.pending = None;
        self.cancel_listing();
        self.probes = (0, 0);
        self.reset_view();
        self.items = SETTINGS.into_iter().collect();
        self.restore_scroll();
        self.rebuild_rows();
        self.dirty = true;
    }

    /// How much a curved video screen bends (see [`config::curvature`]).
    pub fn curvature(&self) -> f32 {
        self.curvature
    }

    /// The browser panel's width, as a multiple of its usual width.
    pub fn browser_width(&self) -> f32 {
        self.browser_width
    }

    /// Whether jumps decode on to their exact target (see [`config::seek_exact`]).
    pub fn seek_exact(&self) -> bool {
        self.seek_exact
    }

    /// A dialog of presets for a setting, the saved one marked.
    fn preset_dialog(
        &mut self,
        title: &str,
        body: &str,
        buttons: Vec<String>,
        choice: Option<usize>,
        purpose: Purpose,
    ) {
        self.view.dialog = Some(Dialog {
            title: title.into(),
            body: vec![body.into()],
            buttons,
            danger: false,
            choice,
        });
        self.purpose = Some(purpose);
        self.dirty = true;
    }

    /// Jump lengths, volume step and continuing, as last saved.
    pub fn preferences(&self) -> config::Preferences {
        self.prefs
    }

    /// Changes and saves a preference; on a failure to save it still applies
    /// until Just Video quits.
    fn change_preference(&mut self, change: impl FnOnce(&mut config::Preferences)) {
        match config::update_preferences(change) {
            Ok(prefs) => self.prefs = prefs,
            Err(e) => eprintln!("Can't save the setting: {e:#}"),
        }
        self.rebuild_rows();
        self.dirty = true;
    }

    /// Thumbnails are shown: switched on, in a folder with videos.
    fn thumbnails_on(&self) -> bool {
        self.prefs.thumbnails && self.in_video_folder()
    }

    fn in_video_folder(&self) -> bool {
        matches!(self.location, Location::Folder { .. })
            && self.items.iter().any(|i| matches!(i, Item::Video { .. }))
    }

    /// Starts a new thumbnail generation: work for the old one stops, and
    /// its pictures go.
    fn reset_thumbnails(&mut self) {
        self.thumb_generation += 1;
        self.library.set_thumbnail_generation(self.thumb_generation);
        self.thumbs.clear();
        self.thumbs_sent.clear();
        self.thumb_focus = 0;
    }

    /// The video at the top of the list, as an index among the folder's videos.
    fn first_visible_video(&self) -> usize {
        let first = self.view.scroll.floor() as usize;
        self.items
            .iter()
            .take(first)
            .filter(|i| matches!(i, Item::Video { .. }))
            .count()
    }

    /// Hands the worker the videos whose layout is now known (as probed, with
    /// the user's override), nearest the top first by index.
    fn request_thumbnails(&mut self) {
        let Location::Folder {
            server,
            share,
            path,
        } = &self.location
        else {
            return;
        };
        if !self.prefs.thumbnails {
            return;
        }
        let mut videos = Vec::new();
        let mut index = 0;
        for item in &self.items {
            let Item::Video {
                name,
                size,
                modified,
                layout,
                broken: None,
                ..
            } = item
            else {
                if matches!(item, Item::Video { .. }) {
                    index += 1;
                }
                continue;
            };
            if let Some(layout) = layout
                && !self.thumbs_sent.contains(name)
            {
                videos.push(ThumbVideo {
                    index,
                    name: name.clone(),
                    size: *size,
                    modified: *modified,
                    layout: *layout,
                });
            }
            index += 1;
        }
        if videos.is_empty() {
            return;
        }
        self.thumbs_sent
            .extend(videos.iter().map(|v| v.name.clone()));
        self.thumb_focus = self.first_visible_video();
        self.library.send(Request::ThumbnailFolder {
            generation: self.thumb_generation,
            server: server.clone(),
            share: share.clone(),
            folder: path.clone(),
            videos,
            first_visible: self.thumb_focus,
        });
    }

    /// The list scrolled: the worker reorders around the new top video.
    fn focus_thumbnails(&mut self) {
        if !self.thumbnails_on() || self.thumbs_sent.is_empty() {
            return;
        }
        let first_visible = self.first_visible_video();
        if first_visible != self.thumb_focus {
            self.thumb_focus = first_visible;
            self.library.send(Request::ThumbnailFocus {
                generation: self.thumb_generation,
                first_visible,
            });
        }
    }

    /// Thumbnails on or off, from the header's button or the Settings row:
    /// one saved preference, so both stay in step.
    fn toggle_thumbnails(&mut self) {
        // Off: pending work stops and the pictures go. On: start afresh.
        self.reset_thumbnails();
        // Rebuilds the rows at the new height; the entry the scroll was at
        // stays on top.
        self.change_preference(|p| p.thumbnails = !p.thumbnails);
        self.request_thumbnails();
    }

    /// What a settings dialog previews, given what the pointer is on: the
    /// choice pointed at, otherwise the saved one.
    pub fn preview(&self, hover: Hit) -> Option<Preview> {
        self.view.dialog.as_ref()?;
        let pointed = match hover {
            Hit::DialogButton(k) => Some(k),
            _ => None,
        };
        match &self.purpose {
            Some(Purpose::Curvature) => Some(Preview::Curvature(
                pointed
                    .and_then(|k| config::CURVATURE_PRESETS.get(k))
                    .map_or(self.curvature, |p| p.1),
            )),
            Some(Purpose::BrowserWidth) => Some(Preview::BrowserWidth(
                pointed
                    .and_then(|k| config::BROWSER_WIDTH_PRESETS.get(k).copied())
                    .unwrap_or(self.browser_width),
            )),
            _ => None,
        }
    }

    fn navigate(&mut self, location: Location) {
        self.leave(&location);
        self.location = location.clone();
        self.folder_loaded = false;
        self.items.clear();
        self.generation += 1;
        self.library.set_probe_generation(self.generation);
        self.reset_thumbnails();
        self.cancel_listing();
        self.probes = (0, 0);
        self.reset_view();
        let id = self.id();
        self.pending = Some(id);
        match location {
            Location::Servers => self.show_servers(),
            Location::Settings => self.show_settings(),
            Location::Shares { server } => {
                self.set_status("Connecting…");
                self.library.send(Request::Shares { id, server });
            }
            Location::Folder {
                server,
                share,
                path,
            } => {
                self.set_status("Loading…");
                let watch = Listing::default();
                self.listing = Some((watch.clone(), 0));
                self.library.send(Request::List {
                    id,
                    server,
                    share,
                    path,
                    watch,
                });
            }
        }
        self.rebuild_rows();
    }

    fn refresh(&mut self) {
        let here = self.location.clone();
        let scroll = self.view.scroll;
        self.navigate(here);
        self.view.scroll = scroll;
    }

    fn rebuild_rows(&mut self) {
        let editing = self.editing();
        let selecting = self.selecting.as_ref();
        let (curvature, browser_width, seek_exact, prefs) = (
            self.curvature,
            self.browser_width,
            self.seek_exact,
            self.prefs,
        );
        self.view.rows = self
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let mut row = match item {
                    // The headset itself: browsed like a server, but it can't
                    // be edited or removed.
                    Item::Server(s) if local::is_local(s) => Row {
                        detail: "Videos, Downloads, home folder, SD card and USB drives".into(),
                        ..Row::new(Icon::Server, &s.name)
                    },
                    Item::Server(s) => Row {
                        detail: s.url.clone(),
                        actions: if selecting.is_some_and(|sel| sel.contains(&i)) {
                            vec![Action::Settings, Action::Remove]
                        } else {
                            Vec::new()
                        },
                        ..Row::new(Icon::Server, &s.name)
                    },
                    Item::AddServer => Row {
                        detail: "A Windows PC, NAS or Samba server on your network".into(),
                        ..Row::new(Icon::Add, "Add server")
                    },
                    Item::Settings => Row {
                        detail: "Jumps, buffering, screen and browser".into(),
                        ..Row::new(Icon::Settings, "Settings")
                    },
                    Item::BufferSize => Row {
                        detail: "Fetched while paused, to play on without the network".into(),
                        right: crate::ui::controls::format_size(config::reserve_bytes()),
                        ..Row::new(Icon::Buffer, "Buffer ahead")
                    },
                    Item::Curvature => Row {
                        detail: "How much a curved video screen bends around you".into(),
                        right: config::CURVATURE_PRESETS[config::curvature_preset(curvature)]
                            .0
                            .into(),
                        ..Row::new(Icon::Curve, "Screen curvature")
                    },
                    Item::BrowserWidth => Row {
                        detail: "How wide this window is".into(),
                        right: config::percent(browser_width),
                        ..Row::new(Icon::Width, "Browser width")
                    },
                    Item::SeekLanding => Row {
                        detail: "The keyframe before the target at once, or exactly on it".into(),
                        right: config::SEEK_LANDING_PRESETS[seek_exact as usize].0.into(),
                        ..Row::new(Icon::Jump, "Jumps land on")
                    },
                    Item::ShortJump => Row {
                        detail: "D-pad left/right or a sideways stick flick while playing".into(),
                        right: config::format_jump(prefs.short_jump),
                        ..Row::new(Icon::Jump, "Jump")
                    },
                    Item::LongJump => Row {
                        detail: "The same with the grip held".into(),
                        right: config::format_jump(prefs.long_jump),
                        ..Row::new(Icon::Jump, "Long jump")
                    },
                    Item::VolumeStep => Row {
                        detail:
                            "D-pad up/down while playing set the headset volume; hold to repeat"
                                .into(),
                        right: format!("{} %", prefs.volume_step),
                        ..Row::new(Icon::Volume, "Volume step")
                    },
                    Item::Resume => Row {
                        detail: "Start videos from where they were stopped".into(),
                        right: if prefs.resume { "On" } else { "Off" }.into(),
                        ..Row::new(Icon::Continue, "Continue where I left off")
                    },
                    Item::AbLoop => Row {
                        detail: "A button on the player bar to repeat a section".into(),
                        right: if prefs.ab_loop { "On" } else { "Off" }.into(),
                        ..Row::new(Icon::Loop, "A-B loop")
                    },
                    Item::DpadMenus => Row {
                        detail: "Move through menus with the D-pad; A chooses, B goes back".into(),
                        right: if prefs.dpad_menus { "On" } else { "Off" }.into(),
                        ..Row::new(Icon::Dpad, "D-pad in menus")
                    },
                    Item::VideoConnections => Row {
                        detail: "Read a playing video over two SMB connections (experimental)"
                            .into(),
                        right: if config::video_connections() > 1 {
                            "Two"
                        } else {
                            "One"
                        }
                        .into(),
                        ..Row::new(Icon::Server, "Video connections")
                    },
                    Item::Thumbnails => Row {
                        detail: "Pictures of videos in folder lists; slower browsing on Wi-Fi"
                            .into(),
                        right: if prefs.thumbnails { "On" } else { "Off" }.into(),
                        ..Row::new(Icon::Thumbnails(prefs.thumbnails), "Thumbnails")
                    },
                    Item::Share(name) => Row::new(Icon::Share, name),
                    Item::Dir(name) => Row::new(Icon::Folder, name),
                    Item::Video {
                        name,
                        size,
                        assessment,
                        layout,
                        broken,
                        resume,
                        ..
                    } => Row {
                        icon: match (assessment, broken) {
                            (_, Some(_)) => Icon::Broken,
                            (a, None) => {
                                Icon::video(a.as_ref().map(|a| a.verdict), layout.as_ref())
                            }
                        },
                        detail: match (assessment, broken) {
                            (_, Some(_)) => "Can't read this file".into(),
                            (Some(a), None) => {
                                let detail = match layout.as_ref().and_then(Layout::short_label) {
                                    Some(format) => format!("{format}  ·  {}", a.summary),
                                    None => a.summary.clone(),
                                };
                                with_progress(&detail, *resume)
                            }
                            (None, None) => with_progress("Checking…", *resume),
                        },
                        right: format_size(*size),
                        watch: match (broken, resume) {
                            (Some(_), _) => None,
                            (None, None) => Some(Watch::Unwatched),
                            (None, Some(Place::Resume(..))) => Some(Watch::Partial),
                            (None, Some(Place::Watched)) => Some(Watch::Watched),
                        },
                        progress: match resume {
                            Some(Place::Resume(at, Some(length))) if *length > 0.0 => {
                                Some((at / length) as f32)
                            }
                            _ => None,
                        },
                        thumbnail: self.thumbs.get(name).cloned(),
                        ..Row::new(Icon::Video(None), name)
                    },
                    Item::File { name, size } => Row {
                        right: format_size(*size),
                        dimmed: true,
                        ..Row::new(Icon::File, name)
                    },
                };
                row.outlined = self.came_from.as_deref() == Some(item.trail_name());
                if item.is_entry()
                    && let Some(selected) = selecting
                {
                    row.checked = Some(selected.contains(&i));
                }
                row
            })
            .collect();
        self.view.thumbnails = self.thumbnails_on();
        let tools: Vec<(Tool, ToolAction)> = if self.move_request.is_some() {
            Vec::new()
        } else if self.listing.is_some() && self.pending.is_some() {
            vec![(Tool::text("Cancel", false), ToolAction::CancelLoad)]
        } else if self.moving.is_some() {
            let mut tools = vec![(Tool::text("Cancel", false), ToolAction::CancelMove)];
            if editing && self.folder_loaded {
                tools.push((Tool::text("Move here", false), ToolAction::MoveHere));
            }
            tools
        } else {
            match &self.selecting {
                Some(selected) => {
                    let mut tools = vec![(Tool::text("Cancel", false), ToolAction::CancelSelect)];
                    if !selected.is_empty() && editing {
                        // One item needs no count.
                        let count = |verb: &str| match selected.len() {
                            1 => verb.to_string(),
                            n => format!("{verb} {n}"),
                        };
                        let delete = Tool::text(count("Delete"), true);
                        tools.push((delete, ToolAction::DeleteSelected));
                        tools.push((Tool::text(count("Move"), false), ToolAction::MoveSelected));
                        if selected.len() == 1 {
                            tools.push((Tool::text("Rename", false), ToolAction::RenameSelected));
                        }
                    }
                    // Forgetting where a video was left changes nothing on
                    // the server: offered on read-only servers too.
                    if !selected.is_empty() && self.selected_resume().is_some() {
                        tools.push((Tool::text("Start over", false), ToolAction::ForgetResume));
                    }
                    tools
                }
                None if matches!(self.location, Location::Servers) => {
                    vec![(Tool::icon(Icon::Exit), ToolAction::Exit)]
                }
                // Only where there are videos: the same setting as on the
                // Settings page, to hand when the pictures are wanted.
                None if self.in_video_folder() => {
                    let mut tools = vec![(
                        Tool::icon(Icon::Thumbnails(prefs.thumbnails)),
                        ToolAction::ToggleThumbnails,
                    )];
                    // Ordering matters with two or more videos.
                    if self
                        .items
                        .iter()
                        .filter(|i| matches!(i, Item::Video { .. }))
                        .count()
                        >= 2
                    {
                        let label = format!("Sort: {}", self.sort.label());
                        tools.push((Tool::text(label, false), ToolAction::CycleSort));
                        let label =
                            format!("Autoplay: {}", if self.autoplay { "On" } else { "Off" });
                        tools.push((Tool::text(label, false), ToolAction::ToggleAutoplay));
                    }
                    tools
                }
                None => Vec::new(),
            }
        };
        (self.view.tools, self.tool_actions) = tools.into_iter().unzip();
        if self.selecting.is_none() && self.view.notice.as_deref() == Some(READ_ONLY_NOTICE) {
            self.view.notice = None;
        }
        // The selection's tools take the header; a count stands in for the crumbs.
        self.view.heading = match &self.selecting {
            Some(selected) if editing => Some(match selected.len() {
                0 => "Select items".to_string(),
                n => format!("{n} selected"),
            }),
            _ => None,
        };
        self.view.clamp_scroll();
        self.dirty = true;
    }

    fn dialog(&mut self, title: impl Into<String>, body: Vec<String>) {
        self.view.dialog = Some(Dialog {
            title: title.into(),
            body,
            buttons: vec!["OK".into()],
            danger: false,
            choice: None,
        });
        self.purpose = Some(Purpose::Info);
        self.dirty = true;
    }

    /// Handles worker results; returns a video ready to play.
    pub fn poll(&mut self) -> Option<Box<Opened>> {
        // A long listing shows how far it has come.
        if let Some((listing, shown)) = &mut self.listing
            && self.pending.is_some()
        {
            let count = listing.count.load(Ordering::Relaxed);
            if count != *shown {
                *shown = count;
                self.view.status = Some(format!("Loading… {count} entries"));
                self.dirty = true;
            }
        }
        // Thumbnails arriving together (a cached folder's come all at once)
        // make one rebuild.
        let mut thumbnails = false;
        while let Some(response) = self.library.try_recv() {
            match response {
                Response::Shares { id, result } if Some(id) == self.pending => {
                    self.pending = None;
                    match result {
                        Ok(shares) if shares.is_empty() => {
                            self.set_status("No shared folders you can open.")
                        }
                        Ok(shares) => {
                            self.view.status = None;
                            self.items = shares.into_iter().map(Item::Share).collect();
                            self.restore_scroll();
                            self.rebuild_rows();
                        }
                        Err(e) => self.set_status(e),
                    }
                }
                Response::List { id, result } if Some(id) == self.pending => {
                    self.pending = None;
                    self.listing = None;
                    let Location::Folder {
                        server,
                        share,
                        path,
                    } = self.location.clone()
                    else {
                        continue;
                    };
                    match result {
                        Ok(entries) => {
                            self.folder_loaded = true;
                            self.items = entries
                                .into_iter()
                                .filter(|e| !e.name.starts_with('.'))
                                .map(|e| {
                                    if e.is_dir {
                                        Item::Dir(e.name)
                                    } else if !is_video(&e.name) {
                                        Item::File {
                                            name: e.name,
                                            size: e.size,
                                        }
                                    } else {
                                        Item::Video {
                                            name: e.name,
                                            size: e.size,
                                            modified: e.modified,
                                            assessment: None,
                                            layout: None,
                                            broken: None,
                                            resume: None,
                                        }
                                    }
                                })
                                .collect();
                            let mut sent = 0;
                            for item in &self.items {
                                if let Item::Video { name, .. } = item {
                                    let mut file = path.clone();
                                    file.push(name.clone());
                                    self.library.send(Request::Probe {
                                        generation: self.generation,
                                        server: server.clone(),
                                        share: share.clone(),
                                        path: file,
                                    });
                                    sent += 1;
                                }
                            }
                            self.probes = (0, sent);
                            self.refresh_resume();
                            self.view.status = if self.items.is_empty() {
                                Some("This folder is empty.".into())
                            } else {
                                None
                            };
                            self.restore_scroll();
                            if self.sort == SortMode::Name {
                                self.rebuild_rows();
                            } else {
                                self.sort_items();
                            }
                        }
                        Err(e) => self.set_status(format!("Can't open this folder: {e}")),
                    }
                }
                Response::Probe {
                    generation,
                    name,
                    result,
                } if generation == self.generation => {
                    self.probes.0 = (self.probes.0 + 1).min(self.probes.1);
                    for item in &mut self.items {
                        if let Item::Video {
                            name: n,
                            assessment,
                            layout,
                            broken,
                            ..
                        } = item
                            && *n == name
                        {
                            match &result {
                                Ok(p) => {
                                    *assessment = Some(p.assessment.clone());
                                    *layout = Some(p.layout);
                                }
                                Err(e) => *broken = Some(e.clone()),
                            }
                        }
                    }
                    self.rebuild_rows();
                    // The layout decides the crop, so a video is handed to
                    // the thumbnail worker once its mark has landed.
                    self.request_thumbnails();
                }
                Response::Thumbnail {
                    generation,
                    name,
                    image,
                } if generation == self.thumb_generation && self.prefs.thumbnails => {
                    self.thumbs.insert(name, image);
                    thumbnails = true;
                }
                Response::Opened { id, result } if Some(id) == self.pending => {
                    self.pending = None;
                    self.view.notice = None;
                    self.dirty = true;
                    match result {
                        Ok(opened) if opened.assessment.verdict == Verdict::Unplayable => {
                            let a = opened.assessment.clone();
                            crate::xr::app::drop_in_background(opened);
                            self.dialog(
                                a.title,
                                [a.detail, a.hint].into_iter().flatten().collect(),
                            );
                        }
                        Ok(opened) => {
                            self.playing_source =
                                Some((opened.server.clone(), opened.share.clone()));
                            self.subtitle_request = None;
                            self.subtitle_result = None;
                            return Some(opened);
                        }
                        Err(e) => self.dialog("Can't open this video", vec![e]),
                    }
                }
                Response::Changed { id, result } if self.pending_changes.remove(&id) => {
                    if let Err(e) = result {
                        self.change_errors.push(e);
                    }
                    if self.pending_changes.is_empty() {
                        self.changes_done();
                    }
                }
                Response::Moved { id, result } if self.move_request == Some(id) => {
                    self.move_request = None;
                    if let Some(selection) = &mut self.moving {
                        selection.paths = result.failed;
                        if selection.paths.is_empty() {
                            self.moving = None;
                        }
                    }
                    self.view.notice = None;
                    self.refresh();
                    if !result.errors.is_empty() {
                        self.dialog("Some items didn't move", result.errors);
                    }
                }
                Response::ServerAdded { id, result } if self.pending_changes.remove(&id) => {
                    match result {
                        Ok(saved) => {
                            // An edit that changed the address replaces the old entry.
                            if let Some(Purpose::EditServer(old)) = &self.purpose
                                && *old != saved.url
                            {
                                let _ = config::remove_server(old);
                            }
                            self.show_servers();
                        }
                        Err(e) => {
                            if let Some(f) = &mut self.view.form {
                                f.busy = None;
                                f.error = Some(e);
                                self.dirty = true;
                            }
                        }
                    }
                }
                Response::Subtitle { id, result }
                    if self.subtitle_request.is_some_and(|(r, _)| r == id) =>
                {
                    let (_, track) = self.subtitle_request.take().expect("request");
                    self.subtitle_result = Some((track, result));
                }
                // A video that finished opening after we moved on: its reader and
                // decoder may take a while to close, so never on the frame loop.
                Response::Opened {
                    result: Ok(opened), ..
                } => crate::xr::app::drop_in_background(opened),
                _ => {} // stale response for a place we already left
            }
        }
        if thumbnails {
            self.rebuild_rows();
        }
        None
    }

    /// Handles worker results while a video plays (subtitle files arriving).
    pub fn poll_while_playing(&mut self) {
        // Nothing is being opened meanwhile, so no video comes back here.
        if let Some(opened) = self.poll() {
            crate::xr::app::drop_in_background(opened);
        }
    }

    /// Fetches the file of the playing video's external subtitle `track`;
    /// the answer comes back through [`Navigator::take_subtitle`].
    pub fn fetch_subtitle(&mut self, track: usize, path: Path) {
        let Some((server, share)) = self.playing_source.clone() else {
            self.subtitle_result = Some((track, Err("No connection.".into())));
            return;
        };
        let id = self.id();
        self.subtitle_request = Some((id, track));
        self.library.send(Request::Subtitle {
            id,
            server,
            share,
            path,
        });
    }

    pub fn take_subtitle(&mut self) -> Option<(usize, Result<Vec<crate::subtitles::Cue>, String>)> {
        self.subtitle_result.take()
    }

    fn changes_done(&mut self) {
        self.view.notice = None;
        let errors = std::mem::take(&mut self.change_errors);
        if let (Some(f), Some(e)) = (&mut self.view.form, errors.first()) {
            // A rename that failed: say why, keep the form open.
            f.busy = None;
            f.error = Some(e.clone());
            self.dirty = true;
            return;
        }
        self.refresh();
        if !errors.is_empty() {
            let mut unique = errors.clone();
            unique.dedup();
            let title = if errors.len() == 1 {
                "That didn't work".to_string()
            } else {
                format!("{} items failed", errors.len())
            };
            self.dialog(title, unique);
        }
    }

    /// Reacts to a click (trigger or A) on the panel.
    pub fn click(&mut self, hit: Hit) {
        if self.move_request.is_some() {
            return;
        }
        match hit {
            Hit::Form(h) => self.click_form(h),
            Hit::DialogButton(i) => self.click_dialog(i),
            Hit::Crumb(i) => self.go_to_crumb(i),
            Hit::Tool(k) => self.click_tool(k),
            Hit::RowAction(i, Action::Settings) => self.start_edit_server(i),
            Hit::RowAction(i, Action::Remove) => self.confirm_remove_server(i),
            Hit::Row(i) => self.select(i),
            Hit::ScrollBar | Hit::Nothing => {}
        }
    }

    fn click_tool(&mut self, k: usize) {
        match self.tool_actions.get(k) {
            Some(ToolAction::CycleSort) => {
                self.sort = self.sort.next();
                if let Err(e) = config::save_sort_mode(self.sort) {
                    eprintln!("Can't save the sort order: {e:#}");
                }
                self.sort_items();
            }
            Some(ToolAction::ToggleAutoplay) => {
                self.autoplay = !self.autoplay;
                if let Err(e) = config::save_autoplay_next(self.autoplay) {
                    eprintln!("Can't save the autoplay setting: {e:#}");
                }
                self.rebuild_rows();
            }
            Some(ToolAction::CancelSelect) => self.selecting = None,
            Some(ToolAction::ForgetResume) => self.forget_resume(),
            Some(ToolAction::DeleteSelected) => {
                let mut indices: Vec<usize> = self.selecting.iter().flatten().copied().collect();
                indices.sort_unstable();
                self.confirm_delete(indices);
            }
            Some(ToolAction::MoveSelected) => self.start_move(),
            Some(ToolAction::RenameSelected) => {
                if let Some(&i) = self.selecting.iter().flatten().next() {
                    self.start_rename(i);
                }
            }
            Some(ToolAction::CancelMove) => self.moving = None,
            Some(ToolAction::Exit) => self.exit_requested = true,
            Some(ToolAction::ToggleThumbnails) => self.toggle_thumbnails(),
            Some(ToolAction::MoveHere) => self.move_here(),
            Some(ToolAction::CancelLoad) => {
                self.back();
            }
            None => {}
        }
        self.rebuild_rows();
    }

    fn start_move(&mut self) {
        if !self.editing() || !self.pending_changes.is_empty() {
            return;
        }
        let Location::Folder {
            server,
            share,
            path,
        } = &self.location
        else {
            return;
        };
        let mut indices: Vec<_> = self.selecting.iter().flatten().copied().collect();
        indices.sort_unstable();
        let paths: Vec<_> = indices
            .into_iter()
            .filter_map(|i| {
                let item = self.items.get(i).filter(|item| item.is_entry())?;
                let mut source = path.clone();
                source.push(item.name().to_string());
                Some(source)
            })
            .collect();
        if paths.is_empty() {
            return;
        }
        self.moving = Some(MoveSelection {
            server: server.clone(),
            share: share.clone(),
            paths,
        });
        self.selecting = None;
    }

    /// The one selected video that was left part way, if that is the selection.
    fn selected_resume(&self) -> Option<usize> {
        let selected = self.selecting.as_ref()?;
        if selected.len() != 1 {
            return None;
        }
        let &index = selected.iter().next()?;
        match self.items.get(index) {
            Some(Item::Video {
                resume: Some(_), ..
            }) => Some(index),
            _ => None,
        }
    }

    /// Forgets where the selected video was left, so it starts from the top.
    fn forget_resume(&mut self) {
        let Some(index) = self.selected_resume() else {
            return;
        };
        let Location::Folder {
            server,
            share,
            mut path,
        } = self.location.clone()
        else {
            return;
        };
        if let Some(Item::Video { name, resume, .. }) = self.items.get_mut(index) {
            path.push(name.clone());
            let key = crate::library::file_key(&server, &share, &path);
            if let Err(e) = config::save_resume_position(&key, None) {
                eprintln!("Can't forget where the video was left: {e:#}");
            }
            *resume = None;
        }
        self.selecting = None;
    }

    /// Orders the open folder by the chosen sort (folders first, by name).
    /// Row positions change, so the playing index (previous/next) is dropped.
    fn sort_items(&mut self) {
        let mode = self.sort;
        let rank = |item: &Item| -> (u8, std::cmp::Reverse<u64>, u8, String) {
            let name = item.name().to_lowercase();
            match (item, mode) {
                (Item::Dir(_), _) => (0, std::cmp::Reverse(0), 0, name),
                (_, SortMode::Name) => (1, std::cmp::Reverse(0), 0, name),
                (Item::Video { size, .. } | Item::File { size, .. }, SortMode::Size) => {
                    (1, std::cmp::Reverse(*size), 0, name)
                }
                (Item::Video { resume, .. }, SortMode::Unwatched) => {
                    let group = match resume {
                        Some(Place::Resume(..)) => 0,
                        None => 1,
                        Some(Place::Watched) => 2,
                    };
                    (1, std::cmp::Reverse(0), group, name)
                }
                (_, _) => (1, std::cmp::Reverse(0), 3, name),
            }
        };
        self.items.sort_by_cached_key(rank);
        self.playing = None;
        self.rebuild_rows();
    }

    /// Marks the videos in this folder that were left part way.
    fn refresh_resume(&mut self) {
        let Location::Folder {
            server,
            share,
            path,
        } = self.location.clone()
        else {
            return;
        };
        let all = config::places();
        for item in &mut self.items {
            if let Item::Video { name, resume, .. } = item {
                let mut file = path.clone();
                file.push(name.clone());
                *resume = all
                    .get(&crate::library::file_key(&server, &share, &file))
                    .copied();
            }
        }
    }

    /// What is shown for the video with this file key in the open folder.
    pub fn place_of_key(&self, key: &str) -> Option<Place> {
        let Location::Folder {
            server,
            share,
            path,
        } = &self.location
        else {
            return None;
        };
        self.items.iter().find_map(|item| match item {
            Item::Video { name, resume, .. } => {
                let mut file = path.clone();
                file.push(name.clone());
                (crate::library::file_key(server, share, &file) == key).then_some(*resume)?
            }
            _ => None,
        })
    }

    /// After playing `key`: how far it got (None: forgotten),
    /// for the row's marker. The file itself is written in the background.
    pub fn note_resume(&mut self, key: &str, point: Option<Place>) {
        let Location::Folder {
            server,
            share,
            path,
        } = self.location.clone()
        else {
            return;
        };
        for item in &mut self.items {
            if let Item::Video { name, resume, .. } = item {
                let mut file = path.clone();
                file.push(name.clone());
                if crate::library::file_key(&server, &share, &file) == key {
                    *resume = point;
                }
            }
        }
        self.rebuild_rows();
    }

    fn move_here(&mut self) {
        if !self.editing() || !self.folder_loaded || !self.pending_changes.is_empty() {
            return;
        }
        let (
            Some(selection),
            Location::Folder {
                server,
                share,
                path,
            },
        ) = (&self.moving, &self.location)
        else {
            return;
        };
        if let Err(e) = selection.targets(server, share, path) {
            self.dialog("Can't move here", vec![e]);
            return;
        }
        let selection = selection.clone();
        let destination = path.clone();
        self.view.notice = Some(format!("Moving {} items…", selection.paths.len()));
        let id = self.id();
        self.move_request = Some(id);
        self.library.send(Request::Move {
            id,
            selection,
            destination,
        });
    }

    /// Asks before deleting these entries.
    fn confirm_delete(&mut self, indices: Vec<usize>) {
        let names: Vec<String> = indices
            .iter()
            .filter_map(|&i| self.items.get(i))
            .map(|it| it.name().to_string())
            .collect();
        let n = names.len();
        if n == 0 {
            return;
        }
        let mut body = vec![format!(
            "{} deleted from the server for good.",
            if n == 1 { "It will be" } else { "They will be" }
        )];
        if n > 1 {
            let mut list = names
                .iter()
                .take(6)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            if n > 6 {
                list += &format!(" and {} more", n - 6);
            }
            body.push(list);
        }
        let title = if n == 1 {
            format!("Delete {}?", names[0])
        } else {
            format!("Delete {n} items?")
        };
        self.view.dialog = Some(Dialog {
            title,
            body,
            buttons: vec!["Cancel".into(), "Delete".into()],
            danger: true,
            choice: None,
        });
        self.purpose = Some(Purpose::Delete(indices));
        self.dirty = true;
    }

    /// A long press on a row: edit mode with that entry selected (a server
    /// shows its Settings and Remove buttons).
    pub fn long_press(&mut self, index: usize) {
        if self.moving.is_some() || !self.pending_changes.is_empty() {
            return;
        }
        match self.items.get(index) {
            Some(Item::Server(s)) if local::is_local(s) => return,
            Some(Item::Server(_)) => {
                self.selecting = Some(HashSet::from([index]));
            }
            Some(item) if item.is_entry() => {
                self.selecting
                    .get_or_insert_with(HashSet::new)
                    .insert(index);
                if !self.editing() {
                    self.view.notice = Some(READ_ONLY_NOTICE.into());
                }
            }
            _ => return,
        }
        self.rebuild_rows();
    }

    /// The playable video `delta` places from the playing one, if any.
    fn adjacent(&self, delta: isize) -> Option<usize> {
        let mut i = self.playing? as isize;
        loop {
            i += delta;
            let item = self.items.get(usize::try_from(i).ok()?)?;
            if let Item::Video {
                assessment,
                broken: None,
                ..
            } = item
                && assessment
                    .as_ref()
                    .is_none_or(|a| a.verdict != Verdict::Unplayable)
            {
                return Some(i as usize);
            }
        }
    }

    /// Back from playing: outlines the video that played, and keeps it in view.
    pub fn playback_ended(&mut self) {
        self.came_from = self
            .playing
            .and_then(|i| self.items.get(i))
            .map(|item| item.trail_name().to_string());
        self.reload_played_layout();
        self.restore_scroll();
        self.rebuild_rows();
    }

    /// The video that played may have had its format changed: mark it as saved.
    fn reload_played_layout(&mut self) {
        if let (
            Some(i),
            Location::Folder {
                server,
                share,
                path,
            },
        ) = (self.playing, &self.location)
            && let Some(Item::Video {
                name,
                layout: Some(layout),
                ..
            }) = self.items.get_mut(i)
        {
            let file: Path = path.iter().chain([&*name]).cloned().collect();
            if let Ok(Some(saved)) = config::layout_override(&file_key(server, share, &file)) {
                saved.apply(layout);
            }
        }
    }

    /// A video ended on its own: should the next one start?
    pub fn autoplay_next(&self) -> bool {
        self.autoplay && self.has_adjacent(1)
    }

    pub fn has_adjacent(&self, delta: isize) -> bool {
        self.adjacent(delta).is_some()
    }

    /// Opens the previous (-1) or next (+1) video in the folder; the result
    /// arrives from [`Navigator::poll`] like any other.
    pub fn open_adjacent(&mut self, delta: isize) -> bool {
        let Some(index) = self.adjacent(delta) else {
            return false;
        };
        self.selecting = None;
        self.view.dialog = None;
        self.view.form = None;
        self.reload_played_layout();
        self.select(index);
        true
    }

    fn go_to_crumb(&mut self, i: usize) {
        match (i, self.location.clone()) {
            (0, _) => self.show_servers(),
            (1, Location::Folder { server, .. }) => self.navigate(Location::Shares { server }),
            (
                n,
                Location::Folder {
                    server,
                    share,
                    path,
                },
            ) if n >= 2 => self.navigate(Location::Folder {
                server,
                share,
                path: path[..n - 2].to_vec(),
            }),
            _ => {}
        }
    }

    fn select(&mut self, index: usize) {
        if let Some(selected) = &mut self.selecting {
            if self.items.get(index).is_some_and(Item::is_entry) {
                if !selected.remove(&index) {
                    selected.insert(index);
                }
                self.rebuild_rows();
                return;
            }
            // A server's buttons close when something else is opened.
            self.selecting = None;
            self.rebuild_rows();
        }
        if self.pending.is_some() && !matches!(self.location, Location::Folder { .. }) {
            return;
        }
        let Some(item) = self.items.get(index) else {
            return;
        };
        match (item, self.location.clone()) {
            (Item::Server(s), _) => self.navigate(Location::Shares { server: s.clone() }),
            (Item::AddServer, _) => self.start_server_form(None),
            (Item::BufferSize, _) => {
                self.view.dialog = Some(Dialog {
                    title: "Buffer ahead while paused".into(),
                    body: vec!["How much the player's buffer button fetches while paused.".into()],
                    buttons: config::RESERVE_PRESETS
                        .iter()
                        .map(|&b| crate::ui::controls::format_size(b))
                        .collect(),
                    danger: false,
                    choice: config::RESERVE_PRESETS
                        .iter()
                        .position(|&b| b == config::reserve_bytes()),
                });
                self.purpose = Some(Purpose::BufferSize);
                self.dirty = true;
            }
            (Item::Settings, _) => self.navigate(Location::Settings),
            (Item::Resume, _) => self.change_preference(|p| p.resume = !p.resume),
            (Item::AbLoop, _) => self.change_preference(|p| p.ab_loop = !p.ab_loop),
            (Item::DpadMenus, _) => self.change_preference(|p| p.dpad_menus = !p.dpad_menus),
            (Item::VideoConnections, _) => {
                let next = if config::video_connections() > 1 {
                    1
                } else {
                    2
                };
                if let Err(e) = config::save_video_connections(next) {
                    eprintln!("Can't save the setting: {e:#}");
                }
                self.rebuild_rows();
                self.dirty = true;
            }
            (Item::Thumbnails, _) => self.toggle_thumbnails(),
            (Item::ShortJump, _) => {
                self.preset_dialog(
                    "Jump",
                    "How far D-pad left/right (or a sideways stick flick) jump.",
                    config::SHORT_JUMP_PRESETS
                        .iter()
                        .map(|&s| config::format_jump(s))
                        .collect(),
                    config::SHORT_JUMP_PRESETS
                        .iter()
                        .position(|&s| s == self.prefs.short_jump),
                    Purpose::ShortJump,
                );
            }
            (Item::LongJump, _) => {
                self.preset_dialog(
                    "Long jump",
                    "How far a D-pad jump goes with the grip held.",
                    config::LONG_JUMP_PRESETS
                        .iter()
                        .map(|&s| config::format_jump(s))
                        .collect(),
                    config::LONG_JUMP_PRESETS
                        .iter()
                        .position(|&s| s == self.prefs.long_jump),
                    Purpose::LongJump,
                );
            }
            (Item::VolumeStep, _) => {
                self.preset_dialog(
                    "Volume step",
                    "How much D-pad up/down change the headset's volume.",
                    config::VOLUME_STEP_PRESETS
                        .iter()
                        .map(|&s| format!("{s} %"))
                        .collect(),
                    config::VOLUME_STEP_PRESETS
                        .iter()
                        .position(|&s| s == self.prefs.volume_step),
                    Purpose::VolumeStep,
                );
            }
            (Item::SeekLanding, _) => {
                self.seek_exact = !self.seek_exact;
                if let Err(e) = config::save_seek_exact(self.seek_exact) {
                    eprintln!("Can't save where jumps land: {e:#}");
                }
                self.rebuild_rows();
                self.dirty = true;
            }
            (Item::Curvature, _) => {
                self.view.dialog = Some(Dialog {
                    title: "Screen curvature".into(),
                    body: vec![
                        "How much a curved video screen bends around you. The outline shows the choice you point at."
                            .into(),
                    ],
                    buttons: config::CURVATURE_PRESETS
                        .iter()
                        .map(|(name, _)| name.to_string())
                        .collect(),
                    danger: false,
                    choice: Some(config::curvature_preset(self.curvature)),
                });
                self.purpose = Some(Purpose::Curvature);
                self.dirty = true;
            }
            (Item::BrowserWidth, _) => {
                self.view.dialog = Some(Dialog {
                    title: "Browser width".into(),
                    body: vec![
                        "How wide this window is. It follows the choice you point at.".into(),
                    ],
                    buttons: config::BROWSER_WIDTH_PRESETS
                        .iter()
                        .map(|&w| config::percent(w))
                        .collect(),
                    danger: false,
                    choice: config::BROWSER_WIDTH_PRESETS
                        .iter()
                        .position(|&w| w == self.browser_width),
                });
                self.purpose = Some(Purpose::BrowserWidth);
                self.dirty = true;
            }
            (Item::Share(name), Location::Shares { server }) => self.navigate(Location::Folder {
                server,
                share: name.clone(),
                path: Vec::new(),
            }),
            (
                Item::Dir(name),
                Location::Folder {
                    server,
                    share,
                    mut path,
                },
            ) => {
                path.push(name.clone());
                self.navigate(Location::Folder {
                    server,
                    share,
                    path,
                });
            }
            (
                Item::Video {
                    name,
                    assessment,
                    broken,
                    ..
                },
                Location::Folder {
                    server,
                    share,
                    mut path,
                },
            ) => {
                if let Some(e) = broken {
                    let body = vec!["Damaged, or not a video.".into(), e.clone()];
                    self.dialog("Can't read this file", body);
                } else if let Some(a) = assessment
                    .as_ref()
                    .filter(|a| a.verdict == Verdict::Unplayable)
                {
                    let body = [a.detail.clone(), a.hint.clone()]
                        .into_iter()
                        .flatten()
                        .collect();
                    self.dialog(a.title.clone(), body);
                } else {
                    path.push(name.clone());
                    self.playing = Some(index);
                    self.view.notice = Some(format!("Opening {name}…"));
                    self.dirty = true;
                    let id = self.id();
                    self.pending = Some(id);
                    self.library.send(Request::Open {
                        id,
                        server,
                        share,
                        path,
                    });
                }
            }
            _ => {}
        }
    }

    /// The add-server form, or with `existing` the edit form prefilled from it.
    fn start_server_form(&mut self, existing: Option<Server>) {
        let field = |label: &str, value: String, placeholder: &str, secret: bool| Field {
            label: label.into(),
            value,
            secret,
            placeholder: placeholder.into(),
        };
        let url = existing.as_ref().and_then(|s| s.url.parse::<SmbUrl>().ok());
        let address = url.as_ref().map_or(String::new(), |u| match u.port {
            Some(port) => format!("{}:{port}", u.host),
            None => u.host.clone(),
        });
        let user = url.as_ref().map_or(String::new(), |u| match &u.domain {
            Some(domain) => format!("{domain};{}", u.user),
            None => u.user.clone(),
        });
        let password_hint = if existing.is_some() {
            "leave empty to keep the saved one"
        } else {
            ""
        };
        let (title, submit) = match &existing {
            Some(s) => (format!("Edit {}", s.name), "Save"),
            None => ("Add server".to_string(), "Connect"),
        };
        let mut form = Form::new(
            title,
            vec![
                field("Address", address, "IP or name, e.g. 192.168.1.10", false),
                field("User", user, "user name (or DOMAIN;user)", false),
                field("Password", String::new(), password_hint, true),
                field(
                    "Name",
                    existing.as_ref().map_or(String::new(), |s| s.name.clone()),
                    "optional, shown in the list",
                    false,
                ),
            ],
            submit,
        );
        form.toggle = Some((
            "Allow changing files".into(),
            existing.as_ref().is_some_and(|s| s.writable),
        ));
        self.view.form = Some(form);
        self.purpose = Some(match existing {
            Some(s) => Purpose::EditServer(s.url),
            None => Purpose::AddServer,
        });
        self.dirty = true;
    }

    fn start_edit_server(&mut self, index: usize) {
        if let Some(Item::Server(s)) = self.items.get(index) {
            let s = s.clone();
            self.start_server_form(Some(s));
        }
    }

    fn start_rename(&mut self, index: usize) {
        let Some(item) = self.items.get(index) else {
            return;
        };
        let current = item.name().to_string();
        self.view.form = Some(Form::new(
            format!("Rename {current}"),
            vec![Field {
                label: "New name".into(),
                value: current,
                secret: false,
                placeholder: String::new(),
            }],
            "Rename",
        ));
        self.purpose = Some(Purpose::Rename(index));
        self.dirty = true;
    }

    fn confirm_remove_server(&mut self, index: usize) {
        let Some(Item::Server(s)) = self.items.get(index) else {
            return;
        };
        self.view.dialog = Some(Dialog {
            title: format!("Remove {}?", s.name),
            body: vec!["Forgets this server and its password.".into()],
            buttons: vec!["Cancel".into(), "Remove".into()],
            danger: true,
            choice: None,
        });
        self.purpose = Some(Purpose::RemoveServer(index));
        self.dirty = true;
    }

    fn click_dialog(&mut self, button: usize) {
        let confirmed = self
            .view
            .dialog
            .as_ref()
            .is_some_and(|d| button + 1 == d.buttons.len());
        self.view.dialog = None;
        self.dirty = true;
        let purpose = self.purpose.take();
        match &purpose {
            Some(Purpose::BufferSize) => {
                if let Some(&bytes) = config::RESERVE_PRESETS.get(button)
                    && let Err(e) = config::save_reserve_bytes(bytes)
                {
                    eprintln!("Can't save the buffer size: {e:#}");
                }
                self.rebuild_rows();
                return;
            }
            Some(Purpose::Curvature) => {
                if let Some(&(_, curvature)) = config::CURVATURE_PRESETS.get(button) {
                    self.curvature = curvature;
                    if let Err(e) = config::save_curvature(curvature) {
                        eprintln!("Can't save the curvature: {e:#}");
                    }
                }
                self.rebuild_rows();
                return;
            }
            Some(Purpose::ShortJump) => {
                if let Some(&s) = config::SHORT_JUMP_PRESETS.get(button) {
                    self.change_preference(|p| p.short_jump = s);
                }
                return;
            }
            Some(Purpose::LongJump) => {
                if let Some(&s) = config::LONG_JUMP_PRESETS.get(button) {
                    self.change_preference(|p| p.long_jump = s);
                }
                return;
            }
            Some(Purpose::VolumeStep) => {
                if let Some(&s) = config::VOLUME_STEP_PRESETS.get(button) {
                    self.change_preference(|p| p.volume_step = s);
                }
                return;
            }
            Some(Purpose::BrowserWidth) => {
                if let Some(&width) = config::BROWSER_WIDTH_PRESETS.get(button) {
                    self.browser_width = width;
                    if let Err(e) = config::save_browser_width(width) {
                        eprintln!("Can't save the browser width: {e:#}");
                    }
                }
                self.rebuild_rows();
                return;
            }
            _ => {}
        }
        if !confirmed {
            return;
        }
        match purpose {
            Some(Purpose::RemoveServer(index)) => {
                if let Some(Item::Server(s)) = self.items.get(index) {
                    let url = s.url.clone();
                    match config::remove_server(&url) {
                        Ok(_) => self.show_servers(),
                        Err(e) => self.dialog("Can't remove the server", vec![format!("{e:#}")]),
                    }
                }
            }
            Some(Purpose::Delete(indices)) => {
                let Location::Folder {
                    server,
                    share,
                    path,
                } = self.location.clone()
                else {
                    return;
                };
                self.selecting = None;
                let selected = indices;
                for index in selected {
                    let Some(item) = self.items.get(index) else {
                        continue;
                    };
                    let mut file = path.clone();
                    file.push(item.name().to_string());
                    let id = self.id();
                    self.pending_changes.insert(id);
                    self.library.send(Request::Delete {
                        id,
                        server: server.clone(),
                        share: share.clone(),
                        path: file,
                    });
                }
                if !self.pending_changes.is_empty() {
                    self.view.notice =
                        Some(format!("Deleting {} items…", self.pending_changes.len()));
                }
                self.rebuild_rows();
            }
            _ => {}
        }
    }

    fn click_form(&mut self, hit: form::Hit) {
        let Some(f) = &mut self.view.form else { return };
        self.dirty = true;
        match hit {
            form::Hit::Field(i, cursor) => f.focus(i, Some(cursor)),
            form::Hit::Toggle => f.flip_toggle(),
            form::Hit::Key(key) => match f.press(key, &mut self.clipboard) {
                Some(Key::Cancel) => {
                    self.view.form = None;
                    self.purpose = None;
                }
                Some(Key::Submit) => self.submit_form(),
                _ => {}
            },
            form::Hit::Nothing => f.clear_selection(),
        }
    }

    /// Dragging across field `i`: selects from character `from` to `to`.
    pub fn drag_select(&mut self, i: usize, from: usize, to: usize) {
        if let Some(f) = &mut self.view.form
            && i < f.fields.len()
        {
            f.select_range(i, from, to);
            self.dirty = true;
        }
    }

    /// Holding on field `i` selects all of its text.
    pub fn select_all_field(&mut self, i: usize) {
        if let Some(f) = &mut self.view.form
            && i < f.fields.len()
        {
            f.focus(i, None);
            f.select_all();
            self.dirty = true;
        }
    }

    fn submit_form(&mut self) {
        let Some(f) = self.view.form.as_mut() else {
            return;
        };
        match &self.purpose {
            Some(Purpose::AddServer | Purpose::EditServer(_)) => {
                let editing = match &self.purpose {
                    Some(Purpose::EditServer(url)) => Some(url.clone()),
                    _ => None,
                };
                let (address, user, typed_password, name) = (
                    f.value(0).trim().to_string(),
                    f.value(1).trim().to_string(),
                    f.value(2).to_string(),
                    f.value(3).trim().to_string(),
                );
                if address.is_empty() {
                    f.error = Some("Enter an address.".into());
                    f.focus(0, None);
                    return;
                }
                let host = address
                    .trim_start_matches("smb://")
                    .trim_end_matches('/')
                    .to_string();
                let user = if user.is_empty() {
                    "guest".to_string()
                } else {
                    user
                };
                let url = match format!("smb://{user}@{host}").parse::<SmbUrl>() {
                    Ok(url) if url.share.is_empty() => url,
                    Ok(_) => {
                        f.error = Some("Server only, without a share or folder.".into());
                        return;
                    }
                    Err(e) => {
                        f.error = Some(format!("{e:#}"));
                        return;
                    }
                };
                // Editing: an empty password keeps the saved one.
                let password = match (&editing, typed_password.is_empty()) {
                    (Some(old), true) => config::password(old).ok().flatten().unwrap_or_default(),
                    _ => typed_password,
                };
                let server = Server {
                    name: if name.is_empty() {
                        url.host.clone()
                    } else {
                        name
                    },
                    url: url.server_url(),
                    writable: f.toggled(),
                };
                // Saving would replace that other entry, and its password.
                if let Some(old) = &editing
                    && *old != server.url
                    && let Some(other) = config::servers()
                        .unwrap_or_default()
                        .into_iter()
                        .find(|s| s.url == server.url)
                {
                    f.error = Some(format!(
                        "{} is already saved with this address and user.",
                        other.name
                    ));
                    return;
                }
                f.busy = Some(format!("Connecting to {}…", url.host));
                let id = self.id();
                self.pending_changes.insert(id);
                self.library.send(Request::AddServer {
                    id,
                    server,
                    password,
                });
            }
            Some(Purpose::Rename(index)) => {
                let index = *index;
                let new_name = f.value(0).trim().to_string();
                let Some(item) = self.items.get(index) else {
                    return;
                };
                if new_name.is_empty() || new_name == item.name() {
                    self.view.form = None;
                    self.purpose = None;
                    return;
                }
                if let Location::Folder {
                    server,
                    share,
                    mut path,
                } = self.location.clone()
                {
                    path.push(item.name().to_string());
                    f.busy = Some("Renaming…".into());
                    let id = self.id();
                    self.pending_changes.insert(id);
                    self.library.send(Request::Rename {
                        id,
                        server,
                        share,
                        path,
                        new_name,
                    });
                }
            }
            _ => {}
        }
    }

    pub fn dialog_open(&self) -> bool {
        self.view.dialog.is_some()
    }

    /// Goes up one level (or closes a dialog, form or selection). False at the top.
    pub fn back(&mut self) -> bool {
        self.dirty = true;
        if self.move_request.is_some() {
            return true;
        }
        if self.view.form.is_some() && self.pending_changes.is_empty() {
            self.view.form = None;
            self.purpose = None;
            return true;
        }
        if self.view.dialog.is_some() {
            self.view.dialog = None;
            self.purpose = None;
            return true;
        }
        if self.selecting.take().is_some() {
            self.rebuild_rows();
            return true;
        }
        match self.location.clone() {
            Location::Servers => return false,
            Location::Settings | Location::Shares { .. } => self.show_servers(),
            Location::Folder {
                server,
                share,
                mut path,
            } => {
                if path.pop().is_some() {
                    self.navigate(Location::Folder {
                        server,
                        share,
                        path,
                    });
                } else {
                    self.navigate(Location::Shares { server });
                }
            }
        }
        true
    }

    #[cfg(test)]
    fn show_entries_for_test(&mut self, server: Server, names: &[&str]) {
        self.location = Location::Folder {
            server,
            share: "s".into(),
            path: Vec::new(),
        };
        self.reset_view();
        self.folder_loaded = true;
        self.items = names.iter().map(|n| Item::Dir(n.to_string())).collect();
        self.rebuild_rows();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn move_navigator() -> (
        Navigator,
        std::sync::mpsc::Receiver<Request>,
        std::sync::mpsc::Sender<Response>,
    ) {
        let (library, requests, responses) = Library::for_test();
        let mut nav = Navigator::new(library);
        let server = Server {
            name: "NAS".into(),
            url: "smb://u@nas".into(),
            writable: true,
        };
        nav.show_entries_for_test(server, &["a", "b", "destination"]);
        if let Location::Folder { path, .. } = &mut nav.location {
            path.push("old".into());
        }
        nav.long_press(0);
        nav.click(Hit::Row(1));
        nav.click(Hit::Tool(2));
        (nav, requests, responses)
    }

    #[test]
    fn moving_keeps_source_paths_through_navigation_and_partial_failure() {
        let (mut nav, requests, responses) = move_navigator();
        let sources = vec![
            vec!["old".to_string(), "a".to_string()],
            vec!["old".to_string(), "b".to_string()],
        ];
        assert_eq!(nav.moving.as_ref().unwrap().paths, sources);
        assert!(nav.selecting.is_none());
        nav.click(Hit::Row(2));
        assert_eq!(nav.view().tools.len(), 1, "only cancel while loading");
        let Request::List { id, path, .. } = requests.try_recv().unwrap() else {
            panic!("expected listing")
        };
        assert_eq!(path, vec!["old", "destination"]);
        responses
            .send(Response::List {
                id,
                result: Ok(vec![]),
            })
            .unwrap();
        nav.poll();
        assert_eq!(
            nav.view().tools[1].label,
            "Move here",
            "empty folders accept moves"
        );
        nav.click(Hit::Tool(1));
        let Request::Move {
            id,
            selection,
            destination,
        } = requests.try_recv().unwrap()
        else {
            panic!("expected move")
        };
        assert_eq!(selection.paths, sources);
        assert_eq!(destination, vec!["old", "destination"]);
        let location = nav.location.clone();
        nav.click(Hit::Crumb(0));
        nav.long_press(0);
        assert!(nav.back());
        assert_eq!(
            nav.location, location,
            "in-flight moves block duplicate actions"
        );
        assert!(requests.try_recv().is_err());

        responses
            .send(Response::Moved {
                id,
                result: crate::library::MoveResult {
                    failed: vec![sources[1].clone()],
                    errors: vec!["b: Name exists".into()],
                },
            })
            .unwrap();
        nav.poll();
        assert_eq!(nav.moving.as_ref().unwrap().paths, vec![sources[1].clone()]);
        assert!(nav.dialog_open());
        let Request::List { id, .. } = requests.try_recv().unwrap() else {
            panic!("expected refresh")
        };
        responses
            .send(Response::List {
                id,
                result: Ok(vec![]),
            })
            .unwrap();
        nav.poll();
        nav.back();
        nav.click(Hit::Tool(1));
        let Request::Move { id, selection, .. } = requests.try_recv().unwrap() else {
            panic!("expected retry")
        };
        assert_eq!(
            selection.paths,
            vec![sources[1].clone()],
            "never retry successful moves"
        );
        responses
            .send(Response::Moved {
                id,
                result: Default::default(),
            })
            .unwrap();
        nav.poll();
        assert!(nav.moving.is_none(), "all done");
        assert!(nav.move_request.is_none());
    }

    #[test]
    fn invalid_destinations_and_cancel_leave_sources_untouched() {
        let (mut nav, requests, _) = move_navigator();
        nav.click(Hit::Tool(1));
        assert!(nav.dialog_open(), "source folder is a no-op");
        assert!(requests.try_recv().is_err());
        nav.back();
        if let Location::Folder { share, .. } = &mut nav.location {
            *share = "other".into();
        }
        nav.rebuild_rows();
        nav.click(Hit::Tool(1));
        assert!(nav.dialog_open(), "other shares need copy-and-delete");
        assert!(requests.try_recv().is_err());
        assert!(nav.moving.is_some());
        nav.back();
        nav.click(Hit::Tool(0));
        assert!(nav.moving.is_none());
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn unloaded_destinations_cannot_start_a_move() {
        let (mut nav, requests, _) = move_navigator();
        nav.folder_loaded = false;
        nav.rebuild_rows();
        assert_eq!(nav.view().tools.len(), 1, "cancel only");
        nav.move_here();
        assert!(nav.move_request.is_none());
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn loading_shows_progress_and_can_be_cancelled() {
        let (library, requests, responses) = Library::for_test();
        let mut nav = Navigator::new(library);
        let server = Server {
            name: "NAS".into(),
            url: "smb://u@nas".into(),
            writable: true,
        };
        nav.show_entries_for_test(server, &["sub"]);
        assert_eq!(nav.progress(), None);
        nav.select(0);
        assert_eq!(nav.progress(), Some(Progress::Busy));
        let Request::List { id, watch, .. } = requests.try_recv().unwrap() else {
            panic!("expected listing")
        };
        watch.count.store(120, Ordering::Relaxed);
        nav.poll();
        assert_eq!(nav.view().status.as_deref(), Some("Loading… 120 entries"));
        assert_eq!(nav.view().tools[0].label, "Cancel");
        nav.click(Hit::Tool(0));
        assert!(watch.cancel.load(Ordering::Relaxed), "the listing stops");
        assert!(matches!(&nav.location, Location::Folder { path, .. } if path.is_empty()));
        // The cancelled listing's answer is ignored; the parent's is used.
        responses
            .send(Response::List {
                id,
                result: Err("Cancelled".into()),
            })
            .unwrap();
        let Request::List { id, .. } = requests.try_recv().unwrap() else {
            panic!("expected the parent listing")
        };
        let video = |name: &str| crate::smb::Entry {
            name: name.into(),
            is_dir: false,
            size: 1,
            modified: 0,
        };
        responses
            .send(Response::List {
                id,
                result: Ok(vec![video("a.mp4"), video("b.mp4")]),
            })
            .unwrap();
        nav.poll();
        assert_eq!(nav.view().rows.len(), 2);
        assert_eq!(
            nav.progress(),
            Some(Progress::Fraction(0.0)),
            "probes pending"
        );
        let Request::Probe { generation, .. } = requests.try_recv().unwrap() else {
            panic!("expected a probe")
        };
        responses
            .send(Response::Probe {
                generation,
                name: "a.mp4".into(),
                result: Err("bad".into()),
            })
            .unwrap();
        nav.poll();
        assert_eq!(nav.progress(), Some(Progress::Fraction(0.5)));
        responses
            .send(Response::Probe {
                generation,
                name: "b.mp4".into(),
                result: Err("bad".into()),
            })
            .unwrap();
        nav.poll();
        assert_eq!(nav.progress(), None);
    }

    #[test]
    fn video_extensions() {
        assert!(is_video("clip_180_LR.MP4"));
        assert!(is_video("movie.mkv"));
        assert!(!is_video("notes.txt"));
        assert!(!is_video("mkv"));
    }

    #[test]
    fn settings_page_shows_values_and_previews_choices() {
        // The jump choice below is saved: not into the user's settings.
        let dir = config::temp_config("nav-settings");
        let mut nav = Navigator::new(Library::start(None));
        let last = nav.view().rows.len() - 1;
        nav.click(Hit::Row(last));
        assert_eq!(nav.view().crumbs, ["Just Video", "Settings"]);
        assert!(nav.view().tools.is_empty(), "no exit button here");
        let icons: Vec<Icon> = nav.view().rows.iter().map(|r| r.icon.clone()).collect();
        assert_eq!(
            icons,
            [
                Icon::Jump,
                Icon::Jump,
                Icon::Jump,
                Icon::Volume,
                Icon::Continue,
                Icon::Loop,
                Icon::Dpad,
                Icon::Thumbnails(false),
                Icon::Buffer,
                Icon::Curve,
                Icon::Width,
                Icon::Server
            ]
        );
        let (jump, long_jump, landing, volume, resume, dpad, thumbnails, curvature, width, lanes) =
            (0, 1, 2, 3, 4, 6, 7, 9, 10, 11);
        // The A-B loop: off by default, shown on the player bar when on.
        let ab_loop = 5;
        assert_eq!(nav.view().rows[ab_loop].right, "Off");
        nav.click(Hit::Row(ab_loop));
        assert_eq!(nav.view().rows[ab_loop].right, "On");
        assert!(config::preferences().ab_loop, "saved");
        nav.click(Hit::Row(ab_loop));
        assert!(!config::preferences().ab_loop);
        // Video connections: two-way, saved outside the player preferences.
        assert_eq!(nav.view().rows[lanes].right, "One");
        nav.click(Hit::Row(lanes));
        assert_eq!(nav.view().rows[lanes].right, "Two");
        assert_eq!(config::video_connections(), 2, "saved");
        nav.click(Hit::Row(lanes));
        assert_eq!(config::video_connections(), 1);
        assert_eq!(nav.view().rows[curvature].right, "Full");
        // The curvature dialog previews the choice pointed at, else the saved one.
        nav.click(Hit::Row(curvature));
        assert_eq!(
            nav.view().dialog.as_ref().map(|d| d.title.as_str()),
            Some("Screen curvature")
        );
        assert_eq!(
            nav.preview(Hit::DialogButton(0)),
            Some(Preview::Curvature(0.25))
        );
        assert_eq!(
            nav.preview(Hit::Nothing),
            Some(Preview::Curvature(nav.curvature()))
        );
        nav.back();
        assert_eq!(nav.preview(Hit::Nothing), None);
        nav.click(Hit::Row(width));
        assert_eq!(
            nav.preview(Hit::DialogButton(0)),
            Some(Preview::BrowserWidth(0.5))
        );
        nav.back();
        // Where jumps land: two-way, so a click switches, saved and shown.
        nav.click(Hit::Row(landing));
        assert!(nav.seek_exact());
        assert_eq!(nav.view().rows[landing].right, "Exact");
        assert!(config::seek_exact(), "saved");
        nav.click(Hit::Row(landing));
        assert!(!nav.seek_exact());
        // Jump lengths and the volume step: preset dialogs, saved together.
        assert_eq!(nav.view().rows[jump].right, "5 s");
        nav.click(Hit::Row(jump));
        assert_eq!(nav.view().dialog.as_ref().map(|d| d.choice), Some(Some(0)));
        nav.click(Hit::DialogButton(2));
        assert_eq!(nav.view().rows[jump].right, "15 s");
        nav.click(Hit::Row(long_jump));
        nav.click(Hit::DialogButton(4));
        assert_eq!(nav.view().rows[long_jump].right, "10 min");
        nav.click(Hit::Row(volume));
        nav.click(Hit::DialogButton(0));
        assert_eq!(nav.view().rows[volume].right, "5 %");
        nav.click(Hit::Row(resume));
        assert_eq!(nav.view().rows[resume].right, "Off");
        nav.click(Hit::Row(dpad));
        assert_eq!(nav.view().rows[dpad].right, "On");
        // Thumbnails: off by default, the same setting as the folder button.
        assert_eq!(nav.view().rows[thumbnails].right, "Off");
        nav.click(Hit::Row(thumbnails));
        assert_eq!(nav.view().rows[thumbnails].right, "On");
        assert_eq!(nav.view().rows[thumbnails].icon, Icon::Thumbnails(true));
        let p = config::preferences();
        assert_eq!((p.short_jump, p.long_jump, p.volume_step), (15, 600, 5));
        assert!(!p.resume && p.dpad_menus && p.thumbnails);
        assert_eq!(nav.preferences(), p);
        // B goes back to the server list, which marks the way in.
        assert!(nav.back());
        assert_eq!(nav.view().crumbs, ["Just Video"]);
        assert!(nav.view().rows[last].outlined, "the way back in");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn server_list_offers_adding_one_and_exiting() {
        let mut nav = Navigator::new(Library::start(None));
        let icons: Vec<Icon> = nav.view().rows.iter().map(|r| r.icon.clone()).collect();
        assert_eq!(&icons[icons.len() - 2..], [Icon::Add, Icon::Settings]);
        assert_eq!(nav.view().crumbs, vec!["Just Video".to_string()]);
        assert_eq!(nav.view().tools.len(), 1, "the exit button, top right");
        assert!(!nav.take_exit());
        nav.click(Hit::Tool(0));
        assert!(nav.take_exit());
        assert!(!nav.take_exit(), "reported once");
    }

    #[test]
    fn server_list_starts_with_the_headset() {
        let mut nav = Navigator::new(Library::start(None));
        assert_eq!(nav.view().rows[0].label, "This headset");
        nav.long_press(0);
        assert!(
            nav.selecting.is_none(),
            "the headset can't be edited or removed"
        );
        assert!(nav.view().rows[0].actions.is_empty());
    }

    #[test]
    fn holding_an_entry_selects_it_and_offers_tools() {
        let mut nav = Navigator::new(Library::start(None));
        let server = Server {
            name: "NAS".into(),
            url: "smb://u@nas".into(),
            writable: true,
        };
        nav.show_entries_for_test(server.clone(), &["a", "b", "c"]);
        assert!(nav.view().rows.iter().all(|r| r.actions.is_empty()));
        assert!(nav.view().tools.is_empty(), "no edit toggle");
        assert!(nav.view().heading.is_none());

        nav.long_press(1);
        assert_eq!(nav.view().rows[1].checked, Some(true), "long press selects");
        assert_eq!(nav.view().rows[0].checked, Some(false));
        assert!(nav.view().rows.iter().all(|r| r.actions.is_empty()));
        assert_eq!(nav.view().heading.as_deref(), Some("1 selected"));
        let labels: Vec<&str> = nav.view().tools.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["Cancel", "Delete", "Move", "Rename"]);

        nav.click(Hit::Row(2));
        assert_eq!(nav.view().heading.as_deref(), Some("2 selected"));
        let labels: Vec<&str> = nav.view().tools.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["Cancel", "Delete 2", "Move 2"], "rename needs one");
        nav.click(Hit::Row(2));
        nav.click(Hit::Tool(1));
        assert!(nav.dialog_open());
        assert!(nav.back(), "closes the dialog");
        assert!(nav.back(), "leaves selection");
        assert!(nav.view().rows.iter().all(|r| r.checked.is_none()));
        assert!(nav.view().heading.is_none(), "crumbs are back");

        nav.long_press(1);
        nav.click(Hit::Tool(3));
        assert!(matches!(nav.purpose, Some(Purpose::Rename(1))));
        assert_eq!(nav.view().form.as_ref().map(|f| f.value(0)), Some("b"));
    }

    #[test]
    fn a_video_left_part_way_can_start_over() {
        let mut nav = Navigator::new(Library::start(None));
        let server = Server {
            name: "NAS".into(),
            url: "smb://u@nas".into(),
            writable: true,
        };
        nav.show_entries_for_test(server.clone(), &["folder"]);
        // Marked directly: the config directory is the user's in tests, and
        // forgetting a key that isn't saved writes nothing.
        nav.items.push(Item::Video {
            name: "clip.mp4".into(),
            size: 1,
            modified: 0,
            assessment: None,
            layout: None,
            broken: None,
            resume: Some(Place::Resume(600.0, Some(2400.0))),
        });
        nav.rebuild_rows();
        assert_eq!(nav.view().rows[1].progress, Some(0.25));

        nav.long_press(1);
        let labels: Vec<&str> = nav.view().tools.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["Cancel", "Delete", "Move", "Rename", "Start over"]);
        nav.click(Hit::Tool(4));
        assert!(nav.selecting.is_none());
        assert_eq!(nav.view().rows[1].progress, None);
        assert!(nav.view().heading.is_none());

        let key = crate::library::file_key(&server, "s", &vec!["clip.mp4".to_string()]);
        nav.note_resume(&key, Some(Place::Resume(1200.0, Some(2400.0))));
        assert_eq!(nav.view().rows[1].progress, Some(0.5));
    }

    fn video(name: &str, size: u64) -> Item {
        Item::Video {
            name: name.into(),
            size,
            modified: 0,
            assessment: None,
            layout: None,
            broken: None,
            resume: None,
        }
    }

    fn names(nav: &Navigator) -> Vec<&str> {
        nav.items.iter().map(Item::name).collect()
    }

    #[test]
    fn sort_orders_folders_first_then_by_mode() {
        let mut nav = Navigator::new(Library::start(None));
        let server = Server {
            name: "NAS".into(),
            url: "smb://u@nas".into(),
            writable: false,
        };
        nav.show_entries_for_test(server, &["zeta", "alpha"]);
        let mut done = video("a.mp4", 50);
        let mut half = video("c.mp4", 20);
        if let (Item::Video { resume: d, .. }, Item::Video { resume: h, .. }) =
            (&mut done, &mut half)
        {
            *d = Some(Place::Watched);
            *h = Some(Place::Resume(60.0, Some(600.0)));
        }
        nav.items.extend([
            video("b.mp4", 5),
            done,
            half,
            Item::File {
                name: "notes.txt".into(),
                size: 1,
            },
        ]);
        nav.sort = SortMode::Size;
        nav.sort_items();
        assert_eq!(
            names(&nav),
            ["alpha", "zeta", "a.mp4", "c.mp4", "b.mp4", "notes.txt"]
        );
        nav.sort = SortMode::Unwatched;
        nav.sort_items();
        assert_eq!(
            names(&nav),
            ["alpha", "zeta", "c.mp4", "b.mp4", "a.mp4", "notes.txt"],
            "started, then unwatched, then watched"
        );
        nav.sort = SortMode::Name;
        nav.sort_items();
        assert_eq!(
            names(&nav),
            ["alpha", "zeta", "a.mp4", "b.mp4", "c.mp4", "notes.txt"]
        );
    }

    #[test]
    fn sort_tool_needs_two_videos_and_cycles() {
        let mut nav = Navigator::new(Library::start(None));
        let server = Server {
            name: "NAS".into(),
            url: "smb://u@nas".into(),
            writable: false,
        };
        nav.show_entries_for_test(server, &[]);
        nav.items.push(video("a.mp4", 1));
        nav.rebuild_rows();
        let labels = |nav: &Navigator| -> Vec<String> {
            nav.view().tools.iter().map(|t| t.label.clone()).collect()
        };
        assert_eq!(labels(&nav), [""], "only the thumbnails toggle");
        nav.items.push(video("b.mp4", 2));
        nav.sort = SortMode::Name;
        nav.autoplay = false;
        nav.rebuild_rows();
        assert_eq!(labels(&nav), ["", "Sort: A-Z", "Autoplay: Off"]);
        nav.autoplay = true;
        nav.rebuild_rows();
        assert_eq!(labels(&nav)[2], "Autoplay: On");
        // Not while selecting: the header belongs to the selection.
        nav.selecting = Some(HashSet::new());
        nav.rebuild_rows();
        assert!(!labels(&nav).iter().any(|l| l.starts_with("Sort")));
    }

    #[test]
    fn rows_carry_a_watched_mark_for_every_readable_video() {
        let mut nav = Navigator::new(Library::start(None));
        let server = Server {
            name: "NAS".into(),
            url: "smb://u@nas".into(),
            writable: false,
        };
        nav.show_entries_for_test(server, &["dir"]);
        let video = |name: &str, broken: Option<&str>, resume| Item::Video {
            name: name.into(),
            size: 1,
            modified: 0,
            assessment: None,
            layout: None,
            broken: broken.map(Into::into),
            resume,
        };
        nav.items.extend([
            video("new.mp4", None, None),
            video("half.mp4", None, Some(Place::Resume(60.0, Some(600.0)))),
            video("done.mp4", None, Some(Place::Watched)),
            video("bad.mp4", Some("bad"), None),
        ]);
        nav.rebuild_rows();
        let marks: Vec<_> = nav.view().rows.iter().map(|r| r.watch).collect();
        assert_eq!(
            marks,
            [
                None,
                Some(Watch::Unwatched),
                Some(Watch::Partial),
                Some(Watch::Watched),
                None
            ],
            "folder, new, half, done, unreadable"
        );
    }

    #[test]
    fn progress_shows_in_the_detail_line() {
        assert_eq!(with_progress("8K HEVC", None), "8K HEVC");
        assert_eq!(
            with_progress("8K HEVC", Some(Place::Resume(754.0, None))),
            "Resume 12:34 · 8K HEVC"
        );
        assert_eq!(
            with_progress("Plays well", Some(Place::Watched)),
            "Watched · Plays well"
        );
        let mut nav = Navigator::new(Library::start(None));
        let server = Server {
            name: "NAS".into(),
            url: "smb://u@nas".into(),
            writable: false,
        };
        nav.show_entries_for_test(server.clone(), &[]);
        nav.items.push(Item::Video {
            name: "a.mp4".into(),
            size: 1,
            modified: 0,
            assessment: None,
            layout: None,
            broken: None,
            resume: Some(Place::Watched),
        });
        nav.rebuild_rows();
        assert_eq!(nav.view().rows[0].detail, "Watched · Checking…");
        assert_eq!(
            nav.view().rows[0].progress,
            None,
            "no line for a watched video"
        );
    }

    #[test]
    fn read_only_servers_offer_start_over_but_no_changes() {
        let mut nav = Navigator::new(Library::start(None));
        let server = Server {
            name: "NAS".into(),
            url: "smb://u@nas".into(),
            writable: false,
        };
        nav.show_entries_for_test(server.clone(), &["a", "b"]);
        nav.items.push(Item::Video {
            name: "clip.mp4".into(),
            size: 1,
            modified: 0,
            assessment: None,
            layout: None,
            broken: None,
            resume: Some(Place::Resume(1200.0, Some(2400.0))),
        });
        nav.rebuild_rows();
        assert!(nav.view().notice.is_none());
        nav.long_press(1);
        assert_eq!(nav.view().rows[1].checked, Some(true), "still selectable");
        let labels: Vec<&str> = nav.view().tools.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["Cancel"], "nothing changes files");
        assert_eq!(nav.view().notice.as_deref(), Some(READ_ONLY_NOTICE));
        assert!(nav.back(), "leaves selection");
        assert!(nav.view().notice.is_none(), "the hint goes with it");
        nav.long_press(2);
        let labels: Vec<&str> = nav.view().tools.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(
            labels,
            ["Cancel", "Start over"],
            "forgetting a position is local"
        );
        nav.back();
        // The new server form starts read only; its checkbox allows changes.
        nav.show_servers();
        nav.start_server_form(None);
        let form = nav.view().form.as_ref().expect("the server form");
        assert!(!form.toggled());
        nav.click(Hit::Form(form::Hit::Toggle));
        assert!(nav.view().form.as_ref().unwrap().toggled());
    }

    #[test]
    fn holding_a_server_reveals_its_buttons() {
        let mut nav = Navigator::new(Library::start(None));
        nav.items = vec![
            Item::Server(Server {
                name: "NAS".into(),
                url: "smb://u@nas".into(),
                writable: true,
            }),
            Item::Server(Server {
                name: "PC".into(),
                url: "smb://u@pc".into(),
                writable: true,
            }),
            Item::AddServer,
        ];
        nav.rebuild_rows();
        assert!(nav.view().rows.iter().all(|r| r.actions.is_empty()));
        nav.long_press(1);
        assert_eq!(
            nav.view().rows[1].actions,
            vec![Action::Settings, Action::Remove]
        );
        assert!(nav.view().rows[0].actions.is_empty());
        assert!(nav.view().rows.iter().all(|r| r.checked.is_none()));
        assert_eq!(nav.view().tools.len(), 1, "cancel");
        nav.click(Hit::RowAction(1, Action::Settings));
        assert!(matches!(nav.purpose, Some(Purpose::EditServer(ref u)) if u == "smb://u@pc"));
        let form = nav.view().form.as_ref().expect("the server form");
        assert_eq!(form.title, "Edit PC");
        assert!(form.toggled(), "as saved");
        nav.click(Hit::Form(form::Hit::Toggle));
        assert!(!nav.view().form.as_ref().unwrap().toggled());
        nav.back();
        nav.long_press(1);
        assert!(nav.back(), "B closes the buttons");
        assert!(nav.view().rows[1].actions.is_empty());
        nav.long_press(2);
        assert!(nav.selecting.is_none(), "nothing to hold on Add server");
    }

    #[test]
    fn editing_a_server_into_another_one_is_refused() {
        let dir = config::temp_config("nav-edit-dup");
        let pc = Server {
            name: "PC".into(),
            url: "smb://alice@pc".into(),
            writable: true,
        };
        let nas = Server {
            name: "NAS".into(),
            url: "smb://bob@nas".into(),
            writable: true,
        };
        config::save_server(pc.clone(), "a").unwrap();
        config::save_server(nas.clone(), "b").unwrap();
        let mut nav = Navigator::new(Library::start(None));
        // Row 0 is the headset itself; PC is the first saved server.
        nav.click(Hit::RowAction(1, Action::Settings));
        let form = nav.view.form.as_mut().expect("edit form");
        form.fields[0].value = "nas".into();
        form.fields[1].value = "bob".into();
        nav.click(Hit::Form(form::Hit::Key(Key::Submit)));
        let form = nav.view().form.as_ref().expect("still open");
        assert_eq!(form.busy, None, "not saved");
        assert!(form.error.as_deref().is_some_and(|e| e.starts_with("NAS ")));
        assert_eq!(config::servers().unwrap(), vec![pc, nas.clone()]);
        assert_eq!(config::password(&nas.url).unwrap().as_deref(), Some("b"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn videos_are_marked_by_format() {
        let mut nav = Navigator::new(Library::start(None));
        let server = Server {
            name: "NAS".into(),
            url: "smb://u@nas".into(),
            writable: true,
        };
        nav.show_entries_for_test(server, &[]);
        let a = crate::playability::assess(crate::playability::Platform::SteamFrame, None);
        nav.items = ["film.mkv", "film_sbs.mkv", "trip_180_LR.mp4", "new_360.mp4"]
            .into_iter()
            .map(|name| Item::Video {
                name: name.into(),
                size: 1,
                modified: 0,
                assessment: Some(a.clone()),
                layout: Some(crate::vr::detect(name, None)),
                broken: None,
                resume: None,
            })
            .collect();
        if let Some(Item::Video {
            assessment, layout, ..
        }) = nav.items.last_mut()
        {
            (*assessment, *layout) = (None, None); // not probed yet
        }
        nav.rebuild_rows();
        let rows = &nav.view().rows;
        let v = Some(a.verdict);
        assert_eq!(rows[0].icon, Icon::Video(v));
        assert_eq!(rows[0].detail, a.summary);
        assert_eq!(rows[1].icon, Icon::Video3d(v));
        assert_eq!(rows[1].detail, format!("3D  ·  {}", a.summary));
        assert_eq!(rows[2].icon, Icon::VideoVr(v));
        assert_eq!(rows[2].detail, format!("VR180 3D  ·  {}", a.summary));
        assert_eq!(rows[3].icon, Icon::Video(None));
    }

    #[test]
    fn going_up_returns_to_the_folder_left() {
        let mut nav = Navigator::new(Library::start(None));
        let server = Server {
            name: "NAS".into(),
            url: "smb://u@nas".into(),
            writable: true,
        };
        let names: Vec<String> = (0..40).map(|i| format!("f{i:02}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        nav.show_entries_for_test(server.clone(), &names);
        nav.set_scroll(20.0);
        nav.select(25);
        assert!(matches!(&nav.location, Location::Folder { path, .. } if path == &["f25"]));
        nav.back();
        assert_eq!(nav.came_from.as_deref(), Some("f25"));
        // The listing arrives.
        nav.items = names.iter().map(|n| Item::Dir(n.to_string())).collect();
        nav.restore_scroll();
        nav.rebuild_rows();
        assert_eq!(nav.scroll(), 20.0);
        assert!(nav.view().rows[25].outlined);
        assert_eq!(nav.view().rows.iter().filter(|r| r.outlined).count(), 1);
    }

    #[test]
    fn previous_and_next_skip_what_cannot_play() {
        let mut nav = Navigator::new(Library::start(None));
        let server = Server {
            name: "NAS".into(),
            url: "smb://u@nas".into(),
            writable: true,
        };
        nav.show_entries_for_test(server, &["folder"]);
        let video = |name: &str, broken: bool| Item::Video {
            name: name.into(),
            size: 1,
            modified: 0,
            assessment: None,
            layout: None,
            broken: broken.then(|| "bad".to_string()),
            resume: None,
        };
        let file = Item::File {
            name: "notes.txt".into(),
            size: 1,
        };
        nav.items
            .extend([video("a", false), video("b", true), file, video("c", false)]);
        nav.playing = Some(1);
        assert!(!nav.has_adjacent(-1), "a folder is not a video");
        assert_eq!(
            nav.adjacent(1),
            Some(4),
            "skips the broken one and the file"
        );
        nav.autoplay = true;
        assert!(nav.autoplay_next(), "a video follows the one playing");
        nav.autoplay = false;
        assert!(!nav.autoplay_next(), "off: stop at the end");
        nav.playing = Some(4);
        nav.autoplay = true;
        assert!(!nav.autoplay_next(), "the last video has no next");
        assert!(!nav.has_adjacent(1));
        nav.playback_ended();
        assert!(nav.view().rows[4].outlined, "the video just played");
        assert_eq!(nav.view().rows.iter().filter(|r| r.outlined).count(), 1);
    }

    fn video_folder(nav: &mut Navigator, names: &[&str]) {
        let server = Server {
            name: "nas".into(),
            url: "smb://me@nas".into(),
            writable: false,
        };
        nav.show_entries_for_test(server, &["sub"]);
        let a = crate::playability::assess(crate::playability::Platform::SteamFrame, None);
        nav.items.extend(names.iter().map(|name| Item::Video {
            name: name.to_string(),
            size: 1,
            modified: 7,
            assessment: Some(a.clone()),
            layout: Some(crate::vr::detect(name, None)),
            broken: None,
            resume: None,
        }));
        nav.rebuild_rows();
    }

    fn thumb() -> Arc<Thumb> {
        Arc::new(Thumb {
            width: 220,
            height: 124,
            rgba: vec![255; 220 * 124 * 4],
        })
    }

    #[test]
    fn the_thumbnails_tool_is_only_in_folders_with_videos() {
        let (library, _responses) = Library::detached();
        let mut nav = Navigator::new(library);
        let tool = |nav: &Navigator| {
            nav.view()
                .tools
                .iter()
                .any(|t| matches!(t.icon, Some(Icon::Thumbnails(_))))
        };
        assert!(!tool(&nav), "server list");
        nav.show_entries_for_test(
            Server {
                name: "nas".into(),
                url: "smb://me@nas".into(),
                writable: false,
            },
            &["a", "b"],
        );
        assert!(!tool(&nav), "only folders");
        video_folder(&mut nav, &["x.mp4"]);
        assert!(tool(&nav));
        assert!(!nav.view().thumbnails, "off by default");
        nav.selecting = Some(HashSet::new());
        nav.rebuild_rows();
        assert!(!tool(&nav), "not while selecting");
    }

    #[test]
    fn toggling_thumbnails_changes_rows_and_drops_pictures() {
        let dir = config::temp_config("nav-thumb-toggle");
        let (library, responses) = Library::detached();
        let mut nav = Navigator::new(library);
        let names: Vec<String> = (0..20).map(|i| format!("clip {i}.mp4")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        video_folder(&mut nav, &names);
        nav.view.scroll = 5.0;
        nav.click(Hit::Tool(0));
        assert!(nav.view().thumbnails);
        assert!(
            nav.preferences().thumbnails && config::preferences().thumbnails,
            "saved"
        );
        assert_eq!(nav.view().scroll, 5.0, "top row stays");
        assert_eq!(nav.thumbs_sent.len(), 20, "all handed to the worker");

        // Pictures arriving together make one rebuild; stale ones are ignored.
        let g = nav.thumb_generation;
        for (generation, name) in [(g, "clip 3.mp4"), (g, "clip 4.mp4"), (g - 1, "clip 5.mp4")] {
            responses
                .send(Response::Thumbnail {
                    generation,
                    name: name.into(),
                    image: thumb(),
                })
                .unwrap();
        }
        nav.take_dirty();
        nav.poll();
        assert!(nav.take_dirty());
        let shown: Vec<bool> = nav
            .view()
            .rows
            .iter()
            .map(|r| r.thumbnail.is_some())
            .collect();
        assert_eq!(shown.iter().filter(|s| **s).count(), 2);
        assert!(shown[1 + 3] && shown[1 + 4], "after the folder row");

        nav.click(Hit::Tool(0));
        assert!(!nav.view().thumbnails);
        assert!(nav.thumbs.is_empty() && nav.thumbs_sent.is_empty());
        assert_eq!(nav.thumb_generation, g + 1, "pending work is dropped");
        assert!(nav.view().rows.iter().all(|r| r.thumbnail.is_none()));
        // A straggler from the old generation is ignored.
        responses
            .send(Response::Thumbnail {
                generation: g,
                name: "clip 3.mp4".into(),
                image: thumb(),
            })
            .unwrap();
        nav.poll();
        assert!(nav.thumbs.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
