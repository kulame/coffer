//! Global resource scheduler for limiting total memory, vCPUs, and instances.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::ResourceLimits;
use crate::error::{CofferError, Result};

/// Tracks global resource usage across all pools.
pub struct ResourceScheduler {
    limits: ResourceLimits,
    total_instances: AtomicU64,
    total_memory_mib: AtomicU64,
    total_vcpus: AtomicU64,
}

impl ResourceScheduler {
    pub fn new(limits: ResourceLimits) -> Self {
        Self {
            limits,
            total_instances: AtomicU64::new(0),
            total_memory_mib: AtomicU64::new(0),
            total_vcpus: AtomicU64::new(0),
        }
    }

    /// Try to reserve resources for a new instance.
    pub async fn reserve(&self, memory_mib: u64, vcpus: u32) -> Result<()> {
        let new_instances = self.total_instances.load(Ordering::Relaxed) + 1;
        if new_instances > self.limits.max_total_instances as u64 {
            return Err(CofferError::ResourceLimit {
                resource: "instances".into(),
                used: new_instances - 1,
                limit: self.limits.max_total_instances as u64,
            });
        }

        let new_memory = self.total_memory_mib.load(Ordering::Relaxed) + memory_mib;
        if new_memory > self.limits.max_total_memory_mib {
            return Err(CofferError::ResourceLimit {
                resource: "memory_mib".into(),
                used: new_memory - memory_mib,
                limit: self.limits.max_total_memory_mib,
            });
        }

        let new_vcpus = self.total_vcpus.load(Ordering::Relaxed) + vcpus as u64;
        if new_vcpus > self.limits.max_total_vcpus as u64 {
            return Err(CofferError::ResourceLimit {
                resource: "vcpus".into(),
                used: new_vcpus - vcpus as u64,
                limit: self.limits.max_total_vcpus as u64,
            });
        }

        self.total_instances.fetch_add(1, Ordering::Relaxed);
        self.total_memory_mib.fetch_add(memory_mib, Ordering::Relaxed);
        self.total_vcpus.fetch_add(vcpus as u64, Ordering::Relaxed);

        Ok(())
    }

    /// Reserve with a memory-only check (for pool pre-warming estimates).
    pub async fn reserve_memory(&self, estimated_mib: u64) -> Result<()> {
        let new_memory = self.total_memory_mib.load(Ordering::Relaxed) + estimated_mib;
        if new_memory > self.limits.max_total_memory_mib {
            return Err(CofferError::ResourceLimit {
                resource: "memory_mib".into(),
                used: new_memory - estimated_mib,
                limit: self.limits.max_total_memory_mib,
            });
        }
        Ok(())
    }

    /// Release resources when an instance is destroyed.
    pub fn release(&self, memory_mib: u64, vcpus: u32) {
        self.total_instances.fetch_sub(1, Ordering::Relaxed);
        self.total_memory_mib.fetch_sub(memory_mib, Ordering::Relaxed);
        self.total_vcpus.fetch_sub(vcpus as u64, Ordering::Relaxed);
    }

    /// Current usage snapshot.
    pub fn usage(&self) -> Usage {
        Usage {
            instances: self.total_instances.load(Ordering::Relaxed),
            memory_mib: self.total_memory_mib.load(Ordering::Relaxed),
            vcpus: self.total_vcpus.load(Ordering::Relaxed) as u32,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Usage {
    pub instances: u64,
    pub memory_mib: u64,
    pub vcpus: u32,
}
