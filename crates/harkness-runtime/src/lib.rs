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

pub mod domain;
pub mod store;
pub mod tool;
