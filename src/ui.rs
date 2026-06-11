// SPDX-License-Identifier: GPL-3.0-or-later
use crate::{AppMode, AppState, AppView, BarEntry, Metric, PeakVals, Side};
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(2)])
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

/// Build the metric-selector line shown in the header.
///
/// If there is an active error, shows the first line of the error in red instead
/// of the metric selectors (error takes priority so it is not hidden).
///
/// Otherwise shows: `← left_metric  ·  right_metric →` with the active side
/// highlighted in yellow/bold and the inactive side dimmed. Additional badges
/// (grouping strategy, TLS warning) are appended as needed.
fn metric_selector_line(state: &AppState) -> Line<'static> {
    if let Some(err) = &state.error {
        Line::from(Span::styled(
            format!(" error: {}", err.lines().next().unwrap_or("")),
            Style::default().fg(Color::Red),
        ))
    } else {
        let left_style = if state.active_side == Side::Left {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let right_style = if state.active_side == Side::Right {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let dim_style = Style::default().fg(Color::Rgb(60, 60, 60));
        let mut spans = vec![
            Span::styled(" ← ", Style::default().fg(Color::DarkGray)),
            Span::styled(state.left_metric.name().to_string(), left_style),
            Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
            Span::styled(state.right_metric.name().to_string(), right_style),
            Span::styled(" →", Style::default().fg(Color::DarkGray)),
        ];
        if matches!(state.mode, AppMode::Local) {
            spans.push(Span::styled(
                format!("  [{}]", state.group_by.name()),
                dim_style,
            ));
        }
        if state.proxmox_insecure {
            spans.push(Span::styled(
                "  ⚠ TLS OFF",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
        }
        Line::from(spans)
    }
}

/// Render the 3-row header block, which varies by view.
///
/// - Groups view: metric selector line + histogram colour swatch (when overlay is on).
/// - Remote view: metric selector line + "remote: <label> · [Esc] disconnect".
/// - Connecting: animated-style "Connecting to <label> …" message.
/// - Threads: heat colour swatch from idle to hot.
/// - Manual: title + scroll/close hints.
///
/// The block has a bottom border in DarkGray that visually separates header from body.
fn render_header(frame: &mut Frame, area: Rect, state: &AppState) {
    let (line1, line2) = match &state.view {
        AppView::Groups => {
            let l1 = metric_selector_line(state);
            let l2 = if state.show_histogram {
                // Show a colour swatch of the blackbody ramp so users can read the histogram.
                let mut spans = vec![Span::styled(
                    " ◻ bright = share of work  ·  left = balanced  ·  right = hot  ",
                    Style::default().fg(Color::DarkGray),
                )];
                const SWATCH: usize = 24;
                for i in 0..SWATCH {
                    let frac = i as f64 / (SWATCH - 1) as f64;
                    spans.push(Span::styled("◻", Style::default().fg(planck_color(frac))));
                }
                Line::from(spans)
            } else {
                Line::default()
            };
            (l1, l2)
        }
        AppView::Remote { label } => {
            let l1 = metric_selector_line(state);
            let l2 = Line::from(vec![
                Span::styled(" remote: ", Style::default().fg(Color::DarkGray)),
                Span::styled(label.clone(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::styled(
                    "  ·  [Esc] disconnect",
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            (l1, l2)
        }
        AppView::Connecting { label } => (
            Line::from(vec![
                Span::styled(" Connecting to ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    label.clone(),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::styled("  …", Style::default().fg(Color::DarkGray)),
            ]),
            Line::default(),
        ),
        AppView::Threads { .. } => {
            // Colour swatch showing the blackbody heat scale for the thread heatmap.
            let mut spans =
                vec![Span::styled(" heat: idle ", Style::default().fg(Color::DarkGray))];
            const SWATCH: usize = 24;
            for i in 0..SWATCH {
                let frac = i as f64 / (SWATCH - 1) as f64;
                spans.push(Span::styled("◻", Style::default().fg(planck_color(frac))));
            }
            spans.push(Span::styled(" hot", Style::default().fg(Color::DarkGray)));
            (Line::from(spans), Line::default())
        }
        AppView::Manual => (
            Line::from(vec![
                Span::styled(" manual", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::styled(
                    "  ·  ↑/↓ to scroll  ·  [m] or [Esc] to close",
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            Line::default(),
        ),
    };

    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(Color::DarkGray));
    frame.render_widget(Paragraph::new(vec![line1, line2]).block(block), area);
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
/// CPU and memory use linear scales (0–100% of a core; 0–100% of RAM).
/// All other metrics use `log2_frac` relative to the current rolling peak.
/// The result is clamped to [0, 1] so the bar never overflows its half.
fn metric_frac(e: &BarEntry, m: Metric, total_ram: u64, peaks: &PeakVals) -> f64 {
    match m {
        // CPU: linear 0–100%; value is already a percent.
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

    let is_local = matches!(state.mode, AppMode::Local);
    let lm = state.left_metric;
    let rm = state.right_metric;

    // The histogram tracks the focused side's metric.
    // Memory (per-process, not per-thread) and Threads count are not per-member
    // attributable in local mode, so the overlay is blanked for those metrics.
    let hist_metric = match state.active_side {
        Side::Left => lm,
        Side::Right => rm,
    };
    let overlay_enabled = state.show_histogram
        && is_local
        && matches!(
            hist_metric,
            Metric::Cpu | Metric::PageFaults | Metric::DiskRead | Metric::DiskWrite
                | Metric::CtxSwitches | Metric::SchedWait
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
    // Shows left metric name above left bar edge, right metric name above right bar edge.
    let lead = " ".repeat(label_w + 1 + ARROW_W + VAL_W);
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
    let header_line = Line::from(vec![
        Span::raw(lead),
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
            } else {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
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
                Span::styled(truncate_label(&e.label, label_w), label_style),
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

/// Render the 2-row footer: status line (top) + key hints (bottom).
///
/// The status line content varies by view and includes metrics such as:
/// - Number of visible groups, mode, interval, next refresh countdown.
/// - System-wide network, GPU, and power totals (local/remote mode).
/// - Thread count and total CPU% (thread view).
/// - Remote host and process count (remote view).
///
/// Key hints show only the keys relevant to the current view.
fn render_footer(frame: &mut Frame, area: Rect, state: &AppState) {
    let mode_label = match &state.mode {
        AppMode::Local => "local /proc".to_string(),
        AppMode::Proxmox { url, .. } => format!("proxmox {url}"),
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

    // Build the system metrics string (net, GPU, RAPL total) for the footer.
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
            let k = " [←/→] metric  [Tab] side  [s] sort  [↑/↓] cursor  [Esc] disconnect  [q] quit".to_string();
            (s, k)
        }
        AppView::Groups => {
            let elapsed = state.last_refresh.map_or(state.interval, |t| t.elapsed());
            let next_ms = state.interval.saturating_sub(elapsed).as_millis();
            // Display next refresh countdown: "850ms" for sub-second, "2s" for whole seconds.
            let next_str = if state.interval.as_millis() < 1000 {
                format!("{next_ms}ms")
            } else {
                format!("{}s", next_ms / 1000)
            };
            let hidden = state.total_groups.saturating_sub(state.entries.len());
            let hidden_str = if hidden > 0 {
                format!("  ({hidden} idle hidden)")
            } else {
                String::new()
            };

            // System metrics only in local mode (Proxmox footer would be too wide).
            let sys_parts = if matches!(state.mode, AppMode::Local) {
                sys_metrics(state)
            } else {
                String::new()
            };

            let enter_hint = if matches!(state.mode, AppMode::Local) {
                "[Enter] threads"
            } else if state.enable_remote {
                "[Enter] drill down"
            } else {
                ""
            };

            let unprivileged_note = if state.running_unprivileged {
                "  │  running unprivileged — disk/ctx/fds/swap/rss incomplete for other users' processes; rerun as root for full data"
            } else {
                ""
            };

            let s = format!(
                " {} groups{hidden_str}  │  {mode_label}  │  {interval_str}s  │  next in {next_str}  │  sort:{}{sys_parts}{unprivileged_note}",
                state.entries.len(),
                state.sort_metric.name()
            );
            let enter_part = if enter_hint.is_empty() {
                String::new()
            } else {
                format!("  {enter_hint}")
            };
            let k = format!(" [←/→] metric  [Tab] side  [s] sort  [h] hist  [g] group-by  [↑/↓] cursor{enter_part}  [r] refresh  [m] manual  [q] quit");
            (s, k)
        }
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(status, Style::default().fg(Color::DarkGray)),
            Line::styled(keys, Style::default().fg(Color::Rgb(60, 60, 60))),
        ]),
        area,
    );
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
        kv("  [h]                      ", "toggle thread-distribution histogram overlay"),
        kv("  [g]                      ", "cycle grouping strategy: comm → cgroup → exe → comm"),
        d("                             current grouping shown as [comm]/[cgroup]/[exe] in header"),
        kv("  [r]                      ", "force an immediate data refresh"),
        kv("  [m]                      ", "toggle this manual  (↑/↓ to scroll)"),
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
        b("  cpu%      CPU time / second, all threads summed."),
        b("             100% = one full core pegged; N×100% = all N cores fully saturated."),
        b("             Normalised to the whole machine — 100% means all CPU time is consumed."),
        d("             Linear scale  0–100%."),
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
        // ── System metrics ────────────────────────────────────────────────────
        h("SYSTEM METRICS  (footer only — not per-process)"),
        kv("  net ↓/↑  ", "system-wide network receive / transmit  bytes / second"),
        kv("  gpu      ", "total GPU utilisation %  (DRM sysfs; shown when GPU detected)"),
        d("             reads /sys/class/drm/card*/device/gpu_busy_percent"),
        kv("  total    ", "system-wide RAPL power draw in watts  (Intel / AMD only)"),
        blank(),
        // ── Incomplete data ───────────────────────────────────────────────────
        h("INCOMPLETE DATA  (running without root)"),
        b("  Metrics that require reading /proc/<pid>/io, /status, or /fd for processes"),
        b("  owned by other users will be denied with EACCES unless you run as root."),
        b("  When denied, the displayed value shows only the processes you own (a lower"),
        b("  bound), and the value is marked with a trailing '?' and the bar is dimmed."),
        blank(),
        b("  Affected metrics: disk-r, disk-w, ctx-sw, swap, fds, runq, mem"),
        b("  Unaffected: cpu% (reads /proc/<pid>/stat, world-readable)"),
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
        b("  Pressing [h] overlays a work-distribution histogram on the bar cells."),
        b("  Cells switch from solid █ to hollow ◻.  Each ◻ carries two layers:"),
        blank(),
        kv("    Background  ", "bar fill colour (left metric, right metric, or transparent)"),
        kv("    Foreground  ", "blackbody heat: dark → deep red → orange → yellow → white"),
        d("                  shows how much of the group's total work falls in that bin"),
        blank(),
        b("  The x-axis encodes 'relative work share per thread' on a log₂ scale:"),
        blank(),
        kv("    Left edge    ", "threads contributing zero work"),
        kv("    Pivot ~40%   ", "all N threads share work exactly equally — the balanced point"),
        kv("    Right edge   ", "one thread carries the entire load"),
        blank(),
        b("  Reading the histogram:"),
        kv("    Heat near the pivot      ", "work is evenly distributed  (good parallelism)"),
        kv("    Heat right of the pivot  ", "one or a few threads dominate — serial bottleneck,"),
        d("                               hot lock, or single-threaded workload"),
        kv("    Dark / all heat left     ", "threads mostly idle; total activity is very low"),
        blank(),
        b("  Why the pivot sits at 40%:"),
        b("  The x-axis runs from log₂(r_min) to log₂(r_max), where r = thread_work /"),
        b("  fair_share.  r = 1 means exactly one fair share → pivot at the point where"),
        b("  log₂(1) = 0 falls on the axis.  Unequal work loads push heat rightward."),
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
        // ── GroupBy ───────────────────────────────────────────────────────────
        h("GROUPING STRATEGY  [g]"),
        b("  Press [g] to cycle through three grouping strategies:"),
        blank(),
        kv("  comm    ", "Process name from /proc/<pid>/stat (default).  Best for"),
        d("            identifying individual applications and daemons."),
        kv("  cgroup  ", "Last meaningful component of /proc/<pid>/cgroup path, with"),
        d("            .service/.scope suffixes stripped.  Groups systemd units together."),
        kv("  exe     ", "Basename of /proc/<pid>/exe symlink.  Groups by the actual"),
        d("            binary, useful when the same binary runs under different names."),
        blank(),
        b("  Switching strategy clears the current snapshot and resets the stable order."),
        blank(),
        d("  apptop — GPLv3-or-later — Copyright (C) 2026 Epsilon Null Operation — see LICENSE"),
    ]
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
