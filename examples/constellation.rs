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

USAGE: constellation [--stall]
  --stall   drive the fake periodic-stall injector on startup
  -h,--help show this help

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

struct State {
    stall: bool,
    paused: bool, // space toggles; while true, advance() freezes the clock
    t: f32,       // seconds since start
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
            paused: false,
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
    /// culprit under `--stall`, else the focused node, else the
    /// highest-significance (first, since `cons.nodes` is kept sorted) node
    /// so startup lands on content rather than an empty canvas corner.
    fn pan_target(&self) -> Option<TileId> {
        if self.stall {
            self.injector.culprit
        } else if let Some(f) = self.focus {
            Some(f)
        } else {
            self.cons.nodes.first().map(|n| n.id)
        }
    }

    /// Move `focus` to the nearest node in `dir`, comparing screen-space rect
    /// centers (matching what the eye sees, not raw canvas coordinates).
    fn move_focus(&mut self, dir: ArrowDir) {
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
        let cons = build_graph(&samples, &mut self.ids);
        self.last_samples = samples; // kept for the Task 8 interior fallback

        // CPU deltas vs previous cumulative jiffies.
        let mut deltas: HashMap<TileId, u64> = HashMap::new();
        for n in &cons.nodes {
            let prev = self.prev_cpu.get(&n.id).copied().unwrap_or(n.cpu_jiffies);
            deltas.insert(n.id, n.cpu_jiffies.saturating_sub(prev));
        }
        self.prev_cpu = cons.nodes.iter().map(|n| (n.id, n.cpu_jiffies)).collect();

        // Task 9: under --stall, pin the injector's culprit to a real, stable
        // app node the first time we see one — the SHOWN node (kernel threads
        // already excluded in build_graph) with the highest cumulative
        // cpu_jiffies, so it reads as a believable, recognizable culprit
        // rather than an arbitrary first-frame pick. Re-elect only if that
        // comm's process disappears out from under us later. A no-op when
        // --stall is off, so the calm (Task 8) path is unaffected.
        if self.stall {
            if let Some(cid) = self.injector.culprit {
                if !cons.nodes.iter().any(|n| n.id == cid) {
                    self.injector.culprit = None;
                }
            }
            if self.injector.culprit.is_none() {
                self.injector.culprit = cons.nodes.iter().max_by_key(|n| n.cpu_jiffies).map(|n| n.id);
            }
        }

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

        // Task 9: the injector's stall pulse, 0..1, peaking every period_s.
        // Only ever fed to visuals when `self.stall` is set — with it off the
        // map stays exactly as calm as it was at the end of Task 8.
        let s = self.injector.intensity(self.t);

        // FOLLOW: how hard to dim every non-culprit node/edge toward
        // near-black this frame. Zero below s=0.35 and rising continuously to
        // 1.0 at a pulse peak — the zero-crossing at the threshold means
        // there's no pop where the effect switches on, only the smooth ramp
        // `s` already gives. Never set when --stall is off.
        let stall_dim = if self.stall { ((s - 0.35) / 0.65).clamp(0.0, 1.0) } else { 0.0 };

        // Weather: a faint full-screen wash + perimeter rim, both scaled by
        // `s`, so the whole map visibly breathes on the stall clock. Drawn
        // before the nodes so it reads as background, not an overlay on top
        // of them.
        if self.stall {
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

            // NOTICE: the detection banner — the reason to look and the
            // answer to "what do I do with this". Spans the top rim row;
            // brightness scales with the pulse so it, too, breathes on the
            // stall clock. Domain-agnostic: period/clock language only.
            let banner = format!(
                "⚠ stall detected — acting on a ~{:.1}s clock",
                self.injector.period_s
            );
            if area.width as usize > banner.chars().count() {
                let banner_color = blend_color(Color::Rgb(120, 90, 20), Color::Rgb(255, 70, 40), s);
                let x = area.x + (area.width - banner.chars().count() as u16) / 2;
                buf.set_string(x, area.y, &banner, Style::default().fg(banner_color).add_modifier(Modifier::BOLD));
            }
        }

        // Nodes.
        for (id, crect) in &placed {
            if let Some(screen) = vp.project(*crect) {
                let cpu = self.cpu_frac.get(id).copied().unwrap_or(0.0);
                let mem = self.mem_frac.get(id).copied().unwrap_or(0.0);
                // Task 9: only the stall culprit pulses, and only under
                // --stall; every other node (and every node when --stall is
                // off) gets zero strain, same as the Task-7 placeholder.
                let strain = if self.stall && self.injector.culprit == Some(*id) { s } else { 0.0 };
                let vis = encode_node(cpu, mem, strain);
                let is_focus = self.zoom_target.is_none() && self.focus == Some(*id);
                let is_culprit = self.stall && self.injector.culprit == Some(*id);
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

        // Backbone edges (structural). FOLLOW: these recede with everything
        // else on the stall clock (stall_dim), same rationale as the nodes.
        render_edges(
            buf,
            &placed,
            &vp,
            &self.cons.edges,
            dim_color(dim_color(Color::Rgb(90, 90, 110), dim_amt), stall_dim),
            self.canvas.size(),
        );

        // Materialize: while the stall pulse is high, light up the culprit's
        // lineage edges in a hot hue — a second overlay edge set atop the
        // backbone, appearing only during a stall and fading between.
        if self.stall && s > 0.5 {
            if let Some(cid) = self.injector.culprit {
                let hot_edges: Vec<(TileId, TileId)> =
                    self.cons.edges.iter().copied().filter(|(a, b)| *a == cid || *b == cid).collect();
                if !hot_edges.is_empty() {
                    render_edges(buf, &placed, &vp, &hot_edges, Color::Rgb(230, 80, 60), self.canvas.size());
                }
            }
        }

        // FOLLOW: a small invitation right under the culprit, present
        // whenever there's somewhere to dive to (not mid-dive already) — the
        // "what do I do with this" answer made concrete at the node itself.
        // Prefer just below the box; the layout can place a node flush
        // against the footer row with no room underneath, so fall back to
        // its right side on the title row rather than silently dropping it.
        if self.stall && self.zoom_target.is_none() {
            if let Some(cid) = self.injector.culprit {
                if let Some(screen) = placed.iter().find(|(id, _)| *id == cid).and_then(|(_, r)| vp.project(*r)) {
                    let prompt = "↵ dive";
                    let footer_y = area.bottom().saturating_sub(1);
                    let prompt_color = Style::default().fg(Color::Rgb(230, 80, 60));
                    if screen.bottom() < footer_y {
                        buf.set_string(screen.x + 1, screen.bottom(), prompt, prompt_color);
                    } else if screen.right() + 1 + prompt.len() as u16 <= area.right() {
                        buf.set_string(screen.right() + 1, screen.y, prompt, prompt_color);
                    }
                }
            }
        }

        // Legibility baseline (both modes): the focused (or, under --stall,
        // culprit) node gets a full readout — its whole comm plus live
        // cpu%/mem% — since its box label is elided to a 2-char stump at
        // small sizes. A header line at top-left, one row below the rim.
        // Drawn last (after nodes/edges/prompt) so routed wires never paint
        // over it, and only in the overview (the dive overlay has its own
        // title_line once zoomed in).
        if self.zoom_target.is_none() {
            // Under --stall, the culprit takes priority over plain focus so
            // its full comm/cpu/mem readout is always on-screen — the
            // "unmistakable" requirement doesn't stop at the border color.
            let readout_id = if self.stall { self.injector.culprit.or(self.focus) } else { self.focus };
            if let Some(readout_id) = readout_id {
                if let Some(n) = self.cons.nodes.iter().find(|n| n.id == readout_id) {
                    let cpu = self.cpu_frac.get(&readout_id).copied().unwrap_or(0.0) * 100.0;
                    let mem = self.mem_frac.get(&readout_id).copied().unwrap_or(0.0) * 100.0;
                    let tag = if self.stall && self.injector.culprit == Some(readout_id) { " [culprit]" } else { "" };
                    let readout = format!("{}{tag}  cpu {cpu:.0}%  mem {mem:.0}%", n.comm);
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
        // and — under --stall — the live injector intensity, so manual
        // verification of the spike proof-goals is legible at a glance.
        // Domain-agnostic: counts/seconds/numbers only, no product names.
        let mut footer = format!("nodes={} age={:.1}s", self.cons.nodes.len(), self.since_sample);
        if self.paused {
            footer.push_str(" paused");
        }
        if self.stall {
            footer.push_str(&format!(" intensity={s:.2}"));
        }
        // Legibility baseline (both modes): a one-line legend so the visual
        // encoding is never a guessing game.
        footer.push_str("  size=CPU  heat=memory  red=stall");
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
                // reveals the injected latency timeline underneath its interior.
                if self.stall && self.injector.culprit == Some(tid) {
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
    /// as a latency timeline — the injector's own pulse train swept across a
    /// small time window via `Field::render_braille`, so the peaks the map has
    /// been breathing to are visible as a trace. Labeled only in neutral
    /// period-seconds terms; no product names or remediation hints.
    fn render_stall_timeline(&self, buf: &mut Buffer, outer: Rect) {
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
}
