# Remove the thread-view warmup blank Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When a thread view first opens (no CPU delta yet), show the real thread list with rate metrics at 0 instead of a blank "waiting for second sample" frame; CPU% fills in on the next tick.

**Architecture:** One change to `local::sample_threads` (src/local.rs): its two no-delta paths (`prev == None`, and the `dt < 1.0` early-return) return the full thread list built from the already-read `rows`, with rate metrics zeroed, rather than `vec![]`. `run_daemon` and all viewers are unchanged — they emit/render whatever `sample_threads` returns.

**Tech Stack:** Rust (edition 2021), single crate (aerie), single file (src/local.rs). No new deps.

## Global Constraints

- Edition 2021; MSRV 1.85. NO new dependencies.
- **Additive / non-regressing:** the `Some(prev)` delta path (real CPU%) is unchanged; the returned `ThreadSnapshot` (delta basis) is unchanged; existing tests stay green. Do NOT touch the look-alike `vec![]` sites in `pub fn sample` (the group-level sampler, ~src/local.rs:1044/1051) — only the two inside `pub fn sample_threads`.
- **Domain-agnostic** ([[aerie-stay-general]]): threads/TIDs/PIDs/CPU% only.
- **Daemon change:** `sample_threads` runs in `aerie --daemon`, so apollo/milkv need the rebuilt binary (auto-deploy hook handles it; the fleet daemon picks it up on the next connection). [[aerie-test-hosts]]

## File Structure

- `src/local.rs` — add a private `zeroed_thread_samples` helper; use it at the two no-delta return sites in `sample_threads`; add a unit test in the existing `#[cfg(test)] mod tests` (line ~1551).

---

### Task 1: `sample_threads` emits the zeroed thread list instead of a blank

**Files:**
- Modify: `src/local.rs` (`sample_threads` two return sites + a new helper)
- Test: `src/local.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `fn zeroed_thread_samples(rows: &[(u32, i32, String, TidCounters)]) -> Vec<ThreadSample>` (private).
- Unchanged public signature: `sample_threads(pids, prev, fields, cpu_total) -> Result<(Vec<ThreadSample>, ThreadSnapshot)>`.

- [ ] **Step 1: Write the failing test.** Add to the `mod tests` in `src/local.rs` (`use super::*;` is already in scope there):

```rust
    #[test]
    fn sample_threads_first_frame_lists_threads_with_zero_rates() {
        // First frame after a focus change: prev == None, so there's no CPU
        // delta yet — but the thread list must appear immediately (not blank),
        // with rate metrics at 0. The test process always has >= 1 thread.
        let pid = std::process::id();
        let (samples, snap) =
            sample_threads(&[pid], None, &ThreadFields::all(), 1000).unwrap();
        assert!(!samples.is_empty(), "first frame must list the group's threads");
        assert!(samples.iter().all(|s| s.cpu_pct == 0.0), "no delta yet -> cpu% 0");
        assert!(!snap.tids.is_empty(), "snapshot carries the delta basis for next tick");

        // dt < 1.0 path: cpu_total unchanged from snap.total -> still lists, not blank.
        let (samples2, _) =
            sample_threads(&[pid], Some(snap), &ThreadFields::all(), 1000).unwrap();
        assert!(!samples2.is_empty(), "dt<1.0 must also list threads, not blank");
        assert!(samples2.iter().all(|s| s.cpu_pct == 0.0));
    }
```

- [ ] **Step 2: Run it — expect FAIL.**

Run: `cargo test --bin aerie sample_threads_first_frame 2>&1 | tail -20`
Expected: FAIL — the assertions `!samples.is_empty()` fail (current code returns `vec![]` for both the `prev == None` and `dt < 1.0` paths).

- [ ] **Step 3: Add the helper.** In `src/local.rs`, just above `pub fn sample_threads` (or immediately after it), add:

```rust
/// Build thread samples with rate metrics zeroed — used on a frame where the
/// threads were read but no previous snapshot is available to compute a CPU/rate
/// delta (the first tick after a focus change, or too-short resample interval).
/// Showing the list immediately (0% bars) beats a blank "waiting" frame; the
/// next tick fills real rates.
fn zeroed_thread_samples(rows: &[(u32, i32, String, TidCounters)]) -> Vec<ThreadSample> {
    rows.iter()
        .map(|(pid, tid, name, _c)| ThreadSample {
            pid: *pid,
            tid: *tid,
            name: name.clone(),
            cpu_pct: 0.0,
            faults_per_s: 0.0,
            disk_read_s: 0.0,
            disk_write_s: 0.0,
            ctx_switches_s: 0.0,
            sched_wait_pct: 0.0,
        })
        .collect()
}
```

- [ ] **Step 4: Use it at the `dt < 1.0` early-return.** In `sample_threads`, find the early return inside the `Some(prev)` arm (search `if dt < 1.0`). It currently consumes `rows` into `tids` and returns `vec![]`:

```rust
            if dt < 1.0 {
                // Interval too short to compute meaningful CPU%; return snapshot for next time.
                let tids = rows.into_iter().map(|(_, tid, _, c)| (tid, c)).collect();
                return Ok((vec![], ThreadSnapshot { total: cpu_total, collected_at, tids }));
            }
```

Replace with (build the zeroed list from `rows` BEFORE consuming `rows` into `tids`):

```rust
            if dt < 1.0 {
                // Interval too short for a CPU delta: show the list with 0 rates
                // (not blank); the snapshot still seeds the next tick's real rates.
                let samples = zeroed_thread_samples(&rows);
                let tids = rows.into_iter().map(|(_, tid, _, c)| (tid, c)).collect();
                return Ok((samples, ThreadSnapshot { total: cpu_total, collected_at, tids }));
            }
```

- [ ] **Step 5: Use it at the `prev == None` arm.** In the same function, find the `match prev { None => vec![], ... }` (search `None => vec![],` — the one INSIDE `sample_threads`, not the one in `pub fn sample`). Replace just the `None` arm:

```rust
    let samples = match prev {
        None => vec![],
```

becomes:

```rust
    let samples = match prev {
        None => zeroed_thread_samples(&rows),
```

(The `Some(prev) => { … }` arm and everything after — the `let tids = rows.into_iter()…` that consumes `rows` — stay exactly as they are. `zeroed_thread_samples(&rows)` borrows `rows`; that borrow ends before the later `rows.into_iter()`.)

- [ ] **Step 6: Run the new test + full suite.**

Run: `cargo test --bin aerie sample_threads_first_frame 2>&1 | tail -10` then `cargo build --bin aerie && cargo test --bin aerie 2>&1 | tail -5`
Expected: the new test passes; the full suite stays green; build warning-clean.

- [ ] **Step 7: Verify against a real host (the payoff).** No blank frame when drilling an app, and CPU% fills in next tick:

```bash
cargo build --bin aerie
SOCK=/tmp/aerie-warmup.sock
tmux -S "$SOCK" kill-server 2>/dev/null
# Build the daemon into ~/aerie-build on apollo so the REMOTE runs this change too,
# OR (simpler for the viewer+local paths) test the LOCAL thread view first:
tmux -S "$SOCK" new-session -d -s a -x 200 -y 50 "./target/debug/aerie --interval 2"
sleep 3
tmux -S "$SOCK" send-keys -t a Down ; sleep 1        # select a group
tmux -S "$SOCK" send-keys -t a Enter ; sleep 1       # local Threads view — FIRST frame
echo "=== local thread view, ~1s after Enter (expect the thread LIST, not '0 threads/waiting') ==="
tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,8p'
sleep 3
echo "=== ~4s later (cpu% now populated) ==="
tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,8p'
tmux -S "$SOCK" kill-server 2>/dev/null
```
Expected (local path proves the shared sampler fix without a redeploy): the first capture already shows the group's real thread rows (names/tids, correct `N threads`) with 0% bars — NOT `0 threads` / "waiting for second sample"; the second shows real CPU%. Paste both frames.

**Remote path** needs the daemon rebuilt on apollo. If the auto-deploy hook has already pushed this branch's binary, also verify over SSH: `./target/debug/aerie --hosts apollo --enable-remote --ssh-accept-new --interval 2`, drill an app, confirm the remote thread view's first frame lists threads immediately. If apollo still runs the old daemon, note that the remote verification is pending redeploy (the local capture already exercises the identical `sample_threads` code path).

- [ ] **Step 8: Commit.**

```bash
git add src/local.rs
git commit -m "fix(threads): show the thread list with 0 rates on the first frame, no warmup blank"
```

---

## Self-Review

**Spec coverage:** the two no-delta paths in `sample_threads` (`prev == None`, `dt < 1.0`) now return the zeroed thread list via `zeroed_thread_samples` (Steps 4-5); the `Some(prev)` real-rate path and the returned `ThreadSnapshot` are untouched; the group-level `sample()` look-alikes are explicitly out of scope. Fixes daemon + local Threads + fleet detail uniformly (all route through this function). ✓

**Placeholder scan:** no TBD/TODO; helper and both edits shown in full.

**Type consistency:** `zeroed_thread_samples(&[(u32, i32, String, TidCounters)]) -> Vec<ThreadSample>` — the row-tuple type matches `rows: Vec<(u32, i32, String, TidCounters)>` (src/local.rs:1224); `ThreadSample`'s 9 fields (pid/tid/name/cpu_pct/faults_per_s/disk_read_s/disk_write_s/ctx_switches_s/sched_wait_pct) are all set. Borrow: `zeroed_thread_samples(&rows)` borrows `rows`; the later `rows.into_iter()` (both sites) runs after that borrow ends — no move conflict.
