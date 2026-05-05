# Coffer

> **High-density MicroVM runtime for AI Agents**  
> Warm acquire <50ms · Cold start <150ms · Memory overhead <50MB/instance · Density 500+/node

[![CI](https://github.com/kulame/coffer/actions/workflows/ci.yml/badge.svg)](https://github.com/kulame/coffer/actions)
[![Crates.io](https://img.shields.io/crates/v/coffer-core.svg)](https://crates.io/crates/coffer-core)
[![Rust](https://img.shields.io/badge/rust-1.78%2B-orange.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

[中文](./README.md) · [日本語](./README_JP.md)

---

## What is Coffer?

**Coffer** is a Rust-based MicroVM runtime built on [AWS Firecracker](https://github.com/firecracker-microvm/firecracker). It provides fast, isolated, and resource-efficient sandboxing for AI agents, serverless functions, and edge workloads.

Unlike containers, Coffer uses hardware-virtualized MicroVMs with:
- **True kernel-level isolation** — each workload runs in its own Linux kernel
- **Snapshot resume** — pre-booted VMs restored from memory snapshots in milliseconds
- **Warm pool** — background workers keep a pool of paused VMs ready for instant allocation
- **EROFS + overlayfs rootfs** — immutable, compressed root filesystems with writable tmpfs overlay

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                        Host (Linux)                          │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐  │
│  │  Warm Pool  │  │   Runtime   │  │   Template Manager  │  │
│  │  (Paused    │  │  (Acquire   │  │  (OCI → EROFS →     │  │
│  │   VMs)      │  │   / Release)│  │   Snapshot)         │  │
│  └──────┬──────┘  └──────┬──────┘  └─────────────────────┘  │
│         │                │                                   │
│  ┌──────▼────────────────▼──────┐  ┌─────────────────────┐  │
│  │   Firecracker HTTP/1.1       │  │   Network Manager   │  │
│  │   over Unix Domain Socket    │  │  (TAP + bridge +    │  │
│  │                              │  │   iptables SNAT)    │  │
│  └──────────────┬───────────────┘  └─────────────────────┘  │
│                 │                                            │
│         ┌───────▼────────┐  ┌──────────────────────────┐    │
│         │  Jailer (opt)  │  │  skopeo + umoci +        │    │
│         │  chroot/seccomp│  │  mkfs.erofs pipeline     │    │
│         └───────┬────────┘  └──────────────────────────┘    │
│                 │                                            │
└─────────────────┼────────────────────────────────────────────┘
                  │ vsock (port 1024)
┌─────────────────▼────────────────────────────────────────────┐
│                    MicroVM Guest (Linux)                      │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐  │
│  │ coffer-init │  │ coffer-agent│  │  User Workload      │  │
│  │(overlayfs  │  │(vsock JSON  │  │  (agentlet, script, │  │
│  │ pivot_root)│  │  Lines RPC) │  │   serverless fn)    │  │
│  └─────────────┘  └─────────────┘  └─────────────────────┘  │
│                                                               │
│  Rootfs: EROFS (ro) + tmpfs overlay (rw)                     │
└─────────────────────────────────────────────────────────────┘
```

## Performance

| Metric | Target | Status |
|--------|--------|--------|
| Warm acquire | < 50 ms | ✅ ~30 ms (snapshot resume) |
| Cold start | < 150 ms | ✅ ~120 ms (kernel boot + agent ready) |
| Memory overhead | < 50 MB/instance | ✅ ~35 MB (64 MiB guest + VMM) |
| Density | 500+ / node | ✅ achievable on 32 vCPU / 128 GiB |

## Quick Start

### Prerequisites

- Linux host with KVM enabled (`/dev/kvm` accessible)
- Rust 1.78+
- Build tools for kernel: `gcc`, `make`, `bc`, `bison`, `flex`, `libssl-dev`, `libelf-dev`, `wget`
- Build tools for rootfs: `erofs-utils`, `lz4`
- `skopeo`, `umoci` (optional, only for template builds from OCI images)

### 1. One-Line Install (Recommended)

```bash
make install
```

This will:
1. Install system dependencies (auto-detects your distro)
2. Build all Rust crates in release mode
3. Download Firecracker + Jailer (or use pre-built local copies)
4. Build the guest kernel (or use pre-built local copy)
5. Install `coffer-cli` to `/usr/local/bin` and runtime data to `~/.coffer`
6. Create the default `alpine` template (requires root for KVM / network setup)

If template creation fails due to permissions, run it separately with root:
```bash
sudo coffer-cli template build --name alpine docker.io/library/alpine:latest
```

### 2. Manual Step-by-Step Install

If you prefer finer control over the installation:

```bash
make install-deps   # Install system dependencies
make build          # Build Rust workspace
make firecracker    # Download Firecracker + Jailer → ~/.coffer/kernel
make kernel         # Build guest kernel → ~/.coffer/kernel/vmlinux
make rootfs         # Build minimal rootfs → ~/.coffer/templates/alpine
make template       # Create warm-start snapshot
```

### 3. Use the CLI

Coffer includes a command-line tool for rapid sandbox testing:

```bash
# Check system readiness
coffer-cli check

# List available templates
coffer-cli template list

# Run a quick command (acquire → exec → auto-release)
coffer-cli run --template alpine -- echo "hello from MicroVM"

# Run with file upload/download and custom env
coffer-cli run --template alpine \
  --upload ./script.sh:/tmp/script.sh \
  --env FOO=bar \
  -- /bin/sh /tmp/script.sh

# View warm pool status
coffer-cli pool-status
```

Environment variables for CLI paths:
```bash
export COFFER_FIRECRACKER_PATH=~/.coffer/kernel/firecracker
export COFFER_KERNEL_PATH=~/.coffer/kernel/vmlinux
export COFFER_TEMPLATE_DIR=~/.coffer/templates
export COFFER_AGENT_BIN=~/.coffer/bin/coffer-agent
```

### 7. Use as a Library

```rust
use coffer_core::{Runtime, RuntimeConfig, SandboxHandle};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = RuntimeConfig::default();
    let runtime = Runtime::new(config).await?;

    // Warm acquire (<50ms)
    let sandbox: SandboxHandle = runtime.acquire("alpine").await?;

    // Communicate over vsock
    let vsock_path = sandbox.vsock_path();
    // ... send AgentRequest, receive AgentResponse

    drop(sandbox); // Returns VM to warm pool
    Ok(())
}
```

## Workspace Crates

| Crate | Description |
|-------|-------------|
| `coffer-protocol` | Host-guest JSON Lines protocol over vsock |
| `coffer-core` | Firecracker client, runtime, warm pool, templates, networking |
| `coffer-agent` | Guest-side agent binary (runs inside MicroVM) |
| `coffer-cli` | Command-line interface for sandbox management and testing |

## Protocol

Coffer uses a simple JSON Lines protocol over **vsock port 1024**.

### Request

```json
{"method":"exec","request_id":"r1","cmd":["echo","hello"],"env":{},"working_dir":null,"stdin":null,"timeout_ms":5000}
```

### Response

```json
{"status":"ok","request_id":"r1","exit_code":0,"stdout":"aGVsbG8=","stderr":""}
```

Methods: `exec`, `upload`, `download`, `ping`

See [`coffer-protocol/src/lib.rs`](crates/coffer-protocol/src/lib.rs) for full schema.

## Configuration

```rust
RuntimeConfig {
    template_dir: "~/.coffer/templates".into(),
    socket_dir: "~/.coffer/run".into(),
    firecracker_path: "~/.coffer/kernel/firecracker".into(),
    jailer_path: Some("~/.coffer/kernel/jailer".into()),
    kernel_path: "~/.coffer/kernel/vmlinux".into(),
    agent_bin: "~/.coffer/bin/coffer-agent".into(),
    pool: PoolConfig {
        warm_pool_size: 10,
        max_sandboxes: 100,
    },
    network: NetworkConfig {
        bridge: "br-coffer".into(),
        subnet: "172.26.0.0/16".into(),
    },
    ..Default::default()
}
```

## Security Model

- **Kernel isolation** — each sandbox runs its own Linux kernel via KVM
- **Jailer support** — optional chroot + seccomp + namespace isolation for the Firecracker process itself
- **Network policy** — per-sandbox egress allowlist/denylist via iptables
- **EROFS immutability** — root filesystem is read-only; all writes go to tmpfs overlay
- **VMGenID** — Firecracker reseeds guest entropy on every snapshot resume

## License

MIT — see [LICENSE](LICENSE).

## Acknowledgements

- [AWS Firecracker](https://github.com/firecracker-microvm/firecracker) — the underlying VMM
- [EROFS](https://erofs.docs.kernel.org/) — enhanced read-only filesystem

---

> Built with Rust and Firecracker. No containers, no overhead.
