# **NULNET** — **N**ode **U**tility & **L**ifecycle **N**etwork **E**nvironment **T**ool

[![CI](https://img.shields.io/github/actions/workflow/status/nulnet/nulnet/ci.yml?branch=main&label=CI)](https://github.com/nulnet/nulnet/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/actions/workflow/status/nulnet/nulnet/release-nulnet.yml?label=Release)](https://github.com/nulnet/nulnet/actions/workflows/release-nulnet.yml)
[![Latest Release](https://img.shields.io/github/v/release/nulnet/nulnet)](https://github.com/nulnet/nulnet/releases/latest)
[![License](https://img.shields.io/github/license/nulnet/nulnet)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.98+-orange.svg?logo=rust)](https://www.rust-lang.org)

Lightweight Linux host agent. Collects periodic telemetry, exposes an authenticated JSON-RPC API over a Unix socket, and self-updates from GitHub Releases or a custom CDN. No web UI, no database.

Built with Rust + Tokio.

> [!NOTE]  
> This repository ships the **agent** only. Clients connect over the Unix socket — use the companion CLI [nulctl](https://github.com/nulnet/nulctl) to authenticate and query telemetry.

---

## Install

> [!TIP]
> On **Debian / Ubuntu with systemd**, install from the latest release in one command:

```bash
curl -fsSL https://github.com/nulnet/nulnet/releases/latest/download/install.sh | bash
```

The script downloads and verifies the release binary, creates the `nulnet` system user, installs to `/opt/nulnet`, writes config if missing, optionally installs a sudoers snippet for post-update restarts, and enables the service.

Non-interactive: append `-y` or set `NULNET_INSTALL_ASSUME_YES=1`.

Custom CDN mirror at install time:

```bash
NULNET_CDN_BASE='https://cdn.example.com/nulnet' \
  curl -fsSL https://github.com/nulnet/nulnet/releases/latest/download/install.sh | bash
```

> [!IMPORTANT]
> **Requires:** `curl`, `sha256sum`, `useradd` or `adduser`, `install`, `systemctl`.
> Release binaries are statically linked (musl) and run on common glibc-based Linux distros (Debian 11+, Ubuntu 20.04+, etc.) without a minimum glibc version.

---

## Build from source

Requires Rust 1.80+ (edition 2024).

```bash
git clone https://github.com/nulnet/nulnet.git
cd nulnet
cargo build --release
# binary: target/release/nulnet
```

> [!NOTE]
> Debug builds load `./config.development.toml`; release builds expect `/opt/nulnet/config.toml`.

---

## Configuration

Config path: `/opt/nulnet/config.toml` (release) or `./config.development.toml` (debug). See [config.example.toml](config.example.toml).

```toml
[agent]
data_dir       = "/opt/nulnet/data"
retention_days = 5
socket_path    = "/opt/nulnet/nulnet.sock"
allowed_keys   = ["<hex Ed25519 public key>"]

[telemetry]
interval_seconds = 30

# Optional — omit to use GitHub Releases.
# [update]
# cdn = "https://cdn.example.com/nulnet"
```

> [!WARNING]
> Add client public keys to `allowed_keys` (hex-encoded, 32 bytes). Without at least one trusted key, no client can authenticate. Restart after changes: `sudo systemctl restart nulnet`.

---

## Options

### Config


| Key                          | Default                   | Description                                   |
| ---------------------------- | ------------------------- | --------------------------------------------- |
| `agent.data_dir`             | `/opt/nulnet/data`        | Snapshot storage directory                    |
| `agent.retention_days`       | `5`                       | Snapshots older than this are pruned hourly   |
| `agent.socket_path`          | `/opt/nulnet/nulnet.sock` | Unix socket for the API                       |
| `agent.allowed_keys`         | `[]`                      | Ed25519 public keys allowed to authenticate   |
| `telemetry.interval_seconds` | `30`                      | Collection interval                           |
| `update.cdn`                 | *(unset)*                 | Custom CDN base URL; omit for GitHub Releases |


### Install script


| Flag / env                                    | Description                                           |
| --------------------------------------------- | ----------------------------------------------------- |
| `-y`, `--yes` / `NULNET_INSTALL_ASSUME_YES=1` | Skip confirmation prompts                             |
| `-n`, `--dry-run`                             | Download and verify; skip system writes               |
| `NULNET_CDN_BASE=<url>`                       | Install from a custom CDN instead of GitHub           |
| `NULNET_INSTALL_SUDOERS=1|0`                  | Force install or skip the self-update sudoers snippet |


---

## Telemetry

Every `interval_seconds`, the agent writes a JSON snapshot to `data_dir` as `<unix_timestamp>.json`. Old files are removed on an hourly schedule per `retention_days`.

Collected fields: OS name, CPU usage and topology, GPU usage and VRAM (NVIDIA via `nvidia-smi`, AMD via `rocm-smi`), memory, disks, load average, uptime, running Docker containers (when `docker` is available), and on-disk cache size.

Example snapshot:

```json
{
  "timestamp": 1747561234,
  "os": "Debian GNU/Linux 13 (trixie)",
  "cpu": {
    "usage": 14.23,
    "info": {
      "model": "Intel Xeon E5-2680 v4",
      "cores": 14,
      "threads": 28,
      "frequency_mhz": 2397,
      "display": "Intel Xeon E5-2680 v4 @ 14x 2.397GHz"
    }
  },
  "gpu": [
    {
      "name": "Tesla V100-SXM2-16GB",
      "usage": 45.0,
      "vram_used": "4.73 GB",
      "vram_total": "16.00 GB",
      "vram_usage": 29.28
    }
  ],
  "memory": { "usage": 62.4, "used_gb": 9.98, "total_gb": 15.99 },
  "disk": [
    { "name": "/dev/sda1", "usage": 71.3, "used_size": "142.60 GB", "total_size": "200.00 GB" }
  ],
  "containers": [],
  "uptime_seconds": 864312,
  "load_average": { "one": 1.23, "five": 0.98, "fifteen": 0.74 },
  "cached_telemetry_size": "8.45 MB"
}
```

---

## API

One JSON object per line in, one per line out. Ed25519 challenge–response auth; all commands except `agent.auth.*` require an authenticated session.


| Command                | Auth | Description                                                    |
| ---------------------- | ---- | -------------------------------------------------------------- |
| `agent.auth.request`   | No   | Start auth; returns a nonce to sign                            |
| `agent.auth.verify`    | No   | Complete auth with signed nonce                                |
| `agent.version`        | Yes  | Running agent version                                          |
| `telemetry.get_latest` | Yes  | Most recent snapshot                                           |
| `telemetry.get_info`   | Yes  | Oldest/newest timestamps, count, retention, interval           |
| `telemetry.get_range`  | Yes  | Snapshots between `since` / `until` (Unix s); optional `limit` |
| `telemetry.get_bulk`   | Yes  | Snapshots for the last `hours` (default 1); optional `limit`   |
| `agent.update`         | Yes  | Streamed self-update from GitHub or CDN                        |


Request shape:

```json
{ "id": "1", "command": "telemetry.get_bulk", "params": { "hours": 24, "limit": 500 } }
```

Response shape:

```json
{ "id": "1", "status": "ok", "data": { ... } }
```

> [!NOTE]
> `agent.update` emits multiple `"status": "streaming"` lines before the final result.

---

## Custom CDN layout

Point `[update].cdn` or `NULNET_CDN_BASE` at a directory that serves these files over HTTPS:


| File             | Used by               | Purpose                        |
| ---------------- | --------------------- | ------------------------------ |
| `nulnet`         | install + self-update | Agent binary                   |
| `nulnet.sha256`  | install + self-update | SHA-256 hex digest of `nulnet` |
| `nulnet.service` | install only          | systemd unit                   |
| `version.txt`    | self-update only      | Latest semver (e.g. `1.3.2`)   |


Example base URL: `https://cdn.example.com/nulnet`

```
https://cdn.example.com/nulnet/nulnet
https://cdn.example.com/nulnet/nulnet.sha256
https://cdn.example.com/nulnet/nulnet.service
https://cdn.example.com/nulnet/version.txt
```

GitHub Releases ship the same artifacts (plus `install.sh`) under `releases/latest/download/`. The agent compares `version.txt` (CDN) or the GitHub Releases API (default) against the running version before downloading.

---

## Contributing

Issues and pull requests are welcome on [github.com/nulnet/nulnet](https://github.com/nulnet/nulnet).

1. Fork and clone the repo
2. Create a branch for your change
3. Run `cargo clippy` and `cargo test` before opening a PR
4. Tag releases follow semver (`v1.2.3`); CI builds and attaches artifacts automatically

Thanks to everyone who reports issues, sends patches, and runs nulnet in production.

---

## License

[BSD-3-Clause](LICENSE) © 2026 Da4ndo
