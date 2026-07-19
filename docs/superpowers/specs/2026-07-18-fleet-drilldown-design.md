# Fleet Drilldown — Region-aware Enter grammar — Design

**Date:** 2026-07-18
**Status:** approved (brainstorm) — ready for implementation plan

## Problem

In the fleet face (`AppView::Fleet`, entered with `f`), pressing **Enter** does
nothing — you cannot drill into a host or an application. Root cause (confirmed
in code + live): the entire Enter drill-down handler (`main.rs:3760`) is gated
on `if matches!(state.view, AppView::Groups)`, and the fleet face is a different
view, so Enter never reaches the handler. The same gap leaves `AppView::Remote`
(a host viewed "as if local") without app→thread drilldown. This was never
implemented, not a regression.

## Goal

A **region-aware Enter grammar** that lets you drill from the fleet face down to
a single application's threads, reusing data that is already streaming — never
opening a redundant connection.

## Mental model: a drill path `(host, app)`

At every level, the **currently-focused `(host, app)`** is the single source of
truth. It decides what `send_focus` streams and what the active view renders.
Drilling changes *how much* of the already-streaming data you see — not the data
source, and never the connection.

## The grammar

| Where | Key | Result |
|-------|-----|--------|
| Fleet face, **Spine** focused, on a host | Enter | **Enter the host** — a full-screen dense process view of that host |
| Fleet face, **Primary** focused, on an app | Enter | **The app's threads** — full-screen per-thread view |
| **Host view**, on an app | Enter | The same app thread view (completes "enter host → browse apps → drill one") |
| Any drilled view | Esc | Pop one level back toward the fleet face |

### Host view ("enter the host as-if-local")
- Renders the **dense classic body** (bars / columns / metrics) from the
  selected host's already-streaming `fleet_clients[host].snap` — **no new SSH,
  no "Connecting…" wait**, live data.
- **Full dense keyboard grammar is live**, operating on the host's data: sort
  (`s`), cycle active-side metric (`←/→`), toggle sort direction (`n`), `↑/↓`
  select, and Enter-on-app → threads. It should feel like aerie running locally
  on that host.
- Esc → back to the fleet face (spine cursor restored on the entered host).

### App thread view
- **Matches the local Threads view** (`AppView::Threads`): the same full-screen
  layout — thread heat grid + per-thread rows (cpu% / names) — but sourced from
  the remote thread stream (`selected_place_threads()` / `focus_threads`) rather
  than the local sampler. The detail pane already proves this data renders.
- Esc → back to where the drill started (the fleet face, or the host view).

## Data & focus routing

- **No new connections.** Host view reuses `fleet_clients[host].snap`; thread
  view reuses the daemon's `focus_threads` stream.
- **`send_focus` follows the drilled `(host, app)`.** Today the fleet poll loop
  routes `send_focus` to the spine-selected host's *primary-cursor* group
  (`selected_fleet_group_label`, main.rs ~2048-2058). This must generalize to
  the **focused app at the current drill level**: the fleet-face primary
  selection when on the face, the host-view cursor's app when in the host view,
  and the fixed drilled app when in the thread view — so the thread stream stays
  pinned on the app you drilled into after leaving the face. Unselected hosts
  still get `send_focus(None)`.

## Back-navigation (a small drill stack)

Esc pops one level instead of dumping to `Groups`:

- Thread view → the view it was opened from (fleet face **or** host view).
- Host view → the fleet face.
- Fleet face → `Groups` (unchanged).

The originating view is recorded on `AppState` (a `fleet_return: Option<…>`
marker or a tiny drill stack). The existing spine/primary cursors
(`spine_cursor`, `fleet_primary_cursor`/`fleet_primary_label`) are preserved so
returning lands where you left.

## Implementation seams (resolve exact shape in the plan)

- **Enter handler** (`main.rs:3760`): add an `AppView::Fleet` arm that branches
  on `state.fleet_region` (Spine → host view; Primary → thread view), plus an
  app→thread arm reachable from the host view.
- **Host view:** the classic dense grammar already gates on
  `AppView::Groups | AppView::Remote` at many key sites (sort main.rs:3626,
  metric 3642, etc.). Prefer **reusing that machinery** — either a fleet-backed
  `AppView::Remote` (data sourced from the fleet snapshot when in fleet mode) or
  a sibling `AppView::FleetHost { host }` added to those same guards. The plan
  picks whichever keeps the data sourcing cleanest; the requirement is that the
  dense grammar works on the host's fleet-streamed snapshot without a second
  connection.
- **Thread view:** reuse `render_threads` / `render_thread_heat`, but source the
  samples from `selected_place_threads()` (already bridges local sampler vs
  remote `focus_threads`) instead of the `AppMode::Local`-gated sampler at
  main.rs:2661. A fleet-backed `AppView::Threads` (or sibling variant) that
  reads the remote stream.
- **`send_focus` routing** (main.rs ~2048): compute the focus `(host, app)` from
  the current drill level, not solely the fleet-face primary cursor.
- **Esc** (`main.rs:3490`): fleet-originated drilled views pop to the fleet face
  (via the recorded return marker), not `Groups`.

## Domain-agnostic

Unchanged constraint ([[aerie-stay-general]]): everything is reported as
hosts / process groups / threads / comm / pid / % — no app/role/desktop
meaning. This feature adds navigation only, no new domain knowledge.

## Testing

- **Unit** where logic is pure: the focus `(host, app)` selection at each drill
  level; the Esc return-target mapping.
- **Integration (tmux, real hosts, [[aerie-test-hosts]]):** against
  `aerie --hosts apollo,milkv --enable-remote` — (1) Spine Enter on a host →
  dense host view of that host's live processes, sort/metric keys work, Esc →
  fleet face; (2) Primary Enter on an app → its per-thread view with real remote
  thread data, Esc → fleet face; (3) host view → Enter on an app → thread view →
  Esc → host view → Esc → fleet face (the full stack). Verify no orphaned
  `ssh`/`aerie --daemon` children and that only the focused host samples threads.

## Out of scope (deferred)

- Drilling below threads (per-thread stacks, syscalls) — the thread view is the
  bottom for now.
- Kube/Nomad region-aware Enter in *their* faces — this spec is the Fleet face;
  the same pattern can extend later.
- Persisting drill state across reconnects.
- A visible breadcrumb of the drill path (nice-to-have; the header can note the
  entered host/app, but a full breadcrumb widget is deferred).

## Related

[[project-aerie]] · [[aerie-stay-general]] · [[aerie-test-hosts]] · [[prefer-mullion-propose-extensions]]
