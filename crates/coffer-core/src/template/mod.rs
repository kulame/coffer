//! Template management: build, store, and version VM templates.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use parking_lot::RwLock;
use tracing::{info, warn};
use uuid::Uuid;

use crate::error::{CofferError, Result};
use crate::firecracker::{
    FirecrackerClient, MachineConfig, KernelConfig, DriveConfig,
    VsockConfig, CreateSnapshotRequest,
};

pub mod builder;
pub use builder::{BuildOutput, ImageBuilder};

/// Specification for building a new template.
#[derive(Debug, Clone)]
pub struct TemplateSpec {
    pub id: String,
    pub name: String,
    pub kernel_path: PathBuf,
    pub rootfs_path: PathBuf,
    pub kernel_args: String,
    pub vcpus: u32,
    pub memory_mib: u32,
    pub init_commands: Vec<String>,
}

/// A template is a pre-built VM image (kernel + rootfs + snapshot) that can be
/// cloned to create sandboxes.
#[derive(Debug, Clone)]
pub struct Template {
    pub id: String,
    pub name: String,
    pub kernel_path: PathBuf,
    pub rootfs_path: PathBuf,
    pub snapshot_state_path: PathBuf,
    pub snapshot_mem_path: PathBuf,
    pub kernel_args: String,
    pub vcpus: u32,
    pub memory_mib: u32,
    pub metadata: HashMap<String, String>,
}

/// Manages template storage and retrieval.
pub struct TemplateManager {
    template_dir: PathBuf,
    kernel_path: PathBuf,
    firecracker_path: PathBuf,
    agent_bin: Option<PathBuf>,
    templates: RwLock<HashMap<String, Template>>,
}

impl TemplateManager {
    pub fn new(template_dir: PathBuf, kernel_path: PathBuf, firecracker_path: PathBuf) -> Self {
        let mut mgr = Self {
            template_dir: template_dir.clone(),
            kernel_path,
            firecracker_path,
            agent_bin: None,
            templates: RwLock::new(HashMap::new()),
        };
        if let Err(e) = mgr.load_all() {
            warn!(error = %e, "Failed to load existing templates");
        }
        mgr
    }

    pub fn with_agent_bin(mut self, path: PathBuf) -> Self {
        self.agent_bin = Some(path);
        self
    }

    /// Register a new template (built externally).
    pub fn register(&self, template: Template) -> Result<()> {
        let mut templates = self.templates.write();
        templates.insert(template.id.clone(), template);
        Ok(())
    }

    /// Get a template by ID.
    pub fn get(&self, id: &str) -> Result<Template> {
        let templates = self.templates.read();
        templates
            .get(id)
            .cloned()
            .ok_or_else(|| CofferError::TemplateNotFound(id.into()))
    }

    /// List all templates.
    pub fn list(&self) -> Vec<Template> {
        self.templates.read().values().cloned().collect()
    }

    /// Build a template from an OCI image.
    ///
    /// Pipeline:
    /// 1. skopeo copy docker://image → OCI layout
    /// 2. umoci unpack → rootfs directory
    /// 3. mkfs.erofs rootfs/ → rootfs.erofs
    /// 4. Boot a throwaway VM, pause, create snapshot.
    pub async fn build_from_image(
        &self,
        name: &str,
        image: &str,
        kernel_args: Option<String>,
    ) -> Result<Template> {
        let id = format!("{}-{}", name, &Uuid::new_v4().to_string()[..8]);
        let dir = self.template_dir.join(&id);
        std::fs::create_dir_all(&dir)?;

        info!(template_id = %id, image, "Building template from OCI image");

        // 1. Build rootfs from OCI image.
        let mut builder = ImageBuilder::new(
            dir.join("build"),
            image.into(),
            self.kernel_path.clone(),
        );
        if let Some(ref args) = kernel_args {
            builder = builder.with_kernel_args(args.clone());
        }
        if let Some(ref agent) = self.agent_bin {
            builder = builder.with_agent_bin(agent.clone());
        }
        let output = builder.build().await?;

        // 2. Create snapshot by booting a temporary VM.
        let snapshot_state = dir.join("snapshot.state");
        let snapshot_mem = dir.join("snapshot.mem");
        self.create_snapshot(
            &id,
            &output.kernel_path,
            &output.rootfs_path,
            &output.kernel_args,
            &snapshot_state,
            &snapshot_mem,
        ).await?;

        let template = Template {
            id: id.clone(),
            name: name.into(),
            kernel_path: output.kernel_path,
            rootfs_path: output.rootfs_path,
            snapshot_state_path: snapshot_state,
            snapshot_mem_path: snapshot_mem,
            kernel_args: kernel_args.unwrap_or_else(|| output.kernel_args),
            vcpus: 1,
            memory_mib: 256,
            metadata: HashMap::new(),
        };

        self.register(template.clone())?;
        info!(template_id = %id, "Template build complete");
        Ok(template)
    }

    /// Create a Firecracker snapshot from a running MicroVM.
    ///
    /// Steps:
    /// 1. Spawn Firecracker process
    /// 2. Configure VM (machine-config, kernel, rootfs drive, vsock)
    /// 3. Boot VM
    /// 4. Wait for agent readiness via vsock
    /// 5. Pause VM
    /// 6. PUT /snapshot/create
    /// 7. Shutdown VM
    pub async fn create_snapshot(
        &self,
        vm_id: &str,
        kernel_path: &Path,
        rootfs_path: &Path,
        kernel_args: &str,
        snapshot_state_path: &Path,
        snapshot_mem_path: &Path,
    ) -> Result<()> {
        info!(%vm_id, "Creating snapshot");

        let socket_path = self.template_dir.join(format!("snap-{}.sock", vm_id));
        let log_path = self.template_dir.join(format!("snap-{}.log", vm_id));
        let vsock_path = self.template_dir.join(format!("snap-{}.vsock", vm_id));

        // Spawn temporary Firecracker.
        let _child = spawn_firecracker(&self.firecracker_path, &socket_path, &log_path)?;
        wait_for_socket(&socket_path, Duration::from_millis(2000)).await?;

        let fc = FirecrackerClient::new(socket_path.clone());

        // Configure VM.
        fc.create_vm(&MachineConfig {
            vcpu_count: 1,
            mem_size_mib: 256,
            smt: false,
            track_dirty_pages: true,
        }).await?;

        fc.set_kernel(&KernelConfig {
            kernel_image_path: kernel_path.to_path_buf(),
            boot_args: kernel_args.into(),
        }).await?;

        fc.add_drive("rootfs", &DriveConfig {
            drive_id: "rootfs".into(),
            path_on_host: rootfs_path.to_path_buf(),
            is_root_device: true,
            is_read_only: true,
        }).await?;

        fc.add_vsock(&VsockConfig {
            guest_cid: 100,
            uds_path: vsock_path.clone(),
        }).await?;

        // Boot.
        fc.start_microvm().await?;

        // Wait for vsock to appear and agent to respond.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if std::time::Instant::now() > deadline {
                let _ = fc.shutdown().await;
                return Err(CofferError::AgentNotReady(vm_id.into()));
            }
            let vsock_conn = format!("{}_1024", vsock_path.display());
            if Path::new(&vsock_conn).exists() {
                if let Ok(mut client) = crate::protocol::VsockClient::connect(Path::new(&vsock_conn)) {
                    if client.ping().is_ok() {
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Pause before snapshot.
        fc.pause().await?;

        // Create snapshot.
        fc.create_snapshot(&CreateSnapshotRequest {
            snapshot_type: "Full".into(),
            snapshot_path: snapshot_state_path.to_path_buf(),
            mem_file_path: snapshot_mem_path.to_path_buf(),
        }).await?;

        info!(%vm_id, "Snapshot created successfully");

        // Shutdown temporary VM.
        let _ = fc.shutdown().await;
        let _ = tokio::fs::remove_file(&socket_path).await;
        let _ = tokio::fs::remove_file(&log_path).await;
        let _ = tokio::fs::remove_file(&vsock_path).await;

        Ok(())
    }

    /// Load all templates from the template directory.
    fn load_all(&mut self) -> Result<()> {
        if !self.template_dir.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(&self.template_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                let id = path.file_name().unwrap().to_string_lossy().to_string();
                let kernel = path.join("kernel");
                let rootfs = path.join("rootfs.erofs");
                let snapshot_state = path.join("snapshot.state");
                let snapshot_mem = path.join("snapshot.mem");

                if kernel.exists() && rootfs.exists() && snapshot_state.exists() && snapshot_mem.exists() {
                    let template = Template {
                        id: id.clone(),
                        name: id.clone(),
                        kernel_path: kernel,
                        rootfs_path: rootfs,
                        snapshot_state_path: snapshot_state,
                        snapshot_mem_path: snapshot_mem,
                        kernel_args: "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/coffer-init".into(),
                        vcpus: 1,
                        memory_mib: 256,
                        metadata: HashMap::new(),
                    };
                    self.templates.write().insert(id, template);
                }
            }
        }
        Ok(())
    }
}

fn spawn_firecracker(
    fc_path: &Path,
    socket_path: &Path,
    log_path: &Path,
) -> Result<tokio::process::Child> {
    use tokio::process::Command;
    let child = Command::new(fc_path)
        .arg("--api-sock").arg(socket_path)
        .arg("--log-path").arg(log_path)
        .arg("--level").arg("Warn")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(child)
}

async fn wait_for_socket(path: &Path, timeout: Duration) -> Result<()> {
    let start = std::time::Instant::now();
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
