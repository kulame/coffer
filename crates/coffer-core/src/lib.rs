//! Coffer — fast, secure MicroVM runtime for AI Agents.
//!
//! Coffer replaces `regbox` with a Firecracker-based MicroVM runtime
//! designed for high density (500+/node) and low latency (<50ms warm,
//! <150ms cold).
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use coffer_core::{Runtime, RuntimeConfig};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let config = RuntimeConfig::default();
//!     let runtime = Runtime::new(config).await?;
//!     let handle = runtime.acquire("python-3.11").await?;
//!     let output = handle.sandbox().exec(
//!         &["python3", "-c", "print('hello')"],
//!         &std::collections::HashMap::new(),
//!         5000,
//!     ).await?;
//!     println!("{}", output.stdout);
//!     drop(handle); // returns to warm pool
//!     Ok(())
//! }
//! ```

pub mod config;
pub mod error;
pub mod firecracker;
pub mod net;
pub mod pool;
pub mod protocol;
pub mod runtime;
pub mod template;

pub use config::RuntimeConfig;
pub use error::{CofferError, Result};
pub use runtime::{Runtime, Sandbox, SandboxHandle};
pub use template::{Template, TemplateManager, TemplateSpec};

/// Re-export protocol types for convenience.
pub mod proto {
    pub use coffer_protocol::*;
}

/// Attempt to raise CAP_NET_ADMIN into the ambient capability set.
///
/// When the coffer-cli binary has file capabilities (`cap_net_admin=eip`),
/// the effective capability is present but is **not** automatically inherited
/// by child processes launched via `exec`.  Raising the capability into the
/// ambient set solves this, allowing external tools such as `ip` and
/// `iptables` to run without `sudo`.
pub fn ensure_cap_net_admin_ambient() {
    const PR_CAP_AMBIENT: libc::c_int = 47;
    const PR_CAP_AMBIENT_RAISE: libc::c_int = 2;
    const CAP_NET_ADMIN: libc::c_int = 12;

    // Safety: prctl with these arguments is a well-defined Linux syscall.
    let rc = unsafe { libc::prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, CAP_NET_ADMIN, 0, 0) };
    if rc == -1 {
        let err = std::io::Error::last_os_error();
        // EINVAL → kernel too old for ambient caps.
        // EPERM  → cap not in permitted/inheritable set (e.g. no file caps).
        // Both are fine to ignore — the caller may be root or use sudo.
        tracing::debug!("Could not raise CAP_NET_ADMIN ambient: {}", err);
    } else {
        tracing::debug!("Raised CAP_NET_ADMIN into ambient capability set");
    }
}
