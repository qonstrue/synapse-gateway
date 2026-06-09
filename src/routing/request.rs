//! OpenAI-compatible chat request body + native extension block.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub response_format: Option<ResponseFormat>,
    #[serde(default)]
    pub routing_strategy: Option<String>,
    #[serde(default)]
    pub vertex: Option<VertexExt>,
    #[serde(flatten, default)]
    pub passthrough: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    pub role: String,
    pub content: Value, // string or array of content parts
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResponseFormat {
    #[serde(rename = "type")]
    pub kind: String, // "text" | "json_object" | "json_schema"
    #[serde(default)]
    pub json_schema: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct VertexExt {
    #[serde(default)]
    pub cached_content: Option<String>,
    #[serde(default)]
    pub media_uris: Option<Vec<String>>,
    #[serde(default)]
    pub response_schema: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_openai_body() {
        let body = serde_json::json!({
            "model": "gemini-pro",
            "messages": [{"role": "user", "content": "hi"}],
            "temperature": 0.2
        });
        let req: ChatRequest = serde_json::from_value(body).unwrap();
        assert_eq!(req.model, "gemini-pro");
        assert_eq!(req.messages.len(), 1);
        assert!(req.vertex.is_none());
        assert!(req.passthrough.is_empty());
    }

    #[test]
    fn captures_vertex_extension_and_passthrough() {
        let body = serde_json::json!({
            "model": "gemini-pro",
            "messages": [{"role": "user", "content": "hi"}],
            "top_k": 40,
            "vertex": { "cached_content": "cachedContents/abc" }
        });
        let req: ChatRequest = serde_json::from_value(body).unwrap();
        assert_eq!(
            req.vertex.unwrap().cached_content.as_deref(),
            Some("cachedContents/abc")
        );
        assert_eq!(req.passthrough.get("top_k"), Some(&serde_json::json!(40)));
    }
}
