// SPDX-License-Identifier: GPL-3.0-or-later
//
// spiral_stress — a mullion stress + "wow" demo.
//
// It draws a stack of nested, empty rectangular frames whose arrangement
// starts out like a Fibonacci / golden-rectangle spiral, then continuously
// *uncurls* through a concentric state and *re-curls the other way* — the kind
// of morphing you see in Electric Sheep fractals, but expressed purely in the
// shape and placement of TUI boxes.
//
// On top of that it demonstrates that the boxes are not static furniture: the
// sides carry "openings" (gaps in the border) that slide, grow, shrink, and
// split into two before merging back together. The outermost box's openings
// hold live text (brand + a stress HUD); inner boxes get smaller decorative
// ports so the whole figure stays alive at every depth.
//
// Why it doubles as a stress test: every frame the entire screen is repainted
// cell-by-cell (full clear + N nested frame perimeters + animated ports) at the
// target frame rate, so the cell-write counter in the HUD is a rough proxy for
// per-frame draw throughput. Crank the depth with +/- to push it harder.
//
// Run:   cargo run --release --example spiral_stress
// Keys:  q / Esc / Ctrl-C  quit
//        space             pause / resume the animation
//        + / -             more / fewer nested boxes (depth)
//        [ / ]             tighten / loosen the curl
//        r                 reverse the curl direction
//
// This example only depends on `mullion` and `crossterm`, both already in
// aerie's dependency set; it does not touch the aerie binary.

use anyhow::Result;
use crossterm::event::Event;
use mullion::backend::CrosstermBackend;
use mullion::capabilities::Capabilities;
use mullion::input::{KeyCode, KeyModifiers};
use mullion::style::{Color, Modifier, Style};
use mullion::{poll_event, Buffer, Rect, Terminal};
use std::io;
use std::time::{Duration, Instant};

/// Target frame budget. ~60 fps; the poll timeout caps how long we wait for
/// input before producing the next animation frame.
const FRAME: Duration = Duration::from_millis(16);

fn main() -> Result<()> {
    let mut backend = CrosstermBackend::new(io::stdout());
    backend.apply_capabilities(&Capabilities::detect());
    let mut terminal = Terminal::new(backend)?;
    terminal.enter()?;

    let mut state = Demo::new();
    let mut last = Instant::now();

    // Run the loop in a closure so a `?` early-exit still falls through to the
    // `terminal.leave()` below, restoring the user's terminal. Inlining here
    // also avoids having to name mullion's `Terminal`/backend generic types.
    let result: Result<()> = (|| {
        loop {
            let now = Instant::now();
            let dt = now.duration_since(last).as_secs_f32().min(0.1);
            last = now;
            state.advance(dt);

            terminal.draw(|buf| state.render(buf))?;

            if let Some(Event::Key(key)) = poll_event(FRAME)? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Char(' ') => state.paused = !state.paused,
                    KeyCode::Char('+') | KeyCode::Char('=') => state.depth = (state.depth + 1).min(40),
                    KeyCode::Char('-') | KeyCode::Char('_') => {
                        state.depth = state.depth.saturating_sub(1).max(2)
                    }
                    KeyCode::Char('[') => state.curl = (state.curl - 0.1).max(0.0),
                    KeyCode::Char(']') => state.curl = (state.curl + 0.1).min(2.5),
                    KeyCode::Char('r') => state.dir = -state.dir,
                    _ => {}
                }
            }
        }
        Ok(())
    })();

    terminal.leave()?;
    result
}

/// Animation + interaction state for the demo.
struct Demo {
    /// Seconds of animation elapsed (frozen while paused).
    t: f32,
    paused: bool,
    /// Number of nested boxes requested (actual count is also bounded by how
    /// many frames fit before they collapse below the minimum drawable size).
    depth: usize,
    /// User curl multiplier on top of the automatic curl envelope.
    curl: f32,
    /// Curl direction, +1.0 or -1.0 (toggled by `r`).
    dir: f32,
    /// Smoothed frames-per-second estimate for the HUD.
    fps: f32,
    /// Cells written on the previous frame (stress proxy), shown in the HUD.
    last_cells: usize,
    frames: u64,
}

impl Demo {
    fn new() -> Self {
        Demo {
            t: 0.0,
            paused: false,
            depth: 14,
            curl: 1.0,
            dir: 1.0,
            fps: 0.0,
            last_cells: 0,
            frames: 0,
        }
    }

    fn advance(&mut self, dt: f32) {
        if !self.paused {
            self.t += dt;
        }
        if dt > 0.0 {
            let inst = 1.0 / dt;
            // Exponential smoothing so the HUD number doesn't jitter.
            self.fps = if self.fps == 0.0 { inst } else { self.fps * 0.9 + inst * 0.1 };
        }
        self.frames += 1;
    }

    fn render(&mut self, buf: &mut Buffer) {
        let area = buf.area;
        if area.width < 4 || area.height < 4 {
            return;
        }
        let mut p = Painter::new(buf);

        // Full repaint each frame — both to avoid ghosting and to make the
        // per-frame cell count a meaningful stress figure.
        let bg = Style::default();
        let blank = " ".repeat(area.width as usize);
        for y in area.y..area.y + area.height {
            p.put_str(area.x as i32, y as i32, &blank, bg);
        }

        // --- spiral geometry -------------------------------------------------
        // Each level shrinks the rectangle by a fixed fraction `f` and re-anchors
        // it somewhere on an inscribed circle whose angle advances by `dtheta`
        // per level. A large |dtheta| (~quarter turn) hugs a rotating corner and
        // traces a golden-rectangle spiral; `dtheta == 0` telescopes straight in
        // (concentric); negative `dtheta` curls the opposite way.
        let t = self.t;
        // Automatic curl envelope: starts at full curl (cos 0 = 1 → spiral),
        // relaxes to 0 (uncurled) and swings negative (curls the other way).
        let dtheta = 1.5 * (t * 0.12).cos() * self.curl * self.dir;
        let theta0 = t * 0.3; // global slow spin of the whole figure
        let f = 0.18 + 0.04 * (t * 0.25).sin(); // gentle "breathing" of the inset

        let mut rx = area.x as f32;
        let mut ry = area.y as f32;
        let mut rw = area.width as f32;
        let mut rh = area.height as f32;
        let mut theta = theta0;

        for i in 0..self.depth {
            let (ix, iy, iw, ih) = (
                rx.round() as i32,
                ry.round() as i32,
                rw.round() as i32,
                rh.round() as i32,
            );
            if iw < 3 || ih < 3 {
                break;
            }

            // Electric-sheep palette: hue flows with time and depth.
            let hue = t * 30.0 + i as f32 * 17.0;
            let val = 0.65 + 0.35 * ((t * 0.7 + i as f32 * 0.5).sin() * 0.5 + 0.5);
            let style = Style::default().fg(hsv(hue, 0.85, val));

            self.draw_box(&mut p, ix, iy, iw, ih, style, i, area);

            // Re-anchor + shrink for the next level inward.
            let ax = 0.5 + 0.5 * theta.cos();
            let ay = 0.5 + 0.5 * theta.sin();
            let dw = f * rw;
            let dh = f * rh;
            rx += ax * dw;
            ry += ay * dh;
            rw -= dw;
            rh -= dh;
            theta += dtheta;
        }

        self.last_cells = p.cells;
    }

    /// Draw one nested frame. Level 0 (outermost) carries live text openings:
    /// brand ports that split/merge on the top edge and a stress HUD on the
    /// bottom edge. Every level also gets small decorative ports that drift and
    /// split on all four sides.
    fn draw_box(
        &self,
        p: &mut Painter,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        st: Style,
        level: usize,
        area: Rect,
    ) {
        let (x0, y0, x1, y1) = (x, y, x + w - 1, y + h - 1);

        // Corners.
        p.put(x0, y0, '┌', st);
        p.put(x1, y0, '┐', st);
        p.put(x0, y1, '└', st);
        p.put(x1, y1, '┘', st);

        // Decorative animated ports (gap intervals along each edge).
        let top = side_gaps(level, self.t, w);
        let bot = side_gaps(level + 7, self.t * 0.9, w);
        let lft = side_gaps(level + 13, self.t * 1.1, h);
        let rgt = side_gaps(level + 19, self.t * 0.8, h);

        // Top & bottom edges (skip the auto ports on level 0 where text lives).
        if level == 0 {
            h_edge(p, y0, x0, x1, &[], st);
            h_edge(p, y1, x0, x1, &[], st);
        } else {
            h_edge(p, y0, x0, x1, &offset(&top, x0), st);
            h_edge(p, y1, x0, x1, &offset(&bot, x0), st);
        }
        v_edge(p, x0, y0, y1, &offset(&lft, y0), st);
        v_edge(p, x1, y0, y1, &offset(&rgt, y0), st);

        if level == 0 {
            self.draw_brand_ports(p, x0, y0, x1, st);
            self.draw_hud_port(p, x0, y1, x1, st, area);
        }
    }

    /// Two text ports on the top edge that split apart and merge back together.
    /// When they would overlap they fuse into a single combined port — the
    /// clearest demonstration of openings splitting and merging.
    fn draw_brand_ports(&self, p: &mut Painter, x0: i32, y: i32, x1: i32, st: Style) {
        let w = (x1 - x0) as f32;
        let mid = x0 as f32 + w * 0.5;
        let s = 0.5 + 0.5 * (self.t * 0.5).sin(); // 0 = merged, 1 = fully split
        let spread = 0.26 * w * s;

        let la = "aerie";
        let lb = "mullion";
        let wa = la.chars().count() as i32 + 4;
        let wb = lb.chars().count() as i32 + 4;
        let ca = mid - spread;
        let cb = mid + spread;

        let a_end = ca + wa as f32 / 2.0;
        let b_start = cb - wb as f32 / 2.0;
        if a_end >= b_start {
            // Overlapping → render as one fused port.
            text_port(p, mid, y, "aerie · mullion", x0, x1, st);
        } else {
            text_port(p, ca, y, la, x0, x1, st);
            text_port(p, cb, y, lb, x0, x1, st);
        }
    }

    /// A single moving port on the bottom edge holding the live stress HUD.
    fn draw_hud_port(&self, p: &mut Painter, x0: i32, y: i32, x1: i32, st: Style, area: Rect) {
        let avail = (x1 - x0 - 4).max(8) as usize;
        let full = format!(
            "{:>3.0} fps · frame {} · depth {} · {} cells · {}x{} │ q quit  space pause  ± depth  [ ] curl  r reverse",
            self.fps, self.frames, self.depth, self.last_cells, area.width, area.height
        );
        let text: String = full.chars().take(avail).collect();

        let w = (x1 - x0) as f32;
        // Drift the HUD port gently left and right.
        let mid = x0 as f32 + w * (0.5 + 0.12 * (self.t * 0.4).sin());
        text_port(p, mid, y, &text, x0, x1, st);
    }
}

// --- drawing helpers --------------------------------------------------------

/// Bounds-checked, cell-counting writer over a mullion `Buffer`. All demo
/// drawing goes through here so out-of-range coordinates are clipped and the
/// per-frame cell total stays accurate.
struct Painter<'a> {
    buf: &'a mut Buffer,
    b: Rect,
    cells: usize,
}

impl<'a> Painter<'a> {
    fn new(buf: &'a mut Buffer) -> Self {
        let b = buf.area;
        Painter { buf, b, cells: 0 }
    }

    fn put(&mut self, x: i32, y: i32, ch: char, st: Style) {
        let bx = self.b.x as i32;
        let by = self.b.y as i32;
        if x < bx || y < by || x >= bx + self.b.width as i32 || y >= by + self.b.height as i32 {
            return;
        }
        let mut tmp = [0u8; 4];
        self.buf.set_string(x as u16, y as u16, ch.encode_utf8(&mut tmp), st);
        self.cells += 1;
    }

    fn put_str(&mut self, x: i32, y: i32, s: &str, st: Style) {
        let mut cx = x;
        for ch in s.chars() {
            self.put(cx, y, ch, st);
            cx += 1;
        }
    }
}

/// Draw a horizontal edge between corners at `x0..=x1` on row `y`, punching the
/// given absolute-x gap intervals (capped with `┤`/`├` connectors).
fn h_edge(p: &mut Painter, y: i32, x0: i32, x1: i32, gaps: &[(i32, i32)], st: Style) {
    for x in x0 + 1..x1 {
        p.put(x, y, '─', st);
    }
    for &(a, b) in gaps {
        let ca = a.max(x0 + 1);
        let cb = b.min(x1 - 1);
        if cb < ca {
            continue;
        }
        for x in ca..=cb {
            p.put(x, y, ' ', st);
        }
        if cb > ca {
            p.put(ca, y, '┤', st);
            p.put(cb, y, '├', st);
        }
    }
}

/// Draw a vertical edge between corners at `y0..=y1` on column `x`, punching the
/// given absolute-y gap intervals (clean openings, no caps).
fn v_edge(p: &mut Painter, x: i32, y0: i32, y1: i32, gaps: &[(i32, i32)], st: Style) {
    for y in y0 + 1..y1 {
        p.put(x, y, '│', st);
    }
    for &(a, b) in gaps {
        let ca = a.max(y0 + 1);
        let cb = b.min(y1 - 1);
        for y in ca..=cb {
            p.put(x, y, ' ', st);
        }
    }
}

/// Render a `┤ text ├` port centered on absolute coordinate `cx`, clamped to fit
/// between the corners at `x0..=x1`.
fn text_port(p: &mut Painter, cx: f32, y: i32, text: &str, x0: i32, x1: i32, st: Style) {
    let len = text.chars().count() as i32;
    let w = len + 4; // ┤ + space + text + space + ├
    let span = x1 - 1 - (x0 + 1);
    if w > span + 1 {
        return; // not enough room between the corners
    }
    let mut start = (cx - w as f32 / 2.0).round() as i32;
    start = start.clamp(x0 + 1, x1 - 1 - (w - 1));
    p.put(start, y, '┤', st);
    p.put(start + 1, y, ' ', st);
    p.put_str(start + 2, y, text, st);
    p.put(start + 2 + len, y, ' ', st);
    p.put(start + 3 + len, y, '├', st);
}

/// Translate edge-local gap indices into absolute coordinates along the edge.
fn offset(gaps: &[(i32, i32)], base: i32) -> Vec<(i32, i32)> {
    gaps.iter().map(|&(a, b)| (base + a, base + b)).collect()
}

/// Produce 1 or 2 animated gap intervals (in edge-local cell indices, `1..len-1`)
/// for one side of one box. The gap drifts, pulses in width, and splits into two
/// before merging back — driven entirely by the box index `i` and time `t`.
fn side_gaps(i: usize, t: f32, len: i32) -> Vec<(i32, i32)> {
    if len < 7 {
        return Vec::new();
    }
    let fi = i as f32;
    let phase = t * 0.8 + fi * 1.3;
    let split = phase.sin() * 0.5 + 0.5; // 0..1
    let base = 0.5 + 0.18 * (t * 0.5 + fi).cos(); // center as fraction of len
    let hw = 1.0 + 1.4 * ((phase * 1.7).sin() * 0.5 + 0.5); // half-width in cells
    let sep = 0.22 * split;

    let mut out = Vec::new();
    if split < 0.18 {
        // Merged: a single port.
        if let Some(g) = make_gap(base, hw, len) {
            out.push(g);
        }
    } else {
        // Split: two diverging ports.
        if let Some(g) = make_gap(base - sep, hw, len) {
            out.push(g);
        }
        if let Some(g) = make_gap(base + sep, hw, len) {
            out.push(g);
        }
    }
    out
}

fn make_gap(center: f32, hw: f32, len: i32) -> Option<(i32, i32)> {
    let cc = center * len as f32;
    let a = (cc - hw).round() as i32;
    let b = (cc + hw).round() as i32;
    let a = a.max(1);
    let b = b.min(len - 1);
    if b > a {
        Some((a, b))
    } else {
        None
    }
}

/// HSV → mullion RGB color. `h` in degrees (wrapped), `s`/`v` in `0..=1`.
fn hsv(h: f32, s: f32, v: f32) -> Color {
    let h = ((h % 360.0) + 360.0) % 360.0;
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h / 60.0) as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    Color::Rgb(
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

// Keep `Modifier` referenced so a future bold/styling tweak is one edit away
// and the import doesn't warn if styling is toggled off during experiments.
#[allow(dead_code)]
fn _bold(st: Style) -> Style {
    st.add_modifier(Modifier::BOLD)
}
