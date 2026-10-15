//! Video playback state: a decode thread feeding a small queue, and frame
//! selection by timestamp against the runtime's predicted display time.

use super::renderer::EyeParams;
use crate::media::{Frame, VideoDecoder};
use crate::subtitles::Cues;
use crate::vr::{Layout, Projection, Stereo};
use openxr as xr;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// Presentation settings shared by all videos.
#[derive(Clone)]
pub struct ViewOptions {
    /// Fisheye lens field of view in degrees.
    pub fisheye_fov: f32,
    /// Flat screen width and distance in metres.
    pub screen_width: f32,
    pub screen_distance: f32,
    /// Shader debug view: 0 off, 1 projection UV, 2 raw YUV samples.
    pub debug_view: u32,
}

impl Default for ViewOptions {
    fn default() -> Self {
        Self {
            fisheye_fov: 180.0,
            screen_width: 3.2,
            screen_distance: 3.0,
            debug_view: 0,
        }
    }
}

/// Where the video is placed around the viewer (changed by dragging).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    /// Rotation around the pivot: yaw (left/right) and pitch (up/down), radians.
    pub yaw: f32,
    pub pitch: f32,
    /// Flat/curved screen distance in metres.
    pub distance: f32,
    pub curved: bool,
    /// How much a curved screen bends: 1 wraps it around the viewer (the
    /// cylinder's axis through the pivot), less is gentler (a wider
    /// cylinder, its axis behind the viewer); see `curve_radius`.
    pub curve: f32,
    /// Size multiplier: screen width for flat/curved, magnification for VR180/360.
    pub zoom: f32,
    /// VR180/360: the viewpoint moved along the placement's -Z towards the
    /// picture, as a fraction of the sphere's radius (0 = centre).
    pub offset: f32,
    /// The point the screen orbits: where the head was when it was last
    /// placed or dragged (the reference space's origin is wherever the head
    /// was at the start, which is no use once the viewer has lain down).
    pub pivot: [f32; 3],
}

/// Steepest pitch: nearly overhead or underfoot, short of the pole where
/// yaw would lose its meaning.
pub const MAX_PITCH: f32 = 1.5;

/// How far the viewpoint may move inside a spherical video: back a little
/// (the picture shrinks), or most of the way to the picture.
pub const MIN_OFFSET: f32 = -0.5;
pub const MAX_OFFSET: f32 = 0.85;

impl Placement {
    pub fn new(options: &ViewOptions) -> Self {
        Self {
            yaw: 0.0,
            pitch: 0.0,
            distance: options.screen_distance,
            curved: false,
            curve: 1.0,
            zoom: 1.0,
            offset: 0.0,
            pivot: [0.0; 3],
        }
    }

    /// Radius of the curved screen's cylinder; its centre stays `distance`
    /// away, so the axis is `curve_radius() - distance` behind the pivot.
    pub fn curve_radius(&self) -> f32 {
        self.distance / self.curve.clamp(0.05, 1.0)
    }

    /// Where the flat screen's centre is.
    pub fn center(&self) -> [f32; 3] {
        let r = self.rotation();
        [0, 1, 2].map(|i| self.pivot[i] - self.distance * r[2][i])
    }

    /// Makes `head` the pivot, keeping the screen where it is (so it turns
    /// to face the viewer from now on, and dragging orbits them, not a
    /// point they have moved away from).
    pub fn anchor_at(&mut self, head: [f32; 3]) {
        let c = self.center();
        let v = [c[0] - head[0], c[1] - head[1], c[2] - head[2]];
        let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        if len < 0.3 {
            // Standing in the screen: just move the pivot.
            self.pivot = head;
            return;
        }
        self.yaw = (-v[0]).atan2(-v[2]);
        self.pitch = (v[1] / len)
            .clamp(-1.0, 1.0)
            .asin()
            .clamp(-MAX_PITCH, MAX_PITCH);
        self.distance = len.max(0.8);
        self.pivot = head;
    }

    /// Rotation matrix (columns) of the placement: yaw about +Y, then pitch about +X.
    pub fn rotation(&self) -> [[f32; 3]; 3] {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        // R = Ry(yaw) * Rx(pitch)
        [
            [cy, 0.0, -sy],
            [sy * sp, cp, cy * sp],
            [sy * cp, -sp, cy * cp],
        ]
    }
}

/// Command-line playback extras (testing aids).
#[derive(Clone, Default)]
pub struct PlayOptions {
    /// Write the left eye as PNG once playback reaches this media time.
    pub screenshot: Option<(PathBuf, f64)>,
    /// Stop at this media time (seconds).
    pub duration: Option<f64>,
    /// Frames before this media time (after a seek to the previous keyframe) are skipped.
    pub start: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(m: [[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
        [0, 1, 2].map(|i| (0..3).map(|k| m[k][i] * v[k]).sum())
    }

    #[test]
    fn anchoring_keeps_the_screen_in_place() {
        let mut p = Placement {
            yaw: 0.4,
            pitch: 0.2,
            distance: 3.0,
            curved: false,
            curve: 1.0,
            zoom: 1.0,
            offset: 0.0,
            pivot: [0.0; 3],
        };
        let before = p.center();
        // The viewer has lain down: 1 m lower, half a metre forward.
        p.anchor_at([0.0, -1.0, -0.5]);
        let after = p.center();
        for i in 0..3 {
            assert!(
                (before[i] - after[i]).abs() < 1e-4,
                "{before:?} vs {after:?}"
            );
        }
        assert_eq!(p.pivot, [0.0, -1.0, -0.5]);
        // Dragging straight up from here reaches overhead.
        p.pitch = MAX_PITCH;
        let c = p.center();
        assert!(c[1] - p.pivot[1] > 2.9, "{c:?}");
    }

    #[test]
    fn buffered_ranges_follow_the_playhead() {
        let state = crate::readahead::Buffered {
            position: 1_000_000,
            len: 10_000_000,
            ranges: vec![(1_000_000, 1_500_000), (1_500_000, 2_000_000)],
            reserved: vec![(5_000_000, 5_200_000)],
        };
        // 100 s video: 100 000 bytes per second; playing at 12.5 s.
        let ranges = buffered_seconds(&state, 12.5, 100.0);
        assert_eq!(ranges, vec![(12.0, 23.0), (52.0, 55.0)]);
        assert!(buffered_seconds(&state, 12.5, 0.0).is_empty());
        // Held ahead: the window and the reserve, up to the cap.
        assert_eq!(reserve_progress(&state, 4_000_000), (1_000_000, 4_000_000));
        assert_eq!(reserve_progress(&state, 1 << 30), (1_200_000, 9_000_000));
    }

    #[test]
    fn tall_screens_fit_the_same_box_as_wide_ones() {
        let options = ViewOptions::default();
        let size = |tex, stereo, zoom, turns| {
            screen_size(tex, &Layout::flat(stereo), &options, zoom, turns)
        };
        let close =
            |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 1e-3 && (a[1] - b[1]).abs() < 1e-3;
        // Landscape video keeps the configured width.
        assert!(close(size((1920, 1080), Stereo::Mono, 1.0, 0), [3.2, 1.8]));
        assert!(close(size((1440, 1080), Stereo::Mono, 1.0, 0), [3.2, 2.4]));
        assert!(close(
            size((3840, 1080), Stereo::SideBySide, 1.0, 0),
            [3.2, 1.8]
        ));
        // Turned on its side, or shot upright: as tall as a 4:3 screen, not 5.7 m.
        let turned = size((1920, 1080), Stereo::Mono, 1.0, 1);
        assert!(close(turned, [2.4 * 1080.0 / 1920.0, 2.4]), "{turned:?}");
        assert!(close(size((1080, 1920), Stereo::Mono, 1.0, 0), turned));
        // And zooming out shrinks it further.
        let small = size((1080, 1920), Stereo::Mono, 0.25, 0);
        assert!((small[1] - 0.6).abs() < 1e-3, "{small:?}");
    }

    #[test]
    fn placement_turns_the_screen_direction() {
        let mut p = Placement {
            yaw: std::f32::consts::FRAC_PI_2,
            pitch: 0.0,
            distance: 3.0,
            curved: false,
            curve: 1.0,
            zoom: 1.0,
            offset: 0.0,
            pivot: [0.0; 3],
        };
        // Yaw 90° (left) moves the screen's forward (-Z) to -X.
        let f = apply(p.rotation(), [0.0, 0.0, -1.0]);
        assert!((f[0] + 1.0).abs() < 1e-5 && f[2].abs() < 1e-5, "{f:?}");
        p.yaw = 0.0;
        p.pitch = 0.3;
        let f = apply(p.rotation(), [0.0, 0.0, -1.0]);
        assert!(f[1] > 0.2, "pitch up raises the screen: {f:?}");
    }
}

/// See [`Playback::buffered`].
fn buffered_seconds(
    state: &crate::readahead::Buffered,
    position: f64,
    duration: f64,
) -> Vec<(f64, f64)> {
    if state.len == 0 || duration <= 0.0 {
        return Vec::new();
    }
    let per_byte = duration / state.len as f64;
    let at = |byte: u64| {
        (position + (byte as f64 - state.position as f64) * per_byte).clamp(0.0, duration)
    };
    let mut out: Vec<(f64, f64)> = Vec::new();
    for (a, b) in state.fetched() {
        let (from, to) = (at(a).floor(), at(b).ceil().min(duration));
        if to <= from {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.1 >= from => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    out
}

/// Bytes held from the read position on, and how many the reserve aims
/// for there: the rest of the file, up to `cap`.
fn reserve_progress(state: &crate::readahead::Buffered, cap: u64) -> (u64, u64) {
    let goal = state.len.saturating_sub(state.position).min(cap);
    let held = state.fetched_within(state.position, state.position + goal);
    (held, goal)
}

/// The reserve as the control bar shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReserveStatus {
    /// Filling (or full): the player is paused for it.
    pub on: bool,
    /// Bytes held ahead, and the bytes it aims for.
    pub held: u64,
    pub goal: u64,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct PlayStats {
    pub displayed_frames: u64,
    pub uploaded_frames: u64,
    pub skipped_frames: u64,
    pub xr_frames: u64,
    /// XR frames the runtime asked us to render (false while not visible).
    pub rendered_xr_frames: u64,
    pub media_seconds: f64,
    pub screenshot: Option<String>,
}

enum Decoded {
    Frame(u64, Frame),
    End(u64),
    Failed(u64, String),
}

struct AudioChunk {
    generation: u64,
    /// Time of the first sample, seconds from the start of the video.
    pts: f64,
    samples: Vec<f32>,
}

/// State shared with the audio thread.
struct AudioShared {
    generation: AtomicU64,
    paused: AtomicBool,
    /// Volume 0..=1 as f32 bits.
    volume: AtomicU32,
    /// Media time heard at an instant, for the current generation.
    clock: Mutex<Option<(u64, f64, Instant)>>,
}

impl AudioShared {
    fn heard_now(&self) -> Option<(u64, f64)> {
        let clock = *self.clock.lock().expect("audio clock");
        clock.map(|(generation, position, at)| (generation, position + at.elapsed().as_secs_f64()))
    }
}

fn spawn_audio(
    name: String,
    chunks: mpsc::Receiver<AudioChunk>,
    shared: Arc<AudioShared>,
    stop: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            let mut output = match crate::audio::Output::open(&name) {
                Ok(output) => Some(output),
                Err(e) => {
                    eprintln!("Audio disabled: {e:#}");
                    None
                }
            };
            let channels = crate::audio::CHANNELS as usize;
            let rate = crate::audio::RATE as f64;
            let mut played_generation = 0;
            while !stop.load(Ordering::Relaxed) {
                let chunk = match chunks.recv_timeout(Duration::from_millis(50)) {
                    Ok(chunk) => chunk,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                };
                let Some(out) = output.as_mut() else { continue };
                if chunk.generation != shared.generation.load(Ordering::Relaxed) {
                    continue; // from before a seek
                }
                if chunk.generation != played_generation {
                    out.flush();
                    played_generation = chunk.generation;
                }
                // ~10 ms slices keep pause and seek responsive.
                let slice = (rate as usize / 100) * channels;
                let mut offset = 0;
                while offset < chunk.samples.len() {
                    if stop.load(Ordering::Relaxed)
                        || chunk.generation != shared.generation.load(Ordering::Relaxed)
                    {
                        break;
                    }
                    if shared.paused.load(Ordering::Relaxed) {
                        *shared.clock.lock().expect("audio clock") = None;
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    let end = (offset + slice).min(chunk.samples.len());
                    let level = f32::from_bits(shared.volume.load(Ordering::Relaxed));
                    // Perceptual curve: bar steps sound evenly spaced.
                    let gain = level * level;
                    let scaled: Vec<f32> = chunk.samples[offset..end]
                        .iter()
                        .map(|s| s * gain)
                        .collect();
                    if let Err(e) = out.write(&scaled) {
                        eprintln!("{e:#}");
                        return;
                    }
                    offset = end;
                    let written_until = chunk.pts + (offset / channels) as f64 / rate;
                    let heard = written_until - out.latency();
                    *shared.clock.lock().expect("audio clock") =
                        Some((chunk.generation, heard, Instant::now()));
                }
            }
        })
        .expect("spawn audio thread");
}

/// A seek: generation, target seconds and how it lands.
type SeekCommand = (u64, f64, Landing);

/// Where a seek shows from.
#[derive(Clone, Copy, Debug)]
enum Landing {
    /// The keyframe at or before the target, at once; for a jump forward
    /// from here, never behind where playback was.
    Keyframe { from: Option<f64> },
    /// The target itself: decode on to it, skipping what nothing refers to.
    Exact,
}

struct DecodeThread {
    frames: mpsc::Receiver<Decoded>,
    control: mpsc::Sender<SeekCommand>,
    requested: Arc<AtomicU64>,
    /// Which embedded subtitle track to decode (None: none).
    subtitle_track: mpsc::Sender<Option<usize>>,
    /// Which audio track to play (a seek follows).
    audio_track: mpsc::Sender<usize>,
}

fn spawn_decoder(
    mut decoder: VideoDecoder,
    start: f64,
    audio: Option<mpsc::Sender<AudioChunk>>,
    stop: Arc<AtomicBool>,
    embedded_cues: Arc<Mutex<Cues>>,
) -> DecodeThread {
    let (subtitle_track, subtitle_changes) = mpsc::channel::<Option<usize>>();
    let (audio_track, audio_changes) = mpsc::channel::<usize>();
    // A few frames of slack absorb decode jitter; more would only cost memory.
    let (tx, frames) = mpsc::sync_channel(4);
    let (control, commands) = mpsc::channel::<SeekCommand>();
    let requested = Arc::new(AtomicU64::new(0));
    let pending = requested.clone();
    std::thread::Builder::new()
        .name("decode".into())
        .spawn(move || {
            let (mut generation, mut start) = (0u64, start);
            // After a seek: where it was aimed, and (for a jump forward) where
            // playback was, until the first frame decides where to show from.
            let mut landing: Option<(f64, Option<f64>)> = None;
            // Continuing part way: jump there first, or every frame from the
            // top would be decoded and dropped.
            if start > 0.0 {
                if let Err(e) = decoder.seek(start) {
                    eprintln!("Seek to {start:.1}s failed: {e:#}");
                }
                landing = Some((start, None));
            }
            'decode: loop {
                // Before seeks: the seek that follows a switch restarts the new track.
                while let Ok(track) = audio_changes.try_recv() {
                    if !decoder.select_audio(track) {
                        eprintln!("Audio: can't decode track {track}");
                    }
                }
                while let Ok((g, t, how)) = commands.try_recv() {
                    if let Err(e) = decoder.seek(t) {
                        eprintln!("Seek to {t:.1}s failed: {e:#}");
                    }
                    (generation, start) = (g, t);
                    landing = match how {
                        Landing::Keyframe { from } => Some((t, from)),
                        Landing::Exact => {
                            decoder.skip_nonref_until(t);
                            None
                        }
                    };
                }
                while let Ok(track) = subtitle_changes.try_recv() {
                    embedded_cues.lock().expect("cues").clear();
                    if !decoder.select_subtitle(track) {
                        eprintln!("Subtitles: can't decode track {track:?}");
                    }
                }
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let message = match decoder.next_frame() {
                    Ok(Some(frame)) => Decoded::Frame(generation, frame),
                    Ok(None) => Decoded::End(generation),
                    Err(e) => Decoded::Failed(generation, format!("{e:#}")),
                };
                // Seeking lands on a keyframe before the target. Show from
                // there at once rather than decoding (and dropping) up to the
                // target, unless that keyframe is behind where a jump forward
                // started: then only the target moves forward.
                if let (Decoded::Frame(_, frame), Some((target, from))) = (&message, landing)
                    && let Some(key) = frame.pts()
                {
                    start = match from {
                        Some(from) if key <= from => target,
                        _ => key,
                    };
                    landing = None;
                }
                let cues = decoder.take_subtitles();
                if !cues.is_empty() {
                    let mut shared = embedded_cues.lock().expect("cues");
                    for cue in cues {
                        shared.insert(cue);
                    }
                }
                if let Some(audio) = &audio {
                    while let Some((samples, pts)) = decoder.take_audio(crate::audio::CHANNELS) {
                        let pts = pts.unwrap_or(start);
                        // After a seek, skip sound from before the target.
                        let skip = (((start - pts) * crate::audio::RATE as f64).max(0.0) as usize
                            * crate::audio::CHANNELS as usize)
                            .min(samples.len());
                        if skip < samples.len() {
                            let pts = pts
                                + (skip / crate::audio::CHANNELS as usize) as f64
                                    / crate::audio::RATE as f64;
                            let _ = audio.send(AudioChunk {
                                generation,
                                pts,
                                samples: samples[skip..].to_vec(),
                            });
                        }
                    }
                }
                // Frames before where we show from are decoded and dropped.
                if let Decoded::Frame(_, frame) = &message
                    && frame.pts().is_some_and(|t| t < start)
                {
                    continue;
                }
                let finished = !matches!(message, Decoded::Frame(..));
                let mut message = Some(message);
                while let Some(m) = message.take() {
                    match tx.try_send(m) {
                        Ok(()) => {}
                        Err(mpsc::TrySendError::Full(m)) => {
                            if stop.load(Ordering::Relaxed) {
                                return;
                            }
                            if pending.load(Ordering::Relaxed) != generation {
                                continue 'decode; // a seek is waiting; drop this frame
                            }
                            message = Some(m);
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(mpsc::TrySendError::Disconnected(_)) => return,
                    }
                }
                if finished {
                    // Wait for a seek (e.g. back from the end) or shutdown.
                    while pending.load(Ordering::Relaxed) == generation {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        })
        .expect("spawn decode thread");
    DecodeThread {
        frames,
        control,
        requested,
        subtitle_track,
        audio_track,
    }
}

/// "Korean · Surround", "English 5.1"…
fn audio_label(index: usize, t: &crate::media::AudioTrackInfo) -> String {
    let mut label = t
        .language
        .as_deref()
        .map(crate::subtitles::language_name)
        .unwrap_or_else(|| format!("Track {}", index + 1));
    match t.title.as_deref() {
        Some(title) if !title.eq_ignore_ascii_case(&label) => label = format!("{label} · {title}"),
        _ => {
            let channels = match t.channels {
                1 => "mono".to_string(),
                2 => "stereo".to_string(),
                6 => "5.1".to_string(),
                8 => "7.1".to_string(),
                0 => String::new(),
                n => format!("{n} ch"),
            };
            if !channels.is_empty() {
                label = format!("{label} {channels}");
            }
        }
    }
    label
}

/// A subtitle track the viewer can pick.
enum SubtitleSource {
    /// Index into the file's subtitle streams; cues arrive while decoding.
    Embedded(usize),
    /// A .srt file found with the video; fetched when first chosen.
    External {
        path: crate::library::Path,
        cues: Option<Cues>,
    },
}

struct SubtitleTrack {
    label: String,
    source: SubtitleSource,
}

pub struct Playback {
    pub layout: Layout,
    /// Picture corrections and rotation.
    pub image: crate::config::ImageAdjust,
    /// The track CC turns back on.
    last_subtitle: usize,
    subtitle_tracks: Vec<SubtitleTrack>,
    subtitle: Option<usize>,
    embedded_cues: Arc<Mutex<Cues>>,
    /// Briefly shown instead of subtitles ("Subtitles: English"), until then.
    subtitle_notice: Option<(String, Instant)>,
    /// The external track whose file is being fetched.
    subtitle_fetching: Option<usize>,
    audio_labels: Vec<String>,
    audio_track: Option<usize>,
    decode: DecodeThread,
    audio: Option<Arc<AudioShared>>,
    stop: Arc<AtomicBool>,
    generation: u64,
    current: Option<Frame>,
    next: Option<Frame>,
    ended: bool,
    pub error: Option<String>,
    fps: f64,
    pub duration: f64,
    /// Jumps decode on to their target (see [`config::seek_exact`]).
    exact_seeks: bool,
    last_pts: f64,
    /// Display time (ns) corresponding to media time 0.
    clock_start: Option<i64>,
    paused_at: Option<i64>,
    pub stats: PlayStats,
    /// The network reader's buffer, when playing from a share.
    buffer: Option<crate::readahead::BufferWatch>,
    /// The reader's reserve (see [`Playback::toggle_reserve`]).
    reserve: Option<Arc<crate::readahead::Reserve>>,
    reserve_on: bool,
}

impl Playback {
    /// Starts decoding (and sound, when the file has audio) at `start` seconds.
    pub fn start(mut decoder: VideoDecoder, layout: Layout, start: f64, volume: f32) -> Self {
        // Embedded text subtitles; the file's default track is on from the start.
        let embedded: Vec<(usize, &crate::media::SubtitleTrackInfo)> = decoder
            .info()
            .subtitles
            .iter()
            .enumerate()
            .filter(|(_, t)| t.supported)
            .collect();
        let subtitle_tracks: Vec<SubtitleTrack> = embedded
            .iter()
            .map(|(i, t)| {
                let mut label = t
                    .language
                    .as_deref()
                    .map(crate::subtitles::language_name)
                    .unwrap_or_else(|| format!("Track {}", i + 1));
                if let Some(title) = t
                    .title
                    .as_deref()
                    .filter(|title| !title.eq_ignore_ascii_case(&label))
                {
                    label = format!("{label} ({title})");
                }
                SubtitleTrack {
                    label,
                    source: SubtitleSource::Embedded(*i),
                }
            })
            .collect();
        let subtitle = embedded
            .iter()
            .position(|(_, t)| t.default)
            .or((!embedded.is_empty()).then_some(0));
        if let Some(SubtitleSource::Embedded(track)) = subtitle.map(|i| &subtitle_tracks[i].source)
        {
            decoder.select_subtitle(Some(*track));
        }
        let embedded_cues = Arc::new(Mutex::new(Cues::default()));
        let info = decoder.info();
        let fps = info.video.as_ref().map_or(30.0, |v| v.fps.max(1.0));
        let duration = info.duration_seconds;
        let stop = Arc::new(AtomicBool::new(false));
        let has_audio = decoder.enable_audio(crate::audio::RATE, crate::audio::CHANNELS);
        let audio_labels: Vec<String> = decoder
            .info()
            .audio_tracks
            .iter()
            .enumerate()
            .map(|(i, t)| audio_label(i, t))
            .collect();
        let audio_track = decoder.info().audio_track.filter(|_| has_audio);
        let audio = has_audio.then(|| {
            Arc::new(AudioShared {
                generation: AtomicU64::new(0),
                paused: AtomicBool::new(false),
                volume: AtomicU32::new(volume.clamp(0.0, 1.0).to_bits()),
                clock: Mutex::new(None),
            })
        });
        let audio_tx = audio.as_ref().map(|shared| {
            let (tx, rx) = mpsc::channel();
            spawn_audio("Video".into(), rx, shared.clone(), stop.clone());
            tx
        });
        Self {
            layout,
            image: Default::default(),
            last_subtitle: subtitle.unwrap_or(0),
            decode: spawn_decoder(
                decoder,
                start,
                audio_tx,
                stop.clone(),
                embedded_cues.clone(),
            ),
            subtitle_tracks,
            subtitle,
            embedded_cues,
            subtitle_notice: None,
            subtitle_fetching: None,
            audio_labels,
            audio_track,
            audio,
            stop,
            generation: 0,
            current: None,
            next: None,
            ended: false,
            error: None,
            fps,
            duration,
            exact_seeks: crate::config::seek_exact(),
            last_pts: start - 1.0 / fps,
            clock_start: None,
            paused_at: None,
            stats: PlayStats::default(),
            buffer: None,
            reserve: None,
            reserve_on: false,
        }
    }

    /// Shows the network reader's buffer on the seek bar.
    pub fn set_buffer(&mut self, buffer: crate::readahead::BufferWatch) {
        self.buffer = Some(buffer);
    }

    /// Offers buffering ahead while paused (see [`Playback::toggle_reserve`]).
    pub fn set_reserve(&mut self, reserve: Arc<crate::readahead::Reserve>) {
        self.reserve = Some(reserve);
    }

    /// Buffering ahead: on, the video pauses and the reader fetches on from
    /// here, up to a gigabyte, so the rest can play without the network.
    /// Off again by hand, or by playing on (see [`Playback::toggle_pause`]):
    /// fetching ahead while playing once starved playback over Wi-Fi.
    pub fn toggle_reserve(&mut self, now: i64) {
        let Some(reserve) = &self.reserve else {
            return;
        };
        // (A reserve that gave up on a failing server shows as off.)
        if self.reserve_on && reserve.is_active() {
            self.set_reserve_on(false);
            return;
        }
        if !self.paused() {
            self.toggle_pause(now);
        }
        self.set_reserve_on(true);
    }

    fn set_reserve_on(&mut self, on: bool) {
        self.reserve_on = on;
        if let Some(reserve) = &self.reserve {
            reserve.set_active(on);
        }
    }

    pub fn reserve_status(&self) -> Option<ReserveStatus> {
        let (reserve, buffer) = (self.reserve.as_ref()?, self.buffer.as_ref()?);
        let state = buffer.lock().ok()?.clone();
        let (held, goal) = reserve_progress(&state, reserve.capacity());
        // Whole MiB: the bar needn't redraw for every read.
        let mib = |bytes: u64| bytes >> 20 << 20;
        Some(ReserveStatus {
            // The reserve gives up when the server keeps failing.
            on: self.reserve_on && reserve.is_active(),
            held: mib(held.min(goal)),
            goal: mib(goal).max(1),
        })
    }

    /// Stretches of the video already fetched, in seconds (sorted). Bytes
    /// map to time at the file's average rate, anchored at the playhead
    /// (so the fetched window always starts where playback is), in whole
    /// seconds so the bar needn't redraw for every block.
    pub fn buffered(&self) -> Vec<(f64, f64)> {
        let Some(buffer) = &self.buffer else {
            return Vec::new();
        };
        let state = match buffer.lock() {
            Ok(state) => state.clone(),
            Err(_) => return Vec::new(),
        };
        buffered_seconds(&state, self.position(), self.duration)
    }

    /// Adds the subtitle files found with the video; the first one is shown
    /// (a file placed next to a video is usually there to be used). Their
    /// text is fetched when chosen: see [`Playback::take_subtitle_fetch`].
    pub fn add_external_subtitles(&mut self, files: Vec<crate::library::ExternalSubtitles>) {
        if files.is_empty() {
            return;
        }
        let count = files.len();
        let external = files.into_iter().map(|f| SubtitleTrack {
            label: f.label,
            source: SubtitleSource::External {
                path: f.path,
                cues: None,
            },
        });
        self.subtitle_tracks.splice(0..0, external);
        self.subtitle = self.subtitle.map(|i| i + count);
        self.select_subtitle(Some(0), false);
    }

    /// The external track to fetch, once: the chosen one whose file hasn't
    /// been read yet. The owner fetches it and calls [`Playback::subtitle_loaded`].
    pub fn take_subtitle_fetch(&mut self) -> Option<(usize, crate::library::Path)> {
        let i = self.subtitle?;
        if self.subtitle_fetching == Some(i) {
            return None;
        }
        let path = match &self.subtitle_tracks[i].source {
            SubtitleSource::External { path, cues: None } => path.clone(),
            _ => return None,
        };
        self.subtitle_fetching = Some(i);
        self.notice("Loading subtitles…".to_string(), Duration::from_secs(20));
        Some((i, path))
    }

    /// The text of external track `index`, or why it couldn't be read.
    pub fn subtitle_loaded(
        &mut self,
        index: usize,
        result: Result<Vec<crate::subtitles::Cue>, String>,
    ) {
        if self.subtitle_fetching == Some(index) {
            self.subtitle_fetching = None;
        }
        let Some(track) = self.subtitle_tracks.get_mut(index) else {
            return;
        };
        let SubtitleSource::External { cues, .. } = &mut track.source else {
            return;
        };
        let chosen = self.subtitle == Some(index);
        match result {
            Ok(loaded) => {
                *cues = Some(Cues::new(loaded));
                if chosen {
                    let text = format!("Subtitles: {}", track.label);
                    self.notice(text, Duration::from_millis(1800));
                }
            }
            Err(e) => {
                eprintln!("Subtitles: can't read {}: {e}", track.label);
                if chosen {
                    self.select_subtitle(None, false);
                }
                self.notice(format!("Can't load subtitles: {e}"), Duration::from_secs(3));
            }
        }
    }

    pub fn has_subtitles(&self) -> bool {
        !self.subtitle_tracks.is_empty()
    }

    pub fn subtitles_on(&self) -> bool {
        self.subtitle.is_some()
    }

    /// CC: subtitles off, or back on with the last track shown.
    pub fn toggle_subtitles(&mut self) {
        if self.subtitle_tracks.is_empty() {
            return;
        }
        let next = match self.subtitle {
            Some(_) => None,
            None => Some(self.last_subtitle.min(self.subtitle_tracks.len() - 1)),
        };
        self.select_subtitle(next, true);
    }

    fn select_subtitle(&mut self, index: Option<usize>, announce: bool) {
        if let Some(i) = index {
            self.last_subtitle = i;
        }
        let embedded =
            |i: Option<usize>, tracks: &[SubtitleTrack]| match i.map(|i| &tracks[i].source) {
                Some(SubtitleSource::Embedded(t)) => Some(*t),
                _ => None,
            };
        let (before, after) = (
            embedded(self.subtitle, &self.subtitle_tracks),
            embedded(index, &self.subtitle_tracks),
        );
        if before != after {
            let _ = self.decode.subtitle_track.send(after);
        }
        self.subtitle = index;
        // A file still to be fetched announces itself once it has arrived.
        let fetching = matches!(
            index.map(|i| &self.subtitle_tracks[i].source),
            Some(SubtitleSource::External { cues: None, .. })
        );
        if announce && !fetching {
            let text = match index {
                Some(i) => format!("Subtitles: {}", self.subtitle_tracks[i].label),
                None => "Subtitles off".to_string(),
            };
            self.notice(text, Duration::from_millis(1800));
        }
    }

    /// Shows `text` where subtitles go for a while.
    pub fn notice(&mut self, text: String, duration: Duration) {
        self.subtitle_notice = Some((text, Instant::now() + duration));
    }

    pub fn audio_labels(&self) -> &[String] {
        &self.audio_labels
    }

    pub fn audio_index(&self) -> Option<usize> {
        self.audio_track
    }

    /// Switches to audio track `index`, continuing from the current picture.
    pub fn set_audio_track(&mut self, index: usize) {
        if self.audio_track.is_none()
            || self.audio_track == Some(index)
            || index >= self.audio_labels.len()
        {
            return;
        }
        let _ = self.decode.audio_track.send(index);
        self.audio_track = Some(index);
        self.notice(
            format!("Audio: {}", self.audio_labels[index]),
            Duration::from_millis(1800),
        );
        self.seek(self.position());
    }

    pub fn subtitle_labels(&self) -> Vec<String> {
        self.subtitle_tracks
            .iter()
            .map(|t| t.label.clone())
            .collect()
    }

    pub fn subtitle_index(&self) -> Option<usize> {
        self.subtitle
    }

    /// Shows track `index` (None: off), with a short notice.
    pub fn set_subtitle(&mut self, index: Option<usize>) {
        if index.is_none_or(|i| i < self.subtitle_tracks.len()) {
            self.select_subtitle(index, true);
        }
    }

    /// Off → each track in turn → off.
    pub fn cycle_subtitles(&mut self) {
        if self.subtitle_tracks.is_empty() {
            return;
        }
        let next = match self.subtitle {
            None => Some(0),
            Some(i) if i + 1 < self.subtitle_tracks.len() => Some(i + 1),
            Some(_) => None,
        };
        self.select_subtitle(next, true);
    }

    /// The subtitle for the frame on screen (or a short notice after switching).
    pub fn caption(&self) -> Option<crate::subtitles::Caption> {
        if let Some((text, until)) = &self.subtitle_notice
            && Instant::now() < *until
        {
            return Some(crate::subtitles::Caption::text(text.clone()));
        }
        let time = self.position();
        match &self.subtitle_tracks.get(self.subtitle?)?.source {
            SubtitleSource::External {
                cues: Some(cues), ..
            } => cues.caption(time),
            SubtitleSource::External { cues: None, .. } => None,
            SubtitleSource::Embedded(_) => self.embedded_cues.lock().expect("cues").caption(time),
        }
    }

    pub fn current(&self) -> Option<&Frame> {
        self.current.as_ref()
    }

    /// Waiting for the first picture after opening or seeking.
    pub fn loading(&self) -> bool {
        self.clock_start.is_none() && !self.ended && self.error.is_none()
    }

    pub fn paused(&self) -> bool {
        self.paused_at.is_some()
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    pub fn set_volume(&self, volume: f32) {
        if let Some(audio) = &self.audio {
            audio
                .volume
                .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        }
    }

    /// Media time shown at display time `now` (ns), once the clock started.
    pub fn media_time(&self, now: i64) -> Option<f64> {
        let now = self.paused_at.unwrap_or(now);
        self.clock_start.map(|start| (now - start) as f64 / 1e9)
    }

    /// Current position for the UI: the shown frame's time.
    pub fn position(&self) -> f64 {
        self.last_pts.max(0.0)
    }

    pub fn toggle_pause(&mut self, now: i64) {
        match self.paused_at.take() {
            // Shift the clock so playback continues where it stopped.
            Some(at) => {
                if let Some(start) = &mut self.clock_start {
                    *start += now - at;
                }
                // Playing on ends buffering ahead; what it holds stays.
                self.set_reserve_on(false);
            }
            None => self.paused_at = Some(now),
        }
        if let Some(audio) = &self.audio {
            audio.paused.store(self.paused(), Ordering::Relaxed);
        }
    }

    /// Jumps to `seconds`; the current picture stays until the new one
    /// arrives: the keyframe at or before the target (a jump forward never
    /// lands behind where it started), or with exact seeks the target itself.
    pub fn seek(&mut self, seconds: f64) {
        let target = seconds.clamp(0.0, (self.duration - 0.5).max(0.0));
        self.generation += 1;
        self.decode
            .requested
            .store(self.generation, Ordering::Relaxed);
        if let Some(audio) = &self.audio {
            audio.generation.store(self.generation, Ordering::Relaxed);
            *audio.clock.lock().expect("audio clock") = None;
        }
        let landing = if self.exact_seeks {
            Landing::Exact
        } else {
            Landing::Keyframe {
                from: (target > self.last_pts).then_some(self.last_pts),
            }
        };
        let _ = self.decode.control.send((self.generation, target, landing));
        self.next = None;
        self.ended = false;
        self.error = None;
        self.clock_start = None;
        self.last_pts = target;
        // When paused, playback stays paused and shows the new position
        // (see `advance`).
    }

    /// Moves to the newest decoded frame due at `now`; true if it changed.
    pub fn advance(&mut self, now: i64) -> bool {
        if self.paused() && self.clock_start.is_some() {
            return false;
        }
        self.sync_to_audio(now);
        let mut media_time = self.media_time(now);
        let mut changed = false;
        loop {
            if self.next.is_none() && !self.ended {
                match self.decode.frames.try_recv() {
                    Ok(Decoded::Frame(g, frame)) if g == self.generation => self.next = Some(frame),
                    Ok(Decoded::End(g)) if g == self.generation => self.ended = true,
                    Ok(Decoded::Failed(g, e)) if g == self.generation => {
                        eprintln!("Decoding stopped: {e}");
                        self.error = Some(e);
                        self.ended = true;
                    }
                    Ok(_) => continue, // from before a seek
                    Err(mpsc::TryRecvError::Disconnected) => self.ended = true,
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            let Some(candidate) = self.next.as_ref() else {
                break;
            };
            let pts = candidate.pts().unwrap_or(self.last_pts + 1.0 / self.fps);
            if media_time.is_some_and(|t| pts > t) {
                break;
            }
            if self.clock_start.is_none() {
                // The first frame starts the clock: it is due exactly now.
                self.clock_start = Some(now - (pts * 1e9) as i64);
                if let Some(at) = &mut self.paused_at {
                    *at = now;
                }
                media_time = Some(pts);
            }
            if changed {
                self.stats.skipped_frames += 1;
            }
            self.last_pts = pts;
            self.current = self.next.take();
            changed = true;
            if self.paused() {
                break; // paused after a seek: show just the target frame
            }
        }
        if let Some(t) = media_time {
            self.stats.media_seconds = t;
        }
        changed
    }

    /// Keeps the video clock on the audio clock (what is actually heard).
    fn sync_to_audio(&mut self, now: i64) {
        let (Some(audio), Some(start)) = (&self.audio, self.clock_start.as_mut()) else {
            return;
        };
        if self.paused_at.is_some() {
            return;
        }
        let Some((generation, heard)) = audio.heard_now() else {
            return;
        };
        if generation != self.generation {
            return;
        }
        // Frames are shown at the predicted display time, slightly after now.
        const DISPLAY_LEAD: f64 = 0.02;
        let video = (now - *start) as f64 / 1e9;
        let error = video - (heard + DISPLAY_LEAD);
        let correction = if error.abs() > 0.25 {
            error
        } else {
            error * 0.1
        };
        *start += (correction * 1e9) as i64;
    }

    /// True once the stream ended and its last frame has been shown for a second.
    pub fn finished(&self, now: i64) -> bool {
        self.ended
            && self.next.is_none()
            && self.media_time(now).is_none_or(|t| t > self.last_pts + 1.0)
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn quat_to_columns(q: xr::Quaternionf) -> [[f32; 4]; 3] {
    let (x, y, z, w) = (q.x, q.y, q.z, q.w);
    [
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y + w * z),
            2.0 * (x * z - w * y),
            0.0,
        ],
        [
            2.0 * (x * y - w * z),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z + w * x),
            0.0,
        ],
        [
            2.0 * (x * z + w * y),
            2.0 * (y * z - w * x),
            1.0 - 2.0 * (x * x + y * y),
            0.0,
        ],
    ]
}

fn mat_mul_t(a: [[f32; 3]; 3], b: [[f32; 4]; 3]) -> [[f32; 4]; 3] {
    // Aᵀ·B with A, B given as columns.
    let mut out = [[0.0; 4]; 3];
    for (j, col) in b.iter().enumerate() {
        for (i, a_col) in a.iter().enumerate() {
            out[j][i] = (0..3).map(|k| a_col[k] * col[k]).sum();
        }
    }
    out
}

/// Tallest flat screen at zoom 1, as a share of its width: 4:3 and wider
/// videos are `screen_width` wide, taller ones (portrait, or turned on their
/// side) this high, so zooming can make them small enough to see whole.
pub const SCREEN_BOX_ASPECT: f32 = 0.75;

/// Flat screen size (metres) for a video of `tex` pixels in `layout`,
/// turned `quarter_turns` clockwise.
pub fn screen_size(
    tex: (u32, u32),
    layout: &Layout,
    options: &ViewOptions,
    zoom: f32,
    quarter_turns: u8,
) -> [f32; 2] {
    // One eye's part of the frame (stretched back when half-packed).
    let mut aspect = crate::vr::eye_aspect(tex.0, tex.1, layout);
    if quarter_turns % 2 == 1 {
        aspect = 1.0 / aspect.max(0.01);
    }
    let aspect = aspect.max(0.1);
    let width = options.screen_width * zoom * (aspect * SCREEN_BOX_ASPECT).min(1.0);
    [width, width / aspect]
}

/// Shader parameters for one eye.
pub fn eye_params(
    view: &xr::View,
    eye: usize,
    layout: &Layout,
    tex: (u32, u32),
    options: &ViewOptions,
    placement: &Placement,
    quarter_turns: u8,
) -> EyeParams {
    let (w, h) = (tex.0 as f32, tex.1 as f32);
    let screen = screen_size(tex, layout, options, placement.zoom, quarter_turns);
    // Express the eye in the placement's frame: the shader's screen/sphere is
    // fixed there, so rotating the placement moves the video around the viewer.
    let placed = placement.rotation();
    let p = view.pose.position;
    let [px, py, pz] = [
        p.x - placement.pivot[0],
        p.y - placement.pivot[1],
        p.z - placement.pivot[2],
    ];
    let origin = mat_mul_t(placed, [[px, py, pz, 0.0], [0.0; 4], [0.0; 4]])[0];
    let curved = layout.projection == Projection::Flat && placement.curved;
    let spherical = layout.projection != Projection::Flat;
    EyeParams {
        rot: mat_mul_t(placed, quat_to_columns(view.pose.orientation)),
        tan: [
            view.fov.angle_left.tan(),
            view.fov.angle_right.tan(),
            view.fov.angle_down.tan(),
            view.fov.angle_up.tan(),
        ],
        origin: [origin[0], origin[1], origin[2], eye as f32],
        mode: [
            match layout.projection {
                Projection::Flat if curved => 4.0,
                Projection::Flat => 0.0,
                Projection::Equirect180 => 1.0,
                Projection::Equirect360 => 2.0,
                Projection::Fisheye180 => 3.0,
            },
            match layout.stereo {
                Stereo::Mono => 0.0,
                Stereo::SideBySide => 1.0,
                Stereo::TopBottom => 2.0,
            },
            if curved {
                placement.curve_radius()
            } else {
                0.0
            },
            options.fisheye_fov.to_radians(),
        ],
        screen: [
            screen[0],
            screen[1],
            placement.distance,
            if spherical { placement.offset } else { 0.0 },
        ],
        tex: [w, h, options.debug_view as f32, placement.zoom],
        // Only a screen has a surround, left transparent for the runtime to
        // fill (passthrough, or its environment); spherical video fills the view.
        surround: [
            if layout.projection == Projection::Flat {
                1.0
            } else {
                0.0
            },
            0.0,
            0.0,
            0.0,
        ],
    }
}
