# aerie navigable face — design

**Date:** 2026-07-16
**Status:** Approved (design); implementation phased (see Phasing)
**Supersedes as the UX direction:** `2026-07-12-constellation-face-design.md` (see Context)

## Context and motivation

aerie grew along three independent axes:

- **Scope** — it began application-centered (is a multi-threaded app's work *balanced*?), then spread across hosts: Proxmox VMs/CTs, Nomad/Consul nodes, docker/podman containers. Crucially it is **fractal**: you can *enter* a place (a VM, say) and run aerie there as if local, and that place may itself contain places.
- **Depth** — host → process groups → threads → (latency offenders).
- **Function** — *monitor* (thread balance, health) vs *diagnose* (the latency/jitter tooling in `diag.rs`: latency probe, pressure probe, periodic-offender detector — built to hunt the audio-latency stalls that stutter a DAW).

The current interface — a single active pane (`Groups` → drill `Threads` → toggle `Scope`), switched by a growing set of mode keys — no longer fits this dimensionality.

**The constellation spike** (`2026-07-12`) tried to make a *spatial map itself* the monitoring surface. Built through nine iterations to a clean, verified state, it was then judged at a live terminal and **rejected as aerie's face**: as an everyday monitoring surface it is information-poorer than aerie's own tables, unstable to navigate (tiles resize under the cursor), and answers "what new information does this give me?" with *none*. The one durable win was the **detector** (real latency probe → ~3 s clock → userspace culprit → bedrock proof), which is real and belongs in the dense UI, not a spatial toy.

The user's split verdict is the seed of this design: **"look/function like this: NO; navigate like this: maybe."** So: keep aerie's dense views; redesign the **connective tissue** — how you move among them and orient in the fractal — and let the spatial idea live only as *navigation*, never as the monitor.

## Goals

- One coherent way to move across scope × depth × function without losing your place.
- Fleet-health-at-a-glance: see which places are in trouble and travel straight to them.
- Never regress information density: every dense view aerie has today survives.
- Make the diagnose tooling (`diag.rs`) *earn its place* inside the dense UI — as a lens and a sort, with proof.
- Degrade gracefully to today's single-pane experience on a narrow terminal.

## Non-goals

- The constellation / spatial map as a monitoring surface. Dead.
- Replacing the tables/heatmaps/scope with new encodings (this is connective tissue, not new views).
- Guaranteed always-on *deep* health for every place regardless of cost (see Tier-0/Tier-1).
- Unrelated refactoring beyond carving out the modules this work touches.

## Design decisions (the settled forks)

1. **Redesign scope:** the connective tissue (navigation/orientation/layout), keeping the dense views.
2. **Navigation mental model:** a spatial *map of places* you travel through — but the payload at each place is its real dense view, not boxes.
3. **Map × view coexistence:** a **persistent, compact map spine** alongside a full dense main view (not semantic-zoom-into-the-map, not an overlay-only switcher). The spine stays small and purely navigational, so it cannot rot into a hollow container.
4. **Spine carries live health**, so it is fleet-status-at-a-glance and navigate-to-trouble — not a pure name tree.
5. **Main area is a context+detail split:** a primary group list plus a detail pane that follows the selection; the monitor/diagnose *lens* reshapes the detail (and, under diagnose, re-ranks the primary).

## The face: three regions

```
┌─map──┐┌─ groups ─────────┐┌─ bitwig ▸ detail ────────┐
│●host ││ ▸bitwig   ███ 42%││ t0 ████  audio          │
│├◉vm-b⚠││  pipewire ██  18%││ t1 ███   worker         │
││ └ct1 ││  chrome   █   12%││ t2 █                    │
│├ vm-c ││  Xorg     ▍    6%││ ── lens: monitor │ scope │
│└ vm-d△││  ...            ││                          │
└──────┘└─────────────────┘└──────────────────────────┘
  WHERE       WHAT                  DETAIL / DIAGNOSIS
  scope       depth: groups         depth: threads · function: lens
```

- **Spine (WHERE / scope):** compact fractal tree of places, each with a live health glyph; `◉` = you are here.
- **Primary (WHAT / depth: groups):** the dense, sortable group table — aerie's strength, unchanged in spirit (`comm/cgroup/exe` grouping retained).
- **Detail (depth: threads + function: lens):** the selected group's internals — thread heatmap (monitor) or latency/offender contribution (diagnose).

**Everyday loop:** glance at the spine → something's hot → arrow to it (primary + detail repopulate) → skim groups → select the suspect → detail shows its threads, or flip to diagnose to see *why* — one flow from "something's wrong in my fleet" to "here's the thread stalling," never losing your place.

## The spine and the health model

**Contents.** A lazily-expanding **containment tree of places** (host → VMs/CTs → containers → nodes), indented under their parents. The path above `◉` is the breadcrumb. Children appear as you explore: enter a VM and the places *it* sees become its spine children — the fractal made navigable down the same rail.

**Two honest tiers of health.** The glyph reflects only what aerie can actually see from where it stands:

| Glyph | Meaning | Tier / source |
|---|---|---|
| (dim) | calm | any |
| `△` | **warm** — coarse pressure elevated (CPU/mem/IO) | **Tier 0**: orchestrator/hypervisor API, cheap, always-on |
| `▲` | **hot** — coarse pressure high/sustained | Tier 0 |
| `⚠` | **stall confirmed** — periodic latency stall detected *by a probe running there* | **Tier 1**: aerie probing (local, or attached) |

The `△/▲` (coarse, seen from outside) vs `⚠` (confirmed, seen from inside) distinction is deliberate and truthful: from outside, aerie can see a VM's CPU is pegged (Proxmox API), but **cannot** claim a periodic latency stall until a probe runs inside. `⚠` appears only for deeply-attached places.

**Attach mechanic — coarse peek, deep enter.** Selecting a place shows its coarse state instantly from cached API data. `Enter` *attaches* (SSH + run aerie / an agent), populating real groups/threads/scope and upgrading that place to Tier-1 health. Reuses the existing `Connecting → Remote` flow.

**Cost, honestly.** Tier-0 fleet health is free where an API already streams it (Proxmox, already polled by `proxmox.rs`). Plain SSH hosts have no free lunch: continuous Tier-0 needs a cheap periodic probe or a small agent, so it is opt-in / slower-cadence; until then those places sit neutral until entered. The glyph must be honest-but-sparse, never confidently wrong.

## The main area and the diagnose flow

**Primary (groups)** — unchanged in spirit; dense, sortable, `comm/cgroup/exe` grouping.

**Detail follows selection**, shaped by the **lens**:

- **Monitor lens →** the group's **thread heatmap** — aerie's founding question: is a multi-threaded app balanced, or is one thread pegged while the rest idle? The spine now lets you ask that of any place through the same motion.
- **Diagnose lens →** the group's **latency/offender contribution**: is *this* group acting on the stall clock? Its CPU-delta periodicity, its threads' wakeup behavior.

**The lens re-lenses both panes; it does not take over the screen.** When you flip to diagnose at a stalling place:

1. A header states the finding: `⚠ stall detected — contention on CPU — ~3.0s clock`.
2. The **primary re-ranks by culpability** — the periodic offender rises to the top, annotated *"acting on a ~3.0 s clock."* (The offender detector, surfaced as a *sort*.)
3. The **detail shows the proof** — the latency-scope trace (braille) + period + contended resource; once you select the offender, *its* contribution to the clock.

**End-to-end payoff — fleet stall to root cause, one flow:**

```
spine: vm-b ⚠   →  Enter (attach)  →  diagnose lens
  "something's stalling      "⚠ contention on CPU · ~3.0s"
   somewhere"                 groups re-ranked: ▸ audio-daemon  "on a ~3.0s clock"
                              detail: ░░▓░░▓░  period 3.0s · magnitude 8ms
  →  select audio-daemon  →  its threads + its periodic contribution = proof
```

This is where the detector work finally earns its place: inside the dense UI, as a lens and a sort — genuinely new information (clock, resource, culprit, proof) the plain table never gave.

## Interaction model

Guiding principle: **left is *where*, right is *why*** (spine → primary → detail runs coarse scope → finest detail). A small, consistent grammar replaces today's growing pile of mode keys:

- **`←/→` (`h/l`) — move focus between regions** (spine ↔ primary ↔ detail); focused region is lit, others stay live.
- **`↑/↓` (`j/k`) — move selection within the focused region.**
- **`Enter` — go deeper / activate:** attach+enter a place / expand its children; hand focus to a group's detail; drill a thread/offender.
- **`Esc` (or `←` at the left edge) — climb out.**
- **Lens toggle** (reuse today's `d`): monitor ↔ diagnose for the current place.
- **`]` / `[` — jump to next / previous *alerting* place** — navigate-to-trouble as one keystroke.

**Orientation is invariant:** the spine always shows `◉` at the current place, even while focus is in the primary/detail; only `Enter` on a new place changes scope. Mouse click-to-focus is a nice-to-have, not core.

## Responsive behavior (must never require a wide terminal)

| Width | Layout |
|---|---|
| **Wide** (≳120) | All three regions: `spine │ primary │ detail` |
| **Medium** (~80–120) | Spine collapses to a **thin glyph rail** (health dot + `◉`, labels on focus); primary + detail share the rest |
| **Narrow** (<80) | **One region full-width at a time** (today's single-pane feel) + a persistent 1-line orientation strip: `host ▸ vm-b ▸ bitwig   fleet: 2⚠ 1△`; `←/→` switch the active region |

Scales *up* to the dashboard and *down* to today's one-pane aerie, keeping the breadcrumb and fleet-health summary even at its narrowest. The spine is also manually collapsible (full → rail → hidden).

## Mapping to the code

**Core refactor — one place-tree instead of separate modes.** Today's `AppMode` (Local/Proxmox/…) + `AppView::{Groups, Remote, Connecting}` are largely separate top-level states. Collapse them into a single **places tree**: local host, Proxmox VM, SSH host, container are all *places* of different kinds; "which mode" becomes "which place." That unification *is* the connective tissue.

| Piece | Code | Change |
|---|---|---|
| Places model + tree + Tier-0/Tier-1 health | **new** `fleet.rs` / `places.rs` | new — "give me health / groups / threads for this place, at the depth available here" |
| Spine widget (tree + glyphs + `◉`) | **new** in `ui.rs`, on mullion `outline`/`panel` | new render |
| Coarse health + VM discovery | `proxmox.rs` | reused/extended to feed the spine |
| Deep attach (`Enter` → SSH + aerie) | `remote.rs` | reused as the Tier-1 upgrade |
| Local place data | `local.rs` | largely reused |
| Diagnose engine (`⚠`, clock, offender rank, trace) | `diag.rs` | reused — surfaced as a *lens + sort*, not a separate full-body `Scope` |
| Three-region layout + focus grammar | `main.rs` + `ui.rs` | restructured event/render around region-focus |

`main.rs` (3543 lines) and `ui.rs` (2380) are already oversized; carve the places-model and spine/region rendering into focused new modules rather than pile onto them. mullion already provides the panels, borders, tables, `field`/braille traces, and responsive primitives — this is composition, not new rendering machinery.

## Risks

1. **The unified place interface leaks** — a not-yet-entered VM can't give threads; a local host can. Tier-0/Tier-1 models this, but the interface must cleanly express "what's available here" or callers over-assume.
2. **Attach lifecycle** — multiple live SSH sessions + their probes to manage/tear down; today assumes one.
3. **Tier-0 cost off-Proxmox** — continuous fleet health for SSH hosts needs cheap probes/agents; keep opt-in, don't over-promise.
4. **Density at TUI sizes** — responsive breakpoints must be verified by real pty/tmux capture at 80/120/200 cols. (Spike lesson: "compiles" ≠ "renders.")

## Testing approach

- **Unit-test** the pure logic: the places-model interface (availability by tier), health-glyph derivation from coarse metrics, the offender-ranking sort, and the responsive breakpoint selection.
- **Render-verify** via pty/tmux capture at 80 / 120 / 200 cols — the spine, the three-region split, the degradation, and the diagnose re-lensing — because real /proc scale and layout only show when actually rendered.
- Reuse the spike's verification discipline: never trust "compiles + tests pass" for the TUI; capture frames.

## Phasing (one spec, delivered in shippable phases)

- **P1 — skeleton:** places-tree + spine + region-focus grammar; **local host + an SSH remote host** (the two kinds `local.rs`/`remote.rs` already cover); monitor lens only. Proves the navigation feels right.
- **P2 — fleet health:** Tier-0 coarse glyphs (Proxmox) + Tier-1 `⚠` + jump-to-trouble.
- **P3 — diagnose lens:** re-rank-by-culpability + proof in detail (wiring `diag.rs`).
- **P4 — responsive + polish:** narrow/medium degradation, collapsible spine.

The implementation plan is written **per phase**, starting with P1.

## Open questions (deferred, not blocking P1)

- Off-Proxmox Tier-0: agent vs periodic SSH probe vs neither — decided when P2 is planned.
- Multi-attach session limits and teardown policy — decided when P1's remote attach is planned.
- Whether the diagnose lens auto-engages when a place is confirmed stalling, or stays manual — decided in P3.
