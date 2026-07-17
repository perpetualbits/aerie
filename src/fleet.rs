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
    /// Not yet read: reserved for per-place `TileId`s once the spine grows
    /// beyond the single local host (SSH hosts, VMs, containers).
    #[allow(dead_code)]
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
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string());
    vec![Place { key: hostname.clone(), label: hostname, ancestor_last: Vec::new(), is_last: true, expanded: None }]
}

/// One spine place per fleet host (from `--hosts`). Flat siblings, no local
/// root for this slice — `local_places` remains the local-mode builder; this
/// is the fleet-mode builder.
pub fn fleet_places(hostnames: &[String]) -> Vec<Place> {
    let n = hostnames.len();
    hostnames.iter().enumerate().map(|(i, h)| Place {
        key: h.clone(), label: h.clone(), ancestor_last: Vec::new(),
        is_last: i + 1 == n, expanded: None,
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fleet_places_one_leaf_per_host() {
        let hosts = vec!["apollo".to_string(), "milkv".to_string(), "vega".to_string()];
        let places = fleet_places(&hosts);
        assert_eq!(places.len(), 3);
        for (i, p) in places.iter().enumerate() {
            assert_eq!(p.label, hosts[i]);
            assert_eq!(p.key, hosts[i]);
            assert!(p.ancestor_last.is_empty(), "flat siblings have no ancestors");
            assert_eq!(p.expanded, None, "a leaf host has no expander");
            assert_eq!(p.is_last, i + 1 == hosts.len());
        }
    }

    #[test]
    fn fleet_places_empty_for_no_hosts() {
        assert!(fleet_places(&[]).is_empty());
    }

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
