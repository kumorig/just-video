//! The OpenXR frame loop for the whole app: a browser panel, video playback
//! with a control bar, and controller input (point, click, drag).

use super::context::{VIEW_TYPE, XrContext};
use super::input::{Input, InputState, Ray};
use super::player::{
    Placement, PlayOptions, PlayStats, Playback, ViewOptions, eye_params, screen_size,
};
use super::renderer::{QuadTarget, Renderer};
use crate::ui::canvas::Fonts;
use crate::ui::navigator::Navigator;
use crate::ui::{browser, captions, controls, form};
use crate::vr::Projection;
use anyhow::Context;
use openxr as xr;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

const CURSOR_PX: u32 = 64;
const CURSOR_SIZE: f32 = 0.035;

pub struct AppOptions {
    pub view: ViewOptions,
    pub play: PlayOptions,
    pub quit: Arc<AtomicBool>,
}

/// A flat UI panel floating in the LOCAL space.
#[derive(Clone, Copy)]
struct Panel {
    center: [f32; 3],
    /// Turn around +Y (radians), then tilt around the panel's X axis.
    yaw: f32,
    tilt: f32,
    size: [f32; 2],
    pixels: [u32; 2],
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

impl Panel {
    fn orientation(&self) -> xr::Quaternionf {
        let (sy, cy) = (self.yaw / 2.0).sin_cos();
        let (sx, cx) = (self.tilt / 2.0).sin_cos();
        // q = q_yaw · q_tilt
        xr::Quaternionf {
            x: cy * sx,
            y: sy * cx,
            z: -sy * sx,
            w: cy * cx,
        }
    }

    /// Unit vectors: right, up, normal (towards the viewer).
    fn basis(&self) -> [[f32; 3]; 3] {
        let (sy, cy) = self.yaw.sin_cos();
        let (st, ct) = self.tilt.sin_cos();
        let right = [cy, 0.0, -sy];
        let up = [sy * st, ct, cy * st];
        let normal = [sy * ct, -st, cy * ct];
        [right, up, normal]
    }

    /// Canvas pixel hit by `ray`, if any.
    fn hit(&self, ray: &Ray) -> Option<(f32, f32)> {
        let [right, up, normal] = self.basis();
        let denom = dot(ray.direction, normal);
        if denom.abs() < 1e-4 {
            return None;
        }
        let to_center = [
            self.center[0] - ray.origin[0],
            self.center[1] - ray.origin[1],
            self.center[2] - ray.origin[2],
        ];
        let t = dot(to_center, normal) / denom;
        if t <= 0.0 {
            return None;
        }
        let p = add(ray.origin, scale(ray.direction, t));
        let local = [
            p[0] - self.center[0],
            p[1] - self.center[1],
            p[2] - self.center[2],
        ];
        let u = dot(local, right) / self.size[0] + 0.5;
        let v = 0.5 - dot(local, up) / self.size[1];
        ((0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v))
            .then_some((u * self.pixels[0] as f32, v * self.pixels[1] as f32))
    }

    /// World position of canvas pixel (x, y), `lift` metres towards the viewer.
    fn point(&self, x: f32, y: f32, lift: f32) -> [f32; 3] {
        let [right, up, normal] = self.basis();
        let u = (x / self.pixels[0] as f32 - 0.5) * self.size[0];
        let v = (0.5 - y / self.pixels[1] as f32) * self.size[1];
        add(
            add(add(self.center, scale(right, u)), scale(up, v)),
            scale(normal, lift),
        )
    }

    fn pose(&self) -> xr::Posef {
        xr::Posef {
            orientation: self.orientation(),
            position: xr::Vector3f {
                x: self.center[0],
                y: self.center[1],
                z: self.center[2],
            },
        }
    }
}

/// The browser starts 2.2 m ahead: far enough that pointing needs only
/// small hand movements, and scaled to look as big as 1.6 × 1.0 m at 1.5 m.
const BROWSER_DISTANCE: f32 = 2.2;
const BROWSER_SIZE: [f32; 2] = [2.347, 1.467];

/// The curvature preview: a 16:9 screen this far ahead, this much wider
/// than the browser looks, so its bent edges show around the panel.
const CURVE_PREVIEW_DISTANCE: f32 = 3.0;
const CURVE_PREVIEW_SCALE: f32 = 1.4;

/// Where the browser starts: a little below eye level.
fn browser_placement(pivot: [f32; 3]) -> Placement {
    Placement {
        yaw: 0.0,
        pitch: (-0.147f32).atan2(BROWSER_DISTANCE),
        distance: BROWSER_DISTANCE,
        curved: false,
        curve: 1.0,
        zoom: 1.0,
        offset: 0.0,
        pivot,
    }
}

/// The browser panel where it has been dragged, facing its pivot and
/// keeping its apparent size at any distance; `width` is the chosen
/// multiple of its usual size.
fn browser_panel(place: &Placement, width: f32) -> Panel {
    let s = place.distance / BROWSER_DISTANCE * width;
    Panel {
        center: place.center(),
        yaw: place.yaw,
        tilt: place.pitch,
        size: [BROWSER_SIZE[0] * s, BROWSER_SIZE[1] * s],
        pixels: [browser::WIDTH, browser::HEIGHT],
    }
}

/// The handle below a panel (a white bar, as under the Steam dashboard):
/// point at it and hold select to move the panel. Or above it, for a panel
/// hanging so low that below would be out of sight.
fn handle_panel(panel: &Panel, above: bool) -> Panel {
    let width = panel.size[0] * 0.22;
    let height = width * HANDLE_PX[1] as f32 / HANDLE_PX[0] as f32;
    let [_, up, _] = panel.basis();
    let gap = (panel.size[1] * 0.035).max(panel.size[0] * 0.012);
    let drop = panel.size[1] / 2.0 + gap + height / 2.0;
    let drop = if above { -drop } else { drop };
    Panel {
        center: [0, 1, 2].map(|i| panel.center[i] - up[i] * drop),
        size: [width, height],
        pixels: HANDLE_PX,
        ..*panel
    }
}

const HANDLE_PX: [u32; 2] = [256, 32];

/// A white pill with premultiplied alpha, for the browser's handle.
fn handle_image(opacity: f32) -> Vec<u8> {
    let mut c = crate::ui::canvas::Canvas::new(HANDLE_PX[0], HANDLE_PX[1]);
    c.translucent_rect(
        0.0,
        0.0,
        HANDLE_PX[0] as f32,
        HANDLE_PX[1] as f32,
        HANDLE_PX[1] as f32 / 2.0,
        [0xff, 0xff, 0xff],
        opacity,
    );
    c.pixels
}

/// Preferred distance of the control bar; it looks as big as 1.2 m wide at 1 m.
const CONTROLS_DISTANCE: f32 = 1.8;

/// Where the VR180/360 control bar starts: [`CONTROLS_DISTANCE`] ahead of
/// the head, this far below eye level (radians), orbiting the head when
/// dragged by its handle, like the browser.
const VR_BAR_PITCH: f32 = -0.4;
/// Hanging lower than this, the bar's handle goes above it, where it can
/// still be seen.
const LOW_BAR_PITCH: f32 = -0.6;

fn vr_bar_placement(head: &xr::Posef) -> Placement {
    let q = head.orientation;
    // Forward (-Z) of the head, flattened to the horizon.
    let forward = [
        -2.0 * (q.x * q.z + q.w * q.y),
        0.0,
        -(1.0 - 2.0 * (q.x * q.x + q.y * q.y)),
    ];
    Placement {
        yaw: (-forward[0]).atan2(-forward[2]),
        pitch: VR_BAR_PITCH,
        distance: CONTROLS_DISTANCE / VR_BAR_PITCH.cos(),
        curved: false,
        curve: 1.0,
        zoom: 1.0,
        offset: 0.0,
        pivot: [head.position.x, head.position.y, head.position.z],
    }
}

/// The VR180/360 control bar where it has been dragged, facing the head
/// and keeping its apparent size at any distance.
fn vr_bar_panel(place: &Placement) -> Panel {
    let w = 1.2 * place.distance * VR_BAR_PITCH.cos();
    Panel {
        center: place.center(),
        yaw: place.yaw,
        tilt: place.pitch,
        size: [w, w * controls::HEIGHT as f32 / controls::WIDTH as f32],
        pixels: [controls::WIDTH, controls::HEIGHT],
    }
}

/// The control bar under a flat or curved screen, so the eyes needn't refocus
/// between the two: just below its bottom edge, at the screen's depth, tilted
/// to face the viewer. As wide as the screen, within a comfortable range of
/// apparent sizes, and it follows the screen when moved or zoomed.
fn screen_controls_panel(placement: &Placement, screen: [f32; 2]) -> Panel {
    let d = placement.distance;
    // A curved screen's arc is wider than the straight line across it.
    let across = if placement.curved {
        let r = placement.curve_radius();
        2.0 * r * (screen[0] / (2.0 * r)).min(std::f32::consts::PI).sin()
    } else {
        screen[0]
    };
    let width = across.clamp(0.8 * d, 1.2 * d);
    let height = width * controls::HEIGHT as f32 / controls::WIDTH as f32;
    let y = -screen[1] / 2.0 - 0.03 * width - height / 2.0;
    // A little in front of the screen, which is drawn behind every panel anyway.
    let depth = d - 0.02;
    let base = Panel {
        center: [0.0; 3],
        yaw: placement.yaw,
        tilt: placement.pitch,
        size: [width, height],
        pixels: [controls::WIDTH, controls::HEIGHT],
    };
    let [_, up, normal] = base.basis();
    Panel {
        center: [0, 1, 2].map(|i| placement.pivot[i] + up[i] * y - normal[i] * depth),
        // Turned up (down above eye level) to face the viewer.
        tilt: placement.pitch + y.atan2(depth),
        ..base
    }
}

/// The dialog panel: above the control bar, in the same plane, as wide.
fn dialog_panel(bar: &Panel) -> Panel {
    let size = [
        bar.size[0],
        bar.size[0] * controls::DIALOG_HEIGHT as f32 / controls::DIALOG_WIDTH as f32,
    ];
    let [_, up, _] = bar.basis();
    let lift = bar.size[1] / 2.0 + bar.size[0] * 0.02 + size[1] / 2.0;
    Panel {
        center: [0, 1, 2].map(|i| bar.center[i] + up[i] * lift),
        size,
        pixels: [controls::DIALOG_WIDTH, controls::DIALOG_HEIGHT],
        ..*bar
    }
}

/// A white-ringed blue dot with premultiplied alpha.
fn cursor_image() -> Vec<u8> {
    let mut pixels = vec![0u8; (CURSOR_PX * CURSOR_PX * 4) as usize];
    let c = CURSOR_PX as f32 / 2.0;
    for y in 0..CURSOR_PX {
        for x in 0..CURSOR_PX {
            let d = ((x as f32 + 0.5 - c).powi(2) + (y as f32 + 0.5 - c).powi(2)).sqrt();
            let outer = (c - 2.0 - d).clamp(0.0, 1.0);
            let inner = (c - 10.0 - d).clamp(0.0, 1.0);
            let rgb = [
                255.0 * (1.0 - inner) + 79.0 * inner,
                255.0 * (1.0 - inner) + 140.0 * inner,
                255.0,
            ];
            let i = ((y * CURSOR_PX + x) * 4) as usize;
            for (k, channel) in rgb.iter().enumerate() {
                pixels[i + k] = (channel * outer) as u8;
            }
            pixels[i + 3] = (255.0 * outer) as u8;
        }
    }
    pixels
}

fn quad_layer<'a>(
    space: &'a xr::Space,
    target: &'a QuadTarget,
    pose: xr::Posef,
    size: [f32; 2],
    blend: bool,
) -> xr::CompositionLayerQuad<'a, xr::Vulkan> {
    let layer = xr::CompositionLayerQuad::new()
        .space(space)
        .eye_visibility(xr::EyeVisibility::BOTH)
        .sub_image(
            xr::SwapchainSubImage::new()
                .swapchain(&target.swapchain)
                .image_array_index(0)
                .image_rect(xr::Rect2Di {
                    offset: xr::Offset2Di { x: 0, y: 0 },
                    extent: xr::Extent2Di {
                        width: target.width as i32,
                        height: target.height as i32,
                    },
                }),
        )
        .pose(pose)
        .size(xr::Extent2Df {
            width: size[0],
            height: size[1],
        });
    if blend {
        layer.layer_flags(xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA)
    } else {
        layer
    }
}

/// How an overlay layer is shaped.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Shape {
    /// A flat quad of this size (metres).
    Quad([f32; 2]),
    /// A section of a cylinder around the pose's Y axis: radius, angle
    /// covered (radians) and width/height of the visible section.
    Cylinder {
        radius: f32,
        angle: f32,
        aspect: f32,
    },
}

/// An overlay layer to submit this frame.
struct Overlay<'a> {
    target: &'a QuadTarget,
    pose: xr::Posef,
    shape: Shape,
    blend: bool,
}

impl<'a> Overlay<'a> {
    fn quad(target: &'a QuadTarget, pose: xr::Posef, size: [f32; 2], blend: bool) -> Self {
        Self {
            target,
            pose,
            shape: Shape::Quad(size),
            blend,
        }
    }
}

/// A built composition layer, kept so the layers can be submitted in order.
enum Built<'a> {
    Quad(xr::CompositionLayerQuad<'a, xr::Vulkan>),
    Cylinder(xr::CompositionLayerCylinderKHR<'a, xr::Vulkan>),
}

fn cylinder_layer<'a>(
    space: &'a xr::Space,
    target: &'a QuadTarget,
    pose: xr::Posef,
    radius: f32,
    angle: f32,
    aspect: f32,
    blend: bool,
) -> xr::CompositionLayerCylinderKHR<'a, xr::Vulkan> {
    let layer = xr::CompositionLayerCylinderKHR::new()
        .space(space)
        .eye_visibility(xr::EyeVisibility::BOTH)
        .sub_image(
            xr::SwapchainSubImage::new()
                .swapchain(&target.swapchain)
                .image_array_index(0)
                .image_rect(xr::Rect2Di {
                    offset: xr::Offset2Di { x: 0, y: 0 },
                    extent: xr::Extent2Di {
                        width: target.width as i32,
                        height: target.height as i32,
                    },
                }),
        )
        .pose(pose)
        .radius(radius)
        .central_angle(angle)
        .aspect_ratio(aspect);
    if blend {
        layer.layer_flags(xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA)
    } else {
        layer
    }
}

enum Mode {
    Browser,
    Playing(Box<Playback>),
}

/// Trigger/A held on the video: a quick press toggles the menu, moving
/// while pressed drags the screen.
/// A select press on the browser panel: a click on release, unless it
/// turned into a drag (scrolling the list or its scrollbar).
struct BrowserPress {
    hand: usize,
    hit: browser::Hit,
    start_y: f32,
    start_scroll: f32,
    since: i64,
    dragging: bool,
    long_pressed: bool,
}

/// Holding select this long on a row selects it for editing.
const LONG_PRESS_NS: i64 = 600_000_000;

/// Canvas pixels the pointer must move before a press on the list scrolls it.
const LIST_DRAG_PX: f32 = 24.0;

/// A select press on the control bar, acted on at release (or long press).
struct ControlPress {
    hand: usize,
    hit: controls::Hit,
    since: i64,
    long: bool,
}

/// Remembers the format chosen for a file.
fn save_layout(
    key: Option<&String>,
    layout: &crate::vr::Layout,
    image: &crate::config::ImageAdjust,
) {
    let Some(key) = key else { return };
    let saved = crate::config::LayoutOverride {
        projection: layout.projection,
        stereo: layout.stereo,
        image: *image,
    };
    if let Err(e) = crate::config::save_layout_override(key, Some(saved)) {
        eprintln!("Can't save the format: {e:#}");
    }
}

/// Remembers where the playing file was left (off the frame loop: the
/// write may be slow). Returns what was saved: None when nothing was, Some(None)
/// when the place was forgotten (watched to the end, or back at the start).
fn save_resume(key: Option<&String>, playback: &Playback) -> Option<ResumeSaved> {
    let key = key.cloned()?;
    // Nothing shown yet: the position is still the saved one, not a new place.
    playback.current()?;
    let point = crate::config::resume_point(playback.position(), playback.duration)
        .map(|s| (s, Some(playback.duration)));
    let _ = std::thread::Builder::new()
        .name("save resume".into())
        .spawn(move || {
            if let Err(e) = crate::config::save_resume_position(&key, point) {
                eprintln!("Can't save where the video was left: {e:#}");
            }
        });
    Some(point)
}

/// A saved place: seconds and the video's length, or None for "from the top".
type ResumeSaved = Option<(f64, Option<f64>)>;

/// How often the place in the playing video is saved (for a crash or power-off).
const RESUME_SAVE_INTERVAL: Duration = Duration::from_secs(15);

/// Resuming starts this much before where the video was left.
const RESUME_REWIND: f64 = 3.0;

struct Press {
    hand: usize,
    since: i64,
    yaw: f32,
    pitch: f32,
    start: Placement,
    dragging: bool,
}

/// Aim change (radians) that turns a press into a drag.
const DRAG_THRESHOLD: f32 = 0.035;
/// Longest press (ns) that still counts as a click.
const CLICK_NS: i64 = 400_000_000;

/// Subtitles on a curved screen: bent along it, just in front (a cylinder
/// section around the screen's axis, as high as the flat panel would be).
fn caption_cylinder(placement: &Placement, panel: &Panel) -> (xr::Posef, Shape) {
    let radius = placement.curve_radius() - 0.02;
    let [_, up, normal] = panel.basis();
    // The axis passes through the pivot (behind it for a gentle curve); the
    // section sits at the panel's height.
    let y = dot([0, 1, 2].map(|i| panel.center[i] - placement.pivot[i]), up);
    let back = placement.curve_radius() - placement.distance;
    let axis = Panel {
        center: [0, 1, 2].map(|i| placement.pivot[i] + up[i] * y + normal[i] * back),
        ..*panel
    };
    (
        axis.pose(),
        Shape::Cylinder {
            radius,
            angle: (panel.size[0] / radius).min(std::f32::consts::PI),
            aspect: panel.size[0] / panel.size[1],
        },
    )
}

/// Where subtitles go: over the bottom of a flat screen, just in front of
/// it (on a curved screen, at its depth, i.e. its backmost point: in front of
/// its nearest part they would float too close); for VR180/360, 2 m ahead
/// in the view's direction, below its centre.
fn caption_panel(
    placement: &Placement,
    projection: Projection,
    screen: [f32; 2],
    settings: crate::config::CaptionSettings,
) -> Panel {
    let flat = projection == Projection::Flat;
    // The layer grows with the chosen size; its bottom edge stays put. On a
    // tall screen it is as wide as on a 4:3 one of the same height, so the
    // text stays readable.
    let quad_w = settings.scale
        * if flat {
            screen[0].max(screen[1] / super::player::SCREEN_BOX_ASPECT) * captions::SCREEN_SHARE
        } else {
            1.5
        };
    let quad_h = quad_w * captions::HEIGHT as f32 / captions::WIDTH as f32;
    let (y, depth) = if flat {
        let bottom = -screen[1] / 2.0 + screen[1] * (0.04 + settings.raise);
        (bottom + quad_h / 2.0, placement.distance - 0.02)
    } else {
        (-0.45 + 0.9 * settings.raise + quad_h / 2.0 - 0.14, 2.0)
    };
    let mut panel = Panel {
        center: [0.0; 3],
        yaw: placement.yaw,
        tilt: placement.pitch,
        size: [quad_w, quad_h],
        pixels: [captions::WIDTH, captions::HEIGHT],
    };
    let [_, up, normal] = panel.basis();
    panel.center = [0, 1, 2].map(|i| placement.pivot[i] + up[i] * y - normal[i] * depth);
    panel
}

/// Sample text shown while editing subtitle size and position.
const SAMPLE_CAPTION: &str = "This is how subtitles will look.\nLong lines wrap onto a second one.";

/// A 4×4 texture of one colour (premultiplied alpha), stretched into lines.
fn solid_image(rgb: [u8; 3], alpha: f32) -> Vec<u8> {
    let px = [
        (rgb[0] as f32 * alpha) as u8,
        (rgb[1] as f32 * alpha) as u8,
        (rgb[2] as f32 * alpha) as u8,
        (255.0 * alpha) as u8,
    ];
    px.repeat(16)
}

fn quat_mul(a: xr::Quaternionf, b: xr::Quaternionf) -> xr::Quaternionf {
    xr::Quaternionf {
        w: a.w * b.w - a.x * b.x - a.y * b.y - a.z * b.z,
        x: a.w * b.x + a.x * b.w + a.y * b.z - a.z * b.y,
        y: a.w * b.y - a.x * b.z + a.y * b.w + a.z * b.x,
        z: a.w * b.z + a.x * b.y - a.y * b.x + a.z * b.w,
    }
}

/// A line of the outline: its pose and size (metres).
type Line = (xr::Posef, [f32; 2]);

/// Lines `t` thick along the edges of a flat panel.
fn panel_outline(panel: &Panel, t: f32) -> Vec<Line> {
    let [right, up, _] = panel.basis();
    let [w, h] = panel.size;
    let at = |dx: f32, dy: f32| xr::Posef {
        orientation: panel.orientation(),
        position: {
            let p = add(add(panel.center, scale(right, dx)), scale(up, dy));
            xr::Vector3f {
                x: p[0],
                y: p[1],
                z: p[2],
            }
        },
    };
    vec![
        (at(0.0, h / 2.0), [w + t, t]),
        (at(0.0, -h / 2.0), [w + t, t]),
        (at(-w / 2.0, 0.0), [t, h + t]),
        (at(w / 2.0, 0.0), [t, h + t]),
    ]
}

/// Lines along the edges of the flat screen (curved: along its arcs).
fn screen_outline(placement: &Placement, screen: [f32; 2], t: f32) -> Vec<Line> {
    let base = Panel {
        center: [0.0; 3],
        yaw: placement.yaw,
        tilt: placement.pitch,
        size: screen,
        pixels: [1, 1],
    };
    let [right, up, normal] = base.basis();
    if !placement.curved {
        let panel = Panel {
            center: placement.center(),
            ..base
        };
        return panel_outline(&panel, t);
    }
    // The cylinder's axis: through the pivot, or behind it for a gentle curve.
    let r = placement.curve_radius();
    let axis = add(placement.pivot, scale(normal, r - placement.distance));
    // A point on the curved screen `angle` to the right, `y` up, facing the axis.
    let at = |angle: f32, y: f32| -> xr::Posef {
        let p = add(
            axis,
            add(
                add(scale(right, r * angle.sin()), scale(up, y)),
                scale(normal, -r * angle.cos()),
            ),
        );
        let (s, c) = (-angle / 2.0).sin_cos();
        let turn = xr::Quaternionf {
            x: 0.0,
            y: s,
            z: 0.0,
            w: c,
        };
        xr::Posef {
            orientation: quat_mul(base.orientation(), turn),
            position: xr::Vector3f {
                x: p[0],
                y: p[1],
                z: p[2],
            },
        }
    };
    let [w, h] = screen;
    let half = w / 2.0 / r;
    let mut lines = vec![(at(-half, 0.0), [t, h + t]), (at(half, 0.0), [t, h + t])];
    const SEGMENTS: usize = 24;
    let step = 2.0 * half / SEGMENTS as f32;
    for i in 0..SEGMENTS {
        let angle = -half + (i as f32 + 0.5) * step;
        // A little longer than the chord, so the segments join up.
        let len = 2.0 * r * (step / 2.0).sin() + t / 2.0;
        for y in [h / 2.0, -h / 2.0] {
            lines.push((at(angle, y), [len, t]));
        }
    }
    lines
}

/// Where `ray` meets the video (flat or curved screen, or a point 5 m out
/// for spherical video), for showing the cursor on it.
fn video_hit(
    ray: &Ray,
    placement: &Placement,
    projection: Projection,
    screen: [f32; 2],
) -> Option<[f32; 3]> {
    let r = placement.rotation();
    // Into the placement's frame (Rᵀ·v), relative to its pivot.
    let local = |v: [f32; 3]| [0, 1, 2].map(|i| (0..3).map(|k| r[i][k] * v[k]).sum::<f32>());
    let world = |v: [f32; 3]| {
        add(
            placement.pivot,
            [0, 1, 2].map(|i| (0..3).map(|k| r[k][i] * v[k]).sum::<f32>()),
        )
    };
    let (o, d) = (
        local([
            ray.origin[0] - placement.pivot[0],
            ray.origin[1] - placement.pivot[1],
            ray.origin[2] - placement.pivot[2],
        ]),
        local(ray.direction),
    );
    let (w, h) = (screen[0], screen[1]);
    let p = match projection {
        Projection::Flat if placement.curved => {
            // Relative to the cylinder's axis, behind the pivot for a gentle curve.
            let radius = placement.curve_radius();
            let o = [o[0], o[1], o[2] - (radius - placement.distance)];
            let a = d[0] * d[0] + d[2] * d[2];
            let b = 2.0 * (o[0] * d[0] + o[2] * d[2]);
            let c = o[0] * o[0] + o[2] * o[2] - radius * radius;
            let disc = b * b - 4.0 * a * c;
            if a < 1e-6 || disc < 0.0 {
                return None;
            }
            let t = (-b + disc.sqrt()) / (2.0 * a);
            let p = add(o, scale(d, t));
            let arc = p[0].atan2(-p[2]) * radius;
            (p[2] < 0.0 && arc.abs() <= w / 2.0 && p[1].abs() <= h / 2.0).then_some([
                p[0],
                p[1],
                p[2] + (radius - placement.distance),
            ])?
        }
        Projection::Flat => {
            if d[2] > -1e-4 {
                return None;
            }
            let t = (-placement.distance - o[2]) / d[2];
            let p = add(o, scale(d, t));
            (p[0].abs() <= w / 2.0 && p[1].abs() <= h / 2.0).then_some(p)?
        }
        _ => add(o, scale(d, 5.0)),
    };
    Some(world(p))
}

/// Drops something that may block (decoder, network reader, audio) on a
/// background thread, so it can never freeze the frame loop.
pub fn drop_in_background<T: Send + 'static>(value: T) {
    let _ = std::thread::Builder::new()
        .name("teardown".into())
        .spawn(move || drop(value));
}

/// Frame-loop phases reported by the watchdog when the loop stalls.
const PHASES: [&str; 8] = [
    "events",
    "wait frame",
    "input",
    "stop playback",
    "browser",
    "playback",
    "render",
    "submit",
];

/// Logs where the frame loop is when it has not finished a frame for 3 s.
fn spawn_watchdog(heartbeat: Arc<AtomicI64>, phase: Arc<AtomicU8>, quit: Arc<AtomicBool>) {
    let started = Instant::now();
    let _ = std::thread::Builder::new()
        .name("watchdog".into())
        .spawn(move || {
            let mut reported = 0;
            while !quit.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                let stalled =
                    started.elapsed().as_millis() as i64 - heartbeat.load(Ordering::Relaxed);
                if stalled > 3000 && stalled / 5000 != reported {
                    reported = stalled / 5000;
                    let p = PHASES
                        .get(phase.load(Ordering::Relaxed) as usize)
                        .unwrap_or(&"?");
                    eprintln!(
                        "Watchdog: frame loop stalled for {:.1} s in phase \"{p}\"",
                        stalled as f64 / 1000.0
                    );
                } else if stalled <= 3000 {
                    reported = 0;
                }
            }
        });
}

/// Between the eyes (the reference space's origin when nothing is tracked).
fn head_position(views: &[xr::View]) -> [f32; 3] {
    if views.is_empty() {
        return [0.0; 3];
    }
    let mut sum = [0.0f32; 3];
    for v in views {
        let p = v.pose.position;
        sum = add(sum, [p.x, p.y, p.z]);
    }
    scale(sum, 1.0 / views.len() as f32)
}

fn aim_angles(ray: &Ray) -> (f32, f32) {
    let d = ray.direction;
    ((-d[0]).atan2(-d[2]), d[1].clamp(-1.0, 1.0).asin())
}

fn wrap_angle(a: f32) -> f32 {
    (a + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU) - std::f32::consts::PI
}

/// Runs the app. With a navigator the browser is shown; `initial` starts
/// playing immediately (and the app exits when it ends if there is no browser).
pub fn run(
    mut navigator: Option<Navigator>,
    initial: Option<Playback>,
    options: AppOptions,
) -> anyhow::Result<PlayStats> {
    let mut ctx = XrContext::new()?;
    let mut renderer = Renderer::new(&ctx)?;
    let mut input = Input::new(&ctx)?;
    let heartbeat = Arc::new(AtomicI64::new(0));
    let phase = Arc::new(AtomicU8::new(0));
    let loop_started = Instant::now();
    spawn_watchdog(heartbeat.clone(), phase.clone(), options.quit.clone());
    let set_phase = |p: u8| phase.store(p, Ordering::Relaxed);
    let space = ctx
        .session
        .create_reference_space(xr::ReferenceSpaceType::LOCAL, xr::Posef::IDENTITY)
        .context("Create LOCAL reference space")?;
    let mut fonts = Fonts::load()?;
    let mut browser_target = match navigator {
        Some(_) => Some(renderer.create_quad(&ctx, browser::WIDTH, browser::HEIGHT)?),
        None => None,
    };
    let mut controls_target = renderer.create_quad(&ctx, controls::WIDTH, controls::HEIGHT)?;
    let mut captions_target = renderer.create_quad(&ctx, captions::WIDTH, captions::HEIGHT)?;
    // Outlines of the screen and the subtitle area while editing subtitles.
    let mut screen_line = renderer.create_quad(&ctx, 4, 4)?;
    renderer.upload_quad(&mut screen_line, &solid_image([0xff, 0xff, 0xff], 0.7))?;
    let mut caption_line = renderer.create_quad(&ctx, 4, 4)?;
    renderer.upload_quad(&mut caption_line, &solid_image([0xff, 0xcc, 0x00], 0.9))?;
    // The browser's progress bar, and the player's loading hairline.
    let mut progress_line = renderer.create_quad(&ctx, 4, 4)?;
    renderer.upload_quad(&mut progress_line, &solid_image([0x4f, 0x8c, 0xff], 1.0))?;
    let mut progress_at: Option<(xr::Posef, [f32; 2])>;
    let mut outlines: Vec<(bool, Line)> = Vec::new();
    let mut dialog_target =
        renderer.create_quad(&ctx, controls::DIALOG_WIDTH, controls::DIALOG_HEIGHT)?;
    // The caption on screen, and where it goes this frame.
    let mut caption_drawn: Option<crate::subtitles::Caption> = None;
    let mut caption_at: Option<(xr::Posef, Shape)> = None;
    // Editing subtitle size and position (from the track dialog).
    let mut caption_edit = false;
    let mut cursor = renderer.create_quad(&ctx, CURSOR_PX, CURSOR_PX)?;
    renderer.upload_quad(&mut cursor, &cursor_image())?;
    // The browser's handle, and a brighter one while pointed at or held.
    let mut handle = renderer.create_quad(&ctx, HANDLE_PX[0], HANDLE_PX[1])?;
    renderer.upload_quad(&mut handle, &handle_image(0.55))?;
    let mut handle_hot = renderer.create_quad(&ctx, HANDLE_PX[0], HANDLE_PX[1])?;
    renderer.upload_quad(&mut handle_hot, &handle_image(1.0))?;

    let mut mode = match initial {
        Some(playback) => Mode::Playing(Box::new(playback)),
        None => Mode::Browser,
    };
    let mut stats = PlayStats::default();
    let mut events = xr::EventDataBuffer::new();
    let mut running = false;
    let mut exit_requested = false;
    let mut active_hand = 1usize;
    let mut hovered: Option<browser::Hit> = None;
    // Where the playing file's chosen format is saved.
    let mut playing_key: Option<String> = None;
    let mut resume_saved_at = Instant::now();
    let mut screenshot = options.play.screenshot.clone();
    let mut placement = Placement::new(&options.view);
    placement.curve = crate::config::curvature();
    // The browser panel's width (a multiple of its usual size), as shown:
    // the saved choice, or the one pointed at in its dialog.
    let mut browser_width = crate::config::browser_width();
    // The VR180/360 control bar, once opened; dragged by its handle.
    let mut bar_place: Option<Placement> = None;
    let mut bar_press: Option<Press> = None;
    // Whether the bar offers buffering ahead.
    let mut buffer_button = crate::config::buffer_button();
    let mut browser_place = browser_placement([0.0; 3]);
    let mut controls_panel_at: Option<Panel> = None;
    let mut controls_drawn: Option<(controls::State, controls::Hit, Option<controls::Pressing>)> =
        None;
    let mut press: Option<Press> = None;
    let mut browser_press: Option<BrowserPress> = None;
    // The control bar shows the page with every format.
    // The dialog open above the control bar, and what it last showed.
    let mut dialog: Option<controls::Dialog> = None;
    let mut dialog_drawn: Option<(controls::State, controls::Hit, Option<controls::Pressing>)> =
        None;
    // Subtitle size and position (the same for every video).
    let mut caption_settings = crate::config::caption_settings();
    let mut list_page = 0usize;
    let mut control_press: Option<ControlPress> = None;
    // Formats the format button steps through.
    let mut favourites: Vec<controls::Format> = crate::config::favourite_formats()
        .into_iter()
        .map(|f| (f.projection, f.stereo))
        .collect();
    // Previous (-1) or next (+1) video requested from the control bar.
    let mut switch_video: Option<isize> = None;

    'main: loop {
        heartbeat.store(loop_started.elapsed().as_millis() as i64, Ordering::Relaxed);
        set_phase(0);
        if options.quit.load(Ordering::Relaxed) && !exit_requested {
            exit_requested = true;
            if running {
                ctx.session.request_exit()?;
            } else {
                break;
            }
        }
        while let Some(event) = ctx.xr.poll_event(&mut events)? {
            match event {
                xr::Event::SessionStateChanged(e) => {
                    eprintln!("OpenXR session: {:?}", e.state());
                    match e.state() {
                        xr::SessionState::READY => {
                            ctx.session.begin(VIEW_TYPE)?;
                            running = true;
                        }
                        xr::SessionState::STOPPING => {
                            ctx.session.end()?;
                            running = false;
                        }
                        xr::SessionState::EXITING | xr::SessionState::LOSS_PENDING => break 'main,
                        _ => {}
                    }
                }
                xr::Event::InstanceLossPending(_) => break 'main,
                _ => {}
            }
        }
        if !running {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        set_phase(1);
        let state = ctx.frame_waiter.wait()?;
        ctx.frame_stream.begin()?;
        stats.xr_frames += 1;
        if !state.should_render {
            ctx.frame_stream
                .end(state.predicted_display_time, ctx.blend_mode, &[])?;
            continue;
        }
        stats.rendered_xr_frames += 1;
        let now = state.predicted_display_time.as_nanos();
        let dt = state.predicted_display_period.as_nanos() as f32 / 1e9;
        set_phase(2);
        let mut buttons = input
            .poll(&ctx, &space, state.predicted_display_time)
            .unwrap_or_else(|e| {
                eprintln!("Input: {e:#}");
                InputState::default()
            });
        for (hand, pressed) in buttons.select.iter().enumerate() {
            if *pressed {
                active_hand = hand;
            }
        }
        let ray = buttons.rays[active_hand]
            .as_ref()
            .or(buttons.rays[1 - active_hand].as_ref());
        let (_, views) =
            ctx.session
                .locate_views(VIEW_TYPE, state.predicted_display_time, &space)?;

        // B while editing subtitles: back to the track dialog.
        if caption_edit && buttons.back {
            caption_edit = false;
            dialog = Some(controls::Dialog::Tracks);
            buttons.back = false;
            controls_drawn = None;
        }
        // Leaving playback: B, end of video, or --duration reached.
        let mut stop_playback = switch_video.is_some();
        if let Mode::Playing(playback) = &mut mode {
            let reached_end = options
                .play
                .duration
                .is_some_and(|d| playback.media_time(now).is_some_and(|t| t >= d));
            stop_playback |= buttons.back || playback.finished(now) || reached_end;
        }
        if stop_playback {
            set_phase(3);
            let mut saved: Option<ResumeSaved> = None;
            if let Mode::Playing(playback) = std::mem::replace(&mut mode, Mode::Browser) {
                saved = save_resume(playing_key.as_ref(), &playback);
                stats.displayed_frames += playback.stats.displayed_frames;
                stats.uploaded_frames += playback.stats.uploaded_frames;
                stats.skipped_frames += playback.stats.skipped_frames;
                stats.media_seconds = playback.stats.media_seconds;
                drop_in_background(playback);
            }
            // This B press ended playback; don't also go up a folder.
            buttons.back = false;
            controls_panel_at = None;
            press = None;
            if let (Some(nav), Some(key), Some(point)) =
                (navigator.as_mut(), playing_key.as_ref(), saved)
            {
                nav.note_resume(key, point);
            }
            match navigator.as_mut() {
                // Previous/next: the browser shows "Opening …" until it plays.
                Some(nav) if let Some(delta) = switch_video.take() => {
                    nav.open_adjacent(delta);
                    nav.redraw();
                }
                Some(nav) => nav.playback_ended(),
                None => options.quit.store(true, Ordering::Relaxed),
            }
        }

        let mut cursor_at: Option<(Panel, f32, f32)> = None;
        // The VR180/360 bar's handle this frame, and whether a handle is
        // pointed at or held (drawn brighter).
        let mut bar_handle_at: Option<Panel> = None;
        let handle_lit: bool;
        // A cursor on the video itself: position, aim yaw/pitch, distance from the eye.
        let mut video_cursor: Option<([f32; 3], f32, f32, f32)> = None;
        // Every frame submits a projection layer (the video, or a dark backdrop
        // in the browser): SteamVR only treats an app showing one as running,
        // and otherwise leaves its own menu in front ("start in background").
        let eyes_rendered;
        match &mut mode {
            Mode::Browser => {
                set_phase(4);
                let (Some(nav), Some(target)) = (navigator.as_mut(), browser_target.as_mut())
                else {
                    ctx.frame_stream
                        .end(state.predicted_display_time, ctx.blend_mode, &[])?;
                    continue;
                };
                if nav.take_exit() {
                    options.quit.store(true, Ordering::Relaxed);
                }
                if let Some(opened) = nav.poll() {
                    playing_key = Some(opened.key.clone());
                    eprintln!(
                        "Playing {} as {:?} / {:?}",
                        opened.name, opened.layout.projection, opened.layout.stereo
                    );
                    let start = opened.resume.map_or(0.0, |t| (t - RESUME_REWIND).max(0.0));
                    let mut playback = Playback::start(
                        opened.decoder,
                        opened.layout,
                        start,
                        // Full level: the headset's volume buttons set loudness.
                        1.0,
                    );
                    resume_saved_at = Instant::now();
                    playback.set_buffer(opened.buffer);
                    playback.set_reserve(opened.reserve);
                    playback.add_external_subtitles(opened.external_subtitles);
                    playback.image = opened.image;
                    renderer.set_adjust(&playback.image);
                    dialog = None;
                    caption_edit = false;
                    // Ahead at the default size and viewpoint: the last
                    // video's zoom, drag and inside-the-sphere offset
                    // don't carry over.
                    placement = Placement {
                        curved: placement.curved,
                        curve: placement.curve,
                        pivot: head_position(&views),
                        ..Placement::new(&options.view)
                    };
                    press = None;
                    mode = Mode::Playing(Box::new(playback));
                    ctx.frame_stream
                        .end(state.predicted_display_time, ctx.blend_mode, &[])?;
                    continue;
                }
                let browser_at = browser_panel(&browser_place, browser_width);
                let handle_at = handle_panel(&browser_at, false);
                let point = ray.and_then(|r| browser_at.hit(r));
                let on_handle = ray.and_then(|r| handle_at.hit(r));
                // Moving the browser: select held on its handle, or the grip
                // held while pointing at it. The stick then moves it nearer
                // or farther; a thumbstick click puts it back ahead.
                if buttons.reset {
                    browser_place = browser_placement(head_position(&views));
                    press = None;
                }
                let grab = (on_handle.is_some() && buttons.select[active_hand])
                    || (buttons.grip && (point.is_some() || on_handle.is_some()));
                if grab
                    && press.is_none()
                    && browser_press.is_none()
                    && let Some(r) = buttons.rays[active_hand].as_ref()
                {
                    let (yaw, pitch) = aim_angles(r);
                    // From the handle it moves at once; by grip after a small move.
                    let dragging = on_handle.is_some() && buttons.select[active_hand];
                    if dragging {
                        browser_place.anchor_at(head_position(&views));
                    }
                    press = Some(Press {
                        hand: active_hand,
                        since: now,
                        yaw,
                        pitch,
                        start: browser_place,
                        dragging,
                    });
                }
                let mut moving = false;
                if let Some(p) = &mut press {
                    let held = buttons.select_held[p.hand] || buttons.grip;
                    match (held, buttons.rays[p.hand].as_ref()) {
                        (true, Some(r)) => {
                            let (yaw, pitch) = aim_angles(r);
                            let (dyaw, dpitch) = (wrap_angle(yaw - p.yaw), pitch - p.pitch);
                            if !p.dragging
                                && (dyaw.abs() > DRAG_THRESHOLD || dpitch.abs() > DRAG_THRESHOLD)
                            {
                                p.dragging = true;
                                browser_place.anchor_at(head_position(&views));
                                (p.yaw, p.pitch, p.start) = (yaw, pitch, browser_place);
                            } else if p.dragging {
                                browser_place.yaw = p.start.yaw + dyaw;
                                browser_place.pitch = (p.start.pitch + dpitch)
                                    .clamp(-super::player::MAX_PITCH, super::player::MAX_PITCH);
                                if buttons.scroll != 0.0 {
                                    browser_place.distance = (browser_place.distance
                                        + buttons.scroll * 1.5 * dt)
                                        .clamp(1.0, 6.0);
                                }
                            }
                            moving = p.dragging;
                        }
                        _ => press = None,
                    }
                }
                handle_lit = on_handle.is_some() || moving;
                let point = if press.is_some() { None } else { point };
                let hit = point.map_or(browser::Hit::Nothing, |(x, y)| {
                    browser::hit(nav.view(), &mut fonts, x, y)
                });
                // The settings dialogs preview the choice pointed at: the
                // browser's width on the panel itself, the screen curvature
                // as an outline bent around the panel. The next video
                // opens with the saved curvature.
                placement.curve = nav.curvature();
                browser_width = nav.browser_width();
                outlines.clear();
                match nav.preview(hit) {
                    Some(crate::ui::navigator::Preview::BrowserWidth(w)) => browser_width = w,
                    Some(crate::ui::navigator::Preview::Curvature(c)) => {
                        let sample = Placement {
                            distance: CURVE_PREVIEW_DISTANCE,
                            curved: true,
                            curve: c,
                            zoom: 1.0,
                            offset: 0.0,
                            ..browser_place
                        };
                        let w = CURVE_PREVIEW_SCALE * BROWSER_SIZE[0] * CURVE_PREVIEW_DISTANCE
                            / BROWSER_DISTANCE;
                        let screen = [w, w * 9.0 / 16.0];
                        outlines.extend(
                            screen_outline(&sample, screen, 0.004 * CURVE_PREVIEW_DISTANCE)
                                .into_iter()
                                .map(|l| (false, l)),
                        );
                    }
                    None => {}
                }
                if buttons.select[active_hand]
                    && browser_press.is_none()
                    && press.is_none()
                    && let Some((_, y)) = point
                {
                    browser_press = Some(BrowserPress {
                        hand: active_hand,
                        hit,
                        start_y: y,
                        start_scroll: nav.scroll(),
                        since: now,
                        dragging: false,
                        long_pressed: false,
                    });
                }
                if let Some(p) = &mut browser_press {
                    if buttons.select_held[p.hand] {
                        if let Some((_, y)) = point {
                            let on_list =
                                matches!(p.hit, browser::Hit::Row(_) | browser::Hit::RowAction(..));
                            if p.hit == browser::Hit::ScrollBar {
                                nav.set_scroll(browser::scroll_at(nav.view(), y));
                            } else if let (
                                browser::Hit::Form(form::Hit::Field(i, from)),
                                browser::Hit::Form(form::Hit::Field(j, to)),
                            ) = (p.hit, hit)
                                && i == j
                                && (p.dragging || from != to)
                                && !p.long_pressed
                            {
                                // Dragging across a field selects its text.
                                p.dragging = true;
                                nav.drag_select(i, from, to);
                            } else if on_list
                                && !p.long_pressed
                                && (p.dragging || (y - p.start_y).abs() > LIST_DRAG_PX)
                            {
                                // Drag the list like a touch screen.
                                p.dragging = true;
                                nav.set_scroll(
                                    p.start_scroll - browser::rows_for_drag(y - p.start_y),
                                );
                            }
                        }
                        if !p.dragging && !p.long_pressed && now - p.since > LONG_PRESS_NS {
                            match p.hit {
                                browser::Hit::Row(i) => {
                                    p.long_pressed = true;
                                    nav.long_press(i);
                                }
                                // Holding on a field selects all of its text.
                                browser::Hit::Form(form::Hit::Field(i, _)) => {
                                    p.long_pressed = true;
                                    nav.select_all_field(i);
                                }
                                _ => {}
                            }
                        }
                    } else {
                        if !p.dragging && !p.long_pressed {
                            nav.click(p.hit);
                        }
                        browser_press = None;
                    }
                }
                if buttons.back {
                    nav.back();
                }
                if buttons.scroll != 0.0 && !moving {
                    // Holding the grip scrolls four times faster.
                    let speed = if buttons.grip { 48.0 } else { 12.0 };
                    nav.scroll_by(-buttons.scroll * speed * dt);
                }
                if nav.take_dirty() || hovered != Some(hit) {
                    hovered = Some(hit);
                    let hover_point = match hit {
                        browser::Hit::Nothing => None,
                        _ => point,
                    };
                    let canvas = browser::render(nav.view(), &mut fonts, hover_point, false);
                    renderer.upload_quad(target, &canvas.pixels)?;
                }
                if let Some((x, y)) = point {
                    cursor_at = Some((browser_at, x, y));
                } else if let Some((x, y)) = on_handle.filter(|_| !moving) {
                    cursor_at = Some((handle_at, x, y));
                }
                // A thin bar under the header: filling as videos are checked,
                // or sweeping while something with no known end runs.
                progress_at = nav.progress().map(|progress| {
                    let (from, to) = match progress {
                        crate::ui::navigator::Progress::Fraction(f) => (0.0, f),
                        crate::ui::navigator::Progress::Busy => {
                            let t = (now as f64 / 1.4e9).fract() as f32 * 1.3 - 0.3;
                            (t, t + 0.3)
                        }
                    };
                    let (x, y, w, h) = browser::progress_rect(from, to);
                    let p = browser_at.point(x + w / 2.0, y + h / 2.0, 0.003);
                    let scale = browser_at.size[0] / browser_at.pixels[0] as f32;
                    (
                        xr::Posef {
                            orientation: browser_at.orientation(),
                            position: xr::Vector3f {
                                x: p[0],
                                y: p[1],
                                z: p[2],
                            },
                        },
                        [w * scale, h * scale],
                    )
                });
                set_phase(6);
                renderer.begin_frame(None)?;
                for (eye, _) in views.iter().enumerate() {
                    let target = &mut renderer.eyes[eye];
                    let index = target.swapchain.acquire_image()?;
                    target.swapchain.wait_image(xr::Duration::INFINITE)?;
                    renderer.draw_eye(eye, index, &Default::default(), false);
                }
                renderer.end_frame()?;
                for eye in renderer.eyes.iter_mut() {
                    eye.swapchain.release_image()?;
                }
                eyes_rendered = true;
            }
            Mode::Playing(playback) => {
                set_phase(5);
                if resume_saved_at.elapsed() >= RESUME_SAVE_INTERVAL {
                    resume_saved_at = Instant::now();
                    save_resume(playing_key.as_ref(), playback);
                }
                let curved_applies = playback.layout.projection == Projection::Flat;
                let screen = screen_size(
                    renderer.video_size(),
                    &playback.layout,
                    &options.view,
                    placement.zoom,
                    playback.image.rotation,
                );
                // A screen's control bar moves with it (dragged, zoomed, curved).
                if curved_applies && controls_panel_at.is_some() {
                    controls_panel_at = Some(screen_controls_panel(&placement, screen));
                } else if controls_panel_at.is_some()
                    && let Some(head) = views.first()
                {
                    // VR180/360: where it was put (dragging moves it).
                    let place = bar_place.get_or_insert_with(|| vr_bar_placement(&head.pose));
                    controls_panel_at = Some(vr_bar_panel(place));
                }
                let ui_state = controls::State {
                    paused: playback.paused(),
                    position: playback.position().floor(),
                    duration: playback.duration,
                    buffered: playback.buffered(),
                    reserve: playback
                        .reserve_status()
                        .filter(|_| buffer_button)
                        .map(|r| controls::Reserve {
                            on: r.on,
                            held: r.held,
                            goal: r.goal,
                        }),
                    has_previous: navigator.as_ref().is_some_and(|n| n.has_adjacent(-1)),
                    has_next: navigator.as_ref().is_some_and(|n| n.has_adjacent(1)),
                    curved: curved_applies.then_some(placement.curved),
                    buffer_button,
                    format: (playback.layout.projection, playback.layout.stereo),
                    favourites: favourites.clone(),
                    subtitle_tracks: playback.subtitle_labels(),
                    subtitle: playback.subtitle_index(),
                    audio_tracks: playback.audio_labels().to_vec(),
                    audio: playback.audio_index(),
                    list_page,
                    image: playback.image,
                    dialog,
                    caption_edit,
                };
                // The pointer on the dialog (above the bar) or on the bar.
                let dialog_at = controls_panel_at
                    .filter(|_| dialog.is_some())
                    .map(|bar| dialog_panel(&bar));
                let dialog_point =
                    dialog_at.and_then(|p| ray.and_then(|r| p.hit(r)).map(|pt| (p, pt)));
                let bar_point =
                    controls_panel_at.and_then(|p| ray.and_then(|r| p.hit(r)).map(|pt| (p, pt)));
                let control_point = dialog_point.or(bar_point);
                let dialog_hit = dialog_point.map_or(controls::Hit::Nothing, |(_, (x, y))| {
                    controls::dialog_hit(&ui_state, x, y)
                });
                let bar_hit = match (dialog_point, bar_point) {
                    (None, Some((_, (x, y)))) => controls::hit(&ui_state, x, y),
                    _ => controls::Hit::Nothing,
                };
                let control_hit = if dialog_point.is_some() {
                    dialog_hit
                } else {
                    bar_hit
                };

                // VR180/360: the bar's handle, under it, or above when it
                // hangs low (above the dialog, if one is open).
                if !curved_applies && let Some(bar) = controls_panel_at {
                    let low = bar_place.is_some_and(|p| p.pitch < LOW_BAR_PITCH);
                    bar_handle_at = Some(match (low, dialog_at) {
                        (true, Some(dialog)) => handle_panel(&dialog, true),
                        (true, None) => handle_panel(&bar, true),
                        (false, _) => handle_panel(&bar, false),
                    });
                }
                let on_bar_handle = bar_handle_at.and_then(|h| ray.and_then(|r| h.hit(r)));
                // Moving the bar: select held on its handle, or the grip held
                // while pointing at it; the stick then moves it nearer or farther.
                let grab_bar = bar_handle_at.is_some()
                    && ((on_bar_handle.is_some() && buttons.select[active_hand])
                        || (buttons.grip && (control_point.is_some() || on_bar_handle.is_some())));
                if grab_bar
                    && press.is_none()
                    && control_press.is_none()
                    && bar_press.is_none()
                    && let Some(r) = buttons.rays[active_hand].as_ref()
                    && let Some(place) = bar_place.as_mut()
                {
                    let (yaw, pitch) = aim_angles(r);
                    // From the handle it moves at once; by grip after a small move.
                    let dragging = on_bar_handle.is_some() && buttons.select[active_hand];
                    if dragging {
                        place.anchor_at(head_position(&views));
                    }
                    bar_press = Some(Press {
                        hand: active_hand,
                        since: now,
                        yaw,
                        pitch,
                        start: *place,
                        dragging,
                    });
                }
                let mut bar_moving = false;
                if let Some(p) = &mut bar_press
                    && let Some(place) = bar_place.as_mut()
                {
                    let held = buttons.select_held[p.hand] || buttons.grip;
                    match (held, buttons.rays[p.hand].as_ref()) {
                        (true, Some(r)) => {
                            let (yaw, pitch) = aim_angles(r);
                            let (dyaw, dpitch) = (wrap_angle(yaw - p.yaw), pitch - p.pitch);
                            if !p.dragging
                                && (dyaw.abs() > DRAG_THRESHOLD || dpitch.abs() > DRAG_THRESHOLD)
                            {
                                p.dragging = true;
                                place.anchor_at(head_position(&views));
                                (p.yaw, p.pitch, p.start) = (yaw, pitch, *place);
                            } else if p.dragging {
                                place.yaw = p.start.yaw + dyaw;
                                place.pitch = (p.start.pitch + dpitch)
                                    .clamp(-super::player::MAX_PITCH, super::player::MAX_PITCH);
                                if buttons.scroll != 0.0 {
                                    place.distance = (place.distance + buttons.scroll * 1.5 * dt)
                                        .clamp(1.0, 5.0);
                                }
                            }
                            bar_moving = p.dragging;
                        }
                        _ => bar_press = None,
                    }
                }
                handle_lit = on_bar_handle.is_some() || bar_moving;

                // Controls act on release, so buttons with a long press can
                // tell the two apart; the seek bar acts at once.
                let mut clicked: Option<controls::Hit> = None;
                let mut long_pressed: Option<controls::Hit> = None;
                if let Some(cp) = &mut control_press {
                    if buttons.select_held[cp.hand] {
                        if !cp.long && cp.hit.has_long_press() && now - cp.since > LONG_PRESS_NS {
                            cp.long = true;
                            long_pressed = Some(cp.hit);
                        }
                    } else {
                        if !cp.long {
                            clicked = Some(cp.hit);
                        }
                        control_press = None;
                    }
                }
                // The held button, drawn pushed in; one with a long press
                // fills up as the press runs (in steps, to limit redraws).
                let pressing = control_press
                    .as_ref()
                    .filter(|cp| buttons.select_held[cp.hand])
                    .map(|cp| {
                        let steps = controls::PRESS_STEPS as f32;
                        let step = if cp.hit.has_long_press() {
                            ((now - cp.since) as f32 / LONG_PRESS_NS as f32 * steps)
                                .clamp(0.0, steps) as u8
                        } else {
                            controls::PRESS_STEPS
                        };
                        controls::Pressing { hit: cp.hit, step }
                    });
                if buttons.select[active_hand]
                    && press.is_none()
                    && control_press.is_none()
                    && bar_press.is_none()
                    && on_bar_handle.is_none()
                {
                    if control_point.is_some() {
                        if let controls::Hit::Seek(_) = control_hit {
                            clicked = Some(control_hit);
                        } else {
                            control_press = Some(ControlPress {
                                hand: active_hand,
                                hit: control_hit,
                                since: now,
                                long: false,
                            });
                        }
                    } else if let Some(r) = buttons.rays[active_hand].as_ref() {
                        let (yaw, pitch) = aim_angles(r);
                        press = Some(Press {
                            hand: active_hand,
                            since: now,
                            yaw,
                            pitch,
                            start: placement,
                            dragging: false,
                        });
                    }
                }
                if let Some(p) = &mut press {
                    match (buttons.select_held[p.hand], buttons.rays[p.hand].as_ref()) {
                        (true, Some(r)) => {
                            let (yaw, pitch) = aim_angles(r);
                            let (dyaw, dpitch) = (wrap_angle(yaw - p.yaw), pitch - p.pitch);
                            if !p.dragging
                                && (dyaw.abs() > DRAG_THRESHOLD || dpitch.abs() > DRAG_THRESHOLD)
                            {
                                // Start from here, so the screen doesn't jump by the
                                // threshold, and orbit the head where it is now.
                                p.dragging = true;
                                placement.anchor_at(head_position(&views));
                                (p.yaw, p.pitch, p.start) = (yaw, pitch, placement);
                            } else if p.dragging {
                                placement.yaw = p.start.yaw + dyaw;
                                placement.pitch = (p.start.pitch + dpitch)
                                    .clamp(-super::player::MAX_PITCH, super::player::MAX_PITCH);
                                // The stick pushes the screen away or pulls it closer.
                                if buttons.scroll != 0.0 {
                                    placement.distance = (placement.distance
                                        + buttons.scroll * 2.0 * dt)
                                        .clamp(0.8, 12.0);
                                }
                            }
                        }
                        _ => {
                            // Released: a short press without dragging toggles the menu.
                            if !p.dragging && now - p.since < CLICK_NS {
                                controls_panel_at = match controls_panel_at {
                                    Some(_) => None,
                                    // Under a flat screen; VR180/360 surround us, so
                                    // ahead of the head instead.
                                    None if curved_applies => {
                                        Some(screen_controls_panel(&placement, screen))
                                    }
                                    None => views.first().map(|v| {
                                        let place = bar_place
                                            .get_or_insert_with(|| vr_bar_placement(&v.pose));
                                        vr_bar_panel(place)
                                    }),
                                };
                                dialog = None;
                                caption_edit = false;
                                controls_drawn = None;
                            }
                            press = None;
                        }
                    }
                }
                let set_format = |playback: &mut Playback, format: controls::Format| {
                    (playback.layout.projection, playback.layout.stereo) = format;
                    save_layout(playing_key.as_ref(), &playback.layout, &playback.image);
                };
                let mut image_changed = false;
                let mut captions_changed = false;
                match clicked {
                    Some(controls::Hit::PlayPause) => playback.toggle_pause(now),
                    Some(controls::Hit::Reserve) => playback.toggle_reserve(now),
                    Some(controls::Hit::Seek(f)) => playback.seek(f as f64 * playback.duration),
                    Some(controls::Hit::Previous) => switch_video = Some(-1),
                    Some(controls::Hit::Next) => switch_video = Some(1),
                    // CC: subtitles on/off (or, with none, straight to audio tracks).
                    Some(controls::Hit::Captions) => {
                        if playback.subtitle_labels().is_empty() {
                            dialog = Some(controls::Dialog::Tracks);
                        } else {
                            playback.toggle_subtitles();
                        }
                    }
                    Some(controls::Hit::Screen) => {
                        let current = (playback.layout.projection, playback.layout.stereo);
                        if let Some(next) = controls::next_favourite(current, &favourites) {
                            set_format(playback, next);
                        }
                    }
                    Some(controls::Hit::Image) if dialog == Some(controls::Dialog::Image) => {
                        dialog = None;
                    }
                    Some(controls::Hit::Image) => dialog = Some(controls::Dialog::Image),
                    Some(controls::Hit::AudioTrack(track)) => playback.set_audio_track(track),
                    Some(controls::Hit::SubtitleTrack(track)) => playback.set_subtitle(track),
                    Some(controls::Hit::MorePage) => {
                        list_page =
                            (list_page + 1) % controls::pages(playback.subtitle_labels().len() + 1);
                    }
                    Some(controls::Hit::CaptionSize(d)) => {
                        caption_settings.scale =
                            (caption_settings.scale * 1.12f32.powi(d as i32)).clamp(0.5, 2.5);
                        captions_changed = true;
                    }
                    Some(controls::Hit::CaptionMove(d)) => {
                        caption_settings.raise =
                            (caption_settings.raise + 0.03 * d as f32).clamp(-0.3, 0.8);
                        captions_changed = true;
                    }
                    Some(controls::Hit::CaptionEdit) => {
                        caption_edit = true;
                        dialog = None;
                    }
                    Some(controls::Hit::CaptionReset) => {
                        caption_settings = Default::default();
                        captions_changed = true;
                    }
                    Some(controls::Hit::CaptionDone) => {
                        caption_edit = false;
                        dialog = Some(controls::Dialog::Tracks);
                    }
                    Some(controls::Hit::Pick(i)) => set_format(playback, controls::FORMATS[i]),
                    Some(controls::Hit::Curved) => placement.curved = !placement.curved,
                    Some(controls::Hit::BufferButton) => {
                        buffer_button = !buffer_button;
                        if let Err(e) = crate::config::save_buffer_button(buffer_button) {
                            eprintln!("Can't save the buffer button setting: {e:#}");
                        }
                    }
                    Some(controls::Hit::Brightness(d)) => {
                        playback.image.brightness = (playback.image.brightness
                            + controls::BRIGHTNESS_STEP * d as f32)
                            .clamp(-0.5, 0.5);
                        image_changed = true;
                    }
                    Some(controls::Hit::Contrast(d)) => {
                        playback.image.contrast = (playback.image.contrast
                            + controls::CONTRAST_STEP * d as f32)
                            .clamp(0.5, 2.0);
                        image_changed = true;
                    }
                    Some(controls::Hit::Saturation(d)) => {
                        playback.image.saturation = (playback.image.saturation
                            + controls::SATURATION_STEP * d as f32)
                            .clamp(0.0, 2.0);
                        image_changed = true;
                    }
                    Some(controls::Hit::Rotate(turns)) => {
                        playback.image.rotation = turns;
                        image_changed = true;
                    }
                    Some(controls::Hit::ResetImage) => {
                        playback.image = Default::default();
                        image_changed = true;
                    }
                    Some(controls::Hit::Close) => dialog = None,
                    Some(controls::Hit::Nothing) | None => {}
                }
                // A long press opens a button's dialog, or closes it again.
                match long_pressed {
                    Some(controls::Hit::Captions) if dialog == Some(controls::Dialog::Tracks) => {
                        dialog = None;
                    }
                    Some(controls::Hit::Captions) => {
                        dialog = Some(controls::Dialog::Tracks);
                        // Start on the page with the track shown.
                        list_page = playback.subtitle_index().map_or(0, |i| (i + 1) / 11);
                    }
                    Some(controls::Hit::Screen) if dialog == Some(controls::Dialog::Screen) => {
                        dialog = None;
                    }
                    Some(controls::Hit::Screen) => dialog = Some(controls::Dialog::Screen),
                    // Star or unstar a favourite.
                    Some(controls::Hit::Pick(i)) => {
                        let format = controls::FORMATS[i];
                        match favourites.iter().position(|f| *f == format) {
                            Some(at) if favourites.len() > 1 => {
                                favourites.remove(at);
                            }
                            Some(_) => {} // keep at least one
                            None => favourites.push(format),
                        }
                        let saved: Vec<crate::config::Format> = favourites
                            .iter()
                            .map(|&(projection, stereo)| crate::config::Format {
                                projection,
                                stereo,
                            })
                            .collect();
                        if let Err(e) = crate::config::save_favourite_formats(&saved) {
                            eprintln!("Can't save favourite formats: {e:#}");
                        }
                    }
                    _ => {}
                }
                if image_changed {
                    renderer.set_adjust(&playback.image);
                    save_layout(playing_key.as_ref(), &playback.layout, &playback.image);
                }
                if captions_changed {
                    caption_drawn = None;
                    if let Err(e) = crate::config::save_caption_settings(caption_settings) {
                        eprintln!("Can't save subtitle settings: {e:#}");
                    }
                }
                if clicked.is_some() || long_pressed.is_some() {
                    controls_drawn = None;
                    dialog_drawn = None;
                }
                let dragging = press.as_ref().is_some_and(|p| p.dragging);
                // Thumbstick click: back to the default size and place.
                // D-pad left/right (or a sideways stick flick): 5 seconds back/forward.
                if buttons.seek != 0 {
                    playback.seek(playback.position() + 5.0 * buttons.seek as f64);
                    controls_drawn = None;
                }
                if buttons.reset {
                    // Ahead of the head, wherever it is now; the VR bar too.
                    placement = Placement {
                        curved: placement.curved,
                        curve: placement.curve,
                        pivot: head_position(&views),
                        ..Placement::new(&options.view)
                    };
                    press = None;
                    bar_place = None;
                    bar_press = None;
                }
                // The sticks zoom, except while dragging, when they move the
                // screen nearer or farther.
                if !dragging && curved_applies {
                    // Either stick resizes a screen, down to a quarter (0.8 m
                    // wide by default).
                    placement.zoom =
                        (placement.zoom * (buttons.scroll * 1.2 * dt).exp()).clamp(0.25, 3.0);
                } else if !dragging {
                    // VR180/360, two ways to compare: the left stick moves the
                    // viewpoint towards the picture; the right magnifies or
                    // shrinks it (below 0.4× it would be mostly black).
                    placement.offset = (placement.offset + buttons.stick[0] * 0.6 * dt)
                        .clamp(super::player::MIN_OFFSET, super::player::MAX_OFFSET);
                    placement.zoom =
                        (placement.zoom * (buttons.stick[1] * 1.2 * dt).exp()).clamp(0.4, 3.0);
                }

                if controls_panel_at.is_some() {
                    if controls_drawn.as_ref() != Some(&(ui_state.clone(), bar_hit, pressing)) {
                        let canvas = controls::render(&ui_state, &mut fonts, bar_hit, pressing);
                        renderer.upload_quad(&mut controls_target, &canvas.pixels)?;
                        controls_drawn = Some((ui_state.clone(), bar_hit, pressing));
                    }
                    if dialog.is_some()
                        && dialog_drawn.as_ref() != Some(&(ui_state.clone(), dialog_hit, pressing))
                    {
                        let canvas =
                            controls::render_dialog(&ui_state, &mut fonts, dialog_hit, pressing);
                        renderer.upload_quad(&mut dialog_target, &canvas.pixels)?;
                        dialog_drawn = Some((ui_state, dialog_hit, pressing));
                    }
                    if let Some((p, (x, y))) = control_point {
                        cursor_at = Some((p, x, y));
                    } else if let (Some((x, y)), Some(h)) = (on_bar_handle, bar_handle_at)
                        && !bar_moving
                    {
                        cursor_at = Some((h, x, y));
                    } else if let (Some(r), Some(head)) = (ray, views.first()) {
                        // Menu open: also show where the ray meets the video.
                        let screen = screen_size(
                            renderer.video_size(),
                            &playback.layout,
                            &options.view,
                            placement.zoom,
                            playback.image.rotation,
                        );
                        if let Some(point) =
                            video_hit(r, &placement, playback.layout.projection, screen)
                        {
                            let eye = head.pose.position;
                            let to_eye = [eye.x - point[0], eye.y - point[1], eye.z - point[2]];
                            let distance = dot(to_eye, to_eye).sqrt();
                            let (yaw, pitch) = aim_angles(r);
                            video_cursor = Some((point, yaw, pitch, distance));
                        }
                    }
                }

                // External subtitle files are fetched when chosen.
                if let Some(nav) = navigator.as_mut() {
                    nav.poll_while_playing();
                    if let Some((track, result)) = nav.take_subtitle() {
                        playback.subtitle_loaded(track, result);
                    }
                    if let Some((track, path)) = playback.take_subtitle_fetch() {
                        nav.fetch_subtitle(track, path);
                    }
                }

                // Subtitles: redrawn only when the text changes. While
                // editing, sample text shows their size and place.
                let caption = if caption_edit {
                    Some(crate::subtitles::Caption::text(SAMPLE_CAPTION))
                } else {
                    playback.caption()
                };
                if caption != caption_drawn {
                    if let Some(caption) = &caption {
                        let canvas = captions::render(caption, &mut fonts);
                        renderer.upload_quad(&mut captions_target, &canvas.pixels)?;
                    }
                    caption_drawn = caption.clone();
                }
                let screen = screen_size(
                    renderer.video_size(),
                    &playback.layout,
                    &options.view,
                    placement.zoom,
                    playback.image.rotation,
                );
                let flat = playback.layout.projection == Projection::Flat;
                let caption_area = caption_panel(
                    &placement,
                    playback.layout.projection,
                    screen,
                    caption_settings,
                );
                caption_at = caption.is_some().then(|| {
                    if flat && placement.curved && ctx.cylinder_layers {
                        caption_cylinder(&placement, &caption_area)
                    } else {
                        (caption_area.pose(), Shape::Quad(caption_area.size))
                    }
                });
                outlines.clear();
                if caption_edit {
                    // Thick enough to see at any distance.
                    let t = 0.004 * if flat { placement.distance } else { 2.0 };
                    if flat {
                        outlines.extend(
                            screen_outline(&placement, screen, t)
                                .into_iter()
                                .map(|l| (false, l)),
                        );
                    }
                    outlines.extend(
                        panel_outline(&caption_area, t)
                            .into_iter()
                            .map(|l| (true, l)),
                    );
                }
                set_phase(6);
                let upload = playback.advance(now);
                // A hairline sweeping along the bar's top edge while the next
                // picture is awaited (after opening or seeking).
                progress_at = controls_panel_at
                    .filter(|_| playback.loading())
                    .map(|panel| {
                        let t = (now as f64 / 1.4e9).fract() as f32 * 1.3 - 0.3;
                        let (x, y, w, h) = controls::progress_rect(t, t + 0.3);
                        let p = panel.point(x + w / 2.0, y + h / 2.0, 0.003);
                        let scale = panel.size[0] / panel.pixels[0] as f32;
                        (
                            xr::Posef {
                                orientation: panel.orientation(),
                                position: xr::Vector3f {
                                    x: p[0],
                                    y: p[1],
                                    z: p[2],
                                },
                            },
                            [w * scale, h * scale],
                        )
                    });
                renderer.begin_frame(if upload { playback.current() } else { None })?;
                if upload {
                    playback.stats.uploaded_frames += 1;
                }
                let show = playback.current().is_some();
                if show {
                    playback.stats.displayed_frames += 1;
                }
                let tex = renderer.video_size();
                let mut indices = [0u32; 2];
                for (eye, view) in views.iter().enumerate() {
                    let target = &mut renderer.eyes[eye];
                    let index = target.swapchain.acquire_image()?;
                    target.swapchain.wait_image(xr::Duration::INFINITE)?;
                    indices[eye] = index;
                    let params = eye_params(
                        view,
                        eye,
                        &playback.layout,
                        tex,
                        &options.view,
                        &placement,
                        playback.image.rotation,
                    );
                    renderer.draw_eye(eye, index, &params, show);
                }
                let media_time = playback.media_time(now);
                let capture = match (&screenshot, media_time) {
                    (Some((_, at)), Some(t)) if t >= *at && show => screenshot.take(),
                    _ => None,
                };
                if capture.is_some() {
                    renderer.record_readback(0, indices[0])?;
                }
                renderer.end_frame()?;
                if let Some((path, _)) = capture {
                    renderer.take_screenshot(0, &path)?;
                    stats.screenshot = Some(path.display().to_string());
                }
                for eye in renderer.eyes.iter_mut() {
                    eye.swapchain.release_image()?;
                }
                eyes_rendered = true;
            }
        }

        // Overlay layers to show this frame, back to front.
        let mut overlays: Vec<Overlay> = Vec::new();
        // Whether this frame's eye images have a transparent surround.
        let see_through_now = match &mode {
            Mode::Playing(p) => p.layout.projection == Projection::Flat,
            Mode::Browser => false,
        };
        match &mode {
            Mode::Browser => {
                let browser_at = browser_panel(&browser_place, browser_width);
                if let Some(target) = browser_target.as_ref().filter(|t| t.ready) {
                    // The curvature preview's outline, behind the panel.
                    for (_, (pose, size)) in &outlines {
                        overlays.push(Overlay::quad(&screen_line, *pose, *size, true));
                    }
                    overlays.push(Overlay::quad(
                        target,
                        browser_at.pose(),
                        browser_at.size,
                        false,
                    ));
                    let bar = handle_panel(&browser_at, false);
                    let image = if handle_lit { &handle_hot } else { &handle };
                    overlays.push(Overlay::quad(image, bar.pose(), bar.size, true));
                }
                if let Some((pose, size)) = progress_at.filter(|(_, s)| s[0] > 0.0) {
                    overlays.push(Overlay::quad(&progress_line, pose, size, true));
                }
            }
            Mode::Playing(_) => {
                for (caption, (pose, size)) in &outlines {
                    let target = if *caption {
                        &caption_line
                    } else {
                        &screen_line
                    };
                    overlays.push(Overlay::quad(target, *pose, *size, true));
                }
                if let Some((pose, shape)) = caption_at.filter(|_| captions_target.ready) {
                    overlays.push(Overlay {
                        target: &captions_target,
                        pose,
                        shape,
                        blend: true,
                    });
                }
                if let Some(panel) = controls_panel_at.filter(|_| controls_target.ready) {
                    overlays.push(Overlay::quad(
                        &controls_target,
                        panel.pose(),
                        panel.size,
                        false,
                    ));
                    if dialog.is_some() && dialog_target.ready {
                        let above = dialog_panel(&panel);
                        overlays.push(Overlay::quad(
                            &dialog_target,
                            above.pose(),
                            above.size,
                            false,
                        ));
                    }
                    if let Some(bar) = bar_handle_at {
                        let image = if handle_lit { &handle_hot } else { &handle };
                        overlays.push(Overlay::quad(image, bar.pose(), bar.size, true));
                    }
                }
                if let Some((pose, size)) = progress_at.filter(|(_, s)| s[0] > 0.0) {
                    overlays.push(Overlay::quad(&progress_line, pose, size, true));
                }
            }
        }
        if let Some((panel, x, y)) = cursor_at {
            // Lifted a few millimetres so the cursor sits in front of the panel,
            // and as big as on the browser at any distance.
            let p = panel.point(x, y, 0.005);
            let distance = views.first().map_or(BROWSER_DISTANCE, |v| {
                let e = v.pose.position;
                dot(
                    [p[0] - e.x, p[1] - e.y, p[2] - e.z],
                    [p[0] - e.x, p[1] - e.y, p[2] - e.z],
                )
                .sqrt()
            });
            let size = CURSOR_SIZE * (distance / BROWSER_DISTANCE).max(0.5);
            let pose = xr::Posef {
                orientation: panel.orientation(),
                position: xr::Vector3f {
                    x: p[0],
                    y: p[1],
                    z: p[2],
                },
            };
            overlays.push(Overlay::quad(&cursor, pose, [size, size], true));
        }

        if let Some((p, yaw, pitch, distance)) = video_cursor {
            // Facing the viewer, and scaled to look the same size at any distance.
            let facing = Panel {
                center: p,
                yaw,
                tilt: pitch,
                size: [0.0; 2],
                pixels: [1, 1],
            };
            let pose = xr::Posef {
                orientation: facing.orientation(),
                position: xr::Vector3f {
                    x: p[0],
                    y: p[1],
                    z: p[2],
                },
            };
            let size = CURSOR_SIZE * distance;
            overlays.push(Overlay::quad(&cursor, pose, [size, size], true));
        }
        set_phase(7);
        let projection_views: Vec<xr::CompositionLayerProjectionView<xr::Vulkan>> = if eyes_rendered
        {
            views
                .iter()
                .zip(&renderer.eyes)
                .map(|(view, target)| {
                    xr::CompositionLayerProjectionView::new()
                        .pose(view.pose)
                        .fov(view.fov)
                        .sub_image(
                            xr::SwapchainSubImage::new()
                                .swapchain(&target.swapchain)
                                .image_array_index(0)
                                .image_rect(xr::Rect2Di {
                                    offset: xr::Offset2Di { x: 0, y: 0 },
                                    extent: xr::Extent2Di {
                                        width: target.width as i32,
                                        height: target.height as i32,
                                    },
                                }),
                        )
                })
                .collect()
        } else {
            Vec::new()
        };
        // A flat screen with a see-through surround leaves the rest of the
        // eye image transparent, and the layer says so, so the runtime can
        // show passthrough (or its environment) there as it does around the
        // browser.
        let projection = xr::CompositionLayerProjection::new()
            .space(&space)
            .views(&projection_views);
        let projection = if see_through_now {
            projection.layer_flags(xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA)
        } else {
            projection
        };
        let built: Vec<Built> = overlays
            .iter()
            .map(|o| match o.shape {
                Shape::Quad(size) => {
                    Built::Quad(quad_layer(&space, o.target, o.pose, size, o.blend))
                }
                Shape::Cylinder {
                    radius,
                    angle,
                    aspect,
                } => Built::Cylinder(cylinder_layer(
                    &space, o.target, o.pose, radius, angle, aspect, o.blend,
                )),
            })
            .collect();
        let mut layers: Vec<&xr::CompositionLayerBase<xr::Vulkan>> = Vec::new();
        if !projection_views.is_empty() {
            layers.push(&projection);
        }
        for layer in &built {
            layers.push(match layer {
                Built::Quad(l) => &**l,
                Built::Cylinder(l) => &**l,
            });
        }
        ctx.frame_stream
            .end(state.predicted_display_time, ctx.blend_mode, &layers)?;
    }
    options.quit.store(true, Ordering::Relaxed);
    if let Mode::Playing(playback) = mode {
        // Written here, not in the background: the app is about to exit.
        let point = crate::config::resume_point(playback.position(), playback.duration)
            .map(|s| (s, Some(playback.duration)));
        if let Some(key) = &playing_key
            && playback.current().is_some()
            && let Err(e) = crate::config::save_resume_position(key, point)
        {
            eprintln!("Can't save where the video was left: {e:#}");
        }
        stats.displayed_frames += playback.stats.displayed_frames;
        stats.uploaded_frames += playback.stats.uploaded_frames;
        stats.skipped_frames += playback.stats.skipped_frames;
        stats.media_seconds = playback.stats.media_seconds;
    }
    drop(renderer);
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_hit_round_trips_through_point() {
        let panel = Panel {
            center: [0.3, -0.4, -1.0],
            yaw: 0.5,
            tilt: -0.45,
            size: [1.2, 1.2 * controls::HEIGHT as f32 / controls::WIDTH as f32],
            pixels: [1200, 260],
        };
        for (x, y) in [(600.0, 130.0), (100.0, 40.0), (1150.0, 250.0)] {
            let target = panel.point(x, y, 0.0);
            let len = dot(target, target).sqrt();
            let ray = Ray {
                origin: [0.0; 3],
                direction: scale(target, 1.0 / len),
            };
            let (hx, hy) = panel.hit(&ray).expect("hit");
            assert!(
                (hx - x).abs() < 2.0 && (hy - y).abs() < 2.0,
                "({x},{y}) -> ({hx},{hy})"
            );
        }
    }

    /// +Z (a quad's normal) turned by `q`.
    fn rotate_z(q: xr::Quaternionf) -> [f32; 3] {
        let v = [0.0f32, 0.0, 1.0];
        let t = [
            2.0 * (q.y * v[2] - q.z * v[1]),
            2.0 * (q.z * v[0] - q.x * v[2]),
            2.0 * (q.x * v[1] - q.y * v[0]),
        ];
        [
            v[0] + q.w * t[0] + (q.y * t[2] - q.z * t[1]),
            v[1] + q.w * t[1] + (q.z * t[0] - q.x * t[2]),
            v[2] + q.w * t[2] + (q.x * t[1] - q.y * t[0]),
        ]
    }

    #[test]
    fn curved_screen_outline_faces_the_viewer() {
        let placement = Placement {
            yaw: 0.4,
            pitch: 0.0,
            distance: 3.0,
            curved: true,
            curve: 1.0,
            zoom: 1.0,
            offset: 0.0,
            pivot: [0.0; 3],
        };
        let lines = screen_outline(&placement, [4.0, 2.25], 0.01);
        for (pose, _) in &lines {
            let p = [pose.position.x, pose.position.y, pose.position.z];
            let horizontal = (p[0] * p[0] + p[2] * p[2]).sqrt();
            assert!((horizontal - 3.0).abs() < 1e-3, "on the arc: {p:?}");
            // The normal points back at the viewer (the axis).
            let n = rotate_z(pose.orientation);
            let towards = [-p[0] / horizontal, 0.0, -p[2] / horizontal];
            assert!(dot(n, towards) > 0.999, "{n:?} vs {towards:?}");
        }
        // A gentler curve: a 6 m cylinder whose axis is 3 m behind the
        // viewer, the screen's centre still 3 m ahead.
        let gentle = Placement {
            curve: 0.5,
            ..placement
        };
        let r = gentle.rotation();
        let forward = [-r[2][0], -r[2][1], -r[2][2]];
        let axis = scale(forward, -3.0);
        let lines = screen_outline(&gentle, [4.0, 2.25], 0.01);
        let (mut nearest, mut farthest) = (f32::MAX, 0.0f32);
        for (pose, _) in &lines {
            let p = [pose.position.x, pose.position.y, pose.position.z];
            let off = [p[0] - axis[0], 0.0, p[2] - axis[2]];
            assert!(
                (dot(off, off).sqrt() - 6.0).abs() < 1e-3,
                "on the arc: {p:?}"
            );
            nearest = nearest.min(dot(p, forward));
            farthest = farthest.max(dot(p, forward));
        }
        let hit = video_hit(
            &Ray {
                origin: [0.0; 3],
                direction: forward,
            },
            &gentle,
            Projection::Flat,
            [4.0, 2.25],
        )
        .expect("ahead");
        assert!((dot(hit, forward) - 3.0).abs() < 1e-3, "{hit:?}");
        // The edges bend towards the viewer; the centre is the farthest point.
        assert!(
            farthest <= 3.0 + 1e-3 && nearest < 2.9,
            "{nearest}..{farthest}"
        );
    }

    #[test]
    fn control_bar_hangs_under_the_screen_facing_the_viewer() {
        for (yaw, pitch, curved, zoom) in [
            (0.0, 0.0, false, 1.0),
            (0.6, 0.3, false, 2.0),
            (-0.4, -0.2, true, 1.0),
            (0.0, 0.0, false, 0.4),
        ] {
            let placement = Placement {
                yaw,
                pitch,
                distance: 3.0,
                curved,
                curve: 1.0,
                zoom,
                offset: 0.0,
                pivot: [0.0; 3],
            };
            let screen = [3.2 * zoom, 1.8 * zoom];
            let bar = screen_controls_panel(&placement, screen);
            // Its top edge is below the screen's bottom edge, at the screen's depth.
            let top = bar.point(bar.pixels[0] as f32 / 2.0, 0.0, 0.0);
            let r = placement.rotation();
            let local = [0, 1, 2].map(|i| (0..3).map(|k| r[i][k] * top[k]).sum::<f32>());
            assert!(local[1] < -screen[1] / 2.0, "{local:?}");
            assert!((local[2] + 3.0).abs() < 0.3, "{local:?}");
            // It faces the viewer at the origin.
            let normal = bar.basis()[2];
            let len = dot(bar.center, bar.center).sqrt();
            let towards = scale(bar.center, -1.0 / len);
            assert!(dot(normal, towards) > 0.999, "{normal:?} vs {towards:?}");
            // Readable but never huge.
            assert!((2.39..=3.61).contains(&bar.size[0]), "{:?}", bar.size);
        }
    }

    /// What the viewer sees: the screen (a checkerboard) and the panels on
    /// top of it, ray-cast from the eye at the origin, looking along `look`
    /// (yaw, pitch). Writes a PNG.
    fn render_scene(
        path: &std::path::Path,
        look: (f32, f32),
        placement: &Placement,
        projection: Projection,
        screen: [f32; 2],
        panels: &[(Panel, &crate::ui::canvas::Canvas, bool)],
    ) {
        let (w, h) = (1000u32, 750u32);
        let half = (100f32.to_radians() / 2.0).tan();
        let view = Panel {
            center: [0.0; 3],
            yaw: look.0,
            tilt: look.1,
            size: [1.0, 1.0],
            pixels: [1, 1],
        };
        let [right, up, normal] = view.basis();
        let r = placement.rotation();
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for py in 0..h {
            for px in 0..w {
                let sx = (px as f32 + 0.5) / w as f32 * 2.0 - 1.0;
                let sy = 1.0 - (py as f32 + 0.5) / h as f32 * 2.0;
                let d = add(
                    add(
                        scale(right, sx * half),
                        scale(up, sy * half * h as f32 / w as f32),
                    ),
                    scale(normal, -1.0),
                );
                let d = scale(d, 1.0 / dot(d, d).sqrt());
                let ray = Ray {
                    origin: [0.0; 3],
                    direction: d,
                };
                // Sky above the horizon, floor below.
                let mut c = if d[1] > 0.0 {
                    [20.0, 24.0, 34.0]
                } else {
                    [40.0, 38.0, 36.0]
                };
                if projection == Projection::Equirect180 {
                    // The shader's sphere, on the CPU: a unit sphere in the
                    // placement's frame, the viewpoint moved along -Z.
                    let ld = [0, 1, 2].map(|i| (0..3).map(|k| r[i][k] * d[k]).sum::<f32>());
                    let k = placement.offset;
                    let od = -k * ld[2];
                    let t = -od + (od * od - k * k + 1.0).max(0.0).sqrt();
                    let n = [ld[0] * t, ld[1] * t, ld[2] * t - k];
                    let n = scale(n, 1.0 / dot(n, n).sqrt());
                    let lon = n[0].atan2(-n[2]) / placement.zoom;
                    let lat = n[1].clamp(-1.0, 1.0).asin() / placement.zoom;
                    c = if lon.abs() <= std::f32::consts::FRAC_PI_2 {
                        // 10° cells; the one straight ahead is lighter still.
                        let cell = 10f32.to_radians();
                        let (i, j) = ((lon / cell).floor() as i32, (lat / cell).floor() as i32);
                        if (i == 0 || i == -1) && (j == 0 || j == -1) {
                            [150.0, 170.0, 200.0]
                        } else if (i + j) & 1 == 0 {
                            [90.0, 110.0, 140.0]
                        } else {
                            [70.0, 85.0, 110.0]
                        }
                    } else {
                        [0.0; 3]
                    };
                } else if let Some(p) = video_hit(&ray, placement, Projection::Flat, screen) {
                    let local = [0, 1, 2].map(|i| (0..3).map(|k| r[i][k] * p[k]).sum::<f32>());
                    let u = if placement.curved {
                        let r = placement.curve_radius();
                        local[0].atan2(-(local[2] - (r - placement.distance))) * r
                    } else {
                        local[0]
                    };
                    let checker = ((u / 0.2).floor() + (local[1] / 0.2).floor()) as i32 & 1;
                    c = if checker == 0 {
                        [90.0, 110.0, 140.0]
                    } else {
                        [70.0, 85.0, 110.0]
                    };
                }
                for (panel, canvas, blend) in panels {
                    let Some((x, y)) = panel.hit(&ray) else {
                        continue;
                    };
                    let (x, y) = (
                        (x as u32).min(canvas.width - 1),
                        (y as u32).min(canvas.height - 1),
                    );
                    let i = ((y * canvas.width + x) * 4) as usize;
                    let src = &canvas.pixels[i..i + 4];
                    let a = if *blend { src[3] as f32 / 255.0 } else { 1.0 };
                    for k in 0..3 {
                        c[k] = src[k] as f32 + c[k] * (1.0 - a);
                    }
                }
                let i = ((py * w + px) * 4) as usize;
                pixels[i..i + 4].copy_from_slice(&[c[0] as u8, c[1] as u8, c[2] as u8, 255]);
            }
        }
        let file = std::io::BufWriter::new(std::fs::File::create(path).expect("png"));
        let mut encoder = png::Encoder::new(file, w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&pixels).expect("png data");
    }

    /// Scene previews for checking layout by eye:
    /// `JUST_VIDEO_SCENE_DIR=/tmp/x cargo test scene_previews -- --ignored`
    #[test]
    #[ignore]
    fn scene_previews() {
        let Ok(dir) = std::env::var("JUST_VIDEO_SCENE_DIR") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let mut fonts = Fonts::load().expect("fonts");
        let state = controls::State {
            paused: false,
            position: 754.0,
            duration: 5530.0,
            buffered: vec![(754.0, 760.0)],
            reserve: Some(controls::Reserve {
                on: false,
                held: 0,
                goal: 1 << 30,
            }),
            has_previous: true,
            has_next: true,
            curved: Some(false),
            buffer_button: true,
            format: controls::FORMATS[0],
            favourites: vec![controls::FORMATS[0]],
            subtitle_tracks: vec!["English".into()],
            subtitle: Some(0),
            audio_tracks: vec!["English 5.1".into()],
            audio: Some(0),
            list_page: 0,
            image: Default::default(),
            dialog: None,
            caption_edit: false,
        };
        let bar = controls::render(&state, &mut fonts, controls::Hit::Nothing, None);
        let caption =
            captions::render(&crate::subtitles::Caption::text(SAMPLE_CAPTION), &mut fonts);
        let options = ViewOptions::default();
        for (name, yaw, pitch, curved, curve, zoom, turns, look) in [
            ("flat", 0.0, 0.0, false, 1.0, 1.0, 0, (0.0, -0.2)),
            ("flat-zoomed", 0.0, 0.0, false, 1.0, 2.0, 0, (0.0, -0.3)),
            ("flat-small", 0.0, 0.0, false, 1.0, 0.5, 0, (0.0, -0.1)),
            ("curved", 0.0, 0.0, true, 1.0, 1.0, 0, (0.0, -0.2)),
            ("curved-wide", 0.0, 0.0, true, 1.0, 2.0, 0, (0.0, -0.3)),
            ("curved-gentle", 0.0, 0.0, true, 0.25, 2.0, 0, (0.0, -0.3)),
            ("raised", 0.3, 0.5, false, 1.0, 1.0, 0, (0.3, 0.3)),
            ("rotated", 0.0, 0.0, false, 1.0, 1.0, 1, (0.0, -0.2)),
            (
                "rotated-smallest",
                0.0,
                0.0,
                false,
                1.0,
                0.25,
                1,
                (0.0, -0.1),
            ),
        ] {
            let placement = Placement {
                yaw,
                pitch,
                distance: options.screen_distance,
                curved,
                curve,
                zoom,
                offset: 0.0,
                pivot: [0.0; 3],
            };
            let screen = screen_size(
                (1920, 1080),
                &crate::vr::Layout::flat(crate::vr::Stereo::Mono),
                &options,
                zoom,
                turns,
            );
            let captions = caption_panel(&placement, Projection::Flat, screen, Default::default());
            let bar_panel = screen_controls_panel(&placement, screen);
            render_scene(
                &dir.join(format!("{name}.png")),
                look,
                &placement,
                Projection::Flat,
                screen,
                &[(captions, &caption, true), (bar_panel, &bar, false)],
            );
        }
        // VR180: the left stick's dolly (offset) against the right's zoom.
        // The lighter cell stays ahead; the others grow with either.
        for (name, offset, zoom) in [
            ("vr180", 0.0, 1.0),
            ("vr180-dolly", 0.6, 1.0),
            ("vr180-back", -0.4, 1.0),
            ("vr180-zoom", 0.0, 2.0),
        ] {
            let placement = Placement {
                zoom,
                offset,
                ..Placement::new(&options)
            };
            let screen = screen_size(
                (3840, 1920),
                &crate::vr::Layout::flat(crate::vr::Stereo::Mono),
                &options,
                zoom,
                0,
            );
            let captions = caption_panel(
                &placement,
                Projection::Equirect180,
                screen,
                Default::default(),
            );
            let bar_panel = vr_bar_panel(&vr_bar_placement(&xr::Posef::IDENTITY));
            render_scene(
                &dir.join(format!("{name}.png")),
                (0.0, -0.1),
                &placement,
                Projection::Equirect180,
                screen,
                &[(captions, &caption, true), (bar_panel, &bar, false)],
            );
        }
    }

    #[test]
    fn curved_captions_hug_the_screen() {
        let placement = Placement {
            yaw: 0.3,
            pitch: 0.1,
            distance: 3.0,
            curved: true,
            curve: 1.0,
            zoom: 1.0,
            offset: 0.0,
            pivot: [0.0, -0.5, 0.2],
        };
        let screen = [3.2, 1.8];
        let panel = caption_panel(&placement, Projection::Flat, screen, Default::default());
        // Flat fallback: at the screen's depth, not in front of its nearest part.
        let to_center = [0, 1, 2].map(|i| panel.center[i] - placement.pivot[i]);
        let [_, up, _] = panel.basis();
        let horizontal = dot(to_center, to_center) - dot(to_center, up).powi(2);
        assert!(
            (horizontal.sqrt() - 2.98).abs() < 1e-3,
            "{}",
            horizontal.sqrt()
        );
        let (pose, shape) = caption_cylinder(&placement, &panel);
        let Shape::Cylinder {
            radius,
            angle,
            aspect,
        } = shape
        else {
            panic!("{shape:?}")
        };
        assert!((radius - 2.98).abs() < 1e-5);
        assert!((angle - panel.size[0] / radius).abs() < 1e-5);
        assert!((aspect - panel.size[0] / panel.size[1]).abs() < 1e-5);
        // The axis runs through the pivot, at the caption's height.
        let p = [pose.position.x, pose.position.y, pose.position.z];
        let off = [0, 1, 2].map(|i| p[i] - placement.pivot[i]);
        assert!((dot(off, off).sqrt() - dot(to_center, up).abs()).abs() < 1e-4);
        assert!(
            dot(off, up).abs() > dot(off, off).sqrt() * 0.999,
            "along up"
        );
        // A gentler curve: the axis moves behind the pivot, along the normal.
        let gentle = Placement {
            curve: 0.5,
            ..placement
        };
        let panel = caption_panel(&gentle, Projection::Flat, screen, Default::default());
        let (pose, shape) = caption_cylinder(&gentle, &panel);
        let Shape::Cylinder { radius, .. } = shape else {
            panic!("{shape:?}")
        };
        assert!((radius - 5.98).abs() < 1e-4);
        let [_, up, normal] = panel.basis();
        let p = [pose.position.x, pose.position.y, pose.position.z];
        let off = [0, 1, 2].map(|i| p[i] - gentle.pivot[i]);
        assert!((dot(off, normal) - 3.0).abs() < 1e-3, "3 m behind: {off:?}");
        let sideways = dot(off, off) - dot(off, up).powi(2) - dot(off, normal).powi(2);
        assert!(sideways.abs() < 1e-3);
    }

    #[test]
    fn vr_bar_keeps_its_apparent_size_and_handle_side() {
        let head = xr::Posef {
            position: xr::Vector3f {
                x: 0.0,
                y: 1.6,
                z: 0.0,
            },
            ..xr::Posef::IDENTITY
        };
        let place = vr_bar_placement(&head);
        let bar = vr_bar_panel(&place);
        // Ahead at the control distance, below eye level, facing the head.
        assert!(
            (bar.center[2] + CONTROLS_DISTANCE).abs() < 1e-3,
            "{:?}",
            bar.center
        );
        assert!(bar.center[1] < 1.6 - 0.5, "{:?}", bar.center);
        assert!((bar.size[0] - 1.2 * CONTROLS_DISTANCE).abs() < 1e-3);
        // Pushed farther away it grows to look the same size.
        let far = vr_bar_panel(&Placement {
            distance: place.distance * 2.0,
            ..place
        });
        assert!((far.size[0] - 2.0 * bar.size[0]).abs() < 1e-3);
        // The handle hangs below, or sits above a bar dragged low.
        let [_, up, _] = bar.basis();
        let below = handle_panel(&bar, false);
        let above = handle_panel(&bar, true);
        let d = |h: &Panel| dot([0, 1, 2].map(|i| h.center[i] - bar.center[i]), up);
        assert!(d(&below) < -bar.size[1] / 2.0);
        assert!(d(&above) > bar.size[1] / 2.0);
    }

    #[test]
    fn browser_handle_hangs_below_the_panel() {
        let mut place = browser_placement([0.0, 1.6, 0.0]);
        place.yaw = 0.5;
        place.distance = 3.3;
        let panel = browser_panel(&place, 1.0);
        // Apparent size is kept: 1.5x the distance, 1.5x the size.
        assert!((panel.size[0] - BROWSER_SIZE[0] * 1.5).abs() < 1e-3);
        let handle = handle_panel(&panel, false);
        let [_, up, normal] = panel.basis();
        let d = [0, 1, 2].map(|i| handle.center[i] - panel.center[i]);
        assert!(dot(d, up) < -panel.size[1] / 2.0, "below the bottom edge");
        assert!(dot(d, normal).abs() < 1e-4, "in the panel's plane");
        assert!(handle.size[0] < panel.size[0] / 3.0);
        // It faces the pivot (the head).
        let to_head = scale(
            [0, 1, 2].map(|i| place.pivot[i] - panel.center[i]),
            1.0 / place.distance,
        );
        assert!(dot(normal, to_head) > 0.999, "{normal:?} vs {to_head:?}");
    }

    #[test]
    fn panel_orientation_matches_basis() {
        let panel = Panel {
            center: [0.0; 3],
            yaw: 0.7,
            tilt: -0.4,
            size: [1.0, 1.0],
            pixels: [1, 1],
        };
        let q = panel.orientation();
        // Rotate +Z (the quad's normal) by q and compare with the basis normal.
        let v = [0.0f32, 0.0, 1.0];
        let t = [
            2.0 * (q.y * v[2] - q.z * v[1]),
            2.0 * (q.z * v[0] - q.x * v[2]),
            2.0 * (q.x * v[1] - q.y * v[0]),
        ];
        let rotated = [
            v[0] + q.w * t[0] + (q.y * t[2] - q.z * t[1]),
            v[1] + q.w * t[1] + (q.z * t[0] - q.x * t[2]),
            v[2] + q.w * t[2] + (q.x * t[1] - q.y * t[0]),
        ];
        let normal = panel.basis()[2];
        for i in 0..3 {
            assert!(
                (rotated[i] - normal[i]).abs() < 1e-5,
                "{rotated:?} vs {normal:?}"
            );
        }
    }
}
