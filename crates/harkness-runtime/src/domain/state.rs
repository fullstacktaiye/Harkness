use serde::{Deserialize, Serialize};

/// Lifecycle state shared by runs and their ordered steps.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// The run or step exists but has not started.
    Queued,
    /// Work is actively being performed.
    Running,
    /// Execution is paused until a durable approval is decided.
    WaitingForApproval,
    /// Work completed successfully.
    Succeeded,
    /// Work completed with a failure.
    Failed,
    /// A user or coordinator cancelled the work.
    Cancelled,
    /// The owning process stopped before completing the work.
    Interrupted,
}

impl RunState {
    /// Every run-state value in its stable declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Queued,
        Self::Running,
        Self::WaitingForApproval,
        Self::Succeeded,
        Self::Failed,
        Self::Cancelled,
        Self::Interrupted,
    ];

    /// Returns the stable persisted spelling of this state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::WaitingForApproval => "waiting_for_approval",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    /// Whether no later state may follow this state.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }

    pub(super) const fn requires_started_at(self) -> bool {
        matches!(
            self,
            Self::Running | Self::WaitingForApproval | Self::Succeeded | Self::Failed
        )
    }

    pub(super) const fn forbids_started_at(self) -> bool {
        matches!(self, Self::Queued)
    }
}

impl std::fmt::Display for RunState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Every legal run/step state edge. An absent edge is invalid.
pub const RUN_TRANSITIONS: &[(RunState, RunState)] = &[
    (RunState::Queued, RunState::Running),
    (RunState::Queued, RunState::Cancelled),
    (RunState::Queued, RunState::Interrupted),
    (RunState::Running, RunState::WaitingForApproval),
    (RunState::Running, RunState::Succeeded),
    (RunState::Running, RunState::Failed),
    (RunState::Running, RunState::Cancelled),
    (RunState::Running, RunState::Interrupted),
    (RunState::WaitingForApproval, RunState::Running),
    (RunState::WaitingForApproval, RunState::Failed),
    (RunState::WaitingForApproval, RunState::Cancelled),
    (RunState::WaitingForApproval, RunState::Interrupted),
];

/// Lifecycle state of a requested tool invocation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallState {
    /// The invocation has been recorded but not evaluated or executed.
    Pending,
    /// Policy requires a durable approval before execution can begin.
    AwaitingApproval,
    /// The tool is actively executing.
    Running,
    /// The tool returned a successful result.
    Succeeded,
    /// The tool completed with a failure.
    Failed,
    /// Policy or a human decision refused the invocation before execution.
    Denied,
    /// A user or coordinator cancelled the invocation.
    Cancelled,
    /// The owning process stopped before the invocation completed.
    Interrupted,
}

impl ToolCallState {
    /// Every tool-call state value in its stable declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Pending,
        Self::AwaitingApproval,
        Self::Running,
        Self::Succeeded,
        Self::Failed,
        Self::Denied,
        Self::Cancelled,
        Self::Interrupted,
    ];

    /// Returns the stable persisted spelling of this state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Denied => "denied",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    /// Whether no later state may follow this state.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Denied | Self::Cancelled | Self::Interrupted
        )
    }

    pub(super) const fn requires_started_at(self) -> bool {
        matches!(self, Self::Running | Self::Succeeded | Self::Failed)
    }

    pub(super) const fn forbids_started_at(self) -> bool {
        matches!(self, Self::Pending | Self::AwaitingApproval | Self::Denied)
    }
}

impl std::fmt::Display for ToolCallState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Every legal tool-call state edge. An absent edge is invalid.
pub const TOOL_CALL_TRANSITIONS: &[(ToolCallState, ToolCallState)] = &[
    (ToolCallState::Pending, ToolCallState::AwaitingApproval),
    (ToolCallState::Pending, ToolCallState::Running),
    (ToolCallState::Pending, ToolCallState::Denied),
    (ToolCallState::Pending, ToolCallState::Cancelled),
    (ToolCallState::Pending, ToolCallState::Interrupted),
    (ToolCallState::AwaitingApproval, ToolCallState::Running),
    (ToolCallState::AwaitingApproval, ToolCallState::Denied),
    (ToolCallState::AwaitingApproval, ToolCallState::Cancelled),
    (ToolCallState::AwaitingApproval, ToolCallState::Interrupted),
    (ToolCallState::Running, ToolCallState::Succeeded),
    (ToolCallState::Running, ToolCallState::Failed),
    (ToolCallState::Running, ToolCallState::Cancelled),
    (ToolCallState::Running, ToolCallState::Interrupted),
];

#[cfg(test)]
mod tests {
    use super::{RUN_TRANSITIONS, RunState, TOOL_CALL_TRANSITIONS, ToolCallState};

    #[test]
    fn run_states_serialize_as_stable_snake_case_strings() {
        let fixtures = [
            (RunState::Queued, "queued"),
            (RunState::Running, "running"),
            (RunState::WaitingForApproval, "waiting_for_approval"),
            (RunState::Succeeded, "succeeded"),
            (RunState::Failed, "failed"),
            (RunState::Cancelled, "cancelled"),
            (RunState::Interrupted, "interrupted"),
        ];

        assert_eq!(
            fixtures.iter().map(|(state, _)| *state).collect::<Vec<_>>(),
            RunState::ALL
        );
        for (state, spelling) in fixtures {
            let json = format!("\"{spelling}\"");
            assert_eq!(state.as_str(), spelling);
            assert_eq!(state.to_string(), spelling);
            assert_eq!(serde_json::to_string(&state).unwrap(), json);
            assert_eq!(serde_json::from_str::<RunState>(&json).unwrap(), state);
        }
    }

    #[test]
    fn tool_call_states_serialize_as_stable_snake_case_strings() {
        let fixtures = [
            (ToolCallState::Pending, "pending"),
            (ToolCallState::AwaitingApproval, "awaiting_approval"),
            (ToolCallState::Running, "running"),
            (ToolCallState::Succeeded, "succeeded"),
            (ToolCallState::Failed, "failed"),
            (ToolCallState::Denied, "denied"),
            (ToolCallState::Cancelled, "cancelled"),
            (ToolCallState::Interrupted, "interrupted"),
        ];

        assert_eq!(
            fixtures.iter().map(|(state, _)| *state).collect::<Vec<_>>(),
            ToolCallState::ALL
        );
        for (state, spelling) in fixtures {
            let json = format!("\"{spelling}\"");
            assert_eq!(state.as_str(), spelling);
            assert_eq!(state.to_string(), spelling);
            assert_eq!(serde_json::to_string(&state).unwrap(), json);
            assert_eq!(serde_json::from_str::<ToolCallState>(&json).unwrap(), state);
        }
    }

    #[test]
    fn no_transition_leaves_a_terminal_state() {
        assert!(RUN_TRANSITIONS.iter().all(|(from, _)| !from.is_terminal()));
        assert!(
            TOOL_CALL_TRANSITIONS
                .iter()
                .all(|(from, _)| !from.is_terminal())
        );
    }
}
