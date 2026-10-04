//! Pluggable data-block read engine (P3.2).
//!
//! OIFS keeps all metadata (superblock, bitmaps, inodes, indirect pointer blocks,
//! directories) on the shared memory map: it is small, hot and benefits from
//! zero-copy access. File *payload* blocks, however, can be large and cold. Reading
//! them through the mmap means every uncached page is a synchronous page fault that
//! blocks the calling thread, one fault (plus kernel readahead) at a time.
//!
//! This module lets the payload-read path pick a backend:
//!
//! | Backend | Mechanism | Platforms |
//! | :--- | :--- | :--- |
//! | [`IoBackend::Mmap`] (default) | `memcpy` out of the shared mapping | all |
//! | [`IoBackend::Pread`] | positional `pread(2)` per coalesced extent | all Unix |
//! | [`IoBackend::IoUring`] | every extent of a request (or of a whole batch) is submitted to one `io_uring` ring and the kernel services them concurrently | Linux ≥ 5.6, `io_uring` cargo feature |
//!
//! All backends read through the same OS page cache as the `MAP_SHARED` mapping
//! (no `O_DIRECT`), so data written via the mmap — including dirty pages that have
//! not been `msync`ed yet under [`crate::disk::DurabilityMode::Lazy`] — is always
//! visible to `Pread` / `IoUring` reads.
//!
//! Requesting `IoUring` where it is unavailable (non-Linux, feature disabled, kernel
//! too old, `io_uring_disabled` sysctl, seccomp-filtered container) never fails:
//! the engine falls back to `Pread`, and the effective backend is reported separately
//! from the requested one.

use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io;

/// Environment variable consulted when a [`crate::DiskManager`] is opened.
///
/// Accepts `mmap`, `pread`, or `io_uring` (aliases: `uring`, `iouring`), case-insensitive.
pub const IO_BACKEND_ENV: &str = "OIFS_IO_BACKEND";

/// Backend used to read file payload blocks (P3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[repr(u8)]
pub enum IoBackend {
    /// Copy payload bytes straight out of the shared memory map (pre-P3.2 behavior).
    /// Fastest when data is resident in the page cache; cold pages block on faults.
    #[default]
    Mmap = 0,
    /// One positional `pread(2)` per physically-contiguous extent. Portable; turns
    /// many page faults into a few large syscalls the kernel can read ahead for.
    Pread = 1,
    /// Batched asynchronous submission through a Linux `io_uring` ring. All extents of a
    /// request — or of every request in [`crate::DiskManager::read_at_batch`] — are in
    /// flight at once, letting the device work at queue depth > 1.
    IoUring = 2,
}

impl IoBackend {
    /// All backends, in declaration order.
    pub const ALL: [IoBackend; 3] = [IoBackend::Mmap, IoBackend::Pread, IoBackend::IoUring];

    /// Total decoding: unknown discriminants map to the default (`Mmap`).
    pub fn from_u8(val: u8) -> Self {
        match val {
            1 => Self::Pread,
            2 => Self::IoUring,
            _ => Self::Mmap,
        }
    }

    /// Parses a backend name (case-insensitive). Returns `None` for unknown names.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "mmap" => Some(Self::Mmap),
            "pread" => Some(Self::Pread),
            "io_uring" | "io-uring" | "iouring" | "uring" => Some(Self::IoUring),
            _ => None,
        }
    }

    /// Reads [`IO_BACKEND_ENV`]. Unset or unparsable values yield `None`.
    pub fn from_env() -> Option<Self> {
        std::env::var(IO_BACKEND_ENV)
            .ok()
            .and_then(|v| Self::parse(&v))
    }

    /// Canonical lowercase name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Mmap => "mmap",
            Self::Pread => "pread",
            Self::IoUring => "io_uring",
        }
    }

    /// Whether this backend can actually be used in the current process.
    ///
    /// `Mmap` and `Pread` are always available. `IoUring` requires Linux, the
    /// `io_uring` cargo feature, and a kernel that permits ring creation and supports
    /// `IORING_OP_READ` (5.6+). The probe result is computed once and cached.
    pub fn is_supported(&self) -> bool {
        match self {
            Self::Mmap | Self::Pread => true,
            Self::IoUring => uring_supported(),
        }
    }

    /// The backend that will actually run when `self` is requested.
    #[inline]
    pub fn resolve(self) -> Self {
        if self.is_supported() {
            self
        } else {
            Self::Pread
        }
    }
}

/// Minimum recommended Linux kernel version for stable production io_uring operation.
pub const MIN_RECOMMENDED_KERNEL: (u32, u32) = (5, 15);

/// Minimum RHEL 9 build number (5.14.0-362+) where io_uring was officially backported (RHEL 9.3+).
///
/// Historical context:
/// - RHEL 9.0 ~ 9.2: Kernel compiled with `CONFIG_IO_URING=n` by default due to upstream CVEs; unsupported.
/// - RHEL 9.3 (Nov 2023, kernel `5.14.0-362.8.1.el9_3`): Red Hat officially backported io_uring
///   kernel patches and SELinux support as a Technology Preview.
/// - RHEL 9.4+ (May 2024+, kernel `5.14.0-427+`): Added sysctl `kernel.io_uring_disabled`
///   (0 = allow all, 1 = privileged/group only, 2 = disabled). If sysctl blocks creation,
///   `IoEngine` cleanly falls back to `Pread`.
pub const MIN_RHEL9_BUILD_FOR_IO_URING: u32 = 362;

/// Parses major and minor numbers from a kernel release string (e.g. "5.15.0-76-generic" -> (5, 15)).
pub fn parse_kernel_version(release: &str) -> Option<(u32, u32)> {
    let mut numbers = release
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty());
    let major = numbers.next()?.parse::<u32>().ok()?;
    let minor = numbers.next()?.parse::<u32>().ok()?;
    Some((major, minor))
}

/// Parses the 4th numeric component in a release string, corresponding to RHEL's build number
/// (e.g. "5.14.0-362.8.1.el9_3.x86_64" -> Some(362)).
pub fn parse_rhel_build(release: &str) -> Option<u32> {
    let mut numbers = release
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty());
    let _maj = numbers.next()?;
    let _min = numbers.next()?;
    let _patch = numbers.next()?;
    numbers.next()?.parse::<u32>().ok()
}

/// Checks whether a given kernel release string qualifies for io_uring support:
/// - Standard mainline/distro Linux >= 5.15, OR
/// - RHEL 9.3+ backported kernel (5.14.0-362+ on .el9).
pub fn is_kernel_version_supported(release: &str) -> bool {
    let Some((major, minor)) = parse_kernel_version(release) else {
        return false;
    };
    if (major, minor) >= MIN_RECOMMENDED_KERNEL {
        return true;
    }
    // RHEL 9: 5.14.0-xxx with "el9" in release
    if major == 5
        && minor == 14
        && release.contains("el9")
        && let Some(build) = parse_rhel_build(release)
    {
        return build >= MIN_RHEL9_BUILD_FOR_IO_URING;
    }
    false
}

/// Returns the running OS kernel release string on Linux as `Option<String>`, or `None` if
/// unsupported or undetectable. Cached in memory via `OnceLock` across the process lifetime.
pub fn current_kernel_release() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        use std::sync::OnceLock;
        static RELEASE: OnceLock<Option<String>> = OnceLock::new();
        RELEASE
            .get_or_init(|| {
                let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
                if unsafe { libc::uname(&mut uts) } != 0 {
                    return None;
                }
                let release_bytes: Vec<u8> = uts
                    .release
                    .iter()
                    .take_while(|&&c| c != 0)
                    .map(|&c| c as u8)
                    .collect();
                std::str::from_utf8(&release_bytes)
                    .ok()
                    .map(|s| s.to_string())
            })
            .clone()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Returns the running OS kernel version on Linux as `Some((major, minor))`, or `None` if
/// unsupported or undetectable.
pub fn current_kernel_version() -> Option<(u32, u32)> {
    current_kernel_release()
        .as_deref()
        .and_then(parse_kernel_version)
}

impl std::fmt::Display for IoBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One contiguous transfer: `len` bytes from image offset `disk_offset` into
/// `buf[buf_offset..buf_offset + len]` of the owning [`ReadTarget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadExtent {
    pub disk_offset: u64,
    pub buf_offset: usize,
    pub len: usize,
}

/// Ordered list of extents for one destination buffer.
///
/// [`ExtentList::push`] coalesces an extent into its predecessor when both the image
/// range and the buffer range continue exactly where the previous one ended. A file
/// whose blocks were allocated contiguously therefore collapses into a single extent,
/// i.e. a single `pread` / single SQE regardless of its size in blocks.
#[derive(Debug, Default, Clone)]
pub struct ExtentList {
    extents: Vec<ReadExtent>,
}

impl ExtentList {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a transfer, merging it into the previous extent when contiguous.
    /// Zero-length transfers are ignored.
    #[inline]
    pub fn push(&mut self, disk_offset: u64, buf_offset: usize, len: usize) {
        if len == 0 {
            return;
        }
        if let Some(last) = self.extents.last_mut()
            && last.disk_offset.checked_add(last.len as u64) == Some(disk_offset)
            && last.buf_offset.checked_add(last.len) == Some(buf_offset)
        {
            last.len += len;
            return;
        }
        self.extents.push(ReadExtent {
            disk_offset,
            buf_offset,
            len,
        });
    }

    #[inline]
    pub fn as_slice(&self) -> &[ReadExtent] {
        &self.extents
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.extents.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.extents.is_empty()
    }

    /// Sum of all extent lengths.
    pub fn total_bytes(&self) -> usize {
        self.extents.iter().map(|e| e.len).sum()
    }

    pub fn clear(&mut self) {
        self.extents.clear();
    }
}

/// A destination buffer together with the extents that fill it.
pub struct ReadTarget<'a> {
    pub buf: &'a mut [u8],
    pub extents: &'a [ReadExtent],
}

/// Rejects any extent that falls outside its buffer or outside the image.
///
/// This is the safety precondition for the `io_uring` backend, which hands raw buffer
/// pointers to the kernel, and keeps all backends' error behavior identical.
fn validate(targets: &[ReadTarget<'_>], image_len: u64) -> io::Result<()> {
    for t in targets {
        for e in t.extents {
            let buf_ok = e
                .buf_offset
                .checked_add(e.len)
                .is_some_and(|end| end <= t.buf.len());
            let disk_ok = e
                .disk_offset
                .checked_add(e.len as u64)
                .is_some_and(|end| end <= image_len);
            if !buf_ok || !disk_ok {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "read extent out of range (disk_offset={}, buf_offset={}, len={}, buf_len={}, image_len={})",
                        e.disk_offset,
                        e.buf_offset,
                        e.len,
                        t.buf.len(),
                        image_len
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Per-`DiskManager` read engine: the requested backend, the backend actually in use,
/// and (for `IoUring`) a pool of reusable rings.
pub(crate) struct IoEngine {
    requested: IoBackend,
    effective: IoBackend,
    #[cfg(all(target_os = "linux", feature = "io_uring"))]
    rings: Option<uring::RingPool>,
}

impl IoEngine {
    /// Builds an engine, falling back to `Pread` if `requested` is unavailable.
    pub fn new(requested: IoBackend) -> Self {
        #[cfg(all(target_os = "linux", feature = "io_uring"))]
        {
            if requested == IoBackend::IoUring {
                if let Ok(pool) = uring::RingPool::new() {
                    return Self {
                        requested,
                        effective: IoBackend::IoUring,
                        rings: Some(pool),
                    };
                }
                return Self {
                    requested,
                    effective: IoBackend::Pread,
                    rings: None,
                };
            }
            Self {
                requested,
                effective: requested,
                rings: None,
            }
        }
        #[cfg(not(all(target_os = "linux", feature = "io_uring")))]
        {
            Self {
                requested,
                effective: requested.resolve(),
            }
        }
    }

    #[inline]
    pub fn requested(&self) -> IoBackend {
        self.requested
    }

    #[inline]
    pub fn effective(&self) -> IoBackend {
        self.effective
    }

    /// Executes every extent of every target.
    ///
    /// `mmap` must be the shared mapping of `file`; its length is the image length
    /// used for bounds checks. Bytes of a buffer not covered by any extent are left
    /// untouched. Extents within one buffer must not overlap.
    pub fn read(&self, file: &File, mmap: &[u8], targets: &mut [ReadTarget<'_>]) -> io::Result<()> {
        validate(targets, mmap.len() as u64)?;
        match self.effective {
            IoBackend::Mmap => {
                read_mmap(mmap, targets);
                Ok(())
            }
            IoBackend::Pread => read_pread(file, targets),
            IoBackend::IoUring => {
                #[cfg(all(target_os = "linux", feature = "io_uring"))]
                {
                    match &self.rings {
                        Some(pool) => pool.read(file, targets),
                        None => read_pread(file, targets),
                    }
                }
                #[cfg(not(all(target_os = "linux", feature = "io_uring")))]
                {
                    read_pread(file, targets)
                }
            }
        }
    }
}

fn read_mmap(mmap: &[u8], targets: &mut [ReadTarget<'_>]) {
    for t in targets.iter_mut() {
        for e in t.extents {
            let src = e.disk_offset as usize;
            t.buf[e.buf_offset..e.buf_offset + e.len].copy_from_slice(&mmap[src..src + e.len]);
        }
    }
}

fn read_pread(file: &File, targets: &mut [ReadTarget<'_>]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    for t in targets.iter_mut() {
        for e in t.extents {
            // read_exact_at retries on EINTR and on short reads.
            file.read_exact_at(
                &mut t.buf[e.buf_offset..e.buf_offset + e.len],
                e.disk_offset,
            )?;
        }
    }
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "io_uring"))]
fn uring_supported() -> bool {
    use std::sync::OnceLock;
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        // Enforce kernel >= 5.15 or RHEL 9.3+ backport (5.14.0-362+ on el9) unless explicitly overridden
        if std::env::var("OIFS_ALLOW_PRE_5_15").is_err() {
            if let Some(release) = current_kernel_release() {
                if !is_kernel_version_supported(&release) {
                    return false;
                }
            }
        }
        uring::probe().is_ok()
    })
}

#[cfg(not(all(target_os = "linux", feature = "io_uring")))]
fn uring_supported() -> bool {
    false
}

#[cfg(all(target_os = "linux", feature = "io_uring"))]
mod uring {
    //! `io_uring` backend. Rings are pooled per engine so concurrent readers holding the
    //! shared `DiskManager` read lock each get their own ring without contention.

    use super::ReadTarget;
    use io_uring::{IoUring, Probe, opcode, types};
    use std::collections::VecDeque;
    use std::fs::File;
    use std::io;
    use std::os::unix::io::AsRawFd;
    use std::sync::Mutex;

    /// Submission queue depth (and cap on in-flight operations) per ring.
    const RING_ENTRIES: u32 = 64;
    /// Largest single SQE transfer. Splitting long extents lets the kernel work on
    /// several parts of one large extent concurrently and keeps `len` within `u32`.
    const MAX_OP_BYTES: usize = 1 << 20;
    /// Idle rings kept for reuse; extra rings created under contention are dropped.
    const MAX_IDLE_RINGS: usize = 16;

    /// Creates a ring and verifies the kernel supports `IORING_OP_READ`.
    fn new_ring() -> io::Result<IoUring> {
        let ring = IoUring::new(RING_ENTRIES)?;
        let mut probe = Probe::new();
        ring.submitter().register_probe(&mut probe)?;
        if !probe.is_supported(opcode::Read::CODE) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "kernel io_uring lacks IORING_OP_READ",
            ));
        }
        Ok(ring)
    }

    pub(super) fn probe() -> io::Result<()> {
        new_ring().map(drop)
    }

    pub(super) struct RingPool {
        idle: Mutex<Vec<IoUring>>,
    }

    /// One pending transfer. Advanced in place on short reads.
    struct Op {
        ptr: *mut u8,
        len: usize,
        off: u64,
    }

    fn is_transient(err: &io::Error) -> bool {
        matches!(
            err.raw_os_error(),
            Some(libc::EINTR) | Some(libc::EAGAIN) | Some(libc::EBUSY)
        )
    }

    impl RingPool {
        pub fn new() -> io::Result<Self> {
            let ring = new_ring()?;
            Ok(Self {
                idle: Mutex::new(vec![ring]),
            })
        }

        fn checkout(&self) -> io::Result<IoUring> {
            if let Some(ring) = self.idle.lock().unwrap_or_else(|p| p.into_inner()).pop() {
                return Ok(ring);
            }
            new_ring()
        }

        fn checkin(&self, ring: IoUring) {
            let mut idle = self.idle.lock().unwrap_or_else(|p| p.into_inner());
            if idle.len() < MAX_IDLE_RINGS {
                idle.push(ring);
            }
        }

        /// Reads all extents. Targets must already be validated by the caller.
        pub fn read(&self, file: &File, targets: &mut [ReadTarget<'_>]) -> io::Result<()> {
            let mut ops = Vec::new();
            for t in targets.iter_mut() {
                let base = t.buf.as_mut_ptr();
                for e in t.extents {
                    let mut done = 0usize;
                    while done < e.len {
                        let len = (e.len - done).min(MAX_OP_BYTES);
                        ops.push(Op {
                            // SAFETY: validate() guaranteed buf_offset + len <= buf.len().
                            ptr: unsafe { base.add(e.buf_offset + done) },
                            len,
                            off: e.disk_offset + done as u64,
                        });
                        done += len;
                    }
                }
            }
            if ops.is_empty() {
                return Ok(());
            }

            let mut ring = self.checkout()?;
            let result = run(&mut ring, file, &mut ops);
            self.checkin(ring);
            result
        }
    }

    /// Drives `ops` to completion on `ring`.
    ///
    /// Invariant: this function never returns while any SQE is still in flight, because
    /// the kernel holds raw pointers into the caller's buffers. After the first hard
    /// error no new SQEs are submitted, but in-flight ones are still reaped.
    fn run(ring: &mut IoUring, file: &File, ops: &mut [Op]) -> io::Result<()> {
        let fd = types::Fd(file.as_raw_fd());
        let depth = RING_ENTRIES as usize;
        let mut pending: VecDeque<usize> = (0..ops.len()).collect();
        let mut inflight = 0usize;
        let mut first_err: Option<io::Error> = None;

        loop {
            if first_err.is_none() {
                let mut sq = ring.submission();
                while inflight < depth {
                    let Some(&idx) = pending.front() else { break };
                    let op = &ops[idx];
                    let sqe = opcode::Read::new(fd, op.ptr, op.len as u32)
                        .offset(op.off)
                        .build()
                        .user_data(idx as u64);
                    // SAFETY: the buffer region [ptr, ptr+len) stays valid and exclusively
                    // borrowed until its completion is reaped below (see invariant above).
                    if unsafe { sq.push(&sqe) }.is_err() {
                        break;
                    }
                    pending.pop_front();
                    inflight += 1;
                }
            }

            if inflight == 0 {
                break;
            }

            if let Err(e) = ring.submit_and_wait(1)
                && !is_transient(&e)
            {
                // A healthy ring only fails io_uring_enter transiently. If it fails hard
                // with SQEs in flight we cannot prove the kernel is done writing into the
                // caller's buffers, so releasing them would risk memory corruption.
                eprintln!(
                    "oifs: fatal io_uring_enter failure with {inflight} reads in flight: {e}"
                );
                std::process::abort();
            }

            for cqe in ring.completion() {
                inflight -= 1;
                let idx = cqe.user_data() as usize;
                let res = cqe.result();
                if res < 0 {
                    let err = io::Error::from_raw_os_error(-res);
                    if is_transient(&err) && first_err.is_none() {
                        pending.push_back(idx);
                    } else if first_err.is_none() {
                        first_err = Some(err);
                    }
                } else if res == 0 {
                    if first_err.is_none() {
                        first_err = Some(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "io_uring read hit end of image",
                        ));
                    }
                } else {
                    let n = res as usize;
                    let op = &mut ops[idx];
                    if n < op.len {
                        // Short read: resubmit the remainder.
                        // SAFETY: n < op.len keeps the pointer inside the same region.
                        op.ptr = unsafe { op.ptr.add(n) };
                        op.len -= n;
                        op.off += n as u64;
                        if first_err.is_none() {
                            pending.push_back(idx);
                        }
                    }
                }
            }
        }

        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extent_list_coalesces_contiguous_runs() {
        let mut l = ExtentList::new();
        l.push(4096, 0, 4096);
        l.push(8192, 4096, 4096);
        l.push(12288, 8192, 100);
        assert_eq!(l.len(), 1);
        assert_eq!(
            l.as_slice()[0],
            ReadExtent {
                disk_offset: 4096,
                buf_offset: 0,
                len: 8292
            }
        );
    }

    #[test]
    fn extent_list_splits_on_disk_or_buffer_gap() {
        let mut l = ExtentList::new();
        l.push(4096, 0, 4096);
        l.push(16384, 4096, 4096); // disk gap
        l.push(20480, 12288, 4096); // buffer gap (sparse hole skipped)
        l.push(0, 0, 0); // ignored
        assert_eq!(l.len(), 3);
        assert_eq!(l.total_bytes(), 3 * 4096);
    }

    #[test]
    fn backend_parse_and_roundtrip() {
        for b in IoBackend::ALL {
            assert_eq!(IoBackend::parse(b.as_str()), Some(b));
            assert_eq!(IoBackend::from_u8(b as u8), b);
        }
        assert_eq!(IoBackend::parse(" URING "), Some(IoBackend::IoUring));
        assert_eq!(IoBackend::parse("io-uring"), Some(IoBackend::IoUring));
        assert_eq!(IoBackend::parse("bogus"), None);
        assert_eq!(IoBackend::from_u8(200), IoBackend::Mmap);
        assert!(IoBackend::Mmap.is_supported());
        assert!(IoBackend::Pread.is_supported());
        assert!(IoBackend::IoUring.resolve().is_supported());
    }

    #[test]
    fn validate_rejects_out_of_range_extents() {
        let mut buf = vec![0u8; 16];
        let bad_buf = [ReadExtent {
            disk_offset: 0,
            buf_offset: 8,
            len: 9,
        }];
        let t = [ReadTarget {
            buf: &mut buf,
            extents: &bad_buf,
        }];
        assert_eq!(
            validate(&t, 1024).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

        let mut buf = vec![0u8; 16];
        let bad_disk = [ReadExtent {
            disk_offset: 1020,
            buf_offset: 0,
            len: 8,
        }];
        let t = [ReadTarget {
            buf: &mut buf,
            extents: &bad_disk,
        }];
        assert!(validate(&t, 1024).is_err());

        let mut buf = vec![0u8; 16];
        let overflow = [ReadExtent {
            disk_offset: u64::MAX,
            buf_offset: 0,
            len: 2,
        }];
        let t = [ReadTarget {
            buf: &mut buf,
            extents: &overflow,
        }];
        assert!(validate(&t, u64::MAX).is_err());
    }

    /// Every available backend must produce byte-identical results.
    #[test]
    fn all_backends_agree_on_raw_file() {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!("oifs_io_engine_{}.bin", std::process::id()));
        let image: Vec<u8> = (0..3 * 1024 * 1024u32)
            .map(|i| (i * 31 + 7) as u8)
            .collect();
        File::create(&path).unwrap().write_all(&image).unwrap();
        let file = File::open(&path).unwrap();
        let mmap = unsafe { memmap2::Mmap::map(&file).unwrap() };

        let mut extents = ExtentList::new();
        extents.push(10, 0, 5000);
        extents.push(1024 * 1024 + 3, 6000, 1_500_000); // > MAX_OP_BYTES: split on io_uring
        extents.push(100, 1_506_000, 1);
        let want_len = 1_506_001;

        let mut expected = vec![0xAAu8; want_len];
        expected[..5000].copy_from_slice(&image[10..5010]);
        expected[6000..1_506_000]
            .copy_from_slice(&image[1024 * 1024 + 3..1024 * 1024 + 3 + 1_500_000]);
        expected[1_506_000] = image[100];

        for b in IoBackend::ALL {
            let engine = IoEngine::new(b);
            assert_eq!(engine.requested(), b);
            assert_eq!(engine.effective(), b.resolve());
            let mut buf = vec![0xAAu8; want_len];
            engine
                .read(
                    &file,
                    &mmap,
                    &mut [ReadTarget {
                        buf: &mut buf,
                        extents: extents.as_slice(),
                    }],
                )
                .unwrap();
            assert!(buf == expected, "backend {b} returned different bytes");
        }
        drop(mmap);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_parse_kernel_version() {
        assert_eq!(parse_kernel_version("5.15.0-76-generic"), Some((5, 15)));
        assert_eq!(parse_kernel_version("6.1.0-18-amd64"), Some((6, 1)));
        assert_eq!(parse_kernel_version("5.10.120-1-pve"), Some((5, 10)));
        assert_eq!(parse_kernel_version("5.4.0-153-generic"), Some((5, 4)));
        assert_eq!(parse_kernel_version("6.8.0-40-generic"), Some((6, 8)));
        assert_eq!(
            parse_kernel_version("5.14.0-362.8.1.el9_3.x86_64"),
            Some((5, 14))
        );
        assert_eq!(parse_kernel_version("invalid"), None);
        assert_eq!(parse_kernel_version("5"), None);
    }

    #[test]
    fn test_rhel9_backport_detection() {
        // Mainline Linux >= 5.15 qualifies
        assert!(is_kernel_version_supported("5.15.0-76-generic"));
        assert!(is_kernel_version_supported("6.1.0-18-amd64"));
        assert!(is_kernel_version_supported("6.8.0-40-generic"));

        // Mainline Linux < 5.15 rejected
        assert!(!is_kernel_version_supported("5.10.120-1-pve"));
        assert!(!is_kernel_version_supported("5.4.0-153-generic"));
        assert!(!is_kernel_version_supported("5.14.0-custom-generic"));

        // RHEL 9.3+ with io_uring backport (build >= 362 on el9) qualifies
        assert!(is_kernel_version_supported("5.14.0-362.8.1.el9_3.x86_64"));
        assert!(is_kernel_version_supported("5.14.0-427.13.1.el9_4.aarch64"));
        assert!(is_kernel_version_supported("5.14.0-503.11.1.el9_5.x86_64"));
        assert!(is_kernel_version_supported("5.14.0-362.el9.x86_64"));

        // RHEL 9.0 ~ 9.2 (build < 362, pre-io_uring backport) rejected
        assert!(!is_kernel_version_supported("5.14.0-70.13.1.el9_0.x86_64"));
        assert!(!is_kernel_version_supported("5.14.0-162.6.1.el9_1.x86_64"));
        assert!(!is_kernel_version_supported("5.14.0-284.11.1.el9_2.x86_64"));
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// Coalescing never loses or invents bytes, and merges exactly when both the image
    /// range and the buffer range of the second extent continue the first.
    #[kani::proof]
    #[kani::unwind(4)]
    fn proof_extent_push_preserves_coverage() {
        let d1: u64 = kani::any();
        let b1: usize = kani::any();
        let l1: usize = kani::any();
        let d2: u64 = kani::any();
        let b2: usize = kani::any();
        let l2: usize = kani::any();
        kani::assume(l1 > 0 && l1 <= 1 << 20);
        kani::assume(l2 > 0 && l2 <= 1 << 20);
        kani::assume(d1 <= u64::MAX / 2 && b1 <= usize::MAX / 2);

        let mut list = ExtentList::new();
        list.push(d1, b1, l1);
        list.push(d2, b2, l2);

        let contiguous = d1 + l1 as u64 == d2 && b1 + l1 == b2;
        assert_eq!(list.total_bytes(), l1 + l2);
        assert_eq!(list.len() == 1, contiguous);
        let first = list.as_slice()[0];
        assert_eq!(first.disk_offset, d1);
        assert_eq!(first.buf_offset, b1);
    }

    /// `from_u8` is total and inverts the `repr(u8)` discriminant.
    #[kani::proof]
    fn proof_io_backend_from_u8_soundness() {
        let v: u8 = kani::any();
        let b = IoBackend::from_u8(v);
        if v <= 2 {
            assert_eq!(b as u8, v);
        } else {
            assert_eq!(b, IoBackend::Mmap);
        }
    }
}
