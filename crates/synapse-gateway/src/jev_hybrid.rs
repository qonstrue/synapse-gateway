//! Jev hybrid extraction: judge candidates with TypeSafe System One, then run
//! a schema-pinned chat extraction per survivor on the route's chat legs.
//! Spec: `docs/superpowers/specs/2026-09-22-jev-hybrid-extraction-design.md`.

use serde_json::Value;

use crate::error::GatewayError;
use crate::gateway::{collect_committed, Gateway, HybridOutcome, RequestCtx};
use crate::routing::classify::vertex_triggers;
use crate::routing::executor::{execute_buffered_with_timeouts, Completion};
use crate::routing::jev_extract;
use crate::routing::request::{
    ChatRequest, ExtractCandidate, ExtractSpec, Message, ResponseFormat,
};
use crate::routing::table::ChainLeg;

impl Gateway {
    /// Buffered hybrid: `jev_attempt` judges, survivors are extracted per
    /// candidate order on the non-`typesafe` legs. Records ledger rows for
    /// the Jev leg and each extraction under one request id.
    pub(crate) async fn chat_hybrid(
        &self,
        req: ChatRequest,
        ctx: &RequestCtx,
        legs: &[ChainLeg],
        started: std::time::Instant,
        request_id: &str,
    ) -> Result<HybridOutcome, GatewayError> {
        let spec = req
            .jev
            .as_ref()
            .and_then(|j| j.extract.as_ref())
            .expect("chat() validates before dispatch");
        let rest: Vec<ChainLeg> = legs
            .iter()
            .filter(|l| l.provider != "typesafe")
            .cloned()
            .collect();

        let (answers, jev_completion, mut degraded) = match self.jev_attempt(&req, legs).await? {
            crate::gateway::JevAttempt::Decided {
                completion,
                answers,
            } => {
                let jev_legs = legs.iter().filter(|l| l.provider == "typesafe").count() as u32;
                self.record(
                    ctx,
                    &req.model,
                    "jev",
                    request_id,
                    &completion,
                    jev_legs.max(1),
                    started,
                );
                (answers, Some(completion), false)
            }
            crate::gateway::JevAttempt::Exhausted(_) => (serde_json::json!({}), None, true),
        };

        let degraded_request = jev_completion.is_none();
        let survivor_keys: Vec<String> = jev_extract::survivors(&answers, spec, degraded_request)
            .iter()
            .map(|s| s.to_string())
            .collect();
        let extraction_ran = !survivor_keys.is_empty();

        let mut extractions = serde_json::Map::new();
        let mut input_tokens = jev_completion.as_ref().map(|c| c.input_tokens).unwrap_or(0);
        let mut output_tokens = jev_completion
            .as_ref()
            .map(|c| c.output_tokens)
            .unwrap_or(0);

        for key in &survivor_keys {
            let candidate = spec
                .candidates
                .iter()
                .find(|c| &c.key == key)
                .expect("survivor keys come from spec.candidates");
            match self
                .extract_one(&req, &rest, &req.model, spec, candidate)
                .await
            {
                Ok((value, completion)) => {
                    self.record(
                        ctx,
                        &req.model,
                        if vertex_triggers(&req) {
                            "native"
                        } else {
                            "standard"
                        },
                        request_id,
                        &completion,
                        rest.len() as u32,
                        started,
                    );
                    input_tokens += completion.input_tokens;
                    output_tokens += completion.output_tokens;
                    extractions.insert(key.clone(), value);
                }
                Err(_) => degraded = true,
            }
        }

        self.metrics.jev_extraction(&req.model, degraded);

        Ok(HybridOutcome {
            model: jev_completion
                .as_ref()
                .map(|c| c.model.clone())
                .unwrap_or_else(|| crate::jev_native::DEFAULT_MODEL.to_string()),
            answers,
            survivors: survivor_keys,
            degraded,
            extraction_ran,
            extractions,
            input_tokens,
            output_tokens,
        })
    }

    /// One survivor's extraction against the route's chat legs. Lane choice
    /// mirrors the degrade path: native Vertex when the original request
    /// carries vertex triggers, standard otherwise.
    async fn extract_one(
        &self,
        original: &ChatRequest,
        rest: &[ChainLeg],
        alias: &str,
        spec: &ExtractSpec,
        candidate: &ExtractCandidate,
    ) -> Result<(Value, Completion), GatewayError> {
        let content = jev_extract::substitute_prompt(&spec.prompt, &candidate.key, &candidate.text);
        let message = Message {
            role: "system".into(),
            content: Value::String(content),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };
        let base = ChatRequest {
            model: alias.to_string(),
            messages: vec![message],
            temperature: None,
            max_tokens: None,
            stream: None,
            response_format: None,
            routing_strategy: None,
            vertex: original.vertex.clone(),
            jev: None,
            tools: None,
            tool_choice: None,
            passthrough: serde_json::Map::new(),
        };

        let completion = if vertex_triggers(original) {
            let mut vertex = original.vertex.clone().unwrap_or_default();
            vertex.response_schema = Some(spec.response_schema.clone());
            let req = ChatRequest {
                vertex: Some(vertex),
                ..base
            };
            let committed = self.native_committed(&req, rest).await?;
            collect_committed(committed).await?
        } else {
            let req = ChatRequest {
                response_format: Some(ResponseFormat {
                    kind: "json_schema".into(),
                    json_schema: Some(spec.response_schema.clone()),
                }),
                ..base
            };
            execute_buffered_with_timeouts(&self.catalog, alias, rest, &req, self.timeouts).await?
        };

        let value: Value = serde_json::from_str(&completion.content)
            .unwrap_or_else(|_| Value::String(completion.content.clone()));
        Ok((value, completion))
    }
}
