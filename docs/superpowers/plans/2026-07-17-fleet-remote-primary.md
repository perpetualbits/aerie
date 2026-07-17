# Fleet remote — Slice B: spine lists hosts + primary shows selected place Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the navigable Fleet face (`AppView::Fleet`) navigate between PLACES: the spine lists the local host (local mode) or the `--hosts` fleet hosts (fleet mode), and selecting a place shows THAT place's group table in the primary via a dedicated `render_fleet_primary` renderer.

**Architecture:** Per the approved decision, the Fleet face gets its OWN primary renderer (`render_fleet_primary`, like it already owns `render_thread_heat` for the detail) — a clean group table built on the same mullion `ColumnGrid`/`Table`/`write_bar` primitives, taking a place's `&[BarEntry]` + a cursor. It renders identically for local and remote entries, sidestepping the `AppMode::Fleet`↔`state.entries` collision (in fleet mode `state.entries` is the host list; the face instead reads `fleet_clients[i].snap.entries`). Task 1 builds the renderer and proves it locally; Task 2 adds the fleet-host places + mode-aware entries source.

**Tech Stack:** Rust (edition 2021), mullion (existing `ColumnGrid`/`Table`/`border`/`outline`), tmux verification (`--hosts localhost` for the remote path).

## Global Constraints

- Edition 2021; MSRV rustc 1.85. NO new dependencies (existing mullion APIs only — the prefer-mullion directive; build the table on the same `ColumnGrid`/`Table`/`write_bar` primitives `render_body` uses).
- Additive / non-regressing: all existing views (`Groups`/`Threads`/`Remote`/`Scope`/…) and the classic `render_body` unchanged; existing 84 tests stay green. Only the `AppView::Fleet` primary changes (from reusing `render_body` to `render_fleet_primary`).
- Domain-agnostic: labels are comm/host strings, %, counts, bytes only.
- Verification discipline: render tasks verified by tmux pty capture. Task 1 is verified in the LOCAL fleet face (no SSH). Task 2's remote path is verified via `aerie --hosts localhost --enable-remote --ssh-accept-new` (SSH to self → local `aerie --daemon`); if passwordless SSH-to-self is unavailable, note it and fall back to a real host (apollo/milkv).
- Reuse points (verified): `BarEntry` (src/main.rs:616) fields `label: String`, `value: f64` (cpu%), `mem_pct: f64`, `rss_bytes: u64`, `count: Option<usize>`; `render_body`'s `ColumnGrid` pattern (src/ui.rs:1347 — `ColumnDef::fixed/fill`, `ColumnKind::{Text,Bar,Custom}`, `.with_align(Align::End)`, `.with_min`); `ColumnGrid::write_bar`/`write_text` static writers; `render_fleet` (src/ui.rs:1250, spine loop 1273, primary call `render_body(buf, primary, state)` at ~1285); the fleet poll site (src/main.rs:1861) storing each host's `conn.snap: Option<DaemonSnapshot>` whose `.entries` is that host's per-group table; `fleet_clients: Vec<FleetConn>` (src/main.rs:1003), `FleetConn.hostname` (src/main.rs:807); `state.spine_cursor` (main.rs:934); the Fleet Primary `Up|Char('k')`/`Down|Char('j')` handler (widened for `AppView::Fleet && fleet_region==Primary`, ~main.rs:3198); `selected_fleet_group_label` (main.rs:1640, currently uses `body_tree.focus()`).

---

### Task 1: `render_fleet_primary` + per-place cursor (local, no SSH)

**Files:**
- Modify: `src/ui.rs` (add `render_fleet_primary`; use it in `render_fleet`'s primary)
- Modify: `src/main.rs` (add `AppState.fleet_primary_cursor`; re-point the Fleet Primary `↑/↓` and `selected_fleet_group_label` to the cursor)
- Verify: tmux (local fleet face)

**Interfaces:**
- Produces: `fn render_fleet_primary(buf: &mut Buffer, area: Rect, entries: &[BarEntry], selected: usize)`; `AppState.fleet_primary_cursor: usize`.

- [ ] **Step 1: Add the `fleet_primary_cursor` field** to `AppState` (near the other `fleet_*` fields, main.rs ~934) and initialize it `0` in the constructor:

```rust
    /// Selected row in the Fleet face's primary group table (index into the
    /// selected place's entries). Reset to 0 when the selected place changes.
    pub fleet_primary_cursor: usize,
```

- [ ] **Step 2: Add `render_fleet_primary`** in `src/ui.rs` near `render_fleet`. A clean group table on mullion `ColumnGrid`: label | cpu% | bar | mem%. Highlight `entries[selected]`; scroll to keep it visible. Model the column layout on `render_body` (src/ui.rs:1347) but simpler:

```rust
/// Dedicated group-table renderer for the Fleet face's primary region. Works on
/// ANY place's entries (local `state.entries` or a remote host's
/// `snap.entries`) — a clean label | cpu% | bar | mem% table on the same mullion
/// primitives `render_body` uses, with a single selection cursor. (The face owns
/// this the way it owns `render_thread_heat` for the detail.)
fn render_fleet_primary(buf: &mut Buffer, area: Rect, entries: &[BarEntry], selected: usize) {
    if area.height == 0 { return; }
    if entries.is_empty() {
        buf.set_string(area.x, area.y, "(no data yet)", Style::default().fg(Color::DarkGray));
        return;
    }
    let label_w = entries.iter().map(|e| e.label.len()).max().unwrap_or(8).clamp(8, 28) as u16;
    let grid = ColumnGrid::new(vec![
        ColumnDef::fixed(label_w, ColumnKind::Text),
        ColumnDef::fixed(7, ColumnKind::Text).with_align(Align::End),   // cpu%
        ColumnDef::fill(1, ColumnKind::Bar).with_min(8),                 // cpu bar
        ColumnDef::fixed(7, ColumnKind::Text).with_align(Align::End),   // mem%
    ]);
    // Scroll window: keep `selected` visible within `area.height` rows.
    let rows = area.height as usize;
    let offset = if selected >= rows { selected + 1 - rows } else { 0 };
    for (row_i, e) in entries.iter().enumerate().skip(offset).take(rows) {
        let y = area.y + (row_i - offset) as u16;
        let cols = grid.resolve(Rect::new(area.x, y, area.width, 1));
        let is_sel = row_i == selected;
        let base = if is_sel { Style::default().fg(Color::Black).bg(Color::Cyan) } else { Style::default() };
        // selection highlight across the row
        if is_sel { buf.set_string(area.x, y, &" ".repeat(area.width as usize), base); }
        ColumnGrid::write_text(buf, cols[0], y, &e.label, Align::Start, base);
        ColumnGrid::write_text(buf, cols[1], y, &format!("{:.1}%", e.value), Align::End, base);
        ColumnGrid::write_bar(buf, cols[2], y, (e.value / 100.0).clamp(0.0, 1.0) as f32, '█', Color::Green, '░', Color::DarkGray, None);
        ColumnGrid::write_text(buf, cols[3], y, &format!("{:.0}%", e.mem_pct), Align::End, base);
    }
}
```
Note: confirm `ColumnGrid::write_bar`'s exact signature against `src/ui.rs`'s existing `write_bar` call (render_threads/render_body use it) and match it — adjust the glyph/color/overlay args to the real signature. If `write_bar` takes different params, use the same call shape the existing code uses.

- [ ] **Step 3: Use it in `render_fleet`.** Replace the primary call `if let Some(primary) = rect_of(PRIMARY_ID) { render_body(buf, primary, state); }` (src/ui.rs ~1285) with:

```rust
    if let Some(primary) = rect_of(PRIMARY_ID) {
        render_fleet_primary(buf, primary, &state.entries, state.fleet_primary_cursor);
    }
```
(Task 2 swaps `&state.entries` for the selected place's entries; for now local entries.)

- [ ] **Step 4: Re-point the Fleet Primary `↑/↓`** to move the cursor. In the widened `Up|Char('k')`/`Down|Char('j')` handler (main.rs ~3198), the `AppView::Fleet && fleet_region==Primary` case currently calls `body_tree.focus_dir(...)`. Change ONLY the Fleet+Primary branch to move `fleet_primary_cursor` instead (clamp to `entries.len()`):

```rust
    // (inside Up handler, Fleet+Primary branch)
    state.fleet_primary_cursor = state.fleet_primary_cursor.saturating_sub(1);
    // (inside Down handler, Fleet+Primary branch)
    let n = state.entries.len();
    if n > 0 { state.fleet_primary_cursor = (state.fleet_primary_cursor + 1).min(n - 1); }
```
Leave the `AppView::Groups | AppView::Remote` branch (body_tree) untouched — that still drives the classic Groups selection.

- [ ] **Step 5: Re-point the detail's group selection to the cursor.** `selected_fleet_group_label` (main.rs:1640) currently maps `body_tree.focus()` → label. Change it to use the cursor so the detail follows the new primary selection:

```rust
    fn selected_fleet_group_label(&self) -> Option<String> {
        self.entries.get(self.fleet_primary_cursor).map(|e| e.label.clone())
    }
```
(This keeps the local detail pipeline — which calls `selected_fleet_group_label` — working with the new cursor. The `id_from_key` import / `body_tree` use here may become unused; remove any now-dead import to keep the build warning-clean.)

- [ ] **Step 6: Build + tests**

Run: `cargo build --bin aerie && cargo test --bin aerie`
Expected: builds clean; 84 tests pass (the `selected_label_matches_focused_tile` test from an earlier slice guards `id_from_key`, which is no longer used by the helper — leave the test if it still compiles, or if it referenced the old helper internals, adjust it minimally; do NOT delete meaningful coverage without noting it).

- [ ] **Step 7: Verify the local fleet face via tmux** (no SSH):

```bash
cargo build --bin aerie
SOCK=/tmp/aerie-fp.sock
tmux -S "$SOCK" kill-server 2>/dev/null
tmux -S "$SOCK" new-session -d -s a -x 200 -y 50 "./target/debug/aerie"
sleep 2
tmux -S "$SOCK" send-keys -t a f ; sleep 1          # Fleet face (focus Primary)
echo "=== primary = render_fleet_primary (group table) ==="; tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,10p'
tmux -S "$SOCK" send-keys -t a Down ; tmux -S "$SOCK" send-keys -t a Down ; sleep 3
echo "=== after Down x2: cursor moved, detail follows the new group ==="; tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,12p'
tmux -S "$SOCK" kill-server 2>/dev/null
```
Expected: the primary shows a clean label | cpu% | bar | mem% group table with a highlighted selected row; `Down` moves the highlight AND the detail header/heat follow the newly-selected group (via the cursor). Paste both frames.

- [ ] **Step 8: Commit**

```bash
git add src/ui.rs src/main.rs
git commit -m "feat(fleet): dedicated render_fleet_primary + per-place cursor"
```

---

### Task 2: Fleet-host places + mode-aware entries source (remote path)

**Files:**
- Modify: `src/fleet.rs` (a `fleet_places` builder from hostnames), `src/ui.rs` (`render_fleet` spine + primary), `src/main.rs` (selected-place entries helper; reset cursor on place change)
- Verify: tmux via `--hosts localhost` (or a real host)

**Interfaces:**
- Produces: `fleet::fleet_places(hostnames: &[String]) -> Vec<Place>`; `AppState` method returning the selected place's entries (local `state.entries` or `fleet_clients[i].snap.entries`).

- [ ] **Step 1: Add `fleet::fleet_places`** in `src/fleet.rs` — one `Place` per hostname (flat siblings, no local root for this slice):

```rust
/// One spine place per fleet host (from `--hosts`). Flat siblings. `local_places`
/// remains the local-mode builder; this is the fleet-mode builder.
pub fn fleet_places(hostnames: &[String]) -> Vec<Place> {
    let n = hostnames.len();
    hostnames.iter().enumerate().map(|(i, h)| Place {
        key: h.clone(), label: h.clone(), ancestor_last: Vec::new(),
        is_last: i + 1 == n, expanded: None,
    }).collect()
}
```

- [ ] **Step 2: Add the selected-place accessors** in `src/main.rs` (`impl AppState`). One returns the spine's places for the current mode; one returns the selected place's entries:

```rust
    /// Spine places for the current mode: the local host in Local mode, or one
    /// per fleet host in Fleet mode.
    fn fleet_spine_places(&self) -> Vec<fleet::Place> {
        if let AppMode::Fleet { .. } = self.mode {
            fleet::fleet_places(&self.fleet_clients.iter().map(|c| c.hostname.clone()).collect::<Vec<_>>())
        } else {
            fleet::local_places()
        }
    }

    /// The selected place's group entries: local `entries` in Local mode, else
    /// the spine-selected fleet host's latest snapshot entries (empty if that
    /// host has no snapshot yet).
    fn selected_place_entries(&self) -> &[BarEntry] {
        if let AppMode::Fleet { .. } = self.mode {
            self.fleet_clients.get(self.spine_cursor)
                .and_then(|c| c.snap.as_ref())
                .map(|s| s.entries.as_slice())
                .unwrap_or(&[])
        } else {
            &self.entries
        }
    }
```

- [ ] **Step 3: Use them in `render_fleet`.** Spine: replace `let places = fleet::local_places();` (src/ui.rs:1275) with `let places = state.fleet_spine_places();`. Primary: replace `&state.entries` in the `render_fleet_primary` call with `state.selected_place_entries()`. (Borrow note: `selected_place_entries` returns `&[BarEntry]` borrowing `state`; call it into a local `let entries = state.selected_place_entries();` before any `&mut` use, or inline — ensure no borrow conflict with the buffer writes, which don't touch `state`.)

- [ ] **Step 4: Reset the primary cursor when the selected place changes.** The spine `Up`/`Down` handler (main.rs ~3244, `Region::Spine` branch) moves `spine_cursor`. After moving it, reset `state.fleet_primary_cursor = 0;` so the primary starts at the top of the newly-selected place's table. Also clamp `fleet_primary_cursor` against `selected_place_entries().len()` where it's incremented (Task 1's Down handler used `state.entries.len()` — in fleet mode that's the host list, wrong; change it to `state.selected_place_entries().len()`).

- [ ] **Step 5: Build + tests**

Run: `cargo build --bin aerie && cargo test --bin aerie`
Expected: builds clean; 84 tests pass.

- [ ] **Step 6: Verify the remote path via tmux with `--hosts localhost`.** This SSHes to self and runs a local `aerie --daemon` as a "remote" place — a full local test of the fleet path. If passwordless SSH-to-self isn't set up, this step will fail to connect; in that case report DONE_WITH_CONCERNS documenting the connect failure and note it must be verified against a real host (apollo/milkv), but STILL confirm the local-mode fleet face is unregressed (Task 1's local test).

```bash
cargo build --bin aerie
SOCK=/tmp/aerie-fp2.sock
tmux -S "$SOCK" kill-server 2>/dev/null
tmux -S "$SOCK" new-session -d -s a -x 200 -y 50 "./target/debug/aerie --hosts localhost --enable-remote --ssh-accept-new"
sleep 6      # allow SSH connect + a couple daemon snapshots
tmux -S "$SOCK" send-keys -t a f ; sleep 2
echo "=== Fleet face in fleet mode: spine lists 'localhost'; primary = its group table ==="
tmux -S "$SOCK" capture-pane -p -t a | sed -n '1,14p'
tmux -S "$SOCK" kill-server 2>/dev/null
```
Expected (if SSH-to-self works): the spine shows `localhost` (the fleet host), and the primary shows that host's live group table via `render_fleet_primary` (real process-group labels + cpu%/mem bars) — NOT the host-list. Paste the frame. Confirm the classic (non-`f`) view still shows the fleet host-list (unregressed). Clean up: kill the tmux server; ensure no leftover `ssh`/`aerie --daemon` children linger (`pgrep -af 'aerie --daemon'` → kill by PID if any).

- [ ] **Step 7: Commit**

```bash
git add src/fleet.rs src/ui.rs src/main.rs
git commit -m "feat(fleet): spine lists fleet hosts; primary shows the selected place's table"
```

---

## Self-Review

**Spec coverage:** the Fleet face now navigates between places — Task 1 gives the dedicated `render_fleet_primary` (uniform for any place's entries) + a per-place cursor, proven in the local face; Task 2 lists the fleet hosts in the spine and points the primary at the spine-selected host's `snap.entries`, verified via `--hosts localhost`. The detail follows the primary cursor (Task 1 re-point). Out of scope (correctly): remote thread detail (Slice C — the detail still uses the local sampler / shows local threads until C routes `focus_threads`); a unified local+remote tree (local place in fleet mode needs dual collection — later); health glyphs (P2).

**Placeholder scan:** no TBD/TODO; new code shown in full. The `write_bar` signature note and the `id_from_key`-now-unused note are read-the-code instructions, not placeholders.

**Type consistency:** `render_fleet_primary(buf, area, &[BarEntry], usize)` produced in Task 1, called in Tasks 1 & 2; `fleet_primary_cursor: usize` defined Task 1, reset in Task 2; `fleet_spine_places()`/`selected_place_entries()` defined Task 2, used in `render_fleet`; `selected_place_entries` returns `&[BarEntry]` matching `render_fleet_primary`'s param; `fleet_places(&[String])` matches the hostname source `fleet_clients[].hostname`. spine_cursor→fleet_clients index is positional and consistent between `fleet_spine_places` and `selected_place_entries`.
