# Coffer

> **面向 AI Agent 的高密度 MicroVM 运行时**  
> 热启动 <50ms · 冷启动 <150ms · 内存开销 <50MB/实例 · 单节点 500+ 密度

[![CI](https://github.com/kulame/coffer/actions/workflows/ci.yml/badge.svg)](https://github.com/kulame/coffer/actions)
[![Crates.io](https://img.shields.io/crates/v/coffer-core.svg)](https://crates.io/crates/coffer-core)
[![Rust](https://img.shields.io/badge/rust-1.78%2B-orange.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

[English](./README.md) · [日本語](./README_JP.md)

---

## 什么是 Coffer？

**Coffer** 是一个基于 Rust 的 MicroVM 运行时，底层采用 [AWS Firecracker](https://github.com/firecracker-microvm/firecracker)。它为 AI Agent、Serverless 函数和边缘工作负载提供快速、隔离且资源高效的沙箱环境。

与传统容器相比，Coffer 使用硬件虚拟化的 MicroVM，具备以下特性：
- **真正的内核级隔离** — 每个工作负载在独立的 Linux 内核中运行
- **快照恢复** — 从内存快照恢复预启动的 VM，耗时仅数毫秒
- **热池（Warm Pool）** — 后台工作线程维持一组暂停状态的 VM，实现即时分配
- **EROFS + overlayfs 根文件系统** — 不可变的压缩只读根文件系统，配合可写的 tmpfs 覆盖层

## 架构

```
┌─────────────────────────────────────────────────────────────┐
│                        Host (Linux)                          │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐  │
│  │  热池        │  │   运行时     │  │   模板管理器         │  │
│  │ (暂停的 VM)  │  │ (获取/释放)  │  │ (OCI → EROFS →      │  │
│  │              │  │              │  │  快照)              │  │
│  └──────┬──────┘  └──────┬──────┘  └─────────────────────┘  │
│         │                │                                   │
│  ┌──────▼────────────────▼──────┐  ┌─────────────────────┐  │
│  │   Firecracker HTTP/1.1       │  │   网络管理器         │  │
│  │   over Unix Domain Socket    │  │  (TAP + 网桥 +      │  │
│  │                              │  │   iptables SNAT)    │  │
│  └──────────────┬───────────────┘  └─────────────────────┘  │
│                 │                                            │
│         ┌───────▼────────┐  ┌──────────────────────────┐    │
│         │  Jailer (可选) │  │  skopeo + umoci +        │    │
│         │  chroot/seccomp│  │  mkfs.erofs 流水线       │    │
│         └───────┬────────┘  └──────────────────────────┘    │
│                 │                                            │
└─────────────────┼────────────────────────────────────────────┘
                  │ vsock (端口 1024)
┌─────────────────▼────────────────────────────────────────────┐
│                    MicroVM Guest (Linux)                      │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐  │
│  │ coffer-init │  │ coffer-agent│  │  用户工作负载        │  │
│  │(overlayfs  │  │(vsock JSON  │  │  (agentlet, 脚本,   │  │
│  │ pivot_root)│  │  Lines RPC) │  │   serverless 函数)  │  │
│  └─────────────┘  └─────────────┘  └─────────────────────┘  │
│                                                               │
│  Rootfs: EROFS (只读) + tmpfs overlay (读写)                 │
└─────────────────────────────────────────────────────────────┘
```

## 性能指标

| 指标 | 目标 | 状态 |
|------|------|------|
| 热启动 | < 50 ms | ✅ ~30 ms（快照恢复） |
| 冷启动 | < 150 ms | ✅ ~120 ms（内核启动 + Agent 就绪） |
| 内存开销 | < 50 MB/实例 | ✅ ~35 MB（64 MiB 客户机 + VMM） |
| 部署密度 | 500+ / 节点 | ✅ 在 32 vCPU / 128 GiB 上可达 |

## 快速开始

### 前置条件

- 启用 KVM 的 Linux 主机（`/dev/kvm` 可访问）
- Rust 1.78+
- 内核编译工具：`gcc`、`make`、`bc`、`bison`、`flex`、`libssl-dev`、`libelf-dev`、`wget`
- 根文件系统工具：`erofs-utils`、`lz4`
- `skopeo`、`umoci`（可选，仅用于从 OCI 镜像构建模板）

### 1. 安装构建依赖

```bash
make install-deps   # 自动检测发行版并安装所需包
```

支持：Debian/Ubuntu、Fedora/RHEL、Arch、Alpine、openSUSE。

### 2. 下载 Firecracker

```bash
make firecracker
# → ~/.coffer/kernel/firecracker
# → ~/.coffer/kernel/jailer
```

### 3. 构建内核

```bash
make kernel
# → ~/.coffer/kernel/vmlinux
```

编译包含以下必需特性的最小化 Linux 内核：
- `VIRTIO_VSOCK` — 宿主机与客户机通信
- `EROFS_FS` — 压缩只读根文件系统
- `OVERLAY_FS` — 在 EROFS 之上提供可写覆盖层

如果你不想自己编译内核，可以使用任何兼容 Firecracker 的预编译 `vmlinux`，并将其放置到 `~/.coffer/kernel/vmlinux`。

### 4. 构建根文件系统

```bash
make rootfs
# → ~/.coffer/templates/alpine/rootfs.erofs
```

这会创建一个包含 `coffer-init`（overlayfs + pivot_root）的最小根文件系统。

### 4. 运行测试

```bash
# 单元测试
cargo test --workspace

# 集成测试（需要内核 + 根文件系统 + firecracker）
make test-integration
```

### 5. 作为库使用

```rust
use coffer_core::{Runtime, RuntimeConfig, SandboxHandle};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = RuntimeConfig::default();
    let runtime = Runtime::new(config).await?;

    // 热启动（<50ms）
    let sandbox: SandboxHandle = runtime.acquire("alpine").await?;

    // 通过 vsock 通信
    let vsock_path = sandbox.vsock_path();
    // ... 发送 AgentRequest，接收 AgentResponse

    drop(sandbox); // 将 VM 归还热池
    Ok(())
}
```

## 工作空间 Crate

| Crate | 描述 |
|-------|------|
| `coffer-protocol` | 基于 vsock 的宿主机-客户机 JSON Lines 协议 |
| `coffer-core` | Firecracker 客户端、运行时、热池、模板、网络 |
| `coffer-agent` | 客户机端 Agent 二进制（在 MicroVM 内部运行） |

## 协议

Coffer 使用简单的 JSON Lines 协议，通过 **vsock 端口 1024** 通信。

### 请求

```json
{"method":"exec","request_id":"r1","cmd":["echo","hello"],"env":{},"working_dir":null,"stdin":null,"timeout_ms":5000}
```

### 响应

```json
{"status":"ok","request_id":"r1","exit_code":0,"stdout":"aGVsbG8=","stderr":""}
```

支持的方法：`exec`、`upload`、`download`、`ping`

完整协议定义见 [`coffer-protocol/src/lib.rs`](crates/coffer-protocol/src/lib.rs)。

## 配置

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

## 安全模型

- **内核隔离** — 每个沙箱通过 KVM 运行独立的 Linux 内核
- **Jailer 支持** — 可选的 chroot + seccomp + 命名空间隔离，保护 Firecracker 进程本身
- **网络策略** — 通过 iptables 为每个沙箱配置出站允许/拒绝列表
- **EROFS 不可变性** — 根文件系统只读；所有写入操作转到 tmpfs 覆盖层
- **VMGenID** — Firecracker 在每次快照恢复时重新播种客户机熵源

## 许可证

MIT — 详见 [LICENSE](LICENSE)。

## 致谢

- [AWS Firecracker](https://github.com/firecracker-microvm/firecracker) — 底层 VMM
- [EROFS](https://erofs.docs.kernel.org/) — 增强型只读文件系统

---

> 基于 Rust 与 Firecracker 构建。无容器，无额外开销。
