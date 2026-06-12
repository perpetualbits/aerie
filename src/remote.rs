// SPDX-License-Identifier: GPL-3.0-or-later
// SSH-based remote daemon client.
// The remote machine runs `apptop --daemon` which streams newline-delimited
// JSON snapshots to stdout. This module handles host discovery, SSH spawn,
// and the reader thread that feeds a channel consumed by the main event loop.

use crate::{proxmox, BarEntry};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, BufReader},
    net::{TcpStream, ToSocketAddrs},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread::JoinHandle,
    time::Duration,
};

/// One snapshot emitted by `apptop --daemon` each refresh cycle.
///
/// Serialised as a single JSON object and written to stdout as one line.
/// The remote UI's reader thread deserialises each line into this struct and
/// forwards it over an mpsc channel to the main event loop.
///
/// The `*_complete` fields in each `BarEntry` use `#[serde(default = "default_true")]`
/// so old daemon versions that lacked these fields decode as fully complete.
#[derive(Serialize, Deserialize)]
pub struct DaemonSnapshot {
    /// One entry per active process group on the remote machine.
    pub entries: Vec<BarEntry>,
    /// Total physical RAM on the remote machine, in bytes.
    pub total_ram_bytes: u64,
    /// Number of snapshots collected by the daemon so far (drives the "collecting…" message).
    pub snap_count: usize,
    /// System-wide network receive bytes/s on the remote machine.
    pub sys_net_rx_s: f64,
    /// System-wide network transmit bytes/s on the remote machine.
    pub sys_net_tx_s: f64,
    /// GPU utilisation % on the remote machine, or None if unavailable.
    pub sys_gpu_pct: Option<f64>,
    /// Total RAPL package power draw on the remote machine in watts.
    pub sys_rapl_w: f64,
    /// System-level CPU PSI "some avg10" from /proc/pressure/cpu.
    #[serde(default)]
    pub sys_psi_cpu: Option<f64>,
    /// System-level memory PSI "some avg10" from /proc/pressure/memory.
    #[serde(default)]
    pub sys_psi_mem: Option<f64>,
    /// System-level I/O PSI "some avg10" from /proc/pressure/io.
    #[serde(default)]
    pub sys_psi_io: Option<f64>,
}

/// SSH host-key checking policy.
///
/// Controls the `StrictHostKeyChecking` SSH option. The default (`Strict`) requires
/// the host key to already be present in `~/.ssh/known_hosts`. `AcceptNew` enables
/// Trust-On-First-Use (TOFU): new host keys are accepted and stored, but changed
/// keys still cause an error. We never use `StrictHostKeyChecking=no` as that is
/// a security risk even in private networks.
pub enum SshHostKeyPolicy {
    /// StrictHostKeyChecking=yes — host key must be in known_hosts (default).
    Strict,
    /// StrictHostKeyChecking=accept-new — TOFU, explicit opt-in only.
    AcceptNew,
}

/// Live SSH connection to a remote `apptop --daemon` instance.
///
/// The connection consists of:
/// - `child`: the `ssh` subprocess with its stdout piped.
/// - A reader thread (`_thread`) that reads JSON lines from the SSH stdout
///   and sends `DaemonSnapshot` values over `recv`.
/// - `host`: the resolved hostname or IP we actually connected to (shown in footer).
pub struct RemoteClient {
    /// The spawned `ssh` subprocess.
    child: Child,
    /// Channel receiver for decoded `DaemonSnapshot` messages from the reader thread.
    recv: Receiver<DaemonSnapshot>,
    /// Reader thread handle. Kept alive via ownership; the thread exits when the
    /// SSH stdout closes or when `tx.send` fails (receiver dropped).
    _thread: JoinHandle<()>,
    /// The hostname or IP that `ssh` is connected to (e.g. "10.0.0.5").
    pub host: String,
}

impl RemoteClient {
    /// Non-blocking drain of the snapshot channel.
    ///
    /// Reads all pending snapshots and returns only the most recent one.
    /// Intermediate snapshots are discarded so the UI always shows the freshest data.
    /// Returns `None` if no new snapshot has arrived since the last call.
    pub fn try_recv(&mut self) -> Option<DaemonSnapshot> {
        let mut latest = None;
        while let Ok(snap) = self.recv.try_recv() {
            latest = Some(snap);
        }
        latest
    }

    /// Returns true while the SSH subprocess is still running.
    ///
    /// Uses `try_wait` (non-blocking) to avoid blocking the main event loop.
    /// `Ok(None)` means the process has not exited yet.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Kill the SSH subprocess and wait for it to exit.
    ///
    /// Consumes `self` so the client cannot be used after closing.
    /// The reader thread will exit naturally once the SSH stdout closes.
    pub fn close(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Validate that `user` and `host` are safe to pass as SSH command-line arguments.
///
/// Rejects values that start with `-` to prevent option injection into the `ssh`
/// command. For example, a host of `-oProxyCommand=evil` would be interpreted
/// as an SSH option, potentially allowing arbitrary command execution.
///
/// Returns `Ok(())` if both values are safe, or an `Err(String)` with a diagnostic
/// message if either is rejected.
pub fn validate_ssh_target(user: &str, host: &str) -> Result<(), String> {
    if user.starts_with('-') {
        return Err(format!(
            "ssh_user '{user}' begins with '-' and would be interpreted as an ssh option"
        ));
    }
    if host.starts_with('-') {
        return Err(format!(
            "host '{host}' begins with '-' and would be interpreted as an ssh option"
        ));
    }
    Ok(())
}

/// Build an ordered, de-duplicated list of `(host, source_description)` candidates
/// for SSH connection to the given VM.
///
/// Candidates are assembled in priority order:
/// 1. IPs returned by the Proxmox guest agent (most reliable — direct from the VM).
/// 2. IPs resolved by DNS from the VM label (works when the VM name is in DNS).
/// 3. The bare VM label itself as a last-resort hostname (for /etc/hosts or mDNS).
///
/// De-duplication: if a DNS result matches an already-added guest-agent IP (string
/// comparison), it is not added again. The bare label is only appended if it is not
/// already in the candidate list.
///
/// The `source_description` field is used in diagnostic messages when connection fails.
pub fn build_candidates(
    label: &str,
    agent_ips: &[String],
    dns_addrs: &[std::net::SocketAddr],
) -> Vec<(String, String)> {
    let mut candidates: Vec<(String, String)> = Vec::new();
    for ip in agent_ips {
        candidates.push((ip.clone(), format!("guest-agent: {ip}")));
    }
    for a in dns_addrs {
        let ip = a.ip().to_string();
        // Skip if this IP was already added from the guest agent.
        if !candidates.iter().any(|(h, _)| h == &ip) {
            candidates.push((ip.clone(), format!("DNS → {ip}")));
        }
    }
    // Append the bare label only if it is not already a candidate string.
    if !candidates.iter().any(|(h, _)| h == label) {
        candidates.push((label.to_string(), format!("hostname: {label}")));
    }
    candidates
}

/// Simple gating predicate: returns true if remote drill-down is enabled.
///
/// This exists as a named function rather than an inline `bool` check so it can
/// be unit-tested (the test covers the gating logic separately from SSH behaviour).
#[allow(dead_code)]
pub fn remote_enabled_for_vm(enable_remote: bool) -> bool {
    enable_remote
}

/// Try to connect to a VM over SSH, attempting multiple host-discovery methods.
///
/// This call is **blocking** and can take up to ~8 seconds in the worst case
/// (TCP probe timeout per candidate × number of candidates). The caller is
/// expected to render a "Connecting…" screen before calling this function.
///
/// Host discovery sequence:
/// 1. Proxmox guest agent (`get_vm_ips`) — returns IPs directly from the VM's
///    network stack. Only available for QEMU VMs with qemu-guest-agent installed.
/// 2. DNS lookup of the VM's display name (label).
/// 3. Bare label as hostname (falls through to /etc/hosts, mDNS, etc.).
///
/// For each candidate:
/// 1. Validates the user/host strings (no leading `-`).
/// 2. TCP-probes port 22 with a 2-second timeout.
/// 3. Attempts to spawn `ssh … apptop --daemon` with `BatchMode=yes`.
/// 4. Waits 600 ms for an immediate SSH failure (bad key, `apptop` not in PATH).
///
/// Returns:
/// - `Ok(RemoteClient)` on the first successful connection.
/// - `Err(Vec<String>)` if all candidates fail; the vector is a diagnostic log
///   suitable for display in the UI error area.
pub fn connect_vm(
    label: &str,
    meta: Option<&proxmox::VmMeta>,
    proxmox_client: Option<&proxmox::Client>,
    ssh_user: &str,
    policy: SshHostKeyPolicy,
) -> Result<RemoteClient, Vec<String>> {
    let mut diag: Vec<String> = Vec::new();

    // Record the SSH policy in diagnostics so the user knows which mode was active.
    let policy_desc = match policy {
        SshHostKeyPolicy::Strict => "SSH policy: strict (host key must be in known_hosts)",
        SshHostKeyPolicy::AcceptNew => "SSH policy: accept-new (TOFU)",
    };
    diag.push(policy_desc.to_string());

    // 1. Proxmox guest agent — works for QEMU VMs with qemu-guest-agent installed.
    let mut agent_ips: Vec<String> = Vec::new();
    if let (Some(m), Some(pve)) = (meta, proxmox_client) {
        if m.kind == "qemu" {
            match pve.get_vm_ips(&m.node, m.vmid) {
                Ok(ips) if !ips.is_empty() => {
                    agent_ips = ips;
                }
                Ok(_) => diag
                    .push("guest-agent: no IPs returned (qemu-guest-agent not running?)".into()),
                Err(e) => diag.push(format!("guest-agent: {e}")),
            }
        }
    }

    // 2. DNS lookup of the VM name.  Format as "label:22" for ToSocketAddrs.
    let dns_addrs: Vec<std::net::SocketAddr> = format!("{label}:22")
        .to_socket_addrs()
        .map(|it| it.collect())
        .unwrap_or_default();
    if dns_addrs.is_empty() {
        diag.push(format!("DNS: '{label}' did not resolve"));
    }

    // Build the ordered, de-duplicated candidate list.
    let candidates = build_candidates(label, &agent_ips, &dns_addrs);

    // Probe port 22 and attempt SSH for each candidate.
    let mut tried: Vec<String> = Vec::new();
    for (host, source) in &candidates {
        // Reject hosts that start with '-' to prevent SSH option injection.
        if let Err(e) = validate_ssh_target(ssh_user, host) {
            tried.push(format!("  {source}  →  skipped: {e}"));
            continue;
        }
        if !probe_port_22(host) {
            tried.push(format!("  {source}  →  port 22 not reachable"));
            continue;
        }
        match spawn_daemon(host, ssh_user, &policy) {
            Ok(client) => return Ok(client),
            Err(e) => tried.push(format!("  {source}  →  {e}")),
        }
    }

    // All candidates failed: build a diagnostic report for the UI.
    if candidates.is_empty() {
        diag.push("No candidate addresses found.".into());
    } else {
        diag.push("All connection attempts failed:".into());
        diag.extend(tried);
    }
    // Provide actionable remediation steps.
    diag.push(String::new());
    diag.push("To fix:".into());
    diag.push(format!("  • SSH key auth:   ssh-copy-id {ssh_user}@{label}"));
    diag.push(format!(
        "  • Install apptop: scp apptop {ssh_user}@{label}:/usr/local/bin/"
    ));
    diag.push("  • Confirm the VM is running and reachable on the network".into());
    Err(diag)
}

/// TCP probe: returns true if port 22 is reachable within 2 seconds.
///
/// Used before attempting SSH so we can report "port 22 not reachable" instead
/// of waiting for the SSH connection timeout (which can be 30+ seconds).
///
/// Resolves `host:22` to a `SocketAddr` and attempts a TCP connect with a 2-second
/// timeout. Returns false on any error (DNS failure, unreachable, timeout).
pub fn probe_port_22(host: &str) -> bool {
    let addr = match format!("{host}:22").to_socket_addrs() {
        Ok(mut it) => match it.next() {
            Some(a) => a,
            None => return false,
        },
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok()
}

/// Spawn an `ssh` subprocess that runs `apptop --daemon` on the remote host.
///
/// SSH options used:
/// - `BatchMode=yes`: disables password prompts; fails immediately if key auth fails.
/// - `ConnectTimeout=5`: 5-second TCP connection timeout.
/// - `StrictHostKeyChecking`: controlled by `policy`.
/// - `ServerAliveInterval=10` + `ServerAliveCountMax=2`: detect dead connections
///   within ~20 seconds (sends keepalive packets every 10 s; fails after 2 misses).
/// - `--`: argument terminator to prevent `user@host` from being parsed as an option.
///
/// After spawning, we sleep 600 ms and call `try_wait` to detect fast failures:
/// SSH exits immediately when key auth fails or `apptop` is not in PATH.
/// If the process has already exited, we return an error with the exit status.
///
/// On success, a reader thread is spawned that parses JSON lines from the SSH
/// stdout and forwards `DaemonSnapshot` values over an mpsc channel.
fn spawn_daemon(host: &str, user: &str, policy: &SshHostKeyPolicy) -> Result<RemoteClient> {
    // Validate before constructing any SSH argument.
    validate_ssh_target(user, host)
        .map_err(|e| anyhow!("{e}"))?;

    let strict_value = match policy {
        SshHostKeyPolicy::Strict    => "StrictHostKeyChecking=yes",
        SshHostKeyPolicy::AcceptNew => "StrictHostKeyChecking=accept-new",
    };

    let target = format!("{user}@{host}");
    let mut child = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            strict_value,
            "-o",
            "ServerAliveInterval=10",
            "-o",
            "ServerAliveCountMax=2",
            "--",           // prevents user@host from being parsed as an option
            &target,
            "apptop --daemon",
        ])
        .stdin(Stdio::null())   // no input needed from the remote daemon
        .stdout(Stdio::piped()) // we read JSON snapshots from stdout
        .stderr(Stdio::null())  // suppress SSH banners and warnings
        .spawn()
        .map_err(|e| anyhow!("could not run ssh: {e}"))?;

    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout pipe"))?;
    let (tx, rx) = mpsc::channel();
    // Spawn a background thread to read JSON lines from SSH stdout.
    // The thread exits when the pipe closes (SSH disconnects) or when the
    // channel send fails (RemoteClient was dropped, tx no longer connected).
    let thread = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(snap) = serde_json::from_str::<DaemonSnapshot>(&line) {
                if tx.send(snap).is_err() {
                    break;
                }
            }
        }
    });

    // Brief pause so SSH can fail fast on obvious errors (bad key, refused, apptop absent…).
    // 600 ms is enough for `ssh` to complete the TCP handshake, authenticate, and fail
    // if the remote `apptop` binary is missing, without being long enough to feel sluggish
    // on a fast LAN connection.
    std::thread::sleep(Duration::from_millis(600));
    match child.try_wait() {
        Ok(Some(status)) => {
            return Err(anyhow!(
                "SSH exited immediately ({status}) — check key auth and that apptop is in PATH on the remote"
            ));
        }
        Err(e) => return Err(anyhow!("process error: {e}")),
        Ok(None) => {} // process is still running — connection looks good
    }

    Ok(RemoteClient { child, recv: rx, _thread: thread, host: host.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_snapshot_round_trip() {
        use crate::{BarEntry, default_true};
        let snap = DaemonSnapshot {
            entries: vec![BarEntry {
                label: "test".into(),
                value: 42.0,
                disk_complete: true,
                status_complete: true,
                ..Default::default()
            }],
            total_ram_bytes: 8 * 1024 * 1024 * 1024,
            snap_count: 3,
            sys_net_rx_s: 1000.0,
            sys_net_tx_s: 2000.0,
            sys_gpu_pct: Some(50.0),
            sys_rapl_w: 35.0,
            sys_psi_cpu: Some(1.5),
            sys_psi_mem: None,
            sys_psi_io: None,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: DaemonSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.snap_count, 3);
        assert_eq!(back.entries[0].label, "test");
        assert!(back.entries[0].disk_complete);
        // suppress unused import warning
        let _ = default_true();
    }

    #[test]
    fn bar_entry_forward_compat_missing_complete_fields() {
        // Simulate a JSON from an old daemon that doesn't have *_complete fields.
        // They should default to true (complete).
        let json = r#"{"label":"old","value":10.0,"count":null,"extra":"","rss_bytes":0,"mem_pct":0.0,"page_faults_s":0.0,"disk_read_s":0.0,"disk_write_s":0.0,"ctx_switches_s":0.0,"open_fds":0,"swap_bytes":0,"sched_wait_pct":0.0,"power_w":0.0,"fading":false}"#;
        let entry: crate::BarEntry = serde_json::from_str(json).unwrap();
        assert!(entry.disk_complete, "missing field should default to true");
        assert!(entry.status_complete, "missing field should default to true");
        assert!(entry.fds_complete, "missing field should default to true");
        assert!(entry.sched_complete, "missing field should default to true");
        assert!(entry.rss_complete, "missing field should default to true");
    }

    #[test]
    fn build_candidates_ordering_and_dedup() {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        let agent_ips = vec!["10.0.0.5".to_string()];
        let dns_addrs = vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)), 22), // duplicate of agent IP
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 6)), 22),
        ];
        let candidates = build_candidates("myvm", &agent_ips, &dns_addrs);

        // agent IP is first
        assert_eq!(candidates[0].0, "10.0.0.5");
        assert!(candidates[0].1.contains("guest-agent"));

        // duplicate IP is not re-added from DNS
        assert!(!candidates.iter().skip(1).any(|(h, _)| h == "10.0.0.5"));

        // 10.0.0.6 from DNS is present
        assert!(candidates.iter().any(|(h, _)| h == "10.0.0.6"));

        // bare hostname is last
        let last = candidates.last().unwrap();
        assert_eq!(last.0, "myvm");
        assert!(last.1.contains("hostname"));
    }

    #[test]
    fn build_candidates_no_agent_no_dns() {
        // When there's no guest-agent and DNS fails, fallback is always the label.
        let candidates = build_candidates("somevm", &[], &[]);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, "somevm");
    }

    #[test]
    fn build_candidates_label_already_in_dns() {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        // If "myvm" resolves to an IP, and we'd add "myvm" as fallback,
        // but "myvm" is already in the list (different string), it still gets added.
        // If "myvm" the string is already a candidate, don't duplicate.
        let dns_addrs = vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7)), 22),
        ];
        let candidates = build_candidates("myvm", &[], &dns_addrs);
        // myvm as string is not "10.0.0.7", so "myvm" fallback is added last
        assert!(candidates.last().unwrap().0 == "myvm");
        // "myvm" appears exactly once
        let count = candidates.iter().filter(|(h, _)| h == "myvm").count();
        assert_eq!(count, 1);
    }

    #[test]
    fn validate_ssh_target_rejects_dash_prefix_host() {
        let result = validate_ssh_target("ubuntu", "-oProxyCommand=evil");
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("begins with '-'"));
    }

    #[test]
    fn validate_ssh_target_rejects_dash_prefix_user() {
        let result = validate_ssh_target("-lroot", "10.0.0.1");
        assert!(result.is_err());
    }

    #[test]
    fn validate_ssh_target_accepts_normal() {
        assert!(validate_ssh_target("ubuntu", "10.0.0.1").is_ok());
        assert!(validate_ssh_target("root", "myvm.example.com").is_ok());
    }

    #[test]
    fn gating_predicate() {
        assert!(!remote_enabled_for_vm(false));
        assert!(remote_enabled_for_vm(true));
    }
}
