//! Run-domain records and their lifecycle state machines.
//!
//! A task identifies work in one workspace. Each attempt is a run, a run is
//! divided into ordered steps, and a step contains tool calls. Relationships
//! are stored as typed IDs so the persistence layer can keep the records
//! normalized without weakening their containment contract.
//!
//! Run and step transitions share one table:
//!
//! - `queued -> running | cancelled | interrupted`
//! - `running -> waiting_for_approval | succeeded | failed | cancelled | interrupted`
//! - `waiting_for_approval -> running | failed | cancelled | interrupted`
//! - terminal states have no outgoing transitions
//!
//! Tool calls use a separate table:
//!
//! - `pending -> awaiting_approval | running | denied | cancelled | interrupted`
//! - `awaiting_approval -> running | denied | cancelled | interrupted`
//! - `running -> succeeded | failed | cancelled | interrupted`
//! - terminal states have no outgoing transitions
//!
//! The public record fields are exposed through accessors. Fresh constructors
//! create only `queued` runs/steps and `pending` calls; [`Run::transition`],
//! [`Step::transition`], and [`ToolCall::transition`] are the only public state
//! mutators.

mod error;
mod id;
mod record;
mod state;
mod wire;

pub use error::{InvalidTransition, RunDomainError};
pub use id::{RunId, StepId, TaskId, ToolCallId};
pub use record::{Run, Step, Task, ToolCall};
pub use state::{RUN_TRANSITIONS, RunState, TOOL_CALL_TRANSITIONS, ToolCallState};
pub use wire::{RunWire, StepWire, TaskWire, ToolCallWire};
