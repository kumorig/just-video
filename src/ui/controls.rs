//! The playback control bar (opened by clicking the video) and its dialogs:
//!
//! ```text
//! [⏮][▶][⏭]                               [CC]    [▭]     [☀]
//!    [⤓] 412 MB of 1 GB                   English 180° 3D  Image
//! 12:34 / 1:32:10  ━━━━━━━━━━●──────────────────────────────────
//! ```
//!
//! The small button under play buffers ahead: a click pauses the video and
//! fetches up to a gigabyte on from here (only while paused, so that
//! playback never competes with it for the network); play, or another
//! click, stops it. The seek bar shows the stretch held.
//!
//! CC: a click turns subtitles on/off, a long press opens the audio and
//! subtitle dialog (tracks, and a way into subtitle editing: the bar then
//! holds the size and position buttons while the screen and subtitle area
//! are outlined, with sample text). The screen button: a
//! click steps through the favourite formats, a long press opens the screen
//! dialog (every format, stars, curved screen, buffer button). The image
//! button opens the image dialog (brightness, contrast, saturation,
//! rotation); a second click closes it.
//! Volume is left to the headset's buttons; the thumbstick click resets the
//! screen.

use super::canvas::{Canvas, Fonts, Rgb};
use crate::ab_loop::AbLoop;
use crate::config::ImageAdjust;
use crate::vr::{Projection, Stereo};

pub const WIDTH: u32 = 1200;
pub const HEIGHT: u32 = 264;
pub const DIALOG_WIDTH: u32 = 1200;
pub const DIALOG_HEIGHT: u32 = 640;

const BG: Rgb = [0x15, 0x17, 0x1c];
const BUTTON: Rgb = [0x26, 0x2b, 0x34];
const HOVER: Rgb = [0x35, 0x3d, 0x4a];
const TEXT: Rgb = [0xe8, 0xea, 0xed];
const SUBTLE: Rgb = [0x9a, 0xa0, 0xa6];
const FAINT: Rgb = [0x4a, 0x4f, 0x58];
const ACCENT: Rgb = [0x4f, 0x8c, 0xff];
const TRACK: Rgb = [0x3a, 0x40, 0x4c];
const BUFFERED: Rgb = [0x5c, 0x66, 0x78];
const STAR: Rgb = [0xff, 0xcc, 0x00];

pub type Format = (Projection, Stereo);

/// Every format, as listed in the screen dialog.
pub const FORMATS: &[Format] = &[
    (Projection::Flat, Stereo::Mono),
    (Projection::Flat, Stereo::SideBySide),
    (Projection::Flat, Stereo::TopBottom),
    (Projection::Equirect180, Stereo::SideBySide),
    (Projection::Equirect180, Stereo::TopBottom),
    (Projection::Equirect180, Stereo::Mono),
    (Projection::Fisheye180, Stereo::SideBySide),
    (Projection::Equirect360, Stereo::Mono),
    (Projection::Equirect360, Stereo::TopBottom),
    (Projection::Equirect360, Stereo::SideBySide),
];

pub fn format_label((projection, stereo): Format) -> String {
    let shape = match projection {
        Projection::Flat => "Flat",
        Projection::Equirect180 => "180°",
        Projection::Equirect360 => "360°",
        Projection::Fisheye180 => "Fisheye",
    };
    let depth = match stereo {
        Stereo::Mono => "2D",
        Stereo::SideBySide => "3D SBS",
        Stereo::TopBottom => "3D TB",
    };
    format!("{shape} {depth}")
}

/// The favourite after `current` (the first one when `current` isn't a favourite).
pub fn next_favourite(current: Format, favourites: &[Format]) -> Option<Format> {
    let next = match favourites.iter().position(|f| *f == current) {
        Some(i) => (i + 1) % favourites.len(),
        None => 0,
    };
    favourites.get(next).copied()
}

/// A dialog shown above the bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialog {
    Tracks,
    Screen,
    Image,
}

/// Steps of the image adjustments (the app clamps the results).
pub const BRIGHTNESS_STEP: f32 = 0.05;
pub const CONTRAST_STEP: f32 = 0.1;
pub const SATURATION_STEP: f32 = 0.1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    Previous,
    PlayPause,
    Next,
    /// Buffer ahead while paused, on or off.
    Reserve,
    /// Fraction of the duration.
    Seek(f32),
    /// CC: click toggles subtitles, long press opens the track dialog.
    Captions,
    /// Click: next favourite format; long press: the screen dialog.
    Screen,
    /// Opens the image dialog (or closes it).
    Image,
    /// A-B loop: set A, set B, clear.
    Loop,
    // In the dialogs:
    AudioTrack(usize),
    /// A subtitle track, or None for off.
    SubtitleTrack(Option<usize>),
    /// The next page of a long subtitle list.
    MorePage,
    /// Subtitles smaller (-1) or larger (+1).
    CaptionSize(i8),
    /// Subtitles lower (-1) or higher (+1).
    CaptionMove(i8),
    /// Into subtitle editing (from the track dialog).
    CaptionEdit,
    /// Subtitle size and position back to the defaults.
    CaptionReset,
    /// Out of subtitle editing.
    CaptionDone,
    /// A format (index into [`FORMATS`]); long press stars it.
    Pick(usize),
    Curved,
    /// Show or hide the buffer-ahead button on the bar.
    BufferButton,
    Brightness(i8),
    Contrast(i8),
    Saturation(i8),
    /// Quarter turns clockwise.
    Rotate(u8),
    ResetImage,
    Close,
    Nothing,
}

impl Hit {
    /// Hits with a long-press action (the rest act on release).
    pub fn has_long_press(self) -> bool {
        matches!(self, Hit::Captions | Hit::Screen | Hit::Pick(_))
    }
}

/// Buffering ahead while paused (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reserve {
    pub on: bool,
    /// Bytes held from here on, and how many it aims for (the rest of the
    /// video, up to the cap).
    pub held: u64,
    pub goal: u64,
}

/// A button being held: how far into its long press, in steps of
/// [`PRESS_STEPS`]. Buttons without a long press are held at the end.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pressing {
    pub hit: Hit,
    pub step: u8,
}

pub const PRESS_STEPS: u8 = 24;

#[derive(Clone, Debug, PartialEq)]
pub struct State {
    pub paused: bool,
    pub position: f64,
    pub duration: f64,
    /// Stretches already fetched from the network, in seconds.
    pub buffered: Vec<(f64, f64)>,
    /// None for a local file, which needs no buffering.
    pub reserve: Option<Reserve>,
    pub has_previous: bool,
    pub has_next: bool,
    /// None when the curve toggle does not apply (VR180/360).
    pub curved: Option<bool>,
    /// The buffer-ahead button is shown (`reserve` is None when hidden).
    pub buffer_button: bool,
    pub format: Format,
    pub favourites: Vec<Format>,
    /// Names of the subtitle tracks (empty: none), and the one shown.
    pub subtitle_tracks: Vec<String>,
    pub subtitle: Option<usize>,
    /// Names of the audio tracks, and the one playing.
    pub audio_tracks: Vec<String>,
    pub audio: Option<usize>,
    /// Page of the subtitle list.
    pub list_page: usize,
    pub image: ImageAdjust,
    pub dialog: Option<Dialog>,
    /// Editing subtitle size and position: the bar holds only those buttons.
    pub caption_edit: bool,
    /// The A-B loop (marks on the seek bar, button state).
    pub ab_loop: AbLoop,
    /// The A-B loop setting: off hides the button and the marks.
    pub ab_loop_enabled: bool,
}

type Rect = (f32, f32, f32, f32);

/// The hairline along the bar's top edge, from `from` to `to` (fractions of
/// the width), sweeping while a picture is awaited after opening or
/// seeking. The app shows it as its own layer, so it can move without
/// redrawing the bar.
pub fn progress_rect(from: f32, to: f32) -> Rect {
    let (from, to) = (from.clamp(0.0, 1.0), to.clamp(0.0, 1.0));
    let full = WIDTH as f32;
    (full * from, 0.0, full * (to - from), PROGRESS_H)
}

/// Thickness of the hairline: 1 canvas pixel shimmers at the bar's size.
pub const PROGRESS_H: f32 = 3.0;

const PREVIOUS: Rect = (24.0, 16.0, 96.0, 92.0);
const PLAY: Rect = (136.0, 16.0, 96.0, 92.0);
const NEXT: Rect = (248.0, 16.0, 96.0, 92.0);
/// Under play: half a button, for buffering ahead.
const RESERVE: Rect = (136.0, 116.0, 96.0, 36.0);
/// What the reserve is doing, beside its button.
const RESERVE_LABEL: Rect = (248.0, 116.0, 440.0, 36.0);
/// Between the buffering text and the setting buttons.
const LOOP: Rect = (640.0, 16.0, 96.0, 92.0);
const CAPTIONS: Rect = (818.0, 16.0, 96.0, 92.0);
const SCREEN: Rect = (948.0, 16.0, 96.0, 92.0);
const IMAGE: Rect = (1078.0, 16.0, 96.0, 92.0);
const SEEK: Rect = (330.0, 164.0, 846.0, 80.0);

fn inside((x, y, w, h): Rect, px: f32, py: f32) -> bool {
    px >= x && px <= x + w && py >= y && py <= y + h
}

/// The bar's buttons while editing subtitles, with their labels.
const EDIT_BUTTONS: [(Hit, &str); 6] = [
    (Hit::CaptionSize(-1), "Smaller"),
    (Hit::CaptionSize(1), "Larger"),
    (Hit::CaptionMove(-1), "Lower"),
    (Hit::CaptionMove(1), "Higher"),
    (Hit::CaptionReset, "Reset"),
    (Hit::CaptionDone, "Done"),
];

fn edit_rect(i: usize) -> Rect {
    (24.0 + i as f32 * 194.0, 120.0, 180.0, 112.0)
}

/// What the bar would do for a pointer at (x, y).
pub fn hit(state: &State, x: f32, y: f32) -> Hit {
    if state.caption_edit {
        return EDIT_BUTTONS
            .iter()
            .enumerate()
            .find(|(i, _)| inside(edit_rect(*i), x, y))
            .map_or(Hit::Nothing, |(_, (hit, _))| *hit);
    }
    let can_caption = !state.subtitle_tracks.is_empty() || state.audio_tracks.len() > 1;
    let buttons = [
        (PREVIOUS, Hit::Previous, state.has_previous),
        (PLAY, Hit::PlayPause, true),
        (NEXT, Hit::Next, state.has_next),
        (RESERVE, Hit::Reserve, state.reserve.is_some()),
        (
            LOOP,
            Hit::Loop,
            state.ab_loop_enabled && state.duration > 0.0,
        ),
        (CAPTIONS, Hit::Captions, can_caption),
        (SCREEN, Hit::Screen, true),
        (IMAGE, Hit::Image, true),
    ];
    for (rect, hit, enabled) in buttons {
        if enabled && inside(rect, x, y) {
            return hit;
        }
    }
    // The seek bar is easy to hit: its whole row, a little beyond its ends.
    let (sx, sy, sw, sh) = SEEK;
    if state.duration > 0.0
        && y >= sy - 10.0
        && y <= sy + sh + 10.0
        && x >= sx - 20.0
        && x <= sx + sw + 20.0
    {
        return Hit::Seek(((x - sx) / sw).clamp(0.0, 1.0));
    }
    Hit::Nothing
}

// ---- Dialogs (DIALOG_WIDTH × DIALOG_HEIGHT) ----

const CLOSE: Rect = (1040.0, 16.0, 136.0, 60.0);
const COL_W: f32 = 276.0;
const ROW_H: f32 = 60.0;

/// A 4-column grid of buttons starting at `top`.
fn grid(hits: Vec<Hit>, top: f32) -> Vec<(Hit, Rect)> {
    hits.into_iter()
        .enumerate()
        .map(|(i, hit)| {
            let (col, row) = ((i % 4) as f32, (i / 4) as f32);
            (
                hit,
                (
                    24.0 + col * (COL_W + 16.0),
                    top + row * (ROW_H + 12.0),
                    COL_W,
                    ROW_H,
                ),
            )
        })
        .collect()
}

/// Subtitle choices per page (3 rows of 4, less the More button).
const PER_PAGE: usize = 11;

/// Pages needed for `items` subtitle choices (Off counts).
pub fn pages(items: usize) -> usize {
    if items <= 12 {
        1
    } else {
        items.div_ceil(PER_PAGE)
    }
}

/// Every button of the open dialog, with its rectangle and whether it is usable.
fn dialog_buttons(state: &State) -> Vec<(Hit, Rect, bool)> {
    let mut out: Vec<(Hit, Rect, bool)> = vec![(Hit::Close, CLOSE, true)];
    let mut add = |items: Vec<(Hit, Rect)>, enabled: bool| {
        out.extend(items.into_iter().map(|(h, r)| (h, r, enabled)))
    };
    match state.dialog {
        Some(Dialog::Tracks) => {
            add(
                grid(
                    (0..state.audio_tracks.len().min(4))
                        .map(Hit::AudioTrack)
                        .collect(),
                    132.0,
                ),
                true,
            );
            let mut subs = vec![Hit::SubtitleTrack(None)];
            subs.extend((0..state.subtitle_tracks.len()).map(|i| Hit::SubtitleTrack(Some(i))));
            let subs = if pages(subs.len()) == 1 {
                subs
            } else {
                let mut page: Vec<Hit> = subs
                    .into_iter()
                    .skip(state.list_page * PER_PAGE)
                    .take(PER_PAGE)
                    .collect();
                page.push(Hit::MorePage);
                page
            };
            add(grid(subs, 246.0), true);
            out.push((Hit::CaptionEdit, (24.0, 548.0, 568.0, ROW_H), true));
        }
        Some(Dialog::Screen) => {
            add(
                grid((0..FORMATS.len()).map(Hit::Pick).collect(), 132.0),
                true,
            );
            out.push((
                Hit::Curved,
                (24.0, 420.0, 568.0, 72.0),
                state.curved.is_some(),
            ));
            out.push((Hit::BufferButton, (608.0, 420.0, 568.0, 72.0), true));
        }
        Some(Dialog::Image) => {
            for (i, (minus, plus)) in [
                (Hit::Brightness(-1), Hit::Brightness(1)),
                (Hit::Contrast(-1), Hit::Contrast(1)),
                (Hit::Saturation(-1), Hit::Saturation(1)),
            ]
            .into_iter()
            .enumerate()
            {
                let y = 104.0 + i as f32 * 88.0;
                out.push((minus, (300.0, y, 96.0, 68.0), true));
                out.push((plus, (1080.0, y, 96.0, 68.0), true));
            }
            for turns in 0..4u8 {
                out.push((
                    Hit::Rotate(turns),
                    (300.0 + turns as f32 * 150.0, 392.0, 136.0, 68.0),
                    true,
                ));
            }
            out.push((Hit::ResetImage, (24.0, 540.0, COL_W, 72.0), true));
        }
        None => {}
    }
    out
}

/// The open dialog's usable buttons with their centres (for the D-pad).
pub fn dialog_targets(state: &State) -> Vec<(Hit, (f32, f32))> {
    dialog_buttons(state)
        .into_iter()
        .filter(|(_, _, enabled)| *enabled)
        .map(|(hit, (x, y, w, h), _)| (hit, (x + w / 2.0, y + h / 2.0)))
        .collect()
}

/// What the open dialog would do for a pointer at (x, y).
pub fn dialog_hit(state: &State, x: f32, y: f32) -> Hit {
    dialog_buttons(state)
        .into_iter()
        .find(|(_, r, enabled)| *enabled && inside(*r, x, y))
        .map_or(Hit::Nothing, |(hit, _, _)| hit)
}

/// "412 MB", "1.5 GB".
pub fn format_size(bytes: u64) -> String {
    let mib = bytes as f64 / (1u64 << 20) as f64;
    if mib >= 1000.0 {
        let gb = format!("{:.1}", mib / 1024.0);
        format!("{} GB", gb.strip_suffix(".0").unwrap_or(&gb))
    } else {
        format!("{mib:.0} MB")
    }
}

pub fn format_time(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn centered(c: &mut Canvas, fonts: &mut Fonts, label: &str, r: Rect, size: f32, color: Rgb) {
    // Shrink long labels to fit.
    let mut size = size;
    while size > 16.0 && fonts.measure(label, size) > r.2 - 16.0 {
        size -= 2.0;
    }
    let w = fonts.measure(label, size);
    fonts.draw(
        c,
        label,
        r.0 + (r.2 - w) / 2.0,
        r.1 + r.3 / 2.0 + size * 0.36,
        size,
        color,
        r.2,
    );
}

/// A button; held, it sits a touch smaller (pushed in), and one with a
/// long press fills from the left as the press runs.
fn button(c: &mut Canvas, r: Rect, hovered: bool, active: bool, pressing: Option<(f32, bool)>) {
    let color = if active {
        ACCENT
    } else if hovered {
        HOVER
    } else {
        BUTTON
    };
    let Some((fraction, long)) = pressing else {
        c.rect(r.0, r.1, r.2, r.3, 18.0, color);
        return;
    };
    let inset = 3.0;
    let (x, y, w, h) = (
        r.0 + inset,
        r.1 + inset,
        r.2 - 2.0 * inset,
        r.3 - 2.0 * inset,
    );
    c.rect(x, y, w, h, 16.0, color);
    if long {
        c.translucent_rect(x, y, w * fraction.clamp(0.0, 1.0), h, 16.0, ACCENT, 0.55);
    }
}

/// How far `hit` is pressed, for [`button`].
fn press_of(pressing: Option<Pressing>, hit: Hit) -> Option<(f32, bool)> {
    pressing
        .filter(|p| p.hit == hit)
        .map(|p| (p.step as f32 / PRESS_STEPS as f32, hit.has_long_press()))
}

/// A right-pointing triangle from vertical strips, `h` tall at its base.
fn triangle(c: &mut Canvas, x: f32, cy: f32, w: f32, h: f32, flip: bool, color: Rgb) {
    let steps = (w * 2.0) as usize;
    for i in 0..steps {
        let t = i as f32 / steps as f32;
        let half = h / 2.0 * (1.0 - t);
        let px = if flip { x + w - t * w } else { x + t * w };
        c.rect(px, cy - half, 1.5, half * 2.0, 0.0, color);
    }
}

/// A five-pointed star (favourite marker) of outer radius `r`.
fn star(c: &mut Canvas, cx: f32, cy: f32, r: f32, color: Rgb) {
    let points: Vec<(f32, f32)> = (0..10)
        .map(|i| {
            let a = std::f32::consts::PI * (i as f32 / 5.0 - 0.5);
            let radius = if i % 2 == 0 { r } else { r * 0.45 };
            (cx + radius * a.cos(), cy + radius * a.sin())
        })
        .collect();
    // Even-odd fill, one pixel at a time (the star is small).
    for py in (cy - r) as i32..=(cy + r) as i32 {
        for px in (cx - r) as i32..=(cx + r) as i32 {
            let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
            let mut inside = false;
            for i in 0..points.len() {
                let (a, b) = (points[i], points[(i + 1) % points.len()]);
                if (a.1 > y) != (b.1 > y) && x < a.0 + (y - a.1) / (b.1 - a.1) * (b.0 - a.0) {
                    inside = !inside;
                }
            }
            if inside {
                c.rect(px as f32, py as f32, 1.0, 1.0, 0.0, color);
            }
        }
    }
}

/// Buffer ahead: an arrow down into a tray.
fn reserve_icon(c: &mut Canvas, cx: f32, cy: f32, color: Rgb) {
    c.rect(cx - 2.0, cy - 13.0, 4.0, 10.0, 1.0, color);
    // The arrowhead, from horizontal strips.
    let steps = 16;
    for i in 0..steps {
        let t = i as f32 / steps as f32;
        let half = 8.0 * (1.0 - t);
        c.rect(cx - half, cy - 4.0 + t * 8.0, half * 2.0, 0.75, 0.0, color);
    }
    c.rect(cx - 12.0, cy + 5.0, 3.0, 8.0, 1.0, color);
    c.rect(cx + 9.0, cy + 5.0, 3.0, 8.0, 1.0, color);
    c.rect(cx - 12.0, cy + 10.0, 24.0, 3.0, 1.0, color);
}

/// A screen: a frame on a stand.
fn screen_icon(c: &mut Canvas, cx: f32, cy: f32, color: Rgb, fill: Rgb) {
    c.rect(cx - 26.0, cy - 20.0, 52.0, 32.0, 4.0, color);
    c.rect(cx - 21.0, cy - 15.0, 42.0, 22.0, 2.0, fill);
    c.rect(cx - 3.0, cy + 12.0, 6.0, 8.0, 0.0, color);
    c.rect(cx - 14.0, cy + 19.0, 28.0, 5.0, 2.0, color);
}

/// Image: a sun (brightness).
fn image_icon(c: &mut Canvas, cx: f32, cy: f32, color: Rgb) {
    c.circle(cx, cy, 11.0, color);
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::FRAC_PI_4;
        let (s, co) = a.sin_cos();
        c.rect(
            cx + co * 20.0 - 3.0,
            cy + s * 20.0 - 3.0,
            6.0,
            6.0,
            3.0,
            color,
        );
    }
}

/// The bar.
pub fn render(state: &State, fonts: &mut Fonts, hover: Hit, pressing: Option<Pressing>) -> Canvas {
    let mut c = Canvas::new(WIDTH, HEIGHT);
    c.clear(BG);
    let press = |hit| press_of(pressing, hit);
    if state.caption_edit {
        fonts.draw(
            &mut c,
            "Subtitle size and position",
            24.0,
            64.0,
            40.0,
            TEXT,
            800.0,
        );
        fonts.draw(&mut c, "B: done", 1040.0, 64.0, 26.0, SUBTLE, 140.0);
        for (i, (hit, label)) in EDIT_BUTTONS.iter().enumerate() {
            let r = edit_rect(i);
            button(
                &mut c,
                r,
                hover == *hit,
                *hit == Hit::CaptionDone,
                press(*hit),
            );
            centered(&mut c, fonts, label, r, 32.0, TEXT);
        }
        return c;
    }

    // Previous / play-pause / next.
    for (r, hit, enabled) in [
        (PREVIOUS, Hit::Previous, state.has_previous),
        (PLAY, Hit::PlayPause, true),
        (NEXT, Hit::Next, state.has_next),
    ] {
        button(&mut c, r, enabled && hover == hit, false, press(hit));
        let color = if enabled { TEXT } else { FAINT };
        let (cx, cy) = (r.0 + r.2 / 2.0, r.1 + r.3 / 2.0);
        match hit {
            Hit::PlayPause if state.paused => {
                triangle(&mut c, cx - 14.0, cy, 36.0, 48.0, false, color)
            }
            Hit::PlayPause => {
                c.rect(cx - 17.0, cy - 22.0, 12.0, 44.0, 3.0, color);
                c.rect(cx + 5.0, cy - 22.0, 12.0, 44.0, 3.0, color);
            }
            Hit::Previous => {
                c.rect(cx - 20.0, cy - 18.0, 7.0, 36.0, 2.0, color);
                triangle(&mut c, cx - 12.0, cy, 30.0, 36.0, true, color);
            }
            _ => {
                triangle(&mut c, cx - 18.0, cy, 30.0, 36.0, false, color);
                c.rect(cx + 13.0, cy - 18.0, 7.0, 36.0, 2.0, color);
            }
        }
    }

    // Buffering ahead: lit while on, with a line along its foot that grows
    // as the reserve fills.
    if let Some(reserve) = state.reserve {
        let hovered = hover == Hit::Reserve;
        let (x, y, w, h) = RESERVE;
        let color = if reserve.on {
            ACCENT
        } else if hovered {
            HOVER
        } else {
            BUTTON
        };
        if press(Hit::Reserve).is_some() {
            c.rect(x + 2.0, y + 2.0, w - 4.0, h - 4.0, 10.0, color);
        } else {
            c.rect(x, y, w, h, 12.0, color);
        }
        reserve_icon(&mut c, x + w / 2.0, y + h / 2.0 - 2.0, TEXT);
        if reserve.on {
            let done = (reserve.held as f32 / reserve.goal.max(1) as f32).clamp(0.0, 1.0);
            let (lx, lw) = (x + 12.0, w - 24.0);
            c.rect(lx, y + h - 7.0, lw, 3.0, 1.5, HOVER);
            c.rect(lx, y + h - 7.0, (lw * done).max(3.0), 3.0, 1.5, TEXT);
        }
        // A share of the goal, not sizes: how far along it is at a glance.
        let percent = (reserve.held as f64 / reserve.goal.max(1) as f64 * 100.0)
            .clamp(0.0, 100.0)
            .floor();
        let label = match (reserve.on, hovered) {
            (true, _) if reserve.held >= reserve.goal => "100% buffered".to_string(),
            (true, _) => format!("Buffering… {percent:.0}%"),
            (false, true) => "Buffer ahead while paused".to_string(),
            (false, false) => String::new(),
        };
        if !label.is_empty() {
            let (lx, ly, lw, lh) = RESERVE_LABEL;
            fonts.draw(
                &mut c,
                &label,
                lx,
                ly + lh / 2.0 + 22.0 * 0.36,
                22.0,
                SUBTLE,
                lw,
            );
        }
    }

    // The three setting buttons, each with a small label underneath.
    let label_under = |c: &mut Canvas, fonts: &mut Fonts, r: Rect, text: &str| {
        let area = (r.0 - 17.0, r.1 + r.3 + 4.0, r.2 + 34.0, 30.0);
        centered(c, fonts, text, area, 22.0, SUBTLE);
    };
    // CC only when there is something to choose: subtitles, or audio tracks.
    let can_caption = !state.subtitle_tracks.is_empty() || state.audio_tracks.len() > 1;
    if can_caption {
        let subtitles_on = state.subtitle.is_some();
        button(
            &mut c,
            CAPTIONS,
            hover == Hit::Captions,
            subtitles_on,
            press(Hit::Captions),
        );
        centered(&mut c, fonts, "CC", CAPTIONS, 34.0, TEXT);
        let caption_label = match state.subtitle {
            Some(i) => state.subtitle_tracks[i].clone(),
            None => "Off".to_string(),
        };
        label_under(&mut c, fonts, CAPTIONS, &caption_label);
    }

    if state.ab_loop_enabled {
        let looping = state.ab_loop.a().is_some();
        button(&mut c, LOOP, hover == Hit::Loop, looping, press(Hit::Loop));
        centered(&mut c, fonts, "A-B", LOOP, 34.0, TEXT);
        label_under(&mut c, fonts, LOOP, state.ab_loop.label());
    }

    let screen_fill = if hover == Hit::Screen { HOVER } else { BUTTON };
    button(
        &mut c,
        SCREEN,
        hover == Hit::Screen,
        false,
        press(Hit::Screen),
    );
    screen_icon(
        &mut c,
        SCREEN.0 + SCREEN.2 / 2.0,
        SCREEN.1 + SCREEN.3 / 2.0,
        TEXT,
        screen_fill,
    );
    label_under(&mut c, fonts, SCREEN, &format_label(state.format));

    let adjusted = state.image != ImageAdjust::default();
    button(
        &mut c,
        IMAGE,
        hover == Hit::Image,
        adjusted,
        press(Hit::Image),
    );
    image_icon(
        &mut c,
        IMAGE.0 + IMAGE.2 / 2.0,
        IMAGE.1 + IMAGE.3 / 2.0,
        TEXT,
    );
    label_under(&mut c, fonts, IMAGE, "Image");

    // Time and seek bar.
    let time = format!(
        "{} / {}",
        format_time(state.position),
        format_time(state.duration)
    );
    fonts.draw(
        &mut c,
        &time,
        24.0,
        SEEK.1 + SEEK.3 / 2.0 + 12.0,
        34.0,
        TEXT,
        SEEK.0 - 40.0,
    );
    let (sx, sy, sw, sh) = SEEK;
    let track_y = sy + sh / 2.0;
    c.rect(sx, track_y - 6.0, sw, 12.0, 6.0, TRACK);
    let fraction = if state.duration > 0.0 {
        (state.position / state.duration).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    // What is fetched already. A 32 MiB window is a sliver of a long video,
    // so a stretch at the playhead always peeks out past the knob.
    if state.duration > 0.0 {
        let knob = sx + sw * fraction;
        for &(from, to) in &state.buffered {
            let x0 = sx + sw * (from / state.duration).clamp(0.0, 1.0) as f32;
            let mut x1 = sx + sw * (to / state.duration).clamp(0.0, 1.0) as f32;
            if x0 <= knob + 14.0 && x1 >= knob - 14.0 {
                x1 = x1.max(knob + 14.0 + 8.0);
            }
            let w = (x1 - x0).max(6.0).min(sx + sw - x0);
            c.rect(x0, track_y - 6.0, w, 12.0, 6.0, BUFFERED);
        }
    }
    c.rect(sx, track_y - 6.0, sw * fraction, 12.0, 6.0, ACCENT);
    if state.ab_loop_enabled && state.duration > 0.0 {
        for mark in [state.ab_loop.a(), state.ab_loop.b()].into_iter().flatten() {
            let x = sx + sw * (mark / state.duration).clamp(0.0, 1.0) as f32;
            c.rect(x - 3.0, track_y - 20.0, 6.0, 40.0, 3.0, SUBTLE);
        }
    }
    if let Hit::Seek(f) = hover {
        c.rect(sx + sw * f - 2.0, track_y - 22.0, 4.0, 44.0, 2.0, SUBTLE);
        let label = format_time(f as f64 * state.duration);
        let w = fonts.measure(&label, 24.0);
        let lx = (sx + sw * f - w / 2.0).clamp(sx, sx + sw - w);
        fonts.draw(&mut c, &label, lx, sy + 6.0, 24.0, SUBTLE, w + 4.0);
    }
    c.circle(sx + sw * fraction, track_y, 14.0, TEXT);
    c
}

/// A slider's value as a bar between its − and + buttons.
fn value_bar(c: &mut Canvas, fonts: &mut Fonts, y: f32, fraction: f32, label: &str) {
    let (x, w) = (420.0, 640.0);
    c.rect(x, y + 28.0, w, 12.0, 6.0, TRACK);
    c.rect(x, y + 28.0, w * fraction.clamp(0.0, 1.0), 12.0, 6.0, ACCENT);
    c.rect(x + w * 0.5 - 1.0, y + 20.0, 2.0, 28.0, 1.0, SUBTLE);
    let lw = fonts.measure(label, 22.0);
    fonts.draw(c, label, x + w - lw, y + 16.0, 22.0, SUBTLE, lw + 4.0);
}

/// The open dialog.
pub fn render_dialog(
    state: &State,
    fonts: &mut Fonts,
    hover: Hit,
    pressing: Option<Pressing>,
) -> Canvas {
    let mut c = Canvas::new(DIALOG_WIDTH, DIALOG_HEIGHT);
    c.clear(BG);
    let title = match state.dialog {
        Some(Dialog::Tracks) => "Audio and subtitles",
        Some(Dialog::Screen) => "Screen",
        Some(Dialog::Image) => "Image",
        None => "",
    };
    fonts.draw(&mut c, title, 24.0, 60.0, 40.0, TEXT, 900.0);
    let section = |c: &mut Canvas, fonts: &mut Fonts, text: &str, y: f32| {
        fonts.draw(c, text, 24.0, y, 26.0, SUBTLE, 1150.0);
    };
    match state.dialog {
        Some(Dialog::Tracks) => {
            section(&mut c, fonts, "Audio", 118.0);
            section(&mut c, fonts, "Subtitles", 232.0);
            section(&mut c, fonts, "Subtitle size and position", 534.0);
        }
        Some(Dialog::Screen) => {
            section(
                &mut c,
                fonts,
                "Format  ·  click to use, long press to star a favourite",
                118.0,
            );
            section(&mut c, fonts, "Screen", 406.0);
        }
        Some(Dialog::Image) => {
            let image = &state.image;
            let rows = [
                (
                    "Brightness",
                    (image.brightness + 0.5) / 1.0,
                    format!("{:+.0}%", image.brightness * 100.0),
                ),
                (
                    "Contrast",
                    // 50%..100% on the left half, 100%..200% on the right.
                    if image.contrast < 1.0 {
                        image.contrast - 0.5
                    } else {
                        0.5 + (image.contrast - 1.0) / 2.0
                    },
                    format!("{:.0}%", image.contrast * 100.0),
                ),
                (
                    "Saturation",
                    image.saturation / 2.0,
                    format!("{:.0}%", image.saturation * 100.0),
                ),
            ];
            for (i, (name, fraction, value)) in rows.iter().enumerate() {
                let y = 104.0 + i as f32 * 88.0;
                fonts.draw(&mut c, name, 24.0, y + 46.0, 32.0, TEXT, 260.0);
                value_bar(&mut c, fonts, y, *fraction, value);
            }
            fonts.draw(&mut c, "Rotate", 24.0, 392.0 + 46.0, 32.0, TEXT, 260.0);
        }
        None => {}
    }
    for (hit, r, enabled) in dialog_buttons(state) {
        let hovered = enabled && hover == hit;
        let (label, active) = match hit {
            Hit::Close => ("Close".to_string(), false),
            Hit::AudioTrack(i) => (state.audio_tracks[i].clone(), state.audio == Some(i)),
            Hit::SubtitleTrack(None) => ("Off".to_string(), state.subtitle.is_none()),
            Hit::SubtitleTrack(Some(i)) => {
                (state.subtitle_tracks[i].clone(), state.subtitle == Some(i))
            }
            Hit::MorePage => {
                let pages = pages(state.subtitle_tracks.len() + 1);
                (format!("More ({}/{pages})", state.list_page + 1), false)
            }
            Hit::CaptionEdit => ("Adjust size and position…".to_string(), false),
            Hit::Pick(i) => (format_label(FORMATS[i]), FORMATS[i] == state.format),
            Hit::Curved => (
                (if state.curved == Some(true) {
                    "Curved screen: on"
                } else {
                    "Curved screen: off"
                })
                .to_string(),
                state.curved == Some(true),
            ),
            Hit::BufferButton => (
                (if state.buffer_button {
                    "Buffer button: shown"
                } else {
                    "Buffer button: hidden"
                })
                .to_string(),
                state.buffer_button,
            ),
            Hit::Brightness(d) | Hit::Contrast(d) | Hit::Saturation(d) => {
                ((if d < 0 { "−" } else { "+" }).to_string(), false)
            }
            Hit::Rotate(t) => (format!("{}°", t as u32 * 90), state.image.rotation == t),
            Hit::ResetImage => ("Reset".to_string(), false),
            _ => (String::new(), false),
        };
        button(
            &mut c,
            r,
            hovered,
            active && enabled,
            press_of(pressing, hit),
        );
        let size = if matches!(
            hit,
            Hit::Brightness(_) | Hit::Contrast(_) | Hit::Saturation(_)
        ) {
            44.0
        } else {
            28.0
        };
        centered(
            &mut c,
            fonts,
            &label,
            r,
            size,
            if enabled { TEXT } else { FAINT },
        );
        if let Hit::Pick(i) = hit
            && state.favourites.contains(&FORMATS[i])
        {
            star(&mut c, r.0 + r.2 - 22.0, r.1 + 20.0, 11.0, STAR);
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State {
            paused: false,
            position: 30.0,
            duration: 120.0,
            buffered: vec![(30.0, 34.0)],
            reserve: None,
            has_previous: true,
            has_next: false,
            curved: Some(false),
            buffer_button: true,
            format: FORMATS[0],
            favourites: vec![FORMATS[0], FORMATS[3]],
            subtitle_tracks: Vec::new(),
            subtitle: None,
            audio_tracks: vec!["Korean 5.1".into()],
            audio: Some(0),
            list_page: 0,
            image: ImageAdjust::default(),
            dialog: None,
            caption_edit: false,
            ab_loop: AbLoop::default(),
            ab_loop_enabled: true,
        }
    }

    fn center(r: Rect) -> (f32, f32) {
        (r.0 + r.2 / 2.0, r.1 + r.3 / 2.0)
    }

    #[test]
    fn bar_buttons_and_seek() {
        let s = state();
        assert_eq!(hit(&s, SEEK.0, SEEK.1 + 10.0), Hit::Seek(0.0));
        assert_eq!(
            hit(&s, SEEK.0 + SEEK.2 + 5.0, SEEK.1 + 10.0),
            Hit::Seek(1.0)
        );
        for (r, h) in [
            (PREVIOUS, Hit::Previous),
            (PLAY, Hit::PlayPause),
            (NEXT, Hit::Nothing),     // no next video
            (CAPTIONS, Hit::Nothing), // no subtitles, one audio track
            (SCREEN, Hit::Screen),
            (IMAGE, Hit::Image),
        ] {
            let (x, y) = center(r);
            assert_eq!(hit(&s, x, y), h);
            assert!(r.0 + r.2 <= WIDTH as f32 && r.1 + r.3 <= HEIGHT as f32);
        }
        let with_subs = State {
            subtitle_tracks: vec!["English".into()],
            ..s
        };
        let (x, y) = center(CAPTIONS);
        assert_eq!(hit(&with_subs, x, y), Hit::Captions);
    }

    #[test]
    fn the_loop_button_sits_clear_of_its_neighbours() {
        let (x, y) = center(LOOP);
        assert_eq!(hit(&state(), x, y), Hit::Loop);
        let unknown_length = State {
            duration: 0.0,
            ..state()
        };
        assert_eq!(hit(&unknown_length, x, y), Hit::Nothing);
        // With the A-B loop setting off its button isn't there.
        let off = State {
            ab_loop_enabled: false,
            ..state()
        };
        assert_eq!(hit(&off, x, y), Hit::Nothing);
        // Between the buffering text and the CC button, on the buttons' row.
        let (rx, _, rw, _) = RESERVE_LABEL;
        assert!(LOOP.0 >= rx + rw - 48.0 && LOOP.0 + LOOP.2 + 16.0 <= CAPTIONS.0);
        assert_eq!((LOOP.1, LOOP.3), (CAPTIONS.1, CAPTIONS.3));
        // Its marks show on the seek bar, and the bar still renders.
        let mut fonts = Fonts::load().expect("fonts");
        let mut looped = AbLoop::default();
        looped.press(10.0);
        looped.press(60.0);
        let canvas = render(
            &State {
                ab_loop: looped,
                ..state()
            },
            &mut fonts,
            Hit::Nothing,
            None,
        );
        let track_y = SEEK.1 + SEEK.3 / 2.0;
        let mark_x = SEEK.0 + SEEK.2 * (10.0 / 120.0);
        let i = ((track_y as u32 - 18) * canvas.width + mark_x as u32) * 4;
        assert_eq!(
            canvas.pixels[i as usize], SUBTLE[0],
            "A is marked above the track"
        );
    }

    #[test]
    fn reserve_button_only_for_network_files() {
        let (x, y) = center(RESERVE);
        assert_eq!(hit(&state(), x, y), Hit::Nothing);
        let network = State {
            reserve: Some(Reserve {
                on: true,
                held: 300 << 20,
                goal: 1 << 30,
            }),
            ..state()
        };
        assert_eq!(hit(&network, x, y), Hit::Reserve);
        // It sits between the play button and the seek row.
        assert!(RESERVE.1 >= PLAY.1 + PLAY.3 && RESERVE.1 + RESERVE.3 < SEEK.1 - 10.0);
        let mut fonts = Fonts::load().expect("fonts");
        let canvas = render(&network, &mut fonts, Hit::Reserve, None);
        assert_eq!(canvas.pixels.len(), (WIDTH * HEIGHT * 4) as usize);
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(format_size(412 << 20), "412 MB");
        assert_eq!(format_size(1 << 30), "1 GB");
        assert_eq!(format_size(1536 << 20), "1.5 GB");
    }

    #[test]
    fn dialogs_fit_and_hit() {
        for dialog in [Dialog::Tracks, Dialog::Screen, Dialog::Image] {
            let s = State {
                dialog: Some(dialog),
                subtitle_tracks: (0..20).map(|i| format!("Track {i}")).collect(),
                audio_tracks: vec!["A".into(), "B".into()],
                format: FORMATS[3],
                ..state()
            };
            for (h, r, enabled) in dialog_buttons(&s) {
                assert!(
                    r.0 >= 0.0
                        && r.0 + r.2 <= DIALOG_WIDTH as f32
                        && r.1 + r.3 <= DIALOG_HEIGHT as f32,
                    "{h:?} outside"
                );
                let (x, y) = center(r);
                assert_eq!(
                    dialog_hit(&s, x, y),
                    if enabled { h } else { Hit::Nothing },
                    "{dialog:?} {h:?}"
                );
            }
        }
    }

    #[test]
    fn caption_editing_replaces_the_bar() {
        let s = State {
            caption_edit: true,
            ..state()
        };
        for (i, (h, _)) in EDIT_BUTTONS.iter().enumerate() {
            let r = edit_rect(i);
            assert!(r.0 + r.2 <= WIDTH as f32 && r.1 + r.3 <= HEIGHT as f32);
            let (x, y) = center(r);
            assert_eq!(hit(&s, x, y), *h);
        }
        let (x, y) = center(PLAY);
        assert_eq!(hit(&s, x, y), Hit::Nothing);
    }

    #[test]
    fn long_subtitle_lists_page() {
        assert_eq!(pages(21), 2);
        assert_eq!(pages(12), 1, "Off + 11 tracks fit");
        let s = State {
            dialog: Some(Dialog::Tracks),
            subtitle_tracks: (0..20).map(|i| format!("Track {i}")).collect(),
            list_page: 1,
            ..state()
        };
        let subs: Vec<Hit> = dialog_buttons(&s)
            .into_iter()
            .map(|(h, _, _)| h)
            .filter(|h| matches!(h, Hit::SubtitleTrack(_) | Hit::MorePage))
            .collect();
        assert_eq!(subs.first(), Some(&Hit::SubtitleTrack(Some(10))));
        assert_eq!(subs.last(), Some(&Hit::MorePage));
    }

    #[test]
    fn format_button_steps_through_favourites() {
        let favourites = [FORMATS[0], FORMATS[3], FORMATS[7]];
        assert_eq!(next_favourite(FORMATS[0], &favourites), Some(FORMATS[3]));
        assert_eq!(next_favourite(FORMATS[7], &favourites), Some(FORMATS[0]));
        assert_eq!(
            next_favourite(FORMATS[5], &favourites),
            Some(FORMATS[0]),
            "not a favourite: start over"
        );
        assert_eq!(next_favourite(FORMATS[5], &[]), None);
    }

    #[test]
    fn times_read_naturally() {
        assert_eq!(format_time(75.0), "1:15");
        assert_eq!(format_time(5530.0), "1:32:10");
    }
}
