//! Font files read in place, glyphs rasterized on demand.
//!
//! fontdue parses and flattens every glyph of a font when it opens it, which
//! for a CJK font is well over 100 MiB of heap. Here the font file is mapped
//! read-only (only the pages of the glyphs drawn are read) and a glyph is
//! outlined and rasterized when first needed, with the same exact-area
//! coverage algorithm fontdue uses.

use std::io;
use ttf_parser::{Face, GlyphId, OutlineBuilder};

/// Layout and size of one rasterized glyph (fontdue's `Metrics`, reduced to
/// what the UI uses).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    /// Whole pixels from the pen to the bitmap's left edge.
    pub xmin: i32,
    /// Whole pixels from the baseline to the bitmap's bottom edge.
    pub ymin: i32,
    pub width: usize,
    pub height: usize,
    pub advance_width: f32,
}

/// A font (one face of a collection) backed by a mapped file.
pub struct Font {
    face: Face<'static>,
    units_per_em: f32,
}

/// Maps `path` read-only for the rest of the process. The mapping is never
/// undone: fonts live as long as the program does.
fn map_file(path: &str) -> io::Result<&'static [u8]> {
    use std::os::fd::AsRawFd;
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len() as usize;
    if len > 0 {
        // SAFETY: a private read-only mapping of an open file; the slice
        // is never mutated and the mapping is never released.
        unsafe {
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            );
            if ptr != libc::MAP_FAILED {
                return Ok(std::slice::from_raw_parts(ptr as *const u8, len));
            }
        }
    }
    // Empty or unmappable: read it instead.
    Ok(Box::leak(std::fs::read(path)?.into_boxed_slice()))
}

impl Font {
    /// Opens face `index` of the font file at `path`.
    pub fn open(path: &str, index: u32) -> io::Result<Self> {
        let data = map_file(path)?;
        let face = Face::parse(data, index)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let units_per_em = face.units_per_em() as f32;
        if units_per_em <= 0.0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "no em size"));
        }
        Ok(Self { face, units_per_em })
    }

    /// The glyph for `ch`, or 0 (the font's "missing" glyph) if it has none.
    pub fn glyph_index(&self, ch: char) -> u16 {
        self.face.glyph_index(ch).map_or(0, |g| g.0)
    }

    /// Metrics and coverage (0-255, row by row from the top) of `ch` at `px`.
    pub fn rasterize(&self, ch: char, px: f32) -> (Metrics, Vec<u8>) {
        let id = GlyphId(self.glyph_index(ch));
        let scale = px / self.units_per_em;
        let advance_width = self
            .face
            .glyph_hor_advance(id)
            .map_or(0.0, |a| a as f32 * scale);
        let empty = Metrics {
            xmin: 0,
            ymin: 0,
            width: 0,
            height: 0,
            advance_width,
        };
        if px <= 0.0 {
            return (empty, Vec::new());
        }
        let mut outline = Outline::new(scale);
        if self.face.outline_glyph(id, &mut outline).is_none() || outline.lines.is_empty() {
            return (empty, Vec::new());
        }
        let b = outline.bounds;
        let (width, height) = (b.xmax - b.xmin, b.ymax - b.ymin);
        // The bitmap's origin is the outline's bounds floored to whole pixels
        // (fontdue's convention, which the layout code relies on).
        let mut offset_x = fract(b.xmin);
        let mut offset_y = fract(1.0 - fract(height) - fract(b.ymin));
        if offset_x < 0.0 {
            offset_x += 1.0;
        }
        if offset_y < 0.0 {
            offset_y += 1.0;
        }
        let metrics = Metrics {
            xmin: b.xmin.floor() as i32,
            ymin: b.ymin.floor() as i32,
            width: (width + offset_x).ceil() as usize,
            height: (height + offset_y).ceil() as usize,
            advance_width,
        };
        let mut raster = Raster::new(metrics.width, metrics.height);
        for &(x0, y0, x1, y1) in &outline.lines {
            // To bitmap space: x right from the left edge, y down from the top.
            raster.line(
                x0 - b.xmin + offset_x,
                b.ymax - y0 + offset_y,
                x1 - b.xmin + offset_x,
                b.ymax - y1 + offset_y,
            );
        }
        (metrics, raster.coverage())
    }
}

fn fract(v: f32) -> f32 {
    v - v.trunc()
}

#[derive(Clone, Copy)]
struct Bounds {
    xmin: f32,
    ymin: f32,
    xmax: f32,
    ymax: f32,
}

/// A glyph outline scaled to pixels (y up), curves flattened to lines.
struct Outline {
    scale: f32,
    lines: Vec<(f32, f32, f32, f32)>,
    bounds: Bounds,
    start: (f32, f32),
    last: (f32, f32),
}

/// How far a flattened curve may stray from the real one, in pixels.
const FLATNESS: f32 = 0.05;

impl Outline {
    fn new(scale: f32) -> Self {
        Self {
            scale,
            lines: Vec::new(),
            bounds: Bounds {
                xmin: f32::MAX,
                ymin: f32::MAX,
                xmax: f32::MIN,
                ymax: f32::MIN,
            },
            start: (0.0, 0.0),
            last: (0.0, 0.0),
        }
    }

    fn point(&self, x: f32, y: f32) -> (f32, f32) {
        (x * self.scale, y * self.scale)
    }

    fn push(&mut self, to: (f32, f32)) {
        let from = self.last;
        // Horizontal edges cover nothing.
        if from.1 != to.1 {
            self.lines.push((from.0, from.1, to.0, to.1));
            for (x, y) in [from, to] {
                self.bounds.xmin = self.bounds.xmin.min(x);
                self.bounds.xmax = self.bounds.xmax.max(x);
                self.bounds.ymin = self.bounds.ymin.min(y);
                self.bounds.ymax = self.bounds.ymax.max(y);
            }
        }
        self.last = to;
    }
}

impl OutlineBuilder for Outline {
    fn move_to(&mut self, x: f32, y: f32) {
        self.start = self.point(x, y);
        self.last = self.start;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.point(x, y);
        self.push(p);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (p0, p1, p2) = (self.last, self.point(x1, y1), self.point(x, y));
        // Uniform subdivision into n pieces strays at most |p0 - 2 p1 + p2| / (4 n^2).
        let d = ((p0.0 - 2.0 * p1.0 + p2.0).powi(2) + (p0.1 - 2.0 * p1.1 + p2.1).powi(2)).sqrt();
        let n = ((d / (4.0 * FLATNESS)).sqrt().ceil() as usize).clamp(1, 64);
        for i in 1..n {
            let t = i as f32 / n as f32;
            let u = 1.0 - t;
            self.push((
                u * u * p0.0 + 2.0 * u * t * p1.0 + t * t * p2.0,
                u * u * p0.1 + 2.0 * u * t * p1.1 + t * t * p2.1,
            ));
        }
        self.push(p2);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (p0, p1, p2, p3) = (
            self.last,
            self.point(x1, y1),
            self.point(x2, y2),
            self.point(x, y),
        );
        let second = |a: (f32, f32), b: (f32, f32), c: (f32, f32)| {
            ((a.0 - 2.0 * b.0 + c.0).powi(2) + (a.1 - 2.0 * b.1 + c.1).powi(2)).sqrt()
        };
        let m = second(p0, p1, p2).max(second(p1, p2, p3));
        // |B''| <= 6 m, so n pieces stray at most 6 m / (8 n^2).
        let n = ((0.75 * m / FLATNESS).sqrt().ceil() as usize).clamp(1, 64);
        for i in 1..n {
            let t = i as f32 / n as f32;
            let u = 1.0 - t;
            let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
            self.push((
                a * p0.0 + b * p1.0 + c * p2.0 + d * p3.0,
                a * p0.1 + b * p1.1 + c * p2.1 + d * p3.1,
            ));
        }
        self.push(p3);
    }

    fn close(&mut self) {
        if self.last != self.start {
            let start = self.start;
            self.push(start);
        }
        self.last = self.start;
    }
}

/// Signed-area accumulation buffer: each edge adds the area it covers to the
/// pixels it crosses, and a running sum along a row gives the coverage.
struct Raster {
    width: usize,
    height: usize,
    stride: usize,
    area: Vec<f32>,
}

impl Raster {
    fn new(width: usize, height: usize) -> Self {
        // Two spare cells per row take what an edge on the right border spills.
        let stride = width + 2;
        Self {
            width,
            height,
            stride,
            area: vec![0.0; stride * height],
        }
    }

    fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        if self.width == 0 || self.height == 0 || y0 == y1 {
            return;
        }
        let (dir, x0, y0, x1, y1) = if y0 < y1 {
            (1.0, x0, y0, x1, y1)
        } else {
            (-1.0, x1, y1, x0, y0)
        };
        let dxdy = (x1 - x0) / (y1 - y0);
        let mut x = x0;
        if y0 < 0.0 {
            x -= y0 * dxdy;
        }
        let top = y0.max(0.0);
        let bottom = y1.min(self.height as f32);
        let wmax = self.width as f32;
        let first = top as usize;
        let last = (bottom.ceil() as usize).min(self.height);
        for y in first..last {
            let row = y * self.stride;
            let dy = ((y + 1) as f32).min(y1) - (y as f32).max(y0);
            let xnext = x + dxdy * dy;
            let d = dy * dir;
            let (a, b) = if x < xnext { (x, xnext) } else { (xnext, x) };
            let (a, b) = (a.clamp(0.0, wmax), b.clamp(0.0, wmax));
            let a_floor = a.floor();
            let ai = a_floor as usize;
            let b_ceil = b.ceil();
            let bi = b_ceil as usize;
            if bi <= ai + 1 {
                let xmf = 0.5 * (a + b) - a_floor;
                self.area[row + ai] += d - d * xmf;
                self.area[row + ai + 1] += d * xmf;
            } else {
                let s = 1.0 / (b - a);
                let af = a - a_floor;
                let a0 = 0.5 * s * (1.0 - af) * (1.0 - af);
                let bf = b - b_ceil + 1.0;
                let am = 0.5 * s * bf * bf;
                self.area[row + ai] += d * a0;
                if bi == ai + 2 {
                    self.area[row + ai + 1] += d * (1.0 - a0 - am);
                } else {
                    let a1 = s * (1.5 - af);
                    self.area[row + ai + 1] += d * (a1 - a0);
                    for xi in ai + 2..bi - 1 {
                        self.area[row + xi] += d * s;
                    }
                    let a2 = a1 + (bi - ai - 3) as f32 * s;
                    self.area[row + bi - 1] += d * (1.0 - a2 - am);
                }
                self.area[row + bi] += d * am;
            }
            x = xnext;
        }
    }

    fn coverage(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.width * self.height);
        for y in 0..self.height {
            let mut sum = 0.0f32;
            for x in 0..self.width {
                sum += self.area[y * self.stride + x];
                out.push((sum.abs() * 255.9).clamp(0.0, 255.0) as u8);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LATIN: &[&str] = &[
        "/usr/share/fonts/noto/NotoSans-Medium.ttf",
        "/usr/share/fonts/google-noto/NotoSans-Medium.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Medium.ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ];
    const CJK: &[&str] = &[
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        "/usr/share/fonts/opentype/ipafont-gothic/ipag.ttf",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Medium.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    ];

    fn first(paths: &[&str]) -> Option<String> {
        paths
            .iter()
            .find(|p| std::path::Path::new(p).exists())
            .map(|p| p.to_string())
    }

    /// Mean and worst coverage difference against fontdue, plus bitmap size
    /// and position differences, over `text` at several sizes.
    fn compare(path: &str, text: &str) -> (f64, u32, i32) {
        let bytes = std::fs::read(path).unwrap();
        let reference = fontdue::Font::from_bytes(
            bytes,
            fontdue::FontSettings {
                collection_index: 0,
                scale: 48.0,
                ..Default::default()
            },
        )
        .unwrap();
        let font = Font::open(path, 0).unwrap();
        let (mut sum, mut count, mut worst, mut geometry) = (0u64, 0u64, 0u32, 0i32);
        for size in [14.0f32, 17.5, 22.0, 30.0, 41.0] {
            for ch in text.chars() {
                let (rm, rb) = reference.rasterize(ch, size);
                let (m, b) = font.rasterize(ch, size);
                assert_eq!(m.advance_width, rm.advance_width, "advance of {ch:?}");
                geometry = geometry
                    .max((m.xmin - rm.xmin).abs())
                    .max((m.ymin - rm.ymin).abs())
                    .max((m.width as i32 - rm.width as i32).abs())
                    .max((m.height as i32 - rm.height as i32).abs());
                if (m.width, m.height, m.xmin, m.ymin) != (rm.width, rm.height, rm.xmin, rm.ymin) {
                    continue;
                }
                for (a, r) in b.iter().zip(&rb) {
                    let d = (*a as i32 - *r as i32).unsigned_abs();
                    sum += d as u64;
                    count += 1;
                    worst = worst.max(d);
                }
            }
        }
        (sum as f64 / count.max(1) as f64, worst, geometry)
    }

    #[test]
    fn latin_glyphs_match_fontdue() {
        let Some(path) = first(LATIN) else { return };
        let (mean, worst, geometry) =
            compare(&path, "AaBbgjQRSWxyz0123456789 ,.:;!?%&@éÄßñ\u{2014}…");
        assert!(geometry <= 1, "bitmap box off by {geometry}");
        assert!(mean < 1.5, "mean difference {mean}");
        assert!(worst <= 64, "worst difference {worst}");
    }

    #[test]
    fn cjk_glyphs_match_fontdue() {
        let Some(path) = first(CJK) else { return };
        let (mean, worst, geometry) = compare(&path, "日本語の動画ファイル한국어中文字幕");
        assert!(geometry <= 1, "bitmap box off by {geometry}");
        assert!(mean < 2.5, "mean difference {mean}");
        assert!(worst <= 96, "worst difference {worst}");
    }

    #[test]
    fn missing_characters_use_the_notdef_glyph_and_space_is_empty() {
        let Some(path) = first(LATIN) else { return };
        let font = Font::open(&path, 0).unwrap();
        let (space, bits) = font.rasterize(' ', 20.0);
        assert!(bits.is_empty() && space.advance_width > 0.0);
        assert_eq!(font.glyph_index('\u{10FFFF}'), 0);
        let (m, b) = font.rasterize('\u{10FFFF}', 20.0);
        assert_eq!(b.len(), m.width * m.height);
    }

    #[test]
    fn a_filled_square_covers_exactly_its_pixels() {
        let mut r = Raster::new(4, 4);
        for (x0, y0, x1, y1) in [
            (0.5, 0.5, 2.5, 0.5),
            (2.5, 0.5, 2.5, 2.5),
            (2.5, 2.5, 0.5, 2.5),
            (0.5, 2.5, 0.5, 0.5),
        ] {
            r.line(x0, y0, x1, y1);
        }
        let c = r.coverage();
        // The centre 1x1 block is fully covered, edges half, corners a quarter.
        assert_eq!(c[5], 255);
        assert!((127..=128).contains(&c[1]));
        assert!((63..=64).contains(&c[0]));
        assert_eq!(c[3], 0);
        assert_eq!(c[15], 0);
    }
}
