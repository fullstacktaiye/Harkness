//! What is waiting, what is running, and why — read without stopping anything.

use serde::Serialize;

use super::WorkspaceKey;

/// One reading of everything the scheduler is doing.
///
/// Assembled by locking each workspace in turn and never all of them at once,
/// so producing one cannot stall dispatch. The consequence is worth stating
/// rather than hiding: a snapshot is a *composite* of instants, not one
/// instant. Two workspaces' counts can describe moments a few microseconds
/// apart, so the totals are what a front end should render and not what a test
/// should assert a global invariant from. The invariants belong to the
/// admission rules; this is how they are observed.
///
/// Serializable because `run show` (#99) and the GUI models (#100) both publish
/// it, and both should publish the same shape.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ScheduleSnapshot {
    workspaces: Vec<WorkspaceLoad>,
    processes: ProcessSlots,
    shutting_down: bool,
}

impl ScheduleSnapshot {
    pub(super) const fn new(
        workspaces: Vec<WorkspaceLoad>,
        processes: ProcessSlots,
        shutting_down: bool,
    ) -> Self {
        Self {
            workspaces,
            processes,
            shutting_down,
        }
    }

    /// Every workspace with work queued or running, in key order.
    ///
    /// A workspace with neither is absent rather than reported as empty: the
    /// scheduler forgets one as soon as it falls idle, so the list describes
    /// live work and does not grow with every workspace ever touched.
    #[must_use]
    pub fn workspaces(&self) -> &[WorkspaceLoad] {
        &self.workspaces
    }

    /// Global child-process slots, used and available.
    #[must_use]
    pub const fn processes(&self) -> ProcessSlots {
        self.processes
    }

    /// Whether the scheduler has begun shutting down.
    #[must_use]
    pub const fn shutting_down(&self) -> bool {
        self.shutting_down
    }

    /// Calls waiting for a slot across every workspace.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.workspaces.iter().map(WorkspaceLoad::queued).sum()
    }

    /// Calls currently executing across every workspace.
    #[must_use]
    pub fn running(&self) -> usize {
        self.workspaces.iter().map(WorkspaceLoad::running).sum()
    }
}

/// What one workspace is carrying.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkspaceLoad {
    workspace: WorkspaceKey,
    queued: usize,
    running: usize,
    waiting: usize,
    mutating: bool,
}

impl WorkspaceLoad {
    pub(super) const fn new(
        workspace: WorkspaceKey,
        queued: usize,
        running: usize,
        waiting: usize,
        mutating: bool,
    ) -> Self {
        Self {
            workspace,
            queued,
            running,
            waiting,
            mutating,
        }
    }

    /// Which workspace this describes.
    #[must_use]
    pub const fn workspace(&self) -> &WorkspaceKey {
        &self.workspace
    }

    /// Calls accepted for this workspace that have not been dispatched.
    #[must_use]
    pub const fn queued(&self) -> usize {
        self.queued
    }

    /// Calls of this workspace executing right now.
    #[must_use]
    pub const fn running(&self) -> usize {
        self.running
    }

    /// Submitters blocked because this workspace's queue is full.
    ///
    /// The visible face of backpressure, and the reason a workspace can appear
    /// here with nothing queued and nothing running: a producer that is parked
    /// is about to fill it, so it is not idle even for the instant in which it
    /// looks empty.
    #[must_use]
    pub const fn waiting(&self) -> usize {
        self.waiting
    }

    /// Whether the workspace's single mutation slot is held.
    ///
    /// The one field that explains a queue rather than merely measuring it: a
    /// depth of eight beside `mutating` says the workspace is waiting on one
    /// writer, and the same depth without it says the reads are waiting on the
    /// read cap or on a process slot.
    #[must_use]
    pub const fn mutating(&self) -> bool {
        self.mutating
    }
}

/// Global child-process concurrency, as it stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ProcessSlots {
    in_use: usize,
    capacity: usize,
}

impl ProcessSlots {
    pub(super) const fn new(in_use: usize, capacity: usize) -> Self {
        Self { in_use, capacity }
    }

    /// Slots held by calls that declared they spawn children.
    #[must_use]
    pub const fn in_use(&self) -> usize {
        self.in_use
    }

    /// Slots this scheduler was built with.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Slots a process-backed call could take right now.
    #[must_use]
    pub const fn available(&self) -> usize {
        self.capacity.saturating_sub(self.in_use)
    }
}

impl std::fmt::Display for ProcessSlots {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{} process slots", self.in_use, self.capacity)
    }
}
