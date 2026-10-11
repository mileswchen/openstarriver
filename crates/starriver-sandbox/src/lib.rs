//! OS sandbox backends for the kernel `SandboxProvider` seam.
//!
//! The kernel owns the vocabulary (modes / policy / errors / `ConfinedArgv`);
//! this crate owns the concrete runners — Seatbelt (macOS), bubblewrap +
//! Landlock (Linux), write-restricted token via `starriver-sbx-helper`
//! (Windows) — plus the cross-platform pure logic: workspace write
//! boundary, runner probe chain, and denial signature normalization.
//! `SystemSandbox` is the platform single point tools call.

pub mod boundary;
pub mod denial;
pub mod probe;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod seatbelt;
#[cfg(target_os = "windows")]
pub mod windows;

pub use boundary::{effective_workspace_root, ensure_workspace_writable};
pub use starriver_kernel::sandbox::{
    ConfinedArgv, SandboxEnforcement, SandboxError, SandboxMode, SandboxPolicy, SandboxProvider,
    SpawnHook,
};

/// Platform-selected sandbox backend — the single seam tools call
/// (`tools/bash.rs`). Backends keep their own availability probing (cached
/// per process) and fail closed with `SandboxError::Unavailable`.
pub struct SystemSandbox;

impl SystemSandbox {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SystemSandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxProvider for SystemSandbox {
    fn confine(
        &self,
        argv: Vec<String>,
        policy: &SandboxPolicy,
    ) -> Result<ConfinedArgv, SandboxError> {
        select(argv, policy)
    }
}

#[cfg(target_os = "macos")]
fn select(argv: Vec<String>, policy: &SandboxPolicy) -> Result<ConfinedArgv, SandboxError> {
    seatbelt::SeatbeltSandboxProvider::new().confine(argv, policy)
}

#[cfg(target_os = "linux")]
fn select(argv: Vec<String>, policy: &SandboxPolicy) -> Result<ConfinedArgv, SandboxError> {
    linux::LinuxSandbox::new().confine(argv, policy)
}

#[cfg(target_os = "windows")]
fn select(argv: Vec<String>, policy: &SandboxPolicy) -> Result<ConfinedArgv, SandboxError> {
    windows::WindowsSandbox::new().confine(argv, policy)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn select(argv: Vec<String>, policy: &SandboxPolicy) -> Result<ConfinedArgv, SandboxError> {
    let _ = argv;
    Err(SandboxError::Unavailable {
        mode: policy.mode,
        detail: "no sandbox backend on this platform".into(),
    })
}
