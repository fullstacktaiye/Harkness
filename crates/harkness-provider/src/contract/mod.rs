//! The provider-neutral model contract.
//!
//! Every type a model endpoint is described by lives here, and nothing here
//! knows what an endpoint looks like: there is no URL, no header, no
//! credential, no wire format and no HTTP client in this module or in this
//! crate. That absence is the point — an adapter's wire types stay private to
//! the adapter, so a second one is a new implementation of
//! [`ModelProvider`] rather than a rewrite of everything above it.
//!
//! # The pieces
//!
//! - [`ProviderId`], [`ModelId`], [`ProviderToolCallId`] — the identities that
//!   reach records.
//! - [`ProviderCapabilities`] — what an endpoint claims, and what unknown means.
//! - [`Role`], [`ContentPart`], [`ModelMessage`], [`ToolDefinition`],
//!   [`ModelRequest`] — what is sent.
//! - [`ModelEvent`], [`StopReason`], [`TokenUsage`] — what comes back.
//! - [`ModelEventSink`], [`SinkControl`] — where it goes while it is arriving.
//! - [`TurnOutcome`] — what one finished turn is worth recording.
//! - [`ProviderError`], [`RetryHint`], [`ContractError`] — how it fails.
//! - [`ModelProvider`] — the trait itself.

mod capability;
mod error;
mod event;
mod ids;
mod message;
mod provider;

pub use capability::ProviderCapabilities;
pub use error::{ContractError, ErrorDetail, MAX_ERROR_DETAIL_BYTES, ProviderError, RetryHint};
pub use event::{
    CANCELLATION_POLL_INTERVAL, DEFAULT_RECORDED_EVENT_CAPACITY, DiscardEvents, ModelEvent,
    ModelEventSink, RecordedEvents, SinkControl, StopReason, TokenUsage, TurnOutcome,
};
pub use ids::{
    MAX_MODEL_ID_BYTES, MAX_PROVIDER_ID_BYTES, MAX_TOOL_CALL_ID_BYTES, ModelId, ProviderId,
    ProviderToolCallId, SYNTHESIZED_TOOL_CALL_ID_PREFIX,
};
pub use message::{ContentPart, ModelMessage, ModelRequest, Role, ToolDefinition};
pub use provider::ModelProvider;
