//! synapse — LLM router and gateway.
pub mod config;
pub mod embeddings;
pub mod error;
pub mod gateway;
pub mod ledger;
pub mod observability;
pub mod pricing;
pub mod providers;
pub mod resilience;
pub mod routing;
#[cfg(feature = "server")]
pub mod server;
pub mod vertex_native;
