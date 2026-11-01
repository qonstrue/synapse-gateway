# synapse-gateway

[![crates.io](https://img.shields.io/crates/v/synapse-gateway.svg)](https://crates.io/crates/synapse-gateway)
[![Docker Hub](https://img.shields.io/docker/v/sustentabilitas/synapse-gateway?logo=docker&label=docker)](https://hub.docker.com/r/sustentabilitas/synapse-gateway)
[![License: MPL-2.0](https://img.shields.io/badge/License-MPL--2.0-brightgreen.svg)](https://github.com/sustentabilitas/synapse-gateway/blob/main/LICENSE)
[![CI](https://github.com/sustentabilitas/synapse-gateway/actions/workflows/ci.yml/badge.svg)](https://github.com/sustentabilitas/synapse-gateway/actions/workflows/ci.yml)

[English](https://github.com/sustentabilitas/synapse-gateway/blob/main/crates/synapse-gateway/README.md) · **Español**

synapse-gateway es un gateway de LLM de código abierto escrito en Rust. Tus clientes envían
peticiones estándar de OpenAI `POST /v1/chat/completions`; el gateway enruta cada una a
través de una cadena de fallback de proveedores definida en la configuración (Vertex AI,
OpenAI, Qwen y servidores autoalojados compatibles con OpenAI) y anota lo que ha costado en un
registro de costes por tenant. Las peticiones van por uno de tres carriles: el carril estándar
compatible con OpenAI, un carril Vertex AI nativo que conserva la caché de contexto, los
medios `gs://` y los esquemas de respuesta estrictos, y un carril Jev para decisiones tipadas
de TypeSafe System One. Streaming, llamadas a herramientas, embeddings, guardrails de entrada
y métricas `synapse_*` vienen incluidos.

**Documentación completa:** https://synapse-gateway.readthedocs.io/en/latest/es/docs/overview/introduction/

## Instalación

```bash
# Imagen de Docker (linux/amd64)
docker pull sustentabilitas/synapse-gateway

# Binario
cargo install synapse-gateway
```

Para embeber el gateway en un servicio Rust, añade la biblioteca sin sus features por defecto
y llama a `Gateway::chat()` en el mismo proceso (el crate de biblioteca se llama `synapse`):

```toml
[dependencies]
synapse-gateway = { version = "2", default-features = false }
```

Consulta [Instalación](https://synapse-gateway.readthedocs.io/en/latest/es/docs/get-started/installation/)
para las features de Cargo y los backends del registro de costes, y
[Synapse como biblioteca embebida](https://synapse-gateway.readthedocs.io/en/latest/es/docs/guides/embedding-as-library/).

## Ejemplo

Guarda una ruta, sus precios y una política de guardrails:

```toml
# config/routes.toml
[routes."gemini-flash"]
legs = [
  { provider = "vertex", model = "gemini-3.5-flash-lite", region = "us" },
]
```

```toml
# config/pricing.toml
"vertex:gemini-3.5-flash-lite" = { input = 0.30, output = 2.50 }
```

```toml
# config/guardrails.toml
[guardrails.default]
scanners = ["prompt_injection", "secrets"]
```

Arranca el gateway con Application Default Credentials para Vertex AI y envía una petición:

```bash
gcloud auth application-default login
VERTEX_PROJECT_ID=my-gcp-project synapse-gateway   # API en :8080, métricas en :9090

# En otra terminal:
curl -s http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-synapse-tenant: my-team" \
  -d '{"model": "gemini-flash", "messages": [{"role": "user", "content": "Hello!"}]}'
```

El [inicio rápido](https://synapse-gateway.readthedocs.io/en/latest/es/docs/get-started/quickstart/)
añade streaming, fallback a OpenAI, peticiones Vertex nativas y el registro de costes.

## Más información

- [Arquitectura](https://synapse-gateway.readthedocs.io/en/latest/es/docs/overview/architecture/): carriles y cadenas de fallback
- [Rutas](https://synapse-gateway.readthedocs.io/en/latest/es/docs/configuration/routes/), [enrutador Jev 2D de arneses (`config/harnesses.toml`)](https://synapse-gateway.readthedocs.io/en/latest/es/docs/guides/jev-harness-router/) y [variables de entorno](https://synapse-gateway.readthedocs.io/en/latest/es/docs/configuration/environment-variables/)
- [Referencia de la API HTTP](https://synapse-gateway.readthedocs.io/en/latest/es/docs/reference/http-api/)
- [Métricas](https://synapse-gateway.readthedocs.io/en/latest/es/docs/operating/metrics/)
- [Limitaciones y hoja de ruta](https://synapse-gateway.readthedocs.io/en/latest/es/docs/reference/limitations-roadmap/)

## Contribuciones

Las contribuciones son bienvenidas. Consulta **[CONTRIBUTING.md](https://github.com/sustentabilitas/synapse-gateway/blob/main/CONTRIBUTING.md)** para saber cómo compilar, probar y enviar cambios. Los commits deben estar firmados bajo el [Developer Certificate of Origin](https://developercertificate.org/) (`git commit -s`); las contribuciones se publican bajo MPL-2.0. Por favor, lee también nuestro **[Código de Conducta](https://github.com/sustentabilitas/synapse-gateway/blob/main/CODE_OF_CONDUCT.md)**.

## Seguridad

¿Has encontrado una vulnerabilidad? **No abras un issue público.** Consulta **[SECURITY.md](https://github.com/sustentabilitas/synapse-gateway/blob/main/SECURITY.md)** para la divulgación privada (correo a `raj@sustentabilitas.com` o un aviso privado de GitHub).

## Licencia

Publicado bajo la **Mozilla Public License 2.0** (MPL-2.0), que permite el uso comercial y pide que los cambios en los archivos de Synapse se compartan. Consulta **[LICENSE](https://github.com/sustentabilitas/synapse-gateway/blob/main/LICENSE)** y la [nota sobre la licencia](https://github.com/sustentabilitas/synapse-gateway#license).
