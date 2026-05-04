//! VM lifecycle state machine.

use std::fmt;

/// The lifecycle of a Firecracker microVM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmLifecycle {
    /// Just created, no config applied yet.
    Unconfigured,
    /// Configured but not booted.
    Configured,
    /// Booted and running.
    Running,
    /// Paused (vCPUs frozen).
    Paused,
    /// Snapshot created while paused.
    SnapshotCreated,
    /// Snapshot loaded, waiting to resume.
    SnapshotLoaded,
    /// Shut down or crashed.
    Exited,
}

impl fmt::Display for VmLifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmLifecycle::Unconfigured => write!(f, "unconfigured"),
            VmLifecycle::Configured => write!(f, "configured"),
            VmLifecycle::Running => write!(f, "running"),
            VmLifecycle::Paused => write!(f, "paused"),
            VmLifecycle::SnapshotCreated => write!(f, "snapshot_created"),
            VmLifecycle::SnapshotLoaded => write!(f, "snapshot_loaded"),
            VmLifecycle::Exited => write!(f, "exited"),
        }
    }
}

impl VmLifecycle {
    /// Valid state transitions.
    pub fn can_transition_to(&self, next: VmLifecycle) -> bool {
        use VmLifecycle::*;
        matches!(
            (self, next),
            (Unconfigured, Configured)
                | (Configured, Running)
                | (Running, Paused)
                | (Paused, SnapshotCreated)
                | (Paused, Running)
                | (SnapshotCreated, Running)
                | (SnapshotCreated, SnapshotLoaded)
                | (SnapshotLoaded, Running)
                | (Running, Exited)
                | (Paused, Exited)
                | (Configured, Exited)
        )
    }
}

/// Guard that ensures state transitions are valid.
pub struct StateMachine {
    state: VmLifecycle,
}

impl StateMachine {
    pub fn new() -> Self {
        Self {
            state: VmLifecycle::Unconfigured,
        }
    }

    pub fn state(&self) -> VmLifecycle {
        self.state
    }

    pub fn transition(&mut self, next: VmLifecycle) -> crate::error::Result<()> {
        if self.state.can_transition_to(next) {
            tracing::debug!("VM state: {} -> {}", self.state, next);
            self.state = next;
            Ok(())
        } else {
            Err(crate::error::CofferError::InvalidVmState {
                id: "unknown".into(),
                expected: format!("transition from {} to valid state", self.state),
                actual: next.to_string(),
            })
        }
    }
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}
