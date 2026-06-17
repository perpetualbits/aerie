// SPDX-License-Identifier: GPL-3.0-or-later
use crate::{AppMode, AppState, AppView, BarEntry, KubeConn, NomadConn, Metric, PeakVals, Side, AnomalyState};
use mullion::{Buffer, Rect};
use mullion::layout::{Constraint, Node, Orientation, Size, TileId};
use mullion::style::{Color, Modifier, Style};

const HEADER_ID: TileId = 1;
const BODY_ID:   TileId = 2;
const FOOTER_ID: TileId = 3;

/// Top-level render entry point: splits the terminal into header / body / footer
/// using a mullion `Node::Split` and dispatches to per-section renderers.
pub fn render(buf: &mut Buffer, state: &AppState) {
    let area = buf.area;
    if area.height == 0 { return; }
    let term_w = area.width as usize;
    let hh = header_height(state);
    let fw = footer_height(state, term_w);
    let bh = area.height.saturating_sub(hh + fw);

    let mut root = Node::Split {
        orientation: Orientation::Vertical,
        children: vec![
            (Constraint::new(Size::Fixed(hh)), Node::Tile(HEADER_ID)),
            (Constraint::new(Size::Fill(1)),   Node::Tile(BODY_ID)),
            (Constraint::new(Size::Fixed(fw)), Node::Tile(FOOTER_ID)),
        ],
    };
    let tiles = mullion::layout::solve(&mut root, area);

    let mut header_rect = Rect::new(area.x, area.y, area.width, hh);
    let mut body_rect   = Rect::new(area.x, area.y + hh, area.width, bh);
    let mut footer_rect = Rect::new(area.x, area.y + hh + bh, area.width, fw);
    for (id, r) in &tiles {
        match *id {
            HEADER_ID => header_rect = *r,
            BODY_ID   => body_rect   = *r,
            FOOTER_ID => footer_rect = *r,
            _ => {}
        }
    }

    render_header(buf, header_rect, state);
    match &state.view {
        AppView::Groups | AppView::Remote { .. } => render_body(buf, body_rect, state),
        AppView::Threads { .. } => render_threads(buf, body_rect, state),
        AppView::Manual => render_manual(buf, body_rect, state),
        AppView::Connecting { label } => render_connecting(buf, body_rect, label),
    }
    render_footer(buf, footer_rect, state);
}

/// Compute the height of the header area.
///
/// The header is always at least 1 row (the divider line). A second row is added
/// when there is content to display above the divider: an error, a TLS warning,
/// remote/connecting info, the threads heat swatch, or the manual title.
fn header_height(state: &AppState) -> u16 {
    let has_content = match &state.view {
        AppView::Groups => state.error.is_some() || state.proxmox_insecure,
        AppView::Remote { .. } | AppView::Connecting { .. } | AppView::Threads { .. } | AppView::Manual => true,
    };
    if has_content { 2 } else { 1 }
}

/// Render the header: an optional content row followed by a divider line.
///
/// Content row (when present):
///   Groups:     error message  OR  TLS warning
///   Remote:     "remote: <label> · [Esc] disconnect"
///   Connecting: "Connecting to <label> …"
///   Threads:    heat colour swatch (idle → hot)
///   Manual:     title + scroll/close hints
///
/// Divider row: a `─` line, with the histogram legend carved in when the
/// overlay is active: `──┤← balanced├──┤◻◻…◻◻ = work density├──┤hot spots →├──`
fn render_header(buf: &mut Buffer, area: Rect, state: &AppState) {
    let h = area.height;
    if h == 0 { return; }

    let (content_y, divider_y) = if h == 1 {
        (None, area.y)
    } else {
        (Some(area.y), area.y + h - 1)
    };

    let dim = Style::default().fg(Color::DarkGray);

    if let Some(cy) = content_y {
        let mut x = area.x;
        match &state.view {
            AppView::Groups => {
                if let Some(err) = &state.error {
                    x = buf.set_string(x, cy,
                        &format!(" error: {}", err.lines().next().unwrap_or("")),
                        Style::default().fg(Color::Red));
                } else if state.proxmox_insecure {
                    x = buf.set_string(x, cy, "  ⚠ TLS OFF",
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
                }
            }
            AppView::Remote { label } => {
                x = buf.set_string(x, cy, " remote: ", dim);
                x = buf.set_string(x, cy, label,
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
                x = buf.set_string(x, cy, "  ·  [Esc] disconnect", dim);
            }
            AppView::Connecting { label } => {
                x = buf.set_string(x, cy, " Connecting to ", dim);
                x = buf.set_string(x, cy, label,
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
                x = buf.set_string(x, cy, "  …", dim);
            }
            AppView::Threads { .. } => {
                x = buf.set_string(x, cy, " heat: idle ", dim);
                const SWATCH: usize = 24;
                for i in 0..SWATCH {
                    let frac = i as f64 / (SWATCH - 1) as f64;
                    x = buf.set_string(x, cy, "◻", Style::default().fg(planck_color(frac)));
                }
                x = buf.set_string(x, cy, " hot", dim);
            }
            AppView::Manual => {
                x = buf.set_string(x, cy, " manual",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
                x = buf.set_string(x, cy,
                    "  ·  ↑/↓ to scroll  ·  [m] or [Esc] to close", dim);
            }
        }
        let _ = x;
    }

    render_divider(buf, Rect::new(area.x, divider_y, area.width, 1), state);
}

/// Render the horizontal divider between header and body.
///
/// When the histogram overlay is active, the divider carries a three-section
/// legend showing the meaning of the colour scale:
///
///   ──┤← balanced├──┤◻◻◻◻◻◻◻◻◻◻◻◻◻◻◻◻◻◻◻◻◻◻◻◻ = work density├──┤hot spots →├──
///
/// The coloured ◻ strip fills all available width between the fixed labels.
/// Below a minimum terminal width the divider degrades to a plain ─ line.
fn render_divider(buf: &mut Buffer, area: Rect, state: &AppState) {
    let w = area.width as usize;
    if w == 0 { return; }
    let y = area.y;
    let dim = Style::default().fg(Color::DarkGray);

    let show_legend = state.show_histogram
        && matches!(state.view, AppView::Groups | AppView::Remote { .. });

    const FIXED: usize = 50;
    const MAX_SWATCH: usize = 28;
    const MIN_SWATCH: usize = 4;

    if !show_legend || w < FIXED + MIN_SWATCH {
        buf.set_string(area.x, y, &"─".repeat(w), dim);
        return;
    }

    let available = w - FIXED;
    let swatch_w  = available.min(MAX_SWATCH);
    let extra     = available - swatch_w;
    let left_pad  = extra / 2;
    let right_pad = extra - left_pad;

    let mut x = area.x;
    x = buf.set_string(x, y, "──┤", dim);
    x = buf.set_string(x, y, "← balanced", Style::default().fg(Color::Rgb(60, 180, 60)));
    x = buf.set_string(x, y, "├──", dim);
    if left_pad > 0 {
        x = buf.set_string(x, y, &"─".repeat(left_pad), dim);
    }
    x = buf.set_string(x, y, "┤", dim);
    for i in 0..swatch_w {
        let frac = i as f64 / (swatch_w - 1).max(1) as f64;
        x = buf.set_string(x, y, "◻", Style::default().fg(planck_color(frac)));
    }
    x = buf.set_string(x, y, " = work density", dim);
    x = buf.set_string(x, y, "├", dim);
    if right_pad > 0 {
        x = buf.set_string(x, y, &"─".repeat(right_pad), dim);
    }
    x = buf.set_string(x, y, "──┤", dim);
    x = buf.set_string(x, y, "hot spots →", Style::default().fg(Color::Rgb(220, 80, 0)));
    x = buf.set_string(x, y, "├──", dim);
    let _ = x;
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
fn truncate_label(label: &str, width: usize) -> String {
    let chars: Vec<char> = label.chars().collect();
    if chars.len() <= width {
        format!("{label:<width$}")
    } else {
        // Truncate to width-1 chars and append the ellipsis character.
        let mut s: String = chars[..width.saturating_sub(1)].iter().collect();
        s.push('…');
        s
    }
}

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
fn render_body(buf: &mut Buffer, area: Rect, state: &AppState) {
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

    let label_w = state.entries.iter().map(|e| e.label.len()).max().unwrap_or(8).clamp(8, 28);
    const VAL_W: usize = 9;
    const ARROW_W: usize = 3;
    let total_w = area.width as usize;
    let fixed = label_w + 1 + ARROW_W + VAL_W + VAL_W + ARROW_W;
    let bar_w = total_w.saturating_sub(fixed).max(16);
    let peaks = &state.peak_vals;

    // ── column-header row ─────────────────────────────────────────────────────
    let lname = lm.name();
    let rname = rm.name();
    let mid_spaces = bar_w.saturating_sub(lname.len() + rname.len());
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
    let hy = area.y;
    let mut x = area.x;
    x = buf.set_string(x, hy, &truncate_label(&group_by_label, label_w),
        Style::default().fg(Color::Rgb(60, 60, 60)));
    x = buf.set_string(x, hy, &" ".repeat(1 + ARROW_W + VAL_W), Style::default());
    x = buf.set_string(x, hy, lname, left_hdr_style);
    x = buf.set_string(x, hy, &" ".repeat(mid_spaces), Style::default());
    x = buf.set_string(x, hy, rname, right_hdr_style);
    let _ = x;

    let max_rows = area.height as usize;
    let scroll = state.scroll_offset.min(state.entries.len().saturating_sub(1));
    let entry_rows = max_rows.saturating_sub(1);

    for (row_i, (i, e)) in state.entries.iter().enumerate()
        .skip(scroll).take(entry_rows).enumerate()
    {
        let ey = area.y + 1 + row_i as u16;
        if ey >= area.bottom() { break; }
        let fading = e.fading;
        let is_selected = i == state.cursor;

        let lf = if fading { 0.0 } else { metric_frac(e, lm, state.total_ram_bytes, peaks) };
        let rf = if fading { 0.0 } else { metric_frac(e, rm, state.total_ram_bytes, peaks) };
        let l_incomplete = !fading && !entry_complete(e, lm);
        let r_incomplete = !fading && !entry_complete(e, rm);
        let lc = if fading { Color::DarkGray } else {
            let c = bar_color(lm, lf); if l_incomplete { dimmed(c) } else { c }
        };
        let rc = if fading { Color::DarkGray } else {
            let c = bar_color(rm, rf); if r_incomplete { dimmed(c) } else { c }
        };

        let usable = (bar_w / 2).saturating_sub(1);
        let l_filled = (lf * usable as f64).round() as usize;
        let r_filled = (rf * usable as f64).round() as usize;
        let right_start = bar_w - r_filled;

        let anomaly = state.anomaly_states.get(&e.label);
        let is_anomaly = anomaly.is_some_and(|s: &AnomalyState| s.alerting);

        let label_style = if is_selected {
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else if e.fading {
            let r = lerp_u8(0, 80, e.fade_t);
            let g = lerp_u8(200, 80, e.fade_t);
            let b = lerp_u8(200, 80, e.fade_t);
            Style::default().fg(Color::Rgb(r, g, b)).add_modifier(Modifier::BOLD)
        } else if is_anomaly {
            Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        };

        let display_label = if is_anomaly && !e.fading && !is_selected {
            truncate_label(&format!("! {}", e.label), label_w)
        } else {
            truncate_label(&e.label, label_w)
        };

        let l_str = metric_display_str(e, lm, state.total_ram_bytes);
        let r_str = metric_display_str(e, rm, state.total_ram_bytes);

        let hist: Option<Vec<f64>> = if state.show_histogram {
            if overlay_enabled && !fading {
                Some(state.group_member_vals.get(&e.label).map(|v| {
                    if v.metric == hist_metric {
                        fair_share_bins(&v.vals, bar_w)
                    } else {
                        vec![0.0; bar_w]
                    }
                }).unwrap_or_else(|| vec![0.0; bar_w]))
            } else {
                Some(vec![0.0; bar_w])
            }
        } else {
            None
        };

        let arrow_style = Style::default().fg(Color::Cyan);
        let blank_arrow = Style::default();
        let (l_arrow, r_arrow, a_style) = if is_selected {
            ("▶▶▶", "◀◀◀", arrow_style)
        } else {
            ("   ", "   ", blank_arrow)
        };

        let mut x = area.x;
        x = buf.set_string(x, ey, &display_label, label_style);
        x = buf.set_string(x, ey, " ", Style::default());
        x = buf.set_string(x, ey, l_arrow, a_style);
        x = buf.set_string(x, ey, &format!("{:>VAL_W$}", l_str), Style::default().fg(lc));
        for bi in 0..bar_w {
            let in_left  = bi < l_filled;
            let in_right = bi >= right_start;
            if let Some(ref h) = hist {
                let bg = if in_left { lc } else if in_right { rc } else { Color::Reset };
                x = buf.set_string(x, ey, "◻", Style::default().fg(planck_color(h[bi])).bg(bg));
            } else if in_left {
                x = buf.set_string(x, ey, "█", Style::default().fg(lc));
            } else if in_right {
                x = buf.set_string(x, ey, "█", Style::default().fg(rc));
            } else {
                x = buf.set_string(x, ey, "░", Style::default().fg(Color::DarkGray));
            }
        }
        x = buf.set_string(x, ey, &format!("{:<VAL_W$}", r_str), Style::default().fg(rc));
        x = buf.set_string(x, ey, r_arrow, a_style);
        let _ = x;
    }
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
    let cells_per_row = (w / 2).max(1);
    const MAX_HEAT_ROWS: usize = 4;
    let max_cells = MAX_HEAT_ROWS * cells_per_row;
    let mut group_size = 1usize;
    while n > 0 && n.div_ceil(group_size) > max_cells { group_size *= 2; }
    let num_cells = if n == 0 { 0 } else { n.div_ceil(group_size) };
    let cell_cpus: Vec<f64> = (0..num_cells).map(|i| {
        let start = i * group_size;
        let end = (start + group_size).min(n);
        state.thread_samples[start..end].iter().map(|t| t.cpu_pct).fold(0.0f64, f64::max)
    }).collect();
    let heat_rows = if num_cells == 0 { 1 } else {
        num_cells.div_ceil(cells_per_row).clamp(1, MAX_HEAT_ROWS)
    };

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
    if state.thread_samples.is_empty() {
        if heat_y < area.bottom() {
            buf.set_string(area.x, heat_y, "  waiting for second sample…", dim);
        }
    } else {
        let mut idx = 0;
        'outer: for row in 0..heat_rows {
            let ry = heat_y + row as u16;
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

    // Horizontal divider
    if div_y < area.bottom() {
        buf.set_string(area.x, div_y, &"─".repeat(w), dim);
    }

    // Thread list
    if list_y < area.bottom() && list_h > 0 {
        let name_w = state.thread_samples.iter().map(|t| t.name.len())
            .max().unwrap_or(10).clamp(8, 24);
        buf.set_string(area.x, list_y,
            &format!("  {:<name_w$}  {:>6}  pid:tid", "thread", "cpu%"), dim);
        for (row, t) in state.thread_samples.iter()
            .take(list_h.saturating_sub(1) as usize).enumerate()
        {
            let ty = list_y + 1 + row as u16;
            if ty >= area.bottom() { break; }
            let mut x = area.x;
            x = buf.set_string(x, ty, "◻ ",
                Style::default().fg(planck_color(t.cpu_pct / max_cpu)));
            x = buf.set_string(x, ty,
                &format!("{:<name_w$}  {:>5.2}%  {}:{}", t.name, t.cpu_pct, t.pid, t.tid),
                Style::default().fg(Color::Gray));
            let _ = x;
        }
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

/// Compute the footer height for the current view and terminal width.
///
/// The Groups view status line is made of wrapping parts, so its height varies.
/// All other views use a fixed 2-row footer.
fn footer_height(state: &AppState, term_w: usize) -> u16 {
    match &state.view {
        AppView::Groups => {
            let parts = groups_status_parts(state);
            (packed_row_count(&parts, term_w) as u16 + 1).min(6)
        }
        _ => 2,
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

/// Count how many display rows `parts` need when packed left-to-right with
/// `"  │  "` separators and a 1-space left margin, wrapping at `width`.
fn packed_row_count(parts: &[String], width: usize) -> usize {
    if parts.is_empty() || width == 0 {
        return 1;
    }
    const SEP: usize = 5; // "  │  "
    let mut rows = 1usize;
    let mut col  = 0usize;
    for (i, p) in parts.iter().enumerate() {
        if i == 0 {
            col = 1 + p.len();
        } else if col + SEP + p.len() > width {
            rows += 1;
            col   = 1 + p.len();
        } else {
            col  += SEP + p.len();
        }
    }
    rows
}

/// Greedy-pack `parts` into rows: each part is separated by "  │  ".
/// Returns (row_index, col_x) offsets for each part in `area`, for direct buffer writes.
fn write_packed_parts(buf: &mut Buffer, parts: &[String], area: Rect) {
    const SEP: &str = "  │  ";
    let dim    = Style::default().fg(Color::DarkGray);
    let sep_st = Style::default().fg(Color::Rgb(50, 50, 50));
    let w = area.width as usize;
    let mut y = area.y;
    let mut col = 0usize;
    let mut x = area.x;

    for (i, p) in parts.iter().enumerate() {
        if y >= area.bottom() { break; }
        if i == 0 {
            x = buf.set_string(area.x, y, " ", dim);
            x = buf.set_string(x, y, p, dim);
            col = 1 + p.len();
        } else if col + SEP.len() + p.len() > w {
            y += 1;
            if y >= area.bottom() { break; }
            x = buf.set_string(area.x, y, " ", dim);
            x = buf.set_string(x, y, p, dim);
            col = 1 + p.len();
        } else {
            x = buf.set_string(x, y, SEP, sep_st);
            x = buf.set_string(x, y, p, dim);
            col += SEP.len() + p.len();
        }
    }
    let _ = x;
}

/// Write the GPU device selector row at `(area.x, y)`. Returns `true` if written.
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

/// Render the Groups-view footer: parts-based status rows + key hints (or GPU selector).
fn render_footer_groups(buf: &mut Buffer, area: Rect, state: &AppState) {
    if area.height == 0 { return; }
    let w = area.width as usize;
    let parts = groups_status_parts(state);
    let status_rows = packed_row_count(&parts, w);
    // Write status rows (may occupy 1 or 2 lines depending on width).
    let status_area = Rect::new(area.x, area.y, area.width,
        (status_rows as u16).min(area.height));
    write_packed_parts(buf, &parts, status_area);

    // Last row: key hints, GPU selector, or replay indicator.
    let last_y = area.y + area.height.saturating_sub(1);
    if last_y < area.bottom() {
        let keys_style = if state.history_cursor.is_some() {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Rgb(60, 60, 60))
        };

        if write_gpu_selector_line(buf, state, area.x, last_y) {
            // GPU selector replaces key hints.
        } else if let Some(cursor) = state.history_cursor {
            let age   = state.history.get(cursor).map(|h| h.at.elapsed().as_secs()).unwrap_or(0);
            let total = state.history.len();
            buf.set_string(area.x, last_y,
                &format!(" PAUSED  ◀ {age}s ago ▶  sample {}/{total}  [←/→] scrub  [p] resume  [q] quit",
                    cursor + 1),
                keys_style);
        } else {
            let enter_part = if matches!(state.mode, AppMode::Local) {
                "  [Enter] threads"
            } else if matches!(state.mode, AppMode::Fleet { .. } | AppMode::Kube { .. }) || state.enable_remote {
                "  [Enter] drill down"
            } else {
                ""
            };
            buf.set_string(area.x, last_y,
                &format!(" [←/→] metric  [Tab] side  [s] sort  [h] hist  [g] group-by  [↑/↓] cursor{enter_part}  [p] pause  [r] refresh  [m] manual  [q] quit"),
                keys_style);
        }
    }
}

/// Render the footer: status line(s) + key hints.
///
/// The Groups view uses a parts-based variable-height layout (see
/// `render_footer_groups`). All other views use a fixed 2-row layout.
fn render_footer(buf: &mut Buffer, area: Rect, state: &AppState) {
    if matches!(state.view, AppView::Groups) {
        render_footer_groups(buf, area, state);
        return;
    }
    let mode_label = match &state.mode {
        AppMode::Local                          => "local /proc".to_string(),
        AppMode::Proxmox { url, .. }           => format!("proxmox {url}"),
        AppMode::Fleet { .. }                  => "fleet".to_string(),
        AppMode::Kube { namespace, .. }        => format!("kube/{namespace}"),
        AppMode::Nomad { addr, namespace, .. } => format!("nomad/{namespace} {addr}"),
    };
    let interval_s = state.interval.as_secs_f64();
    let interval_str = if interval_s < 1.0 {
        format!("{interval_s:.2}")
    } else if interval_s.fract() == 0.0 {
        format!("{}", interval_s as u64)
    } else {
        format!("{interval_s:.1}")
    };

    let sys_metrics = |state: &AppState| -> String {
        let mut s = format!(
            "  │  net ↓{}  ↑{}",
            fmt_bytes_rate(state.sys_net_rx_s),
            fmt_bytes_rate(state.sys_net_tx_s),
        );
        if let Some(gpu) = state.sys_gpu_pct {
            s.push_str(&format!("  │  gpu {:.0}%", gpu));
        }
        if state.sys_rapl_w > 0.0 {
            s.push_str(&format!("  │  {} total", fmt_watts(state.sys_rapl_w)));
        }
        let any_psi = state.sys_psi_cpu.is_some()
            || state.sys_psi_mem.is_some()
            || state.sys_psi_io.is_some();
        if any_psi {
            let fmt_psi = |v: Option<f64>| v.map_or("?".into(), |x| format!("{:.1}%", x));
            s.push_str(&format!(
                "  │  psi cpu:{} mem:{} io:{}",
                fmt_psi(state.sys_psi_cpu),
                fmt_psi(state.sys_psi_mem),
                fmt_psi(state.sys_psi_io),
            ));
        }
        s
    };

    let (status, keys) = match &state.view {
        AppView::Manual => (
            " manual".to_string(),
            " [↑/↓] scroll  [m] close  [q] quit".to_string(),
        ),
        AppView::Connecting { label } => (
            format!(" Connecting to {label}…"),
            " [Esc] cancel  [q] quit".to_string(),
        ),
        AppView::Threads { label } => {
            let n = state.thread_samples.len();
            let total: f64 = state.thread_samples.iter().map(|t| t.cpu_pct).sum();
            let s = format!(" {label}  │  {n} threads  │  total {total:.2}%  │  {mode_label}  │  {interval_str}s");
            let k = " [Esc] back  [r] refresh  [q] quit".to_string();
            (s, k)
        }
        AppView::Remote { label } => {
            let host = state.remote_client.as_ref().map(|c| c.host.as_str()).unwrap_or("?");
            let sys = sys_metrics(state);
            let s = format!(" remote: {label} ({host})  │  {} processes{sys}", state.entries.len());
            let k = if let Some(cursor) = state.history_cursor {
                let age = state.history.get(cursor).map(|h| h.at.elapsed().as_secs()).unwrap_or(0);
                let total = state.history.len();
                format!(" PAUSED  ◀ {}s ago ▶  sample {}/{}  [←/→] scrub  [p] resume  [Esc] disconnect  [q] quit",
                    age, cursor + 1, total)
            } else {
                " [←/→] metric  [Tab] side  [s] sort  [↑/↓] cursor  [p] pause  [Esc] disconnect  [q] quit".to_string()
            };
            (s, k)
        }
        AppView::Groups => unreachable!("handled by render_footer_groups"),
    };

    let keys_style = if state.history_cursor.is_some() {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Rgb(60, 60, 60))
    };

    if area.height >= 1 {
        buf.set_string(area.x, area.y, &status, Style::default().fg(Color::DarkGray));
    }
    if area.height >= 2 {
        let last_y = area.y + 1;
        // For Remote view, GPU selector replaces key hints when devices are present.
        let wrote_gpu = matches!(state.view, AppView::Remote { .. })
            && write_gpu_selector_line(buf, state, area.x, last_y);
        if !wrote_gpu {
            buf.set_string(area.x, last_y, &keys, keys_style);
        }
    }
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
        "  apptop  ·  real-time process-group activity monitor".into(),
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
        "  Start with: apptop --proxmox https://pve.lan:8006 --token USER@REALM!ID=SECRET".into(),
        "  Press Enter on a VM to SSH into it and monitor its processes.".into(),
        "  Requires --enable-remote. Uses your ~/.ssh/known_hosts by default.".into(),
        "".into(),
        "FLEET MODE".into(),
        "  apptop --hosts host1,host2,host3 --enable-remote".into(),
        "  apptop --hosts @/path/to/hosts.txt --enable-remote".into(),
        "  Monitor multiple SSH hosts simultaneously.".into(),
        "  Press Enter on a host to drill into its process list.".into(),
        "".into(),
        "KUBERNETES".into(),
        "  apptop --kube NAMESPACE".into(),
        "  apptop --kube NAMESPACE/SELECTOR".into(),
        "  Monitors pods via kubectl exec. Requires kubectl in PATH and RBAC.".into(),
        "  --kube-context CTX    use a specific kubeconfig context".into(),
        "  --kube-thin           thin /proc probe (no apptop in image needed)".into(),
        "".into(),
        "NOMAD".into(),
        "  apptop --nomad http://nomad.lan:4646".into(),
        "  apptop --nomad http://nomad.lan:4646 --nomad-job myjob".into(),
        "  Monitors Nomad allocations via nomad alloc exec.".into(),
        "  Requires the nomad CLI in PATH.".into(),
        "  --nomad-namespace NS  target namespace (default: default)".into(),
        "  --nomad-job JOB       filter to one job's allocations".into(),
        "  --nomad-thin          thin /proc probe (no apptop in allocation needed)".into(),
        "  ACL token: set NOMAD_TOKEN env var (not --token, which is Proxmox-only).".into(),
        "".into(),
        "DAEMON MODE".into(),
        "  apptop --daemon       stream JSON snapshots to stdout".into(),
        "  Used internally by remote drill-down and fleet mode.".into(),
        "".into(),
        "ANOMALY ALERTS".into(),
        "  apptop --alert-cmd /path/to/script".into(),
        "  Fired when a group's load concentration drops below the alert threshold.".into(),
        "  Args: GROUP_LABEL ANOMALY_KIND BALANCE_FRACTION".into(),
        "  Rate-limited to once per 60 seconds per group.".into(),
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
