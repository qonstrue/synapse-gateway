//! Reasoning effort a route tier asks of its legs, and how each lane sends it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Ordered easiest → hardest, so `Ord` compares intensity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    /// A keyword from JSON (e.g. a client's `reasoning_effort`); `None` when not a known keyword.
    pub fn from_value(v: &Value) -> Option<Self> {
        Self::deserialize(v).ok()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// One step harder, saturating at `Max`.
    pub fn bump(self) -> Self {
        match self {
            Self::None => Self::Minimal,
            Self::Minimal => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Xhigh,
            Self::Xhigh | Self::Max => Self::Max,
        }
    }

    /// Standard-lane option. `None` sends nothing: non-reasoning models reject
    /// `reasoning_effort` outright.
    pub fn to_genai(self) -> Option<genai::chat::ReasoningEffort> {
        use genai::chat::ReasoningEffort as R;
        match self {
            Self::None => Option::None,
            Self::Minimal => Some(R::Minimal),
            Self::Low => Some(R::Low),
            Self::Medium => Some(R::Medium),
            Self::High => Some(R::High),
            Self::Xhigh => Some(R::XHigh),
            Self::Max => Some(R::Max),
        }
    }

    /// Native-Vertex `thinkingBudget`. `None` sends no thinking config.
    pub fn thinking_budget(self) -> Option<u32> {
        match self {
            Self::None => Option::None,
            Self::Minimal => Some(512),
            Self::Low => Some(1024),
            Self::Medium => Some(4096),
            Self::High => Some(8192),
            Self::Xhigh => Some(16384),
            Self::Max => Some(24576),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lowercase_keywords_from_json() {
        assert_eq!(
            Effort::from_value(&serde_json::json!("xhigh")),
            Some(Effort::Xhigh)
        );
        assert_eq!(
            Effort::from_value(&serde_json::json!("none")),
            Some(Effort::None)
        );
        assert_eq!(Effort::from_value(&serde_json::json!("extreme")), None);
        assert_eq!(Effort::from_value(&serde_json::json!(3)), None);
    }

    #[test]
    fn bump_steps_up_and_saturates_at_max() {
        let chain: Vec<Effort> = std::iter::successors(Some(Effort::None), |e| {
            Some(e.bump()).filter(|next| next != e)
        })
        .collect();
        assert_eq!(
            chain,
            vec![
                Effort::None,
                Effort::Minimal,
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::Xhigh,
                Effort::Max
            ]
        );
        assert_eq!(Effort::Max.bump(), Effort::Max);
    }

    #[test]
    fn none_sends_nothing_on_either_lane() {
        assert!(Effort::None.to_genai().is_none());
        assert!(Effort::None.thinking_budget().is_none());
    }

    #[test]
    fn maps_to_genai_keywords() {
        assert_eq!(Effort::High.to_genai().unwrap().variant_name(), "high");
        assert_eq!(Effort::Xhigh.to_genai().unwrap().variant_name(), "xhigh");
        assert_eq!(
            Effort::Minimal.to_genai().unwrap().variant_name(),
            "minimal"
        );
    }

    #[test]
    fn maps_to_vertex_thinking_budgets() {
        assert_eq!(Effort::Minimal.thinking_budget(), Some(512));
        assert_eq!(Effort::Low.thinking_budget(), Some(1024));
        assert_eq!(Effort::Medium.thinking_budget(), Some(4096));
        assert_eq!(Effort::High.thinking_budget(), Some(8192));
        assert_eq!(Effort::Xhigh.thinking_budget(), Some(16384));
        assert_eq!(Effort::Max.thinking_budget(), Some(24576));
    }

    #[test]
    fn as_str_round_trips_through_serde() {
        [
            Effort::None,
            Effort::Minimal,
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::Xhigh,
            Effort::Max,
        ]
        .into_iter()
        .for_each(|e| assert_eq!(Effort::from_value(&serde_json::json!(e.as_str())), Some(e)));
    }
}
