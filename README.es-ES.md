# synapse-gateway

[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)
[![CI](https://github.com/sustentabilitas/synapse-gateway/actions/workflows/ci.yml/badge.svg)](https://github.com/sustentabilitas/synapse-gateway/actions/workflows/ci.yml)

[English](README.md) · **Español**

synapse-gateway es un router y gateway de LLM compatible con la API de OpenAI, escrito en Rust. Acepta peticiones estándar `POST /v1/chat/completions` de OpenAI y las enruta, mediante cadenas de fallback configurables, a uno de dos carriles de backend: un carril estándar (a través del crate `genai`, compatible con OpenAI, Qwen/DashScope y otros proveedores compatibles con OpenAI) o un carril nativo de Vertex AI (mediante HTTP directo a la API REST de Vertex, con soporte para contenido en caché, URIs de medios en Cloud Storage y esquemas de respuesta estrictos). Se emiten métricas de Prometheus y atributos de span `gen_ai.*` de OpenTelemetry en cada petición, y un registro de costes por tenant anota los eventos de consumo de tokens en SQLite o Postgres.

---

## ¿Por qué otro router/gateway de LLM más?

La respuesta honesta: intentamos no escribirlo. Primero evaluamos [`litellm-rs`](https://github.com/majiayu000/litellm-rs) (y el enfoque general de "poner un proxy compatible con OpenAI delante de todo") — y nos habría costado lo único a lo que no podíamos renunciar: **Vertex AI nativo**.

- **No reduce Vertex al mínimo común denominador.** `litellm-rs` y la mayoría de gateways compatibles con OpenAI acceden a Vertex/Gemini a través de un adaptador genérico con forma OpenAI, que descarta las funcionalidades específicas de Vertex de las que realmente dependemos: almacenamiento en caché de contexto (`cachedContent`), URIs de medios en Cloud Storage (`gs://`) y decodificación restringida mediante `responseSchema` nativo. synapse mantiene un **carril Vertex nativo** dedicado que habla directamente con `:generateContent` / `:streamGenerateContent`, preservando todas esas capacidades — mientras que el resto sigue usando el carril estándar compatible con OpenAI a través de [`genai`](https://crates.io/crates/genai). Obtienes enrutamiento multi-proveedor *y* el poder nativo de Vertex, sin tener que elegir entre uno y otro.

- **Es pequeño y propio, no un framework.** synapse es un único binario Rust — o un crate de biblioteca embebible (`default-features = false`, invoca `Gateway::chat()` en el mismo proceso) — con un conjunto de dependencias reducido. Dado que el código de enrutamiento, fallback, registro de costes y observabilidad es nuestro, las cosas que otros gateways no ofrecían fueron sencillas de añadir en lugar de batallas upstream: un **registro de costes por tenant** con distribución a múltiples destinos (SQLite/Postgres + Pub/Sub + SNS), y **spans `gen_ai.*` de OpenTelemetry** + métricas de Prometheus en cada petición.

- **Las partes valiosas son estándar, no complementos de pago.** El streaming es real y está activado por defecto: el gateway siempre hace streaming desde el proveedor upstream internamente, de modo que los clientes con `stream: true` reciben SSE compatible con OpenAI token a token, y los clientes sin streaming reciben esa misma respuesta consolidada en un único objeto JSON — lo que significa que *mantienen el fallback completo a lo largo de toda la cadena*. **Las llamadas a herramientas/funciones funcionan en ambos carriles.** Y como la interfaz es el estándar OpenAI, los SDKs de OpenAI existentes funcionan sin cambios. Nada de esto está bloqueado tras un nivel de precio; es la línea base.

En resumen: synapse es el gateway compatible con OpenAI *sencillo* que no te obliga a sacrificar las capacidades nativas de Vertex para obtener streaming, llamadas a herramientas, fallback multi-proveedor y contabilidad de costes.

---

## Arquitectura: dos carriles

### Carril estándar

Las peticiones sin un bloque de extensión `vertex` son gestionadas por el carril estándar, que utiliza el crate [`genai`](https://crates.io/crates/genai) como adaptador HTTP. Cualquier proveedor accesible mediante una API compatible con OpenAI (OpenAI, Qwen/DashScope, vLLM/Ollama/TGI autoalojado a través de `oai_compat`) puede aparecer en una cadena de fallback.

### Carril Vertex nativo

Si el cuerpo de la petición contiene un objeto de extensión `vertex` con alguno de los campos `cached_content`, `media_uris` o `response_schema`, la petición se enruta al carril Vertex nativo. Este carril se comunica directamente con el endpoint REST `generateContent` de Vertex AI, traduciendo el formato de mensajes de OpenAI mientras preserva las funcionalidades específicas de Vertex:

- **`cached_content`** — nombre de un recurso `cachedContents` para el almacenamiento en caché de contexto.
- **`media_uris`** — URIs de Cloud Storage (`gs://`) adjuntas como partes inline.
- **`response_schema`** — un esquema JSON pasado como `generationConfig.responseSchema` para la decodificación restringida.

Un tramo de ruta accesible únicamente por el carril estándar (es decir, sin tramo `vertex` configurado) devuelve `400 Bad Request` si se le envía una petición Vertex nativa.

### Detección de carril

```json
{
  "model": "gemini-pro",
  "messages": [...],
  "vertex": {
    "cached_content": "projects/my-project/locations/us-central1/cachedContents/abc123",
    "media_uris": ["gs://my-bucket/file.mp4"],
    "response_schema": { "type": "object", "properties": { "answer": { "type": "string" } } }
  }
}
```

La presencia de la clave `vertex` (cualquiera de sus campos) es la única señal. Las peticiones sin ella siempre van al carril estándar.

---

## Inicio rápido

### Requisitos previos

Configura las credenciales de cada proveedor referenciado en tu `config/routes.toml`:

```bash
# Vertex AI (se usan Application Default Credentials mediante google-cloud-auth)
export VERTEX_PROJECT=my-gcp-project

# Qwen / DashScope
export DASHSCOPE_API_KEY=sk-...
# export DASHSCOPE_BASE_URL=https://dashscope.aliyuncs.com/compatible-mode/v1  # opcional

# OpenAI
export OPENAI_API_KEY=sk-...
# export OPENAI_BASE_URL=https://api.openai.com/v1  # opcional

# OAI-compatible self-hosted (vLLM / Ollama / TGI)
export OAI_COMPAT_BASE_URL=http://localhost:8000/v1
# export OAI_COMPAT_API_KEY=token-xyz  # opcional
```

### Ejecución

```bash
cargo run --release
# Server: 0.0.0.0:8080
# Prometheus: 0.0.0.0:9090
```

### Petición estándar

```bash
curl -s http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-synapse-tenant: my-team" \
  -d '{
    "model": "gemini-pro",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

### Petición con streaming

```bash
curl -s http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-synapse-tenant: my-team" \
  -d '{
    "model": "gemini-pro",
    "messages": [{"role": "user", "content": "Count to 5."}],
    "stream": true
  }'
```

Las respuestas son Server-Sent Events (SSE) en el formato estándar `data: {...}` de OpenAI, finalizadas con `data: [DONE]`.

### Petición Vertex nativa

```bash
curl -s http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-synapse-tenant: my-team" \
  -d '{
    "model": "gemini-pro",
    "messages": [{"role": "user", "content": "Describe this video."}],
    "vertex": {
      "media_uris": ["gs://my-bucket/video.mp4"]
    }
  }'
```

---

## Streaming y llamadas a herramientas

### Streaming

Al establecer `"stream": true` se devuelve una respuesta de Server-Sent Events compatible con OpenAI: una secuencia de eventos `chat.completion.chunk` (cada uno con el prefijo `data: `) finalizada con `data: [DONE]`. Sin él, se devuelve un único objeto JSON `chat.completion`.

Internamente, el gateway **siempre** hace streaming desde el proveedor upstream, incluso para clientes sin streaming. Las respuestas sin streaming se almacenan en búfer completamente antes de la entrega, de modo que la cadena de fallback completa (todos los tramos) está disponible ante cualquier fallo — incluidos los fallos a mitad de stream en tramos anteriores.

### Llamadas a herramientas

Las llamadas a herramientas son compatibles en ambos carriles:

- **Carril estándar** — envía `tools` de OpenAI (array de `{type: "function", function: {name, description, parameters}}`) y opcionalmente `tool_choice`. El gateway los traduce para el crate `genai`. Nota: `tool_choice` es de mejor esfuerzo en este carril; el campo `ChatRequest` de genai 0.6 no tiene `tool_choice`, por lo que no se reenvía.
- **Carril Vertex nativo** — los `tools` se traducen a `functionDeclarations` de Vertex; `tool_choice` se respeta de forma nativa mediante `toolConfig.functionCallingConfig`.

Las respuestas incluyen `tool_calls` en el mensaje del asistente y `finish_reason: "tool_calls"`. En modo streaming, los deltas de llamadas a herramientas se emiten como eventos indexados `chat.completion.chunk` (con la misma forma que la especificación de streaming de OpenAI).

### Tiempos de espera

Dos variables de entorno acotan la latencia del stream:

| Variable | Valor por defecto | Descripción |
|----------|-------------------|-------------|
| `SYNAPSE_REQUEST_TIMEOUT_SECS` | `120` | Tiempo máximo hasta el primer fragmento (time-to-first-token). Un tramo que no produce su primer fragmento dentro de esta ventana se abandona y la cadena cae al siguiente tramo. |
| `SYNAPSE_STREAM_IDLE_TIMEOUT_SECS` | `60` | Intervalo máximo entre fragmentos consecutivos. Si no llega ningún fragmento dentro de esta ventana una vez iniciado el streaming, el tramo se termina con un error a mitad de stream. |

Ambos tiempos de espera se aplican al carril estándar. El carril Vertex nativo está actualmente limitado únicamente por el timeout del cliente HTTP subyacente (`SYNAPSE_REQUEST_TIMEOUT_SECS`); el timeout de inactividad y el fallback de primer fragmento para ese carril están pendientes como mejora futura.

---

## Endpoints

| Método | Ruta | Descripción |
|--------|------|-------------|
| `GET` | `/health` | Devuelve `200 OK` con `{"status":"ok"}`. |
| `GET` | `/v1/models` | Lista todos los alias de modelos definidos en `routes.toml`. |
| `POST` | `/v1/chat/completions` | Completados de chat compatibles con OpenAI. Admite `stream: true` (SSE). Acepta el bloque de extensión opcional `vertex`. |

---

## Configuración

### Variables de entorno

| Variable | Valor por defecto | Descripción |
|----------|-------------------|-------------|
| `SYNAPSE_ADDR` | `0.0.0.0:8080` | Dirección y puerto del servidor HTTP principal. |
| `SYNAPSE_METRICS_ADDR` | `0.0.0.0:9090` | Dirección y puerto del endpoint de métricas de Prometheus. |
| `SYNAPSE_ROUTES_PATH` | `config/routes.toml` | Ruta al fichero de configuración de rutas. |
| `SYNAPSE_PRICING_PATH` | `config/pricing.toml` | Ruta al fichero de configuración de precios. |
| `SYNAPSE_LEDGER_BACKENDS` | `sqlite` | Lista separada por comas de los destinos activos del registro de costes (p. ej. `postgres,pubsub`). Cada evento se distribuye a todos los destinos listados. |
| `SYNAPSE_LEDGER_BACKEND` | — | Alias de destino único; se usa cuando `SYNAPSE_LEDGER_BACKENDS` no está definido. |
| `SYNAPSE_LEDGER_SQLITE_DSN` | `sqlite://synapse.db?mode=rwc` | DSN de SQLite. Recurre a `SYNAPSE_LEDGER_DSN` y luego a la ruta por defecto. |
| `SYNAPSE_LEDGER_POSTGRES_DSN` | — | DSN de Postgres. Recurre a `SYNAPSE_LEDGER_DSN`. Obligatorio cuando `postgres` está en la lista de destinos. |
| `SYNAPSE_LEDGER_DSN` | `sqlite://synapse.db?mode=rwc` | DSN heredado de destino único (SQLite o Postgres). Se prefieren las variables por destino indicadas arriba. |
| `SYNAPSE_LEDGER_PUBSUB_TOPIC` | — | ID del topic de Pub/Sub. Obligatorio cuando `pubsub` está en la lista de destinos (feature `ledger-pubsub`). |
| `SYNAPSE_LEDGER_PUBSUB_PROJECT` | — | Proyecto de GCP para Pub/Sub. Recurre a `VERTEX_PROJECT`. |
| `SYNAPSE_LEDGER_SNS_TOPIC_ARN` | — | ARN del topic de SNS. Obligatorio cuando `sns` está en la lista de destinos (feature `ledger-sns`). |
| `SYNAPSE_LEDGER_SNS_REGION` | — | Región de AWS para SNS. Opcional; si no se especifica, se usa la cadena de credenciales por defecto de AWS. |
| `SYNAPSE_DEFAULT_TENANT` | `unattributed` | Nombre de tenant usado cuando el encabezado `x-synapse-tenant` está ausente. |
| `SYNAPSE_REQUEST_TIMEOUT_SECS` | `120` | Timeout de time-to-first-chunk en segundos. Un tramo que no produce su primer fragmento dentro de esta ventana cae al siguiente. |
| `SYNAPSE_STREAM_IDLE_TIMEOUT_SECS` | `60` | Intervalo máximo de inactividad entre fragmentos en segundos. Un tramo que se detiene a mitad de stream durante este tiempo se termina. |

### Variables de credenciales de proveedores

El gateway realiza una comprobación de credenciales al inicio que falla de forma inmediata. Si un proveedor está referenciado en `routes.toml` pero le faltan las credenciales requeridas, el proceso termina de inmediato.

| Proveedor | Obligatorio | Opcional |
|-----------|-------------|----------|
| `vertex` | `VERTEX_PROJECT` (ADC mediante `google-cloud-auth`) | — |
| `qwen` | `DASHSCOPE_API_KEY` | `DASHSCOPE_BASE_URL` |
| `openai` | `OPENAI_API_KEY` | `OPENAI_BASE_URL` |
| `oai_compat` | `OAI_COMPAT_BASE_URL` | `OAI_COMPAT_API_KEY` |

### `config/routes.toml`

Mapea un alias de modelo de cara al cliente a una lista ordenada de tramos de fallback. El gateway prueba cada tramo en orden, avanzando ante un error.

```toml
[routes."gemini-pro"]
legs = [
  { provider = "vertex", model = "gemini-3-pro" },
  { provider = "qwen",   model = "qwen-max" },
]

[routes."fast"]
legs = [{ provider = "vertex", model = "gemini-3-flash" }]
```

### `config/pricing.toml`

Mapea `provider:model` al coste de entrada/salida en USD por 1.000.000 de tokens. Los modelos no listados tienen coste 0.

```toml
# USD por 1.000.000 de tokens. Los modelos de código abierto o autoalojados tienen coste 0 por defecto.
["vertex:gemini-3-pro"]
input  = 1.25
output = 5.0

["vertex:gemini-3-flash"]
input  = 0.30
output = 1.20

["qwen:qwen-max"]
input  = 1.6
output = 6.4
```

---

## Atribución por tenant

Dos encabezados de petición controlan la atribución de costes y observabilidad:

| Encabezado | Descripción |
|------------|-------------|
| `x-synapse-tenant` | Identificador de tenant. Recurre a `SYNAPSE_DEFAULT_TENANT` (`unattributed`). |
| `x-synapse-workspace` | Subagrupación opcional dentro de un tenant (p. ej. un proyecto o equipo). |

Ambos valores se registran en las filas `usage_events` del registro de costes y se incluyen como atributos en los spans `gen_ai.*`.

---

## Observabilidad

### Prometheus

Las métricas se sirven en `SYNAPSE_METRICS_ADDR` (por defecto `:9090`).

| Métrica | Tipo | Etiquetas | Descripción |
|---------|------|-----------|-------------|
| `synapse_requests_total` | Counter | `route`, `model`, `system`, `lane` | Total de peticiones atendidas. |
| `synapse_request_duration_seconds` | Histogram | `route`, `model`, `system`, `lane` | Latencia extremo a extremo de las peticiones. |
| `synapse_input_tokens_total` | Counter | `route`, `model`, `system`, `lane` | Tokens de entrada consumidos acumulados. |
| `synapse_output_tokens_total` | Counter | `route`, `model`, `system`, `lane` | Tokens de salida generados acumulados. |
| `synapse_ledger_dropped_total` | Counter | — | Eventos del registro descartados por canal lleno (desbordamiento fire-and-forget). |
| `synapse_ledger_errors_total` | Counter | `backend` | Fallos de escritura por destino (p. ej. `backend="pubsub"`). Un fallo en un destino no detiene los demás. |

Las cuatro métricas `synapse_*` de tokens/peticiones comparten el mismo conjunto de etiquetas:

- **`route`** — el alias de modelo de cara al cliente (p. ej. `gemini-pro`, `fast`).
- **`model`** — el modelo que realmente atendió la petición (según lo devuelto por el tramo de backend).
- **`system`** — el valor OpenLLMetry `gen_ai.system`: `vertexai`, `openai`, `dashscope` o `oai_compat`.
- **`lane`** — `standard` (crate genai) o `native` (REST de Vertex directo).

El tenant y el workspace **no** son etiquetas de Prometheus. Se registran en el registro de costes (tabla `usage_events`) y se incluyen como atributos en los spans de trazado `gen_ai.*`. Mantenerlos fuera de las etiquetas de métricas evita una cardinalidad no acotada derivada de valores de encabezados suministrados por clientes no confiables.

### Trazado

Los spans estructurados siguen las convenciones semánticas `gen_ai.*` de OpenTelemetry (modelo, proveedor, conteo de tokens, tipos de error). Configura el nivel y formato del log mediante `RUST_LOG` (p. ej. `RUST_LOG=info`).

---

## Registro de costes

El consumo de tokens se registra de forma asíncrona en una tabla `usage_events` tras cada completado exitoso. La escritura en el registro es fire-and-forget: si el canal interno está lleno, el evento se descarta y `synapse_ledger_dropped_total` se incrementa — la latencia de la petición nunca se ve afectada.

### Distribución a múltiples destinos

Varios backends pueden funcionar simultáneamente. Cada evento de uso se entrega a todos los destinos configurados de forma concurrente. El fallo de un destino nunca bloquea a los demás; los fallos por destino se registran y se contabilizan en `synapse_ledger_errors_total{backend=<name>}`.

Selecciona los backends con `SYNAPSE_LEDGER_BACKENDS` (separados por comas). El singular `SYNAPSE_LEDGER_BACKEND` sigue aceptándose como fallback de un único elemento. Cuando ninguna de las dos variables está definida, el valor por defecto es `sqlite`.

```bash
# Distribuir a Postgres y Pub/Sub simultáneamente
SYNAPSE_LEDGER_BACKENDS=postgres,pubsub
```

### Backends

| Backend | Feature de Cargo | Variables de entorno | Notas |
|---------|-----------------|----------------------|-------|
| SQLite | `ledger-sqlite` (por defecto) | `SYNAPSE_LEDGER_SQLITE_DSN` (fallback: `SYNAPSE_LEDGER_DSN`, luego `sqlite://synapse.db?mode=rwc`) | El fichero se crea automáticamente. |
| Postgres | `ledger-postgres` | `SYNAPSE_LEDGER_POSTGRES_DSN` (fallback: `SYNAPSE_LEDGER_DSN`) | Requiere una cadena de conexión. |
| GCP Pub/Sub | `ledger-pubsub` | `SYNAPSE_LEDGER_PUBSUB_TOPIC` (obligatorio), `SYNAPSE_LEDGER_PUBSUB_PROJECT` (fallback: `VERTEX_PROJECT`) | Autenticación ADC; la clave de ordenación es `requestId`. |
| AWS SNS | `ledger-sns` | `SYNAPSE_LEDGER_SNS_TOPIC_ARN` (obligatorio), `SYNAPSE_LEDGER_SNS_REGION` (opcional, si no se usa la cadena por defecto de AWS) | Cadena de credenciales estándar de AWS. |

SQLite está habilitado por defecto. Los backends en la nube (`ledger-pubsub`, `ledger-sns`) están protegidos por feature flags y no arrastran ningún SDK de nube salvo que se habiliten explícitamente.

```bash
# Compilar con soporte para Pub/Sub
cargo build --release --features ledger-pubsub

# Compilar con soporte para SNS
cargo build --release --features ledger-sns

# Compilar con ambos backends en la nube
cargo build --release --features "ledger-pubsub ledger-sns"
```

### Formato del evento publicado (Pub/Sub y SNS)

Ambos backends en la nube publican un payload JSON alineado con talos (`camelCase`; tenant como `namespace`; `type: "usage"`):

```json
{
  "namespace": "my-team",
  "requestId": "01929f3a-...",
  "timestamp": "2026-06-10T15:30:45Z",
  "type": "usage",
  "route": "gemini-pro",
  "provider": "vertex",
  "model": "gemini-3-pro",
  "lane": "standard",
  "inputTokens": 128,
  "outputTokens": 256,
  "costUsd": 0.00042,
  "status": "ok"
}
```

Cada mensaje incluye atributos para el filtrado de suscripciones: `namespace`, `requestId`, `type`, `provider`, `status`. Pub/Sub además establece `requestId` como clave de ordenación del mensaje.

### Esquema

La única migración (`migrations/0001_usage_events.sql`) crea la tabla `usage_events` con columnas para tenant, workspace, proveedor, modelo, tokens de entrada, tokens de salida, coste y fecha/hora.

---

## Compilación

### Cargo

```bash
# Compilación por defecto (registro SQLite)
cargo build --release

# Solo registro Postgres
cargo build --release --no-default-features --features ledger-postgres

# SQLite + distribución a Pub/Sub
cargo build --release --features ledger-pubsub

# SQLite + distribución a SNS
cargo build --release --features ledger-sns

# Los cuatro backends
cargo build --release --features "ledger-pubsub ledger-sns ledger-postgres"
```

El binario de release se encuentra en `target/release/synapse-gateway`.

### Docker

```bash
docker build -t synapse-gateway .
docker run --rm \
  -e VERTEX_PROJECT=my-project \
  -e OPENAI_API_KEY=sk-... \
  -p 8080:8080 \
  -p 9090:9090 \
  -v "$(pwd)/config:/app/config" \
  synapse-gateway
```

El `Dockerfile` multietapa usa `rust:1-bookworm` para compilar y `debian:bookworm-slim` como imagen de ejecución. Los directorios `config/` y `migrations/` se copian en la imagen para que sea autocontenida; monta un volumen sobre `/app/config` para suministrar tus propios ficheros de rutas y precios en tiempo de ejecución.

---

## Pruebas

```bash
# Ejecutar todas las pruebas (feature SQLite, por defecto)
cargo test

# Ejecutar todas las pruebas con todas las features (SQLite + Postgres)
cargo test --all-features
```

La suite de pruebas (66 tests) cubre la resolución de rutas, el comportamiento del fallback, la detección de carril, la atribución de tenant, el análisis de configuración, las escrituras en el registro de costes, la integración del manejador HTTP, las primitivas de streaming, la acumulación de llamadas a herramientas, el fallback por timeout del primer fragmento y la serialización de SSE.

---

## Limitaciones / hoja de ruta

Las siguientes funcionalidades **no están** presentes en v1 y están planificadas para versiones futuras:

- Autenticación / aplicación de claves de API en peticiones entrantes.
- Limitación de tasa (rate limiting).
- Enrutamiento de endpoints de Vertex en múltiples regiones.
- API de administración para la recarga dinámica de rutas.

---

## Contribuciones

Las contribuciones son bienvenidas. Consulta **[CONTRIBUTING.md](CONTRIBUTING.md)** para saber cómo compilar, probar y enviar cambios. Los commits deben estar firmados bajo el [Developer Certificate of Origin](https://developercertificate.org/) (`git commit -s`); las contribuciones se publican bajo AGPL-3.0. Por favor, lee también nuestro **[Código de Conducta](CODE_OF_CONDUCT.md)**.

## Seguridad

¿Has encontrado una vulnerabilidad? **No abras un issue público.** Consulta **[SECURITY.md](SECURITY.md)** para la divulgación privada (correo a `raj@sustentabilitas.com` o un aviso privado de GitHub).

## Licencia

Publicado bajo la **GNU Affero General Public License v3.0** (AGPL-3.0). Consulta **[LICENSE](LICENSE)**.
