//! Main runtime orchestrator: acquire sandboxes from warm pool or cold start.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::{debug, instrument, warn};

use crate::config::RuntimeConfig;
use crate::error::{CofferError, Result};
use crate::firecracker::FirecrackerClient;
use crate::net::{NetworkManager, NetworkConfig};
use crate::pool::WarmPool;
use crate::protocol::ExecOutput;
use crate::template::TemplateManager;

static NEXT_CID: AtomicU64 = AtomicU64::new(1024);

/// The main Coffer runtime.
#[derive(Clone)]
pub struct Runtime {
    config: Arc<RuntimeConfig>,
    templates: Arc<TemplateManager>,
    pool: Arc<WarmPool>,
    network: Arc<NetworkManager>,
    /// Host-side vsock UDS directory.
    vsock_dir: std::path::PathBuf,
}

impl Runtime {
    pub async fn new(config: RuntimeConfig) -> Result<Self> {
        let config = Arc::new(config);
        let templates = Arc::new(TemplateManager::new(
            config.template_dir.clone(),
            config.kernel_path.clone(),
            config.firecracker_path.clone(),
        ));

        let net_config = NetworkConfig {
            bridge_name: config.network.bridge_name.clone(),
            subnet: config.network.subnet.clone(),
            tap_prefix: config.network.tap_prefix.clone(),
        };
        let network = Arc::new(NetworkManager::new(net_config));
        network.setup_bridge().await?;

        let pool = Arc::new(WarmPool::new(
            config.clone(),
            templates.clone(),
            network.clone(),
        ));

        let vsock_dir = config.socket_dir.join("vsock");
        std::fs::create_dir_all(&vsock_dir)?;

        let runtime = Self {
            config,
            templates,
            pool,
            network,
            vsock_dir,
        };

        // Spawn pool background tasks.
        runtime.pool.start_background_tasks().await;

        Ok(runtime)
    }

    /// Acquire a sandbox. Fast path uses warm pool (<50ms).
    #[instrument(skip(self), fields(template_id = template_id))]
    pub async fn acquire(&self, template_id: &str) -> Result<SandboxHandle> {
        debug!("Acquiring sandbox");

        // Fast path: warm pool.
        if let Some(pooled) = self.pool.acquire(template_id).await {
            debug!(vm_id = %pooled.vm_id, "Acquired from warm pool");
            let fc = FirecrackerClient::new(pooled.socket_path.clone());
            fc.resume().await?;
            return self.build_handle(pooled.vm_id, fc, pooled.vsock_path).await;
        }

        warn!("Warm pool empty, cold starting");
        self.create_cold(template_id).await
    }

    /// Create a sandbox from scratch (slow path, <150ms target with snapshot).
    #[instrument(skip(self), fields(template_id = template_id))]
    pub async fn create_cold(&self, template_id: &str) -> Result<SandboxHandle> {
        let template = self.templates.get(template_id)?;
        let vm_id = format!("coffer-{}", uuid::Uuid::new_v4().to_string()[..8].to_string());
        let vsock_path = self.vsock_dir.join(format!("{}.sock", vm_id));

        let tap = self.network.allocate_tap(&vm_id).await?;
        let cid = NEXT_CID.fetch_add(1, Ordering::SeqCst) as u32;

        // Spawn Firecracker process (optionally via Jailer).
        let proc = spawn_vm_process(&self.config, &vm_id, &format!("{}.sock", vm_id), &format!("{}.log", vm_id))?;
        let host_socket = proc.host_socket_path;
        let _fc_proc = proc.child;

        // Wait for socket.
        wait_for_socket(&host_socket, Duration::from_millis(2000)).await?;

        let fc = FirecrackerClient::new(host_socket);

        // Configure VM.
        fc.create_vm(&crate::firecracker::MachineConfig {
            vcpu_count: template.vcpus as u8,
            mem_size_mib: template.memory_mib,
            smt: false,
            track_dirty_pages: true,
        }).await?;

        // Set rootfs drive.
        fc.add_drive("rootfs", &crate::firecracker::DriveConfig {
            drive_id: "rootfs".into(),
            path_on_host: template.rootfs_path.clone(),
            is_root_device: true,
            is_read_only: true,
        }).await?;

        // Set network interface.
        fc.add_network_interface(&crate::firecracker::NetworkInterfaceConfig {
            iface_id: "eth0".into(),
            guest_mac: format!("02:00:00:{:02x}:{:02x}:{:02x}",
                (cid >> 16) & 0xff, (cid >> 8) & 0xff, cid & 0xff),
            host_dev_name: tap.clone(),
        }).await?;

        // Set vsock.
        fc.add_vsock(&crate::firecracker::VsockConfig {
            guest_cid: cid,
            uds_path: vsock_path.clone(),
        }).await?;

        // Load snapshot if available.
        if template.snapshot_mem_path.exists() && template.snapshot_state_path.exists() {
            fc.load_snapshot(&crate::firecracker::LoadSnapshotRequest {
                snapshot_path: template.snapshot_state_path.clone(),
                mem_backend: crate::firecracker::MemoryBackend::File {
                    backend_path: template.snapshot_mem_path.clone(),
                },
                resume_vm: true,
            }).await?;
        } else {
            // Boot from kernel.
            fc.set_kernel(&crate::firecracker::KernelConfig {
                kernel_image_path: template.kernel_path.clone(),
                boot_args: template.kernel_args.clone(),
            }).await?;
            fc.start_microvm().await?;
        }

        self.build_handle(vm_id.clone(), fc, vsock_path).await
    }

    async fn build_handle(
        &self,
        vm_id: String,
        fc: FirecrackerClient,
        vsock_path: std::path::PathBuf,
    ) -> Result<SandboxHandle> {
        // Wait for agent to be ready.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(5000);
        loop {
            if tokio::time::Instant::now() > deadline {
                let _ = fc.shutdown().await;
                return Err(CofferError::AgentNotReady(vm_id));
            }
            if vsock_path.exists() {
                // Try to ping.
                if let Ok(mut client) = crate::protocol::VsockClient::connect(&vsock_path) {
                    if client.ping().is_ok() {
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let sandbox = Sandbox {
            vm_id: vm_id.clone(),
            fc: Mutex::new(fc),
            vsock_path: vsock_path.clone(),
            network: self.network.clone(),
            pool: Some(self.pool.clone()),
        };

        Ok(SandboxHandle {
            vm_id,
            sandbox: Arc::new(sandbox),
        })
    }

    pub fn templates(&self) -> Arc<TemplateManager> {
        self.templates.clone()
    }
}

/// An active sandbox wrapping a Firecracker MicroVM.
pub struct Sandbox {
    vm_id: String,
    fc: Mutex<FirecrackerClient>,
    vsock_path: std::path::PathBuf,
    network: Arc<NetworkManager>,
    pool: Option<Arc<WarmPool>>,
}

impl Sandbox {
    pub fn vm_id(&self) -> &str {
        &self.vm_id
    }

    pub fn vsock_path(&self) -> &std::path::Path {
        &self.vsock_path
    }

    /// Execute a command inside the sandbox.
    pub async fn exec(
        &self,
        cmd: &[&str],
        env: &HashMap<String, String>,
        timeout_ms: u64,
    ) -> Result<ExecOutput> {
        let vsock = self.vsock_path.clone();
        let cmd: Vec<String> = cmd.iter().map(|s| s.to_string()).collect();
        let env = env.clone();

        tokio::task::spawn_blocking(move || {
            let mut client = crate::protocol::VsockClient::connect(&vsock)?;
            client.exec(cmd, env, None, None, Some(timeout_ms))
        })
        .await
        .map_err(|e| CofferError::TaskJoin(e.to_string()))?
    }

    /// Upload a file.
    pub async fn upload_file(&self, guest_path: &str, data: Vec<u8>) -> Result<()> {
        let vsock = self.vsock_path.clone();
        let guest_path = guest_path.to_string();
        let req_id = uuid::Uuid::new_v4().to_string();
        let req = coffer_protocol::AgentRequest::Upload {
            request_id: req_id,
            guest_path,
            data,
            mode: Some(0o644),
        };

        tokio::task::spawn_blocking(move || {
            let mut client = crate::protocol::VsockClient::connect(&vsock)?;
            match client.call(&req)? {
                coffer_protocol::AgentResponse::Ok { .. } => Ok(()),
                coffer_protocol::AgentResponse::Error { message, code, .. } => {
                    Err(CofferError::AgentExec {
                        message: format!("{:?}: {}", code, message),
                        exit_code: None,
                    })
                }
                _ => Err(CofferError::AgentCommunication("Unexpected upload response".into())),
            }
        })
        .await
        .map_err(|e| CofferError::TaskJoin(e.to_string()))?
    }

    /// Download a file.
    pub async fn download_file(&self, guest_path: &str) -> Result<Vec<u8>> {
        let vsock = self.vsock_path.clone();
        let guest_path = guest_path.to_string();
        let req_id = uuid::Uuid::new_v4().to_string();
        let req = coffer_protocol::AgentRequest::Download {
            request_id: req_id,
            guest_path,
        };

        tokio::task::spawn_blocking(move || {
            let mut client = crate::protocol::VsockClient::connect(&vsock)?;
            match client.call(&req)? {
                coffer_protocol::AgentResponse::Ok { body: coffer_protocol::ResponseBody::Download { data }, .. } => {
                    Ok(data)
                }
                coffer_protocol::AgentResponse::Error { message, code, .. } => {
                    Err(CofferError::AgentExec {
                        message: format!("{:?}: {}", code, message),
                        exit_code: None,
                    })
                }
                _ => Err(CofferError::AgentCommunication("Unexpected download response".into())),
            }
        })
        .await
        .map_err(|e| CofferError::TaskJoin(e.to_string()))?
    }

    pub async fn pause(&self) -> Result<()> {
        self.fc.lock().await.pause().await
    }

    pub async fn resume(&self) -> Result<()> {
        self.fc.lock().await.resume().await
    }

    /// Gracefully shutdown the sandbox.
    pub async fn shutdown(&self) -> Result<()> {
        self.fc.lock().await.shutdown().await?;
        self.network.deallocate_tap(&self.vm_id).await?;
        Ok(())
    }

    /// Destroy the VM and clean up all resources.
    pub async fn destroy(&self) -> Result<()> {
        let _ = self.fc.lock().await.shutdown().await;
        self.network.deallocate_tap(&self.vm_id).await?;
        Ok(())
    }

    /// Return this sandbox to the warm pool (if configured).
    pub async fn return_to_pool(&self) -> Result<()> {
        if let Some(ref pool) = self.pool {
            self.pause().await?;
            pool.release(self.vm_id.clone(), self.vsock_path.clone()).await;
            Ok(())
        } else {
            Err(CofferError::Pool("No warm pool configured".into()))
        }
    }
}

/// RAII handle that returns sandbox to pool on drop (unless `into_inner` is called).
pub struct SandboxHandle {
    vm_id: String,
    sandbox: Arc<Sandbox>,
}

impl SandboxHandle {
    pub fn vm_id(&self) -> &str {
        &self.vm_id
    }

    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    /// Consume the handle and return the underlying sandbox (caller takes ownership).
    pub fn into_inner(self) -> Arc<Sandbox> {
        self.sandbox.clone()
    }
}

impl Drop for SandboxHandle {
    fn drop(&mut self) {
        let sandbox = self.sandbox.clone();
        let vm_id = self.vm_id.clone();
        tokio::spawn(async move {
            if let Err(e) = sandbox.return_to_pool().await {
                debug!(%vm_id, error = %e, "Failed to return sandbox to pool, destroying");
                let _ = sandbox.destroy().await;
            }
        });
    }
}

/// Result of spawning a VM process (direct Firecracker or via Jailer).
pub struct VmProcess {
    pub child: tokio::process::Child,
    /// The socket path on the *host* side that the Firecracker API listens on.
    pub host_socket_path: std::path::PathBuf,
}

/// Spawn a Firecracker process, optionally via the Jailer.
pub fn spawn_vm_process(
    config: &crate::config::RuntimeConfig,
    vm_id: &str,
    socket_name: &str,
    log_name: &str,
) -> Result<VmProcess> {
    use tokio::process::Command;

    if let Some(ref jailer_cfg) = config.jailer {
        let jailer_path = config.jailer_path.as_ref()
            .ok_or_else(|| CofferError::Config("jailer_path not set but jailer config present".into()))?;

        let chroot_root = jailer_cfg.chroot_base_dir.join(vm_id).join("root");
        std::fs::create_dir_all(&chroot_root)?;
        std::fs::create_dir_all(chroot_root.join("run"))?;

        let chroot_socket = std::path::PathBuf::from("/run").join(socket_name);
        let chroot_log = std::path::PathBuf::from("/run").join(log_name);
        let host_socket = chroot_root.join("run").join(socket_name);

        let mut cmd = Command::new(jailer_path);
        cmd.arg("--id").arg(vm_id)
            .arg("--uid").arg(jailer_cfg.uid.to_string())
            .arg("--gid").arg(jailer_cfg.gid.to_string())
            .arg("--chroot-base-dir").arg(&jailer_cfg.chroot_base_dir)
            .arg("--exec-file").arg(&config.firecracker_path);

        if let Some(ref netns) = jailer_cfg.netns {
            cmd.arg("--netns").arg(netns);
        }
        if jailer_cfg.daemonize {
            cmd.arg("--daemonize");
        }
        if jailer_cfg.new_pid_ns {
            cmd.arg("--new-pid-ns");
        }

        cmd.arg("--")
            .arg("--api-sock").arg(&chroot_socket)
            .arg("--log-path").arg(&chroot_log)
            .arg("--level").arg("Warn")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let child = cmd.spawn()?;
        Ok(VmProcess { child, host_socket_path: host_socket })
    } else {
        let socket_path = config.socket_dir.join(socket_name);
        let log_path = config.socket_dir.join(log_name);
        let child = Command::new(&config.firecracker_path)
            .arg("--api-sock").arg(&socket_path)
            .arg("--log-path").arg(&log_path)
            .arg("--level").arg("Warn")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        Ok(VmProcess { child, host_socket_path: socket_path })
    }
}

async fn wait_for_socket(path: &std::path::Path, timeout: Duration) -> Result<()> {
    let start = tokio::time::Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Err(CofferError::Firecracker(format!(
        "Socket {} did not appear within {:?}",
        path.display(),
        timeout
    )))
}
