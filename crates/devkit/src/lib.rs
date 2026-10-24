//! Native project management. Ecosystem resolvers select versions; this crate
//! admits their exact inputs and preserves the product graph transactionally.
pub mod catalog;
pub mod compose;
pub mod files;
pub mod graph;
pub mod model;
pub mod native;
pub mod project;
pub mod projections;
pub mod release;
pub mod state;
pub mod transaction;

pub type Result<T> = anyhow::Result<T>;

pub mod archive;
pub mod assembler;
pub mod cache;
pub mod cli;
pub mod delivery;
pub mod desktop;
pub mod distribution;
pub mod download;
mod lifecycle_cli;
pub mod notices;
pub mod processes;
pub mod runtime;
mod runtime_cli;
pub mod rust_gate;
