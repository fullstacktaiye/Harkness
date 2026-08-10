//! Run-domain records and their lifecycle state machines.
//!
//! A task identifies work in one workspace. Each attempt is a run, a run is
//! divided into ordered steps, and a step contains tool calls. Relationships
//! are stored as typed IDs so the persistence layer can keep the records
//! normalized without weakening their containment contract.
//!
//! Run and step transitions share one table:
//!
//! - `queued -> running | failed | cancelled | interrupted`
//! - `running -> waiting_for_approval | succeeded | failed | cancelled | interrupted`
//! - `waiting_for_approval -> running | failed | cancelled | interrupted`
//! - terminal states have no outgoing transitions
//!
//! Tool calls use a separate table:
//!
//! - `pending -> awaiting_approval | running | failed | denied | cancelled | interrupted`
//! - `awaiting_approval -> running | denied | cancelled | interrupted`
//! - `running -> succeeded | failed | cancelled | interrupted`
//! - terminal states have no outgoing transitions
//!
//! The public record fields are exposed through accessors. Fresh constructors
//! create only `queued` runs/steps and `pending` calls. Table-checked transition
//! methods are the only public state mutators; outcome-specific methods attach
//! failure details, tool output, and approval audit records atomically with
//! their transitions.
//!
//! Every durable record carries a schema version. Deserialization probes that
//! version before parsing the strict body, so a future schema produces an
//! actionable upgrade error instead of looking corrupt. Serialization uses
//! borrowing `*WireRef` types to avoid cloning tool input and output values.

mod error;
mod id;
mod record;
mod state;
mod wire;

pub use error::{InvalidTransition, RunDomainError};
pub use id::{RunId, StepId, TaskId, ToolCallId};
pub use record::{Approval, ApprovalDecision, Failure, Run, Step, Task, ToolCall};
pub use state::{EXECUTION_TRANSITIONS, ExecutionState, TOOL_CALL_TRANSITIONS, ToolCallState};
pub use wire::{
    MINIMUM_RUNTIME_RECORD_SCHEMA_VERSION, RUNTIME_RECORD_SCHEMA_VERSION, RunWire, RunWireRef,
    StepWire, StepWireRef, TaskWire, TaskWireRef, ToolCallWire, ToolCallWireRef,
    validate_record_schema_version,
};
