//! Subtitle cues: parsing SubRip (.srt) files, cleaning up the markup of
//! embedded ASS/SSA text, and finding what to show at a given time. DVD and
//! Blu-ray subtitles are pictures ([`Bitmap`]) instead of text.

use std::sync::Arc;

/// One subtitle: shown from `start` to `end` (seconds from video start).
/// A cue with neither text nor image is an erase event: whatever shows at
/// `start` ends there (Blu-ray subtitles have no durations).
#[derive(Clone, Debug, PartialEq)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub image: Option<Arc<Bitmap>>,
}

impl Cue {
    pub fn text(start: f64, end: f64, text: impl Into<String>) -> Self {
        Self {
            start,
            end,
            text: text.into(),
            image: None,
        }
    }

    fn is_erase(&self) -> bool {
        self.text.is_empty() && self.image.is_none()
    }
}

/// A picture subtitle: premultiplied RGBA, placed at (`x`, `y`) in a
/// `frame_width` × `frame_height` picture (its own, e.g. 720×576 for DVDs).
#[derive(Debug, PartialEq)]
pub struct Bitmap {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    pub frame_width: u32,
    pub frame_height: u32,
}

/// What to show now: text, a picture, or both.
#[derive(Clone, Debug)]
pub struct Caption {
    pub text: Option<String>,
    pub image: Option<Arc<Bitmap>>,
}

impl Caption {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            image: None,
        }
    }
}

/// Cheap: pictures compare by identity (they are shared, never copied).
impl PartialEq for Caption {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
            && match (&self.image, &other.image) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}

/// Cues sorted by start time.
#[derive(Clone, Debug, Default)]
pub struct Cues {
    cues: Vec<Cue>,
}

impl Cues {
    pub fn new(mut cues: Vec<Cue>) -> Self {
        cues.sort_by(|a, b| a.start.total_cmp(&b.start));
        Self { cues }
    }

    pub fn len(&self) -> usize {
        self.cues.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }

    pub fn clear(&mut self) {
        self.cues.clear();
    }

    /// Adds a cue unless it is already there (embedded cues repeat after
    /// seeking back); an erase event ends the cues showing at its time.
    pub fn insert(&mut self, cue: Cue) {
        if cue.is_erase() {
            let t = cue.start;
            let from = self.cues.partition_point(|c| c.start < t - 60.0);
            for c in &mut self.cues[from..] {
                if c.start < t && c.end > t {
                    c.end = t;
                }
            }
            return;
        }
        let at = self.cues.partition_point(|c| c.start < cue.start);
        if self.cues[at..]
            .iter()
            .take_while(|c| c.start == cue.start)
            .any(|c| c.text == cue.text && c.image == cue.image)
        {
            return;
        }
        self.cues.insert(at, cue);
    }

    /// What to show at `time`: the text of every active cue (oldest first)
    /// and the newest active picture.
    pub fn caption(&self, time: f64) -> Option<Caption> {
        // Cues rarely last over a minute; look back that far only.
        let from = self.cues.partition_point(|c| c.start < time - 60.0);
        let to = self.cues.partition_point(|c| c.start <= time);
        let active: Vec<&Cue> = self.cues[from..to]
            .iter()
            .filter(|c| c.end > time)
            .collect();
        let texts: Vec<&str> = active
            .iter()
            .filter(|c| !c.text.is_empty())
            .map(|c| c.text.as_str())
            .collect();
        let image = active.iter().rev().find_map(|c| c.image.clone());
        let text = (!texts.is_empty()).then(|| texts.join("\n"));
        (text.is_some() || image.is_some()).then_some(Caption { text, image })
    }

    /// The text to show at `time`.
    pub fn at(&self, time: f64) -> Option<String> {
        self.caption(time).and_then(|c| c.text)
    }
}

/// `00:01:02,345` (or `.345`) in seconds.
fn srt_time(text: &str) -> Option<f64> {
    let text = text.trim();
    let (clock, fraction) = text.split_once([',', '.']).unwrap_or((text, "0"));
    let mut parts = clock.split(':').map(|p| p.trim().parse::<f64>());
    let (h, m, s) = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    let digits = fraction
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    let frac = if digits.is_empty() {
        0.0
    } else {
        digits.parse::<f64>().ok()? / 10f64.powi(digits.len() as i32)
    };
    Some(h * 3600.0 + m * 60.0 + s + frac)
}

/// Parses a SubRip file. Damaged blocks are skipped, not fatal.
pub fn parse_srt(text: &str) -> Vec<Cue> {
    let text = text
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut cues = Vec::new();
    for block in text.split("\n\n") {
        let mut lines = block.lines().skip_while(|l| l.trim().is_empty());
        let mut first = lines.next();
        // The counter line is optional in practice.
        if first.is_some_and(|l| !l.contains("-->")) {
            first = lines.next();
        }
        let Some((start, end)) = first.and_then(|l| l.split_once("-->")) else {
            continue;
        };
        // The end may be followed by position hints ("X1:…").
        let end = end.split_whitespace().next().unwrap_or("");
        let (Some(start), Some(end)) = (srt_time(start), srt_time(end)) else {
            continue;
        };
        let body: Vec<&str> = lines.collect();
        let text = clean_markup(&body.join("\n"));
        if !text.is_empty() {
            cues.push(Cue::text(start, end, text));
        }
    }
    cues
}

/// Plain text from SubRip HTML-ish tags (`<i>`, `<font …>`) and ASS markup
/// (`{\an8}` override blocks, `\N` line breaks, `\h` hard spaces).
pub fn clean_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if text.contains('}') => {
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                }
            }
            '<' => {
                // Only the tags SubRip files use: <i>, <b>, <u>, <s>, <font …>.
                let rest: String = chars.clone().take(80).collect();
                let tag = rest
                    .split('>')
                    .next()
                    .filter(|_| rest.contains('>'))
                    .unwrap_or("");
                let name = tag
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let starts_tag = tag.starts_with(|c: char| c.is_ascii_alphabetic() || c == '/');
                if starts_tag && matches!(name.as_str(), "i" | "b" | "u" | "s" | "font") {
                    for _ in 0..=tag.chars().count() {
                        chars.next();
                    }
                } else {
                    out.push(c);
                }
            }
            '\\' => match chars.peek() {
                Some('N') | Some('n') => {
                    chars.next();
                    out.push('\n');
                }
                Some('h') => {
                    chars.next();
                    out.push(' ');
                }
                _ => out.push(c),
            },
            _ => out.push(c),
        }
    }
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Decodes a subtitle file: UTF-8 (with or without BOM), UTF-16 with BOM,
/// otherwise Windows-1252, which covers most old Western .srt files.
pub fn decode_text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xff, 0xfe]) {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xfe, 0xff]) {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => bytes.iter().map(|&b| windows_1252(b)).collect(),
    }
}

fn windows_1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}',
        'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9f => HIGH[(b - 0x80) as usize],
        _ => b as char,
    }
}

/// The video's name without its extension.
pub fn stem(video: &str) -> &str {
    video.rsplit_once('.').map_or(video, |(s, _)| s)
}

/// Whether `candidate` is a subtitle file for the video `video`: the same
/// name with `.srt`, optionally with a language in between (`movie.en.srt`).
pub fn is_sidecar(video: &str, candidate: &str) -> bool {
    let stem = stem(video).to_lowercase();
    let candidate = candidate.to_lowercase();
    let Some(name) = candidate.strip_suffix(".srt") else {
        return false;
    };
    name == stem
        || name
            .strip_prefix(&stem)
            .is_some_and(|rest| rest.starts_with('.') && rest.len() <= 12)
}

/// A folder of subtitle files beside the videos (`Subs`, `Subtitles`);
/// season packs keep one subfolder per episode in it.
pub fn is_subs_folder(name: &str) -> bool {
    name.eq_ignore_ascii_case("subs") || name.eq_ignore_ascii_case("subtitles")
}

pub fn is_srt(name: &str) -> bool {
    name.to_lowercase().ends_with(".srt")
}

/// How a subtitle file is listed: its language ("Norwegian" for `29_nor.srt`
/// or `movie.no.srt`, "English" for `2_English.srt`), "Subtitles" for a
/// bare `movie.srt`, or whatever else the name says.
pub fn sidecar_label(video: &str, file: &str) -> String {
    let name = match file.rsplit_once('.') {
        Some((n, ext)) if ext.eq_ignore_ascii_case("srt") => n,
        _ => file,
    };
    let name = match name.split_once('_') {
        Some((n, rest)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => rest,
        _ => name,
    };
    let stem = stem(video);
    if name.eq_ignore_ascii_case(stem) {
        return "Subtitles".to_string();
    }
    let token = match name.get(..stem.len()) {
        Some(head) if head.eq_ignore_ascii_case(stem) => {
            name[stem.len()..].strip_prefix('.').unwrap_or(name)
        }
        _ => name,
    };
    language_name(token)
}

/// Sort key: numbered files (`2_English.srt`) in their number's order first,
/// then the rest, shortest name first (`movie.srt` before `movie.en.srt`).
pub fn sidecar_order(file: &str) -> (bool, u32, usize, String) {
    let number = file
        .split_once('_')
        .and_then(|(n, _)| n.parse::<u32>().ok());
    (
        number.is_none(),
        number.unwrap_or(0),
        file.len(),
        file.to_lowercase(),
    )
}

/// Repeated labels get a count: "English", "English (2)".
pub fn distinct_labels(labels: Vec<String>) -> Vec<String> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    labels
        .into_iter()
        .map(|label| {
            let n = seen.entry(label.clone()).or_insert(0);
            *n += 1;
            if *n == 1 {
                label
            } else {
                format!("{label} ({n})")
            }
        })
        .collect()
}

/// "English", "Svenska"… for common ISO 639-2 codes; the code otherwise.
pub fn language_name(code: &str) -> String {
    let name = match code.to_ascii_lowercase().as_str() {
        "eng" | "en" => "English",
        "swe" | "sv" => "Swedish",
        "nor" | "nob" | "no" => "Norwegian",
        "dan" | "da" => "Danish",
        "fin" | "fi" => "Finnish",
        "ger" | "deu" | "de" => "German",
        "fre" | "fra" | "fr" => "French",
        "spa" | "es" => "Spanish",
        "ita" | "it" => "Italian",
        "por" | "pt" => "Portuguese",
        "dut" | "nld" | "nl" => "Dutch",
        "pol" | "pl" => "Polish",
        "rus" | "ru" => "Russian",
        "jpn" | "ja" => "Japanese",
        "kor" | "ko" => "Korean",
        "chi" | "zho" | "zh" => "Chinese",
        "ara" | "ar" => "Arabic",
        "est" | "et" => "Estonian",
        "lav" | "lv" => "Latvian",
        "lit" | "lt" => "Lithuanian",
        "ukr" | "uk" => "Ukrainian",
        "hin" | "hi" => "Hindi",
        "tam" | "ta" => "Tamil",
        "tel" | "te" => "Telugu",
        "cze" | "ces" | "cs" => "Czech",
        "slo" | "slk" | "sk" => "Slovak",
        "slv" | "sl" => "Slovenian",
        "hun" | "hu" => "Hungarian",
        "rum" | "ron" | "ro" => "Romanian",
        "bul" | "bg" => "Bulgarian",
        "hrv" | "hr" => "Croatian",
        "srp" | "sr" => "Serbian",
        "gre" | "ell" | "el" => "Greek",
        "tur" | "tr" => "Turkish",
        "heb" | "he" => "Hebrew",
        "tha" | "th" => "Thai",
        "vie" | "vi" => "Vietnamese",
        "ind" | "id" => "Indonesian",
        "may" | "msa" | "ms" => "Malay",
        "ice" | "isl" | "is" => "Icelandic",
        "cat" | "ca" => "Catalan",
        "per" | "fas" | "fa" => "Persian",
        _ => return code.to_string(),
    };
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_srt_with_markup() {
        let srt = "\u{feff}1\r\n00:00:02,000 --> 00:00:04,400\r\n<font color=\"#00ffff\">Good morning,</font>\r\n<i>everyone</i>\r\n\r\n2\r\n00:01:04,400 --> 00:01:05.960 X1:10\r\nSee you later.\r\n\r\n";
        let cues = parse_srt(srt);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0], Cue::text(2.0, 4.4, "Good morning,\neveryone"));
        assert!((cues[1].end - 65.96).abs() < 1e-9);
    }

    #[test]
    fn skips_damaged_blocks() {
        let cues = parse_srt("1\nnot a time\nhi\n\n2\n00:00:01,000 --> 00:00:02,000\nok\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "ok");
    }

    #[test]
    fn cleans_ass_markup() {
        assert_eq!(
            clean_markup(r"{\an8}Top line\NSecond{\i1} word{\i0}"),
            "Top line\nSecond word"
        );
        assert_eq!(clean_markup("a < b and c > d"), "a < b and c > d");
    }

    #[test]
    fn finds_active_cues() {
        let mut cues = Cues::new(vec![Cue::text(1.0, 3.0, "one"), Cue::text(2.0, 4.0, "two")]);
        cues.insert(Cue::text(2.0, 4.0, "two"));
        assert_eq!(cues.len(), 2, "duplicates are ignored");
        assert_eq!(cues.at(0.5), None);
        assert_eq!(cues.at(2.5).as_deref(), Some("one\ntwo"));
        assert_eq!(cues.at(3.5).as_deref(), Some("two"));
    }

    #[test]
    fn erase_events_end_pictures() {
        let bitmap = Arc::new(Bitmap {
            rgba: vec![255; 4],
            width: 1,
            height: 1,
            x: 0,
            y: 0,
            frame_width: 720,
            frame_height: 576,
        });
        let mut cues = Cues::default();
        cues.insert(Cue {
            start: 1.0,
            end: 11.0,
            text: String::new(),
            image: Some(bitmap.clone()),
        });
        assert!(cues.caption(5.0).is_some_and(|c| c.image.is_some()));
        cues.insert(Cue::text(3.0, 3.0, ""));
        assert!(cues.caption(5.0).is_none(), "erased at 3 s");
        assert!(cues.caption(2.0).is_some());
        cues.insert(Cue {
            start: 1.0,
            end: 11.0,
            text: String::new(),
            image: Some(bitmap),
        });
        assert_eq!(cues.len(), 1, "a repeat after seeking back is ignored");
    }

    #[test]
    fn matches_sidecar_names() {
        assert!(is_sidecar("Show S01E02.mp4", "Show S01E02.srt"));
        assert!(is_sidecar("movie.mkv", "MOVIE.en.SRT"));
        assert!(!is_sidecar("Show S01E02.mp4", "Show S01E03.srt"));
        assert!(!is_sidecar("movie.mkv", "movie.mkv"));
    }

    #[test]
    fn labels_sidecars_by_language() {
        let v = "Show.S01E01.1080p.WEBRip.x265-RARBG[eztv.re].mp4";
        assert_eq!(sidecar_label(v, "29_nor.srt"), "Norwegian");
        assert_eq!(sidecar_label(v, "2_English.srt"), "English");
        assert_eq!(sidecar_label(v, "34_slo.SRT"), "Slovak");
        assert_eq!(sidecar_label(v, "4_Arabic.srt"), "Arabic");
        assert_eq!(
            sidecar_label(v, "Show.S01E01.1080p.WEBRip.x265-RARBG[eztv.re].en.srt"),
            "English"
        );
        assert_eq!(
            sidecar_label(v, "show.s01e01.1080p.webrip.x265-rarbg[eztv.re].srt"),
            "Subtitles"
        );
        assert_eq!(sidecar_label("movie.mkv", "movie.forced.srt"), "forced");
    }

    #[test]
    fn numbered_sidecars_keep_release_order() {
        let mut names = vec![
            "9_Danish.srt",
            "34_slo.srt",
            "2_English.srt",
            "Show.srt",
            "Show.en.srt",
        ];
        names.sort_by_key(|n| sidecar_order(n));
        assert_eq!(
            names,
            [
                "2_English.srt",
                "9_Danish.srt",
                "34_slo.srt",
                "Show.srt",
                "Show.en.srt"
            ]
        );
    }

    #[test]
    fn repeated_labels_are_numbered() {
        let labels = ["English", "English", "Danish", "English"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            distinct_labels(labels),
            ["English", "English (2)", "Danish", "English (3)"]
        );
    }

    #[test]
    fn subs_folder_names() {
        assert!(is_subs_folder("Subs"));
        assert!(is_subs_folder("subs"));
        assert!(is_subs_folder("Subtitles"));
        assert!(!is_subs_folder("Subsonic"));
        assert!(is_srt("2_English.SRT"));
        assert!(!is_srt("notes.txt"));
    }

    #[test]
    fn decodes_legacy_text() {
        assert_eq!(decode_text(b"caf\xe9 \x93hi\x94"), "café “hi”");
        assert_eq!(decode_text("déjà".as_bytes()), "déjà");
    }
}
