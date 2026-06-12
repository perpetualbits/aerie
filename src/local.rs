// SPDX-License-Identifier: GPL-3.0-or-later
use crate::{BarEntry, GroupBy, Metric};
use anyhow::Result;
use std::{
    collections::HashMap,
    fmt::Write as FmtWrite,
    fs,
    io::{self, Read},
    path::Path,
    time::Instant,
};

/// Result of reading a /proc file that may be denied (EACCES) or gone (ENOENT).
///
/// We treat these two error conditions differently:
/// - `Gone`: the process exited between our directory scan and the file read;
///   safe to skip silently — it is a normal TOCTOU race.
/// - `Denied`: EACCES from the kernel (running unprivileged); the metric is
///   a lower bound for that group. We set the corresponding `*_denied` flag on
///   `GroupData` and report it to the UI as an incomplete value (trailing `?`).
enum ProcRead<T> {
    Ok(T),
    /// EACCES — metric is unknown, count as lower bound.
    Denied,
    /// ENOENT — process exited during scan, skip silently.
    Gone,
}

/// A snapshot of all group counters at one point in time.
///
/// Two consecutive snapshots are diffed by `sample()` to produce per-second rates.
/// The `total` field is the system-wide jiffy sum from `/proc/stat`; it is used as
/// the denominator for CPU% so the result is CPU-relative (100% = one full core).
#[derive(Clone)]
pub struct Snapshot {
    /// System-wide total CPU jiffies from `/proc/stat` `cpu ` line (sum of all fields).
    pub total: u64,
    pub collected_at: Instant,
    /// Per-group accumulated counters, keyed by the group name.
    pub groups: HashMap<String, GroupData>,
}

/// Accumulated /proc counters for one process group.
///
/// All fields are monotonically increasing cumulative totals (jiffies, bytes, counts).
/// Rates are computed by the caller by diffing two snapshots and dividing by elapsed time.
#[derive(Clone, Default)]
pub struct GroupData {
    /// Sum of `utime + stime` jiffies across all PIDs in this group.
    pub jiffies: u64,
    /// Total thread count across all PIDs (from `num_threads` in /proc/PID/stat).
    pub threads: u64,
    /// Total resident pages (from /proc/PID/statm field 1). Multiply by 4096 for bytes.
    pub rss_pages: u64,
    /// Cumulative minor page faults (field 8 in /proc/PID/stat after closing paren).
    pub minflt: u64,
    /// Cumulative major page faults (field 10 in /proc/PID/stat after closing paren).
    pub majflt: u64,
    /// PIDs belonging to this group (used by the histogram sampler to find task dirs).
    pub pids: Vec<u32>,
    /// Cumulative bytes read from storage (from /proc/PID/io `read_bytes`).
    pub disk_read: u64,
    /// Cumulative bytes written to storage (from /proc/PID/io `write_bytes`).
    pub disk_write: u64,
    /// Cumulative voluntary context switches (from /proc/PID/status).
    pub vol_ctxt: u64,
    /// Cumulative involuntary context switches (from /proc/PID/status).
    pub nonvol_ctxt: u64,
    /// Current open file descriptor count (from /proc/PID/fd entry count).
    /// Not cumulative; it reflects the instantaneous count.
    pub open_fds: usize,
    /// Current swap in use, KiB (VmSwap field in /proc/PID/status).
    pub swap_kib: u64,
    /// Cumulative nanoseconds spent waiting in the run queue (/proc/PID/schedstat field 1).
    pub sched_wait_ns: u64,
    // Denial flags: true if any PID in the group had EACCES for this metric.
    pub disk_denied: bool,
    pub status_denied: bool,
    pub fds_denied: bool,
    pub sched_denied: bool,
    pub rss_denied: bool,
    // ── cgroup v2 fields (only populated in GroupBy::Cgroup mode on cgroup v2 hosts) ──
    /// Full cgroup v2 filesystem path for this group
    /// (e.g. "/sys/fs/cgroup/system.slice/nginx.service").
    /// Set by collect() for the first PID encountered; used by sample() to read
    /// the group's own accounting files instead of summing per-PID /proc entries.
    pub cgroup_path: Option<String>,
    /// Cumulative bytes read from this cgroup's io.stat (sum across all devices).
    /// Written by sample()'s pre-pass; used for delta computation next call.
    pub cg_diskread: u64,
    /// Cumulative bytes written from this cgroup's io.stat.
    pub cg_diskwrite: u64,
    /// Cumulative CFS periods from cpu.stat (bandwidth controller total).
    pub cg_nr_periods: u64,
    /// Cumulative throttled periods from cpu.stat.
    pub cg_nr_throttled: u64,
    /// Current memory use from memory.current (bytes). 0 = unread.
    pub cg_memory: u64,
    /// PSI "some avg10" from cpu.pressure [0.0, 100.0]. 0.0 = unread/absent.
    pub cg_psi_cpu: f64,
    /// PSI "some avg10" from memory.pressure.
    pub cg_psi_mem: f64,
    /// PSI "some avg10" from io.pressure.
    pub cg_psi_io: f64,
    /// True when cgroup v2 files were successfully read in the last sample pass.
    /// Drives `cg_v2_complete` on BarEntry to distinguish "no throttle/stall" from
    /// "cgroup v2 data unavailable".
    pub cg_data_ok: bool,
}

/// Counters for one thread, populated according to ThreadFields.
///
/// Like `GroupData`, all fields except `sched_wait` are cumulative; rates come
/// from diffing two `ThreadSnapshot`s.
#[derive(Clone, Default)]
pub struct TidCounters {
    /// Sum of utime + stime jiffies for this thread.
    pub jiffies: u64,
    /// Sum of minflt + majflt page faults for this thread.
    pub faults: u64,
    /// Cumulative bytes read from storage for this thread.
    pub disk_read: u64,
    /// Cumulative bytes written to storage for this thread.
    pub disk_write: u64,
    /// Cumulative voluntary context switches for this thread.
    pub vol_ctxt: u64,
    /// Cumulative involuntary context switches for this thread.
    pub nonvol_ctxt: u64,
    /// Cumulative nanoseconds in the run queue for this thread.
    pub sched_wait: u64,
}

/// Per-group thread snapshot. Keyed by TID (unique system-wide on Linux).
///
/// TIDs are used rather than (PID, TID) pairs because a thread may be reparented
/// but retains its TID; keying by TID avoids false delta resets on reparenting.
#[derive(Clone)]
pub struct ThreadSnapshot {
    /// System-wide total CPU jiffies at the time of this snapshot.
    pub total: u64,
    pub collected_at: Instant,
    /// Per-TID counters for each thread in the group.
    pub tids: HashMap<i32, TidCounters>,
}

/// One thread's computed per-second metrics for the thread-detail view.
pub struct ThreadSample {
    pub pid: u32,
    pub tid: i32,
    /// Thread name (comm field from /proc/PID/task/TID/stat, up to 15 chars).
    pub name: String,
    pub cpu_pct: f64,
    /// Page faults per second (minor + major combined).
    pub faults_per_s: f64,
    pub disk_read_s: f64,
    pub disk_write_s: f64,
    /// Context switches per second (voluntary + involuntary).
    pub ctx_switches_s: f64,
    /// Fraction of time this thread was waiting for a CPU (runq%), 0–100.
    pub sched_wait_pct: f64,
}

// MemberSeries is defined in main.rs (crate root) so any level — local threads,
// Proxmox VMs, future fleet nodes — can produce it without depending on this module.

/// System-level sample (network, GPU, RAPL).
///
/// All byte/energy counters are cumulative. Rates are computed by the caller
/// by diffing two `SysSample`s and dividing by `(new.at - prev.at)`.
pub struct SysSample {
    /// Cumulative bytes received across all non-loopback, non-bridge interfaces.
    pub net_rx_bytes: u64,
    /// Cumulative bytes transmitted.
    pub net_tx_bytes: u64,
    /// GPU busy percent from DRM sysfs, or None if no supported GPU was found.
    pub gpu_pct: Option<f64>,
    /// Intel RAPL package energy counter in microjoules, or None if unavailable.
    /// Note: this counter wraps (hardware-defined max, typically ~65 kJ); use
    /// `wrapping_sub` when computing the delta.
    pub rapl_uj: Option<u64>,
    /// System-wide CPU PSI "some avg10" from /proc/pressure/cpu.
    /// None if the kernel was compiled without CONFIG_PSI or the file is absent.
    pub psi_cpu: Option<f64>,
    /// System-wide memory PSI "some avg10" from /proc/pressure/memory.
    pub psi_mem: Option<f64>,
    /// System-wide I/O PSI "some avg10" from /proc/pressure/io.
    pub psi_io: Option<f64>,
    pub at: Instant,
}

/// Read the system-wide CPU total and idle jiffies from /proc/stat.
///
/// /proc/stat aggregate `cpu` line format (space-separated fields after the label):
///   user  nice  system  idle  iowait  irq  softirq  steal  guest  guest_nice
///
/// We return:
///   - `total_jiffies`: sum of **all** fields on the `cpu ` line.
///   - `idle_jiffies`: `idle` (field index 3) + `iowait` (field index 4).
///
/// The idle + iowait sum represents "non-working time": CPU time not spent executing
/// user or kernel code.  CPU% over an interval is therefore:
///
/// ```text
///   cpu% = (Δtotal − Δidle) / Δtotal × 100
/// ```
///
/// Two consecutive calls are needed to compute a meaningful percentage (the delta
/// approach).  The first call just records the baseline; the second call's result
/// minus the first gives the load over that interval.
pub fn cpu_total_and_idle() -> Result<(u64, u64)> {
    let data = fs::read_to_string("/proc/stat")?;
    let line = data
        .lines()
        .find(|l| l.starts_with("cpu "))
        .ok_or_else(|| anyhow::anyhow!("/proc/stat has no 'cpu ' line"))?;
    let fields: Vec<u64> = line[3..]
        .split_whitespace()
        .filter_map(|s| s.parse::<u64>().ok())
        .collect();
    let total: u64 = fields.iter().sum();
    // idle = field[3] (idle) + field[4] (iowait); both represent "not working" time.
    let idle = fields.get(3).copied().unwrap_or(0)
        + fields.get(4).copied().unwrap_or(0);
    Ok((total, idle))
}

/// Read bytes of memory currently in use from /proc/meminfo.
///
/// Returns `MemTotal − MemAvailable` in bytes.  `MemAvailable` (added in Linux 3.14)
/// is preferred over `MemFree` because it includes reclaimable page-cache and slab
/// memory — it is the kernel's own estimate of how much memory is truly available
/// for new allocations without swapping.
///
/// Returns 0 on any parse failure (not `Result`); this metric is best-effort and
/// the UI falls back to a "0 used" display gracefully.
pub fn mem_available_bytes() -> u64 {
    let s = match fs::read_to_string("/proc/meminfo") {
        Ok(s) => s,
        Err(_) => return 0,
    };
    s.lines()
        .find(|l| l.starts_with("MemAvailable:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

/// Read total physical RAM from /proc/meminfo and return the value in bytes.
///
/// /proc/meminfo format: `MemTotal:   16384000 kB`
/// We parse the `MemTotal` line, extract the kB value, and multiply by 1024.
/// Returns 1 (not 0) on parse failure so callers can safely divide by this value.
pub fn total_ram_bytes() -> u64 {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal:"))?
                .split_whitespace()
                .nth(1)?          // the kB number
                .parse::<u64>()
                .ok()
                .map(|kb| kb * 1024)
        })
        .unwrap_or(1)
}

/// Read the system-wide CPU jiffy total from /proc/stat.
///
/// /proc/stat format: `cpu  utime nice stime iowait irq softirq steal guest guest_nice`
/// We sum all whitespace-separated numbers after the `cpu ` prefix. This gives the
/// total clock ticks consumed by the entire system, used as the denominator for CPU%.
pub fn cpu_total() -> Result<u64> {
    let data = fs::read_to_string("/proc/stat")?;
    let line = data
        .lines()
        .find(|l| l.starts_with("cpu "))
        .ok_or_else(|| anyhow::anyhow!("/proc/stat has no 'cpu ' line"))?;
    Ok(line[3..]
        .split_whitespace()
        .filter_map(|s| s.parse::<u64>().ok())
        .sum())
}

// /proc/PID/stat field layout (after splitting on the closing paren):
//
//   Full format: "PID (comm) state ppid pgroup session tty tpgid flags
//                 minflt cminflt majflt cmajflt utime stime cutime cstime
//                 priority nice num_threads itrealvalue starttime ... "
//
// The comm field can contain spaces and parentheses, so we parse it by finding the
// first '(' and the last ')' rather than splitting on whitespace.
//
// Indices into `rest[]` (the fields after the closing paren + space):
//   [0] = state   [1] = ppid   [7] = minflt   [9] = majflt
//   [11] = utime  [12] = stime [17] = num_threads
fn parse_proc_stat(content: &str) -> Option<ProcStat> {
    let lp = content.find('(')?;
    let rp = content.rfind(')')?;
    let pid: i32 = content[..lp].trim().parse().ok()?;
    let comm = content[lp + 1..rp].to_string();
    let rest: Vec<&str> = content[rp + 2..].split_whitespace().collect();
    Some(ProcStat {
        pid,
        comm,
        ppid: rest.get(1)?.parse().ok()?,
        minflt: rest.get(7)?.parse().ok()?,
        majflt: rest.get(9)?.parse().ok()?,
        utime: rest.get(11)?.parse().ok()?,
        stime: rest.get(12)?.parse().ok()?,
        num_threads: rest.get(17)?.parse().ok()?,
    })
}

/// Parsed fields we need from /proc/PID/stat.
struct ProcStat {
    #[allow(dead_code)]
    pid: i32,
    /// Process name (up to 15 chars, truncated by the kernel; can include spaces).
    comm: String,
    /// Parent PID — used to identify kernel threads (ppid == 2 or pid == 2).
    ppid: i32,
    minflt: u64,
    majflt: u64,
    utime: u64,
    stime: u64,
    num_threads: i64,
}

/// Read resident set size (RSS) in pages from /proc/PID/statm.
///
/// /proc/PID/statm format (space-separated, all in pages):
///   size  resident  shared  text  lib  data  dirty
/// We want field index 1 (`resident`), which is the number of physical RAM pages
/// currently backing this process. Multiply by PAGE_SIZE (4096) for bytes.
fn read_rss_pages(pid: u32) -> ProcRead<u64> {
    match fs::read_to_string(format!("/proc/{pid}/statm")) {
        Ok(s) => {
            let v = s.split_whitespace().nth(1).and_then(|x| x.parse().ok()).unwrap_or(0);
            ProcRead::Ok(v)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => ProcRead::Gone,
        Err(_) => ProcRead::Denied,
    }
}

/// Read disk I/O bytes from /proc/PID/io.
///
/// /proc/PID/io format (key: value lines):
///   rchar, wchar, syscr, syscw, read_bytes, write_bytes, cancelled_write_bytes
///
/// We use `read_bytes` and `write_bytes` rather than `rchar`/`wchar` because
/// those are the actual storage I/O bytes, not the bytes passing through the
/// page-cache read/write syscall interface.
///
/// Returns `ProcRead` wrapping (read_bytes, write_bytes).
fn read_io(pid: u32) -> ProcRead<(u64, u64)> {
    let content = match fs::read_to_string(format!("/proc/{pid}/io")) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return ProcRead::Gone,
        Err(_) => return ProcRead::Denied,
    };
    let mut rb = 0u64;
    let mut wb = 0u64;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("read_bytes:") {
            rb = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("write_bytes:") {
            wb = rest.trim().parse().unwrap_or(0);
        }
    }
    ProcRead::Ok((rb, wb))
}

/// Read swap usage and context switch counts from /proc/PID/status.
///
/// /proc/PID/status is a human-readable text file with `Key:\tValue` lines.
/// We extract:
///   - `VmSwap`: swap space in use, reported in kB (we store kB to avoid early overflow).
///   - `voluntary_ctxt_switches`: thread blocked on I/O or sleep (good indicator of I/O wait).
///   - `nonvoluntary_ctxt_switches`: scheduler preempted the thread (CPU contention).
///
/// Returns `ProcRead` wrapping (swap_kib, voluntary_ctxt, nonvoluntary_ctxt).
fn read_status_extras(pid: u32) -> ProcRead<(u64, u64, u64)> {
    let content = match fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return ProcRead::Gone,
        Err(_) => return ProcRead::Denied,
    };
    let mut swap_kib = 0u64;
    let mut vol = 0u64;
    let mut nonvol = 0u64;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("VmSwap:") {
            // VmSwap format: "VmSwap:   1234 kB" — take the first whitespace token.
            swap_kib = rest.split_whitespace().next().and_then(|s| s.parse().ok()).unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("voluntary_ctxt_switches:") {
            vol = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("nonvoluntary_ctxt_switches:") {
            nonvol = rest.trim().parse().unwrap_or(0);
        }
    }
    ProcRead::Ok((swap_kib, vol, nonvol))
}

/// Count open file descriptors for a process by counting entries in /proc/PID/fd.
///
/// Each entry in /proc/PID/fd is a symlink to the open file/socket/pipe.
/// We count them via `read_dir` rather than opening each entry, so this is
/// O(open_fds) in directory entries, not O(open_fds) in syscalls per entry.
///
/// This is the most expensive per-PID operation; only call when `need_fds` is set.
fn count_fds(pid: u32) -> ProcRead<usize> {
    match fs::read_dir(format!("/proc/{pid}/fd")) {
        Ok(d) => ProcRead::Ok(d.flatten().count()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => ProcRead::Gone,
        Err(_) => ProcRead::Denied,
    }
}

/// Which expensive optional metrics to collect in each snapshot.
///
/// Callers set flags based on which metrics are currently displayed or sorted by.
/// Disabling unused flags avoids reading several hundred /proc entries per PID
/// (e.g. not opening /proc/PID/io for every process when disk metrics are hidden).
pub struct CollectOpts {
    /// Disk read/write bytes (reads /proc/PID/io).
    pub need_io: bool,
    /// Context-switch counts and swap usage (reads /proc/PID/status).
    pub need_status: bool,
    /// Open FD count (iterates /proc/PID/fd — expensive). Only enable when needed.
    pub need_fds: bool,
    /// Scheduler wait time (reads /proc/PID/schedstat).
    pub need_schedstat: bool,
    /// RSS memory pages (reads /proc/PID/statm).
    pub need_rss: bool,
}

impl Default for CollectOpts {
    /// All metrics enabled — used by the daemon, which must support all display combinations.
    fn default() -> Self {
        Self { need_io: true, need_status: true, need_fds: true, need_schedstat: true, need_rss: true }
    }
}

/// Which optional files to read per thread. `stat` is always read.
///
/// Used to minimise /proc reads in the histogram sampler: we only open the files
/// that contribute to the metric currently shown in the overlay.
pub struct ThreadFields {
    /// /proc/PID/task/TID/io — provides disk_read, disk_write per thread.
    pub io: bool,
    /// /proc/PID/task/TID/status — provides vol_ctxt, nonvol_ctxt per thread.
    pub status: bool,
    /// /proc/PID/task/TID/schedstat — provides sched_wait per thread.
    pub schedstat: bool,
}

impl ThreadFields {
    /// Enable all optional files (used in the thread-detail view where all metrics are shown).
    pub fn all() -> Self {
        Self { io: true, status: true, schedstat: true }
    }
}

/// Read nanoseconds spent waiting in the run queue from /proc/PID/schedstat.
///
/// /proc/PID/schedstat format (space-separated):
///   cpu_time_ns  wait_time_ns  timeslices
///
/// We want field index 1 (`wait_time_ns`): the total nanoseconds this process
/// was runnable but waiting for a CPU. Dividing by elapsed wall-clock nanoseconds
/// gives the "scheduler wait %" — the fraction of time the process *wants* to run
/// but cannot because all CPUs are busy.
fn read_schedstat(pid: u32) -> ProcRead<u64> {
    match fs::read_to_string(format!("/proc/{pid}/schedstat")) {
        Ok(s) => {
            let v = s.split_whitespace().nth(1).and_then(|x| x.parse().ok()).unwrap_or(0);
            ProcRead::Ok(v)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => ProcRead::Gone,
        Err(_) => ProcRead::Denied,
    }
}

/// Read system-wide network rx/tx bytes from /proc/net/dev, excluding loopback
/// and bridge/bond members (interfaces with a /master symlink in sysfs).
///
/// /proc/net/dev format (after two header lines):
///   iface: rx_bytes rx_packets rx_errs rx_drop ... tx_bytes ...
/// Field positions (0-indexed after the colon):
///   [0] = rx_bytes, [8] = tx_bytes
///
/// Bridge/bond members are excluded because they would double-count traffic:
/// both the member (eth0) and the bridge (br0) would show the same bytes.
/// The check is: if /sys/class/net/IFACE/master exists, skip it.
fn read_net_bytes() -> (u64, u64) {
    let content = match fs::read_to_string("/proc/net/dev") {
        Ok(s) => s,
        Err(_) => return (0, 0),
    };
    let mut rx_total = 0u64;
    let mut tx_total = 0u64;
    for line in content.lines().skip(2) {
        // Format: "  eth0: rx_bytes rx_packets ... tx_bytes ..."
        let line = line.trim();
        let colon = match line.find(':') {
            Some(i) => i,
            None => continue,
        };
        let iface = line[..colon].trim();
        if iface == "lo" {
            continue;
        }
        // Skip bridge/bond members: they have a /master symlink in sysfs.
        if Path::new(&format!("/sys/class/net/{iface}/master")).exists() {
            continue;
        }
        let fields: Vec<&str> = line[colon + 1..].split_whitespace().collect();
        // fields[0] = rx_bytes, fields[8] = tx_bytes (per /proc/net/dev column layout).
        let rx: u64 = fields.first().and_then(|s| s.parse().ok()).unwrap_or(0);
        let tx: u64 = fields.get(8).and_then(|s| s.parse().ok()).unwrap_or(0);
        rx_total += rx;
        tx_total += tx;
    }
    (rx_total, tx_total)
}

/// Try to read GPU busy percentage from sysfs (AMD/Intel DRM).
///
/// The DRM subsystem exposes `gpu_busy_percent` as an integer 0–100 for
/// AMD Radeon and Intel integrated graphics via:
///   /sys/class/drm/card{N}/device/gpu_busy_percent
///
/// Tries card0 through card3; returns None if none are readable (NVIDIA uses
/// a different interface and is not supported here).
fn read_gpu_pct() -> Option<f64> {
    for i in 0..4 {
        let path = format!("/sys/class/drm/card{i}/device/gpu_busy_percent");
        if let Ok(s) = fs::read_to_string(&path) {
            if let Ok(v) = s.trim().parse::<f64>() {
                return Some(v);
            }
        }
    }
    None
}

/// Try to read Intel RAPL (Running Average Power Limit) energy counter in microjoules.
///
/// RAPL exposes a monotonically increasing energy counter that wraps at a
/// hardware-defined maximum (typically 32- or 64-bit). We read the `energy_uj`
/// file and compute watts from the delta between two samples:
///   watts = (new_uj - prev_uj) [wrapping] / 1_000_000 / elapsed_secs
///
/// Two common sysfs paths are tried (kernel version dependent):
///   /sys/class/powercap/intel-rapl/intel-rapl:0/energy_uj  (older kernels)
///   /sys/class/powercap/intel-rapl:0/energy_uj             (newer kernels)
///
/// Returns None if neither path exists or is readable.
fn read_rapl_uj() -> Option<u64> {
    let paths = [
        "/sys/class/powercap/intel-rapl/intel-rapl:0/energy_uj",
        "/sys/class/powercap/intel-rapl:0/energy_uj",
    ];
    for path in &paths {
        if let Ok(s) = fs::read_to_string(path) {
            if let Ok(v) = s.trim().parse::<u64>() {
                return Some(v);
            }
        }
    }
    None
}

/// Collect a system-level sample (network, GPU, RAPL).
///
/// This is cheap — a few sysfs reads — and is called on every main refresh tick.
/// The caller stores the previous sample and computes per-second rates from the delta.
pub fn sample_sys() -> SysSample {
    let (net_rx_bytes, net_tx_bytes) = read_net_bytes();
    let (psi_cpu, psi_mem, psi_io) = read_system_psi();
    SysSample {
        net_rx_bytes,
        net_tx_bytes,
        gpu_pct: read_gpu_pct(),
        rapl_uj: read_rapl_uj(),
        psi_cpu,
        psi_mem,
        psi_io,
        at: Instant::now(),
    }
}

/// Strip the first `/`-delimited component from a kernel thread name.
///
/// Kernel threads often have names like `kworker/0:1` or `migration/3`.
/// We group them by the base name (e.g. `kworker`, `migration`) rather than
/// per-CPU variants so all workers of the same type appear as one row.
fn kernel_base_name(comm: &str) -> String {
    comm.find('/')
        .map_or_else(|| comm.to_string(), |i| comm[..i].to_string())
}

/// Extract a display key from /proc/PID/cgroup for `GroupBy::Cgroup`.
///
/// /proc/PID/cgroup format (one line per hierarchy):
///   hierarchy_id:subsystems:path
///
/// We take the *last* line with a non-trivial path (>1 char) and use the last
/// `/`-separated component, stripping `.service` and `.scope` suffixes so that
/// `nginx.service` becomes `nginx`.
///
/// Returns `None` for root cgroup (`/`) and for the `-` placeholder used on
/// some older kernels so callers can fall back to comm.
fn read_cgroup_key(pid: u32) -> Option<String> {
    let content = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    // Find the cgroup path from the last line with a meaningful path
    let path = content.lines()
        .filter_map(|l| l.splitn(3, ':').nth(2))
        .rfind(|p| p.len() > 1)?;
    // Take last meaningful path component, strip common systemd suffixes
    let comp = path.split('/').rfind(|s| !s.is_empty())?;
    let key = comp.strip_suffix(".service").unwrap_or(
             comp.strip_suffix(".scope").unwrap_or(comp));
    if key.is_empty() || key == "-" { None } else { Some(key.to_string()) }
}

/// Extract the binary basename from /proc/PID/exe for `GroupBy::Exe`.
///
/// Reads the `exe` symlink (requires read permission; may fail for other users'
/// processes). Returns `None` on failure so callers fall back to comm.
fn read_exe_key(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
}

/// Return true when the system uses cgroup v2 (unified hierarchy).
///
/// The presence of `/sys/fs/cgroup/cgroup.controllers` is the canonical indicator.
/// On pure cgroup v2 systems this file lists the available controllers (e.g.
/// "cpuset cpu io memory hugetlb pids rdma"). It is absent on cgroup v1 and
/// partially absent on hybrid v1+v2 systems (where we treat as v1 for safety).
fn is_cgroup_v2() -> bool {
    Path::new("/sys/fs/cgroup/cgroup.controllers").exists()
}

/// Read the raw cgroup v2 path component for a process from /proc/PID/cgroup.
///
/// On a pure cgroup v2 system, /proc/PID/cgroup has exactly one line:
///   `0::/system.slice/nginx.service`
/// We extract the path after "0::" (e.g. "/system.slice/nginx.service").
/// Returns None for the root cgroup ("/"), for cgroup v1 (no "0::" prefix),
/// or when the file can't be read.
fn read_cgroup_full_path(pid: u32) -> Option<String> {
    let content = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("0::") {
            let p = rest.trim();
            if p != "/" && !p.is_empty() {
                return Some(p.to_string());
            }
        }
    }
    None
}

/// Parse /sys/fs/cgroup/.../io.stat and return (rbytes, wbytes) summed across all devices.
///
/// io.stat format — one line per device, space-separated key=value pairs:
///   `8:0 rbytes=1234 wbytes=5678 rios=10 wios=20 dbytes=0 dios=0`
///
/// The first token is the device MAJ:MIN and is skipped. All other tokens are
/// parsed as key=value. Returns None if the file can't be read (e.g. cgroup root
/// or the kernel doesn't support io accounting for this cgroup).
fn read_cg_io_stat(cg_path: &str) -> Option<(u64, u64)> {
    let content = fs::read_to_string(format!("{cg_path}/io.stat")).ok()?;
    let mut rb = 0u64;
    let mut wb = 0u64;
    for line in content.lines() {
        for tok in line.split_whitespace().skip(1) {
            // tok is "key=value"
            if let Some(v) = tok.strip_prefix("rbytes=") {
                rb += v.parse::<u64>().unwrap_or(0);
            } else if let Some(v) = tok.strip_prefix("wbytes=") {
                wb += v.parse::<u64>().unwrap_or(0);
            }
        }
    }
    Some((rb, wb))
}

/// Cumulative CFS bandwidth accounting from /sys/fs/cgroup/.../cpu.stat.
struct CgCpuStat {
    /// Total number of CFS periods since the cgroup was created.
    nr_periods: u64,
    /// Number of periods in which the cgroup exhausted its CPU quota and was throttled.
    nr_throttled: u64,
}

/// Parse /sys/fs/cgroup/.../cpu.stat for CFS throttle data.
///
/// cpu.stat format — one "key value" pair per line (subset shown):
///   `usage_usec 12345678`
///   `nr_periods 1000`
///   `nr_throttled 100`
///   `throttled_usec 50000`
///
/// Returns None if the file is absent (cgroup root, or CPU controller not enabled).
fn read_cg_cpu_stat(cg_path: &str) -> Option<CgCpuStat> {
    let content = fs::read_to_string(format!("{cg_path}/cpu.stat")).ok()?;
    let mut nr_periods = 0u64;
    let mut nr_throttled = 0u64;
    for line in content.lines() {
        let mut parts = line.splitn(2, ' ');
        let key = parts.next().unwrap_or("");
        let val: u64 = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
        match key {
            "nr_periods"   => nr_periods = val,
            "nr_throttled" => nr_throttled = val,
            _              => {}
        }
    }
    Some(CgCpuStat { nr_periods, nr_throttled })
}

/// Read current memory use from /sys/fs/cgroup/.../memory.current (bytes).
///
/// Returns None if the file is absent, not a number (the value "max" is used
/// for the root cgroup), or otherwise unreadable.
fn read_cg_memory_current(cg_path: &str) -> Option<u64> {
    fs::read_to_string(format!("{cg_path}/memory.current"))
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Read PSI "some avg10" from a cgroup pressure file (cpu.pressure / memory.pressure / io.pressure).
///
/// Pressure file format:
///   `some avg10=N.NN avg60=N.NN avg300=N.NN total=N`
///   `full avg10=N.NN avg60=N.NN avg300=N.NN total=N`
///
/// We return the "some avg10" value [0.0, 100.0]: the percentage of the last 10 s
/// during which at least one task in the cgroup was stalled on this resource.
/// "full" (all tasks stalled) is a stricter condition not returned here.
///
/// Returns None if the file is absent (CONFIG_PSI not compiled in, or the
/// cgroup controller does not expose pressure data).
fn read_cg_psi(cg_path: &str, resource: &str) -> Option<f64> {
    let content = fs::read_to_string(format!("{cg_path}/{resource}.pressure")).ok()?;
    parse_psi_some_avg10(&content)
}

/// Read system-wide PSI for all three resources from /proc/pressure/{cpu,memory,io}.
///
/// Returns (cpu_some_avg10, mem_some_avg10, io_some_avg10); each is None if
/// the corresponding file is absent (kernel built without CONFIG_PSI).
fn read_system_psi() -> (Option<f64>, Option<f64>, Option<f64>) {
    let cpu = fs::read_to_string("/proc/pressure/cpu").ok().and_then(|s| parse_psi_some_avg10(&s));
    let mem = fs::read_to_string("/proc/pressure/memory").ok().and_then(|s| parse_psi_some_avg10(&s));
    let io  = fs::read_to_string("/proc/pressure/io").ok().and_then(|s| parse_psi_some_avg10(&s));
    (cpu, mem, io)
}

/// Extract the "some avg10" value from a PSI file's content string.
///
/// Searches for a line beginning with "some " and parses the "avg10=N.NN" token.
/// This is factored out so tests can call it directly on fixture strings without
/// touching the filesystem.
fn parse_psi_some_avg10(content: &str) -> Option<f64> {
    for line in content.lines() {
        if line.starts_with("some ") {
            for tok in line.split_whitespace() {
                if let Some(v) = tok.strip_prefix("avg10=") {
                    return v.parse::<f64>().ok();
                }
            }
        }
    }
    None
}

/// Scan /proc and accumulate per-group counters into a `Snapshot`.
///
/// Iterates all numeric directories in /proc, reads each process's stat file,
/// and accumulates jiffies, pages, fault counts, and (if enabled by `opts`)
/// disk I/O, context switches, open FDs, schedstat, and RSS.
///
/// Kernel threads (PID 2 and children of PID 2) are identified by ppid and
/// grouped by the base name of their comm field (stripping per-CPU `/N` suffixes).
///
/// This function is the hot path. Each `/proc` scan touches O(n_pids) files
/// (at minimum `/proc/PID/stat`). Optional metrics multiply the file-open count.
fn collect(opts: &CollectOpts, group_by: GroupBy) -> Result<Snapshot> {
    let total = cpu_total()?;
    let collected_at = Instant::now();
    let mut groups: HashMap<String, GroupData> = HashMap::new();

    for entry in fs::read_dir("/proc")?.flatten() {
        let fname = entry.file_name();
        let fname = fname.to_string_lossy();
        // /proc contains both numeric PID directories and non-numeric entries (net, sys, …).
        let Ok(pid) = fname.parse::<u32>() else { continue };

        let stat_path = Path::new("/proc").join(&*fname).join("stat");
        let Ok(content) = fs::read_to_string(&stat_path) else { continue };
        let Some(s) = parse_proc_stat(&content) else { continue };

        // Kernel threads: PID 2 is kthreadd (the kernel thread manager), and all
        // threads with ppid == 2 are kernel workers. We still collect their CPU%
        // but strip the per-CPU variant suffix from the comm key.
        let is_kernel = pid == 2 || s.ppid == 2;
        let name = match group_by {
            GroupBy::Comm => if is_kernel { kernel_base_name(&s.comm) } else { s.comm },
            GroupBy::Cgroup => read_cgroup_key(pid).unwrap_or_else(|| {
                if is_kernel { kernel_base_name(&s.comm) } else { s.comm.clone() }
            }),
            GroupBy::Exe => read_exe_key(pid).unwrap_or_else(|| {
                if is_kernel { kernel_base_name(&s.comm) } else { s.comm.clone() }
            }),
        };

        let g = groups.entry(name).or_default();

        // In cgroup mode, store the cgroup v2 filesystem path for the first PID seen
        // in this group. sample() uses it to read the group's own accounting files
        // (io.stat, cpu.stat, *.pressure) which are more accurate and avoid EACCES.
        if matches!(group_by, GroupBy::Cgroup) && g.cgroup_path.is_none() {
            g.cgroup_path = read_cgroup_full_path(pid)
                .map(|p| format!("/sys/fs/cgroup{p}"));
        }

        // utime = user-mode jiffies, stime = kernel-mode jiffies.
        g.jiffies += s.utime + s.stime;
        // num_threads can theoretically be -1 on very unusual kernels; clamp to 1.
        g.threads += s.num_threads.max(1) as u64;
        g.minflt += s.minflt;
        g.majflt += s.majflt;
        g.pids.push(pid);

        if opts.need_rss {
            match read_rss_pages(pid) {
                ProcRead::Ok(v) => g.rss_pages += v,
                ProcRead::Denied => g.rss_denied = true,
                ProcRead::Gone => {}
            }
        }

        if opts.need_io {
            match read_io(pid) {
                ProcRead::Ok((rb, wb)) => {
                    g.disk_read += rb;
                    g.disk_write += wb;
                }
                ProcRead::Denied => g.disk_denied = true,
                ProcRead::Gone => {}
            }
        }

        if opts.need_status {
            match read_status_extras(pid) {
                ProcRead::Ok((swap_kib, vol, nonvol)) => {
                    g.swap_kib += swap_kib;
                    g.vol_ctxt += vol;
                    g.nonvol_ctxt += nonvol;
                }
                ProcRead::Denied => g.status_denied = true,
                ProcRead::Gone => {}
            }
        }

        if opts.need_fds {
            match count_fds(pid) {
                ProcRead::Ok(v) => g.open_fds += v,
                ProcRead::Denied => g.fds_denied = true,
                ProcRead::Gone => {}
            }
        }

        if opts.need_schedstat {
            match read_schedstat(pid) {
                ProcRead::Ok(v) => g.sched_wait_ns += v,
                ProcRead::Denied => g.sched_denied = true,
                ProcRead::Gone => {}
            }
        }
    }

    Ok(Snapshot { total, collected_at, groups })
}

/// Collect a /proc snapshot and compute per-second rates by diffing against `prev`.
///
/// On the very first call (prev = None) or when less than one jiffy has elapsed
/// since the previous snapshot, returns an empty entry list but still returns the
/// fresh snapshot so the caller can store it for next time.
///
/// CPU% formula:
///   cpu_pct = (delta_jiffies / delta_total_jiffies) × 100
/// where delta_total_jiffies is the system-wide jiffy delta from /proc/stat.
/// Groups with CPU < 0.05% are dropped to avoid clutter from idle processes.
///
/// All rate-based metrics (disk_read_s, page_faults_s, etc.) are computed
/// by dividing the counter delta by `elapsed_secs` (wall-clock time between
/// snapshots), not by jiffy count, so they are in SI units (bytes/s, events/s).
pub fn sample(prev: Option<Snapshot>, opts: &CollectOpts, group_by: GroupBy) -> Result<(Vec<BarEntry>, Snapshot)> {
    let mut now = collect(opts, group_by)?;

    // ── cgroup v2 pre-pass ────────────────────────────────────────────────
    // When running in Cgroup mode on a cgroup v2 host, read each group's own
    // accounting files. This replaces summed per-PID /proc data with the
    // kernel's authoritative aggregate, and eliminates EACCES issues because
    // /sys/fs/cgroup files are world-readable.
    //
    // We collect the paths first (to avoid holding a mutable borrow and a
    // shared borrow simultaneously), then mutate the GroupData entries.
    let cg_v2 = is_cgroup_v2() && matches!(group_by, GroupBy::Cgroup);
    if cg_v2 {
        let paths: Vec<(String, String)> = now.groups.iter()
            .filter_map(|(label, g)| g.cgroup_path.as_ref().map(|p| (label.clone(), p.clone())))
            .collect();
        for (label, path) in paths {
            let Some(g) = now.groups.get_mut(&label) else { continue };
            // Cumulative disk I/O: stored for next-call delta computation.
            if let Some((rb, wb)) = read_cg_io_stat(&path) {
                g.cg_diskread = rb;
                g.cg_diskwrite = wb;
            }
            // CFS throttle counters: also cumulative.
            if let Some(cpu) = read_cg_cpu_stat(&path) {
                g.cg_nr_periods  = cpu.nr_periods;
                g.cg_nr_throttled = cpu.nr_throttled;
            }
            // memory.current: instantaneous, no delta needed.
            g.cg_memory = read_cg_memory_current(&path).unwrap_or(0);
            // PSI avg10: already a rolling average — read directly.
            g.cg_psi_cpu = read_cg_psi(&path, "cpu").unwrap_or(0.0);
            g.cg_psi_mem = read_cg_psi(&path, "memory").unwrap_or(0.0);
            g.cg_psi_io  = read_cg_psi(&path, "io").unwrap_or(0.0);
            // Mark this group as having valid cgroup v2 data.
            g.cg_data_ok = true;
        }
    }

    let entries = match prev {
        None => vec![],
        Some(prev) => {
            // System-wide jiffy delta — denominator for CPU%.
            let dt = now.total.saturating_sub(prev.total) as f64;
            if dt < 1.0 {
                // Guard: if the system clock or CPU counter hasn't advanced, skip this sample
                // to avoid division by near-zero producing nonsensical CPU values.
                return Ok((vec![], now));
            }
            // Wall-clock elapsed seconds — denominator for per-second rates.
            let elapsed_secs = now
                .collected_at
                .duration_since(prev.collected_at)
                .as_secs_f64()
                .max(0.001);

            now.groups
                .iter()
                .filter_map(|(name, g)| {
                    let p = prev.groups.get(name);
                    let prev_j = p.map_or(0, |p| p.jiffies);
                    let delta = g.jiffies.saturating_sub(prev_j) as f64;
                    let cpu = (delta / dt * 100.0).clamp(0.0, 100.0);
                    // Filter out completely idle groups to keep the display uncluttered.
                    if cpu < 0.05 {
                        return None;
                    }

                    let prev_faults = p.map_or(0, |p| p.minflt + p.majflt);
                    let delta_faults = (g.minflt + g.majflt).saturating_sub(prev_faults);
                    let page_faults_s = delta_faults as f64 / elapsed_secs;

                    // Disk I/O: prefer cgroup io.stat when available (world-readable,
                    // authoritative); fall back to per-PID /proc/PID/io summation.
                    let (disk_read_s, disk_write_s, disk_complete) = if cg_v2 && g.cg_data_ok {
                        let prev_rb = p.map_or(0, |p| p.cg_diskread);
                        let prev_wb = p.map_or(0, |p| p.cg_diskwrite);
                        (
                            g.cg_diskread.saturating_sub(prev_rb) as f64 / elapsed_secs,
                            g.cg_diskwrite.saturating_sub(prev_wb) as f64 / elapsed_secs,
                            true, // cgroup files are always readable
                        )
                    } else {
                        let prev_dr = p.map_or(0, |p| p.disk_read);
                        let prev_dw = p.map_or(0, |p| p.disk_write);
                        (
                            g.disk_read.saturating_sub(prev_dr) as f64 / elapsed_secs,
                            g.disk_write.saturating_sub(prev_dw) as f64 / elapsed_secs,
                            !g.disk_denied,
                        )
                    };

                    let prev_vol = p.map_or(0, |p| p.vol_ctxt);
                    let prev_nonvol = p.map_or(0, |p| p.nonvol_ctxt);
                    // Sum voluntary + involuntary into a single context-switch rate.
                    let delta_ctx = g.vol_ctxt.saturating_sub(prev_vol)
                        + g.nonvol_ctxt.saturating_sub(prev_nonvol);
                    let ctx_switches_s = delta_ctx as f64 / elapsed_secs;

                    let prev_sw = p.map_or(0, |p| p.sched_wait_ns);
                    let delta_wait_ns = g.sched_wait_ns.saturating_sub(prev_sw) as f64;
                    // sched_wait_pct: fraction of wall-clock time the group's threads were
                    // runnable but waiting. delta_wait_ns / (elapsed_secs × 1e9) × 100.
                    // Not capped at 100 because a multi-threaded group can collectively wait
                    // more than one second per wall-clock second.
                    let sched_wait_pct =
                        (delta_wait_ns / (elapsed_secs * 1e9) * 100.0).clamp(0.0, f64::MAX);

                    // CFS throttle %: delta_throttled / delta_periods × 100.
                    // Only meaningful when cgroup v2 is active; 0.0 otherwise.
                    let cfs_throttle_pct = if cg_v2 && g.cg_data_ok {
                        let prev_periods   = p.map_or(0, |p| p.cg_nr_periods);
                        let prev_throttled = p.map_or(0, |p| p.cg_nr_throttled);
                        let dp = g.cg_nr_periods.saturating_sub(prev_periods);
                        let dt = g.cg_nr_throttled.saturating_sub(prev_throttled);
                        if dp > 0 { (dt as f64 / dp as f64 * 100.0).clamp(0.0, 100.0) } else { 0.0 }
                    } else {
                        0.0
                    };

                    // Memory: prefer cgroup v2 memory.current (authoritative aggregate).
                    let (rss_bytes, rss_complete) = if cg_v2 && g.cg_data_ok && g.cg_memory > 0 {
                        (g.cg_memory, true)
                    } else {
                        (g.rss_pages * 4096, !g.rss_denied)
                    };

                    Some(BarEntry {
                        label: name.clone(),
                        value: cpu,
                        count: Some(g.threads as usize),
                        extra: format!("{} thr", g.threads),
                        fading: false,
                        fade_t: 0.0,
                        rss_bytes,
                        page_faults_s,
                        mem_pct: 0.0, // local mode: use rss_bytes / total_ram instead
                        disk_read_s,
                        disk_write_s,
                        ctx_switches_s,
                        open_fds: g.open_fds,
                        swap_bytes: g.swap_kib * 1024,
                        sched_wait_pct,
                        power_w: 0.0, // filled in by AppState::refresh() after RAPL is computed
                        disk_complete,
                        status_complete: !g.status_denied,
                        fds_complete: !g.fds_denied,
                        sched_complete: !g.sched_denied,
                        rss_complete,
                        cfs_throttle_pct,
                        psi_cpu_avg10: g.cg_psi_cpu,
                        psi_mem_avg10: g.cg_psi_mem,
                        psi_io_avg10:  g.cg_psi_io,
                        cg_v2_complete: g.cg_data_ok,
                    })
                })
                .collect()
        }
    };

    Ok((entries, now))
}

/// Sample per-thread metrics for the given process IDs.
///
/// Walks `/proc/PID/task/` for each PID and reads each TID's `stat` file, plus
/// optional `io`, `status`, and `schedstat` files according to `fields`.
///
/// `cpu_total` is the system-wide jiffy count, passed in to avoid re-reading
/// `/proc/stat` once per group per tick.
///
/// # Invariants
/// - If `prev` is None, returns an empty sample list (no delta to compute).
/// - If the jiffy delta is < 1.0, returns the new snapshot with an empty list;
///   avoids the pathological division-by-zero that would make every thread appear
///   at 100% CPU after a very short interval.
///
/// # Memory allocation
/// Reuses a single `String` buffer for path construction and a single read buffer
/// to avoid per-TID heap allocations in the inner loop.
pub fn sample_threads(
    pids: &[u32],
    prev: Option<ThreadSnapshot>,
    fields: &ThreadFields,
    cpu_total: u64,
) -> Result<(Vec<ThreadSample>, ThreadSnapshot)> {
    let collected_at = Instant::now();
    // Accumulate (pid, tid, name, counters) for all threads.
    let mut rows: Vec<(u32, i32, String, TidCounters)> = Vec::new();

    // Reuse allocations across the inner loop: one path string and one read buffer.
    let mut path = String::with_capacity(64);
    let mut buf = String::with_capacity(512);

    for &pid in pids {
        path.clear();
        let _ = write!(path, "/proc/{pid}/task");
        let base_len = path.len(); // length of "/proc/PID/task" — we truncate here each iteration

        let Ok(dir) = fs::read_dir(&path) else { continue };
        for te in dir.flatten() {
            let fname = te.file_name();
            let fname_s = fname.to_string_lossy();
            let Ok(tid) = fname_s.parse::<i32>() else { continue };

            // stat (always): cpu jiffies, faults, and thread name.
            path.truncate(base_len);
            let _ = write!(path, "/{tid}/stat");
            buf.clear();
            let Ok(mut f) = fs::File::open(&path) else { continue };
            let Ok(_) = f.read_to_string(&mut buf) else { continue };
            let Some(s) = parse_proc_stat(&buf) else { continue };

            let mut c = TidCounters {
                jiffies: s.utime + s.stime,
                faults: s.minflt + s.majflt,
                ..Default::default()
            };

            if fields.io {
                path.truncate(base_len);
                let _ = write!(path, "/{tid}/io");
                buf.clear();
                if let Ok(mut f) = fs::File::open(&path) {
                    let _ = f.read_to_string(&mut buf);
                    for line in buf.lines() {
                        if let Some(rest) = line.strip_prefix("read_bytes:") {
                            c.disk_read = rest.trim().parse().unwrap_or(0);
                        } else if let Some(rest) = line.strip_prefix("write_bytes:") {
                            c.disk_write = rest.trim().parse().unwrap_or(0);
                        }
                    }
                }
            }

            if fields.status {
                path.truncate(base_len);
                let _ = write!(path, "/{tid}/status");
                buf.clear();
                if let Ok(mut f) = fs::File::open(&path) {
                    let _ = f.read_to_string(&mut buf);
                    for line in buf.lines() {
                        if let Some(rest) = line.strip_prefix("voluntary_ctxt_switches:") {
                            c.vol_ctxt = rest.trim().parse().unwrap_or(0);
                        } else if let Some(rest) =
                            line.strip_prefix("nonvoluntary_ctxt_switches:")
                        {
                            c.nonvol_ctxt = rest.trim().parse().unwrap_or(0);
                        }
                    }
                }
            }

            if fields.schedstat {
                path.truncate(base_len);
                let _ = write!(path, "/{tid}/schedstat");
                buf.clear();
                if let Ok(mut f) = fs::File::open(&path) {
                    let _ = f.read_to_string(&mut buf);
                    // schedstat field 1 = wait_ns (nanoseconds spent in run queue).
                    c.sched_wait = buf
                        .split_whitespace()
                        .nth(1)
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                }
            }

            rows.push((pid, tid, s.comm, c));
        }
    }

    let samples = match prev {
        None => vec![],
        Some(prev) => {
            // System-wide jiffy delta — denominator for thread CPU%.
            let dt = cpu_total.saturating_sub(prev.total) as f64;
            if dt < 1.0 {
                // Interval too short to compute meaningful CPU%; return snapshot for next time.
                let tids = rows.into_iter().map(|(_, tid, _, c)| (tid, c)).collect();
                return Ok((vec![], ThreadSnapshot { total: cpu_total, collected_at, tids }));
            }
            let elapsed_secs =
                collected_at.duration_since(prev.collected_at).as_secs_f64().max(0.001);
            rows.iter()
                .map(|(pid, tid, name, c)| {
                    // Look up this TID's counters from the previous snapshot.
                    let p = prev.tids.get(tid).cloned().unwrap_or_default();
                    let delta = c.jiffies.saturating_sub(p.jiffies) as f64;
                    let delta_f = c.faults.saturating_sub(p.faults) as f64;
                    let disk_read_s =
                        c.disk_read.saturating_sub(p.disk_read) as f64 / elapsed_secs;
                    let disk_write_s =
                        c.disk_write.saturating_sub(p.disk_write) as f64 / elapsed_secs;
                    let delta_ctx = c.vol_ctxt.saturating_sub(p.vol_ctxt)
                        + c.nonvol_ctxt.saturating_sub(p.nonvol_ctxt);
                    let ctx_switches_s = delta_ctx as f64 / elapsed_secs;
                    let delta_wait = c.sched_wait.saturating_sub(p.sched_wait) as f64;
                    // sched_wait_pct: nanoseconds waiting / total wall-clock nanoseconds × 100.
                    let sched_wait_pct =
                        (delta_wait / (elapsed_secs * 1e9) * 100.0).clamp(0.0, f64::MAX);
                    ThreadSample {
                        pid: *pid,
                        tid: *tid,
                        name: name.clone(),
                        // CPU% relative to the whole system (same scale as group CPU%).
                        cpu_pct: (delta / dt * 100.0).clamp(0.0, 100.0),
                        faults_per_s: delta_f / elapsed_secs,
                        disk_read_s,
                        disk_write_s,
                        ctx_switches_s,
                        sched_wait_pct,
                    }
                })
                .collect()
        }
    };

    let tids = rows.into_iter().map(|(_, tid, _, c)| (tid, c)).collect();
    let snap = ThreadSnapshot { total: cpu_total, collected_at, tids };
    Ok((samples, snap))
}

/// Sample per-thread values for the histogram overlay.
///
/// A thin wrapper around `sample_threads` that extracts a single metric's values
/// as a plain `Vec<f64>`, packaged as a `MemberSeries` — the source-agnostic type
/// that the renderer (`fair_share_bins`) consumes. The caller (`sample_histograms`)
/// stores these in `group_member_vals` keyed by group label.
///
/// This is the *local-threads* producer of `MemberSeries`. Other levels (Proxmox VM
/// pools, fleet nodes) will supply the same type through different paths.
///
/// Parameters:
/// - `pids`: PIDs belonging to the group.
/// - `prev`: Previous `ThreadSnapshot` for delta computation (None → empty result).
/// - `metric`: Which metric to extract from the `ThreadSample` structs.
/// - `fields`: Controls which /proc files to open (should match `metric`).
/// - `cpu_total`: System-wide jiffy count from the current /proc/stat read.
pub fn sample_member_vals(
    pids: &[u32],
    prev: Option<ThreadSnapshot>,
    metric: Metric,
    fields: &ThreadFields,
    cpu_total: u64,
) -> Result<(crate::MemberSeries, ThreadSnapshot)> {
    let (samples, snap) = sample_threads(pids, prev, fields, cpu_total)?;
    let vals: Vec<f64> = samples
        .iter()
        .map(|s| match metric {
            Metric::Cpu => s.cpu_pct,
            Metric::PageFaults => s.faults_per_s,
            Metric::DiskRead => s.disk_read_s,
            Metric::DiskWrite => s.disk_write_s,
            Metric::CtxSwitches => s.ctx_switches_s,
            Metric::SchedWait => s.sched_wait_pct,
            // Non-attributable metrics: return 0.0; callers should not request these.
            _ => 0.0,
        })
        .collect();
    Ok((crate::MemberSeries { metric, vals }, snap))
}

#[cfg(test)]
mod tests {
    fn cgroup_key_from_str(s: &str) -> Option<String> {
        // Inline the parsing logic from read_cgroup_key for test purposes
        let path = s.lines()
            .filter_map(|l| l.splitn(3, ':').nth(2))
            .filter(|p| p.len() > 1)
            .last()?;
        let comp = path.split('/').filter(|s| !s.is_empty()).last()?;
        let key = comp.strip_suffix(".service").unwrap_or(
                 comp.strip_suffix(".scope").unwrap_or(comp));
        if key.is_empty() || key == "-" { None } else { Some(key.to_string()) }
    }

    #[test]
    fn cgroup_key_strips_service_suffix() {
        let cgroup = "0::/system.slice/nginx.service\n";
        assert_eq!(cgroup_key_from_str(cgroup), Some("nginx".to_string()));
    }

    #[test]
    fn cgroup_key_strips_scope_suffix() {
        let cgroup = "0::/user.slice/user-1000.slice/session-1.scope\n";
        assert_eq!(cgroup_key_from_str(cgroup), Some("session-1".to_string()));
    }

    #[test]
    fn cgroup_key_root_returns_none() {
        let cgroup = "0::/\n";
        assert_eq!(cgroup_key_from_str(cgroup), None);
    }

    #[test]
    fn cgroup_key_dash_returns_none() {
        // Some systems use "-" as the cgroup path placeholder
        let cgroup = "12:cpuset:-\n";
        assert_eq!(cgroup_key_from_str(cgroup), None);
    }

    #[test]
    fn cgroup_key_docker_container() {
        let cgroup = "0::/system.slice/docker-abc123.service\n";
        assert_eq!(cgroup_key_from_str(cgroup), Some("docker-abc123".to_string()));
    }

    // ── parse_psi_some_avg10 ──────────────────────────────────────────────

    #[test]
    fn psi_some_avg10_happy_path() {
        let content = "some avg10=1.23 avg60=0.45 avg300=0.10 total=12345\n\
                       full avg10=0.01 avg60=0.00 avg300=0.00 total=100\n";
        let v = super::parse_psi_some_avg10(content).expect("should parse");
        assert!((v - 1.23).abs() < 1e-9, "expected 1.23, got {v}");
    }

    #[test]
    fn psi_some_avg10_full_line_only_returns_none() {
        // "full" lines must not be mistaken for "some" lines.
        let content = "full avg10=5.00 avg60=4.00 avg300=3.00 total=9999\n";
        assert_eq!(super::parse_psi_some_avg10(content), None);
    }

    #[test]
    fn psi_some_avg10_absent_returns_none() {
        assert_eq!(super::parse_psi_some_avg10(""), None);
    }

    #[test]
    fn psi_some_avg10_zero() {
        let content = "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        let v = super::parse_psi_some_avg10(content).expect("should parse zero");
        assert!((v - 0.0).abs() < 1e-9);
    }

    // ── read_cg_io_stat (pure-parse variant) ─────────────────────────────

    fn cg_io_stat_from_str(content: &str) -> Option<(u64, u64)> {
        let mut rb = 0u64;
        let mut wb = 0u64;
        for line in content.lines() {
            for tok in line.split_whitespace().skip(1) {
                if let Some(v) = tok.strip_prefix("rbytes=") {
                    rb += v.parse::<u64>().unwrap_or(0);
                } else if let Some(v) = tok.strip_prefix("wbytes=") {
                    wb += v.parse::<u64>().unwrap_or(0);
                }
            }
        }
        Some((rb, wb))
    }

    #[test]
    fn io_stat_multi_device_sums_correctly() {
        let content = "8:0 rbytes=1000 wbytes=2000 rios=5 wios=3 dbytes=0 dios=0\n\
                       8:16 rbytes=500 wbytes=300 rios=2 wios=1 dbytes=0 dios=0\n";
        assert_eq!(cg_io_stat_from_str(content), Some((1500, 2300)));
    }

    #[test]
    fn io_stat_empty_gives_zeros() {
        assert_eq!(cg_io_stat_from_str(""), Some((0, 0)));
    }

    // ── read_cg_cpu_stat (pure-parse variant) ────────────────────────────

    fn cg_cpu_stat_from_str(content: &str) -> Option<(u64, u64)> {
        let mut nr_periods = 0u64;
        let mut nr_throttled = 0u64;
        for line in content.lines() {
            let mut parts = line.splitn(2, ' ');
            let key = parts.next().unwrap_or("");
            let val: u64 = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
            match key {
                "nr_periods"   => nr_periods = val,
                "nr_throttled" => nr_throttled = val,
                _              => {}
            }
        }
        Some((nr_periods, nr_throttled))
    }

    #[test]
    fn cpu_stat_with_throttle_parses() {
        let content = "usage_usec 100000000\nnr_periods 1000\nnr_throttled 250\nthrottled_usec 5000\n";
        assert_eq!(cg_cpu_stat_from_str(content), Some((1000, 250)));
    }

    #[test]
    fn cpu_stat_no_throttle_keys_gives_zeros() {
        let content = "usage_usec 9999\nuser_usec 8888\n";
        assert_eq!(cg_cpu_stat_from_str(content), Some((0, 0)));
    }

    // ── read_cgroup_full_path (pure-parse variant) ────────────────────────

    fn cgroup_full_path_from_str(content: &str) -> Option<String> {
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("0::") {
                let p = rest.trim();
                if p != "/" && !p.is_empty() {
                    return Some(p.to_string());
                }
            }
        }
        None
    }

    #[test]
    fn cgroup_full_path_pure_v2() {
        let content = "0::/system.slice/myservice.service\n";
        assert_eq!(
            cgroup_full_path_from_str(content),
            Some("/system.slice/myservice.service".into())
        );
    }

    #[test]
    fn cgroup_full_path_hybrid_v1_ignored() {
        // Hybrid/v1 systems have "0::/" for the unified hierarchy root;
        // non-zero hierarchy IDs must be ignored and "/" must return None.
        let content = "12:memory:/user.slice/user-1000.slice\n0::/\n";
        assert_eq!(cgroup_full_path_from_str(content), None);
    }

    #[test]
    fn cgroup_full_path_root_returns_none() {
        let content = "0::/\n";
        assert_eq!(cgroup_full_path_from_str(content), None);
    }

    // ── /proc/net/dev parsing (without filesystem calls) ─────────────────

    /// Mirror of read_net_bytes parsing logic, minus the sysfs master-file check.
    /// Tests the /proc/net/dev field layout: field 0 = rx_bytes, field 8 = tx_bytes.
    fn parse_net_dev(content: &str) -> (u64, u64) {
        let mut rx_total = 0u64;
        let mut tx_total = 0u64;
        for line in content.lines().skip(2) {
            let line = line.trim();
            let colon = match line.find(':') {
                Some(i) => i,
                None => continue,
            };
            let iface = line[..colon].trim();
            if iface == "lo" {
                continue;
            }
            let fields: Vec<&str> = line[colon + 1..].split_whitespace().collect();
            let rx: u64 = fields.first().and_then(|s| s.parse().ok()).unwrap_or(0);
            let tx: u64 = fields.get(8).and_then(|s| s.parse().ok()).unwrap_or(0);
            rx_total += rx;
            tx_total += tx;
        }
        (rx_total, tx_total)
    }

    #[test]
    fn net_dev_single_interface_parses_rx_tx() {
        // /proc/net/dev: 2 header lines, then "iface: rx_bytes rx_pkt ... tx_bytes ..."
        // (tx is field index 8 after the colon, 0-indexed).
        let content = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
  eth0: 1000 10 0 0 0 0 0 0 2000 20 0 0 0 0 0 0\n";
        assert_eq!(parse_net_dev(content), (1000, 2000));
    }

    #[test]
    fn net_dev_skips_loopback() {
        let content = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 9999 99 0 0 0 0 0 0 9999 99 0 0 0 0 0 0
  eth0: 1000 10 0 0 0 0 0 0 2000 20 0 0 0 0 0 0\n";
        assert_eq!(parse_net_dev(content), (1000, 2000));
    }

    #[test]
    fn net_dev_multi_interface_sums() {
        let content = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
  eth0: 1000 10 0 0 0 0 0 0 2000 20 0 0 0 0 0 0
  eth1: 500  5  0 0 0 0 0 0 300  3  0 0 0 0 0 0\n";
        assert_eq!(parse_net_dev(content), (1500, 2300));
    }

    #[test]
    fn net_dev_empty_after_headers_gives_zeros() {
        let content = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n";
        assert_eq!(parse_net_dev(content), (0, 0));
    }

    // ── cpu_total_and_idle (pure-parse variant) ───────────────────────────

    /// Parse a /proc/stat-style string for total and idle jiffies (no filesystem access).
    fn parse_cpu_total_and_idle(content: &str) -> Option<(u64, u64)> {
        let line = content.lines().find(|l| l.starts_with("cpu "))?;
        let fields: Vec<u64> = line[3..]
            .split_whitespace()
            .filter_map(|s| s.parse::<u64>().ok())
            .collect();
        let total: u64 = fields.iter().sum();
        let idle = fields.get(3).copied().unwrap_or(0)
            + fields.get(4).copied().unwrap_or(0);
        Some((total, idle))
    }

    #[test]
    fn cpu_total_and_idle_parses_fixture() {
        // /proc/stat fixture: user=100 nice=0 system=50 idle=800 iowait=50 irq=0 softirq=0 steal=0
        // total = 100+0+50+800+50+0+0+0 = 1000
        // idle  = idle(800) + iowait(50) = 850
        let content = "cpu  100 0 50 800 50 0 0 0 0 0\n\
                       cpu0 50 0 25 400 25 0 0 0 0 0\n\
                       cpu1 50 0 25 400 25 0 0 0 0 0\n";
        let (total, idle) = parse_cpu_total_and_idle(content).expect("should parse");
        assert_eq!(total, 1000, "total jiffies");
        assert_eq!(idle, 850, "idle + iowait jiffies");
    }
}
