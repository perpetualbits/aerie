# Fleet remote thread detail — Slice A: focused-stream protocol Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend aerie's `--daemon` protocol so a viewer can ask a remote daemon to stream per-thread data for one selected group ("focused-stream"), laying the protocol foundation for remote thread detail in the navigable Fleet face. This slice is protocol-only (daemon + RemoteClient); the viewer UI integration is Slices B/C.

**Architecture:** The daemon (`aerie --daemon`, one-way JSON snapshot stream today) grows a stdin channel: the viewer writes a focus-group name; a daemon reader thread records it; the daemon samples that one group's threads each cycle (maintaining a delta snapshot for correct cpu%) and attaches `focus_threads: Option<(String, Vec<ThreadSample>)>` to each emitted `DaemonSnapshot` (serde-default None → backward compatible). `RemoteClient` opens the SSH child's stdin and gains `send_focus`.

**Tech Stack:** Rust (edition 2021), serde/serde_json (already deps), std threads, tmux/pipe verification. NO new dependencies.

## Global Constraints

- Edition 2021; MSRV rustc 1.85. NO new dependencies (serde/serde_json already present).
- **Backward/forward compatible:** old daemons omit `focus_threads` → decodes as `None`; a new viewer talking to an old daemon sees `None`. `#[serde(default)]` per-field is the entire versioning scheme (no version number exists — match the existing pattern: `DaemonSnapshot.sys_cpu_pct`/`sys_mem_used_bytes` use `#[serde(default)]`).
- **Additive / non-regressing:** the existing one-way stream still works; all existing tests stay green. The daemon's snapshot emit cadence must NOT be stalled by stdin reads (use a separate reader thread — never inline blocking `lines()` in the emit loop).
- **Locally testable (no SSH):** `aerie --daemon` runs locally; the daemon task is verified by piping a focus line to a local `aerie --daemon` and reading `focus_threads` from its stdout JSON.
- Reuse points (verified, exact): daemon loop `run_daemon` (src/main.rs:2541, snapshot literal at 2609, emit at 2626); `DaemonSnapshot` (src/remote.rs:28, serde-derived at 27); `local::ThreadSample` (src/local.rs:151 — currently NO derives, no serde import in local.rs); `local::sample_threads(pids, prev, fields, cpu_total) -> Result<(Vec<ThreadSample>, ThreadSnapshot)>` (src/local.rs:1214); `local::Snapshot.groups: HashMap<String, GroupData>` with `GroupData.pids: Vec<u32>`; `connect_direct` (src/remote.rs:334, `stdin(Stdio::null())` at 362, child.stdout.take at ~368, reader thread 373-381); `RemoteClient` struct (src/remote.rs:84), `try_recv` (102); the other RemoteClient constructors that must keep compiling: `connect_kube_daemon` (src/remote.rs:581), `connect_nomad_daemon` (src/remote.rs:796).

---

### Task 1: Make `ThreadSample` serde-serializable

**Files:**
- Modify: `src/local.rs` (the `ThreadSample` struct ~line 151; add serde import)
- Test: `src/local.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces: `local::ThreadSample` deriving `Serialize + Deserialize` (and `Clone`, to match `DaemonSnapshot`'s derives since it will be nested in a cloned field).

- [ ] **Step 1: Write the failing test** — in `src/local.rs` tests:

```rust
    #[test]
    fn thread_sample_json_round_trip() {
        let s = ThreadSample { pid: 42, tid: 43, name: "worker".into(),
            cpu_pct: 12.5, faults_per_s: 1.0, disk_read_s: 2.0, disk_write_s: 3.0,
            ctx_switches_s: 4.0, sched_wait_pct: 5.5 };
        let json = serde_json::to_string(&s).unwrap();
        let back: ThreadSample = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pid, 42);
        assert_eq!(back.name, "worker");
        assert!((back.cpu_pct - 12.5).abs() < 1e-9);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin aerie thread_sample_json_round_trip`
Expected: FAIL — `ThreadSample: Serialize` not satisfied (no derive).

- [ ] **Step 3: Add serde to `ThreadSample`.** Add the import near the top of `src/local.rs` (only if not already present): `use serde::{Serialize, Deserialize};`. Add the derive on `ThreadSample` (src/local.rs:151). Match `DaemonSnapshot`'s derive set for the traits that must compose — `DaemonSnapshot` (src/remote.rs:27) derives `Clone, Default, Serialize, Deserialize`; `ThreadSample` nested in `focus_threads` needs at least `Clone, Serialize, Deserialize`:

```rust
#[derive(Clone, Serialize, Deserialize)]
pub struct ThreadSample {
```
(If `ThreadSample` already needs `Debug`/`Default` elsewhere, keep those — do not remove existing derives; there are none today.)

- [ ] **Step 4: Run test to verify pass**

Run: `cargo test --bin aerie thread_sample_json_round_trip && cargo build --bin aerie`
Expected: PASS; build clean.

- [ ] **Step 5: Commit**

```bash
git add src/local.rs
git commit -m "feat(remote): make ThreadSample serde-serializable (for focused-stream)"
```

---

### Task 2: Add `focus_threads` to `DaemonSnapshot`

**Files:**
- Modify: `src/remote.rs` (`DaemonSnapshot` struct ~line 28; imports)
- Test: `src/remote.rs` (extend the existing `daemon_snapshot_round_trip` test ~line 912)

**Interfaces:**
- Produces: `DaemonSnapshot.focus_threads: Option<(String, Vec<local::ThreadSample>)>` (serde-default None).

- [ ] **Step 1: Extend the round-trip test** — in `src/remote.rs`'s existing `daemon_snapshot_round_trip` test (~line 912), after building the snapshot, set and assert the new field. Add BOTH a Some case and confirm the default-None decode of old JSON:

```rust
        // focused-stream: the field round-trips...
        let mut snap2 = snap.clone();
        snap2.focus_threads = Some(("nginx".to_string(), vec![local::ThreadSample {
            pid: 1, tid: 2, name: "nginx".into(), cpu_pct: 3.0, faults_per_s: 0.0,
            disk_read_s: 0.0, disk_write_s: 0.0, ctx_switches_s: 0.0, sched_wait_pct: 0.0 }]));
        let j2 = serde_json::to_string(&snap2).unwrap();
        let back2: DaemonSnapshot = serde_json::from_str(&j2).unwrap();
        assert_eq!(back2.focus_threads.as_ref().unwrap().0, "nginx");
        // ...and old JSON without the field decodes as None (backward compat).
        let old_json = r#"{"entries":[],"total_ram_bytes":0,"snap_count":0,"sys_net_rx_s":0.0,"sys_net_tx_s":0.0,"sys_gpu_pct":null,"sys_rapl_w":0.0}"#;
        let old: DaemonSnapshot = serde_json::from_str(old_json).unwrap();
        assert!(old.focus_threads.is_none());
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin aerie daemon_snapshot_round_trip`
Expected: FAIL — no field `focus_threads`.

- [ ] **Step 3: Add the field + import.** In `src/remote.rs`, the `use crate::{...}` line (~line 7) already imports `BarEntry`; add `local` (or `local::ThreadSample`) so the type resolves. Add the field to `DaemonSnapshot` (after the last field, ~line 60), following the established `#[serde(default)]` pattern:

```rust
    /// Focused-stream: per-thread samples for ONE group the viewer asked the
    /// daemon to focus (group label, samples). `None` when no focus is set or
    /// from an old daemon. `#[serde(default)]` keeps old JSON decoding.
    #[serde(default)]
    pub focus_threads: Option<(String, Vec<local::ThreadSample>)>,
```

- [ ] **Step 4: Run test to verify pass**

Run: `cargo test --bin aerie daemon_snapshot_round_trip && cargo build --bin aerie`
Expected: PASS; build clean (the `run_daemon` snapshot literal at src/main.rs:2609 will now fail to compile for a missing field — fix it in the SAME step by adding `focus_threads: None,` to that literal as a temporary default; Task 3 replaces it with the real value). Re-run build to confirm.

- [ ] **Step 5: Commit**

```bash
git add src/remote.rs src/main.rs
git commit -m "feat(remote): add focus_threads field to DaemonSnapshot (serde-default)"
```

---

### Task 3: Daemon-side focused-thread sampling + stdin focus reader

**Files:**
- Modify: `src/main.rs` (`run_daemon` ~line 2541)
- Verify: local pipe test (no SSH)

**Interfaces:**
- Consumes: `local::sample_threads`, `local::Snapshot.groups[label].pids`, `DaemonSnapshot.focus_threads` (Task 2).

- [ ] **Step 1: Add the stdin focus reader thread.** At the top of `run_daemon` (before the emit loop at ~2553), spawn a thread that reads focus-group lines from stdin into a shared cell, so the emit loop never blocks on stdin:

```rust
    use std::sync::{Arc, Mutex};
    let focus: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    {
        let focus = Arc::clone(&focus);
        std::thread::spawn(move || {
            use std::io::BufRead;
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                let g = line.trim();
                *focus.lock().unwrap() = if g.is_empty() { None } else { Some(g.to_string()) };
            }
        });
    }
```

- [ ] **Step 2: Add the daemon-local focus delta state** — alongside the loop's existing prev state (near `prev_cpu_total`, ~line 2550):

```rust
    let mut prev_focus_snap: Option<local::ThreadSnapshot> = None;
    let mut prev_focus_label: Option<String> = None;
```

- [ ] **Step 3: Sample the focused group each cycle** — inside the loop, AFTER `local::sample(...)` produces `new_snap` (the `local::Snapshot`) and BEFORE building the `DaemonSnapshot` literal (~2609), compute `focus_threads`:

```rust
        let focus_label = focus.lock().unwrap().clone();
        // Reset the delta basis when the focused group changes (else cpu% is
        // computed across two different groups' counters).
        if prev_focus_label != focus_label {
            prev_focus_snap = None;
            prev_focus_label = focus_label.clone();
        }
        let focus_threads = match &focus_label {
            Some(label) => {
                let pids = new_snap.groups.get(label).map(|g| g.pids.clone()).unwrap_or_default();
                if pids.is_empty() { None } else {
                    match local::sample_threads(&pids, prev_focus_snap.take(), &local::ThreadFields::all(), new_snap.total) {
                        Ok((mut samples, snap)) => {
                            prev_focus_snap = Some(snap);
                            samples.sort_by(|a, b| b.cpu_pct.partial_cmp(&a.cpu_pct).unwrap_or(std::cmp::Ordering::Equal));
                            Some((label.clone(), samples))
                        }
                        Err(_) => None,
                    }
                }
            }
            None => None,
        };
```
Note: the local variable holding the `local::Snapshot` in `run_daemon` may be named differently (the seam map calls it `new_snap`; confirm the actual binding produced by `local::sample(...)` at ~2554 and use that name). `new_snap.total` is the cpu_total.

- [ ] **Step 4: Attach it to the emitted snapshot** — in the `DaemonSnapshot { ... }` literal (~2609), replace the temporary `focus_threads: None,` (from Task 2) with `focus_threads,`.

- [ ] **Step 5: Build**

Run: `cargo build --bin aerie && cargo test --bin aerie`
Expected: builds clean; existing tests pass.

- [ ] **Step 6: Verify locally by piping focus to a local daemon (no SSH).** The daemon reads focus from stdin and emits JSON with `focus_threads`. cpu% needs two cycles, so keep the pipe open ~3 intervals:

```bash
cargo build --bin aerie
# Feed one focus line, hold stdin open ~4s so the daemon emits several snapshots, capture output.
( printf 'aerie\n'; sleep 4 ) | ./target/debug/aerie --daemon --interval 1 > /tmp/daemon-out.jsonl 2>/dev/null
echo "=== last snapshot's focus_threads (group + a couple thread names/cpu) ==="
tail -1 /tmp/daemon-out.jsonl | python3 -c 'import sys,json; d=json.load(sys.stdin); ft=d.get("focus_threads"); print("group:", ft[0] if ft else None); print("threads:", [(t["name"], round(t["cpu_pct"],1)) for t in (ft[1][:4] if ft else [])])'
```
Expected: `group: aerie` and a non-empty threads list (aerie itself is running as the daemon-plus-monitor, so it has threads; any always-present group name works — pick one that exists locally, e.g. `aerie` or a comm you know is running). The first snapshot may have empty/zero cpu% (delta warmup); the last (after ~4 cycles) should show real thread names. If `focus_threads` is null, the focus reader or sampling is wrong — fix before committing. Also confirm a daemon with NO stdin focus still emits snapshots with `focus_threads: null` (run `./target/debug/aerie --daemon --interval 1 </dev/null | head -1` and check the field is null, proving the no-focus path and non-stalling emit).

- [ ] **Step 7: Commit**

```bash
git add src/main.rs
git commit -m "feat(daemon): focused-stream — sample one group's threads on stdin focus request"
```

---

### Task 4: `RemoteClient` piped stdin + `send_focus`

**Files:**
- Modify: `src/remote.rs` (`RemoteClient` struct ~84; `connect_direct` ~334; the other constructors `connect_kube_daemon` ~581, `connect_nomad_daemon` ~796)
- Verify: build (the daemon read side is already verified in Task 3; this is the viewer write side)

**Interfaces:**
- Produces: `RemoteClient::send_focus(&mut self, group: Option<&str>)`.

- [ ] **Step 1: Add the field to `RemoteClient`** (src/remote.rs:84):

```rust
    /// Stdin of the remote `aerie --daemon`, for sending focused-stream requests
    /// (`connect_direct` only; `None` for kube/nomad daemons which keep stdin null).
    focus_stdin: Option<std::process::ChildStdin>,
    /// Last focus group sent, to avoid rewriting the same line every tick.
    last_focus: Option<String>,
```

- [ ] **Step 2: Pipe stdin in `connect_direct`** (src/remote.rs). Change `.stdin(Stdio::null())` (line 362) to `.stdin(Stdio::piped())`, and after the spawn (near `child.stdout.take()` ~368) capture the stdin handle: `let focus_stdin = child.stdin.take();`. Add `focus_stdin, last_focus: None,` to the `RemoteClient { ... }` literal built at the end of `connect_direct` (~398).

- [ ] **Step 3: Initialize the field in the other constructors.** In `connect_kube_daemon` (~581, `RemoteClient` literal ~615) and `connect_nomad_daemon` (~796, literal ~839), add `focus_stdin: None, last_focus: None,` (these keep `stdin(Stdio::null())` — no focus channel).

- [ ] **Step 4: Add `send_focus`** near `try_recv` (src/remote.rs:102):

```rust
    /// Ask the remote daemon to focus (stream per-thread data for) `group`, or
    /// clear focus with `None`. No-op when unchanged or when there is no stdin
    /// pipe (non-`connect_direct` clients). Best-effort: write errors are ignored
    /// (a dead pipe surfaces via `is_alive`).
    pub fn send_focus(&mut self, group: Option<&str>) {
        let want = group.map(|s| s.to_string());
        if want == self.last_focus { return; }
        self.last_focus = want.clone();
        if let Some(stdin) = self.focus_stdin.as_mut() {
            use std::io::Write;
            // Empty line clears focus; a group name sets it (matches the daemon reader).
            let line = want.unwrap_or_default();
            let _ = writeln!(stdin, "{line}");
            let _ = stdin.flush();
        }
    }
```

- [ ] **Step 5: Build + tests**

Run: `cargo build --bin aerie && cargo test --bin aerie`
Expected: builds clean (all 3 RemoteClient constructors updated); existing tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/remote.rs
git commit -m "feat(remote): RemoteClient piped stdin + send_focus for focused-stream"
```

---

## Self-Review

**Spec coverage:** the focused-stream protocol is complete on both ends — the daemon reads a focus group from stdin (non-blocking reader thread), samples that one group's threads with a reset-on-change delta basis, and streams `focus_threads` (Task 1 serde + Task 2 field + Task 3 daemon); `RemoteClient` opens the SSH child's stdin and sends focus (Task 4). Verified locally end-to-end (Task 3 pipe test) without SSH. Out of scope (Slices B/C, correctly): the viewer UI — spine listing hosts, primary switching to the selected place, detail reading `focus_threads`, and calling `send_focus` from the poll site.

**Placeholder scan:** no TBD/TODO; new code shown in full. Task 3's `new_snap` binding name is flagged to confirm against the actual `local::sample` result variable — that is a read-the-code instruction, not a placeholder.

**Type consistency:** `ThreadSample: Serialize+Deserialize+Clone` (Task 1) is required by `focus_threads: Option<(String, Vec<ThreadSample>)>` (Task 2), produced by `sample_threads` (Task 3), sent via `send_focus` (Task 4). `focus_threads` field name identical across daemon literal, struct, and tests. The daemon delta-reset mirrors the viewer's existing fleet-detail reset (correctness parity). Backward-compat: `#[serde(default)]` on `focus_threads` matches the existing `sys_cpu_pct`/`sys_mem_used_bytes` pattern.
