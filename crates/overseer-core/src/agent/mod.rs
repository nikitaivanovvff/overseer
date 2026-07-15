pub mod adapters;
mod attention;
pub mod drop;
pub mod hook;
mod id;
mod node;
mod registry;
mod status;
mod tree;
pub mod spawn;

pub use id::AgentId;
pub use attention::{Attention, AttentionKind, AttentionUpdate};
pub use node::{AgentNode, AgentRole};
pub use registry::{AgentRegistry, RegistryEvent};
pub use status::AgentStatus;
pub use tree::{AgentTree, FlatNode};

// Only reached from cross-module test helpers (the `overseer` bin crate's
// tui.rs quit-guard tests build a `RegisterArgs` directly, via the
// `test-util` feature); production code goes through `spawn`.
#[cfg(any(test, feature = "test-util"))]
pub use registry::RegisterArgs;
