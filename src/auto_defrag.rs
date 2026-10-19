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
    ///
    /// A `NaN` threshold is meaningless: every comparison against it is
    /// false, which would silently disable the "not fragmented enough" check
    /// and defragment on *every* tick. It is therefore replaced by
    /// [`DEFAULT_FRAGMENTATION_THRESHOLD`] when the configuration is
    /// installed (see [`AutoDefragConfig::sanitized`]).
    pub fragmentation_threshold: f64,
    /// How often the coordinator measures fragmentation.
    ///
    /// Values below [`MIN_CHECK_INTERVAL`] are raised to it, because a zero
    /// interval would turn the worker into a hot loop that never sleeps and
    /// continuously contends for the inner lock.
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
            fragmentation_threshold: DEFAULT_FRAGMENTATION_THRESHOLD,
            check_interval: Duration::from_secs(300),
            min_free_blocks: 0,
            allow_journaled: false,
        }
    }
}

/// Default fragmentation ratio that triggers a defragmentation pass.
pub const DEFAULT_FRAGMENTATION_THRESHOLD: f64 = 0.25;

/// Lower bound for the check interval.
///
/// A zero (or sub-millisecond) interval would make the worker spin without
/// sleeping, hammering the inner lock and burning a full core.
pub const MIN_CHECK_INTERVAL: Duration = Duration::from_millis(10);

impl AutoDefragConfig {
    /// Creates an enabled configuration with the given fragmentation threshold.
    ///
    /// The threshold is clamped to `0.0 ..= 1.0`; a `NaN` threshold falls back
    /// to [`DEFAULT_FRAGMENTATION_THRESHOLD`].
    pub fn new(fragmentation_threshold: f64) -> Self {
        Self {
            enabled: true,
            fragmentation_threshold,
            ..Self::default()
        }
        .sanitized()
    }

    /// Returns a normalized copy of this configuration.
    ///
    /// * A `NaN` fragmentation threshold is replaced by
    ///   [`DEFAULT_FRAGMENTATION_THRESHOLD`]. (`f64::clamp` keeps `NaN`
    ///   unchanged, and every comparison against `NaN` is false, so a `NaN`
    ///   threshold would silently defragment on every tick.)
    /// * The threshold is clamped to `0.0 ..= 1.0`.
    /// * A check interval below [`MIN_CHECK_INTERVAL`] is raised to it, so the
    ///   worker can never degenerate into a hot loop.
    pub fn sanitized(&self) -> Self {
        let mut config = self.clone();
        if config.fragmentation_threshold.is_nan() {
            config.fragmentation_threshold = DEFAULT_FRAGMENTATION_THRESHOLD;
        }
        config.fragmentation_threshold = config.fragmentation_threshold.clamp(0.0, 1.0);
        if config.check_interval < MIN_CHECK_INTERVAL {
            config.check_interval = MIN_CHECK_INTERVAL;
        }
        config
    }

    /// Overrides the check interval.
    ///
    /// The interval is raised to [`MIN_CHECK_INTERVAL`] when it is smaller, so
    /// a zero interval can never turn the worker into a hot loop.
    pub fn with_check_interval(mut self, interval: Duration) -> Self {
        self.check_interval = interval.max(MIN_CHECK_INTERVAL);
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
        crate::lock_unpoisoned(&self.config).clone()
    }

    /// Replaces the configuration and (re)starts or stops the worker.
    ///
    /// The configuration is normalized first (see [`AutoDefragConfig::sanitized`]),
    /// so a `NaN` threshold or a zero check interval can never reach the worker.
    pub(crate) fn set_config(&self, config: AutoDefragConfig) {
        {
            let mut cfg = crate::lock_unpoisoned(&self.config);
            *cfg = config.sanitized();
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
        crate::lock_unpoisoned(&self.stats).clone()
    }

    /// Starts the worker thread if it is not running yet.
    fn ensure_worker(&self) {
        let mut worker = crate::lock_unpoisoned(&self.worker);
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
            let mut stop = crate::lock_unpoisoned(&self.shutdown.0);
            *stop = true;
        }
        self.shutdown.1.notify_all();
        let handle = crate::lock_unpoisoned(&self.worker).take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
        // Allow a later `set_config` to start the worker again.
        let mut stop = crate::lock_unpoisoned(&self.shutdown.0);
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
        let config = crate::lock_unpoisoned(config).clone();
        if !config.enabled {
            return;
        }

        let frag = match crate::disk::DiskManager::analyze_fragmentation_on(inner) {
            Ok(s) => s,
            Err(e) => {
                let mut st = crate::lock_unpoisoned(stats);
                st.errors += 1;
                st.last_error = Some(e.to_string());
                return;
            }
        };

        {
            let mut st = crate::lock_unpoisoned(stats);
            st.checks += 1;
            st.last_fragmentation_ratio = frag.fragmentation_ratio;
        }

        if let Some(reason) = skip_reason(inner, &config, &frag) {
            let mut st = crate::lock_unpoisoned(stats);
            st.skipped += 1;
            st.last_error = Some(reason);
            return;
        }

        match crate::disk::DiskManager::defragment_inplace_on(inner) {
            Ok(pass) => {
                let mut st = crate::lock_unpoisoned(stats);
                st.passes += 1;
                st.last_pass = Some(pass);
                st.last_error = None;
            }
            Err(e) => {
                let mut st = crate::lock_unpoisoned(stats);
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
    // Read-only snapshot of the superblock: a panic here cannot leave the
    // image inconsistent, so the poison is recovered instead of propagated.
    let journaled = crate::read_unpoisoned(inner)
        .superblock
        .has_journal_layout();
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
            let cfg = crate::lock_unpoisoned(&config);
            if !cfg.enabled {
                return;
            }
            // The stored configuration is always sanitized, so the interval is
            // never zero and the worker cannot degenerate into a hot loop.
            cfg.check_interval
        };

        // Sleep until the next tick, or until the controller is reconfigured
        // or shut down.
        {
            let (lock, cvar) = &*shutdown;
            let stop = crate::lock_unpoisoned(lock);
            let (stopped, _) = cvar
                .wait_timeout(stop, interval)
                .unwrap_or_else(|e| e.into_inner());
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

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Prove that a sanitized configuration can never carry a `NaN` threshold
    /// or a zero-length check interval.
    ///
    /// Both would silently break the coordinator: a `NaN` threshold makes every
    /// comparison false (so the "not fragmented enough" check never fires and
    /// the image is defragmented on every tick), and a zero interval turns the
    /// worker into a hot loop that never sleeps.
    #[kani::proof]
    fn proof_sanitized_config_is_always_valid() {
        let threshold: f64 = kani::any();
        let nanos: u64 = kani::any();

        let config = AutoDefragConfig {
            enabled: true,
            fragmentation_threshold: threshold,
            check_interval: Duration::from_nanos(nanos),
            ..AutoDefragConfig::default()
        };

        let sanitized = config.sanitized();

        assert!(
            !sanitized.fragmentation_threshold.is_nan(),
            "a sanitized threshold must never be NaN"
        );
        assert!(sanitized.fragmentation_threshold >= 0.0);
        assert!(sanitized.fragmentation_threshold <= 1.0);
        assert!(
            sanitized.check_interval >= MIN_CHECK_INTERVAL,
            "a sanitized interval must never be shorter than the minimum"
        );
    }
}
