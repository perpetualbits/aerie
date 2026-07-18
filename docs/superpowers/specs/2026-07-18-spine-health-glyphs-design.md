# Spine Health Glyphs (Tier-0 coarse triage) — Design

**Date:** 2026-07-18
**Status:** approved (brainstorm) — ready for implementation plan

## Goal

Put a coarse per-host health signal in the Fleet-face spine's left gutter, so a
single glance tells you **which place to open** — without drilling into each
host. Calm hosts stay blank; only strained ones draw the eye.

## Purpose & UX

The spine is aerie's place tree (host → later VMs/containers). Today every row
looks equally worth opening. A health glyph turns the spine into a triage
surface: your eye runs down one fixed left column and lands on the hosts that
are actually stalling.

- **Calm host:** blank gutter (no clutter — the common case).
- **Elevated pressure:** `◆` in amber (`Theme::warn`).
- **High pressure:** `⚠` in red (`Theme::error`).

Shape carries the tier (colorblind-safe); color reinforces it. Applies in
**both Local and Fleet** modes (one glyph on the local place too — PSI is read
locally, so it's consistent and nearly free).

## Signal → tier

Per place, the raw tier is derived from **Pressure Stall Information (PSI)**,
the kernel's direct measure of tasks stalled waiting on a resource — which is
exactly what aerie exists to surface (latency & thread balance).

- **Primary:** `worst = max(sys_psi_cpu, sys_psi_mem, sys_psi_io)` ("some
  avg10", each 0–100 = % of the last 10 s at least one task stalled on that
  resource). A host stalling on *any* resource is worth a look, so take the
  worst.
- **Fallback:** when all three PSI values are `None` (older daemon without the
  PSI fields, or a non-Linux host), fall back to `sys_cpu_pct`.
- **None available:** if neither PSI nor CPU% is known (e.g. a thin probe with
  no system metrics, or first tick), tier = Calm (never guess a problem).

### Thresholds (balanced sensitivity)

| Signal        | warn (◆) | critical (⚠) |
|---------------|----------|--------------|
| PSI some avg10 | ≥ 25    | ≥ 50         |
| CPU % (fallback) | ≥ 85  | ≥ 97         |

### Hysteresis (anti-flap)

A host hovering at a threshold must not strobe the gutter. Escalate immediately
on breach; de-escalate only after the signal drops well below the trip line.
Given the previous tier `prev` and the current signal value `v` (PSI or CPU):

```
if   v >= CRIT_ON                      -> Critical
elif v >= CRIT_OFF and prev == Critical -> Critical   (sticky down)
elif v >= WARN_ON                       -> Warn
elif v >= WARN_OFF and prev >= Warn     -> Warn        (sticky down)
else                                    -> Calm
```

PSI: `WARN_ON=25, WARN_OFF=15, CRIT_ON=50, CRIT_OFF=40`.
CPU: `WARN_ON=85, WARN_OFF=78, CRIT_ON=97, CRIT_OFF=93`.

This requires a small amount of **per-place previous-tier state**, keyed by
place label (hostname). Stored centrally on `AppState`
(`health_tiers: HashMap<String, HealthTier>`), updated once per refresh for
every spine place — no change to `FleetConn`.

## Data model (aerie side)

```rust
/// Coarse health of a place, worst-signal-wins. Ord: Calm < Warn < Critical.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum HealthTier { Calm, Warn, Critical }
```

Pure, testable helpers (unit-tested without touching AppState):

```rust
/// Map a single signal value through on/off thresholds with hysteresis.
fn stepped_tier(prev: HealthTier, v: f64,
                warn_on: f64, warn_off: f64, crit_on: f64, crit_off: f64) -> HealthTier;

/// Raw tier for a place from its signals (PSI worst-of-3, else CPU%, else Calm),
/// applying hysteresis against `prev`.
fn place_health(prev: HealthTier,
                psi_cpu: Option<f64>, psi_mem: Option<f64>, psi_io: Option<f64>,
                cpu_pct: Option<f64>) -> HealthTier;
```

**Signal sources per place:**
- **Fleet mode:** `fleet_clients[i].snap` → `sys_psi_cpu/mem/io`, `sys_cpu_pct`.
- **Local mode:** the local host's PSI. `AppState` already declares
  `sys_psi_cpu/mem/io` and `local::read_psi()` exists; the plan must ensure the
  local snapshot's PSI actually flows into these fields (wire it in `refresh`
  if not already), so the local place's glyph has real data.

`AppState` gains an accessor the renderer calls per spine row:

```rust
/// The health tier for the spine place at index `i` (mode-aware). Reads the
/// cached, hysteresis-applied tier from `health_tiers`.
fn place_health_tier(&self, place_label: &str) -> HealthTier;
```

`refresh()` updates `health_tiers` for every `fleet_spine_places()` entry each
tick (after snapshots are polled), feeding `place_health` the place's signals
and its own previous tier.

## The mullion extension (general, sibling-useful)

The glyph needs its own color independent of the row's selection fill, so this
is a **mullion** change (per the standing "prefer mullion, propose general
extensions" rule — reusable by census/canopy/shamal).

```rust
// mullion::outline
pub struct RowDecoration<'a> {
    /// One display-cell glyph painted in the leading gutter.
    pub glyph: &'a str,
    /// The glyph's own style — kept even when the row is `selected`.
    pub style: Style,
}

/// Like `render_tree_row`, but reserves a fixed leading gutter (glyph + space)
/// before the guides and paints `deco` there in its own style. `deco == None`
/// leaves the reserved gutter blank (so decorated and calm rows stay
/// column-aligned). The guides + label render in the remaining width exactly as
/// `render_tree_row` does.
pub fn render_tree_row_decorated(
    buf, rect, ancestor_last, is_last, expanded, label, selected, theme, ctx,
    deco: Option<RowDecoration>,
);
```

- **Gutter width is fixed** (glyph cell + 1 space). The glyph MUST be display
  width 1 so calm and decorated rows align. `◆` (U+25C6) is width-1. `⚠`
  (U+26A0) must be forced to **text presentation** (append VS15 `U+FE0E`) to
  stay width-1; if a target terminal (apollo/milkv) still renders it width-2 or
  as emoji, fall back to a guaranteed width-1 critical glyph (`▲`). The plan
  verifies actual width on both hosts and picks accordingly — the *contract*
  (leading gutter, own color, alignment) does not change.
- When `selected`, the row still gets the `Theme::selection` fill; the glyph is
  repainted in its own severity style on top, so severity stays legible on the
  focused row.
- **`render_tree_row` is unchanged in behavior** — reimplement it as
  `render_tree_row_decorated(..., None)` *without* a reserved gutter (a private
  inner takes a `gutter: bool`/width), so every existing caller (census DIT,
  aerie's non-decorated trees) renders byte-for-byte as today.
- Colors come from mullion's existing `Theme::{ok, warn, error}` — **no new
  theme fields**.

aerie's spine renderer (`ui.rs::render_fleet`) switches its per-row call from
`render_tree_row` to `render_tree_row_decorated`, passing
`Some(RowDecoration{ glyph, style })` for warn/critical and `None` for calm.

## Domain-agnostic framing (hard constraint)

The glyph reports **resource pressure (PSI stall)** in neutral, behavioral
terms only — never "slow host", never anything app/role/desktop-specific
([[aerie-stay-general]]). Discoverability via a one-line entry in the existing
help overlay, worded neutrally:

```
◆ elevated resource pressure   ⚠ high resource pressure (PSI stall)
```

## Testing

- **Unit (pure):** `stepped_tier` and `place_health` — threshold boundaries,
  PSI-worst-of-3 selection, CPU fallback when PSI absent, Calm when nothing
  known, and hysteresis (sticky-down: a value between OFF and ON keeps the
  higher tier only if `prev` was already there; a fresh breach escalates).
- **mullion:** `render_tree_row_decorated` — gutter reserved and aligned across
  decorated/calm rows; glyph painted in its style; selected row keeps glyph
  color; `render_tree_row` output identical to before (snapshot/again test).
- **Integration (tmux, real hosts):** the standing aerie discipline — run
  `aerie --hosts apollo,milkv --enable-remote`, confirm calm hosts show a blank
  gutter and a stressed host (induce load, e.g. `stress-ng`/`dd`) flips to ◆
  then ⚠ and settles back without strobing. Verify glyph width/alignment on
  both apollo (x86) and milkv (riscv64) terminals.

## Out of scope (deferred)

- Finer per-resource glyphs (separate cpu/mem/io indicators) — Tier-0 is one
  worst-of glyph.
- Rolling the glyph up a nested tree (a parent host showing its worst child's
  tier) — relevant only once the spine gains real nesting (VMs/containers);
  revisit then.
- History/sparkline of pressure, alerting, thresholds in config/CLI — this is a
  live coarse indicator, not a monitoring-rules engine.
- Coloring the label itself by tier — the gutter glyph is the signal; the label
  stays neutral.

## Related

[[project-aerie]] · [[aerie-stay-general]] · [[prefer-mullion-propose-extensions]] · [[aerie-test-hosts]]
