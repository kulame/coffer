//! OCI image → EROFS rootfs → snapshot builder.

use std::path::PathBuf;

use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::error::{CofferError, Result};

/// Output of a successful image build.
#[derive(Debug, Clone)]
pub struct BuildOutput {
    pub rootfs_path: PathBuf,
    pub kernel_path: PathBuf,
    pub kernel_args: String,
}

/// Builder that turns an OCI container image into an EROFS rootfs suitable
/// for Firecracker MicroVMs.
pub struct ImageBuilder {
    work_dir: PathBuf,
    image: String,
    kernel_path: PathBuf,
    kernel_args: String,
    agent_bin: Option<PathBuf>,
    enable_overlay: bool,
}

impl ImageBuilder {
    pub fn new(
        work_dir: PathBuf,
        image: String,
        kernel_path: PathBuf,
    ) -> Self {
        Self {
            work_dir,
            image,
            kernel_path,
            kernel_args: "console=ttyS0 reboot=k panic=1 pci=off".into(),
            agent_bin: None,
            enable_overlay: true,
        }
    }

    pub fn with_kernel_args(mut self, args: String) -> Self {
        self.kernel_args = args;
        self
    }

    pub fn with_agent_bin(mut self, path: PathBuf) -> Self {
        self.agent_bin = Some(path);
        self
    }

    pub fn with_overlay(mut self, enable: bool) -> Self {
        self.enable_overlay = enable;
        self
    }

    /// Run the full build pipeline:
    /// 1. skopeo copy
    /// 2. umoci unpack
    /// 3. mkfs.erofs
    /// 4. inject coffer-init (overlay) + agentlet binary
    pub async fn build(self) -> Result<BuildOutput> {
        std::fs::create_dir_all(&self.work_dir)?;

        info!(image = %self.image, work_dir = %self.work_dir.display(), "Starting image build");

        // 1. Pull OCI image.
        self.pull_image().await?;

        // 2. Unpack to rootfs directory.
        self.unpack_rootfs().await?;

        // 3. Inject overlay init and agent binary.
        self.inject_overlay_init().await?;
        self.inject_agent().await?;

        // 4. Build EROFS image.
        self.create_erofs().await?;

        info!(rootfs = %self.work_dir.join("rootfs.erofs").display(), "Image build complete");

        Ok(BuildOutput {
            rootfs_path: self.work_dir.join("rootfs.erofs"),
            kernel_path: self.kernel_path.clone(),
            kernel_args: self.kernel_args.clone(),
        })
    }

    // ------------------------------------------------------------------
    // Pipeline steps
    // ------------------------------------------------------------------

    async fn pull_image(&self) -> Result<()> {
        let oci_dir = self.work_dir.join("oci");
        std::fs::create_dir_all(&oci_dir)?;

        info!(image = %self.image, "Pulling OCI image with skopeo");

        let status = Command::new("skopeo")
            .args([
                "copy",
                &format!("docker://{}", self.image),
                &format!("oci:{}:latest", oci_dir.display()),
            ])
            .status()
            .await
            .map_err(|e| CofferError::TemplateBuild(format!(
                "skopeo not found or failed to start: {}. Please install skopeo.", e
            )))?;

        if !status.success() {
            return Err(CofferError::TemplateBuild("skopeo copy failed".into()));
        }
        Ok(())
    }

    async fn unpack_rootfs(&self) -> Result<()> {
        let oci_dir = self.work_dir.join("oci");
        let rootfs_dir = self.work_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs_dir)?;

        info!("Unpacking OCI image with umoci");

        let status = Command::new("umoci")
            .args([
                "unpack",
                "--image",
                &format!("{}:latest", oci_dir.display()),
                &rootfs_dir.display().to_string(),
            ])
            .status()
            .await
            .map_err(|e| CofferError::TemplateBuild(format!(
                "umoci not found or failed to start: {}. Please install umoci.", e
            )))?;

        if !status.success() {
            return Err(CofferError::TemplateBuild("umoci unpack failed".into()));
        }
        Ok(())
    }

    async fn create_erofs(&self) -> Result<()> {
        let rootfs_dir = self.work_dir.join("rootfs");
        let erofs_path = self.work_dir.join("rootfs.erofs");

        info!("Creating EROFS image with mkfs.erofs");

        let status = Command::new("mkfs.erofs")
            .args([
                "-zlz4",
                &erofs_path.display().to_string(),
                &rootfs_dir.display().to_string(),
            ])
            .status()
            .await
            .map_err(|e| CofferError::TemplateBuild(format!(
                "mkfs.erofs not found or failed to start: {}. Please install erofs-utils.", e
            )))?;

        if !status.success() {
            return Err(CofferError::TemplateBuild("mkfs.erofs failed".into()));
        }

        // Clean up unpacked rootfs to save space.
        let _ = tokio::fs::remove_dir_all(&rootfs_dir).await;
        Ok(())
    }

    async fn inject_overlay_init(&self) -> Result<()> {
        if !self.enable_overlay {
            return Ok(());
        }

        let rootfs_dir = self.work_dir.join("rootfs");
        let init_path = rootfs_dir.join("sbin/coffer-init");
        std::fs::create_dir_all(init_path.parent().unwrap())?;

        info!("Injecting coffer-init for overlay rootfs");

        // Write the init script that sets up overlayfs in-place.
        let init_script = r#"#!/bin/sh
# Coffer init — sets up tmpfs-backed overlay on top of EROFS rootfs.
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs dev /dev 2>/dev/null

mkdir -p /newroot/overlay/upper /newroot/overlay/work
mount -t tmpfs -o size=128M tmpfs /newroot/overlay
mount -t overlay overlay -o lowerdir=/,upperdir=/newroot/overlay/upper,workdir=/newroot/overlay/work /newroot

cd /newroot
mkdir -p oldroot
pivot_root . oldroot

# Move essential virtual filesystems
mount --move /oldroot/proc /proc 2>/dev/null
mount --move /oldroot/sys /sys 2>/dev/null
mount --move /oldroot/dev /dev 2>/dev/null

exec chroot . /sbin/init "$@"
"#;

        tokio::fs::write(&init_path, init_script).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&init_path, std::fs::Permissions::from_mode(0o755))?;
        }

        // Update kernel args to use our init.
        // We append init= so that the guest kernel runs coffer-init first.
        // (builder caller can override kernel_args if desired)
        debug!("Overlay init injected at /sbin/coffer-init");
        Ok(())
    }

    async fn inject_agent(&self) -> Result<()> {
        let Some(ref agent_bin) = self.agent_bin else {
            return Ok(());
        };

        if !agent_bin.exists() {
            warn!(path = %agent_bin.display(), "Agent binary not found, skipping injection");
            return Ok(());
        }

        let rootfs_dir = self.work_dir.join("rootfs");
        let target = rootfs_dir.join("usr/local/bin/coffer-agent");
        std::fs::create_dir_all(target.parent().unwrap())?;

        info!(src = %agent_bin.display(), dst = %target.display(), "Injecting agent binary");
        tokio::fs::copy(agent_bin, target).await?;
        Ok(())
    }
}
