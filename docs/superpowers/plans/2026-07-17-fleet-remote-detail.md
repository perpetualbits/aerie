# Fleet remote — Slice C: remote thread detail (focused-stream routing) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the Fleet-face detail show the selected group's live per-thread heatmap for a REMOTE place, completing local/remote parity: route a focus request to the selected remote host (Slice A's `send_focus`) and render its streamed `focus_threads` in the detail (via the shared `render_thread_heat`).

**Architecture:** Viewer-side only — the daemon already produces `focus_threads` (Slice A, deployed on apollo/milkv). Two pieces: (1) each refresh, tell the spine-selected remote daemon which group to focus (`send_focus(selected_group)`), and the others to stop (`send_focus(None)`); (2) the detail renderer reads the selected place's threads — the local sampler's `fleet_detail_samples` in Local mode, or the selected host's `snap.focus_threads` in Fleet mode.

**Tech Stack:** Rust (edition 2021); existing `remote::RemoteClient::send_focus`, `DaemonSnapshot.focus_threads`, `render_thread_heat`. NO new deps. Verified against a REAL host (apollo) — no redeploy needed (daemon-side is already live).

## Global Constraints

- Edition 2021; MSRV rustc 1.85. NO new dependencies.
- Additive / non-regressing: Local-mode Fleet face detail (the existing `fleet_detail_samples` pipeline) unchanged; the classic Fleet host-list view and all other views unchanged; 90 tests stay green; domain-agnostic (comm/tid/%/counts).
- **Efficiency:** only the SELECTED remote host samples threads — send `send_focus(None)` to unselected hosts so they don't waste remote CPU. `send_focus` already dedups (no rewrite when unchanged), so calling it every refresh is cheap.
- **Verification:** exercised end-to-end against apollo (`aerie --hosts apollo --enable-remote`): select a group on apollo, confirm its live thread heatmap appears in the detail. Remote cpu% has a ~2-3 refresh + network warmup after each group change (daemon resets its focus delta) — expected, wait for it.
- Reuse points (verified): `FleetClient::{Daemon(RemoteClient), Thin(ThinProbe)}` (main.rs:801); `RemoteClient::send_focus(&mut self, group: Option<&str>)` (remote.rs:141) — Daemon only, no-op for Thin (ThinProbe has no such method); the fleet poll loop `for conn in &mut self.fleet_clients { ... }` (main.rs:1940-1960, ends ~1960 before the `raw` build at 1961); `selected_fleet_group_label(&self) -> Option<String>` (main.rs — returns the selected place's selected group via `selected_place_entries()[fleet_primary_cursor]`, so for a remote place it's that host's group); `state.spine_cursor`; `DaemonSnapshot.focus_threads: Option<(String, Vec<local::ThreadSample>)>` (remote.rs); `render_fleet` detail region (ui.rs:1293-1308); `render_thread_heat(buf, area, &[local::ThreadSample])` (ui.rs).

---

### Task 1: Route focus to the selected remote host

**Files:**
- Modify: `src/main.rs` (the fleet poll loop in `refresh`, right after the `for conn in &mut self.fleet_clients { ... }` loop ends ~line 1960)

**Interfaces:**
- Consumes: `selected_fleet_group_label()`, `state.spine_cursor`, `FleetClient::Daemon`, `RemoteClient::send_focus`.

- [ ] **Step 1: Add the focus-routing block** immediately AFTER the `for conn in &mut self.fleet_clients { ... }` poll loop (which updates `conn.snap` via `try_recv`), and BEFORE the `let raw: Vec<BarEntry> = ...` build (~line 1961). This must come after the loop so `snap.entries` (which `selected_fleet_group_label` reads for a remote place) is fresh this tick:

```rust
            // Focused-stream routing (Slice C): tell the spine-selected remote
            // daemon which group to stream per-thread data for; tell the others
            // to stop. `send_focus` dedups, so this is cheap every tick, and it
            // only ever asks ONE host to sample threads.
            let focus_group = self.selected_fleet_group_label(); // owned Option<String>
            let sel = self.spine_cursor;
            for (i, conn) in self.fleet_clients.iter_mut().enumerate() {
                if let Some(FleetClient::Daemon(rc)) = conn.client.as_mut() {
                    rc.send_focus(if i == sel { focus_group.as_deref() } else { None });
                }
            }
```
Note: `selected_fleet_group_label()` returns an OWNED `Option<String>` (it clones the label), so the immutable borrow it takes ends before the `iter_mut()` — no borrow conflict. Confirm this compiles; if `selected_fleet_group_label` were to return a borrow, bind the label to an owned `String` first.

- [ ] **Step 2: Build + tests**

Run: `cargo build --bin aerie && cargo test --bin aerie`
Expected: builds clean; 90 tests pass. (No unit test for this routing — it's verified end-to-end in Task 2's step against a real host; but confirm the build and that no existing test regressed.)

- [ ] **Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat(fleet): route focused-stream to the spine-selected remote host"
```

---

### Task 2: Render the selected place's threads in the detail (local or remote)

**Files:**
- Modify: `src/main.rs` (add a `selected_place_threads` accessor on `AppState`)
- Modify: `src/ui.rs` (`render_fleet` detail region ~1293)
- Verify: tmux against apollo (real remote thread heatmap)

**Interfaces:**
- Produces: `AppState::selected_place_threads(&self) -> (Option<&str>, &[local::ThreadSample])`.

- [ ] **Step 1: Add the accessor** on `AppState` (near `selected_place_entries` / `selected_fleet_group_label` in `src/main.rs`):

```rust
    /// The (group label, per-thread samples) for the Fleet detail of the
    /// currently-selected place: the local sampler's output in Local mode, or
    /// the spine-selected remote host's streamed `focus_threads` in Fleet mode.
    /// Empty when nothing is focused or the remote host has no focused sample yet.
    fn selected_place_threads(&self) -> (Option<&str>, &[local::ThreadSample]) {
        if let AppMode::Fleet { .. } = self.mode {
            match self.fleet_clients.get(self.spine_cursor)
                .and_then(|c| c.snap.as_ref())
                .and_then(|s| s.focus_threads.as_ref())
            {
                Some((label, samples)) => (Some(label.as_str()), samples.as_slice()),
                None => (None, &[]),
            }
        } else {
            (self.fleet_detail_label.as_deref(), self.fleet_detail_samples.as_slice())
        }
    }
```

- [ ] **Step 2: Use it in `render_fleet`'s detail region** (`src/ui.rs` ~1293). Replace the `fleet_detail_label`/`fleet_detail_samples` reads with the accessor so it works for local AND remote:

```rust
    // Detail: the selected group's live per-thread heatmap (monitor lens) —
    // local sampler in Local mode, remote focus_threads in Fleet mode.
    if let Some(detail) = rect_of(DETAIL_ID) {
        if detail.height >= 2 {
            let (label, samples) = state.selected_place_threads();
            let header = match label {
                Some(l) => {
                    let n = samples.len();
                    let unit = if n == 1 { "thread" } else { "threads" };
                    format!(" {l} · {n} {unit}")
                }
                None => " (no group selected)".to_string(),
            };
            buf.set_string(detail.x, detail.y, &header.chars().take(detail.width as usize).collect::<String>(),
                Style::default().fg(Color::Gray));
            let heat = Rect::new(detail.x, detail.y + 1, detail.width, detail.height - 1);
            render_thread_heat(buf, heat, samples);
        }
    }
```

- [ ] **Step 3: Build + tests**

Run: `cargo build --bin aerie && cargo test --bin aerie`
Expected: builds clean; 90 tests pass.

- [ ] **Step 4: Verify against a REAL remote host (apollo) — the payoff.** No redeploy needed (apollo's daemon already streams `focus_threads`). Connect, focus the spine on apollo, focus the primary, select a group, wait for the remote warmup, and confirm the detail shows that group's live thread heatmap:

```bash
cargo build --bin aerie
SOCK=/tmp/aerie-sc.sock
tmux -S "$SOCK" kill-server 2>/dev/null
tmux -S "$SOCK" new-session -d -s a -x 200 -y 50 "./target/debug/aerie --hosts apollo --enable-remote --ssh-accept-new --interval 2"
sleep 8                                   # connect + snapshots
tmux -S "$SOCK" send-keys -t a f ; sleep 1        # Fleet face (focus Primary)
tmux -S "$SOCK" send-keys -t a Down ; sleep 6     # select a group; wait ~3 refreshes for remote focus warmup
echo "=== detail = the selected apollo group's LIVE thread heatmap ==="
tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,14p'
tmux -S "$SOCK" kill-server 2>/dev/null
```
Expected: the detail (right region) header names the selected group (e.g. ` steam · N threads`) and shows a heat grid of N cells — the group's threads sampled ON apollo and streamed back via `focus_threads`. (If it shows `0 threads`/blank, wait longer — the daemon needs a couple ticks after the focus request + network round-trip; try another `Down` and re-capture.) Paste the frame. Also confirm the LOCAL-mode Fleet face detail still works (`aerie` alone, press f, select a group → local thread heatmap) — unregressed. Clean up: kill the tmux server; ensure no leftover `ssh`/`aerie --daemon` children linger on dop561 (`pgrep -af 'ssh .*apollo'`).

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/ui.rs
git commit -m "feat(fleet): detail renders the selected place's threads (local sampler or remote focus_threads)"
```

---

## Self-Review

**Spec coverage:** the fleet detail now shows the selected group's live per-thread heatmap for BOTH local and remote places — Task 1 routes the focus request to the spine-selected remote daemon (others told to stop), Task 2's accessor + render pull the threads from the local sampler (Local mode) or the remote `focus_threads` (Fleet mode) and draw them with the shared `render_thread_heat`. Verified end-to-end against apollo. This completes the fractal: fleet → host → group → its live threads, over SSH. Out of scope (correctly): health glyphs / Tier signals on the spine (P2, where the mullion extensions land); a unified local+remote place tree; thin-probe thread detail (thin has no daemon → no focus_threads, correctly shows "(no group selected)").

**Placeholder scan:** no TBD/TODO; new code shown in full.

**Type consistency:** `selected_place_threads() -> (Option<&str>, &[ThreadSample])` (Task 2) is consumed in `render_fleet`; matches `render_thread_heat(&[ThreadSample])`; `focus_threads: Option<(String, Vec<ThreadSample>)>` destructured as `(label, samples)`; `send_focus(Option<&str>)` fed `focus_group.as_deref()` (Task 1). `spine_cursor`→`fleet_clients` index is the same positional mapping Slice B established (send-focus target, entries source, and threads source all use `spine_cursor`, so they stay coherent).
