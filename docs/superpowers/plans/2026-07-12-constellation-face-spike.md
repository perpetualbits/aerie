# Constellation Face Spike — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a throwaway standalone prototype, `examples/constellation.rs`, that renders live `/proc` process-groups as a semantically-zoomable constellation, to learn whether a live graph with semantic zoom feels like a good primary face for aerie.

**Architecture:** A single self-contained `[[bin]]` (same pattern as `examples/spiral_stress.rs`), depending only on `mullion` + `crossterm` + std — it cannot import aerie's binary crate, so all logic lives in this one file. Pure logic (proc parsing, graph build, node encoding, the stall injector, stable id assignment) is unit-tested inline; the render loop, semantic zoom, edge routing, and the stall arc are integration pieces verified by a precise manual on-screen checklist. Layout stability rides on `mullion::sugiyama::auto_layout`'s documented idempotence (placement depends only on node ids, sizes, and edges — never current positions).

**Tech Stack:** Rust (edition 2021, rustc ≥ 1.85), `mullion` (path dep) modules `graph` (`GraphCanvas`, `Viewport`), `sugiyama`, `zoom` (`Zoom`, `Lod`, `FocusTarget`, `lerp_rect`), `route`, `field`, `style`, `ease`; `crossterm` for events.

## Global Constraints

- Rust edition `2021`, `rust-version = "1.85"` — do not use newer-edition-only syntax.
- Dependencies: **only** `mullion` (path `../mullion`), `crossterm`, and std. Add **no** new crates. No `libc` — hardcode nothing that needs it (metrics are relative/normalized, so page size cancels; use raw page counts and CPU jiffies directly).
- The binary is **standalone**: it must not reference any aerie module (aerie is a binary crate with no lib target; imports would not resolve anyway).
- **Domain-agnostic labeling** (standing project rule): nodes are labeled by neutral system facts only — the process `comm`, or a resource category. Never embed product names, desktop/app-specific knowledge, or remediation hints. The materializing culprit is labeled by *what it is on the system*, not by any product guess.
- Register as `[[bin]]` named `constellation` with `path = "examples/constellation.rs"`, mirroring the existing `spiral_stress` entry. Run with `cargo run --bin constellation`; unit-test with `cargo test --bin constellation`.
- Follow the proven terminal scaffold from `examples/spiral_stress.rs:107-185` (`CrosstermBackend` + `Capabilities::detect()` + `Terminal::enter/draw/leave`, background `EventReader`, `input.drain()` each frame, self-paced sleep). Wrap the loop in a closure so `?` still falls through to `terminal.leave()`.

---

### Task 1: Scaffold the standalone binary + registration + run loop

**Files:**
- Create: `examples/constellation.rs`
- Modify: `Cargo.toml` (add a `[[bin]]` entry after the existing `spiral_stress` one)

**Interfaces:**
- Produces: `struct State` with `fn new(stall: bool) -> Self`, `fn advance(&mut self, dt: f32)`, `fn render(&self, buf: &mut Buffer)`; a `main() -> Result<()>` entry running the frame loop and handling `q`/`Esc`/`Ctrl-C` to quit and `--stall`/`--help` argument parsing.

- [ ] **Step 1: Register the bin in Cargo.toml**

Add directly below the existing `spiral_stress` `[[bin]]` block:

```toml
[[bin]]
name = "constellation"
path = "examples/constellation.rs"
```

- [ ] **Step 2: Write the scaffold file**

```rust
// SPDX-License-Identifier: GPL-3.0-or-later
//
// constellation — spike for aerie's unifying "constellation face".
//
// Live /proc process-groups become nodes in one semantically-zoomable graph:
// backbone edges are lineage (parent comm -> child comm), node size = CPU,
// heat = memory, pulse = strain. A --stall injector fakes a periodic system
// stall so we can feel the notice -> materialize -> dive arc before wiring the
// real Instruments subsystem. Standalone: mullion + crossterm only.
//
// Run:  cargo run --bin constellation [--stall]
// Keys: q / Esc / Ctrl-C  quit

use anyhow::Result;
use crossterm::event::Event;
use mullion::backend::CrosstermBackend;
use mullion::capabilities::Capabilities;
use mullion::input::{KeyCode, KeyModifiers};
use mullion::style::{Color, Style};
use mullion::{Buffer, EventReader, Rect, Terminal};
use std::io;
use std::time::{Duration, Instant};

const FRAME: Duration = Duration::from_millis(33); // ~30 fps

const HELP: &str = "\
constellation — aerie unifying-face spike

USAGE: constellation [--stall]
  --stall   drive the fake periodic-stall injector on startup
  -h,--help show this help

KEYS: q/Esc quit\n";

struct State {
    stall: bool,
    t: f32, // seconds since start
}

impl State {
    fn new(stall: bool) -> Self {
        State { stall, t: 0.0 }
    }

    fn advance(&mut self, dt: f32) {
        self.t += dt;
    }

    fn render(&self, buf: &mut Buffer) {
        let area = buf.area;
        let msg = format!("constellation spike — t={:.1}s stall={}", self.t, self.stall);
        buf.set_string(area.x + 1, area.y, &msg, Style::default().fg(Color::White));
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP}");
        return Ok(());
    }
    let stall = args.iter().any(|a| a == "--stall");

    let mut state = State::new(stall);
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
                        KeyCode::Char('q') | KeyCode::Esc => break 'frames,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break 'frames
                        }
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
```

- [ ] **Step 3: Build it**

Run: `cargo build --bin constellation`
Expected: compiles clean (warnings for unused imports `Rect`/`KeyModifiers` are fine at this stage; they are used in later tasks).

- [ ] **Step 4: Manual smoke run**

Run: `cargo run --bin constellation` in a real terminal (or `script -qec "stty rows 40 cols 120; ./target/debug/constellation" /dev/null`).
Expected: the alternate screen shows `constellation spike — t=…s stall=false`, the timer advances, and `q` restores your shell cleanly. `cargo run --bin constellation -- --help` prints usage and exits without entering the TUI.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml examples/constellation.rs
git commit -m "feat(spike): scaffold standalone constellation binary"
```

---

### Task 2: `/proc` sampling — parse stat/statm (TDD)

**Files:**
- Modify: `examples/constellation.rs`
- Test: inline `#[cfg(test)] mod tests` in the same file (run via `cargo test --bin constellation`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `struct ProcStat { pid: u32, ppid: u32, comm: String, cpu_jiffies: u64 }`
  - `fn parse_stat(line: &str) -> Option<ProcStat>` — parses one `/proc/PID/stat` line; robust to spaces/parens in `comm` by splitting on the **last** `)`.
  - `fn parse_statm_resident(line: &str) -> Option<u64>` — returns the resident-set page count (2nd whitespace field of `/proc/PID/statm`).
  - `struct ProcSample { pid: u32, ppid: u32, comm: String, cpu_jiffies: u64, rss_pages: u64 }`
  - `fn sample_procs() -> Vec<ProcSample>` — walks `/proc/<pid>/{stat,statm}` and pairs them (thin glue).

- [ ] **Step 1: Write the failing tests**

Add to a `#[cfg(test)] mod tests` block:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_with_parens_in_comm() {
        // Real-shaped line: comm "(a b)c" contains spaces and parens.
        // fields: 1 pid, 2 comm, 3 state, 4 ppid, ... 14 utime, 15 stime
        let line = "1234 ((a b)c) S 1000 1234 1234 0 -1 0 0 0 0 40 60 0 0 20 0 1 0";
        let s = parse_stat(line).expect("should parse");
        assert_eq!(s.pid, 1234);
        assert_eq!(s.ppid, 1000);
        assert_eq!(s.comm, "(a b)c");
        assert_eq!(s.cpu_jiffies, 100); // utime 40 + stime 60
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
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin constellation`
Expected: FAIL — `cannot find function parse_stat` / `parse_statm_resident`.

- [ ] **Step 3: Implement the parsers and sampler**

```rust
#[derive(Debug, Clone)]
struct ProcStat {
    pid: u32,
    ppid: u32,
    comm: String,
    cpu_jiffies: u64,
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
    //   state=0, ppid=1, ... utime=11, stime=12
    let rest: Vec<&str> = line[close + 1..].split_whitespace().collect();
    if rest.len() < 13 {
        return None;
    }
    let ppid: u32 = rest[1].parse().ok()?;
    let utime: u64 = rest[11].parse().ok()?;
    let stime: u64 = rest[12].parse().ok()?;
    Some(ProcStat { pid, ppid, comm, cpu_jiffies: utime + stime })
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
        });
    }
    out
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin constellation`
Expected: PASS (3 tests). `sample_procs` is exercised in later tasks.

- [ ] **Step 5: Commit**

```bash
git add examples/constellation.rs
git commit -m "feat(spike): /proc stat+statm parsing and sampler"
```

---

### Task 3: Stable comm ids + lineage graph build (TDD)

**Files:**
- Modify: `examples/constellation.rs`
- Test: inline `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `ProcSample` (Task 2), `mullion::layout::TileId`.
- Produces:
  - `struct CommIds { map: std::collections::HashMap<String, TileId>, next: TileId }` with `fn new() -> Self` and `fn id(&mut self, comm: &str) -> TileId` — assigns a stable `TileId` the first time a `comm` is seen and returns the same id thereafter. Stable ids are what make the layout idempotent across frames.
  - `struct GNode { id: TileId, comm: String, cpu_jiffies: u64, rss_pages: u64 }`
  - `struct Constellation { nodes: Vec<GNode>, edges: Vec<(TileId, TileId)> }`
  - `fn build_graph(samples: &[ProcSample], ids: &mut CommIds) -> Constellation` — groups samples by `comm` (summing `cpu_jiffies` and `rss_pages`), and builds deduped lineage edges parent-comm → child-comm (dropping self-edges where parent and child share a comm).

- [ ] **Step 1: Write the failing tests**

```rust
    fn sample(pid: u32, ppid: u32, comm: &str, cpu: u64, rss: u64) -> ProcSample {
        ProcSample { pid, ppid, comm: comm.into(), cpu_jiffies: cpu, rss_pages: rss }
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin constellation`
Expected: FAIL — `cannot find CommIds` / `build_graph`.

- [ ] **Step 3: Implement**

```rust
use mullion::layout::TileId;
use std::collections::{HashMap, HashSet};

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
        });
        n.cpu_jiffies += s.cpu_jiffies;
        n.rss_pages += s.rss_pages;
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

    let mut nodes: Vec<GNode> = agg.into_values().collect();
    nodes.sort_by_key(|n| n.id); // deterministic order for stable layout
    let mut edges: Vec<(TileId, TileId)> = edge_set.into_iter().collect();
    edges.sort();
    Constellation { nodes, edges }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin constellation`
Expected: PASS (5 tests total).

- [ ] **Step 5: Commit**

```bash
git add examples/constellation.rs
git commit -m "feat(spike): stable comm ids + lineage graph build"
```

---

### Task 4: Node visual encoding — size / heat / pulse (TDD)

**Files:**
- Modify: `examples/constellation.rs`
- Test: inline `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `mullion::style::Color`.
- Produces:
  - `struct NodeVisual { cells: u16, color: Color, pulse: f32 }`
  - `fn encode_node(cpu_frac: f32, mem_frac: f32, strain: f32) -> NodeVisual` — `cpu_frac`, `mem_frac`, `strain` are pre-normalized to `0.0..=1.0` by the caller. `cells` grows monotonically with `cpu_frac` between a floor and a ceiling; `color` ramps cool→hot with `mem_frac`; `pulse` is `strain` clamped to `0.0..=1.0`.

- [ ] **Step 1: Write the failing tests**

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin constellation`
Expected: FAIL — `cannot find encode_node`.

- [ ] **Step 3: Implement**

```rust
struct NodeVisual {
    cells: u16, // side length in cells for the node box
    color: Color,
    pulse: f32,
}

fn encode_node(cpu_frac: f32, mem_frac: f32, strain: f32) -> NodeVisual {
    const MIN_CELLS: f32 = 3.0;
    const MAX_CELLS: f32 = 14.0;
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin constellation`
Expected: PASS (8 tests total).

- [ ] **Step 5: Commit**

```bash
git add examples/constellation.rs
git commit -m "feat(spike): node visual encoding (size/heat/pulse)"
```

---

### Task 5: Fake stall injector (TDD)

**Files:**
- Modify: `examples/constellation.rs`
- Test: inline `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `mullion::ease::gaussian`, `mullion::layout::TileId`.
- Produces:
  - `struct Injector { period_s: f32, sigma: f32, culprit: Option<TileId> }` with `fn new() -> Self` (defaults: `period_s = 3.0`, `sigma = 0.10`, `culprit = None`).
  - `fn intensity(&self, t_s: f32) -> f32` — a `0.0..=1.0` pulse that peaks (≈1.0) at each whole multiple of `period_s` and decays to ≈0 between peaks (a gaussian bump on the phase distance to the nearest multiple).

- [ ] **Step 1: Write the failing tests**

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin constellation`
Expected: FAIL — `cannot find Injector`.

- [ ] **Step 3: Implement**

```rust
use mullion::ease::gaussian;

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
```

Note: `mullion::ease::gaussian(distance, sigma)` returns 1.0 at distance 0 and decays; confirm its signature matches (`examples/spiral_stress.rs:66` imports it). If it takes different argument order, adapt the call — the test pins the required behavior.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin constellation`
Expected: PASS (10 tests total).

- [ ] **Step 5: Commit**

```bash
git add examples/constellation.rs
git commit -m "feat(spike): fake periodic-stall injector"
```

---

### Task 6: Layout via Sugiyama + idempotence/stability test (TDD)

This task proves spike-goal #2 (the map does not reshuffle under live churn) as an automated property test, then wires layout into a `GraphCanvas`.

**Files:**
- Modify: `examples/constellation.rs`
- Test: inline `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `Constellation` (Task 3), `NodeVisual` (Task 4), `mullion::{GraphCanvas, FloatRect}`, `mullion::sugiyama::{auto_layout, SugiyamaParams, LayerDir}`.
- Produces:
  - `fn build_canvas(cons: &Constellation, cpu_max: u64) -> GraphCanvas` — creates a `GraphCanvas`, adds each node at a placeholder `FloatRect` sized from its CPU (via `encode_node`), then runs `sugiyama::auto_layout` with the constellation's edges so nodes are placed in stable layered order. `cpu_max` is the frame's max `cpu_jiffies` for normalization (0 → treat as 1).
  - `fn placed_rects(canvas: &GraphCanvas, window: Rect) -> Vec<(TileId, Rect)>` — thin wrapper over `GraphCanvas::solve`.

- [ ] **Step 1: Write the failing test (idempotence = frame-to-frame stability)**

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin constellation`
Expected: FAIL — `cannot find build_canvas` / `placed_rects`.

- [ ] **Step 3: Implement**

```rust
use mullion::sugiyama::{auto_layout, LayerDir, SugiyamaParams};
use mullion::{FloatRect, GraphCanvas};

fn node_side(cpu_jiffies: u64, cpu_max: u64) -> u16 {
    let frac = if cpu_max == 0 { 0.0 } else { cpu_jiffies as f32 / cpu_max as f32 };
    encode_node(frac, 0.0, 0.0).cells
}

fn build_canvas(cons: &Constellation, cpu_max: u64) -> GraphCanvas {
    // Canvas is generously larger than the screen; auto_layout resizes to fit.
    let mut canvas = GraphCanvas::new(200, 80).with_grid(2);
    for n in &cons.nodes {
        let side = node_side(n.cpu_jiffies, cpu_max);
        // Nodes are boxes; height a bit shorter than width reads better in cells.
        let h = (side / 2).max(3);
        canvas.add(n.id, FloatRect::new(0, 0, side.max(3), h));
    }
    auto_layout(
        &mut canvas,
        &cons.edges,
        &SugiyamaParams { dir: LayerDir::LeftRight, layer_gap: 8, node_gap: 3, grid: 2 },
    );
    canvas
}

fn placed_rects(canvas: &GraphCanvas, window: Rect) -> Vec<(TileId, Rect)> {
    canvas.solve(window)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin constellation`
Expected: PASS (11 tests total). If `auto_layout` proves *not* bit-identical across calls (it should be, per manual §3.25 "idempotent"), that is itself a critical spike finding — record it in the design doc's follow-on notes rather than weakening the test.

- [ ] **Step 5: Commit**

```bash
git add examples/constellation.rs
git commit -m "feat(spike): sugiyama layout + idempotence stability test"
```

---

### Task 7: Render the live constellation — nodes + backbone edges

Integration task (visual). Verified by a precise manual checklist, not a unit test.

**Files:**
- Modify: `examples/constellation.rs`

**Interfaces:**
- Consumes: everything from Tasks 2–6; `mullion::border::draw_box`, `mullion::Viewport`, `mullion::route::{route_all, render as render_connectors, RouteRequest}`, `mullion::socket::{Socket, Flow}`, `mullion::float::free_cells_in_window`, `mullion::label::Side`, `mullion::border::LineWeight`.
- Produces: a live `State` that samples `/proc` on a cadence, rebuilds the graph with persistent `CommIds`, lays it out, and renders nodes (boxed, sized/colored by `encode_node` using per-frame CPU deltas) with backbone edges routed between them inside a `Viewport`.

- [ ] **Step 1: Extend `State` to hold the live model**

Replace the Task-1 `State` with fields carrying the persistent id map, previous CPU jiffies (for per-frame deltas), a resample timer, and the current constellation + canvas. Sampling cadence: every 1.0 s (`SAMPLE_EVERY`). CPU metric per node = delta jiffies since last sample, normalized by the frame max; memory metric = `rss_pages` normalized by the frame max.

```rust
const SAMPLE_EVERY: f32 = 1.0;

struct State {
    stall: bool,
    t: f32,
    since_sample: f32,
    ids: CommIds,
    prev_cpu: HashMap<TileId, u64>, // last cumulative jiffies per node id
    cons: Constellation,
    canvas: GraphCanvas,
    cpu_frac: HashMap<TileId, f32>, // per-frame normalized deltas
    mem_frac: HashMap<TileId, f32>,
    injector: Injector,
}
```

Initialize in `new(stall)` with an immediate first sample so the first frame is populated (call the resample routine from Step 2 once). Keep `advance` incrementing `t` and `since_sample`, and when `since_sample >= SAMPLE_EVERY`, resample.

- [ ] **Step 2: Implement resample + metric normalization**

```rust
impl State {
    fn resample(&mut self) {
        let samples = sample_procs();
        let cons = build_graph(&samples, &mut self.ids);

        // CPU deltas vs previous cumulative jiffies.
        let mut deltas: HashMap<TileId, u64> = HashMap::new();
        for n in &cons.nodes {
            let prev = self.prev_cpu.get(&n.id).copied().unwrap_or(n.cpu_jiffies);
            deltas.insert(n.id, n.cpu_jiffies.saturating_sub(prev));
        }
        self.prev_cpu = cons.nodes.iter().map(|n| (n.id, n.cpu_jiffies)).collect();

        let dmax = deltas.values().copied().max().unwrap_or(1).max(1);
        let mmax = cons.nodes.iter().map(|n| n.rss_pages).max().unwrap_or(1).max(1);
        self.cpu_frac = deltas.iter().map(|(&id, &d)| (id, d as f32 / dmax as f32)).collect();
        self.mem_frac = cons
            .nodes
            .iter()
            .map(|n| (n.id, n.rss_pages as f32 / mmax as f32))
            .collect();

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
    }
}
```

- [ ] **Step 3: Render nodes + backbone edges inside a Viewport**

In `render`, build a `Viewport` over the canvas sized to `buf.area`, project each node rect, and `draw_box` it with its encoded color; route backbone edges with `route_all` and `render_connectors`. Use a single output/input socket pair per edge (right side of source, left side of target), matching the manual §3.22 example.

```rust
fn render(&self, buf: &mut Buffer) {
    use mullion::border::{draw_box, Borders, BorderStyle, CornerStyle, LineWeight};
    let area = buf.area;
    let (cw, ch) = self.canvas.size();
    let vp = mullion::Viewport::new(area, cw, ch);
    let placed = self.canvas.solve(Rect::new(0, 0, cw, ch)); // canvas-space rects

    // Nodes.
    let mut node_rects: Vec<Rect> = Vec::new();
    for (id, crect) in &placed {
        if let Some(screen) = vp.project(*crect) {
            let cpu = self.cpu_frac.get(id).copied().unwrap_or(0.0);
            let mem = self.mem_frac.get(id).copied().unwrap_or(0.0);
            let strain = 0.0; // wired in Task 9
            let vis = encode_node(cpu, mem, strain);
            draw_box(buf, screen, Borders::ALL, &BorderStyle {
                weight: LineWeight::Light,
                corners: CornerStyle::Rounded,
                style: Style::default().fg(vis.color),
            });
            // Neutral label: the comm, elided to the box interior.
            if let Some(n) = self.cons.nodes.iter().find(|n| n.id == *id) {
                if screen.width > 2 {
                    buf.set_string(
                        screen.x + 1,
                        screen.y,
                        n.comm.chars().take((screen.width - 2) as usize).collect::<String>().as_str(),
                        Style::default().fg(vis.color),
                    );
                }
            }
            node_rects.push(screen);
        }
    }

    // Backbone edges (structural). Overlay edges are added in Task 9.
    self.render_edges(buf, area, &placed, &vp, &self.cons.edges, Color::Rgb(90, 90, 110));
}
```

Add a helper `render_edges` that assembles `RouteRequest`s in canvas space and renders them (mirrors manual §3.22 / §3.23 — routes are computed in canvas coords, rendered at `vp.origin()` over `vp.visible()`):

```rust
fn render_edges(
    &self,
    buf: &mut Buffer,
    _area: Rect,
    placed: &[(TileId, Rect)],
    vp: &mullion::Viewport,
    edges: &[(TileId, TileId)],
    color: Color,
) {
    use mullion::border::LineWeight;
    use mullion::float::free_cells_in_window;
    use mullion::label::Side;
    use mullion::route::{render as render_connectors, route_all, RouteRequest};
    use mullion::socket::{Flow, Socket};

    let rect_of = |id: TileId| placed.iter().find(|(i, _)| *i == id).map(|(_, r)| *r);
    let node_rects: Vec<Rect> = placed.iter().map(|(_, r)| *r).collect();
    let (cw, ch) = self.canvas.size();
    let canvas = Rect::new(0, 0, cw, ch);
    let free: std::collections::HashSet<(u16, u16)> =
        free_cells_in_window(canvas, &node_rects, 0, canvas).into_iter().collect();

    let mut reqs = Vec::new();
    for (a, b) in edges {
        let (Some(ra), Some(rb)) = (rect_of(*a), rect_of(*b)) else { continue };
        let src = Socket::new(Side::Right, (ra.height / 2).max(1), Flow::Out, 0);
        let dst = Socket::new(Side::Left, (rb.height / 2).max(1), Flow::In, 0);
        let (Some(s), Some(d)) = (src.attach(ra), dst.attach(rb)) else { continue };
        reqs.push(RouteRequest::new(s, d, src.outward().opposite(), dst.outward().opposite()));
    }
    let wires: Vec<_> = route_all(&free, &reqs, 4, 8).into_iter().flatten().collect();
    let styles = vec![Style::default().fg(color); wires.len()];
    render_connectors(buf, vp.visible(), vp.origin(), &wires, &styles, &node_rects, LineWeight::Light);
}
```

Note: exact `Socket::attach` / `outward` / `route_all` / `render` signatures are pinned in `../mullion/src/{socket,route}.rs` and manual §3.22. If `free_cells_in_window`'s argument shape differs, follow the manual §3.22 snippet verbatim.

- [ ] **Step 4: Build and manually verify**

Run: `cargo build --bin constellation` then run it in a real terminal.
Expected on-screen:
- A live field of rounded boxes, one per process-group `comm`, laid out in left-to-right layers.
- Box **sizes differ** by CPU activity; **border colors** range cool→hot by memory.
- Thin connector lines link parents to children (e.g., your shell to the programs it spawned).
- Watching for ~10 s: **boxes do not jump around** between samples — an unchanged group keeps its position (only sizes/colors breathe).

- [ ] **Step 5: Commit**

```bash
git add examples/constellation.rs
git commit -m "feat(spike): render live constellation with backbone edges"
```

---

### Task 8: Semantic zoom — dive and surface with spatial breadcrumb

Integration task (visual).

**Files:**
- Modify: `examples/constellation.rs`

**Interfaces:**
- Consumes: `mullion::zoom::{Zoom, Lod, LodScale, FocusTarget}`, `mullion::zoom::lerp_rect`.
- Produces: focus movement (arrow keys pick a focused node), `Enter`/`+` dives (eases the focused node's rect toward filling the screen; at `Lod::Full` its interior renders the child/thread subgraph), `Esc`/`-` surfaces. The parent scope stays drawn as a receding frame = the spatial breadcrumb.

- [ ] **Step 1: Add focus + zoom state**

```rust
// add to State:
//   focus: Option<TileId>,
//   zoom_t: f32,          // 0 = overview, 1 = focused fills screen
//   zoom_target: Option<TileId>,
```

Arrow keys move `focus` to the nearest node in that direction (compare placed screen-rect centers). `Enter`/`+` set `zoom_target = focus` and ease `zoom_t` up over ~0.3 s in `advance`; `Esc`/`-` ease it back to 0.

- [ ] **Step 2: Apply the zoom to the focused node's rect**

In `render`, when `zoom_target` is set, compute its overview screen rect and a full-screen target rect, and interpolate with `lerp_rect(overview, full, zoom_t)`. Draw the focused node at the interpolated rect; keep the rest of the constellation drawn behind it (dimmed) so the outer scope frames the dive.

```rust
// pseudocode inside render, after computing `placed`:
// if let Some(tid) = self.zoom_target {
//     let overview = /* projected rect of tid */;
//     let full = Rect::new(area.x+2, area.y+1, area.width-4, area.height-2);
//     let grown = mullion::zoom::lerp_rect(overview, full, self.zoom_t);
//     match Lod::for_rect(grown, LodScale::default()) {
//         Lod::Collapsed | Lod::Titled => draw box + title,
//         Lod::Ported => draw box + title + child count,
//         Lod::Full => render the node's interior subgraph (Step 3),
//     }
// }
```

- [ ] **Step 3: Render the interior subgraph at `Lod::Full`**

The interior of a `comm` node is its member processes (the individual PIDs of that comm) and their child comms. For the spike, render the interior as a small inner constellation: re-run `build_canvas` on just the samples whose comm matches (or their child comms), scaled into the grown rect. Reuse `render_edges`. This is the "a node unfolds into its own sub-constellation" primitive from manual §3.24.

- [ ] **Step 4: Build and manually verify**

Run it in a real terminal.
Expected:
- Arrow keys move a highlighted focus box.
- `Enter` smoothly grows the focused node until it fills the screen; as it crosses area thresholds it gains a title, then reveals an **interior graph** of its members/children.
- The outer constellation remains visible behind it as a receding frame (the breadcrumb).
- `Esc` smoothly shrinks it back into place — you land where you left, not somewhere new.

- [ ] **Step 5: Commit**

```bash
git add examples/constellation.rs
git commit -m "feat(spike): semantic zoom dive/surface with breadcrumb framing"
```

---

### Task 9: The stall arc — weather + materialize + bedrock

Integration task (visual). Ties the injector to the three-part narrative.

**Files:**
- Modify: `examples/constellation.rs`

**Interfaces:**
- Consumes: `Injector` (Task 5), `mullion::field::Field`, `mullion::style::Color`, the render pipeline (Tasks 7–8).
- Produces: when `--stall` is on, (a) **weather** — the window rim and a faint canvas wash pulse on `injector.intensity(t)`; (b) **materialize** — a chosen culprit node reaches overlay contention edges (a second edge set in a hot hue) to its lineage neighbors, appearing only while intensity is high; (c) **bedrock** — diving into the culprit at `Lod::Full` shows a latency timeline strip rendered with `Field::render_braille` carrying the injected period/magnitude.

- [ ] **Step 1: Pick a culprit and drive strain**

On the first resample under `--stall`, set `injector.culprit` to the highest-CPU node id. Each frame compute `let s = self.injector.intensity(self.t);`. Feed `s` as the `strain` argument to `encode_node` **for the culprit node only** (others get 0), so the culprit pulses.

- [ ] **Step 2: Weather — rim + wash pulse**

Render a perimeter `Field::strip` (or reuse `draw_box` on `buf.area` with an intensity-scaled color) around the whole screen whose brightness = `s`. Add a faint full-screen wash (a dim overlay color scaled by `s`) so the entire map visibly breathes on the stall clock. Keep it subtle at `s≈0`.

```rust
// let pulse = self.injector.intensity(self.t);
// let rim_col = Color::Rgb((30.0+180.0*pulse) as u8, (20.0+40.0*pulse) as u8, (40.0*(1.0-pulse)) as u8);
// draw_box(buf, area, Borders::ALL, &BorderStyle { weight: LineWeight::Light, corners: CornerStyle::Rounded, style: Style::default().fg(rim_col) });
```

- [ ] **Step 3: Materialize — overlay contention edges**

While `s > 0.5`, draw a second edge set from `injector.culprit` to each of its lineage neighbors (the edges in `cons.edges` touching the culprit) via a second `render_edges` call in a hot hue (e.g. `Color::Rgb(230, 80, 60)`), so the causal wiring lights up only during a stall and fades between.

- [ ] **Step 4: Bedrock — latency timeline at Full LoD**

When the focused/zoomed node is the culprit and it is at `Lod::Full`, render a bottom strip inside its rect as a latency timeline: a `Field::rect` over the strip, fed an `intensity(u, v)` closure that draws the injector's pulse train across the width (peaks spaced by `period_s`), via `field.render_braille(...)`. Label it with the measured period/magnitude in neutral terms (e.g. `period 3.0s  peak`), **no product names**.

- [ ] **Step 5: Build and manually verify**

Run: `cargo run --bin constellation -- --stall` in a real terminal.
Expected:
- The whole map + rim **pulse together on a ~3 s clock** (weather).
- On each pulse, **hot edges flash** from one culprit node out to its neighbors, then fade (materialize).
- Diving into the culprit (`Enter`) reveals a **braille latency timeline** at its floor with a `period 3.0s` label (bedrock).
- Without `--stall`, the map is calm — no rim pulse, no hot edges. `--stall` is the only difference.

- [ ] **Step 6: Commit**

```bash
git add examples/constellation.rs
git commit -m "feat(spike): stall arc — weather, materialize, bedrock"
```

---

### Task 10: Controls, help, and the spike verdict checklist

**Files:**
- Modify: `examples/constellation.rs`
- Modify: `docs/superpowers/specs/2026-07-12-constellation-face-design.md` (append a "Spike findings" section)

**Interfaces:**
- Consumes: all prior tasks.
- Produces: a complete controls handler + `HELP` text, and a written verdict against the four spike proof-goals.

- [ ] **Step 1: Finalize controls + HELP**

Wire and document: `↑↓←→` move focus · `Enter`/`+` dive · `Esc`/`-` surface · `space` pause · `q`/`Ctrl-C` quit · `--stall` flag. Update the `HELP` constant to list them.

- [ ] **Step 2: On-screen HUD line**

Draw a one-line footer showing: node count, current sample age, and (under `--stall`) the live `intensity` value — so the manual verification is legible.

- [ ] **Step 3: Build, clippy, test**

Run:
```bash
cargo build --bin constellation
cargo clippy --bin constellation -- -D warnings
cargo test --bin constellation
```
Expected: all clean; 11 unit tests pass.

- [ ] **Step 4: Run the verdict checklist (manual)**

In a real terminal, confirm each spike proof-goal and jot the answer:
1. **Semantic zoom feels continuous & oriented** — dive/surface lands you where you left, not lost. (yes/no + note)
2. **Layout stable under live churn** — start something (`sleep 30 &`, open/close a program) and confirm existing nodes hold position while the new one appears. (yes/no + note)
3. **Nodes readable at a glance** — can you read "who's busy / who's big" off size+heat without a bar chart? (yes/no + note)
4. **Stall arc reads as one gesture** — notice → follow to node → dive for proof, under `--stall`. (yes/no + note)

- [ ] **Step 5: Write the verdict into the design doc**

Append a `## Spike findings (2026-07-…)` section to `docs/superpowers/specs/2026-07-12-constellation-face-design.md` recording the four answers and a one-line recommendation: pursue as aerie's primary face / iterate the spike / drop. Note any surprises (e.g., layout not bit-idempotent, routing too busy at scale, zoom disorienting).

- [ ] **Step 6: Commit**

```bash
git add examples/constellation.rs docs/superpowers/specs/2026-07-12-constellation-face-design.md
git commit -m "feat(spike): controls, HUD, and recorded spike verdict"
```

---

## Self-Review

**Spec coverage:**
- Unifying spatial model / semantic-zoom engine → Tasks 7 (render), 8 (zoom/dive), verified against `mullion::zoom`.
- Constellation geometry (nodes + position by relatedness) → Tasks 3 (graph), 6 (Sugiyama layout).
- Layered edges: backbone (lineage) always-on → Tasks 3, 7; overlay (contention) on strain → Task 9 (materialize).
- Bars dissolve into nodes (size=CPU, heat=memory, pulse=strain) → Task 4 (encoding), Task 7 (applied), Task 9 (pulse).
- Stall arc: weather + materialize + bedrock → Task 9.
- Persistent map + spatial breadcrumb → Task 7 (`Viewport`), Task 8 (receding frame).
- Prototype-first / standalone / fake injector / local-only / no aerie coupling → Tasks 1, 5; Global Constraints.
- Domain-agnostic labeling → Global Constraints; Tasks 7, 9 label by `comm`/period only.
- Spike proof-goals + verdict → Task 6 (goal #2 automated), Task 10 (all four, recorded).

**Placeholder scan:** No "TBD"/"handle edge cases"/"similar to Task N". Integration tasks (7–9) that cannot be unit-tested carry explicit on-screen expected-observation checklists in place of assertions, and Task 10 records the verdict. Zoom internals (Task 8 Steps 2–3) are given as commented pseudocode because exact `lerp_rect`/`Lod` glue depends on runtime rects; the interfaces and expected behavior are pinned.

**Type consistency:** `TileId` (from `mullion::layout`) is the node id everywhere; `CommIds::id`, `GNode.id`, `Constellation.edges`, `build_canvas`, `placed_rects`, `Injector.culprit`, and `State.prev_cpu`/`focus`/`zoom_target` all use it. `encode_node(cpu_frac, mem_frac, strain) -> NodeVisual` is called with the same argument order in Tasks 6, 7, 9. `build_graph(samples, &mut ids)` and `build_canvas(cons, cpu_max)` signatures match their call sites.

**Note on TDD boundary:** Tasks 2–6 are true red/green TDD (pure logic). Tasks 7–9 are visual integration and are honestly labeled as manually verified with precise expected on-screen output — appropriate for a spike whose deliverable is a *felt* quality. Task 6 converts the single most important risk (layout stability) into an automated property test.
