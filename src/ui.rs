// SPDX-License-Identifier: GPL-3.0-or-later
use crate::{AppMode, AppState, AppView, BarEntry, KubeConn, Metric, PeakVals, Side, AnomalyState};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

/// Top-level render entry point: splits the terminal into header / body / footer and
/// dispatches to the appropriate view renderer based on `state.view`.
///
/// Layout (from top to bottom):
///   - 3 rows: header (metric selectors, hint bar, or colour swatch).
///   - remaining rows: body (group list, thread view, manual, or connecting screen).
///   - 2 rows: footer (status line + key hints).
pub fn render(frame: &mut Frame, state: &AppState) {
    let area = frame.area();
    let term_w = area.width as usize;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(header_height(state)), Constraint::Min(0), Constraint::Length(footer_height(state, term_w))])
        .split(area);
    render_header(frame, chunks[0], state);
    match &state.view {
        AppView::Groups | AppView::Remote { .. } => render_body(frame, chunks[1], state),
        AppView::Threads { .. } => render_threads(frame, chunks[1], state),
        AppView::Manual => render_manual(frame, chunks[1], state),
        AppView::Connecting { label } => render_connecting(frame, chunks[1], label),
    }
    render_footer(frame, chunks[2], state);
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
/// overlay is active: `──┤← balanced├──┤◻◻…◻◻ = focus├──┤hot spots →├──`
fn render_header(frame: &mut Frame, area: Rect, state: &AppState) {
    let h = area.height;
    if h == 0 {
        return;
    }

    let (content_area, divider_area) = if h == 1 {
        (None, area)
    } else {
        let splits = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(h - 1), Constraint::Length(1)])
            .split(area);
        (Some(splits[0]), splits[1])
    };

    if let Some(ca) = content_area {
        let line = match &state.view {
            AppView::Groups => {
                if let Some(err) = &state.error {
                    Line::from(Span::styled(
                        format!(" error: {}", err.lines().next().unwrap_or("")),
                        Style::default().fg(Color::Red),
                    ))
                } else if state.proxmox_insecure {
                    Line::from(Span::styled(
                        "  ⚠ TLS OFF",
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    ))
                } else {
                    Line::default()
                }
            }
            AppView::Remote { label } => Line::from(vec![
                Span::styled(" remote: ", Style::default().fg(Color::DarkGray)),
                Span::styled(label.clone(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::styled("  ·  [Esc] disconnect", Style::default().fg(Color::DarkGray)),
            ]),
            AppView::Connecting { label } => Line::from(vec![
                Span::styled(" Connecting to ", Style::default().fg(Color::DarkGray)),
                Span::styled(label.clone(), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled("  …", Style::default().fg(Color::DarkGray)),
            ]),
            AppView::Threads { .. } => {
                let mut spans = vec![Span::styled(" heat: idle ", Style::default().fg(Color::DarkGray))];
                const SWATCH: usize = 24;
                for i in 0..SWATCH {
                    let frac = i as f64 / (SWATCH - 1) as f64;
                    spans.push(Span::styled("◻", Style::default().fg(planck_color(frac))));
                }
                spans.push(Span::styled(" hot", Style::default().fg(Color::DarkGray)));
                Line::from(spans)
            }
            AppView::Manual => Line::from(vec![
                Span::styled(" manual", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::styled("  ·  ↑/↓ to scroll  ·  [m] or [Esc] to close", Style::default().fg(Color::DarkGray)),
            ]),
        };
        frame.render_widget(Paragraph::new(line), ca);
    }

    render_divider(frame, divider_area, state);
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
fn render_divider(frame: &mut Frame, area: Rect, state: &AppState) {
    let w = area.width as usize;
    if w == 0 {
        return;
    }
    let dim = Style::default().fg(Color::DarkGray);

    let show_legend = state.show_histogram
        && matches!(state.view, AppView::Groups | AppView::Remote { .. });

    // Fixed chars (excluding the variable-width swatch and centering pads):
    //   "──┤" (3) + "← balanced" (10) + "├──" (3) + "┤" (1)
    //   + " = work density├" (16) + "──┤" (3) + "hot spots →" (11) + "├──" (3)  = 50
    const FIXED: usize = 50;
    // Cap the swatch so it sits roughly centred; extra space becomes ─ dashes on each side.
    const MAX_SWATCH: usize = 28;
    const MIN_SWATCH: usize = 4;

    if !show_legend || w < FIXED + MIN_SWATCH {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("─".repeat(w), dim))),
            area,
        );
        return;
    }

    let available = w - FIXED;
    let swatch_w  = available.min(MAX_SWATCH);
    let extra     = available - swatch_w;
    let left_pad  = extra / 2;
    let right_pad = extra - left_pad;

    let mut spans: Vec<Span<'static>> = vec![
        Span::styled("──┤", dim),
        Span::styled("← balanced", Style::default().fg(Color::Rgb(60, 180, 60))),
        Span::styled("├──", dim),
    ];
    if left_pad > 0 {
        spans.push(Span::styled("─".repeat(left_pad), dim));
    }
    spans.push(Span::styled("┤", dim));
    for i in 0..swatch_w {
        let frac = i as f64 / (swatch_w - 1).max(1) as f64;
        spans.push(Span::styled("◻", Style::default().fg(planck_color(frac))));
    }
    spans.push(Span::styled(" = work density", dim));
    spans.push(Span::styled("├", dim));
    if right_pad > 0 {
        spans.push(Span::styled("─".repeat(right_pad), dim));
    }
    spans.push(Span::styled("──┤", dim));
    spans.push(Span::styled("hot spots →", Style::default().fg(Color::Rgb(220, 80, 0))));
    spans.push(Span::styled("├──", dim));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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
/// y-axis = **work share** per bin: wᵦ = (Σᵢ∈b mᵢ)/F.  This encodes "where
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
fn render_body(frame: &mut Frame, area: Rect, state: &AppState) {
    let inner = Block::default().borders(Borders::NONE).inner(area);
    frame.render_widget(Block::default().borders(Borders::NONE), area);

    if state.entries.is_empty() {
        let msg = if state.snap_count < 2 {
            // First sample is just a baseline; second sample is the first with delta data.
            "Collecting first sample — waiting for next refresh tick…"
        } else {
            "Nothing active to display."
        };
        frame.render_widget(
            Paragraph::new(msg).style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let lm = state.left_metric;
    let rm = state.right_metric;

    // The histogram tracks the focused side's metric.
    // Memory (per-process, not per-thread) and Threads count are not per-member
    // attributable in local mode, so the overlay is blanked for those metrics.
    let hist_metric = match state.active_side {
        Side::Left => lm,
        Side::Right => rm,
    };
    // Overlay is enabled for any metric that can have per-member data.
    // Local mode: threads supply CPU/fault/disk/ctx/sched data.
    // Proxmox mode: VMs supply CPU/memory/disk data for the group overlay.
    // Memory is included here; local mode simply won't have Memory member vals,
    // so it shows a dark floor (all-zero bins) for that metric.
    let overlay_enabled = state.show_histogram
        && matches!(
            hist_metric,
            Metric::Cpu | Metric::Memory | Metric::PageFaults | Metric::DiskRead
                | Metric::DiskWrite | Metric::CtxSwitches | Metric::SchedWait
                | Metric::CfsThrottle | Metric::PsiCpu | Metric::PsiMem | Metric::PsiIo
        );

    // Label column width: widest label, clamped to [8, 28] chars.
    let label_w = state.entries.iter().map(|e| e.label.len()).max().unwrap_or(8).clamp(8, 28);
    // Fixed value columns on each side of the bar
    const VAL_W: usize = 9;   // one extra for possible trailing '?'
    const ARROW_W: usize = 3; // ▶▶▶ / ◀◀◀ cursor arrows bracketing the bar

    let total_w = inner.width as usize;
    let fixed = label_w + 1 + ARROW_W + VAL_W + VAL_W + ARROW_W;
    // Ensure at least 16 cells for the bar even on very narrow terminals.
    let bar_w = total_w.saturating_sub(fixed).max(16);

    let peaks = &state.peak_vals;

    // ── column-header line ────────────────────────────────────────────────
    // Shows the grouping strategy on the left and metric names above the bar edges.
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
    let header_line = Line::from(vec![
        Span::styled(
            truncate_label(&group_by_label, label_w),
            Style::default().fg(Color::Rgb(60, 60, 60)),
        ),
        Span::raw(" ".repeat(1 + ARROW_W + VAL_W)),
        Span::styled(lname, left_hdr_style),
        Span::raw(" ".repeat(mid_spaces)),
        Span::styled(rname, right_hdr_style),
    ]);

    let max_rows = inner.height as usize;
    let scroll = state.scroll_offset.min(state.entries.len().saturating_sub(1));

    // Reserve 1 row for the column header; show max_rows-1 entries below it.
    let entry_rows = max_rows.saturating_sub(1);

    let entry_lines: Vec<Line> = state
        .entries
        .iter()
        .enumerate()
        .skip(scroll)
        .take(entry_rows)
        .map(|(i, e)| {
            let fading = e.fading;
            let is_selected = i == state.cursor;

            // Fading rows show no bar fill (all rates have been zeroed).
            let lf = if fading { 0.0 } else { metric_frac(e, lm, state.total_ram_bytes, peaks) };
            let rf = if fading { 0.0 } else { metric_frac(e, rm, state.total_ram_bytes, peaks) };

            let l_incomplete = !fading && !entry_complete(e, lm);
            let r_incomplete = !fading && !entry_complete(e, rm);

            let lc = if fading {
                Color::DarkGray
            } else {
                let c = bar_color(lm, lf);
                if l_incomplete { dimmed(c) } else { c }
            };
            let rc = if fading {
                Color::DarkGray
            } else {
                let c = bar_color(rm, rf);
                if r_incomplete { dimmed(c) } else { c }
            };

            // Each bar fills at most (bar_w/2 - 1) cells, guaranteeing a visible
            // 2-cell dim gap at the centre even when both metrics are at 100%.
            let usable = (bar_w / 2).saturating_sub(1);
            let l_filled = (lf * usable as f64).round() as usize;
            let r_filled = (rf * usable as f64).round() as usize;
            // Right bar starts at `right_start` and extends to the right edge.
            let right_start = bar_w - r_filled;

            // Check anomaly state for this entry.
            let anomaly = state.anomaly_states.get(&e.label);
            let is_anomaly = anomaly.is_some_and(|s: &AnomalyState| s.alerting);

            let label_style = if is_selected {
                // Selected row: black text on cyan background for high contrast.
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else if e.fading {
                // Smoothly fade Cyan (0,200,200) → DarkGray (80,80,80) over the retention window.
                // fade_t: 0.0 = just disappeared, 1.0 = about to be removed.
                let r = lerp_u8(0, 80, e.fade_t);
                let g = lerp_u8(200, 80, e.fade_t);
                let b = lerp_u8(200, 80, e.fade_t);
                Style::default().fg(Color::Rgb(r, g, b)).add_modifier(Modifier::BOLD)
            } else if is_anomaly {
                // Anomaly: highlight label in LightRed to signal concentration/dropout.
                Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            };

            // Anomaly prefix: "! " prepended to the label when alerting.
            let display_label = if is_anomaly && !e.fading && !is_selected {
                let prefixed = format!("! {}", e.label);
                truncate_label(&prefixed, label_w)
            } else {
                truncate_label(&e.label, label_w)
            };

            let l_str = metric_display_str(e, lm, state.total_ram_bytes);
            let r_str = metric_display_str(e, rm, state.total_ram_bytes);

            // Distribution-heat histogram bins for this group.
            // None  → draw solid █/░ bars ('h' toggled off).
            // Some  → always draw ◻ cells (consistent character set across all rows).
            //         Active + attributable: real Planck heat. All other cases: dark
            //         floor outlines (fading, non-attributable metric, Proxmox mode).
            let hist: Option<Vec<f64>> = if state.show_histogram {
                if overlay_enabled && !fading {
                    Some(
                        state
                            .group_member_vals
                            .get(&e.label)
                            .map(|v| {
                                // One dark frame if the user just switched metric.
                                // If the stored metric doesn't match the current one,
                                // show all-dark until the next histogram sample arrives.
                                if v.metric == hist_metric {
                                    fair_share_bins(&v.vals, bar_w)
                                } else {
                                    vec![0.0; bar_w]
                                }
                            })
                            .unwrap_or_else(|| vec![0.0; bar_w]),
                    )
                } else {
                    // Overlay is on but this row is fading or metric is non-attributable:
                    // still use ◻ characters (consistent character set) but all dark.
                    Some(vec![0.0; bar_w])
                }
            } else {
                None // classic solid █/░ mode
            };

            // ▶▶▶ / ◀◀◀ arrows: cyan fg on the selected row, invisible otherwise.
            // We use the default style (which has no fg set) so the arrows blend into
            // the terminal background on non-selected rows without using spaces.
            let arrow_style = Style::default().fg(Color::Cyan);
            let blank_arrow = Style::default(); // invisible (default terminal fg = bg)
            let (l_arrow, r_arrow, a_style) = if is_selected {
                ("▶▶▶", "◀◀◀", arrow_style)
            } else {
                ("   ", "   ", blank_arrow)
            };

            let mut spans = vec![
                Span::styled(display_label, label_style),
                Span::raw(" "),
                Span::styled(l_arrow, a_style),
                // Right-align the value in VAL_W columns so bar edges stay lined up.
                Span::styled(format!("{:>VAL_W$}", l_str), Style::default().fg(lc)),
            ];

            for x in 0..bar_w {
                let in_left = x < l_filled;
                let in_right = x >= right_start;
                if let Some(ref h) = hist {
                    // Overlay mode: hollow ◻, bg = bar fill, fg = Planck heat.
                    // bg shows bar membership; fg shows work-share heat.
                    let bg = if in_left { lc } else if in_right { rc } else { Color::Reset };
                    spans.push(Span::styled(
                        "◻",
                        Style::default().fg(planck_color(h[x])).bg(bg),
                    ));
                } else {
                    // Classic solid bar: filled cells = █, gap = ░.
                    if in_left {
                        spans.push(Span::styled("█", Style::default().fg(lc)));
                    } else if in_right {
                        spans.push(Span::styled("█", Style::default().fg(rc)));
                    } else {
                        spans.push(Span::styled("░", Style::default().fg(Color::DarkGray)));
                    }
                }
            }

            // Left-align the right value so spaces pad between it and the arrow.
            spans.push(Span::styled(format!("{:<VAL_W$}", r_str), Style::default().fg(rc)));
            spans.push(Span::styled(r_arrow, a_style));

            Line::from(spans)
        })
        .collect();

    let mut all_lines = vec![header_line];
    all_lines.extend(entry_lines);
    frame.render_widget(Paragraph::new(all_lines), inner);
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
fn render_threads(frame: &mut Frame, area: Rect, state: &AppState) {
    let label = match &state.view {
        AppView::Threads { label } => label.clone(),
        _ => return,
    };

    let n = state.thread_samples.len();
    let total_cpu: f64 = state.thread_samples.iter().map(|t| t.cpu_pct).sum();
    // Relative scale: hottest thread = planck_color(1.0) = white.
    let max_cpu = state.thread_samples.iter()
        .map(|t| t.cpu_pct)
        .fold(0.0f64, f64::max)
        .max(1e-6); // guard against dividing by zero for an all-idle group
    let w = area.width as usize;

    // Each cell is "◻ " (2 chars wide) so the number of cells per row is w/2.
    let cells_per_row = (w / 2).max(1);
    const MAX_HEAT_ROWS: usize = 4;
    let max_cells = MAX_HEAT_ROWS * cells_per_row;

    // Find the smallest power-of-2 grouping so all threads fit in max_cells.
    // group_size = 1: one cell per thread. group_size = 2: pairs, etc.
    let mut group_size = 1usize;
    while n > 0 && n.div_ceil(group_size) > max_cells {
        group_size *= 2;
    }

    // Each super-cell = max cpu% of its group (threads already sorted hottest first).
    let num_cells = if n == 0 { 0 } else { n.div_ceil(group_size) };
    let cell_cpus: Vec<f64> = (0..num_cells)
        .map(|i| {
            let start = i * group_size;
            let end = (start + group_size).min(n);
            state.thread_samples[start..end].iter()
                .map(|t| t.cpu_pct)
                .fold(0.0f64, f64::max)
        })
        .collect();

    let heat_rows = if num_cells == 0 { 1 } else {
        num_cells.div_ceil(cells_per_row).clamp(1, MAX_HEAT_ROWS)
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),             // info line
            Constraint::Length(heat_rows as u16), // heat grid
            Constraint::Length(1),             // divider
            Constraint::Min(0),                // thread list
        ])
        .split(area);

    // Info line: group name + thread count + total CPU% + grouping note if applicable.
    let group_info = if group_size > 1 {
        format!("  │  {} threads/cell", group_size)
    } else {
        String::new()
    };
    frame.render_widget(
        Paragraph::new(format!(
            " {label}  │  {n} threads  │  total {total_cpu:.2}%{group_info}"
        ))
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    // Heat-map grid: row-major, hottest threads in the top-left.
    if state.thread_samples.is_empty() {
        frame.render_widget(
            Paragraph::new("  waiting for second sample…")
                .style(Style::default().fg(Color::DarkGray)),
            chunks[1],
        );
    } else {
        let mut hm_lines: Vec<Line> = Vec::new();
        let mut row_spans: Vec<Span<'static>> = Vec::new();
        for (idx, &cpu) in cell_cpus.iter().enumerate() {
            row_spans.push(Span::styled(
                "◻ ",
                Style::default().fg(planck_color(cpu / max_cpu)),
            ));
            // Wrap to a new row every `cells_per_row` cells.
            if (idx + 1) % cells_per_row == 0 {
                hm_lines.push(Line::from(std::mem::take(&mut row_spans)));
            }
        }
        // Flush any partial row.
        if !row_spans.is_empty() {
            hm_lines.push(Line::from(row_spans));
        }
        frame.render_widget(Paragraph::new(hm_lines), chunks[1]);
    }

    // Horizontal divider between heat grid and thread list.
    frame.render_widget(
        Paragraph::new("─".repeat(w)).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );

    // Thread list: one row per thread, sorted hottest first (same order as heatmap).
    let legend_h = chunks[3].height as usize;
    let name_w = state
        .thread_samples
        .iter()
        .map(|t| t.name.len())
        .max()
        .unwrap_or(10)
        .clamp(8, 24);

    let mut legend_lines: Vec<Line> = vec![Line::from(Span::styled(
        format!("  {:<name_w$}  {:>6}  pid:tid", "thread", "cpu%"),
        Style::default().fg(Color::DarkGray),
    ))];

    for t in state.thread_samples.iter().take(legend_h.saturating_sub(1)) {
        legend_lines.push(Line::from(vec![
            // Colour swatch matching the heat-map cell for this thread.
            Span::styled(
                "◻ ",
                Style::default().fg(planck_color(t.cpu_pct / max_cpu)),
            ),
            Span::styled(
                format!(
                    "{:<name_w$}  {:>5.2}%  {}:{}",
                    t.name, t.cpu_pct, t.pid, t.tid
                ),
                Style::default().fg(Color::Gray),
            ),
        ]));
    }

    frame.render_widget(Paragraph::new(legend_lines), chunks[3]);
}

/// Render the "Connecting to <label>…" splash shown while `connect_vm` is blocking.
///
/// This is drawn synchronously with `terminal.draw()` before the SSH call starts,
/// so the user sees feedback immediately rather than a frozen screen.
fn render_connecting(frame: &mut Frame, area: Rect, label: &str) {
    let lines = vec![
        Line::default(),
        Line::from(vec![
            Span::styled("  Connecting to ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                label.to_string(),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " — trying guest-agent, DNS, hostname…",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(Span::styled(
            "  This may take a few seconds. [Esc] to cancel.",
            Style::default().fg(Color::Rgb(80, 80, 80)),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), area);
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
        AppMode::Local                  => "local /proc".to_string(),
        AppMode::Proxmox { url, .. }    => format!("proxmox {url}"),
        AppMode::Fleet { .. }           => "fleet".to_string(),
        AppMode::Kube { namespace, .. } => format!("kube/{namespace}"),
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

/// Render `parts` into styled `Line`s, greedy-wrapping at `width`.
fn render_packed_parts(parts: &[String], width: usize) -> Vec<Line<'static>> {
    const SEP: &str = "  │  ";
    let dim    = Style::default().fg(Color::DarkGray);
    let sep_st = Style::default().fg(Color::Rgb(50, 50, 50));
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>>  = Vec::new();
    let mut col = 0usize;

    for (i, p) in parts.iter().enumerate() {
        if i == 0 {
            spans.push(Span::styled(" ".to_string(), dim));
            spans.push(Span::styled(p.clone(), dim));
            col = 1 + p.len();
        } else if col + SEP.len() + p.len() > width {
            lines.push(Line::from(std::mem::take(&mut spans)));
            spans.push(Span::styled(" ".to_string(), dim));
            spans.push(Span::styled(p.clone(), dim));
            col = 1 + p.len();
        } else {
            spans.push(Span::styled(SEP.to_string(), sep_st));
            spans.push(Span::styled(p.clone(), dim));
            col += SEP.len() + p.len();
        }
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

/// Build the GPU device selector line, shown instead of key hints when devices are present.
fn build_gpu_selector_line(state: &AppState) -> Option<Line<'static>> {
    if !state.gpu_enabled || state.gpu_devices.is_empty() {
        return None;
    }
    let mut spans = vec![Span::styled(" GPU [/]: ".to_string(), Style::default().fg(Color::DarkGray))];
    if state.selected_gpu == 0 {
        spans.push(Span::styled("[all]".to_string(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
    } else {
        spans.push(Span::styled("all".to_string(), Style::default().fg(Color::DarkGray)));
    }
    for (i, dev) in state.gpu_devices.iter().enumerate() {
        spans.push(Span::styled("  ".to_string(), Style::default()));
        let label = format!("{}:{}", dev.driver, dev.pci_addr);
        if state.selected_gpu == i + 1 {
            spans.push(Span::styled(format!("[{label}]"), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
        } else {
            spans.push(Span::styled(label, Style::default().fg(Color::DarkGray)));
        }
    }
    Some(Line::from(spans))
}

/// Render the Groups-view footer: parts-based status rows + key hints (or GPU selector).
fn render_footer_groups(frame: &mut Frame, area: Rect, state: &AppState) {
    let w = area.width as usize;
    let mut lines = render_packed_parts(&groups_status_parts(state), w);

    let keys_style = if state.history_cursor.is_some() {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Rgb(60, 60, 60))
    };

    if let Some(gl) = build_gpu_selector_line(state) {
        lines.push(gl);
    } else if let Some(cursor) = state.history_cursor {
        let age   = state.history.get(cursor).map(|h| h.at.elapsed().as_secs()).unwrap_or(0);
        let total = state.history.len();
        lines.push(Line::styled(
            format!(" PAUSED  ◀ {age}s ago ▶  sample {}/{total}  [←/→] scrub  [p] resume  [q] quit",
                cursor + 1),
            keys_style,
        ));
    } else {
        let enter_part = if matches!(state.mode, AppMode::Local) {
            "  [Enter] threads"
        } else if matches!(state.mode, AppMode::Fleet { .. } | AppMode::Kube { .. }) || state.enable_remote {
            "  [Enter] drill down"
        } else {
            ""
        };
        lines.push(Line::styled(
            format!(" [←/→] metric  [Tab] side  [s] sort  [h] hist  [g] group-by  [↑/↓] cursor{enter_part}  [p] pause  [r] refresh  [m] manual  [q] quit"),
            keys_style,
        ));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// Render the footer: status line(s) + key hints.
///
/// The Groups view uses a parts-based variable-height layout (see
/// `render_footer_groups`). All other views use a fixed 2-row layout.
fn render_footer(frame: &mut Frame, area: Rect, state: &AppState) {
    if matches!(state.view, AppView::Groups) {
        render_footer_groups(frame, area, state);
        return;
    }
    let mode_label = match &state.mode {
        AppMode::Local => "local /proc".to_string(),
        AppMode::Proxmox { url, .. } => format!("proxmox {url}"),
        AppMode::Fleet { .. } => "fleet".to_string(),
        AppMode::Kube { namespace, .. } => format!("kube/{namespace}"),
    };
    let interval_s = state.interval.as_secs_f64();
    // Format the interval: "0.50" for sub-second, "2" for whole seconds, "1.5" for fractions.
    let interval_str = if interval_s < 1.0 {
        format!("{interval_s:.2}")
    } else if interval_s.fract() == 0.0 {
        format!("{}", interval_s as u64)
    } else {
        format!("{interval_s:.1}")
    };

    // Build the system metrics string (net, GPU, RAPL total, PSI) for the footer.
    let sys_metrics = |state: &AppState| -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "  │  net ↓{}  ↑{}",
            fmt_bytes_rate(state.sys_net_rx_s),
            fmt_bytes_rate(state.sys_net_tx_s),
        ));
        if let Some(gpu) = state.sys_gpu_pct {
            s.push_str(&format!("  │  gpu {:.0}%", gpu));
        }
        if state.sys_rapl_w > 0.0 {
            s.push_str(&format!("  │  {} total", fmt_watts(state.sys_rapl_w)));
        }
        // Show system PSI only when at least one value is available.
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
            let host = state
                .remote_client
                .as_ref()
                .map(|c| c.host.as_str())
                .unwrap_or("?");
            let sys = sys_metrics(state);
            let s = format!(
                " remote: {label} ({host})  │  {} processes{sys}",
                state.entries.len()
            );
            let k = if let Some(cursor) = state.history_cursor {
                let age = state.history.get(cursor).map(|h| h.at.elapsed().as_secs()).unwrap_or(0);
                let total = state.history.len();
                format!(
                    " PAUSED  ◀ {}s ago ▶  sample {}/{}  [←/→] scrub  [p] resume  [Esc] disconnect  [q] quit",
                    age, cursor + 1, total
                )
            } else {
                " [←/→] metric  [Tab] side  [s] sort  [↑/↓] cursor  [p] pause  [Esc] disconnect  [q] quit".to_string()
            };
            (s, k)
        }
        AppView::Groups => unreachable!("handled by render_footer_groups"),
    };
    // GPU device selector for Remote view (Groups view handles this in render_footer_groups).
    let gpu_device_line = if matches!(state.view, AppView::Remote { .. }) {
        build_gpu_selector_line(state)
    } else {
        None
    };

    let keys_style = if state.history_cursor.is_some() {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Rgb(60, 60, 60))
    };

    let mut lines = vec![
        Line::styled(status, Style::default().fg(Color::DarkGray)),
        Line::styled(keys, keys_style),
    ];
    if let Some(gpu_line) = gpu_device_line {
        // Replace the last line with the GPU selector (footer is only 2 rows in layout,
        // so we insert it as the second line and drop keys to the combined line)
        // Actually, we only have 2 rows — show GPU line instead of keys when devices present,
        // or append it to the status line.
        // Since footer is 2 rows, put gpu line as line 2 (replacing key hints) when devices present.
        lines[1] = gpu_line;
    }
    frame.render_widget(Paragraph::new(lines), area);
}

// ── Manual ────────────────────────────────────────────────────────────────────

/// Render the scrollable in-app manual page.
///
/// Clamps `manual_scroll` to the range [0, max_scroll] so the page never
/// scrolls past the last line. Ratatui's `Paragraph::scroll` takes a (row, col)
/// offset, so we pass `(scroll as u16, 0)`.
fn render_manual(frame: &mut Frame, area: Rect, state: &AppState) {
    let lines = manual_lines();
    let max_scroll = lines.len().saturating_sub(area.height as usize);
    let scroll = state.manual_scroll.min(max_scroll) as u16;
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), area);
}

/// Build the static manual page content as a `Vec<Line>`.
///
/// Helper closures keep the formatting concise:
/// - `h(s)` → bold cyan heading
/// - `b(s)` → gray body text
/// - `d(s)` → dark gray (dimmed) annotation/example
/// - `kv(k, v)` → yellow key + gray description
/// - `blank()` → empty line
fn manual_lines() -> Vec<Line<'static>> {
    fn h(s: &'static str) -> Line<'static> {
        Line::from(Span::styled(
            s,
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
    }
    fn b(s: &'static str) -> Line<'static> {
        Line::from(Span::styled(s, Style::default().fg(Color::Gray)))
    }
    fn d(s: &'static str) -> Line<'static> {
        Line::from(Span::styled(s, Style::default().fg(Color::DarkGray)))
    }
    fn kv(k: &'static str, v: &'static str) -> Line<'static> {
        Line::from(vec![
            Span::styled(k, Style::default().fg(Color::Yellow)),
            Span::styled(v, Style::default().fg(Color::Gray)),
        ])
    }
    fn blank() -> Line<'static> {
        Line::from("")
    }

    vec![
        h("  apptop  ·  real-time process-group activity monitor"),
        blank(),
        // ── Overview ──────────────────────────────────────────────────────────
        h("OVERVIEW"),
        b("  Reads /proc and groups all processes by name (default), cgroup, or exe."),
        b("  Two metrics are shown simultaneously as a split meter bar.  The left"),
        b("  bar grows toward the centre; the right bar grows inward from the far"),
        b("  edge.  Both are capped at the midpoint so they never cross."),
        blank(),
        b("  Eleven per-process metrics are available.  System-wide network, GPU,"),
        b("  and power totals appear in the footer."),
        blank(),
        b("  Local mode (default)  reads /proc on this machine."),
        b("  Proxmox mode (--proxmox URL --token T)  polls the PVE REST API and"),
        b("  shows per-VM CPU and memory."),
        blank(),
        b("  When running without root privileges, metrics that require reading other"),
        b("  users' /proc files (disk I/O, context switches, FDs, swap, RSS) will be"),
        b("  marked with a trailing '?' and bars will be dimmed.  Rerun as root for"),
        b("  complete data."),
        blank(),
        // ── Navigation ────────────────────────────────────────────────────────
        h("NAVIGATION"),
        kv("  [↑] [↓]  or  [j] [k]    ", "move cursor up / down through the group list"),
        kv("  [Enter]                  ", "open thread heatmap for the selected group"),
        kv("  [Esc]                    ", "return from thread view or manual to group list"),
        kv("  [q]  [Ctrl-C]            ", "quit"),
        blank(),
        // ── Display controls ──────────────────────────────────────────────────
        h("DISPLAY CONTROLS"),
        kv("  [Tab]                    ", "switch active side  (left ↔ right)"),
        d("                             active side is highlighted in yellow in the header"),
        kv("  [← / →]                 ", "cycle the metric shown on the active side"),
        kv("  [s]                      ", "re-sort list by the current active-side metric"),
        kv("  [h]                      ", "toggle distribution histogram overlay"),
        kv("  [g]                      ", "cycle grouping: comm→cgroup→exe (local) or flat→pool→tag→node (Proxmox)"),
        d("                             current grouping shown in brackets in the header"),
        kv("  [r]                      ", "force an immediate data refresh"),
        kv("  [m]                      ", "toggle this manual  (↑/↓ line, PgUp/PgDn page)"),
        blank(),
        // ── Meter bar ─────────────────────────────────────────────────────────
        h("THE METER BAR"),
        b("  label  ▶▶▶  left-value  [══left bar══  gap  ══right bar══]  right-value  ◀◀◀"),
        blank(),
        kv("  ▶▶▶ / ◀◀◀  ", "cyan cursor brackets on the selected row.  They bracket the"),
        d("               whole line so you can track one row across the full width."),
        kv("  Left bar    ", "grows L→R from the label side, capped at the bar midpoint."),
        kv("  Right bar   ", "grows R→L from the far edge, capped at the bar midpoint."),
        kv("  Gap         ", "at least 2 dim cells always separate the two bars at centre."),
        blank(),
        // ── Metrics ───────────────────────────────────────────────────────────
        h("METRICS  (per process group)"),
        blank(),
        b("  cpu% mach CPU time consumed by this group, normalised to the whole machine."),
        b("             100% = every CPU core fully saturated (all threads combined)."),
        b("             Note: this differs from top(1), which shows per-core percent"),
        b("             (a process pinned to one core reads 100% in apptop, but 25% in"),
        b("             top(1) on a 4-core machine).  apptop's convention makes groups"),
        b("             comparable regardless of thread count."),
        d("             Linear scale  0–100% of whole-machine CPU."),
        blank(),
        b("  mem       Resident set size — RAM pages physically in use (not virtual memory)."),
        b("             Bar length = group RSS / total physical RAM (linear scale)."),
        d("             Linear scale, fraction of physical RAM."),
        blank(),
        b("  disk-r    Bytes read from storage devices / second  (/proc/<pid>/io)."),
        b("  disk-w    Bytes written to storage devices / second."),
        d("             Log scale, relative to the current busiest group."),
        blank(),
        b("  faults/s  Page faults / second.  A fault happens when the process touches a"),
        b("             virtual memory page not currently in RAM — typical during large"),
        b("             allocations, first access of memory-mapped files, or copy-on-write."),
        d("             Log scale, relative to the current busiest group."),
        blank(),
        b("  ctx-sw    Context switches / second (voluntary + involuntary combined)."),
        b("             Voluntary:   the thread blocked on I/O, sleep, a mutex, or channel."),
        b("             Involuntary: the scheduler preempted the thread (CPU contention)."),
        d("             Log scale, relative to the current busiest group."),
        blank(),
        b("  runq      Scheduler wait % — fraction of time threads are ready to run but"),
        b("             waiting for a free CPU core.  High values indicate CPU saturation:"),
        b("             the threads want to run but cannot."),
        d("             Linear scale  0–100%."),
        blank(),
        b("  fds       Open file descriptor count (sockets + pipes + files).  High or"),
        b("             steadily growing counts can reveal connection or handle leaks."),
        d("             Log scale, relative to the current busiest group."),
        blank(),
        b("  swap      Bytes of swap space in use  (VmSwap from /proc/<pid>/status)."),
        d("             Log scale, relative to the current busiest group."),
        blank(),
        b("  threads   Total thread count for the process group."),
        d("             Log scale, relative to the current busiest group."),
        blank(),
        b("  power     Estimated watts consumed by this group (shown with ≈ prefix):"),
        b("               (group CPU%) / (total CPU%)  ×  RAPL package watts"),
        b("             This is a CPU-proportional split of measured package power — not a"),
        b("             per-process measurement.  Requires Intel or AMD RAPL support in the"),
        b("             kernel.  Shows 0 if RAPL is unavailable.  Bar is anchored to the"),
        b("             measured package watts."),
        d("             Log scale, relative to the current busiest group."),
        blank(),
        // ── cgroup v2 metrics ─────────────────────────────────────────────────
        h("CGROUP v2 METRICS  (group-by cgroup mode only, requires cgroup v2)"),
        b("  These metrics are only available when:"),
        b("    • the kernel uses cgroup v2 (/sys/fs/cgroup/cgroup.controllers exists)"),
        b("    • group-by is set to 'cgroup' ([g] in local mode)"),
        b("  When unavailable, the value shows '?' and the bar is dimmed."),
        blank(),
        b("  throttle  CFS CPU bandwidth throttle %.  When a cgroup has a CPU quota set"),
        b("             (cpu.max), the scheduler throttles it once the quota is exhausted."),
        b("             Calculated as: nr_throttled / nr_periods × 100 from cgroup cpu.stat."),
        b("             A non-zero value means the cgroup is consistently hitting its limit."),
        b("             Linear scale 0–100%."),
        blank(),
        b("  psi-cpu   CPU Pressure Stall Information — 'some avg10' from cpu.pressure."),
        b("  psi-mem   Memory PSI — 'some avg10' from memory.pressure."),
        b("  psi-io    I/O PSI — 'some avg10' from io.pressure."),
        b("             'some avg10' = % of time at least one task in the cgroup was"),
        b("             stalled waiting for that resource, averaged over the last 10 s."),
        b("             Linear scale 0–100%.  System-wide PSI appears in the footer."),
        d("             Per-cgroup PSI also requires that the kernel compiled with CONFIG_PSI."),
        blank(),
        b("  cgroup v2 also provides more accurate disk I/O accounting (io.stat) and"),
        b("  RSS memory (memory.current) when group-by=cgroup.  These replace the"),
        b("  /proc/PID/io and /proc/PID/statm values which are per-process and"),
        b("  can miss I/O done by short-lived subprocesses."),
        blank(),
        // ── System metrics ────────────────────────────────────────────────────
        h("SYSTEM METRICS  (footer only — not per-process)"),
        kv("  net ↓/↑  ", "system-wide network receive / transmit  bytes / second"),
        kv("  gpu      ", "total GPU utilisation %  (DRM sysfs; shown when GPU detected)"),
        d("             reads /sys/class/drm/card*/device/gpu_busy_percent"),
        kv("  total    ", "system-wide RAPL power draw in watts  (Intel / AMD only)"),
        kv("  psi      ", "system-wide CPU/memory/I/O pressure from /proc/pressure/{cpu,memory,io}"),
        d("             shown as 'psi cpu:N.N% mem:N.N% io:N.N%' when PSI is available"),
        blank(),
        // ── Incomplete data ───────────────────────────────────────────────────
        h("INCOMPLETE DATA  (running without root)"),
        b("  Metrics that require reading /proc/<pid>/io, /status, or /fd for processes"),
        b("  owned by other users will be denied with EACCES unless you run as root."),
        b("  When denied, the displayed value shows only the processes you own (a lower"),
        b("  bound), and the value is marked with a trailing '?' and the bar is dimmed."),
        blank(),
        b("  Affected metrics: disk-r, disk-w, ctx-sw, swap, fds, runq, mem"),
        b("  Unaffected: cpu% mach (reads /proc/<pid>/stat, world-readable)"),
        blank(),
        // ── Log scale ─────────────────────────────────────────────────────────
        h("LOG SCALE"),
        b("  Most metrics span many orders of magnitude: a quiet process may generate"),
        b("  1 fault/s; a loaded one may generate 100 000.  On a linear scale everything"),
        b("  except the current maximum would appear as zero."),
        blank(),
        b("    bar length  =  log₂(value + 1)  /  log₂(peak + 1)"),
        blank(),
        b("  Adding 1 before the log makes zero map to zero (avoids log(0) = −∞)."),
        b("  The peak is the busiest non-fading group for that metric, smoothed with a"),
        b("  0.95 decay so the scale shrinks gradually as activity drops."),
        b("  The busiest group always fills ~100% of its half-bar."),
        blank(),
        // ── Distribution histogram ────────────────────────────────────────────
        h("DISTRIBUTION HISTOGRAM  [h]"),
        b("  Pressing [h] overlays a work-density histogram on the bar cells."),
        b("  Cells switch from solid █ to hollow ◻.  Each ◻ carries two layers:"),
        blank(),
        kv("    Background  ", "bar fill colour (left metric, right metric, or transparent)"),
        kv("    Foreground  ", "work density: blackbody heat from dark (none) to white (all)"),
        d("                  how much of the group's total work falls in this load bucket"),
        blank(),
        b("  The x-axis encodes 'relative work share per thread' on a log₂ scale:"),
        blank(),
        kv("    Left edge    ", "threads contributing zero work"),
        kv("    Pivot ≈ 1/3   ", "all N threads share work exactly equally — the balanced point"),
        kv("    Right edge   ", "one thread carries the entire load"),
        blank(),
        b("  Reading the histogram:"),
        kv("    Heat near the pivot      ", "work is evenly distributed  (good parallelism)"),
        kv("    Heat right of the pivot  ", "one or a few threads dominate — serial bottleneck,"),
        d("                               hot lock, or single-threaded workload"),
        kv("    Dark / all heat left     ", "threads mostly idle; total activity is very low"),
        blank(),
        b("  Why the pivot sits at exactly 1/3:"),
        b("  The axis runs from l_min = −(p/(1−p))·log₂N to l_max = log₂N.  At the pivot"),
        b("  r = 1  →  log₂(r) = 0, so its fractional position is:"),
        b("    t = (0 − l_min) / (l_max − l_min) = (p/(1−p)) / (1/(1−p)) = p = 1/3"),
        b("  The constant HIST_PIVOT_FRAC is literally the pivot position — not an input"),
        b("  that produces it by coincidence.  Unequal loads push heat rightward."),
        blank(),
        d("  For N = 1 the histogram is suppressed (all dark ◻) — a distribution"),
        d("  of one thread carries no useful information."),
        d("  The histogram is sampled at most once per second to limit /proc overhead."),
        blank(),
        // ── Thread heatmap ────────────────────────────────────────────────────
        h("THREAD HEATMAP  [Enter]"),
        b("  Pressing Enter drills into the selected group and shows a per-thread grid."),
        blank(),
        b("  Each ◻ cell represents one thread.  Colour = that thread's CPU% relative"),
        b("  to the hottest thread in the group (blackbody scale, same as histogram)."),
        b("  Threads are sorted hottest-first; the top-left cell is always the busiest."),
        blank(),
        b("  When there are too many threads to fit in 4 rows, they are combined in"),
        b("  power-of-2 batches (2, 4, 8, …) — the smallest factor that still fits in"),
        b("  4 rows.  Each cell then shows the maximum CPU% in its group.  The info"),
        b("  line at the top shows 'N threads/cell' when grouping is active."),
        blank(),
        b("  Below the heat grid, threads are listed individually with name, TID,"),
        b("  and CPU%.  Press [Esc] to return to the group list."),
        blank(),
        // ── Fading rows ───────────────────────────────────────────────────────
        h("FADING ROWS"),
        b("  When a process group stops being active, its row stays visible for 5"),
        b("  seconds, fading gradually from cyan to dark grey.  Rate-based metrics"),
        b("  (CPU, disk I/O, page faults, context switches, power) are zeroed during"),
        b("  the fade.  Static metrics (fds, swap, threads) retain their last value."),
        b("  The row is removed once the 5-second retention window expires."),
        blank(),
        // ── Fleet mode ────────────────────────────────────────────────────────
        h("FLEET MODE  (--enable-remote --hosts h1,h2,h3)"),
        b("  Monitor multiple SSH hosts simultaneously. Each host appears as one row;"),
        b("  the bar shows the host's system-wide CPU% and memory."),
        blank(),
        b("  The distribution-heat overlay ([h]) on any host row shows the process"),
        b("  distribution within that host — which processes are driving load."),
        blank(),
        b("  [Enter] drills into the selected host's per-process view (daemon mode only)."),
        blank(),
        kv("  --hosts h1,h2,h3  ", "comma-separated list of hostnames or IPs"),
        kv("  --hosts @/file    ", "read one host per line from a file (# = comment)"),
        kv("  --thin            ", "use a minimal /proc shell probe instead of apptop --daemon;"),
        d("                       provides CPU% and memory only; no drill-down"),
        blank(),
        b("  The disk-r / disk-w metrics show system-wide network rx/tx bytes/s in fleet mode."),
        blank(),
        b("  Hosts must be accessible via SSH. Key auth required (BatchMode=yes)."),
        b("  Use --ssh-accept-new for first-time TOFU connection; always validates host keys."),
        b("  For daemon mode, apptop must be installed and in PATH on each host."),
        blank(),
        // ── Kubernetes ────────────────────────────────────────────────────────
        h("KUBERNETES (EXPERIMENTAL)  (--kube NAMESPACE[/SELECTOR])"),
        b("  Monitor all pods in a Kubernetes namespace via kubectl exec."),
        b("  Each pod appears as one row; the bar shows system-wide CPU% and memory."),
        blank(),
        kv("  --kube NAMESPACE           ", "monitor all pods in a namespace"),
        kv("  --kube NAMESPACE/SELECTOR  ", "filter by label selector (e.g. app=nginx)"),
        kv("  --kube-context CTX         ", "kubectl context from kubeconfig"),
        kv("  --kube-thin                ", "thin /proc probe (no apptop binary required in image)"),
        blank(),
        b("  One row per pod, ordered by the active sort metric. The histogram overlay"),
        b("  groups pods by their 'app' / 'app.kubernetes.io/name' label and shows"),
        b("  load distribution across replicas of the same Deployment — the original"),
        b("  consul-pool fairness question, answered for Kubernetes."),
        blank(),
        kv("  [Enter]  ", "drill into the selected pod (daemon mode only, not --kube-thin)"),
        kv("  [Esc]    ", "exit drill-down back to the pod list"),
        blank(),
        b("  Requirements:"),
        b("    • kubectl in PATH with RBAC permission to exec into pods"),
        b("    • For daemon mode: apptop binary in the container image (same arch)"),
        b("    • Host-network or /proc visibility inside the container for thin mode"),
        blank(),
        b("  Pod list is discovered once at startup; press [r] to re-query."),
        blank(),
        // ── GroupBy ───────────────────────────────────────────────────────────
        h("GROUPING STRATEGY  [g]"),
        b("  Press [g] to cycle grouping.  The active strategy is shown in [brackets] in the header."),
        blank(),
        b("  LOCAL MODE:"),
        kv("  comm    ", "Process name from /proc/<pid>/stat (default).  Best for"),
        d("            identifying individual applications and daemons."),
        kv("  cgroup  ", "Last meaningful component of /proc/<pid>/cgroup path, with"),
        d("            .service/.scope suffixes stripped.  Groups systemd units together."),
        kv("  exe     ", "Basename of /proc/<pid>/exe symlink.  Groups by the actual"),
        d("            binary, useful when the same binary runs under different names."),
        blank(),
        b("  PROXMOX MODE:"),
        kv("  flat    ", "One row per VM/CT — the default, same as before."),
        kv("  pool    ", "Group VMs/CTs by their Proxmox pool.  VMs without a pool"),
        d("            appear as '(no pool)'.  Useful for workload-oriented views."),
        kv("  tag     ", "Group by each VM's first tag (semicolon-delimited).  VMs"),
        d("            with no tags appear as '(untagged)'."),
        kv("  node    ", "Group all VMs/CTs running on the same Proxmox node."),
        d("            Gives a host-rollup: how much of each hypervisor is consumed."),
        blank(),
        b("  In grouped Proxmox mode, the bar shows aggregated CPU/mem/disk for the group."),
        b("  The fair-share overlay shows how load is distributed among VMs in each group."),
        b("  Node/storage status is always shown in the footer in Proxmox mode."),
        blank(),
        b("  Switching strategy clears the current snapshot and resets the stable order."),
        blank(),
        // ── Replay / Scrub ────────────────────────────────────────────────────
        h("REPLAY / SCRUB"),
        blank(),
        kv("  [p]       ", "toggle pause — freezes the display on the current snapshot"),
        kv("  [← →]     ", "while paused: scrub backward/forward through buffered history"),
        kv("  [p]        ", "again to resume live mode"),
        blank(),
        b("  apptop keeps the last N snapshots in memory (--history-depth N, default 120)."),
        b("  At the default 2 s interval this is ~4 minutes of history."),
        b("  While paused the footer shows how far back in time the current view is."),
        b("  Metric cycling (← →) is suspended while paused; resume first, then cycle."),
        blank(),
        // ── Anomaly alerts ────────────────────────────────────────────────────
        h("ANOMALY ALERTS"),
        blank(),
        b("  apptop watches the load-distribution shape within each group using the"),
        b("  Herfindahl N_eff concentration measure:"),
        blank(),
        b("    N_eff = (Σv)² / Σ(v²)   — effective-participant count"),
        blank(),
        b("  N_eff = N  →  all members share the load equally  (balanced)"),
        b("  N_eff = 1  →  one member carries everything        (concentrated)"),
        blank(),
        b("  Two anomaly conditions are flagged (row label turns red, \"! \" prefix):"),
        blank(),
        kv("    concentrated   ", "N_eff/N < 0.35 with ≥ 5 members — most load on one member"),
        kv("    dropout        ", "a member that was active (>15% share) dropped to near-zero (<3%)"),
        d("                    The same member (by position) must drop — not any active + any idle."),
        blank(),
        b("  Optional alert hook (--alert-cmd CMD):"),
        b("    CMD is called as:  CMD GROUP_LABEL ANOMALY_KIND BALANCE_FRACTION"),
        d("    e.g.  alert.sh \"nginx\" \"concentrated\" \"0.18\""),
        b("    Rate-limited to once per 60 s per group. CMD is split on whitespace;"),
        b("    no shell expansion. All stdio is suppressed (fire-and-forget)."),
        b("    Gate explicitly: the hook fires only when --alert-cmd is provided."),
        blank(),
        blank(),
        // ── GPU metrics ───────────────────────────────────────────────────────
        h("GPU METRICS  (--enable-gpu, AMD/Intel DRM + NVIDIA via nvidia-smi)"),
        b("  GPU metrics are disabled by default to avoid reading /proc/PID/fdinfo for every"),
        b("  process.  Pass --enable-gpu to enable them.  Without this flag, gpu% and vram"),
        b("  always show 0.0; the bar is not dimmed and no '?' is appended — the value is"),
        b("  accurate (opt-in, not a permission failure)."),
        blank(),
        b("  gpu%      GPU engine time % for this process group.  Computed as:"),
        b("              Δ(drm-engine-* nanoseconds) / elapsed_wall_clock_ns × 100"),
        b("            Summed across all GPU engines (gfx, compute, enc, dec) and all"),
        b("            DRM file descriptors held by PIDs in the group."),
        b("            Can exceed 100% when multiple GPU engines run simultaneously."),
        b("            For NVIDIA: SM% from nvidia-smi pmon (per-PID)."),
        d("            Linear scale, capped at 1.0 in the bar display."),
        blank(),
        b("  vram      GPU VRAM in use by this group (instantaneous, bytes)."),
        b("            Summed from drm-memory-vram lines across all DRM fds (AMD/Intel)."),
        b("            For NVIDIA: used_gpu_memory from nvidia-smi --query-compute-apps."),
        b("            May slightly over-count when multiple DRM contexts share allocations."),
        d("            Log scale, relative to the current busiest group."),
        blank(),
        b("  Supported drivers:"),
        b("    • AMD amdgpu — DRM fdinfo (kernel ≥ 5.14)"),
        b("    • Intel i915 / xe — DRM fdinfo (kernel ≥ 5.14)"),
        b("    • NVIDIA proprietary — nvidia-smi pmon (nvidia-smi must be in PATH)"),
        b("    • nouveau (NVIDIA open) — DRM fdinfo where supported"),
        blank(),
        b("  Multi-GPU: apptop discovers all GPUs at startup via sysfs.  When multiple"),
        b("  GPUs are present, a device selector appears at the bottom of the screen."),
        blank(),
        kv("  [         ", "cycle GPU selector backward (all → last device → … → first device)"),
        kv("  ]         ", "cycle GPU selector forward  (all → first device → … → last device)"),
        blank(),
        b("  'all' aggregates data from all discovered GPUs (DRM + NVIDIA combined)."),
        b("  Selecting a specific device shows only that device's gpu% and vram."),
        blank(),
        b("  How DRM metrics work: each open DRM file descriptor exposes lines in"),
        b("  /proc/PID/fdinfo/N.  A fd is a DRM fd if 'drm-driver:' appears in the file."),
        b("  The 'drm-pdev:' field identifies the PCI device."),
        b("  drm-engine-* values are cumulative nanoseconds (like CPU jiffies) — the"),
        b("  delta divided by elapsed time gives GPU%.  drm-memory-vram is instantaneous."),
        blank(),
        b("  How NVIDIA metrics work: nvidia-smi pmon -c 1 -s u is called each tick."),
        b("  This requires the nvidia-smi binary in PATH.  If absent, NVIDIA data is"),
        b("  silently omitted (no error shown)."),
        blank(),
        d("  apptop — GPLv3-or-later — Copyright (C) 2026 Epsilon Null Operation — see LICENSE"),
    ]
}

pub fn manual_line_count() -> usize {
    manual_lines().len()
}

pub fn manual_text() -> String {
    let lines = manual_lines();
    let mut out = String::with_capacity(lines.len() * 72);
    for line in lines {
        for span in line.spans {
            out.push_str(&span.content);
        }
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
