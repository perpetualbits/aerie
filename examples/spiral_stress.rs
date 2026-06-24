// SPDX-License-Identifier: GPL-3.0-or-later
//
// spiral_stress — a mullion stress + "wow" demo.
//
// The default scene is a REFLOW FIELD: a recursive fractal of wandering windows.
// Every box — at every level — is filled (most show a paragraph that re-flows as
// the box breathes, `text::wrap`, coloured through the Field abstraction by a
// `Wave` colour source; some are little TVs showing braille static or a
// synthesised braille video via mullion's `Video` widget); carries 4–8 bookended
// bitstream gaps that wander around its border and slide across its corners (via
// `Field::perimeter`); and holds four smaller wandering windows of its own,
// recursing 3–4 levels deep. Press `s` for the spiral swarm, `t` for the surf.
//
// The braille video panel doubles as a video-to-characters prototype: a W×H luma
// frame buffer is filled (synthetically in `synth_frame`; or from an `ffmpeg …
// -f rawvideo -pix_fmt gray -` stream for real footage) and reproduced by the
// `Video` widget — the same trick libcaca / mpv `--vo=caca` / chafa use.
//
// The SWARM/single-spiral scene draws a stack of nested, empty rectangular frames
// whose arrangement starts out like a Fibonacci / golden-rectangle spiral, then
// continuously *uncurls* through a concentric state and *re-curls the other way* —
// the kind of morphing you see in Electric Sheep fractals, but expressed purely
// in the shape and placement of TUI boxes.
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
// per-frame draw throughput. Crank the depth with +/- to push it harder, or
// switch to SWARM mode (`s` / `--swarm`) to tile the screen with many
// independent spirals laid out by mullion's `layout::solve`.
//
// Swarm mode also demonstrates ZOOM: it periodically zooms one tile up to fill
// the screen and back. mullion's built-in `Tree::zoom_to` is a *discrete* state
// change (a sudden jump); here the zoom is driven through the layout solver by
// easing the focused tile's `Fill` weight every frame, so the solver itself
// grows the tile smoothly — an animated zoom, not a jump.
//
// Run:   cargo run --release --example spiral_stress [--swarm]
// Keys:  q / Esc / Ctrl-C  quit
//        space             pause / resume the animation
//        s                 toggle reflow field / swarm mode
//        z                 toggle the animated zoom (swarm mode)
//        + / -             more / fewer nested boxes (depth)
//        [ / ]             tighten / loosen the curl
//        r                 reverse the curl direction
//
// Set SPIRAL_VIDEO=/path/to/clip to feed the braille video panels with real
// footage: the demo spawns `ffmpeg … -f rawvideo -pix_fmt gray -` and plays its
// frames. Without it (or if ffmpeg isn't found) the panels show a synthesised
// channel. Requires `ffmpeg` on PATH.
//
// This example only depends on `mullion` and `crossterm`, both already in
// aerie's dependency set; it does not touch the aerie binary.

use anyhow::Result;
use crossterm::event::Event;
use mullion::backend::CrosstermBackend;
use mullion::capabilities::Capabilities;
use mullion::ease::{gaussian, smoothstep};
use mullion::input::{KeyCode, KeyModifiers};
use mullion::layout::{self, Constraint, Node, Orientation, Size, TileId};
use mullion::style::{Color, Modifier, Style};
use mullion::colorfield::{Palette, Wave};
use mullion::field::Field;
use mullion::text::{wrap, BaseDirection};
use mullion::video::{Filter, Frame, Sampling, Video};
use mullion::{Buffer, EventReader, Rect, Terminal};
use std::io::{self, Read};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Target frame budget. ~60 fps; the poll timeout caps how long we wait for
/// input before producing the next animation frame.
const FRAME: Duration = Duration::from_millis(16);

const HELP: &str = "\
spiral_stress — a mullion stress + \"wow\" demo

USAGE:
    spiral_stress [OPTIONS]

OPTIONS:
    --swarm      Start in swarm mode (a grid of mini-spirals) instead of one big spiral
    -h, --help   Print this help and exit

KEYS (while running):
    q, Esc       Quit
    space        Pause / resume the animation
    s            Toggle reflow field <-> swarm grid
    t            Toggle the surf field (floating tiles riding a 2-D wave field)
    o            Surf tile overlap: cycle none / border / full
    z            Toggle the swarm auto-zoom
    +, =         Increase nesting depth / treemap detail
    -, _         Decrease nesting depth / treemap detail
    [, ]         Decrease / increase curl
    r            Reverse curl direction
";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP}");
        return Ok(());
    }

    // Build the demo first — this may spawn the ffmpeg video source and print a
    // fallback note — before we switch into the alternate screen.
    let mut state = Demo::new();
    if args.iter().any(|a| a == "--swarm") {
        state.mode = Mode::Swarm;
    }

    let mut backend = CrosstermBackend::new(io::stdout());
    backend.apply_capabilities(&Capabilities::detect());
    let mut terminal = Terminal::new(backend)?;
    terminal.enter()?;
    // A background reader captures input the instant it arrives, so a heavy frame
    // never delays a keypress; the loop drains every queued event each frame, so a
    // burst (or mouse motion) never backs up behind the render.
    let input = EventReader::new();
    let mut last = Instant::now();

    // Run the loop in a closure so a `?` early-exit still falls through to the
    // `terminal.leave()` below, restoring the user's terminal. Inlining here
    // also avoids having to name mullion's `Terminal`/backend generic types.
    let result: Result<()> = (|| {
        'frames: loop {
            let frame_start = Instant::now();
            let dt = frame_start.duration_since(last).as_secs_f32().min(0.1);
            last = frame_start;

            // Handle all input first so a keypress takes effect this very frame.
            for ev in input.drain() {
                if let Event::Key(key) = ev {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break 'frames,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break 'frames,
                        KeyCode::Char(' ') => state.paused = !state.paused,
                        KeyCode::Char('s') => {
                            state.mode = match state.mode {
                                Mode::Single => Mode::Swarm,
                                Mode::Swarm | Mode::Tree => Mode::Single,
                            }
                        }
                        KeyCode::Char('t') => {
                            state.mode = match state.mode {
                                Mode::Tree => Mode::Single,
                                _ => Mode::Tree,
                            }
                        }
                        KeyCode::Char('o') => state.overlap = state.overlap.next(),
                        KeyCode::Char('z') => state.zoom_on = !state.zoom_on,
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

            state.advance(dt);
            terminal.draw(|buf| state.render(buf))?;

            // `drain` does not block (the old `poll_event(FRAME)` did), so pace the
            // frame ourselves — sleeping off whatever is left of the budget.
            std::thread::sleep(FRAME.saturating_sub(frame_start.elapsed()));
        }
        Ok(())
    })();

    terminal.leave()?;
    result
}

/// Single big spiral, a grid of many small ones, or the surf treemap.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Single,
    Swarm,
    /// Recursive subdivision driven by a travelling cosine height field — the
    /// spiral generalised to a tree of boxes (see `draw_tree` / `surf_height`).
    Tree,
}

/// How crest tiles in surf mode may sit relative to one another.
#[derive(Clone, Copy, PartialEq)]
enum Overlap {
    /// A clear cell between every tile — all free-floating.
    None,
    /// Tiles may share a wall/corner but their interiors never overlap.
    Border,
    /// Tiles overlap freely, stacking into menus of windows.
    Full,
}

impl Overlap {
    fn name(self) -> &'static str {
        match self {
            Overlap::None => "none",
            Overlap::Border => "border",
            Overlap::Full => "full",
        }
    }
    fn next(self) -> Overlap {
        match self {
            Overlap::None => Overlap::Border,
            Overlap::Border => Overlap::Full,
            Overlap::Full => Overlap::None,
        }
    }
}

/// Animation + interaction state for the demo.
struct Demo {
    /// Seconds of animation elapsed (frozen while paused).
    t: f32,
    paused: bool,
    mode: Mode,
    /// Whether the swarm's animated zoom is running.
    zoom_on: bool,
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
    /// Live telemetry encoded as ASCII bytes, rebuilt once per frame and streamed
    /// out through the border gaps as a scrolling binary feed (see `stream_bit`).
    telemetry: Vec<u8>,
    /// Surf-mode tile packing: free-floating, shared-wall, or stacked.
    overlap: Overlap,
    /// Source feeding the braille video panels (synthesised, or ffmpeg footage).
    video: Box<dyn VideoSource>,
}

impl Demo {
    fn new() -> Self {
        Demo {
            t: 0.0,
            paused: false,
            mode: Mode::Single,
            zoom_on: true,
            depth: 14,
            curl: 1.0,
            dir: 1.0,
            fps: 0.0,
            last_cells: 0,
            frames: 0,
            telemetry: Vec::new(),
            overlap: Overlap::Border,
            video: make_video_source(),
        }
    }

    /// Snapshot the live HUD numbers into the ASCII payload that the border gaps
    /// broadcast as a binary feed. Kept compact and label-prefixed so a decoded
    /// gap reads as legible telemetry. Uses last frame's `last_cells`/`fps` — the
    /// same already-settled values the on-screen HUD shows, so the two agree.
    fn telemetry(&self, area: Rect) -> Vec<u8> {
        let mode = if self.mode == Mode::Swarm { "SWARM" } else { "SINGLE" };
        format!(
            " {mode} FPS={:.0} CELLS={} DEPTH={} {}x{} ",
            self.fps, self.last_cells, self.depth, area.width, area.height
        )
        .into_bytes()
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

        // Refresh the binary feed broadcast through the border gaps this frame.
        self.telemetry = self.telemetry(area);

        // Full repaint each frame — both to avoid ghosting and to make the
        // per-frame cell count a meaningful stress figure.
        let bg = Style::default();
        let blank = " ".repeat(area.width as usize);
        for y in area.y..area.y + area.height {
            p.put_str(area.x as i32, y as i32, &blank, bg);
        }

        let spirals = match self.mode {
            Mode::Single => {
                if !self.paused {
                    self.video.tick(self.t);
                }
                self.draw_reflow(&mut p, area);
                1
            }
            Mode::Swarm => self.render_swarm(&mut p, area),
            Mode::Tree => {
                self.draw_tree(&mut p, area);
                0
            }
        };

        // Single mode shows its stats in the outer box's bottom port; the other
        // modes have no room for that, so they get a global overlay status line.
        if self.mode != Mode::Single {
            self.draw_status_line(&mut p, area, spirals);
        }

        self.last_cells = p.cells;
    }

    /// Draw one full spiral (a stack of nested boxes) inside `rect`. `t` is the
    /// animation phase for this spiral (offset per tile in swarm mode so each
    /// looks distinct); `allow_text` enables the level-0 brand + HUD ports.
    ///
    /// Each level shrinks the rectangle by a fixed fraction `f` and re-anchors
    /// it somewhere on an inscribed circle whose angle advances by `dtheta` per
    /// level. A large |dtheta| (~quarter turn) hugs a rotating corner and traces
    /// a golden-rectangle spiral; `dtheta == 0` telescopes straight in
    /// (concentric); negative `dtheta` curls the opposite way.
    fn draw_spiral(&self, p: &mut Painter, rect: Rect, t: f32, allow_text: bool) {
        if rect.width < 3 || rect.height < 3 {
            return;
        }
        // Automatic curl envelope: starts at full curl (cos 0 = 1 → spiral),
        // relaxes to 0 (uncurled) and swings negative (curls the other way).
        let dtheta = 1.5 * (t * 0.12).cos() * self.curl * self.dir;
        let theta0 = t * 0.3; // global slow spin of the whole figure
        let f = 0.18 + 0.04 * (t * 0.25).sin(); // gentle "breathing" of the inset

        let mut rx = rect.x as f32;
        let mut ry = rect.y as f32;
        let mut rw = rect.width as f32;
        let mut rh = rect.height as f32;
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
            let style = Style::default().fg(Color::from_hsv(hue, 0.85, val));

            self.draw_box(p, ix, iy, iw, ih, style, i, rect, t, allow_text);

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
    }

    /// The surf field: a swarm of free-floating bordered tiles, each riding a
    /// crest of a travelling 2-D wave field (`surf_height`). Every frame the
    /// field is sampled over the screen, its local maxima found, and a tile sized
    /// to each crest's breadth is drawn there. Because the waves travel in many
    /// directions and interfere, crests appear, drift, merge and split in 2-D —
    /// so tiles cluster and overlap into menus-of-windows, then break free and
    /// float apart, all on the rhythm of the wave. `+`/`-` tune crest density.
    fn draw_tree(&self, p: &mut Painter, area: Rect) {
        let (w, h) = (area.width as i32, area.height as i32);
        if w < 3 || h < 3 {
            return;
        }
        let x0 = area.x as i32;
        let y0 = area.y as i32;
        let at = |fld: &[f32], x: i32, y: i32| fld[(y * w + x) as usize];

        // Sample the height field over the whole screen.
        let mut fld = vec![0.0_f32; (w * h) as usize];
        for yy in 0..h {
            for xx in 0..w {
                let nx = (xx as f32 + 0.5) / w as f32;
                let ny = (yy as f32 + 0.5) / h as f32;
                fld[(yy * w + xx) as usize] = surf_height(nx, ny, self.t);
            }
        }

        // Detail knob (via self.depth): a lower crest line and tighter spacing
        // let more, smaller crests through for a busier, finer wave pattern. The
        // tight default spacing means neighbouring crests' tiles overlap into
        // clusters, while lone crests stay free-floating.
        let thresh = (0.64 - 0.008 * self.depth as f32).clamp(0.50, 0.64);
        let supp = (10 - self.depth as i32 / 2).clamp(3, 9);

        // Collect 8-neighbour local maxima above the crest line.
        let mut peaks: Vec<(i32, i32, f32)> = Vec::new();
        for yy in 1..h - 1 {
            for xx in 1..w - 1 {
                let v = at(&fld, xx, yy);
                if v < thresh {
                    continue;
                }
                let mut is_max = true;
                'nb: for dy in -1..=1 {
                    for dx in -1..=1 {
                        if (dx != 0 || dy != 0) && at(&fld, xx + dx, yy + dy) > v {
                            is_max = false;
                            break 'nb;
                        }
                    }
                }
                if is_max {
                    peaks.push((xx, yy, v));
                }
            }
        }

        // Strongest first, then suppress crests within a `supp`-box of a kept one
        // so the swarm thins out without losing the tallest peaks.
        peaks.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        let mut tiles: Vec<(i32, i32, f32)> = Vec::new();
        for pk in peaks {
            if tiles.len() >= 256 {
                break;
            }
            if tiles
                .iter()
                .all(|t| (t.0 - pk.0).abs() >= supp || (t.1 - pk.1).abs() >= supp)
            {
                tiles.push(pk);
            }
        }

        // Draw each crest as a tile sized to how far its bump extends before the
        // field falls `drop` below the peak — broad swells become big windows,
        // sharp chop becomes little floating boxes.
        let drop = 0.10;
        let maxr = 11;
        // Already-placed tile rects (grid coords, inclusive) for overlap control.
        let mut placed: Vec<(i32, i32, i32, i32)> = Vec::new();
        for &(cx, cy, v) in &tiles {
            // Grow each side to the crest's breadth (field still within `drop`).
            let mut rl = 0;
            while rl < maxr && cx - rl - 1 >= 0 && at(&fld, cx - rl - 1, cy) >= v - drop {
                rl += 1;
            }
            let mut rr = 0;
            while rr < maxr && cx + rr + 1 < w && at(&fld, cx + rr + 1, cy) >= v - drop {
                rr += 1;
            }
            let mut ru = 0;
            while ru < maxr && cy - ru - 1 >= 0 && at(&fld, cx, cy - ru - 1) >= v - drop {
                ru += 1;
            }
            let mut rd = 0;
            while rd < maxr && cy + rd + 1 < h && at(&fld, cx, cy + rd + 1) >= v - drop {
                rd += 1;
            }

            // Clip against placed tiles to honour the overlap mode. Tiles are
            // taken strongest-crest first, so the tallest peaks keep their size
            // and weaker ones shrink to fit around them. `None` keeps a 1-cell
            // gap; `Border` allows a shared wall but no interior overlap; `Full`
            // does nothing (free stacking).
            if self.overlap != Overlap::Full {
                let mut guard = 0;
                loop {
                    guard += 1;
                    if guard > 128 || rl + rr < 2 || ru + rd < 2 {
                        break;
                    }
                    let (cx0, cy0, cx1, cy1) = (cx - rl, cy - ru, cx + rr, cy + rd);
                    let mut hit: Option<(i32, i32)> = None;
                    for &(px0, py0, px1, py1) in &placed {
                        let conflict = match self.overlap {
                            // Require a clear cell all around the candidate.
                            Overlap::None => {
                                cx0 - 1 <= px1
                                    && px0 <= cx1 + 1
                                    && cy0 - 1 <= py1
                                    && py0 <= cy1 + 1
                            }
                            // Overlap of >1 cell on *both* axes means interiors
                            // intersect; a shared wall/corner (<=1) is allowed.
                            Overlap::Border => {
                                let ow = cx1.min(px1) - cx0.max(px0) + 1;
                                let oh = cy1.min(py1) - cy0.max(py0) + 1;
                                ow >= 2 && oh >= 2
                            }
                            Overlap::Full => false,
                        };
                        if conflict {
                            hit = Some(((px0 + px1) / 2, (py0 + py1) / 2));
                            break;
                        }
                    }
                    let Some((pcx, pcy)) = hit else { break };
                    // Shrink the side facing the blocker; fall back to any side.
                    let mut cut = false;
                    if (pcx - cx).abs() >= (pcy - cy).abs() {
                        if pcx >= cx && rr > 0 {
                            rr -= 1;
                            cut = true;
                        } else if rl > 0 {
                            rl -= 1;
                            cut = true;
                        }
                        if !cut && rd > 0 {
                            rd -= 1;
                            cut = true;
                        } else if !cut && ru > 0 {
                            ru -= 1;
                            cut = true;
                        }
                    } else {
                        if pcy >= cy && rd > 0 {
                            rd -= 1;
                            cut = true;
                        } else if ru > 0 {
                            ru -= 1;
                            cut = true;
                        }
                        if !cut && rr > 0 {
                            rr -= 1;
                            cut = true;
                        } else if !cut && rl > 0 {
                            rl -= 1;
                            cut = true;
                        }
                    }
                    if !cut {
                        break;
                    }
                }
            }

            let (tw, th) = (rl + rr + 1, ru + rd + 1);
            if tw < 3 || th < 3 {
                continue;
            }
            placed.push((cx - rl, cy - ru, cx + rr, cy + rd));
            // Per-crest colour seed: golden-angle hue tied to position so tiles
            // stay distinct yet shift gently as their crest drifts.
            let level = (cx + cy * 7) as usize;
            self.draw_box(p, x0 + cx - rl, y0 + cy - ru, tw, th, Style::default(), level, area, self.t, false);
        }
    }

    /// Tile the screen into a grid of independent mini-spirals using mullion's
    /// `layout::solve`, then animate a zoom that grows one tile to fill the
    /// screen and back. Returns the number of spirals drawn.
    ///
    /// The zoom is produced *through the solver*: the focused row and column are
    /// given a `Fill` weight that eases up from 1 toward a large value, so the
    /// solver itself expands that tile smoothly. (mullion's `Tree::zoom_to`
    /// would do this in one discrete step — a jump — which is what we're
    /// deliberately avoiding here.)
    fn render_swarm(&self, p: &mut Painter, area: Rect) -> usize {
        let (cols, rows) = grid_dims(area);
        let n = rows * cols;

        let (focus, eased) = self.zoom_state(n);
        let (frow, fcol) = (focus / cols, focus % cols);
        // Eased focus weight: 1 (grid) → ~400 (one tile nearly fills its axis).
        let big = 1 + (eased * 400.0) as u16;

        // Build a row-split of column-splits and let mullion solve the rects.
        let mut row_children: Vec<(Constraint, Node)> = Vec::with_capacity(rows);
        for r in 0..rows {
            let mut col_children: Vec<(Constraint, Node)> = Vec::with_capacity(cols);
            for c in 0..cols {
                let w = if r == frow && c == fcol { big } else { 1 };
                let id = (r * cols + c) as TileId + 1;
                col_children.push((Constraint::new(Size::Fill(w)), Node::Tile(id)));
            }
            let row_weight = if r == frow { big } else { 1 };
            let row = Node::Split {
                orientation: Orientation::Horizontal,
                children: col_children,
            };
            row_children.push((Constraint::new(Size::Fill(row_weight)), row));
        }
        let mut root = Node::Split {
            orientation: Orientation::Vertical,
            children: row_children,
        };
        let tiles = layout::solve(&mut root, area);

        for (id, rect) in &tiles {
            let idx = (*id as usize).saturating_sub(1);
            // Per-tile phase + curl offsets so neighbours never march in lockstep.
            let t = self.t + idx as f32 * 1.7;
            // The focused tile, once it has grown enough, earns text ports.
            let allow_text = idx == focus && rect.width >= 28 && rect.height >= 10;
            self.draw_spiral(p, *rect, t, allow_text);
        }

        n
    }

    /// Which tile is the zoom focus and how far the zoom has eased in (0..1).
    /// Auto-cycles through every tile, holding each zoomed for a beat. Returns
    /// `(0, 0.0)` — a no-op grid — when zoom is disabled.
    fn zoom_state(&self, n: usize) -> (usize, f32) {
        if !self.zoom_on || n == 0 {
            return (0, 0.0);
        }
        const PERIOD: f32 = 8.0; // seconds spent per tile
        let cycle = (self.t / PERIOD).floor();
        let idx = (cycle as usize) % n;
        let local = self.t - cycle * PERIOD; // 0..PERIOD
        let raw = if local < 1.2 {
            local / 1.2 // ease in
        } else if local < 5.0 {
            1.0 // hold zoomed
        } else if local < 6.2 {
            1.0 - (local - 5.0) / 1.2 // ease out
        } else {
            0.0 // hold grid
        };
        (idx, smoothstep(raw))
    }

    /// A one-row overlay status line for the non-single modes.
    fn draw_status_line(&self, p: &mut Painter, area: Rect, spirals: usize) {
        let y = (area.y + area.height - 1) as i32;
        let zoom = if self.zoom_on { "on" } else { "off" };
        let text = match self.mode {
            Mode::Tree => format!(
                " surf · overlap {} · {:>3.0} fps · {} cells · {}x{}  │  s single  o overlap  ± detail  q quit ",
                self.overlap.name(), self.fps, self.last_cells, area.width, area.height
            ),
            _ => format!(
                " swarm · {} spirals · zoom {} · {:>3.0} fps · {} cells · {}x{}  │  s single  z zoom  ± depth  [ ] curl  r reverse  q quit ",
                spirals, zoom, self.fps, self.last_cells, area.width, area.height
            ),
        };
        let text: String = text.chars().take(area.width as usize).collect();
        let st = Style::default()
            .fg(Color::Black)
            .bg(Color::Rgb(120, 200, 255))
            .add_modifier(Modifier::BOLD);
        // Pad to full width so the bar reads as a solid status strip.
        let mut padded = text.clone();
        for _ in padded.chars().count()..area.width as usize {
            padded.push(' ');
        }
        p.put_str(area.x as i32, y, &padded, st);
    }

    /// Draw one nested frame. Level 0 (outermost) carries live text openings:
    /// brand ports that split/merge on the top edge and a stress HUD on the
    /// bottom edge. Every level also gets small decorative ports that drift and
    /// split on all four sides.
    #[allow(clippy::too_many_arguments)]
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
        t: f32,
        allow_text: bool,
    ) {
        let (x0, y0, x1, y1) = (x, y, x + w - 1, y + h - 1);

        // Corners — loop_color takes the absolute (x,y) and box bounds and
        // figures out the position on the closed perimeter loop itself.
        p.put(x0, y0, '╭', loop_color(x0, y0, x0, y0, x1, y1, t, level));
        p.put(x1, y0, '╮', loop_color(x1, y0, x0, y0, x1, y1, t, level));
        p.put(x0, y1, '╰', loop_color(x0, y1, x0, y0, x1, y1, t, level));
        p.put(x1, y1, '╯', loop_color(x1, y1, x0, y0, x1, y1, t, level));

        // Decorative animated ports (gap intervals along each edge).
        let top = side_gaps(level, t, w);
        let bot = side_gaps(level + 7, t * 0.9, w);
        let lft = side_gaps(level + 13, t * 1.1, h);
        let rgt = side_gaps(level + 19, t * 0.8, h);

        // On the level-0 text box the top/bottom edges are drawn clean so the
        // brand and HUD ports own them; otherwise punch the decorative ports.
        let text_box = level == 0 && allow_text;
        let tele = &self.telemetry;
        if text_box {
            h_edge(p, y0, x0, y0, x1, y1, &[],               t, level,      level, tele);
            h_edge(p, y1, x0, y0, x1, y1, &[],               t, level + 7,  level, tele);
        } else {
            h_edge(p, y0, x0, y0, x1, y1, &offset(&top, x0), t, level,      level, tele);
            h_edge(p, y1, x0, y0, x1, y1, &offset(&bot, x0), t, level + 7,  level, tele);
        }
        v_edge(p, x0, x0, y0, x1, y1, &offset(&lft, y0), t, level + 13, level, tele);
        v_edge(p, x1, x0, y0, x1, y1, &offset(&rgt, y0), t, level + 19, level, tele);

        if text_box {
            self.draw_brand_ports(p, x0, y0, x1, st, t);
            self.draw_hud_port(p, x0, y1, x1, st, area);
        }
    }

    /// Two text ports on the top edge that split apart and merge back together.
    /// When they would overlap they fuse into a single combined port — the
    /// clearest demonstration of openings splitting and merging.
    fn draw_brand_ports(&self, p: &mut Painter, x0: i32, y: i32, x1: i32, st: Style, t: f32) {
        let w = (x1 - x0) as f32;
        let mid = x0 as f32 + w * 0.5;
        let s = 0.5 + 0.5 * (t * 0.5).sin(); // 0 = merged, 1 = fully split
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

    /// The **reflow field** scene (replaces the single spiral): a recursive
    /// fractal of wandering windows. Every box — at every level — is filled with
    /// text coloured through the Field abstraction by a [`Wave`] colour source,
    /// carries 4–8 bookended bitstream gaps that wander around its border and
    /// across its corners, and holds four smaller wandering windows of its own,
    /// recursing 3–4 levels deep (as far as the box size allows).
    fn draw_reflow(&self, p: &mut Painter, area: Rect) {
        let t = self.t;
        // One coherent wave field underlies the whole screen; every box samples
        // it at the cell's absolute position, so the colour flows across the
        // nested windows rather than restarting in each. "Wave" colour source.
        let wave = Wave::flag();
        let (x0, y0) = (area.x as i32, area.y as i32);
        let (x1, y1) = (area.right() as i32 - 1, area.bottom() as i32 - 1);
        self.draw_window(p, x0, y0, x1, y1, 0, t, &wave, area, 1);

        // A slim HUD footer one row inside the bottom edge, over everything.
        let inner = Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        self.draw_reflow_hud(p, area, inner);
    }

    /// Draw one window of the reflow fractal, then recurse into four wandering
    /// children. `depth` is the nesting level (0 = root); `seed` distinguishes
    /// sibling boxes so their gaps and text differ. `area` is the whole screen,
    /// used to sample the global wave field.
    #[allow(clippy::too_many_arguments)]
    fn draw_window(
        &self,
        p: &mut Painter,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        depth: usize,
        t: f32,
        wave: &Wave,
        area: Rect,
        seed: u64,
    ) {
        if x1 - x0 < 4 || y1 - y0 < 3 {
            return;
        }
        let lvl = depth + 1;

        // Frame: corners + clean side bars (gaps punched on top afterwards).
        p.put(x0, y0, '╭', loop_color(x0, y0, x0, y0, x1, y1, t, lvl));
        p.put(x1, y0, '╮', loop_color(x1, y0, x0, y0, x1, y1, t, lvl));
        p.put(x0, y1, '╰', loop_color(x0, y1, x0, y0, x1, y1, t, lvl));
        p.put(x1, y1, '╯', loop_color(x1, y1, x0, y0, x1, y1, t, lvl));
        h_edge(p, y0, x0, y0, x1, y1, &[], t, 0, lvl, &[]);
        h_edge(p, y1, x0, y0, x1, y1, &[], t, 0, lvl, &[]);
        v_edge(p, x0, x0, y0, x1, y1, &[], t, 0, lvl, &[]);
        v_edge(p, x1, x0, y0, x1, y1, &[], t, 0, lvl, &[]);

        // Fill the interior. Most windows show wave-coloured text; some are TVs —
        // tuned to braille static, or playing a (synthesised) braille video.
        // All three go through the Field abstraction; the kind is stable per
        // window.
        let interior = Rect::new(
            (x0 + 1) as u16,
            (y0 + 1) as u16,
            (x1 - x0 - 1).max(0) as u16,
            (y1 - y0 - 1).max(0) as u16,
        );
        match window_kind(seed, depth) {
            WindowKind::Static => self.fill_static(p, interior, t),
            WindowKind::Video => self.fill_video(p, interior),
            WindowKind::Text => self.fill_text(p, interior, area, t, wave, depth, seed),
        }

        // Bookended bitstream gaps wandering around this box's border.
        self.draw_box_gaps(p, x0, y0, x1, y1, lvl, t, seed);

        // Recurse: four wandering windows in a drifting 2×2 grid, as deep as the
        // box size allows (≈3–4 levels on a normal terminal).
        const MAX_DEPTH: usize = 4;
        let (iw, ih) = (x1 - x0 - 1, y1 - y0 - 1);
        let (cw, ch) = (iw / 2, ih / 2);
        if depth + 1 <= MAX_DEPTH && cw >= 6 && ch >= 5 {
            for i in 0..4u64 {
                let fi = i as f32;
                let (gx, gy) = ((i % 2) as i32, (i / 2) as i32);
                let cell_x0 = x0 + 1 + gx * cw;
                let cell_y0 = y0 + 1 + gy * ch;
                let ds = depth as f32 + seed as f32 * 0.13;
                let fw = 0.80 + 0.15 * (t * 0.31 + fi + ds).sin();
                let fh = 0.78 + 0.18 * (t * 0.27 + fi * 1.3 + ds).cos();
                // clamp upper bounds are ≥ lower bounds thanks to the cw/ch gate.
                let tw = (cw as f32 * fw).round().clamp(5.0, (cw - 1) as f32) as i32;
                let th = (ch as f32 * fh).round().clamp(4.0, (ch - 1) as f32) as i32;
                let fx = 0.5 + 0.5 * (t * 0.23 + fi * 1.7 + ds).sin();
                let fy = 0.5 + 0.5 * (t * 0.19 + fi * 2.3 + ds).cos();
                let cx0 = cell_x0 + ((cw - tw).max(0) as f32 * fx).round() as i32;
                let cy0 = cell_y0 + ((ch - th).max(0) as f32 * fy).round() as i32;
                self.draw_window(
                    p,
                    cx0,
                    cy0,
                    cx0 + tw - 1,
                    cy0 + th - 1,
                    depth + 1,
                    t,
                    wave,
                    area,
                    seed.wrapping_mul(4).wrapping_add(i + 1),
                );
            }
        }
    }

    /// Fill `interior` with a wrapped paragraph, colouring each glyph by the
    /// global [`Wave`] field at its absolute position — rendered through a
    /// [`Field::rect`] so the colour comes "via the Field abstraction".
    fn fill_text(
        &self,
        p: &mut Painter,
        interior: Rect,
        area: Rect,
        t: f32,
        wave: &Wave,
        level: usize,
        seed: u64,
    ) {
        if interior.width == 0 || interior.height == 0 {
            return;
        }
        let idx = (level.wrapping_add(seed as usize)) % PASSAGES.len();
        // Repeat the passage until it has enough characters to fill every row of
        // the interior, so the tile's free space is packed with text rather than
        // trailing off into blank rows.
        let base = PASSAGES[idx];
        let want = interior.width as usize * interior.height as usize + interior.width as usize;
        let mut text = String::with_capacity(want.max(base.len()) + base.len());
        text.push_str(base);
        while text.len() < want {
            text.push(' ');
            text.push_str(base);
        }
        let wrapped = wrap(&text, interior.width, BaseDirection::Ltr);
        let lines = wrapped.lines();
        let field = Field::rect(interior);
        let (aw, ah) = (area.width.max(1) as f32, area.height.max(1) as f32);
        let mut count = 0usize;
        field.paint(p.buf, |col, row| {
            let cell = lines.get(row as usize)?.cells.get(col as usize)?;
            let x = interior.x as f32 + col as f32;
            let y = interior.y as f32 + row as f32;
            let val = wave.value(x / aw, y / ah, t);
            count += 1;
            Some((cell.symbol.clone(), Style::default().fg(Palette::Rainbow.color(val))))
        });
        p.cells += count;
    }

    /// Fill `interior` with **braille noise** — a TV tuned to static. Each cell
    /// gets a random 2×4 dot mask (`U+2800 + byte`) and a random grey, both
    /// reseeded every frame so the field hisses and crawls. Rendered through the
    /// same [`Field::rect`] abstraction the text uses.
    fn fill_static(&self, p: &mut Painter, interior: Rect, t: f32) {
        if interior.width == 0 || interior.height == 0 {
            return;
        }
        let field = Field::rect(interior);
        // ~24 fps flicker: a fresh noise frame several times a second.
        let frame = (t * 24.0) as u64;
        let mut count = 0usize;
        field.paint(p.buf, |col, row| {
            let gx = interior.x as u32 + col as u32;
            let gy = interior.y as u32 + row as u32;
            let mask = noise_byte(gx, gy, frame);
            let glyph = char::from_u32(0x2800 + mask as u32).unwrap_or(' ');
            // Independent grey so brightness flickers like real static.
            let g = 90 + (noise_byte(gx, gy, frame ^ 0x5151) >> 1); // 90..217
            count += 1;
            Some((glyph.to_string(), Style::default().fg(Color::Rgb(g, g, g))))
        });
        p.cells += count;
    }

    /// Fill `interior` with a **braille video panel** — a TV playing the current
    /// channel through mullion's [`Video`] widget. The shared [`VideoSource`] frame (a
    /// `W×H` luma buffer) is wrapped as a [`Frame`] and reproduced faithfully in
    /// braille, resampled to whatever size this window is (so every video window shows
    /// the same broadcast at its own scale). The CRT look — a cool phosphor tint and
    /// scanlines — rides on top as opt-in [`Filter`]s.
    fn fill_video(&self, p: &mut Painter, interior: Rect) {
        if interior.width == 0 || interior.height == 0 {
            return;
        }
        let (fw, fh) = self.video.dims();
        if fw == 0 || fh == 0 {
            return;
        }
        let frame = Frame::from_luma(fw, fh, self.video.frame());
        Video::new()
            // These panels are small and fast — nearest resampling is ~2× cheaper and
            // the braille dither hides the difference.
            .sampling(Sampling::Nearest)
            .filter(Filter::Phosphor { hue: 195.0, sat: 0.30 })
            .filter(Filter::Scanlines(0.25))
            .render_frame(p.buf, interior, &frame);
        p.cells += interior.width as usize * interior.height as usize;
    }

    /// Punch 4–8 **bookended** gaps that wander around this box's border and
    /// cross its corners. Four sources each drift, pulse in width, and split
    /// into a diverging pair before merging, so the live gap count breathes
    /// between 4 (all merged) and 8 (all split). The border is one continuous
    /// strip ([`Field::perimeter`]), so a window slides across a corner without
    /// a seam; each opening is capped by `┤├` / `┴┬` bookends and reveals the
    /// scrolling telemetry bitstream between them.
    fn draw_box_gaps(&self, p: &mut Painter, x0: i32, y0: i32, x1: i32, y1: i32, level: usize, t: f32, seed: u64) {
        let r = Rect::new(x0 as u16, y0 as u16, (x1 - x0 + 1) as u16, (y1 - y0 + 1) as u16);
        let field = Field::perimeter(r);
        let plen = field.width() as i32;
        if plen < 12 {
            return;
        }
        let msg = &self.telemetry;
        let sd = seed as f32;
        const SOURCES: usize = 4;
        for s in 0..SOURCES {
            let fs = s as f32 + sd * 0.37;
            let dir = if (s + seed as usize) % 2 == 0 { 1.0_f32 } else { -1.0 };
            let centre = ((s as f32 / SOURCES as f32) + 0.04 * t * dir + 0.03 * (t * 0.3 + fs).sin() + sd * 0.13)
                .rem_euclid(1.0)
                * plen as f32;
            let hw = plen as f32 * (0.02 + 0.025 * ((t * 0.7 + fs * 1.7).sin() * 0.5 + 0.5));
            let split = (t * 0.5 + fs * 2.1).sin() * 0.5 + 0.5;
            let sep = plen as f32 * 0.06 * split;
            let centres: [f32; 2] = if split < 0.25 {
                [centre, f32::NAN]
            } else {
                [
                    (centre - sep).rem_euclid(plen as f32),
                    (centre + sep).rem_euclid(plen as f32),
                ]
            };
            for (gi, &c) in centres.iter().enumerate() {
                if c.is_nan() {
                    continue;
                }
                let a = (c - hw).floor() as i32;
                let b = (c + hw).ceil() as i32;
                if b - a < 2 {
                    continue; // need room for two bookend caps
                }
                let band = (seed as usize).wrapping_mul(7).wrapping_add(s * 3 + gi + 1);
                self.put_cap(p, &field, plen, a, a - 1, x0, y0, x1, y1, level, t);
                self.put_cap(p, &field, plen, b, b + 1, x0, y0, x1, y1, level, t);
                let span = (b - a).max(1) as f32;
                for k in a + 1..b {
                    let kk = k.rem_euclid(plen);
                    if let Some((x, y)) = field.cell(kk as u16, 0) {
                        let one = stream_bit(msg, kk as f32 - t * dir * BIT_SPEED + band as f32 * 11.0);
                        let ch = if one { '▪' } else { '▫' };
                        let pos = (k - a) as f32 / span;
                        p.put(x as i32, y as i32, ch, stream_color(pos, t, band, dir, one));
                    }
                }
            }
        }
    }

    /// Draw the bookend cap for a gap end at perimeter index `idx`, choosing the
    /// connector glyph that joins the solid border at neighbour index `nidx`
    /// (`┤`/`├` on a horizontal run, `┴`/`┬` on a vertical one) — so the cap is
    /// correct even when the gap straddles a corner.
    #[allow(clippy::too_many_arguments)]
    fn put_cap(&self, p: &mut Painter, field: &Field, plen: i32, idx: i32, nidx: i32, x0: i32, y0: i32, x1: i32, y1: i32, level: usize, t: f32) {
        let cell = field.cell(idx.rem_euclid(plen) as u16, 0);
        let neigh = field.cell(nidx.rem_euclid(plen) as u16, 0);
        if let (Some((x, y)), Some((nx, ny))) = (cell, neigh) {
            let cap = if ny < y {
                '┴'
            } else if ny > y {
                '┬'
            } else if nx < x {
                '┤'
            } else {
                '├'
            };
            p.put(x as i32, y as i32, cap, loop_color(x as i32, y as i32, x0, y0, x1, y1, t, level));
        }
    }

    /// A compact stress HUD drawn one row inside the bottom edge (so the border
    /// and its travelling gaps stay clear), spanning the interior width.
    fn draw_reflow_hud(&self, p: &mut Painter, area: Rect, inner: Rect) {
        if inner.height < 3 || inner.width < 10 {
            return;
        }
        let y = (inner.y + inner.height - 1) as i32;
        let full = format!(
            " reflow · {:>3.0} fps · {} cells · vid:{} · {}x{}  │  s swarm  t surf  space pause  q quit ",
            self.fps, self.last_cells, self.video.label(), area.width, area.height
        );
        let w = inner.width as usize;
        let text: String = full.chars().take(w).collect();
        let mut padded = text.clone();
        for _ in padded.chars().count()..w {
            padded.push(' ');
        }
        let st = Style::default()
            .fg(Color::Black)
            .bg(Color::Rgb(120, 200, 255))
            .add_modifier(Modifier::BOLD);
        p.put_str(inner.x as i32, y, &padded, st);
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
/// Draw a horizontal edge of the box at row `y`. The box spans `bx0..=bx1`,
/// `by0..=by1`. Each cell is colored by `loop_color` using its position on the
/// closed perimeter loop. Gaps stream a scrolling binary payload (see `stream_bit`).
fn h_edge(p: &mut Painter, y: i32, bx0: i32, by0: i32, bx1: i32, by1: i32,
          gaps: &[(i32, i32)], t: f32, seed: usize, level: usize, msg: &[u8]) {
    for x in bx0 + 1..bx1 {
        p.put(x, y, '─', loop_color(x, y, bx0, by0, bx1, by1, t, level));
    }
    for (gi, &(a, b)) in gaps.iter().enumerate() {
        let ca = a.max(bx0 + 1);
        let cb = b.min(bx1 - 1);
        if cb < ca { continue; }
        let band = seed.wrapping_add(gi * 3);
        let dir = if band % 2 == 0 { 1.0_f32 } else { -1.0 };
        let span = (cb - ca + 1).max(1) as f32;
        for x in ca..=cb {
            let pos = (x - ca) as f32 / span;
            // Pin the bit to the absolute column so the gap is a window sliding
            // over a fixed broadcast; `band` offsets each band into the stream.
            let one = stream_bit(msg, x as f32 - t * dir * BIT_SPEED + band as f32 * 11.0);
            let ch = if one { '▪' } else { '▫' }; // solid square = 1, open square = 0
            p.put(x, y, ch, stream_color(pos, t, band, dir, one));
        }
        if cb > ca {
            p.put(ca, y, '┤', loop_color(ca, y, bx0, by0, bx1, by1, t, level));
            p.put(cb, y, '├', loop_color(cb, y, bx0, by0, bx1, by1, t, level));
        }
    }
}

/// Draw a vertical edge of the box at column `x`. The box spans `bx0..=bx1`,
/// `by0..=by1`. Each cell is colored by `loop_color`. Gaps are independent.
fn v_edge(p: &mut Painter, x: i32, bx0: i32, by0: i32, bx1: i32, by1: i32,
          gaps: &[(i32, i32)], t: f32, seed: usize, level: usize, msg: &[u8]) {
    for y in by0 + 1..by1 {
        p.put(x, y, '│', loop_color(x, y, bx0, by0, bx1, by1, t, level));
    }
    for (gi, &(a, b)) in gaps.iter().enumerate() {
        let ca = a.max(by0 + 1);
        let cb = b.min(by1 - 1);
        if cb < ca { continue; }
        let band = seed.wrapping_add(gi * 3 + 1);
        let dir = if band % 2 == 0 { 1.0_f32 } else { -1.0 };
        let span = (cb - ca + 1).max(1) as f32;
        for y in ca..=cb {
            let pos = (y - ca) as f32 / span;
            // Same broadcast, read top-to-bottom; solid square reads as 1,
            // open square as 0 — the same bit glyphs the horizontal edges use.
            let one = stream_bit(msg, y as f32 - t * dir * BIT_SPEED + band as f32 * 11.0);
            let ch = if one { '▪' } else { '▫' }; // solid square = 1, open square = 0
            p.put(x, y, ch, stream_color(pos, t, band, dir, one));
        }
        if cb > ca {
            p.put(x, ca, '┴', loop_color(x, ca, bx0, by0, bx1, by1, t, level));
            p.put(x, cb, '┬', loop_color(x, cb, bx0, by0, bx1, by1, t, level));
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
    // Scale gap width with the edge so bands stay visible at all box sizes.
    let hw = len as f32 * (0.04 + 0.07 * ((phase * 1.7).sin() * 0.5 + 0.5));
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

/// Travelling cosine height field over normalised coordinates `(nx, ny)` in
/// 0..1, returning 0..1 — a small 2-D Fourier sum (cosine-transform field). Each
/// spectral component's *coefficient breathes* on its own slow cycle while its
/// phase travels, so the field is genuinely animated, not just scrolled: a
/// dominant swell and a steeper wash roll along +x, with a cross-shore
/// undulation and a diagonal interference term. The treemap cuts its boxes at
/// this field's valleys, so as the coefficients drift the partition flows.
fn surf_height(nx: f32, ny: f32, t: f32) -> f32 {
    use std::f32::consts::TAU;
    // A sum of plane waves travelling in many 2-D directions, so the field is
    // isotropic — crests run every which way and interfere, giving ordered
    // chaos rather than x/y banding. Each: (kx, ky, speed, amplitude, breathe).
    // Amplitudes fall off with wavenumber (a ~1/|k| surf spectrum) and each
    // coefficient waxes and wanes over time, so the pattern is alive, not just
    // scrolled — crests are born, merge and split as the components beat.
    let waves = [
        (1.2_f32, 0.3_f32, 0.7_f32, 0.45_f32, 0.21_f32),
        (-0.5, 1.4, 0.9, 0.40, 0.27),
        (2.3, 1.1, 1.3, 0.34, 0.17),
        (1.1, -2.4, 1.1, 0.30, 0.33),
        (3.4, 2.2, 1.7, 0.28, 0.41),
        (-2.6, 3.0, 1.9, 0.26, 0.29),
        (4.8, -1.6, 2.3, 0.24, 0.47),
        (0.6, 4.3, 2.1, 0.24, 0.37),
        (5.5, 2.7, 2.5, 0.20, 0.53),
        (-4.0, 5.1, 2.2, 0.20, 0.23),
        (6.8, -3.4, 2.9, 0.17, 0.61),
        (-6.2, -5.0, 3.0, 0.16, 0.31),
        (8.0, 4.5, 3.3, 0.14, 0.43),
        (3.0, 7.6, 3.1, 0.14, 0.19),
    ];
    let mut h = 0.0;
    let mut amp = 0.0;
    for (kx, ky, spd, a0, br) in waves {
        let a = a0 * (0.7 + 0.3 * (t * br).sin());
        h += a * (TAU * (kx * nx + ky * ny) - spd * t).cos();
        amp += a0;
    }
    (h / (2.0 * amp) + 0.5).clamp(0.0, 1.0)
}

/// Choose a swarm grid (cols, rows) from the terminal size, aiming for tiles
/// roughly large enough to host a recognisable mini-spiral.
fn grid_dims(area: Rect) -> (usize, usize) {
    let cols = (area.width as usize / 18).clamp(2, 8);
    let rows = (area.height as usize / 9).clamp(2, 6);
    (cols, rows)
}

/// Color for one cell on a box's closed perimeter loop.
///
/// Delegates the perimeter geometry to `Rect::border_pos` (mullion) and the
/// Gaussian bump shape to `ease::gaussian` (mullion).  Application logic —
/// the level-based base color and the specific bump parameters — stays here.
fn loop_color(x: i32, y: i32, bx0: i32, by0: i32, bx1: i32, by1: i32, t: f32, level: usize) -> Style {
    // Map (x, y) to a normalised position on the closed clockwise perimeter.
    let rect = Rect::new(
        bx0 as u16, by0 as u16,
        (bx1 - bx0 + 1) as u16, (by1 - by0 + 1) as u16,
    );
    let s_norm = rect.border_pos(x as u16, y as u16);

    // Base color: level-only, no time component, so each level's identity is
    // stable and the bumps are the only moving parts.
    let base_hue = (level as f32 * 137.508) % 360.0;
    let base_val = 0.50 + 0.20 * ((level as f32 * 1.3).sin() * 0.5 + 0.5);

    // Three bumps: (phase_seed, angular_velocity, hue_delta, val_delta, sigma).
    // Level-dependent seeds give each nesting level a distinct starting phase.
    let fi = level as f32;
    let bumps = [
        ((fi * 0.19) % 1.0,  0.07 + fi * 0.003,  40.0_f32,  0.30_f32, 0.06_f32),
        ((fi * 0.41) % 1.0, -0.05 - fi * 0.002, -30.0,      0.18,     0.09),
        ((fi * 0.67) % 1.0,  0.11 + fi * 0.004,  18.0,       0.12,     0.05),
    ];

    let mut hue_add = 0.0_f32;
    let mut val_add = 0.0_f32;
    for (phase0, omega, dhue, dval, sigma) in bumps {
        let center = (phase0 + omega * t).rem_euclid(1.0);
        let diff = (s_norm - center + 0.5).rem_euclid(1.0) - 0.5; // wrap-around distance
        let g = gaussian(diff, sigma);
        hue_add += g * dhue;
        val_add += g * dval;
    }

    let hue = base_hue + hue_add;
    let val = (base_val + val_add).clamp(0.1, 1.0);
    Style::default().fg(Color::from_hsv(hue, 0.85, val))
}

/// Paragraphs flowed inside the reflow-scene tiles. Each tile wraps one of
/// these to its current interior width and re-wraps it as the tile breathes.
const PASSAGES: [&str; 4] = [
    "Mullion re-flows this paragraph inside a tile that breathes: as the box widens and narrows the words re-wrap every frame, and a Gray-Scott reaction-diffusion field tints each glyph by its local concentration.",
    "Four to eight gaps drift around the border and cross the corners without a seam. They pulse, split into diverging pairs, and merge back together while the live telemetry bitstream scrolls through each opening.",
    "The perimeter is one continuous strip, so an opening can slide off a side and turn the corner as a single moving window — the corner glyph itself dissolves into the stream and reforms once the gap has passed.",
    "Colour here is not decoration but data: the same reaction that paints the text is a simulation running under it, feeding and killing concentration so spots divide, drift, and bloom across the wrapped lines.",
];

/// A well-mixed pseudo-random byte from three integer coordinates — used to
/// drive the braille static (a SplitMix64-style finalizer over a hash of the
/// cell position and the frame number).
fn noise_byte(x: u32, y: u32, z: u64) -> u8 {
    let mut h = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (y as u64).wrapping_mul(0xD1B5_4A32_D192_ED03)
        ^ z.wrapping_mul(0x8537_5C0E_84A0_5C5D);
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 31;
    (h & 0xFF) as u8
}

/// What a window's interior is filled with.
enum WindowKind {
    /// Wave-coloured flowing text.
    Text,
    /// A braille noise field (a TV tuned to static).
    Static,
    /// A braille video panel (a TV playing a synthesised channel).
    Video,
}

/// Pick a window's content kind from a stable per-window hash (so a given box
/// keeps its kind across frames). Bigger tiles (shallower `depth`) lean toward video
/// panels — ~¼ static, ~½ video, ~¼ text at depth ≤ 1; ~¼ static, ~¼ video, ~½ text
/// deeper down.
fn window_kind(seed: u64, depth: usize) -> WindowKind {
    let pick = noise_byte(seed as u32, (seed >> 32) as u32, depth as u64) % 4;
    let video_slots: u8 = if depth <= 1 { 2 } else { 1 };
    match pick {
        0 => WindowKind::Static,
        p if p <= video_slots => WindowKind::Video,
        _ => WindowKind::Text,
    }
}

/// Synthesise one `w×h` 8-bit luma frame into `buf` — a stand-in for a decoded
/// video frame (what `ffmpeg … -f rawvideo -pix_fmt gray -` would hand you, and
/// what a real build would `read_exact` instead of generating).
///
/// The "channel" is a moving test pattern: a travelling plasma wash, a bright
/// disc bouncing across the screen, and faint CRT scanlines — enough motion and
/// structure to read clearly as footage once dithered to braille. `seed` phase-
/// shifts everything so different video windows show different channels.
fn synth_frame(buf: &mut [u8], w: usize, h: usize, t: f32, seed: u64) {
    if w == 0 || h == 0 {
        return;
    }
    let sd = seed as f32 * 0.7;
    let aspect = w as f32 / h as f32;
    // Bouncing disc centre (normalised), distinct per channel.
    let bx = 0.5 + 0.38 * (t * 1.3 + sd).sin();
    let by = 0.5 + 0.38 * (t * 1.1 + sd * 0.6).cos();
    for fy in 0..h {
        let v = (fy as f32 + 0.5) / h as f32;
        // Faint scanlines: every other source row a touch dimmer.
        let scan = if fy % 2 == 0 { 1.0 } else { 0.84 };
        for fx in 0..w {
            let u = (fx as f32 + 0.5) / w as f32;
            // Travelling plasma background.
            let bg = 0.5 + 0.5 * ((u * 7.0 + t * 1.2).sin() * (v * 5.0 - t * 0.8).cos());
            // Bright bouncing disc.
            let (dx, dy) = ((u - bx) * aspect, v - by);
            let disc = (1.0 - (dx * dx + dy * dy).sqrt() / 0.16).clamp(0.0, 1.0);
            let val = (bg * 0.45 + disc * 0.95).clamp(0.0, 1.0) * scan;
            buf[fy * w + fx] = (val * 255.0) as u8;
        }
    }
}

/// Frame resolution decoded for the video panels. Fixed and independent of any
/// window's size — each panel resamples it to its own braille grid.
const VIDEO_W: usize = 192;
const VIDEO_H: usize = 108;

/// A source of grayscale video frames for the braille panels. Yields the latest
/// frame as a fixed-resolution `W×H` row-major luma buffer.
trait VideoSource {
    /// Frame dimensions (constant for the source's lifetime).
    fn dims(&self) -> (usize, usize);
    /// The latest frame, row-major 8-bit luma, `len() == w * h`.
    fn frame(&self) -> &[u8];
    /// Refresh the current frame: regenerate (synth) or pull the newest decoded
    /// frame (ffmpeg). Called once per rendered frame. `t` is animation time.
    fn tick(&mut self, t: f32);
    /// Short label for the HUD (`synth` / `ffmpeg`).
    fn label(&self) -> &'static str;
}

/// A synthesised channel — the moving test pattern from [`synth_frame`]. Stands
/// in for real footage when no `SPIRAL_VIDEO` clip is configured.
struct SynthSource {
    w: usize,
    h: usize,
    frame: Vec<u8>,
}

impl SynthSource {
    fn new(w: usize, h: usize) -> Self {
        Self { w, h, frame: vec![0u8; w * h] }
    }
}

impl VideoSource for SynthSource {
    fn dims(&self) -> (usize, usize) {
        (self.w, self.h)
    }
    fn frame(&self) -> &[u8] {
        &self.frame
    }
    fn tick(&mut self, t: f32) {
        synth_frame(&mut self.frame, self.w, self.h, t, 0);
    }
    fn label(&self) -> &'static str {
        "synth"
    }
}

/// Real footage: spawns `ffmpeg … -f rawvideo -pix_fmt gray -` once and a reader
/// thread pumps decoded `W×H` luma frames into a shared buffer. [`tick`] snapshots
/// the newest one. The child is killed on drop, which ends the reader thread.
struct FfmpegSource {
    w: usize,
    h: usize,
    frame: Vec<u8>,
    shared: Arc<Mutex<Vec<u8>>>,
    child: Child,
}

impl FfmpegSource {
    fn new(path: &str, w: usize, h: usize) -> io::Result<Self> {
        let mut child = Command::new("ffmpeg")
            .args([
                "-loglevel", "error",
                "-re", // pace input at native frame rate
                "-stream_loop", "-1", // loop the clip forever
                "-i", path,
                "-vf", &format!("scale={w}:{h}"),
                "-pix_fmt", "gray",
                "-f", "rawvideo",
                "-",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        let mut out = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("ffmpeg produced no stdout"))?;
        let shared = Arc::new(Mutex::new(vec![0u8; w * h]));
        let writer = Arc::clone(&shared);
        let frame_len = w * h;
        thread::spawn(move || {
            let mut buf = vec![0u8; frame_len];
            // Read whole frames until the pipe closes (EOF or the child is killed).
            while out.read_exact(&mut buf).is_ok() {
                if let Ok(mut g) = writer.lock() {
                    g.copy_from_slice(&buf);
                }
            }
        });

        Ok(Self { w, h, frame: vec![0u8; w * h], shared, child })
    }
}

impl Drop for FfmpegSource {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl VideoSource for FfmpegSource {
    fn dims(&self) -> (usize, usize) {
        (self.w, self.h)
    }
    fn frame(&self) -> &[u8] {
        &self.frame
    }
    fn tick(&mut self, _t: f32) {
        if let Ok(g) = self.shared.lock() {
            self.frame.copy_from_slice(&g);
        }
    }
    fn label(&self) -> &'static str {
        "ffmpeg"
    }
}

/// Pick the video source: an ffmpeg stream of `$SPIRAL_VIDEO` if that points at a
/// playable clip, otherwise the synthesised channel. Falls back (with a note)
/// rather than failing if ffmpeg can't be spawned.
fn make_video_source() -> Box<dyn VideoSource> {
    if let Ok(path) = std::env::var("SPIRAL_VIDEO") {
        if !path.is_empty() {
            match FfmpegSource::new(&path, VIDEO_W, VIDEO_H) {
                Ok(src) => return Box::new(src),
                Err(e) => {
                    eprintln!("spiral_stress: could not start ffmpeg for SPIRAL_VIDEO ({e}); using synth");
                }
            }
        }
    }
    Box::new(SynthSource::new(VIDEO_W, VIDEO_H))
}

/// Cells the bitstream scrolls past per second of animation time.
const BIT_SPEED: f32 = 5.0;

/// The bit of `msg` at continuous stream coordinate `s` (measured in edge cells).
/// `msg` is the live telemetry rebuilt each frame (see `Demo::telemetry`): 8 bits
/// per byte, MSB first, looping. The fractional part of `s` is floored to a cell,
/// so every position on every edge is a window onto the same endless broadcast.
/// Screenshot a gap, read filled = 1 / hollow = 0, and it decodes back to ASCII.
fn stream_bit(msg: &[u8], s: f32) -> bool {
    if msg.is_empty() {
        return false;
    }
    let total = (msg.len() * 8) as i32;
    let idx = (s.floor() as i32).rem_euclid(total);
    let byte = msg[(idx / 8) as usize];
    (byte >> (7 - (idx % 8))) & 1 == 1 // MSB first
}

/// Color for one cell inside a streaming band.
///
/// `pos` is 0..1 along the gap, `band` seeds the hue family (golden-angle
/// spacing gives each band a distinct color), `dir` is ±1 for stream direction.
/// As `t` increases the gradient appears to scroll along the edge. `one` lifts
/// the brightness of set bits so the data reads clearly against the zeros.
fn stream_color(pos: f32, t: f32, band: usize, dir: f32, one: bool) -> Style {
    // Golden-angle hue spacing: each band gets a maximally distinct base hue.
    let base_hue = (band as f32 * 137.508) % 360.0;
    // Shift position by time so the gradient scrolls (= streaming motion).
    let p = pos + t * dir * 0.55;
    // 90° hue sweep within the band; wrapping handled inside hsv().
    let hue = base_hue + p * 90.0;
    // Brightness and saturation shimmer independently for a sparkle feel.
    let val = 0.45 + 0.55 * (p * std::f32::consts::TAU * 1.5).sin().powi(2);
    let sat = 0.70 + 0.30 * (p * std::f32::consts::TAU * 2.3).cos().abs();
    // Set bits glow brighter than the zeros so the payload is legible.
    let val = if one { (val + 0.30).min(1.0) } else { val * 0.75 };
    Style::default().fg(Color::from_hsv(hue, sat, val))
}

// Keep `Modifier` referenced so a future bold/styling tweak is one edit away
// and the import doesn't warn if styling is toggled off during experiments.
#[allow(dead_code)]
fn _bold(st: Style) -> Style {
    st.add_modifier(Modifier::BOLD)
}
