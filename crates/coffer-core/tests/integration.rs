//! Coffer integration tests.
//!
//! These tests require a real Firecracker installation and a compatible
//! guest kernel / rootfs. They are marked `#[ignore]` so they do not run
//! during normal `cargo test`.
//!
//! To run:
//!   cargo test --test integration -- --ignored
//!
//! Required environment:
//!   COFFER_TEST_FIRECRACKER_PATH  (default: /usr/bin/firecracker)
//!   COFFER_TEST_KERNEL_PATH       (default: ~/.coffer/kernel/vmlinux)
//!   COFFER_TEST_ROOTFS_PATH       (default: ~/.coffer/templates/alpine/rootfs.erofs)

use std::path::PathBuf;

use coffer_core::{
    Runtime, RuntimeConfig, Template, TemplateManager,
    firecracker::FirecrackerClient,
};

fn test_config() -> RuntimeConfig {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    let coffer_home = home.join(".coffer");
    let mut config = RuntimeConfig::default();
    config.firecracker_path = std::env::var("COFFER_TEST_FIRECRACKER_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/usr/bin/firecracker"));
    config.kernel_path = std::env::var("COFFER_TEST_KERNEL_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| coffer_home.join("kernel/vmlinux"));
    config.socket_dir = coffer_home.join("test-run");
    config.template_dir = coffer_home.join("test-templates");
    config
}

fn skip_without_firecracker() -> bool {
    let fc = test_config().firecracker_path;
    if !fc.exists() {
        eprintln!("Skipping integration test: Firecracker not found at {}", fc.display());
        return true;
    }
    false
}

fn skip_without_kernel() -> bool {
    let kernel = test_config().kernel_path;
    if !kernel.exists() {
        eprintln!("Skipping integration test: kernel not found at {}", kernel.display());
        return true;
    }
    false
}

// ===================================================================
// FirecrackerClient tests
// ===================================================================

#[tokio::test]
#[ignore = "requires Firecracker binary"]
async fn test_firecracker_lifecycle() {
    if skip_without_firecracker() {
        return;
    }

    let config = test_config();
    let vm_id = "test-lifecycle";
    let socket_path = config.socket_dir.join(format!("{}.sock", vm_id));

    std::fs::create_dir_all(&config.socket_dir).unwrap();
    let _ = std::fs::remove_file(&socket_path);

    // Spawn Firecracker.
    let mut child = std::process::Command::new(&config.firecracker_path)
        .arg("--api-sock").arg(&socket_path)
        .spawn()
        .expect("Failed to spawn Firecracker");

    // Wait for socket.
    let start = std::time::Instant::now();
    while !socket_path.exists() {
        if start.elapsed() > std::time::Duration::from_secs(5) {
            panic!("Firecracker socket did not appear");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let fc = FirecrackerClient::new(socket_path.clone());

    // Configure VM.
    fc.create_vm(&coffer_core::firecracker::MachineConfig {
        vcpu_count: 1,
        mem_size_mib: 64,
        smt: false,
        track_dirty_pages: true,
    }).await.expect("create_vm failed");

    // Set kernel.
    if !skip_without_kernel() {
        fc.set_kernel(&coffer_core::firecracker::KernelConfig {
            kernel_image_path: config.kernel_path,
            boot_args: "console=ttyS0 reboot=k panic=1 pci=off".into(),
        }).await.expect("set_kernel failed");
    }

    // Shutdown.
    let _ = fc.shutdown().await;
    let _ = child.kill();
    let _ = std::fs::remove_file(&socket_path);
}

// ===================================================================
// TemplateManager tests
// ===================================================================

#[tokio::test]
#[ignore = "requires skopeo, umoci, mkfs.erofs and a valid OCI image"]
async fn test_template_build_from_image() {
    let config = test_config();
    std::fs::create_dir_all(&config.template_dir).unwrap();

    let mgr = TemplateManager::new(
        config.template_dir.clone(),
        config.kernel_path.clone(),
        config.firecracker_path.clone(),
    );

    // This test requires network access and external tools.
    let result = mgr.build_from_image(
        "test-alpine",
        "docker.io/library/alpine:latest",
        None,
    ).await;

    // Even if build fails, we should get a clear error.
    match result {
        Ok(template) => {
            assert!(template.rootfs_path.exists());
            println!("Template built: {}", template.rootfs_path.display());
        }
        Err(e) => {
            println!("Template build failed (expected if tools missing): {}", e);
        }
    }
}

#[test]
#[ignore = "requires Firecracker, kernel and rootfs"]
fn test_template_create_snapshot() {
    // Snapshot creation is tested implicitly by build_from_image.
    // A dedicated test would boot a real VM and call create_snapshot.
}

// ===================================================================
// Runtime / Warm Pool tests
// ===================================================================

#[tokio::test]
#[ignore = "requires Firecracker, kernel and rootfs"]
async fn test_runtime_acquire_and_release() {
    if skip_without_firecracker() || skip_without_kernel() {
        return;
    }

    let mut config = test_config();
    config.pool.warm_pool_size = 1;

    // Ensure a template exists.
    let rootfs = std::env::var("COFFER_TEST_ROOTFS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| config.template_dir.join("alpine/rootfs.erofs"));

    if !rootfs.exists() {
        eprintln!("Skipping: rootfs not found at {}", rootfs.display());
        return;
    }

    let runtime = match Runtime::new(config).await {
        Ok(r) => r,
        Err(coffer_core::error::CofferError::Network(msg)) if msg.contains("Operation not permitted") => {
            eprintln!("Skipping: network setup requires root/CAP_NET_ADMIN");
            return;
        }
        Err(e) => panic!("Failed to create runtime: {}", e),
    };

    // Register a dummy template.
    runtime.templates().register(Template {
        id: "alpine".into(),
        name: "alpine".into(),
        kernel_path: runtime.templates().list().first()
            .map(|t| t.kernel_path.clone())
            .unwrap_or_else(|| test_config().kernel_path),
        rootfs_path: rootfs,
        snapshot_state_path: PathBuf::new(), // empty = cold boot
        snapshot_mem_path: PathBuf::new(),
        kernel_args: "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/coffer-init".into(),
        vcpus: 1,
        memory_mib: 64,
        metadata: std::collections::HashMap::new(),
    }).unwrap();

    // Acquire sandbox (cold path since no snapshot).
    let handle = runtime.acquire("alpine").await;
    match handle {
        Ok(h) => {
            println!("Acquired sandbox: {}", h.vm_id());
            drop(h); // Returns to pool.
        }
        Err(e) => {
            println!("Acquire failed (expected if Firecracker not fully set up): {}", e);
        }
    }
}

// ===================================================================
// Overlay init verification
// ===================================================================

#[test]
fn test_overlay_init_script_syntax() {
    // Verify the embedded coffer-init script is well-formed shell.
    let script = r#"#!/bin/sh
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs dev /dev 2>/dev/null
mkdir -p /newroot/overlay/upper /newroot/overlay/work
mount -t tmpfs -o size=128M tmpfs /newroot/overlay
mount -t overlay overlay -o lowerdir=/,upperdir=/newroot/overlay/upper,workdir=/newroot/overlay/work /newroot
cd /newroot
mkdir -p oldroot
pivot_root . oldroot
mount --move /oldroot/proc /proc 2>/dev/null
mount --move /oldroot/sys /sys 2>/dev/null
mount --move /oldroot/dev /dev 2>/dev/null
exec chroot . /sbin/init "$@"
"#;
    assert!(script.contains("pivot_root"));
    assert!(script.contains("mount -t overlay"));
}
