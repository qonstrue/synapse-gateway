---
slug: jev-2d-harness-routing
title: "Enrutamiento Jev 2D: elegir tanto el arnés de agente como el nivel de modelo en una sola llamada"
authors: [rajwilkhu]
tags: [routing, observability]
date: 2026-11-01T09:00
description: Cómo el enrutador Jev 2D (arnés × modelo) de Synapse selecciona un perfil de arquetipo de trabajo y un nivel de dificultad de modelo en una sola llamada a TypeSafe System One, fija el arnés durante sesiones multiturno y ajusta el nivel de modelo en turnos posteriores.
---

Cuando lanzamos el [enrutador Jev 1D](/blog/jev-routing-explained), la decisión tenía un solo eje:
*dada una solicitud de chat, ¿cuán exigente es y cuánto esfuerzo de razonamiento necesita?* Eso
funciona muy bien cuando tu aplicación controla el bucle de prompts y llama a
`POST /v1/chat/completions`.

Las cargas de trabajo agénticas son distintas. Cuando un cliente envía una tarea mediante el
Protocolo Universal de Arneses (`POST /v1/responses`), el gateway no solo elige un LLM: también
elige el **arnés de agente** (`pi`, `claude-code`, `codex`, `hermes`, `systemone`) que ejecutará el
bucle de herramientas, el entorno aislado del espacio de trabajo, las conexiones MCP y el
presupuesto de pasos. Una comprobación rápida en consola requiere un ejecutor ligero (`pi`) sobre
un modelo rápido (`gemini-3.5-flash-lite`); una refactorización entre múltiples archivos requiere
`claude-code` o `codex` sobre un modelo de frontera con un horizonte de 60 pasos; una síntesis
abierta requiere `hermes`.

Hoy presentamos el **enrutamiento Jev 2D (`arnés × modelo`)** en `config/harnesses.toml`: una sola
llamada a TypeSafe System One (`/v1/systemone`) que selecciona tanto el **perfil de arquetipo de
trabajo** (Eje 1) como el **nivel de dificultad de la tarea** (Eje 2), fija la base del arnés en las
continuaciones de sesión y recalifica la dificultad en cada turno.

<!-- truncate -->

## Por qué arnés × modelo es una decisión 2D

Tratar `(arnés, modelo)` como una lista plana 1D de pares presenta tres problemas:

1. **El Eje 1 (Arquetipo de trabajo) es categórico, mientras que el Eje 2 (Dificultad) es ordinal.**
   Elegir entre `quick_script`, `repo_engineering` y `deep_research` es una pregunta `choice` entre
   estilos de trabajo cualitativos. Elegir entre `trivial`, `easy`, `moderate` y `hard` es un
   `score` ordenado de `0` a `N-1`.
2. **No todas las bases de arnés hablan el protocolo de cable de todos los proveedores.**
   `claude-code` habla `anthropic_messages`, `vertex_anthropic` y `openai_chat`; `codex` habla
   `openai_responses` y `openai_chat`; `pi` habla `gemini_native` y `openai_chat`. Prefiltrar las
   celdas `(base, leg)` incompatibles antes de llamar a Jev garantiza que el planificador nunca
   seleccione una combinación inválida.
3. **Las sesiones multiturno deben fijar la base del arnés sin fijar el nivel del modelo.** Una vez
   que el turno 1 inicia una sesión en `claude-code` dentro de un contenedor de trabajo, el turno 2
   (`previous_response_id`) debe permanecer en `claude-code` porque allí residen el estado y la
   transcripción de herramientas; pero si el turno 2 es solo *"Gracias, formatea eso como una lista
   de viñetas"*, no debería seguir pagando por el nivel más costoso.

## Una llamada a `/v1/systemone`, cuatro preguntas tipadas

En `config/harnesses.toml`, una ruta con `strategy = "jev"` declara tanto `profiles` como `tiers`:

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

En el turno 1, Synapse prefiltra la cuadrícula según las capacidades requeridas por la solicitud
(visión, entorno adjunto, transportes MCP y bloqueo estricto de herramientas) y envía a Jev una
sola solicitud `/v1/systemone` con hasta cuatro preguntas:

- `profile` (`choice`): selecciona el arquetipo de trabajo entre los perfiles elegibles.
- `difficulty` (`score`): puntúa el turno frente a las descripciones de los niveles.
- `needs_reasoning` (`noul`): incrementa un paso el `effort` del nivel cuando `>= reasoning_threshold`.
- `needs_deep_horizon` (`noul`): eleva `max_step` al presupuesto de pasos del siguiente nivel más difícil cuando `>= horizon_threshold`.

La confianza se evalúa **de forma independiente** en cada eje. Si Jev tiene alta confianza en que
una tarea es `hard` (`difficulty_confidence = 0.91`) pero baja certeza entre `repo_engineering` y
`deep_research` (`profile_confidence = 0.28`), Synapse conserva el nivel `hard`, recurre a
`default_profile` en el Eje 1 e incluye `x-synapse-routing-degraded: low_profile_confidence`.

## Fijación de sesión + recalificación por turno

Cuando un cliente continúa una sesión con `previous_response_id`, Synapse consulta el registro de la
sesión:

1. Fija `pinned_base` en la base de arnés que atendió el turno 1 y **omite la pregunta `profile`** de la llamada a `/v1/systemone`.
2. Sigue preguntando a Jev por `difficulty`, `needs_reasoning` y `needs_deep_horizon` con el `recent_history` actualizado.
3. Si el cliente envía un `metadata.harness_id` contradictorio a mitad de sesión, Synapse rechaza el turno de inmediato con `409 Conflict` (`harness_mismatch`).

Esto ofrece estado persistente de espacio de trabajo en el ejecutor junto con precios de modelo
elásticos turno a turno.

## Conmutación por error: base externa, nivel interno

Cuando un tramo falla, cambiar de base de arnés es mucho más disruptivo que escalar al siguiente
nivel de modelo dentro de la misma base. El planificador 2D ordena su secuencia de respaldo con la
**base de arnés como bucle externo** y **`escalation_order(tiers.len(), start_tier)` como bucle
interno**:

- Primero prueba los tramos del nivel elegido en la base primaria, luego niveles más difíciles en esa misma base y después niveles más sencillos en esa misma base.
- Solo si el binario del ejecutor no está disponible o todos los tramos compatibles de la base primaria fallan, Synapse pasa a la base secundaria del perfil.

## Observabilidad completa integrada

Cada turno de `POST /v1/responses` informa su decisión 2D en las cabeceras de respuesta
(`x-synapse-harness-base`, `x-synapse-profile`, `x-synapse-tier`, `x-synapse-reasoning-effort`,
`x-synapse-max-step`), escribe una fila `route_decision` (`tier = "<profile>/<base>/<tier>"`) y una
fila de turno `uhp` correlacionadas mediante `thread = session_id` y `message = response_id`, y
exporta `synapse_harness_routing_decisions_total` y `synapse_harness_turns_total` a Prometheus.

Consulta la [guía completa del enrutador Jev 2D de arneses](/docs/guides/jev-harness-router/) para
configurar `config/harnesses.toml` en tu despliegue.
