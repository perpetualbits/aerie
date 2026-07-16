# Fleet detail = selected group's thread heatmap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fill the (currently empty) Fleet detail region with the selected primary group's live per-thread CPU heatmap — the monitor lens — completing the context+detail experience of the navigable face over the local place.

**Architecture:** Mirror aerie's existing Threads-view pipeline, keyed to the Fleet primary selection instead of a drilled group. A per-refresh block computes `Vec<ThreadSample>` for the selected group via the existing `local::sample_threads`, kept in dedicated `AppState` fields with their own prev-snapshot (reset on selection change). The heat-grid drawing is extracted from `render_threads` into a shared `render_thread_heat` helper so the Threads view and the Fleet detail render an identical heatmap. `render_fleet`'s detail region then draws a header + that helper.

**Tech Stack:** Rust (edition 2021), existing aerie modules (`local::sample_threads`, `AppState`), mullion (existing APIs), tmux pty verification.

## Global Constraints

- Edition 2021; MSRV rustc 1.85. No new dependencies.
- **Additive / non-regressing:** the existing Threads view (`d`-drill → per-thread heatmap) must render **identically** after the `render_thread_heat` extraction. The existing test suite (82 `--bin aerie` tests) stays green. All other views/keybindings unchanged.
- **Prefer mullion:** reuse existing mullion drawing primitives already used by `render_threads`; no new aerie-local drawing abstractions beyond the shared helper.
- **Domain-agnostic:** thread/group labels are comm strings, tids, %, counts only.
- **Verification discipline:** render tasks are verified by tmux pty capture, not "compiles". Threads-view parity (Task 1) is verified by before/after capture; the Fleet detail (Task 3) by keystroke-driven capture (`f`, select a group, observe the detail heatmap).
- Reuse points (verified): `local::sample_threads(pids: &[u32], prev: Option<local::ThreadSnapshot>, fields: &local::ThreadFields, cpu_total: u64) -> Result<(Vec<local::ThreadSample>, local::ThreadSnapshot)>` (src/local.rs:1214); the Threads per-refresh block (src/main.rs:2340-2369) is the exact pattern to mirror; selection = `state.body_tree.focus()` (a `TileId`) matched against `mullion::tree::id_from_key(&entry.label)` (as `render_body` does at src/ui.rs:1372); selected group's pids = `state.snap.groups[label].pids`.

---

### Task 1: Extract `render_thread_heat` from `render_threads`

**Files:**
- Modify: `src/ui.rs` (`render_threads`, currently starting at line 1521)
- Verify: tmux before/after parity of the Threads view

**Interfaces:**
- Produces: `fn render_thread_heat(buf: &mut Buffer, area: Rect, samples: &[crate::local::ThreadSample])` — draws ONLY the per-thread heat grid (the cell grid of `█`/`◻`/`░` colored by cpu%) into `area`, using the same grouping/coloring logic `render_threads` currently uses inline. Returns nothing.

- [ ] **Step 1: Read `render_threads` and identify the heat-grid block.** Read `src/ui.rs` from `fn render_threads` (line 1521) through the end of the function. Locate the self-contained block that: computes `group_size`/`num_cells`/`cell_cpus`/`heat_rows` from the samples and draws the grid of heat cells into the heat sub-rect. That block (from the `group_size`/`cell_cpus` computation through the cell-drawing loop) is what moves. The info line, the divider, and the per-thread list stay in `render_threads`.

- [ ] **Step 2: Add `render_thread_heat`** next to `render_threads` in `src/ui.rs`. Move the identified heat-grid block into it verbatim, parameterized by `area` (the heat sub-rect) and `samples` (`&[ThreadSample]`) instead of reading `state.thread_samples` and the locally-computed heat rect. Keep every constant (e.g. `MAX_HEAT_ROWS`), glyph (`█`/`◻`/`░`), and the `planck_color`/cpu-fraction coloring exactly as they were — this must be a pure move, no behavior change.

- [ ] **Step 3: Replace the inline block in `render_threads` with a call** to `render_thread_heat(buf, heat_rect, &state.thread_samples)` (using whatever local variable `render_threads` already computes for the heat sub-rect). Leave the info line, divider, and thread list untouched.

- [ ] **Step 4: Build + existing tests**

Run: `cargo build --bin aerie && cargo test --bin aerie`
Expected: builds clean (no new warnings), 82 tests pass.

- [ ] **Step 5: Verify the Threads view is byte-identical** — capture before/after. Since Task 1 only refactors, the Threads view must look the same. Drill into threads with `d` (or the current thread-view key) and capture:

```bash
cargo build --bin aerie
SOCK=/tmp/aerie-heat.sock
tmux -S "$SOCK" kill-server 2>/dev/null
tmux -S "$SOCK" new-session -d -s a -x 200 -y 50 "./target/debug/aerie"
sleep 2
tmux -S "$SOCK" send-keys -t a Enter    # drill into the selected group's threads (Groups→Threads)
sleep 2
tmux -S "$SOCK" capture-pane -p -t a | sed -n '1,20p'
tmux -S "$SOCK" kill-server 2>/dev/null
```
Expected: the thread heatmap + list renders exactly as it did before this task (compare against a capture from `main` if unsure). Paste the frame. If the heat grid shifted, mis-colored, or resized, the extraction changed behavior — fix before committing.

- [ ] **Step 6: Commit**

```bash
git add src/ui.rs
git commit -m "refactor(ui): extract render_thread_heat from render_threads (shared with fleet detail)"
```

---

### Task 2: Fleet-detail sample pipeline (compute the selected group's threads)

**Files:**
- Modify: `src/main.rs` (`AppState` struct + constructor; the per-refresh block near line 2340; a new helper method)
- Test: `src/main.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces: `AppState` fields `pub fleet_detail_label: Option<String>`, `pub fleet_detail_snap: Option<local::ThreadSnapshot>`, `pub fleet_detail_samples: Vec<local::ThreadSample>`; and `fn selected_fleet_group_label(&self) -> Option<String>`.

- [ ] **Step 1: Write the failing test** — append to the `fleet_tests` module in `src/main.rs` (created in the earlier Fleet work). This tests the pure selection→label mapping via a small helper that does not need a running app:

```rust
    #[test]
    fn selected_label_matches_focused_tile() {
        use mullion::tree::id_from_key;
        // Two entries; the focused tile id is entry "beta"'s id.
        let labels = ["alpha", "beta", "gamma"];
        let focused = id_from_key(&"beta");
        // Mirror selected_fleet_group_label's core: find the label whose id matches.
        let found = labels.iter().find(|l| id_from_key(l) == focused).map(|l| l.to_string());
        assert_eq!(found, Some("beta".to_string()));
    }
```

- [ ] **Step 2: Run test to verify it fails / compiles** — this test exercises `id_from_key` matching (the mechanism `selected_fleet_group_label` uses). Run:

Run: `cargo test --bin aerie selected_label_matches_focused_tile`
Expected: PASS if `id_from_key` is deterministic (it is — this test guards the mapping assumption the helper relies on; it fails only if `id_from_key` is not stable per key). If it fails, STOP — the whole selection→label approach is invalid and needs escalation.

- [ ] **Step 3: Add the three `AppState` fields** (in the struct near the other `fleet_*` fields ~line 934, and in the constructor ~line 1558):

```rust
    // Fleet detail (monitor lens): the selected primary group's live per-thread
    // samples, with a dedicated prev-snapshot (reset when the selection changes)
    // so cpu% deltas are computed against the right basis.
    /// Comm-label of the group `fleet_detail_samples` currently describes.
    pub fleet_detail_label: Option<String>,
    /// Prev thread snapshot for the selected group (delta basis for sample_threads).
    pub fleet_detail_snap: Option<local::ThreadSnapshot>,
    /// The selected group's per-thread samples, hottest-first, for the detail heatmap.
    pub fleet_detail_samples: Vec<local::ThreadSample>,
```
```rust
    // In the constructor literal:
    fleet_detail_label: None,
    fleet_detail_snap: None,
    fleet_detail_samples: Vec::new(),
```

- [ ] **Step 4: Add the `selected_fleet_group_label` helper** as an `impl AppState` method. It needs `use mullion::tree::id_from_key;` (add at the top of `main.rs` if not present):

```rust
    /// The comm-label of the group currently selected in the Fleet primary
    /// region — the `body_tree` focus mapped back to an entry label. `None`
    /// when nothing is focused or no entry matches.
    fn selected_fleet_group_label(&self) -> Option<String> {
        let focused = self.body_tree.as_ref()?.focus()?;
        self.entries.iter().find(|e| id_from_key(&e.label) == focused).map(|e| e.label.clone())
    }
```

- [ ] **Step 5: Add the per-refresh Fleet-detail block** immediately AFTER the existing Threads block (which ends at src/main.rs:2369, the `if let (Some(label), AppMode::Local) = (thread_label, ...)` block). Mirror it for Fleet:

```rust
        // Fleet detail (monitor lens): compute the selected primary group's
        // per-thread samples each refresh, mirroring the Threads block above but
        // keyed to the Fleet selection. Reset the delta basis when the selection
        // changes so cpu% isn't computed across two different groups.
        if matches!(self.view, AppView::Fleet) && matches!(self.mode, AppMode::Local) {
            match self.selected_fleet_group_label() {
                Some(label) => {
                    if self.fleet_detail_label.as_deref() != Some(label.as_str()) {
                        self.fleet_detail_snap = None;
                        self.fleet_detail_label = Some(label.clone());
                    }
                    let pids = self.snap.as_ref().and_then(|s| s.groups.get(&label))
                        .map(|g| g.pids.clone()).unwrap_or_default();
                    if pids.is_empty() {
                        self.fleet_detail_samples.clear();
                    } else {
                        let cpu_total = self.snap.as_ref().map_or(0, |s| s.total);
                        if let Ok((mut samples, snap)) = local::sample_threads(
                            &pids, self.fleet_detail_snap.take(), &local::ThreadFields::all(), cpu_total,
                        ) {
                            self.fleet_detail_snap = Some(snap);
                            samples.sort_by(|a, b| b.cpu_pct.partial_cmp(&a.cpu_pct).unwrap_or(std::cmp::Ordering::Equal));
                            self.fleet_detail_samples = samples;
                        }
                    }
                }
                None => {
                    self.fleet_detail_label = None;
                    self.fleet_detail_samples.clear();
                }
            }
        }
```

- [ ] **Step 6: Build + tests**

Run: `cargo build --bin aerie && cargo test --bin aerie`
Expected: builds clean; 82 existing + 1 new test pass (83 total). Note: `fleet_detail_samples`/`_snap`/`_label` are written here but not yet READ (the detail renderer in Task 3 reads them) — a transient `dead_code`/unused warning on the fields is expected until Task 3; if the build is warning-clean except those, that's fine. Do NOT add `#[allow(dead_code)]` — Task 3 (next) reads them.

- [ ] **Step 7: Commit**

```bash
git add src/main.rs
git commit -m "feat(fleet): pipeline — compute selected group's thread samples for the detail"
```

---

### Task 3: Render the Fleet detail (header + heat), remove the placeholder

**Files:**
- Modify: `src/ui.rs` (`render_fleet` — the detail region; the stale doc comment)
- Verify: tmux keystroke-driven capture

**Interfaces:**
- Consumes: `render_thread_heat` (Task 1); `AppState.fleet_detail_samples` and `fleet_detail_label` (Task 2).

- [ ] **Step 1: Replace the detail placeholder in `render_fleet`.** Find the line `if let Some(detail) = rect_of(DETAIL_ID) { render_threads(buf, detail, state); }` and replace it with a header + the shared heat helper:

```rust
    // Detail: the selected group's live per-thread heatmap (monitor lens).
    if let Some(detail) = rect_of(DETAIL_ID) {
        if detail.height >= 2 {
            let header = match &state.fleet_detail_label {
                Some(l) => format!(" {l} · {} threads", state.fleet_detail_samples.len()),
                None => " (no group selected)".to_string(),
            };
            buf.set_string(detail.x, detail.y, &header.chars().take(detail.width as usize).collect::<String>(),
                Style::default().fg(Color::Gray));
            let heat = Rect::new(detail.x, detail.y + 1, detail.width, detail.height - 1);
            render_thread_heat(buf, heat, &state.fleet_detail_samples);
        }
    }
```

- [ ] **Step 2: Fix the stale doc comment** on `render_fleet` — update the clause that says "primary/detail reuse the existing `render_body`/`render_threads`" to accurately describe: primary reuses `render_body`; detail draws the selected group's thread heatmap via `render_thread_heat` from `fleet_detail_samples`.

- [ ] **Step 3: Build + tests**

Run: `cargo build --bin aerie && cargo test --bin aerie`
Expected: builds clean (the Task 2 fields are now read — no unused warnings); 83 tests pass.

- [ ] **Step 4: Verify the detail via tmux keystrokes** — enter Fleet, ensure the primary is focused, wait for two refreshes (cpu% needs a prev+current delta), and confirm the detail shows the selected group's heatmap; then move the selection and confirm the detail follows:

```bash
cargo build --bin aerie
SOCK=/tmp/aerie-detail.sock
tmux -S "$SOCK" kill-server 2>/dev/null
tmux -S "$SOCK" new-session -d -s a -x 200 -y 50 "./target/debug/aerie"
sleep 2
tmux -S "$SOCK" send-keys -t a f     # enter Fleet (focus Primary)
sleep 4                               # two refreshes so thread cpu% deltas populate
echo "=== detail shows selected (top) group's threads ==="
tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,14p'
tmux -S "$SOCK" send-keys -t a Down ; tmux -S "$SOCK" send-keys -t a Down ; sleep 4
echo "=== after moving selection down: detail header names the new group, heat updates ==="
tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,14p'
tmux -S "$SOCK" kill-server 2>/dev/null
```
Expected: the detail (right region) shows a header naming the selected group + N threads, and a heat grid of cells; moving the selection down changes the header to the newly-selected group and redraws the heat. Paste both frames. If the detail stays empty or the header/heat don't follow the selection, fix before committing.

- [ ] **Step 5: Commit**

```bash
git add src/ui.rs
git commit -m "feat(fleet): detail region renders the selected group's thread heatmap"
```

---

## Self-Review

**Spec coverage:** the design spec's "Detail follows selection → Monitor lens → the group's thread heatmap" is implemented: Task 2 computes the selected group's `ThreadSample`s each refresh (keyed to the primary selection, delta-correct via a reset-on-change prev-snapshot), Task 3 renders them, Task 1 makes that heatmap identical to the Threads view via a shared helper. The empty-detail gap from Plan 1 is closed. Out of scope (correctly): the diagnose lens (P3), a second/remote place (separate task), health glyphs (P2).

**Placeholder scan:** no TBD/TODO; new code shown in full; Task 1 is an extraction described by boundaries + the produced signature (the code already exists and is relocated — not a placeholder).

**Type consistency:** `render_thread_heat(buf, area, &[ThreadSample])` produced in Task 1, consumed in Task 3; `fleet_detail_samples: Vec<ThreadSample>` / `fleet_detail_label: Option<String>` / `fleet_detail_snap: Option<ThreadSnapshot>` defined in Task 2, read in Task 3; `selected_fleet_group_label()` defined and used in Task 2; `local::sample_threads` signature matches the reuse point. `sample_threads`' first-call-flat behavior (prev=None → cpu%≈0 until the second refresh) is why Task 3's verification waits two refreshes — same as the Threads view.
