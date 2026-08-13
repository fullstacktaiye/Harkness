//! The decision seam between an agent and the runtime coordinator.
//!
//! An [`Agent`] receives one redacted, bounded [`Observation`] at a time and
//! returns one [`AgentAction`]. An action may *request* a tool invocation, but
//! this module cannot resolve or execute it: registry lookup, policy,
//! approvals, persistence, scheduling, and cancellation all remain on the
//! coordinator side of the seam.
//!
//! [`MockAgent`] is the first implementation. It deterministically replays a
//! versioned [`Scenario`], matching observations structurally and returning a
//! typed `scenario_divergence` failure when reality departs from the script.
//! It has no privileged callback or testing-only execution path; a coordinator
//! drives it through [`Agent::next_action`] exactly as it will drive a future
//! model-backed implementation.
//!
//! The plain action and observation enums are also embedded inside versioned
//! scenario definitions. When a coordinator makes one durable on its own, it
//! uses [`AgentActionRecord`] or [`ObservationRecord`], whose schema version is
//! probed before the strict enum body.

mod mock;
mod scenario;
mod session;
mod types;

pub use mock::MockAgent;
pub use scenario::{
    MAX_SCENARIO_BYTES, MAX_SCENARIO_STEPS, ObservationPattern, SCENARIO_FIXTURE_VERSION, Scenario,
    ScenarioError, ScenarioId, ScenarioStep,
};
pub use session::{AGENT_SESSION_STATE_SCHEMA_VERSION, AgentSessionId, AgentSessionState};
pub use types::{
    AGENT_TURN_RECORD_SCHEMA_VERSION, AgentAction, AgentActionRecord, AgentFailure,
    ApprovalOutcomeView, Observation, ObservationKind, ObservationRecord, PlannedStep,
    RedactedText, TaskRef, ToolErrorView, ToolResultView, WorkspaceRef,
};

/// A decision-maker owned and driven by one run coordinator.
///
/// Implementations are synchronous and single-threaded per run. They receive
/// plain data and return plain data; none is given a store, registry, policy,
/// approval, execution, or scheduling handle. `Send` lets the coordinator own
/// an agent on its run worker without requiring agents to synchronize their
/// internal state.
pub trait Agent: Send {
    /// Stable identity of this agent session.
    fn session_id(&self) -> AgentSessionId;

    /// Consumes one coordinator observation and chooses the next action.
    ///
    /// A coordinator persists the result through [`AgentActionRecord`], never
    /// as an unversioned standalone enum value.
    fn next_action(&mut self, observation: Observation) -> AgentAction;

    /// Returns the serializable checkpoint needed to inspect or resume the
    /// session after a process restart. When stored as a generic run-event
    /// payload, use [`AgentSessionState::to_event_payload`] so text redaction
    /// cannot rewrite checkpoint control fields.
    fn state(&self) -> AgentSessionState;
}
