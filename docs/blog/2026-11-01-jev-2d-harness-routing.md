---
slug: jev-2d-harness-routing
title: "2D Jev routing: picking both the agent harness and the model tier in one call"
authors: [rajwilkhu]
tags: [routing, observability]
date: 2026-11-01T09:00
description: How Synapse's 2D (harness × model) Jev router selects a work archetype profile and a model difficulty tier in a single TypeSafe System One call, pins the harness runtime across multi-turn sessions, and down-shifts model tiers on follow-up turns.
---

When we shipped the [1D Jev router](/blog/jev-routing-explained), the question was single-axis:
*given a chat request, how demanding is it, and how much reasoning effort does it need?* That works
well when your application owns the prompt loop and calls `POST /v1/chat/completions`.

Agentic workloads look different. When a client dispatches a task over the Universal Harness
Protocol (`POST /v1/responses`), the gateway isn't just picking an LLM—it is also picking the
**agent harness** (`pi`, `claude-code`, `codex`, `hermes`, `systemone`) that will own the tool loop,
the workspace sandbox, the MCP connections, and the step budget. A quick shell check wants a
lightweight runner (`pi`) on a fast model (`gemini-3.5-flash-lite`); a cross-repository refactor
wants `claude-code` or `codex` on a frontier model with a 60-step horizon; an open-ended synthesis
task wants `hermes`.

Today we are introducing **2D (`harness × model`) Jev routing** in `config/harnesses.toml`: a
single TypeSafe System One (`/v1/systemone`) call that selects both the **work archetype profile**
(Axis 1) and the **task difficulty tier** (Axis 2), pins the harness base across session
continuations, and re-scores difficulty on every turn.

<!-- truncate -->

## Why harness × model is a 2D decision

Treating `(harness, model)` as a flat 1D list of pairs breaks down quickly:

1. **Axis 1 (Work archetype) is categorical, while Axis 2 (Difficulty) is ordinal.** Choosing between
   `quick_script`, `repo_engineering`, and `deep_research` is a `choice` over qualitative work styles.
   Choosing between `trivial`, `easy`, `moderate`, and `hard` is an ordered `score` from `0` to `N-1`.
2. **Not every harness base speaks every provider's wire protocol.** `claude-code` speaks
   `anthropic_messages`, `vertex_anthropic`, and `openai_chat`; `codex` speaks `openai_responses`
   and `openai_chat`; `pi` speaks `gemini_native` and `openai_chat`. Pre-filtering incompatible
   `(base, leg)` cells before calling Jev guarantees the planner never selects a dead cell.
3. **Multi-turn sessions must pin the harness base without pinning the model tier.** Once Turn 1
   starts a session on `claude-code` inside a workspace container, Turn 2 (`previous_response_id`)
   must stay on `claude-code` because the session state and tool transcript live there—but if Turn 2
   is just *"Thanks, format that as a bullet list"*, it shouldn't keep paying for your hardest tier.

## One `/v1/systemone` call, four typed questions

In `config/harnesses.toml`, a `strategy = "jev"` route declares both `profiles` and `tiers`:

```toml
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
```

On Turn 1, Synapse pre-filters the grid against the request's required capabilities (vision,
environment attachment, MCP transports, and hard tool enforcement) and sends Jev a single
`/v1/systemone` request containing up to four questions:

- `profile` (`choice`): picks the work archetype from surviving profiles.
- `difficulty` (`score`): scores the turn against the tier descriptions.
- `needs_reasoning` (`noul`): bumps the tier's `effort` one step when `>= reasoning_threshold`.
- `needs_deep_horizon` (`noul`): bumps `max_step` to the next harder tier's step budget when `>= horizon_threshold`.

Confidence is evaluated **independently** on each axis. If Jev is confident that a task is `hard`
(`difficulty_confidence = 0.91`) but uncertain whether it fits `repo_engineering` or `deep_research`
(`profile_confidence = 0.28`), Synapse keeps the `hard` tier, falls back to `default_profile` on
Axis 1, and stamps `x-synapse-routing-degraded: low_profile_confidence`.

## Session pinning + per-turn tier re-scoring

When a client continues a session with `previous_response_id`, Synapse looks up the session record:

1. It locks `pinned_base` to the harness base that served Turn 1 and **omits the `profile` question** from the `/v1/systemone` call.
2. It still asks Jev for `difficulty`, `needs_reasoning`, and `needs_deep_horizon` against the updated `recent_history`.
3. If the caller passes a conflicting `metadata.harness_id` mid-session, Synapse rejects the turn immediately with `409 Conflict` (`harness_mismatch`).

This gives you sticky workspace state on the runner alongside elastic per-turn model pricing.

## Base-outer, tier-inner failover

When a leg fails, switching harness bases is much more disruptive than escalating to the next model
tier on the same harness base. The 2D planner orders its fallback sequence with **harness base as
the outer loop** and **`escalation_order(tiers.len(), start_tier)` as the inner loop**:

- First, try the chosen tier's legs on the primary base, then harder tiers on that same base, then easier tiers on that same base.
- Only if the runner binary itself is down or every compatible leg on the primary base fails does Synapse move to the secondary base in the profile.

## Full observability out of the box

Every `POST /v1/responses` turn reports its 2D decision in response headers
(`x-synapse-harness-base`, `x-synapse-profile`, `x-synapse-tier`, `x-synapse-reasoning-effort`,
`x-synapse-max-step`), writes a `route_decision` row (`tier = "<profile>/<base>/<tier>"`) and a
`uhp` turn row correlated by `thread = session_id` and `message = response_id`, and exports
`synapse_harness_routing_decisions_total` and `synapse_harness_turns_total` to Prometheus.

Read the full [Jev 2D harness router guide](/docs/guides/jev-harness-router/) to configure
`config/harnesses.toml` in your deployment.
