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

`coffer-cli` 是 Coffer 的命令行入口，提供沙箱生命周期管理、模板操作和系统诊断等功能。

#### 命令总览

| 命令 | 说明 |
|------|------|
| `coffer-cli check` | 检查系统就绪状态 |
| `coffer-cli template <子命令>` | 管理 VM 模板（构建、列出、查看、删除） |
| `coffer-cli run [选项] -- <命令>` | 在沙箱中运行命令（自动获取 → 执行 → 释放） |
| `coffer-cli pool-status` | 查看热池状态 |
| `coffer-cli --version` | 显示版本信息 |

#### `check` — 系统就绪检查

在安装完成后或遇到异常时运行，验证所有依赖是否就绪：

```bash
coffer-cli check
```

检查项包括：
- Firecracker / Jailer / 内核 / Agent 二进制文件是否存在且可执行
- `/dev/kvm` 是否可读写（会提示如何修复权限）
- 模板目录是否存在
- 外部工具链：`mkfs.erofs`、`skopeo`、`umoci`、`ip`、`iptables`
- 网络权限（`CAP_NET_ADMIN`，缺失时提示 `setcap` 或预创建网桥）

#### `template` — 模板管理

模板是沙箱运行的基础，包含根文件系统、内核和内存快照。

**构建模板**（从 OCI 镜像）：
```bash
# 基础用法
coffer-cli template build --name alpine docker.io/library/alpine:latest

# 自定义内核启动参数
coffer-cli template build --name alpine \
  --kernel-args "console=ttyS0 reboot=k panic=1" \
  docker.io/library/alpine:latest
```

构建流程：`skopeo` 拉取 → `umoci unpack` → `mkfs.erofs` 打包 → 启动临时 VM → 生成 Firecracker 快照。

**列出模板**：
```bash
coffer-cli template list
```

输出示例：
```
ID                           NAME             VCPUS   MEM(MiB)
----------------------------------------------------------------
0192a1f3...e4b2             alpine               1        256
```

**查看模板详情**：
```bash
coffer-cli template info <ID>
```

**删除模板**：
```bash
# 交互式确认
coffer-cli template rm <ID>

# 跳过确认
coffer-cli template rm <ID> --yes
```

#### `run` — 在沙箱中运行命令

`run` 是 CLI 的核心命令，自动完成「获取沙箱 → 上传文件 → 执行命令 → 下载文件 → 归还热池」的完整生命周期。

**语法**：
```bash
coffer-cli run --template <ID> [选项] -- <命令> [参数...]
```

**选项**：

| 选项 | 简写 | 默认值 | 说明 |
|------|------|--------|------|
| `--template <ID>` | `-t` | 必填 | 使用的模板 ID |
| `--env KEY=VALUE` | `-e` | 无 | 设置环境变量（可多次指定） |
| `--timeout <毫秒>` | | `30000` | 命令执行超时 |
| `--upload LOCAL:REMOTE` | | 无 | 上传文件到沙箱（可多次指定） |
| `--download REMOTE:LOCAL` | | 无 | 从沙箱下载文件（可多次指定） |
| `--json` | | `false` | 以 JSON 格式输出结果 |
| `--` | | | 分隔 CLI 选项与待执行的命令 |

**基础示例**：
```bash
# 执行单条命令
coffer-cli run --template alpine -- echo "hello from MicroVM"

# 执行带参数的 shell 命令
coffer-cli run --template alpine -- /bin/sh -c "uname -a && cat /etc/os-release"

# 默认进入 /bin/sh（未提供命令时）
coffer-cli run --template alpine
```

**带环境变量**：
```bash
coffer-cli run --template alpine \
  -e FOO=bar \
  -e API_KEY=sk-xxxx \
  -- /bin/sh -c 'echo $FOO $API_KEY'
```

**文件上传与下载**：
```bash
# 上传本地脚本并在沙箱中执行
coffer-cli run --template alpine \
  --upload ./script.sh:/tmp/script.sh \
  -- /bin/sh /tmp/script.sh

# 上传输入文件，执行处理，下载结果
coffer-cli run --template alpine \
  --upload ./data.json:/tmp/data.json \
  --download /tmp/result.json:./result.json \
  -- python3 /tmp/process.py
```

**JSON 输出**（适合脚本集成）：
```bash
coffer-cli run --template alpine --json -- echo hello
```

输出示例：
```json
{
  "vm_id": "0192a1f3-...",
  "template_id": "alpine",
  "command": ["echo", "hello"],
  "exit_code": 0,
  "stdout": "hello\n",
  "stderr": "",
  "duration_ms": 2,
  "acquire_ms": 28,
  "exec_ms": 3
}
```

字段说明：
- `vm_id`：本次分配的虚拟机 ID
- `exit_code`：沙箱内命令的退出码
- `stdout` / `stderr`：标准输出和标准错误
- `duration_ms`：命令在客户机内的实际执行耗时
- `acquire_ms`：从热池获取沙箱的耗时（热启动通常 <50ms）
- `exec_ms`：宿主机侧命令执行总耗时

**退出码**：
- `run` 命令成功时，CLI 进程退出码与沙箱内命令的退出码一致，便于 Shell 脚本直接判断结果。
- 发生系统错误（如模板不存在、KVM 不可用、网络故障）时，退出码为 `1`。

#### `pool-status` — 查看热池状态

```bash
coffer-cli pool-status
```

输出示例：
```
Warm Pool Status
----------------
In-use sandboxes: 3
Available sandboxes by template:
  TEMPLATE                       COUNT
  ----------------------------------------
  alpine                             4
  ubuntu                             2
```

#### 环境变量

可通过环境变量覆盖默认路径配置：

| 环境变量 | 说明 | 默认值 |
|----------|------|--------|
| `COFFER_FIRECRACKER_PATH` | Firecracker 可执行文件路径 | `~/.coffer/kernel/firecracker` |
| `COFFER_KERNEL_PATH` | 客户机内核镜像路径 | `~/.coffer/kernel/vmlinux` |
| `COFFER_TEMPLATE_DIR` | 模板存储目录 | `~/.coffer/templates` |
| `COFFER_SOCKET_DIR` | Unix Socket 运行时目录 | `~/.coffer/run` |
| `COFFER_AGENT_BIN` | Agent 二进制文件路径 | `~/.coffer/bin/coffer-agent` |

使用示例：
```bash
export COFFER_TEMPLATE_DIR=/data/coffer/templates
export COFFER_FIRECRACKER_PATH=/opt/firecracker/firecracker

coffer-cli template list
coffer-cli run --template alpine -- uname -a
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
