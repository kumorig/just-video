//! List thumbnails kept across runs: `thumbnails/<hash>.png` next to the
//! settings, each at display size (about 110 KB raw, far less as PNG). The
//! name hashes everything the picture depends on (file, size, modification
//! time, layout, thumbnail size), so a changed file or layout just misses and
//! the stale file ages out (anything unused for 24 hours is deleted, and never
//! served). Errors are never kept: a broken read may work
//! next time.

use crate::media::Thumb;
use crate::vr::Layout;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Size of every thumbnail; rows are laid out around it.
pub const THUMB_W: u32 = 220;
pub const THUMB_H: u32 = 124;

/// A thumbnail not used for this long is deleted, and never served.
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// Files beyond this (oldest modification time first) are deleted.
const MAX_FILES: usize = 5000;
/// Pruning runs again after this many stores.
const PRUNE_EVERY: usize = 200;

/// FNV-1a, 64 bit. Written out because `DefaultHasher` may change between
/// Rust releases, which would orphan every saved thumbnail.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// The cache name for a file's thumbnail (16 hex digits); `file_key` is
/// `library::file_key`. Only what changes the picture is in it.
pub fn key(file_key: &str, size: u64, modified: u64, layout: &Layout) -> String {
    let text = format!(
        "{file_key}\0{size}\0{modified}\0{:?}/{:?}\0{THUMB_W}x{THUMB_H}",
        layout.projection, layout.stereo
    );
    format!("{:016x}", fnv1a(text.as_bytes()))
}

pub struct ThumbCache {
    dir: PathBuf,
    stores: usize,
}

impl ThumbCache {
    /// The cache in the config directory (created when first stored to).
    pub fn open() -> Option<Self> {
        let dir = crate::config::dir().ok()?.join("thumbnails");
        Some(Self::at(dir))
    }

    /// A cache in `dir` (for tests).
    pub fn at(dir: PathBuf) -> Self {
        Self { dir, stores: 0 }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.png"))
    }

    /// The stored thumbnail, if there is a readable one of the right size.
    pub fn load(&self, key: &str) -> Option<Thumb> {
        let path = self.path(key);
        // Last used more than MAX_AGE ago: a miss, and gone.
        let used = std::fs::metadata(&path).ok()?.modified().ok()?;
        if SystemTime::now()
            .duration_since(used)
            .is_ok_and(|age| age > MAX_AGE)
        {
            eprintln!("Thumbnails: dropping an expired thumbnail");
            let _ = std::fs::remove_file(&path);
            return None;
        }
        let file = std::io::BufReader::new(std::fs::File::open(&path).ok()?);
        let mut reader = png::Decoder::new(file).read_info().ok()?;
        let mut rgba = vec![0; reader.output_buffer_size()?];
        let info = reader.next_frame(&mut rgba).ok()?;
        let right = (info.width, info.height) == (THUMB_W, THUMB_H)
            && info.color_type == png::ColorType::Rgba
            && info.bit_depth == png::BitDepth::Eight;
        if right {
            // Used now: pruning drops what was not used for longest, not
            // what was made first.
            let _ = std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_modified(SystemTime::now()));
        }
        right.then(|| {
            rgba.truncate(info.buffer_size());
            Thumb {
                width: info.width,
                height: info.height,
                rgba,
            }
        })
    }

    /// Saves a thumbnail (written aside, then renamed, so a reader never
    /// sees half a file); prunes now and then. A failure only costs making
    /// it again.
    pub fn store(&mut self, key: &str, thumb: &Thumb) {
        if let Err(e) = self.write(key, thumb) {
            eprintln!("Thumbnails: can't save: {e:#}");
            return;
        }
        self.stores += 1;
        if self.stores.is_multiple_of(PRUNE_EVERY) {
            self.prune_older_than(MAX_AGE);
            self.prune(MAX_FILES);
        }
    }

    fn write(&self, key: &str, thumb: &Thumb) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!(".{key}.tmp"));
        let file = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        let mut encoder = png::Encoder::new(file, thumb.width, thumb.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(&thumb.rgba)?;
        std::fs::rename(&tmp, self.path(key))?;
        Ok(())
    }

    /// Drops what is older than [`MAX_AGE`], then keeps the newest
    /// [`MAX_FILES`] files.
    pub fn prune_old(&self) {
        // Half-written files from a stop in mid-save (nothing writes yet).
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().ends_with(".tmp") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        self.prune_older_than(MAX_AGE);
        self.prune(MAX_FILES);
    }

    /// Deletes thumbnails last used more than `age` ago.
    pub fn prune_older_than(&self, age: Duration) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let now = SystemTime::now();
        for e in entries.flatten() {
            let is_png = Path::new(&e.file_name())
                .extension()
                .is_some_and(|x| x == "png");
            let used = e.metadata().ok().and_then(|m| m.modified().ok());
            if is_png && used.is_some_and(|u| now.duration_since(u).is_ok_and(|a| a > age)) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    /// Deletes the oldest files beyond `keep`.
    pub fn prune(&self, keep: usize) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                Path::new(&e.file_name())
                    .extension()
                    .is_some_and(|x| x == "png")
            })
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .collect();
        if files.len() <= keep {
            return;
        }
        files.sort();
        let excess = files.len() - keep;
        for (_, path) in files.into_iter().take(excess) {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vr::{Evidence, Projection, Stereo};

    fn layout() -> Layout {
        Layout {
            projection: Projection::Flat,
            stereo: Stereo::Mono,
            projection_from: Evidence::Aspect,
            stereo_from: Evidence::Aspect,
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jv-thumbs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn picture(seed: u8) -> Thumb {
        let rgba = (0..THUMB_W * THUMB_H * 4)
            .map(|i| (i as u8).wrapping_mul(seed))
            .collect();
        Thumb {
            width: THUMB_W,
            height: THUMB_H,
            rgba,
        }
    }

    #[test]
    fn fnv_is_stable() {
        // Published FNV-1a 64 test vectors: names on disk must never change.
        assert_eq!(fnv1a(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a(b"a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn key_follows_what_changes_the_picture() {
        let base = key("smb://nas/media/a.mp4", 100, 7, &layout());
        assert_eq!(base.len(), 16);
        assert_eq!(base, key("smb://nas/media/a.mp4", 100, 7, &layout()));
        assert_ne!(base, key("smb://nas/media/b.mp4", 100, 7, &layout()));
        assert_ne!(base, key("smb://nas/media/a.mp4", 101, 7, &layout()));
        assert_ne!(base, key("smb://nas/media/a.mp4", 100, 8, &layout()));
        let mut other = layout();
        other.projection = Projection::Equirect180;
        assert_ne!(base, key("smb://nas/media/a.mp4", 100, 7, &other));
        let mut other = layout();
        other.stereo = Stereo::SideBySide;
        assert_ne!(base, key("smb://nas/media/a.mp4", 100, 7, &other));
        // How sure the detection was doesn't change the picture.
        let mut other = layout();
        other.projection_from = Evidence::Filename;
        assert_eq!(base, key("smb://nas/media/a.mp4", 100, 7, &other));
    }

    #[test]
    fn stored_thumbnails_load_back() {
        let dir = temp("round-trip");
        let mut cache = ThumbCache::at(dir.clone());
        assert!(cache.load("0123456789abcdef").is_none());
        let thumb = picture(7);
        cache.store("0123456789abcdef", &thumb);
        let loaded = cache.load("0123456789abcdef").unwrap();
        assert_eq!(loaded.rgba, thumb.rgba);
        assert_eq!((loaded.width, loaded.height), (THUMB_W, THUMB_H));
        // No temporary file is left, and junk is a miss, not a crash.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::write(dir.join("bad.png"), b"not a png").unwrap();
        assert!(cache.load("bad").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prune_keeps_the_newest() {
        let dir = temp("prune");
        let mut cache = ThumbCache::at(dir.clone());
        for n in 0..5 {
            cache.store(&format!("{n:016x}"), &picture(3));
            let age = std::time::Duration::from_secs(1000 - n * 100);
            let file = std::fs::File::options()
                .write(true)
                .open(cache.path(&format!("{n:016x}")))
                .unwrap();
            file.set_modified(std::time::SystemTime::now() - age)
                .unwrap();
        }
        cache.prune(3);
        for n in 0..5 {
            assert_eq!(cache.load(&format!("{n:016x}")).is_some(), n >= 2, "{n}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prune_keeps_what_was_used_lately() {
        let dir = temp("prune-used");
        let mut cache = ThumbCache::at(dir.clone());
        for n in 0..4 {
            cache.store(&format!("{n:016x}"), &picture(3));
            let file = std::fs::File::options()
                .write(true)
                .open(cache.path(&format!("{n:016x}")))
                .unwrap();
            let age = std::time::Duration::from_secs(1000 - n * 100);
            file.set_modified(std::time::SystemTime::now() - age)
                .unwrap();
        }
        // Leftovers of an interrupted save go at start-up.
        std::fs::write(dir.join(".dead.tmp"), b"half").unwrap();
        cache.prune_old();
        assert!(!dir.join(".dead.tmp").exists());
        // The oldest is shown again: it is now the newest.
        assert!(cache.load(&format!("{:016x}", 0)).is_some());
        cache.prune(2);
        let kept: Vec<u64> = (0..4)
            .filter(|n| cache.path(&format!("{n:016x}")).exists())
            .collect();
        assert_eq!(kept, [0, 3]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn set_age(cache: &ThumbCache, key: &str, age: Duration) {
        std::fs::File::options()
            .write(true)
            .open(cache.path(key))
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
    }

    #[test]
    fn expired_files_are_pruned_and_fresh_ones_kept() {
        let dir = temp("expire");
        let mut cache = ThumbCache::at(dir.clone());
        for (k, hours) in [("a", 30), ("b", 25), ("c", 23), ("d", 1)] {
            cache.store(k, &picture(3));
            set_age(&cache, k, Duration::from_secs(hours * 3600));
        }
        cache.prune_old();
        let kept: Vec<_> = ["a", "b", "c", "d"]
            .into_iter()
            .filter(|k| cache.path(k).exists())
            .collect();
        assert_eq!(kept, ["c", "d"]);
        // A shorter limit drops more.
        cache.prune_older_than(Duration::from_secs(2 * 3600));
        assert!(!cache.path("c").exists() && cache.path("d").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn expired_lookup_is_a_miss_and_deletes() {
        let dir = temp("expire-load");
        let mut cache = ThumbCache::at(dir.clone());
        cache.store("old", &picture(3));
        cache.store("new", &picture(3));
        set_age(&cache, "old", MAX_AGE + Duration::from_secs(60));
        set_age(&cache, "new", MAX_AGE - Duration::from_secs(600));
        assert!(cache.load("old").is_none());
        assert!(!cache.path("old").exists());
        assert!(cache.load("new").is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn using_a_file_keeps_it_alive() {
        let dir = temp("expire-used");
        let mut cache = ThumbCache::at(dir.clone());
        cache.store("k", &picture(3));
        set_age(&cache, "k", MAX_AGE - Duration::from_secs(600));
        assert!(cache.load("k").is_some()); // refreshes the use time
        cache.prune_older_than(Duration::from_secs(3600));
        assert!(cache.path("k").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
