# Fleet Drilldown — Region-aware Enter grammar Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Enter in the fleet face drill region-aware — Spine→enter a host (dense full-screen process view reusing the fleet's live stream), Primary/host→open an app's per-thread view (from the `focus_threads` stream) — with Esc popping a small drill stack back to the fleet face.

**Architecture:** Reuse existing views, fleet-backed via two markers. The host view is `AppView::Remote` sourced from `fleet_clients[host].snap` instead of `state.remote_client` (both are `DaemonSnapshot`, so the existing Remote render + dense key grammar + header all work unchanged). The thread view is `AppView::Threads` sourced from the host's `focus_threads` instead of the local sampler. A `drill_focus()` helper computes the focused `(host, app)` at the current level and drives `send_focus`. Esc uses the two markers to pop back.

**Tech Stack:** Rust (edition 2021), single crate (aerie). No new deps. No mullion change.

## Global Constraints

- Edition 2021; MSRV 1.85. **NO new dependencies.**
- **Additive / non-regressing:** the classic `AppView::Remote` (drilled from Proxmox/Kube/Nomad via `remote_client`) and the local `AppView::Threads` behave exactly as before; the fleet face's spine/primary/detail and the just-merged health glyphs are untouched; existing tests stay green.
- **No new connections:** the host view and thread view reuse the fleet's already-streaming daemon data. Never call `connect_direct`/`connect_*` for a fleet drill.
- **Domain-agnostic** ([[aerie-stay-general]]): hosts / process groups / threads / comm / pid / % only — no app/role/desktop meaning. This adds navigation, no domain knowledge.
- **Only the focused host samples threads:** `send_focus(app)` goes to exactly one host (the drilled/selected one), `send_focus(None)` to the rest — preserved from Slice C.
- **Verification:** unit tests for the pure focus/return logic; end-to-end via tmux against real hosts ([[aerie-test-hosts]]) — `aerie --hosts apollo,milkv --enable-remote`.

## Drill state model

Two markers on `AppState` encode the fleet drill level; the reused `AppView` says which renderer runs:

| Level | `state.view` | `fleet_host` | `fleet_thread` |
|-------|--------------|--------------|----------------|
| Fleet face | `Fleet` | `None` | `None` |
| Host view (as-if-local) | `Remote{host}` | `Some(host)` | `None` |
| Thread view (from face) | `Threads{app}` | `None` | `Some((host, app))` |
| Thread view (from host view) | `Threads{app}` | `Some(host)` | `Some((host, app))` |

`fleet_host.is_some()` ⇒ the `Remote`/`Threads` view is **fleet-backed** (poll the fleet client, not `remote_client`; Esc returns into the fleet, not `Groups`). Esc from a fleet-backed thread view returns to the host view when `fleet_host` is set, else to the fleet face.

## File Structure

- `src/main.rs` — the two markers + init; `drill_focus` pure helper + `route_fleet_focus`; the `in_remote` fleet-backed poll branch; the `Enter`/`Esc` arms; the thread-sample population for the fleet thread view.
- `src/ui.rs` — one render-dispatch line so `AppView::Threads` uses `render_threads` when fleet-backed.

---

### Task 1: Drill state + focus routing foundation

**Files:**
- Modify: `src/main.rs` (AppState fields + init; `resolve_drill_focus` free fn; `drill_focus`/`route_fleet_focus` methods; replace the inline send_focus block)
- Test: `src/main.rs` (`#[cfg(test)] mod` that imports `super::{…}`)

**Interfaces:**
- Produces: `AppState.fleet_host: Option<String>`, `AppState.fleet_thread: Option<(String, String)>`; `fn resolve_drill_focus(...) -> Option<(String, Option<String>)>`; `AppState::drill_focus(&self) -> Option<(String, Option<String>)>`; `AppState::route_fleet_focus(&mut self)`.

- [ ] **Step 1: Add the markers.** In the `AppState` struct (near `fleet_host`-adjacent fleet fields such as `health_tiers`/`fleet_primary_label`), add:

```rust
    /// When the fleet face has drilled INTO a host, the entered hostname. The
    /// `AppView::Remote` is then fleet-backed (data from `fleet_clients[host]`,
    /// not `remote_client`) and Esc returns to the fleet face, not `Groups`.
    pub fleet_host: Option<String>,
    /// When a fleet app's per-thread view is open, the drilled `(host, app)`.
    /// The `AppView::Threads` is then fleet-backed (samples from the host's
    /// `focus_threads`, not the local sampler). Esc returns to the host view
    /// when `fleet_host` is set, else to the fleet face.
    pub fleet_thread: Option<(String, String)>,
```

In the `AppState { … }` initializer (near `health_tiers: HashMap::new(),`):

```rust
            fleet_host: None,
            fleet_thread: None,
```

- [ ] **Step 2: Write the failing test** for the pure focus resolver (add to the test module that imports `super::{…}` — extend that `use` with `resolve_drill_focus`):

```rust
    #[test]
    fn drill_focus_precedence() {
        // Thread view: the fixed (host, app) wins over everything.
        assert_eq!(
            resolve_drill_focus(Some(&("milkv".into(), "btop".into())), Some("milkv"),
                Some("kworker"), Some("apollo"), Some("steam")),
            Some(("milkv".into(), Some("btop".into()))));
        // Host view (no thread): the host + the host-view cursor's app.
        assert_eq!(
            resolve_drill_focus(None, Some("milkv"), Some("kworker"), Some("apollo"), Some("steam")),
            Some(("milkv".into(), Some("kworker".into()))));
        // Host view with no app selected yet: host, no app to focus.
        assert_eq!(
            resolve_drill_focus(None, Some("milkv"), None, Some("apollo"), Some("steam")),
            Some(("milkv".into(), None)));
        // Fleet face: spine host + primary app.
        assert_eq!(
            resolve_drill_focus(None, None, None, Some("apollo"), Some("steam")),
            Some(("apollo".into(), Some("steam".into()))));
        // Nothing selected: no focus.
        assert_eq!(resolve_drill_focus(None, None, None, None, None), None);
    }
```

- [ ] **Step 3: Run it — expect FAIL** (`resolve_drill_focus` not found): `cargo test --bin aerie drill_focus 2>&1 | tail`

- [ ] **Step 4: Implement the resolver + methods.** Add the free fn near the other free helpers (e.g. after `resolve_fleet_primary`):

```rust
/// Pure core of [`AppState::drill_focus`]: pick the `(host, app)` whose
/// per-thread stream should be live, by drill level (thread view > host view >
/// fleet face). Returns the host and the app to focus on it (`None` app = focus
/// nothing on that host yet). `None` overall = no host selected.
fn resolve_drill_focus(
    fleet_thread: Option<&(String, String)>,
    fleet_host: Option<&str>,
    host_view_app: Option<&str>,
    face_host: Option<&str>,
    face_app: Option<&str>,
) -> Option<(String, Option<String>)> {
    if let Some((h, a)) = fleet_thread {
        Some((h.clone(), Some(a.clone())))
    } else if let Some(h) = fleet_host {
        Some((h.to_string(), host_view_app.map(str::to_string)))
    } else {
        face_host.map(|h| (h.to_string(), face_app.map(str::to_string)))
    }
}
```

Add the methods on `AppState` (near `selected_fleet_group_label`):

```rust
    /// The `(host, app)` whose per-thread stream should be live, given the
    /// current fleet drill level. In the fleet face this is the spine host +
    /// primary app; in the host view it's the entered host + its cursor's app;
    /// in a thread view it's the fixed drilled `(host, app)`.
    fn drill_focus(&self) -> Option<(String, Option<String>)> {
        // Host-view selected app = the dense body's focused entry label.
        let host_view_app = self.fleet_host.as_ref().and(
            self.focused_entry_idx().and_then(|i| self.entries.get(i)).map(|e| e.label.as_str()));
        let face_host = self.fleet_spine_places().get(self.spine_cursor).map(|p| p.label.clone());
        let face_app = self.selected_fleet_group_label();
        resolve_drill_focus(
            self.fleet_thread.as_ref(),
            self.fleet_host.as_deref(),
            host_view_app,
            face_host.as_deref(),
            face_app.as_deref(),
        )
    }

    /// Route `send_focus` to the drilled host's focused app and `None` to every
    /// other fleet host, so exactly one host samples threads. Call each tick
    /// from both the fleet refresh loop and the fleet-backed host-view poll.
    fn route_fleet_focus(&mut self) {
        let target = self.drill_focus(); // Option<(host, Option<app>)>
        for conn in self.fleet_clients.iter_mut() {
            if let Some(FleetClient::Daemon(rc)) = conn.client.as_mut() {
                let app = match &target {
                    Some((h, a)) if *h == conn.hostname => a.as_deref(),
                    _ => None,
                };
                rc.send_focus(app);
            }
        }
    }
```

- [ ] **Step 5: Replace the inline send_focus block** in `refresh` (currently `sync_fleet_primary_to_label()` then the `let focus_group = …; for (i, conn) …` loop — search for `let focus_group = self.selected_fleet_group_label()`). Keep the `sync_fleet_primary_to_label()` call; replace the routing loop with `route_fleet_focus()`:

```rust
            self.sync_fleet_primary_to_label();
            // Route the focused-thread stream to the drilled (host, app) — on
            // the fleet face this is the spine host's primary app (unchanged);
            // when drilled it follows the host view / thread view.
            self.route_fleet_focus();
```

- [ ] **Step 6: Build + test.** `cargo build --bin aerie && cargo test --bin aerie 2>&1 | tail -5`
Expected: clean; new test passes; **fleet-face behavior unchanged** (with `fleet_host`/`fleet_thread` both `None`, `drill_focus` returns the spine-host + primary-app, exactly as the old block). `focused_entry_idx` and `fleet_spine_places` are existing methods; confirm no borrow error (all return owned/`Option`).

- [ ] **Step 7: Commit.**

```bash
git add src/main.rs
git commit -m "feat(fleet): drill state markers + (host,app) focus routing foundation"
```

---

### Task 2: Spine Enter → fleet-backed host view (+ Esc back to the face)

**Files:**
- Modify: `src/main.rs` (the `Enter` handler; the `in_remote` poll block; the `Esc` handler)

**Interfaces:**
- Consumes: `fleet_host` (Task 1), `route_fleet_focus` (Task 1), `fleet_spine_places`, `spine_cursor`, `fleet_region`, `FleetClient::Daemon`, `selected_fleet_conn`.

- [ ] **Step 1: Add the Spine-Enter arm.** In the `KeyCode::Enter` handler (search `KeyCode::Enter =>`), which is currently wholly gated on `AppView::Groups`, add a sibling branch BEFORE that guard for the fleet face:

```rust
                    KeyCode::Enter if matches!(state.view, AppView::Fleet)
                        && state.fleet_region == Region::Spine => {
                        // Enter a host "as-if-local": a dense full-screen process
                        // view sourced from the fleet's already-streaming snapshot
                        // (no new SSH). Reuses AppView::Remote; fleet_host marks it
                        // fleet-backed so the poll reads the fleet client and Esc
                        // returns to the fleet face.
                        if let Some(host) = state.fleet_spine_places()
                            .get(state.spine_cursor).map(|p| p.label.clone())
                        {
                            match state.selected_fleet_conn() {
                                Some(conn) if conn.client.is_some() && !conn.thin => {
                                    state.fleet_host = Some(host.clone());
                                    state.entries = vec![];
                                    state.view = AppView::Remote { label: host };
                                    state.sync_body_tree();
                                }
                                Some(conn) if conn.thin => state.error =
                                    Some("thin probe — no per-process drill-down".into()),
                                _ => state.error = Some(format!("not connected to {host}")),
                            }
                        }
                    }
```

- [ ] **Step 2: Make the `in_remote` poll fleet-backed when `fleet_host` is set.** In the main loop's remote-poll block (search `let in_remote = matches!(state.view, AppView::Remote`), branch: when `state.fleet_host` is `Some`, poll that fleet client instead of `remote_client`, then route focus (refresh is skipped while `in_remote`, so the fleet loop won't run):

```rust
        let in_remote = matches!(state.view, AppView::Remote { .. });

        if in_remote {
            if let Some(host) = state.fleet_host.clone() {
                // Fleet-backed host view: read the fleet client's live snapshot.
                let conn_alive = fleet_conn_for_label(&host, &state.fleet_clients)
                    .and_then(|c| c.client.as_ref())
                    .map(|c| matches!(c, FleetClient::Daemon(rc) if rc.is_alive())
                        || matches!(c, FleetClient::Thin(t) if t.is_alive()))
                    .unwrap_or(false);
                if !conn_alive {
                    state.error = Some(format!("lost connection to {host}"));
                    state.fleet_host = None;
                    state.view = AppView::Fleet;
                } else {
                    // Drain the fleet client for a fresh snapshot and STASH it on
                    // the FleetConn (so a later host→app thread drill can read its
                    // focus_threads — the fleet loop that normally sets c.snap is
                    // skipped while in_remote). Then mirror it into state for the
                    // dense body. Stash first, then read from c.snap to avoid a
                    // partial move; entries is cloned (both places need it).
                    let got = state.fleet_clients.iter_mut()
                        .find(|c| c.hostname == host)
                        .and_then(|c| {
                            let s = match c.client.as_mut() {
                                Some(FleetClient::Daemon(rc)) => rc.try_recv(),
                                Some(FleetClient::Thin(t)) => t.try_recv(),
                                None => None,
                            };
                            if let Some(snap) = s { c.snap = Some(snap); true } else { false }.then_some(())
                        }).is_some();
                    if got {
                        if let Some(snap) = fleet_conn_for_label(&host, &state.fleet_clients)
                            .and_then(|c| c.snap.as_ref())
                        {
                            state.entries = snap.entries.clone();
                            state.total_ram_bytes = snap.total_ram_bytes;
                            state.sys_net_rx_s = snap.sys_net_rx_s;
                            state.sys_net_tx_s = snap.sys_net_tx_s;
                            state.sys_gpu_pct = snap.sys_gpu_pct;
                            state.sys_rapl_w = snap.sys_rapl_w;
                            state.sys_psi_cpu = snap.sys_psi_cpu;
                            state.sys_psi_mem = snap.sys_psi_mem;
                            state.sys_psi_io  = snap.sys_psi_io;
                            state.snap_count = snap.snap_count;
                        }
                        state.sync_body_tree();
                    }
                    // Keep the thread stream pinned to the host-view cursor's app.
                    state.route_fleet_focus();
                }
            } else if let Some(ref mut client) = state.remote_client {
                // …existing classic remote_client polling, unchanged…
```

Note: the existing `else if let Some(ref mut client) = state.remote_client { … }` is the current body — keep it verbatim as the `else` branch (classic Proxmox/Kube/Nomad drill via `remote_client`, unchanged).

- [ ] **Step 3: Esc returns a fleet-backed host view to the face.** In the `Esc` handler's `AppView::Remote { .. } | AppView::Connecting { .. }` arm (search `AppView::Remote { .. } | AppView::Connecting`), branch on `fleet_host` first:

```rust
                            AppView::Remote { .. } if state.fleet_host.is_some() => {
                                // Fleet-backed host view: return to the fleet face;
                                // do NOT close anything — the fleet keeps streaming.
                                state.fleet_host = None;
                                state.entries = vec![];
                                state.view = AppView::Fleet;
                            }
                            AppView::Remote { .. } | AppView::Connecting { .. } => {
                                // …existing classic path: close remote_client → Groups…
```

- [ ] **Step 4: Build + tests.** `cargo build --bin aerie && cargo test --bin aerie 2>&1 | tail -5` — clean, all pass.

- [ ] **Step 5: Verify against a real host (apollo).** Enter a host from the spine, confirm the dense view of ITS processes, that the dense keys work, and Esc returns to the face:

```bash
cargo build --bin aerie
SOCK=/tmp/aerie-t2.sock
tmux -S "$SOCK" kill-server 2>/dev/null
tmux -S "$SOCK" new-session -d -s a -x 200 -y 50 "./target/debug/aerie --hosts apollo,milkv --enable-remote --ssh-accept-new --interval 2"
sleep 10
tmux -S "$SOCK" send-keys -t a f ; sleep 2          # fleet face (focus=Primary)
tmux -S "$SOCK" send-keys -t a Left ; sleep 1       # focus Spine
tmux -S "$SOCK" send-keys -t a Down ; sleep 1       # select apollo (2nd host)
tmux -S "$SOCK" send-keys -t a Enter ; sleep 4      # enter the host
echo "=== host view: apollo's dense process list (bars/columns) ==="
tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,12p'
tmux -S "$SOCK" send-keys -t a s ; sleep 2          # dense grammar: re-sort
echo "=== after 's' (sort still works in the host view) ==="; tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,5p'
tmux -S "$SOCK" send-keys -t a Escape ; sleep 2
echo "=== after Esc: back to the fleet face (spine+primary+detail) ==="
tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,6p'
tmux -S "$SOCK" kill-server 2>/dev/null; pkill -f 'ssh .*apollo' 2>/dev/null
```
Expected: after Enter, a full-width dense process table of apollo's groups (not the 3-region face); `s` re-sorts; Esc restores the fleet face with milkv/apollo in the spine. Paste the frames. Confirm no second SSH to apollo was opened (`pgrep -af 'ssh .*apollo'` shows only the one daemon channel).

- [ ] **Step 6: Commit.**

```bash
git add src/main.rs
git commit -m "feat(fleet): Spine Enter enters a host as-if-local (fleet-backed Remote view) + Esc"
```

---

### Task 3: Primary/host Enter → fleet-backed thread view (+ Esc drill stack)

**Files:**
- Modify: `src/main.rs` (two `Enter` arms; the thread-sample population in `refresh`; the `Esc` arm)
- Modify: `src/ui.rs` (render dispatch line)

**Interfaces:**
- Consumes: `fleet_thread`/`fleet_host` (Task 1/2), `selected_fleet_group_label`, `selected_place_threads`, `focused_entry_idx`, `render_threads`.

- [ ] **Step 1: Add the Primary-Enter and host-view-Enter arms.** After the Spine-Enter arm (Task 2), add:

```rust
                    KeyCode::Enter if matches!(state.view, AppView::Fleet)
                        && state.fleet_region == Region::Primary => {
                        // Drill the selected app into its per-thread view (from the
                        // fleet face). Sourced from the host's focus_threads stream.
                        let host = state.fleet_spine_places()
                            .get(state.spine_cursor).map(|p| p.label.clone());
                        let app = state.selected_fleet_group_label();
                        if let (Some(host), Some(app)) = (host, app) {
                            state.fleet_thread = Some((host, app.clone()));
                            state.thread_samples = vec![];
                            state.view = AppView::Threads { label: app };
                        }
                    }
                    KeyCode::Enter if matches!(state.view, AppView::Remote { .. })
                        && state.fleet_host.is_some() => {
                        // From the entered host view, drill the focused app into
                        // its per-thread view (keeps fleet_host so Esc pops back here).
                        let host = state.fleet_host.clone();
                        let app = state.focused_entry_idx()
                            .and_then(|i| state.entries.get(i)).map(|e| e.label.clone());
                        if let (Some(host), Some(app)) = (host, app) {
                            state.fleet_thread = Some((host, app.clone()));
                            state.thread_samples = vec![];
                            state.view = AppView::Threads { label: app };
                        }
                    }
```

- [ ] **Step 2: Populate `thread_samples` from the fleet stream** in `refresh`. Near the local thread-sampling block (search `AppView::Threads { label } => Some(label.clone())` / the `sample_threads` call gated on `AppMode::Local`), add a fleet-backed branch that copies the drilled host's `focus_threads` into `state.thread_samples` when `fleet_thread` is set:

```rust
        // Fleet-backed thread view: samples come from the drilled host's
        // focus_threads stream (kept live by route_fleet_focus), not the local
        // sampler. Match on the drilled (host, app); take the samples when the
        // streamed label matches the app we drilled.
        if let Some((host, app)) = self.fleet_thread.clone() {
            let samples = fleet_conn_for_label(&host, &self.fleet_clients)
                .and_then(|c| c.snap.as_ref())
                .and_then(|s| s.focus_threads.as_ref())
                .filter(|(label, _)| *label == app)
                .map(|(_, s)| s.clone())
                .unwrap_or_default();
            self.thread_samples = samples;
        }
```

(Place this AFTER the fleet poll loop + `route_fleet_focus()` so `focus_threads` is fresh this tick.)

- [ ] **Step 3: Render `render_threads` for the fleet-backed thread view.** In `src/ui.rs` the dispatch is `AppView::Threads { .. } if matches!(state.mode, AppMode::Local) => render_threads(…)` else `render_body`. Change the guard so a fleet-backed thread view also uses `render_threads`:

```rust
        AppView::Threads { .. } if matches!(state.mode, AppMode::Local)
            || state.fleet_thread.is_some() => render_threads(buf, body_rect, state),
        AppView::Threads { .. } => render_body(buf, body_rect, state),
```

`render_threads` reads `state.thread_samples` (populated in Step 2) — no other change needed. (If `render_threads` also reads a group label for its header, it already takes it from `AppView::Threads { label }`, which we set to the app.)

- [ ] **Step 4: Esc pops the thread view to its origin.** In the `Esc` handler's `AppView::Threads { .. }` arm, branch on `fleet_thread` first:

```rust
                            AppView::Threads { .. } if state.fleet_thread.is_some() => {
                                state.fleet_thread = None;
                                state.thread_samples = vec![];
                                // Pop to the host view if we drilled from it, else the face.
                                state.view = if state.fleet_host.is_some() {
                                    AppView::Remote { label: state.fleet_host.clone().unwrap() }
                                } else {
                                    AppView::Fleet
                                };
                            }
                            AppView::Threads { .. } => {
                                // …existing local path → Groups…
```

- [ ] **Step 5: Build + tests.** `cargo build --bin aerie && cargo test --bin aerie 2>&1 | tail -5` — clean, all pass.

- [ ] **Step 6: Verify the FULL drill stack against apollo (the payoff).** Both entry paths and Esc popping each level:

```bash
cargo build --bin aerie
SOCK=/tmp/aerie-t3.sock
tmux -S "$SOCK" kill-server 2>/dev/null
tmux -S "$SOCK" new-session -d -s a -x 200 -y 50 "./target/debug/aerie --hosts apollo,milkv --enable-remote --ssh-accept-new --interval 2"
sleep 10
tmux -S "$SOCK" send-keys -t a f ; sleep 2               # face (focus=Primary)
# Path A: Primary Enter → app thread view
tmux -S "$SOCK" send-keys -t a Down ; sleep 2
tmux -S "$SOCK" send-keys -t a Enter ; sleep 6           # drill app → threads (warmup)
echo "=== Path A: app per-thread view (remote focus_threads) ==="
tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,10p'
tmux -S "$SOCK" send-keys -t a Escape ; sleep 2
echo "=== Esc → back to the fleet face ==="; tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,5p'
# Path B: Spine Enter → host view → app Enter → threads → Esc → host → Esc → face
tmux -S "$SOCK" send-keys -t a Left ; sleep 1 ; tmux -S "$SOCK" send-keys -t a Enter ; sleep 4   # enter host
tmux -S "$SOCK" send-keys -t a Down ; sleep 1 ; tmux -S "$SOCK" send-keys -t a Enter ; sleep 6   # app → threads
echo "=== Path B: threads drilled from inside the host view ==="
tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,8p'
tmux -S "$SOCK" send-keys -t a Escape ; sleep 2
echo "=== Esc → back to the HOST view (not the face) ==="; tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,5p'
tmux -S "$SOCK" send-keys -t a Escape ; sleep 2
echo "=== Esc → back to the fleet face ==="; tmux -S "$SOCK" capture-pane -p -t a | sed -n '2,6p'
tmux -S "$SOCK" kill-server 2>/dev/null; pkill -f 'ssh .*apollo' 2>/dev/null
```
Expected: Path A shows a real per-thread view of the selected apollo app (thread rows with cpu%, like the local Threads view), Esc → face. Path B: enter host → dense list → Enter an app → its threads → Esc returns to the HOST view → Esc returns to the face. Paste the frames. If the thread view shows 0 threads, wait one more tick (focus warmup) and re-capture. Clean up: no orphaned `ssh`/`aerie --daemon` (`pgrep -af 'ssh .*apollo'`).

- [ ] **Step 7: Commit.**

```bash
git add src/main.rs src/ui.rs
git commit -m "feat(fleet): Primary/host Enter opens the app per-thread view (fleet-backed) + Esc drill stack"
```

---

## Self-Review

**Spec coverage:**
- Region-aware Enter (Spine→host, Primary→app, host-view app→threads): Task 2 (spine) + Task 3 (primary + host-view arms). ✓
- Host view reuses the fleet stream, no new SSH: Task 2 `in_remote` fleet-backed poll (no `connect_*`). ✓
- Full dense grammar in the host view: reuses `AppView::Remote`, which the sort/metric/direction key guards already include — verified live in Task 2 Step 5. ✓
- Thread view matches the local Threads view, from `focus_threads`: Task 3 Steps 2-3 (`render_threads` + fleet-sourced `thread_samples`). ✓
- `send_focus` follows the drilled `(host, app)`: Task 1 `drill_focus`/`route_fleet_focus`, called from both refresh and the host-view poll. ✓
- Esc pops the drill stack back to the fleet face: Task 2 (host→face) + Task 3 (thread→host/face). ✓
- Domain-agnostic; additive (classic Remote/Threads paths kept as the `else`/non-fleet branches). ✓

**Placeholder scan:** no TBD/TODO; code shown in full. The one prose note (Task 2 Step 2 partial-move ordering) is an explicit implementer instruction, not a gap.

**Type consistency:** `fleet_host: Option<String>`, `fleet_thread: Option<(String, String)>` (Task 1) drive `resolve_drill_focus(Option<&(String,String)>, Option<&str>, Option<&str>, Option<&str>, Option<&str>) -> Option<(String, Option<String>)>` (Task 1), consumed by `route_fleet_focus` → `send_focus(Option<&str>)`. The `Enter` arms set `fleet_host`/`fleet_thread` + `AppView::{Remote,Threads}`; the `Esc` arms read them to pop; `render_threads(&AppState)` reads `thread_samples` (Task 3 Step 2). `fleet_conn_for_label`/`selected_fleet_conn`/`focused_entry_idx`/`fleet_spine_places`/`selected_fleet_group_label` are all existing methods.
