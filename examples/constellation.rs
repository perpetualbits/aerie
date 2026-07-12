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
use mullion::ease::gaussian;
use mullion::input::{KeyCode, KeyModifiers};
use mullion::layout::TileId;
use mullion::style::{Color, Style};
use mullion::sugiyama::{auto_layout, LayerDir, SugiyamaParams};
use mullion::{Buffer, EventReader, FloatRect, GraphCanvas, Rect, Terminal};
use std::collections::{HashMap, HashSet};
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

#[derive(Debug, Clone, PartialEq)]
struct NodeVisual {
    cells: u16,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(pid: u32, ppid: u32, comm: &str, cpu: u64, rss: u64) -> ProcSample {
        ProcSample { pid, ppid, comm: comm.into(), cpu_jiffies: cpu, rss_pages: rss }
    }

    #[test]
    fn parses_stat_with_parens_in_comm() {
        // Real-shaped line: comm "(a b)c" contains spaces and parens.
        // fields: 1 pid, 2 comm, 3 state, 4 ppid, ... 14 utime, 15 stime
        let line = "1234 ((a b)c) S 1000 1234 1234 0 -1 0 0 0 0 0 40 60 0 0 20 0 1 0";
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
}
