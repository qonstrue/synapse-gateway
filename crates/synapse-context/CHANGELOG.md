# Changelog


## 0.1.0

- Initial release. Shared bound-context store (base ⊕ TTL'd overlay), extracted from `synapse-proxy` so `synapse-proxy` and `synapse-mcp` can share the same `ContextStore` type without a cyclic dependency.
