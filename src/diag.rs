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

//! `diag` — aerie's **Instruments** subsystem: *probes* that produce time-series
//! and *analyzers* that find structure in them.
//!
//! The first probe is [`LatencyProbe`], a built-in `cyclictest`: a dedicated
//! thread asks the OS to wake it every `tick` and records how *late* each wakeup
//! actually was. That overshoot series is the scheduling jitter that makes every
//! realtime UI on the machine stutter at the same cadence — captured
//! independently of any one application, so it isolates a *system* cause from a
//! per-app one.
//!
//! The module is deliberately self-contained and grows by adding probes (CPU
//! frequency / RAPL, IRQ counts, per-process spectral) and analyzers
//! (periodicity, …) behind the same seams, rather than by threading new fields
//! through the rest of aerie.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// One wakeup-latency sample produced by [`LatencyProbe`].
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    /// Seconds since the probe started.
    pub t: f64,
    /// How much longer than the requested tick this wakeup actually took, in
    /// milliseconds. `0.0` means the wakeup was on time; spikes are the
    /// scheduling stalls we are hunting.
    pub overshoot_ms: f32,
}

/// Summary statistics over a window of [`Sample`]s.
///
/// Computed by [`stats`] from a borrowed slice so the same snapshot can feed both
/// the readout and (later) the periodicity analyzer without re-locking the ring.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeStats {
    /// Number of samples in the window.
    pub count: usize,
    /// The probe's requested tick, in milliseconds (for context next to the spikes).
    pub tick_ms: f32,
    /// Most recent sample's overshoot.
    pub last_ms: f32,
    /// Mean overshoot across the window.
    pub mean_ms: f32,
    /// 99th-percentile overshoot — the "typical worst" stall.
    pub p99_ms: f32,
    /// Largest overshoot seen in the window.
    pub max_ms: f32,
    /// Wall-clock seconds the window spans (newest.t − oldest.t).
    pub window_s: f64,
}

/// Configuration for [`LatencyProbe`].
#[derive(Clone, Copy, Debug)]
pub struct ProbeConfig {
    /// How long the probe asks to sleep between wakeups. Smaller = finer
    /// temporal resolution but more wakeups. 2 ms (500 Hz) resolves stalls well
    /// below one render frame while costing almost no CPU.
    pub tick: Duration,
    /// Maximum samples retained in the ring buffer. Oldest are dropped first.
    pub capacity: usize,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        // 2 ms tick → 500 Hz. 60_000 samples ≈ 120 s of rolling history
        // (~0.9 MB), enough for the analyzer to resolve periods up to tens of
        // seconds while staying cheap.
        Self { tick: Duration::from_millis(2), capacity: 60_000 }
    }
}

/// State shared between the probe thread (writer) and the UI thread (reader).
struct Shared {
    ring: VecDeque<Sample>,
    capacity: usize,
}

/// A built-in `cyclictest`: a thread that measures its own wakeup latency.
///
/// Spawn with [`spawn`](Self::spawn); read with [`snapshot`](Self::snapshot).
/// The thread runs until the probe is dropped. Measuring the *probe thread's*
/// own scheduling delay is exactly the right signal: it is subject to the same
/// system-wide preemption that stalls every other realtime thread, so its
/// overshoot series mirrors the stutter the user sees — without depending on any
/// particular application's event loop.
pub struct LatencyProbe {
    shared: Arc<Mutex<Shared>>,
    tick: Duration,
    stop: Arc<AtomicBool>,
    _handle: JoinHandle<()>,
}

impl LatencyProbe {
    /// Spawn the probe thread with the given configuration.
    pub fn spawn(cfg: ProbeConfig) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            ring: VecDeque::with_capacity(cfg.capacity.min(4096)),
            capacity: cfg.capacity.max(1),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let tick = cfg.tick;

        let shared_t = Arc::clone(&shared);
        let stop_t = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("aerie-latency-probe".into())
            .spawn(move || probe_loop(shared_t, stop_t, tick))
            .expect("spawn latency probe thread");

        Self { shared, tick, stop, _handle: handle }
    }

    /// The probe's requested tick in milliseconds.
    pub fn tick_ms(&self) -> f32 {
        self.tick.as_secs_f32() * 1000.0
    }

    /// Copy the current ring contents (oldest → newest) for analysis/rendering.
    ///
    /// One clone under the lock keeps the probe thread's critical section short;
    /// callers then compute [`stats`] and (later) periodicity off-lock.
    pub fn snapshot(&self) -> Vec<Sample> {
        let g = self.shared.lock().unwrap();
        g.ring.iter().copied().collect()
    }

    /// Maximum overshoot among samples newer than `after_t` (probe seconds),
    /// plus the newest sample's `t`. Used by the attributor to tag each of its
    /// own intervals with "did a stall happen in this window?" without copying
    /// the whole ring. Returns `(0.0, after_t)` when no newer samples exist.
    pub fn max_overshoot_after(&self, after_t: f64) -> (f32, f64) {
        let g = self.shared.lock().unwrap();
        let mut max = 0.0f32;
        let mut newest = after_t;
        for s in g.ring.iter().rev() {
            if s.t <= after_t {
                break; // ring is time-ordered; everything earlier is older
            }
            if s.t > newest {
                newest = s.t;
            }
            if s.overshoot_ms > max {
                max = s.overshoot_ms;
            }
        }
        (max, newest)
    }
}

impl Drop for LatencyProbe {
    fn drop(&mut self) {
        // Signal the thread to exit; we don't join (it may be mid-sleep for up to
        // `tick`), letting the process tear it down. The flag keeps a long-lived
        // probe from leaking a busy thread if the probe is ever dropped early.
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The probe thread body: sleep `tick`, measure overshoot, record, repeat.
fn probe_loop(shared: Arc<Mutex<Shared>>, stop: Arc<AtomicBool>, tick: Duration) {
    let start = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let t0 = Instant::now();
        std::thread::sleep(tick);
        let elapsed = t0.elapsed();
        // Overshoot = (actual elapsed) − (requested tick). A perfectly scheduled
        // wakeup gives ~0; the small steady baseline is timer granularity, and
        // anything well above it is a scheduling stall.
        let overshoot = elapsed.saturating_sub(tick);
        let sample = Sample {
            t: t0.duration_since(start).as_secs_f64(),
            overshoot_ms: overshoot.as_secs_f32() * 1000.0,
        };
        let mut g = shared.lock().unwrap();
        if g.ring.len() >= g.capacity {
            g.ring.pop_front();
        }
        g.ring.push_back(sample);
    }
}

// ── Pressure probe (instrument #2) ─────────────────────────────────────────

/// One system-pressure sample: the signals that move when the machine stalls in
/// ways a CPU-wakeup probe can't see (a compositor blocking on memory refault or
/// GPU work, the run queue spiking). Rates are per wall-second over the tick.
#[derive(Clone, Copy, Debug, Default)]
pub struct PressureSample {
    /// Seconds since the probe started.
    pub t: f64,
    /// Run-queue depth — `procs_running` from /proc/stat.
    pub run_q: f32,
    /// CPU PSI "some" stall, microseconds per second.
    pub cpu_us_s: f32,
    /// Memory PSI "some" stall, microseconds per second.
    pub mem_us_s: f32,
    /// I/O PSI "some" stall, microseconds per second.
    pub io_us_s: f32,
}

/// Which channel of a [`PressureSample`] to analyse / display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressureChannel {
    RunQueue,
    CpuStall,
    MemStall,
    IoStall,
}

impl PressureChannel {
    pub const ALL: [PressureChannel; 4] = [
        PressureChannel::RunQueue,
        PressureChannel::CpuStall,
        PressureChannel::MemStall,
        PressureChannel::IoStall,
    ];
    pub fn label(self) -> &'static str {
        match self {
            PressureChannel::RunQueue => "run-queue",
            PressureChannel::CpuStall => "CPU stall",
            PressureChannel::MemStall => "memory stall",
            PressureChannel::IoStall => "I/O stall",
        }
    }
    pub fn unit(self) -> &'static str {
        match self {
            PressureChannel::RunQueue => "procs",
            _ => "µs/s",
        }
    }
    pub fn value(self, s: &PressureSample) -> f32 {
        match self {
            PressureChannel::RunQueue => s.run_q,
            PressureChannel::CpuStall => s.cpu_us_s,
            PressureChannel::MemStall => s.mem_us_s,
            PressureChannel::IoStall => s.io_us_s,
        }
    }
}

struct PressureShared {
    ring: VecDeque<PressureSample>,
    capacity: usize,
}

/// Samples system-wide stall indicators on a dedicated thread. Companion to
/// [`LatencyProbe`]: where that measures CPU scheduling latency, this measures the
/// pressure signals whose periodicity reveals compositor/memory-bound freezes.
pub struct PressureProbe {
    shared: Arc<Mutex<PressureShared>>,
    stop: Arc<AtomicBool>,
    _handle: JoinHandle<()>,
}

impl PressureProbe {
    /// Spawn the pressure-sampling thread. `tick` ~20 ms (50 Hz) resolves the
    /// few-Hz rhythms typical of a thrash/refault stall cheaply.
    pub fn spawn(tick: Duration, capacity: usize) -> Self {
        let shared = Arc::new(Mutex::new(PressureShared {
            ring: VecDeque::with_capacity(capacity.min(4096)),
            capacity: capacity.max(1),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let shared_t = Arc::clone(&shared);
        let stop_t = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("aerie-pressure-probe".into())
            .spawn(move || pressure_loop(shared_t, stop_t, tick))
            .expect("spawn pressure probe thread");
        Self { shared, stop, _handle: handle }
    }

    pub fn snapshot(&self) -> Vec<PressureSample> {
        let g = self.shared.lock().unwrap();
        g.ring.iter().copied().collect()
    }
}

impl Drop for PressureProbe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Map one channel of a pressure series into the generic [`Sample`] shape so the
/// shared periodicity analyzer and stats can run on it unchanged.
pub fn pressure_channel_series(samples: &[PressureSample], ch: PressureChannel) -> Vec<Sample> {
    samples.iter().map(|s| Sample { t: s.t, overshoot_ms: ch.value(s) }).collect()
}

fn pressure_loop(shared: Arc<Mutex<PressureShared>>, stop: Arc<AtomicBool>, tick: Duration) {
    let start = Instant::now();
    let mut prev: Option<(u64, u64, u64, Instant)> = None; // psi cpu/mem/io totals + when
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(tick);
        let now = Instant::now();
        let run_q = read_procs_running();
        let cpu = read_psi_some_total("/proc/pressure/cpu").unwrap_or(0);
        let mem = read_psi_some_total("/proc/pressure/memory").unwrap_or(0);
        let io = read_psi_some_total("/proc/pressure/io").unwrap_or(0);
        if let Some((pc, pm, pi, pt)) = prev {
            let dt = now.duration_since(pt).as_secs_f64().max(1e-4);
            let rate = |c: u64, p: u64| (c.saturating_sub(p) as f64 / dt) as f32;
            let sample = PressureSample {
                t: now.duration_since(start).as_secs_f64(),
                run_q: run_q as f32,
                cpu_us_s: rate(cpu, pc),
                mem_us_s: rate(mem, pm),
                io_us_s: rate(io, pi),
            };
            let mut g = shared.lock().unwrap();
            if g.ring.len() >= g.capacity {
                g.ring.pop_front();
            }
            g.ring.push_back(sample);
        }
        prev = Some((cpu, mem, io, now));
    }
}

/// Read `procs_running` (run-queue depth) from /proc/stat.
fn read_procs_running() -> u64 {
    if let Ok(stat) = std::fs::read_to_string("/proc/stat") {
        for line in stat.lines() {
            if let Some(rest) = line.strip_prefix("procs_running ") {
                return rest.trim().parse().unwrap_or(0);
            }
        }
    }
    0
}

// ── Periodic-offender detector (instrument #3) ─────────────────────────────

/// One time step of a process group's activity (instrument #3). Opaque outside
/// `diag` — produced by [`OffenderProbe::snapshot`] and consumed by [`analyze_offenders`].
#[derive(Clone, Copy, Debug, Default)]
pub struct GroupActivity {
    t: f64,
    /// CPU jiffies used by the group in this interval (shape, not absolute %).
    cpu: f32,
    /// Short-lived children spawned *by* this group in this interval.
    spawns: f32,
}

/// What kind of periodic behaviour an offender exhibits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OffenderKind {
    /// Periodic CPU bursts (e.g. a timer callback doing work on the main loop).
    CpuBurst,
    /// Periodically spawning short-lived helpers (poll-on-a-timer; the Astra/lsblk pattern).
    Spawns,
}

/// A process group whose activity is periodic — the automated version of "find the
/// thing doing something every N seconds".
#[derive(Clone, Debug)]
pub struct Offender {
    pub group: String,
    pub kind: OffenderKind,
    pub period_s: f64,
    pub freq_hz: f64,
    pub confidence: f32,
    /// Representative spawned child comm (for `Spawns`).
    pub child: Option<String>,
    /// Mean spawns/interval (Spawns) or mean CPU jiffies/interval (CpuBurst).
    pub rate: f32,
}

#[derive(Clone, Debug, Default)]
pub struct OffenderReport {
    pub offenders: Vec<Offender>,
}

struct OffShared {
    groups: HashMap<String, VecDeque<GroupActivity>>,
    /// Representative recent child comm per spawning group.
    children: HashMap<String, String>,
    cap: usize,
    max_groups: usize,
}

/// Scans /proc on a dedicated thread, tracking per-group CPU and spawn activity so
/// [`analyze_offenders`] can find process groups behaving on a clock — the class of
/// fault that froze the desktop here (a periodic extension blocking the compositor).
pub struct OffenderProbe {
    shared: Arc<Mutex<OffShared>>,
    stop: Arc<AtomicBool>,
    _handle: JoinHandle<()>,
}

impl OffenderProbe {
    pub fn spawn(tick: Duration) -> Self {
        let shared = Arc::new(Mutex::new(OffShared {
            groups: HashMap::new(),
            children: HashMap::new(),
            cap: 300,        // ~60 s at 5 Hz
            max_groups: 64,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let shared_t = Arc::clone(&shared);
        let stop_t = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("aerie-offender-probe".into())
            .spawn(move || offender_loop(shared_t, stop_t, tick))
            .expect("spawn offender probe thread");
        Self { shared, stop, _handle: handle }
    }

    /// Clone the per-group activity series and representative children for analysis.
    pub fn snapshot(&self) -> (HashMap<String, Vec<GroupActivity>>, HashMap<String, String>) {
        let g = self.shared.lock().unwrap();
        let groups = g.groups.iter().map(|(k, v)| (k.clone(), v.iter().copied().collect())).collect();
        (groups, g.children.clone())
    }
}

impl Drop for OffenderProbe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Read `(comm, ppid, utime+stime)` from /proc/PID/stat. The comm field is wrapped
/// in parens and may contain spaces, so split on the last ')'.
fn read_pid_stat(pid: &str) -> Option<(String, u32, u64)> {
    let data = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let lp = data.find('(')?;
    let rp = data.rfind(')')?;
    let comm = data.get(lp + 1..rp)?.to_string();
    let rest: Vec<&str> = data.get(rp + 2..)?.split_whitespace().collect();
    // After comm: state(0) ppid(1) ... utime(11) stime(12) (0-based into `rest`).
    let ppid: u32 = rest.get(1)?.parse().ok()?;
    let utime: u64 = rest.get(11)?.parse().ok()?;
    let stime: u64 = rest.get(12)?.parse().ok()?;
    Some((comm, ppid, utime + stime))
}

/// Scan every numeric /proc entry once: pid → (comm, ppid, jiffies).
fn scan_procs() -> HashMap<u32, (String, u32, u64)> {
    let mut map = HashMap::new();
    let Ok(rd) = std::fs::read_dir("/proc") else { return map };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Some((comm, ppid, j)) = read_pid_stat(name) {
            if let Ok(pid) = name.parse::<u32>() {
                map.insert(pid, (comm, ppid, j));
            }
        }
    }
    map
}

fn offender_loop(shared: Arc<Mutex<OffShared>>, stop: Arc<AtomicBool>, tick: Duration) {
    let start = Instant::now();
    let mut prev: HashMap<u32, (String, u32, u64)> = scan_procs();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(tick);
        let t = Instant::now().duration_since(start).as_secs_f64();
        let cur = scan_procs();

        let mut cpu: HashMap<String, f64> = HashMap::new();
        let mut spawns: HashMap<String, u32> = HashMap::new();
        let mut child_of: HashMap<String, String> = HashMap::new();
        for (pid, (comm, ppid, j)) in &cur {
            match prev.get(pid) {
                Some((_, _, pj)) => {
                    *cpu.entry(comm.clone()).or_default() += j.saturating_sub(*pj) as f64;
                }
                None => {
                    // A pid we didn't see last tick = newly spawned. Blame the parent
                    // group (the spawner is the offender, not the short-lived child).
                    if let Some((pcomm, _, _)) = cur.get(ppid) {
                        *spawns.entry(pcomm.clone()).or_default() += 1;
                        child_of.entry(pcomm.clone()).or_insert_with(|| comm.clone());
                    }
                    *cpu.entry(comm.clone()).or_default() += *j as f64;
                }
            }
        }

        let mut g = shared.lock().unwrap();
        let cap = g.cap;
        // Append this tick to every tracked group, plus any newly active one,
        // pushing zeros for tracked-but-idle groups so each series stays uniform.
        let active: std::collections::HashSet<String> =
            cpu.keys().chain(spawns.keys()).cloned().collect();
        let tracked: std::collections::HashSet<String> = g.groups.keys().cloned().collect();
        for grp in active.union(&tracked) {
            let sample = GroupActivity {
                t,
                cpu: *cpu.get(grp).unwrap_or(&0.0) as f32,
                spawns: *spawns.get(grp).unwrap_or(&0) as f32,
            };
            let ring = g.groups.entry(grp.clone()).or_default();
            if ring.len() >= cap {
                ring.pop_front();
            }
            ring.push_back(sample);
        }
        for (grp, ch) in child_of {
            g.children.insert(grp, ch);
        }
        // Prune all-idle groups, then cap the tracked set by recent activity.
        g.groups.retain(|_, ring| ring.iter().any(|a| a.cpu > 0.0 || a.spawns > 0.0));
        if g.groups.len() > g.max_groups {
            let mut by_activity: Vec<(String, f32)> = g.groups.iter()
                .map(|(k, v)| (k.clone(), v.iter().map(|a| a.cpu + a.spawns * 50.0).sum()))
                .collect();
            by_activity.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let keep: std::collections::HashSet<String> =
                by_activity.into_iter().take(g.max_groups).map(|(k, _)| k).collect();
            g.groups.retain(|k, _| keep.contains(k));
        }
        drop(g);
        prev = cur;
    }
}

/// Find process groups whose CPU or spawn activity is periodic. Spawn periodicity
/// (a clean impulse train) is preferred over CPU when both fire, since it is the
/// more actionable signal (poll-on-a-timer). Confidence comes from the autocorrelation.
pub fn analyze_offenders(
    groups: &HashMap<String, Vec<GroupActivity>>,
    children: &HashMap<String, String>,
) -> OffenderReport {
    const MIN_CONF: f32 = 0.25;
    let mut offenders = Vec::new();
    for (grp, series) in groups {
        if series.len() < 24 {
            continue;
        }
        let cpu_series: Vec<Sample> =
            series.iter().map(|a| Sample { t: a.t, overshoot_ms: a.cpu }).collect();
        let spawn_series: Vec<Sample> =
            series.iter().map(|a| Sample { t: a.t, overshoot_ms: a.spawns }).collect();
        let cfg = AnalysisConfig::default();
        let cpu_p = analyze_periodicity(&cpu_series, cfg);
        let spawn_p = analyze_periodicity(&spawn_series, cfg);

        // Prefer the spawn signal when it is at least as confident.
        let (kind, p) = match (spawn_p.period_s, cpu_p.period_s) {
            (Some(_), Some(_)) if spawn_p.confidence >= cpu_p.confidence =>
                (OffenderKind::Spawns, spawn_p),
            (Some(_), Some(_)) => (OffenderKind::CpuBurst, cpu_p),
            (Some(_), None) => (OffenderKind::Spawns, spawn_p),
            (None, Some(_)) => (OffenderKind::CpuBurst, cpu_p),
            (None, None) => continue,
        };
        if p.confidence < MIN_CONF {
            continue;
        }
        let rate = match kind {
            OffenderKind::Spawns => mean(&spawn_series.iter().map(|s| s.overshoot_ms).collect::<Vec<_>>()),
            OffenderKind::CpuBurst => mean(&cpu_series.iter().map(|s| s.overshoot_ms).collect::<Vec<_>>()),
        };
        offenders.push(Offender {
            group: grp.clone(),
            kind,
            period_s: p.period_s.unwrap(),
            freq_hz: p.freq_hz.unwrap_or(0.0),
            confidence: p.confidence,
            child: if kind == OffenderKind::Spawns { children.get(grp).cloned() } else { None },
            rate,
        });
    }
    offenders.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));
    offenders.truncate(6);
    OffenderReport { offenders }
}

/// Compute [`ProbeStats`] over a slice of samples (oldest → newest).
///
/// `tick_ms` is carried through for display context. Percentile uses the
/// nearest-rank method on a sorted copy of the overshoots.
pub fn stats(samples: &[Sample], tick_ms: f32) -> ProbeStats {
    if samples.is_empty() {
        return ProbeStats { tick_ms, ..Default::default() };
    }
    let count = samples.len();
    let mut sum = 0.0f64;
    let mut max = 0.0f32;
    for s in samples {
        sum += s.overshoot_ms as f64;
        if s.overshoot_ms > max {
            max = s.overshoot_ms;
        }
    }
    let mean = (sum / count as f64) as f32;

    let mut sorted: Vec<f32> = samples.iter().map(|s| s.overshoot_ms).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // Nearest-rank p99: index ceil(0.99 · n) − 1, clamped into range.
    let rank = ((0.99 * count as f64).ceil() as usize).clamp(1, count) - 1;
    let p99 = sorted[rank];

    let window_s = samples[count - 1].t - samples[0].t;
    ProbeStats {
        count,
        tick_ms,
        last_ms: samples[count - 1].overshoot_ms,
        mean_ms: mean,
        p99_ms: p99,
        max_ms: max,
        window_s,
    }
}

// ── Periodicity analyzer ──────────────────────────────────────────────────

/// Configuration for [`analyze_periodicity`].
#[derive(Clone, Copy, Debug)]
pub struct AnalysisConfig {
    /// Lowest frequency considered, Hz (longest detectable period = 1/freq_lo).
    pub freq_lo: f64,
    /// Highest frequency considered, Hz.
    pub freq_hi: f64,
    /// Number of frequency bins in the rendered spectrum.
    pub freq_bins: usize,
}

impl Default for AnalysisConfig {
    fn default() -> Self {
        // 0.05 Hz (20 s period) up to 25 Hz covers the band where a periodic
        // system stall plausibly lives — from slow timers to fast IRQ storms.
        Self { freq_lo: 0.05, freq_hi: 25.0, freq_bins: 240 }
    }
}

/// Result of periodicity analysis over a latency series.
#[derive(Clone, Debug, Default)]
pub struct Periodicity {
    /// Best period estimate in seconds (from autocorrelation), if one stands out.
    pub period_s: Option<f64>,
    /// Corresponding frequency in Hz.
    pub freq_hz: Option<f64>,
    /// Strength of the autocorrelation peak in `[0, 1]` — how cleanly periodic.
    /// Roughly: < 0.2 noise, 0.2–0.4 weak/quasi-periodic, > 0.4 strong.
    pub confidence: f32,
    /// Normalised power per frequency bin (log-spaced `freq_lo`→`freq_hi`), for the plot.
    pub spectrum: Vec<f32>,
    /// Frequency band actually analysed (for axis labels).
    pub freq_lo: f64,
    pub freq_hi: f64,
    /// Bin width used to resample the series, in seconds (diagnostic).
    pub bin_dt: f64,
}

/// Resample an (approximately uniform but jittered) sample series onto a strictly
/// uniform time grid of step `bin_dt`, taking the **max** overshoot in each bin so
/// a single-tick spike is never averaged away. Empty bins are 0.
fn resample_uniform(samples: &[Sample], bin_dt: f64) -> Vec<f32> {
    if samples.len() < 2 || bin_dt <= 0.0 {
        return Vec::new();
    }
    let t0 = samples[0].t;
    let span = samples[samples.len() - 1].t - t0;
    let n = ((span / bin_dt).floor() as usize) + 1;
    if n < 4 {
        return Vec::new();
    }
    let mut grid = vec![0.0f32; n];
    for s in samples {
        let b = (((s.t - t0) / bin_dt).floor() as usize).min(n - 1);
        if s.overshoot_ms > grid[b] {
            grid[b] = s.overshoot_ms;
        }
    }
    grid
}

/// Find the period of a latency series via autocorrelation, plus a narrow-band
/// DFT spectrum for display.
///
/// The latency series is resampled to a uniform grid, mean-subtracted, then:
///  - **autocorrelation** over the lag range implied by `[freq_lo, freq_hi]`
///    gives the dominant period — the first strong peak away from lag 0. This is
///    robust for the spiky, non-sinusoidal signal a periodic stall produces.
///  - a **log-spaced DFT** over the same band gives a renderable power spectrum
///    and a cross-check frequency.
pub fn analyze_periodicity(samples: &[Sample], cfg: AnalysisConfig) -> Periodicity {
    let mut out = Periodicity {
        freq_lo: cfg.freq_lo,
        freq_hi: cfg.freq_hi,
        spectrum: vec![0.0; cfg.freq_bins.max(1)],
        ..Default::default()
    };
    if samples.len() < 8 {
        return out;
    }

    // Choose a bin step: fine enough to resolve freq_hi (≥ 4 bins/period at the
    // top frequency via Nyquist headroom), but coarse enough to keep the grid
    // bounded for the O(n·lags) autocorrelation.
    let span = samples[samples.len() - 1].t - samples[0].t;
    // 4× oversampling above the top frequency keeps periods well-resolved (so a
    // period isn't a half-integer number of bins, which would alias energy onto a
    // harmonic), bounded by a grid-size cap for very long windows. Crucially it is
    // also floored at ~1.5× the data's own mean sample spacing: binning finer than
    // the source cadence leaves most bins empty, and that zero-padded comb injects
    // a spurious peak near the sampling rate. This matters for offline analysis of
    // capture logs (~tens of Hz) — the live probe (500 Hz) is unaffected.
    let mean_spacing = span / (samples.len() - 1) as f64;
    let bin_dt = (1.0 / (cfg.freq_hi * 4.0))
        .max(span / 6000.0)
        .max(mean_spacing * 1.5);
    out.bin_dt = bin_dt;

    let mut grid = resample_uniform(samples, bin_dt);
    let n = grid.len();
    if n < 16 {
        return out;
    }
    // Mean-subtract (detrend the DC component).
    let mean = grid.iter().map(|&v| v as f64).sum::<f64>() / n as f64;
    for v in &mut grid {
        *v -= mean as f32;
    }
    let energy: f64 = grid.iter().map(|&v| (v as f64) * (v as f64)).sum();
    if energy < 1e-12 {
        return out; // flat series, nothing periodic
    }

    // ── Autocorrelation over the feasible lag band ────────────────────────
    let lag_min = ((1.0 / cfg.freq_hi) / bin_dt).floor().max(1.0) as usize;
    // Cap longest period to a third of the window so we see ≥ 3 cycles.
    let lag_max = (((1.0 / cfg.freq_lo) / bin_dt).floor() as usize)
        .min(n / 3)
        .max(lag_min + 1);
    let mut corr = vec![0.0f64; lag_max + 1];
    for (lag, slot) in corr.iter_mut().enumerate().take(lag_max + 1).skip(lag_min) {
        let mut acc = 0.0f64;
        for i in 0..(n - lag) {
            acc += grid[i] as f64 * grid[i + lag] as f64;
        }
        *slot = acc / energy; // normalised: r(0) == 1
    }
    // Skip the *central lobe*. The autocorrelation always rises toward lag 0
    // because a stall spans several bins, so short-lag self-similarity is spike
    // *width*, not periodicity — reporting it (e.g. "every 0.25 s") is wrong. Start
    // searching past the first point where the correlation falls to zero.
    let search_from = corr
        .iter()
        .enumerate()
        .take(lag_max)
        .skip(lag_min)
        .find(|(_, &r)| r <= 0.0)
        .map(|(lag, _)| lag)
        .unwrap_or(lag_min);
    // Strongest correlation beyond the central lobe is the candidate period (or a
    // harmonic of it). A spike train peaks at every multiple of the period with
    // near-equal height, so fold to the *smallest* lag that reaches most of that
    // height — the fundamental.
    let best_corr = (search_from..=lag_max).map(|l| corr[l]).fold(0.0f64, f64::max);
    // Require a genuine peak: below this it's noise and we honestly report "none".
    const PERIOD_MIN_CORR: f64 = 0.20;
    if best_corr > PERIOD_MIN_CORR {
        let thresh = best_corr * 0.9;
        let fundamental = (search_from..=lag_max).find(|&lag| corr[lag] >= thresh).unwrap_or(0);
        if fundamental > 0 {
            let period = fundamental as f64 * bin_dt;
            out.period_s = Some(period);
            out.freq_hz = Some(1.0 / period);
            out.confidence = best_corr.clamp(0.0, 1.0) as f32;
        }
    }

    // ── Log-spaced DFT for the displayed spectrum ─────────────────────────
    let bins = cfg.freq_bins.max(1);
    let ln_lo = cfg.freq_lo.ln();
    let ln_hi = cfg.freq_hi.ln();
    let mut max_power = 0.0f64;
    for k in 0..bins {
        let frac = if bins > 1 { k as f64 / (bins - 1) as f64 } else { 0.0 };
        let f = (ln_lo + (ln_hi - ln_lo) * frac).exp();
        let w = 2.0 * std::f64::consts::PI * f * bin_dt;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &v) in grid.iter().enumerate() {
            let ang = w * i as f64;
            re += v as f64 * ang.cos();
            im -= v as f64 * ang.sin();
        }
        let power = re * re + im * im;
        out.spectrum[k] = power as f32;
        if power > max_power {
            max_power = power;
        }
    }
    if max_power > 0.0 {
        for p in &mut out.spectrum {
            *p = (*p as f64 / max_power) as f32;
        }
    }

    out
}

// ── Spike attribution ─────────────────────────────────────────────────────

/// Raw cumulative system counters read from /proc, for delta computation.
///
/// All fields are best-effort: a missing kernel feature (e.g. PSI) leaves the
/// corresponding field `None`/0 and that signal simply never accuses anyone.
#[derive(Clone, Copy, Debug, Default)]
struct Counters {
    /// /proc/stat aggregate jiffies.
    total: u64,
    system: u64,
    irq: u64,
    softirq: u64,
    iowait: u64,
    /// /proc/pressure "some" cumulative stall, microseconds.
    psi_cpu_us: Option<u64>,
    psi_io_us: Option<u64>,
    psi_mem_us: Option<u64>,
    /// /proc/interrupts grand total.
    irq_count: u64,
    /// RAPL package energy, microjoules (wraps; use wrapping_sub).
    rapl_uj: Option<u64>,
}

/// Per-interval signal values (rates / fractions) derived from two [`Counters`].
///
/// These are the columns the attributor compares between stall intervals and
/// calm intervals. Field order mirrors [`SIGNALS`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Signals {
    /// Kernel CPU time as a fraction of total CPU over the interval [0, 1].
    pub sys_frac: f32,
    /// IRQ + softirq time as a fraction of total CPU [0, 1].
    pub irq_frac: f32,
    /// iowait as a fraction of total CPU [0, 1].
    pub iowait_frac: f32,
    /// CPU pressure: microseconds of "some" stall per wall second.
    pub psi_cpu_us_s: f32,
    /// I/O pressure: microseconds of "some" stall per wall second.
    pub psi_io_us_s: f32,
    /// Memory pressure: microseconds of "some" stall per wall second.
    pub psi_mem_us_s: f32,
    /// Hardware interrupts per wall second.
    pub irq_rate: f32,
    /// Mean power over the interval, watts (RAPL).
    pub power_w: f32,
}

/// Descriptor for one attributable signal: how to pull it out of [`Signals`],
/// what to call it, its unit, and a one-line interpretation of a high value.
pub struct SignalDesc {
    pub name: &'static str,
    pub unit: &'static str,
    pub hint: &'static str,
    pub get: fn(&Signals) -> f32,
}

/// The signals the attributor ranks, with human-readable interpretations.
pub const SIGNALS: &[SignalDesc] = &[
    SignalDesc { name: "IRQ/softirq CPU", unit: "%cpu", hint: "interrupt or softirq storm (NIC, timer, USB)", get: |s| s.irq_frac * 100.0 },
    SignalDesc { name: "interrupt rate", unit: "/s", hint: "a device firing interrupts in bursts", get: |s| s.irq_rate },
    SignalDesc { name: "I/O pressure", unit: "µs/s", hint: "disk writeback / fsync stalling tasks", get: |s| s.psi_io_us_s },
    SignalDesc { name: "iowait", unit: "%cpu", hint: "blocking on disk or other I/O", get: |s| s.iowait_frac * 100.0 },
    SignalDesc { name: "memory pressure", unit: "µs/s", hint: "reclaim / swap thrash stalling tasks", get: |s| s.psi_mem_us_s },
    SignalDesc { name: "CPU pressure", unit: "µs/s", hint: "runnable threads waiting for a core", get: |s| s.psi_cpu_us_s },
    SignalDesc { name: "kernel CPU", unit: "%cpu", hint: "time in syscalls / driver code", get: |s| s.sys_frac * 100.0 },
    SignalDesc { name: "power draw", unit: "W", hint: "frequency/C-state transition around the stall", get: |s| s.power_w },
];

/// One attribution interval: the worst probe overshoot seen during it, plus the
/// system signals over the same window.
#[derive(Clone, Copy, Debug)]
struct Interval {
    max_overshoot_ms: f32,
    signals: Signals,
}

/// A ranked suspect: a signal that is reliably elevated during stall intervals.
#[derive(Clone, Debug)]
pub struct Suspect {
    pub name: &'static str,
    pub unit: &'static str,
    pub hint: &'static str,
    /// Mean of this signal across stall intervals.
    pub spike_mean: f32,
    /// Mean across calm intervals.
    pub base_mean: f32,
    /// Separation score: (spike_mean − base_mean) / (base_std + ε). Higher =
    /// more reliably tied to the stalls rather than to background noise.
    pub score: f32,
}

/// Outcome of [`Attributor::rank`].
#[derive(Clone, Debug, Default)]
pub struct AttribReport {
    pub spike_count: usize,
    pub base_count: usize,
    pub threshold_ms: f32,
    /// Suspects ordered most → least implicated.
    pub suspects: Vec<Suspect>,
}

/// Drives spike attribution from the main loop: each tick it reads cheap /proc
/// counters, deltas them, tags the interval with the worst probe overshoot since
/// the last tick, and accumulates a ring of intervals. [`rank`](Self::rank) then
/// asks which signals are elevated when stalls happen.
pub struct Attributor {
    prev: Option<Counters>,
    last_probe_t: f64,
    intervals: VecDeque<Interval>,
    cap: usize,
}

impl Default for Attributor {
    fn default() -> Self {
        // ~20 Hz × 120 s ≈ 2400 intervals.
        Self { prev: None, last_probe_t: 0.0, intervals: VecDeque::new(), cap: 2400 }
    }
}

impl Attributor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read counters, compute the interval since the last call, and record it.
    /// `wall_dt` is the real time elapsed since the previous sample, in seconds.
    ///
    /// Returns the recorded interval's worst probe overshoot (ms), or `None` when
    /// this call only primed the counters (first tick / after a gap). Used by the
    /// capture log to emit one downsampled latency sample per tick.
    pub fn sample(&mut self, probe: &LatencyProbe, wall_dt: f64) -> Option<f32> {
        let cur = read_counters();
        let (max_overshoot, newest) = probe.max_overshoot_after(self.last_probe_t);
        self.last_probe_t = newest;

        let recorded = if let Some(prev) = self.prev {
            if wall_dt > 1e-4 {
                let signals = derive_signals(&prev, &cur, wall_dt);
                if self.intervals.len() >= self.cap {
                    self.intervals.pop_front();
                }
                self.intervals.push_back(Interval { max_overshoot_ms: max_overshoot, signals });
                Some(max_overshoot)
            } else {
                None
            }
        } else {
            None
        };
        self.prev = Some(cur);
        recorded
    }

    /// Rank signals by how reliably they are elevated during stall intervals
    /// (overshoot ≥ `threshold_ms`) versus calm ones.
    pub fn rank(&self, threshold_ms: f32) -> AttribReport {
        let mut report = AttribReport { threshold_ms, ..Default::default() };
        let (spikes, calm): (Vec<&Interval>, Vec<&Interval>) = self
            .intervals
            .iter()
            .partition(|iv| iv.max_overshoot_ms >= threshold_ms);
        report.spike_count = spikes.len();
        report.base_count = calm.len();
        // Need a few of each to say anything trustworthy.
        if spikes.len() < 3 || calm.len() < 3 {
            return report;
        }

        for desc in SIGNALS {
            let spike_vals: Vec<f32> = spikes.iter().map(|iv| (desc.get)(&iv.signals)).collect();
            let base_vals: Vec<f32> = calm.iter().map(|iv| (desc.get)(&iv.signals)).collect();
            let spike_mean = mean(&spike_vals);
            let base_mean = mean(&base_vals);
            let base_std = std_dev(&base_vals, base_mean);
            // Only accuse a signal that is genuinely higher during stalls.
            if spike_mean <= base_mean {
                continue;
            }
            let score = (spike_mean - base_mean) / (base_std + 1e-3);
            // Require clear separation from baseline noise.
            if score < 1.5 {
                continue;
            }
            report.suspects.push(Suspect {
                name: desc.name,
                unit: desc.unit,
                hint: desc.hint,
                spike_mean,
                base_mean,
                score,
            });
        }
        report.suspects.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        report
    }
}

fn mean(v: &[f32]) -> f32 {
    if v.is_empty() { 0.0 } else { v.iter().sum::<f32>() / v.len() as f32 }
}

fn std_dev(v: &[f32], mean: f32) -> f32 {
    if v.len() < 2 { return 0.0; }
    let var = v.iter().map(|&x| { let d = x - mean; d * d }).sum::<f32>() / (v.len() - 1) as f32;
    var.max(0.0).sqrt()
}

/// Convert two cumulative counter snapshots into per-interval [`Signals`].
fn derive_signals(prev: &Counters, cur: &Counters, wall_dt: f64) -> Signals {
    let dtotal = cur.total.saturating_sub(prev.total) as f32;
    let frac = |a: u64, b: u64| if dtotal > 0.0 { a.saturating_sub(b) as f32 / dtotal } else { 0.0 };
    let per_s = |cur: Option<u64>, prev: Option<u64>| -> f32 {
        match (cur, prev) {
            (Some(c), Some(p)) => (c.saturating_sub(p) as f64 / wall_dt) as f32,
            _ => 0.0,
        }
    };
    let power_w = match (cur.rapl_uj, prev.rapl_uj) {
        // RAPL counter wraps; wrapping_sub recovers the delta across a wrap.
        (Some(c), Some(p)) => (c.wrapping_sub(p) as f64 / 1e6 / wall_dt) as f32,
        _ => 0.0,
    };
    Signals {
        sys_frac: frac(cur.system, prev.system),
        irq_frac: frac(cur.irq.saturating_add(cur.softirq), prev.irq.saturating_add(prev.softirq)),
        iowait_frac: frac(cur.iowait, prev.iowait),
        psi_cpu_us_s: per_s(cur.psi_cpu_us, prev.psi_cpu_us),
        psi_io_us_s: per_s(cur.psi_io_us, prev.psi_io_us),
        psi_mem_us_s: per_s(cur.psi_mem_us, prev.psi_mem_us),
        irq_rate: if wall_dt > 0.0 { (cur.irq_count.saturating_sub(prev.irq_count) as f64 / wall_dt) as f32 } else { 0.0 },
        power_w: power_w.max(0.0),
    }
}

/// Read all the cheap /proc counters the attributor needs. Best-effort: any
/// unreadable source contributes zero/None and is silently ignored.
fn read_counters() -> Counters {
    let mut c = Counters::default();

    if let Ok(stat) = std::fs::read_to_string("/proc/stat") {
        if let Some(line) = stat.lines().find(|l| l.starts_with("cpu ")) {
            let f: Vec<u64> = line[3..].split_whitespace().filter_map(|s| s.parse().ok()).collect();
            // user nice system idle iowait irq softirq steal guest guest_nice
            c.total = f.iter().sum();
            c.system = f.get(2).copied().unwrap_or(0);
            c.iowait = f.get(4).copied().unwrap_or(0);
            c.irq = f.get(5).copied().unwrap_or(0);
            c.softirq = f.get(6).copied().unwrap_or(0);
        }
    }

    c.psi_cpu_us = read_psi_some_total("/proc/pressure/cpu");
    c.psi_io_us = read_psi_some_total("/proc/pressure/io");
    c.psi_mem_us = read_psi_some_total("/proc/pressure/memory");

    if let Ok(irqs) = std::fs::read_to_string("/proc/interrupts") {
        let mut total: u64 = 0;
        // Skip the header line (CPU0 CPU1 …); sum every integer token on the rest.
        for line in irqs.lines().skip(1) {
            for tok in line.split_whitespace() {
                if let Ok(n) = tok.parse::<u64>() {
                    total = total.saturating_add(n);
                }
            }
        }
        c.irq_count = total;
    }

    c.rapl_uj = read_rapl_uj();
    c
}

/// Parse the `some ... total=NNN` microsecond counter from a PSI pressure file.
fn read_psi_some_total(path: &str) -> Option<u64> {
    let data = std::fs::read_to_string(path).ok()?;
    let line = data.lines().find(|l| l.starts_with("some"))?;
    line.split_whitespace()
        .find_map(|tok| tok.strip_prefix("total=").and_then(|v| v.parse::<u64>().ok()))
}

/// Read the first available RAPL package energy counter, microjoules.
fn read_rapl_uj() -> Option<u64> {
    // Try the common package-0 paths; fall back to scanning powercap.
    for p in [
        "/sys/class/powercap/intel-rapl:0/energy_uj",
        "/sys/class/powercap/intel-rapl/intel-rapl:0/energy_uj",
    ] {
        if let Ok(s) = std::fs::read_to_string(p) {
            if let Ok(v) = s.trim().parse::<u64>() {
                return Some(v);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(series: &[(f64, f32)]) -> Vec<Sample> {
        series.iter().map(|&(t, o)| Sample { t, overshoot_ms: o }).collect()
    }

    fn iv(overshoot: f32, irq_frac: f32) -> Interval {
        Interval {
            max_overshoot_ms: overshoot,
            signals: Signals { irq_frac, ..Default::default() },
        }
    }

    #[test]
    fn attribution_finds_correlated_signal() {
        // 10 stall intervals with high IRQ fraction, 20 calm with low IRQ.
        let mut a = Attributor::new();
        for _ in 0..10 { a.intervals.push_back(iv(30.0, 0.40)); }
        for _ in 0..20 { a.intervals.push_back(iv(0.2, 0.02)); }
        let rep = a.rank(5.0);
        assert_eq!(rep.spike_count, 10);
        assert_eq!(rep.base_count, 20);
        let top = rep.suspects.first().expect("a suspect");
        assert_eq!(top.name, "IRQ/softirq CPU");
        assert!(top.spike_mean > top.base_mean);
    }

    #[test]
    fn offender_detects_periodic_spawner() {
        // 300 samples at 0.2 s (60 s). "poller" spawns one child every 3 s (15 ticks).
        let series: Vec<GroupActivity> = (0..300)
            .map(|i| GroupActivity {
                t: i as f64 * 0.2,
                cpu: 0.0,
                spawns: if i % 15 == 0 { 1.0 } else { 0.0 },
            })
            .collect();
        let mut groups = HashMap::new();
        groups.insert("poller".to_string(), series);
        let mut children = HashMap::new();
        children.insert("poller".to_string(), "lsblk".to_string());

        let rep = analyze_offenders(&groups, &children);
        let o = rep.offenders.first().expect("should flag the periodic spawner");
        assert_eq!(o.group, "poller");
        assert_eq!(o.kind, OffenderKind::Spawns);
        assert!((o.period_s - 3.0).abs() < 0.4, "period was {}", o.period_s);
        assert_eq!(o.child.as_deref(), Some("lsblk"));
    }

    #[test]
    fn offender_ignores_steady_group() {
        // Constant activity (no rhythm) must not be flagged.
        let series: Vec<GroupActivity> = (0..300)
            .map(|i| GroupActivity { t: i as f64 * 0.2, cpu: 5.0, spawns: 0.0 })
            .collect();
        let mut groups = HashMap::new();
        groups.insert("steady".to_string(), series);
        let rep = analyze_offenders(&groups, &HashMap::new());
        assert!(rep.offenders.is_empty(), "steady group should not be an offender");
    }

    #[test]
    fn attribution_quiet_when_uncorrelated() {
        // IRQ fraction identical in stall and calm intervals → no accusation.
        let mut a = Attributor::new();
        for _ in 0..10 { a.intervals.push_back(iv(30.0, 0.10)); }
        for _ in 0..20 { a.intervals.push_back(iv(0.2, 0.10)); }
        let rep = a.rank(5.0);
        assert!(rep.suspects.is_empty(), "should not accuse a flat signal");
    }

    /// Build a synthetic spike train: a baseline tick with a tall spike every
    /// `period_s`, sampled at `tick_s`.
    fn spike_train(tick_s: f64, period_s: f64, dur_s: f64) -> Vec<Sample> {
        let n = (dur_s / tick_s) as usize;
        let spike_every = (period_s / tick_s).round() as usize;
        (0..n)
            .map(|i| {
                let o = if spike_every > 0 && i % spike_every == 0 { 40.0 } else { 0.2 };
                Sample { t: i as f64 * tick_s, overshoot_ms: o }
            })
            .collect()
    }

    #[test]
    fn recovers_known_period() {
        // Spike every 2.0 s, 2 ms ticks, 60 s long.
        let s = spike_train(0.002, 2.0, 60.0);
        let p = analyze_periodicity(&s, AnalysisConfig::default());
        let period = p.period_s.expect("should find a period");
        assert!((period - 2.0).abs() < 0.1, "recovered period {period}");
        assert!(p.confidence > 0.4, "confidence {} too low", p.confidence);
    }

    #[test]
    fn recovers_fast_period() {
        // Spike every 0.25 s (4 Hz).
        let s = spike_train(0.002, 0.25, 30.0);
        let p = analyze_periodicity(&s, AnalysisConfig::default());
        let f = p.freq_hz.expect("should find a frequency");
        assert!((f - 4.0).abs() < 0.3, "recovered freq {f} Hz");
    }

    #[test]
    fn resample_keeps_spikes() {
        // A lone spike between two calm samples must survive max-binning.
        let s = mk(&[(0.0, 0.1), (0.05, 30.0), (0.10, 0.1), (0.15, 0.1), (0.20, 0.1)]);
        let grid = resample_uniform(&s, 0.05);
        assert!(grid.len() >= 4);
        assert_eq!(grid[1], 30.0, "spike must land in its bin");
        assert_eq!(grid[0], 0.1);
    }

    #[test]
    fn flat_series_has_no_period() {
        let s: Vec<Sample> =
            (0..10_000).map(|i| Sample { t: i as f64 * 0.002, overshoot_ms: 0.2 }).collect();
        let p = analyze_periodicity(&s, AnalysisConfig::default());
        assert!(p.period_s.is_none(), "flat series should not report a period");
    }

    #[test]
    fn stats_empty() {
        let s = stats(&[], 2.0);
        assert_eq!(s.count, 0);
        assert_eq!(s.tick_ms, 2.0);
        assert_eq!(s.max_ms, 0.0);
    }

    #[test]
    fn stats_basic() {
        // 100 samples at 2 ms spacing: 98 at 1.0 ms, then 2 spikes at 50.0 ms.
        // Two spikes (top 2%) put the nearest-rank p99 onto the spike level,
        // verifying that p99 reflects the stall tail rather than the baseline.
        let mut series: Vec<(f64, f32)> =
            (0..98).map(|i| (i as f64 * 0.002, 1.0)).collect();
        series.push((98.0 * 0.002, 50.0));
        series.push((99.0 * 0.002, 50.0));
        let s = stats(&mk(&series), 2.0);
        assert_eq!(s.count, 100);
        assert_eq!(s.max_ms, 50.0);
        assert_eq!(s.last_ms, 50.0);
        // p99 nearest-rank index = ceil(0.99·100) − 1 = 98 → first spike.
        assert_eq!(s.p99_ms, 50.0);
        // Mean = (98·1 + 2·50) / 100 = 1.98.
        assert!((s.mean_ms - 1.98).abs() < 1e-3, "mean was {}", s.mean_ms);
        assert!((s.window_s - 0.198).abs() < 1e-6);
    }
}
