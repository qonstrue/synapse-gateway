---
sidebar_position: 1
title: ¿Qué es Synapse?
description: Synapse es un gateway de LLM de código abierto escrito en Rust, compatible con OpenAI hacia fuera y que mantiene internamente el carril Vertex AI nativo y el enrutamiento con Jev.
---

Synapse es un gateway de LLM de código abierto escrito en Rust. Tus clientes envían
peticiones estándar de OpenAI `POST /v1/chat/completions`; Synapse enruta cada una a través
de una cadena de fallback de proveedores definida en la configuración y registra lo que ha
costado. A diferencia de los proxies genéricos compatibles con OpenAI, mantiene un carril
Vertex AI nativo, para que las funcionalidades exclusivas de Vertex lleguen intactas, y un
carril para TypeSafe System One (Jev), un servicio alojado que responde preguntas tipadas
con decisiones estructuradas. Puedes ejecutar Synapse como un único binario o una imagen de
Docker, o embeberlo como biblioteca en un servicio Rust. Consulta
[Instalación](../get-started/installation.md).

Synapse es un workspace de Cargo. Sus crates son:

- **`synapse-gateway`**: el gateway de LLM, y el tema principal de esta documentación.
- **`synapse-proxy`**: un sidecar de proxy inverso definido por configuración, con
  enrutamiento por prefijo de path, inyección de contexto, transformaciones de petición y
  respuesta, y passthrough de streaming. Consulta
  [synapse-proxy](../synapse-family/proxy/overview.md).
- **`synapse-a2a`**: un registro de agentes agent-to-agent (A2A) con endpoints de registro
  para administración y de descubrimiento públicos, servido por el binario del gateway.
  Consulta [synapse-a2a](../synapse-family/a2a/overview.md).
- **`synapse-mcp`**: un gateway MCP bajo demanda que enruta el tráfico Streamable HTTP por
  servidor e inyecta la identidad del tenant en cada sesión. Consulta
  [synapse-mcp](../synapse-family/mcp/overview.md).
- **`synapse-context`**: el almacén de contexto compartido (una base estática más una capa
  con TTL) que usan `synapse-proxy` y `synapse-mcp`. Consulta
  [Crates del workspace](../internals/workspace-crates.md#synapse-context).

## ¿En qué se diferencia Synapse? {#why-is-synapse-different}

Synapse nació después de evaluar
[`litellm-rs`](https://github.com/majiayu000/litellm-rs) y el enfoque general de "poner un
proxy compatible con OpenAI delante de todo". Ese enfoque accede a Gemini mediante un
adaptador genérico con forma de OpenAI, que descarta la caché de contexto, los medios de
Cloud Storage (`gs://`) y los esquemas de respuesta estrictos. Synapse los conserva enviando
esas peticiones por un carril Vertex nativo dedicado, mientras que el resto de peticiones
va por el carril estándar compatible con OpenAI. Obtienes enrutamiento multiproveedor y las
funcionalidades nativas de Vertex, sin tener que elegir entre uno y otro.

Además, utiliza TypeSafe System One (**Jev**) a lo largo de todo el ciclo de vida de la
petición: una política de guardrails puede pedir a Jev que clasifique semánticamente las
entradas antes de cualquier decisión de enrutamiento o llamada a un proveedor, el router Jev
pregunta a Jev lo exigente que es cada petición para elegir un nivel de modelo y un esfuerzo
de razonamiento, y el carril Jev responde preguntas tipadas o filtra candidatos en
extracciones híbridas. Y como Synapse es un código Rust pequeño y no un framework, el código
de enrutamiento, fallback, guardrails, registro de costes y observabilidad es tuyo para
leerlo, ejecutarlo y embeberlo.

## Cuándo usar Synapse {#when-to-use-synapse}

Usa Synapse cuando:

- Dependes de funcionalidades exclusivas de Vertex, como la caché de contexto, las URIs de
  medios `gs://` o un `responseSchema` estricto, y aun así quieres una API compatible con
  OpenAI con fallback a otros proveedores.
- Quieres que cada petición se enrute al modelo y al esfuerzo de razonamiento adecuados
  para su dificultad, en lugar de fijar un modelo por caso de uso.
- Necesitas contabilidad de costes por tenant: cada petición se atribuye a un tenant y se
  valora en un registro que te pertenece.
- Quieres embeber un gateway de LLM dentro de un servicio Rust en lugar de ejecutar un
  proceso aparte.

Synapse no encaja bien cuando:

- Necesitas hoy autenticación de entrada o limitación de tasa. Synapse todavía no tiene
  ninguna de las dos; ejecútalo detrás de tu propio API gateway, ingress o service mesh.
  Consulta [Seguridad](../operating/security.md#callers-arent-authenticated) y
  [Limitaciones y hoja de ruta](../reference/limitations-roadmap.md).
- Quieres un SaaS alojado. Synapse es software autoalojado.

## Funcionalidades principales {#key-features}

Capacidades nativas:

- **Carril Vertex nativo**: `cachedContent`, URIs de medios `gs://`, `responseSchema`
  estricto y `thinking_config`, enviados directamente al endpoint `:streamGenerateContent`
  de Vertex AI. Consulta [Carril Vertex nativo](architecture.md#native-vertex-lane) y la
  [guía de Vertex nativo](../guides/native-vertex.md).
- **Llamadas a herramientas nativas**: las herramientas funcionan en los carriles estándar y
  Vertex nativo; en el carril Vertex nativo, `tool_choice` se respeta mediante `toolConfig` de Vertex. Consulta
  [Streaming y llamadas a herramientas](../guides/streaming-and-tools.md#tool-calling).
- **Carril Jev**: decisiones tipadas de Jev, además de un modo híbrido que juzga y luego
  extrae en una sola llamada. Consulta [Carril Jev](architecture.md#jev-lane) y la
  [guía del carril Jev](../guides/jev-lane.md).
- **Router Jev**: nivel y esfuerzo de razonamiento por petición, traducidos a
  `thinkingBudget` de Vertex en los tramos Vertex nativos e indicados en los encabezados de
  respuesta `x-synapse-routing` y `x-synapse-tier`. Consulta la
  [guía del router Jev](../guides/jev-router.md) y
  [Rutas Jev](../configuration/routes.md#jev-routes).
- **Router Jev 2D de arneses (`config/harnesses.toml`)**: enruta turnos del Protocolo
  Universal de Arneses (`POST /v1/responses`) en una cuadrícula 2D `(arnés × modelo)`
  —eligiendo tanto el entorno del agente (`pi`, `claude-code`, `codex`, `hermes`,
  `systemone`) como el nivel de dificultad del modelo en una sola llamada a Jev, fijando la
  base del arnés en las continuaciones con `previous_response_id` y recalificando el nivel
  de modelo en cada turno. Consulta la
  [guía del enrutador Jev 2D de arneses](../guides/jev-harness-router.md).
- **Passthrough nativo de Gemini y Jev**: reenvía llamadas del SDK `@google/genai`
  (`POST /v1beta/models/{model}:{action}`) y de TypeSafe System One
  (`POST /typesafe/v1/systemone`) con cadenas de fallback en Vertex, guardrails de entrada y
  medición en el registro de costes por tenant. Consulta la
  [referencia de la API HTTP](../reference/http-api.md).
- **Streaming real**: Synapse siempre hace streaming desde el proveedor upstream, así que
  los clientes con `stream: true` reciben eventos enviados por el servidor (SSE) token a
  token; los clientes sin streaming reciben el resultado consolidado y, en el carril
  estándar, conservan la cadena de fallback completa. Consulta
  [Streaming y llamadas a herramientas](../guides/streaming-and-tools.md).
- **Embebible**: ejecuta el binario `synapse-gateway`, o añade el crate de biblioteca como
  dependencia y llama a `Gateway::chat()` en el mismo proceso. Consulta
  [Embeber Synapse como biblioteca](../guides/embedding-as-library.md).

Compatibilidad y operación:

- **API compatible con OpenAI**: los SDKs de OpenAI existentes funcionan sin cambios,
  incluido `POST /v1/embeddings`. Consulta la
  [referencia de la API HTTP](../reference/http-api.md) y la
  [guía de embeddings](../guides/embeddings.md).
- **Fallback multiproveedor**: Vertex AI, OpenAI, Qwen (DashScope) y vLLM, Ollama o TGI
  autoalojados a través del proveedor `oai_compat`. Consulta
  [Proveedores](../configuration/providers.md) y
  [Cadenas de fallback](../guides/fallback-chains.md).
- **Registro de costes por tenant**: uso de tokens y coste por petición, escritos en SQLite
  o Postgres y, opcionalmente, distribuidos a Google Cloud Pub/Sub y AWS SNS. Consulta la
  [guía del registro de costes](../guides/cost-ledger.md) y
  [Atribución por tenant](../guides/tenant-attribution.md).
- **Observabilidad**: métricas `synapse_*` de OpenTelemetry, servidas en formato Prometheus
  y, opcionalmente, enviadas por OTLP. Consulta [Métricas](../operating/metrics.md), el
  [catálogo de métricas](../reference/metrics-catalogue.md) y el
  [panel de Grafana](../operating/grafana-dashboard.md).
- **Guardrails de entrada y salida**: políticas con nombre que combinan escáneres
  deterministas en proceso (inyección de prompts, secretos, PII, evasión de aislamiento entre
  tenants, exfiltración de datos y más), enmascaramiento `[REDACTED:...]` in situ,
  clasificación semántica opcional con Jev, escaneo de salida con retención en streaming y
  modo `observe` (dry-run). Consulta [Política de guardrails](../configuration/guardrails-policy.md).
