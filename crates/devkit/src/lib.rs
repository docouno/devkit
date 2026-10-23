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
