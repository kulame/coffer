//! Host-side network isolation: bridge, TAP, iptables SNAT, egress policy.

use dashmap::DashMap;
use tokio::process::Command;
use tracing::{debug, info};

use crate::error::{CofferError, Result};

#[derive(Debug, Clone)]
pub struct NetworkConfig {
    pub bridge_name: String,
    pub subnet: String,
    pub tap_prefix: String,
}

pub struct NetworkManager {
    config: NetworkConfig,
    allocated_taps: DashMap<String, TapDevice>,
    bridge_setup: std::sync::atomic::AtomicBool,
}

#[derive(Debug, Clone)]
pub struct TapDevice {
    pub name: String,
    pub vm_id: String,
}

impl NetworkManager {
    pub fn new(config: NetworkConfig) -> Self {
        Self {
            config,
            allocated_taps: DashMap::new(),
            bridge_setup: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Ensure the bridge exists.
    pub async fn setup_bridge(&self) -> Result<()> {
        if self.bridge_setup.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }

        let bridge = &self.config.bridge_name;

        // Check if bridge exists.
        let exists = Command::new("ip")
            .args(["link", "show", bridge])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);

        if !exists {
            info!(bridge, "Creating bridge");
            run_cmd("ip", &["link", "add", bridge, "type", "bridge"]).await?;
            run_cmd("ip", &["link", "set", bridge, "up"]).await?;
            run_cmd("ip", &["addr", "add", &self.config.subnet, "dev", bridge]).await?;
        }

        // Enable IP forwarding.
        let _ = tokio::fs::write("/proc/sys/net/ipv4/ip_forward", b"1\n").await;

        // Setup SNAT for bridge subnet.
        let snat_rule = format!("POSTROUTING -s {} ! -o {} -j MASQUERADE", self.config.subnet, bridge);
        if !iptables_has_rule("nat", &snat_rule).await? {
            run_cmd("iptables", &["-t", "nat", "-A", "POSTROUTING", "-s", &self.config.subnet, "!", "-o", bridge, "-j", "MASQUERADE"]).await?;
        }

        Ok(())
    }

    /// Allocate a TAP device for a VM.
    pub async fn allocate_tap(&self, vm_id: &str) -> Result<String> {
        let tap_name = format!("{}{:.6}", self.config.tap_prefix, vm_id);
        let bridge = &self.config.bridge_name;

        // Create TAP.
        let create = Command::new("ip")
            .args(["tuntap", "add", &tap_name, "mode", "tap"])
            .output()
            .await?;
        if !create.status.success() {
            let err = String::from_utf8_lossy(&create.stderr);
            if !err.contains("File exists") {
                return Err(CofferError::Network(format!("Failed to create TAP {}: {}", tap_name, err)));
            }
        }

        // Bring up and attach to bridge.
        run_cmd("ip", &["link", "set", &tap_name, "up"]).await?;
        run_cmd("ip", &["link", "set", &tap_name, "master", bridge]).await?;

        self.allocated_taps.insert(vm_id.to_string(), TapDevice {
            name: tap_name.clone(),
            vm_id: vm_id.to_string(),
        });

        debug!(%tap_name, %vm_id, "Allocated TAP");
        Ok(tap_name)
    }

    /// Deallocate a TAP device.
    pub async fn deallocate_tap(&self, vm_id: &str) -> Result<()> {
        if let Some((_, tap)) = self.allocated_taps.remove(vm_id) {
            let _ = run_cmd("ip", &["link", "set", &tap.name, "down"]).await;
            let _ = run_cmd("ip", &["link", "delete", &tap.name]).await;
            debug!(tap = %tap.name, %vm_id, "Deallocated TAP");
        }
        Ok(())
    }

    /// Apply egress firewall rules for a sandbox.
    pub async fn apply_egress_policy(
        &self,
        tap_name: &str,
        allowlist: &[String],
        denylist: &[String],
    ) -> Result<()> {
        let chain = format!("COFFER-{}", tap_name);

        // Create chain.
        let _ = run_cmd("iptables", &["-N", &chain]).await;
        let _ = run_cmd("iptables", &["-F", &chain]).await;

        // Jump from FORWARD.
        let jump_rule = format!("FORWARD -i {} -j {}", tap_name, chain);
        if !iptables_has_rule("filter", &jump_rule).await? {
            run_cmd("iptables", &["-A", "FORWARD", "-i", tap_name, "-j", &chain]).await?;
        }

        // Default drop.
        run_cmd("iptables", &["-A", &chain, "-j", "DROP"]).await?;

        // Allowlist.
        for addr in allowlist {
            run_cmd("iptables", &["-I", &chain, "1", "-d", addr, "-j", "ACCEPT"]).await?;
        }

        // Denylist (insert before default drop).
        for addr in denylist {
            run_cmd("iptables", &["-I", &chain, "1", "-d", addr, "-j", "DROP"]).await?;
        }

        // Allow established.
        run_cmd("iptables", &["-I", &chain, "1", "-m", "state", "--state", "ESTABLISHED,RELATED", "-j", "ACCEPT"]).await?;

        Ok(())
    }
}

async fn run_cmd(cmd: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(cmd).args(args).output().await?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(CofferError::Network(format!("{} {:?} failed: {}", cmd, args, err)));
    }
    Ok(())
}

async fn iptables_has_rule(table: &str, rule: &str) -> Result<bool> {
    let output = Command::new("iptables")
        .args(["-t", table, "-C"])
        .args(rule.split_whitespace())
        .output()
        .await?;
    Ok(output.status.success())
}
