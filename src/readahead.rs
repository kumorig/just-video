//! Blocking `Read + Seek` with pipelined read-ahead over any async block source.
//!
//! The demuxer reads synchronously; behind it, a window of large block reads is
//! kept in flight so network latency overlaps decode instead of adding to it.

use std::{
    collections::BTreeMap,
    future::Future,
    io::{self, Read, Seek, SeekFrom},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{runtime::Runtime, task::JoinHandle};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Random-access byte source, e.g. an open SMB file.
pub trait BlockSource: Send + Sync + 'static {
    /// Reads exactly `len` bytes at `offset` (the reader never asks past the end).
    fn fetch(self: Arc<Self>, offset: u64, len: usize) -> BoxFuture<'static, io::Result<Vec<u8>>>;

    /// Releases the remote resource; called once when the reader drops.
    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// Read-ahead policy. Defaults target high-bitrate 8K VR over Wi-Fi: 1 MiB
/// reads (within every SMB2.1+ server's max read size) with a 32 MiB window.
#[derive(Clone, Copy, Debug)]
pub struct ReadAhead {
    pub block_size: usize,
    pub blocks_ahead: usize,
}

impl Default for ReadAhead {
    fn default() -> Self {
        Self {
            block_size: 1 << 20,
            blocks_ahead: 32,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct ReadStats {
    pub bytes_delivered: u64,
    pub bytes_fetched: u64,
    pub requests: u64,
    /// Time the demuxer spent blocked waiting on the network.
    pub stall_seconds: f64,
    pub discarded_blocks: u64,
}

enum Block {
    Pending(JoinHandle<io::Result<Vec<u8>>>),
    Ready(Vec<u8>),
}

/// What the reader holds, for a buffer indicator: the read position, the
/// file length and the byte ranges already fetched (sorted, disjoint).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Buffered {
    pub position: u64,
    pub len: u64,
    pub ranges: Vec<(u64, u64)>,
}

/// A live view of a reader's [`Buffered`] state, shared with the UI.
pub type BufferWatch = Arc<Mutex<Buffered>>;

/// How often a read waiting for its block logs (the request itself keeps
/// running: cancelling it would leak SMB credits).
const BLOCK_WAIT: Duration = Duration::from_secs(10);
/// How long a read may wait in total before failing.
const STALL_LIMIT: Duration = Duration::from_secs(45);
/// Failed block reads are retried this many times.
const READ_RETRIES: u32 = 3;
/// How long closing waits for outstanding reads.
const CLOSE_WAIT: Duration = Duration::from_secs(10);
/// After a jump, this many blocks are requested even past the in-flight
/// cap: a keyframe spans several, and waiting for the old window's reads
/// to drain would fetch them one round trip at a time. It is also where the
/// window restarts: requesting the whole window at once would queue tens
/// of MiB on the link, behind which the next seek's data would wait.
const JUMP_BLOCKS: u64 = 4;
/// Blocks the window grows by for each block read sequentially.
const RAMP_BLOCKS: u64 = 2;

pub struct ReadAheadReader<S: BlockSource> {
    runtime: Arc<Runtime>,
    source: Arc<S>,
    len: u64,
    pos: u64,
    options: ReadAhead,
    blocks: BTreeMap<u64, Block>,
    /// Reads no longer needed, left to finish. In-flight requests are never
    /// aborted: smb-rs returns a request's credits only when its response is
    /// received, so cancelled reads would leak credits until the connection
    /// can send nothing at all.
    detached: Vec<JoinHandle<io::Result<Vec<u8>>>>,
    in_flight: Arc<AtomicUsize>,
    /// Blocks requested ahead: small after a jump, growing with sequential
    /// reads up to `blocks_ahead`.
    window: u64,
    /// The block the last read was in.
    last_block: Option<u64>,
    stats: ReadStats,
    watch: BufferWatch,
}

impl<S: BlockSource> ReadAheadReader<S> {
    pub fn new(runtime: Arc<Runtime>, source: S, len: u64, options: ReadAhead) -> Self {
        assert!(options.block_size > 0 && options.blocks_ahead > 0);
        Self {
            runtime,
            source: Arc::new(source),
            len,
            pos: 0,
            options,
            blocks: BTreeMap::new(),
            detached: Vec::new(),
            in_flight: Arc::new(AtomicUsize::new(0)),
            window: JUMP_BLOCKS,
            last_block: None,
            stats: ReadStats::default(),
            watch: Arc::new(Mutex::new(Buffered {
                position: 0,
                len,
                ranges: Vec::new(),
            })),
        }
    }

    /// The reader's buffer state, kept up to date as it reads.
    pub fn watch(&self) -> BufferWatch {
        self.watch.clone()
    }

    /// Publishes the fetched ranges (contiguous runs of ready blocks).
    fn publish(&self) {
        let size = self.options.block_size as u64;
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for (&index, block) in &self.blocks {
            if !matches!(block, Block::Ready(_)) {
                continue;
            }
            let (start, end) = (index * size, ((index + 1) * size).min(self.len));
            match ranges.last_mut() {
                Some(last) if last.1 == start => last.1 = end,
                _ => ranges.push((start, end)),
            }
        }
        let state = Buffered {
            position: self.pos,
            len: self.len,
            ranges,
        };
        if let Ok(mut shared) = self.watch.lock()
            && *shared != state
        {
            *shared = state;
        }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn stats(&self) -> ReadStats {
        self.stats
    }

    fn spawn(&mut self, index: u64) {
        let size = self.options.block_size as u64;
        let offset = index * size;
        let length = size.min(self.len - offset) as usize;
        self.stats.requests += 1;
        self.stats.bytes_fetched += length as u64;
        let fetch = self.source.clone().fetch(offset, length);
        let in_flight = self.in_flight.clone();
        in_flight.fetch_add(1, Ordering::SeqCst);
        let task = self.runtime.spawn(async move {
            let result = fetch.await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
            result
        });
        self.blocks.insert(index, Block::Pending(task));
    }

    /// Keeps `[current, current + window)` requested, where the window
    /// restarts small at a jump and grows with sequential reading to
    /// `blocks_ahead`. Blocks outside `[current - 1, current + blocks_ahead)`
    /// are dropped; the one behind absorbs small backward seeks by demuxers.
    fn schedule(&mut self, current: u64) {
        let blocks_ahead = self.options.blocks_ahead as u64;
        match self.last_block {
            Some(last) if last == current => {}
            Some(last) if last + 1 == current => {
                self.window = (self.window + RAMP_BLOCKS).min(blocks_ahead);
            }
            _ => self.window = JUMP_BLOCKS.min(blocks_ahead),
        }
        self.last_block = Some(current);
        let block_count = self.len.div_ceil(self.options.block_size as u64);
        let end = (current + self.window).min(block_count);
        let keep_until = (current + blocks_ahead).min(block_count);
        let keep_from = current.saturating_sub(1);
        let stale: Vec<u64> = self
            .blocks
            .keys()
            .copied()
            .filter(|&i| i < keep_from || i >= keep_until)
            .collect();
        for index in stale {
            if let Some(Block::Pending(handle)) = self.blocks.remove(&index) {
                self.detached.push(handle);
            }
            self.stats.discarded_blocks += 1;
        }
        self.detached.retain(|h| !h.is_finished());
        // Detached reads count against the window, so seeking around cannot
        // pile up requests; the blocks needed first are always requested.
        for index in current..end {
            if !self.blocks.contains_key(&index) {
                if index >= current + JUMP_BLOCKS
                    && self.in_flight.load(Ordering::SeqCst) >= self.options.blocks_ahead
                {
                    break;
                }
                self.spawn(index);
            }
        }
    }
}

impl<S: BlockSource> Read for ReadAheadReader<S> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.pos >= self.len {
            return Ok(0);
        }
        let size = self.options.block_size as u64;
        let index = self.pos / size;
        self.schedule(index);
        let mut retries = 0;
        let mut waited = Duration::ZERO;
        loop {
            let block = self.blocks.get_mut(&index).expect("scheduled block");
            let Block::Pending(handle) = block else { break };
            let started = Instant::now();
            let result = self
                .runtime
                .block_on(async { tokio::time::timeout(BLOCK_WAIT, &mut *handle).await });
            waited += started.elapsed();
            self.stats.stall_seconds += started.elapsed().as_secs_f64();
            match result {
                Ok(Ok(Ok(data))) => *block = Block::Ready(data),
                // Keep the request running (see `detached`) and keep waiting:
                // a slow server is better than a broken video.
                Err(_) if waited < STALL_LIMIT => {
                    eprintln!(
                        "Read-ahead: waited {:.0}s for data at {}",
                        waited.as_secs_f64(),
                        self.pos
                    );
                }
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "The server stopped sending data",
                    ));
                }
                Ok(failed) => {
                    self.blocks.remove(&index);
                    let error = match failed {
                        Ok(e) => e.err().unwrap_or_else(|| io::Error::other("read failed")),
                        Err(e) => io::Error::other(e),
                    };
                    // A failed request is retried; a truncated packet would
                    // corrupt or stop the video.
                    if retries >= READ_RETRIES {
                        return Err(error);
                    }
                    retries += 1;
                    eprintln!("Read-ahead: retrying read at {} ({error})", self.pos);
                    std::thread::sleep(Duration::from_millis(200 * retries as u64));
                    self.spawn(index);
                }
            }
        }
        let block = self.blocks.get_mut(&index).expect("scheduled block");
        let Block::Ready(data) = block else {
            unreachable!()
        };
        let start = (self.pos - index * size) as usize;
        let n = out.len().min(data.len() - start);
        out[..n].copy_from_slice(&data[start..start + n]);
        self.pos += n as u64;
        self.stats.bytes_delivered += n as u64;
        self.publish();
        Ok(n)
    }
}

impl<S: BlockSource> Seek for ReadAheadReader<S> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = target.ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        Ok(self.pos)
    }
}

impl<S: BlockSource> Drop for ReadAheadReader<S> {
    fn drop(&mut self) {
        let runtime = self.runtime.clone();
        // Sources may spawn cleanup work on drop (smb-rs does).
        let _guard = runtime.enter();
        let mut pending = std::mem::take(&mut self.detached);
        for (_, block) in std::mem::take(&mut self.blocks) {
            if let Block::Pending(handle) = block {
                pending.push(handle);
            }
        }
        // Let outstanding reads finish (never abort them; see `detached`).
        runtime.block_on(async {
            let _ = tokio::time::timeout(CLOSE_WAIT, async {
                for handle in pending {
                    let _ = handle.await;
                }
            })
            .await;
        });
        runtime.block_on(self.source.close());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// In-memory source with fixed per-request latency, like a network round trip.
    struct Slow {
        data: Vec<u8>,
        latency: Duration,
        completed: Arc<AtomicUsize>,
    }

    impl BlockSource for Slow {
        fn fetch(
            self: Arc<Self>,
            offset: u64,
            len: usize,
        ) -> BoxFuture<'static, io::Result<Vec<u8>>> {
            Box::pin(async move {
                tokio::time::sleep(self.latency).await;
                self.completed.fetch_add(1, Ordering::SeqCst);
                let start = offset as usize;
                Ok(self.data[start..start + len].to_vec())
            })
        }
    }

    fn reader(len: usize, latency_ms: u64, options: ReadAhead) -> (ReadAheadReader<Slow>, Vec<u8>) {
        let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_time()
                .build()
                .unwrap(),
        );
        let source = Slow {
            data: data.clone(),
            latency: Duration::from_millis(latency_ms),
            completed: Arc::new(AtomicUsize::new(0)),
        };
        (
            ReadAheadReader::new(runtime, source, len as u64, options),
            data,
        )
    }

    #[test]
    fn watch_reports_fetched_ranges() {
        let (mut reader, _) = reader(
            10 * 1024,
            1,
            ReadAhead {
                block_size: 1024,
                blocks_ahead: 4,
            },
        );
        let watch = reader.watch();
        assert_eq!(watch.lock().unwrap().len, 10 * 1024);
        let mut buf = [0u8; 100];
        reader.read_exact(&mut buf).unwrap();
        let state = watch.lock().unwrap().clone();
        assert_eq!(state.position, 100);
        assert_eq!(state.ranges.first().map(|r| r.0), Some(0));
        // Reading on waits for the rest of the window, which becomes one run.
        std::thread::sleep(Duration::from_millis(30));
        reader.seek(SeekFrom::Start(3000)).unwrap();
        reader.read_exact(&mut buf).unwrap();
        let state = watch.lock().unwrap().clone();
        assert!(
            state.ranges.iter().any(|&(a, b)| a <= 3000 && b >= 4096),
            "{state:?}"
        );
    }

    #[test]
    fn sequential_read_matches_source() {
        let options = ReadAhead {
            block_size: 4096,
            blocks_ahead: 8,
        };
        let (mut r, data) = reader(100_003, 0, options);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
        assert_eq!(r.stats().bytes_delivered, data.len() as u64);
    }

    #[test]
    fn seeks_land_on_the_right_bytes() {
        let options = ReadAhead {
            block_size: 1000,
            blocks_ahead: 4,
        };
        let (mut r, data) = reader(50_000, 0, options);
        let mut buf = [0u8; 1500];
        for &(from, expect) in &[
            (SeekFrom::End(-1500), 48_500u64),
            (SeekFrom::Start(0), 0),
            (SeekFrom::Start(31_337), 31_337),
            (SeekFrom::Current(-2000), 30_837),
        ] {
            assert_eq!(r.seek(from).unwrap(), expect);
            r.read_exact(&mut buf).unwrap();
            let at = expect as usize;
            assert_eq!(&buf[..], &data[at..at + 1500]);
        }
        r.seek(SeekFrom::Start(50_000)).unwrap();
        assert_eq!(r.read(&mut buf).unwrap(), 0);
        assert!(r.seek(SeekFrom::Current(-60_000)).is_err());
    }

    /// Regression: aborting SMB reads leaked credits and hung the connection.
    #[test]
    fn reads_are_never_cancelled() {
        let options = ReadAhead {
            block_size: 1000,
            blocks_ahead: 8,
        };
        let (mut r, data) = reader(100_000, 20, options);
        let completed = r.source.completed.clone();
        let mut buf = [0u8; 10];
        r.read_exact(&mut buf).unwrap();
        // Jump far away: the first window's reads must still complete.
        r.seek(SeekFrom::Start(90_000)).unwrap();
        r.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &data[90_000..90_010]);
        let requests = r.stats().requests;
        assert!(requests <= 16 + 1, "in-flight cap exceeded: {requests}");
        drop(r);
        assert_eq!(completed.load(Ordering::SeqCst) as u64, requests);
    }

    /// Fails every block's first request.
    struct Flaky {
        data: Vec<u8>,
        failed: std::sync::Mutex<std::collections::HashSet<u64>>,
    }

    impl BlockSource for Flaky {
        fn fetch(
            self: Arc<Self>,
            offset: u64,
            len: usize,
        ) -> BoxFuture<'static, io::Result<Vec<u8>>> {
            Box::pin(async move {
                if self.failed.lock().unwrap().insert(offset) {
                    return Err(io::Error::other("network hiccup"));
                }
                Ok(self.data[offset as usize..offset as usize + len].to_vec())
            })
        }
    }

    #[test]
    fn failed_reads_are_retried() {
        let data: Vec<u8> = (0..20_000).map(|i| (i % 251) as u8).collect();
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_time()
                .build()
                .unwrap(),
        );
        let source = Flaky {
            data: data.clone(),
            failed: Default::default(),
        };
        let options = ReadAhead {
            block_size: 4096,
            blocks_ahead: 2,
        };
        let mut r = ReadAheadReader::new(runtime, source, data.len() as u64, options);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }

    /// A jump requests a few blocks, not the whole window: the next seek
    /// would wait behind all of it.
    #[test]
    fn jumps_request_few_blocks_then_ramp_up() {
        let options = ReadAhead {
            block_size: 1000,
            blocks_ahead: 16,
        };
        let (mut r, _) = reader(200_000, 0, options);
        let mut buf = [0u8; 1000];
        r.read_exact(&mut buf).unwrap();
        assert_eq!(r.stats().requests, JUMP_BLOCKS);
        r.seek(SeekFrom::Start(100_000)).unwrap();
        r.read_exact(&mut buf).unwrap();
        assert_eq!(r.stats().requests, 2 * JUMP_BLOCKS);
        // Reading on grows the window until it is full.
        for _ in 0..12 {
            r.read_exact(&mut buf).unwrap();
        }
        let requested_ahead = r.blocks.keys().filter(|&&i| i > 112).count() as u64;
        assert_eq!(
            requested_ahead,
            16 - 1,
            "{:?}",
            r.blocks.keys().collect::<Vec<_>>()
        );
        // Blocks already fetched ahead survive a small backward seek.
        r.seek(SeekFrom::Start(110_000)).unwrap();
        r.read_exact(&mut buf).unwrap();
        assert!(
            r.blocks.keys().any(|&i| i > 120),
            "fetched blocks were dropped"
        );
    }

    #[test]
    fn pipelining_hides_latency() {
        // 64 blocks at 10 ms each: ~640 ms if serial, ~40 ms with 16 in flight.
        let options = ReadAhead {
            block_size: 8192,
            blocks_ahead: 16,
        };
        let (mut r, _) = reader(64 * 8192, 10, options);
        let started = Instant::now();
        std::io::copy(&mut r, &mut std::io::sink()).unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_millis(250), "took {elapsed:?}");
        assert_eq!(r.stats().requests, 64);
    }
}
