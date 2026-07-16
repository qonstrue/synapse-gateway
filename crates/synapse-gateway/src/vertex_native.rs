//! Native Vertex REST lane: preserves cachedContents, gs:// media URIs, and
//! strict responseSchema that the OpenAI-compatible standard lane cannot express.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::providers::vertex_auth::VertexAuth;
use crate::routing::executor::Completion;
use crate::routing::request::{ChatRequest, VertexExt};
use crate::routing::stream::{FinishReason, StreamItem};

/// Total-response ceiling for streamed passthrough requests, overriding the
/// shared client timeout (which is sized for buffered calls).
const PASSTHROUGH_STREAM_TIMEOUT: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone)]
pub struct VertexNativeProvider {
    http: reqwest::Client,
    auth: Arc<VertexAuth>,
    project: String,
    /// Default region, used when a leg carries no per-leg `region` override.
    region: String,
    /// Explicit host override (a wiremock URI in tests, or a custom endpoint).
    /// When set it wins for every region; otherwise the host is derived from the
    /// effective region so a per-leg override can target a different location.
    endpoint_override: Option<String>,
}

impl VertexNativeProvider {
    pub fn new(
        auth: Arc<VertexAuth>,
        project: String,
        region: String,
        request_timeout: Duration,
        endpoint_override: Option<String>,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(request_timeout)
                .build()
                .unwrap(),
            auth,
            project,
            region,
            endpoint_override,
        }
    }

    /// Resolve the API host for a region: the explicit override if configured,
    /// else Vertex's regional host (`global` has its own non-prefixed host).
    fn endpoint_for(&self, region: &str) -> String {
        if let Some(base) = &self.endpoint_override {
            base.clone()
        } else if region == "global" {
            "https://aiplatform.googleapis.com".into()
        } else {
            format!("https://{region}-aiplatform.googleapis.com")
        }
    }

    fn generate_url(&self, model: &str, region: &str) -> String {
        format!(
            "{}/v1/projects/{}/locations/{}/publishers/google/models/{}:generateContent",
            self.endpoint_for(region),
            self.project,
            region,
            model
        )
    }

    /// SUPERSEDED by `stream_generate` + the buffered aggregator. The live native
    /// path (both stream and non-stream via `collect_committed`) goes through
    /// `stream_generate`; this unary call is retained only for its endpoint/auth
    /// tests. Note `parse_response` does NOT extract tool calls — do not re-wire
    /// the request path back through here without restoring that.
    pub async fn generate(
        &self,
        model: &str,
        req: &ChatRequest,
        region: Option<&str>,
    ) -> Result<Completion, crate::error::GatewayError> {
        let region = region.unwrap_or(&self.region);
        let ext = req.vertex.clone().unwrap_or_default();
        let payload = build_payload(req, &ext);
        let token = self
            .auth
            .token()
            .await
            .map_err(|e| crate::error::GatewayError::Upstream {
                status: 401,
                body: format!("vertex auth: {e}"),
            })?;

        let resp = self
            .http
            .post(self.generate_url(model, region))
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| crate::error::GatewayError::Upstream {
                status: 502,
                body: e.to_string(),
            })?;

        let status = resp.status();
        let value: Value = resp
            .json()
            .await
            .map_err(|e| crate::error::GatewayError::Upstream {
                status: status.as_u16(),
                body: e.to_string(),
            })?;
        if !status.is_success() {
            if status.is_client_error() {
                return Err(crate::error::GatewayError::BadRequest(format!(
                    "vertex {}: {}",
                    status.as_u16(),
                    value
                )));
            }
            return Err(crate::error::GatewayError::Upstream {
                status: status.as_u16(),
                body: value.to_string(),
            });
        }
        parse_response("vertex", model, &value)
    }

    /// Forward a raw Gemini-native `models/{model}:{action}` request to Vertex,
    /// re-authenticated with the gateway's own credentials. Used by the
    /// passthrough surface (`server::gemini_passthrough`), which meters usage
    /// from the response's `usageMetadata`; the body is forwarded verbatim.
    pub async fn passthrough_request(
        &self,
        model: &str,
        action: &str,
        alt_sse: bool,
        body: Value,
    ) -> Result<reqwest::Response, crate::error::GatewayError> {
        let region = &self.region;
        let alt = if alt_sse { "?alt=sse" } else { "" };
        let url = format!(
            "{}/v1/projects/{}/locations/{}/publishers/google/models/{}:{}{}",
            self.endpoint_for(region),
            self.project,
            region,
            model,
            action,
            alt
        );
        let token = self
            .auth
            .token()
            .await
            .map_err(|e| crate::error::GatewayError::Upstream {
                status: 401,
                body: format!("vertex auth: {e}"),
            })?;
        let mut request = self.http.post(url).bearer_auth(token).json(&body);
        if alt_sse {
            // The shared client's request timeout covers the WHOLE response
            // body. Long agent generations routinely stream past it, which
            // surfaced client-side as mid-stream read errors — so streamed
            // passthrough gets its own generous ceiling instead.
            request = request.timeout(PASSTHROUGH_STREAM_TIMEOUT);
        }
        request
            .send()
            .await
            .map_err(|e| crate::error::GatewayError::Upstream {
                status: 502,
                body: e.to_string(),
            })
    }

    fn stream_url(&self, model: &str, region: &str) -> String {
        format!(
            "{}/v1/projects/{}/locations/{}/publishers/google/models/{}:streamGenerateContent?alt=sse",
            self.endpoint_for(region),
            self.project,
            region,
            model
        )
    }

    /// Open a Vertex SSE stream and normalize chunks into `StreamItem`s.
    ///
    /// The outer `Err` is the connection phase, returned as a `GatewayError` so
    /// the handler preserves Vertex's status distinction (4xx -> 400 BadRequest,
    /// 5xx/transport -> 502 Upstream) — the same mapping the non-stream
    /// `generate` used. Per-item (mid-stream) errors remain `LegError`.
    pub async fn stream_generate(
        &self,
        model: &str,
        req: &ChatRequest,
        region: Option<&str>,
    ) -> Result<
        impl futures::Stream<Item = Result<StreamItem, crate::routing::executor::LegError>>,
        crate::error::GatewayError,
    > {
        use crate::error::GatewayError;
        use crate::routing::executor::LegError;
        use crate::routing::stream::FinishReason;
        use futures::StreamExt;

        let region = region.unwrap_or(&self.region);
        let ext = req.vertex.clone().unwrap_or_default();
        let payload = build_payload(req, &ext);
        let token = self
            .auth
            .token()
            .await
            .map_err(|e| GatewayError::Upstream {
                status: 401,
                body: format!("vertex auth: {e}"),
            })?;

        let resp = self
            .http
            .post(self.stream_url(model, region))
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| GatewayError::Upstream {
                status: 502,
                body: e.to_string(),
            })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            if status.is_client_error() {
                return Err(GatewayError::BadRequest(format!(
                    "vertex {}: {body}",
                    status.as_u16()
                )));
            }
            return Err(GatewayError::Upstream {
                status: status.as_u16(),
                body,
            });
        }

        // State threaded across byte chunks via `unfold`: SSE line buffer,
        // a running tool index, a saw_tool flag (to override the finish reason),
        // and a queue of already-parsed items ready to yield.
        struct St<S> {
            inner: S,
            // Raw byte buffer (NOT String): a multi-byte UTF-8 codepoint can be
            // split across two `bytes_stream()` chunks, so we must not lossily
            // decode per chunk. We decode only whole `\n\n`-terminated events,
            // whose boundary is always on an ASCII byte.
            buf: Vec<u8>,
            tool_index: u32,
            saw_tool: bool,
            pending: std::collections::VecDeque<Result<StreamItem, LegError>>,
        }
        let state = St {
            inner: Box::pin(resp.bytes_stream()),
            buf: Vec::new(),
            tool_index: 0,
            saw_tool: false,
            pending: std::collections::VecDeque::new(),
        };

        let items = futures::stream::unfold(state, |mut st| async move {
            loop {
                if let Some(item) = st.pending.pop_front() {
                    return Some((item, st));
                }
                match st.inner.next().await {
                    None => return None,
                    Some(Err(e)) => return Some((Err(LegError::MidStream(e.to_string())), st)),
                    Some(Ok(bytes)) => {
                        st.buf.extend_from_slice(&bytes);
                        // Drain complete SSE events (separated by a blank line).
                        // Vertex/gemini-3 terminates events with CRLF (`\r\n\r\n`),
                        // not just `\n\n`; match either or every event is dropped
                        // (empty content + 0 tokens). The partial tail (possibly
                        // mid-codepoint) stays in `buf` until its terminator arrives.
                        while let Some((pos, sep_len)) = next_sse_boundary(&st.buf) {
                            let event_bytes: Vec<u8> = st.buf.drain(..pos + sep_len).collect();
                            let event = String::from_utf8_lossy(&event_bytes);
                            for line in event.lines() {
                                let data = match line.strip_prefix("data:") {
                                    Some(d) => d.trim(),
                                    None => continue,
                                };
                                if data == "[DONE]" || data.is_empty() {
                                    continue;
                                }
                                match serde_json::from_str::<serde_json::Value>(data) {
                                    Ok(json) => {
                                        for mut item in
                                            vertex_chunk_to_items(&json, &mut st.tool_index)
                                        {
                                            if matches!(item, StreamItem::ToolCallDelta { .. }) {
                                                st.saw_tool = true;
                                            }
                                            if let StreamItem::Done { finish_reason, .. } =
                                                &mut item
                                            {
                                                if st.saw_tool {
                                                    *finish_reason = FinishReason::ToolCalls;
                                                }
                                            }
                                            st.pending.push_back(Ok(item));
                                        }
                                    }
                                    Err(e) => st.pending.push_back(Err(LegError::MidStream(
                                        format!("bad sse json: {e}"),
                                    ))),
                                }
                            }
                        }
                        // loop back: either yield from `pending` or poll more bytes
                    }
                }
            }
        });

        Ok(items)
    }
}

/// Find the next SSE event boundary (a blank line) in `buf`, returning its start
/// offset and separator byte length. Handles both `\n\n` (LF) and `\r\n\r\n`
/// (CRLF) — Vertex/gemini-3 emits CRLF, and matching only `\n\n` drops every
/// event (empty content + 0 tokens). Picks the earliest boundary.
fn next_sse_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = buf
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|p| (p, 2usize));
    let crlf = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| (p, 4usize));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Build a Vertex `generateContent`/`streamGenerateContent` body, threading
/// native features (cache, media, schema) AND tool calling through.
fn build_payload(req: &ChatRequest, ext: &VertexExt) -> Value {
    let media_parts = ext
        .media_uris
        .iter()
        .flatten()
        .map(|uri| json!({ "fileData": { "fileUri": uri, "mimeType": "video/mp4" } }))
        .collect::<Vec<_>>();

    // One Vertex `content` per gateway message, mapping roles + tool turns.
    let mut contents: Vec<Value> = Vec::new();
    for m in &req.messages {
        match m.role.as_str() {
            "assistant" if m.tool_calls.is_some() => {
                let parts = m
                    .tool_calls
                    .as_ref()
                    .unwrap()
                    .iter()
                    .filter_map(|tc| {
                        let f = tc.get("function")?;
                        let name = f.get("name")?.as_str()?;
                        let raw = f.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}");
                        let args: Value = serde_json::from_str(raw).unwrap_or_else(|_| json!({}));
                        Some(json!({ "functionCall": { "name": name, "args": args } }))
                    })
                    .collect::<Vec<_>>();
                contents.push(json!({ "role": "model", "parts": parts }));
            }
            "tool" => {
                let name = m.name.clone().unwrap_or_default();
                let response: Value = m
                    .content
                    .as_str()
                    .map(|s| json!({ "content": s }))
                    .unwrap_or_else(|| json!({ "content": m.content.to_string() }));
                contents.push(json!({ "role": "user", "parts": [
                    { "functionResponse": { "name": name, "response": response } }
                ]}));
            }
            role => {
                let vrole = if role == "assistant" { "model" } else { "user" };
                let parts = crate::routing::content_parts::content_to_vertex_parts(&m.content);
                contents.push(json!({ "role": vrole, "parts": parts }));
            }
        }
    }
    // Attach media to the last user content (or a fresh one) if present.
    if !media_parts.is_empty() {
        if let Some(last) = contents.iter_mut().rev().find(|c| c["role"] == "user") {
            if let Some(arr) = last["parts"].as_array_mut() {
                arr.extend(media_parts);
            }
        } else {
            contents.push(json!({ "role": "user", "parts": media_parts }));
        }
    }

    let mut body = json!({ "contents": contents });

    if let Some(cache) = &ext.cached_content {
        body["cachedContent"] = json!(cache);
    }
    // Assemble generationConfig from every native knob in one place so that
    // schema, temperature, token cap, and thinking budget compose cleanly
    // (a missing schema must not drop a maxOutputTokens/thinkingConfig).
    let mut gen_cfg = serde_json::Map::new();
    if let Some(schema) = &ext.response_schema {
        gen_cfg.insert("responseMimeType".into(), json!("application/json"));
        gen_cfg.insert("responseSchema".into(), schema.clone());
    }
    if let Some(t) = req.temperature {
        gen_cfg.insert("temperature".into(), json!(t));
    }
    if let Some(max) = req.max_tokens {
        gen_cfg.insert("maxOutputTokens".into(), json!(max));
    }
    if let Some(thinking) = &ext.thinking_config {
        gen_cfg.insert("thinkingConfig".into(), thinking.clone());
    }
    if !gen_cfg.is_empty() {
        body["generationConfig"] = Value::Object(gen_cfg);
    }
    if let Some(tools) = &req.tools {
        let decls = tools.iter().filter_map(|t| {
            let f = t.get("function")?;
            Some(json!({
                "name": f.get("name")?.as_str()?,
                "description": f.get("description").and_then(|d| d.as_str()).unwrap_or(""),
                "parameters": f.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object"})),
            }))
        }).collect::<Vec<_>>();
        if !decls.is_empty() {
            body["tools"] = json!([{ "functionDeclarations": decls }]);
        }
    }
    if let Some(choice) = &req.tool_choice {
        let mode = match choice {
            Value::String(s) if s == "none" => "NONE",
            Value::String(s) if s == "required" => "ANY",
            Value::String(_) => "AUTO",
            Value::Object(_) => "ANY",
            _ => "AUTO",
        };
        body["toolConfig"] = json!({ "functionCallingConfig": { "mode": mode } });
    }

    body
}

/// A Vertex "thinking" part (`{"text": "...", "thought": true}`) carries the
/// model's reasoning, not answer content. It must be excluded from the emitted
/// text — otherwise a thinking model (Gemini 3, Gemini 2.5) pollutes or, under
/// a `responseSchema`, invalidates the structured output.
fn is_thought_part(p: &Value) -> bool {
    p.get("thought").and_then(Value::as_bool).unwrap_or(false)
}

/// Map a Vertex response into the shared `Completion`, extracting usage.
fn parse_response(
    provider: &str,
    model: &str,
    v: &Value,
) -> Result<Completion, crate::error::GatewayError> {
    let content = v["candidates"][0]["content"]["parts"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter(|p| !is_thought_part(p))
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
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        input_tokens: usage["promptTokenCount"].as_u64().unwrap_or(0),
        output_tokens: usage["candidatesTokenCount"].as_u64().unwrap_or(0),
    })
}

/// Map a Vertex finishReason string to our FinishReason.
fn map_vertex_finish(s: &str) -> FinishReason {
    match s {
        "MAX_TOKENS" => FinishReason::Length,
        "STOP" => FinishReason::Stop,
        _ => FinishReason::Stop,
    }
}

/// Convert one Vertex stream chunk into zero or more `StreamItem`s.
/// `tool_index` is a running counter the caller threads across the whole stream
/// so synthesized ids (`call_{n}`) and indices stay stable and unique.
pub fn vertex_chunk_to_items(chunk: &Value, tool_index: &mut u32) -> Vec<StreamItem> {
    let mut out = Vec::new();
    if let Some(parts) = chunk["candidates"][0]["content"]["parts"].as_array() {
        for p in parts {
            if is_thought_part(p) {
                continue;
            }
            if let Some(text) = p["text"].as_str() {
                if !text.is_empty() {
                    out.push(StreamItem::Delta(text.to_string()));
                }
            } else if let Some(fc) = p.get("functionCall") {
                let name = fc["name"].as_str().unwrap_or_default().to_string();
                let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                let i = *tool_index;
                *tool_index += 1;
                out.push(StreamItem::ToolCallDelta {
                    index: i,
                    id: Some(format!("call_{i}")),
                    name: Some(name),
                    args_fragment: args.to_string(),
                });
            }
        }
    }
    // Emit the terminal Done only when Vertex signals end-of-turn via
    // `finishReason`. Gemini includes (cumulative) `usageMetadata` on
    // intermediate chunks too, so gating on usage presence would emit a
    // spurious Done per chunk — harmless for the buffered accumulator but it
    // would inject premature `finish_reason` chunks into the SSE stream.
    // The tool-call finish override (functionCall ends with finishReason STOP)
    // is applied by the stream driver via its `saw_tool` flag.
    if let Some(finish) = chunk["candidates"][0]["finishReason"].as_str() {
        let usage = &chunk["usageMetadata"];
        out.push(StreamItem::Done {
            input_tokens: usage["promptTokenCount"].as_u64().unwrap_or(0),
            output_tokens: usage["candidatesTokenCount"].as_u64().unwrap_or(0),
            finish_reason: map_vertex_finish(finish),
        });
    }
    out
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
            ..Default::default()
        };
        let body = build_payload(&req_with(ext.clone()), &ext);
        assert_eq!(
            body["cachedContent"],
            serde_json::json!("cachedContents/abc")
        );
        assert_eq!(
            body["generationConfig"]["responseSchema"],
            serde_json::json!({"type": "object"})
        );
        let parts = body["contents"][0]["parts"].as_array().unwrap();
        assert!(parts
            .iter()
            .any(|p| p["fileData"]["fileUri"] == "gs://bucket/v.mp4"));
    }

    #[test]
    fn payload_includes_max_output_tokens_and_thinking_config() {
        let ext = VertexExt {
            thinking_config: Some(serde_json::json!({ "thinkingLevel": "low" })),
            ..Default::default()
        };
        let mut req = req_with(ext.clone());
        req.max_tokens = Some(8192);
        let body = build_payload(&req, &ext);
        assert_eq!(
            body["generationConfig"]["maxOutputTokens"],
            serde_json::json!(8192)
        );
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            serde_json::json!({ "thinkingLevel": "low" })
        );
    }

    #[test]
    fn vertex_chunk_skips_thought_parts() {
        use crate::routing::stream::StreamItem;
        let mut idx = 0u32;
        let chunk = serde_json::json!({
            "candidates": [{"content": {"role": "model", "parts": [
                {"text": "internal reasoning", "thought": true},
                {"text": "answer"}
            ]}}]
        });
        let items = vertex_chunk_to_items(&chunk, &mut idx);
        assert_eq!(items, vec![StreamItem::Delta("answer".into())]);
    }

    #[test]
    fn parse_response_skips_thought_parts() {
        let v = serde_json::json!({
            "candidates": [{"content": {"parts": [
                {"text": "reasoning", "thought": true},
                {"text": "real"}
            ], "role": "model"}}],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1}
        });
        let c = parse_response("vertex", "gemini-3-pro", &v).unwrap();
        assert_eq!(c.content, "real");
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

    #[test]
    fn payload_includes_tools_and_function_messages() {
        let r: ChatRequest = serde_json::from_value(serde_json::json!({
            "model": "gemini-pro",
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_0", "type": "function", "function": {"name": "get_weather", "arguments": "{\"c\":\"SF\"}"}}]},
                {"role": "tool", "tool_call_id": "call_0", "content": "21C"}
            ],
            "tools": [{"type": "function", "function": {"name": "get_weather",
                "description": "Lookup", "parameters": {"type": "object"}}}],
            "tool_choice": "auto"
        })).unwrap();
        let body = build_payload(&r, &VertexExt::default());
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["name"],
            "get_weather"
        );
        assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "AUTO");
        let contents = body["contents"].as_array().unwrap();
        assert!(contents
            .iter()
            .any(|c| c["parts"][0]["functionCall"]["name"] == "get_weather"));
        assert!(contents
            .iter()
            .any(|c| c["parts"][0]["functionResponse"]["name"].is_string()));
    }

    #[test]
    fn parses_vertex_chunk_text_and_functioncall() {
        use crate::routing::stream::{FinishReason, StreamItem};
        let mut idx = 0u32;

        let text_chunk = serde_json::json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "Hi"}]}}]
        });
        let items = vertex_chunk_to_items(&text_chunk, &mut idx);
        assert_eq!(items, vec![StreamItem::Delta("Hi".into())]);

        let fc_chunk = serde_json::json!({
            "candidates": [{"content": {"role": "model", "parts": [
                {"functionCall": {"name": "get_weather", "args": {"c": "SF"}}}]}}]
        });
        let items = vertex_chunk_to_items(&fc_chunk, &mut idx);
        assert_eq!(
            items,
            vec![StreamItem::ToolCallDelta {
                index: 0,
                id: Some("call_0".into()),
                name: Some("get_weather".into()),
                args_fragment: "{\"c\":\"SF\"}".into(),
            }]
        );

        let final_chunk = serde_json::json!({
            "candidates": [{"finishReason": "STOP", "content": {"role": "model", "parts": []}}],
            "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 3}
        });
        let items = vertex_chunk_to_items(&final_chunk, &mut idx);
        assert_eq!(
            items,
            vec![StreamItem::Done {
                input_tokens: 7,
                output_tokens: 3,
                finish_reason: FinishReason::Stop
            }]
        );
    }

    #[test]
    fn endpoint_for_region_picks_regional_or_global_host() {
        let auth = Arc::new(VertexAuth::with_fetcher(|| {
            Box::pin(async { Ok(("t".into(), Duration::from_secs(3600))) })
        }));
        let provider = VertexNativeProvider::new(
            auth,
            "p".into(),
            "global".into(),
            Duration::from_secs(5),
            None,
        );
        assert_eq!(
            provider.endpoint_for("global"),
            "https://aiplatform.googleapis.com"
        );
        assert_eq!(
            provider.endpoint_for("us-central1"),
            "https://us-central1-aiplatform.googleapis.com"
        );
    }

    #[tokio::test]
    async fn generate_uses_per_leg_region_override_in_url() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/p/locations/us-central1/publishers/google/models/gemini-x:generateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [{"content": {"parts": [{"text": "ok"}], "role": "model"}}],
                "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1}
            })))
            .mount(&mock)
            .await;

        let auth = Arc::new(VertexAuth::with_fetcher(|| {
            Box::pin(async { Ok(("test-token".into(), Duration::from_secs(3600))) })
        }));
        // Provider default region is `global`; the per-leg override must win.
        let provider = VertexNativeProvider::new(
            auth,
            "p".into(),
            "global".into(),
            Duration::from_secs(5),
            Some(mock.uri()),
        );
        let c = provider
            .generate(
                "gemini-x",
                &req_with(VertexExt::default()),
                Some("us-central1"),
            )
            .await
            .unwrap();
        assert_eq!(c.content, "ok");
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
            auth,
            "p".into(),
            "global".into(),
            Duration::from_secs(5),
            Some(mock.uri()),
        );
        let c = provider
            .generate(
                "gemini-3-pro",
                &req_with(VertexExt {
                    cached_content: Some("cachedContents/x".into()),
                    ..Default::default()
                }),
                None,
            )
            .await
            .unwrap();
        assert_eq!(c.content, "ok");
        assert_eq!(c.input_tokens, 2);
    }

    #[tokio::test]
    async fn stream_generate_yields_text_and_tool_done() {
        use crate::routing::stream::{FinishReason, StreamItem};
        use futures::StreamExt;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let sse = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hi\"}]}}]}\n\n\
                   data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"f\",\"args\":{\"a\":1}}}]}}]}\n\n\
                   data: {\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"role\":\"model\",\"parts\":[]}}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":4}}\n\n";
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/p/locations/global/publishers/google/models/gemini-3-pro:streamGenerateContent"))
            .respond_with(ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse))
            .mount(&mock)
            .await;

        let auth = Arc::new(VertexAuth::with_fetcher(|| {
            Box::pin(async { Ok(("test-token".into(), Duration::from_secs(3600))) })
        }));
        let provider = VertexNativeProvider::new(
            auth,
            "p".into(),
            "global".into(),
            Duration::from_secs(5),
            Some(mock.uri()),
        );

        let req: ChatRequest = serde_json::from_value(serde_json::json!({
            "model":"gemini-3-pro","messages":[{"role":"user","content":"hi"}],"stream":true}))
        .unwrap();
        let mut stream = std::pin::pin!(provider
            .stream_generate("gemini-3-pro", &req, None)
            .await
            .expect("starts"));
        let mut items = Vec::new();
        while let Some(it) = stream.next().await {
            items.push(it.unwrap());
        }

        assert!(items
            .iter()
            .any(|i| matches!(i, StreamItem::Delta(t) if t == "Hi")));
        assert!(items
            .iter()
            .any(|i| matches!(i, StreamItem::ToolCallDelta { name: Some(n), .. } if n == "f")));
        assert!(matches!(
            items.last().unwrap(),
            StreamItem::Done {
                input_tokens: 5,
                output_tokens: 4,
                finish_reason: FinishReason::ToolCalls
            }
        ));
    }

    #[tokio::test]
    async fn stream_generate_parses_crlf_terminated_events() {
        // Vertex / gemini-3 terminates SSE events with CRLF blank lines
        // (`\r\n\r\n`), not `\n\n`. The boundary scanner must handle both, or
        // every event is dropped → empty content + 0 tokens (the native-lane
        // outage). Mirrors a real gemini-3-flash response: answer text in the
        // first chunk, then an empty-text + thoughtSignature final chunk
        // carrying finishReason + usage.
        use crate::routing::stream::{FinishReason, StreamItem};
        use futures::StreamExt;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let sse = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"{\\\"answer\\\":\\\"hi\\\"}\"}]}}],\"usageMetadata\":{\"trafficType\":\"ON_DEMAND\"}}\r\n\r\n\
                   data: {\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"\",\"thoughtSignature\":\"abc\"}]}}],\"usageMetadata\":{\"promptTokenCount\":56,\"candidatesTokenCount\":8}}\r\n\r\n";
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/p/locations/global/publishers/google/models/gemini-3-pro:streamGenerateContent"))
            .respond_with(ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse))
            .mount(&mock)
            .await;

        let auth = Arc::new(VertexAuth::with_fetcher(|| {
            Box::pin(async { Ok(("test-token".into(), Duration::from_secs(3600))) })
        }));
        let provider = VertexNativeProvider::new(
            auth,
            "p".into(),
            "global".into(),
            Duration::from_secs(5),
            Some(mock.uri()),
        );
        let req: ChatRequest = serde_json::from_value(serde_json::json!({
            "model":"gemini-3-pro","messages":[{"role":"user","content":"hi"}],"stream":true}))
        .unwrap();
        let mut stream = std::pin::pin!(provider
            .stream_generate("gemini-3-pro", &req, None)
            .await
            .expect("starts"));
        let mut items = Vec::new();
        while let Some(it) = stream.next().await {
            items.push(it.unwrap());
        }

        let text: String = items
            .iter()
            .filter_map(|i| match i {
                StreamItem::Delta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            text, "{\"answer\":\"hi\"}",
            "answer text must survive CRLF events"
        );
        assert!(matches!(
            items.last().unwrap(),
            StreamItem::Done {
                input_tokens: 56,
                output_tokens: 8,
                finish_reason: FinishReason::Stop
            }
        ));
    }

    #[tokio::test]
    async fn stream_generate_maps_vertex_4xx_to_bad_request() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/p/locations/global/publishers/google/models/gemini-3-pro:streamGenerateContent"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad responseSchema"))
            .mount(&mock)
            .await;

        let auth = Arc::new(VertexAuth::with_fetcher(|| {
            Box::pin(async { Ok(("test-token".into(), Duration::from_secs(3600))) })
        }));
        let provider = VertexNativeProvider::new(
            auth,
            "p".into(),
            "global".into(),
            Duration::from_secs(5),
            Some(mock.uri()),
        );
        let req: ChatRequest = serde_json::from_value(serde_json::json!({
            "model":"gemini-3-pro","messages":[{"role":"user","content":"hi"}],"stream":true}))
        .unwrap();

        let err = provider
            .stream_generate("gemini-3-pro", &req, None)
            .await
            .err()
            .expect("4xx should be an error");
        // A Vertex 4xx must surface as a client error (400), not a flat 502.
        assert!(
            matches!(err, crate::error::GatewayError::BadRequest(_)),
            "expected BadRequest, got {err:?}"
        );
    }
}
