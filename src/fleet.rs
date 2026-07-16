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
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string());
    vec![Place { key: hostname.clone(), label: hostname, ancestor_last: Vec::new(), is_last: true, expanded: None }]
}

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
