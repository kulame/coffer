# Coffer

> **面向 AI Agent 的高密度 MicroVM 运行时**  
> 热启动 <50ms · 冷启动 <150ms · 内存开销 <50MB/实例 · 单节点 500+ 密度

[![CI](https://github.com/kulame/coffer/actions/workflows/ci.yml/badge.svg)](https://github.com/kulame/coffer/actions)
[![Crates.io](https://img.shields.io/crates/v/coffer-core.svg)](https://crates.io/crates/coffer-core)
[![Rust](https://img.shields.io/badge/rust-1.78%2B-orange.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

[English](./README_EN.md) · [日本語](./README_JP.md)

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

### 1. 一行命令安装（推荐）

```bash
make install
```

这会完成以下步骤：
1. 安装系统依赖（自动检测发行版）
2. 以 Release 模式编译所有 Rust crate
3. 下载 Firecracker + Jailer（或使用本地预构建副本）
4. 构建客户机内核（或使用本地预构建副本）
5. 安装 `coffer-cli` 到 `/usr/local/bin`，运行时数据到 `~/.coffer`
6. 创建默认的 `alpine` 模板（需要 root 权限进行 KVM / 网络设置）

如果模板创建因权限失败，可单独用 root 补执行：
```bash
sudo coffer-cli template build --name alpine docker.io/library/alpine:latest
```

### 2. 手动分步安装

如果你希望对安装过程有更细粒度的控制：

```bash
make install-deps   # 安装系统依赖
make build          # 编译 Rust 工作空间
make firecracker    # 下载 Firecracker + Jailer → ~/.coffer/kernel
make kernel         # 构建客户机内核 → ~/.coffer/kernel/vmlinux
make rootfs         # 构建最小根文件系统 → ~/.coffer/templates/alpine
make template       # 创建热启动快照
```

### 3. 使用 CLI

Coffer 提供了命令行工具，方便快速进行沙箱测试：

```bash
# 检查系统就绪状态
coffer-cli check

# 列出可用模板
coffer-cli template list

# 快速运行一条命令（获取 → 执行 → 自动释放）
coffer-cli run --template alpine -- echo "hello from MicroVM"

# 带文件上传/下载和环境变量的运行
ccoffer-cli run --template alpine \
  --upload ./script.sh:/tmp/script.sh \
  --env FOO=bar \
  -- /bin/sh /tmp/script.sh

# 查看热池状态
coffer-cli pool-status
```

CLI 路径支持通过环境变量覆盖：
```bash
export COFFER_FIRECRACKER_PATH=~/.coffer/kernel/firecracker
export COFFER_KERNEL_PATH=~/.coffer/kernel/vmlinux
export COFFER_TEMPLATE_DIR=~/.coffer/templates
export COFFER_AGENT_BIN=~/.coffer/bin/coffer-agent
```

### 4. 作为库使用

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
| `coffer-cli` | 用于沙箱管理和测试的命令行界面 |

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
