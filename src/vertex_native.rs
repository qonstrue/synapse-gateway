//! Native Vertex REST lane: preserves cachedContents, gs:// media URIs, and
//! strict responseSchema that the OpenAI-compatible standard lane cannot express.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tap::Pipe;

use crate::providers::vertex_auth::VertexAuth;
use crate::routing::executor::Completion;
use crate::routing::request::{ChatRequest, VertexExt};

#[derive(Debug, Clone)]
pub struct VertexNativeProvider {
    http: reqwest::Client,
    auth: Arc<VertexAuth>,
    project: String,
    region: String,
    /// Base host; overridden in tests with a wiremock URI.
    endpoint_base: String,
}

impl VertexNativeProvider {
    pub fn new(
        auth: Arc<VertexAuth>,
        project: String,
        region: String,
        request_timeout: Duration,
        endpoint_override: Option<String>,
    ) -> Self {
        let endpoint_base = endpoint_override.unwrap_or_else(|| {
            if region == "global" {
                "https://aiplatform.googleapis.com".into()
            } else {
                format!("https://{region}-aiplatform.googleapis.com")
            }
        });
        Self {
            http: reqwest::Client::builder().timeout(request_timeout).build().unwrap(),
            auth,
            project,
            region,
            endpoint_base,
        }
    }

    fn generate_url(&self, model: &str) -> String {
        format!(
            "{}/v1/projects/{}/locations/{}/publishers/google/models/{}:generateContent",
            self.endpoint_base, self.project, self.region, model
        )
    }

    pub async fn generate(&self, model: &str, req: &ChatRequest) -> Result<Completion, crate::error::GatewayError> {
        let ext = req.vertex.clone().unwrap_or_default();
        let payload = build_payload(req, &ext);
        let token = self
            .auth
            .token()
            .await
            .map_err(|e| crate::error::GatewayError::Upstream { status: 401, body: format!("vertex auth: {e}") })?;

        let resp = self
            .http
            .post(self.generate_url(model))
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| crate::error::GatewayError::Upstream { status: 502, body: e.to_string() })?;

        let status = resp.status();
        let value: Value = resp
            .json()
            .await
            .map_err(|e| crate::error::GatewayError::Upstream { status: status.as_u16(), body: e.to_string() })?;
        if !status.is_success() {
            if status.is_client_error() {
                return Err(crate::error::GatewayError::BadRequest(format!("vertex {}: {}", status.as_u16(), value)));
            }
            return Err(crate::error::GatewayError::Upstream { status: status.as_u16(), body: value.to_string() });
        }
        parse_response("vertex", model, &value)
    }
}

/// Build a Vertex `generateContent` body, threading native features through.
fn build_payload(req: &ChatRequest, ext: &VertexExt) -> Value {
    let parts = req
        .messages
        .iter()
        .map(|m| json!({ "text": m.content.as_str().map(str::to_string).unwrap_or_else(|| m.content.to_string()) }))
        .chain(
            ext.media_uris
                .iter()
                .flatten()
                .map(|uri| json!({ "fileData": { "fileUri": uri, "mimeType": "video/mp4" } })),
        )
        .collect::<Vec<_>>();

    json!({
        "contents": [{ "role": "user", "parts": parts }],
    })
    .pipe(|mut body| {
        if let Some(cache) = &ext.cached_content {
            body["cachedContent"] = json!(cache);
        }
        if let Some(schema) = &ext.response_schema {
            body["generationConfig"] = json!({
                "responseMimeType": "application/json",
                "responseSchema": schema,
            });
        }
        if let Some(t) = req.temperature {
            body["generationConfig"]["temperature"] = json!(t);
        }
        body
    })
}

/// Map a Vertex response into the shared `Completion`, extracting usage.
fn parse_response(provider: &str, model: &str, v: &Value) -> Result<Completion, crate::error::GatewayError> {
    let content = v["candidates"][0]["content"]["parts"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let usage = &v["usageMetadata"];
    Ok(Completion {
        provider: provider.to_string(),
        model: model.to_string(),
        content,
        input_tokens: usage["promptTokenCount"].as_u64().unwrap_or(0),
        output_tokens: usage["candidatesTokenCount"].as_u64().unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_with(ext: VertexExt) -> ChatRequest {
        let mut r: ChatRequest = serde_json::from_value(serde_json::json!({
            "model": "gemini-pro", "messages": [{"role": "user", "content": "describe"}]
        }))
        .unwrap();
        r.vertex = Some(ext);
        r
    }

    #[test]
    fn payload_includes_cached_content_and_schema_and_media() {
        let ext = VertexExt {
            cached_content: Some("cachedContents/abc".into()),
            media_uris: Some(vec!["gs://bucket/v.mp4".into()]),
            response_schema: Some(serde_json::json!({"type": "object"})),
        };
        let body = build_payload(&req_with(ext.clone()), &ext);
        assert_eq!(body["cachedContent"], serde_json::json!("cachedContents/abc"));
        assert_eq!(body["generationConfig"]["responseSchema"], serde_json::json!({"type": "object"}));
        let parts = body["contents"][0]["parts"].as_array().unwrap();
        assert!(parts.iter().any(|p| p["fileData"]["fileUri"] == "gs://bucket/v.mp4"));
    }

    #[test]
    fn parses_usage_from_vertex_response() {
        let v = serde_json::json!({
            "candidates": [{"content": {"parts": [{"text": "a"}, {"text": "b"}], "role": "model"}}],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 4}
        });
        let c = parse_response("vertex", "gemini-3-pro", &v).unwrap();
        assert_eq!(c.content, "ab");
        assert_eq!(c.input_tokens, 10);
        assert_eq!(c.output_tokens, 4);
    }

    #[tokio::test]
    async fn generate_posts_to_vertex_url_with_bearer_and_parses() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/p/locations/global/publishers/google/models/gemini-3-pro:generateContent"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [{"content": {"parts": [{"text": "ok"}], "role": "model"}}],
                "usageMetadata": {"promptTokenCount": 2, "candidatesTokenCount": 1}
            })))
            .mount(&mock)
            .await;

        let auth = Arc::new(VertexAuth::with_fetcher(|| {
            Box::pin(async { Ok(("test-token".into(), Duration::from_secs(3600))) })
        }));
        let provider = VertexNativeProvider::new(
            auth, "p".into(), "global".into(), Duration::from_secs(5), Some(mock.uri()),
        );
        let c = provider
            .generate("gemini-3-pro", &req_with(VertexExt { cached_content: Some("cachedContents/x".into()), ..Default::default() }))
            .await
            .unwrap();
        assert_eq!(c.content, "ok");
        assert_eq!(c.input_tokens, 2);
    }
}
