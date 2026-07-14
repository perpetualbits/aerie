// SPDX-License-Identifier: GPL-3.0-or-later
//
// constellation — spike for aerie's unifying "constellation face".
//
// Live /proc process-groups become nodes in one semantically-zoomable graph:
// backbone edges are lineage (parent comm -> child comm), node size = CPU,
// heat = memory, pulse = strain. By default the pulse/banner/culprit are
// driven by a REAL stall detector (`mod detect`: a background latency probe +
// PSI + periodicity/offender analysis) — the map only ever shows a red stall
// banner when a real stall is actually detected; otherwise a quiet
// "monitoring" status. Pass `--demo` to drive the same visuals from a
// synthetic periodic-stall injector instead (clearly labeled DEMO), useful
// when there's no real stall around to feel the notice -> materialize -> dive
// arc. Standalone: mullion + crossterm only.
//
// Run:  cargo run --bin constellation [--demo]
// Keys: q / Ctrl-C  quit
//       arrows      move focus
//       Enter / + / = dive into the focused node
//       Esc / - / _  surface back to the overview
//       space       pause / resume the sampling + animation clock

use anyhow::Result;
use crossterm::event::Event;
use mullion::backend::CrosstermBackend;
use mullion::capabilities::Capabilities;
use mullion::ease::{gaussian, smoothstep};
use mullion::input::{KeyCode, KeyModifiers};
use mullion::layout::TileId;
use mullion::style::{Color, Modifier, Style};
use mullion::sugiyama::{auto_layout, LayerDir, SugiyamaParams};
use mullion::zoom::{lerp_rect, Lod, LodScale};
use mullion::{Buffer, Cell, EventReader, Field, FloatRect, GraphCanvas, Rect, Terminal, Viewport};
use std::collections::{HashMap, HashSet};
use std::io;
use std::time::{Duration, Instant};

const FRAME: Duration = Duration::from_millis(33); // ~30 fps

const HELP: &str = "\
constellation — aerie unifying-face spike

USAGE: constellation [--demo] [--probe-debug]
  --demo    drive a synthetic periodic-stall injector instead of the real
            detector; the banner reads DEMO so it's never mistaken for a
            real detection
  --probe-debug  dump raw probe samples/event onsets/period to stderr each
            resample (diagnostic; real mode only)
  -h,--help show this help

Default (no flag): real stall detection — a background latency probe + PSI
+ periodicity/offender analysis. Shows a red stall banner only when a real
stall is detected; otherwise a quiet monitoring status.

KEYS: q/Ctrl-C quit; arrows move focus; Enter/+/= dive; Esc/-/_ surface;
      space pause/resume clock\n";

/// Seconds for a full dive (zoom_t: 0 -> 1) or surface (1 -> 0) ease.
const ZOOM_SECS: f32 = 0.3;

/// A screen-space arrow direction, used for nearest-neighbour focus movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArrowDir {
    Up,
    Down,
    Left,
    Right,
}

const SAMPLE_EVERY: f32 = 1.0;

/// Cap on distinct comm-groups shown as nodes. Real `/proc` can have hundreds
/// of distinct `comm`s; per-frame edge routing (`render_edges` runs grid-A*
/// routing over the whole canvas every frame, not just on resample) over that
/// many nodes blows well past the ~33ms frame budget. Measured on real
/// `/proc` (~889 procs / 592 comm-groups): 40 nodes => 80-220ms/frame and the
/// process pins a core; 20 nodes => ~18ms/frame average, comfortably under
/// budget with the render loop idling between frames. Lowered further to 12
/// so the map reads as a legible star (bigger, readable boxes) rather than a
/// crowd — still comfortably under the render budget.
const MAX_NODES: usize = 12;

/// How long (in `t` seconds) a detected stall stays displayed after the
/// probe's reading momentarily drops below the active threshold — a single
/// quiet sample shouldn't flicker the whole readout off (brief item 3).
///
/// Fix 4 (iteration 4d): a real burst is only a small fraction of its own
/// period (e.g. 250ms out of a 3.0s cycle), so a FIXED short hold lapses
/// `active` in every inter-burst gap — the observed ~28%-of-polls firing.
/// Once a period is known, hold `STALL_HOLD_PERIOD_MULT` times it (so the
/// hold outlasts the gap between bursts, not just the burst itself);
/// before a period is known yet, fall back to a fixed floor. See
/// `State::stall_hold_secs`.
const STALL_HOLD_FALLBACK_SECS: f32 = 4.0;
const STALL_HOLD_PERIOD_MULT: f32 = 1.5;

/// Fix 3 (iteration 4d): how many consecutive ACTIVE resamples the raw
/// detected period must agree (within `PERIOD_STABLE_TOLERANCE`) before the
/// DISPLAYED period adopts it. Stops the banner flickering between e.g.
/// `~9.0s`/`~12s`/`irregular` on transient aliasing noise — shows the last
/// stable value (or `irregular` before any value has stabilized) instead.
const PERIOD_STABLE_N: usize = 3;
/// Fractional tolerance for "the last `PERIOD_STABLE_N` raw periods agree".
const PERIOD_STABLE_TOLERANCE: f32 = 0.2;

/// Time constant for easing the shown weather pulse toward the cached real
/// report's `pulse` between resamples, so a once-a-second report update
/// doesn't read as a jerky step function.
const PULSE_EASE_TAU: f32 = 0.4;

struct State {
    demo: bool,   // --demo: synthetic injector, clearly labeled, instead of real detection
    probe_debug: bool, // --probe-debug: STEP-0 diagnostic eprintln dump (see detect::Detector::debug_dump)
    paused: bool, // space toggles; while true, advance() freezes the clock
    t: f32,       // seconds since start
    since_sample: f32,
    ids: CommIds,
    prev_cpu: HashMap<TileId, u64>, // last cumulative jiffies per node id
    prev_io: HashMap<TileId, u64>,  // last cumulative blkio ticks per node id
    prev_cpu_by_comm: HashMap<String, u64>, // last cumulative jiffies per comm, uncapped (feeds the detector)
    cons: Constellation,
    canvas: GraphCanvas,
    cpu_frac: HashMap<TileId, f32>, // per-frame normalized deltas
    mem_frac: HashMap<TileId, f32>,
    io_frac: HashMap<TileId, f32>, // per-frame normalized blkio (disk-wait) deltas
    injector: Injector,
    // Iteration 4b: the real detector (`None` under --demo). Spawned once in
    // `main` before entering the TUI; `epoch` is the single `Instant` shared
    // between its latency-probe clock and every `now_secs` we pass it, so the
    // probe's own sample timestamps and our window queries stay aligned.
    detector: Option<detect::Detector>,
    epoch: Instant,
    cached_report: Option<detect::DetectionReport>, // latest raw report (recomputed only on resample)
    pulse_shown: f32,                                // eased weather pulse, real mode only
    // Honest, held (hysteresis) display state — updated on resample, kept
    // through a short dynamic hold (`stall_hold_secs`) so a single quiet
    // sample doesn't flicker the banner/flare/culprit off.
    held_active: bool,
    held_resource: Option<Resource>,
    held_period_s: Option<f32>,
    held_culprit_comm: Option<String>,
    held_culprit_id: Option<TileId>,
    last_active_at: Option<f32>,
    // Fix 3 (iteration 4d): last `PERIOD_STABLE_N` raw (undebounced) period
    // readings from active resamples, oldest first — see `stable_period`.
    recent_periods: std::collections::VecDeque<Option<f32>>,
    // Task 8: semantic zoom (dive/surface) + spatial breadcrumb.
    last_samples: Vec<ProcSample>, // for the interior fallback (member PIDs)
    last_area: Rect,               // most recent screen area rendered into
    focus: Option<TileId>,         // currently highlighted node
    // Bug 1 fix: true once the user has arrowed to a node during the
    // CURRENT stall activation — while false, `resample` keeps `focus`
    // auto-following the culprit as it's attributed, so Enter with no
    // prior arrow dives the real culprit; an arrow press sets this so a
    // later Enter dives the arrowed node instead. Reset to false on every
    // fresh stall rising-edge (a new stall starts back in auto-follow).
    focus_explicit: bool,
    zoom_target: Option<TileId>,   // node being dived into, if any
    zoom_t: f32,                   // eased 0..1 progress (0 = overview, 1 = filled)
    zoom_goal: f32,                // 0.0 (surfacing) or 1.0 (diving)
}

impl State {
    fn new(demo: bool, detector: Option<detect::Detector>, epoch: Instant) -> Self {
        let mut state = State {
            demo,
            probe_debug: false,
            paused: false,
            t: 0.0,
            since_sample: 0.0,
            ids: CommIds::new(),
            prev_cpu: HashMap::new(),
            prev_io: HashMap::new(),
            prev_cpu_by_comm: HashMap::new(),
            cons: Constellation { nodes: Vec::new(), edges: Vec::new() },
            canvas: GraphCanvas::new(1, 1),
            cpu_frac: HashMap::new(),
            mem_frac: HashMap::new(),
            io_frac: HashMap::new(),
            injector: Injector::new(),
            detector,
            epoch,
            cached_report: None,
            pulse_shown: 0.0,
            held_active: false,
            held_resource: None,
            held_period_s: None,
            held_culprit_comm: None,
            held_culprit_id: None,
            last_active_at: None,
            recent_periods: std::collections::VecDeque::new(),
            last_samples: Vec::new(),
            last_area: Rect::new(0, 0, 80, 24),
            focus: None,
            focus_explicit: false,
            zoom_target: None,
            zoom_t: 0.0,
            zoom_goal: 0.0,
        };
        state.resample(); // populate the first frame immediately
        state
    }

    /// Whether the stall visuals (banner/flare/wash/dim) should be "on" this
    /// frame: always true under `--demo` (the synthetic clock never stops),
    /// else the held (hysteresis) real-detection active flag.
    fn stall_on(&self) -> bool {
        self.demo || self.held_active
    }

    /// Fix 4 (iteration 4d): how long `active` survives after the probe's
    /// reading last cleared the active threshold. Once a period is known,
    /// `STALL_HOLD_PERIOD_MULT` times it — long enough to bridge the quiet
    /// gap between bursts (a burst is a small fraction of its own period),
    /// not just the burst's own duration — else a fixed floor before a
    /// period is known yet.
    fn stall_hold_secs(&self) -> f32 {
        self.held_period_s
            .map(|p| (p * STALL_HOLD_PERIOD_MULT).max(STALL_HOLD_FALLBACK_SECS))
            .unwrap_or(STALL_HOLD_FALLBACK_SECS)
    }

    /// The node currently playing "culprit": the injector's pinned node under
    /// `--demo`, else the real detector's held culprit mapped to a shown node
    /// id (see `force_include_culprit`).
    fn culprit(&self) -> Option<TileId> {
        if self.demo {
            self.injector.culprit
        } else {
            self.held_culprit_id
        }
    }

    fn advance(&mut self, dt: f32) {
        if self.paused {
            // Freeze the view entirely: no clock, no resample, no zoom
            // easing. Dive/surface key presses still update zoom_goal (set
            // directly by dive()/surface(), not here), so the intent is
            // recorded and simply resumes easing once unpaused.
            return;
        }
        self.t += dt;
        self.since_sample += dt;
        if self.since_sample >= SAMPLE_EVERY {
            self.resample();
        }

        // Ease the shown weather pulse toward the cached real report's pulse
        // every frame (not just on resample) so it breathes smoothly rather
        // than stepping once a second. No-op under --demo, which drives its
        // pulse straight from the continuous injector formula instead.
        if !self.demo {
            let target = self.cached_report.as_ref().map(|r| r.pulse).unwrap_or(0.0);
            let k = (dt / PULSE_EASE_TAU).clamp(0.0, 1.0);
            self.pulse_shown += (target - self.pulse_shown) * k;
        }

        // Ease zoom_t toward zoom_goal over ~ZOOM_SECS, in either direction.
        if (self.zoom_t - self.zoom_goal).abs() > f32::EPSILON {
            let step = dt / ZOOM_SECS;
            if self.zoom_goal > self.zoom_t {
                self.zoom_t = (self.zoom_t + step).min(self.zoom_goal);
            } else {
                self.zoom_t = (self.zoom_t - step).max(self.zoom_goal);
            }
        }
        // Fully surfaced: drop the target so we stop drawing the overlay.
        if self.zoom_t <= 0.0 && self.zoom_goal <= 0.0 {
            self.zoom_target = None;
        }
    }

    /// The node the viewport should be panned to keep on-screen: the stall
    /// culprit while a stall is on-screen (real or `--demo`), else the
    /// focused node, else the highest-significance (first, since
    /// `cons.nodes` is kept sorted) node so startup lands on content rather
    /// than an empty canvas corner.
    fn pan_target(&self) -> Option<TileId> {
        if self.stall_on() {
            self.culprit().or(self.focus).or_else(|| self.cons.nodes.first().map(|n| n.id))
        } else if let Some(f) = self.focus {
            Some(f)
        } else {
            self.cons.nodes.first().map(|n| n.id)
        }
    }

    /// Move `focus` to the nearest node in `dir`, comparing screen-space rect
    /// centers (matching what the eye sees, not raw canvas coordinates).
    fn move_focus(&mut self, dir: ArrowDir) {
        // Bug 1 fix: any arrow press is the user taking explicit control of
        // focus, so `resample` stops auto-following the culprit until the
        // next fresh stall (see `focus_explicit` on `State`).
        self.focus_explicit = true;
        let (cw, ch) = self.canvas.size();
        let ga = graph_area(self.last_area);
        let mut vp = Viewport::new(ga, cw, ch);
        let placed = placed_rects(&self.canvas, Rect::new(0, 0, cw, ch));
        center_pan_on(&mut vp, ga, &placed, self.pan_target());
        let centers: Vec<(TileId, (f32, f32))> = placed
            .iter()
            .filter_map(|(id, crect)| {
                vp.project(*crect).map(|s| {
                    (*id, (s.x as f32 + s.width as f32 / 2.0, s.y as f32 + s.height as f32 / 2.0))
                })
            })
            .collect();
        if centers.is_empty() {
            return;
        }
        let current = self.focus.and_then(|f| centers.iter().find(|(id, _)| *id == f).copied());
        let Some((_, from)) = current else {
            self.focus = Some(centers[0].0);
            return;
        };

        let (dx, dy): (f32, f32) = match dir {
            ArrowDir::Up => (0.0, -1.0),
            ArrowDir::Down => (0.0, 1.0),
            ArrowDir::Left => (-1.0, 0.0),
            ArrowDir::Right => (1.0, 0.0),
        };
        let mut best: Option<(TileId, f32)> = None;
        for (id, (cx, cy)) in &centers {
            if self.focus == Some(*id) {
                continue;
            }
            let (vx, vy) = (cx - from.0, cy - from.1);
            let along = vx * dx + vy * dy;
            if along <= 0.5 {
                continue; // not in the requested direction
            }
            // Penalize perpendicular drift so a straight arrow prefers the
            // node most directly ahead, not merely the nearest by Euclidean
            // distance.
            let perp = if dx != 0.0 { vy } else { vx };
            let score = along + perp.abs() * 2.0;
            if best.is_none_or(|(_, b)| score < b) {
                best = Some((*id, score));
            }
        }
        if let Some((id, _)) = best {
            self.focus = Some(id);
        }
    }

    /// Dive into the focused node: it grows to fill the screen over ~ZOOM_SECS.
    fn dive(&mut self) {
        if let Some(f) = self.focus {
            if self.zoom_target != Some(f) {
                self.zoom_t = 0.0;
            }
            self.zoom_target = Some(f);
            self.zoom_goal = 1.0;
        }
    }

    /// Surface back to the overview: the dived node eases back into place.
    fn surface(&mut self) {
        self.zoom_goal = 0.0;
    }

    fn resample(&mut self) {
        let samples = sample_procs();
        let mut cons = build_graph(&samples, &mut self.ids);

        // CPU deltas vs previous cumulative jiffies.
        let mut deltas: HashMap<TileId, u64> = HashMap::new();
        for n in &cons.nodes {
            let prev = self.prev_cpu.get(&n.id).copied().unwrap_or(n.cpu_jiffies);
            deltas.insert(n.id, n.cpu_jiffies.saturating_sub(prev));
        }
        self.prev_cpu = cons.nodes.iter().map(|n| (n.id, n.cpu_jiffies)).collect();

        // Task 9: under --demo, pin the injector's culprit to a real, stable
        // app node the first time we see one — the SHOWN node (kernel threads
        // already excluded in build_graph) with the highest cumulative
        // cpu_jiffies, so it reads as a believable, recognizable culprit
        // rather than an arbitrary first-frame pick. Re-elect only if that
        // comm's process disappears out from under us later. A no-op when
        // --demo is off, so the real-detection path below is unaffected.
        if self.demo {
            if let Some(cid) = self.injector.culprit {
                if !cons.nodes.iter().any(|n| n.id == cid) {
                    self.injector.culprit = None;
                }
            }
            if self.injector.culprit.is_none() {
                self.injector.culprit = cons.nodes.iter().max_by_key(|n| n.cpu_jiffies).map(|n| n.id);
            }
        }

        // Iteration 4b: feed the real detector this resample's per-comm CPU
        // deltas — UNCAPPED (every comm this sample saw), not just the
        // MAX_NODES currently shown, so a periodic offender outside the
        // top-12 can still be identified and then forced on-screen below.
        if let Some(detector) = &mut self.detector {
            let comm_cpu = cpu_by_comm(&samples);
            let group_cpu_deltas: Vec<(String, u64)> = comm_cpu
                .iter()
                .map(|(comm, &total)| {
                    let prev = self.prev_cpu_by_comm.get(comm).copied().unwrap_or(total);
                    (comm.clone(), total.saturating_sub(prev))
                })
                .collect();
            self.prev_cpu_by_comm = comm_cpu;

            let now_secs = self.epoch.elapsed().as_secs_f64();
            detector.observe(now_secs, &group_cpu_deltas);
            let report = detector.report(now_secs);
            if self.probe_debug {
                detector.debug_dump(now_secs, 15.0);
                // Also dump the assembled report's own pulse/active gate —
                // this is what surfaced the second STEP-0 finding: period
                // extraction was fixed, but `active` (pulse vs
                // PULSE_ACTIVE_THRESHOLD) barely crossed on this box's
                // modest real overshoot until PULSE_NORM_MS was recalibrated.
                eprintln!(
                    "[probe-debug] report pulse={:.3} active={} magnitude_ms={:.1} period_s={:?}",
                    report.pulse, report.active, report.magnitude_ms, report.period_s
                );
            }

            // Bug 1 fix: remember whether the stall was on-screen BEFORE this
            // resample updates `held_active`, so we can detect the rising
            // edge (stall just turned on) below and reset `focus_explicit` —
            // a fresh stall starts back in auto-follow-the-culprit mode even
            // if the user had arrowed away during a previous, now-ended,
            // stall.
            let was_active = self.held_active;

            // Hold (hysteresis): only refresh the displayed active/resource/
            // period/culprit while genuinely active, or within a dynamic
            // hold window (fix 4, `stall_hold_secs`) after the last active
            // reading — a single quiet sample, or the ordinary inter-burst
            // gap, must not flicker the whole readout off (brief item 3).
            if report.active {
                self.held_active = true;
                self.last_active_at = Some(self.t);
                self.held_resource = report.resource;
                self.held_culprit_comm = report.culprit_comm.clone();

                // Fix 3: debounce the DISPLAYED period across resamples —
                // only adopt a new raw period once the last
                // `PERIOD_STABLE_N` active readings agree within
                // `PERIOD_STABLE_TOLERANCE`; otherwise keep showing
                // whatever was last stable (or `irregular`/None before
                // anything has stabilized yet).
                self.recent_periods.push_back(report.period_s);
                while self.recent_periods.len() > PERIOD_STABLE_N {
                    self.recent_periods.pop_front();
                }
                if self.recent_periods.len() == PERIOD_STABLE_N {
                    if let Some(p) = stable_period(&self.recent_periods, PERIOD_STABLE_TOLERANCE) {
                        self.held_period_s = Some(p);
                    }
                }
            } else if !(self.held_active
                && self.last_active_at.is_some_and(|at| self.t - at < self.stall_hold_secs()))
            {
                self.held_active = false;
                self.held_resource = None;
                self.held_period_s = None;
                self.held_culprit_comm = None;
                self.recent_periods.clear();
            }

            // Force-include: if the held culprit isn't among the shown
            // top-12 nodes, swap it in so it's visible, can flare, and can
            // be dived (same principle as the earlier culprit-on-screen fix).
            if let Some(name) = self.held_culprit_comm.clone() {
                force_include_culprit(&mut cons, &mut self.ids, &samples, &name);
            }
            self.held_culprit_id = self
                .held_culprit_comm
                .as_deref()
                .and_then(|name| cons.nodes.iter().find(|n| n.comm == name).map(|n| n.id));

            // Bug 1 fix: on the rising edge of the stall (was off, now on),
            // forget any manual arrow pick from a previous stall so this
            // fresh one starts in auto-follow mode again.
            if !was_active && self.held_active {
                self.focus_explicit = false;
            }
            // While the stall is on-screen and the user hasn't taken
            // explicit control via an arrow press, keep `focus` following
            // the culprit as it's attributed (attribution can lag a sample
            // or two behind the stall itself first showing "unattributed")
            // — so the readout line, the dive target, and the pan target
            // all agree with what's on-screen, and Enter with no prior
            // arrow dives the actual culprit, not merely the
            // highest-significance node.
            if let Some(cid) = auto_follow_culprit(self.held_active, self.focus_explicit, self.held_culprit_id) {
                self.focus = Some(cid);
            }

            self.cached_report = Some(report);
        }

        self.last_samples = samples; // kept for the Task 8 interior fallback

        let dmax = deltas.values().copied().max().unwrap_or(1).max(1);
        let mmax = cons.nodes.iter().map(|n| n.rss_pages).max().unwrap_or(1).max(1);
        self.cpu_frac = deltas.iter().map(|(&id, &d)| (id, d as f32 / dmax as f32)).collect();
        self.mem_frac = cons
            .nodes
            .iter()
            .map(|n| (n.id, n.rss_pages as f32 / mmax as f32))
            .collect();

        // Disk (block-I/O wait) deltas vs previous cumulative blkio ticks —
        // same delta-normalization pattern as CPU: seed prev=current on first
        // sight, saturating_sub, divisor floored at 1.
        let mut io_deltas: HashMap<TileId, u64> = HashMap::new();
        for n in &cons.nodes {
            let prev = self.prev_io.get(&n.id).copied().unwrap_or(n.blkio_ticks);
            io_deltas.insert(n.id, n.blkio_ticks.saturating_sub(prev));
        }
        self.prev_io = cons.nodes.iter().map(|n| (n.id, n.blkio_ticks)).collect();
        let imax = io_deltas.values().copied().max().unwrap_or(1).max(1);
        self.io_frac = io_deltas.iter().map(|(&id, &d)| (id, d as f32 / imax as f32)).collect();

        // Node size follows CPU delta; lay out with the same metric so sizes match.
        let cpu_max_for_size = dmax;
        // Build canvas using delta-based sizes: temporarily map jiffies->delta.
        let sized = Constellation {
            nodes: cons
                .nodes
                .iter()
                .map(|n| GNode { cpu_jiffies: *deltas.get(&n.id).unwrap_or(&0), ..n.clone() })
                .collect(),
            edges: cons.edges.clone(),
        };
        self.canvas = build_canvas(&sized, cpu_max_for_size);
        self.cons = cons;
        self.since_sample = 0.0;

        // Focus/zoom validity: the process graph reshuffles every sample, so a
        // previously focused or dived-into comm can vanish. Re-anchor focus to
        // some node rather than pointing at nothing, and surface immediately if
        // the dive target disappeared out from under us.
        if self.focus.is_none_or(|f| !self.cons.nodes.iter().any(|n| n.id == f)) {
            self.focus = self.cons.nodes.first().map(|n| n.id);
        }
        if let Some(zt) = self.zoom_target {
            if !self.cons.nodes.iter().any(|n| n.id == zt) {
                self.zoom_target = None;
                self.zoom_goal = 0.0;
                self.zoom_t = 0.0;
            }
        }
    }

    fn render(&mut self, buf: &mut Buffer) {
        use mullion::border::{draw_box, BorderStyle, Borders, CornerStyle, LineWeight};
        let area = buf.area;
        self.last_area = area; // used by move_focus on the next key press
        let (cw, ch) = self.canvas.size();
        // The map itself renders into a 1-cell-inset sub-rect so it never
        // paints over the perimeter rim, the banner row, or the footer row.
        let ga = graph_area(area);
        let mut vp = Viewport::new(ga, cw, ch);
        let placed = placed_rects(&self.canvas, Rect::new(0, 0, cw, ch)); // canvas-space rects
        // Pan-to-culprit/focus/most-significant: keeps the node that matters
        // on-screen even when the canvas is wider/taller than the terminal.
        center_pan_on(&mut vp, ga, &placed, self.pan_target());

        // While a dive is in progress (or easing out), the rest of the
        // constellation dims to frame the focused node as a receding scope —
        // the spatial breadcrumb. dim_amt is 0 at the start of a dive/end of a
        // surface, so there is no visible pop when the overlay first appears.
        let dim_amt = if self.zoom_target.is_some() { smoothstep(self.zoom_t.clamp(0.0, 1.0)) } else { 0.0 };

        // Iteration 4b: `s` is the weather pulse driving every stall visual,
        // 0..1. Under `--demo` it's the continuous synthetic injector
        // formula; by default it's the real detector's cached report pulse,
        // eased frame-to-frame by `advance` (see `pulse_shown`) since the
        // report itself is only recomputed once a resample.
        let s = if self.demo { self.injector.intensity(self.t) } else { self.pulse_shown };
        // `stall_on`: whether the stall visuals are "on" this frame at all —
        // always true under `--demo`, else the held (hysteresis) real
        // detection active flag. With it off the map stays exactly as calm
        // as it was at the end of Task 8.
        let stall_on = self.stall_on();

        // FOLLOW: how hard to dim every non-culprit node/edge toward
        // near-black this frame. Zero below s=0.35 and rising continuously to
        // 1.0 at a pulse peak — the zero-crossing at the threshold means
        // there's no pop where the effect switches on, only the smooth ramp
        // `s` already gives. Never set when no stall is on-screen.
        let stall_dim = if stall_on { ((s - 0.35) / 0.65).clamp(0.0, 1.0) } else { 0.0 };

        // Weather: a faint full-screen wash + perimeter rim, both scaled by
        // `s`, so the whole map visibly breathes on the stall clock. Drawn
        // before edges/nodes so it reads as background, not an overlay on
        // top of them.
        if stall_on {
            let wash = Color::Rgb((25.0 * s) as u8, (10.0 * s) as u8, (10.0 * s) as u8);
            buf.fill(area, Cell::new(" ", Style::default().bg(wash)));
            let rim_col =
                Color::Rgb((30.0 + 180.0 * s) as u8, (20.0 + 40.0 * s) as u8, (40.0 * (1.0 - s)) as u8);
            draw_box(
                buf,
                area,
                Borders::ALL,
                &BorderStyle {
                    weight: LineWeight::Light,
                    corners: CornerStyle::Rounded,
                    style: Style::default().fg(rim_col),
                },
            );
        }

        // Draw order (iteration 3b, item 5): edges, then node boxes, then
        // banner + readout + footer text last, so routed wires and box
        // outlines never paint over legible text.

        // Backbone edges (structural). FOLLOW: these recede with everything
        // else on the stall clock, same rationale as the nodes — and while a
        // stall is active they're additionally held to a heavy dim floor for
        // the whole duration (not just at a pulse peak), so they don't add
        // to the tangle while the culprit's resource star (below) is meant
        // to be the only thing competing for ink.
        let backbone_dim = if stall_on { stall_dim.max(0.6) } else { stall_dim };
        render_edges(
            buf,
            &placed,
            &vp,
            &self.cons.edges,
            dim_color(dim_color(Color::Rgb(90, 90, 110), dim_amt), backbone_dim),
            self.canvas.size(),
        );

        // Contention overlays (iteration 3b, items 1-2): draw-only edges
        // meaning "these groups share a resource axis". In CALM mode this is
        // structure, not a stall effect — faint, colour-coded, capped to the
        // busiest top-2 resources by activity so at most two axes' worth of
        // edges ever compete for ink (item 1). Under `--stall` this whole
        // background picture is suppressed: the only contention edges drawn
        // are the culprit's own hot star below, so the stall view reads as a
        // clean hub-and-spoke instead of a tangle (item 2).
        let node_ids: Vec<TileId> = self.cons.nodes.iter().map(|n| n.id).collect();
        if !stall_on {
            let contention = contention_edges(&node_ids, &self.cpu_frac, &self.mem_frac, &self.io_frac);
            let mut activity = [
                (Resource::Cpu, resource_activity(&node_ids, &self.cpu_frac)),
                (Resource::Mem, resource_activity(&node_ids, &self.mem_frac)),
                (Resource::Disk, resource_activity(&node_ids, &self.io_frac)),
            ];
            activity.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            for &(resource, total) in activity.iter().take(2) {
                if total <= 0.0 {
                    continue; // nothing on this axis at all this frame
                }
                let subset: Vec<(TileId, TileId)> = contention
                    .iter()
                    .filter(|(_, _, r)| *r == resource)
                    .map(|(a, b, _)| (*a, *b))
                    .collect();
                if subset.is_empty() {
                    continue;
                }
                let faint = dim_color(faint_color(resource_color(resource)), dim_amt);
                // Same elevated crossing penalty as the stall hot star
                // (item 3): all contention-edge routing biases hard toward
                // crossing-free routes, not just the culprit's overlay.
                render_edges_penalized(buf, &placed, &vp, &subset, faint, self.canvas.size(), 4, 20);
            }
        }

        // During a stall pulse, flare the culprit's own dominant resource hot
        // — edges running FROM the culprit to the other nodes it's currently
        // contending with on that axis. Under `--stall` this is the ONLY
        // contention drawing (the faint background overlay above is
        // suppressed for the duration), and it routes with a much higher
        // crossing penalty (item 3) than every other edge set so it reads as
        // a clean hub-and-spoke rather than tangling in the thin gaps
        // between a horizontal row of boxes. Anchored explicitly at the
        // culprit (not merely filtered from the generic star above) so it
        // flares whenever the culprit has any qualifying partner on its
        // dominant axis, independent of whether the culprit happens to also
        // be that axis's top-ranked node overall.
        if stall_on && s > 0.5 {
            if let Some(cid) = self.culprit() {
                // Prefer the real detector's own held resource reading when
                // we have one (item 4: "use the REAL report values"); fall
                // back to the frac-derived dominant axis under --demo or
                // when the resource is momentarily unattributed.
                let dom = if !self.demo {
                    self.held_resource.unwrap_or_else(|| {
                        dominant_resource(cid, &self.cpu_frac, &self.mem_frac, &self.io_frac)
                    })
                } else {
                    dominant_resource(cid, &self.cpu_frac, &self.mem_frac, &self.io_frac)
                };
                let frac = match dom {
                    Resource::Cpu => &self.cpu_frac,
                    Resource::Mem => &self.mem_frac,
                    Resource::Disk => &self.io_frac,
                };
                let partners = top_partners(cid, &node_ids, frac, CONTENTION_K - 1);
                if !partners.is_empty() {
                    let hot_edges: Vec<(TileId, TileId)> = partners.into_iter().map(|id| (cid, id)).collect();
                    let hot_color = blend_color(resource_color(dom), Color::Rgb(255, 255, 255), s);
                    render_edges_penalized(buf, &placed, &vp, &hot_edges, hot_color, self.canvas.size(), 4, 20);
                }
            }
        }

        // Nodes (boxes), drawn after every edge set so a box's fill/outline
        // always sits on top of a routed wire rather than being crossed by
        // one (item 5).
        let culprit_id = self.culprit();
        for (id, crect) in &placed {
            if let Some(screen) = vp.project(*crect) {
                let cpu = self.cpu_frac.get(id).copied().unwrap_or(0.0);
                let mem = self.mem_frac.get(id).copied().unwrap_or(0.0);
                // Task 9: only the stall culprit pulses, and only while a
                // stall is on-screen; every other node (and every node when
                // no stall is on-screen) gets zero strain, same as the
                // Task-7 placeholder.
                let strain = if stall_on && culprit_id == Some(*id) { s } else { 0.0 };
                let vis = encode_node(cpu, mem, strain);
                let is_focus = self.zoom_target.is_none() && self.focus == Some(*id);
                let is_culprit = stall_on && culprit_id == Some(*id);
                // FOLLOW: every node except the culprit recedes toward
                // near-black on the stall clock, so at a pulse peak the eye
                // has nowhere else to go. The culprit is exempt here — its
                // own hot pulse (below) carries its visibility instead.
                let color = dim_color(dim_color(vis.color, dim_amt), if is_culprit { 0.0 } else { stall_dim });
                // Culprit border: a distinct always-hot hue, independent of
                // the per-frame dim/pulse phase, so it stays unmistakable
                // even in the quiet part of the stall clock — not merely
                // blended in proportion to the instantaneous intensity `s`.
                // It still brightens further at a pulse peak (vis.pulse), but
                // never fades back toward the ambient node color like every
                // other node's border does.
                let border_color = if is_culprit {
                    blend_color(Color::Rgb(220, 40, 30), Color::Rgb(255, 140, 60), vis.pulse)
                } else {
                    blend_color(color, Color::Rgb(230, 80, 60), vis.pulse)
                };
                draw_box(
                    buf,
                    screen,
                    Borders::ALL,
                    &BorderStyle {
                        weight: if is_focus || is_culprit { LineWeight::Heavy } else { LineWeight::Light },
                        corners: CornerStyle::Rounded,
                        // The culprit's hot border always wins (even over the
                        // focus-ring white) so it stays unmistakable; focus
                        // still gets the white ring on every other node.
                        style: Style::default().fg(if is_focus && !is_culprit { Color::White } else { border_color }),
                    },
                );
                // Neutral label: the comm, elided to the box interior.
                if let Some(n) = self.cons.nodes.iter().find(|n| n.id == *id) {
                    if screen.width > 2 {
                        buf.set_string(
                            screen.x + 1,
                            screen.y,
                            n.comm.chars().take((screen.width - 2) as usize).collect::<String>().as_str(),
                            Style::default().fg(color),
                        );
                    }
                }
            }
        }

        // NOTICE: the detection banner — the reason to look and the answer
        // to "what do I do with this". Spans the top rim row; brightness
        // scales with the pulse so it, too, breathes on the stall clock.
        // Domain-agnostic: period/clock + resource-category language only.
        // Drawn after every edge and box this frame (item 5) so a routed
        // wire can never paint over it. Honest states (item 3): a red banner
        // ONLY while a stall is on-screen; otherwise (real mode only) a
        // quiet, non-alarming monitoring status — never a red banner when
        // the real probe hasn't detected anything.
        if stall_on {
            let banner = if self.demo {
                let contended = self
                    .injector
                    .culprit
                    .map(|cid| dominant_resource(cid, &self.cpu_frac, &self.mem_frac, &self.io_frac))
                    .unwrap_or(Resource::Cpu);
                format!(
                    "⚠ DEMO stall (synthetic) — contention on {} — acting on a ~{:.1}s clock",
                    resource_label(contended),
                    self.injector.period_s
                )
            } else {
                let mut b = String::from("⚠ stall detected");
                if let Some(r) = self.held_resource {
                    b.push_str(&format!(" — contention on {}", resource_label(r)));
                }
                match self.held_period_s {
                    Some(p) => b.push_str(&format!(" — ~{p:.1}s clock")),
                    None => b.push_str(" — irregular"),
                }
                if self.held_culprit_comm.is_none() {
                    b.push_str(" — unattributed");
                }
                b
            };
            if area.width as usize > banner.chars().count() {
                let banner_color = blend_color(Color::Rgb(120, 90, 20), Color::Rgb(255, 70, 40), s);
                let x = area.x + (area.width - banner.chars().count() as u16) / 2;
                buf.set_string(x, area.y, &banner, Style::default().fg(banner_color).add_modifier(Modifier::BOLD));
            }
        } else if !self.demo {
            // Calm (real mode only): honest default on a healthy system —
            // quiet, no red, but still legible that monitoring is live and
            // shows the probe's current peak so "nothing to see" is
            // distinguishable from "not running".
            let peak = self.cached_report.as_ref().map(|r| r.magnitude_ms).unwrap_or(0.0);
            let status = format!("monitoring — no stall detected (peak {peak:.0}ms)");
            if area.width as usize > status.chars().count() {
                let x = area.x + (area.width - status.chars().count() as u16) / 2;
                buf.set_string(x, area.y, &status, Style::default().fg(Color::Gray));
            }
        }

        // Legibility baseline (both modes): the focused (or, while a stall is
        // on-screen, culprit) node gets a full readout — its whole comm plus
        // live cpu%/mem% — since its box label is elided to a 2-char stump at
        // small sizes. A header line at top-left, one row below the rim.
        // While a stall is on-screen this line also carries the dive
        // invitation (item 4): appended here instead of drawn under the
        // culprit box, it's permanently out of the edge band, so no routed
        // wire can ever clip it. Drawn last (after nodes/edges) so routed
        // wires never paint over it, and only in the overview (the dive
        // overlay has its own title_line once zoomed in).
        if self.zoom_target.is_none() {
            // While a stall is on-screen, the culprit takes priority over
            // plain focus so its full comm/cpu/mem readout is always
            // on-screen — the "unmistakable" requirement doesn't stop at the
            // border color.
            let readout_id = if stall_on { culprit_id.or(self.focus) } else { self.focus };
            if let Some(readout_id) = readout_id {
                if let Some(n) = self.cons.nodes.iter().find(|n| n.id == readout_id) {
                    let cpu = self.cpu_frac.get(&readout_id).copied().unwrap_or(0.0) * 100.0;
                    let mem = self.mem_frac.get(&readout_id).copied().unwrap_or(0.0) * 100.0;
                    let is_readout_culprit = stall_on && culprit_id == Some(readout_id);
                    let tag = if is_readout_culprit { " [culprit]" } else { "" };
                    let dive_hint = if is_readout_culprit { "   ↵ dive to inspect" } else { "" };
                    let readout = format!("{}{tag}  cpu {cpu:.0}%  mem {mem:.0}%{dive_hint}", n.comm);
                    let y = area.y + 1;
                    if y < area.bottom() {
                        buf.set_string(area.x + 1, y, &readout, Style::default().fg(Color::White));
                    }
                }
            }
        }

        // The dive overlay itself: the focused node grown toward fullscreen.
        if let Some(tid) = self.zoom_target {
            self.render_zoom_overlay(buf, area, tid, &placed, &vp);
        }

        // HUD footer: node count, sample age (seconds since last resample),
        // and — while a stall is on-screen — the live pulse intensity, so
        // manual verification of the spike proof-goals is legible at a
        // glance. Domain-agnostic: counts/seconds/numbers only, no product
        // names.
        let mut footer = format!("nodes={} age={:.1}s", self.cons.nodes.len(), self.since_sample);
        if self.paused {
            footer.push_str(" paused");
        }
        if stall_on {
            footer.push_str(&format!(" intensity={s:.2}"));
        }
        // Legibility baseline (both modes): a one-line legend so the visual
        // encoding is never a guessing game.
        footer.push_str("  size=CPU  heat=memory  edges=contention");
        if area.height > 0 {
            let y = area.bottom().saturating_sub(1);
            buf.set_string(area.x + 1, y, &footer, Style::default().fg(Color::White));
        }
    }

    /// Draw the dived-into node's rect eased from its overview position toward
    /// fullscreen, swapping detail level as it crosses LoD area thresholds.
    fn render_zoom_overlay(
        &self,
        buf: &mut Buffer,
        area: Rect,
        tid: TileId,
        placed: &[(TileId, Rect)],
        vp: &Viewport,
    ) {
        use mullion::border::{draw_box, BorderStyle, Borders, CornerStyle, LineWeight};

        let Some(overview) = placed.iter().find(|(i, _)| *i == tid).and_then(|(_, cr)| vp.project(*cr)) else {
            return; // node scrolled out of view this frame; nothing to draw
        };
        let full = Rect::new(
            area.x + 2,
            area.y + 1,
            area.width.saturating_sub(4),
            area.height.saturating_sub(2),
        );
        let eased = smoothstep(self.zoom_t.clamp(0.0, 1.0));
        let grown = lerp_rect(overview, full, eased);

        let comm = self.cons.nodes.iter().find(|n| n.id == tid).map(|n| n.comm.as_str()).unwrap_or("?");
        let members = self.last_samples.iter().filter(|s| s.comm == comm).count();

        // Bug 2 fix: the room must become an OPAQUE surface, not merely a
        // border drawn over a dimmed-but-still-visible map. Fill the grown
        // rect with blank cells first so nothing underneath (dimmed box
        // borders, edges) shows through the interior before we draw
        // anything into it. Once the dive is mostly complete (`eased` past
        // ~0.6), also clear the whole graph area — not just `grown` — so no
        // dimmed geometry survives in the thin margin between `grown` and
        // `area` either; a partial bleed during the early grow is fine
        // (it reads as the room arriving over the map), but by the end there
        // must be zero bleed-through.
        if eased > 0.6 {
            buf.fill(graph_area(area), Cell::new(" ", Style::default()));
        }
        buf.fill(grown, Cell::new(" ", Style::default()));

        draw_box(
            buf,
            grown,
            Borders::ALL,
            &BorderStyle { weight: LineWeight::Heavy, corners: CornerStyle::Rounded, style: Style::default().fg(Color::Cyan) },
        );

        match Lod::for_rect(grown, LodScale::default()) {
            Lod::Collapsed => {}
            Lod::Titled => title_line(buf, grown, &format!(" {comm} "), Color::Cyan),
            Lod::Ported => {
                title_line(buf, grown, &format!(" {comm} "), Color::Cyan);
                let sub = format!("{members} procs");
                if grown.width > sub.len() as u16 + 2 && grown.height > 2 {
                    buf.set_string(grown.x + 2, grown.y + 1, &sub, Style::default().fg(Color::Gray));
                }
            }
            Lod::Full => {
                self.render_interior(buf, grown, tid, comm, members);
                title_line(buf, grown, &format!(" {comm} "), Color::Cyan);
                // Bedrock: diving into the stall culprit itself, at Full LoD,
                // reveals the latency timeline underneath its interior — the
                // real detector's overshoot series by default, or the
                // synthetic injector's own pulse train under --demo.
                if self.stall_on() && self.culprit() == Some(tid) {
                    self.render_stall_timeline(buf, grown);
                }
            }
        }
    }

    /// The node's interior at `Lod::Full`: a small sub-constellation of its
    /// child comms (nodes reached by an outgoing backbone edge from `tid`).
    /// Leaf comms have no children to unfold, so they fall back to a legible
    /// listing of member PIDs instead.
    fn render_interior(&self, buf: &mut Buffer, outer: Rect, tid: TileId, comm: &str, members: usize) {
        use mullion::border::{draw_box, BorderStyle, Borders, CornerStyle, LineWeight};

        let interior = Rect::new(
            outer.x + 2,
            outer.y + 2,
            outer.width.saturating_sub(4),
            outer.height.saturating_sub(4),
        );
        if interior.width < 10 || interior.height < 5 {
            return; // too small to show anything past the title
        }

        let child_ids: HashSet<TileId> =
            self.cons.edges.iter().filter(|(a, _)| *a == tid).map(|(_, b)| *b).collect();

        if child_ids.is_empty() {
            // Fallback: legible member-PID listing (no child comms to unfold).
            buf.set_string(interior.x, interior.y, &format!("{members} member pids:"), Style::default().fg(Color::Gray));
            let pids: Vec<u32> = self.last_samples.iter().filter(|s| s.comm == comm).map(|s| s.pid).collect();
            for (y, chunk) in (interior.y + 1..interior.y + interior.height).zip(pids.chunks(6)) {
                let line: String = chunk.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(" ");
                buf.set_string(
                    interior.x,
                    y,
                    &line.chars().take(interior.width as usize).collect::<String>(),
                    Style::default().fg(Color::Gray),
                );
            }
            return;
        }

        // Faithful interior: the child comms as their own tiny constellation,
        // scaled into `interior` by a fresh Viewport (same trick the outer
        // canvas uses to map canvas-space to screen-space).
        let nodes: Vec<GNode> = self.cons.nodes.iter().filter(|n| child_ids.contains(&n.id)).cloned().collect();
        let edges: Vec<(TileId, TileId)> =
            self.cons.edges.iter().filter(|(a, b)| child_ids.contains(a) && child_ids.contains(b)).copied().collect();
        let cpu_max = nodes.iter().map(|n| n.cpu_jiffies).max().unwrap_or(1).max(1);
        let child_cons = Constellation { nodes, edges };
        let inner_canvas = build_canvas(&child_cons, cpu_max);
        let (icw, ich) = inner_canvas.size();
        let inner_vp = Viewport::new(interior, icw, ich);
        let inner_placed = placed_rects(&inner_canvas, Rect::new(0, 0, icw, ich));

        for (id, crect) in &inner_placed {
            if let Some(screen) = inner_vp.project(*crect) {
                let color = Color::Rgb(160, 160, 220);
                draw_box(
                    buf,
                    screen,
                    Borders::ALL,
                    &BorderStyle { weight: LineWeight::Light, corners: CornerStyle::Rounded, style: Style::default().fg(color) },
                );
                if let Some(n) = child_cons.nodes.iter().find(|n| n.id == *id) {
                    if screen.width > 2 {
                        buf.set_string(
                            screen.x + 1,
                            screen.y,
                            &n.comm.chars().take((screen.width - 2) as usize).collect::<String>(),
                            Style::default().fg(color),
                        );
                    }
                }
            }
        }
        render_edges(buf, &inner_placed, &inner_vp, &child_cons.edges, Color::Rgb(100, 100, 150), inner_canvas.size());
    }

    /// Bedrock: a bottom strip inside the culprit's `Lod::Full` rect, rendered
    /// as a latency timeline via `Field::render_braille`. Dispatches to the
    /// real detector's own overshoot series by default, or the synthetic
    /// injector's pulse train under `--demo` (item 4/5: no synthetic values
    /// mixed into the real story, and vice versa).
    fn render_stall_timeline(&self, buf: &mut Buffer, outer: Rect) {
        if self.demo {
            self.render_demo_timeline(buf, outer);
        } else {
            self.render_real_timeline(buf, outer);
        }
    }

    /// `--demo` bedrock: the injector's own pulse train swept across a small
    /// time window, so the peaks the map has been breathing to are visible
    /// as a trace. Labeled only in neutral period-seconds terms; no product
    /// names or remediation hints.
    fn render_demo_timeline(&self, buf: &mut Buffer, outer: Rect) {
        let strip_h = 3u16.min(outer.height.saturating_sub(2));
        if outer.width < 12 || strip_h < 2 {
            return; // too small to show a timeline
        }
        let strip = Rect::new(
            outer.x + 2,
            outer.bottom().saturating_sub(strip_h + 1),
            outer.width.saturating_sub(4),
            strip_h,
        );

        // Sweep a window a few periods wide across the strip so the peaks
        // read as a train, and slide it with the clock so it keeps moving.
        let window = (self.injector.period_s * 3.0).max(1.0);
        let window_start = self.t - window;
        let field = Field::rect(strip);
        field.render_braille(
            buf,
            |u, _v| self.injector.intensity(window_start + u * window),
            |mean| Style::default().fg(Color::Rgb((60.0 + 195.0 * mean) as u8, 90, 70)),
        );

        // PROVE: the payoff readout — "I found the periodic thing stalling
        // the system", in domain-agnostic period/clock terms. Stacked right
        // above the braille strip when there's room for all three lines;
        // degrades to just the period line (the old behavior) in a squeezed
        // Lod::Full rect rather than disappearing outright.
        let duration_ms = self.duration_ms();
        let lines = [
            format!("period {:.1}s", self.injector.period_s),
            format!("duration ~{duration_ms}ms"),
            "acting on a clock".to_string(),
        ];
        let gap = strip.y.saturating_sub(outer.y + 1); // rows free above the strip, below the title
        if gap >= lines.len() as u16 {
            for (i, line) in lines.iter().enumerate() {
                let y = strip.y - lines.len() as u16 + i as u16;
                if (line.len() as u16) < strip.width {
                    buf.set_string(strip.x, y, line, Style::default().fg(Color::Gray));
                }
            }
        } else if gap >= 1 {
            let label = &lines[0];
            if (label.len() as u16) < strip.width {
                buf.set_string(strip.x, strip.y.saturating_sub(1), label, Style::default().fg(Color::Gray));
            }
        }
    }

    /// A plausible synthetic duration for the injected hiccup, in whole ms:
    /// the injector's `sigma` is the pulse's std-dev as a fraction of its
    /// period, so `sigma * period_s * 1000` is the width (in ms) of the
    /// "busy" portion of one pulse — a stand-in for a measured stall
    /// duration, honestly derived from the same synthetic clock as the rest
    /// of the story rather than an unrelated made-up number.
    fn duration_ms(&self) -> i64 {
        (self.injector.sigma * self.injector.period_s * 1000.0).round() as i64
    }

    /// Real bedrock (item 4): the detector's own recent `overshoot_series`
    /// swept across the strip as a braille trace — no synthetic values. The
    /// readout is `period {p:.1}s · magnitude {m:.0}ms` from the cached
    /// report, or `irregular · magnitude {m:.0}ms` when there's no clear
    /// period.
    fn render_real_timeline(&self, buf: &mut Buffer, outer: Rect) {
        let strip_h = 3u16.min(outer.height.saturating_sub(2));
        if outer.width < 12 || strip_h < 2 {
            return; // too small to show a timeline
        }
        let strip = Rect::new(
            outer.x + 2,
            outer.bottom().saturating_sub(strip_h + 1),
            outer.width.saturating_sub(4),
            strip_h,
        );

        let Some(report) = self.cached_report.as_ref() else { return };
        let series = &report.overshoot_series;
        if series.len() >= 2 {
            let t0 = series[0].0;
            let t1 = series[series.len() - 1].0;
            let span = (t1 - t0).max(1e-3);
            let max_ms = series.iter().map(|&(_, m)| m).fold(0.0f32, f32::max).max(1.0);
            let field = Field::rect(strip);
            field.render_braille(
                buf,
                |u, _v| {
                    let target_t = t0 + u as f64 * span;
                    // Nearest sample to this point in the swept window —
                    // the series is irregularly spaced (probe ticks), so a
                    // simple nearest-neighbour lookup is enough for a strip
                    // a few dozen cells wide.
                    let nearest = series.iter().min_by(|a, b| {
                        (a.0 - target_t).abs().partial_cmp(&(b.0 - target_t).abs()).unwrap()
                    });
                    nearest.map(|&(_, m)| (m / max_ms).clamp(0.0, 1.0)).unwrap_or(0.0)
                },
                |mean| Style::default().fg(Color::Rgb((60.0 + 195.0 * mean) as u8, 90, 70)),
            );
        }

        let readout = match report.period_s {
            Some(p) => format!("period {p:.1}s · magnitude {:.0}ms", report.magnitude_ms),
            None => format!("irregular · magnitude {:.0}ms", report.magnitude_ms),
        };
        let gap = strip.y.saturating_sub(outer.y + 1);
        if gap >= 1 && (readout.len() as u16) < strip.width {
            buf.set_string(strip.x, strip.y.saturating_sub(1), &readout, Style::default().fg(Color::Gray));
        }
    }
}

/// Fix 3 (iteration 4d): `Some(mean)` only if every entry in `history` is
/// `Some` and all are within `tol` fractional tolerance of their mean —
/// i.e. the raw per-resample period reading has been stable across the
/// whole window, not flickering (e.g. 9.0s / 12.0s / irregular on
/// transient aliasing noise). `None` (including on any `None` entry, or an
/// empty/non-positive mean) means "not yet stable" — the caller should
/// keep showing the last stable value.
fn stable_period(history: &std::collections::VecDeque<Option<f32>>, tol: f32) -> Option<f32> {
    let mut vals = Vec::with_capacity(history.len());
    for h in history {
        vals.push((*h)?);
    }
    if vals.is_empty() {
        return None;
    }
    let mean = vals.iter().sum::<f32>() / vals.len() as f32;
    if mean <= 0.0 {
        return None;
    }
    let max_dev = vals.iter().map(|v| (v - mean).abs() / mean).fold(0.0f32, f32::max);
    (max_dev <= tol).then_some(mean)
}

/// Bug 1 fix: what `focus` should become on this resample, given whether the
/// stall is currently on-screen (`is_active`), whether the user has taken
/// explicit control of focus via an arrow press since the current stall
/// activation began (`focus_explicit`), and the (possibly still-unknown,
/// e.g. for the first sample or two after the stall turns on) culprit.
/// `Some(_)` means "auto-follow: point focus at the culprit", which keeps
/// firing every resample while the stall stays on-screen and the culprit
/// updates — not just once on the stall's rising edge — since attribution
/// can lag a sample or two behind `is_active` first going true. Returns
/// `None` (meaning "leave focus alone") once the culprit isn't known yet, or
/// once the user has arrowed away, so an explicit pick sticks instead of
/// being clobbered back onto the culprit on the next resample.
fn auto_follow_culprit(is_active: bool, focus_explicit: bool, culprit: Option<TileId>) -> Option<TileId> {
    if is_active && !focus_explicit {
        culprit
    } else {
        None
    }
}

/// Blend a color toward dark gray by `amt` in `[0, 1]` — used to recede the
/// overview constellation into the background while a dive is in progress.
fn dim_color(c: Color, amt: f32) -> Color {
    let amt = amt.clamp(0.0, 1.0);
    match c {
        Color::Rgb(r, g, b) => {
            let f = 1.0 - amt * 0.7;
            Color::Rgb((r as f32 * f) as u8, (g as f32 * f) as u8, (b as f32 * f) as u8)
        }
        other => other,
    }
}

/// Blend color `a` toward color `b` by fraction `t` in `[0, 1]` — used to make
/// the stall culprit's border breathe toward a hot hue proportional to its
/// pulse. `t <= 0` returns `a` untouched (byte-identical), so callers that
/// pass a real `0.0` pulse for every non-culprit node see no change at all.
/// If `a` isn't an `Rgb` color and `t > 0`, falls back to `b` directly since
/// there are no channels to lerp.
fn blend_color(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    if t <= 0.0 {
        return a;
    }
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
            Color::Rgb(lerp(ar, br), lerp(ag, bg), lerp(ab, bb))
        }
        _ => b,
    }
}

/// Centered title line at the top of `rect`, if it fits.
fn title_line(buf: &mut Buffer, rect: Rect, label: &str, color: Color) {
    if (label.len() as u16) < rect.width {
        let x = rect.x + (rect.width - label.len() as u16) / 2;
        buf.set_string(x, rect.y, label, Style::default().fg(color).add_modifier(Modifier::BOLD));
    }
}

/// Iteration 3: a shared resource axis that two groups can be said to
/// "contend" for. Generic category names only — domain-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resource {
    Cpu,
    Mem,
    Disk,
}

/// Dim, resource-tinted colour for the faint (both-modes) contention overlay.
fn resource_color(r: Resource) -> Color {
    match r {
        Resource::Cpu => Color::Rgb(220, 170, 40),
        Resource::Mem => Color::Rgb(150, 110, 230),
        Resource::Disk => Color::Rgb(60, 190, 180),
    }
}

/// Domain-agnostic label for the stall banner.
fn resource_label(r: Resource) -> &'static str {
    match r {
        Resource::Cpu => "CPU",
        Resource::Mem => "memory",
        Resource::Disk => "disk",
    }
}

/// Scale an RGB colour to a faint fraction of itself — the baseline
/// brightness for contention edges in BOTH modes (structure, not a stall
/// effect), distinct from `dim_color`'s dive-recede blend.
fn faint_color(c: Color) -> Color {
    match c {
        Color::Rgb(r, g, b) => {
            Color::Rgb((r as f32 * 0.35) as u8, (g as f32 * 0.35) as u8, (b as f32 * 0.35) as u8)
        }
        other => other,
    }
}

/// The culprit's own dominant resource axis this frame: whichever of its
/// cpu/mem/disk normalized values is largest. Memory (RSS) is essentially
/// never zero for a surviving node, so this always resolves to something
/// sane even when the culprit's current-frame cpu/disk deltas are both 0.
fn dominant_resource(
    cid: TileId,
    cpu_frac: &HashMap<TileId, f32>,
    mem_frac: &HashMap<TileId, f32>,
    io_frac: &HashMap<TileId, f32>,
) -> Resource {
    let c = cpu_frac.get(&cid).copied().unwrap_or(0.0);
    let m = mem_frac.get(&cid).copied().unwrap_or(0.0);
    let d = io_frac.get(&cid).copied().unwrap_or(0.0);
    if d >= c && d >= m {
        Resource::Disk
    } else if m >= c {
        Resource::Mem
    } else {
        Resource::Cpu
    }
}

/// Contention-edge threshold/fan-out: a node's per-resource value must clear
/// this to count as "contending" at all, and at most this many nodes per
/// resource enter the star (hub + up to K-1 spokes).
const CONTENTION_THRESHOLD: f32 = 0.05;
const CONTENTION_K: usize = 3;

/// Rank `node_ids` by `frac`, excluding `hub`, keeping only values above
/// `CONTENTION_THRESHOLD`, and return up to `k` ids highest-first. Shared by
/// `contention_edges` (hub = the top-ranked node overall) and the stall
/// hot-overlay (hub = the culprit, regardless of whether it happens to be
/// top-ranked) so both draw the same shape of star.
fn top_partners(hub: TileId, node_ids: &[TileId], frac: &HashMap<TileId, f32>, k: usize) -> Vec<TileId> {
    let mut ranked: Vec<(TileId, f32)> = node_ids
        .iter()
        .copied()
        .filter(|&id| id != hub)
        .filter_map(|id| frac.get(&id).copied().map(|v| (id, v)))
        .filter(|&(_, v)| v > CONTENTION_THRESHOLD)
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(k);
    ranked.into_iter().map(|(id, _)| id).collect()
}

/// Draw-only contention edges (item 3): for each resource axis, rank the
/// shown nodes by that axis's per-frame value; if at least 2 clear
/// `CONTENTION_THRESHOLD`, emit a star from the top (hub) node to each other
/// qualifying node (top `CONTENTION_K`), tagged with the resource. A resource
/// with fewer than 2 heavy nodes contributes no edges — nothing to contend.
/// These are separate from lineage (`cons.edges`) and are never fed to
/// `auto_layout`; they only ever affect what gets drawn on top.
fn contention_edges(
    node_ids: &[TileId],
    cpu_frac: &HashMap<TileId, f32>,
    mem_frac: &HashMap<TileId, f32>,
    io_frac: &HashMap<TileId, f32>,
) -> Vec<(TileId, TileId, Resource)> {
    let mut edges = Vec::new();
    for (resource, frac) in
        [(Resource::Cpu, cpu_frac), (Resource::Mem, mem_frac), (Resource::Disk, io_frac)]
    {
        let mut ranked: Vec<(TileId, f32)> = node_ids
            .iter()
            .filter_map(|&id| frac.get(&id).copied().map(|v| (id, v)))
            .filter(|&(_, v)| v > CONTENTION_THRESHOLD)
            .collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(CONTENTION_K);
        if ranked.len() < 2 {
            continue; // nothing to contend on this axis this frame
        }
        let hub = ranked[0].0;
        for &(id, _) in &ranked[1..] {
            edges.push((hub, id, resource));
        }
    }
    edges
}

/// Route and draw backbone edges between already-placed node rects, using
/// the default (bend=4, crossing=8) routing penalties. Free function (not a
/// `State` method) so both the outer constellation and an inner interior
/// sub-constellation (Task 8) can reuse it with their own canvas size.
fn render_edges(
    buf: &mut Buffer,
    placed: &[(TileId, Rect)],
    vp: &Viewport,
    edges: &[(TileId, TileId)],
    color: Color,
    canvas_size: (u16, u16),
) {
    render_edges_penalized(buf, placed, vp, edges, color, canvas_size, 4, 8);
}

/// As `render_edges`, but with explicit `route_all` bend/crossing penalties.
/// Contention edges (iteration 3b) route with a much higher crossing
/// penalty than backbone edges so the router biases hard toward
/// crossing-free, straight routes — the culprit's resource star should read
/// as a clean hub-and-spoke, not a tangle, in the thin gaps between a
/// horizontal row of boxes.
#[allow(clippy::too_many_arguments)] // thin routing/drawing wrapper; a struct would be more ceremony than the two extra params it replaces
fn render_edges_penalized(
    buf: &mut Buffer,
    placed: &[(TileId, Rect)],
    vp: &Viewport,
    edges: &[(TileId, TileId)],
    color: Color,
    canvas_size: (u16, u16),
    bend_penalty: u32,
    crossing_penalty: u32,
) {
    use mullion::border::LineWeight;
    use mullion::float::free_cells_in_window;
    use mullion::label::Side;
    use mullion::route::{render as render_connectors, route_all, RouteRequest};
    use mullion::socket::{Flow, Socket};

    let rect_of = |id: TileId| placed.iter().find(|(i, _)| *i == id).map(|(_, r)| *r);
    let node_rects: Vec<Rect> = placed.iter().map(|(_, r)| *r).collect();
    let (cw, ch) = canvas_size;
    let canvas = Rect::new(0, 0, cw, ch);
    let free: HashSet<(u16, u16)> =
        free_cells_in_window(canvas, &node_rects, 0, canvas).into_iter().collect();

    let mut reqs = Vec::new();
    for (a, b) in edges {
        let (Some(ra), Some(rb)) = (rect_of(*a), rect_of(*b)) else { continue };
        let src = Socket::new(Side::Right, (ra.height / 2).max(1), Flow::Out, 0);
        let dst = Socket::new(Side::Left, (rb.height / 2).max(1), Flow::In, 0);
        let (Some(s), Some(d)) = (src.attach(ra), dst.attach(rb)) else { continue };
        reqs.push(RouteRequest::new(s, d, src.outward().opposite(), dst.outward().opposite()));
    }
    let wires: Vec<_> = route_all(&free, &reqs, bend_penalty, crossing_penalty).into_iter().flatten().collect();
    let styles = vec![Style::default().fg(color); wires.len()];
    render_connectors(buf, vp.visible(), vp.origin(), &wires, &styles, &node_rects, LineWeight::Light);
}

/// Total per-resource contention activity this frame — sum of qualifying
/// (above-threshold) fractions across all shown nodes. Used to rank
/// resources by "how much is currently being contended for" so calm mode
/// can draw just the busiest couple of axes instead of all three at once.
fn resource_activity(node_ids: &[TileId], frac: &HashMap<TileId, f32>) -> f32 {
    node_ids.iter().filter_map(|id| frac.get(id).copied()).filter(|&v| v > CONTENTION_THRESHOLD).sum()
}

/// Sum of `cpu_jiffies` per comm over EVERY sample this resample saw —
/// deliberately uncapped (unlike `build_graph`'s MAX_NODES-limited nodes),
/// so the real detector (iteration 4b) can be fed activity for comms that
/// don't currently make the shown top-12, and still name one as the culprit.
fn cpu_by_comm(samples: &[ProcSample]) -> HashMap<String, u64> {
    let mut out: HashMap<String, u64> = HashMap::new();
    for s in samples {
        *out.entry(s.comm.clone()).or_insert(0) += s.cpu_jiffies;
    }
    out
}

/// Iteration 4b, item 2: if the real detector's culprit isn't among the
/// shown (MAX_NODES-capped) nodes, force it on-screen so it's visible, can
/// flare, and can be dived — same principle as the earlier "culprit must be
/// on-screen" fix for the synthetic injector. Aggregates the culprit's own
/// `GNode` straight from this resample's raw `samples` (same aggregation
/// `build_graph` does internally), then either fills a free slot or swaps out
/// the current lowest-significance node to keep the shown set at MAX_NODES.
/// A no-op if the culprit is already shown, or if it vanished from this
/// sample (its process(es) exited between the detector's analysis and now).
fn force_include_culprit(cons: &mut Constellation, ids: &mut CommIds, samples: &[ProcSample], culprit_comm: &str) {
    if cons.nodes.iter().any(|n| n.comm == culprit_comm) {
        return; // already on-screen
    }
    let mut cpu_jiffies = 0u64;
    let mut rss_pages = 0u64;
    let mut blkio_ticks = 0u64;
    let mut found = false;
    for s in samples {
        if s.comm == culprit_comm {
            cpu_jiffies += s.cpu_jiffies;
            rss_pages += s.rss_pages;
            blkio_ticks += s.blkio_ticks;
            found = true;
        }
    }
    if !found {
        return; // culprit's process(es) are gone this sample; nothing to force
    }
    let id = ids.id(culprit_comm);
    let culprit_node = GNode { id, comm: culprit_comm.to_string(), cpu_jiffies, rss_pages, blkio_ticks };

    if cons.nodes.len() < MAX_NODES {
        cons.nodes.push(culprit_node);
    } else {
        // Swap out the current lowest-significance node (min cpu_jiffies,
        // then min rss_pages as a tiebreak — the mirror of build_graph's own
        // significance ordering) so the shown set stays at MAX_NODES.
        let lo_idx = cons
            .nodes
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.cpu_jiffies.cmp(&b.1.cpu_jiffies).then(a.1.rss_pages.cmp(&b.1.rss_pages)))
            .map(|(i, _)| i)
            .expect("cons.nodes is non-empty (len >= MAX_NODES > 0)");
        cons.nodes[lo_idx] = culprit_node;
    }
    cons.nodes.sort_by_key(|n| n.id); // deterministic order for stable layout

    // Drop any edges that no longer have both endpoints among the survivors
    // (mirrors build_graph's own post-cap edge filter).
    let survivors: HashSet<TileId> = cons.nodes.iter().map(|n| n.id).collect();
    cons.edges.retain(|&(a, b)| survivors.contains(&a) && survivors.contains(&b));
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP}");
        return Ok(());
    }
    let demo = args.iter().any(|a| a == "--demo");
    // Iteration 4d STEP-0: temporary, gated diagnostic dump (see
    // `detect::Detector::debug_dump`) — eprintln's the raw overshoot
    // samples, extracted event onsets, and resulting period once per
    // resample so a real induced stall's numbers can be read directly
    // instead of guessed at. No-op unless passed.
    let probe_debug = args.iter().any(|a| a == "--probe-debug");

    // Iteration 4b: the real detector spawns its background latency-probe
    // thread here, before entering the TUI, under a single shared `epoch` so
    // its own sample timestamps and every `now_secs` we later pass it stay
    // aligned. Skipped entirely under --demo, which drives its visuals from
    // the synthetic injector instead.
    let epoch = Instant::now();
    let detector = if demo { None } else { Some(detect::Detector::spawn()) };

    let mut state = State::new(demo, detector, epoch);
    state.probe_debug = probe_debug;
    let mut backend = CrosstermBackend::new(io::stdout());
    backend.apply_capabilities(&Capabilities::detect());
    let mut terminal = Terminal::new(backend)?;
    terminal.enter()?;
    let input = EventReader::new();
    let mut last = Instant::now();

    let result: Result<()> = (|| {
        'frames: loop {
            let frame_start = Instant::now();
            let dt = frame_start.duration_since(last).as_secs_f32().min(0.1);
            last = frame_start;

            for ev in input.drain() {
                if let Event::Key(key) = ev {
                    match key.code {
                        // Only q / Ctrl-C quit now — Esc is repurposed to surface (Task 8).
                        KeyCode::Char('q') => break 'frames,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break 'frames
                        }
                        KeyCode::Up => state.move_focus(ArrowDir::Up),
                        KeyCode::Down => state.move_focus(ArrowDir::Down),
                        KeyCode::Left => state.move_focus(ArrowDir::Left),
                        KeyCode::Right => state.move_focus(ArrowDir::Right),
                        KeyCode::Enter | KeyCode::Char('+') | KeyCode::Char('=') => state.dive(),
                        KeyCode::Esc | KeyCode::Char('-') | KeyCode::Char('_') => state.surface(),
                        KeyCode::Char(' ') => state.paused = !state.paused,
                        _ => {}
                    }
                }
            }

            state.advance(dt);
            terminal.draw(|buf| state.render(buf))?;
            std::thread::sleep(FRAME.saturating_sub(frame_start.elapsed()));
        }
        Ok(())
    })();

    terminal.leave()?;
    result
}

#[derive(Debug, Clone)]
struct ProcStat {
    pid: u32,
    ppid: u32,
    comm: String,
    cpu_jiffies: u64,
    blkio_ticks: u64,
}

fn parse_stat(line: &str) -> Option<ProcStat> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    if close <= open {
        return None;
    }
    let pid: u32 = line[..open].trim().parse().ok()?;
    let comm = line[open + 1..close].to_string();
    // Remainder starts at field 3 (state). 0-based within remainder:
    //   state=0, ppid=1, ... utime=11, stime=12, ... delayacct_blkio_ticks=39
    let rest: Vec<&str> = line[close + 1..].split_whitespace().collect();
    if rest.len() < 13 {
        return None;
    }
    let ppid: u32 = rest[1].parse().ok()?;
    let utime: u64 = rest[11].parse().ok()?;
    let stime: u64 = rest[12].parse().ok()?;
    // field 42 (delayacct_blkio_ticks) isn't present on every kernel/proc
    // snapshot; treat it as 0 rather than failing the whole parse.
    let blkio_ticks: u64 = if rest.len() >= 40 { rest[39].parse().unwrap_or(0) } else { 0 };
    Some(ProcStat { pid, ppid, comm, cpu_jiffies: utime + stime, blkio_ticks })
}

fn parse_statm_resident(line: &str) -> Option<u64> {
    line.split_whitespace().nth(1)?.parse().ok()
}

#[derive(Debug, Clone)]
struct ProcSample {
    pid: u32,
    ppid: u32,
    comm: String,
    cpu_jiffies: u64,
    rss_pages: u64,
    blkio_ticks: u64,
}

fn sample_procs() -> Vec<ProcSample> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let base = entry.path();
        let Ok(stat_line) = std::fs::read_to_string(base.join("stat")) else { continue };
        let Some(st) = parse_stat(&stat_line) else { continue };
        let rss_pages = std::fs::read_to_string(base.join("statm"))
            .ok()
            .and_then(|s| parse_statm_resident(&s))
            .unwrap_or(0);
        out.push(ProcSample {
            pid: st.pid,
            ppid: st.ppid,
            comm: st.comm,
            cpu_jiffies: st.cpu_jiffies,
            rss_pages,
            blkio_ticks: st.blkio_ticks,
        });
    }
    out
}

struct CommIds {
    map: HashMap<String, TileId>,
    next: TileId,
}

impl CommIds {
    fn new() -> Self {
        CommIds { map: HashMap::new(), next: 1 }
    }
    fn id(&mut self, comm: &str) -> TileId {
        if let Some(&id) = self.map.get(comm) {
            return id;
        }
        let id = self.next;
        self.next += 1;
        self.map.insert(comm.to_string(), id);
        id
    }
}

#[derive(Debug, Clone)]
struct GNode {
    id: TileId,
    comm: String,
    cpu_jiffies: u64,
    rss_pages: u64,
    blkio_ticks: u64,
}

struct Constellation {
    nodes: Vec<GNode>,
    edges: Vec<(TileId, TileId)>,
}

fn build_graph(samples: &[ProcSample], ids: &mut CommIds) -> Constellation {
    // pid -> comm, so a child can look up its parent's comm.
    let pid_comm: HashMap<u32, &str> =
        samples.iter().map(|s| (s.pid, s.comm.as_str())).collect();

    // Aggregate per comm.
    let mut agg: HashMap<String, GNode> = HashMap::new();
    for s in samples {
        let id = ids.id(&s.comm);
        let n = agg.entry(s.comm.clone()).or_insert(GNode {
            id,
            comm: s.comm.clone(),
            cpu_jiffies: 0,
            rss_pages: 0,
            blkio_ticks: 0,
        });
        n.cpu_jiffies += s.cpu_jiffies;
        n.rss_pages += s.rss_pages;
        n.blkio_ticks += s.blkio_ticks;
    }

    // Lineage edges parent-comm -> child-comm, deduped, self-edges dropped.
    let mut edge_set: HashSet<(TileId, TileId)> = HashSet::new();
    for s in samples {
        let Some(parent_comm) = pid_comm.get(&s.ppid) else { continue };
        if *parent_comm == s.comm.as_str() {
            continue; // self-edge
        }
        let p = ids.id(parent_comm);
        let c = ids.id(&s.comm);
        edge_set.insert((p, c));
    }

    // Kernel threads have no user-space memory, so /proc/PID/statm resident
    // aggregates to 0 for them. Drop those BEFORE the significance cap so the
    // map shows the user's real apps (kt*/ir*/ksoftirqd/etc. never compete for
    // one of the MAX_NODES slots). If this ever leaves zero nodes, downstream
    // code already guards against an empty node set (no panics).
    let mut nodes: Vec<GNode> = agg.into_values().filter(|n| n.rss_pages > 0).collect();
    if nodes.len() > MAX_NODES {
        // Keep only the top MAX_NODES by significance (cpu_jiffies desc, then
        // rss_pages desc as a tiebreak) so the visible set stays stable
        // frame-to-frame; both fields are always present every frame.
        nodes.sort_by(|a, b| {
            b.cpu_jiffies.cmp(&a.cpu_jiffies).then_with(|| b.rss_pages.cmp(&a.rss_pages))
        });
        nodes.truncate(MAX_NODES);
    }
    nodes.sort_by_key(|n| n.id); // deterministic order for stable layout

    let survivors: HashSet<TileId> = nodes.iter().map(|n| n.id).collect();
    let mut edges: Vec<(TileId, TileId)> = edge_set
        .into_iter()
        .filter(|&(p, c)| survivors.contains(&p) && survivors.contains(&c))
        .collect();
    edges.sort();
    Constellation { nodes, edges }
}

#[derive(Debug, Clone, PartialEq)]
struct NodeVisual {
    cells: u16,
    color: Color,
    pulse: f32,
}

fn encode_node(cpu_frac: f32, mem_frac: f32, strain: f32) -> NodeVisual {
    // Floor raised so every box is wide enough to show ~8-10 chars of the
    // comm label (interior width = cells - 2 for the borders); still grows
    // with CPU up to MAX_CELLS for the busiest nodes.
    const MIN_CELLS: f32 = 12.0;
    const MAX_CELLS: f32 = 22.0;
    let cpu = cpu_frac.clamp(0.0, 1.0);
    let cells = (MIN_CELLS + (MAX_CELLS - MIN_CELLS) * cpu).round() as u16;

    // Cool (blue) -> hot (red) ramp on memory.
    let m = mem_frac.clamp(0.0, 1.0);
    let r = (40.0 + 215.0 * m) as u8;
    let g = (60.0 * (1.0 - m)) as u8;
    let b = (200.0 * (1.0 - m)) as u8;
    let color = Color::Rgb(r, g, b);

    NodeVisual { cells, color, pulse: strain.clamp(0.0, 1.0) }
}

fn node_side(cpu_jiffies: u64, cpu_max: u64) -> u16 {
    let frac = if cpu_max == 0 { 0.0 } else { cpu_jiffies as f32 / cpu_max as f32 };
    encode_node(frac, 0.0, 0.0).cells
}

fn build_canvas(cons: &Constellation, cpu_max: u64) -> GraphCanvas {
    // Canvas is generously larger than the screen; auto_layout resizes to fit.
    let mut canvas = GraphCanvas::new(200, 80).with_grid(2);
    for n in &cons.nodes {
        let side = node_side(n.cpu_jiffies, cpu_max);
        // Nodes are wide-but-short boxes: width carries the label (comm text),
        // height only needs to fit the top border (which doubles as the
        // title row), one interior row, and the bottom border.
        let h = (side / 4).max(3);
        canvas.add(n.id, FloatRect::new(0, 0, side, h));
    }
    // Process lineage is shallow (systemd -> app -> worker, ~2-4 layers), so
    // laying out TopBottom stacks the few layers vertically and spreads each
    // layer's many nodes HORIZONTALLY across the wide screen. LeftRight on
    // this same shallow-wide shape degenerates into one tall column stacked
    // against the left edge, which is what a real /proc capture showed.
    auto_layout(
        &mut canvas,
        &cons.edges,
        &SugiyamaParams { dir: LayerDir::TopDown, layer_gap: 5, node_gap: 3, grid: 2 },
    );
    canvas
}

fn placed_rects(canvas: &GraphCanvas, window: Rect) -> Vec<(TileId, Rect)> {
    canvas.solve(window)
}

/// The sub-rect of `area` available for the node/edge map itself: inset so
/// the graph never paints over the perimeter rim (drawn under `--stall`,
/// sharing the outermost row/col), the banner text (top row), the
/// focused/culprit readout line (the row right below the rim), or the
/// footer/legend text (bottom row) — all of which use `area`'s own
/// border/near-border rows/cols. Applied unconditionally (not just under
/// `--stall`) so node positions don't jitter when the stall banner toggles
/// on/off. Two rows are reserved at the top (rim/banner, then the readout
/// line) since both are always drawn in the overview.
fn graph_area(area: Rect) -> Rect {
    Rect::new(area.x + 1, area.y + 2, area.width.saturating_sub(2), area.height.saturating_sub(3))
}

/// Pan `vp` so `target`'s canvas-space rect is centered in `window` (clamped
/// to the valid pan range by `Viewport::set_pan`). This is what keeps the
/// stall culprit / focused / most-significant node on-screen instead of
/// stuck at the canvas's top-left corner whenever the canvas is bigger than
/// the terminal window.
fn center_pan_on(vp: &mut Viewport, window: Rect, placed: &[(TileId, Rect)], target: Option<TileId>) {
    let Some(tid) = target else { return };
    let Some((_, crect)) = placed.iter().find(|(id, _)| *id == tid) else { return };
    let cx = crect.x as i32 + crect.width as i32 / 2;
    let cy = crect.y as i32 + crect.height as i32 / 2;
    let px = (cx - window.width as i32 / 2).max(0) as u16;
    let py = (cy - window.height as i32 / 2).max(0) as u16;
    vp.set_pan(px, py);
}

struct Injector {
    period_s: f32,
    sigma: f32,
    culprit: Option<TileId>,
}

impl Injector {
    fn new() -> Self {
        Injector { period_s: 3.0, sigma: 0.10, culprit: None }
    }

    fn intensity(&self, t_s: f32) -> f32 {
        if self.period_s <= 0.0 {
            return 0.0;
        }
        // phase in 0..1, distance to nearest whole period (wrap-aware).
        let phase = (t_s / self.period_s).fract();
        let d = phase.min(1.0 - phase); // 0 at a peak, 0.5 at the trough
        gaussian(d, self.sigma).clamp(0.0, 1.0)
    }
}

// ── Detector: a real, self-contained stall detector (Iteration 4a/4b) ──────
//
// Ported (not imported — this example is standalone and cannot `use
// crate::diag`) from aerie's `src/diag.rs`: `LatencyProbe` (a background
// cyclictest measuring its own wakeup overshoot), the PSI reader, the
// autocorrelation/DFT periodicity analyzer, and a simplified periodic-
// offender attributor built on the same analyzer.
//
// Iteration 4a built and unit-tested the detection core (`Detector` /
// `DetectionReport`); iteration 4b wires it into `main`'s loop and the
// render path (see `State::detector`/`resample`/`render`), so the module is
// no longer dead code and no longer carries a blanket allow. The one
// remaining genuinely-unused pair of fields (`Periodicity::freq_lo`/`freq_hi`,
// echoed from the analysis config for a spectrum display this example never
// draws) keeps its own narrow `#[allow(dead_code)]` instead.
mod detect {
    use super::Resource;
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    /// One wakeup-latency sample produced by [`LatencyProbe`]. Ported from
    /// `diag::Sample`.
    #[derive(Clone, Copy, Debug)]
    pub(super) struct Sample {
        /// Seconds since the probe thread started.
        pub t: f64,
        /// How much longer than the requested tick this wakeup actually
        /// took, in milliseconds. ~0 is on time; spikes are the stalls.
        pub overshoot_ms: f32,
    }

    /// Configuration for [`LatencyProbe`]. Ported from `diag::ProbeConfig`.
    #[derive(Clone, Copy, Debug)]
    pub(super) struct ProbeConfig {
        pub tick: Duration,
        pub capacity: usize,
    }

    impl Default for ProbeConfig {
        fn default() -> Self {
            // 2 ms tick -> 500 Hz. 60_000 samples ~= 120 s of rolling
            // history, enough for the analyzer to resolve periods up to
            // tens of seconds while staying cheap. Same as diag.rs.
            Self { tick: Duration::from_millis(2), capacity: 60_000 }
        }
    }

    struct ProbeShared {
        ring: VecDeque<Sample>,
        capacity: usize,
    }

    /// A built-in `cyclictest`: a thread that measures its own wakeup
    /// latency. Ported from `diag::LatencyProbe`. Measuring the *probe
    /// thread's* own scheduling delay is the right signal: it is subject to
    /// the same system-wide preemption that stalls every other realtime
    /// thread, independent of any one application.
    pub(super) struct LatencyProbe {
        shared: Arc<Mutex<ProbeShared>>,
        stop: Arc<AtomicBool>,
        _handle: JoinHandle<()>,
    }

    impl LatencyProbe {
        pub fn spawn(cfg: ProbeConfig) -> Self {
            let shared = Arc::new(Mutex::new(ProbeShared {
                ring: VecDeque::with_capacity(cfg.capacity.min(4096)),
                capacity: cfg.capacity.max(1),
            }));
            let stop = Arc::new(AtomicBool::new(false));
            let tick = cfg.tick;
            let shared_t = Arc::clone(&shared);
            let stop_t = Arc::clone(&stop);
            let handle = std::thread::Builder::new()
                .name("constellation-latency-probe".into())
                .spawn(move || probe_loop(shared_t, stop_t, tick))
                .expect("spawn latency probe thread");
            Self { shared, stop, _handle: handle }
        }

        /// Copy the current ring contents (oldest -> newest) for analysis.
        pub fn snapshot(&self) -> Vec<Sample> {
            let g = self.shared.lock().unwrap();
            g.ring.iter().copied().collect()
        }
    }

    impl Drop for LatencyProbe {
        fn drop(&mut self) {
            // Signal the thread to exit; we don't join (it may be mid-sleep
            // for up to `tick`), letting the process tear it down.
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    /// The probe thread body: sleep `tick`, measure overshoot, record, repeat.
    fn probe_loop(shared: Arc<Mutex<ProbeShared>>, stop: Arc<AtomicBool>, tick: Duration) {
        let start = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let t0 = Instant::now();
            std::thread::sleep(tick);
            let elapsed = t0.elapsed();
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

    // ── PSI reader ─────────────────────────────────────────────────────

    /// Parse the `some ... total=NNN` microsecond counter from a PSI
    /// pressure file. Ported from `diag::read_psi_some_total`. Returns
    /// `None` on any missing file/field (older kernels, no PSI mounted) —
    /// callers must treat that as "unavailable", not "zero".
    fn read_psi_some_total(path: &str) -> Option<u64> {
        let data = std::fs::read_to_string(path).ok()?;
        let line = data.lines().find(|l| l.starts_with("some"))?;
        line.split_whitespace()
            .find_map(|tok| tok.strip_prefix("total=").and_then(|v| v.parse::<u64>().ok()))
    }

    /// Which PSI channel is contended. Ported from `diag::PressureChannel`,
    /// trimmed to the three PSI resources and mapped onto the example's own
    /// [`Resource`] (`Io` -> `Disk`) rather than duplicating names.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum PressureChannel {
        Cpu,
        Mem,
        Io,
    }

    impl PressureChannel {
        fn resource(self) -> Resource {
            match self {
                PressureChannel::Cpu => Resource::Cpu,
                PressureChannel::Mem => Resource::Mem,
                PressureChannel::Io => Resource::Disk,
            }
        }
    }

    // ── Periodicity analyzer ───────────────────────────────────────────

    /// Configuration for [`analyze_periodicity`]. Ported from
    /// `diag::AnalysisConfig`.
    #[derive(Clone, Copy, Debug)]
    pub(super) struct AnalysisConfig {
        pub freq_lo: f64,
        pub freq_hi: f64,
        pub freq_bins: usize,
    }

    impl Default for AnalysisConfig {
        fn default() -> Self {
            // 0.05 Hz (20 s period) up to 25 Hz covers the band where a
            // periodic system stall plausibly lives. Same as diag.rs.
            Self { freq_lo: 0.05, freq_hi: 25.0, freq_bins: 240 }
        }
    }

    /// Result of periodicity analysis over a latency series. Ported from
    /// `diag::Periodicity`.
    #[derive(Clone, Debug, Default)]
    pub(super) struct Periodicity {
        pub period_s: Option<f64>,
        pub freq_hz: Option<f64>,
        pub confidence: f32,
        pub spectrum: Vec<f32>,
        // Echoed from the analysis config for a would-be spectrum display
        // (axis labeling); this example never renders `spectrum`, so these
        // two are legitimately write-only here — narrow allow rather than
        // the whole module losing dead-code visibility for it.
        #[allow(dead_code)]
        pub freq_lo: f64,
        #[allow(dead_code)]
        pub freq_hi: f64,
        pub bin_dt: f64,
    }

    /// Resample an (approximately uniform but jittered) sample series onto a
    /// strictly uniform time grid of step `bin_dt`, taking the **max**
    /// overshoot in each bin so a single-tick spike is never averaged away.
    /// Ported from `diag::resample_uniform`.
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

    /// Find the period of a latency series via autocorrelation, plus a
    /// narrow-band DFT spectrum for display. Ported ~verbatim from
    /// `diag::analyze_periodicity` — this is the key correctness gate (see
    /// the `recovers_known_period` test below), so the algorithm is
    /// unchanged from the proven original.
    pub(super) fn analyze_periodicity(samples: &[Sample], cfg: AnalysisConfig) -> Periodicity {
        let mut out = Periodicity {
            freq_lo: cfg.freq_lo,
            freq_hi: cfg.freq_hi,
            spectrum: vec![0.0; cfg.freq_bins.max(1)],
            ..Default::default()
        };
        if samples.len() < 8 {
            return out;
        }

        let span = samples[samples.len() - 1].t - samples[0].t;
        let mean_spacing = span / (samples.len() - 1) as f64;
        let bin_dt = (1.0 / (cfg.freq_hi * 4.0)).max(span / 6000.0).max(mean_spacing * 1.5);
        out.bin_dt = bin_dt;

        let mut grid = resample_uniform(samples, bin_dt);
        let n = grid.len();
        if n < 16 {
            return out;
        }
        let mean = grid.iter().map(|&v| v as f64).sum::<f64>() / n as f64;
        for v in &mut grid {
            *v -= mean as f32;
        }
        let energy: f64 = grid.iter().map(|&v| (v as f64) * (v as f64)).sum();
        if energy < 1e-12 {
            return out; // flat series, nothing periodic
        }

        // ── Autocorrelation over the feasible lag band ──────────────
        let lag_min = ((1.0 / cfg.freq_hi) / bin_dt).floor().max(1.0) as usize;
        let lag_max = (((1.0 / cfg.freq_lo) / bin_dt).floor() as usize).min(n / 3).max(lag_min + 1);
        let mut corr = vec![0.0f64; lag_max + 1];
        for (lag, slot) in corr.iter_mut().enumerate().take(lag_max + 1).skip(lag_min) {
            let mut acc = 0.0f64;
            for i in 0..(n - lag) {
                acc += grid[i] as f64 * grid[i + lag] as f64;
            }
            *slot = acc / energy;
        }
        let search_from = corr
            .iter()
            .enumerate()
            .take(lag_max)
            .skip(lag_min)
            .find(|(_, &r)| r <= 0.0)
            .map(|(lag, _)| lag)
            .unwrap_or(lag_min);
        let best_corr = (search_from..=lag_max).map(|l| corr[l]).fold(0.0f64, f64::max);
        const PERIOD_MIN_CORR: f64 = 0.20;
        if best_corr > PERIOD_MIN_CORR {
            let thresh = best_corr * 0.9;
            let mut fundamental = (search_from..=lag_max).find(|&lag| corr[lag] >= thresh).unwrap_or(0);
            if fundamental > 0 {
                // Fix 2 (safety net): reject supra-harmonics. If the
                // candidate lag has a sub-multiple (/2, /3) that ALSO
                // lands within the feasible lag band and correlates
                // comparably strongly, prefer the smaller (truer
                // fundamental) — a real Nx period must not display as a
                // multiple of itself just because the subsampled/aliased
                // event series happens to correlate best at the larger
                // lag. Checked /3 before /2 so the most-reduced valid
                // candidate wins. This is a backstop: the primary cure for
                // the observed 3x supra-harmonic is catching every real
                // burst as an event upstream (see EVENT_THRESHOLD_MS).
                const SUPRA_HARMONIC_REJECT_FRACTION: f64 = 0.8;
                for divisor in [3usize, 2usize] {
                    let candidate = fundamental / divisor;
                    if candidate >= lag_min
                        && candidate <= lag_max
                        && corr[candidate] >= best_corr * SUPRA_HARMONIC_REJECT_FRACTION
                    {
                        fundamental = candidate;
                        break;
                    }
                }
                let period = fundamental as f64 * bin_dt;
                out.period_s = Some(period);
                out.freq_hz = Some(1.0 / period);
                out.confidence = best_corr.clamp(0.0, 1.0) as f32;
            }
        }

        // ── Log-spaced DFT for the displayed spectrum ───────────────
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

    // ── Stall-event clock (fix: lock the inter-burst period) ────────────
    //
    // `analyze_periodicity` is proven correct on a clean spike series (see
    // `recovers_known_period`), but a REAL stall burst isn't a single clean
    // spike: the probe is ~500 Hz, so one ~250ms scheduler stall shows up as
    // dozens of densely-elevated samples with their own jittery intra-burst
    // texture. Autocorrelation over the raw series locks onto that
    // intra-burst harmonic (recurring ~50x within the recording) rather than
    // the true inter-burst clock (recurring only a handful of times), so it
    // reported ~0.1s instead of ~3s. Fix: collapse each burst into a single
    // EVENT onset first, grid the onsets onto a coarse uniform impulse
    // series, and run the same proven `analyze_periodicity` on THAT.

    /// Overshoot magnitude that counts as "stalled" for event-extraction.
    ///
    /// STEP-0 diagnosis (iteration 4d), read via `--probe-debug` under a
    /// real induced ~3.0s/250ms 16-core stall (41 phase-aligned burners):
    /// the probe DOES overshoot on every single burst (onsets landed at
    /// 0.85/3.85/6.85/9.87/12.87/15.88/18.86s — a clean 3.0s train, exactly
    /// as expected), but the per-burst PEAK overshoot on this box was only
    /// ~1.0-5.7ms (typically 1.3-2.9ms; only occasionally topping 4ms) —
    /// nowhere near the "dozens of ms" a synthetic 20ms-amplitude test
    /// burst assumes. The old 4.0ms threshold cleared just 1 of 7 real
    /// bursts in a 20s window, leaving a sparse subsampled onset train that
    /// `analyze_periodicity` correctly, but uselessly, read as a ~9s clock.
    /// Lowered to comfortably UNDER the smallest observed real-burst peak
    /// (1.6ms) while staying well above ordinary idle jitter (a small
    /// fraction of the 2ms probe tick), so every real burst clears it.
    const EVENT_THRESHOLD_MS: f32 = 1.0;
    /// A rising edge only starts a NEW event if the series has been below
    /// `EVENT_THRESHOLD_MS` for at least this long; briefer dips are still
    /// "inside" the same burst (avoids splitting one burst into several
    /// onsets on intra-burst jitter).
    const EVENT_DEBOUNCE_S: f64 = 0.4;
    /// Bin width for the coarse onset-impulse grid. Coarse enough that
    /// dense intra-burst texture collapses away, fine enough to still
    /// resolve periods down to a few hundred ms.
    const EVENT_BIN_S: f64 = 0.05;

    /// Collapse a raw overshoot series into stall-EVENT onset timestamps:
    /// one rising-edge-after-a-debounce-gap per burst, however many
    /// densely-elevated samples that burst actually contains.
    pub(super) fn extract_event_onsets(
        samples: &[Sample],
        threshold_ms: f32,
        debounce_s: f64,
    ) -> Vec<f64> {
        let mut onsets = Vec::new();
        if samples.is_empty() {
            return onsets;
        }
        let mut above = false;
        // Pretend we've been quiet since before the series started, so a
        // burst right at the first sample still counts as a genuine onset.
        let mut below_since = samples[0].t - debounce_s;
        for s in samples {
            let is_above = s.overshoot_ms >= threshold_ms;
            if is_above {
                if !above && (s.t - below_since) >= debounce_s {
                    onsets.push(s.t);
                }
                above = true;
            } else {
                if above {
                    below_since = s.t;
                }
                above = false;
            }
        }
        onsets
    }

    /// Grid stall-event onsets onto a coarse uniform impulse series (1.0 in
    /// any bin containing an onset, 0.0 elsewhere) spanning
    /// `[span_start, span_end]`, shaped as a `Sample` series so the proven
    /// `analyze_periodicity` can run on it directly.
    pub(super) fn build_event_impulse_series(
        onsets: &[f64],
        span_start: f64,
        span_end: f64,
        bin_s: f64,
    ) -> Vec<Sample> {
        if bin_s <= 0.0 || span_end <= span_start {
            return Vec::new();
        }
        let n = ((span_end - span_start) / bin_s).floor() as usize + 1;
        let mut grid = vec![0.0f32; n.max(1)];
        for &t in onsets {
            if t < span_start {
                continue;
            }
            let b = (((t - span_start) / bin_s).floor() as usize).min(grid.len() - 1);
            grid[b] = 1.0;
        }
        grid.iter()
            .enumerate()
            .map(|(i, &v)| Sample { t: span_start + i as f64 * bin_s, overshoot_ms: v })
            .collect()
    }

    /// The stall's own clock, at the event scale: collapse bursts to onsets,
    /// grid them, and find the period of THAT — see the module comment
    /// above for why running `analyze_periodicity` on the raw per-tick
    /// series gets the wrong (intra-burst) answer.
    pub(super) fn event_clock(samples: &[Sample]) -> Periodicity {
        if samples.len() < 2 {
            return Periodicity::default();
        }
        let onsets = extract_event_onsets(samples, EVENT_THRESHOLD_MS, EVENT_DEBOUNCE_S);
        let impulse =
            build_event_impulse_series(&onsets, samples[0].t, samples[samples.len() - 1].t, EVENT_BIN_S);
        analyze_periodicity(&impulse, AnalysisConfig::default())
    }

    // ── Offender attribution (simplified) ───────────────────────────────
    //
    // Simplified from `diag::OffenderProbe` + `diag::analyze_offenders`:
    // CPU-jiffy-delta periodicity is the core signal we port; child-spawn
    // tracking is dropped per the brief ("skip if it bloats") since the
    // example already computes per-comm CPU deltas each resample and feeding
    // those in is exactly the cheap, no-second-/proc-scanner path the brief
    // asks for.

    const MIN_GROUP_SAMPLES: usize = 24; // matches diag.rs's analyze_offenders floor
    const OFFENDER_MIN_CONFIDENCE: f32 = 0.25; // matches diag.rs's MIN_CONF
    const PERIOD_MATCH_TOLERANCE: f64 = 0.25; // fractional tolerance vs the stall clock

    /// Find the process group whose CPU-delta activity is both convincingly
    /// periodic (via [`analyze_periodicity`]) and lands on `clock_period_s`
    /// — the stall's own clock. `None` when no group is a convincing match
    /// (honest "unattributed"), matching `diag::analyze_offenders`'s
    /// `MIN_CONF` gate.
    fn find_periodic_offender(
        group_history: &HashMap<String, VecDeque<(f64, u64)>>,
        clock_period_s: f64,
    ) -> Option<String> {
        if clock_period_s <= 0.0 {
            return None;
        }
        group_history
            .iter()
            .filter(|(_, hist)| hist.len() >= MIN_GROUP_SAMPLES)
            .filter_map(|(name, hist)| {
                let series: Vec<Sample> =
                    hist.iter().map(|&(t, d)| Sample { t, overshoot_ms: d as f32 }).collect();
                let p = analyze_periodicity(&series, AnalysisConfig::default());
                let period = p.period_s?;
                if p.confidence < OFFENDER_MIN_CONFIDENCE {
                    return None;
                }
                if ((period - clock_period_s).abs() / clock_period_s) > PERIOD_MATCH_TOLERANCE {
                    return None;
                }
                Some((name.clone(), p.confidence))
            })
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(name, _)| name)
    }

    // ── Detector: the interface 4b will consume ─────────────────────────

    /// One frame's worth of stall diagnosis, assembled from the latency
    /// probe, PSI, and offender attribution above.
    #[derive(Clone, Debug)]
    pub(super) struct DetectionReport {
        /// 0..1 weather intensity from recent max overshoot (decays); 0 when calm.
        pub pulse: f32,
        /// True when a stall is currently felt (pulse above a threshold).
        pub active: bool,
        /// Detected clock; `None` = irregular / no clear period.
        pub period_s: Option<f32>,
        /// Recent max overshoot, in ms.
        pub magnitude_ms: f32,
        /// Contended resource from PSI during the stall; `None` if PSI is
        /// unavailable or nothing is currently elevated.
        pub resource: Option<Resource>,
        /// Periodic-offender group name; `None` = unattributed.
        pub culprit_comm: Option<String>,
        /// Recent (t_secs, overshoot_ms) pairs for the bedrock timeline.
        pub overshoot_series: Vec<(f64, f32)>,
    }

    const PULSE_DECAY_S: f32 = 1.5; // time constant for the weather-intensity decay
    const PULSE_LOOKBACK_S: f32 = PULSE_DECAY_S * 6.0; // ~7 half-lives; negligible beyond this
    /// Overshoot that saturates the weather pulse to ~1.0.
    ///
    /// Additional STEP-0 finding (iteration 4d), beyond the event-onset
    /// threshold: the old 20.0ms norm assumed a real stall shows up as
    /// tens-of-ms overshoot. On the box used for the real induced-stall
    /// re-proof, idle/ambient overshoot (890 background tasks, no induced
    /// stall) sat at a steady ~0.2-0.4ms, while every real 3.0s/250ms burst
    /// peaked at ~1.0-5.7ms — a clean >4x separation, but one the old
    /// 20.0ms norm mostly couldn't SEE: `active`'s pulse gate
    /// (`overshoot_ms / PULSE_NORM_MS > PULSE_ACTIVE_THRESHOLD`) rarely
    /// crossed 0.15 (needs ≥3.0ms at the old norm), leaving fix 3/4 (period
    /// debounce + hold extension) almost nothing to work with — `active`
    /// has to fire at least occasionally for a hold to have something to
    /// extend. Lowered to sit inside the observed real gap (idle ~0.4ms ->
    /// ratio 0.07, comfortably below threshold; burst floor ~1.6ms -> ratio
    /// 0.27, comfortably above it) so `active` fires on real bursts of this
    /// magnitude without false-triggering on ordinary background jitter.
    const PULSE_NORM_MS: f32 = 6.0;
    const PULSE_ACTIVE_THRESHOLD: f32 = 0.15;
    const MAGNITUDE_WINDOW_S: f64 = 1.0; // window for "recent max overshoot"
    const SERIES_WINDOW_S: f64 = 30.0; // bedrock timeline span

    /// Owns the probe thread(s) and the rolling per-group activity history.
    /// Spawn once; feed it observations each frame; read a report whenever
    /// the render path wants one (not wired up until 4b).
    pub(super) struct Detector {
        probe: LatencyProbe,
        /// Per-group CPU-jiffy-delta history, fed by [`Detector::observe`]
        /// from the example's own per-comm deltas — no second /proc scanner.
        group_history: HashMap<String, VecDeque<(f64, u64)>>,
        group_cap: usize,
        /// Previous (cpu, mem, io) cumulative PSI "some" totals + when, to
        /// derive per-second stall rates on each `observe`.
        psi_prev: Option<(u64, u64, u64, f64)>,
        /// Most recently identified contended resource. Sticky across PSI
        /// reads that come back "nothing elevated" so a brief dip doesn't
        /// erase the reading mid-stall; only a hard PSI-unavailable clears it.
        resource: Option<Resource>,
    }

    impl Detector {
        /// Start the real latency-probe thread (+ read PSI on demand).
        pub fn spawn() -> Self {
            Detector {
                probe: LatencyProbe::spawn(ProbeConfig::default()),
                group_history: HashMap::new(),
                group_cap: 300, // ~5 min at the example's ~1 Hz resample cadence
                psi_prev: None,
                resource: None,
            }
        }

        /// Feed the latest per-group CPU-jiffy deltas (as the example
        /// already computes each `resample`) and refresh the PSI reading.
        pub fn observe(&mut self, now_secs: f64, group_cpu_deltas: &[(String, u64)]) {
            for (name, delta) in group_cpu_deltas {
                let ring = self.group_history.entry(name.clone()).or_default();
                if ring.len() >= self.group_cap {
                    ring.pop_front();
                }
                ring.push_back((now_secs, *delta));
            }

            // PSI: rate the three channels since the previous observe() and
            // remember whichever is currently most elevated. A read failure
            // (any file missing — no PSI on this kernel) clears `resource`
            // outright rather than reporting a stale one.
            match (
                read_psi_some_total("/proc/pressure/cpu"),
                read_psi_some_total("/proc/pressure/memory"),
                read_psi_some_total("/proc/pressure/io"),
            ) {
                (Some(cpu), Some(mem), Some(io)) => {
                    if let Some((pc, pm, pi, pt)) = self.psi_prev {
                        let dt = (now_secs - pt).max(1e-3);
                        let rate = |c: u64, p: u64| (c.saturating_sub(p) as f64 / dt) as f32;
                        let candidates = [
                            (PressureChannel::Cpu, rate(cpu, pc)),
                            (PressureChannel::Mem, rate(mem, pm)),
                            (PressureChannel::Io, rate(io, pi)),
                        ];
                        let best = candidates
                            .into_iter()
                            .fold((PressureChannel::Cpu, 0.0f32), |acc, x| if x.1 > acc.1 { x } else { acc });
                        self.resource = if best.1 > 0.0 { Some(best.0.resource()) } else { None };
                    }
                    self.psi_prev = Some((cpu, mem, io, now_secs));
                }
                _ => {
                    self.psi_prev = None;
                    self.resource = None;
                }
            }
        }

        /// STEP-0 diagnostic scaffolding (iteration 4d), gated behind
        /// `--probe-debug`: dumps the raw elevated-tick overshoot
        /// magnitudes, the extracted event-onset timestamps (and their
        /// spacing), and the resulting event-clock period to stderr for a
        /// window of recent history. Exists to let a real induced stall's
        /// numbers be READ (not guessed at) to explain why the displayed
        /// period was drifting to a 3x supra-harmonic. Temporary; kept
        /// cleanly gated rather than ripped out so the next regression can
        /// be diagnosed the same way.
        pub fn debug_dump(&self, now_secs: f64, window_s: f64) {
            let samples = self.probe.snapshot();
            let recent: Vec<Sample> =
                samples.iter().copied().filter(|s| now_secs - s.t <= window_s).collect();
            if recent.len() < 2 {
                eprintln!("[probe-debug] t={now_secs:.2} not enough samples yet ({})", recent.len());
                return;
            }
            let elevated: Vec<(f64, f32)> = recent
                .iter()
                .filter(|s| s.overshoot_ms > 1.0)
                .map(|s| (s.t, s.overshoot_ms))
                .collect();
            eprintln!(
                "[probe-debug] t={now_secs:.2} window={window_s}s samples={} elevated_ticks={}",
                recent.len(),
                elevated.len()
            );
            for (t, ms) in &elevated {
                eprintln!("[probe-debug]   tick t={t:.3} overshoot={ms:.1}ms");
            }
            let onsets = extract_event_onsets(&recent, EVENT_THRESHOLD_MS, EVENT_DEBOUNCE_S);
            let onset_strs: Vec<String> = onsets.iter().map(|t| format!("{t:.2}")).collect();
            eprintln!("[probe-debug] onsets ({}): {:?}", onsets.len(), onset_strs);
            if onsets.len() >= 2 {
                let gaps: Vec<String> =
                    onsets.windows(2).map(|w| format!("{:.2}", w[1] - w[0])).collect();
                eprintln!("[probe-debug] onset gaps (s): {:?}", gaps);
            }
            let clock = event_clock(&recent);
            eprintln!(
                "[probe-debug] event_clock period_s={:?} confidence={:.2}",
                clock.period_s, clock.confidence
            );
        }

        /// Assemble the current diagnosis. Pure w.r.t. `self` — recomputes
        /// pulse/period/culprit from the probe snapshot and group history
        /// each call rather than caching, since 4a doesn't yet call this
        /// every render frame.
        pub fn report(&self, now_secs: f64) -> DetectionReport {
            let samples = self.probe.snapshot();

            let magnitude_ms = samples
                .iter()
                .rev()
                .take_while(|s| now_secs - s.t <= MAGNITUDE_WINDOW_S)
                .map(|s| s.overshoot_ms)
                .fold(0.0f32, f32::max);

            // Pulse: an exponentially-decayed "weather intensity" driven by
            // every recent overshoot (not just the latest tick), so a spike
            // is still felt for a moment after it passes.
            let pulse = samples
                .iter()
                .rev()
                .take_while(|s| now_secs - s.t <= PULSE_LOOKBACK_S as f64)
                .map(|s| {
                    let age = (now_secs - s.t).max(0.0) as f32;
                    (s.overshoot_ms / PULSE_NORM_MS).clamp(0.0, 1.0) * (-age / PULSE_DECAY_S).exp()
                })
                .fold(0.0f32, f32::max);

            // The stall's own clock: periodicity of stall-EVENT onsets, not
            // the raw per-tick series (see `event_clock`'s doc comment —
            // the raw series' intra-burst texture drowns the true clock).
            let clock = event_clock(&samples);
            let culprit_comm =
                clock.period_s.and_then(|cp| find_periodic_offender(&self.group_history, cp));

            let mut overshoot_series: Vec<(f64, f32)> = samples
                .iter()
                .rev()
                .take_while(|s| now_secs - s.t <= SERIES_WINDOW_S)
                .map(|s| (s.t, s.overshoot_ms))
                .collect();
            overshoot_series.reverse(); // oldest -> newest, for the timeline

            DetectionReport {
                pulse,
                active: pulse > PULSE_ACTIVE_THRESHOLD,
                period_s: clock.period_s.map(|p| p as f32),
                magnitude_ms,
                resource: self.resource,
                culprit_comm,
                overshoot_series,
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn mk(series: &[(f64, f32)]) -> Vec<Sample> {
            series.iter().map(|&(t, o)| Sample { t, overshoot_ms: o }).collect()
        }

        /// Build a synthetic spike train: a baseline tick with a tall spike
        /// every `period_s`, sampled at `tick_s`. Ported from diag.rs's test helper.
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

        /// THE key correctness gate: `analyze_periodicity` must recover a
        /// known period from a synthetic periodic overshoot series. Ported
        /// verbatim from `diag.rs`'s `recovers_known_period`.
        #[test]
        fn recovers_known_period() {
            // Spike every 2.0 s, 2 ms ticks, 60 s long.
            let s = spike_train(0.002, 2.0, 60.0);
            let p = analyze_periodicity(&s, AnalysisConfig::default());
            let period = p.period_s.expect("should find a period");
            assert!((period - 2.0).abs() < 0.1, "recovered period {period}");
            assert!(p.confidence > 0.4, "confidence {} too low", p.confidence);
        }

        /// Ported verbatim from `diag.rs`'s `recovers_fast_period`.
        #[test]
        fn recovers_fast_period() {
            // Spike every 0.25 s (4 Hz).
            let s = spike_train(0.002, 0.25, 30.0);
            let p = analyze_periodicity(&s, AnalysisConfig::default());
            let f = p.freq_hz.expect("should find a frequency");
            assert!((f - 4.0).abs() < 0.3, "recovered freq {f} Hz");
        }

        /// Build a synthetic *bursty* stall series that models a REAL
        /// scheduler stall, not a clean spike: every `period_s`, overshoot
        /// is elevated in a `burst_s`-long window made of many discrete
        /// narrow sub-spikes (repeated "wakeup was late" events) spaced
        /// `intra_spacing_s` apart — the intra-burst texture a raw
        /// autocorrelation locks onto — separated by long calm stretches at
        /// the true `period_s` clock.
        fn bursty_stall_train(
            tick_s: f64,
            period_s: f64,
            burst_s: f64,
            intra_spacing_s: f64,
            dur_s: f64,
        ) -> Vec<Sample> {
            let n = (dur_s / tick_s) as usize;
            (0..n)
                .map(|i| {
                    let t = i as f64 * tick_s;
                    let phase = t % period_s;
                    let in_burst = phase < burst_s && (phase % intra_spacing_s) < tick_s * 1.5;
                    let o = if in_burst { 20.0 } else { 0.2 };
                    Sample { t, overshoot_ms: o }
                })
                .collect()
        }

        /// THE key correctness gate for fix 1. With only 5 repeats of the
        /// true 3.0s clock in a 15s window but 50 repeats/cycle of a 20ms
        /// intra-burst spike spacing, the raw (unfixed) autocorrelation's
        /// correlation estimate for the sparse 3s clock is weak enough that
        /// it locks onto the intra-burst spacing instead — reproducing the
        /// real reported bug (period ~0.06s, i.e. "~0.1s") verbatim. The
        /// event-collapsed clock (`event_clock`) must recover the true
        /// ~3.0s inter-burst period from the exact same series.
        #[test]
        fn event_clock_locks_onto_inter_burst_period_not_intra_burst_texture() {
            // 250ms dense burst (20ms-spaced sub-spikes, ~13 per burst) every
            // 3.0s, 2ms probe tick, 15s long -> 5 bursts.
            let s = bursty_stall_train(0.002, 3.0, 0.25, 0.02, 15.0);

            // Reproduce the bug: raw analyze_periodicity over the per-tick
            // series locks onto the intra-burst ~20ms-scale harmonic, not
            // the true 3.0s clock.
            let raw = analyze_periodicity(&s, AnalysisConfig::default());
            let raw_period = raw.period_s.expect("raw series should still find *a* period");
            assert!(
                raw_period < 0.5,
                "test setup check: raw period {raw_period} should reproduce the intra-burst-lock bug (~0.1s), \
                 not the true 3.0s clock — otherwise this input no longer demonstrates the bug"
            );

            // The fix: event-collapse first, then find the period of THAT.
            let clock = event_clock(&s);
            let period = clock.period_s.expect("event clock should find a period");
            assert!((period - 3.0).abs() < 0.5, "event-clock period {period}, expected ~3.0s");
        }

        /// THE key correctness gate for iteration 4d, fix 1. STEP-0
        /// diagnosis on a real 16-core box under a real induced 3.0s/250ms
        /// stall (41 phase-aligned burners) read via `--probe-debug`: the
        /// probe overshot on EVERY single burst (a clean 3.0s onset train:
        /// 0.85, 3.85, 6.85, 9.87, 12.87, 15.88, 18.86), but the per-burst
        /// PEAK was only ~1.0-5.7ms (typically 1.3-2.9ms) — nowhere near
        /// the tens-of-ms the old fixed 4.0ms `EVENT_THRESHOLD_MS` assumed.
        /// That threshold cleared just 1 of those 7 real bursts, leaving a
        /// sparse onset train that (correctly, but uselessly) resolved to
        /// a ~9s supra-harmonic. This test reproduces the diagnosed shape
        /// with the exact observed peak magnitudes and demonstrates both
        /// the bug (at the old threshold) and the fix (at the new one).
        #[test]
        fn event_clock_catches_modest_real_burst_peaks_not_just_the_rare_tall_one() {
            // Peaks (ms) observed per-burst in the real STEP-0 diagnosis,
            // one per 3.0s-spaced burst, over a 21s window (7 bursts).
            const OLD_THRESHOLD_MS: f32 = 4.0;
            let peaks = [2.3f32, 4.0, 2.5, 1.6, 5.7, 4.0, 2.3];
            let tick_s = 0.002;
            let period_s = 3.0;
            let dur_s = period_s * peaks.len() as f64;
            let n = (dur_s / tick_s) as usize;
            let samples: Vec<Sample> = (0..n)
                .map(|i| {
                    let t = i as f64 * tick_s;
                    let burst_idx = (t / period_s).floor() as usize;
                    let phase = t % period_s;
                    // A brief (~30ms) elevated window per burst, peaking at
                    // that burst's diagnosed magnitude — modeling how few
                    // real ticks within a 250ms burst actually cleared even
                    // a low ms-scale gate on this box.
                    let o = if phase < 0.03 && burst_idx < peaks.len() { peaks[burst_idx] } else { 0.2 };
                    Sample { t, overshoot_ms: o }
                })
                .collect();

            // Reproduce the bug: the OLD threshold drops most of these
            // modest, real-shaped bursts.
            let old_onsets = extract_event_onsets(&samples, OLD_THRESHOLD_MS, EVENT_DEBOUNCE_S);
            assert!(
                old_onsets.len() <= 3,
                "test setup check: the old {OLD_THRESHOLD_MS}ms threshold should drop most of \
                 these modest real-shaped bursts (got {} of {} onsets) — otherwise this no \
                 longer demonstrates the diagnosed bug",
                old_onsets.len(),
                peaks.len()
            );

            // The fix: the new (lower) EVENT_THRESHOLD_MS catches every
            // single burst -> a clean 3.0s onset train, not a subsampled one.
            let onsets = extract_event_onsets(&samples, EVENT_THRESHOLD_MS, EVENT_DEBOUNCE_S);
            assert_eq!(
                onsets.len(),
                peaks.len(),
                "should catch every one of the {} real bursts, not just the rare tall one",
                peaks.len()
            );
            for w in onsets.windows(2) {
                let gap = w[1] - w[0];
                assert!(
                    (gap - period_s).abs() < 0.2,
                    "onset gap {gap} should be ~3.0s, not a supra-harmonic multiple of it"
                );
            }

            let clock = event_clock(&samples);
            let period = clock.period_s.expect("event clock should find a period");
            assert!(
                (period - period_s).abs() < 0.5,
                "event-clock period {period}, expected ~3.0s (not a 9s supra-harmonic)"
            );
        }

        /// Fix 2 (safety net) correctness gate: even when event extraction
        /// misses an occasional burst (not systematically every Nth, just a
        /// couple of real-world drops), the fundamental must still win over
        /// a supra-harmonic that the sparser onset pattern could otherwise
        /// make look comparably strong.
        #[test]
        fn event_clock_prefers_fundamental_over_supra_harmonic_with_occasional_misses() {
            // True 3.0s clock, 9 onsets, but drop 2 of them (indices 2 and
            // 5) to model occasional real-world misses rather than a clean
            // train — the scenario fix 1 alone doesn't fully guarantee away.
            let all_onsets: Vec<f64> = (0..9).map(|i| i as f64 * 3.0).collect();
            let onsets: Vec<f64> = all_onsets
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != 2 && *i != 5)
                .map(|(_, &t)| t)
                .collect();
            let impulse = build_event_impulse_series(&onsets, 0.0, 26.0, EVENT_BIN_S);
            let p = analyze_periodicity(&impulse, AnalysisConfig::default());
            let period = p.period_s.expect("should find a period");
            assert!(
                (period - 3.0).abs() < 0.5,
                "period {period}, expected the fundamental ~3.0s even with occasional missed onsets, \
                 not a supra-harmonic (~6s/~9s)"
            );
        }

        /// Ported verbatim from `diag.rs`'s `flat_series_has_no_period`.
        #[test]
        fn flat_series_has_no_period() {
            let s: Vec<Sample> =
                (0..10_000).map(|i| Sample { t: i as f64 * 0.002, overshoot_ms: 0.2 }).collect();
            let p = analyze_periodicity(&s, AnalysisConfig::default());
            assert!(p.period_s.is_none(), "flat series should not report a period");
        }

        /// Ported verbatim from `diag.rs`'s `resample_keeps_spikes`.
        #[test]
        fn resample_keeps_spikes() {
            // A lone spike between two calm samples must survive max-binning.
            let s = mk(&[(0.0, 0.1), (0.05, 30.0), (0.10, 0.1), (0.15, 0.1), (0.20, 0.1)]);
            let grid = resample_uniform(&s, 0.05);
            assert!(grid.len() >= 4);
            assert_eq!(grid[1], 30.0, "spike must land in its bin");
            assert_eq!(grid[0], 0.1);
        }

        /// Offender attribution: a group whose CPU-delta activity is
        /// periodic at the same period as the stall clock gets flagged.
        /// Parameters mirror diag.rs's `offender_detects_periodic_spawner`
        /// (300 samples @ 0.2 s, burst every 15 ticks = 3.0 s).
        #[test]
        fn finds_periodic_offender_matching_clock() {
            let mut groups: HashMap<String, VecDeque<(f64, u64)>> = HashMap::new();
            let poller: VecDeque<(f64, u64)> =
                (0..300).map(|i| (i as f64 * 0.2, if i % 15 == 0 { 50 } else { 0 })).collect();
            let steady: VecDeque<(f64, u64)> = (0..300).map(|i| (i as f64 * 0.2, 5)).collect();
            groups.insert("poller".to_string(), poller);
            groups.insert("steady".to_string(), steady);

            let culprit = find_periodic_offender(&groups, 3.0);
            assert_eq!(culprit.as_deref(), Some("poller"));
        }

        /// No group periodic at the clock's period -> honest "unattributed".
        #[test]
        fn no_offender_when_nothing_matches_clock() {
            let mut groups: HashMap<String, VecDeque<(f64, u64)>> = HashMap::new();
            let steady: VecDeque<(f64, u64)> = (0..300).map(|i| (i as f64 * 0.2, 5)).collect();
            groups.insert("steady".to_string(), steady);

            assert_eq!(find_periodic_offender(&groups, 3.0), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(pid: u32, ppid: u32, comm: &str, cpu: u64, rss: u64) -> ProcSample {
        ProcSample { pid, ppid, comm: comm.into(), cpu_jiffies: cpu, rss_pages: rss, blkio_ticks: 0 }
    }

    #[test]
    fn parses_stat_with_parens_in_comm() {
        // Real-shaped line: comm "(a b)c" contains spaces and parens.
        // fields: 1 pid, 2 comm, 3 state, 4 ppid, ... 14 utime, 15 stime,
        // ... 42 delayacct_blkio_ticks (rest[39], padded out with trailing
        // fields so rest.len() >= 40 and the blkio field is present).
        let line = "1234 ((a b)c) S 1000 1234 1234 0 -1 0 0 0 0 0 40 60 0 0 20 0 1 \
                     0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 777";
        let s = parse_stat(line).expect("should parse");
        assert_eq!(s.pid, 1234);
        assert_eq!(s.ppid, 1000);
        assert_eq!(s.comm, "(a b)c");
        assert_eq!(s.cpu_jiffies, 100); // utime 40 + stime 60
        assert_eq!(s.blkio_ticks, 777);
    }

    #[test]
    fn parses_stat_defaults_blkio_when_field_absent() {
        // Same shape as before iteration 3 (short rest[]): blkio should
        // default to 0 rather than failing the whole parse.
        let line = "1234 ((a b)c) S 1000 1234 1234 0 -1 0 0 0 0 0 40 60 0 0 20 0 1 0";
        let s = parse_stat(line).expect("should parse");
        assert_eq!(s.blkio_ticks, 0);
    }

    #[test]
    fn rejects_garbage_stat() {
        assert!(parse_stat("not a stat line").is_none());
    }

    #[test]
    fn parses_statm_resident() {
        // size resident shared ... — we want the 2nd field.
        assert_eq!(parse_statm_resident("5000 1234 200 10 0 300 0"), Some(1234));
        assert_eq!(parse_statm_resident("bad"), None);
    }

    #[test]
    fn comm_ids_are_stable() {
        let mut ids = CommIds::new();
        let a1 = ids.id("bash");
        let b = ids.id("cat");
        let a2 = ids.id("bash");
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
    }

    #[test]
    fn build_graph_groups_and_edges() {
        // shell(1) -> worker(2), worker(2) -> worker(3) [self-edge dropped],
        // worker(3) -> tool(4). Two "worker" procs merge into one node.
        let samples = vec![
            sample(1, 0, "shell", 10, 100),
            sample(2, 1, "worker", 20, 200),
            sample(3, 2, "worker", 5, 50),
            sample(4, 3, "tool", 7, 70),
        ];
        let mut ids = CommIds::new();
        let g = build_graph(&samples, &mut ids);

        // 3 distinct comms -> 3 nodes; worker aggregates cpu 20+5, rss 200+50.
        assert_eq!(g.nodes.len(), 3);
        let worker = g.nodes.iter().find(|n| n.comm == "worker").unwrap();
        assert_eq!(worker.cpu_jiffies, 25);
        assert_eq!(worker.rss_pages, 250);

        let shell = ids.id("shell");
        let worker_id = ids.id("worker");
        let tool = ids.id("tool");
        // Edges: shell->worker and worker->tool; the worker->worker self-edge is gone.
        assert!(g.edges.contains(&(shell, worker_id)));
        assert!(g.edges.contains(&(worker_id, tool)));
        assert!(!g.edges.iter().any(|&(a, b)| a == b));
        assert_eq!(g.edges.len(), 2);
    }

    #[test]
    fn build_graph_caps_to_max_nodes() {
        // MAX_NODES + 10 distinct comms in a parent -> child chain, each with a
        // distinct (and strictly decreasing) cpu_jiffies so the top-MAX_NODES
        // significance cut is unambiguous: p0 (highest cpu) .. p{N-1} (lowest).
        let total = MAX_NODES + 10;
        let samples: Vec<ProcSample> = (0..total)
            .map(|i| {
                let pid = i as u32 + 1;
                let ppid = if i == 0 { 0 } else { pid - 1 };
                sample(pid, ppid, &format!("p{i}"), (total - i) as u64, 10)
            })
            .collect();
        let mut ids = CommIds::new();
        let g = build_graph(&samples, &mut ids);

        // Capped to MAX_NODES, keeping the highest-cpu comms (p0..p{MAX_NODES-1}).
        assert_eq!(g.nodes.len(), MAX_NODES);
        for i in 0..MAX_NODES {
            assert!(g.nodes.iter().any(|n| n.comm == format!("p{i}")), "p{i} should survive the cut");
        }
        for i in MAX_NODES..total {
            assert!(!g.nodes.iter().any(|n| n.comm == format!("p{i}")), "p{i} should be culled");
        }

        // Final node order is still by id (deterministic layout order).
        let ids_seq: Vec<TileId> = g.nodes.iter().map(|n| n.id).collect();
        let mut sorted_ids = ids_seq.clone();
        sorted_ids.sort();
        assert_eq!(ids_seq, sorted_ids, "surviving nodes must stay sorted by id");

        // Edges are filtered to survivors only: the chain has `total - 1` edges,
        // but the edge crossing from the last survivor to the first culled node
        // (p{MAX_NODES-1} -> p{MAX_NODES}) must be dropped, along with every
        // edge further down the chain among culled nodes.
        assert_eq!(g.edges.len(), MAX_NODES - 1);
        let survivors: HashSet<TileId> = ids_seq.into_iter().collect();
        for &(p, c) in &g.edges {
            assert!(survivors.contains(&p) && survivors.contains(&c), "edge endpoints must both survive");
        }
    }

    #[test]
    fn encode_size_is_monotonic_in_cpu() {
        let lo = encode_node(0.0, 0.0, 0.0);
        let mid = encode_node(0.5, 0.0, 0.0);
        let hi = encode_node(1.0, 0.0, 0.0);
        assert!(lo.cells >= 3, "min node stays legible");
        assert!(mid.cells > lo.cells);
        assert!(hi.cells > mid.cells);
    }

    #[test]
    fn encode_heat_endpoints_differ() {
        let cool = encode_node(0.0, 0.0, 0.0).color;
        let hot = encode_node(0.0, 1.0, 0.0).color;
        assert_ne!(cool, hot);
    }

    #[test]
    fn encode_pulse_is_clamped() {
        assert_eq!(encode_node(0.0, 0.0, -1.0).pulse, 0.0);
        assert_eq!(encode_node(0.0, 0.0, 2.0).pulse, 1.0);
    }

    #[test]
    fn injector_peaks_on_period() {
        let inj = Injector::new(); // period 3s
        let at_peak = inj.intensity(6.0);   // exact multiple
        let between = inj.intensity(7.5);    // half a period away
        assert!(at_peak > 0.9, "peak near a multiple of the period");
        assert!(between < 0.1, "quiet between peaks");
    }

    #[test]
    fn injector_intensity_in_unit_range() {
        let inj = Injector::new();
        for i in 0..200 {
            let v = inj.intensity(i as f32 * 0.05);
            assert!((0.0..=1.0).contains(&v));
        }
    }

    use mullion::Rect as MRect;

    fn tiny_constellation() -> (Constellation, u64) {
        // shell -> worker -> tool, plus an isolated daemon.
        let samples = vec![
            sample(1, 0, "shell", 10, 100),
            sample(2, 1, "worker", 30, 200),
            sample(3, 2, "tool", 5, 40),
            sample(9, 0, "daemon", 8, 60),
        ];
        let mut ids = CommIds::new();
        let g = build_graph(&samples, &mut ids);
        let cpu_max = g.nodes.iter().map(|n| n.cpu_jiffies).max().unwrap_or(1);
        (g, cpu_max)
    }

    #[test]
    fn layout_is_stable_across_identical_frames() {
        let (g, cpu_max) = tiny_constellation();
        let window = MRect::new(0, 0, 120, 40);
        let a = placed_rects(&build_canvas(&g, cpu_max), window);
        let b = placed_rects(&build_canvas(&g, cpu_max), window);
        // Same ids, sizes, edges -> identical placement (auto_layout is idempotent).
        assert_eq!(a, b, "an unchanged graph must not move between frames");
        assert_eq!(a.len(), 4);
    }

    #[test]
    fn contention_edges_star_per_resource_with_threshold() {
        // Five nodes with known per-resource values:
        //   cpu:  1=0.9 (hub), 2=0.6, 3=0.4, 4=0.3 (4 qualify, but K=3 caps
        //         the star to hub + top-2 spokes), 5=0.01 (below threshold)
        //     -> star from 1 to {2, 3}; node 4 is excluded by the K=3 cap
        //        even though it clears the threshold, and node 5 is excluded
        //        by the threshold itself.
        //   mem:  1=0.9 only heavy node, everyone else below threshold
        //     -> fewer than 2 qualify: no mem edges at all.
        //   disk: all nodes below threshold -> no disk edges at all.
        let ids: Vec<TileId> = vec![1, 2, 3, 4, 5];
        let cpu: HashMap<TileId, f32> =
            [(1, 0.9), (2, 0.6), (3, 0.4), (4, 0.3), (5, 0.01)].into_iter().collect();
        let mem: HashMap<TileId, f32> =
            [(1, 0.9), (2, 0.02), (3, 0.01), (4, 0.0), (5, 0.0)].into_iter().collect();
        let disk: HashMap<TileId, f32> =
            [(1, 0.02), (2, 0.01), (3, 0.0), (4, 0.0), (5, 0.0)].into_iter().collect();

        let edges = contention_edges(&ids, &cpu, &mem, &disk);

        // Only the CPU axis has >=2 qualifying nodes; hub is node 1 (highest).
        assert!(!edges.iter().any(|(_, _, r)| *r == Resource::Mem), "mem: <2 heavy nodes -> no edges");
        assert!(!edges.iter().any(|(_, _, r)| *r == Resource::Disk), "disk: <2 heavy nodes -> no edges");
        let cpu_edges: Vec<(TileId, TileId)> =
            edges.iter().filter(|(_, _, r)| *r == Resource::Cpu).map(|(a, b, _)| (*a, *b)).collect();
        assert_eq!(cpu_edges.len(), 2, "cpu star (K=3): hub -> 2 and hub -> 3 only");
        assert!(cpu_edges.contains(&(1, 2)));
        assert!(cpu_edges.contains(&(1, 3)));
        assert!(!cpu_edges.iter().any(|&(a, b)| a == 4 || b == 4), "K=3 cap excludes node 4 despite clearing threshold");
        assert!(!cpu_edges.iter().any(|&(a, b)| a == 5 || b == 5), "below-threshold node 5 excluded");
    }

    #[test]
    fn auto_follow_culprit_tracks_until_user_takes_explicit_control() {
        // Bug 1: Enter must dive the actual culprit during a stall, not
        // merely the highest-significance node. The fix keeps `focus`
        // auto-following the culprit every resample while the stall is
        // on-screen and the user hasn't arrowed away — not just once on the
        // rising edge — because real attribution can lag a sample or two
        // behind the stall itself first showing as active/"unattributed".
        let culprit: Option<TileId> = Some(7);

        // Active, no explicit user pick -> follow the culprit.
        assert_eq!(auto_follow_culprit(true, false, culprit), culprit);

        // Still active a sample later, still no explicit pick, culprit just
        // became known (was None on an earlier frame) -> still follows.
        assert_eq!(auto_follow_culprit(true, false, culprit), culprit);

        // Not active -> nothing to follow.
        assert_eq!(auto_follow_culprit(false, false, culprit), None);

        // User has arrowed (explicit) -> leave focus alone even though the
        // stall is still active and the culprit is known.
        assert_eq!(auto_follow_culprit(true, true, culprit), None);

        // Active, no explicit pick, but culprit not yet attributed -> leave
        // focus alone (nothing to follow yet).
        assert_eq!(auto_follow_culprit(true, false, None), None);
    }
}
