# Spine Health Glyphs (Tier-0 PSI triage) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put a coarse per-place health glyph in the Fleet-face spine's leading gutter — calm hosts blank, `◆` amber for elevated pressure, `⚠` red for high pressure — so a glance triages which host to open.

**Architecture:** Two repos. First a general mullion extension `render_tree_row_decorated` (a leading-gutter status glyph in its own style) that census/canopy/shamal can reuse; then aerie derives each place's `HealthTier` from PSI (worst-of cpu/mem/io "some avg10", CPU% fallback) with hysteresis, caches it per place, and passes the glyph decoration to the spine renderer.

**Tech Stack:** Rust (edition 2021). mullion (`../mullion`, path dep, branch will be `outline-row-decoration`); aerie (branch `spine-health-glyphs`). No new dependencies. Reuses mullion `Theme::{warn, error}` for colors.

## Global Constraints

- Edition 2021; MSRV rustc 1.85. **NO new dependencies** in either repo.
- **Additive / non-regressing:** `render_tree_row`'s existing output stays byte-for-byte identical (every current caller — census DIT, aerie's other trees — unchanged). aerie's 95 tests stay green; the classic views and the (just-fixed) fleet primary/detail behavior are untouched.
- **Domain-agnostic** ([[aerie-stay-general]]): the glyph reports *resource pressure (PSI stall)* in neutral behavioral terms only — never "slow host" or any app/role/desktop meaning.
- **Glyph display width MUST be 1** so the fixed 2-col gutter keeps calm and decorated rows column-aligned. `◆` (U+25C6) is width-1. `⚠` uses text-presentation (`⚠\u{FE0E}`); if a target terminal still renders it wide, fall back to `▲` (Task 4 verifies on apollo + milkv).
- **Colors from mullion `Theme` only** (`warn`, `error`) — no new theme fields.
- **Prefer mullion, propose general extensions** ([[prefer-mullion-propose-extensions]]): the decoration API is generic (glyph + style), not health-specific.
- **Verification:** unit tests for pure logic + mullion buffer-inspection tests; final end-to-end via tmux against real hosts ([[aerie-test-hosts]]), including induced load and glyph-width/alignment on both x86 (apollo) and riscv64 (milkv).

## File Structure

- `../mullion/src/outline.rs` — add `RowDecoration` + `render_tree_row_decorated`; refactor `render_tree_row` to share a private inner (Task 1).
- `src/main.rs` (aerie) — `HealthTier` enum + pure `stepped_tier`/`place_health` (Task 2); `AppState.health_tiers` field + `place_signals`/`place_health_tier` + `refresh` update (Task 3).
- `src/ui.rs` (aerie) — spine loop → `render_tree_row_decorated`; `tier_decoration` mapping + glyph consts (Task 4); manual legend (Task 4).

---

### Task 1: mullion — `render_tree_row_decorated` (leading-gutter status glyph)

**Repo:** `../mullion` — create branch `outline-row-decoration` first (`git -C ../mullion checkout -b outline-row-decoration`).

**Files:**
- Modify: `../mullion/src/outline.rs`
- Test: `../mullion/src/outline.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: existing `tree_prefix`, `shape_line`, `render_line`, `elide`, `Buffer::set_string`, `Buffer::fill`, `Cell::new`, `Theme`, `Style`, `TextCtx`.
- Produces: `pub struct RowDecoration<'a> { pub glyph: &'a str, pub style: Style }` and `pub fn render_tree_row_decorated(buf, rect, ancestor_last, is_last, expanded, label, selected, theme, ctx, deco: Option<RowDecoration>)`. `render_tree_row` keeps its exact current signature and output.

- [ ] **Step 1: Write the failing tests.** Add to the `tests` module in `outline.rs`:

```rust
    #[test]
    fn decorated_row_reserves_gutter_and_paints_glyph() {
        let theme = Theme::default();
        let mut term = Terminal::new(TestBackend::new(20, 1)).unwrap();
        term.draw(|buf| {
            render_tree_row_decorated(
                buf, Rect::new(0, 0, 20, 1), &[false], true, Some(false), "users",
                false, &theme, TextCtx::LTR,
                Some(RowDecoration { glyph: "◆", style: theme.warn }));
        }).unwrap();
        let buf = term.backend().buffer();
        let row: String = (0..20).map(|x| buf.get(x, 0).symbol.chars().next().unwrap_or(' ')).collect();
        // Glyph at col 0, one space, then the SAME guide/label as render_tree_row.
        assert!(row.starts_with("◆ │  └─ ▸ users"), "got {row:?}");
        assert_eq!(buf.get(0, 0).symbol, "◆");
        assert_eq!(buf.get(0, 0).style, theme.warn, "glyph keeps its own style");
    }

    #[test]
    fn decorated_calm_row_blank_gutter_but_aligned() {
        let theme = Theme::default();
        let mut term = Terminal::new(TestBackend::new(20, 1)).unwrap();
        term.draw(|buf| {
            render_tree_row_decorated(
                buf, Rect::new(0, 0, 20, 1), &[false], true, Some(false), "users",
                false, &theme, TextCtx::LTR, None);
        }).unwrap();
        let buf = term.backend().buffer();
        let row: String = (0..20).map(|x| buf.get(x, 0).symbol.chars().next().unwrap_or(' ')).collect();
        // No glyph, but guides start at the SAME col 2 as a decorated row (aligned).
        assert!(row.starts_with("  │  └─ ▸ users"), "got {row:?}");
    }

    #[test]
    fn decorated_glyph_survives_selection() {
        let theme = Theme::default();
        let mut term = Terminal::new(TestBackend::new(20, 1)).unwrap();
        term.draw(|buf| {
            render_tree_row_decorated(
                buf, Rect::new(0, 0, 20, 1), &[], true, None, "web01",
                true, &theme, TextCtx::LTR,
                Some(RowDecoration { glyph: "⚠", style: theme.error }));
        }).unwrap();
        let buf = term.backend().buffer();
        assert_eq!(buf.get(0, 0).symbol, "⚠");
        assert_eq!(buf.get(0, 0).style, theme.error, "severity style wins over selection on the glyph");
    }
```

- [ ] **Step 2: Run the tests to confirm they fail.**

Run: `cargo test -p mullion --lib outline 2>&1 | tail -20` (from `../mullion`, or `cargo test --lib outline` inside it)
Expected: FAIL — `render_tree_row_decorated` / `RowDecoration` not found.

- [ ] **Step 3: Implement the extension.** In `outline.rs`, add the struct and the gutter constant near the top of the module (after the imports):

```rust
/// Width of the leading status gutter reserved by [`render_tree_row_decorated`]:
/// one glyph cell + one space. The glyph MUST be display width 1 so decorated
/// and calm rows stay column-aligned.
const DECO_GUTTER_W: u16 = 2;

/// A one-cell status glyph painted in a tree row's leading gutter — a health
/// tier, a replication state, a sync marker. The `style` is the glyph's own
/// (kept even when the row is `selected`), so severity/status colour stays
/// legible on the focused row. Generic on purpose: the app supplies the glyph
/// and colour; mullion only reserves the column and paints it.
pub struct RowDecoration<'a> {
    pub glyph: &'a str,
    pub style: Style,
}
```

Replace the body of `render_tree_row` so it delegates to a shared inner (its signature and behavior are unchanged — no gutter):

```rust
#[allow(clippy::too_many_arguments)]
pub fn render_tree_row(
    buf:           &mut Buffer,
    rect:          Rect,
    ancestor_last: &[bool],
    is_last:       bool,
    expanded:      Option<bool>,
    label:         &str,
    selected:      bool,
    theme:         &Theme,
    ctx:           TextCtx,
) {
    render_tree_row_inner(buf, rect, ancestor_last, is_last, expanded, label,
        selected, theme, ctx, false, None);
}

/// Like [`render_tree_row`], but reserves a fixed leading gutter (glyph + space)
/// before the guides and paints `deco` there in its own [`RowDecoration::style`].
/// `deco == None` leaves the reserved gutter blank, so decorated and calm rows
/// stay column-aligned. The guides + label render in the remaining width exactly
/// as [`render_tree_row`] does. The glyph must be display width 1.
#[allow(clippy::too_many_arguments)]
pub fn render_tree_row_decorated(
    buf:           &mut Buffer,
    rect:          Rect,
    ancestor_last: &[bool],
    is_last:       bool,
    expanded:      Option<bool>,
    label:         &str,
    selected:      bool,
    theme:         &Theme,
    ctx:           TextCtx,
    deco:          Option<RowDecoration>,
) {
    render_tree_row_inner(buf, rect, ancestor_last, is_last, expanded, label,
        selected, theme, ctx, true, deco);
}

#[allow(clippy::too_many_arguments)]
fn render_tree_row_inner(
    buf:           &mut Buffer,
    rect:          Rect,
    ancestor_last: &[bool],
    is_last:       bool,
    expanded:      Option<bool>,
    label:         &str,
    selected:      bool,
    theme:         &Theme,
    ctx:           TextCtx,
    reserve_gutter: bool,
    deco:          Option<RowDecoration>,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    if selected {
        buf.fill(Rect::new(rect.x, rect.y, rect.width, 1), Cell::new(" ", theme.selection));
    }
    let guide_style = if selected { theme.selection } else { theme.text_dim };
    let label_style = if selected { theme.selection } else { theme.text };

    let mut x = rect.x;
    let mut avail = rect.width;
    if reserve_gutter {
        let gw = DECO_GUTTER_W.min(avail);
        if gw >= 1 {
            // Always paint the glyph cell (a space when calm) so no stale glyph
            // from a prior frame lingers, regardless of whether the caller
            // clears the buffer. The glyph keeps its own style; a calm cell uses
            // the row's base style.
            match deco {
                Some(RowDecoration { glyph, style }) => { buf.set_string(x, rect.y, glyph, style); }
                None => { buf.set_string(x, rect.y, " ", label_style); }
            }
        }
        x += gw;
        avail = avail.saturating_sub(gw);
    }
    if avail == 0 {
        return;
    }

    // Guides are box-drawing characters — always LTR.
    let prefix = tree_prefix(ancestor_last, is_last, expanded);
    let pline = shape_line(&prefix, 0, crate::text::BaseDirection::Ltr);
    let pw = render_line(buf, x, rect.y, &pline, avail, guide_style);

    if pw < avail {
        let rem = avail - pw;
        let full = shape_line(label, 0, ctx.base);
        let line = if full.width() <= rem { full } else { elide(label, rem, ctx) };
        render_line(buf, x + pw, rect.y, &line, rem, label_style);
    }
}
```

- [ ] **Step 4: Run tests.**

Run: `cargo test -p mullion --lib 2>&1 | tail -20`
Expected: PASS — the three new tests, plus the existing `row_draws_prefix_then_label` / `more_row_draws_guides_and_ellipsis` still pass (proving `render_tree_row` output is unchanged).

- [ ] **Step 5: Commit (in the mullion repo).**

```bash
git -C ../mullion add src/outline.rs
git -C ../mullion commit -m "feat(outline): render_tree_row_decorated — leading-gutter status glyph

A generic per-row decoration (glyph + own style) in a fixed leading gutter,
kept legible even on the selected row. render_tree_row is now the gutter-less
path through a shared inner, so its output is unchanged. Reusable for status
indicators in DIT/tree views (census/canopy/shamal, aerie fleet health)."
```

---

### Task 2: aerie — `HealthTier` + pure tier logic

**Files:**
- Modify: `src/main.rs` (add enum + two free functions near the other free helpers, e.g. after `stabilize_fleet_entries`)
- Test: `src/main.rs` (`#[cfg(test)] mod tests` — the module that imports `super::{...}` around line 3923)

**Interfaces:**
- Produces: `enum HealthTier { Calm, Warn, Critical }` (Ord: Calm < Warn < Critical); `fn stepped_tier(prev, v, warn_on, warn_off, crit_on, crit_off) -> HealthTier`; `fn place_health(prev, psi_cpu, psi_mem, psi_io, cpu_pct) -> HealthTier`.

- [ ] **Step 1: Write the failing tests.** Add to the test module (extend the `use super::{...}` line to also import `HealthTier, stepped_tier, place_health`):

```rust
    #[test]
    fn tier_ordering() {
        assert!(HealthTier::Calm < HealthTier::Warn);
        assert!(HealthTier::Warn < HealthTier::Critical);
    }

    #[test]
    fn stepped_tier_thresholds_and_hysteresis() {
        use HealthTier::*;
        // Fresh escalation on breach.
        assert_eq!(stepped_tier(Calm, 26.0, 25.0, 15.0, 50.0, 40.0), Warn);
        assert_eq!(stepped_tier(Calm, 55.0, 25.0, 15.0, 50.0, 40.0), Critical);
        // Below warn-on but above warn-off: stays Warn only if already >= Warn.
        assert_eq!(stepped_tier(Warn, 20.0, 25.0, 15.0, 50.0, 40.0), Warn, "sticky down");
        assert_eq!(stepped_tier(Calm, 20.0, 25.0, 15.0, 50.0, 40.0), Calm, "no escalation from calm");
        // Critical sticks until below crit-off, then drops to Warn (not straight to Calm).
        assert_eq!(stepped_tier(Critical, 45.0, 25.0, 15.0, 50.0, 40.0), Critical, "sticky crit");
        assert_eq!(stepped_tier(Critical, 39.0, 25.0, 15.0, 50.0, 40.0), Warn, "crit falls to warn");
        // Full clear below warn-off.
        assert_eq!(stepped_tier(Warn, 14.0, 25.0, 15.0, 50.0, 40.0), Calm);
    }

    #[test]
    fn place_health_prefers_psi_worst_of_three() {
        use HealthTier::*;
        // io PSI is worst → critical, regardless of a benign cpu%.
        assert_eq!(place_health(Calm, Some(2.0), Some(10.0), Some(60.0), Some(3.0)), Critical);
        // All PSI calm → Calm even if cpu% is high (PSI present suppresses fallback).
        assert_eq!(place_health(Calm, Some(1.0), Some(1.0), Some(1.0), Some(99.0)), Calm);
    }

    #[test]
    fn place_health_falls_back_to_cpu_then_calm() {
        use HealthTier::*;
        // No PSI at all → CPU fallback thresholds (>=85 warn, >=97 crit).
        assert_eq!(place_health(Calm, None, None, None, Some(90.0)), Warn);
        assert_eq!(place_health(Calm, None, None, None, Some(98.0)), Critical);
        // Nothing known → Calm (never invent a problem).
        assert_eq!(place_health(Calm, None, None, None, None), Calm);
    }
```

- [ ] **Step 2: Run tests to confirm they fail.**

Run: `cargo test --bin aerie tier 2>&1 | tail -20`
Expected: FAIL — `HealthTier` / `stepped_tier` / `place_health` not found.

- [ ] **Step 3: Implement.** Add near the other free helpers in `main.rs` (e.g. just after `stabilize_fleet_entries`):

```rust
/// Coarse health of a spine place, worst-signal-wins. Ordered so `max` and
/// comparisons work: `Calm < Warn < Critical`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum HealthTier {
    #[default]
    Calm,
    Warn,
    Critical,
}

// PSI "some avg10" thresholds (balanced). On = escalate; Off = clear (hysteresis).
const PSI_WARN_ON: f64 = 25.0;
const PSI_WARN_OFF: f64 = 15.0;
const PSI_CRIT_ON: f64 = 50.0;
const PSI_CRIT_OFF: f64 = 40.0;
// CPU% fallback thresholds (used only when no PSI is available).
const CPU_WARN_ON: f64 = 85.0;
const CPU_WARN_OFF: f64 = 78.0;
const CPU_CRIT_ON: f64 = 97.0;
const CPU_CRIT_OFF: f64 = 93.0;

/// Map one signal value `v` through on/off thresholds with hysteresis against
/// `prev`: escalate immediately when `v` crosses an *on* threshold, but only
/// de-escalate once `v` falls below the (lower) *off* threshold — so a value
/// hovering at a boundary does not strobe the tier.
fn stepped_tier(prev: HealthTier, v: f64,
    warn_on: f64, warn_off: f64, crit_on: f64, crit_off: f64) -> HealthTier {
    if v >= crit_on {
        HealthTier::Critical
    } else if v >= crit_off && prev == HealthTier::Critical {
        HealthTier::Critical
    } else if v >= warn_on {
        HealthTier::Warn
    } else if v >= warn_off && prev >= HealthTier::Warn {
        HealthTier::Warn
    } else {
        HealthTier::Calm
    }
}

/// Coarse health for a place: the worst of the three PSI "some avg10" stall
/// signals if any is known, else the CPU% fallback, else `Calm`. Hysteresis is
/// applied against `prev`. (CPU fallback exists for remote snapshots from older
/// daemons that predate the PSI fields; a modern host reports PSI.)
fn place_health(prev: HealthTier,
    psi_cpu: Option<f64>, psi_mem: Option<f64>, psi_io: Option<f64>,
    cpu_pct: Option<f64>) -> HealthTier {
    let psi_worst = [psi_cpu, psi_mem, psi_io].into_iter().flatten()
        .fold(None::<f64>, |acc, x| Some(acc.map_or(x, |a| a.max(x))));
    if let Some(p) = psi_worst {
        stepped_tier(prev, p, PSI_WARN_ON, PSI_WARN_OFF, PSI_CRIT_ON, PSI_CRIT_OFF)
    } else if let Some(c) = cpu_pct {
        stepped_tier(prev, c, CPU_WARN_ON, CPU_WARN_OFF, CPU_CRIT_ON, CPU_CRIT_OFF)
    } else {
        HealthTier::Calm
    }
}
```

- [ ] **Step 4: Run tests.**

Run: `cargo test --bin aerie tier 2>&1 | tail -20` and `cargo test --bin aerie place_health 2>&1 | tail`
Expected: PASS — all four new tests.

- [ ] **Step 5: Commit.**

```bash
git add src/main.rs
git commit -m "feat(health): HealthTier + pure PSI/CPU tier logic with hysteresis"
```

---

### Task 3: aerie — per-place health state, signals, and refresh update

**Files:**
- Modify: `src/main.rs` (`AppState` struct field + init; `place_signals` + `place_health_tier` methods near `selected_place_entries`; health-update block at the end of `refresh`)

**Interfaces:**
- Consumes: `HealthTier`, `place_health` (Task 2); `fleet_conn_for_label`; `AppState.{sys_psi_cpu, sys_psi_mem, sys_psi_io}` (already populated locally at `main.rs:2432-2434`); `FleetConn.snap` PSI fields (`sys_psi_cpu/mem/io`, `sys_cpu_pct`); `fleet_spine_places()`.
- Produces: `AppState.health_tiers: HashMap<String, HealthTier>`; `fn place_health_tier(&self, place_label: &str) -> HealthTier`.

- [ ] **Step 1: Add the field.** In the `AppState` struct (near `stable_order` / the fleet fields), add:

```rust
    /// Coarse health tier per spine place, keyed by place label (hostname).
    /// Rebuilt each refresh with hysteresis carried from the prior tick. Drives
    /// the spine's health gutter glyph (see `place_health`).
    pub health_tiers: HashMap<String, HealthTier>,
```

And in the `AppState { ... }` initializer (near `stable_order: vec![],`):

```rust
            health_tiers: HashMap::new(),
```

- [ ] **Step 2: Add the accessors.** Near `selected_place_entries` in `main.rs`:

```rust
    /// The (psi_cpu, psi_mem, psi_io, cpu_pct) signals for a spine place by
    /// label: the remote host's latest snapshot in Fleet mode, or this machine's
    /// own metrics in Local mode. The local place uses PSI only — a live local
    /// kernel always reports PSI, and the CPU fallback exists for old remote
    /// daemons, so `None` cpu% here just means "rely on local PSI".
    fn place_signals(&self, place_label: &str)
        -> (Option<f64>, Option<f64>, Option<f64>, Option<f64>) {
        if let AppMode::Fleet { .. } = self.mode {
            if let Some(snap) = fleet_conn_for_label(place_label, &self.fleet_clients)
                .and_then(|c| c.snap.as_ref())
            {
                return (snap.sys_psi_cpu, snap.sys_psi_mem, snap.sys_psi_io, snap.sys_cpu_pct);
            }
            (None, None, None, None)
        } else {
            (self.sys_psi_cpu, self.sys_psi_mem, self.sys_psi_io, None)
        }
    }

    /// The cached coarse health tier for a spine place (updated each refresh in
    /// `refresh`). `Calm` when the place is unknown or has no data yet.
    fn place_health_tier(&self, place_label: &str) -> HealthTier {
        self.health_tiers.get(place_label).copied().unwrap_or_default()
    }
```

- [ ] **Step 3: Update tiers each refresh.** At the **end of `refresh`** (after the mode branches — the `refresh` body ends near `main.rs:2651`; place this just before its final closing brace, after `self.entries` / snapshots are current for the tick):

```rust
        // Refresh coarse per-place health for the spine gutter. Rebuild the map
        // from the current places (dropping departed hosts), carrying each
        // place's previous tier so `place_health` can apply hysteresis.
        let places = self.fleet_spine_places();
        let mut next: HashMap<String, HealthTier> = HashMap::with_capacity(places.len());
        for p in &places {
            let prev = self.health_tiers.get(&p.label).copied().unwrap_or_default();
            let (pc, pm, pio, cpu) = self.place_signals(&p.label);
            next.insert(p.label.clone(), place_health(prev, pc, pm, pio, cpu));
        }
        self.health_tiers = next;
```

- [ ] **Step 4: Build + test.**

Run: `cargo build --bin aerie && cargo test --bin aerie 2>&1 | tail -5`
Expected: builds clean; all tests pass (this task adds no unit test of its own — it's wiring exercised end-to-end in Task 4; the pure logic it calls is covered by Task 2). Confirm no borrow-checker regressions: `fleet_spine_places()` returns an owned `Vec`, and `place_signals` returns owned `Option<f64>`s, so building `next` before assigning `self.health_tiers` has no aliasing conflict.

- [ ] **Step 5: Commit.**

```bash
git add src/main.rs
git commit -m "feat(health): cache per-place health tier each refresh (PSI signals + hysteresis)"
```

---

### Task 4: aerie — spine gutter glyph, legend, and real-host verification

**Files:**
- Modify: `src/ui.rs` (spine loop in `render_fleet` ~line 1274-1283; add `tier_decoration` + glyph consts; import `HealthTier` and mullion `render_tree_row_decorated`/`RowDecoration`)
- Modify: `src/ui.rs` (`manual_lines` ~line 2255 — add a neutral legend section)
- Verify: tmux against apollo + milkv

**Interfaces:**
- Consumes: `AppState::place_health_tier` (Task 3); `HealthTier` (Task 2); mullion `outline::{render_tree_row_decorated, RowDecoration}` (Task 1).

- [ ] **Step 1: Imports + glyph consts + mapping.** At the top of `ui.rs`, add to the mullion outline import (currently `use mullion::outline::render_tree_row;`):

```rust
use mullion::outline::{render_tree_row, render_tree_row_decorated, RowDecoration};
```

Import the tier type from the crate root (aerie is a bin; `HealthTier` is in `main.rs` / crate root):

```rust
use crate::HealthTier;
```

Add the glyph constants and the tier→decoration mapping near the other `render_fleet` helpers in `ui.rs`:

```rust
/// Health gutter glyphs. Width-1 required (fixed 2-col gutter). `⚠` forces text
/// presentation (VS15) to stay width-1; if a target terminal renders it wide,
/// change CRIT_GLYPH to "▲" (see plan Global Constraints / Step 5 verification).
const WARN_GLYPH: &str = "◆";
const CRIT_GLYPH: &str = "⚠\u{FE0E}";

/// The leading-gutter decoration for a place's health tier, or `None` when calm
/// (blank gutter). Colours come from the mullion theme (`warn`/`error`).
/// `Theme` is already imported unqualified in ui.rs (`use mullion::Theme;`).
fn tier_decoration(tier: HealthTier, theme: &Theme) -> Option<RowDecoration<'static>> {
    match tier {
        HealthTier::Calm => None,
        HealthTier::Warn => Some(RowDecoration { glyph: WARN_GLYPH, style: theme.warn }),
        HealthTier::Critical => Some(RowDecoration { glyph: CRIT_GLYPH, style: theme.error }),
    }
}
```

- [ ] **Step 2: Switch the spine row render to the decorated variant.** In `render_fleet`, replace the spine loop's `render_tree_row(...)` call (ui.rs ~1280) with:

```rust
            let deco = tier_decoration(state.place_health_tier(&p.label), &theme);
            render_tree_row_decorated(buf, row, &p.ancestor_last, p.is_last, p.expanded,
                &p.label, i == state.spine_cursor, &theme, TextCtx::default(), deco);
```

- [ ] **Step 3: Add the neutral help legend.** In `manual_lines()` (ui.rs ~2255), add a section (place it after the `NAVIGATION` block for visibility):

```rust
        "".into(),
        "FLEET HEALTH  (spine gutter)".into(),
        "  ◆   elevated resource pressure   (PSI stall, some avg10)".into(),
        "  ⚠   high resource pressure       (PSI stall, some avg10)".into(),
        "      calm hosts show no glyph.".into(),
```

- [ ] **Step 4: Build + tests.**

Run: `cargo build --bin aerie && cargo test --bin aerie 2>&1 | tail -5`
Expected: builds clean; all tests pass (still green — this is render wiring).

- [ ] **Step 5: Verify LOCAL render (calm gutter aligned).**

```bash
cargo build --bin aerie
SOCK=/tmp/aerie-glyph.sock
tmux -S "$SOCK" kill-server 2>/dev/null
tmux -S "$SOCK" new-session -d -s a -x 200 -y 50 "./target/debug/aerie --interval 2"
sleep 3
tmux -S "$SOCK" send-keys -t a f ; sleep 2
echo "=== spine (should be aligned; calm host = blank 2-col gutter, guides at col ~3) ==="
tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,6p' | cut -c1-40
tmux -S "$SOCK" kill-server 2>/dev/null
```
Expected: the spine label (`apollo`/hostname) is indented by the 2-col gutter and the tree guides/label are intact — no misalignment, no stray glyph on a calm host.

- [ ] **Step 6: Verify FLEET render + glyph under induced load on BOTH hosts (the payoff).** Confirm glyph width/alignment on apollo (x86) and milkv (riscv64), and that a stressed host flips to `◆`/`⚠` then settles.

```bash
SOCK=/tmp/aerie-glyph2.sock
tmux -S "$SOCK" kill-server 2>/dev/null
tmux -S "$SOCK" new-session -d -s a -x 200 -y 50 "./target/debug/aerie --hosts apollo,milkv --enable-remote --ssh-accept-new --interval 2"
sleep 10
tmux -S "$SOCK" send-keys -t a f ; sleep 2
echo "=== spine BEFORE load (expect calm/blank gutters, both hosts aligned) ==="
tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,6p' | cut -c1-40
# Induce memory+io pressure on apollo for ~20s (PSI some avg10 will climb):
ssh apollo 'timeout 20 sh -c "for i in 1 2 3; do dd if=/dev/zero of=/tmp/aerie-load.$i bs=1M count=2048 oflag=direct 2>/dev/null & done; wait"' &
sleep 18
echo "=== spine UNDER load on apollo (expect ◆ or ⚠ in apollo's gutter, width-1, aligned) ==="
tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,6p' | cut -c1-40
sleep 14
echo "=== spine AFTER load settles (expect apollo returns to calm, no strobe) ==="
tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,6p' | cut -c1-40
ssh apollo 'rm -f /tmp/aerie-load.*' 2>/dev/null
tmux -S "$SOCK" kill-server 2>/dev/null; pkill -f 'ssh .*apollo' 2>/dev/null
```
Expected: calm gutters before load; apollo's gutter shows `◆`/`⚠` under load with the label still column-aligned (glyph is width-1); returns to blank after load without flapping between frames. **If `⚠` renders width-2 / misaligns on either host, change `CRIT_GLYPH` to `"▲"` in ui.rs, rebuild, and re-verify.** Paste the three frames. Clean up: no leftover `ssh`/`aerie --daemon` on dop561 (`pgrep -af 'ssh .*apollo'`) or load files on apollo.

- [ ] **Step 7: Commit.**

```bash
git add src/ui.rs
git commit -m "feat(fleet): spine health gutter glyph (◆ elevated / ⚠ high PSI pressure) + legend"
```

---

## Self-Review

**Spec coverage:**
- Signal → tier (PSI worst-of-3, CPU fallback, Calm otherwise): Task 2 `place_health`. ✓
- Thresholds + hysteresis (balanced): Task 2 consts + `stepped_tier`. ✓
- Glyph vocabulary (blank / ◆ amber / ⚠ red, shape+color redundant): Task 4 consts + `tier_decoration`. ✓
- Leading gutter, own color, selected-row legibility, alignment, `render_tree_row` unchanged: Task 1 mullion extension + tests. ✓
- Both Local & Fleet: Task 3 `place_signals` handles both modes; Task 4 verifies local (Step 5) and fleet (Step 6). ✓
- Domain-agnostic framing + neutral legend: Task 4 Step 3. ✓
- Real-host width/alignment verification (apollo x86 + milkv riscv64) + ⚠ fallback to ▲: Task 4 Step 6. ✓
- General mullion extension reusable by siblings: Task 1 (generic glyph+style, not health-specific). ✓

**Placeholder scan:** no TBD/TODO; all code shown in full.

**Type consistency:** `HealthTier` (Task 2) is consumed by `place_health_tier`/`place_signals` (Task 3) and `tier_decoration` (Task 4). `RowDecoration<'a>{ glyph: &str, style: Style }` (Task 1) is produced by `tier_decoration -> Option<RowDecoration<'static>>` (Task 4, `&'static str` consts) and consumed by `render_tree_row_decorated(..., Option<RowDecoration>)` (Task 1). `place_signals -> (Option<f64>×4)` feeds `place_health(prev, psi_cpu, psi_mem, psi_io, cpu_pct)` in the same order (Task 3 → Task 2). PSI thresholds in the Task 2 code (25/15/50/40) match the Task 2 tests and the spec table.

**Cross-repo note:** Task 1 is committed in `../mullion` (branch `outline-row-decoration`); Tasks 2-4 in aerie (branch `spine-health-glyphs`). aerie builds against the mullion **path** dependency, so Task 1's change is picked up with no version bump. Finishing the branch must complete **both** repos' branches (merge mullion's `outline-row-decoration` and aerie's `spine-health-glyphs`).
