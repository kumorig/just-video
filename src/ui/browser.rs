//! The headset browser panel: a list of servers, shares, folders or videos,
//! drawn into a canvas that is shown on an OpenXR quad layer.

use super::canvas::{Canvas, Fonts, Rgb};
use super::form::{self, Form};
use crate::media::Thumb;
use crate::playability::Verdict;
use crate::vr::{Layout, Projection, Stereo};
use std::sync::Arc;

pub const WIDTH: u32 = 1600;
pub const HEIGHT: u32 = 1000;
const HEADER: f32 = 120.0;
const BOTTOM: f32 = 24.0;
/// Row heights: plain, and with a thumbnail slot.
const ROW: f32 = 88.0;
const ROW_THUMB: f32 = 140.0;
/// The thumbnail slot at the left of a tall row, and its corner radius.
const SLOT_W: f32 = crate::thumb_cache::THUMB_W as f32;
const SLOT_H: f32 = crate::thumb_cache::THUMB_H as f32;
const SLOT_R: f32 = 10.0;
const PAD: f32 = 32.0;
const CRUMB_SIZE: f32 = 40.0;
const CRUMB_SEP: &str = "  ›  ";
/// Row actions, from the right edge of a row.
const ACTION_W: f32 = 150.0;
const ICON_ACTION_W: f32 = 84.0;
/// The scrollbar's grab zone at the right edge of the list.
const SCROLL_W: f32 = 64.0;
/// Header tool buttons (e.g. "Cancel", "Delete 3").
const TOOL_W: f32 = 220.0;
/// An icon-only tool: a square.
const TOOL_ICON_W: f32 = 62.0;
const TOOL_GAP: f32 = 12.0;

/// Shown top right so installs can be told apart.
pub const BUILD: &str = env!("JUST_VIDEO_BUILD");

const BG: Rgb = [0x15, 0x17, 0x1c];
const ROW_BG: Rgb = [0x1d, 0x21, 0x28];
/// Behind the mark on a thumbnail, and in the slot until one arrives.
const SLOT_BG: Rgb = [0x11, 0x13, 0x17];
const HOVER: Rgb = [0x2c, 0x33, 0x40];
const TEXT: Rgb = [0xe8, 0xea, 0xed];
const SUBTLE: Rgb = [0x9a, 0xa0, 0xa6];
const FAINT: Rgb = [0x5f, 0x63, 0x68];
const ACCENT: Rgb = [0x4f, 0x8c, 0xff];
const GREEN: Rgb = [0x34, 0xc7, 0x59];
const YELLOW: Rgb = [0xff, 0xcc, 0x00];
const ORANGE: Rgb = [0xff, 0x95, 0x00];
const RED: Rgb = [0xff, 0x45, 0x3a];
const GREY: Rgb = [0x5f, 0x63, 0x68];

#[derive(Clone, Debug, PartialEq)]
pub enum Icon {
    Server,
    Share,
    Folder,
    /// A video; `None` while its playability is still being checked.
    Video(Option<Verdict>),
    /// A 3D video for a flat screen (side by side or top/bottom): glasses.
    Video3d(Option<Verdict>),
    /// A VR180, VR360 or fisheye video: a headset.
    VideoVr(Option<Verdict>),
    /// A file that could not be read.
    Broken,
    /// A file that isn't a video.
    File,
    Add,
    /// Buffering ahead: an arrow into a tray, as on the player's button.
    Buffer,
    /// A section that opens (true) and closes.
    Chevron(bool),
    /// A curved screen, seen from above.
    Curve,
    /// Width: a double-headed arrow.
    Width,
    /// A jump in the video: an arrow to a bar.
    Jump,
    /// Settings: a gear.
    Settings,
    /// A D-pad: a cross.
    Dpad,
    /// Loudness: a speaker with sound waves.
    Volume,
    /// Continuing a video: a play triangle after a bar.
    Continue,
    /// List thumbnails, on (true) or off: a picture in a frame.
    Thumbnails(bool),
    /// Close the app.
    Exit,
}

impl Icon {
    /// A video's mark: its shape says flat, 3D or VR, its colour how well it plays.
    pub fn video(verdict: Option<Verdict>, layout: Option<&Layout>) -> Icon {
        match layout {
            Some(l) if l.projection != Projection::Flat => Icon::VideoVr(verdict),
            Some(l) if l.stereo != Stereo::Mono => Icon::Video3d(verdict),
            _ => Icon::Video(verdict),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// A server's address, login and name.
    Settings,
    /// Remove a saved server.
    Remove,
}

impl Action {
    /// Remove is a trash can; the rest are labelled.
    fn is_icon(self) -> bool {
        matches!(self, Action::Remove)
    }

    fn width(self) -> f32 {
        if self.is_icon() {
            ICON_ACTION_W
        } else {
            ACTION_W
        }
    }

    fn label(self) -> &'static str {
        match self {
            Action::Settings => "Settings",
            Action::Remove => "Remove",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub icon: Icon,
    pub label: String,
    pub detail: String,
    pub right: String,
    /// Buttons at the right end, left to right.
    pub actions: Vec<Action>,
    /// A checkbox in place of the icon (selecting what to delete).
    pub checked: Option<bool>,
    /// Greyed out: listed, but not something to open (non-video files).
    pub dimmed: bool,
    /// Outlined: the entry just come back out of.
    pub outlined: bool,
    /// How much of a video was watched (a line along the row's foot).
    pub progress: Option<f32>,
    /// Indented: part of the section above it.
    pub inset: bool,
    /// A video's picture (for views with [`View::thumbnails`]).
    pub thumbnail: Option<Arc<Thumb>>,
}

impl Row {
    pub fn new(icon: Icon, label: impl Into<String>) -> Self {
        Self {
            icon,
            label: label.into(),
            detail: String::new(),
            right: String::new(),
            actions: Vec::new(),
            checked: None,
            dimmed: false,
            outlined: false,
            progress: None,
            inset: false,
            thumbnail: None,
        }
    }

    /// A video (or one that can't be read): what gets a thumbnail.
    fn is_video(&self) -> bool {
        matches!(
            self.icon,
            Icon::Video(_) | Icon::Video3d(_) | Icon::VideoVr(_) | Icon::Broken
        )
    }
}

/// A button in the header, right-aligned.
#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    pub label: String,
    pub danger: bool,
    /// Drawn instead of the label: a small square button.
    pub icon: Option<Icon>,
}

impl Tool {
    pub fn text(label: impl Into<String>, danger: bool) -> Self {
        Self {
            label: label.into(),
            danger,
            icon: None,
        }
    }

    pub fn icon(icon: Icon) -> Self {
        Self {
            label: String::new(),
            danger: false,
            icon: Some(icon),
        }
    }

    fn width(&self) -> f32 {
        if self.icon.is_some() {
            TOOL_ICON_W
        } else {
            TOOL_W
        }
    }
}

#[derive(Clone, Debug)]
pub struct Dialog {
    pub title: String,
    pub body: Vec<String>,
    /// Left to right; the last one is the primary action.
    pub buttons: Vec<String>,
    /// The primary button is destructive (drawn red).
    pub danger: bool,
    /// The buttons are equal choices, and this one is current (lit); no
    /// primary button then.
    pub choice: Option<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct View {
    /// Breadcrumb path; the last entry is the current place.
    pub crumbs: Vec<String>,
    /// Shown in place of the breadcrumbs (e.g. "2 selected" while the
    /// header holds the selection's tools).
    pub heading: Option<String>,
    pub rows: Vec<Row>,
    /// Shown instead of rows (loading, errors, empty folders).
    pub status: Option<String>,
    /// Small line at the bottom (e.g. "Opening …").
    pub notice: Option<String>,
    pub dialog: Option<Dialog>,
    pub form: Option<Form>,
    pub tools: Vec<Tool>,
    /// First visible row (fractional while scrolling).
    pub scroll: f32,
    /// Tall rows with a thumbnail slot (a folder of videos, thumbnails on).
    pub thumbnails: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Row(usize),
    Crumb(usize),
    RowAction(usize, Action),
    Tool(usize),
    /// The scrollbar: scroll with [`scroll_at`].
    ScrollBar,
    DialogButton(usize),
    Form(form::Hit),
    Nothing,
}

/// The thin progress bar under the header line, from `from` to `to`
/// (fractions of its full width): (x, y, w, h) in canvas pixels. The app
/// shows it as its own layer, so it can move without redrawing the panel.
pub fn progress_rect(from: f32, to: f32) -> (f32, f32, f32, f32) {
    let full = WIDTH as f32 - 2.0 * PAD;
    let (from, to) = (from.clamp(0.0, 1.0), to.clamp(0.0, 1.0));
    (PAD + full * from, HEADER - 8.0, full * (to - from), 3.0)
}

/// Canvas y where the list starts (below the header).
pub fn list_top() -> f32 {
    HEADER
}

impl View {
    /// Height of one row, in canvas pixels.
    pub fn row_h(&self) -> f32 {
        if self.thumbnails { ROW_THUMB } else { ROW }
    }

    /// How many rows fit in the list (fractional).
    pub fn visible_rows(&self) -> f32 {
        (HEIGHT as f32 - HEADER - BOTTOM) / self.row_h()
    }

    pub fn max_scroll(&self) -> f32 {
        (self.rows.len() as f32 - self.visible_rows()).max(0.0)
    }

    pub fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.clamp(0.0, self.max_scroll());
    }
}

pub type Rect = (f32, f32, f32, f32);

fn inside((x, y, w, h): Rect, px: f32, py: f32) -> bool {
    px >= x && px <= x + w && py >= y && py <= y + h
}

/// Up to three buttons in a row; more go in two columns, the last row at
/// the bottom.
fn dialog_buttons(n: usize) -> Vec<Rect> {
    let (h, gap) = (80.0, 24.0);
    let (cols, w) = if n <= 3 {
        (n.max(1), 280.0)
    } else {
        (2, 400.0)
    };
    let rows = n.div_ceil(cols);
    let total = cols as f32 * w + (cols - 1) as f32 * gap;
    let x0 = WIDTH as f32 / 2.0 - total / 2.0;
    (0..n)
        .map(|i| {
            let (col, row) = ((i % cols) as f32, (i / cols) as f32);
            let y = HEIGHT as f32 - 250.0 - (rows as f32 - 1.0 - row) * (h + gap);
            (x0 + col * (w + gap), y, w, h)
        })
        .collect()
}

fn tools_width(view: &View) -> f32 {
    view.tools.iter().map(|t| t.width() + TOOL_GAP).sum()
}

fn tool_rect(view: &View, k: usize) -> Rect {
    let right: f32 = view.tools[k..].iter().map(|t| t.width() + TOOL_GAP).sum();
    (
        WIDTH as f32 - PAD - right + TOOL_GAP,
        36.0,
        view.tools[k].width(),
        62.0,
    )
}

fn scroll_track() -> (f32, f32) {
    (HEADER, HEIGHT as f32 - BOTTOM - HEADER)
}

fn thumb_height(view: &View) -> f32 {
    (view.visible_rows() / view.rows.len().max(1) as f32 * scroll_track().1).max(60.0)
}

/// The scroll position that puts the scrollbar's thumb under canvas `y`.
pub fn scroll_at(view: &View, y: f32) -> f32 {
    let (top, track) = scroll_track();
    let thumb = thumb_height(view);
    let f = ((y - top - thumb / 2.0) / (track - thumb).max(1.0)).clamp(0.0, 1.0);
    f * view.max_scroll()
}

/// Rows moved by a vertical drag of `dy` canvas pixels.
pub fn rows_for_drag(view: &View, dy: f32) -> f32 {
    dy / view.row_h()
}

/// Where the breadcrumbs are drawn. A deep path folds its earliest crumbs
/// into one "…" so the row stops short of the header tools.
#[derive(Clone, Debug, PartialEq)]
pub struct CrumbLayout {
    /// Each crumb's horizontal extent; None when folded into the ellipsis.
    pub spans: Vec<Option<(f32, f32)>>,
    /// The ellipsis: its extent and the deepest crumb it stands for
    /// (where clicking it leads).
    pub ellipsis: Option<((f32, f32), usize)>,
}

impl CrumbLayout {
    /// What to draw, left to right: text, extent, and the crumb it is.
    fn elements<'a>(&self, view: &'a View) -> Vec<(&'a str, (f32, f32), usize)> {
        let mut out: Vec<(&str, (f32, f32), usize)> = self
            .spans
            .iter()
            .enumerate()
            .filter_map(|(i, span)| span.map(|s| (view.crumbs[i].as_str(), s, i)))
            .collect();
        if let Some((span, target)) = self.ellipsis {
            out.push((ELLIPSIS, span, target));
        }
        out.sort_by(|a, b| a.1.0.total_cmp(&b.1.0));
        out
    }
}

const ELLIPSIS: &str = "…";

pub fn crumb_layout(view: &View, fonts: &mut Fonts) -> CrumbLayout {
    let n = view.crumbs.len();
    let max_w = crumb_limit(view, fonts);
    let right = crumb_right(view, fonts);
    let sep = fonts.measure(CRUMB_SEP, CRUMB_SIZE);
    let ellipsis_w = fonts.measure(ELLIPSIS, CRUMB_SIZE);
    let widths: Vec<f32> = view
        .crumbs
        .iter()
        .map(|c| fonts.measure(c, CRUMB_SIZE).min(max_w))
        .collect();
    // Fold nothing; then the crumbs after the first, one more each time;
    // at last everything but the current place.
    for folded in 0..n {
        let hidden = if folded == 0 {
            0..0
        } else if folded < n - 1 {
            1..folded + 1
        } else {
            0..n - 1
        };
        let mut x = PAD;
        let mut first = true;
        let mut spans = vec![None; n];
        let mut ellipsis = None;
        for i in 0..n {
            if hidden.contains(&i) {
                if i == hidden.start {
                    if !first {
                        x += sep;
                    }
                    first = false;
                    ellipsis = Some(((x, x + ellipsis_w), hidden.end - 1));
                    x += ellipsis_w;
                }
                continue;
            }
            if !first {
                x += sep;
            }
            first = false;
            spans[i] = Some((x, x + widths[i]));
            x += widths[i];
        }
        if x <= right || folded == n - 1 {
            return CrumbLayout { spans, ellipsis };
        }
    }
    CrumbLayout {
        spans: Vec::new(),
        ellipsis: None,
    }
}

/// Where the crumbs must end: left of the tools and the build label.
fn crumb_right(view: &View, fonts: &mut Fonts) -> f32 {
    let right = (fonts.measure(BUILD, 20.0) + 40.0).max(tools_width(view) + 24.0);
    WIDTH as f32 - PAD - right
}

fn crumb_limit(view: &View, fonts: &mut Fonts) -> f32 {
    // The room, shared by at most a few long names.
    ((crumb_right(view, fonts) - PAD) / 3.0).max(160.0)
}

/// How far an inset row is indented.
const INSET: f32 = 64.0;

fn row_rect(view: &View, i: usize) -> Rect {
    let row_h = view.row_h();
    let y = HEADER + (i as f32 - view.scroll) * row_h;
    let inset = if view.rows[i].inset { INSET } else { 0.0 };
    (
        PAD + inset,
        y + 4.0,
        WIDTH as f32 - 2.0 * PAD - SCROLL_W - inset,
        row_h - 8.0,
    )
}

/// The `k`th action button of row `i`.
fn action_rect(view: &View, i: usize, k: usize) -> Rect {
    let (x, y, w, h) = row_rect(view, i);
    let right = x + w - 8.0;
    let actions = &view.rows[i].actions;
    let from_right: f32 = actions[k..].iter().map(|a| a.width() + 10.0).sum();
    (
        right - from_right + 10.0,
        y + 6.0,
        actions[k].width(),
        h - 12.0,
    )
}

/// What the D-pad moves between besides the list's rows, with their
/// rectangles: a form's fields and keys, a dialog's buttons (left to right),
/// or the header's breadcrumbs and tools (left to right).
pub fn fixed_targets(view: &View, fonts: &mut Fonts) -> Vec<(Hit, Rect)> {
    if let Some(f) = &view.form {
        return form::targets(f, WIDTH as f32)
            .into_iter()
            .map(|(h, r)| (Hit::Form(h), r))
            .collect();
    }
    if let Some(dialog) = &view.dialog {
        return dialog_buttons(dialog.buttons.len())
            .into_iter()
            .enumerate()
            .map(|(i, r)| (Hit::DialogButton(i), r))
            .collect();
    }
    // The last crumb is where we are: not a link (and none while a heading
    // stands in for them).
    let last = view.crumbs.len().saturating_sub(1);
    let crumbs: Vec<(Hit, Rect)> = if view.heading.is_some() {
        Vec::new()
    } else {
        crumb_layout(view, fonts)
            .elements(view)
            .into_iter()
            .filter(|&(_, _, i)| i < last)
            .map(|(_, (a, b), i)| (Hit::Crumb(i), (a - 8.0, 36.0, b - a + 16.0, 62.0)))
            .collect()
    };
    let tools = (0..view.tools.len()).map(|k| (Hit::Tool(k), tool_rect(view, k)));
    crumbs.into_iter().chain(tools).collect()
}

/// Row `i`'s targets, left to right: the row itself, then its buttons.
pub fn row_targets(view: &View, i: usize) -> Vec<(Hit, Rect)> {
    let Some(row) = view.rows.get(i) else {
        return Vec::new();
    };
    let (x, y, _, h) = row_rect(view, i);
    // The row: its label, left of any buttons.
    let mut out = vec![(Hit::Row(i), (x, y, 400.0, h))];
    for (k, action) in row.actions.iter().enumerate() {
        out.push((Hit::RowAction(i, *action), action_rect(view, i, k)));
    }
    out
}

/// What the pointer at canvas pixel (x, y) would activate.
pub fn hit(view: &View, fonts: &mut Fonts, x: f32, y: f32) -> Hit {
    if let Some(f) = &view.form {
        return Hit::Form(form::hit(f, fonts, WIDTH as f32, x, y));
    }
    if let Some(dialog) = &view.dialog {
        return dialog_buttons(dialog.buttons.len())
            .into_iter()
            .position(|r| inside(r, x, y))
            .map_or(Hit::Nothing, Hit::DialogButton);
    }
    if y < HEADER - 10.0 {
        if let Some(k) = (0..view.tools.len()).find(|&k| inside(tool_rect(view, k), x, y)) {
            return Hit::Tool(k);
        }
        if view.heading.is_some() {
            return Hit::Nothing;
        }
        // The last crumb is where we are: not a link.
        let last = view.crumbs.len().saturating_sub(1);
        return crumb_layout(view, fonts)
            .elements(view)
            .into_iter()
            .find(|&(_, (a, b), _)| x >= a - 8.0 && x <= b + 8.0 && y > 30.0)
            .filter(|&(_, _, i)| i < last)
            .map_or(Hit::Nothing, |(_, _, i)| Hit::Crumb(i));
    }
    if view.status.is_some() || y > HEIGHT as f32 - BOTTOM || x < PAD || x > WIDTH as f32 - PAD {
        return Hit::Nothing;
    }
    if x > WIDTH as f32 - PAD - SCROLL_W + 8.0 {
        return if view.max_scroll() > 0.0 {
            Hit::ScrollBar
        } else {
            Hit::Nothing
        };
    }
    let index = ((y - HEADER) / view.row_h() + view.scroll).floor();
    if index < 0.0 || index as usize >= view.rows.len() {
        return Hit::Nothing;
    }
    let i = index as usize;
    let row = &view.rows[i];
    for (k, action) in row.actions.iter().enumerate() {
        if inside(action_rect(view, i, k), x, y) {
            return Hit::RowAction(i, *action);
        }
    }
    Hit::Row(i)
}

/// `bg` is the row behind the icon, for cut-outs.
fn draw_icon(canvas: &mut Canvas, icon: &Icon, cx: f32, cy: f32, bg: Rgb) {
    draw_icon_scaled(canvas, icon, cx, cy, 1.0, bg);
}

/// [`draw_icon`] at `scale` times its size. Only a video's marks scale (they
/// go on thumbnail badges); the rest draw at their one size.
fn draw_icon_scaled(canvas: &mut Canvas, icon: &Icon, cx: f32, cy: f32, scale: f32, bg: Rgb) {
    match icon {
        Icon::Server => {
            canvas.rect(cx - 22.0, cy - 20.0, 44.0, 16.0, 4.0, ACCENT);
            canvas.rect(cx - 22.0, cy + 2.0, 44.0, 16.0, 4.0, ACCENT);
            canvas.circle(cx + 13.0, cy - 12.0, 3.0, BG);
            canvas.circle(cx + 13.0, cy + 10.0, 3.0, BG);
        }
        Icon::Share | Icon::Folder => {
            let color = if *icon == Icon::Share {
                ACCENT
            } else {
                [0x8a, 0xb4, 0xf8]
            };
            canvas.rect(cx - 24.0, cy - 18.0, 20.0, 10.0, 3.0, color);
            canvas.rect(cx - 24.0, cy - 12.0, 48.0, 32.0, 4.0, color);
        }
        Icon::Video(verdict) => {
            Pen::new(canvas, cx, cy, scale).circle(0.0, 0.0, 16.0, verdict_color(*verdict))
        }
        Icon::Video3d(verdict) => {
            // Glasses: two lenses on a bar, with short arms.
            let color = verdict_color(*verdict);
            let mut pen = Pen::new(canvas, cx, cy, scale);
            pen.rect(-26.0, -10.0, 52.0, 5.0, 2.0, color);
            pen.rect(-24.0, -10.0, 21.0, 19.0, 6.0, color);
            pen.rect(3.0, -10.0, 21.0, 19.0, 6.0, color);
        }
        Icon::VideoVr(verdict) => {
            // A headset from the front: visor with two lenses and a nose gap,
            // and the strap at the sides.
            let color = verdict_color(*verdict);
            let mut pen = Pen::new(canvas, cx, cy, scale);
            pen.rect(-29.0, -6.0, 58.0, 8.0, 3.0, color);
            pen.rect(-24.0, -17.0, 48.0, 33.0, 10.0, color);
            pen.circle(-11.0, -2.0, 7.0, bg);
            pen.circle(11.0, -2.0, 7.0, bg);
            pen.circle(0.0, 17.0, 7.0, bg);
        }
        Icon::Broken => {
            let mut pen = Pen::new(canvas, cx, cy, scale);
            pen.circle(0.0, 0.0, 16.0, RED);
            pen.rect(-9.0, -3.0, 18.0, 6.0, 2.0, BG);
        }
        Icon::Thumbnails(on) => {
            // A picture: frame, sun and a mountain. The frame lights up when
            // thumbnails are on.
            let frame = if *on { ACCENT } else { TEXT };
            canvas.rect(cx - 22.0, cy - 17.0, 44.0, 34.0, 6.0, frame);
            canvas.rect(cx - 18.0, cy - 13.0, 36.0, 26.0, 3.0, bg);
            canvas.circle(cx + 8.0, cy - 5.0, 4.0, frame);
            for i in 0..12 {
                let t = i as f32;
                canvas.rect(
                    cx - 17.0 + t * 1.2,
                    cy + 13.0 - 3.0 - t,
                    2.4,
                    3.0 + t,
                    0.0,
                    frame,
                );
            }
            for i in 0..8 {
                let t = i as f32;
                canvas.rect(
                    cx - 3.0 + t * 1.5,
                    cy + 13.0 - 3.0 - t * 0.9,
                    1.6,
                    3.0 + t * 0.9,
                    0.0,
                    frame,
                );
            }
        }
        Icon::File => {
            // A page with a folded top-right corner.
            canvas.rect(cx - 16.0, cy - 20.0, 32.0, 40.0, 4.0, FAINT);
            canvas.rect(cx - 12.0, cy - 16.0, 24.0, 32.0, 2.0, ROW_BG);
            canvas.rect(cx + 2.0, cy - 21.0, 15.0, 15.0, 0.0, ROW_BG);
            canvas.rect(cx + 2.0, cy - 20.0, 4.0, 14.0, 1.0, FAINT);
            canvas.rect(cx + 2.0, cy - 10.0, 14.0, 4.0, 1.0, FAINT);
        }
        Icon::Add => {
            canvas.rect(cx - 3.0, cy - 18.0, 6.0, 36.0, 3.0, ACCENT);
            canvas.rect(cx - 18.0, cy - 3.0, 36.0, 6.0, 3.0, ACCENT);
        }
        Icon::Buffer => {
            canvas.rect(cx - 3.0, cy - 20.0, 6.0, 16.0, 2.0, ACCENT);
            let steps = 24;
            for i in 0..steps {
                let t = i as f32 / steps as f32;
                let half = 12.0 * (1.0 - t);
                canvas.rect(
                    cx - half,
                    cy - 6.0 + t * 12.0,
                    half * 2.0,
                    0.75,
                    0.0,
                    ACCENT,
                );
            }
            canvas.rect(cx - 18.0, cy + 6.0, 5.0, 12.0, 2.0, ACCENT);
            canvas.rect(cx + 13.0, cy + 6.0, 5.0, 12.0, 2.0, ACCENT);
            canvas.rect(cx - 18.0, cy + 14.0, 36.0, 5.0, 2.0, ACCENT);
        }
        Icon::Chevron(open) => {
            // A triangle from strips: pointing right (closed) or down (open).
            for i in 0..24 {
                let t = i as f32;
                let half = 11.0 * (1.0 - t / 24.0);
                if *open {
                    canvas.rect(cx - half, cy - 6.0 + t, half * 2.0, 1.5, 0.0, SUBTLE);
                } else {
                    canvas.rect(cx - 6.0 + t, cy - half, 1.5, half * 2.0, 0.0, SUBTLE);
                }
            }
        }
        Icon::Curve => {
            // A screen bent around the viewer (the dot), seen from above.
            let steps = 32;
            for i in 0..=steps {
                let a = (i as f32 / steps as f32 - 0.5) * 1.6;
                canvas.rect(
                    cx + 22.0 * a.sin() - 2.5,
                    cy - 2.0 - 22.0 * a.cos() + 14.0,
                    5.0,
                    5.0,
                    2.0,
                    ACCENT,
                );
            }
            canvas.circle(cx, cy + 14.0, 5.0, SUBTLE);
        }
        Icon::Dpad => {
            canvas.rect(cx - 9.0, cy - 24.0, 18.0, 48.0, 4.0, ACCENT);
            canvas.rect(cx - 24.0, cy - 9.0, 48.0, 18.0, 4.0, ACCENT);
            canvas.circle(cx, cy, 4.0, bg);
        }
        Icon::Settings => {
            // Eight teeth around a wheel with a hole in the row's colour.
            for k in 0..8 {
                let a = k as f32 * std::f32::consts::FRAC_PI_4;
                let (x, y) = (cx + 16.0 * a.cos(), cy + 16.0 * a.sin());
                canvas.rect(x - 5.0, y - 5.0, 10.0, 10.0, 2.0, SUBTLE);
            }
            canvas.circle(cx, cy, 15.0, SUBTLE);
            canvas.circle(cx, cy, 6.0, bg);
        }
        Icon::Volume => {
            // Body, a cone widening to the right, and two waves.
            canvas.rect(cx - 24.0, cy - 6.0, 8.0, 12.0, 2.0, ACCENT);
            for i in 0..10 {
                let t = i as f32;
                canvas.rect(
                    cx - 17.0 + t,
                    cy - 6.0 - t,
                    1.5,
                    12.0 + 2.0 * t,
                    0.0,
                    ACCENT,
                );
            }
            canvas.rect(cx + 2.0, cy - 8.0, 3.0, 16.0, 1.5, ACCENT);
            canvas.rect(cx + 10.0, cy - 14.0, 3.0, 28.0, 1.5, ACCENT);
        }
        Icon::Continue => {
            // A bar, then a play triangle.
            canvas.rect(cx - 22.0, cy - 14.0, 5.0, 28.0, 2.0, ACCENT);
            for i in 0..24 {
                let t = i as f32;
                canvas.rect(
                    cx - 10.0 + t,
                    cy - 14.0 + t * 0.58,
                    1.5,
                    28.0 - t * 1.16,
                    0.0,
                    ACCENT,
                );
            }
        }
        Icon::Jump => {
            // An arrow to a bar: the shaft, its head of shortening strokes, the bar.
            canvas.rect(cx - 22.0, cy - 2.5, 28.0, 5.0, 2.0, ACCENT);
            for i in 0..10 {
                let t = i as f32;
                canvas.rect(
                    cx - 4.0 + t,
                    cy - 11.0 + t,
                    1.5,
                    22.0 - 2.0 * t,
                    0.0,
                    ACCENT,
                );
            }
            canvas.rect(cx + 12.0, cy - 14.0, 5.0, 28.0, 2.0, ACCENT);
        }
        Icon::Width => {
            canvas.rect(cx - 20.0, cy - 2.5, 40.0, 5.0, 2.0, ACCENT);
            for i in 0..12 {
                let t = i as f32;
                canvas.rect(
                    cx - 20.0 + t,
                    cy - 12.0 + t,
                    1.5,
                    24.0 - 2.0 * t,
                    0.0,
                    ACCENT,
                );
                canvas.rect(
                    cx + 19.0 - t,
                    cy - 12.0 + t,
                    1.5,
                    24.0 - 2.0 * t,
                    0.0,
                    ACCENT,
                );
            }
        }
        Icon::Exit => {
            // A power symbol: a ring open at the top with a bar through the gap.
            // (Drawn on the header's tool button, so the gap takes its fill.)
            let fill = [0x2a, 0x2f, 0x38];
            canvas.circle(cx, cy + 2.0, 17.0, SUBTLE);
            canvas.circle(cx, cy + 2.0, 11.0, fill);
            canvas.rect(cx - 7.0, cy - 18.0, 14.0, 14.0, 0.0, fill);
            canvas.rect(cx - 3.0, cy - 20.0, 6.0, 22.0, 3.0, SUBTLE);
        }
    }
}

/// Draws relative to a centre, at a scale.
struct Pen<'a> {
    canvas: &'a mut Canvas,
    cx: f32,
    cy: f32,
    scale: f32,
}

impl<'a> Pen<'a> {
    fn new(canvas: &'a mut Canvas, cx: f32, cy: f32, scale: f32) -> Self {
        Self {
            canvas,
            cx,
            cy,
            scale,
        }
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, color: Rgb) {
        let s = self.scale;
        let (x, y) = (self.cx + x * s, self.cy + y * s);
        self.canvas.rect(x, y, w * s, h * s, radius * s, color);
    }

    fn circle(&mut self, x: f32, y: f32, radius: f32, color: Rgb) {
        let s = self.scale;
        let (x, y) = (self.cx + x * s, self.cy + y * s);
        self.canvas.circle(x, y, radius * s, color);
    }
}

/// How well a video plays; grey while that is being checked.
fn verdict_color(verdict: Option<Verdict>) -> Rgb {
    match verdict {
        Some(Verdict::Hardware) => GREEN,
        Some(Verdict::Software) => YELLOW,
        Some(Verdict::SoftwareMarginal) => ORANGE,
        Some(Verdict::Unplayable) => RED,
        None => GREY,
    }
}

/// A trash can: lid with handle, and a body with three slots.
fn draw_trash(canvas: &mut Canvas, cx: f32, cy: f32, color: Rgb) {
    canvas.rect(cx - 6.0, cy - 21.0, 12.0, 5.0, 2.0, color);
    canvas.rect(cx - 17.0, cy - 16.0, 34.0, 5.0, 2.0, color);
    canvas.rect(cx - 13.0, cy - 8.0, 26.0, 29.0, 4.0, color);
    for dx in [-6.0, 0.0, 6.0] {
        canvas.rect(cx + dx - 1.5, cy - 3.0, 3.0, 19.0, 1.5, [0x6a, 0x1d, 0x19]);
    }
}

/// A ticked box in `fill`, or an empty one showing `background` inside.
pub(super) fn draw_checkbox(
    canvas: &mut Canvas,
    cx: f32,
    cy: f32,
    checked: bool,
    fill: Rgb,
    background: Rgb,
) {
    if checked {
        canvas.rect(cx - 18.0, cy - 18.0, 36.0, 36.0, 7.0, fill);
        // A tick from two strokes of small squares.
        for i in 0..8 {
            let t = i as f32;
            canvas.rect(cx - 11.0 + t, cy - 1.0 + t, 5.0, 5.0, 1.0, TEXT);
        }
        for i in 0..14 {
            let t = i as f32;
            canvas.rect(cx - 4.0 + t, cy + 6.0 - t * 1.2, 5.0, 5.0, 1.0, TEXT);
        }
    } else {
        canvas.rect(cx - 18.0, cy - 18.0, 36.0, 36.0, 7.0, SUBTLE);
        canvas.rect(cx - 14.0, cy - 14.0, 28.0, 28.0, 5.0, background);
    }
}

/// The left slot of a tall row: a video's thumbnail (or a dark placeholder)
/// with its mark as a badge in the bottom-left corner, or another entry's
/// icon centred. `slot` is its top-left, `cy` the row's vertical centre.
fn draw_slot(canvas: &mut Canvas, row: &Row, slot: (f32, f32), cy: f32, bg: Rgb) {
    let (sx, sy) = slot;
    let cx = sx + SLOT_W / 2.0;
    if !row.is_video() {
        match row.checked {
            Some(checked) => draw_checkbox(canvas, cx, cy, checked, RED, bg),
            None => draw_icon(canvas, &row.icon, cx, cy, bg),
        }
        return;
    }
    match &row.thumbnail {
        Some(thumb) => {
            // Centred, should it ever differ from the slot's size.
            let x = sx + (SLOT_W - thumb.width as f32) / 2.0;
            let y = sy + (SLOT_H - thumb.height as f32) / 2.0;
            canvas.image(x.round() as i32, y.round() as i32, thumb, SLOT_R);
            // The mark on a dark backing, so it reads over any picture.
            let (bw, bh) = (62.0, 40.0);
            let (bx, by) = (sx + 6.0, sy + SLOT_H - 6.0 - bh);
            canvas.rect(bx, by, bw, bh, 10.0, SLOT_BG);
            let (ix, iy) = (bx + bw / 2.0, by + bh / 2.0);
            draw_icon_scaled(canvas, &row.icon, ix, iy, 0.8, SLOT_BG);
            if let Some(checked) = row.checked {
                // Top-left, on its own backing.
                canvas.rect(sx + 6.0, sy + 6.0, 48.0, 48.0, 10.0, SLOT_BG);
                draw_checkbox(canvas, sx + 30.0, sy + 30.0, checked, RED, SLOT_BG);
            }
        }
        None => {
            canvas.rect(sx, sy, SLOT_W, SLOT_H, SLOT_R, SLOT_BG);
            match row.checked {
                Some(checked) => draw_checkbox(canvas, cx, cy, checked, RED, SLOT_BG),
                None => draw_icon(canvas, &row.icon, cx, cy, SLOT_BG),
            }
        }
    }
}

/// Renders the panel; `pointer` highlights what it hovers and, with
/// `draw_cursor`, marks its position (previews; the headset has a cursor layer).
pub fn render(
    view: &View,
    fonts: &mut Fonts,
    pointer: Option<(f32, f32)>,
    draw_cursor: bool,
) -> Canvas {
    render_into(
        Canvas::new(WIDTH, HEIGHT),
        view,
        fonts,
        pointer,
        draw_cursor,
    )
}

/// [`render`] over a canvas from an earlier render (any content), which saves
/// allocating and faulting in 6 MB for every repaint.
pub fn render_into(
    mut canvas: Canvas,
    view: &View,
    fonts: &mut Fonts,
    pointer: Option<(f32, f32)>,
    draw_cursor: bool,
) -> Canvas {
    debug_assert_eq!((canvas.width, canvas.height), (WIDTH, HEIGHT));
    canvas.clear(BG);
    let w = WIDTH as f32;
    let crumbs = crumb_layout(view, fonts);
    let hover = pointer.map(|(x, y)| hit(view, fonts, x, y));
    let bottom = HEIGHT as f32 - BOTTOM;

    if let Some(f) = &view.form {
        let form_hover = match hover {
            Some(Hit::Form(h)) => h,
            _ => form::Hit::Nothing,
        };
        fonts.draw(&mut canvas, &f.title, PAD, 78.0, 46.0, TEXT, w - 2.0 * PAD);
        form::render(&mut canvas, fonts, f, form_hover);
    } else if let Some(status) = &view.status {
        let lines = fonts.wrap(status, 36.0, w - 4.0 * PAD);
        for (i, line) in lines.iter().enumerate() {
            fonts.draw(
                &mut canvas,
                line,
                2.0 * PAD,
                HEADER + 90.0 + i as f32 * 52.0,
                36.0,
                SUBTLE,
                w - 4.0 * PAD,
            );
        }
    } else {
        let first = view.scroll.floor() as usize;
        for (i, row) in view.rows.iter().enumerate().skip(first) {
            let (rx, ry, rw, rh) = row_rect(view, i);
            if ry > bottom {
                break;
            }
            // A dimmed row opens nothing, so it only lights up while selecting.
            let hovered = matches!(hover, Some(Hit::Row(r)) if r == i)
                && (!row.dimmed || row.checked.is_some());
            let label_color = if row.dimmed { FAINT } else { TEXT };
            let detail_color = if row.dimmed { FAINT } else { SUBTLE };
            if row.outlined {
                canvas.rect(rx - 3.0, ry - 3.0, rw + 6.0, rh + 6.0, 17.0, ACCENT);
            }
            let bg = if hovered { HOVER } else { ROW_BG };
            canvas.rect(rx, ry, rw, rh, 14.0, bg);
            let cy = ry + rh / 2.0;
            let text_x = if view.thumbnails {
                let slot = (rx + 4.0, ry + (rh - SLOT_H) / 2.0);
                draw_slot(&mut canvas, row, slot, cy, bg);
                slot.0 + SLOT_W + 24.0
            } else {
                let icon_x = rx + 48.0;
                match row.checked {
                    Some(checked) => draw_checkbox(&mut canvas, icon_x, cy, checked, RED, ROW_BG),
                    None => draw_icon(&mut canvas, &row.icon, icon_x, cy, bg),
                }
                rx + 96.0
            };
            let mut right_edge = rx + rw - 24.0;
            for (k, action) in row.actions.iter().enumerate() {
                let (ax, ay, aw, ah) = action_rect(view, i, k);
                let color = if *action == Action::Remove {
                    RED
                } else {
                    ACCENT
                };
                let hovered = hover == Some(Hit::RowAction(i, *action));
                let fill = if hovered {
                    color
                } else {
                    [color[0] / 3, color[1] / 3, color[2] / 3]
                };
                canvas.rect(ax, ay, aw, ah, 12.0, fill);
                if action.is_icon() {
                    draw_trash(&mut canvas, ax + aw / 2.0, ay + ah / 2.0, TEXT);
                } else {
                    let label = action.label();
                    let lw = fonts.measure(label, 30.0);
                    fonts.draw(
                        &mut canvas,
                        label,
                        ax + (aw - lw) / 2.0,
                        ay + ah / 2.0 + 11.0,
                        30.0,
                        TEXT,
                        aw,
                    );
                }
                if k == 0 {
                    right_edge = ax - 16.0;
                }
            }
            let has_actions = !row.actions.is_empty();
            let right_w = if row.right.is_empty() || has_actions {
                0.0
            } else {
                fonts.measure(&row.right, 28.0) + 24.0
            };
            let text_w = right_edge - right_w - text_x;
            if row.detail.is_empty() {
                fonts.draw(
                    &mut canvas,
                    &row.label,
                    text_x,
                    cy + 12.0,
                    36.0,
                    label_color,
                    text_w,
                );
            } else {
                fonts.draw(
                    &mut canvas,
                    &row.label,
                    text_x,
                    cy - 2.0,
                    34.0,
                    label_color,
                    text_w,
                );
                fonts.draw(
                    &mut canvas,
                    &row.detail,
                    text_x,
                    cy + 30.0,
                    24.0,
                    detail_color,
                    text_w,
                );
            }
            if right_w > 0.0 {
                fonts.draw(
                    &mut canvas,
                    &row.right,
                    right_edge - right_w + 12.0,
                    cy + 12.0,
                    28.0,
                    detail_color,
                    right_w,
                );
            }
            // Where a video was left: a line along the row's foot.
            if let Some(progress) = row.progress {
                let (px, pw) = (text_x, right_edge - text_x);
                canvas.rect(px, ry + rh - 5.0, pw, 3.0, 1.5, [0x3a, 0x42, 0x50]);
                canvas.rect(
                    px,
                    ry + rh - 5.0,
                    pw * progress.clamp(0.0, 1.0),
                    3.0,
                    1.5,
                    ACCENT,
                );
            }
        }
        // Cover rows that scrolled under the header or off the bottom.
        canvas.rect(0.0, 0.0, w, HEADER - 8.0, 0.0, BG);
        canvas.rect(0.0, bottom, w, BOTTOM, 0.0, BG);
        let max = view.max_scroll();
        if max > 0.0 {
            let (track_top, track) = scroll_track();
            let thumb = thumb_height(view);
            let top = track_top + view.scroll / max * (track - thumb);
            let hovered = hover == Some(Hit::ScrollBar);
            let (bar_w, color) = if hovered {
                (18.0, TEXT)
            } else {
                (10.0, SUBTLE)
            };
            let x = w - PAD - SCROLL_W / 2.0 + 4.0 - bar_w / 2.0;
            canvas.rect(x, track_top, bar_w, track, bar_w / 2.0, [0x2a, 0x2f, 0x38]);
            canvas.rect(x, top, bar_w, thumb, bar_w / 2.0, color);
        }
    }

    // Header: breadcrumbs (links except the last) and the build number.
    if view.form.is_none() {
        for (k, tool) in view.tools.iter().enumerate() {
            let (x, y, tw, th) = tool_rect(view, k);
            let hovered = hover == Some(Hit::Tool(k));
            let fill = match (tool.danger, hovered) {
                (true, true) => [0xff, 0x6b, 0x60],
                (true, false) => RED,
                (false, true) => HOVER,
                (false, false) => [0x2a, 0x2f, 0x38],
            };
            canvas.rect(x, y, tw, th, 14.0, fill);
            let (cx, cy) = (x + tw / 2.0, y + th / 2.0);
            if let Some(icon) = &tool.icon {
                draw_icon(&mut canvas, icon, cx, cy, fill);
                continue;
            }
            let lw = fonts.measure(&tool.label, 30.0);
            fonts.draw(
                &mut canvas,
                &tool.label,
                cx - lw / 2.0,
                cy + 11.0,
                30.0,
                TEXT,
                tw,
            );
        }
        let limit = crumb_limit(view, fonts);
        let last = view.crumbs.len().saturating_sub(1);
        if let Some(heading) = &view.heading {
            // The selection's tools need the header: the crumbs make way.
            let room = WIDTH as f32 - 2.0 * PAD - tools_width(view) - 24.0;
            fonts.draw(&mut canvas, heading, PAD, 78.0, CRUMB_SIZE, TEXT, room);
        }
        let elements = if view.heading.is_none() {
            crumbs.elements(view)
        } else {
            Vec::new()
        };
        for (k, (text, (x0, x1), i)) in elements.iter().enumerate() {
            let (x0, x1, i) = (*x0, *x1, *i);
            let hovered = hover == Some(Hit::Crumb(i));
            if hovered {
                canvas.rect(x0 - 10.0, 34.0, x1 - x0 + 20.0, 60.0, 10.0, HOVER);
            }
            let color = if i == last || hovered { TEXT } else { SUBTLE };
            fonts.draw(&mut canvas, text, x0, 78.0, CRUMB_SIZE, color, limit);
            if k + 1 < elements.len() {
                fonts.draw(&mut canvas, CRUMB_SEP, x1, 78.0, CRUMB_SIZE, FAINT, 200.0);
            }
        }
    }
    // Above the tools, so both fit.
    let build_w = fonts.measure(BUILD, 20.0);
    let build_y = if view.tools.is_empty() { 44.0 } else { 26.0 };
    fonts.draw(
        &mut canvas,
        BUILD,
        w - PAD - build_w,
        build_y,
        20.0,
        FAINT,
        build_w + 2.0,
    );
    if view.form.is_none() {
        canvas.rect(
            PAD,
            HEADER - 8.0,
            w - 2.0 * PAD,
            2.0,
            0.0,
            [0x2a, 0x2f, 0x38],
        );
    }
    if let Some(notice) = &view.notice {
        canvas.rect(0.0, HEIGHT as f32 - 60.0, w, 60.0, 0.0, BG);
        fonts.draw(
            &mut canvas,
            notice,
            PAD,
            HEIGHT as f32 - 22.0,
            26.0,
            SUBTLE,
            w - 2.0 * PAD,
        );
    }

    if let Some(dialog) = &view.dialog {
        // Dim the list behind the dialog.
        for px in canvas.pixels.chunks_exact_mut(4) {
            for c in &mut px[..3] {
                *c /= 3;
            }
        }
        let (dx, dy, dw, dh) = (160.0, 120.0, w - 320.0, HEIGHT as f32 - 240.0);
        canvas.rect(dx, dy, dw, dh, 24.0, [0x22, 0x26, 0x2e]);
        fonts.draw(
            &mut canvas,
            &dialog.title,
            dx + 48.0,
            dy + 84.0,
            42.0,
            TEXT,
            dw - 96.0,
        );
        let mut y = dy + 150.0;
        for paragraph in &dialog.body {
            for line in fonts.wrap(paragraph, 30.0, dw - 96.0) {
                fonts.draw(&mut canvas, &line, dx + 48.0, y, 30.0, SUBTLE, dw - 96.0);
                y += 44.0;
            }
            y += 16.0;
        }
        let rects = dialog_buttons(dialog.buttons.len());
        for (i, (label, rect)) in dialog.buttons.iter().zip(&rects).enumerate() {
            let primary = match dialog.choice {
                Some(chosen) => chosen == i,
                None => i + 1 == dialog.buttons.len(),
            };
            let hovered = hover == Some(Hit::DialogButton(i));
            let color = match (primary, dialog.danger, hovered) {
                (true, true, true) => [0xff, 0x6b, 0x60],
                (true, true, false) => RED,
                (true, false, true) => [0x6b, 0xa0, 0xff],
                (true, false, false) => ACCENT,
                (false, _, true) => HOVER,
                (false, _, false) => [0x2a, 0x2f, 0x38],
            };
            canvas.rect(rect.0, rect.1, rect.2, rect.3, 16.0, color);
            let label_w = fonts.measure(label, 34.0);
            fonts.draw(
                &mut canvas,
                label,
                rect.0 + (rect.2 - label_w) / 2.0,
                rect.1 + 53.0,
                34.0,
                TEXT,
                rect.2,
            );
        }
    }

    if let Some((x, y)) = pointer.filter(|_| draw_cursor) {
        canvas.circle(x, y, 12.0, [0xff, 0xff, 0xff]);
        canvas.circle(x, y, 8.0, ACCENT);
    }
    canvas
}

/// Human-readable size, e.g. "4.7 GB".
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> View {
        View {
            crumbs: vec!["Just Video".into(), "PC".into(), "media".into()],
            rows: (0..30)
                .map(|i| Row::new(Icon::Folder, format!("{i}")))
                .collect(),
            ..Default::default()
        }
    }

    /// `cargo test --release -- --ignored --nocapture render_time`: how long
    /// one repaint of a folder of videos with thumbnails takes.
    #[test]
    #[ignore]
    fn render_time() {
        let thumb = Arc::new(Thumb {
            width: 220,
            height: 124,
            rgba: vec![90; 220 * 124 * 4],
        });
        let mut view = View {
            crumbs: vec!["Just Video".into(), "PC".into(), "media".into()],
            rows: (0..30)
                .map(|i| {
                    let mut row =
                        Row::new(Icon::Video(None), format!("Some video file name {i}.mp4"));
                    row.detail = "VR180 3D · 6144x3072 HEVC · 12 GB".into();
                    row.right = "1:23:45".into();
                    row.actions = vec![Action::Settings, Action::Remove];
                    row.thumbnail = Some(thumb.clone());
                    row
                })
                .collect(),
            thumbnails: true,
            ..Default::default()
        };
        let mut fonts = Fonts::load().expect("fonts");
        // As the app does: each repaint reuses the last canvas.
        let mut canvas = render(&view, &mut fonts, Some((400.0, 300.0)), false);
        let started = std::time::Instant::now();
        let n = 50;
        for i in 0..n {
            view.scroll = i as f32 * 0.1;
            canvas = render_into(canvas, &view, &mut fonts, Some((400.0, 300.0)), false);
        }
        println!(
            "browser repaint: {:.2} ms",
            started.elapsed().as_secs_f64() * 1e3 / n as f64
        );
        let started = std::time::Instant::now();
        for _ in 0..n {
            let mut c = Canvas::new(WIDTH, HEIGHT);
            c.clear(BG);
            std::hint::black_box(&c);
        }
        println!(
            "  of which a new cleared canvas: {:.2} ms",
            started.elapsed().as_secs_f64() * 1e3 / n as f64
        );
    }

    #[test]
    fn hit_testing_follows_scroll_and_dialog() {
        let mut view = view();
        let mut fonts = Fonts::load().expect("fonts");
        let spans = crumb_layout(&view, &mut fonts).spans;
        let (a, b) = spans[2].expect("shown");
        let last = (a + b) / 2.0;
        assert_eq!(hit(&view, &mut fonts, 400.0, HEADER + 10.0), Hit::Row(0));
        view.scroll = 5.0;
        assert_eq!(
            hit(&view, &mut fonts, 400.0, HEADER + view.row_h() + 10.0),
            Hit::Row(6)
        );
        assert_eq!(hit(&view, &mut fonts, 100.0, 70.0), Hit::Crumb(0));
        assert_eq!(
            hit(&view, &mut fonts, last, 70.0),
            Hit::Nothing,
            "current place is not a link"
        );
        view.dialog = Some(Dialog {
            title: String::new(),
            body: vec![],
            buttons: vec!["Cancel".into(), "Delete".into()],
            danger: true,
            choice: None,
        });
        let rects = dialog_buttons(2);
        let (x, y, w, h) = rects[1];
        assert_eq!(
            hit(&view, &mut fonts, x + w / 2.0, y + h / 2.0),
            Hit::DialogButton(1)
        );
        assert_eq!(hit(&view, &mut fonts, 400.0, HEADER + 10.0), Hit::Nothing);
    }

    #[test]
    fn deep_paths_fold_before_the_tools() {
        let deep = View {
            crumbs: [
                "Just Video",
                "NAS",
                "media",
                "Series",
                "Show.S01.1080p.WEBRip.x265-RARBG[eztv.re]",
                "Subs",
                "Show.S01E01.1080p.WEBRip.x265-RARBG[eztv.re]",
            ]
            .map(String::from)
            .to_vec(),
            tools: vec![Tool::text("Cancel", false), Tool::text("Move here", false)],
            ..Default::default()
        };
        let mut fonts = Fonts::load().expect("fonts");
        let layout = crumb_layout(&deep, &mut fonts);
        let edge = tool_rect(&deep, 0).0 - 24.0;
        let ((ex0, ex1), target) = layout.ellipsis.expect("folded");
        assert!(ex1 <= edge);
        for (a, b) in layout.spans.iter().flatten() {
            assert!(*b <= edge, "{a}..{b} crosses the tools at {edge}");
            assert!(*a >= PAD);
        }
        assert!(layout.spans[6].is_some(), "the current place always shows");
        assert!(layout.spans[0].is_some(), "the root fits");
        assert!(layout.spans[target].is_none());
        assert_eq!(
            hit(&deep, &mut fonts, (ex0 + ex1) / 2.0, 70.0),
            Hit::Crumb(target),
            "the ellipsis leads to the deepest folded crumb"
        );
        let (a, b) = layout.spans[6].unwrap();
        assert_eq!(hit(&deep, &mut fonts, (a + b) / 2.0, 70.0), Hit::Nothing);
        // Short paths stay as they were.
        let short = view();
        let layout = crumb_layout(&short, &mut fonts);
        assert!(layout.ellipsis.is_none());
        assert!(layout.spans.iter().all(Option::is_some));
    }

    #[test]
    fn scrollbar_is_not_a_row() {
        let view = view();
        let x = WIDTH as f32 - PAD - SCROLL_W / 2.0;
        let mut fonts = Fonts::load().expect("fonts");
        assert_eq!(hit(&view, &mut fonts, x, HEADER + 10.0), Hit::ScrollBar);
        assert_eq!(scroll_at(&view, 0.0), 0.0);
        assert_eq!(scroll_at(&view, HEIGHT as f32), view.max_scroll());
        let short = View {
            rows: view.rows[..3].to_vec(),
            ..view.clone()
        };
        assert_eq!(hit(&short, &mut fonts, x, HEADER + 10.0), Hit::Nothing);
    }

    #[test]
    fn row_actions_and_tools() {
        let mut view = view();
        let mut fonts = Fonts::load().expect("fonts");
        view.rows[2].actions = vec![Action::Settings, Action::Remove];
        for (k, action) in [Action::Settings, Action::Remove].into_iter().enumerate() {
            let (ax, ay, aw, ah) = action_rect(&view, 2, k);
            assert_eq!(
                hit(&view, &mut fonts, ax + aw / 2.0, ay + ah / 2.0),
                Hit::RowAction(2, action)
            );
        }
        let (ax, ay, aw, ah) = action_rect(&view, 2, 0);
        assert_eq!(
            hit(
                &view,
                &mut fonts,
                ax + aw / 2.0,
                ay + ah / 2.0 + view.row_h()
            ),
            Hit::Row(3),
            "no actions on row 3"
        );
        view.tools = vec![
            Tool::text("Cancel", false),
            Tool::text("Delete 2", true),
            Tool::text("Move 2", false),
        ];
        for k in 0..3 {
            let (tx, ty, tw, th) = tool_rect(&view, k);
            assert_eq!(
                hit(&view, &mut fonts, tx + tw / 2.0, ty + th / 2.0),
                Hit::Tool(k)
            );
        }
        let (tx, _, tw, _) = tool_rect(&view, 2);
        assert!(tx + tw <= WIDTH as f32 - PAD + 0.5);
        view.heading = Some("2 selected".into());
        assert_eq!(
            hit(&view, &mut fonts, 100.0, 70.0),
            Hit::Nothing,
            "no crumbs under a heading"
        );
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(5_097_390_883), "5.1 GB");
    }

    fn tall() -> View {
        View {
            thumbnails: true,
            ..view()
        }
    }

    fn pattern() -> Arc<Thumb> {
        let (w, h) = (SLOT_W as u32, SLOT_H as u32);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[200, (x % 256) as u8, (y * 2) as u8, 255]);
            }
        }
        Arc::new(Thumb {
            width: w,
            height: h,
            rgba,
        })
    }

    fn pixel(canvas: &Canvas, x: f32, y: f32) -> Rgb {
        let i = ((y as u32 * canvas.width + x as u32) * 4) as usize;
        [canvas.pixels[i], canvas.pixels[i + 1], canvas.pixels[i + 2]]
    }

    #[test]
    fn row_geometry_follows_the_row_height() {
        let (plain, tall) = (view(), tall());
        assert_eq!((plain.row_h(), tall.row_h()), (ROW, ROW_THUMB));
        assert!((plain.visible_rows() - 856.0 / 88.0).abs() < 1e-3);
        assert!((tall.visible_rows() - 856.0 / 140.0).abs() < 1e-3);
        assert!((tall.max_scroll() - (30.0 - tall.visible_rows())).abs() < 1e-3);
        assert_eq!(rows_for_drag(&plain, 176.0), 2.0);
        assert_eq!(rows_for_drag(&tall, 280.0), 2.0);
        let mut fonts = Fonts::load().expect("fonts");
        for mut v in [plain, tall] {
            v.scroll = 4.5;
            let h = v.row_h();
            // Row 4 shows its lower half at the top; row 5 follows it.
            assert_eq!(hit(&v, &mut fonts, 400.0, HEADER + 1.0), Hit::Row(4));
            assert_eq!(
                hit(&v, &mut fonts, 400.0, HEADER + h / 2.0 + 1.0),
                Hit::Row(5)
            );
            let (_, y, _, rh) = row_rect(&v, 6);
            assert_eq!(y, HEADER + 1.5 * h + 4.0);
            assert_eq!(rh, h - 8.0);
            assert_eq!(hit(&v, &mut fonts, 400.0, y + rh / 2.0), Hit::Row(6));
            let thumb = thumb_height(&v);
            assert!(thumb >= 60.0 && thumb < scroll_track().1);
            assert_eq!(scroll_at(&v, HEIGHT as f32), v.max_scroll());
        }
    }

    #[test]
    fn a_thumbnail_fills_the_slot_with_the_mark_in_the_corner() {
        let mut fonts = Fonts::load().expect("fonts");
        let mut view = View {
            rows: vec![
                Row {
                    thumbnail: Some(pattern()),
                    ..Row::new(Icon::Video(Some(Verdict::Hardware)), "a.mp4")
                },
                Row::new(Icon::Video(Some(Verdict::Hardware)), "b.mp4"),
                Row::new(Icon::Folder, "dir"),
            ],
            ..tall()
        };
        let canvas = render(&view, &mut fonts, None, false);
        let (rx, ry, _, rh) = row_rect(&view, 0);
        let (sx, sy) = (rx + 4.0, ry + (rh - SLOT_H) / 2.0);
        // Interior: the thumbnail's own pixels, copied straight.
        assert_eq!(pixel(&canvas, sx + 150.0, sy + 20.0), [200, 150, 40]);
        // Rounded: the corner pixel shows the row behind, the next one in does not.
        assert_eq!(pixel(&canvas, sx, sy), ROW_BG);
        assert_eq!(pixel(&canvas, sx + 20.0, sy), [200, 20, 0]);
        // The badge: dark backing with the green mark, bottom left.
        let (bx, by) = (sx + 6.0 + 31.0, sy + SLOT_H - 6.0 - 20.0);
        assert_eq!(pixel(&canvas, bx, by), GREEN);
        assert_eq!(pixel(&canvas, sx + 8.0, by), SLOT_BG);
        // No picture yet: a placeholder with the mark centred; a folder has none.
        let (_, ry1, _, rh1) = row_rect(&view, 1);
        let (cx, cy) = (sx + SLOT_W / 2.0, ry1 + rh1 / 2.0);
        assert_eq!(pixel(&canvas, cx, cy), GREEN);
        assert_eq!(pixel(&canvas, sx + 12.0, ry1 + 20.0), SLOT_BG);
        let (_, ry2, _, rh2) = row_rect(&view, 2);
        assert_eq!(pixel(&canvas, sx + 12.0, ry2 + 12.0), ROW_BG, "no slot");
        assert_ne!(pixel(&canvas, cx, ry2 + rh2 / 2.0), ROW_BG, "its icon");
        // Plain rows are untouched by any of this.
        view.thumbnails = false;
        let plain = render(&view, &mut fonts, None, false);
        let (_, py, _, ph) = row_rect(&view, 0);
        assert_eq!(pixel(&plain, PAD + 48.0, py + ph / 2.0), GREEN);
    }

    #[test]
    fn full_screens_of_thumbnails_redraw_quickly() {
        let mut fonts = Fonts::load().expect("fonts");
        let rows = |thumb: bool| -> Vec<Row> {
            (0..30)
                .map(|i| Row {
                    detail: "VR180 3D  ·  Plays with hardware decoding".into(),
                    right: "3.3 GB".into(),
                    thumbnail: thumb.then(pattern),
                    ..Row::new(
                        Icon::VideoVr(Some(Verdict::Hardware)),
                        format!("clip {i}.mp4"),
                    )
                })
                .collect()
        };
        let time = |view: &View, fonts: &mut Fonts| {
            render(view, fonts, None, false);
            let start = std::time::Instant::now();
            for _ in 0..3 {
                std::hint::black_box(render(view, fonts, None, false));
            }
            start.elapsed().as_secs_f64() * 1e3 / 3.0
        };
        let plain = View {
            rows: rows(false),
            ..Default::default()
        };
        let empty = View {
            rows: rows(false),
            thumbnails: true,
            ..Default::default()
        };
        let full = View {
            rows: rows(true),
            thumbnails: true,
            ..Default::default()
        };
        let (a, b, c) = (
            time(&plain, &mut fonts),
            time(&empty, &mut fonts),
            time(&full, &mut fonts),
        );
        let mut canvas = Canvas::new(WIDTH, HEIGHT);
        let image = pattern();
        let start = std::time::Instant::now();
        for _ in 0..200 {
            canvas.image(36, 130, &image, SLOT_R);
        }
        let blit = start.elapsed().as_secs_f64() * 1e3 / 200.0;
        eprintln!(
            "redraw: {a:.1} ms plain, {b:.1} ms tall with placeholders, {c:.1} ms with 7 thumbnails; one thumbnail blit {blit:.3} ms"
        );
        // Only a sanity bound (debug builds, shared machines): blitting is cheap.
        assert!(blit < 1.0, "{blit} ms per thumbnail");
        assert!(c < a * 3.0 + 20.0, "{c} ms vs {a} ms");
    }
}
