use crate::approval::ApprovalId;
use crate::domain::RunId;
use crate::store::StoreError;

/// Stable failures of the shared run-coordination service.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RuntimeError {
    /// Durable state could not be read or changed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A schedulable workspace has no catalog identity.
    #[error(
        "task {task} has no project identity; scheduled work requires a stable workspace identity"
    )]
    WorkspaceIdentityRequired { task: crate::domain::TaskId },
    /// The supplied agent view does not describe the stored task workspace.
    #[error("the supplied workspace does not match task {task}")]
    WorkspaceMismatch { task: crate::domain::TaskId },
    /// The stored workspace root is unavailable or unsafe to schedule.
    #[error("the workspace for task {task} is unavailable: {reason}")]
    WorkspaceUnavailable {
        task: crate::domain::TaskId,
        reason: String,
    },
    /// The run worker could not be created.
    #[error("could not start worker for run {run}: {reason}")]
    WorkerSpawn { run: RunId, reason: String },
    /// Cancellation targeted a run this coordinator did not start.
    #[error("run {run} is not active in this coordinator")]
    RunNotActive { run: RunId },
    /// The approval was not found or did not belong to an active run.
    #[error("approval {approval} is not attached to an active run")]
    ApprovalNotActive { approval: ApprovalId },
}

impl RuntimeError {
    /// Every kind this namespace declares, in variant declaration order.
    ///
    /// `Store` is absent because it delegates to
    /// [`StoreError::KINDS`](crate::store::StoreError::KINDS) rather than
    /// naming a spelling of its own; the two tables are concatenated by the
    /// front ends that publish `exit_code_by_kind`, so they must not collide.
    pub const KINDS: [&'static str; 6] = [
        "workspace_identity_required",
        "workspace_mismatch",
        "workspace_unavailable",
        "worker_spawn_failed",
        "run_not_active",
        "approval_not_active",
    ];

    /// Stable machine-readable discriminant.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Store(error) => error.kind(),
            Self::WorkspaceIdentityRequired { .. } => "workspace_identity_required",
            Self::WorkspaceMismatch { .. } => "workspace_mismatch",
            Self::WorkspaceUnavailable { .. } => "workspace_unavailable",
            Self::WorkerSpawn { .. } => "worker_spawn_failed",
            Self::RunNotActive { .. } => "run_not_active",
            Self::ApprovalNotActive { .. } => "approval_not_active",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RuntimeError;
    use crate::approval::ApprovalId;
    use crate::domain::{RunId, TaskId};
    use crate::store::StoreError;

    #[test]
    fn runtime_error_kinds_round_trip_through_the_kinds_table() {
        let task = TaskId::new();
        let run = RunId::new();
        let declared = [
            RuntimeError::WorkspaceIdentityRequired { task },
            RuntimeError::WorkspaceMismatch { task },
            RuntimeError::WorkspaceUnavailable {
                task,
                reason: "gone".to_owned(),
            },
            RuntimeError::WorkerSpawn {
                run,
                reason: "no thread".to_owned(),
            },
            RuntimeError::RunNotActive { run },
            RuntimeError::ApprovalNotActive {
                approval: ApprovalId::new(),
            },
        ];
        assert_eq!(declared.len(), RuntimeError::KINDS.len());
        for (error, kind) in declared.iter().zip(RuntimeError::KINDS) {
            assert_eq!(error.kind(), kind);
        }
    }

    #[test]
    fn runtime_kinds_do_not_collide_with_the_store_namespace() {
        for kind in RuntimeError::KINDS {
            assert!(
                !StoreError::KINDS.contains(&kind),
                "{kind} is published by two namespaces whose tables are concatenated"
            );
        }
    }
}
