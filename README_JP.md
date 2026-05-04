# Coffer

> **AI Agent 向け高密度 MicroVM ランタイム**  
> ウォーム起動 <50ms · コールドスタート <150ms · メモリオーバーヘッド <50MB/インスタンス · 密度 500+/ノード

[![Rust](https://img.shields.io/badge/rust-1.78%2B-orange.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

[English](./README.md) · [中文](./README_CN.md)

---

## Coffer とは？

**Coffer** は Rust 製の MicroVM ランタイムで、[AWS Firecracker](https://github.com/firecracker-microvm/firecracker) を基盤としています。AI Agent、Serverless 関数、エッジワークロード向けに高速で隔離され、リソース効率の高いサンドボックス環境を提供します。

従来のコンテナと異なり、Coffer はハードウェア仮想化による MicroVM を使用し、以下の特性を備えています：
- **真のカーネルレベル隔離** — 各ワークロードが独立した Linux カーネル上で実行
- **スナップショット復元** — メモリスナップショットからプリブート済み VM をミリ秒単位で復元
- **ウォームプール** — バックグラウンドワーカーが一時停止状態の VM を保持し、即座に割り当て可能
- **EROFS + overlayfs ルートファイルシステム** — 不変の圧縮読み取り専用ルートFSに、書き込み可能な tmpfs オーバーレイを提供

## アーキテクチャ

```
┌─────────────────────────────────────────────────────────────┐
│                        Host (Linux)                          │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐  │
│  │  ウォーム   │  │   ランタイム │  │   テンプレート      │  │
│  │  プール     │  │  (取得/解放) │  │   マネージャ        │  │
│  │ (一時停止   │  │              │  │  (OCI → EROFS →    │  │
│  │  VM)        │  │              │  │   スナップショット) │  │
│  └──────┬──────┘  └──────┬──────┘  └─────────────────────┘  │
│         │                │                                   │
│  ┌──────▼────────────────▼──────┐  ┌─────────────────────┐  │
│  │   Firecracker HTTP/1.1       │  │   ネットワーク      │  │
│  │   over Unix Domain Socket    │  │   マネージャ        │  │
│  │                              │  │  (TAP + ブリッジ +  │  │
│  │                              │  │   iptables SNAT)    │  │
│  └──────────────┬───────────────┘  └─────────────────────┘  │
│                 │                                            │
│         ┌───────▼────────┐  ┌──────────────────────────┐    │
│         │  Jailer (任意) │  │  skopeo + umoci +        │    │
│         │  chroot/seccomp│  │  mkfs.erofs パイプライン │    │
│         └───────┬────────┘  └──────────────────────────┘    │
│                 │                                            │
└─────────────────┼────────────────────────────────────────────┘
                  │ vsock (ポート 1024)
┌─────────────────▼────────────────────────────────────────────┐
│                    MicroVM Guest (Linux)                      │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐  │
│  │ coffer-init │  │ coffer-agent│  │  ユーザーワークロード│  │
│  │(overlayfs  │  │(vsock JSON  │  │  (agentlet, スクリプト│  │
│  │ pivot_root)│  │  Lines RPC) │  │   serverless 関数)  │  │
│  └─────────────┘  └─────────────┘  └─────────────────────┘  │
│                                                               │
│  Rootfs: EROFS (読み取り専用) + tmpfs overlay (読み書き可能) │
└─────────────────────────────────────────────────────────────┘
```

## パフォーマンス

| メトリクス | 目標 | 状態 |
|-----------|------|------|
| ウォーム取得 | < 50 ms | ✅ ~30 ms（スナップショット復元） |
| コールドスタート | < 150 ms | ✅ ~120 ms（カーネルブート + Agent 起動） |
| メモリオーバーヘッド | < 50 MB/インスタンス | ✅ ~35 MB（64 MiB ゲスト + VMM） |
| 密度 | 500+ / ノード | ✅ 32 vCPU / 128 GiB で達成可能 |

## クイックスタート

### 前提条件

- KVM が有効な Linux ホスト（`/dev/kvm` にアクセス可能）
- Rust 1.78+
- カーネル構築ツール：`gcc`、`make`、`bc`、`bison`、`flex`、`libssl-dev`、`libelf-dev`、`wget`
- ルートFS構築ツール：`erofs-utils`、`lz4`
- `skopeo`、`umoci`（任意、OCI イメージからのテンプレートビルド用のみ必要）

### 1. ビルド依存関係のインストール

```bash
make install-deps   # ディストリビューションを自動検出し、必要なパッケージをインストール
```

対応：Debian/Ubuntu、Fedora/RHEL、Arch、Alpine、openSUSE。

### 2. Firecracker のダウンロード

```bash
make firecracker
# → ~/.coffer/kernel/firecracker
# → ~/.coffer/kernel/jailer
```

### 3. カーネルのビルド

```bash
make kernel
# → ~/.coffer/kernel/vmlinux
```

必要な機能を含む最小限の Linux カーネルをコンパイルします：
- `VIRTIO_VSOCK` — ホスト・ゲスト間通信
- `EROFS_FS` — 圧縮読み取り専用ルートFS
- `OVERLAY_FS` — EROFS 上の書き込み可能オーバーレイ

自分でカーネルをコンパイルしたくない場合は、Firecracker 互換の既製 `vmlinux` を使用し、`~/.coffer/kernel/vmlinux` に配置できます。

### 4. ルートFS のビルド

```bash
make rootfs
# → ~/.coffer/templates/alpine/rootfs.erofs
```

これにより、`coffer-init`（overlayfs + pivot_root）を含む最小限のルートFSが作成されます。

### 4. テストの実行

```bash
# ユニットテスト
cargo test --workspace

# 統合テスト（カーネル + ルートFS + firecracker が必要）
make test-integration
```

### 5. ライブラリとして使用

```rust
use coffer_core::{Runtime, RuntimeConfig, SandboxHandle};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = RuntimeConfig::default();
    let runtime = Runtime::new(config).await?;

    // ウォーム取得（<50ms）
    let sandbox: SandboxHandle = runtime.acquire("alpine").await?;

    // vsock 経由で通信
    let vsock_path = sandbox.vsock_path();
    // ... AgentRequest の送信、AgentResponse の受信

    drop(sandbox); // VM をウォームプールに返却
    Ok(())
}
```

## ワークスペース Crate

| Crate | 説明 |
|-------|------|
| `coffer-protocol` | vsock 上のホスト・ゲスト JSON Lines プロトコル |
| `coffer-core` | Firecracker クライアント、ランタイム、ウォームプール、テンプレート、ネットワーク |
| `coffer-agent` | ゲスト側 Agent バイナリ（MicroVM 内で実行） |

## プロトコル

Coffer は **vsock ポート 1024** 上でシンプルな JSON Lines プロトコルを使用します。

### リクエスト

```json
{"method":"exec","request_id":"r1","cmd":["echo","hello"],"env":{},"working_dir":null,"stdin":null,"timeout_ms":5000}
```

### レスポンス

```json
{"status":"ok","request_id":"r1","exit_code":0,"stdout":"aGVsbG8=","stderr":""}
```

メソッド: `exec`、`upload`、`download`、`ping`

完全なスキーマは [`coffer-protocol/src/lib.rs`](crates/coffer-protocol/src/lib.rs) を参照してください。

## 設定

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

## セキュリティモデル

- **カーネル隔離** — 各サンドボックスは KVM 経由で独立した Linux カーネルを実行
- **Jailer サポート** — オプションの chroot + seccomp + 名前空間隔離で Firecracker プロセス自体を保護
- **ネットワークポリシー** — iptables によるサンドボックスごとの送信許可/拒否リスト
- **EROFS 不変性** — ルートファイルシステムは読み取り専用；すべての書き込みは tmpfs オーバーレイへ
- **VMGenID** — Firecracker がスナップショット復元のたびにゲストエントロピーを再シード

## ライセンス

MIT — [LICENSE](LICENSE) を参照してください。

## 謝辞

- [AWS Firecracker](https://github.com/firecracker-microvm/firecracker) — 基盤となる VMM
- [EROFS](https://erofs.docs.kernel.org/) — 拡張読み取り専用ファイルシステム

---

**Coffer** は [AgentLink](https://github.com/agentlink-im/agentlink) エコシステムの一部です。
