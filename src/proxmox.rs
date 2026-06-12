// SPDX-License-Identifier: GPL-3.0-or-later
// Proxmox VE API client (v2 REST).
// Authentication: PVE API token in Authorization header.
// Token format: USER@REALM!TOKENID=UUID-SECRET
//
// Tier 1: all resource data comes from a single call to
//   GET /api2/json/cluster/resources
// which returns VMs, nodes, and storage in one response.
// Grouping (Flat/Pool/Tag/Node) is applied here; the caller receives
// pre-aggregated BarEntries and per-group fair-share member values.

use crate::{BarEntry, GroupLabel, Metric, PveGroupBy};
use anyhow::{Context, Result};
use reqwest::blocking::Client as Http;
use serde::Deserialize;
use std::{collections::HashMap, time::{Duration, Instant}};

/// Proxmox-specific metadata for a VM/CT; used for SSH host discovery.
///
/// Stored in `AppState::vm_meta` keyed by the VM's display label.
/// When the user presses Enter on a row, this tells `connect_vm` which
/// node the VM lives on and whether it is QEMU (supports guest agent) or LXC.
pub struct VmMeta {
    /// Proxmox node name, e.g. "pve1".
    pub node: String,
    /// Numeric VMID, used to build API paths like `/nodes/{node}/qemu/{vmid}/…`.
    pub vmid: u64,
    /// "qemu" for full VMs, "lxc" for containers.
    pub kind: String,
}

/// CPU/memory snapshot for one Proxmox node; shown in the footer status line.
pub struct NodeStatus {
    /// Node hostname, e.g. "pve1".
    pub node: String,
    /// CPU utilisation as a percentage [0, 100].
    pub cpu_pct: f64,
    /// RAM currently in use, bytes.
    pub mem_used: u64,
    /// Total RAM capacity, bytes.
    pub mem_total: u64,
}

/// Disk usage for one Proxmox storage; shown in the footer status line.
pub struct StorageStatus {
    /// Storage ID, e.g. "local-zfs".
    pub storage: String,
    /// Node that owns this storage entry.
    pub node: String,
    /// Bytes currently used.
    pub used: u64,
    /// Total storage capacity, bytes.
    pub total: u64,
}

/// Full result of one `sample()` call, handed to `PvePacket`.
pub struct SampleResult {
    /// One `BarEntry` per group (one VM in Flat mode, one pool/tag/node otherwise).
    pub entries: Vec<BarEntry>,
    /// VM metadata for SSH host discovery, keyed by the entry label.
    pub meta: HashMap<String, VmMeta>,
    /// Per-group fair-share values for each of the four Proxmox metrics.
    /// The main thread picks whichever metric is currently active for the overlay.
    pub member_vals: HashMap<GroupLabel, HashMap<Metric, Vec<f64>>>,
    /// Node-level CPU/memory snapshot for the footer.
    pub node_status: Vec<NodeStatus>,
    /// Storage fill snapshot for the footer.
    pub storage_status: Vec<StorageStatus>,
}

// One serde struct covers all resource types from the flat cluster/resources endpoint.
// Fields absent for a given resource type get their Default via #[serde(default)].
#[derive(Deserialize)]
struct Res {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)] node: String,
    // VM / container fields
    #[serde(default)] vmid: Option<u64>,
    #[serde(default)] name: Option<String>,
    #[serde(default)] pool: Option<String>,
    #[serde(default)] tags: Option<String>, // semicolon-separated
    #[serde(default)] cpu: f64,
    #[serde(default)] cpus: u32,
    #[serde(default)] mem: u64,
    #[serde(default)] maxmem: u64,
    #[serde(default)] diskread: u64,
    #[serde(default)] diskwrite: u64,
    #[serde(default)] uptime: u64,
    #[serde(default)] status: String,
    // node-only (maxcpu used to verify the field is populated)
    #[serde(default)] maxcpu: u32,
    // storage-only
    #[serde(default)] storage: Option<String>,
    #[serde(default)] disk: u64,
    #[serde(default)] maxdisk: u64,
}

type GroupsResult = (
    Vec<BarEntry>,
    HashMap<String, VmMeta>,
    HashMap<GroupLabel, HashMap<Metric, Vec<f64>>>,
    Vec<NodeStatus>,
    Vec<StorageStatus>,
);

/// Pure grouping transform: takes already-parsed resource structs and pre-computed disk
/// rates, applies the grouping strategy, and returns fully aggregated BarEntries.
///
/// No HTTP, no time-delta logic — those belong in `Client::sample`. Keeping this pure
/// makes it trivially testable against fixture data.
///
/// `read_rates`: vmid → (read_bytes/s, write_bytes/s) computed by the caller from
/// successive cumulative counters.
fn build_groups(
    resources: &[Res],
    read_rates: &HashMap<u64, (f64, f64)>,
    group_by: PveGroupBy,
) -> GroupsResult {
    // One accumulator per group: collects all member VMs before building the BarEntry.
    struct Accum {
        cpu_load: f64,   // sum of (res.cpu × res.cpus); numerator of weighted avg
        cpus: u32,       // sum of res.cpus; denominator
        mem: u64,
        maxmem: u64,
        read_s: f64,
        write_s: f64,
        count: u32,
        cpu_vals: Vec<f64>,   // per-VM: res.cpu × 100 (% of that VM's total capacity)
        mem_vals: Vec<f64>,   // per-VM: res.mem as f64 (bytes used)
        read_vals: Vec<f64>,  // per-VM: disk read bytes/s
        write_vals: Vec<f64>, // per-VM: disk write bytes/s
        uptime_max: u64,
        // Representative VM info (first in group) for VmMeta and Flat extra text.
        first_vmid: u64,
        first_node: String,
        first_kind: String,  // "qemu" or "lxc"
    }

    let mut node_status: Vec<NodeStatus> = Vec::new();
    let mut storage_status: Vec<StorageStatus> = Vec::new();

    // Insertion-ordered group accumulation: HashMap for O(1) lookup,
    // Vec for stable output order (matches order VMs appear in the API response).
    let mut group_order: Vec<String> = Vec::new();
    let mut group_map: HashMap<String, Accum> = HashMap::new();
    let mut meta_map: HashMap<String, VmMeta> = HashMap::new();

    for res in resources {
        match res.kind.as_str() {
            "node" => {
                // Node resources report cpu as a fraction [0,1] of total cores.
                node_status.push(NodeStatus {
                    node: res.node.clone(),
                    cpu_pct: res.cpu * 100.0,
                    mem_used: res.mem,
                    mem_total: if res.maxmem > 0 { res.maxmem } else { res.mem.max(1) },
                });
                let _ = res.maxcpu; // silence unused-field warning
            }
            // Only include storage entries with a known capacity.
            "storage" if res.maxdisk > 0 => {
                storage_status.push(StorageStatus {
                    storage: res.storage.clone().unwrap_or_default(),
                    node: res.node.clone(),
                    used: res.disk,
                    total: res.maxdisk,
                });
            }
            "qemu" | "lxc" => {
                if res.status != "running" {
                    continue; // skip stopped / paused VMs
                }
                let vmid = res.vmid.unwrap_or(0);
                let vm_label = res.name.clone()
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| format!("vm{vmid}"));

                let (read_s, write_s) = read_rates.get(&vmid).copied().unwrap_or((0.0, 0.0));

                // Group key depends on the selected grouping strategy.
                let gkey: String = match group_by {
                    PveGroupBy::Flat => vm_label.clone(),
                    PveGroupBy::Pool => res.pool.clone()
                        .filter(|p| !p.is_empty())
                        .unwrap_or_else(|| "(no pool)".into()),
                    PveGroupBy::Tag => res.tags.as_deref()
                        .and_then(|t| t.split(';').next().map(|s| s.trim().to_string()))
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "(untagged)".into()),
                    PveGroupBy::Node => res.node.clone(),
                };

                // In Flat mode each VM gets its own VmMeta entry.
                if group_by == PveGroupBy::Flat {
                    meta_map.insert(
                        vm_label.clone(),
                        VmMeta { node: res.node.clone(), vmid, kind: res.kind.clone() },
                    );
                }

                let acc = group_map.entry(gkey.clone()).or_insert_with(|| {
                    group_order.push(gkey.clone());
                    Accum {
                        cpu_load: 0.0, cpus: 0, mem: 0, maxmem: 0,
                        read_s: 0.0, write_s: 0.0, count: 0,
                        cpu_vals: vec![], mem_vals: vec![],
                        read_vals: vec![], write_vals: vec![],
                        uptime_max: 0,
                        first_vmid: vmid,
                        first_node: res.node.clone(),
                        first_kind: res.kind.clone(),
                    }
                });

                // Accumulate weighted CPU load: cpu is fraction of total vCPU capacity.
                acc.cpu_load += res.cpu * res.cpus as f64;
                acc.cpus += res.cpus;
                acc.mem += res.mem;
                acc.maxmem += res.maxmem;
                acc.read_s += read_s;
                acc.write_s += write_s;
                acc.count += 1;
                acc.uptime_max = acc.uptime_max.max(res.uptime);

                // Per-VM values for the fair-share overlay (all four Proxmox metrics).
                acc.cpu_vals.push(res.cpu * 100.0); // % of that VM's own vCPU capacity
                acc.mem_vals.push(res.mem as f64);
                acc.read_vals.push(read_s);
                acc.write_vals.push(write_s);
            }
            _ => {} // "pool" resource type and future additions
        }
    }

    let mut entries: Vec<BarEntry> = Vec::with_capacity(group_order.len());
    let mut member_vals: HashMap<GroupLabel, HashMap<Metric, Vec<f64>>> =
        HashMap::with_capacity(group_order.len());

    for gkey in &group_order {
        let acc = &group_map[gkey];

        // Pool-weighted average CPU%: sum(cpu_frac × cpus) / sum(cpus) × 100.
        // Represents "what fraction of this group's combined vCPU capacity is used?"
        let cpu_pct = if acc.cpus > 0 {
            (acc.cpu_load / acc.cpus as f64 * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };
        let mem_pct = if acc.maxmem > 0 {
            acc.mem as f64 / acc.maxmem as f64 * 100.0
        } else {
            0.0
        };

        let mem_g = acc.mem as f64 / 1_073_741_824.0;
        let max_g = acc.maxmem as f64 / 1_073_741_824.0;

        let extra = if group_by == PveGroupBy::Flat {
            let badge = if acc.first_kind == "qemu" { "Q" } else { "C" };
            format!("[{badge}] {mem_g:.1}/{max_g:.1}G  up {}", fmt_uptime(acc.uptime_max))
        } else {
            let n = acc.count;
            format!("{n} VM{}  {mem_g:.1}/{max_g:.1}G total", if n == 1 { "" } else { "s" })
        };

        // In Flat mode count is None (no thread count concept for VMs).
        // In grouped mode we use count to surface the VM count for callers.
        let count = if group_by == PveGroupBy::Flat { None } else { Some(acc.count as usize) };

        entries.push(BarEntry {
            label: gkey.clone(),
            value: cpu_pct,
            count,
            extra,
            rss_bytes: acc.mem,
            mem_pct,
            disk_read_s: acc.read_s,
            disk_write_s: acc.write_s,
            ..Default::default()
        });

        // Store one VmMeta per group in non-Flat modes (representative = first VM).
        if group_by != PveGroupBy::Flat {
            meta_map.insert(
                gkey.clone(),
                VmMeta { node: acc.first_node.clone(), vmid: acc.first_vmid, kind: acc.first_kind.clone() },
            );
        }

        // Pre-compute member-value vectors for all four Proxmox metrics.
        let mut m: HashMap<Metric, Vec<f64>> = HashMap::new();
        m.insert(Metric::Cpu,       acc.cpu_vals.clone());
        m.insert(Metric::Memory,    acc.mem_vals.clone());
        m.insert(Metric::DiskRead,  acc.read_vals.clone());
        m.insert(Metric::DiskWrite, acc.write_vals.clone());
        member_vals.insert(gkey.clone(), m);
    }

    (entries, meta_map, member_vals, node_status, storage_status)
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
/// Holds a `reqwest` blocking client (synchronous, thread-safe) and a
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
    /// - `base_url`: Proxmox API base, e.g. "https://pve.lan:8006". Trailing
    ///   slashes are stripped so path construction is consistent.
    /// - `token`: API token in "USER@REALM!TOKENID=UUID" format.
    /// - `insecure`: disables TLS certificate verification (self-signed certs).
    ///
    /// The HTTP client has a 10-second timeout so a slow Proxmox server does
    /// not block the UI indefinitely.
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
    /// All PVE REST responses have the envelope `{"data": T}`. This method
    /// unwraps the envelope transparently so callers receive `T` directly.
    fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        #[derive(Deserialize)]
        struct Wrap<T> { data: T }
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

    /// Fetch all cluster resources in one call and build grouped `BarEntry` rows.
    ///
    /// Uses `GET /api2/json/cluster/resources` which returns every VM, container,
    /// node, and storage entry for the whole cluster in a single HTTP response.
    /// This is efficient and works on single-node PVE installs too.
    ///
    /// The `group_by` parameter controls how VMs/CTs are bucketed:
    /// - `Flat`: one row per VM (original behaviour).
    /// - `Pool`: one row per Proxmox pool; VMs without a pool → "(no pool)".
    /// - `Tag`: one row per first tag; untagged VMs → "(untagged)".
    /// - `Node`: one row per Proxmox node; gives a host-rollup view.
    ///
    /// Each group's `BarEntry` aggregates its members:
    /// - CPU%: pool-weighted average (sum of cpu×cpus / sum of cpus × 100).
    /// - Memory: summed bytes used / summed max bytes.
    /// - Disk I/O: summed byte rates.
    ///
    /// The returned `member_vals` map carries per-VM values for all four
    /// Proxmox metrics so the main thread can drive the fair-share overlay
    /// for whichever metric is currently active.
    pub fn sample(&mut self, group_by: PveGroupBy) -> Result<SampleResult> {
        let now = Instant::now();
        let resources: Vec<Res> = self.get("/api2/json/cluster/resources")?;

        // Compute per-VM disk rates from previous cumulative counters, then update.
        let mut read_rates: HashMap<u64, (f64, f64)> = HashMap::new();
        for res in &resources {
            if matches!(res.kind.as_str(), "qemu" | "lxc") {
                let vmid = res.vmid.unwrap_or(0);
                let (rs, ws) = if let Some(p) = self.prev.get(&vmid) {
                    let dt = now.duration_since(p.at).as_secs_f64().max(0.001);
                    (
                        res.diskread.saturating_sub(p.diskread) as f64 / dt,
                        res.diskwrite.saturating_sub(p.diskwrite) as f64 / dt,
                    )
                } else {
                    (0.0, 0.0) // first sample: no previous counters available
                };
                read_rates.insert(vmid, (rs, ws));
                self.prev.insert(vmid, PrevVals { diskread: res.diskread, diskwrite: res.diskwrite, at: now });
            }
        }

        let (entries, meta, member_vals, node_status, storage_status) =
            build_groups(&resources, &read_rates, group_by);

        Ok(SampleResult { entries, meta, member_vals, node_status, storage_status })
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
        struct AgentData { result: Vec<Iface> }
        #[derive(Deserialize)]
        struct Iface {
            #[serde(default)] name: String,
            #[serde(rename = "ip-addresses", default)] ips: Vec<IpEntry>,
        }
        #[derive(Deserialize)]
        struct IpEntry {
            #[serde(rename = "ip-address")] addr: String,
            #[serde(rename = "ip-address-type")] kind: String,
        }

        let path = format!(
            "/api2/json/nodes/{node}/qemu/{vmid}/agent/network-get-interfaces"
        );
        let data: AgentData = self.get(&path)?;

        let mut ipv4 = Vec::new();
        let mut ipv6 = Vec::new();
        for iface in &data.result {
            if iface.name == "lo" { continue; }
            for ip in &iface.ips {
                let a = ip.addr.as_str();
                // Skip loopback and link-local addresses which are not routable.
                if a.starts_with("127.") || a.starts_with("169.254.") || a == "::1" {
                    continue;
                }
                if ip.kind == "ipv4" { ipv4.push(ip.addr.clone()); }
                else { ipv6.push(ip.addr.clone()); }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_vm(
        vmid: u64, name: &str, pool: Option<&str>, tags: Option<&str>,
        cpu: f64, cpus: u32, mem: u64, maxmem: u64,
        node: &str, status: &str, kind: &str,
    ) -> Res {
        Res {
            kind: kind.to_string(),
            node: node.to_string(),
            vmid: Some(vmid),
            name: Some(name.to_string()),
            pool: pool.map(str::to_string),
            tags: tags.map(str::to_string),
            cpu,
            cpus,
            mem,
            maxmem,
            diskread: 0,
            diskwrite: 0,
            uptime: 100,
            status: status.to_string(),
            maxcpu: 0,
            storage: None,
            disk: 0,
            maxdisk: 0,
        }
    }

    #[test]
    fn pool_grouping_basic() {
        let resources = vec![
            make_vm(101, "vm-a", Some("production"), None, 0.5, 4, 1u64<<30, 4u64<<30, "pve1", "running", "qemu"),
            make_vm(102, "vm-b", Some("production"), None, 0.25, 4, 2u64<<30, 4u64<<30, "pve1", "running", "qemu"),
            make_vm(103, "vm-c", None,               None, 0.1,  2, 512<<20,  2u64<<30, "pve1", "running", "lxc"),
        ];
        let rates = HashMap::new();
        let (entries, _, _, _, _) = build_groups(&resources, &rates, PveGroupBy::Pool);

        assert_eq!(entries.len(), 2);
        let prod = entries.iter().find(|e| e.label == "production").expect("production group missing");
        assert_eq!(prod.count, Some(2));
        let no_pool = entries.iter().find(|e| e.label == "(no pool)").expect("(no pool) group missing");
        assert_eq!(no_pool.count, Some(1));
    }

    #[test]
    fn pool_grouping_empty_pool_string_falls_back() {
        // An explicit pool="" should still fall through to "(no pool)".
        let resources = vec![
            make_vm(101, "vm-x", Some(""), None, 0.1, 2, 0, 1u64<<30, "pve1", "running", "qemu"),
        ];
        let rates = HashMap::new();
        let (entries, _, _, _, _) = build_groups(&resources, &rates, PveGroupBy::Pool);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, "(no pool)");
    }

    #[test]
    fn tag_grouping_first_tag_wins() {
        let resources = vec![
            make_vm(101, "vm-a", None, Some("web;db;cache"), 0.5, 4, 1u64<<30, 4u64<<30, "pve1", "running", "qemu"),
            make_vm(102, "vm-b", None, Some("web"),          0.25, 4, 1u64<<30, 4u64<<30, "pve1", "running", "qemu"),
            make_vm(103, "vm-c", None, Some("db"),           0.1,  2, 512<<20,  2u64<<30, "pve1", "running", "lxc"),
            make_vm(104, "vm-d", None, None,                 0.1,  2, 512<<20,  2u64<<30, "pve1", "running", "lxc"),
        ];
        let rates = HashMap::new();
        let (entries, _, _, _, _) = build_groups(&resources, &rates, PveGroupBy::Tag);

        assert_eq!(entries.len(), 3, "expected web / db / (untagged)");
        let web = entries.iter().find(|e| e.label == "web").expect("web group missing");
        assert_eq!(web.count, Some(2), "vm-a and vm-b both have 'web' as first tag");
        assert!(entries.iter().any(|e| e.label == "db"));
        assert!(entries.iter().any(|e| e.label == "(untagged)"));
    }

    #[test]
    fn node_grouping() {
        let resources = vec![
            make_vm(101, "vm-a", None, None, 0.5,  4, 1u64<<30, 4u64<<30, "pve1", "running", "qemu"),
            make_vm(102, "vm-b", None, None, 0.25, 4, 1u64<<30, 4u64<<30, "pve2", "running", "qemu"),
            make_vm(103, "vm-c", None, None, 0.1,  2, 512<<20,  2u64<<30, "pve1", "running", "lxc"),
        ];
        let rates = HashMap::new();
        let (entries, _, _, _, _) = build_groups(&resources, &rates, PveGroupBy::Node);

        assert_eq!(entries.len(), 2);
        let pve1 = entries.iter().find(|e| e.label == "pve1").expect("pve1 group missing");
        assert_eq!(pve1.count, Some(2));
        let pve2 = entries.iter().find(|e| e.label == "pve2").expect("pve2 group missing");
        assert_eq!(pve2.count, Some(1));
    }

    #[test]
    fn vcpu_weighted_cpu_average() {
        // VM1: cpu=0.50, cpus=4 → load contribution = 2.0
        // VM2: cpu=0.25, cpus=8 → load contribution = 2.0
        // Combined: cpu_pct = (2.0 + 2.0) / (4 + 8) * 100 = 33.333…
        let resources = vec![
            make_vm(101, "vm-1", Some("grp"), None, 0.50, 4, 0, 1u64<<30, "pve1", "running", "qemu"),
            make_vm(102, "vm-2", Some("grp"), None, 0.25, 8, 0, 1u64<<30, "pve1", "running", "qemu"),
        ];
        let rates = HashMap::new();
        let (entries, _, _, _, _) = build_groups(&resources, &rates, PveGroupBy::Pool);

        assert_eq!(entries.len(), 1);
        let expected = (0.5 * 4.0 + 0.25 * 8.0) / (4.0 + 8.0) * 100.0;
        assert!(
            (entries[0].value - expected).abs() < 0.001,
            "cpu_pct {} != expected {expected:.3}", entries[0].value,
        );
    }

    #[test]
    fn non_running_vms_excluded() {
        let resources = vec![
            make_vm(101, "vm-running", None, None, 0.5, 4, 1u64<<30, 4u64<<30, "pve1", "running", "qemu"),
            make_vm(102, "vm-stopped", None, None, 0.0, 4, 0,        4u64<<30, "pve1", "stopped", "qemu"),
            make_vm(103, "vm-paused",  None, None, 0.0, 4, 1u64<<30, 4u64<<30, "pve1", "paused",  "qemu"),
        ];
        let rates = HashMap::new();
        let (entries, _, _, _, _) = build_groups(&resources, &rates, PveGroupBy::Flat);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, "vm-running");
    }

    #[test]
    fn empty_input_no_panic() {
        let resources: Vec<Res> = vec![];
        let rates = HashMap::new();
        let (entries, meta, member_vals, node_status, storage_status) =
            build_groups(&resources, &rates, PveGroupBy::Pool);

        assert!(entries.is_empty());
        assert!(meta.is_empty());
        assert!(member_vals.is_empty());
        assert!(node_status.is_empty());
        assert!(storage_status.is_empty());
    }
}
