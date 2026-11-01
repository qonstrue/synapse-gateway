---
slug: the-20-watt-cognitive-architecture
title: "La arquitectura cognitiva de 20 vatios: hacia la AGI con modelos de decisión de Sistema 1 y habilidades reutilizables"
authors: [rajwilkhu]
tags: [routing, tooling]
date: 2026-11-01T17:00
description: Por qué escalar hacia la AGI exige invertir la pila actual basada únicamente en el Sistema 2, combinando modelos de decisión de Sistema 1 en menos de 100 ms (TypeSafe Jev), habilidades de agente reutilizables y el gateway UHP 2D (arnés × modelo) de Synapse para acercarse a la eficiencia de 20 vatios de la inteligencia biológica.
---

La infraestructura moderna de inteligencia artificial consume kilovatios de energía por bastidor
para generar tokens de forma secuencial, palabra por palabra. Sin embargo, como señala el
neurocientífico Henning Beck, el cerebro humano opera con apenas **20 vatios de potencia** —apenas
lo suficiente para encender una bombilla tenue— y aun así abarca todo el espectro de la percepción,
el control motor, el aprendizaje abstracto y el razonamiento creativo.

¿Cómo logra la inteligencia biológica esa versatilidad sin colapsar bajo su presupuesto energético?
No calcula cada acción desde los primeros principios. En su lugar, el cerebro funciona como una
**máquina de intervención por defecto**: compara continuamente el estado entrante con un repertorio
de **habilidades** (*skills*) aprendidas, activando heurísticas rápidas y de bajo consumo para las
tareas rutinarias y reservando la deliberación consciente —intensiva en energía— estrictamente para
desafíos novedosos o fuera de distribución.

Si los agentes de software autónomos van a escalar hacia la AGI sin chocar contra un muro de
latencia y energía, nuestra infraestructura debe adoptar esa misma arquitectura de doble sistema.

<!-- truncate -->

## Construimos la pila cognitiva al revés

En psicología cognitiva, Daniel Kahneman describió dos modos de pensamiento:

- **Sistema 1**: rápido, instintivo, paralelo, automático y de consumo energético ultrabajo.
- **Sistema 2**: lento, deliberado, secuencial, lógico y metabólicamente costoso.

Como destaca el investigador Georgije Stanisic en su análisis de la arquitectura hacia la AGI, la IA
moderna ha construido esta pila cognitiva al revés. Casi todos los agentes en producción hoy en día
enrutan el 100 % de sus ciclos operativos a través de grandes modelos de lenguaje (LLM) que actúan
como razonadores de Sistema 2.

Invocar un LLM de frontera de más de 70 mil millones de parámetros para clasificar un estado,
enrutar una intención, evaluar una barrera de seguridad o elegir una herramienta es el equivalente
computacional de contratar a un comité de catedráticos para encender un interruptor de luz.
Introduce segundos de latencia, dispara el gasto de tokens y añade fragilidad probabilística (claves
JSON alucinadas o esquemas desviados) exactamente en la capa de la pila que debería ser un reflejo.

| Atributo cognitivo | Sistema 1 (Modelos de decisión / Jev) | Sistema 2 (LLM deliberativos) |
|---|---|---|
| **Función principal** | Reflejo intuitivo rápido y decisiones acotadas por esquema | Deliberación profunda, síntesis compleja y generación |
| **Velocidad de ejecución** | Menos de 100 ms (`70–500 ms` con muestreo paralelo) | Generación secuencial de tokens de varios segundos |
| **Costo energético y de cómputo** | Energía ultrabaja (eficiencia alineada con los 20 vatios) | Alta huella de cómputo y electricidad |
| **Espacio de salida** | Probabilidades estructuradas con seguridad de tipos (sin texto libre) | Texto en lenguaje natural y código sin restricciones |
| **Riesgo de alucinación** | Cero (acotado por validación estricta de esquema) | Variable (requiere barreras de seguridad y verificación externas) |
| **Rol en la pila AGI** | Filtro operativo por defecto, enrutador y compuerta de seguridad | Respaldo de escalado para razonamiento novedoso y no rutinario |

## Modelos de decisión de Sistema 1: la mecánica de Jev

Para llevar el Sistema 1 a los agentes de software, TypeSafe AI presentó **Jev**, un **modelo
System One** diseñado para *decidir* en lugar de redactar prosa. En vez de muestrear tokens de
forma autorregresiva para ojos humanos, un modelo de decisión de Sistema 1 evalúa un estado acotado
frente a preguntas tipadas (`choice`, `score`, `noul`) y devuelve distribuciones de probabilidad
legibles por máquina y epistémicamente calibradas:

1. **Muestreo paralelo de decisiones**: En lugar de generar un párrafo token por token, Jev evalúa
   múltiples preguntas estructuradas en paralelo en una sola pasada, hasta dos órdenes de magnitud
   más rápido que un turno deliberativo de un LLM.
2. **Cero alucinaciones de esquema**: Como el espacio de salida está estrictamente restringido a
   los tipos de pregunta declarados (opciones `choice`, criterios ordinales `score` o probabilidades
   `noul` en `[0, 1]`), el modelo no puede inventar nombres de campos, emitir JSON malformado ni
   salirse del esquema.
3. **Confianza epistémicamente calibrada**: Cada decisión incluye un puntaje de confianza calibrado,
   lo que brinda a los sistemas un umbral determinista (`min_confidence`) para saber *cuándo sabe
   el Sistema 1* y *cuándo debe escalar al Sistema 2*.
4. **Flujo agéntico ininterrumpido**: Al igual que las arquitecturas multimodales en tiempo real
   (como Gemini Live Avatar de Google) dependen de un despacho rápido en segundo plano para
   ejecutar herramientas sin interrumpir el diálogo, las decisiones de Sistema 1 en menos de 100 ms
   mantienen ágil el bucle de control.

## Habilidades aprendidas y arneses agénticos

En el software, los patrones motores y cognitivos aprendidos por el cerebro biológico adoptan la
forma de **habilidades** (*skills*): flujos de trabajo componibles y deterministas, definiciones de
herramientas y arneses de ejecución (`pi`, `claude-code`, `codex`, `hermes`, `systemone`) que
estructuran cómo interactúa un agente con su entorno.

En lugar de lanzar un LLM de frontera a un bucle sin restricciones esperando que redescubra el
procedimiento adecuado desde cero en cada turno, una arquitectura cognitiva de doble sistema
combina **enrutamiento de Sistema 1**, **habilidades reutilizables** y **escalado controlado al
Sistema 2** en cuatro etapas:

```
┌────────────────────────┐
│ 1. Ingesta rápida      │  El estado multimodal / UHP llega a Synapse
└───────────┬────────────┘
            ▼
┌────────────────────────┐
│ 2. Evaluación Sistema 1│  Jev evalúa el estado frente a habilidades, perfiles y niveles (<100 ms)
│    (TypeSafe Jev)      │  ──► ¿Alta confianza y rutinario? Ejecuta habilidad en nivel rápido
└───────────┬────────────┘
            │ Ambiguo, novedoso o de alta complejidad
            ▼
┌────────────────────────┐
│ 3. Deliberación        │  Escala al nivel de razonamiento profundo + arnés de ingeniería
│    Sistema 2           │  con mayor esfuerzo y horizonte de pasos
└───────────┬────────────┘
            ▼
┌────────────────────────┐
│ 4. Bucle de            │  Destila la solución novedosa del Sistema 2 en una habilidad reutilizable,
│    habilidades         │  ampliando el repertorio de bajo consumo del Sistema 1
└────────────────────────┘
```

1. **Ingesta de alta velocidad**: El estado entrante —ya sea un turno de chat OpenAI, un flujo
   multimodal o una tarea del Protocolo Universal de Arneses (`POST /v1/responses`)— llega al
   gateway.
2. **Emparejamiento de habilidades y arneses con el Sistema 1**: Antes de despertar un modelo
   pesado, Synapse consulta a Jev para filtrar barreras de seguridad
   ([clasificadores de guardrails](/docs/configuration/guardrails-policy/)) y puntuar la solicitud
   según los perfiles de arquetipo de trabajo y los niveles de dificultad
   ([enrutador Jev 1D](/docs/guides/jev-router/) y
   [enrutador Jev 2D de arneses](/docs/guides/jev-harness-router/)). Cuando la tarea coincide con
   habilidades conocidas con alta confianza, se ejecuta de inmediato en un ejecutor ligero y un
   nivel rápido a una fracción de la energía y el costo.
3. **Escalado controlado al Sistema 2**: Cuando Jev detecta alta dificultad, baja confianza,
   necesidad de razonamiento paso a paso (`needs_reasoning`) o un horizonte largo de herramientas
   (`needs_deep_horizon`), Synapse escala el turno hacia un nivel de modelo de Sistema 2 de
   frontera con un presupuesto ampliado de razonamiento y límite de pasos.
4. **El bucle de retroalimentación de habilidades**: Una vez que el Sistema 2 resuelve un problema
   novedoso —depurar una condición de carrera desconocida, sintetizar un nuevo flujo de trabajo en
   el repositorio o verificar una migración de varios pasos—, esa solución se destila y empaqueta
   en una **habilidad** reutilizable. La próxima vez que aparezca un reto similar, el agente ya no
   necesita razonar desde cero: el Sistema 1 reconoce el patrón y despacha la habilidad
   directamente.

## Cómo el gateway de arneses de Synapse impulsa el bucle AGI de 20 vatios

En el cerebro humano, el **tálamo y los ganglios basales** se sitúan entre la entrada sensorial y el
neocórtex: filtran las señales entrantes, seleccionan qué circuito motor o cognitivo especializado
activar y regulan cuánta energía metabólica se asigna a la tarea.

En nuestra pila de software, el [gateway UHP 2D (`arnés × modelo`) de Synapse](/docs/guides/jev-harness-router/)
cumple exactamente esa función de tálamo ejecutivo. Los clientes envían tareas agénticas a
`POST /v1/responses` con `"model": "auto"`, y `config/harnesses.toml` proyecta la arquitectura
cognitiva directamente sobre entornos de ejecución, habilidades y niveles de modelo concretos:

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

# Eje 1 — Arquetipos de trabajo (qué base de arnés equipada con habilidades activar)
[harness_routes."auto".profiles.skill_reflex]
description = "Routine tasks covered by existing skills, quick shell checks, or single-file edits"
bases = ["pi", "codex"]

[harness_routes."auto".profiles.repo_engineering]
description = "Multi-file software engineering, refactoring, test execution, and git workflows"
bases = ["claude-code", "codex", "pi"]

[harness_routes."auto".profiles.deep_research]
description = "Open-ended research, multi-source synthesis, and long-horizon exploration"
bases = ["hermes", "claude-code", "pi"]

# Eje 2 — Presupuesto metabólico (nivel de modelo, esfuerzo de razonamiento y horizonte de pasos)
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

Cada turno que atraviesa el gateway de arneses ejecuta cinco mecanismos concretos que reflejan el
diseño cognitivo de 20 vatios:

### 1. Filtrado estructural de capacidades antes de invocar cualquier modelo

Antes de gastar un solo token de Sistema 1 o Sistema 2, `eligible_grid` poda la matriz
`(base × leg)` frente a las restricciones estructurales de la solicitud:

- ¿El turno adjunta un espacio de trabajo (`env_id`) o requiere **habilidades** (`skills = true`)?
- ¿Conecta servidores MCP mediante `stdio`, `http` o `sse`?
- ¿La configuración del arnés exige bloqueo estricto de herramientas (`disabled_tools` con `tool_enforcement = "hard"`)?
- ¿Comparten la base del arnés y el tramo de modelo un protocolo de cable nativo (`vertex_anthropic`, `gemini_native`, `openai_responses`, `openai_chat`)?

Las celdas incompatibles se descartan de forma determinista en microsegundos.

### 2. Una llamada de Sistema 1 en menos de 100 ms sobre dos ejes ortogonales

A continuación, Synapse envía un estado UHP acotado (`latest_user_message`, `recent_history`,
`system_prompt`, `skill_count`, `mcp_server_count`, `has_environment` e `is_continuation`) a
TypeSafe System One (`/v1/systemone`) con hasta cuatro preguntas tipadas en una sola pasada
paralela:

- **`profile` (`choice`)**: Selecciona el arquetipo de trabajo (`skill_reflex`, `repo_engineering` o `deep_research`), eligiendo qué tiempo de ejecución de arnés controla el bucle de herramientas.
- **`difficulty` (`score`)**: Califica el presupuesto metabólico necesario (`trivial` → `hard`).
- **`needs_reasoning` (`noul`)**: Incrementa un paso el `effort` de razonamiento del nivel (`none` → `minimal` → `low` → `medium` → `high` → `xhigh` → `max`) cuando se requiere deducción paso a paso.
- **`needs_deep_horizon` (`noul`)**: Eleva `max_step` al presupuesto de pasos del siguiente nivel más difícil cuando la tarea exige muchas rondas iterativas de herramientas.

Cuando una solicitud UHP entrante coincide con una **habilidad** existente con alta confianza, Jev
la enruta hacia `profile = "skill_reflex"` (`pi`) + `tier = "trivial"` (`gemini-3.5-flash-lite`,
`effort = "none"`, `max_step = 5`). Cuando la tarea es novedosa o ambigua —por ejemplo, si
`profile_confidence` cae por debajo de `min_profile_confidence` o `difficulty` entra en la banda
`hard`—, el gateway escala la solicitud hacia `claude-code` en el nivel `deep` con
`effort = "high"` y un horizonte de 60 pasos.

### 3. Memoria de trabajo fijada con reducción elástica por turno

Un cerebro biológico no reconstruye su memoria de trabajo desde cero en cada frase de una
conversación, pero reduce de inmediato el esfuerzo metabólico en cuanto se resuelve la parte difícil
de una tarea.

El gateway de arneses implementa esto mediante la fijación de sesión con `previous_response_id`:

- **Turno 1 (Síntesis novedosa en Sistema 2)**: Un ingeniero pide al agente diagnosticar un error
  de bloqueo distribuido y crear una habilidad reutilizable para futuras regresiones. Jev elige
  `repo_engineering` (`claude-code`) + `hard` (`gemini-3.1-pro-preview`, `effort = "high"`,
  `max_step = 60`). El ejecutor resuelve el fallo y escribe el nuevo `SKILL.md` en el espacio de
  trabajo.
- **Turno 2 (Seguimiento rutinario en el mismo espacio de trabajo)**: El ingeniero envía
  `previous_response_id` con *"Perfecto, haz commit de la nueva habilidad y abre un PR"*. Como
  `previous_response_id` está presente, Synapse **fija `base = "claude-code"`** (conservando el
  contenedor activo, el estado de git y las habilidades cargadas) y omite la pregunta `profile` en
  Jev, pero **recalifica `difficulty`**. Jev puntúa el turno 2 como `trivial`, reduciendo el modelo
  dentro de la sesión fijada de `claude-code` a `gemini-3.5-flash-lite` (`effort = "none"`,
  `max_step = 5`).

### 4. Resiliencia con base externa y nivel interno

Si un tramo de modelo experimenta un error transitorio `429` o `503` a mitad de sesión, cambiar de
base de arnés perdería el contexto en memoria del ejecutor. Por ello, el planificador `order_grid`
de Synapse recorre la cuadrícula de respaldo con la **base de arnés como bucle externo** y
**`escalation_order(tiers.len(), start_tier)` como bucle interno**, escalando hacia niveles de
modelo adyacentes sobre la *misma* base de arnés antes de conmutar a una base secundaria del perfil.

### 5. Telemetría de caja negra para el bucle de destilación de habilidades

Cada turno emite cabeceras de respuesta (`x-synapse-harness-base`, `x-synapse-profile`,
`x-synapse-tier`, `x-synapse-reasoning-effort`, `x-synapse-max-step`), contadores de Prometheus
(`synapse_harness_routing_decisions_total`, `synapse_harness_turns_total`) y filas emparejadas en el
libro mayor de costos:

- Una fila `route_decision` (`lane = "jev"`, `tier = "<profile>/<base>/<tier>"`) que registra el
  `score`, la `confidence` y el `outcome` de Jev.
- Una fila `harness_turn` (`lane = "uhp"`) que registra los tokens, turnos y costo en USD del
  ejecutor, correlacionada mediante `thread = session_id` y `message = response_id`.

Al consultar en el libro mayor las sesiones que activan repetidamente niveles `hard` de Sistema 2
en tipos de tarea similares, los equipos pueden identificar con precisión qué flujos de trabajo
recurrentes deben destilarse en nuevas **habilidades**, desplazando de forma continua el tráfico
desde la costosa deliberación de Sistema 2 hacia el despacho rápido y de bajo consumo del Sistema 1.

## Por qué esto importa en el camino hacia la AGI

El camino hacia la inteligencia autónoma de propósito general no se ganará convirtiendo cada acción
en un monólogo de cadena de pensamiento de 100 vatios y 10 segundos. Se ganará con sistemas que
**piensen rápido por defecto y piensen despacio bajo demanda**: utilizando modelos de decisión de
Sistema 1 en menos de 100 ms para proteger, enrutar y activar **habilidades** reutilizables en el
90 % de los turnos operativos, reservando la deliberación profunda del Sistema 2 para el 10 % de
los problemas donde ocurre la verdadera invención.

Esa es la arquitectura que estamos construyendo en Synapse: TypeSafe System One (Jev) como la capa
de reflejo rápido, `routes.toml` y `harnesses.toml` como la columna vertebral de escalado calibrado,
y arneses de agentes equipados con habilidades como las manos que convierten las decisiones en
acción.

## Referencias

- Beck, Henning. (2026). *The Brain Works with 20 Watts: Energy Efficiency and Human Cognition*. Munich Re Interview Series.
- Stanisic, Georgije. (2026). *Why Jev Is the Next Step to AGI*. Medium Research Series.
- TypeSafe AI Blog. (2026). *Introducing System One Models & Jev: AI That Decides Instead of Writing*.
- Google AI. (2026). *Introducing Gemini 3.8 Live with Live Avatar: Real-Time Multimodal Streaming & Asynchronous Tool Execution*.
- AGI Jetzt. (2026). *System One & Decision Models: Machine-Readable Probabilities in Programmatic Workflows*.
