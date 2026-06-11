// SPDX-License-Identifier: GPL-3.0-or-later
// apptop — process-group performance monitor
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

use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{
    collections::HashMap,
    io,
    sync::mpsc,
    time::{Duration, Instant},
};

#[derive(Parser, Debug)]
#[command(
    name = "apptop",
    about = "Thread / VM activity bar-chart monitor.\n\
             Local mode (default): reads /proc and groups processes by name.\n\
             Proxmox mode: polls the PVE API and shows per-VM CPU + memory."
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
    #[arg(long, env = "APPTOP_ENABLE_REMOTE", default_value_t = false)]
    enable_remote: bool,

    /// Accept unknown SSH host keys on first use (TOFU).
    /// By default apptop requires the host key to already be in known_hosts.
    /// Never passes StrictHostKeyChecking=no.
    #[arg(long, default_value_t = false)]
    ssh_accept_new: bool,
}

/// Metric displayed on one side of the combined meter bar.
///
/// Each variant maps to a field in `BarEntry`. The `name()` method returns the
/// short label shown in the header and column headers. `cycle_next`/`cycle_prev`
/// walk the ordered list appropriate for the current mode (local has more options
/// than Proxmox, which only exposes CPU, memory, and disk I/O).
#[derive(Clone, Copy, PartialEq, Eq)]
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
}

impl Metric {
    /// Short display label shown in the header bar and column headings.
    pub fn name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu%",
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
        }
    }

    /// Ordered list of metrics available in local (/proc) mode.
    /// CPU and Memory lead because they are the most commonly needed.
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

/// serde default helper: returns true (so old daemon output is treated as complete).
///
/// `BarEntry` completeness flags use `#[serde(default = "default_true")]` so that
/// JSON produced by older daemon versions (which lacked these fields) deserialises
/// as "fully complete" rather than "all denied". This avoids spurious `?` markers
/// when drilling into an older remote apptop.
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
    /// Non-None when the API call failed; the string is shown in the UI error bar.
    err: Option<String>,
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
    /// Index into `entries` of the highlighted row.
    pub cursor: usize,
    /// First visible entry index (for scrolling).
    pub scroll_offset: usize,
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
    /// Per-group: cpu% and faults/s per thread (updated ~once/sec).
    pub group_member_vals: HashMap<String, local::MemberVals>,
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
    // ── rolling peak values ───────────────────────────────────────────────
    pub peak_vals: PeakVals,
    // ── privilege flag ────────────────────────────────────────────────────
    /// Set to true once we observe any entry with an incomplete metric (EACCES).
    /// Drives the "running unprivileged" notice in the footer.
    pub running_unprivileged: bool,
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
        let mode = match &cli.proxmox {
            Some(url) => {
                let token = cli.token.clone().ok_or_else(|| {
                    anyhow::anyhow!(
                        "--token (or PROXMOX_TOKEN env var) is required with --proxmox"
                    )
                })?;
                AppMode::Proxmox { url: url.clone(), token, insecure: cli.insecure }
            }
            None => AppMode::Local,
        };

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
                // Spawn the Proxmox poller thread. It loops forever, sleeping `interval`
                // between calls. The main thread drains `rx` non-blocking on each UI tick.
                std::thread::spawn(move || {
                    let mut client = match proxmox::Client::new(&url2, &token2, insecure) {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx.send(PvePacket {
                                entries: vec![],
                                meta: HashMap::new(),
                                err: Some(e.to_string()),
                            });
                            return;
                        }
                    };
                    loop {
                        let packet = match client.sample() {
                            Ok((entries, meta)) => PvePacket { entries, meta, err: None },
                            Err(e) => PvePacket {
                                entries: vec![],
                                meta: HashMap::new(),
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
        let total_ram_bytes = if is_local { local::total_ram_bytes() } else { 0 };
        // Default SSH user to the current OS user; "root" as last resort for headless systems.
        let ssh_user = cli.ssh_user.clone().unwrap_or_else(|| {
            std::env::var("USER").unwrap_or_else(|_| "root".to_string())
        });

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
            cursor: 0,
            scroll_offset: 0,
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
            peak_vals: PeakVals::default(),
            running_unprivileged: false,
        })
    }

    /// Keep `scroll_offset` so that `cursor` is always within the visible window.
    ///
    /// - If cursor is above the window, scroll up to put it at the top.
    /// - If cursor is below the window, scroll down to put it at the bottom.
    /// - No-ops when `body_height` is zero (terminal too small to display anything).
    pub fn adjust_scroll(&mut self, body_height: usize) {
        if body_height == 0 {
            return;
        }
        if self.cursor < self.scroll_offset {
            self.scroll_offset = self.cursor;
        }
        if self.cursor >= self.scroll_offset + body_height {
            self.scroll_offset = self.cursor.saturating_sub(body_height - 1);
        }
    }

    /// Sample per-thread metric values for all groups currently visible on screen.
    ///
    /// This is intentionally called at most once per second (the caller gates on
    /// `last_hist_sample`), independent of the main refresh interval, because
    /// reading `/proc/PID/task/*/stat` for every visible group is moderately
    /// expensive.
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

        // Only sample the visible window (scroll_offset .. scroll_offset + entry_rows).
        let entry_rows = self.last_body_height.saturating_sub(1);
        let visible_end = self.scroll_offset + entry_rows;

        // Collect (label, pids) pairs for visible, non-fading groups.
        let group_pids: Vec<(String, Vec<u32>)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(i, e)| !e.fading && *i >= self.scroll_offset && *i < visible_end)
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
        local::CollectOpts {
            need_io: metrics.iter().any(|m| matches!(m, Metric::DiskRead | Metric::DiskWrite)),
            need_status: metrics.iter().any(|m| matches!(m, Metric::CtxSwitches | Metric::SwapMem)),
            need_fds: metrics.iter().any(|m| matches!(m, Metric::OpenFds)),
            need_schedstat: metrics.iter().any(|m| matches!(m, Metric::SchedWait)),
            need_rss: metrics.iter().any(|m| matches!(m, Metric::Memory)),
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

        let is_local = matches!(self.mode, AppMode::Local);
        let result: Result<Vec<BarEntry>> = if is_local {
            let opts = self.collect_opts();
            let group_by = self.group_by;
            local::sample(self.snap.take(), &opts, group_by).map(|(entries, snap)| {
                self.snap = Some(snap);
                entries
            })
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

        // Clamp cursor so it never points past the last row.
        if self.entries.is_empty() {
            self.cursor = 0;
        } else {
            self.cursor = self.cursor.min(self.entries.len() - 1);
        }

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
    }
}

/// Headless daemon mode: stream newline-delimited JSON snapshots to stdout.
///
/// This is the server side of the remote drill-down feature. The remote apptop
/// (in Proxmox mode) SSHes into a VM, runs `apptop --daemon`, and reads its
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

    loop {
        let (mut entries, new_snap) = local::sample(snap, &opts, GroupBy::Comm)?;
        snap = Some(new_snap);
        snap_count += 1;

        let new_sys = local::sample_sys();
        let (rx_s, tx_s, gpu, rapl_w) = if let Some(ref ps) = prev_sys {
            let dt = new_sys.at.duration_since(ps.at).as_secs_f64().max(0.001);
            let rx = new_sys.net_rx_bytes.saturating_sub(ps.net_rx_bytes) as f64 / dt;
            let tx = new_sys.net_tx_bytes.saturating_sub(ps.net_tx_bytes) as f64 / dt;
            // wrapping_sub handles the RAPL counter overflow (32- or 64-bit depending on kernel).
            let rapl = match (new_sys.rapl_uj, ps.rapl_uj) {
                (Some(n), Some(p)) => n.wrapping_sub(p) as f64 / 1_000_000.0 / dt,
                _ => 0.0,
            };
            (rx, tx, new_sys.gpu_pct, rapl)
        } else {
            (0.0, 0.0, new_sys.gpu_pct, 0.0)
        };
        prev_sys = Some(new_sys);

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

    if cli.daemon {
        return run_daemon(Duration::from_secs_f64(cli.interval));
    }

    let mut state = AppState::new(&cli)?;
    // Do an initial refresh before entering the TUI so the first frame shows data.
    state.refresh();

    // Set up the crossterm alternate screen. Alternate screen keeps the user's
    // terminal history intact when apptop exits.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Track last rendered body height so `adjust_scroll` has the correct window size.
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
                    state.snap_count = snap.snap_count;
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

        state.adjust_scroll(last_body_height);

        terminal.draw(|f| {
            // The body area is the full terminal height minus 3 rows (header) minus 2 rows (footer).
            last_body_height = f.area().height.saturating_sub(5) as usize;
            state.last_body_height = last_body_height;
            ui::render(f, &state);
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

        if event::poll(wait)? {
            if let Event::Key(key) = event::read()? {
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
                                state.view = AppView::Groups;
                                state.thread_snap = None;
                                state.thread_samples = vec![];
                            }
                            AppView::Manual => state.view = AppView::Groups,
                            AppView::Remote { .. } | AppView::Connecting { .. } => {
                                if let Some(c) = state.remote_client.take() {
                                    c.close();
                                }
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
                    // Cycle grouping strategy (Groups view, local mode only)
                    KeyCode::Char('g')
                        if matches!(state.view, AppView::Groups)
                            && matches!(state.mode, AppMode::Local) =>
                    {
                        state.group_by = state.group_by.next();
                        // Discard the previous snapshot so the next collect() uses the new strategy.
                        state.snap = None;
                        state.stable_order.clear();
                        state.entries.clear();
                    }
                    // Cursor navigation: arrow keys (and vim j/k); also manual scroll
                    KeyCode::Up | KeyCode::Char('k') => {
                        if matches!(state.view, AppView::Groups | AppView::Remote { .. })
                            && state.cursor > 0
                        {
                            state.cursor -= 1;
                        } else if matches!(state.view, AppView::Manual) {
                            state.manual_scroll = state.manual_scroll.saturating_sub(1);
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if matches!(state.view, AppView::Groups | AppView::Remote { .. })
                            && state.cursor + 1 < state.entries.len()
                        {
                            state.cursor += 1;
                        } else if matches!(state.view, AppView::Manual) {
                            state.manual_scroll = state.manual_scroll.saturating_add(1);
                        }
                    }
                    // Metric cycling for the active side (left/right arrows)
                    KeyCode::Left => {
                        if matches!(state.view, AppView::Groups | AppView::Remote { .. }) {
                            match state.active_side {
                                Side::Left => {
                                    state.left_metric = state.left_metric.cycle_prev(is_local)
                                }
                                Side::Right => {
                                    state.right_metric = state.right_metric.cycle_prev(is_local)
                                }
                            }
                        }
                    }
                    KeyCode::Right => {
                        if matches!(state.view, AppView::Groups | AppView::Remote { .. }) {
                            match state.active_side {
                                Side::Left => {
                                    state.left_metric = state.left_metric.cycle_next(is_local)
                                }
                                Side::Right => {
                                    state.right_metric = state.right_metric.cycle_next(is_local)
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
                    // Enter: drill down — threads (local), SSH daemon (proxmox)
                    KeyCode::Enter => {
                        if matches!(state.view, AppView::Groups) {
                            if let Some(e) = state.entries.get(state.cursor) {
                                let label = e.label.clone();
                                if matches!(state.mode, AppMode::Local) {
                                    // Local: open per-thread heat-map view.
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
                                    // Proxmox mode: connect over SSH to remote apptop --daemon.
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
                                    state.view = AppView::Connecting { label: label.clone() };
                                    // Force-render the connecting screen before blocking on SSH.
                                    terminal.draw(|f| ui::render(f, &state))?;
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
                                        }
                                        Err(diag) => {
                                            // Connection failed; show diagnostics in error bar.
                                            state.view = AppView::Groups;
                                            state.error = Some(diag.join("\n"));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Restore the terminal: leave alternate screen, disable raw mode, show cursor.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
