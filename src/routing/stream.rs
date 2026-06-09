//! Lane-agnostic streaming primitives shared by both lanes and both consumers.
//! Pure: no I/O, no provider types. Heavily unit-tested.

use serde_json::{json, Value};

/// Why the model stopped. Maps to the OpenAI `finish_reason` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FinishReason {
    #[default]
    Stop,
    ToolCalls,
    Length,
}

impl FinishReason {
    pub fn as_str(self) -> &'static str {
        match self {
            FinishReason::Stop => "stop",
            FinishReason::ToolCalls => "tool_calls",
            FinishReason::Length => "length",
        }
    }
}

/// A fully-reassembled tool call (buffered output / final message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallOut {
    pub id: String,
    pub name: String,
    /// JSON arguments as a STRING (OpenAI wire shape), reassembled from fragments.
    pub arguments: String,
}

/// One lane-agnostic streaming item. Both lanes normalize their upstream
/// events into this; both consumers read it.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamItem {
    /// A text content delta.
    Delta(String),
    /// A tool-call delta. `index` is the stable position within the (possibly
    /// parallel) tool-call set. `id`/`name` appear on the first delta for an
    /// index; later deltas carry only `args_fragment`.
    ToolCallDelta {
        index: u32,
        id: Option<String>,
        name: Option<String>,
        args_fragment: String,
    },
    /// Terminal item: usage + finish reason. Exactly one per successful stream.
    Done {
        input_tokens: u64,
        output_tokens: u64,
        finish_reason: FinishReason,
    },
}

/// Folds a stream of `StreamItem` into final content + tool calls + usage.
/// Used by the buffered consumer and (for the running totals) by the stream guard.
#[derive(Debug, Default)]
pub struct Accumulator {
    pub content: String,
    pub tool_calls: Vec<ToolCallOut>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub finish_reason: FinishReason,
    pub got_done: bool,
}

impl Accumulator {
    /// Fold one item into the running result.
    ///
    /// Assumes `ToolCallDelta.index` values are **dense and 0-based** — which both
    /// lanes guarantee by construction (the standard lane assigns indices by
    /// first-seen call id, the native lane by a running counter). A sparse index
    /// would leave empty `ToolCallOut` slots; we never produce one.
    pub fn push(&mut self, item: StreamItem) {
        match item {
            StreamItem::Delta(t) => self.content.push_str(&t),
            StreamItem::ToolCallDelta {
                index,
                id,
                name,
                args_fragment,
            } => {
                let i = index as usize;
                if self.tool_calls.len() <= i {
                    self.tool_calls.resize(
                        i + 1,
                        ToolCallOut {
                            id: String::new(),
                            name: String::new(),
                            arguments: String::new(),
                        },
                    );
                }
                let slot = &mut self.tool_calls[i];
                if let Some(id) = id {
                    slot.id = id;
                }
                if let Some(name) = name {
                    slot.name = name;
                }
                slot.arguments.push_str(&args_fragment);
            }
            StreamItem::Done {
                input_tokens,
                output_tokens,
                finish_reason,
            } => {
                self.input_tokens = input_tokens;
                self.output_tokens = output_tokens;
                self.finish_reason = finish_reason;
                self.got_done = true;
            }
        }
    }

    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    /// Serialize the accumulated result as an OpenAI `chat.completion` object.
    pub fn to_openai_response(&self, id: &str, model: &str) -> Value {
        let message = if self.has_tool_calls() {
            json!({
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": self.tool_calls.iter().map(|c| json!({
                    "id": c.id,
                    "type": "function",
                    "function": { "name": c.name, "arguments": c.arguments },
                })).collect::<Vec<_>>(),
            })
        } else {
            json!({ "role": "assistant", "content": self.content })
        };
        json!({
            "id": format!("chatcmpl-{id}"),
            "object": "chat.completion",
            "created": chrono::Utc::now().timestamp(),
            "model": model,
            "choices": [{ "index": 0, "message": message, "finish_reason": self.finish_reason.as_str() }],
            "usage": {
                "prompt_tokens": self.input_tokens,
                "completion_tokens": self.output_tokens,
                "total_tokens": self.input_tokens + self.output_tokens
            }
        })
    }
}

/// Render one `StreamItem` as an OpenAI `chat.completion.chunk` object.
/// Tool-call deltas omit `id`/`function.name` when absent (continuation fragments).
pub fn stream_item_to_sse_json(item: &StreamItem, id: &str, model: &str) -> Value {
    let base = |delta: Value, finish: Value| {
        json!({
            "id": format!("chatcmpl-{id}"),
            "object": "chat.completion.chunk",
            "created": 0,
            "model": model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }]
        })
    };
    match item {
        StreamItem::Delta(t) => base(json!({ "content": t }), Value::Null),
        StreamItem::ToolCallDelta {
            index,
            id: cid,
            name,
            args_fragment,
        } => {
            let mut func = json!({ "arguments": args_fragment });
            if let Some(name) = name {
                func["name"] = json!(name);
            }
            let mut call = json!({ "index": index, "type": "function", "function": func });
            if let Some(cid) = cid {
                call["id"] = json!(cid);
            }
            base(json!({ "tool_calls": [call] }), Value::Null)
        }
        StreamItem::Done { finish_reason, .. } => base(json!({}), json!(finish_reason.as_str())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_reason_strings() {
        assert_eq!(FinishReason::Stop.as_str(), "stop");
        assert_eq!(FinishReason::ToolCalls.as_str(), "tool_calls");
        assert_eq!(FinishReason::Length.as_str(), "length");
    }

    #[test]
    fn accumulates_text_and_usage() {
        let mut acc = Accumulator::default();
        acc.push(StreamItem::Delta("Hel".into()));
        acc.push(StreamItem::Delta("lo".into()));
        acc.push(StreamItem::Done {
            input_tokens: 3,
            output_tokens: 2,
            finish_reason: FinishReason::Stop,
        });
        assert_eq!(acc.content, "Hello");
        assert!(acc.tool_calls.is_empty());
        assert_eq!(acc.input_tokens, 3);
        assert_eq!(acc.output_tokens, 2);
        assert_eq!(acc.finish_reason, FinishReason::Stop);
    }

    #[test]
    fn accumulates_parallel_tool_calls_from_fragments() {
        let mut acc = Accumulator::default();
        acc.push(StreamItem::ToolCallDelta {
            index: 0,
            id: Some("call_a".into()),
            name: Some("f".into()),
            args_fragment: "{\"x\":".into(),
        });
        acc.push(StreamItem::ToolCallDelta {
            index: 1,
            id: Some("call_b".into()),
            name: Some("g".into()),
            args_fragment: "{\"y\":2}".into(),
        });
        acc.push(StreamItem::ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            args_fragment: "1}".into(),
        });
        acc.push(StreamItem::Done {
            input_tokens: 5,
            output_tokens: 9,
            finish_reason: FinishReason::ToolCalls,
        });
        assert_eq!(acc.tool_calls.len(), 2);
        assert_eq!(
            acc.tool_calls[0],
            ToolCallOut {
                id: "call_a".into(),
                name: "f".into(),
                arguments: "{\"x\":1}".into()
            }
        );
        assert_eq!(
            acc.tool_calls[1],
            ToolCallOut {
                id: "call_b".into(),
                name: "g".into(),
                arguments: "{\"y\":2}".into()
            }
        );
        assert_eq!(acc.finish_reason, FinishReason::ToolCalls);
    }

    #[test]
    fn buffered_json_text_response() {
        let mut acc = Accumulator::default();
        acc.push(StreamItem::Delta("hi".into()));
        acc.push(StreamItem::Done {
            input_tokens: 1,
            output_tokens: 1,
            finish_reason: FinishReason::Stop,
        });
        let v = acc.to_openai_response("abc", "gemini-3-pro");
        assert_eq!(v["object"], "chat.completion");
        assert_eq!(v["choices"][0]["message"]["content"], "hi");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        assert_eq!(v["usage"]["total_tokens"], 2);
        assert!(v["choices"][0]["message"].get("tool_calls").is_none());
    }

    #[test]
    fn buffered_json_tool_call_response() {
        let mut acc = Accumulator::default();
        acc.push(StreamItem::ToolCallDelta {
            index: 0,
            id: Some("call_0".into()),
            name: Some("f".into()),
            args_fragment: "{}".into(),
        });
        acc.push(StreamItem::Done {
            input_tokens: 4,
            output_tokens: 2,
            finish_reason: FinishReason::ToolCalls,
        });
        let v = acc.to_openai_response("abc", "m");
        assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
        assert!(v["choices"][0]["message"]["content"].is_null());
        let tc = &v["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tc["id"], "call_0");
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "f");
        assert_eq!(tc["function"]["arguments"], "{}");
    }

    #[test]
    fn sse_text_chunk() {
        let v = stream_item_to_sse_json(&StreamItem::Delta("hi".into()), "abc", "m");
        assert_eq!(v["object"], "chat.completion.chunk");
        assert_eq!(v["choices"][0]["delta"]["content"], "hi");
        assert!(v["choices"][0]["finish_reason"].is_null());
    }

    #[test]
    fn sse_tool_call_chunk() {
        let item = StreamItem::ToolCallDelta {
            index: 2,
            id: Some("call_2".into()),
            name: Some("f".into()),
            args_fragment: "{\"a\":1}".into(),
        };
        let v = stream_item_to_sse_json(&item, "abc", "m");
        let tc = &v["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["index"], 2);
        assert_eq!(tc["id"], "call_2");
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "f");
        assert_eq!(tc["function"]["arguments"], "{\"a\":1}");
    }

    #[test]
    fn sse_done_chunk_sets_finish_reason() {
        let item = StreamItem::Done {
            input_tokens: 1,
            output_tokens: 1,
            finish_reason: FinishReason::ToolCalls,
        };
        let v = stream_item_to_sse_json(&item, "abc", "m");
        assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(v["choices"][0]["delta"], serde_json::json!({}));
    }
}
