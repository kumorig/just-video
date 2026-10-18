//! What each video's header said (`probe-cache.json`), kept across runs so
//! a folder shows its marks without reading every file again.
//!
//! It keeps facts ([`VideoInfo`]), not verdicts: playability and layout are
//! worked out from them each time, so rule changes apply at once. A file
//! whose size or modification time changed is probed again. Errors are
//! never kept: a broken read may work next time.

use crate::media::VideoInfo;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Bump when [`VideoInfo`]'s meaning changes (new fields alone don't need it).
const VERSION: u32 = 1;
/// Least recently used entries beyond this are dropped when saving (this
/// also forgets files that were renamed, moved or deleted).
const MAX_ENTRIES: usize = 10_000;
const FILE: &str = "probe-cache.json";

#[derive(Serialize, Deserialize)]
struct Cached {
    size: u64,
    /// Last write time as the server reports it (see `smb::Entry`).
    modified: u64,
    video: Option<VideoInfo>,
    /// When last looked up or stored (see `ProbeCache::clock`).
    used: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    version: u32,
    entries: HashMap<String, Cached>,
}

#[derive(Default)]
pub struct ProbeCache {
    entries: HashMap<String, Cached>,
    /// Counts lookups and stores, for least-recently-used order.
    clock: u64,
    /// Changed since the last save.
    dirty: bool,
}

impl ProbeCache {
    /// The saved cache, or an empty one (missing, unreadable or old).
    pub fn load() -> Self {
        let stored: Stored = crate::config::read_json(FILE)
            .ok()
            .filter(|s: &Stored| s.version == VERSION)
            .unwrap_or_default();
        let clock = stored.entries.values().map(|c| c.used).max().unwrap_or(0);
        Self {
            entries: stored.entries,
            clock,
            dirty: false,
        }
    }

    /// The video info stored for `key` (see `library::file_key`), if the
    /// file still has this size and modification time. `Some(None)`: no
    /// video stream.
    pub fn get(&mut self, key: &str, size: u64, modified: u64) -> Option<Option<VideoInfo>> {
        let entry = self.entries.get_mut(key)?;
        if entry.size != size || entry.modified != modified {
            return None;
        }
        self.clock += 1;
        entry.used = self.clock;
        Some(entry.video.clone())
    }

    pub fn insert(&mut self, key: String, size: u64, modified: u64, video: Option<VideoInfo>) {
        self.clock += 1;
        let used = self.clock;
        let cached = Cached {
            size,
            modified,
            video,
            used,
        };
        self.entries.insert(key, cached);
        self.dirty = true;
    }

    /// Writes the cache if it changed. A failure only costs probing again.
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        if self.entries.len() > MAX_ENTRIES {
            let mut used: Vec<u64> = self.entries.values().map(|c| c.used).collect();
            used.sort_unstable();
            let cutoff = used[self.entries.len() - MAX_ENTRIES];
            self.entries.retain(|_, c| c.used >= cutoff);
        }
        let stored = Stored {
            version: VERSION,
            entries: std::mem::take(&mut self.entries),
        };
        if let Err(e) = crate::config::write_json(FILE, &stored, false) {
            eprintln!("Probe cache: can't save: {e:#}");
        }
        self.entries = stored.entries;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(width: u32) -> VideoInfo {
        VideoInfo {
            codec: "hevc".into(),
            profile: Some("Main".into()),
            pixel_format: Some("yuv420p".into()),
            width,
            height: width / 2,
            bit_depth: 8,
            fps: 60.0,
            stereo_mode: None,
            stereo_inverted: false,
            projection: None,
            horizontal_degrees: None,
        }
    }

    #[test]
    fn survives_a_restart_and_notices_changed_files() {
        crate::config::temp_config("probe-cache");
        let mut cache = ProbeCache::load();
        cache.insert("smb://a/s/x.mp4".into(), 100, 7, Some(video(6144)));
        cache.insert("smb://a/s/audio.mp4".into(), 50, 7, None);
        cache.save();

        let mut cache = ProbeCache::load();
        let hit = cache.get("smb://a/s/x.mp4", 100, 7).unwrap().unwrap();
        assert_eq!(hit.width, 6144);
        assert!(cache.get("smb://a/s/audio.mp4", 50, 7).unwrap().is_none());
        // Rewritten or replaced files are probed again.
        assert!(cache.get("smb://a/s/x.mp4", 101, 7).is_none());
        assert!(cache.get("smb://a/s/x.mp4", 100, 8).is_none());
        assert!(cache.get("smb://a/s/other.mp4", 100, 7).is_none());
    }

    #[test]
    fn ignores_a_file_of_another_version_or_garbage() {
        let dir = crate::config::temp_config("probe-cache-version");
        let mut entries = HashMap::new();
        entries.insert(
            "k".to_string(),
            Cached {
                size: 1,
                modified: 1,
                video: None,
                used: 1,
            },
        );
        let old = Stored {
            version: 0,
            entries,
        };
        crate::config::write_json(FILE, &old, false).unwrap();
        assert!(ProbeCache::load().entries.is_empty());
        std::fs::write(dir.join(FILE), "not json").unwrap();
        assert!(ProbeCache::load().entries.is_empty());
    }

    #[test]
    fn keeps_the_most_recently_used() {
        crate::config::temp_config("probe-cache-lru");
        let mut cache = ProbeCache::default();
        for i in 0..MAX_ENTRIES + 10 {
            cache.insert(format!("k{i}"), 1, 1, None);
        }
        // The oldest entry, used again, stays.
        assert!(cache.get("k0", 1, 1).is_some());
        cache.save();
        let mut cache = ProbeCache::load();
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
        assert!(cache.get("k0", 1, 1).is_some());
        assert!(cache.get("k1", 1, 1).is_none());
        assert!(cache.get(&format!("k{}", MAX_ENTRIES + 9), 1, 1).is_some());
    }
}
