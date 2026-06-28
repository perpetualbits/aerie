<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
# The rim as a latency instrument

The animated glow that orbits aerie's outer border is not just decoration — it
is a live readout of how smoothly aerie's own draw loop is being scheduled by
the system. This document explains the mechanism, the current implementation
("Design 1 — the comet"), how to read it, and the planned next step
("Design 2 — strobe orbiters" for periodic offenders).

The whole idea began as an accident: the orbiting Gaussian visibly *hitched*
whenever the desktop stalled, and that stutter is how the recurring system
latency problem was first noticed. Design 1 turns that accidental tell into a
calibrated gauge.

---

## 1. The mechanism

A blob's position is computed from **wall-clock time**:

```
cw_pos = (t * 2 * BASE) % 1            // yellow, one orbit / 10 s
```

But the blob is only *painted* when the main loop runs `terminal.draw(...)`.
The loop is capped at 20 fps (`RENDER_TICK = 50 ms` in `src/main.rs`) and wakes
earlier for input or data deadlines. So between two paints the blob's *ideal*
position advances by however much wall-clock time passed — and on a healthy
loop that is ~50 ms, an imperceptible step.

When the system stalls (compositor hold-up, scheduler latency, a memory or I/O
freeze) the loop cannot paint. Wall-clock time keeps moving, so on the next
frame the blob **jumps forward** by the whole missed interval. That jump is the
hitch. Formally:

> **An orbiter is a clock hand. Perimeter position is a phase, angular velocity
> is a frequency, and a stall is a phase error.** The size of the jump *is* the
> render-loop latency for that frame.

The rim therefore already measures the latency of the loop that draws it. Design
1 only makes the measurement legible.

---

## 2. Design 1 — the clean orbiter (implemented)

The orbiter is left as a plain Gaussian blob driven by wall-clock time, sweeping
**continuously around the whole rim**. Its own motion is the live signal: when
the loop stalls, no frame is painted, so the blob visibly **freezes and then
jumps** to where the clock moved on. The gap you see — where it stopped, and how
far it skipped — is the stall's *onset* and *duration*. We deliberately do not
dress this up (an earlier version smeared the blob into a "comet" and shattered
it into braille; that read as clutter against the moving glow). The job of
*capturing* the stall falls to the knot (Design 2); Design 1 just keeps the
heartbeat clean and legible.

### Continuous, gap-crossing glow

* `Field::perimeter(area)` (mullion) gives a 1-row strip that walks the border
  clockwise **across all four corners**, so the glow flows around the box without
  breaking at a corner — the same corner-crossing edge strip the `spiral_stress`
  example uses for its sliding border ports.
* The glow is **not** gap-aware: it recolours every cell it reaches, *including
  the legend/status text*, so the blobs sweep over the entire rim without a hole
  where the text sits. Only the foreground colour changes — the underlying glyph
  (box rule or text) is preserved, so the text stays readable, briefly tinted as
  a blob passes.
* Two plain Gaussians (`SIGMA = 0.05`), yellow CW (10 s orbit) and red CCW (4 s),
  added like light where they overlap. No smear, no severity flare — steady.

### Stall detection (drives the knots)

`apply_border_glow` runs once per painted frame, so the gap between two calls
*is* the frame interval. A `RimTrail` static records the previous frame's
timestamp and a decaying **stall severity**:

* Each frame `dt = now − last`; the part of `dt` above `FLOOR_MS = 60 ms`
  (just over the 50 ms render tick, which also filters aerie's own few-ms refresh
  read) feeds the severity.
* Severity rises instantly and **decays smoothly** (`exp(−dt / TAU_S)`,
  `TAU_S = 0.6 s`), then latches an `engaged` flag with hysteresis
  (`BRAILLE_ON ≈ 0.14`, `BRAILLE_OFF ≈ 0.07` of `LEN_MAX`, ≈ a frame 130 ms late).
* `engaged` gates the knot pass: a calm rim is pure orbiter; the knots appear
  only while a stall is actually being felt.

### Known limitations

* **Self-measurement.** The detector reads aerie's *own* draw cadence, which
  includes aerie's refresh cost. `FLOOR_MS` filters the normal few-ms refresh,
  but on a heavily loaded box a slow refresh could contribute. The probe threads
  behind the `d` scope (`LatencyProbe`, `PressureProbe`, `OffenderProbe`) are
  immune to this and remain the authoritative source.
* **Onset/duration not yet quantified on the rim.** Design 1 makes the
  freeze-and-jump *visible* and detects *that* a stall is happening; turning the
  jump into a measured onset-phase and duration that sharpen over passes (and
  separating timing jitter from length variance) is the next step — see §5.
* **Resolution.** The perimeter has a finite number of cells, so very short
  stalls below the floor are intentionally invisible.

---

## 3. Design 2 — periodic-offender knots (implemented)

Design 1 shows *that* the loop stalled and *how long*. Design 2 adds *who* and
*whether it recurs*, bound to `OffenderProbe` + `analyze_periodicity` in
`src/diag.rs`.

### From strobe to phase knot

The original sketch was a stroboscope: give each offender an orbiter whose lap
time equals its period, so its stall lands at the same rim angle every lap and
phase-locks into a stationary knot. The implementation reaches the same place
more directly — **it places a stationary knot at the offender's phase** rather
than animating an orbiter and waiting for it to lock:

* `fundamental_phase(series, freq_hz)` projects the offender's activity series
  onto cos/sin at its detected fundamental, using **absolute** sample times. The
  argument of that projection is the periodic component's phase; normalised to
  `[0, 1)` it is a rim position. Referencing absolute time (not the sliding
  analysis window) makes the phase **stable**: a genuinely periodic offender
  yields a near-constant value, so its knot holds still; a drifting period makes
  the value precess. This is stored as `Offender::phase` and unit-tested for both
  recovery accuracy and window-stability.
* The rim draws one knot per confident offender (top 3, `confidence ≥ 0.30`) at
  `phase`, as a tight braille mark (`KNOT_SIGMA = 0.015`). **Hue** encodes the
  kind — cyan for `Spawns`, violet for `CpuBurst` — a deliberately different
  palette from the yellow/red latency orbiters, so the diagnostic layer reads
  apart from the ambient one. **Fill height / brightness** encodes confidence
  (via the bottom-anchored `comet_braille_mask`), pulsed up by the live stall
  severity. Knots are drawn after the orbiters, so they ride on top.
* **Stall-gated.** The knots are drawn *only while a stall is currently being
  felt* (`engaged`). A calm rim is pure orbiter; a knot blinking on at a fixed
  angle exactly as the lag hits reads as "this is the thing stalling you, and
  it's happening right now" — and because it is gated, it cannot clutter the rim
  when nothing is wrong.

So the geometric reading the stroboscope promised falls out directly:

* perfectly periodic → a **stationary** knot;
* jittery / quasi-periodic → a knot that **wanders within a small arc** (the
  per-window phase wobble);
* drifting period → a knot that **slowly precesses** (the estimate is detuned).

### When the data is available

The offender report is refreshed at ~1 Hz whenever the offender probe is alive —
both inside the scope view and, via a dedicated block in the main loop, outside
it. The probe is spawned the first time the scope is opened with `d` (or at
startup under `--scope-log`) and keeps scanning afterwards, so once you have
peeked at the scope the knots are available in every view (shown whenever a stall
is being felt). They do **not** appear before the probe has ever run; making the
probe start at boot for everyone is a separate policy choice, deliberately not
taken here.

### Not yet done

* **Hue by subsystem.** Knot hue is the offender *kind*, not yet the `Attributor`
  suspect (IRQ vs io vs mem). Wiring the latency/pressure channel distinction
  into knot colour is the natural next refinement.
* **Additive compositing.** A knot core overwrites the orbiter cell beneath it
  rather than blending; fine in practice because knots are tight and bright.
* **Onset/duration split.** The knot marks *where/whether* it recurs, not yet the
  separate shapes of onset-timing jitter vs duration variance — see §5.

### Staying domain-agnostic

Like the rest of the Instruments subsystem, the rim reports only the **shape** of
a problem — period via angle, magnitude via brightness, kind/subsystem via hue,
periodicity-quality via stationarity. It never names a product or suggests a fix;
a human reads the knot and decides what it is.

---

## 4. Reading the rim — quick manual

| What you see on the border        | What it means |
|-----------------------------------|---------------|
| Two smooth blobs gliding evenly around the rim | Healthy. The draw loop is being scheduled on time (< 60 ms/frame). |
| A blob **freezes, then jumps ahead** | A stall. It stopped where the freeze began; the size of the jump is roughly how long it stalled. |
| A **cyan or violet braille knot** blinks on at a fixed angle during the stall (needs `d` opened once) | The periodic offender behind it. **Violet** = periodic CPU bursts, **cyan** = periodically spawning helpers; taller/brighter = more confident. A knot at the *same* angle each stall = cleanly periodic. |
| The knot **wanders in a small arc** or **slowly drifts** | The offender is quasi-periodic (arc = period jitter) or its period is slowly changing (drift = detuning). |

When the rim stutters, press **`d`** to open the latency scope, where
`LatencyProbe` / `PressureProbe` / `OffenderProbe` quantify the wakeup jitter,
system pressure, and any periodic offender — the authoritative read behind the
ambient hint on the border.

---

## 5. Next: the stutter characteriser

Design 1 makes a stall *visible* and Design 2 marks *whether/where* it recurs.
The open work is to characterise the **shape** of the recurring stutter directly
from the freeze-and-jump events, and to do it with the rim's two usable axes so
two independent properties read at once:

* **Capture events.** Extend `RimTrail` into a small ring of recent stalls, each
  `{ onset_t, duration }` taken straight from the frame-cadence freeze (already
  measured) — self-contained, no probe required.
* **Onset timing → rim angle.** Fold onsets by the detected period (reuse
  `analyze_periodicity` / `fundamental_phase`). The fold **sharpens over passes**
  as more cycles confirm the period — the "second or third pass becomes more
  precise" behaviour. A tight angular cluster = precise onset timing; an arc =
  onset jitter (the process starts a bit early/late each time).
* **Duration → braille height.** At that angle, the braille fill height encodes
  the stall length, and its variation encodes length variance. So the two failure
  modes separate cleanly: *fixed onset / varying length* = a knot steady in angle
  but breathing in height; *jittery onset / fixed length* = a knot smeared in
  angle but steady in height.
* **Patterns.** A "long burst then short bursts" signature shows as a tall knot
  with a fixed train of shorter knots at characteristic angular offsets.

The goal is recognition, not attribution: make the long-standing desktop stutter
*obvious* and give its fingerprint (period, onset jitter, duration shape) so a
human can act on it — file the bug, switch the offending software, or push the
desktop's maintainers to fix the single-threaded main-loop stalls that cause it.
