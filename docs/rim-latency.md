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

## 2. Design 1 — the comet (implemented)

Instead of teleporting a round blob, we **smear** it into a comet whose trailing
length equals the latency of the frame that just elapsed.

* `apply_border_glow` runs once per painted frame, so the gap between two calls
  *is* the frame interval. A `RimTrail` static records the previous frame's
  timestamp and the current comet length.
* Each frame: `dt = now − last`. The **slow part** of `dt` above a floor becomes
  the comet length, in fractions of the perimeter.
* Length rises instantly and **decays smoothly** (exponential, `TAU_S = 0.6 s`),
  so a single long frame leaves a ~1 s fading comet the eye can catch rather
  than a one-frame flash.
* Both orbiters share the same length because a loop stall is **common-mode** —
  it delays the whole render equally, not one blob. (Per-offender phase is
  Design 2's job.)
* As the comet grows it flares brighter and gains a touch of blue (a warm-white
  head), so a stall is unmistakable at a glance.

### Rendering: braille dots around the whole perimeter

A faint colour smear turned out too subtle to read against the smoothly orbiting
glow, so the comet is drawn as a **structural** change, not just a colour one.

* `Field::perimeter(area)` (mullion) gives a 1-row strip that walks the border
  clockwise **across all four corners**, so the comet flows around the box
  without breaking at a corner — the same corner-crossing edge technique the
  `spiral_stress` example uses for its sliding border ports.
* **Two looks, by severity.** At rest the orbiters are the original **smooth
  solid glow** — the existing box glyph is simply recoloured. Only a real stall
  *engages* the braille comet: the glow shatters into travelling dots, then
  reforms as the comet decays. The switch is latched with hysteresis
  (`BRAILLE_ON ≈ 0.14`, `BRAILLE_OFF ≈ 0.07` of `LEN_MAX`, ≈ a frame 130 ms late)
  so a severity hovering near the trigger does not flicker the two looks.
* When engaged, each cell the comet reaches is replaced by a **braille glyph**
  (a 2×4 dot matrix) whose lit dots encode the local intensity; cells it does not
  reach keep their box-drawing glyph. Legend / status / key gaps are skipped just
  as `render_rim` would (`!gap.rim_glow && gap.contains`).
* `comet_braille_mask(amp, col)` fills the cell **bottom-anchored** — the bottom
  dot row lights first, the top row last — and Bayer-dithers the partial rows:
  * On the **horizontal** top/bottom edges the lit height reads as a little
    level-meter bar — the *height* encoding.
  * As the tail fades, the dither thins the dots into ever-sparser specks — the
    *distance between them* growing with the hold-up.
  * On the **vertical** side edges a within-cell height bar would not read
    (travel is vertical there), so the signal is carried by the comet's *length*
    along the rim instead; the same mask still renders as a coherent dot cluster.
* Colour is still the green→red heat plus a white-hot lift under stress, applied
  as the braille cell's foreground.

### Geometry: `smear_intensity`

A comet is a Gaussian blob stretched along its **trailing** arc — the side the
blob came from. `smear_intensity(p, head, len, dir, sigma)`:

```
behind = (dir * (head − p)) mod 1      // how far p sits *behind* the head
if behind ≤ len:  d = 0                // p is on the swept trail → solid core
else:             d = min distance to head or tail endpoint
return gaussian(d, sigma)
```

* `head` — the blob's leading edge (its instantaneous orbit position).
* `dir` — `+1` clockwise (trail extends toward smaller `p`), `−1`
  counter-clockwise (trail extends toward larger `p`).
* With `len == 0` this reduces *exactly* to the original point blob
  `gaussian(d, sigma)`, so a healthy loop is visually identical to the old glow.
* The arithmetic is modular, so a comet whose tail crosses the `0/1` seam stays
  continuous. (All three properties are covered by unit tests in `src/ui.rs`.)

### Calibration constants (`apply_border_glow`)

| Constant        | Value     | Meaning |
|-----------------|-----------|---------|
| `FLOOR_MS`      | `60 ms`   | Below this the loop is healthy → no smear. Sits just above the 50 ms render tick, and conveniently filters aerie's own few-ms refresh read so a routine `/proc` scan is not mistaken for a stall. |
| `MS_PER_PERIM`  | `0.0006`  | Gain: 200 ms over the floor ≈ 0.08 of the perimeter; 500 ms ≈ 0.26. |
| `LEN_MAX`       | `0.33`    | A comet wraps at most a third of the rim, however long the stall. |
| `TAU_S`         | `0.6 s`   | Comet fade time-constant; frame-rate independent (`exp(−dt/τ)`). |
| `SIGMA`         | `0.05`    | Blob half-width (unchanged from the original glow). |

The gain is a *display* choice — the rim is a gauge tuned for readability, like
the braille scope traces that normalise to a ceiling. Tune `MS_PER_PERIM` /
`FLOOR_MS` to taste.

### Known limitations

* **Self-measurement.** The rim measures aerie's *own* draw cadence, which
  includes aerie's refresh cost. `FLOOR_MS` filters the normal few-ms refresh,
  but on a heavily loaded box a slow refresh could contribute. The probe threads
  behind the `d` scope (`LatencyProbe`, `PressureProbe`, `OffenderProbe`) are
  immune to this and remain the authoritative source.
* **Common-mode only.** Design 1 shows *that* the loop stalled and *how long* —
  not *who* caused it or *whether it recurs*. That is Design 2.
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
  (via the same bottom-anchored `comet_braille_mask`). Knots are drawn after the
  orbiters, so they ride on top.

So the geometric reading the stroboscope promised falls out directly:

* perfectly periodic → a **stationary** knot;
* jittery / quasi-periodic → a knot that **wanders within a small arc** (the
  per-window phase wobble);
* drifting period → a knot that **slowly precesses** (the estimate is detuned).

### Ambient vs diagnostic

The knots are **ambient**: the offender report is refreshed at ~1 Hz whenever the
offender probe is alive — both inside the scope view and, via a dedicated block
in the main loop, outside it. The probe is spawned the first time the scope is
opened with `d` (or at startup under `--scope-log`), and keeps scanning
afterwards, so once you have peeked at the scope the knots persist on the border
in every view. (They do **not** appear before the probe has ever run; making the
probe start at boot for everyone is a separate policy choice, deliberately not
taken here.)

### Not yet done

* **Hue by subsystem.** Knot hue is the offender *kind*, not yet the `Attributor`
  suspect (IRQ vs io vs mem). Wiring the latency/pressure channel distinction
  into knot colour is the natural next refinement.
* **Additive compositing.** A knot core overwrites the orbiter cell beneath it
  rather than blending; fine in practice because knots are tight and bright.

### Staying domain-agnostic

Like the rest of the Instruments subsystem, the rim reports only the **shape** of
a problem — period via angle, magnitude via brightness, kind/subsystem via hue,
periodicity-quality via stationarity. It never names a product or suggests a fix;
a human reads the knot and decides what it is.

### Staying domain-agnostic

Like the rest of the Instruments subsystem, the rim reports only the **shape** of
a problem — period via angle, magnitude via brightness, subsystem-category via
hue, periodicity-quality via stationarity. It never names a product or suggests a
fix; a human reads the knot and decides what it is.

---

## 4. Reading the rim — quick manual

| What you see on the border        | What it means |
|-----------------------------------|---------------|
| Two smooth solid blobs gliding | Healthy. The draw loop is being scheduled on time (< 60 ms/frame). |
| A blob **shatters into a comet of braille dots** | A real stall hit (≳ 130 ms frame). Longer comet = longer hold-up. On the top/bottom edges the dot **height** rises with it. |
| The tail thins into **sparse, spread-out dots** | The fading edge of the comet — wider dot gaps mean a longer hold-up. |
| The comet turns **white-hot**     | A large stall (hundreds of ms). |
| Comets stretch and fade roughly **rhythmically** | Something on the system stalls the render on a cycle — open the latency scope to identify it. |
| A **cyan or violet knot** sits on the rim (after you've opened `d` once) | A periodic offender. **Stationary** = cleanly periodic; **violet** = periodic CPU bursts, **cyan** = periodically spawning helpers; taller/brighter = more confident. |
| A knot **wanders in a small arc** or **slowly drifts** around the rim | The offender is quasi-periodic (arc = period jitter) or its period is slowly changing (drift = detuning). |

When the rim stutters, press **`d`** to open the latency scope, where
`LatencyProbe` / `PressureProbe` / `OffenderProbe` quantify the wakeup jitter,
system pressure, and any periodic offender — the authoritative read behind the
ambient hint on the border.
