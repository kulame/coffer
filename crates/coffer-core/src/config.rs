use std::path::PathBuf;

/// Global runtime configuration.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Directory for template storage.
    pub template_dir: PathBuf,
    /// Directory for runtime sockets and logs.
    pub socket_dir: PathBuf,
    /// Firecracker binary path.
    pub firecracker_path: PathBuf,
    /// Jailer binary path (optional, for seccomp isolation).
    pub jailer_path: Option<PathBuf>,
    /// Path to the default guest kernel.
    pub kernel_path: PathBuf,
    /// Path to the coffer-agent binary (injected into rootfs).
    pub agent_bin: PathBuf,
    /// Warm pool configuration.
    pub pool: PoolConfig,
    /// Network configuration.
    pub network: NetworkConfig,
    /// Jailer configuration (optional).
    pub jailer: Option<JailerConfig>,
    /// Resource limits.
    pub limits: ResourceLimits,
    /// VMM base configuration.
    pub vmm: VmmConfig,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        let coffer_home = home.join(".coffer");
        Self {
            template_dir: coffer_home.join("templates"),
            socket_dir: coffer_home.join("run"),
            firecracker_path: coffer_home.join("kernel/firecracker"),
            jailer_path: Some(coffer_home.join("kernel/jailer")),
            kernel_path: coffer_home.join("kernel/vmlinux"),
            agent_bin: coffer_home.join("bin/coffer-agent"),
            pool: PoolConfig::default(),
            network: NetworkConfig::default(),
            jailer: None,
            limits: ResourceLimits::default(),
            vmm: VmmConfig::default(),
        }
    }
}

/// Warm pool configuration.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Number of warm instances to maintain per template.
    pub warm_pool_size: usize,
    /// Max total sandboxes.
    pub max_sandboxes: usize,
    /// Timeout for cold start (ms).
    pub cold_start_timeout_ms: u64,
    /// Interval to recycle stale warm VMs (secs).
    pub recycle_interval_secs: u64,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            warm_pool_size: 4,
            max_sandboxes: 512,
            cold_start_timeout_ms: 150,
            recycle_interval_secs: 300,
        }
    }
}

/// Network configuration.
#[derive(Debug, Clone)]
pub struct NetworkConfig {
    /// Bridge device name.
    pub bridge_name: String,
    /// Subnet CIDR for sandboxes.
    pub subnet: String,
    /// TAP device name prefix.
    pub tap_prefix: String,
    /// Gateway IP (host side).
    pub gateway_ip: String,
    /// MTU.
    pub mtu: u32,
    /// Enable outbound SNAT.
    pub enable_snat: bool,
    /// Default outbound policy.
    pub default_policy: OutboundPolicy,
    /// Explicit allow list (CIDRs).
    pub allow_list: Vec<String>,
    /// Explicit deny list (CIDRs).
    pub deny_list: Vec<String>,
    /// Per-sandbox bandwidth limit (Mbps, 0 = unlimited).
    pub bandwidth_mbps: u32,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            bridge_name: "br-coffer".into(),
            subnet: "192.168.100.0/24".into(),
            tap_prefix: "tap".into(),
            gateway_ip: "192.168.100.1".into(),
            mtu: 1500,
            enable_snat: true,
            default_policy: OutboundPolicy::Allow,
            allow_list: vec![],
            deny_list: vec![
                "10.0.0.0/8".into(),
                "172.16.0.0/12".into(),
                "192.168.0.0/16".into(),
                "127.0.0.0/8".into(),
                "169.254.0.0/16".into(),
            ],
            bandwidth_mbps: 0,
        }
    }
}

/// Jailer isolation configuration.
#[derive(Debug, Clone)]
pub struct JailerConfig {
    pub chroot_base_dir: PathBuf,
    pub uid: u32,
    pub gid: u32,
    pub netns: Option<String>,
    pub daemonize: bool,
    pub new_pid_ns: bool,
}

impl Default for JailerConfig {
    fn default() -> Self {
        Self {
            chroot_base_dir: PathBuf::from("/srv/jailer"),
            uid: 1000,
            gid: 1000,
            netns: None,
            daemonize: false,
            new_pid_ns: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundPolicy {
    Allow,
    Deny,
}

/// Global resource limits.
#[derive(Debug, Clone)]
pub struct ResourceLimits {
    /// Max total sandboxes across all templates.
    pub max_total_instances: usize,
    /// Max total guest memory (MiB).
    pub max_total_memory_mib: u64,
    /// Max total vCPUs.
    pub max_total_vcpus: u32,
    /// Max concurrent creations.
    pub max_concurrent_creations: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_total_instances: 512,
            max_total_memory_mib: 65536, // 64 GB
            max_total_vcpus: 256,
            max_concurrent_creations: 8,
        }
    }
}

/// Per-VMM base configuration.
#[derive(Debug, Clone)]
pub struct VmmConfig {
    /// Default vCPUs per sandbox.
    pub default_vcpus: u32,
    /// Default memory (MiB) per sandbox.
    pub default_memory_mib: u32,
    /// Enable SMT.
    pub smt: bool,
    /// Enable dirty page tracking for diff snapshots.
    pub track_dirty_pages: bool,
    /// Enable huge pages.
    pub huge_pages: bool,
}

impl Default for VmmConfig {
    fn default() -> Self {
        Self {
            default_vcpus: 1,
            default_memory_mib: 256,
            smt: false,
            track_dirty_pages: true,
            huge_pages: false,
        }
    }
}
