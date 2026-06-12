# apptop

Real-time process-group performance monitor for Linux.

`apptop` reads `/proc` and groups processes by name, cgroup, or executable,
displaying a dual-metric bar chart updated every 2 seconds. It supports local
monitoring, SSH fleet fan-out, Proxmox VE API polling, and Kubernetes pod
drill-down.

Press `m` for the full built-in manual, or `apptop -m | less` from the shell.

## Features

- **Dual-metric bar chart** — two metrics side by side per group; cycle with `←`/`→`
- **17 metrics** — CPU%, memory, disk read/write, page faults, context switches,
  open FDs, swap, scheduler wait, estimated power (RAPL), CFS throttle, PSI
  pressure (cpu/mem/io), GPU%, VRAM
- **Three grouping modes** — by process name (`comm`), cgroup, or executable path; cycle with `g`
- **GPU support** (`--enable-gpu`) — per-process GPU engine % and VRAM via
  `/proc/PID/fdinfo` (Intel i915/xe, AMD amdgpu, kernel ≥ 5.14); NVIDIA via
  `nvidia-smi` pmon; multi-GPU with per-device selection using `[`/`]`
- **Fleet mode** (`--hosts`, `--enable-remote`) — monitor many SSH hosts in one view;
  press Enter to drill into any host
- **Thin probe** (`--thin`) — CPU% + memory without apptop installed on the remote;
  works over any SSH connection via a `/proc` shell one-liner
- **Proxmox VE** (`--proxmox`) — poll the PVE REST API; group VMs by pool, tag, or
  node; press Enter to SSH into any VM and monitor its processes live
- **Kubernetes** (`--kube`, experimental) — discover pods via `kubectl`, fan out
  metrics, drill into any pod with `kubectl exec`
- **Replay / scrub** — `p` to pause, `←`/`→` to scrub through buffered history
  (default 4 minutes at 2 s interval)
- **Anomaly detection** — load-concentration alerts with optional shell hook
  (`--alert-cmd`)
- **Built-in manual** — `m` in the TUI or `apptop -m` at the shell

## Installation

### Pre-built packages

Download from [GitHub Releases](https://github.com/perpetualbits/apptop/releases/latest):

| Architecture | Binary | Debian/Ubuntu | Fedora/RHEL | Snap |
|---|---|---|---|---|
| x86-64 | `apptop-vX.Y.Z-x86_64-linux` | `apptop_X.Y.Z_amd64.deb` | `apptop-X.Y.Z.x86_64.rpm` | `apptop_X.Y.Z_amd64.snap` |
| aarch64 | `apptop-vX.Y.Z-aarch64-linux` | `apptop_X.Y.Z_arm64.deb` | `apptop-X.Y.Z.aarch64.rpm` | — |
| riscv64 | `apptop-vX.Y.Z-riscv64-linux` | `apptop_X.Y.Z_riscv64.deb` | `apptop-X.Y.Z.riscv64gc.rpm` | — |

```bash
# Debian/Ubuntu
sudo dpkg -i apptop_*.deb

# Fedora/RHEL
sudo rpm -i apptop-*.rpm

# Raw binary
chmod +x apptop-*-linux && sudo mv apptop-*-linux /usr/local/bin/apptop
```

### Build from source

Requires Rust 1.70+:

```bash
git clone https://github.com/perpetualbits/apptop
cd apptop
cargo build --release
sudo cp target/release/apptop /usr/local/bin/
```

## Quick start

```bash
# Local process monitor
apptop

# Show only the 20 busiest groups, refresh every second
apptop -n 20 -i 1

# Enable GPU metrics (Intel/AMD via fdinfo, NVIDIA via nvidia-smi)
apptop --enable-gpu

# Monitor a Proxmox cluster
apptop --proxmox https://pve.lan:8006 --token user@pam!mytoken=SECRET

# Monitor a fleet of SSH hosts
apptop --enable-remote --hosts web1,web2,web3

# Monitor fleet from a file, use thin probe (no apptop needed on remotes)
apptop --enable-remote --hosts @/etc/apptop/hosts --thin

# Monitor Kubernetes pods in a namespace
apptop --kube monitoring

# Print the built-in manual
apptop -m | less
```

## Key bindings

| Key | Action |
|---|---|
| `←` / `→` | Cycle left / right metric |
| `Tab` | Switch active metric side |
| `↑` / `↓` or `j` / `k` | Move cursor |
| `g` | Cycle grouping: comm → cgroup → exe |
| `s` | Cycle sort order |
| `h` | Toggle log scale |
| `r` | Force immediate refresh |
| `p` | Pause / resume (frozen display) |
| `[` / `]` | Cycle GPU device (with `--enable-gpu`) |
| `Enter` | Drill into VM / host / pod |
| `Esc` | Return to group list |
| `m` | Toggle built-in manual |
| `q` | Quit |

## CLI reference

```
apptop [OPTIONS]

Options:
  -i, --interval <SECS>       Refresh interval (default: 2)
  -n, --top <N>               Show only top-N busiest groups (0 = all)
  -m, --manual                Print built-in manual and exit
  -V, --version               Print version

Proxmox:
      --proxmox <URL>         Proxmox API base URL (e.g. https://pve.lan:8006)
      --token <TOKEN>         API token (USER@REALM!TOKENID=SECRET) [$PROXMOX_TOKEN]
      --insecure              Accept self-signed TLS certificates

Remote / Fleet:
      --enable-remote         Enable SSH drill-down [$APPTOP_ENABLE_REMOTE]
      --hosts <LIST|@FILE>    Comma-separated hostnames or @/path/to/file
      --ssh-user <USER>       SSH username (default: current user)
      --ssh-accept-new        Accept unknown host keys on first use (TOFU)
      --thin                  Use shell /proc probe instead of apptop --daemon

Kubernetes (experimental):
      --kube <NS[/SELECTOR]>  Namespace or namespace/label-selector
      --kube-context <CTX>    kubeconfig context (default: current)
      --kube-thin             Use shell probe instead of apptop --daemon in pod

GPU:
      --enable-gpu            Enable GPU metrics (fdinfo + nvidia-smi pmon)

History / Alerts:
      --history-depth <N>     Ring-buffer depth in snapshots (default: 120)
      --alert-cmd <CMD>       Shell command fired on anomaly detection
```

## GPU support

Pass `--enable-gpu` to enable GPU metrics. Two backends are used automatically:

| Backend | Drivers | Metrics |
|---|---|---|
| `/proc/PID/fdinfo` | Intel i915/xe, AMD amdgpu, kernel ≥ 5.14 | GPU engine %, VRAM (bytes) |
| `nvidia-smi pmon` | NVIDIA proprietary (any version) | SM utilisation %, VRAM (MiB) |

On multi-GPU systems, `[`/`]` cycle through discovered devices. The footer shows
which device is selected. By default all devices are aggregated.

NVIDIA support requires `nvidia-smi` in `PATH`; missing or failing silently produces
zero values without an error.

## Proxmox mode

```bash
apptop --proxmox https://pve.lan:8006 --token user@pam!token=SECRET
```

Groups VMs/CTs by pool, tag, or node (press `g` to cycle). The fair-share
overlay shows how load is distributed within each group. Press Enter on any VM
row to SSH in and monitor its processes live (requires `--enable-remote`).

## Fleet mode

```bash
apptop --enable-remote --hosts web1,web2,db1
```

Each host appears as a row; metrics are the busiest process group on that host.
Press Enter to drill in. Use `--thin` for hosts without apptop installed.

## Anomaly alerts

```bash
apptop --alert-cmd /usr/local/bin/alert.sh
```

The hook is called as `CMD GROUP KIND BALANCE_FRACTION` (e.g.
`alert.sh nginx concentrated 0.12`) when a group's load distribution becomes
pathological. Rate-limited to once per 60 s per group.

## Requirements

- Linux kernel 4.15+ (5.14+ for GPU fdinfo metrics)
- No root required (some metrics show `?` without it)
- Fleet/remote mode: `ssh` in PATH, host keys in `known_hosts`
- Kubernetes mode: `kubectl` in PATH with exec RBAC
- GPU (NVIDIA): `nvidia-smi` in PATH

## License

GPL-3.0-or-later — Copyright (C) 2026 Epsilon Null Operation
