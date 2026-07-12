# Constellation face — design & spike

**Date:** 2026-07-12
**Status:** design approved; spike not yet built
**Artifact of this design:** a throwaway prototype, `examples/constellation.rs`

## Problem

aerie has grown a set of powerful but disconnected modes: local `/proc` monitoring,
SSH fleet fan-out, Proxmox VE, kube pods, and the Instruments subsystem (latency
scope, pressure probe, offender detector) surfaced behind a `d` key. Each mode
reads as a separate app. The tool lacks a *unifying face* from which these modes
logically flow. This is a UX problem: we want to **make the abstract tangible** so
people can still see the big concepts instead of a pile of side-by-side features.

The modes are not really a flat list — they sit in a 2-D space:

- **Scope of subject** — this machine → SSH fleet → Proxmox VM → process → thread.
- **Kind of question** — "who is *consuming*?" (the bar chart) vs. "what is
  *stalling everything*?" (the latency scope / rim instrument).

## The unifying concept: one spatial world you move through

Every mode becomes **a place at a zoom level in one map**. The decisions, in the
order they were settled:

1. **Unifying face = a spatial model.** One continuous "place" you move through, not
   a set of screens. Each mode is a viewpoint on the same map.

2. **Semantic zoom is the engine.** One world, magnified. You dive and surface.
   "Rooms" are not separate screens — each zoom stop *renders* a room appropriate to
   its scope, so the designed-per-scope feel is emergent. **Breadcrumbs are spatial**:
   the outer scopes you dove through stay visible as framing, so "back" is literally
   zooming out. A **persistent map** always frames the whole and shows where you are —
   you are never lost.

3. **Geometry = a constellation / topology graph.** Entities are nodes; position
   means *relatedness*; a stall can be shown propagating along edges. Chosen over a
   nested treemap or a fixed dashboard grid because it best serves "make the abstract
   tangible": you literally see relationships and propagation.

4. **Edges are layered.**
   - **Backbone (always on):** lineage — parent→child spawn + shared cgroup. Cheap,
     ever-present, and it *pins the layout* so the map never reshuffles under you.
     This is what makes positions memorable.
   - **Overlay (only during strain):** contention/flow edges that light up when
     measured — the "why." This is what a stall ripples along. Calm system → clean
     structural map; strain → the causal wiring illuminates.

5. **Bars dissolve into nodes.** No separate dual-metric bar chart. Metrics are
   encoded into node appearance: **size = CPU%, heat = memory, pulse = strain**. The
   map *is* the readout. The one thing this trades away — precise side-by-side numeric
   comparison — is preserved as an **exact readout on the focused node only**, so we
   keep precision without a separate widget.

6. **The stall manifests three ways at once, forming one narrative arc:**
   - **Weather** — the ambient signal: the whole map (and the rim) pulses on the bad
     clock. This is how you *notice* without looking.
   - **Materializing node** — attribution: the cause condenses into a first-class
     node on the map, overlay edges reaching to everyone it stalls. This is *where*
     and *what*.
   - **Bedrock** — evidence: dive into that node and its floor is the Instruments
     readout (period, kind, magnitude, observation traces). This is the *proof*.

   The arc is semantic zoom applied to a diagnosis: **notice → follow the ripple to
   the node → dive for the proof.** It folds the entire Instruments subsystem into
   the map instead of hiding it behind a `d` key.

## Why this is buildable: mullion already ships the primitives

Verified against `docs/mullion-manual.md` §3.21–§3.25:

- **`mullion::zoom` (§3.24)** — semantic level-of-detail zoom. `Lod::for_rect` maps a
  node's on-screen area to `Collapsed → Titled → Ported → Full`, where **`Full` = the
  node's internal graph**. That is exactly "a node unfolds into its own
  sub-constellation." `Zoom` + `FocusTarget::Node` animate a graph node's growth via
  `lerp_rect`.
- **`mullion::sugiyama` (§3.25)** — layered auto-layout, and crucially **idempotent**:
  placement depends only on node ids, sizes, and edges, never on current positions.
  Re-run it every frame under live `/proc` churn and it reproduces the same layout.
  This is what makes the map stable, killing the "graph reshuffles and I get lost"
  risk.
- **`mullion::graph` (§3.21, §3.23)** — `GraphCanvas` holds nodes as floating children
  with stable `TileId`s across re-solves (identity survives churn); `Viewport` gives a
  pannable window over a canvas larger than the screen, with exact scrollbars — the
  persistent map + breadcrumb frame.
- **`mullion::route` (§3.22)** — orthogonal connector routing with colour-per-net,
  routed in stable canvas space. Backbone and overlay edges are two net classes with
  distinct hues.
- **`mullion::field`** — sub-cell braille rendering for the bedrock latency timeline,
  the node pulse, and the rim weather strip (reusing aerie's rim heritage).

## The spike: `examples/constellation.rs`

Standalone example binary — same pattern as `examples/spiral_stress.rs`, zero coupling
to aerie's app loop, fastest iteration, no blast radius on the shipping binary.

### What the spike must PROVE (its only job)

1. Semantic zoom over a *live* graph feels continuous and legible — diving and
   surfacing is oriented, not disorienting.
2. Backbone layout stays stable frame-to-frame as processes come and go (idempotent
   Sugiyama + stable `TileId`s should deliver this; the spike confirms it *feels*
   stable).
3. Nodes encode two metrics + strain readably at a glance (size / heat / pulse) with
   no bar chart.
4. The stall arc reads as one gesture: weather → materialize → bedrock.

### Deliberately OMITTED (to stay a spike)

- **Real contention measurement.** Overlay edges and the stall are driven by a **fake
  periodic injector** (a synthetic ~3 s stall) so we test the visual/interaction feel
  now. Real detection wiring is a later step.
- Fleet / Proxmox / kube scopes — **local `/proc` only**.
- Retiring or touching aerie's real view — throwaway `[[example]]`.
- History scrubbing, alerts, GPU, precise persistence.

### Model

- **Nodes** = process-groups by `comm`, from a single `/proc` sweep (read directly, as
  `local.rs` does).
- **Backbone edges** = lineage (parent→child `comm` + shared cgroup) → `sugiyama::auto_layout`.
- **Node encoding** = size:CPU%, heat:memory, pulse:strain; exact numbers on the
  focused node only.
- **Zoom/dive** = `Zoom` + `FocusTarget::Node` + `Lod::for_rect`; `Full` LoD renders the
  node's interior subgraph (threads/children). Parent tiles recede as the breadcrumb
  frame. `graph::Viewport` = the persistent map.

### Stall arc (faked injector, real visuals)

- **Weather** — on the injector clock, canvas + window rim pulse together (`Field`
  strip on the perimeter).
- **Materialize** — injector picks a culprit node; it lights up with overlay contention
  edges (a second `route_all` net in a hot hue) to the nodes it "stalls."
- **Bedrock** — dive to `Lod::Full` on the culprit; its floor renders a latency timeline
  (`Field::render_braille`) with the injected period/magnitude, standing in for the
  real Instruments readout.

### Controls

`↑↓←→` / mouse — move focus · `Enter` / `+` — dive · `Esc` / `-` — surface · `q` — quit ·
`--stall` flag toggles the injector (test calm vs. strained feel).

### Build shape

Single file. `[[example]]` (and optional `[[bin]]`) in `Cargo.toml`, mirroring
`spiral_stress`. Primitives: `GraphCanvas`, `sugiyama::auto_layout`, `Zoom`/`Lod`,
`graph::Viewport`, `route::{route_all, render}`, `Field`. No new aerie modules.

## Constraint carried through: stay domain-agnostic

Per the standing rule ([[aerie-stay-general]]): the constellation reports the *shape*
of behavior in neutral, system-wide terms — a node acting on a clock, a resource under
contention, a stall of period P and magnitude M. It never names specific products or
suggests remedies. The materializing node is labeled by what it *is* on the system
(`comm`, a resource category), not by any product knowledge.

## Success criterion for the whole effort

After the spike, we can answer one question with a felt, not theoretical, answer:
**does a live constellation with semantic zoom feel like a good primary face for
aerie?** If yes, the follow-on work is real contention edges, the scope axis
(fleet/Proxmox/kube populating the top layer), and wiring the real Instruments readout
into bedrock. If no, we learned it cheaply and the shipping binary was never touched.

## Spike findings (2026-07-13)

Tasks 1–10 are implemented and committed. Unit tests (11) cover the pure logic:
`/proc` parsing, comm aggregation/edges, size/heat/pulse encoding, the injector's
intensity curve, and layout stability across identical frames. The four proof-goals
below, however, assess *felt* quality at a live terminal and cannot be answered from
a non-interactive environment — this section is a template for the human running the
spike to fill in after an actual session.

To run the spike:

```bash
cargo run --bin constellation          # calm path — no injector
cargo run --bin constellation -- --stall   # stall arc — weather/materialize/bedrock
```

Controls: `↑↓←→` move focus · `Enter` / `+` dive into the focused node ·
`Esc` / `-` surface back to the overview · `space` pause/resume the sampling +
animation clock · `q` / `Ctrl-C` quit. The footer line shows node count, seconds
since the last resample, and (under `--stall`) the injector's live intensity value.

### 1. Semantic zoom feels continuous & oriented

Dive (`Enter`/`+`) and surface (`Esc`/`-`) should land you back where you left off,
not lost.

_(awaiting live-terminal verification)_

### 2. Layout stable under live churn

Start or stop a process (e.g. `sleep 30 &`, or open/close a program) while the
constellation is running; existing nodes should hold their position while the new
one appears elsewhere.

_(awaiting live-terminal verification)_

### 3. Nodes readable at a glance

You should be able to read "who's busy / who's big" off node size (CPU) and heat
(memory) alone, without a bar chart or numeric readout.

_(awaiting live-terminal verification)_

### 4. Stall arc reads as one gesture

Under `--stall`, the sequence — notice the weather (wash + rim pulse) → follow it to
the materializing culprit node (hot overlay edges) → dive for the bedrock proof
(latency timeline) — should read as one continuous gesture, not three disconnected
effects.

_(awaiting live-terminal verification)_

**Recommendation:** _(pending human verdict)_
