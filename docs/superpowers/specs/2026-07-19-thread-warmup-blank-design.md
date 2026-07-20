# Remove the thread-view warmup blank — Design

**Date:** 2026-07-19
**Status:** approved (brainstorm) — ready for implementation plan

## Problem

When you drill into an application's per-thread view (fleet host view → app, fleet
face → app, or the local `Enter`-on-group Threads view), the first frame after the
focus changes is blank: it shows `<app> · 0 threads` and "waiting for second
sample…" for ~1 refresh interval (~2 s at `--interval 2`), then populates. Users
read the blank frame as "pressing Enter does nothing."

## Root cause

`local::sample_threads` (src/local.rs) computes per-thread **rate** metrics
(CPU%, faults/s, disk io/s, ctx-switches/s, sched-wait%) from the delta between a
previous and current thread snapshot. On the tick right after the focused group
changes there is no previous snapshot (`prev == None`), so it returns an **empty**
sample list — even though it already read every thread's name/TID/counters into
`rows`. The same happens in the `dt < 1.0` early-return (resample interval too
short). That discarded list is the blank frame.

The daemon (`run_daemon`) resets its `prev_focus_snap` to `None` on every focus
change, so the remote focused stream emits an empty `focus_threads` on the first
tick — the blank the user sees over SSH.

## Change

In `local::sample_threads`, the two no-delta paths return the **full thread list
with rate metrics zeroed** instead of `vec![]`:

- `prev == None` branch: build a `ThreadSample` per row with `name`/`pid`/`tid`
  from `rows` and `cpu_pct = faults_per_s = disk_read_s = disk_write_s =
  ctx_switches_s = sched_wait_pct = 0.0`.
- `dt < 1.0` early-return: same — return the zeroed list (plus the fresh
  `ThreadSnapshot` for next time), not `vec![]`.

Nothing else changes. The `ThreadSnapshot` returned in both cases is already
correct (it's the delta basis for the next tick, which produces real rates).

## Why this is the whole fix

`sample_threads` is the single sampler behind all three thread views:

- the remote daemon's focused stream (`run_daemon` → `focus_threads`),
- the local `AppView::Threads` view (`AppState::refresh`),
- the local fleet-detail pane (`selected_place_threads` in Local mode).

So the one change removes the blank from all three uniformly. `run_daemon` is
unchanged — it emits whatever `sample_threads` returns, which is now a non-empty
list on the first tick. The viewer's "waiting for second sample" message is shown
only when the sample list is empty, so with a non-empty first frame it no longer
appears (it still correctly covers a genuinely thread-less group).

## Behaviour after the change

- First frame: real thread list — names, TIDs, correct `N threads` count — with
  empty bars (0% CPU, 0 rates).
- Next tick (~1 interval): CPU% and the other rates fill in with real values.
- No blank frame, no "waiting for second sample" for a group that has threads.

## Known minor (accepted)

On the first frame every thread reads 0%, so when real CPU% arrives the list
re-sorts once (callers sort by CPU%-desc: `run_daemon` and the local thread
block). This is a one-time "loading → settle," not a per-tick flicker; a stable
sort keeps `/proc`/TID order on the all-zero frame. No mitigation needed;
document it.

## Domain-agnostic

Unchanged ([[aerie-stay-general]]): threads / TIDs / PIDs / CPU% only — no
app-specific meaning. This changes when the list is emitted, not what it means.

## Testing

- **Unit** (`src/local.rs`): `sample_threads(&[own pid], None, &ThreadFields::all(),
  cpu_total)` returns a **non-empty** list (the test process always has ≥1
  thread) with every `cpu_pct == 0.0`; and a `ThreadSnapshot` whose `tids` is
  non-empty. A second call showing the `dt < 1.0` path (pass a `cpu_total` equal
  to the prior snapshot's `total`) likewise returns the zeroed list, not empty.
- **Integration (tmux, real host, [[aerie-test-hosts]]):** `aerie --hosts apollo
  --enable-remote`, drill an app → the thread view shows the real thread list
  **immediately** (correct `N threads`, no "waiting for second sample"), and
  CPU% fills in on the next tick. Confirm the local `Enter`-on-group Threads view
  is likewise blank-free.

## Deploy

This changes the **daemon** (`sample_threads` runs in `aerie --daemon` on the
remote), so apollo/milkv need the new binary. The auto-deploy hook
([[aerie-test-hosts]]) redeploys at end of turn; the fleet daemon picks it up on
the next connection.

## Out of scope

- The double-sample-on-focus approach (real CPU% on frame 1 via a quick
  back-to-back sample) — considered and declined: more daemon complexity and
  added per-focus latency for a ~1-tick accuracy gain.
- Changing the sort so the first frame doesn't re-order — the one-time settle is
  acceptable.

## Related

[[project-aerie]] · [[aerie-stay-general]] · [[aerie-test-hosts]]
