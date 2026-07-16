// SPDX-License-Identifier: GPL-3.0-or-later
use crate::{AppMode, AppState, AppView, BarEntry, KubeConn, NomadConn, Metric, PeakVals, Side, AnomalyState, fleet, Region};
use mullion::{Buffer, BorderGap, Rect, gaussian, tree::id_from_key};
use mullion::border::{draw_box, render_rim, render_shared, Borders, BorderStyle, CornerStyle, LineWeight};
use mullion::field::Field;
use mullion::layout::{TileId, Node, Orientation, Constraint, Size};
use mullion::label::Align;
use mullion::outline::render_tree_row;
use mullion::style::{Color, Modifier, Style};
use mullion::table::{ColumnDef, ColumnGrid, ColumnKind};
use mullion::text::TextCtx;
use mullion::Table;
use mullion::Theme;
use std::collections::HashMap;

pub const BODY_ID: TileId = 2;

/// Top-level render entry point.
///
/// The terminal is framed by a single outer box whose top edge carries the
/// histogram legend (when active) and whose bottom edge carries compact status
/// and key-hint gaps.  Inside the box an optional 1-row header-content strip
/// holds contextual info (errors, remote mode, manual title).  The remaining
/// interior is the body — either a single pane or a 2/3 + 1/3 horizontal split
/// when the thread view is open.
pub fn render(buf: &mut Buffer, state: &mut AppState) {
    let area = buf.area;
    if area.width < 6 || area.height < 4 { return; }

    draw_outer_border(buf, area, state);

    // Interior is inset by 1 on all sides.
    let inner = Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2);
    if inner.height == 0 { return; }

    // One optional header-content row.
    let has_hdr = has_header_content(state);
    let (content_row, body_rect) = if has_hdr && inner.height >= 2 {
        (Some(Rect::new(inner.x, inner.y, inner.width, 1)),
         Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1))
    } else {
        (None, inner)
    };
    if let Some(cr) = content_row { render_header_content(buf, cr, state); }

    let outer_y0 = area.y;
    let outer_y1 = area.y + area.height - 1;
    match state.view.clone() {
        AppView::Groups | AppView::Remote { .. } => render_body(buf, body_rect, state),
        AppView::Threads { .. } if matches!(state.mode, AppMode::Local) =>
            render_body_with_threads(buf, body_rect, outer_y0, outer_y1, state),
        AppView::Threads { .. } => render_body(buf, body_rect, state),
        AppView::Manual => render_manual(buf, body_rect, state),
        AppView::Connecting { label } => render_connecting(buf, body_rect, &label),
        AppView::Scope if state.scope_detect => render_verdict(buf, body_rect, state),
        AppView::Scope => render_scope(buf, body_rect, state),
        AppView::Fleet => render_fleet(buf, body_rect, state),
    }
}

/// Returns true when a 1-row content strip is needed just inside the top border.
fn has_header_content(state: &AppState) -> bool {
    match &state.view {
        AppView::Groups => state.error.is_some() || state.proxmox_insecure,
        AppView::Remote { .. } | AppView::Connecting { .. } | AppView::Manual => true,
        AppView::Threads { .. } => false, // thread info lives in the right pane
        AppView::Scope => false,          // scope draws its own header rows in the body
        AppView::Fleet => false,          // Fleet draws its own regions; no separate header strip.
    }
}

/// Render the 1-row header content strip (errors, remote info, manual title).
fn render_header_content(buf: &mut Buffer, area: Rect, state: &AppState) {
    let dim = Style::default().fg(Color::DarkGray);
    let mut x = area.x;
    match &state.view {
        AppView::Groups => {
            if let Some(err) = &state.error {
                x = buf.set_string(x, area.y,
                    &format!(" error: {}", err.lines().next().unwrap_or("")),
                    Style::default().fg(Color::Red));
            } else if state.proxmox_insecure {
                x = buf.set_string(x, area.y, "  ⚠ TLS OFF",
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
            }
        }
        AppView::Remote { label } => {
            x = buf.set_string(x, area.y, " remote: ", dim);
            x = buf.set_string(x, area.y, label,
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
            x = buf.set_string(x, area.y, "  ·  [Esc] disconnect", dim);
        }
        AppView::Connecting { label } => {
            x = buf.set_string(x, area.y, " Connecting to ", dim);
            x = buf.set_string(x, area.y, label,
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
            x = buf.set_string(x, area.y, "  …", dim);
        }
        AppView::Manual => {
            x = buf.set_string(x, area.y, " manual",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
            x = buf.set_string(x, area.y,
                "  ·  ↑/↓ to scroll  ·  [m] or [Esc] to close", dim);
        }
        AppView::Threads { .. } | AppView::Scope | AppView::Fleet => {}
    }
    let _ = x;
}

/// Draw the outer box: `╭`/`╮`/`╰`/`╯` corners, `│` sides, top border with
/// histogram legend gap, bottom border with status and key-hint gaps.
/// Three-pass outer border rendering:
///   1. structural glyphs — corners, side bars, dash fills
///   2. gap content       — text/animations drawn into gap rects
///   3. rim glow          — applied last; gap-aware skip protects content cells
///
/// With glow last, `rim_glow = false` gaps are skipped entirely so their
/// content colours survive.  `rim_glow = true` gaps are coloured by the
/// animation even though content has already been drawn there.
fn draw_outer_border(buf: &mut Buffer, area: Rect, state: &AppState) {
    let dim = Style::default().fg(Color::DarkGray);
    let x0 = area.x;
    let y0 = area.y;
    let x1 = area.x + area.width - 1;
    let y1 = area.y + area.height - 1;

    // Pass 1 — structural glyphs: one rounded box drawn by mullion (corners
    // ╭╮╰╯, side │ bars, top/bottom ─ fills). Content (pass 2) and the rim glow
    // (pass 3) overwrite the gap cells afterwards.
    draw_box(buf, area, Borders::ALL, &BorderStyle {
        weight: LineWeight::Light,
        corners: CornerStyle::Rounded,
        style: dim,
    });

    // Offender fallback positions (banded by period) when no confident fingerprint.
    let offenders: Vec<RimOffender> = state
        .offender_report
        .as_ref()
        .map(|r| {
            r.offenders
                .iter()
                .filter(|o| o.confidence >= 0.25) // match the scope's MIN_CONF
                .take(4)
                .map(|o| RimOffender { phase: o.phase, period: o.period_s })
                .collect()
        })
        .unwrap_or_default();
    // Probe-grade stutter fingerprint when the latency probe is alive; otherwise
    // the rim folds its own frame-cadence events.
    let probe_shape = if state.scope.is_some() { state.stutter_shape.as_ref() } else { None };

    // Fold the current stutter into segments *first* (no drawing yet) so the
    // header/footer can flow around them.
    let (segs, sev, orbit_pos) = compute_rim(area, &offenders, probe_shape);
    let top_reserved = edge_reserved(&segs, y0);
    let bot_reserved = edge_reserved(&segs, y1);

    // Pass 2 — content, flowing around the stutter braille so both stay visible.
    let mut gaps = draw_top_border(buf, y0, x0, x1, state, dim, &top_reserved);
    gaps.extend(draw_bottom_border(buf, y1, x0, x1, state, dim, &bot_reserved));

    // Pass 3 — the stutter braille at its phase (skipping cells the content kept),
    // then the orbiters last (skipping every content + stutter gap).
    let braille_gaps = draw_stutter_segments(buf, &segs, sev, &gaps);
    gaps.extend(braille_gaps);
    draw_orbiters(buf, area, &gaps, orbit_pos);
}

/// A periodic offender reduced to what the rim needs for the fallback marker when
/// no fingerprint is confident: where it sits (`phase` ∈ [0,1)) and its `period`
/// (for the band colour). Built from [`crate::diag::Offender`] in
/// [`draw_outer_border`].
struct RimOffender {
    phase: f32,
    period: f64,
}

// ── Gap declarations (between passes 1 and 2) ─────────────────────────────────

/// Compute the **top** border's legend gaps for the current frame.
///
/// (The bottom border draws its content and returns its own gaps together — see
/// [`draw_bottom_border`] — so a single layout drives both, with no duplicated
/// geometry to drift out of sync.)
///
/// Structural chars (──, ┤, ├, padding dashes) between and around the legend
/// sections are left exposed so the rim glow travels visibly across the full top
/// edge and only skips the coloured content.
fn draw_top_border(buf: &mut Buffer, y: u16, x0: u16, x1: u16, state: &AppState, dim: Style, reserved: &[(u16, u16)]) -> Vec<BorderGap> {
    let mut gaps: Vec<BorderGap> = Vec::new();
    const FIXED: usize = 50;
    const MAX_SWATCH: usize = 28;
    const MIN_SWATCH: usize = 4;
    let show_legend = state.show_histogram
        && matches!(state.view, AppView::Groups | AppView::Remote { .. });
    let inner_w = (x1 - x0).saturating_sub(1) as usize;
    if !show_legend || inner_w < FIXED + MIN_SWATCH {
        return gaps;
    }
    let available = inner_w - FIXED;
    let swatch_w  = available.min(MAX_SWATCH);
    let extra     = available - swatch_w;
    let left_pad  = extra / 2;
    let right_pad = extra - left_pad;

    // The three coloured pieces flow past any stutter-braille reserved interval
    // (skip_reserved before each) so the braille and the legend coexist; the gap
    // recorded for each is its *actual* drawn span, so the glow protects it.
    let mut x = x0 + 1;
    x = buf.set_string(x, y, "──┤", dim);
    x = skip_reserved(x, reserved);
    let g = x;
    x = buf.set_string(x, y, "← balanced", Style::default().fg(Color::Rgb(60, 180, 60)));
    gaps.push(BorderGap::new(Rect::new(g, y, x.saturating_sub(g), 1)));
    x = buf.set_string(x, y, "├──", dim);
    if left_pad > 0 { x = buf.set_string(x, y, &"─".repeat(left_pad), dim); }
    x = buf.set_string(x, y, "┤", dim);
    x = skip_reserved(x, reserved);
    let g = x;
    for i in 0..swatch_w {
        let frac = i as f64 / (swatch_w - 1).max(1) as f64;
        x = buf.set_string(x, y, "◻", Style::default().fg(planck_color(frac)));
    }
    x = buf.set_string(x, y, " = work density", dim);
    gaps.push(BorderGap::new(Rect::new(g, y, x.saturating_sub(g), 1)));
    x = buf.set_string(x, y, "├", dim);
    if right_pad > 0 { x = buf.set_string(x, y, &"─".repeat(right_pad), dim); }
    x = buf.set_string(x, y, "──┤", dim);
    x = skip_reserved(x, reserved);
    let g = x;
    x = buf.set_string(x, y, "hot spots →", Style::default().fg(Color::Rgb(220, 80, 0)));
    gaps.push(BorderGap::new(Rect::new(g, y, x.saturating_sub(g), 1)));
    x = buf.set_string(x, y, "├──", dim);
    let _ = x;
    gaps
}

/// One bookended segment on the bottom border, drawn as `┤ text ├` with `style`
/// on the text. The text cells become a (non-glow) gap so the rim glow skips
/// them — the bookends and the dashes *between* segments still glow, so the glow
/// is the connective tissue and each segment is a crisp island.
struct BottomSeg {
    text: String,
    style: Style,
}

impl BottomSeg {
    /// Full `┤ text ├` span (assumes single-width glyphs, as the rest of the
    /// border layout does).
    fn full_w(&self) -> u16 {
        4 + self.text.chars().count() as u16
    }

    /// Draw at leading-`┤` position `x`; return the protected content gap.
    fn draw(&self, buf: &mut Buffer, x: u16, y: u16, dim: Style) -> BorderGap {
        let text_x = buf.set_string(x, y, "┤ ", dim);
        let after = buf.set_string(text_x, y, &self.text, self.style);
        buf.set_string(after, y, " ├", dim);
        BorderGap::new(Rect::new(text_x, y, after.saturating_sub(text_x), 1))
    }
}

/// The colour-popped stutter readout, or `None` when no confident fingerprint.
fn stutter_segment(state: &AppState) -> Option<BottomSeg> {
    // Probe-grade fingerprint when the latency probe is alive, else the rim's own
    // frame-cadence fold — so the readout works with no probe (press-`d`-free).
    let sh = state.stutter_shape.clone().or_else(rim_frame_shape)?;
    if sh.confidence < 0.30 {
        return None;
    }
    let text = format!(
        "stutter ~{:.1}s · {:.0}–{:.0}ms",
        sh.period_s, sh.dur_mean_ms, sh.dur_max_ms
    );
    // Pop: heat colour by the worst stall length (≈0 ms green → ≥300 ms red), bold.
    let level: f32 = (sh.dur_max_ms / 300.0).clamp(0.0, 1.0);
    let style = Style::default().fg(heat_color(level)).add_modifier(Modifier::BOLD);
    Some(BottomSeg { text, style })
}

/// Draw the bottom border content as a coordinated layout — `status` left,
/// `stutter` centre (colour-popped), `keys` right — and return the content gaps.
/// Segments are placed so they never overlap; the centre stutter is dropped first
/// when space is tight.
fn draw_bottom_border(buf: &mut Buffer, y: u16, x0: u16, x1: u16, state: &AppState, dim: Style, reserved: &[(u16, u16)]) -> Vec<BorderGap> {
    // GPU selector takes the whole edge (one wide region, unchanged behaviour).
    if write_gpu_selector_line(buf, state, x0 + 2, y) {
        return vec![BorderGap::new(Rect::new(x0 + 2, y, x1.saturating_sub(x0 + 2), 1))];
    }

    let mut gaps = Vec::new();

    // Status, anchored left but shifted right past any stutter braille at the left.
    let left = BottomSeg { text: border_status(state), style: Style::default().fg(Color::DarkGray) };
    let lx = skip_reserved(x0 + 2, reserved);
    let l_end = lx + left.full_w();
    gaps.push(left.draw(buf, lx, y, dim));

    // Keys, anchored right but shifted left past any stutter braille on the right.
    let keys_style = if state.history_cursor.is_some() {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Rgb(55, 55, 55))
    };
    let right = BottomSeg { text: border_keys(state), style: keys_style };
    let rw = right.full_w();
    let mut rx = x1.saturating_sub(rw + 2);
    for &(a, b) in reserved {
        if rx <= b && rx + rw >= a {
            rx = a.saturating_sub(rw + 1); // clear the braille on its left
        }
    }
    let right_bound = if rx > l_end + 1 {
        gaps.push(right.draw(buf, rx, y, dim));
        rx
    } else {
        x1.saturating_sub(2)
    };

    // The stutter readout sits just right of the status, flowed past any braille,
    // drawn only if it still clears the keys.
    if let Some(seg) = stutter_segment(state) {
        let sx = skip_reserved(l_end + 2, reserved);
        if sx + seg.full_w() + 1 < right_bound {
            gaps.push(seg.draw(buf, sx, y, dim));
        }
    }

    gaps
}

// ── Gap content (pass 3) ──────────────────────────────────────────────────────

/// Rasterise one border cell of a knot into a 2×4 **braille** dot mask.
///
/// `amp ∈ [0, 1]` is the comet intensity at this cell; `col` is the cell's index
/// around the perimeter (it seeds the Bayer dither so neighbours don't share a
/// pattern).  Dots fill **bottom-anchored**: the bottom row lights first and the
/// top row last, so on the horizontal top/bottom edges the lit height reads as a
/// little level-meter bar (the "height" encoding) while a 4×4 ordered-dither
/// scatters the partial rows — a fading comet tail therefore thins into ever
/// sparser dots (the "distance between them" growing with the hold-up).  On the
/// vertical side edges the same mask reads as a dot cluster whose *length* along
/// the rim carries the signal instead, since a within-cell height bar only reads
/// cleanly where travel is horizontal.
///
/// Returns `0` (no dots → `⠀`) when the comet has not reached this cell, so the
/// caller can leave the box-drawing glyph untouched and the rim stays a clean
/// line at rest.
fn comet_braille_mask(amp: f32, col: usize) -> u8 {
    // 4×4 Bayer matrix → 16 ordered-dither levels (same matrix mullion's
    // `Field::render_braille_xy` uses), tiled over the sub-pixel grid.
    const BAYER: [[u8; 4]; 4] =
        [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
    let mut mask = 0u8;
    for sy in 0..4u16 {
        // Bottom row (sy = 3) needs the least intensity → fills first.
        let row_thr = (4 - sy) as f32 / 4.0;
        for sx in 0..2u16 {
            let gx = ((col as u16 * 2 + sx) % 4) as usize;
            let dither = BAYER[(sy % 4) as usize][gx] as f32 / 16.0;
            let thr = row_thr * 0.8 + dither * 0.2;
            if amp > thr {
                // Unicode braille bit layout: dots 1-6 column-major, 7-8 the
                // bottom row (U+2800 + mask).
                mask |= if sy < 3 { 1 << (sx * 3 + sy) } else { 1 << (6 + sx) };
            }
        }
    }
    mask
}

/// Stall detector shared across redraws of the rim glow.
///
/// `apply_border_glow` runs once per rendered frame, so the wall-clock gap
/// between two successive calls *is* the draw loop's frame interval.  A healthy
/// loop wakes on the 50 ms `RENDER_TICK`; a system-wide stall (compositor or
/// scheduler hold-up — the very thing the orbiting glow visibly hitches on)
/// stretches that gap.  We decay that excess into a 0..1 severity used to *pulse*
/// the knots' brightness (they are shown whenever a recurring problem is
/// detected, not only while aerie itself stalls).  See `docs/rim-latency.md`.
/// One persistent stutter knot, kept across frames so that brief gaps in
/// detection (the offender/fingerprint confidence wobbling across its threshold
/// on a noisy system) do not blink the knot off.  Refreshed to `intensity = 1.0`
/// whenever a matching knot is detected, and decayed otherwise.
#[derive(Clone, Copy)]
struct HeldKnot {
    /// Rim position in `[0, 1)`.
    phase: f32,
    /// Normalised stall duration (fingerprint) or a fixed marker (offender fallback).
    height: f32,
    /// Band colour, chosen from the stutter's period by [`band_colour`].
    colour: (f32, f32, f32),
    /// 1.0 when freshly detected, decays by half every `HALF_LIFE` otherwise, so a
    /// vanished stutter fades out; a recurrence resets it to 1.0.
    intensity: f32,
}

/// An RGB triple in float form, as the rim mixes colours additively.
type Rgb = (f32, f32, f32);

/// Colour for a stutter of period `period_s`, matching the orbiter bands:
/// `[0,1)` → violet, `[1,2)` → teal, `[2,4)` → orange-yellow, `[4,8+)` → red.
fn band_colour(period_s: f64) -> (f32, f32, f32) {
    if period_s < 1.0 {
        (170.0, 80.0, 255.0) // violet
    } else if period_s < 2.0 {
        (0.0, 200.0, 180.0) // teal
    } else if period_s < 4.0 {
        (255.0, 175.0, 0.0) // orange-yellow
    } else {
        (235.0, 55.0, 40.0) // red
    }
}

struct RimTrail {
    /// Wall-clock seconds of the previous frame (same clock as the orbit phase).
    last_t: f32,
    /// Decaying stall severity (in perimeter-fraction units): jumps up on a slow
    /// frame, falls off smoothly, so a single long frame leaves a ~1 s tail.
    smear: f32,
    /// Persistent knot set (see [`HeldKnot`]) and the wall-clock of its last
    /// decay, so detection dropouts hold rather than flicker.
    held: Vec<HeldKnot>,
    held_t: f32,
    /// Rolling `(t, lateness_ms)` per frame for ~60 s — the always-on,
    /// self-contained event source the stutter characteriser folds when no
    /// latency probe is running.  `lateness_ms` is the frame interval above the
    /// 50 ms render tick, so it is ~0 on a healthy frame and spikes on a stall.
    ring: std::collections::VecDeque<(f32, f32)>,
    /// Wall-clock of the last characterisation (throttles the ~heavy fold).
    last_char_t: f32,
    /// Cached frame-cadence stutter fingerprint (the ambient fallback).
    shape: Option<crate::diag::StutterShape>,
}

/// The rim's per-frame trail state. Module-scoped (not a function-local static)
/// so the bottom-border readout can read the frame-cadence fingerprint too —
/// that is what lets the stutter readout work with no latency probe running.
static RIM_TRAIL: std::sync::OnceLock<std::sync::Mutex<RimTrail>> = std::sync::OnceLock::new();

/// The latest frame-cadence stutter fingerprint the rim has folded, if any.
/// Lets [`stutter_segment`] show the stutter without a probe (press-`d`-free).
fn rim_frame_shape() -> Option<crate::diag::StutterShape> {
    RIM_TRAIL.get()?.lock().ok()?.shape.clone()
}

/// Animate four tight clockwise Gaussian orbiters (red 1 s, orange-yellow 2 s,
/// teal 4 s, violet 8 s) around the outer border, and draw the banded bookended
/// stutter readout at the stall's phase.
///
/// The orbiters are driven by wall-clock time, so a loop stall makes them visibly
/// freeze and then jump — that motion *is* the live latency signal.  Their colours
/// name the four stutter-period bands.  The glow is gap-aware: it flows over the
/// structural dashes *between* the content islands, not over their text.
///
/// `offenders` provide the fallback marker positions when no fingerprint is
/// confident; `probe_shape` is the probe-grade fingerprint when the latency probe
/// is alive, else the rim folds its own frame-cadence events.
/// The four orbiter bands: `(orbit_period_s, colour)`, octave-spaced.  The colour
/// names the stutter-period band — see [`band_colour`].
const ORBITERS: [(f32, Rgb); 4] = [
    (1.0, (235.0,  55.0,  40.0)), // red
    (2.0, (255.0, 175.0,   0.0)), // orange-yellow
    (4.0, (  0.0, 200.0, 180.0)), // teal
    (8.0, (170.0,  80.0, 255.0)), // violet
];
const ORBIT_SIGMA: f32 = 0.025; // tight, so the four read as distinct dots

/// A contiguous stutter structure to draw on the rim: its lit braille cells
/// (`x, y, mask`), the two `┤ ├` bookend cells framing it, and its band colour.
struct StutterSeg {
    cells: Vec<(u16, u16, u8)>,
    bookends: [((u16, u16), char); 2],
    colour: Rgb,
}

/// Update the rim's trail state and fold the current stutter into drawable
/// [`StutterSeg`]s, **without touching the buffer** — so the caller can lay the
/// header/footer content out around them (reflow) before anything is drawn.
///
/// Returns the segments, the live stall severity (for the brightness pulse), and
/// the four orbiter positions.
fn compute_rim(
    area: Rect,
    offenders: &[RimOffender],
    probe_shape: Option<&crate::diag::StutterShape>,
) -> (Vec<StutterSeg>, f32, [f32; 4]) {
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let t = START.get_or_init(Instant::now).elapsed().as_secs_f32();
    let orbit_pos: [f32; 4] = std::array::from_fn(|i| (t / ORBITERS[i].0).rem_euclid(1.0));

    if area.width < 2 || area.height < 2 {
        return (Vec::new(), 0.0, orbit_pos);
    }

    // ── Frame latency → stall severity + frame-cadence fingerprint ───────────
    const FLOOR_MS:     f32 = 60.0;
    const MS_PER_PERIM: f32 = 0.0006;
    const LEN_MAX:      f32 = 0.33;
    const TAU_S:        f32 = 0.6;
    const RING_S:       f32 = 60.0;
    const CHAR_EVERY:   f32 = 0.5;
    const EVENT_FLOOR:  f32 = 30.0;
    let (smear, frame_shape) = {
        let trail = RIM_TRAIL.get_or_init(|| {
            Mutex::new(RimTrail {
                last_t: t,
                smear: 0.0,
                held: Vec::new(),
                held_t: t,
                ring: std::collections::VecDeque::new(),
                last_char_t: t,
                shape: None,
            })
        });
        let mut s = trail.lock().unwrap();
        let dt = (t - s.last_t).max(0.0);
        s.last_t = t;
        let dt_ms = (dt * 1000.0).min(2000.0);
        let instant = ((dt_ms - FLOOR_MS).max(0.0) * MS_PER_PERIM).min(LEN_MAX);
        let decay = (-dt / TAU_S).exp();
        s.smear = (s.smear * decay).max(instant);
        s.ring.push_back((t, (dt_ms - 50.0).max(0.0)));
        while s.ring.front().is_some_and(|&(ft, _)| t - ft > RING_S) {
            s.ring.pop_front();
        }
        if probe_shape.is_none() && t - s.last_char_t >= CHAR_EVERY && s.ring.len() >= 64 {
            let samples: Vec<crate::diag::Sample> = s
                .ring
                .iter()
                .map(|&(ft, late)| crate::diag::Sample { t: ft as f64, overshoot_ms: late })
                .collect();
            s.shape = crate::diag::characterise_stutter(&samples, EVENT_FLOOR);
            s.last_char_t = t;
        }
        (s.smear, s.shape.clone())
    };
    let sev = (smear / LEN_MAX).clamp(0.0, 1.0);
    let shape = probe_shape.or(frame_shape.as_ref());

    // ── Held set: candidates → sticky merge with a half-life ─────────────────
    const KNOT_SIGMA: f32 = 0.02;
    const MATCH:      f32 = 0.03;
    const HALF_LIFE:  f32 = 4.0;
    let mut current: Vec<HeldKnot> = Vec::new();
    if let Some(fp) = shape.filter(|s| s.confidence >= 0.30) {
        let colour = band_colour(fp.period_s);
        for (i, &h) in fp.hist.iter().enumerate() {
            if h > 0.15 {
                let phase = (i as f32 + 0.5) / crate::diag::STUTTER_BINS as f32;
                current.push(HeldKnot { phase, height: h, colour, intensity: 1.0 });
            }
        }
    }
    if current.is_empty() {
        for off in offenders {
            current.push(HeldKnot {
                phase: off.phase,
                height: 0.75,
                colour: band_colour(off.period),
                intensity: 1.0,
            });
        }
    }
    let held: Vec<HeldKnot> = {
        let mut s = RIM_TRAIL.get().expect("RIM_TRAIL initialised above").lock().unwrap();
        let dt = (t - s.held_t).max(0.0);
        s.held_t = t;
        let decay = 0.5_f32.powf(dt / HALF_LIFE);
        for k in s.held.iter_mut() {
            k.intensity *= decay;
        }
        for c in &current {
            if let Some(k) = s.held.iter_mut().find(|k| {
                let d = (k.phase - c.phase).abs();
                d.min(1.0 - d) < MATCH
            }) {
                *k = *c;
            } else {
                s.held.push(*c);
            }
        }
        s.held.retain(|k| k.intensity > 0.1);
        if s.held.len() > 16 {
            s.held.sort_by(|a, b| b.intensity.partial_cmp(&a.intensity).unwrap_or(std::cmp::Ordering::Equal));
            s.held.truncate(16);
        }
        s.held.clone()
    };
    if held.is_empty() {
        return (Vec::new(), sev, orbit_pos);
    }

    // ── Fold the held set into a per-cell lit field over the whole perimeter ──
    let perim = Field::perimeter(area);
    let n = perim.cells().len();
    let mut cell: Vec<Option<(u8, Rgb)>> = vec![None; n];
    for (col, &(x, y)) in perim.cells().iter().enumerate() {
        let p = area.border_pos(x, y);
        let mut height = 0.0f32;
        let mut colour = (0.0, 0.0, 0.0);
        for k in &held {
            let d = { let d = (p - k.phase).abs(); d.min(1.0 - d) };
            let i = k.height * k.intensity * gaussian(d, KNOT_SIGMA);
            if i > height {
                height = i;
                colour = k.colour;
            }
        }
        if height < 0.20 {
            continue;
        }
        let mask = comet_braille_mask(height.min(1.0), col);
        if mask != 0 {
            cell[col] = Some((mask, colour));
        }
    }

    // ── Collect contiguous lit runs into segments (from a non-lit anchor) ─────
    let mut segs = Vec::new();
    if let Some(anchor) = (0..n).find(|&i| cell[i].is_none()) {
        let cells = perim.cells();
        let mut i = 0;
        while i < n {
            if cell[(anchor + i) % n].is_some() {
                let mut j = i;
                let mut run = Vec::new();
                while j < n && cell[(anchor + j) % n].is_some() {
                    let idx = (anchor + j) % n;
                    run.push((cells[idx].0, cells[idx].1, cell[idx].unwrap().0));
                    j += 1;
                }
                let colour = cell[(anchor + i) % n].unwrap().1;
                let start = cells[(anchor + i + n - 1) % n];
                let end = cells[(anchor + j) % n];
                segs.push(StutterSeg {
                    cells: run,
                    bookends: [(start, '┤'), (end, '├')],
                    colour,
                });
                i = j;
            } else {
                i += 1;
            }
        }
    }
    (segs, sev, orbit_pos)
}

/// Draw the four orbiters around the perimeter, skipping protected content gaps.
///
/// `render_rim` (mullion §3.11) does the clockwise perimeter walk, the non-glow
/// gap skip, and the `border_pos` lookup, and preserves each cell's glyph while
/// re-styling it — so all that is left here is the additive Gaussian colour mix.
fn draw_orbiters(buf: &mut Buffer, area: Rect, gaps: &[BorderGap], orbit_pos: [f32; 4]) {
    render_rim(buf, area, gaps, |p, cur| {
        let (mut r, mut g, mut b) = (0.0f32, 0.0f32, 0.0f32);
        for i in 0..4 {
            let d = { let d = (p - orbit_pos[i]).abs(); d.min(1.0 - d) };
            let inten = gaussian(d, ORBIT_SIGMA);
            let (cr, cg, cb) = ORBITERS[i].1;
            r += cr * inten;
            g += cg * inten;
            b += cb * inten;
        }
        let (r, g, b) = (r.min(255.0) as u8, g.min(255.0) as u8, b.min(255.0) as u8);
        (r > 12 || g > 12 || b > 12).then(|| cur.fg(Color::Rgb(r, g, b)))
    });
}

/// Draw the stutter segments (braille + `┤ ├` bookends), brightness floored and
/// pulsed by `sev`, and return their cells as gaps so the orbiters skip them.
///
/// `content` are the already-drawn header/footer gaps; a braille cell that lands
/// on one is skipped rather than clobbering the text — on the bottom edge the
/// content has flowed out of the way, on the top edge the legend wins its cells.
fn draw_stutter_segments(buf: &mut Buffer, segs: &[StutterSeg], sev: f32, content: &[BorderGap]) -> Vec<BorderGap> {
    let f = (0.6 + 0.4 * sev).clamp(0.0, 1.0);
    let blocked = |x: u16, y: u16| content.iter().any(|g| !g.rim_glow && g.contains(x, y));
    let mut gaps = Vec::new();
    for seg in segs {
        let (cr, cg, cb) = seg.colour;
        for &(x, y, mask) in &seg.cells {
            if blocked(x, y) {
                continue;
            }
            let glyph = char::from_u32(0x2800 + mask as u32).unwrap_or('⠿');
            let st = buf.get(x, y).style.fg(Color::Rgb((cr * f) as u8, (cg * f) as u8, (cb * f) as u8));
            buf.set_char(x, y, glyph, st);
            gaps.push(BorderGap::new(Rect::new(x, y, 1, 1)));
        }
        for &((x, y), glyph) in &seg.bookends {
            if blocked(x, y) {
                continue;
            }
            let st = buf.get(x, y).style.fg(Color::Rgb((cr * f) as u8, (cg * f) as u8, (cb * f) as u8));
            buf.set_char(x, y, glyph, st);
            gaps.push(BorderGap::new(Rect::new(x, y, 1, 1)));
        }
    }
    gaps
}

/// The x-ranges (inclusive) occupied by stutter segments on the edge at `edge_y`,
/// including their bookends — the obstacles the header/footer content flows around.
fn edge_reserved(segs: &[StutterSeg], edge_y: u16) -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    for seg in segs {
        let (mut lo, mut hi, mut any) = (u16::MAX, 0u16, false);
        for &(x, y, _) in &seg.cells {
            if y == edge_y { lo = lo.min(x); hi = hi.max(x); any = true; }
        }
        for &((x, y), _) in &seg.bookends {
            if y == edge_y { lo = lo.min(x); hi = hi.max(x); any = true; }
        }
        if any { out.push((lo, hi)); }
    }
    out
}

/// Advance `x` past any reserved interval covering it — content flows around the
/// stutter braille rather than overlapping it.
fn skip_reserved(mut x: u16, reserved: &[(u16, u16)]) -> u16 {
    while let Some(&(_, b)) = reserved.iter().find(|&&(a, b)| x >= a && x <= b) {
        x = b + 1;
    }
    x
}

/// Compact single-line status text for the bottom border left gap.
fn border_status(state: &AppState) -> String {
    match &state.view {
        AppView::Threads { label } => {
            let n = state.thread_samples.len();
            let total: f64 = state.thread_samples.iter().map(|t| t.cpu_pct).sum();
            format!("{label} · {n} threads · {total:.1}% total")
        }
        AppView::Manual => "manual".to_string(),
        AppView::Connecting { label } => format!("connecting to {label}"),
        AppView::Scope => {
            if state.scope_detect {
                "stutter · detection".to_string()
            } else {
                "stutter · observation · system wakeup jitter".to_string()
            }
        }
        _ => {
            let parts = groups_status_parts(state);
            parts.join(" · ")
        }
    }
}

/// Key hints text for the bottom border right gap.
fn border_keys(state: &AppState) -> String {
    if let Some(cursor) = state.history_cursor {
        let age   = state.history.get(cursor).map(|h| h.at.elapsed().as_secs()).unwrap_or(0);
        let total = state.history.len();
        return format!("PAUSED ◀ {age}s ago ▶ {}/{} [←/→] scrub [p] resume [q] quit",
            cursor + 1, total);
    }
    match &state.view {
        AppView::Groups => {
            let enter = if matches!(state.mode, AppMode::Local) {
                " [↵] threads"
            } else if matches!(state.mode, AppMode::Fleet { .. } | AppMode::Kube { .. })
                   || state.enable_remote {
                " [↵] drill"
            } else { "" };
            let smooth = if state.smooth_display { " [v] raw" } else { " [v] smooth" };
            format!("[←/→] metric [Tab] side [s] sort [h] hist [g] group{enter} [p] pause{smooth} [m] manual [q] quit")
        }
        AppView::Remote { .. } =>
            "[Esc] disconnect [p] pause [r] refresh [q] quit".to_string(),
        AppView::Threads { .. } =>
            "[Esc] close [r] refresh [q] quit".to_string(),
        AppView::Manual =>
            "[↑/↓] scroll [m] close [q] quit".to_string(),
        AppView::Connecting { .. } =>
            "[Esc] cancel [q] quit".to_string(),
        AppView::Scope => {
            let other = if state.scope_detect { "observe" } else { "detect" };
            format!("[Tab] {other} [d]/[Esc] close [q] quit")
        }
        AppView::Fleet =>
            "[←/→ Tab] region  [↑/↓] select  [f] close  [q] quit".to_string(),
    }
}

/// Body split for thread view: left 2/3 shows the group list, right 1/3 the thread detail.
/// The vertical divider connects to the outer border via `┬`/`┴` connectors.
fn render_body_with_threads(
    buf: &mut Buffer, body: Rect, outer_y0: u16, outer_y1: u16, state: &mut AppState,
) {
    let dim = Style::default().fg(Color::DarkGray);
    let left_w = body.width * 2 / 3;
    let right_w = body.width.saturating_sub(left_w + 1);

    // Fall back to body-only if the terminal is too narrow for a useful split.
    if right_w < 24 || left_w < 30 {
        render_body(buf, body, state);
        return;
    }

    let split_x = body.x + left_w;
    let left  = Rect::new(body.x,       body.y, left_w,  body.height);
    let right = Rect::new(split_x + 1,  body.y, right_w, body.height);

    for y in body.y..body.bottom() { buf.set_string(split_x, y, "│", dim); }
    buf.set_string(split_x, outer_y0, "┬", dim);
    buf.set_string(split_x, outer_y1, "┴", dim);

    render_body(buf, left, state);
    render_threads(buf, right, state);
}


/// Planck-curve blackbody colour ramp: black (0) → deep red → orange → yellow → white (1).
///
/// Used for both the histogram overlay and the thread heat-map. The colour stops
/// are chosen to approximate the visual appearance of blackbody radiation across
/// the temperature range ~800 K (dark red) to ~10 000 K (white), compressed into
/// the 0–1 interval with a slightly lifted black floor so "zero" is visible
/// against a black terminal background.
///
/// `lerp_u8` linearly interpolates between two byte values at parameter `t`.
fn lerp_u8(a: u8, b: u8, t: f64) -> u8 {
    (a as f64 + t.clamp(0.0, 1.0) * (b as f64 - a as f64)).round() as u8
}

/// Map `frac` ∈ [0, 1] to a blackbody RGB colour by piecewise linear interpolation
/// over the `STOPS` table.
///
/// Each stop is `(threshold, R, G, B)`. We binary-search for the bounding interval,
/// compute `s = (frac - t0) / (t1 - t0)` (local parameter within the segment),
/// then lerp R, G, B independently. The result is `Color::Rgb(r, g, b)`.
fn planck_color(frac: f64) -> Color {
    let t = frac.clamp(0.0, 1.0);
    const STOPS: &[(f64, u8, u8, u8)] = &[
        (0.00,  45,  45,  45), // lifted dark floor (not pure black, to stay visible)
        (0.05,  80,   0,   0), // deep red — very low activity
        (0.15, 180,  30,   0), // orange-red
        (0.30, 220, 100,   0), // orange
        (0.50, 240, 180,   0), // yellow-orange — moderate activity
        (0.70, 255, 240,  80), // yellow — high activity
        (0.85, 255, 255, 180), // warm white
        (1.00, 255, 255, 255), // pure white — maximum activity
    ];
    // partition_point finds the first stop whose threshold is >= t; we want the one before it.
    let i = STOPS.partition_point(|(th, ..)| *th < t).saturating_sub(1);
    let i = i.min(STOPS.len() - 2); // guard against the final stop
    let (t0, r0, g0, b0) = STOPS[i];
    let (t1, r1, g1, b1) = STOPS[i + 1];
    // s = local interpolation parameter within this segment.
    let s = if t1 > t0 { ((t - t0) / (t1 - t0)).clamp(0.0, 1.0) } else { 0.0 };
    let l = |a: u8, b: u8| (a as f64 + s * (b as f64 - a as f64)).round() as u8;
    Color::Rgb(l(r0, r1), l(g0, g1), l(b0, b1))
}

/// Fraction of the log-scale bin range where the fair-share pivot (r = 1) sits.
/// 1/3 puts it left of centre, matching the "balanced pool" visual pattern.
/// Changing this constant shifts the pivot without touching the algorithm.
const HIST_PIVOT_FRAC: f64 = 1.0 / 3.0;

/// Compute the distribution-heat histogram for one metric line.
///
/// # Encoding
/// x-axis = each member's load as a **fair-share multiple** `rᵢ = mᵢ·N/F`,
/// log₂-scaled across W bins.  Bin 0 is the idle slot (mᵢ = 0); bins 1..W-1
/// cover [L_min, log₂N] where L_min = −(p/(1−p))·log₂N and p = HIST_PIVOT_FRAC,
/// so r = 1 (exact fair share) falls at bin W·p ≈ W/3, left of centre.
///
/// y-axis = **work density** per bin: wᵦ = (Σᵢ∈b mᵢ)/F.  This encodes "where
/// is the work going?" not "where are the threads?": a single hog carrying
/// 100% of the load always appears white-hot at the far-right edge, regardless
/// of N.  A gamma lift (√wᵦ) keeps small-but-non-zero shares visible.
///
/// Reading rules:
///   balanced (all N at fair share)  → one bright cell at the 1/3 pivot
///   one hog (1 of N does everything) → bright cell at far-right edge
///   k of N busy equally             → bright cell near log₂(N/k) position
///   skewed tail                     → heat spread right of the pivot
fn fair_share_bins(members: &[f64], w: usize) -> Vec<f64> {
    let mut bins = vec![0.0f64; w];
    if w < 2 {
        return bins;
    }
    let n = members.len();
    if n < 2 {
        return bins; // single thread: distribution is meaningless
    }
    let f: f64 = members.iter().sum();
    if f < 1e-9 {
        return bins; // all idle → all black
    }

    // log₂N sets the scale; max(1e-9) guards the N=1 degenerate case where
    // l_min == l_max == 0 — that member is sent straight to the last bin.
    let log2n = (n as f64).log2().max(1e-9);
    // scale = p / (1-p) determines how far left of the pivot l_min extends.
    // With p = 1/3: scale = 0.5, so the axis covers 1.5 × log₂N units.
    let scale = HIST_PIVOT_FRAC / (1.0 - HIST_PIVOT_FRAC);
    let l_min = -scale * log2n; // log₂(r) for the leftmost active bin
    let l_max = log2n;           // log₂(N) = log₂ of max fair-share multiple
    let range = l_max - l_min;  // total span = (1 + scale) × log₂N

    for &m in members {
        if m < 1e-9 {
            continue; // idle: zero work share, skip
        }
        let r = m * n as f64 / f; // fair-share multiple: 1.0 = exactly one share
        // Map log₂(r) into [0,1] on the axis, then into bin indices 1..w-1.
        let t = ((r.log2() - l_min) / range).clamp(0.0, 1.0);
        let cell = (1.0 + t * (w - 2) as f64).round() as usize;
        // Accumulate the work share (m/f) in this bin, not a thread count.
        bins[cell.clamp(1, w - 1)] += m / f;
    }

    // √ gamma lift: small-but-nonzero shares stay visible; peak = 1 → white.
    // Without the sqrt, bins with < 10% work share would be nearly invisible.
    bins.iter().map(|&b| b.sqrt()).collect()
}

/// Map a CPU percentage to a traffic-light colour (green / yellow / red).
fn cpu_color(pct: f64) -> Color {
    if pct >= 80.0 { Color::Red } else if pct >= 50.0 { Color::Yellow } else { Color::Green }
}

/// Map a (metric, bar_fill_fraction) pair to the colour used for that metric's bar.
///
/// CPU and memory use traffic-light colouring (green→red).
/// Other metrics use distinctive hues so left/right bars are visually distinct:
/// disk-r is teal-to-cyan, disk-w is amber-to-orange, ctx-sw is purple, etc.
/// The `frac` parameter (0.0–1.0) controls how saturated/bright the colour is.
fn bar_color(m: Metric, frac: f64) -> Color {
    let f = frac.clamp(0.0, 1.0);
    match m {
        Metric::Cpu => cpu_color(f * 100.0),
        Metric::Memory => if f * 100.0 >= 90.0 { Color::Red } else { Color::Blue },
        Metric::PageFaults => {
            let p = f * 100.0;
            if p >= 80.0 { Color::Red } else if p >= 40.0 { Color::Yellow } else { Color::Magenta }
        }
        Metric::Threads => Color::Cyan,
        // Disk-read: dark teal → bright cyan as load increases.
        Metric::DiskRead => Color::Rgb(
            lerp_u8(0, 0, f),
            lerp_u8(100, 255, f),
            lerp_u8(100, 220, f),
        ),
        // Disk-write: dark amber → bright orange.
        Metric::DiskWrite => Color::Rgb(
            lerp_u8(80, 255, f),
            lerp_u8(50, 160, f),
            lerp_u8(0, 0, f),
        ),
        // Context switches: dark purple → bright magenta.
        Metric::CtxSwitches => Color::Rgb(
            lerp_u8(80, 220, f),
            lerp_u8(0, 0, f),
            lerp_u8(80, 220, f),
        ),
        // Open FDs: dark navy → bright blue.
        Metric::OpenFds => Color::Rgb(
            lerp_u8(0, 60, f),
            lerp_u8(0, 120, f),
            lerp_u8(80, 255, f),
        ),
        // Swap: dark maroon → bright crimson.
        Metric::SwapMem => Color::Rgb(
            lerp_u8(60, 255, f),
            lerp_u8(0, 40, f),
            lerp_u8(0, 40, f),
        ),
        // Scheduler wait: dark brown → bright orange-red.
        Metric::SchedWait => Color::Rgb(
            lerp_u8(60, 255, f),
            lerp_u8(30, 140, f),
            lerp_u8(0, 0, f),
        ),
        // Power: dark maroon → bright orange (distinct from SchedWait by the green channel).
        Metric::Power => Color::Rgb(
            lerp_u8(60, 255, f),
            lerp_u8(30, 100, f),
            lerp_u8(0, 0, f),
        ),
        // CFS throttle: dark olive → bright yellow.
        Metric::CfsThrottle => Color::Rgb(
            lerp_u8(60, 230, f),
            lerp_u8(60, 230, f),
            lerp_u8(0, 0, f),
        ),
        // PSI metrics: shades of teal (cpu), violet (mem), salmon (io).
        Metric::PsiCpu => Color::Rgb(
            lerp_u8(0, 0, f),
            lerp_u8(80, 210, f),
            lerp_u8(80, 180, f),
        ),
        Metric::PsiMem => Color::Rgb(
            lerp_u8(80, 180, f),
            lerp_u8(0, 50, f),
            lerp_u8(80, 210, f),
        ),
        Metric::PsiIo => Color::Rgb(
            lerp_u8(180, 255, f),
            lerp_u8(80, 120, f),
            lerp_u8(60, 80, f),
        ),
        // GPU engine time: dark magenta → bright magenta/pink.
        Metric::GpuPct => Color::Rgb(
            lerp_u8(100, 255, f),
            lerp_u8(0, 0, f),
            lerp_u8(80, 200, f),
        ),
        // GPU VRAM: dark purple → deep purple.
        Metric::Vram => Color::Rgb(
            lerp_u8(60, 148, f),
            lerp_u8(0, 0, f),
            lerp_u8(80, 211, f),
        ),
    }
}

/// Truncate a label to exactly `width` display cells, appending `…` when cut.
///
/// All characters are assumed to occupy one cell (no CJK wide-char support).
/// If the label fits, it is left-padded to exactly `width` characters with spaces
/// so column widths are consistent across all rows.
/// Dim a colour toward dark gray when a metric is incomplete (EACCES).
///
/// Applied to bar fill and value text when `entry_complete` returns false.
/// The formula blends 60% of the original colour with 40% of a fixed dark grey
/// `(50,50,50)` so the colour remains identifiable but is visually subordinate.
/// Named colours are converted to their RGB approximations first.
fn dimmed(c: Color) -> Color {
    const DARK: (u8, u8, u8) = (50, 50, 50);
    match c {
        Color::Rgb(r, g, b) => Color::Rgb(
            // 60% original + 40% dark grey for each channel.
            (r as u16 * 60 / 100 + DARK.0 as u16 * 40 / 100) as u8,
            (g as u16 * 60 / 100 + DARK.1 as u16 * 40 / 100) as u8,
            (b as u16 * 60 / 100 + DARK.2 as u16 * 40 / 100) as u8,
        ),
        Color::Red     => Color::Rgb(100, 50, 50),
        Color::Yellow  => Color::Rgb(100, 90, 50),
        Color::Green   => Color::Rgb(50, 100, 50),
        Color::Blue    => Color::Rgb(50, 50, 100),
        Color::Cyan    => Color::Rgb(50, 100, 100),
        Color::Magenta => Color::Rgb(100, 50, 100),
        other          => other,
    }
}

/// Format a byte count as a human-readable string using binary prefixes (GiB/MiB/KiB).
///
/// Thresholds use power-of-2 boundaries (1 GiB = 2^30, etc.) consistent with how
/// the Linux kernel reports memory values.
fn human_bytes(b: u64) -> String {
    const G: u64 = 1 << 30;
    const M: u64 = 1 << 20;
    const K: u64 = 1 << 10;
    if b >= G {
        format!("{:.1}G", b as f64 / G as f64)
    } else if b >= M {
        format!("{:.0}M", b as f64 / M as f64)
    } else if b >= K {
        format!("{:.0}K", b as f64 / K as f64)
    } else {
        format!("{b}B")
    }
}

/// Format a dimensionless rate (events/s, faults/s, ctx-sw/s) as a short string.
///
/// Uses SI prefixes (k = 1 000, M = 1 000 000) because event rates are not
/// memory sizes; this differs from `fmt_bytes_rate` which uses binary prefixes.
fn human_rate(r: f64) -> String {
    if r >= 1_000_000.0 {
        format!("{:.1}M/s", r / 1_000_000.0)
    } else if r >= 1_000.0 {
        format!("{:.0}k/s", r / 1_000.0)
    } else {
        format!("{:.0}/s", r)
    }
}

/// Format a byte rate as a human-readable string: "1.2G/s", "45M/s", "890k/s", "12B/s".
///
/// Uses binary prefixes (1 MiB/s = 1 048 576 B/s) because disk/network rates
/// are typically expressed in powers of 2 on Linux.
fn fmt_bytes_rate(r: f64) -> String {
    const G: f64 = 1_073_741_824.0;
    const M: f64 = 1_048_576.0;
    const K: f64 = 1_024.0;
    if r >= G {
        format!("{:.1}G/s", r / G)
    } else if r >= M {
        format!("{:.1}M/s", r / M)
    } else if r >= K {
        format!("{:.0}k/s", r / K)
    } else {
        format!("{:.0}B/s", r)
    }
}

/// Format a wattage: "12.3W" for ≥ 1 W, "850mW" for < 1 W.
fn fmt_watts(w: f64) -> String {
    if w >= 1.0 {
        format!("{:.1}W", w)
    } else {
        format!("{:.0}mW", w * 1000.0)
    }
}

/// Returns true when the metric's data is fully readable for this entry.
///
/// False means at least one PID in this group returned EACCES for the file(s)
/// that back this metric. The caller dims the bar and appends `?` to the value.
fn entry_complete(e: &BarEntry, m: Metric) -> bool {
    match m {
        Metric::DiskRead | Metric::DiskWrite => e.disk_complete,
        Metric::CtxSwitches | Metric::SwapMem => e.status_complete,
        Metric::OpenFds => e.fds_complete,
        Metric::SchedWait => e.sched_complete,
        Metric::Memory => e.rss_complete,
        Metric::CfsThrottle => e.cg_v2_complete,
        // PSI completeness is per-field: the pressure controller may be absent even
        // when other cgroup v2 files are readable, so each field carries its own Option.
        Metric::PsiCpu => e.psi_cpu_avg10.is_some(),
        Metric::PsiMem => e.psi_mem_avg10.is_some(),
        Metric::PsiIo  => e.psi_io_avg10.is_some(),
        Metric::GpuPct | Metric::Vram => true,
        // GPU metrics show 0.0 when --enable-gpu is off or no GPU fds; never show '?'
        // because 0 is accurate (the process genuinely has no GPU allocation/time).
        _ => true,
    }
}

/// Formatted value string for a metric (shown beside the bar in the value column).
///
/// If the metric is incomplete (EACCES for at least one PID), appends `?` to
/// signal that the displayed value is a lower bound.
///
/// Memory display: in local mode uses `rss_bytes` (absolute bytes with SI suffix);
/// in Proxmox mode uses `mem_pct` (percentage of VM's maxmem allocation).
fn metric_display_str(e: &BarEntry, m: Metric, total_ram: u64) -> String {
    let incomplete = !e.fading && !entry_complete(e, m);
    let base = match m {
        Metric::Cpu => format!("{:.1}%", e.value),
        Metric::Memory => {
            if total_ram > 0 {
                // Local mode: show RSS as a human-readable byte count.
                human_bytes(e.rss_bytes)
            } else {
                // Proxmox mode: show the API-reported mem/maxmem percentage.
                format!("{:.0}%", e.mem_pct)
            }
        }
        Metric::PageFaults => human_rate(e.page_faults_s),
        Metric::Threads => format!("{}", e.count.unwrap_or(0)),
        Metric::DiskRead => fmt_bytes_rate(e.disk_read_s),
        Metric::DiskWrite => fmt_bytes_rate(e.disk_write_s),
        Metric::CtxSwitches => human_rate(e.ctx_switches_s),
        Metric::OpenFds => format!("{}", e.open_fds),
        Metric::SwapMem => human_bytes(e.swap_bytes),
        Metric::SchedWait => format!("{:.1}%", e.sched_wait_pct),
        // Power is an estimate derived from RAPL; the ≈ prefix makes this explicit.
        Metric::Power => format!("≈{}", fmt_watts(e.power_w)),
        Metric::CfsThrottle => format!("{:.1}%", e.cfs_throttle_pct),
        Metric::PsiCpu => e.psi_cpu_avg10.map_or("-.-%".into(), |v| format!("{v:.1}%")),
        Metric::PsiMem => e.psi_mem_avg10.map_or("-.-%".into(), |v| format!("{v:.1}%")),
        Metric::PsiIo  => e.psi_io_avg10 .map_or("-.-%".into(), |v| format!("{v:.1}%")),
        Metric::GpuPct => format!("{:.1}%", e.gpu_pct),
        Metric::Vram   => human_bytes(e.gpu_vram_bytes),
    };
    if incomplete { format!("{base}?") } else { base }
}

/// Bar fill fraction (0.0–1.0) for a metric using log₂ scale anchored to rolling peak.
///
/// Formula: log₂(value + 1) / log₂(reference + 1)
///
/// Adding 1 before the log makes value=0 map to fraction=0 (avoids log(0) = -∞).
/// The reference is the rolling peak for that metric across all visible groups.
/// When value == reference, the result is 1.0 (full bar). The log scale compresses
/// the wide dynamic range of metrics like disk I/O and page faults so small values
/// remain visible next to large ones.
fn log2_frac(value: f64, reference: f64) -> f64 {
    (value.max(0.0) + 1.0).log2() / (reference.max(1.0) + 1.0).log2()
}

/// Compute the bar fill fraction (0.0–1.0) for a given metric on a given entry.
///
/// CPU and memory use linear scales (0–100% of the whole machine; 0–100% of RAM).
/// All other metrics use `log2_frac` relative to the current rolling peak.
/// The result is clamped to [0, 1] so the bar never overflows its half.
fn metric_frac(e: &BarEntry, m: Metric, total_ram: u64, peaks: &PeakVals) -> f64 {
    match m {
        // CPU: linear 0–100% of whole-machine capacity; value is already machine-normalised.
        Metric::Cpu => e.value / 100.0,
        Metric::Memory => {
            if total_ram > 0 {
                // Local mode: fraction of total physical RAM.
                (e.rss_bytes as f64 / total_ram as f64).clamp(0.0, 1.0)
            } else {
                // Proxmox mode: PVE reports mem/maxmem as a percent [0,100].
                e.mem_pct / 100.0
            }
        }
        // Log-scale metrics anchored to their rolling peak.
        Metric::PageFaults  => log2_frac(e.page_faults_s,            peaks.page_faults_s),
        Metric::Threads     => log2_frac(e.count.unwrap_or(0) as f64, peaks.threads),
        Metric::DiskRead    => log2_frac(e.disk_read_s,               peaks.disk_read_s),
        Metric::DiskWrite   => log2_frac(e.disk_write_s,              peaks.disk_write_s),
        Metric::CtxSwitches => log2_frac(e.ctx_switches_s,            peaks.ctx_switches_s),
        Metric::OpenFds     => log2_frac(e.open_fds as f64,           peaks.open_fds),
        Metric::SwapMem     => log2_frac(e.swap_bytes as f64,         peaks.swap_bytes),
        // SchedWait: linear 0–100% (but can exceed 100 for multi-threaded groups).
        Metric::SchedWait   => e.sched_wait_pct / 100.0,
        Metric::Power       => log2_frac(e.power_w,                   peaks.power_w),
        // CFS throttle and PSI: linear 0–100%.
        Metric::CfsThrottle => e.cfs_throttle_pct / 100.0,
        // PSI: linear 0–100%. unwrap_or(0.0) is safe because entry_complete()
        // returns false for None, causing the bar to be rendered dimmed anyway.
        Metric::PsiCpu      => e.psi_cpu_avg10.unwrap_or(0.0) / 100.0,
        Metric::PsiMem      => e.psi_mem_avg10.unwrap_or(0.0) / 100.0,
        Metric::PsiIo       => e.psi_io_avg10 .unwrap_or(0.0) / 100.0,
        // GpuPct can theoretically exceed 100% on multi-engine GPUs; cap bar at 1.0.
        Metric::GpuPct      => (e.gpu_pct / 100.0).clamp(0.0, 1.0),
        Metric::Vram        => if peaks.gpu_vram_bytes > 0.0 { e.gpu_vram_bytes as f64 / peaks.gpu_vram_bytes } else { 0.0 },
    }
    .clamp(0.0, 1.0)
}

const SPINE_ID:   TileId = 10;
const PRIMARY_ID: TileId = 11;
const DETAIL_ID:  TileId = 12;

/// The additive three-region "fleet face": spine │ primary │ detail.
/// Layout + shared-border focus via `mullion::border::render_shared` (census
/// `dit.rs` idiom); spine via `mullion::outline::render_tree_row`; primary
/// reuses the existing `render_body` into its sub-rect. The detail sub-rect
/// draws the selected group's live per-thread heatmap via `render_thread_heat`,
/// fed by `fleet_detail_samples` (refreshed each tick for the primary
/// selection).
fn render_fleet(buf: &mut Buffer, area: Rect, state: &mut AppState) {
    if area.width < 20 || area.height < 3 { render_body(buf, area, state); return; }

    // Which region's border to thicken.
    let focused = match state.fleet_region {
        Region::Spine => SPINE_ID,
        Region::Primary => PRIMARY_ID,
        Region::Detail => DETAIL_ID,
    };

    let mut tree = Node::Split {
        orientation: Orientation::Horizontal,
        children: vec![
            (Constraint::new(Size::Percent(20)).with_min(16), Node::Tile(SPINE_ID)),
            (Constraint::new(Size::Fill(1)),                   Node::Tile(PRIMARY_ID)),
            (Constraint::new(Size::Percent(38)).with_min(24),  Node::Tile(DETAIL_ID)),
        ],
    };
    let style = BorderStyle { weight: LineWeight::Light, corners: CornerStyle::Rounded, style: Style::default().fg(Color::DarkGray) };
    let rects = render_shared(buf, &mut tree, area, &style, &[(focused, LineWeight::Heavy)]);
    let rect_of = |id: TileId| rects.iter().find(|(t, _)| *t == id).map(|(_, r)| *r);

    // Spine: flatten places, paint one row each.
    if let Some(spine) = rect_of(SPINE_ID) {
        let theme = Theme::default();
        let places = fleet::local_places();
        for (i, p) in places.iter().enumerate() {
            let row = Rect::new(spine.x, spine.y + i as u16, spine.width, 1);
            if row.y >= spine.y + spine.height { break; }
            render_tree_row(buf, row, &p.ancestor_last, p.is_last, p.expanded,
                &p.label, i == state.spine_cursor, &theme, TextCtx::default());
        }
    }

    // Primary: the existing group table into its rect.
    if let Some(primary) = rect_of(PRIMARY_ID) { render_body(buf, primary, state); }

    // Detail: the selected group's live per-thread heatmap (monitor lens).
    if let Some(detail) = rect_of(DETAIL_ID) {
        if detail.height >= 2 {
            let header = match &state.fleet_detail_label {
                Some(l) => {
                    let n = state.fleet_detail_samples.len();
                    let unit = if n == 1 { "thread" } else { "threads" };
                    format!(" {l} · {n} {unit}")
                }
                None => " (no group selected)".to_string(),
            };
            buf.set_string(detail.x, detail.y, &header.chars().take(detail.width as usize).collect::<String>(),
                Style::default().fg(Color::Gray));
            let heat = Rect::new(detail.x, detail.y + 1, detail.width, detail.height - 1);
            render_thread_heat(buf, heat, &state.fleet_detail_samples);
        }
    }
}

/// Render the main group list (Groups and Remote views).
///
/// Layout per row:
///   `label ▶▶▶ <left_value>  [████left░░░gap░░░right████]  <right_value> ◀◀◀`
///
/// Key design decisions:
/// - A 2-cell gap at the bar centre is always preserved so the two bars never
///   visually merge, even when both metrics are near 100%.
/// - Each bar occupies at most `(bar_w / 2) - 1` cells so the gap is guaranteed.
/// - When the histogram overlay is on (`show_histogram`), bar cells use `◻`
///   with a planck-heat foreground colour and the bar-fill colour as background.
///   When off, classic solid `█` / dim `░` characters are used.
/// - Fading rows (group gone from /proc, within retention window) have zeroed
///   rate metrics and a label that smoothly transitions from cyan to dark grey.
fn render_body(buf: &mut Buffer, area: Rect, state: &mut AppState) {
    if area.height == 0 { return; }

    if state.entries.is_empty() {
        let msg = if state.snap_count < 2 {
            "Collecting first sample — waiting for next refresh tick…"
        } else {
            "Nothing active to display."
        };
        buf.set_string(area.x, area.y, msg, Style::default().fg(Color::DarkGray));
        return;
    }

    let lm = state.left_metric;
    let rm = state.right_metric;
    let hist_metric = match state.active_side { Side::Left => lm, Side::Right => rm };
    let overlay_enabled = state.show_histogram
        && matches!(hist_metric,
            Metric::Cpu | Metric::Memory | Metric::PageFaults | Metric::DiskRead
                | Metric::DiskWrite | Metric::CtxSwitches | Metric::SchedWait
                | Metric::CfsThrottle | Metric::PsiCpu | Metric::PsiMem | Metric::PsiIo);

    let label_w = state.entries.iter().map(|e| e.label.len()).max().unwrap_or(8).clamp(8, 28) as u16;
    const VAL_W: u16 = 9;
    const ARROW_W: u16 = 3;

    // Declare the 7-column layout: label | sep | left_arrow | left_val | bar | right_val | right_arrow
    let grid = ColumnGrid::new(vec![
        ColumnDef::fixed(label_w, ColumnKind::Text),
        ColumnDef::fixed(1,       ColumnKind::Custom),
        ColumnDef::fixed(ARROW_W, ColumnKind::Custom),
        ColumnDef::fixed(VAL_W,   ColumnKind::Text).with_align(Align::End),
        ColumnDef::fill(1,        ColumnKind::Bar).with_min(16),
        ColumnDef::fixed(VAL_W,   ColumnKind::Text),
        ColumnDef::fixed(ARROW_W, ColumnKind::Custom),
    ]);
    // bar_w is needed in the row closure; resolve once before moving grid into Table.
    let bar_w = grid.resolve(Rect::new(area.x, 0, area.width, 1))[4].width as usize;
    let table = Table::new(grid);

    // ── header values (captured by the header closure below) ──────────────────
    let lname = lm.name();
    let rname = rm.name();
    let left_hdr_style = if state.active_side == Side::Left {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let right_hdr_style = if state.active_side == Side::Right {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let group_by_label = if matches!(state.mode, AppMode::Local) {
        format!("[{}]", state.group_by.name())
    } else if matches!(state.mode, AppMode::Proxmox { .. }) {
        format!("[{}]", state.pve_group_by.name())
    } else {
        String::new()
    };

    // ── row values (captured by the row closure below) ────────────────────────
    let focused_id    = state.body_tree.as_ref().and_then(|t| t.focus());
    let mut tree      = match state.body_tree.take() { Some(t) => t, None => return };
    tree.scroll_focus_into_view(table.body_area(area, true, false));

    let entries_by_id: HashMap<TileId, usize> = state.entries.iter()
        .enumerate()
        .map(|(i, e)| (id_from_key(&e.label), i))
        .collect();

    let total_ram_bytes   = state.total_ram_bytes;
    let show_histogram    = state.show_histogram;
    let smooth_display    = state.smooth_display;
    let entries           = &state.entries;
    let peaks             = &state.peak_vals;
    let anomaly_states    = &state.anomaly_states;
    let group_member_vals = &state.group_member_vals;
    let ewma_vals         = &state.ewma_vals;

    table.render(buf, area, tree.root_mut(),
        Some(|buf: &mut Buffer, cols: &[Rect]| {
            let hy = cols[0].y;
            ColumnGrid::write_text(buf, cols[0], hy,
                &group_by_label,
                Align::Start, Style::default().fg(Color::Rgb(60, 60, 60)));
            buf.set_string(cols[4].x, hy, lname, left_hdr_style);
            let rname_x = cols[4].x + cols[4].width.saturating_sub(rname.len() as u16);
            buf.set_string(rname_x, hy, rname, right_hdr_style);
        }),
        None::<fn(&mut Buffer, &[Rect])>,
        |buf: &mut Buffer, id: TileId, cols: &[Rect]| {
            let entry_idx = match entries_by_id.get(&id) { Some(&i) => i, None => return };
            let raw_e = &entries[entry_idx];
            let ey = cols[0].y;
            let fading      = raw_e.fading;
            let is_selected = Some(id) == focused_id;

            // When smooth_display is on, overlay EWMA values onto a cloned entry so
            // metric_frac / metric_display_str see smoothed rates without any other
            // changes to the display logic.
            let smoothed: Option<crate::BarEntry> = if smooth_display && !fading {
                ewma_vals.get(&raw_e.label).map(|ew| {
                    let mut e2 = raw_e.clone();
                    e2.value          = ew.cpu;
                    e2.disk_read_s    = ew.disk_read_s;
                    e2.disk_write_s   = ew.disk_write_s;
                    e2.ctx_switches_s = ew.ctx_switches_s;
                    e2.page_faults_s  = ew.page_faults_s;
                    e2.sched_wait_pct = ew.sched_wait_pct;
                    e2.gpu_pct        = ew.gpu_pct;
                    e2
                })
            } else { None };
            let e = smoothed.as_ref().unwrap_or(raw_e);

            let lf = if fading { 0.0 } else { metric_frac(e, lm, total_ram_bytes, peaks) };
            let rf = if fading { 0.0 } else { metric_frac(e, rm, total_ram_bytes, peaks) };
            let lc = if fading { Color::DarkGray } else {
                let c = bar_color(lm, lf);
                if !entry_complete(e, lm) { dimmed(c) } else { c }
            };
            let rc = if fading { Color::DarkGray } else {
                let c = bar_color(rm, rf);
                if !entry_complete(e, rm) { dimmed(c) } else { c }
            };

            let usable      = (bar_w / 2).saturating_sub(1);
            let l_filled    = (lf * usable as f64).round() as usize;
            let r_filled    = (rf * usable as f64).round() as usize;
            let right_start = bar_w - r_filled;

            let anomaly    = anomaly_states.get(&e.label);
            let is_anomaly = anomaly.is_some_and(|s: &AnomalyState| s.alerting);

            let label_style = if is_selected {
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else if e.fading {
                let ft = mullion::ease::smoothstep(e.fade_t as f32) as f64;
                Style::default().fg(Color::Rgb(
                    lerp_u8(0,   80, ft),
                    lerp_u8(200, 80, ft),
                    lerp_u8(200, 80, ft),
                )).add_modifier(Modifier::BOLD)
            } else if is_anomaly {
                Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            };

            let display_label = if is_anomaly && !e.fading && !is_selected {
                format!("! {}", e.label)
            } else {
                e.label.clone()
            };

            let l_str = metric_display_str(e, lm, total_ram_bytes);
            let r_str = metric_display_str(e, rm, total_ram_bytes);

            let hist: Option<Vec<f64>> = if show_histogram {
                if overlay_enabled && !fading {
                    Some(group_member_vals.get(&e.label).map(|v| {
                        if v.metric == hist_metric { fair_share_bins(&v.vals, bar_w) }
                        else { vec![0.0; bar_w] }
                    }).unwrap_or_else(|| vec![0.0; bar_w]))
                } else {
                    Some(vec![0.0; bar_w])
                }
            } else {
                None
            };

            let (l_arrow, r_arrow, a_style) = if is_selected {
                ("▶▶▶", "◀◀◀", Style::default().fg(Color::Cyan))
            } else {
                ("   ", "   ", Style::default())
            };

            // Fill the label cell first so a selection/anomaly highlight spans the
            // whole column, then overlay the label — write_text elides it
            // grapheme/width-correctly (mullion §3.13), so no manual truncation.
            buf.set_string(cols[0].x, ey, &" ".repeat(cols[0].width as usize), label_style);
            ColumnGrid::write_text(buf, cols[0], ey, &display_label, Align::Start, label_style);
            buf.set_string(cols[2].x, ey, l_arrow, a_style);
            ColumnGrid::write_text(buf, cols[3], ey, &l_str, Align::End, Style::default().fg(lc));
            for bi in 0..bar_w {
                let bx = cols[4].x + bi as u16;
                let in_left  = bi < l_filled;
                let in_right = bi >= right_start;
                if let Some(ref h) = hist {
                    let bg = if in_left { lc } else if in_right { rc } else { Color::Reset };
                    buf.set_string(bx, ey, "◻", Style::default().fg(planck_color(h[bi])).bg(bg));
                } else if in_left {
                    buf.set_string(bx, ey, "█", Style::default().fg(lc));
                } else if in_right {
                    buf.set_string(bx, ey, "█", Style::default().fg(rc));
                } else {
                    buf.set_string(bx, ey, "░", Style::default().fg(Color::DarkGray));
                }
            }
            ColumnGrid::write_text(buf, cols[5], ey, &r_str, Align::Start, Style::default().fg(rc));
            buf.set_string(cols[6].x, ey, r_arrow, a_style);
        },
    );

    state.body_tree = Some(tree);
    state.last_body_height = area.height as usize;
}

/// Thread heat-map view. Each ◻ cell encodes CPU heat relative to the
/// hottest thread. When there are too many threads to fit in 4 rows, threads
/// are grouped by the smallest power of 2 that fits; each cell shows the
/// max cpu% of its group.
///
/// Layout (top to bottom):
///   - 1 row: info line (label, thread count, total CPU%)
///   - 1..4 rows: heat-map grid
///   - 1 row: horizontal divider
///   - remaining rows: thread list with name, CPU%, pid:tid
fn render_threads(buf: &mut Buffer, area: Rect, state: &AppState) {
    let label = match &state.view {
        AppView::Threads { label } => label.clone(),
        _ => return,
    };
    if area.height == 0 { return; }

    let n = state.thread_samples.len();
    let total_cpu: f64 = state.thread_samples.iter().map(|t| t.cpu_pct).sum();
    let max_cpu = state.thread_samples.iter()
        .map(|t| t.cpu_pct).fold(0.0f64, f64::max).max(1e-6);
    let w = area.width as usize;
    let (group_size, heat_rows) = heat_layout(n, area.width);

    // Manual rect split: info / heat / divider / list
    let mut y = area.y;
    let info_y    = y; y += 1;
    let heat_y    = y; y += heat_rows as u16;
    let div_y     = y; y += 1;
    let list_y    = y;
    let list_h    = area.bottom().saturating_sub(list_y);

    let dim = Style::default().fg(Color::DarkGray);

    // Info line
    if info_y < area.bottom() {
        let group_info = if group_size > 1 { format!("  │  {} threads/cell", group_size) } else { String::new() };
        buf.set_string(area.x, info_y,
            &format!(" {label}  │  {n} threads  │  total {total_cpu:.2}%{group_info}"),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    }

    // Heat-map grid
    let heat_rect = Rect::new(area.x, heat_y, area.width, area.bottom().saturating_sub(heat_y));
    render_thread_heat(buf, heat_rect, &state.thread_samples);

    // Horizontal divider
    if div_y < area.bottom() {
        buf.set_string(area.x, div_y, &"─".repeat(w), dim);
    }

    // Thread list
    if list_y < area.bottom() && list_h > 0 {
        let name_w = (state.thread_samples.iter().map(|t| t.name.len())
            .max().unwrap_or(10).clamp(8, 24)) as u16;

        // Columns: [marker:2] [name:fill 8-24] [sep:2] [cpu%:6] [pid:tid:fill]
        let tgrid = ColumnGrid::new(vec![
            ColumnDef::fixed(2,       ColumnKind::Custom),
            ColumnDef::fill(1,        ColumnKind::Text).with_min(name_w).with_max(name_w),
            ColumnDef::fixed(2,       ColumnKind::Custom),
            ColumnDef::fixed(6,       ColumnKind::Number { unit_cols: 1 }),
            ColumnDef::fill(1,        ColumnKind::Text),
        ]);
        let tcols = tgrid.resolve(Rect::new(area.x, list_y, area.width, 1));

        // Header row
        buf.set_string(tcols[0].x, list_y, "  ", dim);
        ColumnGrid::write_text(buf, tcols[1], list_y, "thread", Align::Start, dim);
        ColumnGrid::write_text(buf, Rect::new(tcols[3].x, list_y, tcols[3].width, 1), list_y,
            "cpu%", Align::End, dim);
        ColumnGrid::write_text(buf, tcols[4], list_y, "pid:tid", Align::Start, dim);

        // Data rows
        for (row, t) in state.thread_samples.iter()
            .take(list_h.saturating_sub(1) as usize).enumerate()
        {
            let ty = list_y + 1 + row as u16;
            if ty >= area.bottom() { break; }
            let gray = Style::default().fg(Color::Gray);
            buf.set_string(tcols[0].x, ty, "◻ ",
                Style::default().fg(planck_color(t.cpu_pct / max_cpu)));
            ColumnGrid::write_text(buf, Rect::new(tcols[1].x, ty, tcols[1].width, 1),
                ty, &t.name, Align::Start, gray);
            ColumnGrid::write_number(buf, Rect::new(tcols[3].x, ty, tcols[3].width, 1), ty,
                &format!("{:>5.1}", t.cpu_pct), gray, "%", dim, 1);
            ColumnGrid::write_text(buf, Rect::new(tcols[4].x, ty, tcols[4].width, 1),
                ty, &format!("{}:{}", t.pid, t.tid), Align::Start, gray);
        }
    }
}

/// Grid layout for the per-thread heat cells: given the thread count `n` and the
/// available `width`, returns (group_size, heat_rows) — how many threads each cell
/// aggregates, and how many rows of cells the grid occupies. Shared by
/// `render_threads` (to size its layout split) and `render_thread_heat` (to draw),
/// so the two can never drift.
fn heat_layout(n: usize, width: u16) -> (usize, usize) {
    let w = width as usize;
    let cells_per_row = (w / 2).max(1);
    const MAX_HEAT_ROWS: usize = 4;
    let max_cells = MAX_HEAT_ROWS * cells_per_row;
    let mut group_size = 1usize;
    while n > 0 && n.div_ceil(group_size) > max_cells { group_size *= 2; }
    let num_cells = if n == 0 { 0 } else { n.div_ceil(group_size) };
    let heat_rows = if num_cells == 0 { 1 } else {
        num_cells.div_ceil(cells_per_row).clamp(1, MAX_HEAT_ROWS)
    };
    (group_size, heat_rows)
}

/// Draw the per-thread heat grid — cells of `group_size` threads each, coloured
/// by cpu% via [`planck_color`] — into `area`. Extracted from [`render_threads`]
/// so the identical heatmap can be reused by the Fleet detail region.
fn render_thread_heat(buf: &mut Buffer, area: Rect, samples: &[crate::local::ThreadSample]) {
    let n = samples.len();
    let max_cpu = samples.iter()
        .map(|t| t.cpu_pct).fold(0.0f64, f64::max).max(1e-6);
    let cells_per_row = ((area.width as usize) / 2).max(1);
    let (group_size, heat_rows) = heat_layout(n, area.width);
    let num_cells = if n == 0 { 0 } else { n.div_ceil(group_size) };
    let cell_cpus: Vec<f64> = (0..num_cells).map(|i| {
        let start = i * group_size;
        let end = (start + group_size).min(n);
        samples[start..end].iter().map(|t| t.cpu_pct).fold(0.0f64, f64::max)
    }).collect();

    let dim = Style::default().fg(Color::DarkGray);
    if samples.is_empty() {
        if area.y < area.bottom() {
            buf.set_string(area.x, area.y, "  waiting for second sample…", dim);
        }
    } else {
        let mut idx = 0;
        'outer: for row in 0..heat_rows {
            let ry = area.y + row as u16;
            if ry >= area.bottom() { break; }
            let mut x = area.x;
            for _ in 0..cells_per_row {
                if idx >= cell_cpus.len() { break 'outer; }
                x = buf.set_string(x, ry, "◻ ",
                    Style::default().fg(planck_color(cell_cpus[idx] / max_cpu)));
                idx += 1;
            }
        }
    }
}

/// Map a normalised level `[0, 1]` to a green → yellow → red heat colour.
/// Used to colour latency bars by height (low = calm green, high = alarming red).
fn heat_color(level: f32) -> Color {
    let l = level.clamp(0.0, 1.0);
    let lerp = |a: f32, b: f32, t: f32| (a + (b - a) * t) as u8;
    if l < 0.5 {
        let t = l * 2.0; // green → yellow
        Color::Rgb(lerp(40.0, 220.0, t), lerp(200.0, 200.0, t), lerp(60.0, 40.0, t))
    } else {
        let t = (l - 0.5) * 2.0; // yellow → red
        Color::Rgb(lerp(220.0, 235.0, t), lerp(200.0, 55.0, t), lerp(40.0, 40.0, t))
    }
}

/// Fill `area` with a bottom-anchored braille plot from a pre-normalised
/// `heights` array (`[0, 1]`, one entry per sub-pixel column, length = `2·width`).
/// Each column is coloured by its height via [`heat_color`]. Shared by the
/// latency trace and the spectrum plot.
fn fill_braille_plot(buf: &mut Buffer, area: Rect, heights: &[f32]) {
    if area.width == 0 || area.height == 0 || heights.is_empty() {
        return;
    }
    let nb = heights.len();
    let field = Field::rect(area);
    field.render_braille_xy(
        buf,
        |u, v| {
            let b = ((u * nb as f32) as usize).min(nb - 1);
            if (1.0 - v) <= heights[b] { 1.0 } else { 0.0 }
        },
        |_mean, u, _v| {
            let b = ((u * nb as f32) as usize).min(nb - 1);
            Style::default().fg(heat_color(heights[b]))
        },
    );
}

/// Draw a bottom-anchored, severity-coloured braille trace of a time-series into
/// `area`, normalised to `ceil`, with a top-left scale label and a "0" baseline.
/// Shared by the latency trace and the system-pressure trace.
fn draw_series_trace(
    buf: &mut Buffer,
    area: Rect,
    samples: &[crate::diag::Sample],
    ceil: f32,
    top_label: &str,
) {
    if area.height == 0 || area.width == 0 || samples.len() < 2 {
        return;
    }
    let faint = Style::default().fg(Color::Rgb(90, 90, 90));
    let nb = area.width as usize * 2;
    let t0 = samples[0].t;
    let span = (samples[samples.len() - 1].t - t0).max(1e-9);
    let c = ceil.max(1e-6);
    let mut heights = vec![0.0f32; nb];
    for s in samples {
        let frac = ((s.t - t0) / span).clamp(0.0, 1.0);
        let b = ((frac * nb as f64) as usize).min(nb - 1);
        let h = (s.overshoot_ms / c).clamp(0.0, 1.0);
        if h > heights[b] {
            heights[b] = h;
        }
    }
    fill_braille_plot(buf, area, &heights);
    buf.set_string(area.x, area.y, top_label, faint);
    buf.set_string(area.x, area.bottom().saturating_sub(1), "0 ┘", faint);
}

/// Render the **detection** view of stutter mode: a plain-language verdict on
/// whether the system has a recurring stutter, plus its fingerprint and likely
/// cause. The live traces are one Tab away ([`render_scope`]).
fn render_verdict(buf: &mut Buffer, area: Rect, state: &AppState) {
    let faint = Style::default().fg(Color::DarkGray);
    let gray = Style::default().fg(Color::Gray);
    let label = |buf: &mut Buffer, y: u16, k: &str, v: String| {
        let x = buf.set_string(area.x + 4, y, k, faint);
        buf.set_string(x, y, &v, gray);
    };

    let analysis = state.scope_analysis.as_ref();
    let shape = state.stutter_shape.as_ref();
    let conf = shape.map(|s| s.confidence).unwrap_or(0.0);
    let has_period = analysis.is_some_and(|a| a.period_s.is_some());

    // Verdict light + headline.
    let (light, headline, lcolor) = if state.scope.is_none() {
        ("·", "starting probe…", Color::DarkGray)
    } else if has_period && conf >= 0.45 {
        ("●", "RECURRING STUTTER DETECTED", Color::Rgb(235, 80, 40))
    } else if has_period {
        ("◐", "possible stutter — keep watching", Color::Rgb(220, 180, 60))
    } else if analysis.is_some() {
        ("○", "no recurring stutter detected", Color::Rgb(60, 200, 90))
    } else {
        ("·", "gathering data…", Color::DarkGray)
    };

    let mut y = area.y + 1;
    buf.set_string(area.x + 2, y, &format!("{light}  {headline}"),
        Style::default().fg(lcolor).add_modifier(Modifier::BOLD));
    y += 2;

    if let Some(sh) = shape.filter(|s| s.confidence >= 0.30) {
        label(buf, y, "every     ", format!("{:.1} s   ({:.2} Hz)", sh.period_s,
            if sh.period_s > 0.0 { 1.0 / sh.period_s } else { 0.0 }));
        y += 1;
        label(buf, y, "duration  ", format!("{:.0}–{:.0} ms  (mean {:.0})",
            sh.dur_mean_ms.min(sh.dur_max_ms), sh.dur_max_ms, sh.dur_mean_ms));
        y += 1;
        let onset = if sh.onset_jitter < 0.06 { "steady onset" } else { "jittery onset" };
        let length = if sh.dur_cv < 0.25 { "fixed length" } else { "varying length" };
        label(buf, y, "shape     ", format!("{onset} · {length}"));
        y += 2;

        // Confidence bar.
        let pct = (conf * 100.0).round() as u32;
        let cells = (conf * 16.0).round() as usize;
        let bar: String = (0..16).map(|i| if i < cells { '█' } else { '░' }).collect();
        let x = buf.set_string(area.x + 4, y, "confidence ", faint);
        let x = buf.set_string(x, y, &bar, Style::default().fg(heat_color(conf)));
        buf.set_string(x, y, &format!(" {pct}%"), gray);
        y += 2;
    }

    // Likely cause (the offender "who"), if any.
    if let Some(o) = state.offender_report.as_ref().and_then(|r| r.offenders.first()) {
        let what = match (o.kind, o.child.as_deref()) {
            (crate::diag::OffenderKind::Spawns, Some(c)) => format!("spawning {c}"),
            (crate::diag::OffenderKind::Spawns, None) => "spawning helpers".to_string(),
            (crate::diag::OffenderKind::CpuBurst, _) => "CPU bursts".to_string(),
        };
        let x = buf.set_string(area.x + 4, y, "likely    ", faint);
        let x = buf.set_string(x, y, &o.group,
            Style::default().fg(Color::Rgb(235, 120, 40)).add_modifier(Modifier::BOLD));
        buf.set_string(x, y, &format!(" — {what} every {:.1} s", o.period_s), gray);
    }

    buf.set_string(area.x + 2, area.bottom().saturating_sub(1),
        "[Tab] live traces   [d]/[Esc] close", faint);
}

/// Render the latency scope: a live braille trace of the wakeup-jitter probe.
///
/// The probe ([`crate::diag::LatencyProbe`]) measures how late each of its own
/// timed wakeups is; that overshoot series is the system scheduling jitter that
/// stutters every realtime UI on the machine. The trace fills from the bottom and
/// is coloured by height (green calm → red stall); the readout row gives the
/// honest magnitudes so a flat-but-tall-looking trace can be read as "all fine,
/// peak 0.2 ms" rather than misread off the normalised shape.
fn render_scope(buf: &mut Buffer, area: Rect, state: &AppState) {
    let dim = Style::default().fg(Color::DarkGray);
    let faint = Style::default().fg(Color::Rgb(90, 90, 90));

    let Some(probe) = state.scope.as_ref() else {
        buf.set_string(area.x, area.y, "  starting latency probe…", dim);
        return;
    };
    if area.height < 4 || area.width < 12 {
        buf.set_string(area.x, area.y, " (resize)", dim);
        return;
    }

    let samples = probe.snapshot();
    let st = crate::diag::stats(&samples, probe.tick_ms());

    // ── Header rows ───────────────────────────────────────────────────────
    let hz = if st.tick_ms > 0.0 { 1000.0 / st.tick_ms } else { 0.0 };
    let title = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let mut x = buf.set_string(area.x, area.y, " latency scope", title);
    x = buf.set_string(x, area.y,
        &format!("   tick {:.1} ms ({:.0} Hz)   window {:.0} s   n={}",
            st.tick_ms, hz, st.window_s, st.count),
        dim);
    let _ = x;

    // Readout row: honest magnitudes (max coloured by severity).
    let max_style = Style::default()
        .fg(if st.max_ms >= 10.0 { Color::Rgb(235, 60, 40) }
            else if st.max_ms >= 3.0 { Color::Rgb(220, 200, 40) }
            else { Color::Rgb(40, 200, 60) })
        .add_modifier(Modifier::BOLD);
    let mut x = buf.set_string(area.x, area.y + 1, " now ", faint);
    x = buf.set_string(x, area.y + 1, &format!("{:.2} ms", st.last_ms),
        Style::default().fg(Color::Gray));
    x = buf.set_string(x, area.y + 1, "   mean ", faint);
    x = buf.set_string(x, area.y + 1, &format!("{:.2} ms", st.mean_ms),
        Style::default().fg(Color::Gray));
    x = buf.set_string(x, area.y + 1, "   p99 ", faint);
    x = buf.set_string(x, area.y + 1, &format!("{:.2} ms", st.p99_ms),
        Style::default().fg(Color::Gray));
    x = buf.set_string(x, area.y + 1, "   max ", faint);
    x = buf.set_string(x, area.y + 1, &format!("{:.2} ms", st.max_ms), max_style);
    let _ = x;

    // ── Periodicity headline (row y+2) ────────────────────────────────────
    let headline_y = area.y + 2;
    match state.scope_analysis.as_ref() {
        Some(a) if a.period_s.is_some() => {
            let period = a.period_s.unwrap();
            let f = a.freq_hz.unwrap_or(0.0);
            let conf = (a.confidence * 100.0).round() as u32;
            let strong = a.confidence > 0.4;
            let verdict = if strong { "strong" } else { "weak / quasi-periodic" };
            let col = if strong { Color::Rgb(235, 120, 40) } else { Color::Rgb(190, 190, 70) };
            let mut x = buf.set_string(area.x, headline_y, " recurring stall ≈ every ", faint);
            x = buf.set_string(x, headline_y, &format!("{period:.2} s"),
                Style::default().fg(col).add_modifier(Modifier::BOLD));
            x = buf.set_string(x, headline_y,
                &format!("  ({f:.2} Hz)  ·  confidence {conf}% ({verdict})"), faint);
            // Stutter shape: separate the onset-timing axis from the duration axis
            // (the two vary independently — see docs/rim-latency.md §5).
            if let Some(sh) = state.stutter_shape.as_ref() {
                let onset = if sh.onset_jitter < 0.06 { "steady onset" } else { "jittery onset" };
                let length = if sh.dur_cv < 0.25 { "fixed length" } else { "varying length" };
                x = buf.set_string(x, headline_y,
                    &format!("  ·  {onset}, {length} (~{:.0}–{:.0} ms)",
                        sh.dur_mean_ms, sh.dur_max_ms), faint);
            }
            let _ = x;
        }
        Some(_) => {
            buf.set_string(area.x, headline_y,
                " no clear periodicity yet — keep the scope open while the stutter happens", dim);
        }
        None => {
            buf.set_string(area.x, headline_y, " analysing…", dim);
        }
    }

    // ── Attribution line (row y+3): what co-occurs with the stalls ────────
    let attrib_y = area.y + 3;
    match state.scope_report.as_ref() {
        Some(r) if !r.suspects.is_empty() => {
            let top = &r.suspects[0];
            let mut x = buf.set_string(area.x, attrib_y, " likely cause: ", faint);
            x = buf.set_string(x, attrib_y, top.name,
                Style::default().fg(Color::Rgb(235, 120, 40)).add_modifier(Modifier::BOLD));
            x = buf.set_string(x, attrib_y,
                &format!(" ({:.0}{} during stalls vs {:.0} calm)", top.spike_mean, top.unit, top.base_mean),
                Style::default().fg(Color::Gray));
            x = buf.set_string(x, attrib_y, &format!("  ·  {}", top.hint), faint);
            if r.suspects.len() > 1 {
                let also: Vec<&str> = r.suspects[1..].iter().take(2).map(|s| s.name).collect();
                buf.set_string(x, attrib_y, &format!("   also: {}", also.join(", ")), faint);
            }
        }
        Some(r) if r.spike_count < 3 => {
            buf.set_string(area.x, attrib_y,
                &format!(" attribution: only {} stall(s) seen — waiting for more", r.spike_count), dim);
        }
        Some(_) => {
            buf.set_string(area.x, attrib_y,
                " attribution: no system signal clearly tracks the stalls", dim);
        }
        None => {
            buf.set_string(area.x, attrib_y, " gathering attribution…", dim);
        }
    }

    // ── Periodic offenders (row y+4): groups acting on a clock ────────────
    let off_y = area.y + 4;
    match state.offender_report.as_ref() {
        Some(r) if !r.offenders.is_empty() => {
            let o = &r.offenders[0];
            let what = match (o.kind, o.child.as_deref()) {
                (crate::diag::OffenderKind::Spawns, Some(c)) => format!("spawning {c}"),
                (crate::diag::OffenderKind::Spawns, None) => "spawning helpers".to_string(),
                (crate::diag::OffenderKind::CpuBurst, _) => "CPU bursts".to_string(),
            };
            let mut x = buf.set_string(area.x, off_y, " periodic offender: ", faint);
            x = buf.set_string(x, off_y, &o.group,
                Style::default().fg(Color::Rgb(235, 120, 40)).add_modifier(Modifier::BOLD));
            x = buf.set_string(x, off_y,
                &format!(" every {:.1} s — {} (conf {:.0}%)", o.period_s, what, o.confidence * 100.0),
                Style::default().fg(Color::Gray));
            if r.offenders.len() > 1 {
                let also: Vec<&str> = r.offenders[1..].iter().take(2).map(|o| o.group.as_str()).collect();
                buf.set_string(x, off_y, &format!("   also: {}", also.join(", ")), faint);
            }
        }
        Some(_) => {
            buf.set_string(area.x, off_y,
                " periodic offenders: none — no process group is acting on a clock", dim);
        }
        None => { buf.set_string(area.x, off_y, " scanning for periodic offenders…", dim); }
    }

    if samples.len() < 2 {
        buf.set_string(area.x, area.y + 5, "  collecting latency samples…", dim);
        return;
    }

    // ── Layout: latency trace (top) + system-pressure trace (bottom) ──────
    // The two instruments answer different questions: the CPU-wakeup trace shows
    // scheduling jitter; the pressure trace shows run-queue / PSI stall, which
    // catches compositor- and memory-bound freezes the wakeup probe is blind to.
    let body_top = area.y + 5;
    let body_h = area.height.saturating_sub(5);
    if body_h == 0 { return; }

    let two = body_h >= 8 && state.pressure.is_some();
    let lat_h = if two { (body_h - 1) / 2 } else { body_h };

    let lat_ceil = st.max_ms.max(1e-3);
    draw_series_trace(buf, Rect::new(area.x, body_top, area.width, lat_h),
        &samples, lat_ceil, &format!("{lat_ceil:.1} ms ┐  wakeup latency"));

    if two {
        let head_y = body_top + lat_h;
        let pres_rect = Rect::new(area.x, head_y + 1, area.width, body_h - lat_h - 1);
        let ch = state.pressure_analysis.as_ref().map(|(c, _)| *c)
            .unwrap_or(crate::diag::PressureChannel::MemStall);

        // Pressure periodicity headline — the key contrast with the latency one.
        match state.pressure_analysis.as_ref() {
            Some((c, p)) if p.period_s.is_some() => {
                let strong = p.confidence > 0.4;
                let col = if strong { Color::Rgb(235, 120, 40) } else { Color::Rgb(190, 190, 70) };
                let mut x = buf.set_string(area.x, head_y,
                    &format!(" system pressure [{}]: recurring ≈ every ", c.label()), faint);
                x = buf.set_string(x, head_y, &format!("{:.2} s", p.period_s.unwrap()),
                    Style::default().fg(col).add_modifier(Modifier::BOLD));
                buf.set_string(x, head_y,
                    &format!("  ({:.2} Hz)  ·  confidence {:.0}% ({})",
                        p.freq_hz.unwrap_or(0.0), p.confidence * 100.0,
                        if strong { "strong" } else { "weak / quasi-periodic" }), faint);
            }
            Some((c, _)) => {
                buf.set_string(area.x, head_y, &format!(
                    " system pressure [{}]: no clear period (showing the most active channel)",
                    c.label()), dim);
            }
            None => { buf.set_string(area.x, head_y, " system pressure: gathering…", dim); }
        }

        let ps = state.pressure.as_ref().map(|p| p.snapshot()).unwrap_or_default();
        let series = crate::diag::pressure_channel_series(&ps, ch);
        let ceil = series.iter().map(|s| s.overshoot_ms).fold(0.0f32, f32::max).max(1e-6);
        draw_series_trace(buf, pres_rect, &series, ceil,
            &format!("{:.0} {} ┐  {}", ceil, ch.unit(), ch.label()));
    }
}

/// Render the "Connecting to <label>…" splash shown while `connect_vm` is blocking.
///
/// This is drawn synchronously with `terminal.draw()` before the SSH call starts,
/// so the user sees feedback immediately rather than a frozen screen.
fn render_connecting(buf: &mut Buffer, area: Rect, label: &str) {
    let dim = Style::default().fg(Color::DarkGray);
    // Row 1: blank (same as ratatui Line::default())
    // Row 2: "  Connecting to LABEL — trying guest-agent, DNS, hostname…"
    if area.height >= 2 {
        let mut x = area.x;
        x = buf.set_string(x, area.y + 1, "  Connecting to ", dim);
        x = buf.set_string(x, area.y + 1, label,
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
        x = buf.set_string(x, area.y + 1, " — trying guest-agent, DNS, hostname…", dim);
        let _ = x;
    }
    if area.height >= 3 {
        buf.set_string(area.x, area.y + 2,
            "  This may take a few seconds. [Esc] to cancel.",
            Style::default().fg(Color::Rgb(80, 80, 80)));
    }
}

/// Build the status-line parts for the Groups footer.
///
/// Each element is a self-contained display chunk. Numeric values are
/// right-aligned in fixed-width fields so their unit suffixes stay at a
/// constant column offset — the number grows leftward, the unit stays put.
fn groups_status_parts(state: &AppState) -> Vec<String> {
    let mode_label = match &state.mode {
        AppMode::Local                              => "local /proc".to_string(),
        AppMode::Proxmox { url, .. }               => format!("proxmox {url}"),
        AppMode::Fleet { .. }                      => "fleet".to_string(),
        AppMode::Kube { namespace, .. }            => format!("kube/{namespace}"),
        AppMode::Nomad { addr, namespace, .. }     => format!("nomad/{namespace} {addr}"),
    };
    let interval_s = state.interval.as_secs_f64();
    let interval_str = if interval_s < 1.0 {
        format!("{interval_s:.2}")
    } else if interval_s.fract() == 0.0 {
        format!("{}", interval_s as u64)
    } else {
        format!("{interval_s:.1}")
    };

    let mut parts: Vec<String> = Vec::new();

    let n = state.entries.len();
    let hidden = state.total_groups.saturating_sub(n);
    if hidden > 0 {
        parts.push(format!("{n:>4} groups  ({hidden} hidden)"));
    } else {
        parts.push(format!("{n:>4} groups"));
    }
    parts.push(mode_label);
    parts.push(format!("{interval_str}s"));
    parts.push(format!("sort:{}", state.sort_metric.name()));

    match &state.mode {
        AppMode::Local => {
            // Right-align each rate in an 8-char field: unit stays fixed, number grows left.
            parts.push(format!(
                "net ↓{:>8}  ↑{:>8}",
                fmt_bytes_rate(state.sys_net_rx_s),
                fmt_bytes_rate(state.sys_net_tx_s),
            ));
            if let Some(gpu) = state.sys_gpu_pct {
                parts.push(format!("gpu {:>3.0}%", gpu));
            }
            if state.sys_rapl_w > 0.0 {
                parts.push(format!("{:>7} total", fmt_watts(state.sys_rapl_w)));
            }
            let any_psi = state.sys_psi_cpu.is_some()
                || state.sys_psi_mem.is_some()
                || state.sys_psi_io.is_some();
            if any_psi {
                let p = |v: Option<f64>| v.map_or("     ?".to_string(), |x| format!("{:>5.1}%", x));
                parts.push(format!(
                    "psi cpu:{}  mem:{}  io:{}",
                    p(state.sys_psi_cpu), p(state.sys_psi_mem), p(state.sys_psi_io),
                ));
            }
        }
        AppMode::Fleet { .. } => {
            let total = state.fleet_clients.len();
            let conn  = state.fleet_clients.iter().filter(|c| c.snap.is_some()).count();
            let errs  = state.fleet_clients.iter().filter(|c| c.err.is_some() && c.client.is_some()).count();
            let thin  = state.fleet_clients.iter().filter(|c| c.thin).count();
            let mut s = format!("{conn:>3}/{total} hosts");
            if thin > 0 { s.push_str(&format!("  {thin} thin")); }
            if errs > 0 { s.push_str(&format!("  {errs} disconnected")); }
            parts.push(s);
        }
        AppMode::Kube { .. } => {
            let total = state.kube_conns.len();
            let conn  = state.kube_conns.iter().filter(|c: &&KubeConn| c.client.is_some() && c.snap.is_some()).count();
            let errs  = state.kube_conns.iter().filter(|c: &&KubeConn| c.err.is_some()).count();
            let mut s = format!("pods {conn:>3}/{total}");
            if errs > 0 { s.push_str(&format!("  {errs} err")); }
            parts.push(s);
        }
        AppMode::Nomad { .. } => {
            let total = state.nomad_conns.len();
            let conn  = state.nomad_conns.iter().filter(|c: &&NomadConn| c.client.is_some() && c.snap.is_some()).count();
            let errs  = state.nomad_conns.iter().filter(|c: &&NomadConn| c.err.is_some()).count();
            let mut s = format!("allocs {conn:>3}/{total}");
            if errs > 0 { s.push_str(&format!("  {errs} err")); }
            parts.push(s);
        }
        AppMode::Proxmox { .. } => {
            for ns in &state.pve_node_status {
                let mem_g = ns.mem_used  as f64 / 1_073_741_824.0;
                let max_g = ns.mem_total as f64 / 1_073_741_824.0;
                parts.push(format!("{} {:>4.0}%cpu  {:>5.1}/{:>5.1}G",
                    ns.node, ns.cpu_pct, mem_g, max_g));
            }
            for ss in &state.pve_storage_status {
                if ss.total > 0 {
                    let pct = ss.used as f64 / ss.total as f64 * 100.0;
                    parts.push(format!("{} {:>3.0}%", ss.storage, pct));
                }
            }
        }
    }

    if state.running_unprivileged {
        parts.push("running unprivileged — disk/ctx/fds/swap/rss partial; rerun as root".to_string());
    }

    parts
}

/// Write the GPU device selector row at `(x0, y)`. Returns `true` if written.
fn write_gpu_selector_line(buf: &mut Buffer, state: &AppState, x0: u16, y: u16) -> bool {
    if !state.gpu_enabled || state.gpu_devices.is_empty() {
        return false;
    }
    let dim  = Style::default().fg(Color::DarkGray);
    let hi   = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let mut x = buf.set_string(x0, y, " GPU [/]: ", dim);
    if state.selected_gpu == 0 {
        x = buf.set_string(x, y, "[all]", hi);
    } else {
        x = buf.set_string(x, y, "all", dim);
    }
    for (i, dev) in state.gpu_devices.iter().enumerate() {
        x = buf.set_string(x, y, "  ", Style::default());
        let label = format!("{}:{}", dev.driver, dev.pci_addr);
        if state.selected_gpu == i + 1 {
            x = buf.set_string(x, y, &format!("[{label}]"), hi);
        } else {
            x = buf.set_string(x, y, &label, dim);
        }
    }
    let _ = x;
    true
}

// ── Manual ────────────────────────────────────────────────────────────────────

/// Render the scrollable in-app manual page.
///
/// Clamps `manual_scroll` to the range [0, max_scroll] so the page never
/// scrolls past the last line. Ratatui's `Paragraph::scroll` takes a (row, col)
/// offset, so we pass `(scroll as u16, 0)`.
fn render_manual(buf: &mut Buffer, area: Rect, state: &AppState) {
    if area.height == 0 { return; }
    let lines = manual_lines();
    let max_scroll = lines.len().saturating_sub(area.height as usize);
    let scroll = state.manual_scroll.min(max_scroll);
    let dim = Style::default().fg(Color::DarkGray);
    for (row, line) in lines.iter().skip(scroll).take(area.height as usize).enumerate() {
        buf.set_string(area.x, area.y + row as u16, line, dim);
    }
}

/// Build the static manual page content as a `Vec<Line>`.
///
/// Helper closures keep the formatting concise:
/// - `h(s)` → bold cyan heading
/// - `b(s)` → gray body text
/// - `d(s)` → dark gray (dimmed) annotation/example
/// - `kv(k, v)` → yellow key + gray description
/// - `blank()` → empty line
fn manual_lines() -> Vec<String> {
    vec![
        "  aerie  ·  real-time process-group activity monitor".into(),
        "".into(),
        "OVERVIEW".into(),
        "  Reads /proc and groups all processes by name (default), cgroup, or exe.".into(),
        "  Two metrics are shown simultaneously as a split meter bar.  The left".into(),
        "  bar grows toward the centre; the right bar grows inward from the far".into(),
        "  edge.  Both are capped at the midpoint so they never cross.".into(),
        "".into(),
        "  Eleven per-process metrics are available.  System-wide network, GPU,".into(),
        "  and power totals appear in the footer.".into(),
        "".into(),
        "  Local mode (default)  reads /proc on this machine.".into(),
        "  Proxmox mode (--proxmox URL --token T)  polls the PVE REST API and".into(),
        "  shows per-VM CPU and memory.".into(),
        "".into(),
        "  When running without root privileges, metrics that require reading other".into(),
        "  users' /proc files (disk I/O, context switches, FDs, swap, RSS) will be".into(),
        "  marked with a trailing '?' and bars will be dimmed.  Rerun as root for".into(),
        "  complete data.".into(),
        "".into(),
        "NAVIGATION".into(),
        "  [↑] [↓]  or  [j] [k]    move cursor up / down through the group list".into(),
        "  [Enter]                  open thread heatmap for the selected group".into(),
        "  [d]                      toggle the latency scope (diagnostics)".into(),
        "  [Esc]                    return from thread view or manual to group list".into(),
        "  [q]  [Ctrl-C]            quit".into(),
        "".into(),
        "DISPLAY CONTROLS".into(),
        "  [Tab]                    switch active side  (left <-> right)".into(),
        "  [<- / ->]                cycle the metric shown on the active side".into(),
        "  [s]                      re-sort list by the current active-side metric".into(),
        "  [h]                      toggle distribution histogram overlay".into(),
        "  [g]                      cycle grouping: comm->cgroup->exe (local) or flat->pool->tag->node (Proxmox)".into(),
        "  [n]                      toggle sort direction (descending / ascending)".into(),
        "  [r]                      toggle replay / live mode".into(),
        "  [,] [.]                  step backward / forward in replay history".into(),
        "  [/]                      cycle GPU device selector (when --enable-gpu)".into(),
        "".into(),
        "METRICS".into(),
        "  cpu%    CPU usage as percent of one core".into(),
        "  mem     Resident set size (fraction of physical RAM)".into(),
        "  faults/s  Minor+major page faults per second".into(),
        "  threads Thread count".into(),
        "  disk-r  Disk bytes read per second".into(),
        "  disk-w  Disk bytes written per second".into(),
        "  ctx-sw  Context switches per second (voluntary + involuntary)".into(),
        "  fds     Open file descriptor count".into(),
        "  swap    Swap in use, bytes".into(),
        "  runq    Scheduler wait %".into(),
        "  power   Estimated RAPL power (watts)".into(),
        "  throttle  CFS bandwidth throttle % (cgroup v2 + Cgroup mode)".into(),
        "  psi-cpu   CPU pressure stall avg10 (cgroup v2)".into(),
        "  psi-mem   Memory pressure stall avg10 (cgroup v2)".into(),
        "  psi-io    I/O pressure stall avg10 (cgroup v2)".into(),
        "  gpu%    GPU engine time % (--enable-gpu, AMD/Intel DRM)".into(),
        "  vram    GPU VRAM in use, bytes (--enable-gpu)".into(),
        "".into(),
        "PROXMOX MODE".into(),
        "  Start with: aerie --proxmox https://pve.lan:8006 --token USER@REALM!ID=SECRET".into(),
        "  Press Enter on a VM to SSH into it and monitor its processes.".into(),
        "  Requires --enable-remote. Uses your ~/.ssh/known_hosts by default.".into(),
        "".into(),
        "FLEET MODE".into(),
        "  aerie --hosts host1,host2,host3 --enable-remote".into(),
        "  aerie --hosts @/path/to/hosts.txt --enable-remote".into(),
        "  Monitor multiple SSH hosts simultaneously.".into(),
        "  Press Enter on a host to drill into its process list.".into(),
        "".into(),
        "KUBERNETES".into(),
        "  aerie --kube NAMESPACE".into(),
        "  aerie --kube NAMESPACE/SELECTOR".into(),
        "  Monitors pods via kubectl exec. Requires kubectl in PATH and RBAC.".into(),
        "  --kube-context CTX    use a specific kubeconfig context".into(),
        "  --kube-thin           thin /proc probe (no aerie in image needed)".into(),
        "".into(),
        "NOMAD".into(),
        "  aerie --nomad http://nomad.lan:4646".into(),
        "  aerie --nomad http://nomad.lan:4646 --nomad-job myjob".into(),
        "  Monitors Nomad allocations via nomad alloc exec.".into(),
        "  Requires the nomad CLI in PATH.".into(),
        "  --nomad-namespace NS  target namespace (default: default)".into(),
        "  --nomad-job JOB       filter to one job's allocations".into(),
        "  --nomad-thin          thin /proc probe (no aerie in allocation needed)".into(),
        "  ACL token: set NOMAD_TOKEN env var (not --token, which is Proxmox-only).".into(),
        "".into(),
        "DAEMON MODE".into(),
        "  aerie --daemon       stream JSON snapshots to stdout".into(),
        "  Used internally by remote drill-down and fleet mode.".into(),
        "".into(),
        "ANOMALY ALERTS".into(),
        "  aerie --alert-cmd /path/to/script".into(),
        "  Fired when a group's load concentration drops below the alert threshold.".into(),
        "  Args: GROUP_LABEL ANOMALY_KIND BALANCE_FRACTION".into(),
        "  Rate-limited to once per 60 seconds per group.".into(),
        "".into(),
        "LATENCY SCOPE (DIAGNOSTICS)".into(),
        "  Press [d] to open a built-in cyclictest: a dedicated thread measures how".into(),
        "  late its own timed wakeups are. That overshoot is the system scheduling".into(),
        "  jitter that makes every realtime UI (TUIs, video, audio) stutter at the".into(),
        "  same cadence — so it isolates a system-wide cause from a per-app one.".into(),
        "  Two stacked traces: wakeup latency (CPU scheduling jitter) and system".into(),
        "  pressure (run-queue + PSI stall) — the latter catches compositor- and".into(),
        "  memory-bound freezes that delay rendering without delaying CPU threads.".into(),
        "  Each carries a periodicity readout (the recurring stall's period); a".into(),
        "  ranked list names the system signals (IRQ/softirq, I/O & memory pressure,".into(),
        "  power) that co-occur with the latency stalls; and a periodic-offender line".into(),
        "  names any process group acting on a clock — CPU bursts or spawning helpers".into(),
        "  on a timer (the pattern that lets one app periodically stall the desktop).".into(),
        "".into(),
        "  --scope-log FILE      capture diagnostics to FILE (line-delimited JSON)".into(),
        "                        for the whole session, even without opening the view.".into(),
        "  --scope-analyze FILE  print an offline report from a captured log and exit.".into(),
        "  Tip: run  aerie --scope-log run.jsonl  during a session that stutters (e.g.".into(),
        "  a DAW), then  aerie --scope-analyze run.jsonl  to inspect it afterwards.".into(),
    ]
}

pub fn manual_line_count() -> usize {
    manual_lines().len()
}

pub fn manual_text() -> String {
    let lines = manual_lines();
    let mut out = String::with_capacity(lines.len() * 72);
    for line in lines {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comet_mask_full_and_empty() {
        // Solid comet head lights every dot (⣿ = U+28FF); no intensity → no dots.
        assert_eq!(comet_braille_mask(1.0, 0), 0xFF);
        assert_eq!(comet_braille_mask(1.0, 7), 0xFF);
        assert_eq!(comet_braille_mask(0.0, 0), 0x00);
        assert_eq!(comet_braille_mask(-1.0, 3), 0x00);
    }

    #[test]
    fn comet_mask_fills_from_the_bottom() {
        // A faint tail lights some dots but not the full cell, and never lights
        // the top row (dots 1 & 4) before the bottom row (dots 7 & 8).
        let m = comet_braille_mask(0.35, 0);
        assert!(m != 0x00 && m != 0xFF, "partial fill expected, got {m:#04x}");
        let top_row = 0b0000_1001u8; // dots 1 (0x01) + 4 (0x08): the top sub-row
        let bot_row = 0b1100_0000u8; // dots 7 (0x40) + 8 (0x80): the bottom sub-row
        assert!(m & top_row == 0, "top row must stay dark at low amp");
        assert!(m & bot_row != 0, "bottom row must light first");
    }

    #[test]
    fn comet_mask_brighter_lights_at_least_as_many_dots() {
        // Monotonic: raising intensity never removes a dot (per fixed cell).
        for col in 0..8 {
            let mut prev = 0u32;
            for step in 0..=10 {
                let amp = step as f32 / 10.0;
                let n = comet_braille_mask(amp, col).count_ones();
                assert!(n >= prev, "dots dropped as amp rose at col {col}");
                prev = n;
            }
        }
    }

    /// Return the index of the bin with the highest value.
    fn hot_bin(bins: &[f64]) -> usize {
        bins.iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    /// Compute the expected bin index for a member with fair-share multiple `r`,
    /// given `n` total members and `w` bins.  Mirrors the formula in `fair_share_bins`.
    fn expected_cell(r: f64, n: usize, w: usize) -> usize {
        let log2n = (n as f64).log2().max(1e-9);
        let scale = HIST_PIVOT_FRAC / (1.0 - HIST_PIVOT_FRAC);
        let l_min = -scale * log2n;
        let range = (1.0 + scale) * log2n;
        let t = ((r.log2() - l_min) / range).clamp(0.0, 1.0);
        ((1.0 + t * (w - 2) as f64).round() as usize).clamp(1, w - 1)
    }

    #[test]
    fn all_idle_is_all_black() {
        let bins = fair_share_bins(&[0.0, 0.0, 0.0, 0.0], 30);
        assert!(bins.iter().all(|&b| b < 1e-9), "all-idle bins should all be zero");
    }

    #[test]
    fn empty_members_is_all_black() {
        let bins = fair_share_bins(&[], 30);
        assert!(bins.iter().all(|&b| b < 1e-9));
    }

    #[test]
    fn perfectly_even_lands_at_pivot() {
        let w = 60;
        // N members all at identical load → each r_i = 1 → lands at pivot cell
        let vals = vec![2.5f64; 20];
        let bins = fair_share_bins(&vals, w);
        let hot = hot_bin(&bins);
        let pivot = expected_cell(1.0, vals.len(), w);
        assert!(
            (hot as isize - pivot as isize).abs() <= 1,
            "balanced: hot={hot}, pivot={pivot} (w={w})"
        );
    }

    #[test]
    fn one_hog_at_far_right() {
        let w = 40;
        let n = 100usize;
        // One member carries all the work; the rest are idle.
        let mut vals = vec![0.0f64; n];
        vals[0] = 1.0;
        let bins = fair_share_bins(&vals, w);
        let hot = hot_bin(&bins);
        // r_hog = 1 * N / 1 = N → log2(N) = L_max → cell w-1
        assert_eq!(hot, w - 1, "one hog: bright cell must be at far-right edge");
    }

    #[test]
    fn k_of_n_busy_equally_hits_expected_position() {
        let w = 60;
        let n = 64usize;
        let k = 8usize; // 8 of 64 members each carry 1/8 of the total
        let mut vals = vec![0.0f64; n];
        for v in vals.iter_mut().take(k) {
            *v = 1.0;
        }
        let bins = fair_share_bins(&vals, w);
        let hot = hot_bin(&bins);
        // Each busy member: r = (1/k * F) * N / F = N/k
        let expected = expected_cell(n as f64 / k as f64, n, w);
        assert!(
            (hot as isize - expected as isize).abs() <= 1,
            "k={k} of n={n}: hot={hot}, expected={expected}"
        );
    }

    #[test]
    fn skewed_tail_heat_is_right_of_pivot() {
        let w = 40;
        let n = 20usize;
        // One thread 10× busy, four at 1×, rest idle.
        let mut vals = vec![0.0f64; n];
        vals[0] = 10.0;
        for v in vals.iter_mut().take(5).skip(1) {
            *v = 1.0;
        }
        let bins = fair_share_bins(&vals, w);
        let pivot = expected_cell(1.0, n, w);
        let heat_left: f64 = bins[..pivot].iter().sum();
        let heat_right: f64 = bins[pivot..].iter().sum();
        assert!(
            heat_right > heat_left,
            "skewed: heat right of pivot ({heat_right:.3}) should exceed left ({heat_left:.3})"
        );
    }

    #[test]
    fn single_member_returns_all_zeros() {
        // N=1: distribution is meaningless, always return dark cells.
        let bins = fair_share_bins(&[5.0], 30);
        assert!(bins.iter().all(|&b| b == 0.0), "single member must produce all-zero bins");
    }
}
