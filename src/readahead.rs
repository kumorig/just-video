//! Blocking `Read + Seek` with pipelined read-ahead over any async block source.
//!
//! The demuxer reads synchronously; behind it, a window of large block reads is
//! kept in flight so network latency overlaps decode instead of adding to it.
//!
//! Beside the window sits the [`Reserve`]: up to a gigabyte of the video past
//! the window, fetched only while the player says so (while paused, so that
//! nothing competes with the reads playback needs), and handed to the window
//! as reading reaches it. A paused video can thus be carried off the network.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    io::{self, Read, Seek, SeekFrom},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{runtime::Runtime, sync::Notify, task::JoinHandle};

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
    /// Most the [`Reserve`] may hold; 0 for none.
    pub reserve_bytes: u64,
}

impl Default for ReadAhead {
    fn default() -> Self {
        Self {
            block_size: 1 << 20,
            blocks_ahead: 32,
            reserve_bytes: RESERVE_BYTES,
        }
    }
}

/// What the reserve holds by default: an hour or two of most videos.
pub const RESERVE_BYTES: u64 = 1 << 30;
/// Reads the reserve keeps in flight. It fills only while playback is
/// paused, so nothing competes for the link; but reads are never cancelled
/// (see `detached`), so these are what still arrives after play is pressed.
/// A few MiB, not tens, keeps that short.
const RESERVE_IN_FLIGHT: usize = 8;
/// Failed reserve reads in a row before it gives up (the player shows it off).
const RESERVE_FAILURES: u32 = 5;

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
/// file length and the byte ranges already fetched (sorted, disjoint) by
/// the window and by the reserve.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Buffered {
    pub position: u64,
    pub len: u64,
    pub ranges: Vec<(u64, u64)>,
    pub reserved: Vec<(u64, u64)>,
}

impl Buffered {
    /// Everything fetched, window and reserve together (sorted, merged).
    pub fn fetched(&self) -> Vec<(u64, u64)> {
        let mut all: Vec<(u64, u64)> = self.ranges.iter().chain(&self.reserved).copied().collect();
        all.sort_unstable();
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(all.len());
        for (a, b) in all {
            match out.last_mut() {
                Some(last) if last.1 >= a => last.1 = last.1.max(b),
                _ => out.push((a, b)),
            }
        }
        out
    }

    /// Bytes fetched within `[from, to)`.
    pub fn fetched_within(&self, from: u64, to: u64) -> u64 {
        self.fetched()
            .iter()
            .map(|&(a, b)| b.min(to).saturating_sub(a.max(from)))
            .sum()
    }
}

/// A live view of a reader's [`Buffered`] state, shared with the UI.
pub type BufferWatch = Arc<Mutex<Buffered>>;

struct ReserveInner {
    blocks: BTreeMap<u64, Vec<u8>>,
    /// Being fetched by the fill task.
    pending: BTreeSet<u64>,
    bytes: u64,
    fill: Option<JoinHandle<()>>,
}

/// Blocks fetched past the reader's window while the player asks for it
/// (see the module docs). Shared between the reader, its fill task and the
/// player, which switches it on and off.
pub struct Reserve {
    runtime: Arc<Runtime>,
    source: Arc<dyn BlockSource>,
    block_size: u64,
    len: u64,
    cap: u64,
    inner: Mutex<ReserveInner>,
    /// The block the reader is in, and the end of its requested window:
    /// the reserve fills from there, up to `cap` past the position.
    position: AtomicU64,
    fill_from: AtomicU64,
    active: AtomicBool,
    closed: AtomicBool,
    wake: Notify,
    watch: BufferWatch,
}

impl Reserve {
    /// Starts or stops filling. Reads in flight finish either way.
    pub fn set_active(self: &Arc<Self>, active: bool) {
        if self.cap == 0 || self.closed.load(Ordering::SeqCst) {
            return;
        }
        self.active.store(active, Ordering::SeqCst);
        if active {
            let mut inner = self.inner.lock().expect("reserve");
            if inner.fill.as_ref().is_none_or(|t| t.is_finished()) {
                // The task holds the reserve until it ends (at close).
                inner.fill = Some(self.runtime.spawn(self.clone().fill()));
            }
        }
        self.wake.notify_waiters();
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    pub fn capacity(&self) -> u64 {
        self.cap
    }

    fn block_count(&self) -> u64 {
        self.len.div_ceil(self.block_size)
    }

    /// The next block to fetch, making room for it by dropping a block
    /// behind the reader (or far beyond what it can hold); None when full
    /// or nothing is wanted.
    fn next_block(&self) -> Option<u64> {
        let position = self.position.load(Ordering::SeqCst);
        let fill_from = self.fill_from.load(Ordering::SeqCst).max(position);
        let end = (position + self.cap / self.block_size).min(self.block_count());
        let mut inner = self.inner.lock().expect("reserve");
        let wanted = (fill_from..end)
            .find(|i| !inner.blocks.contains_key(i) && !inner.pending.contains(i))?;
        while inner.bytes + (inner.pending.len() as u64 + 1) * self.block_size > self.cap {
            let behind = inner
                .blocks
                .keys()
                .next()
                .copied()
                .filter(|&i| i + 1 < position);
            let beyond = inner
                .blocks
                .keys()
                .next_back()
                .copied()
                .filter(|&i| i >= end);
            let index = behind.or(beyond)?;
            let dropped = inner.blocks.remove(&index).map_or(0, |b| b.len() as u64);
            inner.bytes -= dropped;
        }
        inner.pending.insert(wanted);
        Some(wanted)
    }

    fn ranges_of(&self, inner: &ReserveInner) -> Vec<(u64, u64)> {
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for (&index, block) in &inner.blocks {
            let start = index * self.block_size;
            let end = start + block.len() as u64;
            match ranges.last_mut() {
                Some(last) if last.1 == start => last.1 = end,
                _ => ranges.push((start, end)),
            }
        }
        ranges
    }

    /// The byte ranges held (sorted, disjoint).
    pub fn ranges(&self) -> Vec<(u64, u64)> {
        self.ranges_of(&self.inner.lock().expect("reserve"))
    }

    fn publish(&self, inner: &ReserveInner) {
        let ranges = self.ranges_of(inner);
        if let Ok(mut shared) = self.watch.lock()
            && shared.reserved != ranges
        {
            shared.reserved = ranges;
        }
    }

    /// Hands a block to the reader.
    fn take(&self, index: u64) -> Option<Vec<u8>> {
        let mut inner = self.inner.lock().expect("reserve");
        let block = inner.blocks.remove(&index)?;
        inner.bytes -= block.len() as u64;
        self.publish(&inner);
        Some(block)
    }

    /// Takes a lent block back, while it fits (the window's own fetches
    /// never come here, so playing with the reserve off holds nothing).
    fn give_back(&self, index: u64, data: Vec<u8>) {
        let mut inner = self.inner.lock().expect("reserve");
        if self.closed.load(Ordering::SeqCst)
            || inner.bytes + data.len() as u64 > self.cap
            || inner.blocks.contains_key(&index)
        {
            return;
        }
        inner.bytes += data.len() as u64;
        inner.blocks.insert(index, data);
        self.publish(&inner);
    }

    fn is_pending(&self, index: u64) -> bool {
        self.inner.lock().expect("reserve").pending.contains(&index)
    }

    fn settle(&self, index: u64, result: io::Result<Vec<u8>>) -> bool {
        let mut inner = self.inner.lock().expect("reserve");
        inner.pending.remove(&index);
        let ok = result.is_ok();
        if let Ok(block) = result
            && !self.closed.load(Ordering::SeqCst)
        {
            inner.bytes += block.len() as u64;
            inner.blocks.insert(index, block);
            self.publish(&inner);
        }
        drop(inner);
        // A read waiting for this block (see `ReadAheadReader::read`).
        self.wake.notify_waiters();
        ok
    }

    /// Fetches wanted blocks while active; idles otherwise. Ends when the
    /// reader closes, after its reads in flight have landed.
    async fn fill(self: Arc<Self>) {
        let mut tasks = tokio::task::JoinSet::new();
        // Which block each task fetches, for one that panics.
        let mut indices: std::collections::HashMap<tokio::task::Id, u64> = Default::default();
        let mut failures = 0u32;
        loop {
            if self.closed.load(Ordering::SeqCst) {
                break;
            }
            let next = if self.is_active() && tasks.len() < RESERVE_IN_FLIGHT {
                self.next_block()
            } else {
                None
            };
            if let Some(index) = next {
                let offset = index * self.block_size;
                let length = self.block_size.min(self.len - offset) as usize;
                let fetch = self.source.clone().fetch(offset, length);
                let id = tasks.spawn(async move { (index, fetch.await) }).id();
                indices.insert(id, index);
                continue;
            }
            if tasks.is_empty() {
                // Woken by the player (on/off), the reader (position) or a
                // landed read; the timeout only guards a wakeup lost between
                // the check above and here.
                let _ =
                    tokio::time::timeout(Duration::from_millis(500), self.wake.notified()).await;
                continue;
            }
            let (index, result) = match tasks.join_next_with_id().await {
                Some(Ok((id, (index, result)))) => {
                    indices.remove(&id);
                    (index, result)
                }
                Some(Err(e)) => match indices.remove(&e.id()) {
                    Some(index) => (index, Err(io::Error::other(e))),
                    None => continue,
                },
                None => continue,
            };
            if self.settle(index, result) {
                failures = 0;
            } else {
                failures += 1;
                if failures >= RESERVE_FAILURES {
                    eprintln!("Reserve: reads keep failing; giving up");
                    self.active.store(false, Ordering::SeqCst);
                    failures = 0;
                } else {
                    tokio::time::sleep(Duration::from_millis(200 * failures as u64)).await;
                }
            }
        }
        while let Some(landed) = tasks.join_next_with_id().await {
            match landed {
                Ok((_, (index, result))) => {
                    self.settle(index, result);
                }
                Err(e) => {
                    if let Some(index) = indices.remove(&e.id()) {
                        self.settle(index, Err(io::Error::other(e)));
                    }
                }
            }
        }
    }

    /// Stops filling and waits for reads in flight (never cancelled).
    fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.active.store(false, Ordering::SeqCst);
        self.wake.notify_waiters();
        let fill = self.inner.lock().expect("reserve").fill.take();
        if let Some(fill) = fill {
            self.runtime.block_on(async {
                let _ = tokio::time::timeout(CLOSE_WAIT, fill).await;
            });
        }
    }
}

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
    /// Blocks in `blocks` that came from the reserve: dropped from the
    /// window, they go back there (see [`Reserve::give_back`]).
    borrowed: BTreeSet<u64>,
    stats: ReadStats,
    watch: BufferWatch,
    reserve: Arc<Reserve>,
    /// Asked before a read and while it waits for a block: true makes the
    /// read fail at once (see [`ReadAheadReader::with_cancel`]).
    cancel: Option<Cancel>,
    /// Set when a read failed for `cancel`: no more requests are issued, and
    /// dropping does not wait for those in flight.
    cancelled: bool,
    /// A cap on reads in flight that holds even right after a jump (see
    /// [`ReadAheadReader::with_max_in_flight`]); None for the playback rules.
    max_in_flight: Option<usize>,
}

/// See [`ReadAheadReader::with_cancel`].
pub type Cancel = Arc<dyn Fn() -> bool + Send + Sync>;

/// How often a read waiting for a block looks at `cancel`.
const CANCEL_POLL: Duration = Duration::from_millis(10);

fn interrupted() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "Read cancelled")
}

impl<S: BlockSource> ReadAheadReader<S> {
    pub fn new(runtime: Arc<Runtime>, source: S, len: u64, options: ReadAhead) -> Self {
        assert!(options.block_size > 0 && options.blocks_ahead > 0);
        let source = Arc::new(source);
        let watch = Arc::new(Mutex::new(Buffered {
            position: 0,
            len,
            ranges: Vec::new(),
            reserved: Vec::new(),
        }));
        let reserve = Arc::new(Reserve {
            runtime: runtime.clone(),
            source: source.clone(),
            block_size: options.block_size as u64,
            len,
            cap: options.reserve_bytes,
            inner: Mutex::new(ReserveInner {
                blocks: BTreeMap::new(),
                pending: BTreeSet::new(),
                bytes: 0,
                fill: None,
            }),
            position: AtomicU64::new(0),
            fill_from: AtomicU64::new(0),
            active: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            wake: Notify::new(),
            watch: watch.clone(),
        });
        Self {
            runtime,
            source,
            len,
            pos: 0,
            options,
            blocks: BTreeMap::new(),
            detached: Vec::new(),
            in_flight: Arc::new(AtomicUsize::new(0)),
            window: JUMP_BLOCKS,
            last_block: None,
            borrowed: BTreeSet::new(),
            stats: ReadStats::default(),
            watch,
            reserve,
            cancel: None,
            cancelled: false,
            max_in_flight: None,
        }
    }

    /// Reads fail with `ErrorKind::Interrupted` as soon as `cancel` says so,
    /// also while waiting for data, and no read is requested after that.
    /// (Requests already sent can't be taken back, see `detached`; dropping
    /// the reader then leaves them to finish in the background.)
    pub fn with_cancel(mut self, cancel: Cancel) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Most reads in flight at a time, also right after a jump (playback
    /// requests a few blocks past its cap then): less data still on the
    /// link when the reader is abandoned.
    pub fn with_max_in_flight(mut self, reads: usize) -> Self {
        self.max_in_flight = Some(reads.max(1));
        self
    }

    /// True once `cancel` has stopped a read.
    fn check_cancel(&mut self) -> bool {
        if self.cancelled || self.cancel.as_ref().is_some_and(|c| c()) {
            self.cancelled = true;
        }
        self.cancelled
    }

    /// The reader's buffer state, kept up to date as it reads.
    pub fn watch(&self) -> BufferWatch {
        self.watch.clone()
    }

    /// The reserve, for the player to switch on and off.
    pub fn reserve(&self) -> Arc<Reserve> {
        self.reserve.clone()
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
            reserved: self.reserve.ranges(),
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

    /// Asks again for every block in the window whose request has already
    /// failed (finished blocks are kept as they are). Returns how many were
    /// asked for again.
    fn respawn_failed(&mut self) -> usize {
        use futures_util::FutureExt;
        let finished: Vec<u64> = self
            .blocks
            .iter()
            .filter(|(_, b)| matches!(b, Block::Pending(h) if h.is_finished()))
            .map(|(&i, _)| i)
            .collect();
        let mut again = 0;
        for i in finished {
            let Some(Block::Pending(handle)) = self.blocks.get_mut(&i) else {
                continue;
            };
            match handle.now_or_never() {
                Some(Ok(Ok(data))) => {
                    self.blocks.insert(i, Block::Ready(data));
                }
                Some(_) => {
                    self.spawn(i);
                    again += 1;
                }
                None => {}
            }
        }
        again
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
            // Back into (or a few blocks past) what is already held is a
            // demuxer going between its audio and video, not a jump.
            Some(_) if self.blocks.contains_key(&current) => {}
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
            match self.blocks.remove(&index) {
                Some(Block::Pending(handle)) => self.detached.push(handle),
                // A block the reserve lent is its to keep (the demuxer
                // may come back for it, with the network gone).
                Some(Block::Ready(data)) if self.borrowed.remove(&index) => {
                    self.reserve.give_back(index, data);
                }
                _ => {}
            }
            self.stats.discarded_blocks += 1;
        }
        self.detached.retain(|h| !h.is_finished());
        // Detached reads count against the window, so seeking around cannot
        // pile up requests; the blocks needed first are always requested.
        for index in current..end {
            if self.blocks.contains_key(&index) {
                continue;
            }
            if let Some(data) = self.reserve.take(index) {
                self.blocks.insert(index, Block::Ready(data));
                self.borrowed.insert(index);
                continue;
            }
            if self.reserve.is_pending(index) {
                continue; // on its way; `read` waits for it if needed
            }
            let in_flight = self.in_flight.load(Ordering::SeqCst);
            let full = match self.max_in_flight {
                Some(cap) => index != current && in_flight >= cap,
                None => index >= current + JUMP_BLOCKS && in_flight >= self.options.blocks_ahead,
            };
            if full {
                break;
            }
            self.spawn(index);
        }
        // The reserve fills on from the first block the window doesn't hold
        // (not from `end`: with many reads in flight the loop above stops
        // short of it, and a gap nobody fetched would end offline playback
        // a few blocks in).
        let mut held_to = current;
        while self.blocks.contains_key(&held_to) {
            held_to += 1;
        }
        self.reserve.position.store(current, Ordering::SeqCst);
        self.reserve.fill_from.store(held_to, Ordering::SeqCst);
        if self.reserve.is_active() {
            self.reserve.wake.notify_waiters();
        }
    }

    /// Waits for a block the reserve is fetching, then takes it (or, should
    /// that read fail, requests it itself).
    fn await_reserve(&mut self, index: u64) -> io::Result<()> {
        let started = Instant::now();
        loop {
            if let Some(data) = self.reserve.take(index) {
                self.blocks.insert(index, Block::Ready(data));
                self.borrowed.insert(index);
                return Ok(());
            }
            if !self.reserve.is_pending(index) {
                self.spawn(index);
                return Ok(());
            }
            if started.elapsed() >= STALL_LIMIT {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "The server stopped sending data",
                ));
            }
            if self.check_cancel() {
                return Err(interrupted());
            }
            let wake = &self.reserve.wake;
            self.runtime.block_on(async {
                let _ = tokio::time::timeout(Duration::from_millis(10), wake.notified()).await;
            });
        }
    }
}

impl<S: BlockSource> Read for ReadAheadReader<S> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.pos >= self.len {
            return Ok(0);
        }
        if self.check_cancel() {
            return Err(interrupted());
        }
        let size = self.options.block_size as u64;
        let index = self.pos / size;
        self.schedule(index);
        if !self.blocks.contains_key(&index) {
            let started = Instant::now();
            let waited = self.await_reserve(index);
            self.stats.stall_seconds += started.elapsed().as_secs_f64();
            waited?;
        }
        let mut retries = 0;
        let mut waited = Duration::ZERO;
        let mut logged = Duration::ZERO;
        loop {
            let block = self.blocks.get_mut(&index).expect("scheduled block");
            let Block::Pending(handle) = block else { break };
            let started = Instant::now();
            // Short slices while cancellable, to look at `cancel` between.
            let slice = if self.cancel.is_some() {
                CANCEL_POLL
            } else {
                BLOCK_WAIT
            };
            let result = self
                .runtime
                .block_on(async { tokio::time::timeout(slice, &mut *handle).await });
            waited += started.elapsed();
            self.stats.stall_seconds += started.elapsed().as_secs_f64();
            if result.is_err() {
                if self.cancel.as_ref().is_some_and(|c| c()) {
                    self.cancelled = true;
                    return Err(interrupted());
                }
                // Short slices only look at `cancel`: logging and giving up
                // keep their pace (every BLOCK_WAIT, and at STALL_LIMIT).
                if waited < logged + BLOCK_WAIT && waited < STALL_LIMIT {
                    continue;
                }
                logged = waited;
            }
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
                    // A connection that gives up fails everything queued on it
                    // at once (a dropped link, every connection): ask again
                    // for all of those now, not one block per read, and for
                    // the first retry without waiting. A lone failure, and
                    // later retries, still back off.
                    let batch = self.respawn_failed();
                    if retries > 1 || batch == 0 {
                        std::thread::sleep(Duration::from_millis(200 * retries as u64));
                    }
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
        self.reserve.shutdown();
        let mut pending = std::mem::take(&mut self.detached);
        for (_, block) in std::mem::take(&mut self.blocks) {
            if let Block::Pending(handle) = block {
                pending.push(handle);
            }
        }
        if self.cancelled {
            // Abandoned: whoever dropped us goes on; the reads finish (never
            // aborted; see `detached`) and the file closes behind them.
            let source = self.source.clone();
            runtime.spawn(async move {
                let _ = tokio::time::timeout(CLOSE_WAIT, async {
                    for handle in pending {
                        let _ = handle.await;
                    }
                })
                .await;
                source.close().await;
            });
            return;
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
        /// When set, every offset not listed takes 500x the latency.
        fast_only: Option<Vec<u64>>,
        /// The network is gone: every read fails at once.
        down: Arc<AtomicBool>,
    }

    impl BlockSource for Slow {
        fn fetch(
            self: Arc<Self>,
            offset: u64,
            len: usize,
        ) -> BoxFuture<'static, io::Result<Vec<u8>>> {
            Box::pin(async move {
                let slow = self
                    .fast_only
                    .as_ref()
                    .is_some_and(|f| !f.contains(&offset));
                if self.down.load(Ordering::SeqCst) {
                    return Err(io::Error::other("The connection dropped."));
                }
                tokio::time::sleep(self.latency * if slow { 500 } else { 1 }).await;
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
            fast_only: None,
            down: Arc::new(AtomicBool::new(false)),
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
                reserve_bytes: 0,
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
    fn cancel_fails_a_read_waiting_for_data_and_requests_nothing_more() {
        let options = ReadAhead {
            block_size: 1000,
            blocks_ahead: 8,
            reserve_bytes: 0,
        };
        let (r, _) = reader(100_000, 400, options);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let mut r = r.with_cancel(Arc::new(move || flag.load(Ordering::SeqCst)));
        let completed = r.source.completed.clone();
        let mut buf = [0u8; 10];
        let flag = stop.clone();
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            flag.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let error = r.read(&mut buf).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(started.elapsed() < Duration::from_millis(250));
        stopper.join().unwrap();
        let requests = r.stats().requests;
        // Failed for good, with nothing new requested, also after a seek.
        r.seek(SeekFrom::Start(50_000)).unwrap();
        assert!(r.read(&mut buf).is_err());
        assert_eq!(r.stats().requests, requests);
        // Dropping doesn't wait for the reads in flight, which still finish.
        let _runtime = r.runtime.clone();
        let dropping = Instant::now();
        drop(r);
        assert!(dropping.elapsed() < Duration::from_millis(200));
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(completed.load(Ordering::SeqCst) as u64, requests);
    }

    #[test]
    fn max_in_flight_limits_read_ahead() {
        let options = ReadAhead {
            block_size: 1000,
            blocks_ahead: 8,
            reserve_bytes: 0,
        };
        let (r, _) = reader(100_000, 50, options);
        let mut r = r.with_max_in_flight(1);
        let mut buf = [0u8; 10];
        r.read_exact(&mut buf).unwrap();
        assert_eq!(r.stats().requests, 1);
    }

    #[test]
    fn sequential_read_matches_source() {
        let options = ReadAhead {
            block_size: 4096,
            blocks_ahead: 8,
            reserve_bytes: 0,
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
            reserve_bytes: 0,
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
            reserve_bytes: 0,
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
            reserve_bytes: 0,
        };
        let mut r = ReadAheadReader::new(runtime, source, data.len() as u64, options);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }

    /// A demuxer alternating between streams stored a block or two apart reads
    /// into blocks already held (the one behind, or ahead): that must not
    /// shrink the window.
    #[test]
    fn reading_between_held_blocks_keeps_the_window() {
        let options = ReadAhead {
            block_size: 1000,
            blocks_ahead: 16,
            reserve_bytes: 0,
        };
        let (mut r, _) = reader(200_000, 0, options);
        let mut buf = [0u8; 10];
        for block in 0..20u64 {
            r.seek(SeekFrom::Start(block * 1000)).unwrap();
            r.read_exact(&mut buf).unwrap();
        }
        assert_eq!(r.window, 16);
        for block in 20..60u64 {
            for at in [block + 2, block + 1] {
                r.seek(SeekFrom::Start(at * 1000)).unwrap();
                r.read_exact(&mut buf).unwrap();
                assert_eq!(r.window, 16, "after block {at}");
            }
        }
        // A jump to blocks not held still starts small.
        r.seek(SeekFrom::Start(150_000)).unwrap();
        r.read_exact(&mut buf).unwrap();
        assert_eq!(r.window, JUMP_BLOCKS);
    }

    /// A jump requests a few blocks, not the whole window: the next seek
    /// would wait behind all of it.
    #[test]
    fn jumps_request_few_blocks_then_ramp_up() {
        let options = ReadAhead {
            block_size: 1000,
            blocks_ahead: 16,
            reserve_bytes: 0,
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

    fn wait_until(what: impl Fn() -> bool) -> bool {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            if what() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// The reserve fetches past the window while active, within its cap,
    /// stops when switched off, and the reader plays on from it without
    /// asking the network again.
    #[test]
    fn reserve_fills_while_active_and_feeds_the_reader() {
        let options = ReadAhead {
            block_size: 1000,
            blocks_ahead: 4,
            reserve_bytes: 20_000,
        };
        let (mut r, data) = reader(100_000, 2, options);
        let completed = r.source.completed.clone();
        let reserve = r.reserve();
        let mut buf = [0u8; 1000];
        r.read_exact(&mut buf).unwrap();
        let before = r.stats().requests;
        // Nothing happens until the player asks.
        std::thread::sleep(Duration::from_millis(50));
        assert!(reserve.ranges().is_empty());
        reserve.set_active(true);
        // From the window's end to the cap past the read position.
        assert!(
            wait_until(|| reserve.ranges() == vec![(JUMP_BLOCKS * 1000, 20_000)]),
            "{:?}",
            reserve.ranges()
        );
        let fetched = completed.load(Ordering::SeqCst);
        reserve.set_active(false);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(completed.load(Ordering::SeqCst), fetched, "kept fetching");
        assert_eq!(
            r.stats().requests,
            before,
            "the reader itself asked for nothing"
        );
        // Reading on through the reserve costs no requests.
        let mut out = vec![0u8; 19_000];
        r.read_exact(&mut out).unwrap();
        assert_eq!(out, data[1000..20_000]);
        // (The window looks a few blocks past the reserve's end.)
        assert!(r.stats().requests <= before + 3, "{:?}", r.stats());
        // What the window dropped behind went back to the reserve; nothing
        // ahead is left.
        assert!(
            reserve.ranges().iter().all(|&(_, b)| b <= 20_000),
            "{:?}",
            reserve.ranges()
        );
        // Past it, the window fetches as usual.
        r.read_exact(&mut buf).unwrap();
        assert!(r.stats().requests > before);
        let state = r.watch().lock().unwrap().clone();
        assert!(state.reserved.iter().all(|&(_, b)| b <= 20_000));
        assert_eq!(state.position, 21_000);
    }

    /// A seek while filling moves the reserve along; blocks left behind
    /// make room for the ones now wanted.
    #[test]
    fn reserve_follows_a_seek_and_keeps_its_cap() {
        let options = ReadAhead {
            block_size: 1000,
            blocks_ahead: 2,
            reserve_bytes: 10_000,
        };
        let (mut r, data) = reader(100_000, 1, options);
        let reserve = r.reserve();
        let mut buf = [0u8; 1000];
        r.read_exact(&mut buf).unwrap();
        reserve.set_active(true);
        assert!(
            wait_until(|| reserve.ranges() == vec![(2000, 10_000)]),
            "{:?}",
            reserve.ranges()
        );
        r.seek(SeekFrom::Start(50_000)).unwrap();
        r.read_exact(&mut buf).unwrap();
        // Blocks left behind go only as room is needed (a seek back finds them).
        let holds = |range| reserve.ranges().contains(&range);
        assert!(
            wait_until(|| holds((52_000, 60_000))),
            "{:?}",
            reserve.ranges()
        );
        let held: u64 = reserve.ranges().iter().map(|(a, b)| b - a).sum();
        assert!(held <= 10_000, "{:?}", reserve.ranges());
        // Reading on moves it on; the indicator shows window and reserve.
        r.read_exact(&mut buf).unwrap();
        assert!(
            wait_until(|| holds((53_000, 61_000))),
            "{:?}",
            reserve.ranges()
        );
        let state = r.watch().lock().unwrap().clone();
        assert!(state.fetched().contains(&(50_000, 61_000)), "{state:?}");
        assert_eq!(state.fetched_within(55_000, 70_000), 6_000);
        r.seek(SeekFrom::Start(55_000)).unwrap();
        r.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[..], &data[55_000..56_000]);
        drop(r);
    }

    /// Regression: with the in-flight cap reached (right after a seek) the
    /// window requests fewer blocks than its size; the reserve must start
    /// at the first block not held, or a gap stops offline playback.
    #[test]
    fn reserve_starts_where_the_window_really_ends() {
        let options = ReadAhead {
            block_size: 1000,
            blocks_ahead: 5,
            reserve_bytes: 20_000,
        };
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_time()
                .build()
                .unwrap(),
        );
        // Only the blocks read directly answer at once; the rest of each
        // window stays in flight.
        let source = Slow {
            data: vec![0u8; 200_000],
            latency: Duration::from_millis(1),
            completed: Arc::new(AtomicUsize::new(0)),
            fast_only: Some(vec![0, 50_000]),
            down: Arc::new(AtomicBool::new(false)),
        };
        let mut r = ReadAheadReader::new(runtime, source, 200_000, options);
        let reserve = r.reserve();
        let mut buf = [0u8; 1000];
        r.read_exact(&mut buf).unwrap();
        // Jump: the old window's reads stay in flight, so the new window
        // stops short.
        r.seek(SeekFrom::Start(50_000)).unwrap();
        r.read_exact(&mut buf).unwrap();
        r.schedule(51);
        let held: Vec<u64> = r.blocks.keys().copied().collect();
        let first_gap = (51..).find(|i| !held.contains(i)).unwrap();
        assert!(first_gap < 56, "no gap to test: {held:?}");
        reserve.set_active(true);
        assert!(
            wait_until(|| reserve
                .ranges()
                .first()
                .is_some_and(|&(a, _)| a == first_gap * 1000)),
            "{:?} (gap at {first_gap})",
            reserve.ranges()
        );
    }

    /// Buffered up, then carried off the network: everything held plays,
    /// through the demuxer's small backward seeks too.
    #[test]
    fn reserve_plays_on_without_the_network() {
        let options = ReadAhead {
            block_size: 1000,
            blocks_ahead: 4,
            reserve_bytes: 40_000,
        };
        let (mut r, data) = reader(100_000, 1, options);
        let down = r.source.down.clone();
        let reserve = r.reserve();
        let mut buf = [0u8; 1000];
        r.read_exact(&mut buf).unwrap();
        reserve.set_active(true);
        assert!(
            wait_until(|| reserve.ranges().last().is_some_and(|&(_, b)| b >= 40_000)),
            "{:?}",
            reserve.ranges()
        );
        // The window's reads in flight land too.
        std::thread::sleep(Duration::from_millis(50));
        down.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(50));
        reserve.set_active(false);
        // Read on like a demuxer: a few blocks, then a little back.
        let mut pos = 1000u64;
        let mut out = vec![0u8; 2500];
        while pos + 2500 <= 38_000 {
            r.read_exact(&mut out)
                .unwrap_or_else(|e| panic!("offline at {pos}: {e}"));
            assert_eq!(out, data[pos as usize..pos as usize + 2500]);
            pos += 2500;
            pos -= 700;
            r.seek(SeekFrom::Start(pos)).unwrap();
        }
    }

    #[test]
    fn pipelining_hides_latency() {
        // 64 blocks at 10 ms each: ~640 ms if serial, ~40 ms with 16 in flight.
        let options = ReadAhead {
            block_size: 8192,
            blocks_ahead: 16,
            reserve_bytes: 0,
        };
        let (mut r, _) = reader(64 * 8192, 10, options);
        let started = Instant::now();
        std::io::copy(&mut r, &mut std::io::sink()).unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_millis(250), "took {elapsed:?}");
        assert_eq!(r.stats().requests, 64);
    }

    /// The first request for every block fails after a while (a link that
    /// went silent: every connection gives up on all it was asked at once),
    /// the next requests answer quickly.
    struct Blackout {
        data: Vec<u8>,
        asked: std::sync::Mutex<std::collections::HashSet<u64>>,
    }

    impl BlockSource for Blackout {
        fn fetch(
            self: Arc<Self>,
            offset: u64,
            len: usize,
        ) -> BoxFuture<'static, io::Result<Vec<u8>>> {
            Box::pin(async move {
                if self.asked.lock().unwrap().insert(offset) {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    return Err(io::Error::other("timed out"));
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok(self.data[offset as usize..offset as usize + len].to_vec())
            })
        }
    }

    #[test]
    fn a_blackout_of_every_connection_costs_one_round_trip_not_one_per_block() {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_time()
                .build()
                .unwrap(),
        );
        const BLOCK: usize = 1000;
        let data: Vec<u8> = (0..16 * BLOCK).map(|i| (i % 251) as u8).collect();
        let source = Blackout {
            data: data.clone(),
            asked: Default::default(),
        };
        let options = ReadAhead {
            block_size: BLOCK,
            blocks_ahead: 16,
            reserve_bytes: 0,
        };
        let mut r = ReadAheadReader::new(runtime, source, data.len() as u64, options);
        let started = Instant::now();
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        let took = started.elapsed();
        assert_eq!(got, data);
        // Each block alone would cost a pause and a round trip (over 3 s).
        assert!(took < Duration::from_millis(1000), "took {took:?}");
    }
}
