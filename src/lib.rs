//! tamga: a standalone SCIP orchestrator.
//!
//! This crate is organized as a small library (this file) plus a thin
//! binary (`main.rs`) that parses the CLI and dispatches into it. Keeping
//! the logic in the library makes it directly testable without shelling
//! out to the compiled binary for everything.

pub mod cli;
pub mod config;
pub mod detect;
pub mod doctor;
pub mod exec;
pub mod families;
pub mod indexers;
pub mod merge;
pub mod pipeline;
pub mod prepare;
pub mod report;
pub mod workspace;
