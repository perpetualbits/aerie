// SPDX-License-Identifier: GPL-3.0-or-later
// Proxmox VE API client (v2 REST).
// Authentication: PVE API token in Authorization header.
// Token format: USER@REALM!TOKENID=UUID-SECRET
//
// Per-VM CPU% is normalised to vCPU capacity so 100 % = all vCPUs pegged.

use crate::BarEntry;
use anyhow::{Context, Result};
use reqwest::blocking::Client as Http;
use serde::Deserialize;
use std::{collections::HashMap, time::{Duration, Instant}};

/// Proxmox-specific metadata for a VM; used for SSH host discovery.
///
/// Stored in `AppState::vm_meta` keyed by the VM's display label (name or "vmN").
/// When the user presses Enter on a Proxmox VM row, this struct tells `connect_vm`
/// which node the VM lives on (needed for the guest-agent API path) and whether
/// it is a QEMU VM (supports the guest agent) or an LXC container (does not).
pub struct VmMeta {
    /// Proxmox node name where this VM is running, e.g. "pve1".
    pub node: String,
    /// Numeric VMID, used to construct API paths like `/nodes/{node}/qemu/{vmid}/...`.
    pub vmid: u64,
    /// "qemu" for full VMs, "lxc" for containers.
    pub kind: String,
}

/// Previous disk I/O counters for one VM, used to compute per-second rates.
///
/// The PVE API returns cumulative `diskread` / `diskwrite` byte counters.
/// We store them with the timestamp of the previous sample to compute the delta.
struct PrevVals {
    diskread: u64,
    diskwrite: u64,
    at: Instant,
}

/// Blocking HTTP client for the Proxmox VE REST API.
///
/// Holds a `reqwest` blocking client (thread-safe but synchronous) and the
/// per-VM previous-sample map for disk I/O delta computation.
pub struct Client {
    http: Http,
    /// API base URL, e.g. "https://pve.lan:8006" (no trailing slash).
    base: String,
    /// Value of the Authorization header: "PVEAPIToken=USER@REALM!TOKENID=SECRET".
    auth: String,
    /// Per-VMID disk I/O counters from the previous `sample()` call.
    prev: HashMap<u64, PrevVals>,
}

impl Client {
    /// Construct a new API client.
    ///
    /// - `base_url`: Proxmox API base, e.g. "https://pve.lan:8006".  Trailing slashes
    ///   are stripped so path construction (`format!("{}{}", base, path)`) is consistent.
    /// - `token`: API token in "USER@REALM!TOKENID=UUID" format.
    /// - `insecure`: If true, disables TLS certificate verification.  This is useful
    ///   for home-lab Proxmox installs with self-signed certificates.
    ///
    /// The HTTP client has a 10-second timeout per request so a slow or unreachable
    /// Proxmox server does not block the UI thread indefinitely.
    pub fn new(base_url: &str, token: &str, insecure: bool) -> Result<Self> {
        let http = Http::builder()
            .danger_accept_invalid_certs(insecure)
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            base: base_url.trim_end_matches('/').to_string(),
            auth: format!("PVEAPIToken={token}"),
            prev: HashMap::new(),
        })
    }

    /// Issue a GET request to the PVE API and deserialise the `data` field.
    ///
    /// All PVE REST responses have the envelope `{"data": T}`.  This method
    /// unwraps the envelope transparently, so callers receive `T` directly.
    ///
    /// Errors include network failures, HTTP 4xx/5xx status codes, and JSON
    /// parse errors, each annotated with the full URL for diagnostics.
    fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        #[derive(Deserialize)]
        struct Wrap<T> {
            data: T,
        }
        let url = format!("{}{}", self.base, path);
        let w: Wrap<T> = self
            .http
            .get(&url)
            .header("Authorization", &self.auth)
            .send()
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("HTTP error from {url}"))?
            .json()
            .with_context(|| format!("parse JSON from {url}"))?;
        Ok(w.data)
    }

    /// Fetch all running VMs/containers across all nodes and compute metrics.
    ///
    /// Returns `(entries, meta_map)` where:
    /// - `entries`: one `BarEntry` per running VM, with CPU%, memory, and disk I/O.
    /// - `meta_map`: keyed by VM label (name or "vmN"); used by `connect_vm` for
    ///   SSH host discovery via the Proxmox guest agent.
    ///
    /// API paths used:
    /// - `/api2/json/nodes` — list of cluster nodes.
    /// - `/api2/json/nodes/{node}/qemu` — QEMU VMs on each node.
    /// - `/api2/json/nodes/{node}/lxc` — LXC containers on each node.
    ///
    /// CPU% normalisation:
    ///   The PVE API returns `cpu` as a fraction [0, 1] relative to all vCPUs.
    ///   We compute `cpu_pct = (cpu / cpus) × 100` so 100% = one full vCPU pegged,
    ///   consistent with the local mode where 100% = one full physical core.
    ///
    /// Disk I/O delta:
    ///   `diskread` / `diskwrite` are cumulative byte counters. We compute the rate
    ///   by diffing against `self.prev` using the actual elapsed time. On the first
    ///   call (no previous data) disk rates are reported as 0.
    pub fn sample(&mut self) -> Result<(Vec<BarEntry>, HashMap<String, VmMeta>)> {
        #[derive(Deserialize)]
        struct Node {
            node: String,
        }

        #[derive(Deserialize, Default)]
        struct Vm {
            #[serde(default)]
            name: Option<String>,
            vmid: u64,
            #[serde(default)]
            /// CPU utilisation as a fraction [0,1] of the VM's total vCPU capacity.
            cpu: f64,
            #[serde(default)]
            /// Number of vCPUs allocated to this VM.
            cpus: u32,
            #[serde(default)]
            /// Bytes of RAM currently in use by the VM.
            mem: u64,
            #[serde(default)]
            /// Maximum RAM allocation in bytes.
            maxmem: u64,
            #[serde(default)]
            /// VM status string; we only process "running" VMs.
            status: String,
            #[serde(default)]
            /// Cumulative bytes read from storage since VM start.
            diskread: u64,
            #[serde(default)]
            /// Cumulative bytes written to storage since VM start.
            diskwrite: u64,
            #[serde(default)]
            /// VM uptime in seconds; used to format the uptime badge in `extra`.
            uptime: u64,
        }

        let now = Instant::now();
        let nodes: Vec<Node> = self.get("/api2/json/nodes")?;
        let mut out = Vec::new();
        let mut meta_map = HashMap::new();

        for node in &nodes {
            for kind in ["qemu", "lxc"] {
                let path = format!("/api2/json/nodes/{}/{kind}", node.node);
                // unwrap_or_default: if the node returns an error for this kind
                // (e.g. LXC not installed), treat it as empty rather than aborting.
                let vms: Vec<Vm> = self.get(&path).unwrap_or_default();
                for vm in vms {
                    if vm.status != "running" {
                        continue;
                    }

                    // Compute disk I/O rates from the cumulative counter delta.
                    let (disk_read_s, disk_write_s) = if let Some(p) = self.prev.get(&vm.vmid) {
                        let dt = now.duration_since(p.at).as_secs_f64().max(0.001);
                        (
                            vm.diskread.saturating_sub(p.diskread) as f64 / dt,
                            vm.diskwrite.saturating_sub(p.diskwrite) as f64 / dt,
                        )
                    } else {
                        (0.0, 0.0) // no previous sample yet
                    };

                    // Store current counters for the next delta computation.
                    self.prev.insert(
                        vm.vmid,
                        PrevVals { diskread: vm.diskread, diskwrite: vm.diskwrite, at: now },
                    );

                    // Normalise CPU: PVE `cpu` is [0,1] relative to total vCPU capacity.
                    // Dividing by `cpus` gives per-vCPU fraction; × 100 for percentage.
                    let cpu_pct = if vm.cpus > 0 {
                        (vm.cpu / vm.cpus as f64 * 100.0).min(100.0)
                    } else {
                        // Fallback if cpus is missing: treat cpu as a fraction [0,1].
                        (vm.cpu * 100.0).min(100.0)
                    };
                    let mem_pct = if vm.maxmem > 0 {
                        vm.mem as f64 / vm.maxmem as f64 * 100.0
                    } else {
                        0.0
                    };

                    // Use VM name if provided; fall back to "vmN" for nameless VMs.
                    let label =
                        vm.name.clone().unwrap_or_else(|| format!("vm{}", vm.vmid));
                    // Display memory in GiB (1 073 741 824 bytes per GiB).
                    let mem_used = vm.mem as f64 / 1_073_741_824.0;
                    let mem_max = vm.maxmem as f64 / 1_073_741_824.0;
                    // [Q] for QEMU VMs, [C] for LXC containers.
                    let kind_badge = if kind == "qemu" { "Q" } else { "C" };
                    let uptime_str = fmt_uptime(vm.uptime);

                    out.push(BarEntry {
                        label: label.clone(),
                        value: cpu_pct,
                        count: None, // Proxmox doesn't expose per-VM thread counts
                        extra: format!(
                            "[{kind_badge}] {mem_used:.1}/{mem_max:.1}G  up {uptime_str}"
                        ),
                        rss_bytes: vm.mem, // store raw bytes; display logic formats it
                        mem_pct,
                        disk_read_s,
                        disk_write_s,
                        ..Default::default()
                    });

                    meta_map.insert(
                        label,
                        VmMeta { node: node.node.clone(), vmid: vm.vmid, kind: kind.to_string() },
                    );
                }
            }
        }
        Ok((out, meta_map))
    }

    /// Query the QEMU guest agent for a VM's network interface IPs.
    ///
    /// This requires `qemu-guest-agent` to be installed and running inside the VM.
    /// For LXC containers, the guest agent is not available; callers should skip this.
    ///
    /// API path: `/api2/json/nodes/{node}/qemu/{vmid}/agent/network-get-interfaces`
    ///
    /// Returns IPv4 addresses first, then IPv6, with the following filtered out:
    /// - Loopback interface (`lo`).
    /// - `127.0.0.1` and `127.x.x.x` (IPv4 loopback).
    /// - `169.254.x.x` (IPv4 link-local / APIPA — not routable).
    /// - `::1` (IPv6 loopback).
    pub fn get_vm_ips(&self, node: &str, vmid: u64) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct AgentData {
            result: Vec<Iface>,
        }
        #[derive(Deserialize)]
        struct Iface {
            #[serde(default)]
            name: String,
            #[serde(rename = "ip-addresses", default)]
            ips: Vec<IpEntry>,
        }
        #[derive(Deserialize)]
        struct IpEntry {
            #[serde(rename = "ip-address")]
            addr: String,
            #[serde(rename = "ip-address-type")]
            kind: String,
        }

        let path = format!(
            "/api2/json/nodes/{node}/qemu/{vmid}/agent/network-get-interfaces"
        );
        let data: AgentData = self.get(&path)?;

        let mut ipv4 = Vec::new();
        let mut ipv6 = Vec::new();
        for iface in &data.result {
            if iface.name == "lo" {
                continue;
            }
            for ip in &iface.ips {
                let a = ip.addr.as_str();
                // Skip loopback and link-local addresses which are not routable.
                if a.starts_with("127.") || a.starts_with("169.254.") || a == "::1" {
                    continue;
                }
                if ip.kind == "ipv4" {
                    ipv4.push(ip.addr.clone());
                } else {
                    ipv6.push(ip.addr.clone());
                }
            }
        }
        // IPv4 first (preferred for SSH); append IPv6 as fallback.
        ipv4.extend(ipv6);
        Ok(ipv4)
    }
}

/// Format uptime in seconds as a compact human-readable string.
///
/// Examples: "3d14h", "5h30m", "45m"
/// Days are shown when the VM has been running for ≥ 24 h.
fn fmt_uptime(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{mins}m")
    } else {
        format!("{mins}m")
    }
}
