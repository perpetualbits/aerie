// SPDX-License-Identifier: GPL-3.0-or-later
// aerie — process-group performance monitor
// Copyright (C) 2026 Epsilon Null Operation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
mod local;
mod proxmox;
mod remote;
mod ui;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::Event;
use mullion::{
    backend::CrosstermBackend,
    capabilities::Capabilities,
    input::{KeyCode, KeyModifiers},
    layout::{carousel_visible_range, Node, Orientation, TileId},
    poll_event, Rect, Terminal, Theme,
    tree::{id_from_key, reconcile_carousel, Direction, Tree},
};
use std::{
    collections::HashMap,
    io,
    sync::{
        mpsc,
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};


#[derive(Parser, Debug)]
#[command(
    name = "aerie",
    version,
    about = "Thread / VM activity bar-chart monitor.\n\
             Local mode (default): reads /proc and groups processes by name.\n\
             Proxmox mode: polls the PVE API and shows per-VM CPU + memory.\n\
             Nomad mode (--nomad): monitors allocations via nomad alloc exec."
)]
struct Cli {
    /// Proxmox API base URL, e.g. https://pve.lan:8006
    #[arg(long)]
    proxmox: Option<String>,

    /// Proxmox API token — format: USER@REALM!TOKENID=SECRET
    #[arg(long, env = "PROXMOX_TOKEN")]
    token: Option<String>,

    /// Accept self-signed TLS certificates (useful for home-lab Proxmox)
    #[arg(long)]
    insecure: bool,

    /// Refresh interval in seconds (decimals ok, e.g. 0.5)
    #[arg(short, long, default_value = "2")]
    interval: f64,

    /// Show only the top-N busiest groups (0 = all)
    #[arg(short = 'n', long, default_value = "0")]
    top: usize,

    /// Run as headless daemon: stream JSON snapshots to stdout (used by remote drill-down)
    #[arg(long)]
    daemon: bool,

    /// SSH user for remote VM drill-down (defaults to current OS user)
    #[arg(long)]
    ssh_user: Option<String>,

    /// Enable SSH remote drill-down into Proxmox VMs.
    /// Without this flag, pressing Enter on a VM does nothing.
    #[arg(long, env = "AERIE_ENABLE_REMOTE", default_value_t = false)]
    enable_remote: bool,

    /// Accept unknown SSH host keys on first use (TOFU).
    /// By default aerie requires the host key to already be in known_hosts.
    /// Never passes StrictHostKeyChecking=no.
    #[arg(long, default_value_t = false)]
    ssh_accept_new: bool,

    /// Monitor a fleet of SSH hosts. Accepts a comma-separated list or @/path/to/file.
    /// Each line/item is a hostname or IP. Requires --enable-remote.
    #[arg(long)]
    hosts: Option<String>,

    /// Use a thin /proc shell probe instead of aerie --daemon on fleet hosts.
    /// Provides CPU% and memory only; works without aerie installed remotely.
    /// No per-process breakdown or drill-down available in thin mode.
    #[arg(long)]
    thin: bool,

    /// Number of snapshots to keep in the replay ring buffer (default: 120).
    /// At the default 2-second interval this is 4 minutes of history.
    #[arg(long, default_value = "120")]
    history_depth: usize,

    /// Command to run when a load-distribution anomaly is detected.
    /// Called as: CMD GROUP_LABEL ANOMALY_KIND BALANCE_FRACTION
    /// Rate-limited to at most once per 60 seconds per group.
    #[arg(long)]
    alert_cmd: Option<String>,

    /// Enable per-process GPU metrics via /proc/PID/fdinfo (AMD/Intel DRM, kernel ≥ 5.14).
    /// Without this flag, gpu% and vram always show 0 and fdinfo is never read.
    #[arg(long, default_value_t = false)]
    enable_gpu: bool,

    /// Print the built-in manual and exit
    #[arg(short = 'm', long)]
    manual: bool,

    /// [EXPERIMENTAL] Monitor Kubernetes pods via kubectl exec.
    /// Accepts NAMESPACE or NAMESPACE/SELECTOR (label selector).
    /// Examples: --kube default   --kube monitoring/app=prometheus
    /// Requires kubectl in PATH and RBAC permission to exec into pods.
    #[arg(long)]
    kube: Option<String>,

    /// kubectl context name from kubeconfig (default: current context).
    /// Used with --kube.
    #[arg(long)]
    kube_context: Option<String>,

    /// Use a thin /proc shell probe instead of aerie --daemon in the pod.
    /// Works without aerie installed in the container image; provides CPU%
    /// and memory only — no per-process breakdown or drill-down.
    /// Used with --kube.
    #[arg(long)]
    kube_thin: bool,

    /// [EXPERIMENTAL] Monitor Nomad allocations via nomad alloc exec.
    /// Provide the Nomad HTTP API address (e.g. http://nomad.lan:4646).
    /// Requires the nomad CLI in PATH.
    #[arg(long)]
    nomad: Option<String>,

    /// Nomad namespace to monitor.
    /// Used with --nomad.
    #[arg(long, env = "NOMAD_NAMESPACE", default_value = "default")]
    nomad_namespace: String,

    /// Nomad ACL token. Can also be provided via the NOMAD_TOKEN environment variable.
    /// Omit for clusters with ACL disabled.
    /// Used with --nomad.
    #[arg(long, env = "NOMAD_TOKEN")]
    nomad_token: Option<String>,

    /// Filter --nomad to allocations of one specific job name.
    /// Used with --nomad.
    #[arg(long)]
    nomad_job: Option<String>,

    /// Use a thin /proc shell probe instead of aerie --daemon in the allocation.
    /// Works without aerie installed in the task image; provides CPU% and memory only.
    /// Used with --nomad.
    #[arg(long)]
    nomad_thin: bool,
}

/// Metric displayed on one side of the combined meter bar.
///
/// Each variant maps to a field in `BarEntry`. The `name()` method returns the
/// short label shown in the header and column headers. `cycle_next`/`cycle_prev`
/// walk the ordered list appropriate for the current mode (local has more options
/// than Proxmox, which only exposes CPU, memory, and disk I/O).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Metric {
    Cpu,
    Memory,
    PageFaults,
    Threads,
    DiskRead,
    DiskWrite,
    CtxSwitches,
    OpenFds,
    SwapMem,
    SchedWait,
    Power,
    /// CFS bandwidth throttle percentage (cgroup v2, Cgroup mode only).
    /// Fraction of CFS scheduler periods in which this cgroup exhausted its
    /// CPU quota and was throttled: nr_throttled / nr_periods × 100.
    CfsThrottle,
    /// CPU pressure stall: "some avg10" from cgroup cpu.pressure [0, 100].
    /// Percentage of the last 10 s in which at least one task in this cgroup
    /// was stalled waiting for a CPU. Requires cgroup v2 + Cgroup mode.
    PsiCpu,
    /// Memory pressure stall: "some avg10" from cgroup memory.pressure.
    PsiMem,
    /// I/O pressure stall: "some avg10" from cgroup io.pressure.
    PsiIo,
    /// GPU engine time % — delta of drm-engine-* nanoseconds over elapsed time × 100.
    /// Requires --enable-gpu. AMD/Intel DRM only (kernel ≥ 5.14). Can exceed 100%
    /// when multiple GPU engines (gfx, compute, enc) run simultaneously.
    GpuPct,
    /// GPU VRAM in use, bytes — sum of drm-memory-vram across DRM file descriptors.
    /// Instantaneous (no delta). Requires --enable-gpu.
    Vram,
}

impl Metric {
    /// Short display label shown in the header bar and column headings.
    pub fn name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu% mach",
            Self::Memory => "mem",
            Self::PageFaults => "faults/s",
            Self::Threads => "threads",
            Self::DiskRead => "disk-r",
            Self::DiskWrite => "disk-w",
            Self::CtxSwitches => "ctx-sw",
            Self::OpenFds => "fds",
            Self::SwapMem => "swap",
            Self::SchedWait => "runq",
            Self::Power => "power",
            Self::CfsThrottle => "throttle",
            Self::PsiCpu => "psi-cpu",
            Self::PsiMem => "psi-mem",
            Self::PsiIo => "psi-io",
            Self::GpuPct => "gpu%",
            Self::Vram => "vram",
        }
    }

    /// Ordered list of metrics available in local (/proc) mode.
    /// CPU and Memory lead because they are the most commonly needed.
    /// The four cgroup v2 metrics appear at the end; they show 0.0 / `?` when
    /// not in cgroup v2 Cgroup mode, making them self-documenting.
    fn local_options() -> &'static [Self] {
        &[
            Self::Cpu,
            Self::Memory,
            Self::DiskRead,
            Self::DiskWrite,
            Self::PageFaults,
            Self::CtxSwitches,
            Self::SchedWait,
            Self::Power,
            Self::SwapMem,
            Self::OpenFds,
            Self::Threads,
            Self::CfsThrottle,
            Self::PsiCpu,
            Self::PsiMem,
            Self::PsiIo,
            Self::GpuPct,
            Self::Vram,
        ]
    }

    /// Ordered list of metrics available in Proxmox mode.
    /// Proxmox only exposes cpu, mem, disk-r, disk-w via the PVE API.
    fn proxmox_options() -> &'static [Self] {
        &[Self::Cpu, Self::Memory, Self::DiskRead, Self::DiskWrite]
    }

    /// Advance to the next metric in the cycle, wrapping around.
    /// `is_local` selects between the local and Proxmox option lists.
    pub fn cycle_next(self, is_local: bool) -> Self {
        let opts = if is_local { Self::local_options() } else { Self::proxmox_options() };
        let i = opts.iter().position(|&m| m == self).unwrap_or(0);
        opts[(i + 1) % opts.len()]
    }

    /// Step back to the previous metric in the cycle, wrapping around.
    pub fn cycle_prev(self, is_local: bool) -> Self {
        let opts = if is_local { Self::local_options() } else { Self::proxmox_options() };
        let i = opts.iter().position(|&m| m == self).unwrap_or(0);
        // checked_sub avoids underflow on index 0; falls back to the last element.
        opts[i.checked_sub(1).unwrap_or(opts.len() - 1)]
    }
}

/// Which side of the meter bar is being configured by ↑/↓.
///
/// Tab switches the active side. The active side determines which metric the
/// ←/→ keys cycle through, and also which metric's distribution the histogram
/// overlay visualises.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// High-level operating mode of the application.
#[derive(Clone)]
pub enum AppMode {
    /// Read /proc on the local machine.
    Local,
    /// Poll the Proxmox VE REST API at `url` using `token`.
    Proxmox { url: String, token: String, insecure: bool },
    /// Monitor multiple SSH hosts simultaneously.
    Fleet {
        /// Validated hostnames/IPs.
        hosts: Vec<String>,
        /// SSH login user.
        ssh_user: String,
        /// When true, use thin /proc probe; when false, use aerie --daemon.
        thin: bool,
    },
    /// Monitor Kubernetes pods via `kubectl exec`.
    /// Experimental — requires kubectl in PATH and appropriate RBAC.
    Kube {
        /// Kubernetes namespace to query.
        namespace: String,
        /// Optional label selector (e.g. "app=nginx"). None = all pods in namespace.
        selector: Option<String>,
        /// kubectl context name from kubeconfig. None = current context.
        context: Option<String>,
        /// When true, use thin /proc shell probe; when false, use aerie --daemon.
        thin: bool,
    },
    /// Monitor Nomad allocations via `nomad alloc exec`.
    /// Experimental — requires nomad CLI in PATH and appropriate ACL policy.
    Nomad {
        /// Nomad HTTP API address (e.g. "http://nomad.lan:4646").
        addr: String,
        /// Nomad namespace (default: "default").
        namespace: String,
        /// Optional ACL token. None = anonymous / ACL disabled.
        token: Option<String>,
        /// Optional job name filter. None = all running allocations in the namespace.
        job_filter: Option<String>,
        /// When true, use thin /proc shell probe; when false, use aerie --daemon.
        thin: bool,
    },
}

/// Which screen is currently shown.
#[derive(Clone)]
pub enum AppView {
    /// Main group list (default).
    Groups,
    /// Per-thread heat-map for a single process group.
    Threads { label: String },
    /// Scrollable in-app manual page.
    Manual,
    /// Transitional state while SSH connection is in progress.
    /// The UI displays a "Connecting…" message while `connect_vm` blocks.
    Connecting { label: String },
    /// Showing live process data from a remote VM over SSH.
    Remote { label: String },
}

/// Grouping strategy for local /proc scanning.
///
/// Controls how individual PIDs are bucketed into display rows.
/// The strategy is applied by `local::collect` when building the `Snapshot`.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum GroupBy {
    #[default]
    /// `/proc/PID/stat` comm field — the short process name (up to 15 chars).
    Comm,
    /// Last meaningful component of `/proc/PID/cgroup`, with .service/.scope stripped.
    /// Groups all processes belonging to the same systemd unit.
    Cgroup,
    /// Basename of the `/proc/PID/exe` symlink.
    /// Groups by the actual binary on disk, regardless of argv[0].
    Exe,
}

impl GroupBy {
    /// Short name shown in the header brackets, e.g. `[comm]`.
    pub fn name(self) -> &'static str {
        match self { Self::Comm => "comm", Self::Cgroup => "cgroup", Self::Exe => "exe" }
    }

    /// Advance to the next strategy in the cycle: comm → cgroup → exe → comm.
    pub fn next(self) -> Self {
        match self { Self::Comm => Self::Cgroup, Self::Cgroup => Self::Exe, Self::Exe => Self::Comm }
    }
}

/// Grouping strategy for Proxmox VM display.
///
/// Controls how VMs/CTs are bucketed into display rows in Proxmox mode.
/// The active strategy is shared with the background polling thread via
/// `AppState::pve_group_by_shared` (an `AtomicU8`) so grouping changes
/// take effect on the next sample without restarting the thread.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum PveGroupBy {
    #[default]
    /// One row per VM — same flat list as before Tier 1.
    Flat = 0,
    /// Group VMs/CTs by their Proxmox pool name.
    /// VMs without a pool appear under "(no pool)".
    Pool = 1,
    /// Group VMs/CTs by their first tag (semicolon-delimited).
    /// VMs with no tags appear under "(untagged)".
    Tag = 2,
    /// Group VMs/CTs by the Proxmox node they run on.
    /// Gives a node-rollup view: how much of each host is consumed.
    Node = 3,
}

impl PveGroupBy {
    /// Short name for the header indicator, e.g. `[pool]`.
    pub fn name(self) -> &'static str {
        match self { Self::Flat => "flat", Self::Pool => "pool", Self::Tag => "tag", Self::Node => "node" }
    }

    /// Advance through the cycle: flat → pool → tag → node → flat.
    pub fn next(self) -> Self {
        match self { Self::Flat => Self::Pool, Self::Pool => Self::Tag, Self::Tag => Self::Node, Self::Node => Self::Flat }
    }

    /// Decode from the raw `u8` stored in the shared `AtomicU8`.
    pub fn from_u8(v: u8) -> Self {
        match v { 1 => Self::Pool, 2 => Self::Tag, 3 => Self::Node, _ => Self::Flat }
    }
}

/// Compute N_eff (inverse-participation / Herfindahl effective-participant count).
///
/// N_eff = (Σv)² / Σ(v²) — ranges from 1.0 (one member dominates)
/// to N (all members carry equal load). Returns N for all-zero input
/// (idle-but-balanced by convention).
fn n_eff(vals: &[f64]) -> f64 {
    let n = vals.len();
    if n < 2 {
        return n as f64;
    }
    let sum: f64 = vals.iter().sum();
    if sum < 1e-9 {
        return n as f64;
    }
    let sum_sq: f64 = vals.iter().map(|v| v * v).sum();
    if sum_sq < 1e-12 {
        return n as f64;
    }
    (sum * sum / sum_sq).clamp(1.0, n as f64)
}

/// Alert threshold: balance_frac below this flags a group as concentrated.
/// 0.35 means the effective participants drop to 35% of the total member count.
const BALANCE_ALERT_THRESHOLD: f64 = 0.35;

/// Minimum seconds between consecutive --alert-cmd firings for the same group.
const ALERT_RATE_LIMIT_S: u64 = 60;

/// Spawn the --alert-cmd with three positional arguments: GROUP_LABEL ANOMALY_KIND BALANCE_FRACTION.
///
/// The command string is split on whitespace (no shell expansion). The child is
/// spawned non-blocking (fire-and-forget) with all stdio suppressed so it does
/// not interfere with the TUI.
fn fire_alert_cmd(cmd: &str, label: &str, kind: &str, balance_frac: f64) {
    let mut parts = cmd.split_whitespace();
    let program = match parts.next() {
        Some(p) => p,
        None => return,
    };
    let extra_args: Vec<&str> = parts.collect();
    let _ = std::process::Command::new(program)
        .args(&extra_args)
        .arg(label)
        .arg(kind)
        .arg(format!("{:.3}", balance_frac))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// A GPU device discovered on this machine.
#[derive(Clone)]
pub struct GpuDevice {
    pub pci_addr: String,
    pub driver: String,
}

/// Label identifying a group across all display levels.
///
/// In local mode this is the process-group name (comm/cgroup/exe key).
/// In Proxmox mode it is the VM/CT name. In future levels it might be a pool
/// name, a node hostname, or a Kubernetes deployment label.
/// Using a named alias makes the intent clear at every call site.
pub type GroupLabel = String;

/// One entry in the replay ring buffer.
pub struct HistoryEntry {
    pub at: std::time::Instant,
    pub entries: Vec<BarEntry>,
    pub member_vals: HashMap<GroupLabel, MemberSeries>,
}

/// Per-group concentration-anomaly tracking state.
pub struct AnomalyState {
    /// N_eff / N — effective balance fraction [0, 1]. 1.0 = perfectly balanced.
    pub balance_frac: f64,
    /// Previous sample's per-member shares (for dropout detection).
    pub prev_shares: Vec<f64>,
    /// Whether an anomaly condition is currently active (drives UI highlight).
    pub alerting: bool,
    /// "concentrated" or "dropout" (empty when not alerting).
    pub kind: String,
    /// When --alert-cmd was last fired for this group (rate limit: 60 s).
    pub last_alert_at: Option<std::time::Instant>,
}

/// Per-group member values for the fair-share histogram overlay.
///
/// This is the single source-agnostic unit that the renderer (`fair_share_bins`)
/// consumes. Any level that wants to drive the overlay just produces a
/// `HashMap<GroupLabel, MemberSeries>` — the renderer doesn't care whether the
/// members are threads, VMs, pods, or nodes.
///
/// Only the one metric the overlay is currently showing is kept; storing a single
/// metric per group bounds both memory and sampling work.
#[derive(Clone)]
pub struct MemberSeries {
    /// Which metric these values are for.
    /// Used by the UI to detect mid-flight metric switches (stale data shows
    /// all-zero heat for one frame rather than wrongly-coloured heat).
    pub metric: Metric,
    /// One value per member, in the unit of `metric`
    /// (cpu%, faults/s, bytes/s, …). Order is arbitrary; `fair_share_bins`
    /// treats them as an unordered multiset.
    pub vals: Vec<f64>,
}

/// serde default helper: returns true (so old daemon output is treated as complete).
///
/// `BarEntry` completeness flags use `#[serde(default = "default_true")]` so that
/// JSON produced by older daemon versions (which lacked these fields) deserialises
/// as "fully complete" rather than "all denied". This avoids spurious `?` markers
/// when drilling into an older remote aerie.
pub fn default_true() -> bool { true }

/// One row in the display: all metrics for a single process group or VM.
///
/// In local mode the fields are filled by `local::sample`.
/// In Proxmox mode only `value` (cpu%), `rss_bytes`/`mem_pct`, and `disk_*_s`
/// are populated; the rest remain at their Default (0).
///
/// The `fading` / `fade_t` fields are set by `AppState::refresh` when a group
/// has disappeared from /proc but is still within the 5-second retention window.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct BarEntry {
    pub label: String,
    /// CPU % — always collected.
    pub value: f64,
    /// Thread count for the group (None in Proxmox mode).
    pub count: Option<usize>,
    /// Short human-readable summary shown in the extra column (thread count, VM uptime, etc.).
    pub extra: String,
    /// True while the group is idle but still within the retention window.
    #[serde(default)]
    pub fading: bool,
    /// 0.0 = active; 1.0 = at the far end of the retention window.
    /// Used to smoothly interpolate the label colour as the row fades out.
    #[serde(default)]
    pub fade_t: f64,
    /// Resident set size in bytes (from /proc/PID/statm field 1 × PAGE_SIZE, or PVE `mem`).
    pub rss_bytes: u64,
    /// Page faults per second (minor + major combined).
    pub page_faults_s: f64,
    /// Memory % of allocation (proxmox only — mem/maxmem × 100).
    pub mem_pct: f64,
    /// Disk bytes read per second (from /proc/PID/io `read_bytes`).
    pub disk_read_s: f64,
    /// Disk bytes written per second (from /proc/PID/io `write_bytes`).
    pub disk_write_s: f64,
    /// Context switches per second (voluntary + involuntary from /proc/PID/status).
    pub ctx_switches_s: f64,
    /// Open file descriptor count (number of entries in /proc/PID/fd).
    pub open_fds: usize,
    /// Swap in use, bytes (VmSwap from /proc/PID/status × 1024).
    pub swap_bytes: u64,
    /// Scheduler wait % — time threads are runnable but waiting for a CPU core.
    /// Computed from /proc/PID/schedstat field 1 (wait_ns) / elapsed_ns × 100.
    pub sched_wait_pct: f64,
    /// CPU-proportional RAPL power estimate in watts:
    /// (group_cpu% / total_cpu%) × sys_rapl_w. Only non-zero when RAPL is available.
    pub power_w: f64,
    // Completeness flags: false means EACCES was encountered for this metric.
    // serde default = true so old daemon output is treated as complete.
    #[serde(default = "default_true")]
    pub disk_complete: bool,
    #[serde(default = "default_true")]
    pub status_complete: bool,
    #[serde(default = "default_true")]
    pub fds_complete: bool,
    #[serde(default = "default_true")]
    pub sched_complete: bool,
    #[serde(default = "default_true")]
    pub rss_complete: bool,
    // ── cgroup v2 metrics ─────────────────────────────────────────────────
    /// CFS bandwidth throttle %: nr_throttled / nr_periods × 100.
    /// 0.0 when cgroup v2 is unavailable or group_by ≠ Cgroup.
    #[serde(default)]
    pub cfs_throttle_pct: f64,
    /// CPU PSI "some avg10" [0, 100] from cgroup cpu.pressure.
    /// None when the pressure file was absent or unreadable (unknown, not zero pressure).
    #[serde(default)]
    pub psi_cpu_avg10: Option<f64>,
    /// Memory PSI "some avg10" from cgroup memory.pressure. None = unread.
    #[serde(default)]
    pub psi_mem_avg10: Option<f64>,
    /// I/O PSI "some avg10" from cgroup io.pressure. None = unread.
    #[serde(default)]
    pub psi_io_avg10: Option<f64>,
    /// True when cgroup v2 accounting files were successfully read for this group.
    /// When false, cfs_throttle_pct should be shown as `?`.
    #[serde(default)]
    pub cg_v2_complete: bool,
    /// GPU engine time % (delta drm-engine-* ns / elapsed ns × 100).
    /// 0.0 when --enable-gpu is off or process has no DRM file descriptors.
    #[serde(default)]
    pub gpu_pct: f64,
    /// GPU VRAM in use, bytes (drm-memory-vram from fdinfo). 0 when --enable-gpu is off.
    #[serde(default)]
    pub gpu_vram_bytes: u64,
}

/// Rolling peak values across visible (non-fading) entries.
///
/// Used to anchor the log-scale bar lengths to the current busiest group.
/// For CPU (linear 0–100%) and memory (fraction of RAM) the bar length is
/// computed directly; all other metrics normalise against their rolling peak.
///
/// Each field decays by 5% per refresh tick (`old * 0.95`) and is then
/// replaced by the current maximum if that is larger. This keeps the bar
/// from snapping to zero the instant the busiest group exits; instead, the
/// scale shrinks gracefully.
#[derive(Default)]
pub struct PeakVals {
    pub disk_read_s: f64,
    pub disk_write_s: f64,
    pub page_faults_s: f64,
    pub ctx_switches_s: f64,
    pub threads: f64,
    pub open_fds: f64,
    pub swap_bytes: f64,
    pub power_w: f64,
    pub gpu_pct: f64,
    pub gpu_vram_bytes: f64,
}

impl PeakVals {
    /// Recompute rolling peaks from the current visible (non-fading) entries.
    ///
    /// `entries` is the full display list; fading rows are excluded so that a
    /// disappearing outlier cannot keep the scale artificially inflated.
    ///
    /// The decay formula `(old * 0.95).max(current_max)` means:
    ///   - If the new maximum is higher, it wins immediately.
    ///   - If the new maximum is lower, the scale shrinks at 5% per tick
    ///     rather than snapping down, giving visual stability.
    pub fn update(&mut self, entries: &[BarEntry]) {
        fn smooth(old: f64, cur: f64) -> f64 { (old * 0.95).max(cur) }
        // Predicate to exclude fading rows from peak computation.
        let nf = |e: &BarEntry| !e.fading;
        self.disk_read_s   = smooth(self.disk_read_s,   entries.iter().filter(|e| nf(e)).map(|e| e.disk_read_s).fold(0.0f64, f64::max));
        self.disk_write_s  = smooth(self.disk_write_s,  entries.iter().filter(|e| nf(e)).map(|e| e.disk_write_s).fold(0.0f64, f64::max));
        self.page_faults_s = smooth(self.page_faults_s, entries.iter().filter(|e| nf(e)).map(|e| e.page_faults_s).fold(0.0f64, f64::max));
        self.ctx_switches_s= smooth(self.ctx_switches_s,entries.iter().filter(|e| nf(e)).map(|e| e.ctx_switches_s).fold(0.0f64, f64::max));
        self.threads       = smooth(self.threads,       entries.iter().filter(|e| nf(e)).map(|e| e.count.unwrap_or(0) as f64).fold(0.0f64, f64::max));
        self.open_fds      = smooth(self.open_fds,      entries.iter().filter(|e| nf(e)).map(|e| e.open_fds as f64).fold(0.0f64, f64::max));
        self.swap_bytes    = smooth(self.swap_bytes,    entries.iter().filter(|e| nf(e)).map(|e| e.swap_bytes as f64).fold(0.0f64, f64::max));
        self.power_w       = smooth(self.power_w,       entries.iter().filter(|e| nf(e)).map(|e| e.power_w).fold(0.0f64, f64::max));
        self.gpu_pct        = smooth(self.gpu_pct,        entries.iter().filter(|e| nf(e)).map(|e| e.gpu_pct).fold(0.0f64, f64::max));
        self.gpu_vram_bytes = smooth(self.gpu_vram_bytes, entries.iter().filter(|e| nf(e)).map(|e| e.gpu_vram_bytes as f64).fold(0.0f64, f64::max));
    }
}

/// Numeric sort value of `e` for the given metric.
///
/// Returns a `f64` that is monotonically comparable for that metric so the
/// sort key can be unified across all metrics. For Memory, the local RAM
/// fraction (rss_bytes / total_ram) is preferred when total_ram is known;
/// otherwise the Proxmox `mem_pct` field is used.
pub fn metric_sort_val(e: &BarEntry, m: Metric, total_ram: u64) -> f64 {
    match m {
        Metric::Cpu => e.value,
        Metric::Memory => {
            if total_ram > 0 {
                // Local mode: express as percent of physical RAM for a consistent scale.
                e.rss_bytes as f64 / total_ram as f64 * 100.0
            } else {
                // Proxmox mode: PVE already provides mem/maxmem as a percent.
                e.mem_pct
            }
        }
        Metric::PageFaults => e.page_faults_s,
        Metric::Threads => e.count.unwrap_or(0) as f64,
        Metric::DiskRead => e.disk_read_s,
        Metric::DiskWrite => e.disk_write_s,
        Metric::CtxSwitches => e.ctx_switches_s,
        Metric::OpenFds => e.open_fds as f64,
        Metric::SwapMem => e.swap_bytes as f64,
        Metric::SchedWait => e.sched_wait_pct,
        Metric::Power => e.power_w,
        Metric::CfsThrottle => e.cfs_throttle_pct,
        Metric::PsiCpu => e.psi_cpu_avg10.unwrap_or(0.0),
        Metric::PsiMem => e.psi_mem_avg10.unwrap_or(0.0),
        Metric::PsiIo => e.psi_io_avg10.unwrap_or(0.0),
        Metric::GpuPct => e.gpu_pct,
        Metric::Vram => e.gpu_vram_bytes as f64,
    }
}

/// Packet sent from the Proxmox background thread to the main thread.
///
/// The Proxmox poller runs on a dedicated thread and pushes `PvePacket`s over
/// an mpsc channel. The main thread drains the channel on each UI tick and
/// takes the latest packet (discarding stale intermediate ones).
pub(crate) struct PvePacket {
    entries: Vec<BarEntry>,
    meta: HashMap<String, proxmox::VmMeta>,
    /// Per-group, per-metric member values for the fair-share overlay.
    /// All four Proxmox metrics are always pre-computed so the main thread can
    /// pick whichever is currently displayed without an extra round-trip.
    member_vals: HashMap<GroupLabel, HashMap<Metric, Vec<f64>>>,
    /// Per-node CPU/memory snapshot for the footer status line.
    node_status: Vec<proxmox::NodeStatus>,
    /// Per-storage fill snapshot for the footer status line.
    storage_status: Vec<proxmox::StorageStatus>,
    /// Non-None when the API call failed; the string is shown in the UI error bar.
    err: Option<String>,
}

/// Connection type for a fleet member.
pub enum FleetClient {
    Daemon(remote::RemoteClient),
    Thin(remote::ThinProbe),
}

/// One fleet member connection (daemon or thin probe).
pub struct FleetConn {
    /// Hostname exactly as supplied on the command line (validated).
    pub hostname: String,
    /// Live connection, or None if connection failed at startup.
    pub client: Option<FleetClient>,
    /// Last successful snapshot from this host.
    pub snap: Option<remote::DaemonSnapshot>,
    /// Error from the most recent connection attempt (shown in footer).
    pub err: Option<String>,
    /// Whether this connection uses thin probe (affects drill-down availability).
    pub thin: bool,
}

/// One Kubernetes pod connection in --kube mode.
///
/// Analogous to `FleetConn` but keyed by pod name. The `app_label` field groups
/// pods by their workload (Deployment, DaemonSet, etc.) for the fair-share histogram
/// overlay — so you can see whether a Deployment's replicas share the load evenly.
pub struct KubeConn {
    /// Pod name — used as the BarEntry label and as the kubectl exec target.
    pub pod_name: String,
    /// Workload/app label for histogram overlay grouping.
    /// From the pod's `app` or `app.kubernetes.io/name` label; derived from the
    /// pod name as a heuristic fallback when neither label is present.
    pub app_label: String,
    /// Active kubectl exec connection, or None if connection failed at startup.
    pub client: Option<FleetClient>,
    /// Most recent snapshot received from this pod.
    pub snap: Option<remote::DaemonSnapshot>,
    /// Error from the most recent connection attempt (shown in the footer).
    pub err: Option<String>,
    /// Whether this connection uses the thin /proc shell probe.
    pub thin: bool,
}

/// One Nomad allocation connection in --nomad mode.
///
/// Analogous to `KubeConn`. The display row label is `"{job_id}[{alloc_short}]"` so
/// the job name is always visible even when multiple replicas of the same job run.
/// The histogram overlay shows process distribution within the allocation (daemon mode),
/// using the entry label as the key — identical to the fleet-host pattern.
pub struct NomadConn {
    /// Full allocation UUID — the exec target for `nomad alloc exec`.
    pub alloc_id: String,
    /// First 8 hex chars of the UUID — used in the display label.
    pub alloc_short: String,
    /// Task name within the allocation to exec into.
    pub task_name: String,
    /// Job name — used as the grouping key and in the display label.
    pub job_id: String,
    /// Task group name — shown in the `extra` column.
    pub task_group: String,
    /// Active `nomad alloc exec` connection, or None if not yet connected / failed.
    pub client: Option<FleetClient>,
    /// Most recent snapshot received from this allocation.
    pub snap: Option<remote::DaemonSnapshot>,
    /// Error from the most recent connection attempt (shown in the footer).
    pub err: Option<String>,
    /// Whether this connection uses the thin /proc shell probe.
    pub thin: bool,
}

/// All mutable state owned by the main event loop.
pub struct AppState {
    pub mode: AppMode,
    /// How long to wait between data refreshes.
    pub interval: Duration,
    /// Maximum number of groups to display (0 = unlimited).
    pub top: usize,
    /// Current display rows, in stable order.
    pub entries: Vec<BarEntry>,
    /// Total unique groups seen so far (including fading ones, used in footer).
    pub total_groups: usize,
    /// Insertion-ordered group labels; new groups are appended, old ones removed
    /// after the 5-second retention window expires. This ensures rows do not
    /// jump around as activity changes.
    pub stable_order: Vec<String>,
    /// When each group was last seen alive (used to compute fade_t).
    pub last_seen: HashMap<String, Instant>,
    /// Most recent live `BarEntry` for each group; used to populate fading rows.
    pub last_values: HashMap<String, BarEntry>,
    /// Timestamp of the last successful refresh, used to schedule the next one.
    pub last_refresh: Option<Instant>,
    /// Number of snapshots collected so far (drives the "collecting…" message).
    pub snap_count: usize,
    /// Previous /proc snapshot for delta computation in local mode.
    pub snap: Option<local::Snapshot>,
    // Proxmox background thread
    pub(crate) proxmox_rx: Option<mpsc::Receiver<PvePacket>>,
    pub proxmox_url: Option<String>,
    pub proxmox_token: Option<String>,
    pub proxmox_insecure: bool,
    /// When the last successful Proxmox API response was received (for age display on error).
    pub proxmox_last_ok: Option<Instant>,
    /// Per-VM metadata indexed by VM label; populated from the Proxmox background thread.
    pub vm_meta: HashMap<String, proxmox::VmMeta>,
    /// Active SSH remote session, if any.
    pub remote_client: Option<remote::RemoteClient>,
    /// SSH username used when connecting to remote VMs.
    pub ssh_user: String,
    /// Whether remote drill-down via SSH is enabled.
    pub enable_remote: bool,
    /// Whether to use TOFU (accept-new) instead of strict host-key checking.
    pub ssh_accept_new: bool,
    /// Last error string to display in the header.
    pub error: Option<String>,
    /// Total physical RAM in bytes, read once at startup from /proc/meminfo.
    pub total_ram_bytes: u64,
    // ── meter bar metrics ─────────────────────────────────────────────────
    /// Metric shown on the left side (bar grows left→right).
    pub left_metric: Metric,
    /// Metric shown on the right side (bar grows right→left).
    pub right_metric: Metric,
    /// Which side ↑/↓ currently adjusts.
    pub active_side: Side,
    /// Which metric drives the sort order.
    pub sort_metric: Metric,
    // ── navigation ───────────────────────────────────────────────────────
    pub view: AppView,
    /// Scroll position in the manual page (row index).
    pub manual_scroll: usize,
    /// Height of the last-rendered body area in rows; used to bound histogram sampling.
    pub last_body_height: usize,
    // ── thread detail ────────────────────────────────────────────────────
    /// Previous per-thread snapshot for CPU delta computation in thread view.
    pub thread_snap: Option<local::ThreadSnapshot>,
    /// Sorted list of per-thread CPU/fault/etc samples for the current group.
    pub thread_samples: Vec<local::ThreadSample>,
    // ── group histogram overlay ───────────────────────────────────────────
    /// Per-group thread snapshots used for CPU+fault delta computation.
    pub group_snaps: HashMap<String, local::ThreadSnapshot>,
    /// Per-group member series for the fair-share overlay (updated ~once/sec).
    ///
    /// The key is a `GroupLabel`; the value is a `MemberSeries` produced by
    /// whichever level is active (currently always local threads via
    /// `local::sample_member_vals`). Future levels — Proxmox VM pools, fleet
    /// nodes — will write the same type here; the renderer (`fair_share_bins`)
    /// is already level-agnostic.
    pub group_member_vals: HashMap<GroupLabel, MemberSeries>,
    /// When the histogram overlay was last sampled (caps at 1 Hz).
    pub last_hist_sample: Option<Instant>,
    /// Whether the distribution-heat overlay is drawn ('h' to toggle).
    pub show_histogram: bool,
    // ── system-level metrics ──────────────────────────────────────────────
    /// System-wide network receive bytes/sec (summed across non-bridge interfaces).
    pub sys_net_rx_s: f64,
    /// System-wide network transmit bytes/sec.
    pub sys_net_tx_s: f64,
    /// GPU utilisation % from DRM sysfs, or None if no GPU was detected.
    pub sys_gpu_pct: Option<f64>,
    /// System-wide RAPL power draw in watts (0.0 if RAPL is unavailable).
    pub sys_rapl_w: f64,
    /// Previous system-level sample for delta computation.
    pub prev_sys: Option<local::SysSample>,
    // ── grouping strategy ─────────────────────────────────────────────────
    pub group_by: GroupBy,
    // ── Proxmox grouping strategy ─────────────────────────────────────────
    /// Current Proxmox grouping mode (Flat/Pool/Tag/Node).
    pub pve_group_by: PveGroupBy,
    /// Shared with the background thread; written by the 'g' key handler,
    /// read by the poller each iteration. `Relaxed` ordering is fine here
    /// because the next sample picks up whatever is current at that instant.
    pub pve_group_by_shared: Arc<AtomicU8>,
    /// Per-node CPU/memory from the most recent Proxmox sample (footer display).
    pub pve_node_status: Vec<proxmox::NodeStatus>,
    /// Per-storage fill from the most recent Proxmox sample (footer display).
    pub pve_storage_status: Vec<proxmox::StorageStatus>,
    // ── rolling peak values ───────────────────────────────────────────────
    pub peak_vals: PeakVals,
    // ── privilege flag ────────────────────────────────────────────────────
    /// Set to true once we observe any entry with an incomplete metric (EACCES).
    /// Drives the "running unprivileged" notice in the footer.
    pub running_unprivileged: bool,
    // ── system-wide PSI (local mode) ──────────────────────────────────────
    /// System-level CPU PSI "some avg10" from /proc/pressure/cpu.
    pub sys_psi_cpu: Option<f64>,
    /// System-level memory PSI "some avg10" from /proc/pressure/memory.
    pub sys_psi_mem: Option<f64>,
    /// System-level I/O PSI "some avg10" from /proc/pressure/io.
    pub sys_psi_io: Option<f64>,
    /// Fleet connections (AppMode::Fleet only). One entry per host in --hosts.
    pub fleet_clients: Vec<FleetConn>,
    /// Kubernetes pod connections (AppMode::Kube only). One entry per discovered pod.
    pub kube_conns: Vec<KubeConn>,
    /// Nomad allocation connections (AppMode::Nomad only). One entry per running allocation.
    pub nomad_conns: Vec<NomadConn>,
    /// Ring buffer of recent snapshots for replay. Oldest at front, newest at back.
    pub history: std::collections::VecDeque<HistoryEntry>,
    /// Maximum history entries kept (from --history-depth, default 120).
    pub history_depth: usize,
    /// When Some(i), display is paused showing history[i]. None = live.
    pub history_cursor: Option<usize>,
    /// Per-group anomaly tracking state.
    pub anomaly_states: HashMap<GroupLabel, AnomalyState>,
    /// Alert command string (from --alert-cmd), if provided.
    pub alert_cmd: Option<String>,
    /// True when --enable-gpu was passed; gates fdinfo reads and GPU metric display.
    pub gpu_enabled: bool,
    /// GPU devices discovered on first refresh (empty until then).
    pub gpu_devices: Vec<GpuDevice>,
    /// Index into gpu_devices for the selected device. 0 = aggregate all.
    /// 1..=N selects gpu_devices[selected_gpu-1].
    pub selected_gpu: usize,
    /// Carousel node for the group body list; persists scroll position and
    /// is reconciled with entries on every data tick.
    pub body_tree: Option<Tree>,
    /// Active colour palette; swap to re-theme the whole UI atomically.
    pub theme: Theme,
}

/// Parse the --hosts argument into a validated list of hostnames.
///
/// Accepts a comma-separated list ("h1,h2,h3") or a file reference ("@/path/to/file").
/// File format: one host per line; lines starting with '#' or empty lines are skipped.
/// Each hostname is validated: must not start with '-' (SSH option injection guard).
fn parse_hosts_arg(arg: &str) -> Result<Vec<String>> {
    let raw: Vec<String> = if let Some(path) = arg.strip_prefix('@') {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read hosts file: {path}"))?;
        content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect()
    } else {
        arg.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    if raw.is_empty() {
        anyhow::bail!("--hosts: no hosts found");
    }
    for host in &raw {
        remote::validate_ssh_target("dummy", host).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    Ok(raw)
}

/// Parse the --kube argument into a (namespace, optional_selector) pair.
///
/// Format: "NAMESPACE" or "NAMESPACE/SELECTOR" where SELECTOR is a kubectl
/// label selector (e.g. "app=nginx", "tier=frontend,env=prod").
/// Both parts are validated for option-injection safety before returning.
fn parse_kube_arg(arg: &str) -> Result<(String, Option<String>)> {
    let (ns, sel) = match arg.split_once('/') {
        Some((ns, sel)) => (ns.trim().to_string(), Some(sel.trim().to_string())),
        None => (arg.trim().to_string(), None),
    };
    remote::validate_kube_target("namespace", &ns)?;
    if let Some(ref s) = sel {
        remote::validate_kube_target("selector", s)?;
    }
    Ok((ns, sel))
}

/// Derive a workload label from a pod name by stripping ReplicaSet/Pod hash suffixes.
///
/// Kubernetes appends hash segments to pods managed by Deployments and DaemonSets.
/// We strip trailing segments that are ≥ 5 characters and entirely alphanumeric
/// (the heuristic for a k8s-generated hash), up to a maximum of two segments.
///
/// Examples:
///   "nginx-deployment-7d6fb9f9-xl5gr" → "nginx-deployment"  (2 hash segments)
///   "fluentd-ds-abc12"                → "fluentd-ds"         (1 hash segment)
///   "my-pod"                          → "my-pod"             (no hash suffix)
///
/// Falls back to the full pod name when no hash-like suffix is detected.
/// This is a heuristic; the `app` label (when present) is always preferred.
fn derive_app_label(pod_name: &str) -> String {
    let parts: Vec<&str> = pod_name.split('-').collect();
    let is_hash = |s: &&str| s.len() >= 5 && s.chars().all(|c| c.is_ascii_alphanumeric());
    let mut end = parts.len();
    for _ in 0..2 {
        if end > 1 && is_hash(&parts[end - 1]) { end -= 1; } else { break; }
    }
    if end == 0 { return pod_name.to_string(); }
    parts[..end].join("-")
}

/// Discover pods in a Kubernetes namespace via `kubectl get pods -o json`.
///
/// Returns a Vec of `(pod_name, app_label)` pairs. The `app_label` is used to
/// group pods of the same Deployment in the fair-share histogram overlay.
///
/// Label resolution order:
/// 1. `app` label (most common for Deployments)
/// 2. `app.kubernetes.io/name` label (CNCF recommended standard)
/// 3. Derived from pod name by stripping trailing hash segments (heuristic)
///
/// Uses `serde_json` (already a dependency) to parse the kubectl JSON output.
fn discover_pods(
    namespace: &str,
    selector: Option<&str>,
    context: Option<&str>,
) -> Result<Vec<(String, String)>> {
    let mut cmd = std::process::Command::new("kubectl");
    cmd.arg("get").arg("pods");
    cmd.arg("-n").arg(namespace);
    if let Some(sel) = selector { cmd.arg("-l").arg(sel); }
    if let Some(ctx) = context { cmd.arg("--context").arg(ctx); }
    cmd.arg("-o").arg("json");

    let output = cmd.output()
        .context("kubectl get pods failed — is kubectl in PATH?")?;
    if !output.status.success() {
        anyhow::bail!(
            "kubectl get pods: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let v: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("kubectl get pods: invalid JSON response")?;
    let items = v["items"].as_array()
        .ok_or_else(|| anyhow::anyhow!("kubectl get pods: response missing 'items' field"))?;

    let pods = items.iter().filter_map(|item| {
        let name = item["metadata"]["name"].as_str()?.to_string();
        if name.is_empty() { return None; }
        // Prefer explicit `app` label, then the CNCF-standard name label, then heuristic.
        let labels = &item["metadata"]["labels"];
        let app_label = labels["app"].as_str()
            .or_else(|| labels["app.kubernetes.io/name"].as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| derive_app_label(&name));
        Some((name, app_label))
    }).collect();
    Ok(pods)
}

/// Discover running Nomad allocations via the Nomad HTTP API.
///
/// Calls `GET {addr}/v1/allocations?namespace={ns}` and filters to entries with
/// `ClientStatus == "running"` and `DesiredStatus == "run"`.  Returns a
/// `Vec<(alloc_id, task_name, job_id, task_group)>` sorted by job name so the
/// display order is stable across refreshes.
///
/// The task name is resolved from `TaskStates` (first running task, alphabetically).
/// Falls back to the task group name when `TaskStates` is absent or empty — this
/// covers the common case of single-task task groups where task name == group name.
fn discover_nomad_allocs(
    addr: &str,
    namespace: &str,
    token: Option<&str>,
    job_filter: Option<&str>,
) -> anyhow::Result<Vec<(String, String, String, String)>> {
    use reqwest::blocking::Client;
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("failed to build HTTP client")?;

    let url = format!("{addr}/v1/allocations?namespace={namespace}");
    let mut req = client.get(&url);
    if let Some(tok) = token {
        req = req.header("X-Nomad-Token", tok);
    }

    let resp = req.send().context("Nomad API request failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("Nomad API {url}: HTTP {}", resp.status());
    }

    let body: serde_json::Value = resp.json().context("Nomad API: invalid JSON response")?;
    let items = body.as_array()
        .ok_or_else(|| anyhow::anyhow!("Nomad API: expected a JSON array of allocations"))?;

    let mut result: Vec<(String, String, String, String)> = Vec::new();
    for item in items {
        let client_status = item["ClientStatus"].as_str().unwrap_or("");
        let desired_status = item["DesiredStatus"].as_str().unwrap_or("");
        if client_status != "running" || desired_status != "run" {
            continue;
        }

        let alloc_id = match item["ID"].as_str() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let job_id = match item["JobID"].as_str() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let task_group = item["TaskGroup"].as_str().unwrap_or("").to_string();

        if let Some(filter) = job_filter {
            if job_id != filter {
                continue;
            }
        }

        // Pick the first running task alphabetically from TaskStates.
        let task_name = item["TaskStates"].as_object()
            .and_then(|ts| {
                let mut running: Vec<&str> = ts.iter()
                    .filter(|(_, v)| v["State"].as_str() == Some("running"))
                    .map(|(k, _)| k.as_str())
                    .collect();
                running.sort_unstable();
                running.into_iter().next().map(|s| s.to_string())
            })
            .unwrap_or_else(|| task_group.clone());

        // Reject names that would cause option injection.
        if remote::validate_nomad_target("alloc", &alloc_id).is_err() { continue; }
        if remote::validate_nomad_target("task", &task_name).is_err() { continue; }

        result.push((alloc_id, task_name, job_id, task_group));
    }

    // Stable sort by job_id so same-job allocations appear together.
    result.sort_by(|a, b| a.2.cmp(&b.2));
    Ok(result)
}

impl AppState {
    /// Construct a new `AppState` from parsed CLI arguments.
    ///
    /// In Proxmox mode this also spawns the background polling thread that pushes
    /// `PvePacket`s over an mpsc channel. The thread runs until the channel is
    /// dropped (i.e., until the main thread exits).
    ///
    /// Returns an error if Proxmox mode is requested but `--token` is missing.
    fn new(cli: &Cli) -> Result<Self> {
        let mode = if let Some(nomad_addr) = &cli.nomad {
            AppMode::Nomad {
                addr: nomad_addr.clone(),
                namespace: cli.nomad_namespace.clone(),
                token: cli.nomad_token.clone(),
                job_filter: cli.nomad_job.clone(),
                thin: cli.nomad_thin,
            }
        } else if let Some(kube_arg) = &cli.kube {
            let (namespace, selector) = parse_kube_arg(kube_arg)?;
            AppMode::Kube {
                namespace,
                selector,
                context: cli.kube_context.clone(),
                thin: cli.kube_thin,
            }
        } else if let Some(hosts_arg) = &cli.hosts {
            if !cli.enable_remote {
                anyhow::bail!("--hosts requires --enable-remote");
            }
            let hosts = parse_hosts_arg(hosts_arg)?;
            let ssh_user = cli.ssh_user.clone().unwrap_or_else(|| {
                std::env::var("USER").unwrap_or_else(|_| "root".to_string())
            });
            AppMode::Fleet { hosts, ssh_user, thin: cli.thin }
        } else if let Some(url) = &cli.proxmox {
            let token = cli.token.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "--token (or PROXMOX_TOKEN env var) is required with --proxmox"
                )
            })?;
            AppMode::Proxmox { url: url.clone(), token, insecure: cli.insecure }
        } else {
            AppMode::Local
        };

        // Shared AtomicU8 so the 'g' key can change grouping without restarting
        // the poller thread. Initialised to Flat (0). Created unconditionally so
        // it is always valid regardless of mode.
        let pve_group_by_shared = Arc::new(AtomicU8::new(PveGroupBy::Flat as u8));

        let (proxmox_rx, proxmox_url, proxmox_token, proxmox_insecure) =
            if let AppMode::Proxmox { ref url, ref token, insecure } = mode {
                if insecure {
                    eprintln!("WARNING: TLS certificate verification disabled (--insecure)");
                }
                let url_c = url.clone();
                let token_c = token.clone();
                let (tx, rx) = mpsc::channel::<PvePacket>();
                let interval = Duration::from_secs_f64(cli.interval);
                let url2 = url.clone();
                let token2 = token.clone();
                let shared = Arc::clone(&pve_group_by_shared);
                // Spawn the Proxmox poller thread. It loops forever, sleeping `interval`
                // between calls. The main thread drains `rx` non-blocking on each UI tick.
                std::thread::spawn(move || {
                    let mut client = match proxmox::Client::new(&url2, &token2, insecure) {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx.send(PvePacket {
                                entries: vec![],
                                meta: HashMap::new(),
                                member_vals: HashMap::new(),
                                node_status: vec![],
                                storage_status: vec![],
                                err: Some(e.to_string()),
                            });
                            return;
                        }
                    };
                    loop {
                        // Read the current grouping strategy each iteration so 'g'
                        // key changes are picked up without restarting the thread.
                        let group_by = PveGroupBy::from_u8(shared.load(Ordering::Relaxed));
                        let packet = match client.sample(group_by) {
                            Ok(r) => PvePacket {
                                entries: r.entries,
                                meta: r.meta,
                                member_vals: r.member_vals,
                                node_status: r.node_status,
                                storage_status: r.storage_status,
                                err: None,
                            },
                            Err(e) => PvePacket {
                                entries: vec![],
                                meta: HashMap::new(),
                                member_vals: HashMap::new(),
                                node_status: vec![],
                                storage_status: vec![],
                                err: Some(e.to_string()),
                            },
                        };
                        if tx.send(packet).is_err() {
                            break;
                        }
                        std::thread::sleep(interval);
                    }
                });
                (Some(rx), Some(url_c), Some(token_c), insecure)
            } else {
                (None, None, None, false)
            };

        let is_local = matches!(mode, AppMode::Local);
        // Fetch total RAM once; it rarely changes and avoids a /proc/meminfo read on every tick.
        // Both Proxmox and Fleet modes use 0 (memory uses mem_pct / sys_mem_used_bytes instead).
        let total_ram_bytes = if is_local { local::total_ram_bytes() } else { 0 };
        // Default SSH user to the current OS user; "root" as last resort for headless systems.
        let ssh_user = cli.ssh_user.clone().unwrap_or_else(|| {
            std::env::var("USER").unwrap_or_else(|_| "root".to_string())
        });

        // Build fleet connections (Fleet mode only): one connection per host, spawned in parallel.
        let fleet_clients: Vec<FleetConn> = if let AppMode::Fleet { ref hosts, ref ssh_user, thin } = mode {
            let accept_new = cli.ssh_accept_new;
            let (tx, rx) = std::sync::mpsc::channel::<(String, Result<FleetClient>)>();
            for host in hosts {
                let tx = tx.clone();
                let user = ssh_user.clone();
                let host = host.clone();
                let is_thin = thin;
                std::thread::spawn(move || {
                    let pol = if accept_new {
                        remote::SshHostKeyPolicy::AcceptNew
                    } else {
                        remote::SshHostKeyPolicy::Strict
                    };
                    let result: Result<FleetClient> = if is_thin {
                        remote::connect_thin(&user, &host, pol).map(FleetClient::Thin)
                    } else {
                        remote::connect_direct(&host, &user, pol).map(FleetClient::Daemon)
                    };
                    let _ = tx.send((host, result));
                });
            }
            drop(tx); // close sender so the receiver loop terminates when all threads finish
            rx.into_iter().map(|(hostname, result)| {
                let thin_flag = thin;
                match result {
                    Ok(client) => FleetConn {
                        hostname,
                        client: Some(client),
                        snap: None,
                        err: None,
                        thin: thin_flag,
                    },
                    Err(e) => FleetConn {
                        hostname,
                        client: None,
                        snap: None,
                        err: Some(e.to_string()),
                        thin: thin_flag,
                    },
                }
            }).collect()
        } else {
            vec![]
        };

        let kube_conns: Vec<KubeConn> = if let AppMode::Kube { ref namespace, ref selector, ref context, thin } = mode {
            // Discover pods synchronously at startup. On failure (no kubectl, RBAC denied),
            // log to stderr and start with an empty pod list so the UI can show the error.
            let pods = discover_pods(namespace, selector.as_deref(), context.as_deref())
                .unwrap_or_else(|e| {
                    eprintln!("aerie: kubectl discover: {e}");
                    vec![]
                });
            let ns = namespace.clone();
            let ctx = context.clone();
            let (tx, rx) = std::sync::mpsc::channel::<(String, String, Result<FleetClient>)>();
            for (pod_name, app_label) in pods {
                let tx = tx.clone();
                let ns2 = ns.clone();
                let ctx2 = ctx.clone();
                let pn = pod_name.clone();
                let al = app_label.clone();
                let is_thin = thin;
                std::thread::spawn(move || {
                    let result: Result<FleetClient> = if is_thin {
                        remote::connect_kube_thin(&pn, &ns2, ctx2.as_deref()).map(FleetClient::Thin)
                    } else {
                        remote::connect_kube_daemon(&pn, &ns2, ctx2.as_deref()).map(FleetClient::Daemon)
                    };
                    let _ = tx.send((pn, al, result));
                });
            }
            drop(tx);
            rx.into_iter().map(|(pod_name, app_label, result)| {
                match result {
                    Ok(client) => KubeConn {
                        pod_name, app_label, client: Some(client),
                        snap: None, err: None, thin,
                    },
                    Err(e) => KubeConn {
                        pod_name, app_label, client: None,
                        snap: None, err: Some(e.to_string()), thin,
                    },
                }
            }).collect()
        } else {
            vec![]
        };

        let nomad_conns: Vec<NomadConn> = if let AppMode::Nomad { ref addr, ref namespace, ref token, ref job_filter, thin } = mode {
            let allocs = discover_nomad_allocs(addr, namespace, token.as_deref(), job_filter.as_deref())
                .unwrap_or_else(|e| {
                    eprintln!("aerie: nomad discover: {e}");
                    vec![]
                });
            let addr_c = addr.clone();
            let token_c = token.clone();
            let (tx, rx) = std::sync::mpsc::channel::<(String, String, String, String, Result<FleetClient>)>();
            for (alloc_id, task_name, job_id, task_group) in allocs {
                let tx = tx.clone();
                let addr2 = addr_c.clone();
                let tok2 = token_c.clone();
                let aid = alloc_id.clone();
                let tn = task_name.clone();
                let is_thin = thin;
                std::thread::spawn(move || {
                    let result: Result<FleetClient> = if is_thin {
                        remote::connect_nomad_thin(&aid, &tn, &addr2, tok2.as_deref()).map(FleetClient::Thin)
                    } else {
                        remote::connect_nomad_daemon(&aid, &tn, &addr2, tok2.as_deref()).map(FleetClient::Daemon)
                    };
                    let _ = tx.send((alloc_id, task_name, job_id, task_group, result));
                });
            }
            drop(tx);
            rx.into_iter().map(|(alloc_id, task_name, job_id, task_group, result)| {
                let alloc_short = alloc_id.chars().take(8).collect::<String>();
                match result {
                    Ok(client) => NomadConn {
                        alloc_id, alloc_short, task_name, job_id, task_group,
                        client: Some(client), snap: None, err: None, thin,
                    },
                    Err(e) => NomadConn {
                        alloc_id, alloc_short, task_name, job_id, task_group,
                        client: None, snap: None, err: Some(e.to_string()), thin,
                    },
                }
            }).collect()
        } else {
            vec![]
        };

        Ok(Self {
            mode,
            proxmox_rx,
            proxmox_url,
            proxmox_token,
            proxmox_insecure,
            proxmox_last_ok: None,
            vm_meta: HashMap::new(),
            remote_client: None,
            ssh_user,
            enable_remote: cli.enable_remote,
            ssh_accept_new: cli.ssh_accept_new,
            interval: Duration::from_secs_f64(cli.interval),
            top: cli.top,
            entries: vec![],
            total_groups: 0,
            stable_order: vec![],
            last_seen: HashMap::new(),
            last_values: HashMap::new(),
            last_refresh: None,
            snap_count: 0,
            snap: None,
            error: None,
            total_ram_bytes,
            left_metric: Metric::Cpu,
            right_metric: Metric::Memory,
            active_side: Side::Left,
            sort_metric: Metric::Cpu,
            view: AppView::Groups,
            manual_scroll: 0,
            thread_snap: None,
            thread_samples: vec![],
            last_body_height: 30,
            group_snaps: HashMap::new(),
            group_member_vals: HashMap::new(),
            last_hist_sample: None,
            show_histogram: true,
            sys_net_rx_s: 0.0,
            sys_net_tx_s: 0.0,
            sys_gpu_pct: None,
            sys_rapl_w: 0.0,
            prev_sys: None,
            group_by: GroupBy::Comm,
            pve_group_by: PveGroupBy::Flat,
            pve_group_by_shared,
            pve_node_status: vec![],
            pve_storage_status: vec![],
            peak_vals: PeakVals::default(),
            running_unprivileged: false,
            sys_psi_cpu: None,
            sys_psi_mem: None,
            sys_psi_io: None,
            fleet_clients,
            kube_conns,
            nomad_conns,
            history: std::collections::VecDeque::new(),
            history_depth: cli.history_depth.max(10),
            history_cursor: None,
            anomaly_states: HashMap::new(),
            alert_cmd: cli.alert_cmd.clone(),
            gpu_enabled: cli.enable_gpu,
            gpu_devices: Vec::new(),
            selected_gpu: 0,
            theme: Theme::default(),
            body_tree: Some(Tree::new(Node::Carousel {
                id: ui::BODY_ID,
                orientation: Orientation::Vertical,
                scroll: 0,
                children: vec![],
            })),
        })
    }

    /// Index of the carousel's focused entry within `self.entries`, or `None`
    /// if the tree has no focus or the focused tile is not in `entries`.
    pub fn focused_entry_idx(&self) -> Option<usize> {
        let fid = self.body_tree.as_ref()?.focus()?;
        self.entries.iter().position(|e| id_from_key(&e.label) == fid)
    }

    /// Reconcile the body carousel with the current entry list.
    ///
    /// Must be called whenever `self.entries` changes — both from `refresh()` and
    /// from the remote-snapshot polling path (which updates entries outside refresh).
    pub fn sync_body_tree(&mut self) {
        let desired: Vec<(TileId, u16)> = self.entries.iter()
            .map(|e| (id_from_key(&e.label), 1u16))
            .collect();
        if let Some(tree) = &mut self.body_tree {
            reconcile_carousel(tree.root_mut(), &desired);
            tree.ensure_focus_valid();
            tree.ensure_zoom_valid();
        }
    }

    /// Sample per-member values for all groups currently visible on screen and
    /// store them as `MemberSeries` in `group_member_vals`.
    ///
    /// This is the *local-threads* implementation of member-series production:
    /// each group's members are its threads, sampled via `/proc/PID/task/*/stat`.
    /// Future levels (Proxmox pools, fleet nodes) will populate `group_member_vals`
    /// through their own paths but store the same `MemberSeries` type, which the
    /// renderer (`fair_share_bins`) consumes without knowing the source.
    ///
    /// Called at most once per second (the caller gates on `last_hist_sample`),
    /// independent of the main refresh interval, because reading per-thread /proc
    /// files for every visible group is moderately expensive.
    ///
    /// Only groups within the visible scroll window are sampled; groups that scroll
    /// off screen have their data evicted from `group_member_vals` and `group_snaps`
    /// to bound memory use.
    ///
    /// The `fields` mask is computed from the currently displayed metric (the active
    /// side's metric), so we only open the files we actually need.
    fn sample_histograms(&mut self) {
        if self.snap.is_none() {
            return;
        }
        let cpu_total = match local::cpu_total() {
            Ok(t) => t,
            Err(_) => return,
        };

        // Determine the overlay metric (focused side) and which files each thread needs.
        let hist_metric = match self.active_side {
            Side::Left => self.left_metric,
            Side::Right => self.right_metric,
        };
        let fields = match hist_metric {
            // CPU and page-faults only need /proc/PID/task/TID/stat (always read).
            Metric::Cpu | Metric::PageFaults =>
                local::ThreadFields { io: false, status: false, schedstat: false },
            // Disk metrics additionally need /proc/PID/task/TID/io.
            Metric::DiskRead | Metric::DiskWrite =>
                local::ThreadFields { io: true, status: false, schedstat: false },
            // Context switches need /proc/PID/task/TID/status.
            Metric::CtxSwitches =>
                local::ThreadFields { io: false, status: true, schedstat: false },
            // Scheduler wait needs /proc/PID/task/TID/schedstat.
            Metric::SchedWait =>
                local::ThreadFields { io: false, status: false, schedstat: true },
            // Memory, Threads, OpenFds, SwapMem, Power are not per-thread attributable.
            _ => return,
        };

        // Only sample the visible window according to the carousel's current scroll.
        let visible_range = if let Some(tree) = &self.body_tree {
            let h = self.last_body_height.saturating_sub(1) as u16;
            carousel_visible_range(tree.root(), Rect::new(0, 0, 1, h))
        } else {
            0..self.entries.len()
        };

        // Collect (label, pids) pairs for visible, non-fading groups.
        let group_pids: Vec<(String, Vec<u32>)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(i, e)| !e.fading && visible_range.contains(i))
            .filter_map(|(_, e)| {
                let pids = self
                    .snap
                    .as_ref()
                    .and_then(|s| s.groups.get(&e.label))
                    .map(|g| g.pids.clone())?;
                Some((e.label.clone(), pids))
            })
            .collect();

        let visible: std::collections::HashSet<String> =
            group_pids.iter().map(|(l, _)| l.clone()).collect();

        for (label, pids) in group_pids {
            // Take ownership of the previous snapshot (delta basis) for this group.
            let prev = self.group_snaps.remove(&label);
            if let Ok((vals, snap)) =
                local::sample_member_vals(&pids, prev, hist_metric, &fields, cpu_total)
            {
                self.group_snaps.insert(label.clone(), snap);
                if !vals.vals.is_empty() {
                    self.group_member_vals.insert(label, vals);
                }
            }
        }

        // Evict data for groups outside the visible window to bound memory use.
        self.group_member_vals.retain(|k, _| visible.contains(k));
        self.group_snaps.retain(|k, _| visible.contains(k));
    }

    /// Build the `CollectOpts` mask that tells `local::sample` which optional
    /// /proc files to read on this tick.
    ///
    /// We only open expensive files (io, status, fd dir, schedstat, statm) when
    /// at least one of the three active metrics (left, right, sort) actually uses
    /// that file. This avoids reading several hundred /proc entries per PID when
    /// the user is only looking at CPU and memory.
    fn collect_opts(&self) -> local::CollectOpts {
        let metrics = [self.left_metric, self.right_metric, self.sort_metric];
        let selected_gpu_pci = if self.selected_gpu == 0 {
            None
        } else {
            self.gpu_devices.get(self.selected_gpu - 1).map(|d| d.pci_addr.clone())
        };
        local::CollectOpts {
            need_io: metrics.iter().any(|m| matches!(m, Metric::DiskRead | Metric::DiskWrite)),
            need_status: metrics.iter().any(|m| matches!(m, Metric::CtxSwitches | Metric::SwapMem)),
            need_fds: metrics.iter().any(|m| matches!(m, Metric::OpenFds)),
            need_schedstat: metrics.iter().any(|m| matches!(m, Metric::SchedWait)),
            need_rss: metrics.iter().any(|m| matches!(m, Metric::Memory)),
            need_gpu: self.gpu_enabled && metrics.iter().any(|m| matches!(m, Metric::GpuPct | Metric::Vram)),
            selected_gpu_pci,
        }
    }

    /// Core data-acquisition method: fetch the latest metrics and update `self.entries`.
    ///
    /// In local mode, calls `local::sample` which reads /proc.
    /// In Proxmox mode, drains the background thread's channel non-blocking; if no
    /// new packet is available (the poller hasn't run yet), the call returns early
    /// without touching `self.entries`.
    ///
    /// After updating entries this method:
    /// - Distributes RAPL power proportionally across groups by CPU share.
    /// - Merges new groups into `stable_order` (new entries sorted by `sort_metric`,
    ///   then appended so existing rows keep their position).
    /// - Fills fading rows for groups that have disappeared within the 5 s window.
    /// - Updates `peak_vals` for log-scale bar anchoring.
    /// - Sets `running_unprivileged` if any entry has an incomplete metric.
    /// - Samples per-thread data when the thread-detail view is open.
    fn refresh(&mut self) {
        self.last_refresh = Some(Instant::now());
        self.snap_count += 1;
        if self.history_cursor.is_some() {
            return; // paused: display frozen on selected history entry
        }

        let is_local = matches!(self.mode, AppMode::Local);
        let result: Result<Vec<BarEntry>> = if is_local {
            // Discover GPU devices once
            if self.gpu_enabled && self.gpu_devices.is_empty() {
                self.gpu_devices = local::discover_gpu_devices()
                    .into_iter()
                    .map(|d| GpuDevice { pci_addr: d.pci_addr, driver: d.driver })
                    .collect();
            }

            // Sample NVIDIA pmon each tick if any NVIDIA device present
            let nvidia_data: HashMap<u32, (f64, u64)> = if self.gpu_enabled
                && self.gpu_devices.iter().any(|d| d.driver == "nvidia")
            {
                local::sample_nvidia_pmon()
            } else {
                HashMap::new()
            };

            let opts = self.collect_opts();
            let group_by = self.group_by;
            local::sample(self.snap.take(), &opts, group_by).map(|(mut entries, snap)| {
                // Merge NVIDIA pmon data into entries
                if !nvidia_data.is_empty() {
                    let is_nvidia_selected = self.selected_gpu > 0
                        && self.gpu_devices.get(self.selected_gpu - 1)
                            .is_some_and(|d| d.driver == "nvidia");
                    let show_nvidia = self.selected_gpu == 0 || is_nvidia_selected;

                    if show_nvidia {
                        for entry in &mut entries {
                            if let Some(group) = snap.groups.get(&entry.label) {
                                let mut sm_sum = 0f64;
                                let mut vram_sum = 0u64;
                                for &pid in &group.pids {
                                    if let Some(&(sm, vram)) = nvidia_data.get(&pid) {
                                        sm_sum += sm;
                                        vram_sum += vram;
                                    }
                                }
                                if sm_sum > 0.0 || vram_sum > 0 {
                                    if is_nvidia_selected {
                                        // replace with nvidia-only data
                                        entry.gpu_pct = sm_sum;
                                        entry.gpu_vram_bytes = vram_sum * 1024 * 1024;
                                    } else {
                                        // aggregate mode: add nvidia on top of DRM
                                        entry.gpu_pct += sm_sum;
                                        entry.gpu_vram_bytes = entry.gpu_vram_bytes
                                            .saturating_add(vram_sum * 1024 * 1024);
                                    }
                                }
                            }
                        }
                    }
                }
                self.snap = Some(snap);
                entries
            })
        } else if matches!(self.mode, AppMode::Fleet { .. }) {
            // Poll each fleet member for new snapshots.
            let hist_metric = match self.active_side {
                Side::Left => self.left_metric,
                Side::Right => self.right_metric,
            };
            for conn in &mut self.fleet_clients {
                let new_snap = match &mut conn.client {
                    Some(FleetClient::Daemon(c)) => c.try_recv(),
                    Some(FleetClient::Thin(t))   => t.try_recv(),
                    None => None,
                };
                if let Some(snap) = new_snap {
                    conn.snap = Some(snap);
                    conn.err = None;
                }
                // Mark error if SSH process died.
                let alive = match &mut conn.client {
                    Some(FleetClient::Daemon(c)) => c.is_alive(),
                    Some(FleetClient::Thin(t))   => t.is_alive(),
                    None => false,
                };
                if !alive && conn.client.is_some() {
                    conn.err = Some("connection lost".into());
                }
            }
            // Build BarEntries: one per fleet member.
            let raw: Vec<BarEntry> = self.fleet_clients.iter().map(|conn| {
                let (cpu, mem_pct, mem_used, net_rx, net_tx, count, has_data) =
                    if let Some(ref snap) = conn.snap {
                        let cpu = snap.sys_cpu_pct.unwrap_or_else(|| {
                            // Fallback: sum active process CPU% (daemon without sys_cpu_pct field)
                            snap.entries.iter().map(|e| e.value).sum::<f64>().min(100.0)
                        });
                        let mem_pct = if snap.total_ram_bytes > 0 {
                            snap.sys_mem_used_bytes as f64 / snap.total_ram_bytes as f64 * 100.0
                        } else {
                            0.0
                        };
                        (cpu, mem_pct, snap.sys_mem_used_bytes, snap.sys_net_rx_s,
                         snap.sys_net_tx_s, snap.entries.len(), true)
                    } else {
                        (0.0, 0.0, 0, 0.0, 0.0, 0, false)
                    };
                let extra = if conn.thin {
                    "(thin)".into()
                } else if !has_data {
                    "connecting…".into()
                } else {
                    format!("{count} procs")
                };
                BarEntry {
                    label: conn.hostname.clone(),
                    value: cpu,
                    count: Some(count),
                    extra,
                    fading: false,
                    fade_t: 0.0,
                    rss_bytes: mem_used,
                    mem_pct,
                    page_faults_s: 0.0,
                    disk_read_s: net_rx,   // repurposed: host network rx
                    disk_write_s: net_tx,  // repurposed: host network tx
                    ctx_switches_s: 0.0,
                    open_fds: 0,
                    swap_bytes: 0,
                    sched_wait_pct: 0.0,
                    power_w: 0.0,
                    disk_complete: true,
                    status_complete: true,
                    fds_complete: true,
                    sched_complete: true,
                    rss_complete: true,
                    cfs_throttle_pct: 0.0,
                    psi_cpu_avg10: None,
                    psi_mem_avg10: None,
                    psi_io_avg10: None,
                    cg_v2_complete: false,
                    gpu_pct: 0.0,
                    gpu_vram_bytes: 0,
                }
            }).collect();
            // Update MemberSeries: per-host process CPU distribution for the histogram overlay.
            self.group_member_vals.clear();
            for conn in &self.fleet_clients {
                if let Some(ref snap) = conn.snap {
                    let vals: Vec<f64> = snap.entries.iter().map(|e| e.value).collect();
                    if !vals.is_empty() {
                        self.group_member_vals.insert(
                            conn.hostname.clone(),
                            MemberSeries { metric: hist_metric, vals },
                        );
                    }
                }
            }
            Ok(raw)
        } else if matches!(self.mode, AppMode::Kube { .. }) {
            // Poll each pod connection for new snapshots.
            let hist_metric = match self.active_side {
                Side::Left => self.left_metric,
                Side::Right => self.right_metric,
            };
            for conn in &mut self.kube_conns {
                let new_snap = match &mut conn.client {
                    Some(FleetClient::Daemon(c)) => c.try_recv(),
                    Some(FleetClient::Thin(t))   => t.try_recv(),
                    None => None,
                };
                if let Some(snap) = new_snap {
                    conn.snap = Some(snap);
                    conn.err = None;
                }
                // Mark connection lost if the kubectl process exited unexpectedly.
                let alive = match &mut conn.client {
                    Some(FleetClient::Daemon(c)) => c.is_alive(),
                    Some(FleetClient::Thin(t))   => t.is_alive(),
                    None => false,
                };
                if !alive && conn.client.is_some() {
                    conn.err = Some("connection lost".into());
                }
            }
            // Build BarEntries: one row per pod.
            let raw: Vec<BarEntry> = self.kube_conns.iter().map(|conn| {
                let (cpu, mem_pct, mem_used, has_data) = if let Some(ref snap) = conn.snap {
                    let cpu = snap.sys_cpu_pct.unwrap_or_else(|| {
                        snap.entries.iter().map(|e| e.value).sum::<f64>().min(100.0)
                    });
                    let mem_pct = if snap.total_ram_bytes > 0 {
                        snap.sys_mem_used_bytes as f64 / snap.total_ram_bytes as f64 * 100.0
                    } else {
                        0.0
                    };
                    (cpu, mem_pct, snap.sys_mem_used_bytes, true)
                } else {
                    (0.0, 0.0, 0u64, false)
                };
                let extra = if conn.err.is_some() {
                    "error".into()
                } else if conn.thin {
                    "(thin)".into()
                } else if !has_data {
                    "connecting…".into()
                } else {
                    conn.app_label.clone()
                };
                BarEntry {
                    label: conn.pod_name.clone(),
                    value: cpu,
                    count: None,
                    extra,
                    fading: false,
                    fade_t: 0.0,
                    rss_bytes: mem_used,
                    mem_pct,
                    page_faults_s: 0.0,
                    disk_read_s: 0.0,
                    disk_write_s: 0.0,
                    ctx_switches_s: 0.0,
                    open_fds: 0,
                    swap_bytes: 0,
                    sched_wait_pct: 0.0,
                    power_w: 0.0,
                    disk_complete: true,
                    status_complete: true,
                    fds_complete: true,
                    sched_complete: true,
                    rss_complete: true,
                    cfs_throttle_pct: 0.0,
                    psi_cpu_avg10: None,
                    psi_mem_avg10: None,
                    psi_io_avg10: None,
                    cg_v2_complete: false,
                    gpu_pct: 0.0,
                    gpu_vram_bytes: 0,
                }
            }).collect();

            // Fair-share overlay: group pods by app_label.
            // Each group's MemberSeries has one entry per pod; pods with data only.
            // Shows whether replicas of a Deployment share load evenly.
            self.group_member_vals.clear();
            let mut by_app: std::collections::HashMap<String, Vec<f64>> = std::collections::HashMap::new();
            for conn in &self.kube_conns {
                if let Some(ref snap) = conn.snap {
                    let cpu = snap.sys_cpu_pct.unwrap_or(0.0);
                    by_app.entry(conn.app_label.clone()).or_default().push(cpu);
                }
            }
            for (app_label, vals) in by_app {
                // Only insert groups with >= 2 pods: single-replica groups have no
                // distribution to visualise and would produce meaningless heat.
                if vals.len() >= 2 {
                    self.group_member_vals.insert(app_label, MemberSeries { metric: hist_metric, vals });
                }
            }

            Ok(raw)
        } else if matches!(self.mode, AppMode::Nomad { .. }) {
            // Poll each Nomad allocation connection for new snapshots.
            let hist_metric = match self.active_side {
                Side::Left => self.left_metric,
                Side::Right => self.right_metric,
            };
            for conn in &mut self.nomad_conns {
                let new_snap = match &mut conn.client {
                    Some(FleetClient::Daemon(c)) => c.try_recv(),
                    Some(FleetClient::Thin(t))   => t.try_recv(),
                    None => None,
                };
                if let Some(snap) = new_snap {
                    conn.snap = Some(snap);
                    conn.err = None;
                }
                let alive = match &mut conn.client {
                    Some(FleetClient::Daemon(c)) => c.is_alive(),
                    Some(FleetClient::Thin(t))   => t.is_alive(),
                    None => false,
                };
                if !alive && conn.client.is_some() {
                    conn.err = Some("connection lost".into());
                }
            }
            // Build BarEntries: one row per allocation.
            // Label format: "{job_id}[{alloc_short}]" keeps the job name visible at a glance.
            let raw: Vec<BarEntry> = self.nomad_conns.iter().map(|conn| {
                let label = format!("{}[{}]", conn.job_id, conn.alloc_short);
                let (cpu, mem_pct, mem_used, has_data) = if let Some(ref snap) = conn.snap {
                    let cpu = snap.sys_cpu_pct.unwrap_or_else(|| {
                        snap.entries.iter().map(|e| e.value).sum::<f64>().min(100.0)
                    });
                    let mem_pct = if snap.total_ram_bytes > 0 {
                        snap.sys_mem_used_bytes as f64 / snap.total_ram_bytes as f64 * 100.0
                    } else { 0.0 };
                    (cpu, mem_pct, snap.sys_mem_used_bytes, true)
                } else {
                    (0.0, 0.0, 0u64, false)
                };
                let extra = if conn.err.is_some() {
                    "error".into()
                } else if conn.thin {
                    "(thin)".into()
                } else if !has_data {
                    "connecting…".into()
                } else {
                    conn.task_group.clone()
                };
                BarEntry {
                    label,
                    value: cpu,
                    count: None,
                    extra,
                    fading: false,
                    fade_t: 0.0,
                    rss_bytes: mem_used,
                    mem_pct,
                    page_faults_s: 0.0,
                    disk_read_s: 0.0,
                    disk_write_s: 0.0,
                    ctx_switches_s: 0.0,
                    open_fds: 0,
                    swap_bytes: 0,
                    sched_wait_pct: 0.0,
                    power_w: 0.0,
                    disk_complete: true,
                    status_complete: true,
                    fds_complete: true,
                    sched_complete: true,
                    rss_complete: true,
                    cfs_throttle_pct: 0.0,
                    psi_cpu_avg10: None,
                    psi_mem_avg10: None,
                    psi_io_avg10: None,
                    cg_v2_complete: false,
                    gpu_pct: 0.0,
                    gpu_vram_bytes: 0,
                }
            }).collect();
            // Work-density overlay: process distribution within each allocation.
            // Key = entry label (same fleet-host pattern), value = per-process CPU from daemon.
            self.group_member_vals.clear();
            for conn in &self.nomad_conns {
                if let Some(ref snap) = conn.snap {
                    let label = format!("{}[{}]", conn.job_id, conn.alloc_short);
                    let vals: Vec<f64> = snap.entries.iter().map(|e| e.value).collect();
                    if !vals.is_empty() {
                        self.group_member_vals.insert(label, MemberSeries { metric: hist_metric, vals });
                    }
                }
            }
            Ok(raw)
        } else {
            // Non-blocking drain of the Proxmox background thread channel.
            // We take the latest packet (skipping any stale intermediate ones)
            // so the UI always shows the freshest available data.
            let rx = match self.proxmox_rx.as_ref() {
                Some(r) => r,
                None => return,
            };
            let mut latest: Option<PvePacket> = None;
            while let Ok(pkt) = rx.try_recv() {
                latest = Some(pkt);
            }
            match latest {
                None => return, // no new data yet; keep existing entries
                Some(pkt) => {
                    if let Some(e) = pkt.err {
                        let age = self.proxmox_last_ok
                            .map(|t| format!(", last ok {}s ago", t.elapsed().as_secs()))
                            .unwrap_or_default();
                        return self.error = Some(format!("{e}{age}"));
                    }
                    self.vm_meta = pkt.meta;
                    self.proxmox_last_ok = Some(Instant::now());
                    self.pve_node_status = pkt.node_status;
                    self.pve_storage_status = pkt.storage_status;

                    // Populate group_member_vals from the per-group per-metric map.
                    // The packet always carries all four Proxmox metrics; we pick
                    // whichever the user is currently looking at for the overlay.
                    let hist_metric = match self.active_side {
                        Side::Left => self.left_metric,
                        Side::Right => self.right_metric,
                    };
                    self.group_member_vals.clear();
                    for (label, metric_map) in &pkt.member_vals {
                        if let Some(vals) = metric_map.get(&hist_metric) {
                            if !vals.is_empty() {
                                self.group_member_vals.insert(
                                    label.clone(),
                                    MemberSeries { metric: hist_metric, vals: vals.clone() },
                                );
                            }
                        }
                    }

                    Ok(pkt.entries)
                }
            }
        };

        // Update system-level metrics (local mode only)
        if is_local {
            let new_sys = local::sample_sys();
            if let Some(ref prev) = self.prev_sys {
                // Compute per-second rates from cumulative counters using actual elapsed time.
                let dt = new_sys.at.duration_since(prev.at).as_secs_f64().max(0.001);
                self.sys_net_rx_s =
                    new_sys.net_rx_bytes.saturating_sub(prev.net_rx_bytes) as f64 / dt;
                self.sys_net_tx_s =
                    new_sys.net_tx_bytes.saturating_sub(prev.net_tx_bytes) as f64 / dt;
                self.sys_gpu_pct = new_sys.gpu_pct;
                if let (Some(new_uj), Some(prev_uj)) = (new_sys.rapl_uj, prev.rapl_uj) {
                    // RAPL counter is in microjoules; wrapping_sub handles the hardware counter
                    // wrapping. Divide by 1_000_000 to get joules, then by dt for watts.
                    let delta_uj = new_uj.wrapping_sub(prev_uj) as f64;
                    self.sys_rapl_w = delta_uj / 1_000_000.0 / dt;
                }
            }
            self.sys_psi_cpu = new_sys.psi_cpu;
            self.sys_psi_mem = new_sys.psi_mem;
            self.sys_psi_io  = new_sys.psi_io;
            self.prev_sys = Some(new_sys);
        }

        match result {
            Ok(mut raw) => {
                // Distribute total RAPL package watts across groups in proportion to CPU share.
                // This is an estimate (not a per-process measurement), which is why the UI
                // shows a "≈" prefix. Total_cpu guards against division by zero.
                let total_cpu: f64 = raw.iter().map(|e| e.value).sum();
                if total_cpu > 0.001 && self.sys_rapl_w > 0.0 {
                    for e in raw.iter_mut() {
                        e.power_w = e.value / total_cpu * self.sys_rapl_w;
                    }
                }

                use std::collections::HashSet;
                // Groups disappear from /proc when all their PIDs exit. We keep them
                // visible for 5 seconds, fading from active colour to dark grey.
                const RETENTION: Duration = Duration::from_secs(5);
                let now = Instant::now();

                let mut map: HashMap<String, BarEntry> =
                    raw.into_iter().map(|e| (e.label.clone(), e)).collect();

                // Record when each group was last seen alive.
                for (label, entry) in &map {
                    self.last_seen.insert(label.clone(), now);
                    self.last_values.insert(label.clone(), entry.clone());
                }

                // Trim stable_order: remove groups that are both gone from /proc
                // and past the retention window.
                self.stable_order.retain(|label| {
                    map.contains_key(label)
                        || self
                            .last_seen
                            .get(label)
                            .is_some_and(|t| now.duration_since(*t) < RETENTION)
                });

                // Append new groups that have appeared since the last tick, sorted by
                // sort_metric so newcomers enter in a meaningful position.
                let existing: HashSet<&str> =
                    self.stable_order.iter().map(String::as_str).collect();
                let sort_m = self.sort_metric;
                let total_ram = self.total_ram_bytes;
                let mut new_labels: Vec<String> = map
                    .keys()
                    .filter(|k| !existing.contains(k.as_str()))
                    .cloned()
                    .collect();
                new_labels.sort_by(|a, b| {
                    let va = map.get(a).map(|e| metric_sort_val(e, sort_m, total_ram)).unwrap_or(0.0);
                    let vb = map.get(b).map(|e| metric_sort_val(e, sort_m, total_ram)).unwrap_or(0.0);
                    vb.partial_cmp(&va).unwrap_or(std::cmp::Ordering::Equal)
                });
                self.stable_order.extend(new_labels);

                self.total_groups = self.stable_order.len();
                let limit = if self.top > 0 { self.top } else { usize::MAX };

                let mut entries = Vec::with_capacity(self.stable_order.len().min(limit));
                for label in self.stable_order.iter().take(limit) {
                    if let Some(entry) = map.remove(label) {
                        // Group is still alive: use fresh data.
                        entries.push(entry);
                    } else if let Some(mut e) = self.last_values.get(label).cloned() {
                        // Group is gone: zero rate-based metrics (CPU, disk, faults, etc.)
                        // but keep static ones (fds, swap, threads) at their last value.
                        e.value = 0.0;
                        e.page_faults_s = 0.0;
                        e.disk_read_s = 0.0;
                        e.disk_write_s = 0.0;
                        e.ctx_switches_s = 0.0;
                        e.sched_wait_pct = 0.0;
                        e.power_w = 0.0;
                        e.fading = true;
                        // fade_t: 0.0 when just gone, 1.0 at the end of the retention window.
                        let elapsed = self
                            .last_seen
                            .get(label)
                            .map(|t| now.duration_since(*t).as_secs_f64())
                            .unwrap_or(RETENTION.as_secs_f64());
                        e.fade_t = (elapsed / RETENTION.as_secs_f64()).clamp(0.0, 1.0);
                        entries.push(e);
                    }
                }
                self.entries = entries;
                self.error = None;

                // Update rolling peaks (only non-fading entries contribute).
                self.peak_vals.update(&self.entries);

                // Set unprivileged flag if any entry has incomplete metrics.
                // We use a one-way latch (only set, never cleared) so the note
                // stays visible even after switching to a view with no denied entries.
                if !self.running_unprivileged {
                    self.running_unprivileged = self.entries.iter().any(|e| {
                        !e.disk_complete || !e.status_complete || !e.fds_complete
                            || !e.sched_complete || !e.rss_complete
                    });
                }
            }
            Err(e) => self.error = Some(e.to_string()),
        }

        // Reconcile the body carousel with the current entry list.
        self.sync_body_tree();

        // Thread sampling when the thread-detail view is open (local only).
        // We re-sample on every refresh so thread CPU% stays current.
        let thread_label: Option<String> = match &self.view {
            AppView::Threads { label } => Some(label.clone()),
            AppView::Groups
            | AppView::Manual
            | AppView::Connecting { .. }
            | AppView::Remote { .. } => None,
        };
        if let (Some(label), AppMode::Local) = (thread_label, &self.mode) {
            let pids = self
                .snap
                .as_ref()
                .and_then(|s| s.groups.get(&label))
                .map(|g| g.pids.clone())
                .unwrap_or_default();

            if !pids.is_empty() {
                let cpu_total = self.snap.as_ref().map_or(0, |s| s.total);
                match local::sample_threads(
                    &pids,
                    self.thread_snap.take(),
                    &local::ThreadFields::all(),
                    cpu_total,
                ) {
                    Ok((mut samples, snap)) => {
                        self.thread_snap = Some(snap);
                        // Sort hottest thread first so the top-left heatmap cell is always the busiest.
                        samples.sort_by(|a, b| {
                            b.cpu_pct
                                .partial_cmp(&a.cpu_pct)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        });
                        self.thread_samples = samples;
                    }
                    Err(e) => self.error = Some(e.to_string()),
                }
            }
        }

        // Push current state to the replay ring buffer.
        {
            let snap = HistoryEntry {
                at: Instant::now(),
                entries: self.entries.clone(),
                member_vals: self.group_member_vals.clone(),
            };
            self.history.push_back(snap);
            if self.history.len() > self.history_depth {
                self.history.pop_front();
            }
        }
        // Compute anomalies from the current group_member_vals.
        self.compute_anomalies();
    }

    /// Compute per-group N_eff concentration and update anomaly_states.
    ///
    /// Flags groups where the effective load distribution becomes concentrated
    /// (balance_frac < BALANCE_ALERT_THRESHOLD) or where a member's share
    /// collapses from active to near-zero ("stopped pulling weight").
    /// Fires --alert-cmd when an anomaly is first detected, or after the
    /// 60-second rate-limit window expires.
    fn compute_anomalies(&mut self) {
        let now = Instant::now();
        let alert_cmd = self.alert_cmd.clone();

        // Collect (label, vals) to avoid borrow conflict with self.anomaly_states.
        let items: Vec<(GroupLabel, Vec<f64>)> = self
            .group_member_vals
            .iter()
            .map(|(k, v)| (k.clone(), v.vals.clone()))
            .collect();

        // Drop state for groups no longer in the member-vals map.
        self.anomaly_states.retain(|k, _| self.group_member_vals.contains_key(k));

        for (label, vals) in items {
            let n = vals.len();
            if n < 2 {
                continue;
            }
            let eff = n_eff(&vals);
            let balance_frac = (eff / n as f64).clamp(0.0, 1.0);

            let sum: f64 = vals.iter().sum();
            let shares: Vec<f64> = if sum > 1e-9 {
                vals.iter().map(|v| v / sum).collect()
            } else {
                vec![1.0 / n as f64; n]
            };

            let concentrated = n >= 5 && balance_frac < BALANCE_ALERT_THRESHOLD;

            // Dropout: the SAME member (by stable index) that was carrying significant
            // load (>15% share) has dropped to near-zero (<3%).  We use index-stable
            // comparison rather than any()/any() to avoid false positives in groups like
            // chrome or kworker where some members are always idle while others are active.
            let prev = self.anomaly_states.get(&label);
            let dropout = if let Some(ps) = prev {
                if ps.prev_shares.len() == n {
                    ps.prev_shares
                        .iter()
                        .zip(shares.iter())
                        .any(|(&prev_s, &now_s)| prev_s > 0.15 && now_s < 0.03)
                } else {
                    false
                }
            } else {
                false
            };

            let alerting = concentrated || dropout;
            let kind = if dropout { "dropout" } else if concentrated { "concentrated" } else { "" };

            // Determine whether to fire the alert command.
            let should_fire = alerting && match prev {
                None => true,
                Some(ps) => {
                    !ps.alerting
                        || ps.last_alert_at
                            .is_none_or(|t| t.elapsed().as_secs() >= ALERT_RATE_LIMIT_S)
                }
            };

            let last_alert_at = if should_fire {
                if let Some(ref cmd) = alert_cmd {
                    fire_alert_cmd(cmd, &label, kind, balance_frac);
                }
                Some(now)
            } else {
                prev.and_then(|ps| ps.last_alert_at)
            };

            self.anomaly_states.insert(label, AnomalyState {
                balance_frac,
                prev_shares: shares,
                alerting,
                kind: kind.to_string(),
                last_alert_at,
            });
        }
    }
}

/// Headless daemon mode: stream newline-delimited JSON snapshots to stdout.
///
/// This is the server side of the remote drill-down feature. The remote aerie
/// (in Proxmox mode) SSHes into a VM, runs `aerie --daemon`, and reads its
/// stdout. Each line is a complete `DaemonSnapshot` JSON object.
///
/// The daemon uses `CollectOpts::default()` (all metrics enabled) because the
/// consumer (remote UI) can display any combination of metrics.
///
/// `interval` is how long to sleep between samples; it is passed through from
/// the CLI so remote refresh rate matches the parent's setting.
fn run_daemon(interval: Duration) -> Result<()> {
    use std::io::Write;
    let total_ram_bytes = local::total_ram_bytes();
    // Enable all metrics so the consumer can show any combination.
    let opts = local::CollectOpts::default();
    let mut snap: Option<local::Snapshot> = None;
    let mut prev_sys: Option<local::SysSample> = None;
    let mut snap_count = 0usize;
    // Previous /proc/stat jiffy counts for system-wide CPU% delta computation.
    let mut prev_cpu_total: Option<u64> = None;
    let mut prev_cpu_idle: Option<u64> = None;

    loop {
        let (mut entries, new_snap) = local::sample(snap, &opts, GroupBy::Comm)?;
        snap = Some(new_snap);
        snap_count += 1;

        let new_sys = local::sample_sys();
        let (rx_s, tx_s, gpu, rapl_w, psi_cpu, psi_mem, psi_io) = if let Some(ref ps) = prev_sys {
            let dt = new_sys.at.duration_since(ps.at).as_secs_f64().max(0.001);
            let rx = new_sys.net_rx_bytes.saturating_sub(ps.net_rx_bytes) as f64 / dt;
            let tx = new_sys.net_tx_bytes.saturating_sub(ps.net_tx_bytes) as f64 / dt;
            // wrapping_sub handles the RAPL counter overflow (32- or 64-bit depending on kernel).
            let rapl = match (new_sys.rapl_uj, ps.rapl_uj) {
                (Some(n), Some(p)) => n.wrapping_sub(p) as f64 / 1_000_000.0 / dt,
                _ => 0.0,
            };
            (rx, tx, new_sys.gpu_pct, rapl, new_sys.psi_cpu, new_sys.psi_mem, new_sys.psi_io)
        } else {
            (0.0, 0.0, new_sys.gpu_pct, 0.0, new_sys.psi_cpu, new_sys.psi_mem, new_sys.psi_io)
        };
        prev_sys = Some(new_sys);

        // Compute system-wide CPU% from /proc/stat jiffy deltas.
        let sys_cpu_pct: Option<f64> = if let Ok((now_total, now_idle)) = local::cpu_total_and_idle() {
            let result = match (prev_cpu_total, prev_cpu_idle) {
                (Some(pt), Some(pi)) => {
                    let d_total = now_total.saturating_sub(pt) as f64;
                    let d_idle  = now_idle.saturating_sub(pi) as f64;
                    if d_total > 0.0 {
                        Some(((d_total - d_idle) / d_total * 100.0).clamp(0.0, 100.0))
                    } else {
                        None
                    }
                }
                _ => None, // first iteration: no previous sample yet
            };
            prev_cpu_total = Some(now_total);
            prev_cpu_idle  = Some(now_idle);
            result
        } else {
            None
        };

        // Compute memory-in-use from MemAvailable.
        let mem_avail = local::mem_available_bytes();
        let sys_mem_used_bytes = total_ram_bytes.saturating_sub(mem_avail);

        // Distribute RAPL package watts across groups proportionally to CPU share.
        if rapl_w > 0.0 {
            let total_cpu: f64 = entries.iter().map(|e| e.value).sum();
            if total_cpu > 0.001 {
                for e in &mut entries {
                    e.power_w = e.value / total_cpu * rapl_w;
                }
            }
        }

        let snapshot = remote::DaemonSnapshot {
            entries,
            total_ram_bytes,
            snap_count,
            sys_net_rx_s: rx_s,
            sys_net_tx_s: tx_s,
            sys_gpu_pct: gpu,
            sys_rapl_w: rapl_w,
            sys_psi_cpu: psi_cpu,
            sys_psi_mem: psi_mem,
            sys_psi_io: psi_io,
            sys_cpu_pct,
            sys_mem_used_bytes,
        };

        // Emit the snapshot as a single JSON line, then flush immediately so the
        // remote reader's BufReader sees it without buffering delay.
        let json = serde_json::to_string(&snapshot)?;
        println!("{json}");
        std::io::stdout().flush()?;

        std::thread::sleep(interval);
    }
}

/// Entry point: parse CLI, enter daemon mode or run the TUI event loop.
fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.manual {
        print!("{}", ui::manual_text());
        return Ok(());
    }

    if cli.daemon {
        return run_daemon(Duration::from_secs_f64(cli.interval));
    }

    let mut state = AppState::new(&cli)?;
    // Do an initial refresh before entering the TUI so the first frame shows data.
    state.refresh();

    let mut backend = CrosstermBackend::new(io::stdout());
    backend.apply_capabilities(&Capabilities::detect());
    let mut terminal = Terminal::new(backend)?;
    terminal.enter()?;

    // Track last rendered body height for histogram visible-range computation.
    let mut last_body_height: usize = 30;

    'main: loop {
        let in_remote = matches!(state.view, AppView::Remote { .. });

        // Poll remote daemon for new snapshots.
        if in_remote {
            if let Some(ref mut client) = state.remote_client {
                if !client.is_alive() {
                    // SSH process died; return to group view with an error notice.
                    state.error = Some("Remote connection lost.".into());
                    if let Some(c) = state.remote_client.take() {
                        c.close();
                    }
                    if let Some(tree) = &mut state.body_tree { tree.zoom_reset(); }
                    state.view = AppView::Groups;
                    state.last_refresh = None;
                } else if let Some(snap) = client.try_recv() {
                    // Latest snapshot from the remote daemon.
                    state.entries = snap.entries;
                    state.total_ram_bytes = snap.total_ram_bytes;
                    state.sys_net_rx_s = snap.sys_net_rx_s;
                    state.sys_net_tx_s = snap.sys_net_tx_s;
                    state.sys_gpu_pct = snap.sys_gpu_pct;
                    state.sys_rapl_w = snap.sys_rapl_w;
                    state.sys_psi_cpu = snap.sys_psi_cpu;
                    state.sys_psi_mem = snap.sys_psi_mem;
                    state.sys_psi_io  = snap.sys_psi_io;
                    state.snap_count = snap.snap_count;
                    // Reconcile the carousel with the remote entries so render_body
                    // can display them (reconcile normally runs in refresh(), which
                    // is skipped while in_remote).
                    state.sync_body_tree();
                }
            }
        }

        // Refresh local/proxmox data when the interval has elapsed.
        // Remote view is excluded: it refreshes on the daemon's own schedule.
        if !in_remote && state.last_refresh.is_none_or(|t| t.elapsed() >= state.interval) {
            state.refresh();
        }

        // Sample histograms on their own 1 s cadence, independent of the refresh interval.
        if matches!(state.mode, AppMode::Local)
            && state.snap.is_some()
            && state.last_hist_sample.is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
        {
            state.last_hist_sample = Some(Instant::now());
            state.sample_histograms();
        }

        terminal.draw(|buf| {
            // The body area is the full terminal height minus 3 rows (header) minus 2 rows (footer).
            last_body_height = buf.area.height.saturating_sub(5) as usize;
            state.last_body_height = last_body_height;
            ui::render(buf, &mut state);
        })?;

        // Compute the shortest possible poll timeout so we wake exactly when the next
        // event is expected: in Remote view we poll at 100 ms to pick up daemon snapshots
        // quickly; otherwise we sleep until the next refresh or histogram tick.
        let now = Instant::now();
        let wait = if in_remote {
            Duration::from_millis(100)
        } else {
            let next_refresh =
                state.last_refresh.map(|t| t + state.interval).unwrap_or(now);
            let next_hist = if matches!(state.mode, AppMode::Local) {
                state
                    .last_hist_sample
                    .map(|t| t + Duration::from_secs(1))
                    .unwrap_or(now)
            } else {
                next_refresh
            };
            // Wake at whichever of the two comes first.
            next_refresh.min(next_hist).saturating_duration_since(now)
        };

        if let Some(event) = poll_event(wait)? {
            if let Event::Key(key) = event {
                // In Remote view, treat the data source as local for metric cycling
                // because the daemon sends local /proc data.
                let is_local = matches!(state.mode, AppMode::Local)
                    || matches!(state.view, AppView::Remote { .. });
                match key.code {
                    KeyCode::Char('q') => break 'main,
                    KeyCode::Char('c')
                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        break 'main
                    }
                    KeyCode::Esc => {
                        match &state.view {
                            AppView::Threads { .. } => {
                                // Clear thread state so the next Enter into a different group
                                // doesn't momentarily show stale data.
                                if let Some(tree) = &mut state.body_tree { tree.zoom_out(); }
                                state.view = AppView::Groups;
                                state.thread_snap = None;
                                state.thread_samples = vec![];
                            }
                            AppView::Manual => state.view = AppView::Groups,
                            AppView::Remote { .. } | AppView::Connecting { .. } => {
                                if let Some(c) = state.remote_client.take() {
                                    c.close();
                                }
                                if let Some(tree) = &mut state.body_tree { tree.zoom_reset(); }
                                state.view = AppView::Groups;
                                // Force an immediate Proxmox refresh so the group list
                                // is up-to-date after disconnecting.
                                state.last_refresh = None;
                            }
                            // Pressing Esc on the top-level group list exits the app.
                            AppView::Groups => break 'main,
                        }
                    }
                    KeyCode::Char('m') => {
                        if matches!(state.view, AppView::Manual) {
                            state.view = AppView::Groups;
                        } else {
                            state.manual_scroll = 0;
                            state.view = AppView::Manual;
                        }
                    }
                    KeyCode::Char('r')
                        if !matches!(state.view, AppView::Remote { .. }) =>
                    {
                        state.refresh();
                    }
                    // Toggle distribution-heat histogram overlay
                    KeyCode::Char('h') => {
                        if matches!(state.view, AppView::Groups | AppView::Remote { .. }) {
                            state.show_histogram = !state.show_histogram;
                        }
                    }
                    // Cycle grouping strategy (Groups view, local or Proxmox mode)
                    KeyCode::Char('g') if matches!(state.view, AppView::Groups) => {
                        if matches!(state.mode, AppMode::Local) {
                            state.group_by = state.group_by.next();
                            // Discard snapshot so the next collect() uses the new strategy.
                            state.snap = None;
                            state.stable_order.clear();
                            state.entries.clear();
                        } else if matches!(state.mode, AppMode::Proxmox { .. }) {
                            state.pve_group_by = state.pve_group_by.next();
                            // Publish new grouping to the background thread atomically.
                            state.pve_group_by_shared.store(
                                state.pve_group_by as u8,
                                Ordering::Relaxed,
                            );
                            // Clear member vals so the overlay doesn't show stale data.
                            state.group_member_vals.clear();
                            state.stable_order.clear();
                            state.entries.clear();
                        }
                        // Fleet / Kube / Nomad: no grouping to cycle; ignore silently.
                    }
                    // Navigation: arrow keys (and vim j/k) route through the carousel focus.
                    KeyCode::Up | KeyCode::Char('k') => {
                        if matches!(state.view, AppView::Groups | AppView::Remote { .. }) {
                            if let Some(tree) = &mut state.body_tree {
                                tree.focus_dir(Direction::Up);
                            }
                        } else if matches!(state.view, AppView::Manual) {
                            state.manual_scroll = state.manual_scroll.saturating_sub(1);
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if matches!(state.view, AppView::Groups | AppView::Remote { .. }) {
                            if let Some(tree) = &mut state.body_tree {
                                tree.focus_dir(Direction::Down);
                            }
                        } else if matches!(state.view, AppView::Manual) {
                            let max_scroll = ui::manual_line_count()
                                .saturating_sub(state.last_body_height);
                            state.manual_scroll =
                                state.manual_scroll.saturating_add(1).min(max_scroll);
                        }
                    }
                    KeyCode::PageUp => {
                        if matches!(state.view, AppView::Manual) {
                            let page = state.last_body_height.max(1);
                            state.manual_scroll = state.manual_scroll.saturating_sub(page);
                        }
                    }
                    KeyCode::PageDown => {
                        if matches!(state.view, AppView::Manual) {
                            let page = state.last_body_height.max(1);
                            let max_scroll = ui::manual_line_count()
                                .saturating_sub(state.last_body_height);
                            state.manual_scroll =
                                state.manual_scroll.saturating_add(page).min(max_scroll);
                        }
                    }
                    // Metric cycling for the active side (left/right arrows),
                    // or scrub through history when paused.
                    KeyCode::Left => {
                        if matches!(state.view, AppView::Groups | AppView::Remote { .. }) {
                            if let Some(cursor) = state.history_cursor {
                                // Scrub backward in time (toward older samples).
                                if cursor > 0 {
                                    let new_cursor = cursor - 1;
                                    state.history_cursor = Some(new_cursor);
                                    if let Some(h) = state.history.get(new_cursor) {
                                        state.entries = h.entries.clone();
                                        state.group_member_vals = h.member_vals.clone();
                                    }
                                }
                            } else {
                                match state.active_side {
                                    Side::Left  => state.left_metric  = state.left_metric.cycle_prev(is_local),
                                    Side::Right => state.right_metric = state.right_metric.cycle_prev(is_local),
                                }
                            }
                        }
                    }
                    KeyCode::Right => {
                        if matches!(state.view, AppView::Groups | AppView::Remote { .. }) {
                            if let Some(cursor) = state.history_cursor {
                                // Scrub forward in time (toward newer samples).
                                if cursor + 1 < state.history.len() {
                                    let new_cursor = cursor + 1;
                                    state.history_cursor = Some(new_cursor);
                                    if let Some(h) = state.history.get(new_cursor) {
                                        state.entries = h.entries.clone();
                                        state.group_member_vals = h.member_vals.clone();
                                    }
                                }
                            } else {
                                match state.active_side {
                                    Side::Left  => state.left_metric  = state.left_metric.cycle_next(is_local),
                                    Side::Right => state.right_metric = state.right_metric.cycle_next(is_local),
                                }
                            }
                        }
                    }
                    // Toggle active side
                    KeyCode::Tab => {
                        if matches!(state.view, AppView::Groups | AppView::Remote { .. }) {
                            state.active_side = match state.active_side {
                                Side::Left => Side::Right,
                                Side::Right => Side::Left,
                            };
                        }
                    }
                    // Sort by the active side's metric
                    KeyCode::Char('s') => {
                        if matches!(state.view, AppView::Groups | AppView::Remote { .. }) {
                            let m = match state.active_side {
                                Side::Left => state.left_metric,
                                Side::Right => state.right_metric,
                            };
                            state.sort_metric = m;
                            let total_ram = state.total_ram_bytes;
                            // Re-sort the stable_order in-place using last_values so fading
                            // rows also end up in a sensible position.
                            state.stable_order.sort_by(|a, b| {
                                let va = state.last_values.get(a)
                                    .map(|e| metric_sort_val(e, m, total_ram))
                                    .unwrap_or(0.0);
                                let vb = state.last_values.get(b)
                                    .map(|e| metric_sort_val(e, m, total_ram))
                                    .unwrap_or(0.0);
                                vb.partial_cmp(&va).unwrap_or(std::cmp::Ordering::Equal)
                            });
                        }
                    }
                    // Enter: drill down — threads (local), SSH daemon (proxmox/fleet), kubectl exec (kube), nomad alloc exec (nomad)
                    KeyCode::Enter => {
                        if matches!(state.view, AppView::Groups) {
                            if matches!(state.mode, AppMode::Nomad { .. }) {
                                if let Some(conn) = state.focused_entry_idx().and_then(|i| state.nomad_conns.get(i)) {
                                    if conn.thin {
                                        state.error = Some(
                                            "thin probe mode — no per-process drill-down; remove --nomad-thin to enable".into()
                                        );
                                    } else if conn.client.is_none() {
                                        state.error = Some(format!("not connected to alloc {}", conn.alloc_short));
                                    } else {
                                        let alloc_id = conn.alloc_id.clone();
                                        let task = conn.task_name.clone();
                                        let label = format!("{}[{}]", conn.job_id, conn.alloc_short);
                                        let (addr, token) = if let AppMode::Nomad { ref addr, ref token, .. } = state.mode {
                                            (addr.clone(), token.clone())
                                        } else {
                                            unreachable!()
                                        };
                                        if let Some(tree) = &mut state.body_tree {
                                            tree.zoom_to(id_from_key(&label));
                                        }
                                        state.view = AppView::Connecting { label: label.clone() };
                                        terminal.draw(|buf| ui::render(buf, &mut state))?;
                                        match remote::connect_nomad_daemon(&alloc_id, &task, &addr, token.as_deref()) {
                                            Ok(client) => {
                                                state.remote_client = Some(client);
                                                state.view = AppView::Remote { label };
                                                state.entries = vec![];
                                                state.snap_count = 0;
                                                state.error = None;
                                                state.sync_body_tree();
                                            }
                                            Err(e) => {
                                                if let Some(tree) = &mut state.body_tree { tree.zoom_reset(); }
                                                state.view = AppView::Groups;
                                                state.error = Some(format!("drill-down failed: {e}"));
                                            }
                                        }
                                    }
                                }
                            } else if matches!(state.mode, AppMode::Kube { .. }) {
                                // Kube drill-down: connect to the selected pod for per-process detail.
                                if let Some(conn) = state.focused_entry_idx().and_then(|i| state.kube_conns.get(i)) {
                                    if conn.thin {
                                        state.error = Some(
                                            "thin probe mode — no per-process drill-down; remove --kube-thin to enable".into()
                                        );
                                    } else if conn.client.is_none() {
                                        state.error = Some(format!("not connected to pod {}", conn.pod_name));
                                    } else {
                                        let pod = conn.pod_name.clone();
                                        // Extract namespace and context from the current AppMode.
                                        let (ns, ctx) = if let AppMode::Kube { ref namespace, ref context, .. } = state.mode {
                                            (namespace.clone(), context.clone())
                                        } else {
                                            unreachable!()
                                        };
                                        if let Some(tree) = &mut state.body_tree {
                                            tree.zoom_to(id_from_key(&pod));
                                        }
                                        state.view = AppView::Connecting { label: pod.clone() };
                                        terminal.draw(|buf| ui::render(buf, &mut state))?;
                                        match remote::connect_kube_daemon(&pod, &ns, ctx.as_deref()) {
                                            Ok(client) => {
                                                state.remote_client = Some(client);
                                                state.view = AppView::Remote { label: pod };
                                                state.entries = vec![];
                                                state.snap_count = 0;
                                                state.error = None;
                                                state.sync_body_tree();
                                            }
                                            Err(e) => {
                                                if let Some(tree) = &mut state.body_tree { tree.zoom_reset(); }
                                                state.view = AppView::Groups;
                                                state.error = Some(format!("drill-down failed: {e}"));
                                            }
                                        }
                                    }
                                }
                            } else if matches!(state.mode, AppMode::Fleet { .. }) {
                                // Fleet: drill into the selected host's per-process view.
                                if let Some(conn) = state.focused_entry_idx().and_then(|i| state.fleet_clients.get(i)) {
                                    if conn.thin {
                                        state.error = Some(
                                            "thin probe mode — no per-process drill-down; remove --thin to enable".into()
                                        );
                                    } else if conn.client.is_none() {
                                        state.error = Some(format!("not connected to {}", conn.hostname));
                                    } else if !state.enable_remote {
                                        state.error = Some("remote drill-down disabled — re-run with --enable-remote".into());
                                    } else {
                                        let ssh_user = state.ssh_user.clone();
                                        let host = conn.hostname.clone();
                                        let policy = if state.ssh_accept_new {
                                            remote::SshHostKeyPolicy::AcceptNew
                                        } else {
                                            remote::SshHostKeyPolicy::Strict
                                        };
                                        if let Some(tree) = &mut state.body_tree {
                                            tree.zoom_to(id_from_key(&host));
                                        }
                                        state.view = AppView::Connecting { label: host.clone() };
                                        terminal.draw(|buf| ui::render(buf, &mut state))?;
                                        match remote::connect_direct(&host, &ssh_user, policy) {
                                            Ok(client) => {
                                                state.remote_client = Some(client);
                                                state.view = AppView::Remote { label: host };
                                                state.entries = vec![];
                                                state.snap_count = 0;
                                                state.error = None;
                                                state.sync_body_tree();
                                            }
                                            Err(e) => {
                                                if let Some(tree) = &mut state.body_tree { tree.zoom_reset(); }
                                                state.view = AppView::Groups;
                                                state.error = Some(format!("drill-down failed: {e}"));
                                            }
                                        }
                                    }
                                }
                            } else if let Some(e) = state.focused_entry_idx().and_then(|i| state.entries.get(i)) {
                                let label = e.label.clone();
                                if matches!(state.mode, AppMode::Local) {
                                    // Local: open per-thread heat-map view.
                                    if let Some(tree) = &mut state.body_tree {
                                        tree.zoom_to(id_from_key(&label));
                                    }
                                    state.view = AppView::Threads { label };
                                    state.thread_snap = None;
                                    state.thread_samples = vec![];
                                    state.refresh();
                                } else if !state.enable_remote {
                                    // Remote drill-down is disabled; show a hint.
                                    state.error = Some(
                                        "remote drill-down disabled — re-run with --enable-remote".into()
                                    );
                                } else {
                                    // Proxmox mode: connect over SSH to remote aerie --daemon.
                                    // We show a "Connecting…" screen immediately (blocking draw)
                                    // because connect_vm can take up to ~8 s.
                                    let meta = state.vm_meta.get(&label).map(|m| proxmox::VmMeta {
                                        node: m.node.clone(),
                                        vmid: m.vmid,
                                        kind: m.kind.clone(),
                                    });
                                    let ssh_user = state.ssh_user.clone();
                                    let policy = if state.ssh_accept_new {
                                        remote::SshHostKeyPolicy::AcceptNew
                                    } else {
                                        remote::SshHostKeyPolicy::Strict
                                    };
                                    if let Some(tree) = &mut state.body_tree {
                                        tree.zoom_to(id_from_key(&label));
                                    }
                                    state.view = AppView::Connecting { label: label.clone() };
                                    // Force-render the connecting screen before blocking on SSH.
                                    terminal.draw(|buf| ui::render(buf, &mut state))?;
                                    // Create a temporary client for get_vm_ips (SSH host discovery).
                                    let temp_pve = if let (Some(url), Some(token)) =
                                        (&state.proxmox_url, &state.proxmox_token)
                                    {
                                        proxmox::Client::new(url, token, state.proxmox_insecure).ok()
                                    } else {
                                        None
                                    };
                                    match remote::connect_vm(&label, meta.as_ref(), temp_pve.as_ref(), &ssh_user, policy) {
                                        Ok(client) => {
                                            state.remote_client = Some(client);
                                            state.view = AppView::Remote { label };
                                            // Clear stale Proxmox entries; remote will populate them.
                                            state.entries = vec![];
                                            state.snap_count = 0;
                                            state.error = None;
                                            state.sync_body_tree();
                                        }
                                        Err(diag) => {
                                            // Connection failed; show diagnostics in error bar.
                                            if let Some(tree) = &mut state.body_tree { tree.zoom_reset(); }
                                            state.view = AppView::Groups;
                                            state.error = Some(diag.join("\n"));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // Cycle GPU device selection ([ = previous, ] = next)
                    KeyCode::Char('[')
                        if state.gpu_enabled && !state.gpu_devices.is_empty() => {
                        let n = state.gpu_devices.len() + 1; // 0=all, 1..=N=specific
                        state.selected_gpu = if state.selected_gpu == 0 {
                            n - 1
                        } else {
                            state.selected_gpu - 1
                        };
                    }
                    KeyCode::Char(']')
                        if state.gpu_enabled && !state.gpu_devices.is_empty() => {
                        let n = state.gpu_devices.len() + 1;
                        state.selected_gpu = (state.selected_gpu + 1) % n;
                    }
                    // Toggle pause/scrub mode for the replay ring buffer.
                    KeyCode::Char('p')
                        if matches!(state.view, AppView::Groups | AppView::Remote { .. }) =>
                    {
                        if state.history_cursor.is_some() {
                            // Resume live mode.
                            state.history_cursor = None;
                        } else if !state.history.is_empty() {
                            // Pause at the most recent snapshot.
                            let idx = state.history.len() - 1;
                            state.history_cursor = Some(idx);
                            // entries/member_vals are already showing the latest — no reload needed.
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    terminal.leave()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn n_eff_balanced_equals_n() {
        let vals = vec![10.0f64; 5];
        let eff = n_eff(&vals);
        assert!((eff - 5.0).abs() < 1e-9, "balanced: expected N_eff=5, got {eff}");
    }

    #[test]
    fn n_eff_concentrated_near_one() {
        let mut vals = vec![0.1f64; 9];
        vals[0] = 100.0;
        let eff = n_eff(&vals);
        assert!(eff < 1.5, "concentrated: expected N_eff≈1, got {eff}");
    }

    #[test]
    fn n_eff_empty_returns_zero() {
        assert_eq!(n_eff(&[]), 0.0);
    }

    #[test]
    fn n_eff_single_returns_one() {
        assert!((n_eff(&[42.0]) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn n_eff_all_zero_returns_n() {
        let vals = vec![0.0f64; 4];
        assert!((n_eff(&vals) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn parse_kube_arg_namespace_only() {
        let (ns, sel) = parse_kube_arg("default").unwrap();
        assert_eq!(ns, "default");
        assert!(sel.is_none());
    }

    #[test]
    fn parse_kube_arg_with_selector() {
        let (ns, sel) = parse_kube_arg("monitoring/app=prometheus").unwrap();
        assert_eq!(ns, "monitoring");
        assert_eq!(sel.unwrap(), "app=prometheus");
    }

    #[test]
    fn parse_kube_arg_rejects_dash_namespace() {
        assert!(parse_kube_arg("-bad").is_err());
    }

    #[test]
    fn derive_app_label_strips_two_hash_segments() {
        // Standard Deployment pod: name + replicaset hash + pod hash
        assert_eq!(derive_app_label("nginx-deployment-7d6fb9f9-xl5gr"), "nginx-deployment");
    }

    #[test]
    fn derive_app_label_strips_one_hash_segment() {
        // DaemonSet-style pod with one hash suffix
        assert_eq!(derive_app_label("fluentd-ds-abc12"), "fluentd-ds");
    }

    #[test]
    fn derive_app_label_no_hash_returns_full() {
        // Bare pod with no hash suffix
        assert_eq!(derive_app_label("mypod"), "mypod");
    }
}
