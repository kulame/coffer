//! Host-side network isolation: bridge, TAP, iptables SNAT, egress policy.

use dashmap::DashMap;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::error::{CofferError, Result};

mod raw;

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
    allocation_lock: tokio::sync::Mutex<()>,
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
            allocation_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Ensure the bridge exists.  Called lazily from `allocate_tap`.
    pub async fn setup_bridge(&self) -> Result<()> {
        if self.bridge_setup.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        if let Err(e) = self.do_setup_bridge().await {
            // Reset flag so the next caller can retry (e.g. after gaining root).
            self.bridge_setup.store(false, std::sync::atomic::Ordering::SeqCst);
            return Err(e);
        }
        Ok(())
    }

    async fn do_setup_bridge(&self) -> Result<()> {
        let bridge = &self.config.bridge_name;

        // Create bridge (idempotent — succeeds if it already exists).
        match raw::create_bridge(bridge) {
            Ok(()) => {
                info!(bridge, "Created bridge");
            }
            Err(CofferError::Network(ref msg)) if msg.contains("File exists") => {
                // already exists
            }
            Err(e) => return Err(e),
        }

        raw::set_link_up(bridge)?;
        raw::add_ip_to_interface(bridge, &self.config.subnet)?;

        // Enable IP forwarding.
        let _ = tokio::fs::write("/proc/sys/net/ipv4/ip_forward", b"1\n").await;

        // Setup SNAT for bridge subnet.
        let snat_rule = format!("POSTROUTING -s {} ! -o {} -j MASQUERADE", self.config.subnet, bridge);
        if !iptables_has_rule("nat", &snat_rule).await.unwrap_or(false) {
            if let Err(e) = run_cmd_with_sudo_fallback("iptables", &["-t", "nat", "-A", "POSTROUTING", "-s", &self.config.subnet, "!", "-o", bridge, "-j", "MASQUERADE"]).await {
                warn!("iptables SNAT setup failed (guest outbound NAT may not work): {}", e);
            }
        }

        Ok(())
    }

    /// Allocate a TAP device for a VM.
    pub async fn allocate_tap(&self, vm_id: &str) -> Result<String> {
        let _guard = self.allocation_lock.lock().await;
        self.setup_bridge().await?;
        // Linux interface names are limited to IFNAMSIZ-1 = 15 chars.
        let tap_name = format!("{}{}", self.config.tap_prefix, vm_id);
        let tap_name = if tap_name.len() > 15 {
            tap_name[..15].to_string()
        } else {
            tap_name
        };
        let bridge = &self.config.bridge_name;

        // Create TAP via ioctl so that our effective CAP_NET_ADMIN is used
        // directly — no capability drop across fork+exec.
        raw::create_tap(&tap_name)?;
        raw::set_link_up(&tap_name)?;
        raw::add_to_bridge(&tap_name, bridge)?;

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
            let bridge = &self.config.bridge_name;
            let _ = raw::remove_from_bridge(&tap.name, bridge);
            let _ = raw::set_link_down(&tap.name);
            let _ = raw::delete_tap(&tap.name);
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
        let _ = run_cmd_with_sudo_fallback("iptables", &["-N", &chain]).await;
        let _ = run_cmd_with_sudo_fallback("iptables", &["-F", &chain]).await;

        // Jump from FORWARD.
        let jump_rule = format!("FORWARD -i {} -j {}", tap_name, chain);
        if !iptables_has_rule("filter", &jump_rule).await? {
            run_cmd_with_sudo_fallback("iptables", &["-A", "FORWARD", "-i", tap_name, "-j", &chain]).await?;
        }

        // Default drop.
        run_cmd_with_sudo_fallback("iptables", &["-A", &chain, "-j", "DROP"]).await?;

        // Allowlist.
        for addr in allowlist {
            run_cmd_with_sudo_fallback("iptables", &["-I", &chain, "1", "-d", addr, "-j", "ACCEPT"]).await?;
        }

        // Denylist (insert before default drop).
        for addr in denylist {
            run_cmd_with_sudo_fallback("iptables", &["-I", &chain, "1", "-d", addr, "-j", "DROP"]).await?;
        }

        // Allow established.
        run_cmd_with_sudo_fallback("iptables", &["-I", &chain, "1", "-m", "state", "--state", "ESTABLISHED,RELATED", "-j", "ACCEPT"]).await?;

        Ok(())
    }
}

async fn run_cmd_with_sudo_fallback(cmd: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(cmd).args(args).output().await?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        if err.contains("Operation not permitted") || err.contains("Permission denied") {
            let mut sudo_args = vec!["-n", cmd];
            sudo_args.extend(args);
            match tokio::time::timeout(
                std::time::Duration::from_secs(3),
                Command::new("sudo").args(&sudo_args).output(),
            ).await {
                Ok(Ok(sudo_out)) if sudo_out.status.success() => return Ok(()),
                Ok(Ok(sudo_out)) => {
                    let sudo_err = String::from_utf8_lossy(&sudo_out.stderr);
                    return Err(CofferError::Network(format!(
                        "{} {:?} failed: {} (sudo -n fallback: {})",
                        cmd, args, err.trim(), sudo_err.trim()
                    )));
                }
                Ok(Err(e)) => {
                    return Err(CofferError::Network(format!(
                        "{} {:?} failed: {} (sudo spawn error: {})",
                        cmd, args, err.trim(), e
                    )));
                }
                Err(_) => {
                    return Err(CofferError::Network(format!(
                        "{} {:?} failed: {} (sudo timed out after 3s — passwordless sudo required)",
                        cmd, args, err.trim()
                    )));
                }
            }
        }
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
