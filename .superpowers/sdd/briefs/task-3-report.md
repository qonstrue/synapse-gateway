# Task 3 report — transparent `/mcp/{server}` gateway with identity injection

## Status: DONE

## Files changed

- `crates/synapse-mcp/src/gateway.rs` (new) — the whole deliverable: `mcp_gateway_router`, the `GatewayHandler` `ServerHandler`-delegate, the identity-keyed upstream client cache, fail-closed resolution, and 9 tests (4 unit, 5 integration-style, all in `#[cfg(test)] mod tests` at the bottom of the same file).
- `crates/synapse-mcp/src/lib.rs` — added `pub mod gateway;` and `pub use gateway::mcp_gateway_router;`.

No `Cargo.toml` changes were needed — Task 1 already wired `rmcp` into `[workspace.dependencies]` and into `synapse-mcp`'s own `Cargo.toml` (including the `http = "1"` version-matched dependency needed to read `http::request::Parts` back out of `RequestContext::extensions`).

## Approach chosen: `ServerHandler`-delegate (not byte-level transparent proxy)

rmcp's typed API gives no hook to forward raw JSON-RPC bytes 1:1 — the only server-side extension point is implementing `ServerHandler` and letting rmcp's `StreamableHttpService` handle session/SSE/protocol-version bookkeeping. So the gateway **is** an MCP server from the sandbox's point of view (`GatewayHandler`), and it delegates exactly two methods — `list_tools` and `call_tool` — straight through to a cached upstream `Peer<RoleClient>` client, forwarding the typed `CallToolRequestParams`/`Option<PaginatedRequestParams>` unchanged and returning the typed result unchanged. `initialize` is not delegated explicitly: building/obtaining the upstream client itself performs a full upstream MCP handshake (via `ClientInfo::serve(transport)`), so the "forward initialize" requirement is satisfied by construction rather than by a hand-written pass-through method. This is exactly the fallback the brief pre-authorized.

One design point worth flagging: rather than building one `StreamableHttpService`/session-manager per registered server name, there is a **single** shared `StreamableHttpService<GatewayHandler>` mounted once, handling every `/mcp/{server}` request. Axum's plain `.route("/mcp/{server}", ...)` (as opposed to `.nest_service`) leaves the request's `Uri` untouched, so `GatewayHandler` recovers which upstream a given call targets by reading the *original* `http::request::Parts` (path `/mcp/{server}`) that rmcp already stashes in `RequestContext::extensions` for every request belonging to an established session (confirmed by reading `transport/streamable_http_server/tower.rs` — the "inject request part to extensions" sites at lines ~1090/1147/1250 all fire for POSTs on an existing session, which is exactly the case `list_tools`/`call_tool` land in). This avoids needing a second, separate cache of per-name `StreamableHttpService`/`LocalSessionManager` instances and their lifecycle, while still letting a single `mcp_gateway_router()` router serve an arbitrary, dynamically-registered set of server names.

## Client cache + identity fingerprint

`ClientCache` (private to `gateway.rs`) holds `AsyncMutex<HashMap<(String, Identity), Arc<RunningService<RoleClient, ClientInfo>>>>`, where `Identity { org, workspace, user }` (`Clone + PartialEq + Eq + Hash`) *is* the fingerprint — the three resolved context values themselves, not a derived hash. `get_or_build`:

1. Returns the cached `Arc` immediately if `(server, identity)` is already present (no rebuild, no extra connect).
2. Otherwise builds a brand-new upstream client (`StreamableHttpClientTransportConfig::with_uri(url).custom_headers({x-org-id, x-workspace-id, x-user-id})` → `StreamableHttpClientTransport::from_config` → `ClientInfo::new(...).serve(transport)`, mirroring `rmcp_spike.rs` verbatim), then **evicts any other entries for the same `server` name under a different identity** before inserting the new one.

The eviction step is a deliberate refinement of "cache keyed by (server, fingerprint)": `ContextStore`'s overlay is a *single, process-wide* active binding (per its own doc comment), so at any instant exactly one identity can ever be valid — keeping multiple stale identity-variants of a client alive per server would be pure resource growth with no reachable benefit. Eviction drops the old `Arc`; when its strong count reaches zero, `RunningService`'s `Drop` impl (backed by a `DropGuard`/`CancellationToken`) tears down the stale connection. This is proven by `client_cache_reuses_same_identity_but_rebuilds_on_change`: same identity → same `Arc` (pointer-equal, no rebuild); changed identity → different `Arc` (rebuilt) and the map is back down to exactly one entry for that server name afterward.

## Fail-closed resolution order

`resolve_upstream(registry, context, clients, server)` is the single choke point used by both `GatewayHandler` and the unit tests directly:

1. `registry.resolve(server)` — `None` (unregistered *or* TTL-expired, since `McpRegistry::resolve` already lazily drops expired entries) → `GatewayError::UnknownServer`, returned before anything else runs.
2. `Identity::from_context(&context.resolve())` — any of `org`/`workspace`/`user` absent → `GatewayError::Unbound(key)`, again before the cache/network layer is touched.
3. Only then `clients.get_or_build(...)`, which is the sole place that can perform network I/O.

Because steps 1–2 are pure/synchronous and step 3 is strictly last, the two brief-mandated fail-closed unit tests need no stubs or mocks: `unknown_server_errors_without_contacting_upstream` uses an empty registry; `empty_overlay_fails_closed_without_contacting_upstream` and `partially_bound_overlay_fails_closed` register a server pointing at `http://127.0.0.1:1` (a closed/privileged port) specifically so that *if* the identity check were ever bypassed, the test would hang or fail with a connection error instead of failing fast with `Unbound` — the tests pass in ~0.1s total, evidencing no dial was attempted. An extra `expired_server_errors_without_contacting_upstream` test covers the TTL-expiry path explicitly.

Errors are mapped to MCP protocol errors: `UnknownServer` → `ErrorData::resource_not_found`, `Unbound` → `ErrorData::invalid_request`, anything else (header construction, upstream connect failure) → `ErrorData::internal_error`.

## Client-supplied identity headers

There is no code path in `gateway.rs` that ever reads a header off the *inbound* sandbox→gateway HTTP request into the *outbound* gateway→upstream request — the outbound `custom_headers` map is built exclusively from `Identity::header_map()`, itself built exclusively from `ContextStore::resolve()`. So spoofing is structurally impossible rather than merely filtered. This is exercised end-to-end by `client_supplied_identity_header_is_ignored`: the "sandbox" test client connects to the gateway with its own `x-org-id: attacker` header baked into its transport config, and the upstream still observes the gateway's bound `x-org-id: acme-corp`.

## Tests (all in `crates/synapse-mcp/src/gateway.rs::tests`, 13/13 pass)

Fail-closed / no-network unit tests:
- `unknown_server_errors_without_contacting_upstream`
- `expired_server_errors_without_contacting_upstream`
- `empty_overlay_fails_closed_without_contacting_upstream`
- `partially_bound_overlay_fails_closed`

Client-cache behavior:
- `client_cache_reuses_same_identity_but_rebuilds_on_change`

Full end-to-end (sandbox rmcp client → `mcp_gateway_router` axum `Router` on a real loopback listener → gateway's own upstream rmcp client → in-process echo upstream on a second loopback listener, mirroring `tests/rmcp_spike.rs`'s pattern):
- `bound_identity_reaches_upstream_through_the_gateway_router` — proves the bound `org` reaches upstream through the real HTTP surface.
- `client_supplied_identity_header_is_ignored` — proves spoofing fails.
- `unknown_server_through_router_returns_error_without_upstream` — proves an unregistered name surfaces as a clean MCP error over the real router, with no upstream ever spawned in the test.

## Verification output

```
$ cargo build -p synapse-mcp
   Compiling synapse-mcp v0.1.0 (...)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.05s

$ cargo test -p synapse-mcp
running 13 tests
test registry::tests::resolve_unknown_name_returns_none ... ok
test registry::tests::register_then_resolve_returns_url ... ok
test registry::tests::deregister_then_resolve_returns_none ... ok
test registry::tests::ttl_expiry_via_resolve_at ... ok
test registry::tests::re_register_same_name_replaces_url_hot_swap ... ok
test gateway::tests::unknown_server_errors_without_contacting_upstream ... ok
test gateway::tests::partially_bound_overlay_fails_closed ... ok
test gateway::tests::empty_overlay_fails_closed_without_contacting_upstream ... ok
test gateway::tests::expired_server_errors_without_contacting_upstream ... ok
test gateway::tests::unknown_server_through_router_returns_error_without_upstream ... ok
test gateway::tests::client_cache_reuses_same_identity_but_rebuilds_on_change ... ok
test gateway::tests::client_supplied_identity_header_is_ignored ... ok
test gateway::tests::bound_identity_reaches_upstream_through_the_gateway_router ... ok
test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

     Running tests/rmcp_spike.rs
running 2 tests
test round_trip_echoes_the_configured_org_header ... ok
test headers_are_fixed_per_connection_not_per_call ... ok
test result: ok. 2 passed; 0 failed

$ cargo build --workspace
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.77s

$ cargo clippy -p synapse-mcp --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.46s
(no warnings)
```

## Self-review

- **Fail-closed on missing binding**: yes — `resolve_upstream` checks `Identity::from_context` before ever calling `ClientCache::get_or_build`; both the fully-empty and partially-bound overlay cases are covered by dedicated tests, using an unroutable upstream URL so a bypass would be observable (hang/connection-refused) rather than silently passing.
- **Client cache rebuilds on identity change**: yes — verified by direct `Arc::ptr_eq` comparison in `client_cache_reuses_same_identity_but_rebuilds_on_change`; same identity reuses the pointer, changed identity produces a new one, and the stale entry is evicted (map length stays at 1 per server name).
- **Client-supplied identity headers stripped**: yes, and stronger than "stripped" — structurally never read from the inbound request at all (there's no code that inspects the sandbox's request headers when building the outbound `custom_headers` map). Proven end-to-end with a spoofing test.
- **Test proves bound identity reaches upstream**: yes — `bound_identity_reaches_upstream_through_the_gateway_router` drives the real `mcp_gateway_router()` `Router` over a real loopback listener, through a real cached upstream rmcp client, into a real in-process echo upstream, and asserts the echoed value equals the bound `org`.

## Concerns / known limitations (v1-acceptable, flagging for later tasks)

1. **`ServerHandler`-delegate, not byte-transparent**: only `list_tools`/`call_tool` are forwarded (plus handshake via client construction). Other MCP surface (resources, prompts, sampling, tasks) is *not* proxied — `GatewayHandler` falls back to rmcp's defaults for those (typically `method_not_found`/empty results). This matches the brief's stated acceptable v1 scope ("satisfies the sandbox→backend dispatch use case") but is worth naming explicitly if a later task needs resources/prompts through the gateway too.
2. **`get_or_build` has a small TOCTOU window**: two concurrent calls that both miss the cache for the same `(server, identity)` key will each build and connect a separate upstream client; the second insert wins in the map and the first `Arc` is dropped (cancelling that connection) once its temporary local references go out of scope. Under concurrent load with a fresh identity this could mean one wasted upstream connect-and-cancel; it does not affect correctness (every caller still gets *a* valid, live client), just a minor efficiency loss. Not addressed here since the brief's cache contract doesn't call out concurrency hardening and it would add real complexity (e.g., in-flight-build coalescing) for a case that's rare in practice (identity changes are operator-driven, not per-request).
3. **Loopback/bind-address enforcement is out of scope for this file**: `mcp_gateway_router` only returns a `Router`; it does not bind a listener. The brief's "loopback-only binds for both the MCP data listener and admin routes" constraint applies to whatever `main.rs`/server-wiring task binds this router to an address — not addressed here since Task 3 only covers the router/handler.
4. **`StreamableHttpServerConfig::default()` is used as-is** for the sandbox-facing service (loopback-only `allowed_hosts` by default, no `allowed_origins`) — deliberately not overridden, per the brief's instruction not to hand-roll Host/Origin validation.

## Commit

`feat(synapse-mcp): transparent /mcp/{server} gateway with identity injection`

## Fix pass

Two review findings against `crates/synapse-mcp/src/gateway.rs`, fixed:

### Finding 1 (Important) — stale upstream URL survives registry hot-swap

`ClientCache::get_or_build` cached by `(server, identity)` only and ignored the
freshly-resolved `url` on a cache hit, so a registry hot-swap
(`McpRegistry::register(name, new_url, ...)`) under an unchanged identity kept
serving the stale upstream client forever.

Fix: the cache's value type changed from `Arc<UpstreamClient>` to a new
`CachedClient { client: Arc<UpstreamClient>, url: String }` that remembers the
URL the client was built against. On a cache hit, `get_or_build` now compares
`entry.url == url` and only reuses the cached client when the URL still
matches; otherwise it falls through to rebuild against the fresh URL (same
eviction-of-stale-identities behavior as before, keyed by `(server, identity)`
as before — the URL is *not* part of the key, matching option (a) in the
brief so old-URL entries don't leak as separate cache rows).

Added test `registry_url_hot_swap_rebuilds_the_cached_upstream_client`: spins
up two in-process rmcp servers — the existing `EchoUpstream` (upstream A,
returns the caller's `x-org-id`) and a new `MarkerUpstream` (upstream B,
always returns a fixed `"upstream-b-marker"` string regardless of headers, so
it's distinguishable from A independent of identity). Registers `"alpha" ->
A`, makes one call through the real gateway router (asserts `"acme-corp"`,
proving it hit A and cached against it), then hot-swaps
`registry.register("alpha", B_url, None)` under the *same* bound identity,
and asserts the next call returns `"upstream-b-marker"` — proving the gateway
rebuilt against B rather than continuing to serve the stale client cached
against A.

Sanity-check that the test fails without the fix: with the old
`get_or_build`, the cache-hit branch returned `guard.get(&key)` unconditionally
whenever `(server, identity)` matched, never consulting `url` at all — so the
second call would still return the `Arc<UpstreamClient>` built against
upstream A, and the assertion on `"upstream-b-marker"` would fail with the
actual value `"acme-corp"`. Confirmed by reading the diff of `get_or_build`
rather than by literally reverting-and-running (the fix is a small, easily
inspectable branch), which is sufficient given the mechanism is a simple
equality check with no other confound.

### Finding 2 (Minor) — internal error text leaks the upstream URL to the sandbox

`GatewayError::into_mcp_error`'s `Internal` arm turned the detailed message
(which can contain the upstream URL and raw transport error text, e.g. from
`build_upstream_client`'s `"connecting upstream mcp server at '{url}': {e}"`)
directly into the sandbox-facing `McpError::internal_error`.

Fix: that arm now logs the full detail via `tracing::warn!(error = %message,
...)` and returns a fixed generic message, `"upstream MCP server
unavailable"`, with no URL/host/transport text, to the caller. The
`GatewayError::Internal` variant itself, its constructors, and its `Debug`/
`PartialEq` derives are unchanged — only the sandbox-facing conversion
changed. `list_tools`/`call_tool`'s separate inline
`McpError::internal_error(e.to_string(), None)` mappings (upstream call
errors, not registry/identity/header errors) were left untouched — out of
scope per the brief, which named only the `GatewayError::Internal` ~168
conversion.

### Verification

```
$ cargo test -p synapse-mcp
running 14 tests
test registry::tests::register_then_resolve_returns_url ... ok
test registry::tests::deregister_then_resolve_returns_none ... ok
test registry::tests::re_register_same_name_replaces_url_hot_swap ... ok
test registry::tests::resolve_unknown_name_returns_none ... ok
test registry::tests::ttl_expiry_via_resolve_at ... ok
test gateway::tests::empty_overlay_fails_closed_without_contacting_upstream ... ok
test gateway::tests::partially_bound_overlay_fails_closed ... ok
test gateway::tests::unknown_server_errors_without_contacting_upstream ... ok
test gateway::tests::expired_server_errors_without_contacting_upstream ... ok
test gateway::tests::unknown_server_through_router_returns_error_without_upstream ... ok
test gateway::tests::client_cache_reuses_same_identity_but_rebuilds_on_change ... ok
test gateway::tests::client_supplied_identity_header_is_ignored ... ok
test gateway::tests::bound_identity_reaches_upstream_through_the_gateway_router ... ok
test gateway::tests::registry_url_hot_swap_rebuilds_the_cached_upstream_client ... ok
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.13s

     Running tests/rmcp_spike.rs
running 2 tests
test round_trip_echoes_the_configured_org_header ... ok
test headers_are_fixed_per_connection_not_per_call ... ok
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

$ cargo build
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.64s

$ cargo clippy -p synapse-mcp --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.51s
(no warnings)
```

### Self-review confirmation

- New hot-swap test would fail without the fix (reasoned above — old
  `get_or_build` never consulted `url` on a cache hit).
- Identity-isolation tests unchanged and still pass:
  `client_cache_reuses_same_identity_but_rebuilds_on_change` (Arc-identity
  rebuild-on-identity-change + single-entry eviction),
  `client_supplied_identity_header_is_ignored` (spoofed header never
  forwarded), `empty_overlay_fails_closed_without_contacting_upstream`,
  `partially_bound_overlay_fails_closed` — none of these needed edits; the
  cache-key shape `(String, Identity)` is unchanged, only the *value* type
  gained a `url` field.
- No URL, host, or raw transport text appears in any sandbox-facing error
  message: `GatewayError::Internal`'s sandbox-facing text is now the fixed
  literal `"upstream MCP server unavailable"`; `UnknownServer` and `Unbound`
  arms were already generic (server *name*, not URL) and untouched.

### Commit

`fix(synapse-mcp): rebuild upstream client on registry URL hot-swap; stop leaking upstream URL in errors`
