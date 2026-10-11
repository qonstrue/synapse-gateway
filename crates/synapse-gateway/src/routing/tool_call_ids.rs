//! Tool-call ids for upstreams that do not supply ids unique across responses.

/// Mints `<prefix>_<index>` ids. The prefix is random per response, so no two
/// responses share a minted id.
#[derive(Debug, Clone)]
pub struct ToolCallIds {
    prefix: String,
}

impl ToolCallIds {
    /// A fresh prefix: `call_<32 hex chars>`.
    pub fn per_response() -> Self {
        Self {
            prefix: format!("call_{}", uuid::Uuid::new_v4().simple()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_prefix(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
        }
    }

    /// The upstream id when it is non-empty, else this response's id for `index`.
    pub fn id_for(&self, upstream: Option<&str>, index: u32) -> String {
        match upstream.filter(|id| !id.is_empty()) {
            Some(id) => id.to_string(),
            None => format!("{}_{index}", self.prefix),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_responses_never_share_an_id() {
        let (a, b) = (ToolCallIds::per_response(), ToolCallIds::per_response());
        assert_ne!(a.id_for(None, 0), b.id_for(None, 0));
        assert!(a.id_for(None, 0).starts_with("call_"));
        assert_eq!(a.id_for(None, 0).len(), "call_".len() + 32 + "_0".len());
    }

    #[test]
    fn index_distinguishes_calls_within_a_response() {
        let ids = ToolCallIds::with_prefix("call_t");
        assert_eq!(ids.id_for(None, 0), "call_t_0");
        assert_eq!(ids.id_for(None, 1), "call_t_1");
    }

    #[test]
    fn upstream_id_passes_through_and_empty_is_replaced() {
        let ids = ToolCallIds::with_prefix("call_t");
        assert_eq!(ids.id_for(Some("fc-7"), 3), "fc-7");
        assert_eq!(ids.id_for(Some(""), 3), "call_t_3");
    }
}
