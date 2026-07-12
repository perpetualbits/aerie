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
// Keys: q / Ctrl-C  quit
//       arrows      move focus
//       Enter / +   dive into the focused node
//       Esc / -     surface back to the overview

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
use mullion::{Buffer, EventReader, FloatRect, GraphCanvas, Rect, Terminal, Viewport};
use std::collections::{HashMap, HashSet};
use std::io;
use std::time::{Duration, Instant};

const FRAME: Duration = Duration::from_millis(33); // ~30 fps

const HELP: &str = "\
constellation — aerie unifying-face spike

USAGE: constellation [--stall]
  --stall   drive the fake periodic-stall injector on startup
  -h,--help show this help

KEYS: q/Ctrl-C quit; arrows move focus; Enter/+ dive; Esc/- surface\n";

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

struct State {
    stall: bool,
    t: f32, // seconds since start
    since_sample: f32,
    ids: CommIds,
    prev_cpu: HashMap<TileId, u64>, // last cumulative jiffies per node id
    cons: Constellation,
    canvas: GraphCanvas,
    cpu_frac: HashMap<TileId, f32>, // per-frame normalized deltas
    mem_frac: HashMap<TileId, f32>,
    injector: Injector,
    // Task 8: semantic zoom (dive/surface) + spatial breadcrumb.
    last_samples: Vec<ProcSample>, // for the interior fallback (member PIDs)
    last_area: Rect,               // most recent screen area rendered into
    focus: Option<TileId>,         // currently highlighted node
    zoom_target: Option<TileId>,   // node being dived into, if any
    zoom_t: f32,                   // eased 0..1 progress (0 = overview, 1 = filled)
    zoom_goal: f32,                // 0.0 (surfacing) or 1.0 (diving)
}

impl State {
    fn new(stall: bool) -> Self {
        let mut state = State {
            stall,
            t: 0.0,
            since_sample: 0.0,
            ids: CommIds::new(),
            prev_cpu: HashMap::new(),
            cons: Constellation { nodes: Vec::new(), edges: Vec::new() },
            canvas: GraphCanvas::new(1, 1),
            cpu_frac: HashMap::new(),
            mem_frac: HashMap::new(),
            injector: Injector::new(),
            last_samples: Vec::new(),
            last_area: Rect::new(0, 0, 80, 24),
            focus: None,
            zoom_target: None,
            zoom_t: 0.0,
            zoom_goal: 0.0,
        };
        state.resample(); // populate the first frame immediately
        state
    }

    fn advance(&mut self, dt: f32) {
        self.t += dt;
        self.since_sample += dt;
        if self.since_sample >= SAMPLE_EVERY {
            self.resample();
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

    /// Move `focus` to the nearest node in `dir`, comparing screen-space rect
    /// centers (matching what the eye sees, not raw canvas coordinates).
    fn move_focus(&mut self, dir: ArrowDir) {
        let (cw, ch) = self.canvas.size();
        let vp = Viewport::new(self.last_area, cw, ch);
        let placed = placed_rects(&self.canvas, Rect::new(0, 0, cw, ch));
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
        let cons = build_graph(&samples, &mut self.ids);
        self.last_samples = samples; // kept for the Task 8 interior fallback

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
        let vp = Viewport::new(area, cw, ch);
        let placed = placed_rects(&self.canvas, Rect::new(0, 0, cw, ch)); // canvas-space rects

        // While a dive is in progress (or easing out), the rest of the
        // constellation dims to frame the focused node as a receding scope —
        // the spatial breadcrumb. dim_amt is 0 at the start of a dive/end of a
        // surface, so there is no visible pop when the overlay first appears.
        let dim_amt = if self.zoom_target.is_some() { smoothstep(self.zoom_t.clamp(0.0, 1.0)) } else { 0.0 };

        // Nodes.
        for (id, crect) in &placed {
            if let Some(screen) = vp.project(*crect) {
                let cpu = self.cpu_frac.get(id).copied().unwrap_or(0.0);
                let mem = self.mem_frac.get(id).copied().unwrap_or(0.0);
                let strain = 0.0; // wired in Task 9
                let vis = encode_node(cpu, mem, strain);
                let is_focus = self.zoom_target.is_none() && self.focus == Some(*id);
                let color = dim_color(vis.color, dim_amt);
                draw_box(
                    buf,
                    screen,
                    Borders::ALL,
                    &BorderStyle {
                        weight: if is_focus { LineWeight::Heavy } else { LineWeight::Light },
                        corners: CornerStyle::Rounded,
                        style: Style::default().fg(if is_focus { Color::White } else { color }),
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

        // Backbone edges (structural). Overlay edges are added in Task 9.
        render_edges(
            buf,
            &placed,
            &vp,
            &self.cons.edges,
            dim_color(Color::Rgb(90, 90, 110), dim_amt),
            self.canvas.size(),
        );

        // The dive overlay itself: the focused node grown toward fullscreen.
        if let Some(tid) = self.zoom_target {
            self.render_zoom_overlay(buf, area, tid, &placed, &vp);
        }

        // Corner readout: keep the injector state visible during the spike.
        let strain = self.injector.intensity(self.t);
        let msg = format!(
            "constellation — t={:.1}s stall={} nodes={} strain={:.2} zoom={:.2}",
            self.t,
            self.stall,
            self.cons.nodes.len(),
            strain,
            self.zoom_t,
        );
        buf.set_string(area.x + 1, area.y, &msg, Style::default().fg(Color::White));
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

/// Centered title line at the top of `rect`, if it fits.
fn title_line(buf: &mut Buffer, rect: Rect, label: &str, color: Color) {
    if (label.len() as u16) < rect.width {
        let x = rect.x + (rect.width - label.len() as u16) / 2;
        buf.set_string(x, rect.y, label, Style::default().fg(color).add_modifier(Modifier::BOLD));
    }
}

/// Route and draw backbone edges between already-placed node rects. Free
/// function (not a `State` method) so both the outer constellation and an
/// inner interior sub-constellation (Task 8) can reuse it with their own
/// canvas size.
fn render_edges(
    buf: &mut Buffer,
    placed: &[(TileId, Rect)],
    vp: &Viewport,
    edges: &[(TileId, TileId)],
    color: Color,
    canvas_size: (u16, u16),
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
    let wires: Vec<_> = route_all(&free, &reqs, 4, 8).into_iter().flatten().collect();
    let styles = vec![Style::default().fg(color); wires.len()];
    render_connectors(buf, vp.visible(), vp.origin(), &wires, &styles, &node_rects, LineWeight::Light);
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
    #[allow(dead_code)] // wired in Task 9 (notice -> materialize -> dive)
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
