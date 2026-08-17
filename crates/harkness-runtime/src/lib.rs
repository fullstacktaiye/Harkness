//! Typed run records and execution contracts shared by Harkness front ends.
//!
//! The runtime is organized around a containment hierarchy: a [`domain::Task`]
//! owns runs, each [`domain::Run`] owns ordered [`domain::Step`] records, and a
//! step owns [`domain::ToolCall`] records. [`domain`] is pure data and
//! lifecycle validation; [`store`] makes those records durable in a migrated
//! SQLite database under the Harkness data directory, and execution builds on
//! both.
//!
//! [`tool`] is the third piece: the typed contract every executable operation
//! implements once — a descriptor with generated JSON Schemas, a risk taxonomy,
//! structured errors, an execution context, and a registry that validates both
//! directions of a call. A [`domain::ToolCall`] records *that* a tool ran and
//! with what; [`tool`] is what defines the tool and executes it.
//!
//! [`trust`] classifies one concrete invocation — its boundary-checked paths,
//! its effects, and its force-push variant — and [`policy`] decides whether the
//! classification may proceed. The split matters: policy never accepts a
//! separately asserted risk level, only a [`trust::RequestClassification`], so
//! a caller cannot describe a request as milder than the descriptor and its
//! validated input make it.
//!
//! [`schedule`] sits above [`tool`] and decides *when* a call runs: mutations
//! of one workspace are serialized against each other, reads of it run
//! concurrently up to a cap, child processes are bounded across every run, and
//! cancelling a run reaches every call it owns down to the child's process
//! group. The executor promises one call reaches a terminal state; the
//! scheduler is what says anything at all about a second one.
//!
//! [`agent`] is the plain-data decision seam above those gates. An agent sees a
//! redacted observation and may return a tool *request*, but it receives no
//! registry, policy, approval, store, scheduler, or execution handle. The
//! deterministic [`agent::MockAgent`] exercises that same seam from versioned
//! scenarios, so its tests cannot bypass protections a future model-backed
//! implementation will face.
//!
//! [`approval`] is where a policy `Ask` becomes durable. It owns the request
//! record and its lifecycle, the frozen canonical hash a grant is bound to, the
//! matcher that decides whether an existing grant covers a new call, and the
//! channel a parked call is woken through. It is the only production source of
//! a [`policy::RunGrant`], which is what keeps "an approval exists for this
//! call" a claim one module makes rather than one any caller can assert.
//!
//! [`integration`] carries the same idea across the process boundary. An ACP
//! agent, an MCP server and its tool schemas, a recipe, a forge account and a
//! forge repository are all controlled by somebody else and can change under a
//! decision the user already made, so a grant binds to an
//! [`integration::IdentityBasis`] — hashes, endpoints, fingerprints — and a
//! pure check reports which of eight typed reasons made it stop applying.
//! Trust there is a precondition and never an authorization: a trusted subject
//! still passes [`policy`] and [`approval`] on every action it takes.

pub mod agent;
pub mod approval;
pub mod check;
pub mod coordinator;
pub mod domain;
pub mod integration;
mod json;
pub mod policy;
pub mod schedule;
pub mod store;
pub mod tool;
pub mod tools;
pub mod trust;

pub use json::canonical_json;

// Declared at the test binary's root so a scenario's frozen argv — a bare
// program name plus `--exact <role>` — names the role with no module prefix,
// exactly as it does in the integration-test binary. Without these roles
// present, `--exact` would match nothing, libtest would run zero tests and exit
// zero, and a scenario asserting `passed: true` would be satisfied by a child
// that did nothing at all.
#[cfg(test)]
harkness_test_fixtures::scenario_process_fixture_tests!();
