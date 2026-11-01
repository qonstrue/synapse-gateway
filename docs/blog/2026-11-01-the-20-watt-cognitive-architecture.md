---
slug: the-20-watt-cognitive-architecture
title: "The 20-Watt cognitive architecture: towards AGI with System 1 decision models and reusable skills"
authors: [rajwilkhu]
tags: [routing, tooling]
date: 2026-11-01T17:00
description: Why scaling towards AGI requires reversing today's System-2-only LLM stack—combining sub-100ms System 1 decision models (TypeSafe Jev), reusable agent skills, and Synapse's 2D (harness × model) UHP gateway to approach the 20-Watt efficiency of biological intelligence.
---

Modern artificial intelligence infrastructure burns kilowatts of power per rack to generate tokens
sequentially, one word at a time. Yet as neuroscientist Henning Beck points out, the human brain
operates on roughly **20 Watts of power**—barely enough to run a dim lightbulb—while covering the
entire spectrum of perception, motor control, abstract learning, and creative reasoning.

How does biological intelligence achieve that versatility without melting under its energy budget?
It does not calculate every action from first principles. Instead, the brain acts as a
**default-interventionist machine**: it continuously matches incoming state against a repertoire of
learned **skills**, firing rapid, low-energy heuristics for routine work and reserving
energy-intensive conscious deliberation strictly for novel, out-of-distribution challenges.

If autonomous software agents are going to scale towards AGI without hitting a latency and energy
wall, our infrastructure has to adopt the same dual-system architecture.

<!-- truncate -->

## We built the cognitive stack backwards

In cognitive psychology, Daniel Kahneman described two modes of thought:

- **System 1**: fast, instinctive, parallel, automatic, and ultra-low-energy.
- **System 2**: slow, deliberate, sequential, logical, and metabolically expensive.

As researcher Georgije Stanisic highlights in his analysis of AGI architecture, modern AI has built
this cognitive stack backwards. Almost every production agent today routes 100% of its operational
cycles through Large Language Models acting as System 2 reasoners.

Invoking a frontier 70B+ parameter LLM to classify state, route an intent, screen a guardrail, or
pick a tool is the computational equivalent of hiring a committee of professors to flip a light
switch. It introduces multi-second latency, runaway token spend, and probabilistic fragility
(hallucinated JSON keys, drifted schemas) at the exact layer of the stack that should be a reflex.

| Cognitive Attribute | System 1 (Decision Models / Jev) | System 2 (Deliberate LLMs) |
|---|---|---|
| **Primary Function** | Fast intuitive reflex & schema-bound decisions | Deep deliberation, complex synthesis & generation |
| **Execution Speed** | Sub-100 ms (`70–500 ms` parallel sampling) | Multi-second sequential token generation |
| **Energy & Compute Cost** | Ultra-low energy (20-Watt-aligned efficiency) | High compute & electricity footprint |
| **Output Space** | Type-safe structured probabilities (no raw strings) | Unconstrained natural language text & code |
| **Hallucination Risk** | Zero (bounded by strict schema validation) | Variable (requires external guardrails & verification) |
| **Role in the AGI Stack** | Default operational filter, router & safety gate | Escalation fallback for novel, non-routine reasoning |

## System 1 decision models: the mechanics of Jev

To operationalize System 1 in software agents, TypeSafe AI introduced **Jev**—a **System One
Model** designed to *decide* rather than write prose. Instead of sampling tokens auto-regressively
for human eyes, a System 1 decision model evaluates a bounded state against typed questions
(`choice`, `score`, `noul`) and returns machine-readable, epistemically calibrated probability
distributions:

1. **Parallel decision sampling**: Rather than generating a paragraph token by token, Jev evaluates
   multiple structured questions in parallel in a single pass—up to two orders of magnitude faster
   than a deliberative LLM turn.
2. **Zero schema hallucination**: Because the output space is strictly constrained to the declared
   question types (`choice` options, ordinal `score` criteria, or `[0, 1]` `noul` probabilities),
   the model cannot invent field names, emit broken JSON, or drift off schema.
3. **Epistemically calibrated confidence**: Every decision carries a calibrated confidence score,
   giving systems a deterministic threshold (`min_confidence`) to know *when System 1 knows* and
   *when it must escalate to System 2*.
4. **Uninterrupted agentic flow**: Just as real-time multimodal architectures (such as Google's
   Gemini Live Avatar) rely on fast background dispatch to run tools without stalling conversation,
   sub-100 ms System 1 decisions keep the control loop responsive.

## Learned skills and agentic harnesses

In software, the biological brain's learned motor and cognitive patterns take the form of
**skills**—composable, deterministic workflows, tool definitions, and execution harnesses (`pi`,
`claude-code`, `codex`, `hermes`, `systemone`) that structure how an agent interacts with an
environment.

Rather than dropping a frontier LLM into an unconstrained loop and hoping it rediscovers the right
procedure from scratch on every turn, a dual-system cognitive architecture combines **System 1
routing**, **reusable skills**, and **gated System 2 escalation** in four stages:

```
┌────────────────────────┐
│ 1. High-Speed Ingest   │  Multimodal / UHP state arrives at Synapse
└───────────┬────────────┘
            ▼
┌────────────────────────┐
│ 2. System 1 Matching   │  Jev evaluates state against skills, profiles & tiers (<100 ms)
│    (TypeSafe Jev)      │  ──► High confidence & routine? Execute skill on fast tier
└───────────┬────────────┘
            │ Ambiguous, novel, or high-complexity
            ▼
┌────────────────────────┐
│ 3. Gated System 2      │  Escalate to deep reasoning tier + full engineering harness
│    Deliberation        │  with bumped effort & step horizon
└───────────┬────────────┘
            ▼
┌────────────────────────┐
│ 4. Skill Feedback Loop │  Distill novel System 2 solution into a reusable skill,
└────────────────────────┘  expanding the agent's low-energy System 1 repertoire
```

1. **High-speed ingestion**: Incoming state—whether an OpenAI chat turn, a multimodal stream, or a
   Universal Harness Protocol (`POST /v1/responses`) task—arrives at the gateway.
2. **System 1 skill and harness matching**: Before waking a heavy model, Synapse queries Jev to
   screen guardrails ([guard classifiers](/docs/configuration/guardrails-policy/)) and score the
   request across work archetype profiles and difficulty tiers ([1D Jev router](/docs/guides/jev-router/)
   and [2D Jev harness router](/docs/guides/jev-harness-router/)). When the task matches known
   skills at high confidence, it executes immediately on a lightweight runner and fast tier at a
   fraction of the energy and cost.
3. **Gated System 2 escalation**: When Jev detects high task difficulty, low confidence, deep
   reasoning requirements (`needs_reasoning`), or a long tool horizon (`needs_deep_horizon`),
   Synapse gates the turn up to a frontier System 2 model tier with an expanded reasoning budget
   and step limit.
4. **The skill feedback loop**: Once System 2 solves a novel problem—debugging an unfamiliar race
   condition, synthesizing a new repository workflow, or verifying a multi-step migration—that
   solution is captured and packaged into a reusable **skill**. On the next encounter, the agent
   no longer needs to reason from scratch: System 1 recognizes the pattern and dispatches the
   compiled skill directly.

## How the Synapse Harness Gateway powers the 20-Watt AGI loop

In the human brain, the **thalamus and basal ganglia** sit between sensory input and the neocortex:
they gate incoming signals, select which specialized motor or cognitive circuit to activate, and
regulate how much metabolic energy is allocated to the task.

In our software stack, Synapse's [2D (`harness × model`) UHP Harness Gateway](/docs/guides/jev-harness-router/)
serves as that executive thalamus. Clients send agentic tasks to `POST /v1/responses` with
`"model": "auto"`, and `config/harnesses.toml` maps the cognitive architecture directly onto
concrete runtimes, skills, and model tiers:

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

[bases.hermes]
label = "Hermes Agent"
wire = ["openai_chat"]
skills = true
environments = true
mcp_transports = ["stdio", "http", "sse"]
tool_enforcement = "instruction"

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

# Axis 1 — Work archetypes (which skill-equipped harness base to engage)
[harness_routes."auto".profiles.skill_reflex]
description = "Routine tasks covered by existing skills, quick shell checks, or single-file edits"
bases = ["pi", "codex"]

[harness_routes."auto".profiles.repo_engineering]
description = "Multi-file software engineering, refactoring, test execution, and git workflows"
bases = ["claude-code", "codex", "pi"]

[harness_routes."auto".profiles.deep_research]
description = "Open-ended research, multi-source synthesis, and long-horizon exploration"
bases = ["hermes", "claude-code", "pi"]

# Axis 2 — Metabolic budget (model tier, reasoning effort, and step horizon)
[[harness_routes."auto".tiers]]
name = "trivial"
description = "Routine skill execution, status checks, or single-command lookups"
effort = "none"
max_step = 5
timeout_seconds = 60
legs = [{ route = "lite" }]

[[harness_routes."auto".tiers]]
name = "moderate"
description = "Standard feature work, test writing, and multi-file edits"
effort = "medium"
max_step = 30
timeout_seconds = 600
legs = [{ route = "fast" }, { route = "deep" }]

[[harness_routes."auto".tiers]]
name = "hard"
description = "Novel architecture, subtle concurrency bugs, or synthesizing a new skill from scratch"
effort = "high"
max_step = 60
timeout_seconds = 1200
bases = ["claude-code", "codex"]
legs = [{ route = "deep" }]
```

Every turn flowing through the Harness Gateway executes five concrete mechanisms that mirror the
20-Watt cognitive design:

### 1. Structural capability gating before any model call

Before spending even a single System 1 or System 2 token, `eligible_grid` prunes the `(base × leg)`
matrix against the request's structural constraints:

- Does the turn attach a workspace (`env_id`) or require **skills** (`skills = true`)?
- Does it attach MCP servers over `stdio`, `http`, or `sse`?
- Does the harness configuration enforce hard tool blocks (`disabled_tools` requiring `tool_enforcement = "hard"`)?
- Do the harness base and upstream model leg share a native wire protocol (`vertex_anthropic`, `gemini_native`, `openai_responses`, `openai_chat`)?

Incompatible cells are eliminated deterministically in microseconds.

### 2. One sub-100ms System 1 call across two orthogonal axes

Next, Synapse sends a bounded UHP state (`latest_user_message`, `recent_history`, `system_prompt`,
`skill_count`, `mcp_server_count`, `has_environment`, and `is_continuation`) to TypeSafe System One
(`/v1/systemone`) with up to four typed questions in a single parallel pass:

- **`profile` (`choice`)**: Selects the work archetype (`skill_reflex`, `repo_engineering`, or `deep_research`), choosing which harness runtime owns the tool loop.
- **`difficulty` (`score`)**: Rates the metabolic budget required (`trivial` → `hard`).
- **`needs_reasoning` (`noul`)**: Bumps the tier's reasoning `effort` one step (`none` → `minimal` → `low` → `medium` → `high` → `xhigh` → `max`) when step-by-step deduction is required.
- **`needs_deep_horizon` (`noul`)**: Bumps `max_step` to the next harder tier's step budget when the task requires many iterative tool rounds.

When an incoming UHP request matches an existing **skill** at high confidence, Jev routes it to
`profile = "skill_reflex"` (`pi`) + `tier = "trivial"` (`gemini-3.5-flash-lite`, `effort = "none"`,
`max_step = 5`). When the task is novel or ambiguous—for instance, `profile_confidence` drops below
`min_profile_confidence` or `difficulty` scores in the `hard` band—the gateway gates the request up
to `claude-code` on the `deep` tier with `effort = "high"` and a 60-step horizon.

### 3. Pinned working memory with elastic per-turn down-shifting

A biological brain does not rebuild its working memory from scratch on every sentence of a
conversation, yet it immediately relaxes metabolic effort once the hard part of a task is solved.

The Harness Gateway implements this through `previous_response_id` session pinning:

- **Turn 1 (Novel System 2 synthesis)**: An engineer asks the agent to diagnose a distributed lock
  bug and author a reusable skill for future regressions. Jev selects `repo_engineering` (`claude-code`)
  + `hard` (`gemini-3.1-pro-preview`, `effort = "high"`, `max_step = 60`). The runner solves the bug
  and writes the new `SKILL.md` into the workspace.
- **Turn 2 (Routine follow-up on the same workspace)**: The engineer sends `previous_response_id`
  with *"Great—commit the new skill and open a PR."* Because `previous_response_id` is present,
  Synapse **pins `base = "claude-code"`** (preserving the live container, git state, and loaded
  skills) and omits the `profile` question from Jev, while **re-scoring `difficulty`**. Jev rates
  Turn 2 as `trivial`, down-shifting the model inside the pinned `claude-code` session to
  `gemini-3.5-flash-lite` (`effort = "none"`, `max_step = 5`).

### 4. Base-outer, tier-inner resilience

If an upstream model leg experiences a transient `429` or `503` mid-session, switching agent
harnesses would lose the runner's in-memory context. Synapse's `order_grid` planner therefore walks
the fallback grid with **harness base as the outer loop** and
**`escalation_order(tiers.len(), start_tier)` as the inner loop**—escalating to adjacent model
tiers on the *same* harness base before ever failing over to a secondary base in the profile.

### 5. Flight-recorder telemetry for the skill distillation loop

Every turn emits response headers (`x-synapse-harness-base`, `x-synapse-profile`, `x-synapse-tier`,
`x-synapse-reasoning-effort`, `x-synapse-max-step`), Prometheus counters
(`synapse_harness_routing_decisions_total`, `synapse_harness_turns_total`), and paired ledger rows:

- A `route_decision` row (`lane = "jev"`, `tier = "<profile>/<base>/<tier>"`) recording Jev's
  `score`, `confidence`, and `outcome`.
- A `harness_turn` row (`lane = "uhp"`) recording the tokens, turns, and USD cost spent by the
  runner, joined via `thread = session_id` and `message = response_id`.

By querying the ledger for sessions that repeatedly trigger `hard` System 2 tiers on similar task
types, teams can identify exactly which recurring workflows should be distilled into new **skills**—
steadily shifting traffic from expensive System 2 deliberation into fast, low-watt System 1 skill
dispatch.

## Why this matters for the road to AGI

The path to general-purpose autonomous intelligence will not be won by making every single action a
100-watt, 10-second chain-of-thought monologue. It will be won by systems that **think fast by
default and think slow on demand**—using sub-100 ms System 1 decision models to guard, route, and
trigger reusable **skills** for 90% of operational turns, while reserving deep System 2
deliberation for the 10% of problems where genuine invention happens.

That is the architecture we are building in Synapse: TypeSafe System One (Jev) as the fast
reflex layer, `routes.toml` and `harnesses.toml` as the calibrated escalation spine, and
skill-equipped agent harnesses as the hands that turn decisions into action.

## References

- Beck, Henning. (2026). *The Brain Works with 20 Watts: Energy Efficiency and Human Cognition*. Munich Re Interview Series.
- Stanisic, Georgije. (2026). *Why Jev Is the Next Step to AGI*. Medium Research Series.
- TypeSafe AI Blog. (2026). *Introducing System One Models & Jev: AI That Decides Instead of Writing*.
- Google AI. (2026). *Introducing Gemini 3.8 Live with Live Avatar: Real-Time Multimodal Streaming & Asynchronous Tool Execution*.
- AGI Jetzt. (2026). *System One & Decision Models: Machine-Readable Probabilities in Programmatic Workflows*.
