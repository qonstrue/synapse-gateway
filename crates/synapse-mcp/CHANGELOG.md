# Changelog


## 0.1.0

- Initial release. On-demand MCP gateway for the `synapse-proxy` sandbox broker: transparent per-server routing over Streamable HTTP, a TTL-scoped upstream registry with admin routes, and per-session tenant-identity injection driven by a generic `McpGatewayConfig` (context-key → header mapping) resolved from the shared `ContextStore`. Loopback-only with DNS-rebinding protection.
