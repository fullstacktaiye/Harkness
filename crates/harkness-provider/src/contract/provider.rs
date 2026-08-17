//! The one trait every model endpoint implements.

use harkness_git::Cancellation;

use super::{
    ModelEventSink, ModelId, ModelRequest, ProviderCapabilities, ProviderError, ProviderId,
    TurnOutcome,
};

/// A model endpoint: messages and tool definitions in, streamed text and tool
/// call *requests* out.
///
/// # What an implementation is, and is not
///
/// A provider produces text and asks for tools. It has no filesystem, Git,
/// process, or credential access and executes nothing — a tool call it emits is
/// a request the runtime is free to refuse. It is therefore not the Harkness
/// agent, which owns the loop, and not an external coding agent, which owns its
/// own loop and edits files itself. ADR-0002 fixes those three as separate
/// contracts, and no type in this workspace implements two of them.
///
/// # Obligations
///
/// - **Blocking.** [`stream`](Self::stream) runs on the caller's worker thread
///   and returns when the turn is over. The workspace has no async runtime
///   (ADR-0003), so an implementation that wants concurrency owns its own
///   threads and still returns from this call synchronously.
/// - **Cancellation-polled.** Check `cancellation` at least every
///   [`CANCELLATION_POLL_INTERVAL`](super::CANCELLATION_POLL_INTERVAL) and
///   between events, return
///   [`ProviderError::Cancelled`](super::ProviderError::Cancelled), and deliver
///   nothing to the sink after the poll that observed it. The token is the
///   workspace's own, not a second mechanism, so an implementation passes down
///   the token it was handed rather than translating.
/// - **Assembled once.** The returned [`TurnOutcome`] carries the assembled
///   turn, so no caller re-derives it from the events it saw. Running the
///   events through [`TurnDriver`](crate::assemble::TurnDriver) is how an
///   implementation gets every rule in this module for free; writing one by
///   hand means holding to all of them by hand.
/// - **Wire types stay private.** Whatever an adapter parses off its endpoint is
///   its own; nothing provider-shaped may appear in this contract, in
///   `harkness-runtime`, or in anything persisted. That is the boundary this
///   crate exists to be.
///
/// # Sharing
///
/// `&self` on every method, so one provider serves many turns. `Send` lets a
/// coordinator own one on a run worker; an implementation holding a connection
/// pool synchronizes it internally rather than requiring a mutable borrow that
/// would serialize unrelated runs.
pub trait ModelProvider: Send {
    /// Stable identity of this adapter.
    fn id(&self) -> ProviderId;

    /// What this provider claims `model` supports.
    ///
    /// Answering [`ProviderCapabilities::unknown`] is correct and expected for
    /// a model the adapter has never been told about. It is never a reason to
    /// refuse a request: callers degrade conservatively instead.
    fn capabilities(&self, model: &ModelId) -> ProviderCapabilities;

    /// Runs one turn, delivering each event to `sink` as it happens.
    ///
    /// # Errors
    ///
    /// Returns one of the ten [`ProviderError`] kinds. A failure means no
    /// assistant turn was produced; it never means a workspace changed, because
    /// a provider cannot change one.
    fn stream(
        &self,
        request: &ModelRequest,
        sink: &mut dyn ModelEventSink,
        cancellation: &Cancellation,
    ) -> Result<TurnOutcome, ProviderError>;
}
