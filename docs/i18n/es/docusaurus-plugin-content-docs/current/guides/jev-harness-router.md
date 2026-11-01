---
sidebar_position: 10
title: Enrutador Jev 2D de arneses
description: Enruta turnos del Protocolo Universal de Arneses (UHP) en una cuadrícula 2D (arnés × modelo) con TypeSafe System One (Jev), fija la base del arnés durante las continuaciones de sesión y recalifica el nivel de modelo en cada turno.
---

Usa el enrutador Jev 2D de arneses cuando tus aplicaciones envíen turnos agénticos mediante el
Protocolo Universal de Arneses (`POST /v1/responses`) y necesiten que Synapse elija **tanto** el
entorno de ejecución del agente (`pi`, `claude-code`, `codex`, `hermes`, `systemone`) como el
nivel de dificultad del modelo en una sola llamada a TypeSafe System One (`/v1/systemone`).

Mientras que el [enrutador Jev](jev-router.md) 1D puntúa una solicitud de chat en un único eje de
`difficulty`, una ruta 2D de arnés en `config/harnesses.toml` evalúa dos ejes ortogonales a la vez:

- **Eje 1 — Arquetipo de trabajo (`profiles`)**: pregunta `choice` sobre perfiles de trabajo con nombre (`quick_script`, `repo_engineering`, `deep_research`), cada uno asociado a una lista ordenada de bases de arnés preferidas.
- **Eje 2 — Dificultad de la tarea (`tiers`)**: pregunta `score` sobre niveles de dificultad ordenados (`trivial` → `hard`), cada uno con `effort`, `max_step`, `timeout_seconds` y tramos de modelo (que pueden referenciar alias existentes de `routes.toml` mediante `{ route = "..." }`), además de los modificadores `noul` `needs_reasoning` y `needs_deep_horizon`.

## Configurar `config/harnesses.toml`

`config/harnesses.toml` declara tres secciones:

1. `[bases.<id>]`: capacidades en tiempo de ejecución y protocolos de cable soportados por cada binario de arnés.
2. `[harnesses.<id>]`: semillas de arnés UHP devueltas por `GET /v1/harnesses`. Definir `base = "auto"` delega la selección de base a `route`.
3. `[harness_routes.<alias>]`: tablas de enrutamiento estáticas o 2D con `strategy = "jev"`.

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

## Cómo se toma una decisión 2D

1. **Prefiltrado de capacidades**: Antes de llamar a Jev, Synapse descarta cualquier `base` que no cumpla las capacidades requeridas (como `environments`, `skills`, transportes MCP o `tool_enforcement = "hard"` cuando `disabled_tools` no está vacío) y descarta cualquier par `(base, leg)` cuyo proveedor y modelo no compartan ningún protocolo de cable con `base.wire` o no cumplan las restricciones de visión o funciones nativas.
2. **Llamada única a `/v1/systemone`**: Synapse envía un estado UHP acotado (`latest_user_message`, `recent_history`, `system_prompt`, extensiones de archivos adjuntos, conteos de MCP/skills e `is_continuation`) y formula hasta cuatro preguntas en una sola solicitud:
   - `profile` (`choice`): qué arquetipo de trabajo se ajusta mejor a la tarea (se omite en turnos de continuación o cuando sobreviven menos de 2 perfiles al prefiltrado).
   - `difficulty` (`score`): cuán exigente es el turno frente a las descripciones de los niveles.
   - `needs_reasoning` (`noul`): si se requiere razonamiento paso a paso; al alcanzar o superar `reasoning_threshold`, incrementa un paso el `effort` del nivel.
   - `needs_deep_horizon` (`noul`, cuando se configura `horizon_threshold`): si la tarea requiere múltiples rondas iterativas de herramientas; al alcanzar o superar `horizon_threshold`, eleva `max_step` al del siguiente nivel más difícil.
3. **Verificaciones de confianza independientes**: la confianza de `profile` se compara con `min_profile_confidence` y la de `difficulty` con `min_confidence`. Si la confianza de `profile` es baja pero la de `difficulty` es alta, Synapse recurre a `default_profile`, conserva el nivel decidido e incluye `x-synapse-routing-degraded: low_profile_confidence`.

## Fijación de sesión en `previous_response_id`

Una sesión de arnés mantiene un espacio de trabajo activo y una transcripción nativa en su ejecutor.
Cuando un cliente envía `previous_response_id` en el turno 2 o posterior:

- **La base queda fijada**: Synapse bloquea `pinned_base` en la base de arnés que atendió el turno 1 y omite la pregunta `profile` de la llamada a Jev.
- **El nivel se recalifica en cada turno**: Jev sigue puntuando `difficulty`, `needs_reasoning` y `needs_deep_horizon` en cada continuación. Una sesión que comienza con una refactorización arquitectónica compleja en `claude-code + gemini-3.1-pro-preview` (`hard`) puede bajar a `claude-code + gemini-3.5-flash-lite` (`trivial`) cuando el usuario continúa con *"Gracias, ahora formatea la entrada del changelog"*.
- **Protección contra discrepancias**: Enviar un `metadata.harness_id` distinto en un turno de continuación devuelve `409 Conflict` (`harness_mismatch`). Los turnos concurrentes sobre la misma sesión devuelven `409 Conflict` (`session_busy`).

## Conmutación por error: base externa, nivel interno

Cuando un ejecutor o un tramo de modelo falla de forma reintentable, Synapse recorre la cuadrícula
2D sobreviviente con la **base de arnés como bucle externo** y
**`escalation_order(tiers.len(), start_tier)` como bucle interno**:

1. Mantiene la base de arnés elegida y escala hacia niveles más difíciles, luego hacia niveles más sencillos.
2. Solo si todos los tramos de modelo compatibles en la base primaria fallan (o el propio binario del ejecutor no está disponible), avanza a la siguiente base del perfil elegido, luego a `default_profile` y finalmente a los perfiles restantes.

## Cabeceras de respuesta, libro mayor y métricas

Cada respuesta de `POST /v1/responses` incluye:

| Cabecera | Valor |
|---|---|
| `x-synapse-routing` | `jev`, `static` o `static_override` |
| `x-synapse-harness-base` | Base de arnés que atendió el turno (`pi`, `claude-code`, `codex`, `hermes`, `systemone`) |
| `x-synapse-profile` | Perfil que atendió el turno |
| `x-synapse-profile-decided` | Presente cuando la conmutación por error cambió a un perfil distinto del elegido por Jev |
| `x-synapse-tier` | Nivel de dificultad de modelo que atendió el turno |
| `x-synapse-tier-decided` | Presente cuando la conmutación por error cambió a un nivel distinto del elegido por Jev |
| `x-synapse-reasoning-effort` | Esfuerzo de razonamiento efectivo (`none`..`max` o `client`) |
| `x-synapse-max-step` | Presupuesto de pasos efectivo para el turno |
| `x-synapse-routing-degraded` | `low_confidence`, `low_profile_confidence`, `timeout`, `error` o `unavailable` |

Cada decisión 2D de Jev registra una fila `route_decision` en el libro mayor de costos con
`lane = "jev"`, `thread = session_id`, `message = response_id` y `tier = "<profile>/<base>/<tier>"`,
seguida de una fila de turno `uhp` (`op = "harness_turn"`). Prometheus exporta
`synapse_harness_routing_decisions_total{route,profile,base,tier,outcome}`,
`synapse_harness_turns_total{harness_id,base,model,status}` y
`synapse_harness_turn_duration_seconds`.
