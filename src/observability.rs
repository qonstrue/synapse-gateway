//! OpenLLMetry `gen_ai.*` span attributes + Prometheus metric emission.
//! Both lanes emit the same shape so native-Vertex calls are not blind.

use metrics::{counter, histogram, Label};

use crate::routing::classify::Lane;
use crate::routing::executor::Completion;

/// Attributes describing one served request, used to build a tracing span and
/// to emit metrics. `provider`/`model` reflect the leg that actually served.
#[derive(Debug, Clone)]
pub struct GenAiSpan {
    pub system: &'static str, // gen_ai.system: vertexai|openai|dashscope|oai_compat
    pub request_model: String,
    pub response_model: String,
    pub lane: Lane,
    pub route: String,
    pub tenant: String,
    pub workspace: Option<String>,
    pub legs_attempted: u32,
    pub stream: bool,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Map a provider id to its OpenLLMetry `gen_ai.system` value.
pub fn system_for(provider: &str) -> &'static str {
    match provider {
        "vertex" => "vertexai",
        "openai" => "openai",
        "qwen" => "dashscope",
        _ => "oai_compat",
    }
}

impl GenAiSpan {
    pub fn from_completion(
        c: &Completion,
        lane: Lane,
        route: &str,
        tenant: &str,
        workspace: Option<&str>,
        legs_attempted: u32,
        stream: bool,
    ) -> Self {
        Self {
            system: system_for(&c.provider),
            request_model: route.to_string(),
            response_model: c.model.clone(),
            lane,
            route: route.to_string(),
            tenant: tenant.to_string(),
            workspace: workspace.map(str::to_string),
            legs_attempted,
            stream,
            input_tokens: c.input_tokens,
            output_tokens: c.output_tokens,
        }
    }

    /// Emit Prometheus counters/histograms. Span emission (tracing) is wired in
    /// the server handler via `tracing::info_span!` using these same fields.
    pub fn emit_metrics(&self, latency_secs: f64) {
        let lane = match self.lane {
            Lane::Standard => "standard",
            Lane::NativeVertex => "native",
        };
        let labels: Vec<Label> = vec![
            Label::new("route", self.route.clone()),
            Label::new("model", self.response_model.clone()),
            Label::new("system", self.system),
            Label::new("lane", lane),
        ];
        counter!("synapse_requests_total", labels.clone()).increment(1);
        histogram!("synapse_request_duration_seconds", labels.clone()).record(latency_secs);
        counter!("synapse_input_tokens_total", labels.clone()).increment(self.input_tokens);
        counter!("synapse_output_tokens_total", labels).increment(self.output_tokens);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_provider_to_gen_ai_system() {
        assert_eq!(system_for("vertex"), "vertexai");
        assert_eq!(system_for("qwen"), "dashscope");
        assert_eq!(system_for("openai"), "openai");
        assert_eq!(system_for("oai_compat"), "oai_compat");
    }

    #[test]
    fn from_completion_carries_actual_served_model() {
        let c = Completion {
            provider: "vertex".into(),
            model: "gemini-3-flash".into(),
            content: "x".into(),
            tool_calls: Vec::new(),
            finish_reason: crate::routing::stream::FinishReason::Stop,
            input_tokens: 1,
            output_tokens: 2,
        };
        let span =
            GenAiSpan::from_completion(&c, Lane::Standard, "fast", "acme", Some("ws1"), 2, false);
        assert_eq!(span.system, "vertexai");
        assert_eq!(span.response_model, "gemini-3-flash");
        assert_eq!(span.workspace.as_deref(), Some("ws1"));
        assert_eq!(span.input_tokens, 1);
    }
}
