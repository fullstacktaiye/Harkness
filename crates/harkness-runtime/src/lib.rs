//! Typed run records and execution contracts shared by Harkness front ends.
//!
//! The runtime is organized around a containment hierarchy: a [`domain::Task`]
//! owns runs, each [`domain::Run`] owns ordered [`domain::Step`] records, and a
//! step owns [`domain::ToolCall`] records. [`domain`] is pure data and
//! lifecycle validation; [`store`] makes those records durable in a migrated
//! SQLite database under the Harkness data directory, and execution builds on
//! both.

pub mod domain;
pub mod store;
