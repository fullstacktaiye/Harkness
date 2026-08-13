//! Production tools shipped with the Harkness runtime.
//!
//! A tool stays independently invocable: registration needs no agent or
//! coordinator, and execution needs only the ordinary registry and execution
//! context. Policy and scheduling consume the descriptors but remain outside
//! the tool bodies.

mod fs_apply_patch;
mod process_exec;

pub use fs_apply_patch::{
    ApplyPatchInput, ApplyPatchOutput, FileBase, FileChangeKind, FileChangeSummary, FsApplyPatch,
};
pub use process_exec::{
    BoundedText, ProcessExec, ProcessExecInput, ProcessExecOutput, TestRun, TestRunInput,
    TestRunOutput,
};

use crate::tool::{RegistryError, ToolRegistry};

/// Registers the workspace-mutating and process tools from issue 95.
///
/// All three identities are published at `1.0.0`. The function is intentionally
/// separate from the read-only tool set so front ends can assemble a registry
/// explicitly while the two issue tracks land independently.
///
/// # Errors
///
/// Returns the first schema, metadata, or duplicate-registration refusal.
pub fn register_mutating_tools(registry: &mut ToolRegistry) -> Result<(), RegistryError> {
    registry.register(FsApplyPatch)?;
    registry.register(ProcessExec)?;
    registry.register(TestRun)?;
    Ok(())
}

#[cfg(test)]
mod tests;
