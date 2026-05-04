//! Warm pool: maintain pre-created MicroVMs ready for instant resume.

use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};

use crate::config::RuntimeConfig;
use crate::firecracker::{FirecrackerClient, MachineConfig, DriveConfig, NetworkInterfaceConfig, VsockConfig, LoadSnapshotRequest, MemoryBackend};
use crate::net::NetworkManager;
use crate::template::TemplateManager;

static NEXT_CID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1024);

/// A VM that has been pre-created and paused, ready for resume.
pub struct PooledVm {
    pub vm_id: String,
    pub socket_path: PathBuf,
    pub vsock_path: PathBuf,
}

/// Warm pool maintains a queue of paused VMs per template.
pub struct WarmPool {
    config: Arc<RuntimeConfig>,
    templates: Arc<TemplateManager>,
    network: Arc<NetworkManager>,
    available: DashMap<String, Arc<Mutex<Vec<PooledVm>>>>,
    in_use: DashMap<String, ()>,
    tx: mpsc::UnboundedSender<PoolCommand>,
}

#[allow(dead_code)]
enum PoolCommand {
    Warm { template_id: String },
    Release { vm_id: String, vsock_path: PathBuf },
    Destroy { vm_id: String },
}

impl WarmPool {
    pub fn new(
        config: Arc<RuntimeConfig>,
        templates: Arc<TemplateManager>,
        network: Arc<NetworkManager>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let pool = Self {
            config,
            templates,
            network,
            available: DashMap::new(),
            in_use: DashMap::new(),
            tx,
        };
        pool.start_worker(rx);
        pool
    }

    pub async fn start_background_tasks(&self) {
        // Pre-warm templates that exist.
        for template in self.templates.list() {
            let count = self.config.pool.warm_pool_size;
            for _ in 0..count {
                let _ = self.tx.send(PoolCommand::Warm {
                    template_id: template.id.clone(),
                });
            }
        }

        // Spawn periodic re-balance task.
        let tx = self.tx.clone();
        let templates = self.templates.clone();
        let _warm_size = self.config.pool.warm_pool_size;
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(10));
            loop {
                ticker.tick().await;
                for template in templates.list() {
                    let _ = tx.send(PoolCommand::Warm {
                        template_id: template.id.clone(),
                    });
                }
            }
        });
    }

    /// Acquire a VM from the warm pool.
    pub async fn acquire(&self, template_id: &str) -> Option<PooledVm> {
        let queue = self.available.entry(template_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(Vec::new())));
        let vm = queue.lock().pop()?;
        self.in_use.insert(vm.vm_id.clone(), ());
        debug!(vm_id = %vm.vm_id, "Acquired from warm pool");
        Some(vm)
    }

    /// Release a VM back to the pool (called by Sandbox::return_to_pool).
    pub async fn release(&self, vm_id: String, _vsock_path: PathBuf) {
        self.in_use.remove(&vm_id);
        // We don't directly add back here; the background worker will re-warm.
        // But we do need to clean up resources if pool is full.
        debug!(%vm_id, "Released from active use");
    }

    fn start_worker(&self, mut rx: mpsc::UnboundedReceiver<PoolCommand>) {
        let available = self.available.clone();
        let templates = self.templates.clone();
        let network = self.network.clone();
        let config = self.config.clone();

        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    PoolCommand::Warm { template_id } => {
                        let queue = available.entry(template_id.clone())
                            .or_insert_with(|| Arc::new(Mutex::new(Vec::new())));
                        let current = queue.value().lock().len();
                        if current >= config.pool.warm_pool_size {
                            continue;
                        }
                        if let Err(e) = warm_one(&config, &templates, &network, &template_id, queue.value()).await {
                            warn!(error = %e, template_id, "Failed to warm VM");
                        }
                    }
                    PoolCommand::Release { vm_id, vsock_path } => {
                        // Destroy old VM, background worker will create a new one.
                        let _ = destroy_vm(&vm_id, &vsock_path).await;
                    }
                    PoolCommand::Destroy { vm_id: _ } => {
                        // Clean up.
                    }
                }
            }
        });
    }
}

async fn warm_one(
    config: &RuntimeConfig,
    templates: &TemplateManager,
    network: &NetworkManager,
    template_id: &str,
    queue: &Arc<Mutex<Vec<PooledVm>>>,
) -> crate::error::Result<()> {
    let template = templates.get(template_id)?;
    let vm_id = format!("coffer-pool-{}", uuid::Uuid::new_v4().to_string()[..8].to_string());
    let vsock_path = config.socket_dir.join("vsock").join(format!("{}.sock", vm_id));

    let tap = network.allocate_tap(&vm_id).await?;
    let cid = NEXT_CID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    // Spawn Firecracker (optionally via Jailer).
    let proc = crate::runtime::spawn_vm_process(config, &vm_id, &format!("{}.sock", vm_id), &format!("{}.log", vm_id))?;
    let host_socket = proc.host_socket_path;
    let _child = proc.child;

    // Wait for socket.
    wait_for_socket(&host_socket, Duration::from_millis(2000)).await?;

    let fc = FirecrackerClient::new(host_socket.clone());

    fc.create_vm(&MachineConfig {
        vcpu_count: template.vcpus as u8,
        mem_size_mib: template.memory_mib,
        smt: false,
        track_dirty_pages: true,
    }).await?;

    fc.add_drive("rootfs", &DriveConfig {
        drive_id: "rootfs".into(),
        path_on_host: template.rootfs_path.clone(),
        is_root_device: true,
        is_read_only: true,
    }).await?;

    fc.add_network_interface(&NetworkInterfaceConfig {
        iface_id: "eth0".into(),
        guest_mac: format!("02:00:00:{:02x}:{:02x}:{:02x}",
            (cid >> 16) & 0xff, (cid >> 8) & 0xff, cid & 0xff),
        host_dev_name: tap,
    }).await?;

    fc.add_vsock(&VsockConfig {
        guest_cid: cid,
        uds_path: vsock_path.clone(),
    }).await?;

    // Load snapshot or boot.
    if template.snapshot_mem_path.exists() && template.snapshot_state_path.exists() {
        fc.load_snapshot(&LoadSnapshotRequest {
            snapshot_path: template.snapshot_state_path.clone(),
            mem_backend: MemoryBackend::File {
                backend_path: template.snapshot_mem_path.clone(),
            },
            resume_vm: false,
        }).await?;
    } else {
        fc.set_kernel(&crate::firecracker::KernelConfig {
            kernel_image_path: template.kernel_path.clone(),
            boot_args: template.kernel_args.clone(),
        }).await?;
        fc.start_microvm().await?;

        // Wait for agent readiness, then pause.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if tokio::time::Instant::now() > deadline {
                return Err(crate::error::CofferError::AgentNotReady(vm_id));
            }
            if vsock_path.exists() {
                if let Ok(mut client) = crate::protocol::VsockClient::connect(&vsock_path) {
                    if client.ping().is_ok() {
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fc.pause().await?;

    queue.lock().push(PooledVm {
        vm_id,
        socket_path: host_socket,
        vsock_path,
    });

    info!(template_id, "Warmed one VM");
    Ok(())
}

async fn wait_for_socket(path: &std::path::Path, timeout: Duration) -> crate::error::Result<()> {
    let start = tokio::time::Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Err(crate::error::CofferError::Firecracker(format!(
        "Socket {} did not appear within {:?}",
        path.display(),
        timeout
    )))
}

async fn destroy_vm(_vm_id: &str, vsock_path: &std::path::Path) -> crate::error::Result<()> {
    // Kill process, clean up socket files.
    if vsock_path.exists() {
        let _ = tokio::fs::remove_file(vsock_path).await;
    }
    Ok(())
}
