---
sidebar_position: 10
title: Jev 2D harness router
description: Route Universal Harness Protocol (UHP) turns across a 2D (harness × model) grid with TypeSafe System One (Jev), pin the harness base across session continuations, and re-score model tiers per turn.
---

Use the Jev 2D harness router when your applications dispatch agentic turns over the Universal
Harness Protocol (`POST /v1/responses`) and need Synapse to choose **both** the right agent
runtime (`pi`, `claude-code`, `codex`, `hermes`, `systemone`) and the right model difficulty tier
in a single TypeSafe System One (`/v1/systemone`) call.

Where the 1D [Jev router](jev-router.md) scores a chat request along a single `difficulty` axis,
a 2D harness route in `config/harnesses.toml` evaluates two orthogonal axes at once:

- **Axis 1 — Work archetype (`profiles`)**: `choice` question over named work profiles (`quick_script`, `repo_engineering`, `deep_research`), each mapping to an ordered preference list of harness bases.
- **Axis 2 — Task difficulty (`tiers`)**: `score` question over ordered difficulty tiers (`trivial` → `hard`), each setting `effort`, `max_step`, `timeout_seconds`, and model legs (which can reference existing `routes.toml` aliases via `{ route = "..." }`), plus `needs_reasoning` and `needs_deep_horizon` `noul` modifiers.

## Configure `config/harnesses.toml`

`config/harnesses.toml` declares three sections:

1. `[bases.<id>]`: runtime capabilities and supported wire protocols for each harness binary.
2. `[harnesses.<id>]`: UHP harness seeds returned by `GET /v1/harnesses`. Setting `base = "auto"` delegates base selection to `route`.
3. `[harness_routes.<alias>]`: static or `strategy = "jev"` 2D routing tables.

```toml
[bases.pi]
label = "Pi Coding Agent"
wire = ["openai_chat", "gemini_native"]
skills = true
environments = true
mcp_transports = ["stdio", "http", "sse"]
tool_enforcement = "hard"

[bases.claude-code]
label = "Claude Code"
wire = ["openai_chat", "anthropic_messages", "vertex_anthropic"]
skills = true
environments = true
mcp_transports = ["stdio", "http", "sse"]
tool_enforcement = "hard"

[bases.codex]
label = "Codex CLI"
wire = ["openai_chat", "openai_responses"]
skills = true
environments = true
mcp_transports = ["stdio", "http"]
tool_enforcement = "hard"

[bases.hermes]
label = "Hermes Agent"
wire = ["openai_chat"]
skills = true
environments = true
mcp_transports = ["stdio", "http", "sse"]
tool_enforcement = "instruction"

[harnesses."chrn_default"]
name = "default-agent"
base = "auto"
route = "auto"
system_prompt = "You are a helpful software engineering agent."

[harness_routes."auto"]
policy = "default"
strategy = "jev"

[harness_routes."auto".jev_router]
model = "jev-latest"
default_profile = "repo_engineering"
default_tier = "moderate"
timeout_ms = 400
min_confidence = 0.5
min_profile_confidence = 0.5
reasoning_threshold = 0.7
horizon_threshold = 0.7

[harness_routes."auto".profiles.quick_script]
description = "Fast shell commands, file inspection, one-file edits, or quick lookups"
bases = ["pi", "codex"]

[harness_routes."auto".profiles.repo_engineering]
description = "Multi-file software engineering, refactoring, test execution, and git workflows"
bases = ["claude-code", "codex", "pi"]

[harness_routes."auto".profiles.deep_research]
description = "Open-ended research, multi-source synthesis, and long-horizon exploration"
bases = ["hermes", "claude-code", "pi"]

[[harness_routes."auto".tiers]]
name = "trivial"
description = "Greetings, status checks, or single-command file reads"
effort = "none"
max_step = 5
timeout_seconds = 60
legs = [{ route = "lite" }]

[[harness_routes."auto".tiers]]
name = "moderate"
description = "Multi-file feature work, test writing, and standard debugging"
effort = "medium"
max_step = 30
timeout_seconds = 600
legs = [{ route = "fast" }, { route = "deep" }]

[[harness_routes."auto".tiers]]
name = "hard"
description = "Complex architecture, subtle concurrency bugs, or large cross-cutting refactors"
effort = "high"
max_step = 60
timeout_seconds = 1200
bases = ["claude-code", "codex"]
legs = [{ route = "deep" }]
```

## How a 2D decision is made

1. **Capability pre-filtering**: Before calling Jev, Synapse prunes any `base` that lacks required capabilities (such as `environments`, `skills`, MCP transports, or `tool_enforcement = "hard"` when `disabled_tools` is non-empty) and prunes any `(base, leg)` pair whose provider and model have no shared wire protocol with `base.wire` or fail vision/native-feature constraints.
2. **Single `/v1/systemone` call**: Synapse sends a bounded UHP state (`latest_user_message`, `recent_history`, `system_prompt`, attachment extensions, MCP/skill counts, and `is_continuation`) and asks up to four questions in one request:
   - `profile` (`choice`): which work archetype best matches the task (omitted on continuation turns or when fewer than 2 profiles survive pre-filtering).
   - `difficulty` (`score`): how demanding the turn is against the tier descriptions.
   - `needs_reasoning` (`noul`): whether step-by-step reasoning is needed; at or above `reasoning_threshold`, bumps the tier's `effort` one step.
   - `needs_deep_horizon` (`noul`, when `horizon_threshold` is set): whether the task requires many iterative tool rounds; at or above `horizon_threshold`, bumps `max_step` to the next harder tier's `max_step`.
3. **Independent confidence checks**: `profile` confidence is checked against `min_profile_confidence` and `difficulty` confidence against `min_confidence`. If `profile` confidence is low while `difficulty` confidence is high, Synapse falls back to `default_profile`, keeps the decided tier, and stamps `x-synapse-routing-degraded: low_profile_confidence`.

## Session pinning on `previous_response_id`

A harness session owns a live workspace and native CLI transcript on its runner. When a client
sends `previous_response_id` on Turn 2+:

- **Base is pinned**: Synapse locks `pinned_base` to the harness base that served Turn 1 and omits the `profile` question from the Jev call.
- **Tier re-scores every turn**: Jev still scores `difficulty`, `needs_reasoning`, and `needs_deep_horizon` on every continuation turn. A session that starts with a deep architectural refactor on `claude-code + gemini-3.1-pro-preview` (`hard`) can down-shift to `claude-code + gemini-3.5-flash-lite` (`trivial`) when the user follows up with *"Thanks, now format the changelog entry"*.
- **Mismatch protection**: Passing a conflicting `metadata.harness_id` on a continuation turn returns `409 Conflict` (`harness_mismatch`). Concurrent turns on the same session return `409 Conflict` (`session_busy`).

## Base-outer, tier-inner failover

When a runner or upstream model leg fails retryably, Synapse walks the surviving 2D grid with
**harness base as the outer loop** and **`escalation_order(tiers.len(), start_tier)` as the inner
loop**:

1. Keep the chosen harness base and escalate to harder tiers, then easier tiers.
2. Only if every compatible model leg on the primary base fails (or the runner binary itself is unavailable), advance to the next base in the chosen profile, then `default_profile`, then remaining profiles.

## Response headers, ledger, and metrics

Every `POST /v1/responses` reply carries:

| Header | Value |
|---|---|
| `x-synapse-routing` | `jev`, `static`, or `static_override` |
| `x-synapse-harness-base` | Harness base that served the turn (`pi`, `claude-code`, `codex`, `hermes`, `systemone`) |
| `x-synapse-profile` | Profile that served the turn |
| `x-synapse-profile-decided` | Present when failover moved to a different profile than Jev chose |
| `x-synapse-tier` | Model difficulty tier that served the turn |
| `x-synapse-tier-decided` | Present when failover moved to a different tier than Jev chose |
| `x-synapse-reasoning-effort` | Effective reasoning effort (`none`..`max` or `client`) |
| `x-synapse-max-step` | Effective step budget for the turn |
| `x-synapse-routing-degraded` | `low_confidence`, `low_profile_confidence`, `timeout`, `error`, or `unavailable` |

Each 2D Jev decision records a `route_decision` row in the cost ledger with `lane = "jev"`,
`thread = session_id`, `message = response_id`, and composite `tier = "<profile>/<base>/<tier>"`,
followed by a `uhp` turn row (`op = "harness_turn"`). Prometheus exports
`synapse_harness_routing_decisions_total{route,profile,base,tier,outcome}`,
`synapse_harness_turns_total{harness_id,base,model,status}`, and
`synapse_harness_turn_duration_seconds`.
