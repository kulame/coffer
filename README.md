# Coffer

> **High-density MicroVM runtime for AI Agents**  
> Warm acquire <50ms · Cold start <150ms · Memory overhead <50MB/instance · Density 500+/node

[![Rust](https://img.shields.io/badge/rust-1.78%2B-orange.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

[中文](./README_CN.md) · [日本語](./README_JP.md)

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

### 1. Install Build Dependencies

```bash
make install-deps   # Auto-detects your distro and installs required packages
```

Supports: Debian/Ubuntu, Fedora/RHEL, Arch, Alpine, openSUSE.

### 2. Download Firecracker

```bash
make firecracker
# → ~/.coffer/kernel/firecracker
# → ~/.coffer/kernel/jailer
```

### 3. Build Kernel

```bash
make kernel
# → ~/.coffer/kernel/vmlinux
```

This compiles a minimal Linux kernel with required features:
- `VIRTIO_VSOCK` — host-guest communication
- `EROFS_FS` — compressed read-only rootfs
- `OVERLAY_FS` — writable overlay on top of EROFS

If you prefer not to compile the kernel yourself, you can use any Firecracker-compatible `vmlinux` and place it at `~/.coffer/kernel/vmlinux`.

### 4. Build Rootfs

```bash
make rootfs
# → ~/.coffer/templates/alpine/rootfs.erofs
```

This creates a minimal rootfs with `coffer-init` (overlayfs + pivot_root) embedded.

### 4. Run Tests

```bash
# Unit tests
cargo test --workspace

# Integration tests (requires kernel + rootfs + firecracker)
make test-integration
```

### 5. Use as a Library

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

**Coffer** is part of the [AgentLink](https://github.com/agentlink-im/agentlink) ecosystem.
