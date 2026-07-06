pub mod adapters;
pub mod drop;
pub mod hook;
mod id;
mod node;
mod registry;
mod status;
mod tree;
pub mod spawn;

pub use id::AgentId;
pub use node::{AgentNode, AgentRole};
pub use registry::AgentRegistry;
pub use status::AgentStatus;
pub use tree::{AgentTree, FlatNode};

// Only reached from cross-module test helpers (main.rs's quit-guard tests
// build a `RegisterArgs` directly); production code goes through `spawn`.
#[cfg(test)]
pub use registry::RegisterArgs;
