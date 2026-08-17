//! The single orchestration loop shared by every Harkness front end.
//!
//! A run owns one worker thread and one agent. Every requested tool follows the
//! same route: schema-valid preparation, policy, optional durable approval,
//! scheduler admission, and executor delivery. The approval path is:
//!
//! ```text
//! request -> take gate ticket -> persist request -> mark run waiting
//!         -> park with no store transaction or scheduler slot held
//!         -> persisted decision -> gate wake -> re-check binding -> schedule
//! ```

mod error;
mod snapshot;
mod subscription;
#[cfg(test)]
mod tests;

use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::thread;

use harkness_git::Cancellation;
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::agent::{
    Agent, AgentAction, AgentActionRecord, Observation, ObservationRecord, TaskRef, ToolErrorView,
    ToolResultView, WorkspaceRef,
};
use crate::approval::{
    ApprovalDecision, ApprovalGate, ApprovalRequest, ApprovalScope, ApprovalState, ApprovalVerdict,
    CandidateCall, PendingApproval, WorkspaceBinding, canonical_input_hash, grant_applies,
    matching_grants,
};
use crate::domain::{
    ExecutionState, Failure, Run, RunId, Step, StepId, Task, TaskId, ToolCall, ToolCallState,
};
use crate::policy::{PolicyEngine, PolicyRequest, PolicyVerdict};
use crate::schedule::{ScheduledCall, Scheduler, WorkspaceKey};
use crate::store::{
    DEFAULT_EVENT_PAGE_LIMIT, EventKind, EventSeq, RunEvent, RunPage, Store, StoredEvent,
};
use crate::tool::{
    CallOutcome, ExecutionError, MAX_FAILURE_MESSAGE_BYTES, ToolExecutor, ToolRegistry,
    WorkspaceMetadata, truncate_failure_text,
};
use crate::trust::{ExecutionMode, PathBoundary};

const MAX_AGENT_TURNS: usize = 1_024;
const WAIT_SLICE: std::time::Duration = std::time::Duration::from_millis(20);

struct ActiveRun {
    cancellation: Cancellation,
}

#[derive(Default)]
struct DeliveryState {
    cursor: Option<EventSeq>,
    subscribers: Vec<Weak<subscription::Subscriber>>,
    closed: bool,
}

#[derive(Default)]
struct RunDelivery {
    state: Mutex<DeliveryState>,
}

struct CoordinatorInner {
    store: Arc<Store>,
    registry: Arc<ToolRegistry>,
    policy: Arc<PolicyEngine>,
    approvals: Arc<ApprovalGate>,
    scheduler: Arc<Scheduler>,
    active: Mutex<HashMap<RunId, ActiveRun>>,
    deliveries: Mutex<HashMap<RunId, Arc<RunDelivery>>>,
}

/// Shared application service that owns every run's orchestration loop.
#[derive(Clone)]
pub struct RunCoordinator {
    inner: Arc<CoordinatorInner>,
}

impl RunCoordinator {
    /// Builds a coordinator with the production executor and scheduler.
    #[must_use]
    pub fn new(store: Arc<Store>, registry: Arc<ToolRegistry>, policy: PolicyEngine) -> Self {
        let executor = ToolExecutor::new(Arc::clone(&store), Arc::clone(&registry));
        Self::with_scheduler(
            store,
            registry,
            Arc::new(policy),
            Arc::new(ApprovalGate::new()),
            Arc::new(Scheduler::new(executor)),
        )
    }

    /// Builds a coordinator from explicitly shared runtime services.
    #[must_use]
    pub fn with_scheduler(
        store: Arc<Store>,
        registry: Arc<ToolRegistry>,
        policy: Arc<PolicyEngine>,
        approvals: Arc<ApprovalGate>,
        scheduler: Arc<Scheduler>,
    ) -> Self {
        Self {
            inner: Arc::new(CoordinatorInner {
                store,
                registry,
                policy,
                approvals,
                scheduler,
                active: Mutex::new(HashMap::new()),
                deliveries: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The run store this coordinator records into.
    ///
    /// A caller that has to build a projection out of the same records — or
    /// redact a value the way this coordinator will — needs the store that
    /// belongs to the coordinator rather than one it opened separately, so that
    /// the two cannot be different stores.
    #[must_use]
    pub fn store(&self) -> &Arc<Store> {
        &self.inner.store
    }

    /// Persists a user task and returns its stable identity.
    pub fn start_task(&self, task: Task) -> Result<TaskId, RuntimeError> {
        let id = task.id();
        self.inner.store.insert_task(&task)?;
        Ok(id)
    }

    /// Starts one run on its own worker and returns immediately.
    pub fn start_run(
        &self,
        task_id: TaskId,
        agent: Box<dyn Agent>,
        workspace: WorkspaceRef,
    ) -> Result<RunId, RuntimeError> {
        self.start_run_inner(task_id, agent, workspace, None)
    }

    /// Starts a run with authoritative project catalog metadata available to
    /// every tool invocation.
    pub fn start_run_with_workspace_metadata(
        &self,
        task_id: TaskId,
        agent: Box<dyn Agent>,
        workspace: WorkspaceRef,
        metadata: WorkspaceMetadata,
    ) -> Result<RunId, RuntimeError> {
        self.start_run_inner(task_id, agent, workspace, Some(metadata))
    }

    fn start_run_inner(
        &self,
        task_id: TaskId,
        agent: Box<dyn Agent>,
        workspace: WorkspaceRef,
        workspace_metadata: Option<WorkspaceMetadata>,
    ) -> Result<RunId, RuntimeError> {
        let task = self.inner.store.load_task(task_id)?;
        let Some(project_id) = task.project_id() else {
            return Err(RuntimeError::WorkspaceIdentityRequired { task: task_id });
        };
        let expected = WorkspaceRef::from_task(&task, &**self.inner.store.redactor());
        if workspace.project_id() != expected.project_id() || workspace.root() != expected.root() {
            return Err(RuntimeError::WorkspaceMismatch { task: task_id });
        }
        let workspace_key =
            WorkspaceKey::new(project_id, task.workspace_root()).map_err(|error| {
                RuntimeError::WorkspaceUnavailable {
                    task: task_id,
                    reason: error.to_string(),
                }
            })?;
        if let Some(metadata) = workspace_metadata.as_ref() {
            if metadata.project_id() != workspace_key.project_id() {
                return Err(RuntimeError::WorkspaceMismatch { task: task_id });
            }
            let canonical = std::fs::canonicalize(metadata.canonical_root()).map_err(|error| {
                RuntimeError::WorkspaceUnavailable {
                    task: task_id,
                    reason: format!("catalog workspace metadata is unavailable: {error}"),
                }
            })?;
            if canonical != workspace_key.canonical_root() {
                return Err(RuntimeError::WorkspaceMismatch { task: task_id });
            }
        }
        let policy = self
            .inner
            .policy
            .for_workspace(workspace_key.canonical_root());

        let now = OffsetDateTime::now_utc();
        let run = Run::new(task_id, now);
        let run_id = run.id();
        self.inner.store.insert_run_with_event(
            &run,
            RunEvent::new(EventKind::RunStateChanged, now)
                .with_payload(json!({"state": ExecutionState::Queued.as_str()})),
        )?;
        self.delivery(run_id);
        if let Err(error) = self.publish(run_id) {
            let _ = self.inner.store.append_event(
                run_id,
                RunEvent::new(EventKind::Diagnostic, OffsetDateTime::now_utc()).with_payload(
                    json!({
                        "kind": "startup_delivery_failed",
                        "error_kind": error.kind(),
                        "message": error.to_string(),
                    }),
                ),
            );
        }

        let cancellation = Cancellation::default();
        self.inner
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                run_id,
                ActiveRun {
                    cancellation: cancellation.clone(),
                },
            );

        let inner = Arc::clone(&self.inner);
        let spawned = thread::Builder::new()
            .name(format!("harkness-run-{run_id}"))
            .spawn(move || {
                RunWorker::new(
                    inner,
                    run_id,
                    task,
                    workspace,
                    workspace_key,
                    workspace_metadata,
                    policy,
                    cancellation,
                    agent,
                )
                .drive();
            });
        if let Err(error) = spawned {
            self.inner
                .active
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&run_id);
            let failure = Failure::new("worker_spawn_failed", error.to_string());
            let at = OffsetDateTime::now_utc();
            let _ = self.inner.store.fail_run_with_event(
                run_id,
                failure,
                at,
                run_state_event(ExecutionState::Failed, at),
            );
            let _ = self.publish(run_id);
            self.close_delivery(run_id);
            return Err(RuntimeError::WorkerSpawn {
                run: run_id,
                reason: error.to_string(),
            });
        }
        Ok(run_id)
    }

    /// Cancels queued work, in-flight execution, or a parked approval wait.
    pub fn cancel_run(&self, run: RunId) -> Result<(), RuntimeError> {
        let cancellation = self
            .inner
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&run)
            .map(|active| active.cancellation.clone())
            .ok_or(RuntimeError::RunNotActive { run })?;
        cancellation.cancel();
        self.inner.scheduler.cancel_run(run);

        for request in self.inner.store.run_approvals(run)? {
            if request.state() != ApprovalState::Pending {
                continue;
            }
            match self.inner.store.resolve_approval(
                request.id(),
                ApprovalState::Cancelled,
                OffsetDateTime::now_utc(),
            ) {
                Ok((resolved, _)) => self.inner.approvals.resolve_from(&resolved),
                Err(_) => {
                    if let Ok(resolved) = self.inner.store.approval(request.id()) {
                        self.inner.approvals.resolve_from(&resolved);
                    }
                }
            }
        }
        self.publish(run)?;
        Ok(())
    }

    /// Persists an approval decision, then wakes its parked run.
    pub fn decide_approval(&self, decision: ApprovalDecision) -> Result<(), RuntimeError> {
        let approval = decision.approval_id();
        let current = self.inner.store.approval(approval)?;
        let run = current.run_id();
        if !self
            .inner
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(&run)
        {
            return Err(RuntimeError::ApprovalNotActive { approval });
        }
        let (resolved, _) = self.inner.store.decide_approval(approval, decision)?;
        self.inner.approvals.resolve_from(&resolved);
        self.publish(run)?;
        Ok(())
    }

    /// Loads a complete, consistent-enough read view from durable records.
    pub fn run_snapshot(&self, run_id: RunId) -> Result<RunSnapshot, RuntimeError> {
        let run = self.inner.store.load_run(run_id)?;
        let task = self.inner.store.load_task(run.task_id())?;
        let events = load_all_events(&self.inner.store, run_id)?;
        Ok(RunSnapshot {
            task,
            run,
            steps: self.inner.store.load_run_steps(run_id)?,
            tool_calls: self.inner.store.load_run_tool_calls(run_id)?,
            approvals: self.inner.store.run_approvals(run_id)?,
            artifacts: self.inner.store.run_artifacts(run_id)?,
            events,
        })
    }

    /// Subscribes to persisted events, replaying existing history first.
    pub fn subscribe(&self, run: RunId) -> Result<EventReceiver, RuntimeError> {
        let record = self.inner.store.load_run(run)?;
        let delivery = self.delivery(run);
        let mut state = delivery
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let replay_tip = self.inner.store.latest_event_seq(run)?;
        let subscriber = Arc::new(subscription::Subscriber::new(replay_tip));
        // `closed` is set by the worker that drove the run, so it is only ever
        // true for a run *this* coordinator started. A run that reached a
        // terminal state in another process — after a restart, from a second
        // front end, or simply a historical run — mints a fresh open delivery
        // here, and nothing will ever publish to it or close it: the receiver
        // replays the durable history and then blocks forever, with `try_recv`
        // reporting `Empty` rather than `Disconnected`. The run's own recorded
        // state is the authority when no worker is left to speak for it.
        if state.closed || (record.state().is_terminal() && !self.is_active(run)) {
            subscriber.close();
        } else {
            state.subscribers.push(Arc::downgrade(&subscriber));
        }
        Ok(EventReceiver::new(
            subscriber,
            Arc::clone(&self.inner.store),
            run,
            replay_tip,
        ))
    }

    /// Delegates newest-first run listing to the durable store.
    pub fn list_runs(&self, page: RunPage) -> Result<crate::store::RunListing, RuntimeError> {
        Ok(self.inner.store.list_runs(page)?)
    }

    /// Whether a worker in this process is still driving `run`.
    ///
    /// A terminal record plus no live worker means nothing will ever publish
    /// again, which is what lets a subscription close instead of waiting.
    fn is_active(&self, run: RunId) -> bool {
        self.inner
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(&run)
    }

    fn delivery(&self, run: RunId) -> Arc<RunDelivery> {
        Arc::clone(
            self.inner
                .deliveries
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(run)
                .or_default(),
        )
    }

    fn publish(&self, run: RunId) -> Result<(), RuntimeError> {
        publish(&self.inner, run).map_err(Into::into)
    }

    fn close_delivery(&self, run: RunId) {
        close_delivery(&self.inner, run);
    }
}

fn publish(inner: &CoordinatorInner, run: RunId) -> Result<(), crate::store::StoreError> {
    let delivery = Arc::clone(
        inner
            .deliveries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(run)
            .or_default(),
    );
    let mut state = delivery
        .state
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    loop {
        let page = inner
            .store
            .events(run, state.cursor, DEFAULT_EVENT_PAGE_LIMIT)?;
        if page.is_empty() {
            break;
        }
        for event in page {
            state.cursor = Some(event.seq);
            state.subscribers.retain(|subscriber| {
                subscriber.upgrade().is_some_and(|subscriber| {
                    subscriber.push(event.clone());
                    true
                })
            });
        }
    }
    Ok(())
}

fn close_delivery(inner: &CoordinatorInner, run: RunId) {
    let delivery = inner
        .deliveries
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&run)
        .cloned();
    let Some(delivery) = delivery else {
        return;
    };
    let mut state = delivery
        .state
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    state.closed = true;
    state.subscribers.retain(|subscriber| {
        subscriber.upgrade().is_some_and(|subscriber| {
            subscriber.close();
            true
        })
    });
}

fn load_all_events(
    store: &Store,
    run: RunId,
) -> Result<Vec<StoredEvent>, crate::store::StoreError> {
    let mut events = Vec::new();
    let mut after = None;
    loop {
        let page = store.events(run, after, DEFAULT_EVENT_PAGE_LIMIT)?;
        if page.is_empty() {
            break;
        }
        after = page.last().map(|event| event.seq);
        events.extend(page);
    }
    Ok(events)
}

fn run_state_event(state: ExecutionState, at: OffsetDateTime) -> RunEvent {
    RunEvent::new(EventKind::RunStateChanged, at).with_payload(json!({"state": state.as_str()}))
}

struct RunWorker {
    inner: Arc<CoordinatorInner>,
    run: RunId,
    task: Task,
    workspace: WorkspaceRef,
    workspace_key: WorkspaceKey,
    workspace_metadata: Option<WorkspaceMetadata>,
    policy: PolicyEngine,
    cancellation: Cancellation,
    agent: Box<dyn Agent>,
    planned: VecDeque<StepId>,
    next_ordinal: u32,
}

#[derive(Debug)]
struct WorkerFault {
    kind: &'static str,
    message: String,
}

impl WorkerFault {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: truncate_failure_text(message.into(), MAX_FAILURE_MESSAGE_BYTES),
        }
    }
}

impl RunWorker {
    #[allow(clippy::too_many_arguments)]
    fn new(
        inner: Arc<CoordinatorInner>,
        run: RunId,
        task: Task,
        workspace: WorkspaceRef,
        workspace_key: WorkspaceKey,
        workspace_metadata: Option<WorkspaceMetadata>,
        policy: PolicyEngine,
        cancellation: Cancellation,
        agent: Box<dyn Agent>,
    ) -> Self {
        Self {
            inner,
            run,
            task,
            workspace,
            workspace_key,
            workspace_metadata,
            policy,
            cancellation,
            agent,
            planned: VecDeque::new(),
            next_ordinal: 0,
        }
    }

    fn drive(mut self) {
        self.inner
            .store
            .set_run_owner(self.run, Some(std::process::id()))
            .ok();
        let outcome = catch_unwind(AssertUnwindSafe(|| self.drive_inner()));
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(fault)) => self.record_fault(fault),
            Err(payload) => self.record_fault(WorkerFault::new(
                "coordinator_panicked",
                panic_payload(&*payload),
            )),
        }
        let _ = self.inner.store.set_run_owner(self.run, None);
        let _ = publish(&self.inner, self.run);
        self.inner
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.run);
        close_delivery(&self.inner, self.run);
    }

    fn drive_inner(&mut self) -> Result<(), WorkerFault> {
        let at = OffsetDateTime::now_utc();
        self.inner
            .store
            .transition_run_with_event(
                self.run,
                ExecutionState::Running,
                at,
                run_state_event(ExecutionState::Running, at),
            )
            .map_err(store_fault)?;
        self.publish()?;

        let mut observation = Observation::RunStarted {
            task: TaskRef::from_task(&self.task, &**self.inner.store.redactor()),
            workspace: self.workspace.clone(),
        };
        for _ in 0..MAX_AGENT_TURNS {
            if self.cancellation.is_cancelled() {
                return self.finish_cancelled(None);
            }

            self.record_observation(&observation)?;
            let action = catch_unwind(AssertUnwindSafe(|| {
                self.agent.next_action(observation.clone())
            }))
            .map_err(|payload| WorkerFault::new("agent_panicked", panic_payload(&*payload)))?;
            if self.cancellation.is_cancelled() {
                return self.finish_cancelled(None);
            }
            self.record_action(&action)?;
            self.record_checkpoint()?;
            if self.cancellation.is_cancelled() {
                return self.finish_cancelled(None);
            }

            match action {
                AgentAction::Plan { steps } => {
                    self.persist_plan(steps)?;
                    // The agent seam has no separate plan-acknowledged
                    // observation. Re-offering the last observation lets the
                    // next scripted turn proceed without inventing a wire
                    // variant that would require a schema migration.
                }
                AgentAction::CallTool {
                    tool_id,
                    tool_version,
                    input,
                } => {
                    let Some(next) = self.dispatch_tool(tool_id, tool_version, input)? else {
                        return Ok(());
                    };
                    observation = next;
                }
                AgentAction::CompleteRun { summary } => {
                    self.terminalize_unexecuted_steps()?;
                    let at = OffsetDateTime::now_utc();
                    self.inner
                        .store
                        .transition_run_with_event(
                            self.run,
                            ExecutionState::Succeeded,
                            at,
                            run_state_event(ExecutionState::Succeeded, at)
                                .with_payload(json!({"state": "succeeded", "summary": summary})),
                        )
                        .map_err(store_fault)?;
                    self.publish()?;
                    return Ok(());
                }
                AgentAction::FailRun { reason } => {
                    self.terminalize_unexecuted_steps()?;
                    let at = OffsetDateTime::now_utc();
                    let message = serde_json::to_string(&reason)
                        .unwrap_or_else(|_| "the agent failed the run".to_owned());
                    self.inner
                        .store
                        .fail_run_with_event(
                            self.run,
                            Failure::new(reason.kind(), message),
                            at,
                            run_state_event(ExecutionState::Failed, at),
                        )
                        .map_err(store_fault)?;
                    self.publish()?;
                    return Ok(());
                }
            }
        }
        Err(WorkerFault::new(
            "agent_turn_limit",
            format!("the agent exceeded the {MAX_AGENT_TURNS}-turn run limit"),
        ))
    }

    fn record_observation(&self, observation: &Observation) -> Result<(), WorkerFault> {
        let payload = ObservationRecord::new(observation.clone())
            .to_event_payload()
            .map_err(|error| WorkerFault::new("agent_record_invalid", error.to_string()))?;
        self.inner
            .store
            .append_event(
                self.run,
                RunEvent::new(EventKind::AgentObservation, OffsetDateTime::now_utc())
                    .with_payload(payload),
            )
            .map_err(store_fault)?;
        self.publish()
    }

    fn record_action(&self, action: &AgentAction) -> Result<(), WorkerFault> {
        let payload = AgentActionRecord::new(action.clone(), &**self.inner.store.redactor())
            .to_event_payload()
            .map_err(|error| WorkerFault::new("agent_record_invalid", error.to_string()))?;
        self.inner
            .store
            .append_event(
                self.run,
                RunEvent::new(EventKind::AgentAction, OffsetDateTime::now_utc())
                    .with_payload(payload),
            )
            .map_err(store_fault)?;
        self.publish()
    }

    fn record_checkpoint(&self) -> Result<(), WorkerFault> {
        let state = catch_unwind(AssertUnwindSafe(|| self.agent.state()))
            .map_err(|payload| WorkerFault::new("agent_panicked", panic_payload(&*payload)))?;
        self.inner
            .store
            .append_event(
                self.run,
                RunEvent::new(EventKind::AgentCheckpoint, OffsetDateTime::now_utc())
                    .with_payload(state.to_event_payload()),
            )
            .map_err(store_fault)?;
        self.publish()
    }

    fn persist_plan(&mut self, steps: Vec<crate::agent::PlannedStep>) -> Result<(), WorkerFault> {
        for planned in steps {
            let at = OffsetDateTime::now_utc();
            let step = Step::new(self.run, self.next_ordinal, planned.title, at);
            self.next_ordinal = self
                .next_ordinal
                .checked_add(1)
                .ok_or_else(|| WorkerFault::new("step_limit", "step ordinal overflow"))?;
            self.inner.store.insert_step(&step).map_err(store_fault)?;
            self.inner
                .store
                .append_event(
                    self.run,
                    RunEvent::new(EventKind::Diagnostic, at)
                        .for_step(step.id())
                        .with_payload(json!({"kind": "step_planned", "title": step.title()})),
                )
                .map_err(store_fault)?;
            self.planned.push_back(step.id());
        }
        self.publish()
    }

    fn begin_step(&mut self, title: &str) -> Result<Step, WorkerFault> {
        let mut step = if let Some(id) = self.planned.pop_front() {
            self.inner.store.load_step(id).map_err(store_fault)?
        } else {
            let step = Step::new(
                self.run,
                self.next_ordinal,
                title,
                OffsetDateTime::now_utc(),
            );
            self.next_ordinal = self
                .next_ordinal
                .checked_add(1)
                .ok_or_else(|| WorkerFault::new("step_limit", "step ordinal overflow"))?;
            self.inner.store.insert_step(&step).map_err(store_fault)?;
            step
        };
        let at = OffsetDateTime::now_utc();
        (step, _) = self
            .inner
            .store
            .transition_step_with_event(
                step.id(),
                ExecutionState::Running,
                at,
                RunEvent::new(EventKind::StepStarted, at)
                    .for_step(step.id())
                    .with_payload(json!({"state": "running"})),
            )
            .map_err(store_fault)?;
        self.publish()?;
        Ok(step)
    }

    fn dispatch_tool(
        &mut self,
        tool_id: crate::tool::ToolId,
        tool_version: crate::tool::ToolVersion,
        input: Value,
    ) -> Result<Option<Observation>, WorkerFault> {
        let step = self.begin_step(&format!("Run {tool_id}"))?;
        let at = OffsetDateTime::now_utc();
        let call = ToolCall::new(
            &step,
            tool_id.to_string(),
            tool_version.to_string(),
            input.clone(),
            at,
        );
        let call_id = call.id();
        self.inner
            .store
            .insert_tool_call(&call)
            .map_err(store_fault)?;

        let Some(tool) = self
            .inner
            .registry
            .get(&tool_id, Some(&tool_version))
            .cloned()
        else {
            let error = self
                .inner
                .registry
                .resolve(&tool_id, Some(&tool_version))
                .expect_err("get and resolve agree on a missing tool");
            return self.fail_before_policy(&step, call_id, error.kind(), error.to_string());
        };
        let raw = serde_json::value::to_raw_value(&input)
            .map_err(|error| WorkerFault::new("invalid_input", error.to_string()))?;
        let boundary = PathBoundary::new(
            self.workspace_key.canonical_root(),
            std::iter::empty::<&std::path::Path>(),
        )
        .map_err(|error| WorkerFault::new(error.kind(), error.to_string()))?;
        let prepared = match tool.prepare_json(&raw, &boundary) {
            Ok(prepared) => prepared,
            Err(error) => {
                return self.fail_before_policy(&step, call_id, error.kind(), error.to_string());
            }
        };
        if self.cancellation.is_cancelled() {
            let at = OffsetDateTime::now_utc();
            self.inner
                .store
                .transition_tool_call_with_event(
                    call_id,
                    ToolCallState::Cancelled,
                    at,
                    RunEvent::new(EventKind::ToolCallStateChanged, at)
                        .for_step(step.id())
                        .for_tool_call(call_id)
                        .with_payload(json!({"state": "cancelled"})),
                )
                .map_err(store_fault)?;
            self.finish_cancelled(Some(step.id()))?;
            return Ok(None);
        }

        let input_hash = canonical_input_hash(&input)
            .map_err(|error| WorkerFault::new(error.kind(), error.to_string()))?;
        let binding = WorkspaceBinding::new(
            Some(self.workspace_key.project_id()),
            self.workspace_key.canonical_root(),
        );
        let identity = tool.descriptor().identity().clone();
        let grants = self.inner.store.run_grants(self.run).map_err(store_fault)?;
        let candidate = CandidateCall::new(self.run, call_id, &binding, &identity, input_hash)
            .with_capabilities(tool.descriptor().capabilities());
        let matching = matching_grants(&grants, &candidate);
        let trust = self
            .inner
            .store
            .resolve_workspace_trust(
                self.workspace_key.project_id(),
                self.workspace_key.canonical_root(),
            )
            .map_err(store_fault)?;
        let request = PolicyRequest::new(
            tool.descriptor(),
            prepared.classification(),
            trust,
            ExecutionMode::Interactive,
        )
        .with_paths(prepared.paths())
        .with_grants(&matching);
        let decision = self.policy.evaluate(&request);
        let risk = request.risk();
        if self.cancellation.is_cancelled() {
            let at = OffsetDateTime::now_utc();
            self.inner
                .store
                .transition_tool_call_with_event(
                    call_id,
                    ToolCallState::Cancelled,
                    at,
                    RunEvent::new(EventKind::ToolCallStateChanged, at)
                        .for_step(step.id())
                        .for_tool_call(call_id)
                        .with_payload(json!({"state": "cancelled"})),
                )
                .map_err(store_fault)?;
            self.finish_cancelled(Some(step.id()))?;
            return Ok(None);
        }
        let at = OffsetDateTime::now_utc();
        self.inner
            .store
            .apply_tool_call_policy_decision_with_event(
                call_id,
                decision.clone(),
                at,
                RunEvent::new(EventKind::PolicyDecision, at)
                    .for_step(step.id())
                    .for_tool_call(call_id)
                    .with_payload(json!({
                        "decision": decision,
                        "risk": risk.as_str(),
                    })),
            )
            .map_err(store_fault)?;
        self.publish()?;

        match decision.verdict() {
            PolicyVerdict::Allow => self.execute_scheduled(step, call_id, risk, None),
            PolicyVerdict::Deny => {
                self.finish_step_failed(step.id(), Failure::new("policy", decision.reason()))?;
                Ok(Some(Observation::PolicyDenied {
                    call: call_id,
                    reason: crate::agent::RedactedText::new(
                        decision.reason(),
                        &**self.inner.store.redactor(),
                    ),
                }))
            }
            PolicyVerdict::Ask => {
                self.await_approval(step, call_id, input_hash, binding, identity, risk)
            }
        }
    }

    fn fail_before_policy(
        &self,
        step: &Step,
        call: crate::domain::ToolCallId,
        kind: &str,
        message: String,
    ) -> Result<Option<Observation>, WorkerFault> {
        // Clamped, for the reason `ToolError::as_failure` clamps: the store
        // refuses an oversized `failure_message`, and here that refusal becomes
        // a `WorkerFault` that fails the whole *run* rather than handing the
        // agent a recoverable `ToolFailed`. A caller-chosen path is enough to
        // reach it — a boundary refusal embeds the path it was given, so an
        // input that fit the inline bound produces a message that does not.
        let message = truncate_failure_text(message, MAX_FAILURE_MESSAGE_BYTES);
        let failure = Failure::new(kind, &message);
        let at = OffsetDateTime::now_utc();
        self.inner
            .store
            .fail_tool_call_with_event(
                call,
                failure.clone(),
                at,
                RunEvent::new(EventKind::ToolCallStateChanged, at)
                    .for_step(step.id())
                    .for_tool_call(call)
                    .with_payload(json!({"state": "failed", "kind": kind})),
            )
            .map_err(store_fault)?;
        self.finish_step_failed(step.id(), failure)?;
        self.publish()?;
        Ok(Some(Observation::ToolFailed {
            call,
            error: ToolErrorView::new(kind, message, &**self.inner.store.redactor()),
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn await_approval(
        &self,
        step: Step,
        call: crate::domain::ToolCallId,
        input_hash: crate::approval::InputHash,
        binding: WorkspaceBinding,
        identity: crate::tool::ToolIdentity,
        risk: crate::tool::RiskLevel,
    ) -> Result<Option<Observation>, WorkerFault> {
        let one_call_only = self
            .inner
            .store
            .load_tool_call(call)
            .map_err(store_fault)?
            .policy_decision()
            .is_some_and(|decision| decision.one_call_only());
        let requested_scope = if one_call_only {
            ApprovalScope::ExactCall
        } else {
            ApprovalScope::ToolForRun
        };
        let pending = PendingApproval::new(
            self.run,
            call,
            identity.clone(),
            input_hash,
            binding.clone(),
            risk,
            OffsetDateTime::now_utc(),
        )
        .requesting(requested_scope)
        .with_capabilities(
            self.inner
                .registry
                .get(&identity.id, Some(&identity.version))
                .expect("the prepared tool remains registered")
                .descriptor()
                .capabilities()
                .iter()
                .cloned(),
        )
        .summarized_as(format!("request to run {identity}"));
        let request = ApprovalRequest::open(pending)
            .map_err(|error| WorkerFault::new(error.kind(), error.to_string()))?;
        let ticket = self.inner.approvals.ticket(request.id()).ok_or_else(|| {
            WorkerFault::new("approval_waiter_exists", "approval already has a waiter")
        })?;
        self.inner
            .store
            .open_approval(request)
            .map_err(store_fault)?;
        let at = OffsetDateTime::now_utc();
        self.inner
            .store
            .transition_run_with_event(
                self.run,
                ExecutionState::WaitingForApproval,
                at,
                run_state_event(ExecutionState::WaitingForApproval, at),
            )
            .map_err(store_fault)?;
        self.publish()?;

        self.wait_approval_ticket(ticket, step, call, input_hash, binding, identity, risk)
    }

    #[allow(clippy::too_many_arguments)]
    fn wait_approval_ticket(
        &self,
        mut ticket: crate::approval::ApprovalTicket<'_>,
        step: Step,
        call: crate::domain::ToolCallId,
        input_hash: crate::approval::InputHash,
        binding: WorkspaceBinding,
        identity: crate::tool::ToolIdentity,
        risk: crate::tool::RiskLevel,
    ) -> Result<Option<Observation>, WorkerFault> {
        loop {
            match ticket.wait_for(WAIT_SLICE) {
                Ok(observation) => {
                    return self.resume_approval(
                        observation,
                        step,
                        call,
                        input_hash,
                        binding,
                        identity,
                        risk,
                    );
                }
                Err(returned) => {
                    ticket = returned;
                    self.publish()?;
                    if self.cancellation.is_cancelled() {
                        if let Ok((resolved, _)) = self.inner.store.resolve_approval(
                            ticket.approval_id(),
                            ApprovalState::Cancelled,
                            OffsetDateTime::now_utc(),
                        ) {
                            self.inner.approvals.resolve_from(&resolved);
                        } else if let Ok(resolved) = self.inner.store.approval(ticket.approval_id())
                        {
                            self.inner.approvals.resolve_from(&resolved);
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn resume_approval(
        &self,
        observation: crate::approval::ApprovalObservation,
        step: Step,
        call: crate::domain::ToolCallId,
        input_hash: crate::approval::InputHash,
        binding: WorkspaceBinding,
        identity: crate::tool::ToolIdentity,
        risk: crate::tool::RiskLevel,
    ) -> Result<Option<Observation>, WorkerFault> {
        let request = self
            .inner
            .store
            .approval(observation.approval_id())
            .map_err(store_fault)?;
        if observation.state() == ApprovalState::Cancelled || self.cancellation.is_cancelled() {
            let at = OffsetDateTime::now_utc();
            self.inner
                .store
                .transition_tool_call_with_event(
                    call,
                    ToolCallState::Cancelled,
                    at,
                    RunEvent::new(EventKind::ToolCallStateChanged, at)
                        .for_step(step.id())
                        .for_tool_call(call)
                        .with_payload(json!({"state": "cancelled"})),
                )
                .map_err(store_fault)?;
            self.finish_cancelled(Some(step.id()))?;
            return Ok(None);
        }

        let decided_by = request
            .decision()
            .map(|decision| decision.decided_via().as_str())
            .unwrap_or("approval_service");
        if observation.verdict() == ApprovalVerdict::Denied {
            let failure = Failure::new(
                "approval_denied",
                observation.reason().unwrap_or("the approval was denied"),
            );
            let at = OffsetDateTime::now_utc();
            self.inner
                .store
                .reject_tool_call_approval_with_event(
                    call,
                    decided_by,
                    failure.clone(),
                    at,
                    RunEvent::new(EventKind::ToolCallStateChanged, at)
                        .for_step(step.id())
                        .for_tool_call(call)
                        .with_payload(json!({"state": "denied", "kind": "approval_denied"})),
                )
                .map_err(store_fault)?;
            self.inner
                .store
                .resume_run_after_denial_with_event(
                    self.run,
                    decided_by,
                    at,
                    run_state_event(ExecutionState::Running, at),
                )
                .map_err(store_fault)?;
            self.finish_step_failed(step.id(), failure)?;
            self.publish()?;
            return Ok(Some(Observation::ApprovalOutcome {
                call,
                outcome: crate::agent::ApprovalOutcomeView::Denied,
            }));
        }

        let grant = request.grant().ok_or_else(|| {
            WorkerFault::new(
                "approval_binding_mismatch",
                "a granted request produced no grant",
            )
        })?;
        let tool = self
            .inner
            .registry
            .get(&identity.id, Some(&identity.version))
            .ok_or_else(|| {
                WorkerFault::new("approval_binding_mismatch", "approved tool disappeared")
            })?;
        let candidate = CandidateCall::new(self.run, call, &binding, &identity, input_hash)
            .with_capabilities(tool.descriptor().capabilities());
        if !grant_applies(&grant, &candidate) {
            return Err(WorkerFault::new(
                "approval_binding_mismatch",
                "the persisted approval does not bind to the exact scheduled request",
            ));
        }
        let at = OffsetDateTime::now_utc();
        self.inner
            .store
            .approve_run_with_event(
                self.run,
                decided_by,
                at,
                run_state_event(ExecutionState::Running, at),
            )
            .map_err(store_fault)?;
        self.publish()?;
        self.execute_scheduled(
            step,
            call,
            risk,
            Some((decided_by.to_owned(), identity, input_hash)),
        )
    }

    fn execute_scheduled(
        &self,
        step: Step,
        call: crate::domain::ToolCallId,
        risk: crate::tool::RiskLevel,
        approval: Option<(
            String,
            crate::tool::ToolIdentity,
            crate::approval::InputHash,
        )>,
    ) -> Result<Option<Observation>, WorkerFault> {
        let mut scheduled = ScheduledCall::new(
            call,
            self.workspace_key.clone(),
            risk,
            self.cancellation.clone(),
        );
        if let Some(metadata) = self.workspace_metadata.as_ref() {
            scheduled = scheduled
                .with_workspace_metadata(metadata.clone())
                .map_err(|error| WorkerFault::new(error.kind(), error.to_string()))?;
        }
        if let Some((decided_by, expected_tool, expected_input_hash)) = approval {
            scheduled =
                scheduled.approved_with_binding(decided_by, expected_tool, expected_input_hash);
        }
        let mut ticket = self
            .inner
            .scheduler
            .submit(scheduled)
            .map_err(|error| WorkerFault::new(error.kind(), error.to_string()))?;
        let completed = loop {
            match ticket.wait_for(WAIT_SLICE) {
                Ok(outcome) => break outcome,
                Err(returned) => {
                    ticket = returned;
                    self.publish()?;
                }
            }
        }
        .map_err(|error| match error {
            crate::schedule::ScheduleError::Execution { source, .. } => match *source {
                ExecutionError::Store(error) => store_fault(error),
                source => WorkerFault::new(source.kind(), source.to_string()),
            },
            error => WorkerFault::new(error.kind(), error.to_string()),
        })?;
        self.publish()?;

        match completed.outcome() {
            CallOutcome::Succeeded { output } => {
                self.finish_step(step.id(), ExecutionState::Succeeded)?;
                let artifacts = self
                    .inner
                    .store
                    .run_artifacts(self.run)
                    .map_err(store_fault)?
                    .into_iter()
                    .filter(|artifact| artifact.tool_call_id() == Some(call))
                    .map(|artifact| artifact.reference())
                    .collect();
                Ok(Some(Observation::ToolResult {
                    call,
                    result: ToolResultView::with_artifacts(
                        output.clone(),
                        artifacts,
                        &**self.inner.store.redactor(),
                    ),
                }))
            }
            CallOutcome::Failed { .. } | CallOutcome::TimedOut { .. } => {
                let failure = completed
                    .record()
                    .failure()
                    .cloned()
                    .unwrap_or_else(|| Failure::new("execution_failed", "the tool failed"));
                self.finish_step_failed(step.id(), failure.clone())?;
                Ok(Some(Observation::ToolFailed {
                    call,
                    error: ToolErrorView::new(
                        failure.kind(),
                        failure.message(),
                        &**self.inner.store.redactor(),
                    ),
                }))
            }
            CallOutcome::Cancelled => {
                self.finish_cancelled(Some(step.id()))?;
                Ok(None)
            }
            CallOutcome::Interrupted => {
                self.finish_step(step.id(), ExecutionState::Interrupted)?;
                let at = OffsetDateTime::now_utc();
                self.inner
                    .store
                    .transition_run_with_event(
                        self.run,
                        ExecutionState::Interrupted,
                        at,
                        run_state_event(ExecutionState::Interrupted, at),
                    )
                    .map_err(store_fault)?;
                self.publish()?;
                Ok(None)
            }
        }
    }

    fn finish_step(&self, step: StepId, state: ExecutionState) -> Result<(), WorkerFault> {
        let at = OffsetDateTime::now_utc();
        self.inner
            .store
            .transition_step_with_event(
                step,
                state,
                at,
                RunEvent::new(EventKind::StepFinished, at)
                    .for_step(step)
                    .with_payload(json!({"state": state.as_str()})),
            )
            .map_err(store_fault)?;
        self.publish()
    }

    fn finish_step_failed(&self, step: StepId, failure: Failure) -> Result<(), WorkerFault> {
        let at = OffsetDateTime::now_utc();
        self.inner
            .store
            .fail_step_with_event(
                step,
                failure.clone(),
                at,
                RunEvent::new(EventKind::StepFinished, at)
                    .for_step(step)
                    .with_payload(json!({
                        "state": "failed",
                        "kind": failure.kind(),
                    })),
            )
            .map_err(store_fault)?;
        self.publish()
    }

    fn finish_cancelled(&self, step: Option<StepId>) -> Result<(), WorkerFault> {
        if let Some(step) = step {
            let record = self.inner.store.load_step(step).map_err(store_fault)?;
            if !record.state().is_terminal() {
                self.finish_step(step, ExecutionState::Cancelled)?;
            }
        }
        // A plan the agent already published outlives the step that was
        // running. Terminalizing only the current one left every later planned
        // step `queued` under a run that had reached `Cancelled`, with no
        // worker left to move it and nothing that sweeps it — a terminal run
        // whose own children contradict it. `CompleteRun` and `FailRun` have
        // always drained this queue; the cancel and interrupt exits did not.
        self.terminalize_planned_steps()?;
        let run = self.inner.store.load_run(self.run).map_err(store_fault)?;
        if !run.state().is_terminal() {
            let at = OffsetDateTime::now_utc();
            self.inner
                .store
                .transition_run_with_event(
                    self.run,
                    ExecutionState::Cancelled,
                    at,
                    run_state_event(ExecutionState::Cancelled, at),
                )
                .map_err(store_fault)?;
            self.publish()?;
        }
        Ok(())
    }

    fn terminalize_unexecuted_steps(&mut self) -> Result<(), WorkerFault> {
        self.terminalize_planned_steps()?;
        self.planned.clear();
        Ok(())
    }

    /// Cancels every step the agent planned but no worker ever began.
    ///
    /// Borrows shared rather than exclusively so the cancellation exits can
    /// call it: those run while an approval ticket borrows the gate out of
    /// `self.inner`, and draining the queue there would need a second mutable
    /// borrow. Not draining costs nothing — the worker is finished either way.
    fn terminalize_planned_steps(&self) -> Result<(), WorkerFault> {
        for step in &self.planned {
            let record = self.inner.store.load_step(*step).map_err(store_fault)?;
            if !record.state().is_terminal() {
                self.finish_step(*step, ExecutionState::Cancelled)?;
            }
        }
        Ok(())
    }

    fn record_fault(&self, fault: WorkerFault) {
        let at = OffsetDateTime::now_utc();
        for request in self.inner.store.run_approvals(self.run).unwrap_or_default() {
            if request.state() == ApprovalState::Pending
                && let Ok((resolved, _)) =
                    self.inner
                        .store
                        .resolve_approval(request.id(), ApprovalState::Cancelled, at)
            {
                self.inner.approvals.resolve_from(&resolved);
            }
        }
        for call in self
            .inner
            .store
            .load_run_tool_calls(self.run)
            .unwrap_or_default()
        {
            if !call.state().is_terminal() {
                if call.state() == ToolCallState::AwaitingApproval {
                    let _ = self.inner.store.transition_tool_call_with_event(
                        call.id(),
                        ToolCallState::Cancelled,
                        at,
                        RunEvent::new(EventKind::ToolCallStateChanged, at)
                            .for_step(call.step_id())
                            .for_tool_call(call.id())
                            .with_payload(json!({"state": "cancelled", "kind": fault.kind})),
                    );
                } else {
                    let failure = Failure::new(fault.kind, &fault.message);
                    let _ = self.inner.store.fail_tool_call_with_event(
                        call.id(),
                        failure,
                        at,
                        RunEvent::new(EventKind::ToolCallStateChanged, at)
                            .for_step(call.step_id())
                            .for_tool_call(call.id())
                            .with_payload(json!({"state": "failed", "kind": fault.kind})),
                    );
                }
            }
        }
        for step in self
            .inner
            .store
            .load_run_steps(self.run)
            .unwrap_or_default()
        {
            if !step.state().is_terminal() {
                let failure = Failure::new(fault.kind, &fault.message);
                let _ = self.inner.store.fail_step_with_event(
                    step.id(),
                    failure,
                    at,
                    RunEvent::new(EventKind::StepFinished, at)
                        .for_step(step.id())
                        .with_payload(json!({"state": "failed", "kind": fault.kind})),
                );
            }
        }
        let _ = self.inner.store.append_event(
            self.run,
            RunEvent::new(EventKind::Diagnostic, at).with_payload(json!({
                "kind": "coordinator_error",
                "error_kind": fault.kind,
                "message": fault.message,
            })),
        );
        if let Ok(run) = self.inner.store.load_run(self.run)
            && !run.state().is_terminal()
        {
            let _ = self.inner.store.fail_run_with_event(
                self.run,
                Failure::new(fault.kind, fault.message),
                at,
                run_state_event(ExecutionState::Failed, at),
            );
        }
    }

    fn publish(&self) -> Result<(), WorkerFault> {
        publish(&self.inner, self.run).map_err(store_fault)
    }
}

fn panic_payload(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&'static str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a component panicked with a non-string payload".to_owned())
}

fn store_fault(error: crate::store::StoreError) -> WorkerFault {
    WorkerFault::new(error.kind(), error.to_string())
}

pub use error::RuntimeError;
pub use snapshot::RunSnapshot;
pub use subscription::{
    EventDelivery, EventReceiver, ReceiveTimeoutError, SUBSCRIBER_CAPACITY, TryReceiveError,
};
