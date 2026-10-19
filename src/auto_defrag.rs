//! Background automatic defragmentation coordinator (Blueprint item 7).
//!
//! The coordinator periodically measures the fragmentation of the data area
//! and, when it exceeds a configured threshold, runs an in-place defragmentation
//! pass. Its purpose is to keep large contiguous free runs available so that
//! the contiguous block allocator (`P4.4`) and the seekable 64 KiB chunked
//! compressor (`P4.2`) stop falling back to fragmented allocations.
//!
//! Design notes:
//!
//! * The worker owns only an `Arc<RwLock<DiskManagerInner>>`, never a
//!   `DiskManager`, so it can never keep a manager alive past its last owner.
//! * Every pass takes the inner **write** lock, so it serializes against
//!   metadata mutations exactly like any other writer; no two defrag passes
//!   can overlap.
//! * In-place defragmentation is *not* journalled. On a WAL-enabled image the
//!   coordinator therefore refuses to run unless `allow_journaled` is set
//!   explicitly.
//! * The worker is a daemon thread that exits as soon as the manager is
//!   dropped (see `AutoDefragController::shutdown`).

use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::disk::{DefragStats, DiskManagerInner, FragmentationStats};

/// Configuration of the background automatic defragmentation coordinator.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoDefragConfig {
    /// Whether the background coordinator is running.
    pub enabled: bool,
    /// Fragmentation ratio above which a defragmentation pass is triggered.
    ///
    /// The ratio is `(free_runs - 1) / (free_blocks - 1)`: `0.0` means all
    /// free space sits in a single contiguous run, `1.0` means every single
    /// free block is isolated from its neighbours.
    pub fragmentation_threshold: f64,
    /// How often the coordinator measures fragmentation.
    pub check_interval: Duration,
    /// Minimum number of free data blocks required before a pass is allowed.
    ///
    /// Guards against compacting a nearly-full image, where the pass would
    /// gain little and stall every writer for the whole rewrite.
    pub min_free_blocks: u64,
    /// Whether the coordinator may run on a WAL-enabled (journaled) image.
    ///
    /// In-place defragmentation rewrites block pointers without emitting WAL
    /// transactions, so a crash mid-pass would leave the image unrecoverable
    /// by the journal. It stays disabled by default.
    pub allow_journaled: bool,
}

impl Default for AutoDefragConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fragmentation_threshold: 0.25,
            check_interval: Duration::from_secs(300),
            min_free_blocks: 0,
            allow_journaled: false,
        }
    }
}

impl AutoDefragConfig {
    /// Creates an enabled configuration with the given fragmentation threshold.
    ///
    /// The threshold is clamped to `0.0 ..= 1.0`.
    pub fn new(fragmentation_threshold: f64) -> Self {
        Self {
            enabled: true,
            fragmentation_threshold: fragmentation_threshold.clamp(0.0, 1.0),
            ..Self::default()
        }
    }

    /// Overrides the check interval.
    pub fn with_check_interval(mut self, interval: Duration) -> Self {
        self.check_interval = interval;
        self
    }

    /// Overrides the minimum number of free blocks required for a pass.
    pub fn with_min_free_blocks(mut self, min_free_blocks: u64) -> Self {
        self.min_free_blocks = min_free_blocks;
        self
    }

    /// Allows (or forbids) defragmenting WAL-enabled images.
    pub fn with_allow_journaled(mut self, allow: bool) -> Self {
        self.allow_journaled = allow;
        self
    }
}

/// Counters describing what the background coordinator has done so far.
#[derive(Debug, Clone, Default)]
pub struct AutoDefragStats {
    /// Number of fragmentation measurements performed.
    pub checks: u64,
    /// Number of defragmentation passes that actually ran.
    pub passes: u64,
    /// Number of passes skipped because the image was not fragmented enough,
    /// too full, or journaled without `allow_journaled`.
    pub skipped: u64,
    /// Number of passes that failed.
    pub errors: u64,
    /// Fragmentation ratio measured by the most recent check.
    pub last_fragmentation_ratio: f64,
    /// Statistics of the most recent successful pass.
    pub last_pass: Option<DefragStats>,
    /// Message describing the most recent skip or failure.
    pub last_error: Option<String>,
}

/// Internal state of the coordinator.
///
/// The controller owns the shared state through `Arc`s so the worker thread can
/// be a plain function instead of a self-referential closure.
pub(crate) struct AutoDefragController {
    inner: Weak<RwLock<DiskManagerInner>>,
    config: Arc<Mutex<AutoDefragConfig>>,
    stats: Arc<Mutex<AutoDefragStats>>,
    shutdown: Arc<(Mutex<bool>, Condvar)>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl AutoDefragController {
    pub(crate) fn new(inner: &Arc<RwLock<DiskManagerInner>>) -> Self {
        Self {
            inner: Arc::downgrade(inner),
            config: Arc::new(Mutex::new(AutoDefragConfig::default())),
            stats: Arc::new(Mutex::new(AutoDefragStats::default())),
            shutdown: Arc::new((Mutex::new(false), Condvar::new())),
            worker: Mutex::new(None),
        }
    }

    /// Current configuration.
    pub(crate) fn config(&self) -> AutoDefragConfig {
        self.config.lock().unwrap().clone()
    }

    /// Replaces the configuration and (re)starts or stops the worker.
    pub(crate) fn set_config(&self, config: AutoDefragConfig) {
        {
            let mut cfg = self.config.lock().unwrap();
            *cfg = config;
        }
        // Wake the worker so it picks up the new interval / enabled flag.
        self.shutdown.1.notify_all();
        if self.config().enabled {
            self.ensure_worker();
        } else {
            self.stop_worker();
        }
    }

    /// Current counters.
    pub(crate) fn stats(&self) -> AutoDefragStats {
        self.stats.lock().unwrap().clone()
    }

    /// Starts the worker thread if it is not running yet.
    fn ensure_worker(&self) {
        let mut worker = self.worker.lock().unwrap();
        if worker.is_some() {
            return;
        }
        let handle = std::thread::Builder::new()
            .name("oifs-auto-defrag".to_string())
            .spawn({
                let config = self.config.clone();
                let stats = self.stats.clone();
                let shutdown = self.shutdown.clone();
                let inner = self.inner.clone();
                move || run_worker(config, stats, shutdown, inner)
            })
            .ok();
        *worker = handle;
    }

    /// Stops the worker thread and waits for it to finish.
    fn stop_worker(&self) {
        {
            let mut stop = self.shutdown.0.lock().unwrap();
            *stop = true;
        }
        self.shutdown.1.notify_all();
        let handle = self.worker.lock().unwrap().take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
        // Allow a later `set_config` to start the worker again.
        let mut stop = self.shutdown.0.lock().unwrap();
        *stop = false;
    }

    /// Stops the worker; called when the owning `DiskManager` is dropped.
    pub(crate) fn shutdown(&self) {
        self.stop_worker();
    }

    /// Runs one evaluation cycle: measure fragmentation, maybe defragment.
    fn run_once(
        inner: &Arc<RwLock<DiskManagerInner>>,
        config: &Arc<Mutex<AutoDefragConfig>>,
        stats: &Arc<Mutex<AutoDefragStats>>,
    ) {
        let config = config.lock().unwrap().clone();
        if !config.enabled {
            return;
        }

        let frag = match crate::disk::DiskManager::analyze_fragmentation_on(inner) {
            Ok(s) => s,
            Err(e) => {
                let mut st = stats.lock().unwrap();
                st.errors += 1;
                st.last_error = Some(e.to_string());
                return;
            }
        };

        {
            let mut st = stats.lock().unwrap();
            st.checks += 1;
            st.last_fragmentation_ratio = frag.fragmentation_ratio;
        }

        if let Some(reason) = skip_reason(inner, &config, &frag) {
            let mut st = stats.lock().unwrap();
            st.skipped += 1;
            st.last_error = Some(reason);
            return;
        }

        match crate::disk::DiskManager::defragment_inplace_on(inner) {
            Ok(pass) => {
                let mut st = stats.lock().unwrap();
                st.passes += 1;
                st.last_pass = Some(pass);
                st.last_error = None;
            }
            Err(e) => {
                let mut st = stats.lock().unwrap();
                st.errors += 1;
                st.last_error = Some(e.to_string());
            }
        }
    }
}

/// Returns the reason a pass must be skipped, or `None` when it may run.
fn skip_reason(
    inner: &Arc<RwLock<DiskManagerInner>>,
    config: &AutoDefragConfig,
    frag: &FragmentationStats,
) -> Option<String> {
    if frag.fragmentation_ratio < config.fragmentation_threshold {
        return Some(format!(
            "fragmentation {:.4} below threshold {:.4}",
            frag.fragmentation_ratio, config.fragmentation_threshold
        ));
    }
    if (frag.free_blocks as u64) < config.min_free_blocks {
        return Some(format!(
            "only {} free blocks, minimum is {}",
            frag.free_blocks, config.min_free_blocks
        ));
    }
    let journaled = inner.read().unwrap().superblock.has_journal_layout();
    if journaled && !config.allow_journaled {
        return Some(
            "image is WAL-enabled; in-place defragmentation is not journalled \
             (set allow_journaled to enable)"
                .to_string(),
        );
    }
    None
}

/// The background worker loop.
///
/// The worker only holds a [`Weak`] reference to the inner state: as soon as
/// the last `DiskManager` clone is dropped, the upgrade fails and the thread
/// exits instead of keeping the image mapped forever.
fn run_worker(
    config: Arc<Mutex<AutoDefragConfig>>,
    stats: Arc<Mutex<AutoDefragStats>>,
    shutdown: Arc<(Mutex<bool>, Condvar)>,
    inner: Weak<RwLock<DiskManagerInner>>,
) {
    loop {
        let interval = {
            let cfg = config.lock().unwrap();
            if !cfg.enabled {
                return;
            }
            cfg.check_interval
        };

        // Sleep until the next tick, or until the controller is reconfigured
        // or shut down.
        {
            let (lock, cvar) = &*shutdown;
            let stop = lock.lock().unwrap();
            let (stopped, _) = cvar.wait_timeout(stop, interval).unwrap();
            if *stopped {
                return;
            }
        }

        // The manager may have been dropped while we slept.
        let Some(inner) = inner.upgrade() else {
            return;
        };
        AutoDefragController::run_once(&inner, &config, &stats);
    }
}
