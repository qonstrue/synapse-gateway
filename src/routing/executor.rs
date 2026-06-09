//! Standard-lane executor: walk a route's legs with per-leg breaker + retry.

use std::sync::Arc;

use tap::Pipe;

use crate::error::{GatewayError, LegFailure};
use crate::providers::genai_provider::Provider;
use crate::providers::Catalog;
use crate::resilience::{run_with_classifier, ResilienceError};
use crate::routing::request::ChatRequest;
use crate::routing::table::ChainLeg;

/// Normalised result of one completed LLM call.
#[derive(Debug, Clone)]
pub struct Completion {
    pub provider: String,
    pub model: String,
    pub content: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Build a genai `ChatRequest` from the gateway request's messages.
/// Only text content is mapped in v1 (multimodal goes through the native lane).
fn to_genai_request(req: &ChatRequest) -> genai::chat::ChatRequest {
    let messages = req
        .messages
        .iter()
        .map(|m| {
            let text = m.content.as_str().map(str::to_string).unwrap_or_else(|| m.content.to_string());
            match m.role.as_str() {
                "system" => genai::chat::ChatMessage::system(text),
                "assistant" => genai::chat::ChatMessage::assistant(text),
                _ => genai::chat::ChatMessage::user(text),
            }
        })
        .collect::<Vec<_>>();
    genai::chat::ChatRequest::new(messages)
}

fn to_genai_options(req: &ChatRequest) -> genai::chat::ChatOptions {
    genai::chat::ChatOptions::default()
        .pipe(|o| match req.temperature {
            Some(t) => o.with_temperature(t as f64),
            None => o,
        })
}

/// True if a genai error is worth advancing the chain for (transient/5xx/timeout).
/// 4xx (auth, bad request) are NOT retryable — abort the chain immediately.
///
/// Implementation note: we use STRUCTURED matching on the genai error enum rather than
/// string inspection. The previous implementation checked for `" 500"`, `" 502"`, etc.
/// (space before digits), but genai's `webc::Error::ResponseFailedStatus` Display format
/// is `"Request failed with status code '503 ...'` — digits are preceded by a single-quote,
/// not a space — so the old checks never matched and 5xx errors were treated as non-retryable,
/// causing `execute_chain` to break out of the fallback loop instead of advancing to the
/// next leg.
///
/// Structured approach: match the two web-call wrapper variants
/// (`WebModelCall` / `WebAdapterCall`) that carry a `genai::webc::Error`, then match
/// `ResponseFailedStatus { status, .. }` and call `status.is_server_error()` on the
/// typed `reqwest::StatusCode`. Timeout and connection errors are detected via
/// `webc::Error::Reqwest(e)` with `e.is_timeout() || e.is_connect()`.
/// `genai::Error::HttpError { status, .. }` is also matched for completeness.
fn is_genai_retryable(e: &genai::Error) -> bool {
    /// Check whether a `genai::webc::Error` represents a transient/server error.
    fn webc_retryable(we: &genai::webc::Error) -> bool {
        match we {
            genai::webc::Error::ResponseFailedStatus { status, .. } => status.is_server_error(),
            genai::webc::Error::Reqwest(re) => re.is_timeout() || re.is_connect(),
            _ => false,
        }
    }

    match e {
        genai::Error::WebModelCall { webc_error, .. } => webc_retryable(webc_error),
        genai::Error::WebAdapterCall { webc_error, .. } => webc_retryable(webc_error),
        genai::Error::HttpError { status, .. } => status.is_server_error(),
        _ => false,
    }
}

async fn run_one_leg(
    provider: &Arc<Provider>,
    leg: &ChainLeg,
    req: &ChatRequest,
) -> Result<Completion, ResilienceError<genai::Error>> {
    let client = provider.client.clone();
    let model = leg.model.clone();
    let chat_req = to_genai_request(req);
    let opts = to_genai_options(req);

    let resp = run_with_classifier(
        move || {
            let (client, model, chat_req, opts) =
                (client.clone(), model.clone(), chat_req.clone(), opts.clone());
            async move { client.exec_chat(model, chat_req, Some(&opts)).await }
        },
        provider.profile,
        &provider.breaker,
        provider.label,
        is_genai_retryable,
    )
    .await?;

    let content = resp.first_text().unwrap_or_default().to_string();
    let usage = &resp.usage;
    Ok(Completion {
        provider: leg.provider.clone(),
        model: leg.model.clone(),
        content,
        input_tokens: usage.prompt_tokens.unwrap_or(0).max(0) as u64,
        output_tokens: usage.completion_tokens.unwrap_or(0).max(0) as u64,
    })
}

/// Walk legs in order. Retryable failure (or open breaker) advances; the first
/// non-retryable failure aborts. Returns `AllLegsFailed` if every leg fails.
pub async fn execute_chain(
    catalog: &Catalog,
    route_name: &str,
    legs: &[ChainLeg],
    req: &ChatRequest,
) -> Result<Completion, GatewayError> {
    let mut failures: Vec<LegFailure> = Vec::new();
    let mut all_circuit_open = true;
    for leg in legs {
        let provider = catalog.get(&leg.provider).ok_or_else(|| {
            GatewayError::BadRequest(format!("route '{route_name}' references unbuilt provider '{}'", leg.provider))
        })?;
        match run_one_leg(provider, leg, req).await {
            Ok(c) => return Ok(c),
            Err(ResilienceError::CircuitOpen { name }) => failures.push(LegFailure {
                provider: leg.provider.clone(),
                model: leg.model.clone(),
                message: format!("circuit open: {name}"),
            }),
            Err(ResilienceError::Exhausted(e)) => {
                all_circuit_open = false;
                let retryable = is_genai_retryable(&e);
                failures.push(LegFailure {
                    provider: leg.provider.clone(),
                    model: leg.model.clone(),
                    message: e.to_string(),
                });
                if !retryable {
                    break; // non-retryable: abort the chain
                }
            }
        }
    }
    if all_circuit_open && !failures.is_empty() {
        return Err(GatewayError::AllCircuitsOpen(route_name.to_string()));
    }
    Err(GatewayError::AllLegsFailed { route: route_name.to_string(), failures })
}
