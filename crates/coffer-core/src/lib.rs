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
