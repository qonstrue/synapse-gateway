//! OpenAI-compatible embeddings adapter (`POST {base}/embeddings`).
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::embeddings::{EmbedOut, EmbeddingProvider};
use crate::error::GatewayError;

pub const OPENAI_EMBED_BATCH: usize = 2048;

pub struct OpenAiEmbedder {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl OpenAiEmbedder {
    pub fn new(base_url: String, api_key: String, timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest client");
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            client,
        }
    }
}

#[derive(Serialize)]
struct EmbedReq<'a> {
    input: &'a [String],
    model: &'a str,
    dimensions: u32,
}

#[derive(Deserialize)]
struct RespDatum {
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct RespUsage {
    #[serde(default)]
    total_tokens: u64,
}

#[derive(Deserialize)]
struct EmbedResp {
    data: Vec<RespDatum>,
    #[serde(default)]
    usage: Option<RespUsage>,
}

pub fn parse_openai_response(raw: serde_json::Value) -> Result<EmbedOut, GatewayError> {
    let mut parsed: EmbedResp =
        serde_json::from_value(raw).map_err(|e| GatewayError::Upstream {
            status: 502,
            body: format!("openai embed parse: {e}"),
        })?;
    parsed.data.sort_by_key(|d| d.index);
    let input_tokens = parsed.usage.map(|u| u.total_tokens).unwrap_or(0);
    let vectors = parsed.data.into_iter().map(|d| d.embedding).collect();
    Ok(EmbedOut {
        vectors,
        input_tokens,
    })
}

#[async_trait::async_trait]
impl EmbeddingProvider for OpenAiEmbedder {
    async fn embed(
        &self,
        model: &str,
        inputs: &[String],
        dims: u32,
    ) -> Result<EmbedOut, GatewayError> {
        let resp = self
            .client
            .post(format!("{}/embeddings", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&EmbedReq {
                input: inputs,
                model,
                dimensions: dims,
            })
            .send()
            .await
            .map_err(|e| GatewayError::Upstream {
                status: 502,
                body: format!("openai embed send: {e}"),
            })?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(GatewayError::Upstream { status, body });
        }
        let raw = resp
            .json::<serde_json::Value>()
            .await
            .map_err(|e| GatewayError::Upstream {
                status: 502,
                body: format!("openai embed body: {e}"),
            })?;
        parse_openai_response(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_data_sorted_by_index_and_usage() {
        let raw = serde_json::json!({
            "data": [
                { "index": 1, "embedding": [0.3, 0.4] },
                { "index": 0, "embedding": [0.1, 0.2] }
            ],
            "usage": { "total_tokens": 9 }
        });
        let out = parse_openai_response(raw).unwrap();
        assert_eq!(out.vectors[0], vec![0.1, 0.2]); // re-ordered by index
        assert_eq!(out.vectors[1], vec![0.3, 0.4]);
        assert_eq!(out.input_tokens, 9);
    }
}
