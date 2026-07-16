# Navigable Face — P1 Plan 1: Fleet shell (three-region layout + region focus) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an additive `AppView::Fleet` view that renders aerie's new three-region face — spine (place tree) │ primary (existing group table) │ detail (existing thread heatmap) — with a region-focus grammar, built entirely on existing mullion, over the local place only.

**Architecture:** Purely additive — a new `AppView` variant toggled by `f`, leaving every current view untouched. `render_fleet` builds a horizontal `Node::Split` of three tiles via `mullion::border::render_shared` (focused tile thickened, exactly like census's `dit.rs`), renders the spine with `mullion::outline::render_tree_row`, and reuses aerie's existing `render_body`/`render_threads` into the primary/detail rects. Region focus and spine selection are plain app state.

**Tech Stack:** Rust (edition 2021), mullion (path dep, existing APIs only), crossterm event loop, verification via tmux pty capture.

## Global Constraints

- Edition 2021; MSRV rustc 1.85. Copy the exact edition/rust settings already in `Cargo.toml`.
- **No new dependencies.** Use only existing mullion APIs (`layout`, `border`, `outline`, `Theme`, `text::TextCtx`). This is the "prefer mullion" directive: no aerie-local reimplementations of tree/layout/border.
- **Domain-agnostic** (aerie-stay-general): labels/health use only host/VM/container names, comm strings, counts, %, seconds, resource categories. No product/app/desktop-specific knowledge or remediation text.
- **Additive:** all existing views (`Groups`, `Threads`, `Manual`, `Connecting`, `Remote`, `Scope`) keep working unchanged. `cargo test` (existing suite) stays green.
- **Verification discipline:** "compiles + unit tests pass" is NOT sufficient for TUI render tasks — verify with `tmux new-session -d -x 200 -y 50 "<bin>"; capture-pane -p`. Real /proc scale and layout only show when rendered.
- Reference blueprint for the spine + shared-border focus: `~/git/census/src/tui/screens/dit.rs` (2-tile version of exactly this pattern).

---

### Task 1: Region focus enum + `AppView::Fleet` variant + state

**Files:**
- Modify: `src/main.rs` (the `AppView` enum ~line 366; the `AppState` struct ~line 847; `AppState` construction ~line 1526)
- Test: `src/main.rs` (`#[cfg(test)] mod tests` — add if absent, else append)

**Interfaces:**
- Produces: `pub enum Region { Spine, Primary, Detail }` with `fn next(self) -> Region` and `fn prev(self) -> Region`; `AppView::Fleet`; `AppState` fields `pub fleet_region: Region` and `pub spine_cursor: usize`.

- [ ] **Step 1: Write the failing test** — append to `src/main.rs` tests module:

```rust
#[cfg(test)]
mod fleet_tests {
    use super::Region;

    #[test]
    fn region_cycles_forward_and_back() {
        assert_eq!(Region::Spine.next(), Region::Primary);
        assert_eq!(Region::Primary.next(), Region::Detail);
        assert_eq!(Region::Detail.next(), Region::Spine);
        assert_eq!(Region::Spine.prev(), Region::Detail);
        assert_eq!(Region::Primary.prev(), Region::Spine);
        assert_eq!(Region::Detail.prev(), Region::Primary);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin aerie region_cycles`
Expected: FAIL — `cannot find type Region in this scope`.

- [ ] **Step 3: Add the `Region` enum** near the `AppView` enum in `src/main.rs`:

```rust
/// Which of the three Fleet-face regions currently has keyboard focus.
/// `left is where, right is why`: Spine (scope) → Primary (groups) → Detail.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Region {
    Spine,
    #[default]
    Primary,
    Detail,
}

impl Region {
    pub fn next(self) -> Region {
        match self { Region::Spine => Region::Primary, Region::Primary => Region::Detail, Region::Detail => Region::Spine }
    }
    pub fn prev(self) -> Region {
        match self { Region::Spine => Region::Detail, Region::Primary => Region::Spine, Region::Detail => Region::Primary }
    }
}
```

- [ ] **Step 4: Add the `AppView::Fleet` variant** to the `AppView` enum (~line 366):

```rust
    /// The navigable "fleet face": spine (places) │ primary (groups) │ detail.
    /// Additive — toggled with `f`, leaves the other views untouched.
    Fleet,
```

- [ ] **Step 5: Add `AppState` fields** (in the struct ~line 847, and initialise them in the constructor ~line 1526):

```rust
    // In the struct:
    pub fleet_region: Region,
    pub spine_cursor: usize,
```
```rust
    // In the constructor's AppState { ... } literal:
    fleet_region: Region::default(),
    spine_cursor: 0,
```

- [ ] **Step 6: Run tests to verify pass**

Run: `cargo test --bin aerie region_cycles && cargo build --bin aerie`
Expected: test PASS; build succeeds (the new `Fleet` arm may cause a non-exhaustive-match warning/error in `ui.rs` — that is fixed in Task 3; if the build blocks, add a temporary `AppView::Fleet => render_body(buf, body_rect, state),` arm to `ui.rs`'s match at line 44 and note it for Task 3).

- [ ] **Step 7: Commit**

```bash
git add src/main.rs
git commit -m "feat(fleet): Region focus enum + AppView::Fleet variant + state"
```

---

### Task 2: The place model (local-only, structured to grow)

**Files:**
- Create: `src/fleet.rs`
- Modify: `src/main.rs` (add `mod fleet;`)
- Test: `src/fleet.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Produces: `pub struct Place { pub key: String, pub label: String, pub ancestor_last: Vec<bool>, pub is_last: bool, pub expanded: Option<bool> }` and `pub fn local_places() -> Vec<Place>` (one entry: the local host).

- [ ] **Step 1: Write the failing test** — in `src/fleet.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_places_has_one_leaf_host() {
        let places = local_places();
        assert_eq!(places.len(), 1);
        let p = &places[0];
        assert!(p.is_last, "the sole host is its own last child");
        assert!(p.ancestor_last.is_empty(), "root has no ancestors");
        assert_eq!(p.expanded, None, "a leaf host has no expander");
        assert!(!p.label.is_empty(), "host label is the hostname");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin aerie local_places_has_one_leaf_host`
Expected: FAIL — `cannot find function local_places` / unresolved module `fleet`.

- [ ] **Step 3: Create `src/fleet.rs`** with the model + local builder:

```rust
// SPDX-License-Identifier: GPL-3.0-or-later
//! The fleet place-model: the tree of places the spine navigates. P1 populates
//! only the local host; later phases add SSH hosts, Proxmox VMs, and containers
//! under the same flat `Vec<Place>` shape that `mullion::outline::render_tree_row`
//! consumes (the app owns the tree; mullion just paints one flattened row).

/// One row in the spine's flattened place tree. `ancestor_last`/`is_last`/`expanded`
/// are exactly the guide-glyph inputs `mullion::outline::tree_prefix` takes.
#[derive(Clone, Debug)]
pub struct Place {
    /// Stable identity (hostname / VM id / container id) — used for a stable
    /// `TileId` via `mullion::tree::id_from_key`, never derived from position.
    pub key: String,
    /// Human-readable label shown in the spine.
    pub label: String,
    /// One flag per ancestor depth: true when that ancestor is its parent's last child.
    pub ancestor_last: Vec<bool>,
    /// True when this place is its parent's last child (guide connector `└─` vs `├─`).
    pub is_last: bool,
    /// `Some(true/false)` for an expandable branch (open/closed); `None` for a leaf.
    pub expanded: Option<bool>,
}

/// The local host as the sole (leaf) place. Hostname from `/proc/sys/kernel/hostname`,
/// falling back to `"localhost"`.
pub fn local_places() -> Vec<Place> {
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|_| "localhost".to_string());
    vec![Place { key: hostname.clone(), label: hostname, ancestor_last: Vec::new(), is_last: true, expanded: None }]
}
```

- [ ] **Step 4: Register the module** — add near the other `mod` lines in `src/main.rs`:

```rust
mod fleet;
```

- [ ] **Step 5: Run test to verify pass**

Run: `cargo test --bin aerie local_places_has_one_leaf_host`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/fleet.rs src/main.rs
git commit -m "feat(fleet): place model + local-host place builder"
```

---

### Task 3: `render_fleet` — three-region layout + spine, reusing group/thread renderers

**Files:**
- Modify: `src/ui.rs` (add `render_fleet`; wire it into the `render` match ~line 44; add imports)
- Verify: tmux render (no unit test — layout is a render concern)

**Interfaces:**
- Consumes: `fleet::local_places()`, `AppState.fleet_region`, `AppState.spine_cursor`; existing `render_body(buf, area, state)` (ui.rs:1246) and `render_threads(buf, area, state)` (ui.rs:1465).
- Produces: `fn render_fleet(buf: &mut Buffer, area: Rect, state: &mut AppState)`.

- [ ] **Step 1: Add imports** at the top of `src/ui.rs` (extend the existing `use mullion::...` lines):

```rust
use mullion::layout::{Node, Orientation, Constraint, Size};
use mullion::border::render_shared;
use mullion::outline::render_tree_row;
use mullion::text::TextCtx;
use mullion::Theme;
use crate::{fleet, Region};
```

- [ ] **Step 2: Add the region TileId constants + `render_fleet`** in `src/ui.rs` (place `render_fleet` next to `render_body`). This mirrors `census/src/tui/screens/dit.rs:24-32`, extended to three tiles:

```rust
const SPINE_ID:   TileId = 10;
const PRIMARY_ID: TileId = 11;
const DETAIL_ID:  TileId = 12;

/// The additive three-region "fleet face": spine │ primary │ detail.
/// Layout + shared-border focus via `mullion::border::render_shared` (census
/// `dit.rs` idiom); spine via `mullion::outline::render_tree_row`; primary/detail
/// reuse the existing `render_body`/`render_threads` into their sub-rects.
fn render_fleet(buf: &mut Buffer, area: Rect, state: &mut AppState) {
    if area.width < 20 || area.height < 3 { render_body(buf, area, state); return; }

    // Which region's border to thicken.
    let focused = match state.fleet_region {
        Region::Spine => SPINE_ID,
        Region::Primary => PRIMARY_ID,
        Region::Detail => DETAIL_ID,
    };

    let mut tree = Node::Split {
        orientation: Orientation::Horizontal,
        children: vec![
            (Constraint::new(Size::Percent(20)).with_min(16), Node::Tile(SPINE_ID)),
            (Constraint::new(Size::Fill(1)),                   Node::Tile(PRIMARY_ID)),
            (Constraint::new(Size::Percent(38)).with_min(24),  Node::Tile(DETAIL_ID)),
        ],
    };
    let style = BorderStyle { weight: LineWeight::Light, corners: CornerStyle::Rounded, style: Style::default().fg(Color::DarkGray) };
    let rects = render_shared(buf, &mut tree, area, &style, &[(focused, LineWeight::Heavy)]);
    let rect_of = |id: TileId| rects.iter().find(|(t, _)| *t == id).map(|(_, r)| *r);

    // Spine: flatten places, paint one row each.
    if let Some(spine) = rect_of(SPINE_ID) {
        let theme = Theme::default();
        let places = fleet::local_places();
        for (i, p) in places.iter().enumerate() {
            let row = Rect::new(spine.x, spine.y + i as u16, spine.width, 1);
            if row.y >= spine.y + spine.height { break; }
            render_tree_row(buf, row, &p.ancestor_last, p.is_last, p.expanded,
                &p.label, i == state.spine_cursor, &theme, TextCtx::default());
        }
    }

    // Primary: the existing group table into its rect.
    if let Some(primary) = rect_of(PRIMARY_ID) { render_body(buf, primary, state); }

    // Detail: the selected group's threads into its rect (monitor lens).
    if let Some(detail) = rect_of(DETAIL_ID) { render_threads(buf, detail, state); }
}
```

Note: if `Theme::default()` or `TextCtx::default()` do not exist on this mullion HEAD, construct them the same way `census/src/tui/screens/dit.rs` does (grep it for `Theme` / `TextCtx`) — do not invent a new theme.

- [ ] **Step 3: Wire into the render match** — in `render` (ui.rs ~line 44), add the arm (and remove any temporary arm from Task 1 Step 6):

```rust
        AppView::Fleet => render_fleet(buf, body_rect, state),
```

- [ ] **Step 4: Build**

Run: `cargo build --bin aerie`
Expected: builds clean.

- [ ] **Step 5: Render-verify via tmux** — temporarily default the view to Fleet, or (better) rely on Task 4's `f` toggle if doing tasks in order. If verifying Task 3 alone, set `view: AppView::Fleet` in the constructor temporarily:

```bash
cargo build --bin aerie
SOCK=/tmp/aerie-fleet.sock
tmux -S "$SOCK" kill-server 2>/dev/null
tmux -S "$SOCK" new-session -d -s a -x 200 -y 50 "./target/debug/aerie"
sleep 2
tmux -S "$SOCK" capture-pane -p -t a | sed -n '1,20p'
tmux -S "$SOCK" kill-server 2>/dev/null
```
Expected: three regions separated by vertical dividers; the spine shows the hostname row; the primary shows the group table; the detail region is present. Revert the temporary `view:` default before commit (Task 4 adds the real toggle).

- [ ] **Step 6: Commit**

```bash
git add src/ui.rs
git commit -m "feat(fleet): render_fleet — 3-region layout + spine, reusing group/thread renderers"
```

---

### Task 4: Key grammar — `f` toggle, `←/→` region focus, `↑/↓` within region

**Files:**
- Modify: `src/main.rs` (the key `match key.code` in the main loop ~line 3038)
- Verify: tmux keystroke-driven capture

**Interfaces:**
- Consumes: `Region::next/prev`, `AppView::Fleet`, `AppState.fleet_region`, `AppState.spine_cursor`, `fleet::local_places()`.

- [ ] **Step 1: Add the `f` toggle** — in the key match (~line 3038), add an arm (near the other letter keys like `'v'`/`'d'`):

```rust
                    KeyCode::Char('f') => {
                        state.view = if matches!(state.view, AppView::Fleet) {
                            AppView::Groups
                        } else {
                            state.fleet_region = Region::default();
                            state.spine_cursor = 0;
                            AppView::Fleet
                        };
                    }
```

- [ ] **Step 2: Add region focus + within-region movement** — add arms that only fire in Fleet view. Place these BEFORE the existing generic `Left`/`Right`/`Up`/`Down` arms so Fleet intercepts them:

```rust
                    KeyCode::Left if matches!(state.view, AppView::Fleet) => {
                        state.fleet_region = state.fleet_region.prev();
                    }
                    KeyCode::Right if matches!(state.view, AppView::Fleet) => {
                        state.fleet_region = state.fleet_region.next();
                    }
                    KeyCode::Up if matches!(state.view, AppView::Fleet)
                        && state.fleet_region == Region::Spine => {
                        state.spine_cursor = state.spine_cursor.saturating_sub(1);
                    }
                    KeyCode::Down if matches!(state.view, AppView::Fleet)
                        && state.fleet_region == Region::Spine => {
                        let n = fleet::local_places().len();
                        if n > 0 { state.spine_cursor = (state.spine_cursor + 1).min(n - 1); }
                    }
```

Note: when the Fleet focus is `Primary` or `Detail`, `Up`/`Down` fall through to aerie's existing group-selection / thread-scroll handlers (they already key off `state.view`/selection) — no new code needed for those to work inside the regions. Verify in Step 4; if an existing handler guards on `AppView::Groups` specifically, widen its guard to also accept `AppView::Fleet` when `fleet_region == Region::Primary`.

- [ ] **Step 3: Build**

Run: `cargo build --bin aerie && cargo test --bin aerie`
Expected: builds; all existing tests + Task 1/2 tests pass.

- [ ] **Step 4: Render-verify the grammar via tmux keystrokes:**

```bash
cargo build --bin aerie
SOCK=/tmp/aerie-fleet.sock
tmux -S "$SOCK" kill-server 2>/dev/null
tmux -S "$SOCK" new-session -d -s a -x 200 -y 50 "./target/debug/aerie"
sleep 2
tmux -S "$SOCK" send-keys -t a f          # enter Fleet
sleep 0.5; echo "=== after f (Fleet, focus Primary) ==="; tmux -S "$SOCK" capture-pane -p -t a | sed -n '1,6p'
tmux -S "$SOCK" send-keys -t a Left       # focus Spine
sleep 0.5; echo "=== after Left (focus Spine — its border thickens) ==="; tmux -S "$SOCK" capture-pane -p -t a | sed -n '1,6p'
tmux -S "$SOCK" send-keys -t a f          # leave Fleet
sleep 0.5; echo "=== after f (back to Groups) ==="; tmux -S "$SOCK" capture-pane -p -t a | sed -n '1,6p'
tmux -S "$SOCK" kill-server 2>/dev/null
```
Expected: `f` enters the three-region face; `Left` moves focus to the spine (its border renders Heavy `┏━┓` vs the others' Light); `f` again returns to the normal Groups table. Confirm CPU is not pegged and the existing views are unaffected.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat(fleet): key grammar — f toggle, arrows for region focus + spine nav"
```

---

## Self-Review

**Spec coverage (P1 Plan 1 slice):** three-region layout ✓ (Task 3), spine over local place ✓ (Task 2/3), region-focus grammar `←/→` + within-region `↑/↓` ✓ (Task 4), monitor lens (threads in detail) ✓ (Task 3 reuses `render_threads`), additive/non-destructive ✓ (new `AppView` arm only). Deferred to later plans (correctly out of scope here): SSH remote place + attach (Plan 2), health glyphs/tiers + `render_tree_row_decorated` (P2), diagnose lens (P3), responsive breakpoints + `render_shared_styled` colored focus (P4). The narrow-terminal guard in Task 3 Step 2 (`< 20` cols → fall back to `render_body`) prevents a crash before P4 lands the real degradation.

**Placeholder scan:** no TBD/TODO; every code step shows complete code. The two "if the API differs, match census/dit.rs" notes point at a real, in-repo reference file (not a vague placeholder) and cover only the `Theme`/`TextCtx` construction idiom.

**Type consistency:** `Region` (Spine/Primary/Detail) used identically in Tasks 1/3/4; `Place` fields (`ancestor_last`/`is_last`/`expanded`/`label`/`key`) match `render_tree_row`'s parameters; `render_fleet(buf, area, state)` signature matches the reused `render_body`/`render_threads` signatures; TileId constants (`SPINE_ID`/`PRIMARY_ID`/`DETAIL_ID`) consistent across Task 3.

**Risk to watch during execution:** reusing `render_body`/`render_threads` into sub-rects assumes they draw only within the passed `area` — `render_body` was confirmed to (ui.rs:1246 uses `area.*`); `render_threads` must be spot-checked the same way in Task 3 Step 5. If either paints outside its rect, wrap the call to clip or pass a deflated rect.
