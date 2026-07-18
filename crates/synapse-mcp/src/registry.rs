//! On-demand, TTL-scoped registry of upstream MCP servers.
//!
//! Mirrors the `ContextStore` pattern in `synapse-proxy::context`: a sync
//! lock guards a plain map, expiry is tracked via `Instant`, and a
//! `resolve_at(now)` seam makes TTL expiry deterministically testable. No
//! lock is ever held across an `.await` — all operations here are
//! synchronous by construction.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// A single registered upstream MCP server.
pub struct RegisteredUpstream {
    pub name: String,
    pub url: String,
    expires_at: Option<Instant>,
}

/// Registry of upstream MCP servers, keyed by name. Registering a name that
/// already exists replaces the prior entry (hot-swap) rather than erroring
/// or duplicating.
pub struct McpRegistry {
    servers: RwLock<HashMap<String, RegisteredUpstream>>,
}

impl McpRegistry {
    pub fn new() -> Self {
        Self {
            servers: RwLock::new(HashMap::new()),
        }
    }

    /// Register (or hot-swap replace) an upstream. `ttl` of `None` means no
    /// expiry.
    pub fn register(&self, name: String, url: String, ttl: Option<Duration>) {
        let expires_at = ttl.map(|d| Instant::now() + d);
        let entry = RegisteredUpstream {
            name: name.clone(),
            url,
            expires_at,
        };
        self.servers.write().unwrap().insert(name, entry);
    }

    /// Remove an upstream by name. No-op if absent.
    pub fn deregister(&self, name: &str) {
        self.servers.write().unwrap().remove(name);
    }

    /// Resolve the URL for `name` if present and unexpired. An expired entry
    /// is lazily dropped from the registry.
    pub fn resolve(&self, name: &str) -> Option<String> {
        self.resolve_at(name, Instant::now())
    }

    /// Test seam: resolve as of `now` instead of `Instant::now()`.
    pub fn resolve_at(&self, name: &str, now: Instant) -> Option<String> {
        let mut guard = self.servers.write().unwrap();
        match guard.get(name) {
            Some(entry) if entry.expires_at.map(|e| now >= e).unwrap_or(false) => {
                guard.remove(name);
                None
            }
            Some(entry) => Some(entry.url.clone()),
            None => None,
        }
    }
}

impl Default for McpRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_resolve_returns_url() {
        let r = McpRegistry::new();
        r.register("alpha".to_string(), "http://alpha.local".to_string(), None);
        assert_eq!(r.resolve("alpha"), Some("http://alpha.local".to_string()));
    }

    #[test]
    fn re_register_same_name_replaces_url_hot_swap() {
        let r = McpRegistry::new();
        r.register("alpha".to_string(), "http://old.local".to_string(), None);
        r.register("alpha".to_string(), "http://new.local".to_string(), None);
        assert_eq!(r.resolve("alpha"), Some("http://new.local".to_string()));
        // still only one entry under the name — hot-swap, not a duplicate.
        assert_eq!(r.servers.read().unwrap().len(), 1);
    }

    #[test]
    fn deregister_then_resolve_returns_none() {
        let r = McpRegistry::new();
        r.register("alpha".to_string(), "http://alpha.local".to_string(), None);
        r.deregister("alpha");
        assert_eq!(r.resolve("alpha"), None);
    }

    #[test]
    fn resolve_unknown_name_returns_none() {
        let r = McpRegistry::new();
        assert_eq!(r.resolve("missing"), None);
    }

    #[test]
    fn ttl_expiry_via_resolve_at() {
        let r = McpRegistry::new();
        let now = Instant::now();
        r.register(
            "alpha".to_string(),
            "http://alpha.local".to_string(),
            Some(Duration::from_secs(10)),
        );

        // Before expiry: resolves.
        assert_eq!(
            r.resolve_at("alpha", now + Duration::from_secs(5)),
            Some("http://alpha.local".to_string())
        );

        // After expiry: None, and the entry is dropped from the map.
        assert_eq!(r.resolve_at("alpha", now + Duration::from_secs(20)), None);
        assert!(!r.servers.read().unwrap().contains_key("alpha"));
    }
}
