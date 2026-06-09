//! Pure lane detection: standard vs native-Vertex.

use crate::routing::request::ChatRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Standard,
    NativeVertex,
}

/// Classify by inspecting the request for native-Vertex triggers.
/// Pure and allocation-free; safe to call on the hot path.
pub fn classify(req: &ChatRequest) -> Lane {
    let v = match &req.vertex {
        Some(v) => v,
        None => return Lane::Standard,
    };
    let triggers_native = v.cached_content.is_some()
        || v.response_schema.is_some()
        || v.media_uris
            .as_ref()
            .is_some_and(|uris| uris.iter().any(|u| u.starts_with("gs://")));
    if triggers_native {
        Lane::NativeVertex
    } else {
        Lane::Standard
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::request::{ChatRequest, VertexExt};

    fn base() -> ChatRequest {
        serde_json::from_value(serde_json::json!({
            "model": "m", "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap()
    }

    #[test]
    fn no_vertex_block_is_standard() {
        assert_eq!(classify(&base()), Lane::Standard);
    }

    #[test]
    fn cached_content_is_native() {
        let req = ChatRequest {
            vertex: Some(VertexExt {
                cached_content: Some("cachedContents/x".into()),
                ..Default::default()
            }),
            ..base()
        };
        assert_eq!(classify(&req), Lane::NativeVertex);
    }

    #[test]
    fn response_schema_is_native() {
        let req = ChatRequest {
            vertex: Some(VertexExt {
                response_schema: Some(serde_json::json!({"type": "object"})),
                ..Default::default()
            }),
            ..base()
        };
        assert_eq!(classify(&req), Lane::NativeVertex);
    }

    #[test]
    fn gs_media_uri_is_native_but_https_is_not() {
        let gs = ChatRequest {
            vertex: Some(VertexExt {
                media_uris: Some(vec!["gs://b/v.mp4".into()]),
                ..Default::default()
            }),
            ..base()
        };
        assert_eq!(classify(&gs), Lane::NativeVertex);
        let https = ChatRequest {
            vertex: Some(VertexExt {
                media_uris: Some(vec!["https://x/v.mp4".into()]),
                ..Default::default()
            }),
            ..base()
        };
        assert_eq!(classify(&https), Lane::Standard);
    }
}
