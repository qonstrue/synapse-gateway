//! Re-export of the shared `synapse-context` crate.
//!
//! The actual `ContextStore`/`ResolvedContext` implementation lives in the
//! leaf crate `synapse-context` so that other crates (e.g. `synapse-mcp`,
//! mounted independently by a downstream broker) can depend on it directly
//! (they need the exact same type, not a fork) without creating a cyclic
//! package dependency on `synapse-proxy`. Re-exporting here keeps every
//! existing `synapse_proxy::context::{ContextStore, ResolvedContext}` import
//! path unchanged.
pub use synapse_context::{ContextStore, ResolvedContext};
