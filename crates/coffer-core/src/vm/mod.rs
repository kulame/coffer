pub mod client;
pub mod state;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::{Child, Command};
use uuid::Uuid;

use client::FirecrackerClient;
use state::{StateMachine, VmLifecycle};

use crate::error::{CofferError, Result};
use crate::vm::client::{
    Action, ActionType, BootSource, Drive, MachineConfig, MemBackend, MemBackendType,
    NetworkInterface, SnapshotCreate, SnapshotLoad, SnapshotType, VmState, VmStateEnum, Vsock,
};

/// A single Firecracker microVM instance.
pub struct FirecrackerVm {
    pub id: String,
    pub config: VmConfig,
    pub state: StateMachine,
    pub client: FirecrackerClient,
    pub process: Child,
    pub socket_path: PathBuf,
    pub vsock_path: PathBuf,
    pub snapshot_mem_path: Option<PathBuf>,
    pub snapshot_state_path: Option<PathBuf>,
}

/// Configuration for a single VM.
#[derive(Debug, Clone)]
pub struct VmConfig {
    pub kernel_path: PathBuf,
    pub rootfs_path: PathBuf,
    pub vcpus: u32,
    pub memory_mib: u32,
    pub tap_name: String,
    pub guest_mac: String,
    pub guest_ip: String,
    pub vsock_uds_path: PathBuf,
    pub is_rootfs_writable: bool,
}

impl FirecrackerVm {
    /// Create and boot a fresh VM from scratch (cold start).
    pub async fn create_and_boot(
        id: String,
        config: VmConfig,
        firecracker_bin: &Path,
        run_dir: &Path,
    ) -> Result<Self> {
        let socket_path = run_dir.join(format!("vm-{}.sock", id));
        let vsock_path = config.vsock_uds_path.clone();

        // 1. Start Firecracker process.
        let mut cmd = Command::new(firecracker_bin);
        cmd.arg("--api-sock")
            .arg(&socket_path)
            .arg("--id")
            .arg(&id)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut process = cmd.spawn()?;

        // 2. Wait for socket to appear.
        let start = std::time::Instant::now();
        while !socket_path.exists() {
            if start.elapsed().as_secs() > 10 {
                let _ = process.kill().await;
                return Err(CofferError::VmTimeout {
                    id: id.clone(),
                    dur_ms: 10000,
                });
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // 3. Connect API client.
        let client = FirecrackerClient::new(&socket_path)?;
        let mut vm = Self {
            id: id.clone(),
            config: config.clone(),
            state: StateMachine::new(),
            client,
            process,
            socket_path,
            vsock_path,
            snapshot_mem_path: None,
            snapshot_state_path: None,
        };

        // 4. Configure boot source.
        vm.client
            .put_boot_source(&BootSource {
                kernel_image_path: config.kernel_path.to_string_lossy().to_string(),
                boot_args: Some(
                    "console=ttyS0 reboot=k panic=1 pci=off nomodules".into(),
                ),
                initrd_path: None,
            })
            .await?;
        vm.state.transition(VmLifecycle::Configured)?;

        // 5. Configure machine.
        vm.client
            .put_machine_config(&MachineConfig {
                vcpu_count: config.vcpus,
                mem_size_mib: config.memory_mib,
                smt: Some(false),
                track_dirty_pages: Some(true),
                huge_pages: None,
            })
            .await?;

        // 6. Configure rootfs drive.
        vm.client
            .put_drive(
                "rootfs",
                &Drive {
                    drive_id: "rootfs".into(),
                    path_on_host: config.rootfs_path.to_string_lossy().to_string(),
                    is_root_device: true,
                    is_read_only: !config.is_rootfs_writable,
                    cache_type: Some("Unsafe".into()),
                    partuuid: None,
                },
            )
            .await?;

        // 7. Configure network.
        vm.client
            .put_network_interface(
                "eth0",
                &NetworkInterface {
                    iface_id: "eth0".into(),
                    host_dev_name: config.tap_name.clone(),
                    guest_mac: config.guest_mac.clone(),
                },
            )
            .await?;

        // 8. Configure vsock.
        vm.client
            .put_vsock(&Vsock {
                guest_cid: 3,
                uds_path: vsock_path.to_string_lossy().to_string(),
            })
            .await?;

        // 9. Boot.
        vm.client
            .put_action(&Action {
                action_type: ActionType::InstanceStart,
            })
            .await?;
        vm.state.transition(VmLifecycle::Running)?;

        tracing::info!(vm_id = %id, "VM created and booted");
        Ok(vm)
    }

    /// Create a VM from a snapshot (warm start path).
    pub async fn from_snapshot(
        id: String,
        config: VmConfig,
        snapshot_state: &Path,
        snapshot_mem: &Path,
        firecracker_bin: &Path,
        run_dir: &Path,
    ) -> Result<Self> {
        let socket_path = run_dir.join(format!("vm-{}-resume.sock", id));
        let vsock_path = config.vsock_uds_path.clone();

        // 1. Start Firecracker.
        let mut cmd = Command::new(firecracker_bin);
        cmd.arg("--api-sock")
            .arg(&socket_path)
            .arg("--id")
            .arg(&id)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut process = cmd.spawn()?;

        // 2. Wait for socket.
        let start = std::time::Instant::now();
        while !socket_path.exists() {
            if start.elapsed().as_secs() > 10 {
                let _ = process.kill().await;
                return Err(CofferError::VmTimeout {
                    id: id.clone(),
                    dur_ms: 10000,
                });
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let client = FirecrackerClient::new(&socket_path)?;
        let mut vm = Self {
            id: id.clone(),
            config: config.clone(),
            state: StateMachine::new(),
            client,
            process,
            socket_path,
            vsock_path,
            snapshot_mem_path: Some(snapshot_mem.to_path_buf()),
            snapshot_state_path: Some(snapshot_state.to_path_buf()),
        };

        // 3. Load snapshot.
        vm.client
            .load_snapshot(&SnapshotLoad {
                snapshot_path: snapshot_state.to_string_lossy().to_string(),
                mem_backend: MemBackend {
                    backend_type: MemBackendType::File,
                    backend_path: snapshot_mem.to_string_lossy().to_string(),
                },
                track_dirty_pages: Some(true),
                resume_vm: Some(false),
                enable_diff_snapshots: Some(false),
            })
            .await?;

        vm.state.transition(VmLifecycle::SnapshotLoaded)?;
        tracing::info!(vm_id = %id, "VM loaded from snapshot (paused)");
        Ok(vm)
    }

    /// Resume from Paused or SnapshotLoaded.
    pub async fn resume(&mut self) -> Result<()> {
        match self.state.state() {
            VmLifecycle::Paused | VmLifecycle::SnapshotLoaded => {
                self.client
                    .patch_vm_state(&VmState {
                        state: VmStateEnum::Resumed,
                    })
                    .await?;
                self.state.transition(VmLifecycle::Running)?;
                tracing::info!(vm_id = %self.id, "VM resumed");
                Ok(())
            }
            other => Err(CofferError::InvalidVmState {
                id: self.id.clone(),
                expected: "paused or snapshot_loaded".into(),
                actual: other.to_string(),
            }),
        }
    }

    /// Pause the VM (for returning to pool).
    pub async fn pause(&mut self) -> Result<()> {
        match self.state.state() {
            VmLifecycle::Running => {
                self.client
                    .patch_vm_state(&VmState {
                        state: VmStateEnum::Paused,
                    })
                    .await?;
                self.state.transition(VmLifecycle::Paused)?;
                tracing::info!(vm_id = %self.id, "VM paused");
                Ok(())
            }
            other => Err(CofferError::InvalidVmState {
                id: self.id.clone(),
                expected: "running".into(),
                actual: other.to_string(),
            }),
        }
    }

    /// Create a snapshot (for template building).
    pub async fn create_snapshot(&mut self, state_path: &Path, mem_path: &Path) -> Result<()> {
        match self.state.state() {
            VmLifecycle::Paused => {
                self.client
                    .create_snapshot(&SnapshotCreate {
                        snapshot_type: SnapshotType::Full,
                        snapshot_path: state_path.to_string_lossy().to_string(),
                        mem_file_path: mem_path.to_string_lossy().to_string(),
                    })
                    .await?;
                self.snapshot_state_path = Some(state_path.to_path_buf());
                self.snapshot_mem_path = Some(mem_path.to_path_buf());
                self.state.transition(VmLifecycle::SnapshotCreated)?;
                tracing::info!(vm_id = %self.id, "Snapshot created");
                Ok(())
            }
            other => Err(CofferError::InvalidVmState {
                id: self.id.clone(),
                expected: "paused".into(),
                actual: other.to_string(),
            }),
        }
    }

    /// Graceful shutdown via CtrlAltDel.
    pub async fn shutdown(&mut self) -> Result<()> {
        self.client
            .put_action(&Action {
                action_type: ActionType::SendCtrlAltDel,
            })
            .await?;

        // Wait for process exit with timeout.
        let timeout = tokio::time::Duration::from_secs(10);
        match tokio::time::timeout(timeout, self.process.wait()).await {
            Ok(Ok(status)) => {
                tracing::info!(vm_id = %self.id, exit_code = ?status.code(), "VM shut down");
            }
            Ok(Err(e)) => {
                tracing::warn!(vm_id = %self.id, error = %e, "VM process wait error");
            }
            Err(_) => {
                tracing::warn!(vm_id = %self.id, "VM shutdown timeout, killing");
                let _ = self.process.kill().await;
            }
        }
        self.state.transition(VmLifecycle::Exited)?;
        Ok(())
    }

    /// Force kill the VM process.
    pub async fn kill(&mut self) -> Result<()> {
        let _ = self.process.kill().await?;
        self.state.transition(VmLifecycle::Exited)?;
        Ok(())
    }
}
