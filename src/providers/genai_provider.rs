//! Provider catalog: genai Client + CircuitBreaker per provider id.

use std::sync::Arc;
use std::time::Duration;

use crate::providers::vertex_auth::VertexAuth;
use crate::resilience::{CircuitBreaker, Profile};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// Vertex AI / Gemini (PDF-native).
    Vertex,
    /// OpenAI-compatible Chat Completions (image-only; PDFs need rasterising).
    OpenAiCompat,
}

/// A configured LLM provider — genai Client + the circuit breaker the
/// router uses to gate calls to it.
pub struct Provider {
    pub id: &'static str,
    pub kind: ProviderKind,
    pub client: genai::Client,
    pub breaker: Arc<CircuitBreaker>,
    pub profile: Profile,
    /// "qwen", "vertex_global", etc. — used for metric labels and DashScope quirks.
    pub label: &'static str,
}

impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Provider")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("label", &self.label)
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

/// Configuration consumed by `build_vertex_provider`.
#[derive(Debug, Clone)]
pub struct VertexProviderConfig {
    pub project: String,
    /// Always `"global"` for the `vertex_global` provider id. Kept as a field
    /// so future region-pinned providers can reuse the builder.
    pub region: String,
    pub request_timeout: Duration,
    /// Override the derived endpoint URL (project/region) with a literal base.
    /// `None` in production; `Some(wiremock_uri)` in tests.
    pub endpoint_override: Option<String>,
}

/// Configuration consumed by `build_openai_compat_provider`.
#[derive(Debug, Clone)]
pub struct OpenAiCompatConfig {
    pub base_url: String,
    pub api_key: String,
    pub request_timeout: Duration,
    /// Override the derived endpoint URL with a literal. genai's OpenAI adapter
    /// internally calls `Url::join("chat/completions")` on the endpoint, so this
    /// must be a BASE URL (with a trailing slash if non-host-only) — NOT a full
    /// `/chat/completions` URL. `None` in production; `Some(wiremock_uri)` in tests.
    pub endpoint_override: Option<String>,
}

/// Construct a Vertex-backed `Provider`.
///
/// `id` should be `"vertex_global"` for the default route table.
///
/// `AdapterKind::Vertex` routes all model names through the Vertex adapter;
/// no `vertex::` prefix is needed on the model string passed to `exec_chat`.
pub fn build_vertex_provider(
    id: &'static str,
    config: VertexProviderConfig,
    auth: Arc<VertexAuth>,
) -> anyhow::Result<Provider> {
    let endpoint_host = if config.region == "global" {
        "aiplatform.googleapis.com".to_string()
    } else {
        format!("{}-aiplatform.googleapis.com", config.region)
    };
    let endpoint_base = config
        .endpoint_override
        .clone()
        .unwrap_or_else(|| format!("https://{endpoint_host}"));

    let project = config.project.clone();
    let region = config.region.clone();

    let client = genai::Client::builder()
        .with_auth_resolver(auth.into_auth_resolver())
        .with_service_target_resolver_fn(
            move |service_target: genai::ServiceTarget| -> genai::resolver::Result<genai::ServiceTarget> {
                // Vertex URL shape (the trailing slash is load-bearing):
                //   {base}/v1/projects/{project}/locations/{region}/
                // The Vertex adapter appends `publishers/{google|anthropic}/models/{name}:generateContent`
                // off `endpoint.base_url()` itself; no `{model}` placeholder here.
                let endpoint = genai::resolver::Endpoint::from_owned(format!(
                    "{endpoint_base}/v1/projects/{project}/locations/{region}/",
                ));
                // Preserve the AuthResolver's result — adapters read auth from the
                // service-target after the target resolver runs (genai resolution order).
                Ok(genai::ServiceTarget {
                    endpoint,
                    auth: service_target.auth,
                    model: service_target.model,
                })
            },
        )
        .with_adapter_kind(genai::adapter::AdapterKind::Vertex)
        .with_web_config(genai::WebConfig::default().with_timeout(config.request_timeout))
        .build();

    let breaker = Arc::new(CircuitBreaker::new(id, Profile::Aggressive));
    Ok(Provider {
        id,
        kind: ProviderKind::Vertex,
        client,
        breaker,
        profile: Profile::Aggressive,
        label: id,
    })
}

/// Derive the endpoint URL handed to genai's OpenAI adapter from a `base_url`.
///
/// genai's OpenAI adapter calls `Url::join("chat/completions")` on the endpoint,
/// so we must hand it a BASE URL with a trailing slash (so the join lands at
/// `{base}/chat/completions`) — NOT a full `/chat/completions` URL (which would
/// double the path) and NOT a base without a trailing slash (which would drop
/// the last path segment per RFC 3986 `Url::join` semantics).
fn derive_openai_compat_endpoint(base_url: &str) -> String {
    let mut s = base_url.trim_end_matches('/').to_string();
    s.push('/');
    s
}

/// Construct an OpenAI-compatible `Provider` (Qwen DashScope, OpenAI, Grok).
pub fn build_openai_compat_provider(
    id: &'static str,
    config: OpenAiCompatConfig,
) -> anyhow::Result<Provider> {
    let api_key = config.api_key.clone();
    let endpoint_url = config
        .endpoint_override
        .clone()
        .unwrap_or_else(|| derive_openai_compat_endpoint(&config.base_url));

    let client = genai::Client::builder()
        .with_auth_resolver(genai::resolver::AuthResolver::from_resolver_fn(
            move |_model_iden: genai::ModelIden| -> genai::resolver::Result<Option<genai::resolver::AuthData>> {
                Ok(Some(genai::resolver::AuthData::from_single(api_key.clone())))
            },
        ))
        .with_service_target_resolver_fn(
            move |service_target: genai::ServiceTarget| -> genai::resolver::Result<genai::ServiceTarget> {
                let endpoint = genai::resolver::Endpoint::from_owned(endpoint_url.clone());
                // Preserve the AuthResolver's result — adapters read auth from the
                // service-target after the target resolver runs (genai resolution order).
                Ok(genai::ServiceTarget {
                    endpoint,
                    auth: service_target.auth,
                    model: service_target.model,
                })
            },
        )
        .with_adapter_kind(genai::adapter::AdapterKind::OpenAI)
        .with_web_config(genai::WebConfig::default().with_timeout(config.request_timeout))
        .build();

    let breaker = Arc::new(CircuitBreaker::new(id, Profile::Default));
    Ok(Provider {
        id,
        kind: ProviderKind::OpenAiCompat,
        client,
        breaker,
        profile: Profile::Default,
        label: id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn dummy_vertex_auth() -> Arc<VertexAuth> {
        Arc::new(VertexAuth::with_fetcher(|| {
            Box::pin(async { Ok(("test-token".into(), Duration::from_secs(3600))) })
        }))
    }

    #[test]
    fn build_vertex_provider_yields_vertex_kind_and_aggressive_profile() {
        let p = build_vertex_provider(
            "vertex_global",
            VertexProviderConfig {
                project: "test-proj".into(),
                region: "global".into(),
                request_timeout: Duration::from_secs(120),
                endpoint_override: None,
            },
            dummy_vertex_auth(),
        )
        .unwrap();
        assert_eq!(p.id, "vertex_global");
        assert_eq!(p.kind, ProviderKind::Vertex);
        assert!(matches!(p.profile, Profile::Aggressive));
    }

    #[test]
    fn build_openai_compat_provider_yields_oai_kind_and_default_profile() {
        let p = build_openai_compat_provider(
            "qwen",
            OpenAiCompatConfig {
                base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1".into(),
                api_key: "sk-test".into(),
                request_timeout: Duration::from_secs(120),
                endpoint_override: None,
            },
        )
        .unwrap();
        assert_eq!(p.id, "qwen");
        assert_eq!(p.kind, ProviderKind::OpenAiCompat);
        assert!(matches!(p.profile, Profile::Default));
    }

    /// Regression test for the bug where the service-target resolver replaced
    /// `service_target.auth` with an empty placeholder, and the related bug
    /// where the Gemini adapter (used in place of Vertex) emitted a literal
    /// `{model}` placeholder plus a doubled `:generateContent` suffix in the
    /// URL. The Vertex adapter reads the bearer from the final `ServiceTarget`
    /// and forwards it as `Authorization: Bearer ...`. The `path_regex`
    /// matcher also pins the URL shape so we catch a regression to the old
    /// Gemini adapter (which would emit `x-goog-api-key` and a literal
    /// `{model}`) without relying on auth alone.
    #[tokio::test]
    async fn vertex_provider_sends_bearer_token_on_request() {
        use wiremock::matchers::{header, method, path_regex};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        let mock = MockServer::start().await;
        // Pin the URL: must end in `/publishers/google/models/<name>:generateContent`
        // with no literal `{model}` and no doubled `:generateContent` suffix.
        Mock::given(method("POST"))
            .and(path_regex(
                r"^/v1/projects/test-proj/locations/global/publishers/google/models/gemini-2\.5-flash:generateContent$",
            ))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [{
                    "content": { "parts": [{ "text": "ok" }], "role": "model" },
                    "finishReason": "STOP"
                }],
                "modelVersion": "gemini-2.5-flash",
                "usageMetadata": { "promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2 }
            })))
            .mount(&mock)
            .await;

        let provider = build_vertex_provider(
            "vertex_global",
            VertexProviderConfig {
                project: "test-proj".into(),
                region: "global".into(),
                request_timeout: Duration::from_secs(5),
                endpoint_override: Some(mock.uri()),
            },
            dummy_vertex_auth(),
        )
        .unwrap();

        // Drive a chat call. The bare model name works because the client is
        // bound to `AdapterKind::Vertex` (no `vertex::` prefix needed). If the
        // mock matcher matched, both the auth header and URL shape are correct.
        let _ = provider
            .client
            .exec_chat(
                "gemini-2.5-flash",
                genai::chat::ChatRequest::from_user("hi"),
                None,
            )
            .await;

        let received: Vec<Request> = mock.received_requests().await.unwrap_or_default();
        assert!(
            received.iter().any(|r| {
                r.headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    == Some("Bearer test-token")
            }),
            "expected Authorization: Bearer test-token on at least one outbound request, got headers: {:?}",
            received.iter().map(|r| r.headers.clone()).collect::<Vec<_>>(),
        );
        assert!(
            received.iter().any(|r| {
                let p = r.url.path();
                p == "/v1/projects/test-proj/locations/global/publishers/google/models/gemini-2.5-flash:generateContent"
            }),
            "expected exact Vertex URL with no literal {{model}} and no doubled :generateContent, got URLs: {:?}",
            received.iter().map(|r| r.url.path().to_string()).collect::<Vec<_>>(),
        );
    }

    /// Regression test for the same bug on the OpenAI-compat path. The OpenAI
    /// adapter reads auth from the final `ServiceTarget` and forwards it as
    /// `Authorization: Bearer {key}`. If auth is dropped, the header is
    /// `Bearer ` (empty) and the upstream returns 401.
    #[tokio::test]
    async fn openai_compat_provider_sends_bearer_token_on_request() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        let mock = MockServer::start().await;
        // Pin the full path including a non-trivial `/v1` base segment. This
        // mirrors production (DashScope `.../compatible-mode/v1`) and catches
        // the URL-doubling bug: with the bug, genai's `Url::join` would resolve
        // to `/v1/chat/chat/completions` instead of `/v1/chat/completions`.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "created": 0,
                "model": "qwen-vl-max",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "ok" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
            })))
            .mount(&mock)
            .await;

        // Use `base_url` (not `endpoint_override`) so this test actually
        // exercises the production endpoint-derivation path. Append `/v1` so a
        // regression that hands genai a full `/chat/completions` URL causes
        // genai's `Url::join` to double the suffix to `/v1/chat/chat/completions`,
        // tripping the `path("/v1/chat/completions")` matcher above.
        let provider = build_openai_compat_provider(
            "qwen",
            OpenAiCompatConfig {
                base_url: format!("{}/v1", mock.uri()),
                api_key: "test-key".into(),
                request_timeout: Duration::from_secs(5),
                endpoint_override: None,
            },
        )
        .unwrap();

        let _ = provider
            .client
            .exec_chat(
                "qwen-vl-max",
                genai::chat::ChatRequest::from_user("hi"),
                None,
            )
            .await;

        let received: Vec<Request> = mock.received_requests().await.unwrap_or_default();
        assert!(
            received.iter().any(|r| {
                r.headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    == Some("Bearer test-key")
            }),
            "expected Authorization: Bearer test-key on at least one outbound request, got headers: {:?}",
            received.iter().map(|r| r.headers.clone()).collect::<Vec<_>>(),
        );
        // Pin the exact URL path. The wiremock matcher returns 404 (not an
        // assertion error) on misses, so without this explicit check the URL-
        // doubling regression (`.../v1/chat/chat/completions`) would slip past
        // the matcher and the bearer assertion above would still pass against
        // the unmatched-but-recorded request.
        assert!(
            received
                .iter()
                .any(|r| r.url.path() == "/v1/chat/completions"),
            "expected exact path /v1/chat/completions, got: {:?}",
            received
                .iter()
                .map(|r| r.url.path().to_string())
                .collect::<Vec<_>>(),
        );
    }

    /// Static check that the endpoint derived in production from the DashScope
    /// base URL, once passed through genai's `Url::join("chat/completions")`,
    /// resolves to exactly `.../v1/chat/completions` — not the doubled path
    /// (`.../v1/chat/chat/completions`) and not the segment-dropped path
    /// (`.../chat/completions`). Catches regressions to the URL-construction
    /// logic without driving a real HTTP call.
    #[test]
    fn openai_compat_endpoint_resolves_to_chat_completions() {
        use reqwest::Url;

        let base = "https://dashscope-intl.aliyuncs.com/compatible-mode/v1";
        // Exercise the SAME logic build_openai_compat_provider uses to derive
        // the endpoint when no override is set.
        let endpoint = derive_openai_compat_endpoint(base);
        let parsed = Url::parse(&endpoint).unwrap();
        let joined = parsed.join("chat/completions").unwrap();
        assert_eq!(
            joined.as_str(),
            "https://dashscope-intl.aliyuncs.com/compatible-mode/v1/chat/completions",
        );

        // And smoke-test that build_openai_compat_provider still constructs
        // without panic for this config.
        let _ = build_openai_compat_provider(
            "qwen",
            OpenAiCompatConfig {
                base_url: base.into(),
                api_key: "k".into(),
                request_timeout: Duration::from_secs(5),
                endpoint_override: None,
            },
        )
        .unwrap();
    }
}
