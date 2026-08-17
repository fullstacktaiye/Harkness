//! `harkness approvals` — list pending requests and answer them.
//!
//! # Two listings, because the store indexes two different questions
//!
//! The pending queue is unpaged. A request exists only while a call is parked
//! waiting for it and the scheduler caps how many calls can be in flight, so
//! the set is bounded by construction — which is why
//! [`RunCoordinator::pending_approvals`] takes no page. History is not bounded
//! that way, and the store keys approvals by their run, so `--all` pages by
//! *run* and reports every approval the runs on that page recorded. Both use
//! the reads the coordinator already publishes; neither invents an index.
//!
//! # Answering reaches a waiter, not a row
//!
//! `decide_approval` wakes a thread parked on a condition variable in the
//! process that started the run. A second process can read the request and
//! cannot answer it, and the coordinator says so with `approval_not_active`
//! rather than persisting a decision nothing would ever act on. In practice
//! that means these two verbs answer a run this invocation is driving —
//! `agent run --interactive` and `tool invoke --interactive` are the surfaces
//! that do — while a run driven by the application is answered there.

use std::path::Path;

use clap::{Args, Subcommand};
use harkness_git::Cancellation;
use harkness_runtime::approval::{
    ApprovalDecision, ApprovalId, ApprovalRequest, ApprovalScope, DecidedVia,
};
use harkness_runtime::coordinator::RunCoordinator;
use harkness_runtime::store::{DEFAULT_RUN_PAGE_LIMIT, RunPage};
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::runtime_support::{
    approval_value, decode_run_cursor, encode_run_cursor, open_existing_runtime, open_runtime,
    parse_run_limit, recorded_input,
};
use crate::{CliError, CommandResult, command_result, load_service, single_line};

/// How far a granted decision reaches.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub(crate) enum GrantScope {
    /// This tool, this version, this exact input, this run.
    #[default]
    Call,
    /// This tool and version for the remainder of the run.
    ToolThisRun,
    /// The declared capabilities of this request, for the remainder of the run.
    CapabilityThisRun,
}

impl From<GrantScope> for ApprovalScope {
    fn from(scope: GrantScope) -> Self {
        match scope {
            GrantScope::Call => Self::ExactCall,
            GrantScope::ToolThisRun => Self::ToolForRun,
            GrantScope::CapabilityThisRun => Self::CapabilityForRun,
        }
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum ApprovalsCommand {
    /// List approval requests: pending across every run by default.
    List(ListArguments),
    /// Grant a pending request this invocation's run is parked on.
    Approve(ApproveArguments),
    /// Deny a pending request this invocation's run is parked on.
    Deny {
        /// Approval identifier.
        #[arg(value_name = "APPROVAL_ID")]
        approval_id: String,
        /// Recorded reason for the refusal.
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
    },
}

#[derive(Debug, Args)]
pub(crate) struct ListArguments {
    /// Include answered requests, paged by run rather than by request.
    #[arg(long)]
    all: bool,
    /// Maximum number of runs whose approvals are reported, with `--all`.
    #[arg(
        long,
        value_name = "COUNT",
        default_value_t = DEFAULT_RUN_PAGE_LIMIT,
        value_parser = parse_run_limit,
        requires = "all",
    )]
    limit: usize,
    /// Opaque run continuation token returned as `next_cursor`, with `--all`.
    #[arg(long, value_name = "TOKEN", requires = "all")]
    cursor: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct ApproveArguments {
    /// Approval identifier.
    #[arg(value_name = "APPROVAL_ID")]
    approval_id: String,
    /// How far the grant reaches; never wider than the stored request allows.
    #[arg(long, value_enum, default_value_t = GrantScope::Call)]
    scope: GrantScope,
    /// Recorded reason for the decision.
    #[arg(long, value_name = "TEXT")]
    reason: Option<String>,
}

pub(crate) fn run_approvals(
    command: ApprovalsCommand,
    data_dir: Option<&Path>,
    json_output: bool,
    cancellation: &Cancellation,
) -> Result<CommandResult, CliError> {
    let _ = cancellation;
    match command {
        ApprovalsCommand::List(arguments) => list(arguments, data_dir, json_output),
        ApprovalsCommand::Approve(arguments) => approve(arguments, data_dir, json_output),
        ApprovalsCommand::Deny {
            approval_id,
            reason,
        } => deny(&approval_id, reason, data_dir, json_output),
    }
}

fn parse_approval_id(value: &str) -> Result<ApprovalId, CliError> {
    value.parse::<ApprovalId>().map_err(|_| {
        CliError::Usage(format!(
            "{:?} is not a Harkness approval identifier",
            single_line(value)
        ))
    })
}

fn list(
    arguments: ListArguments,
    data_dir: Option<&Path>,
    json_output: bool,
) -> Result<CommandResult, CliError> {
    let service = load_service(data_dir)?;
    let cursor = arguments
        .cursor
        .as_deref()
        .map(decode_run_cursor)
        .transpose()?;
    // A listing must not create the run store: no database means nothing has
    // been recorded, which is the empty queue.
    let Some(coordinator) = open_existing_runtime(service.data_dir())? else {
        return listing_result(json_output, Vec::new(), Vec::new(), None);
    };
    let (requests, next) = if arguments.all {
        let page = match cursor {
            Some(cursor) => RunPage::after(cursor, arguments.limit),
            None => RunPage::new(arguments.limit),
        };
        let listing = coordinator.list_runs(page).map_err(CliError::Runtime)?;
        let mut requests = Vec::new();
        for run in &listing.runs {
            requests.extend(
                coordinator
                    .store()
                    .run_approvals(run.id())
                    .map_err(CliError::Store)?,
            );
        }
        let next = listing
            .next_cursor
            .as_ref()
            .map(encode_run_cursor)
            .transpose()?;
        (requests, next)
    } else {
        (
            coordinator.pending_approvals().map_err(CliError::Runtime)?,
            None,
        )
    };
    let inputs = requests
        .iter()
        .map(|request| recorded_input(&coordinator, request))
        .collect::<Vec<_>>();
    listing_result(json_output, requests, inputs, next)
}

fn listing_result(
    json_output: bool,
    requests: Vec<ApprovalRequest>,
    inputs: Vec<Value>,
    next_cursor: Option<String>,
) -> Result<CommandResult, CliError> {
    command_result(
        json_output,
        || {
            if requests.is_empty() {
                return "no approval requests".to_owned();
            }
            requests
                .iter()
                .map(|request| {
                    format!(
                        "{}\t{}\t{}\t{}\t{}\t{}",
                        request.id(),
                        request.state().as_str(),
                        request.tool().id.as_str(),
                        request.tool().version,
                        request.risk().as_str(),
                        single_line(request.input_summary()),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        },
        || {
            Ok(json!({
                "kind": "approval_list",
                "approvals": requests
                    .iter()
                    .zip(inputs.iter())
                    .map(|(request, input)| approval_value(request, Some(input)))
                    .collect::<Vec<_>>(),
                "next_cursor": next_cursor,
            }))
        },
    )
}

fn approve(
    arguments: ApproveArguments,
    data_dir: Option<&Path>,
    json_output: bool,
) -> Result<CommandResult, CliError> {
    let approval = parse_approval_id(&arguments.approval_id)?;
    let service = load_service(data_dir)?;
    let coordinator = open_runtime(service.data_dir())?;
    let request = coordinator
        .store()
        .approval(approval)
        .map_err(CliError::Store)?;
    // Narrowed against the stored request rather than trusted from the flag. A
    // decision may narrow to the single call in front of a human and may never
    // widen, and the stored `effective_scope` already carries the ceiling the
    // request's risk imposed when it was created.
    let requested = ApprovalScope::from(arguments.scope);
    let scope = narrowest(requested, request.effective_scope());
    let mut decision =
        ApprovalDecision::grant(approval, scope, DecidedVia::Cli, OffsetDateTime::now_utc());
    if let Some(reason) = arguments.reason {
        decision = decision.because(reason);
    }
    coordinator
        .decide_approval(decision)
        .map_err(CliError::Runtime)?;
    decided_result(&coordinator, approval, json_output, "granted")
}

fn deny(
    approval_id: &str,
    reason: Option<String>,
    data_dir: Option<&Path>,
    json_output: bool,
) -> Result<CommandResult, CliError> {
    let approval = parse_approval_id(approval_id)?;
    let service = load_service(data_dir)?;
    let coordinator = open_runtime(service.data_dir())?;
    let mut decision = ApprovalDecision::deny(approval, DecidedVia::Cli, OffsetDateTime::now_utc());
    if let Some(reason) = reason {
        decision = decision.because(reason);
    }
    coordinator
        .decide_approval(decision)
        .map_err(CliError::Runtime)?;
    decided_result(&coordinator, approval, json_output, "denied")
}

/// The narrower of what was asked for and what the record permits.
///
/// `ExactCall` is the narrowest and `CapabilityForRun` the widest. A stored
/// request that was already downgraded to an exact call — every remote write
/// and every destructive request is — cannot be answered more broadly here.
fn narrowest(requested: ApprovalScope, permitted: ApprovalScope) -> ApprovalScope {
    let rank = |scope: ApprovalScope| match scope {
        ApprovalScope::ExactCall => 0_u8,
        ApprovalScope::ToolForRun => 1,
        ApprovalScope::CapabilityForRun => 2,
    };
    if rank(requested) <= rank(permitted) {
        requested
    } else {
        permitted
    }
}

fn decided_result(
    coordinator: &RunCoordinator,
    approval: ApprovalId,
    json_output: bool,
    verdict: &'static str,
) -> Result<CommandResult, CliError> {
    let request = coordinator
        .store()
        .approval(approval)
        .map_err(CliError::Store)?;
    let input = recorded_input(coordinator, &request);
    command_result(
        json_output,
        || format!("{verdict} approval {approval}"),
        || {
            Ok(json!({
                "kind": "approval_decision",
                "approval": approval_value(&request, Some(&input)),
            }))
        },
    )
}

#[cfg(test)]
mod tests {
    use harkness_runtime::approval::ApprovalScope;

    use super::{GrantScope, narrowest};

    #[test]
    fn a_decision_narrows_to_the_stored_scope_and_never_widens_past_it() {
        assert_eq!(
            narrowest(ApprovalScope::CapabilityForRun, ApprovalScope::ExactCall),
            ApprovalScope::ExactCall,
            "a request downgraded to one call must not be answered run-wide"
        );
        assert_eq!(
            narrowest(ApprovalScope::ExactCall, ApprovalScope::ToolForRun),
            ApprovalScope::ExactCall,
            "a human may always answer for the single call in front of them"
        );
        assert_eq!(
            narrowest(ApprovalScope::ToolForRun, ApprovalScope::ToolForRun),
            ApprovalScope::ToolForRun
        );
    }

    #[test]
    fn every_grant_scope_flag_names_a_stored_scope() {
        for (flag, scope) in [
            (GrantScope::Call, ApprovalScope::ExactCall),
            (GrantScope::ToolThisRun, ApprovalScope::ToolForRun),
            (
                GrantScope::CapabilityThisRun,
                ApprovalScope::CapabilityForRun,
            ),
        ] {
            assert_eq!(ApprovalScope::from(flag), scope);
        }
    }
}
