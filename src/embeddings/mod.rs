//! Embeddings engine: OpenAI-shaped types, the provider trait, and batch splitting.
use crate::error::GatewayError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod openai;
pub mod vertex;

/// One provider call's output: a vector per input (input order) + input tokens.
#[derive(Debug, Clone)]
pub struct EmbedOut {
    pub vectors: Vec<Vec<f32>>,
    pub input_tokens: u64,
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed `inputs`, pinning output to `dims`. One vector per input, in order.
    async fn embed(
        &self,
        model: &str,
        inputs: &[String],
        dims: u32,
    ) -> Result<EmbedOut, GatewayError>;
}

/// OpenAI-shaped request. `input` accepts a single string or an array.
#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddingRequest {
    pub input: EmbeddingInput,
    pub model: String,
    #[serde(default)]
    pub dimensions: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    One(String),
    Many(Vec<String>),
}

impl EmbeddingInput {
    pub fn into_vec(self) -> Vec<String> {
        match self {
            EmbeddingInput::One(s) => vec![s],
            EmbeddingInput::Many(v) => v,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EmbeddingResponse {
    pub object: &'static str, // "list"
    pub data: Vec<EmbeddingData>,
    pub model: String,
    pub usage: EmbeddingUsage,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmbeddingData {
    pub object: &'static str, // "embedding"
    pub index: usize,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmbeddingUsage {
    pub prompt_tokens: u64,
    pub total_tokens: u64,
}

/// Split `inputs` into contiguous batches no larger than `limit`, preserving order.
pub fn split_batches(inputs: &[String], limit: usize) -> Vec<&[String]> {
    if inputs.is_empty() {
        return vec![];
    }
    inputs.chunks(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_batches_preserves_order_and_limit() {
        let v: Vec<String> = (0..5).map(|i| i.to_string()).collect();
        let batches = split_batches(&v, 2);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0], &["0".to_string(), "1".to_string()]);
        assert_eq!(batches[2], &["4".to_string()]);
        assert!(split_batches(&[], 2).is_empty());
    }

    #[test]
    fn input_into_vec_handles_one_and_many() {
        assert_eq!(
            EmbeddingInput::One("a".into()).into_vec(),
            vec!["a".to_string()]
        );
        assert_eq!(
            EmbeddingInput::Many(vec!["a".into(), "b".into()])
                .into_vec()
                .len(),
            2
        );
    }
}
