# apptop

Process-group performance monitor with Proxmox VM support and SSH drill-down.

`apptop` reads `/proc` and groups processes by name, cgroup, or executable,
displaying a dual-metric bar chart. Metrics include CPU%, memory (fraction of
physical RAM), disk I/O rates, page faults, context switches, open file
descriptors, swap usage, scheduler wait time, and estimated power (RAPL).

Optionally poll a Proxmox VE API to monitor VMs and containers. With
`--enable-remote`, press Enter on a VM row to SSH in and monitor it live.

Press `g` to cycle grouping modes, `m` for the built-in manual.

## Installation

### Pre-built packages (Linux x86-64)

Download from [GitHub Releases](https://github.com/perpetualbits/apptop/releases/latest):

| Format | Command |
|--------|---------|
| Binary | `wget …/apptop-vX.Y.Z-x86_64-linux && chmod +x apptop-vX.Y.Z-x86_64-linux` |
| Debian/Ubuntu `.deb` | `sudo dpkg -i apptop_X.Y.Z_amd64.deb` |
| Fedora/RHEL `.rpm` | `sudo rpm -i apptop-X.Y.Z-1.x86_64.rpm` |
| Snap | `sudo snap install apptop --classic` |
| Arch Linux | See `packaging/arch/PKGBUILD` (AUR submission planned) |

### Build from source

Requires Rust 1.70+:

```bash
git clone https://github.com/perpetualbits/apptop
cd apptop
cargo build --release
sudo cp target/release/apptop /usr/local/bin/
```

## License

GPL-3.0-or-later
