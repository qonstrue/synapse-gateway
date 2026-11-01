//! Gateway error type → OpenAI-shaped JSON + HTTP status.

#[cfg(feature = "server")]
use axum::http::StatusCode;
#[cfg(feature = "server")]
use axum::response::{IntoResponse, Response};
#[cfg(feature = "server")]
use axum::Json;
#[cfg(feature = "server")]
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("unknown model alias '{0}'")]
    UnknownModel(String),
    #[error("native feature '{feature}' is not available on route '{route}'")]
    NativeFeatureUnsupported { feature: String, route: String },
    #[error("invalid request: {0}")]
    BadRequest(String),
    #[error("all legs of route '{route}' failed")]
    AllLegsFailed {
        route: String,
        failures: Vec<LegFailure>,
    },
    #[error("all providers for route '{0}' are unavailable")]
    AllCircuitsOpen(String),
    #[error("upstream timed out")]
    UpstreamTimeout,
    #[error("upstream error {status}: {body}")]
    Upstream { status: u16, body: String },
    #[error("request blocked by content policy '{policy}'")]
    ContentBlocked {
        policy: String,
        scanners: Vec<String>,
        /// Pattern ids of the blocking matches; never the matched text.
        patterns: Vec<String>,
    },
    #[error("guard classifier for policy '{policy}' is unavailable")]
    GuardUnavailable { policy: String },
    #[error("session belongs to harness '{expected}', cannot switch to '{requested}'")]
    HarnessMismatch { expected: String, requested: String },
    #[error("session '{0}' already has an in-flight turn")]
    SessionBusy(String),
    #[error("model unavailable: {0}")]
    ModelUnavailable(String),
    #[error("harness unavailable: {0}")]
    HarnessUnavailable(String),
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LegFailure {
    pub provider: String,
    pub model: String,
    pub message: String,
}

impl GatewayError {
    #[cfg(feature = "server")]
    pub fn status(&self) -> StatusCode {
        match self {
            GatewayError::UnknownModel(_) => StatusCode::NOT_FOUND,
            GatewayError::NativeFeatureUnsupported { .. } | GatewayError::BadRequest(_) => {
                StatusCode::BAD_REQUEST
            }
            GatewayError::AllLegsFailed { .. } | GatewayError::Upstream { .. } => {
                StatusCode::BAD_GATEWAY
            }
            GatewayError::AllCircuitsOpen(_) => StatusCode::SERVICE_UNAVAILABLE,
            GatewayError::UpstreamTimeout => StatusCode::GATEWAY_TIMEOUT,
            GatewayError::ContentBlocked { .. } => StatusCode::BAD_REQUEST,
            GatewayError::GuardUnavailable { .. } | GatewayError::HarnessUnavailable(_) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            GatewayError::HarnessMismatch { .. } | GatewayError::SessionBusy(_) => {
                StatusCode::CONFLICT
            }
            GatewayError::ModelUnavailable(_) => StatusCode::UNPROCESSABLE_ENTITY,
        }
    }

    #[cfg(feature = "server")]
    pub(crate) fn code(&self) -> &'static str {
        match self {
            GatewayError::UnknownModel(_) => "model_not_found",
            GatewayError::NativeFeatureUnsupported { .. } => "native_feature_unsupported",
            GatewayError::BadRequest(_) => "invalid_request_error",
            GatewayError::AllLegsFailed { .. } => "all_legs_failed",
            GatewayError::AllCircuitsOpen(_) => "circuit_open",
            GatewayError::UpstreamTimeout => "upstream_timeout",
            GatewayError::Upstream { .. } => "upstream_error",
            GatewayError::ContentBlocked { .. } => "content_blocked",
            GatewayError::GuardUnavailable { .. } => "guard_unavailable",
            GatewayError::HarnessMismatch { .. } => "harness_mismatch",
            GatewayError::SessionBusy(_) => "session_busy",
            GatewayError::ModelUnavailable(_) => "model_unavailable",
            GatewayError::HarnessUnavailable(_) => "harness_unavailable",
        }
    }
}

#[cfg(feature = "server")]
impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let mut error = json!({
            "type": self.code(),
            "message": self.to_string(),
            "code": self.code(),
        });
        let legs_attempted = if let GatewayError::AllLegsFailed { failures, .. } = &self {
            error["failures"] = json!(failures);
            error["legs_attempted"] = json!(failures.len());
            Some(failures.len())
        } else {
            None
        };
        if let GatewayError::ContentBlocked {
            policy,
            scanners,
            patterns,
        } = &self
        {
            error["type"] = json!("content_policy_violation");
            error["message"] = json!(format!(
                "Request blocked by content policy '{policy}' (scanners: {})",
                scanners.join(", ")
            ));
            error["scanners"] = json!(scanners);
            error["patterns"] = json!(patterns);
        }
        let mut resp = (self.status(), Json(json!({ "error": error }))).into_response();
        if let Some(count) = legs_attempted {
            if let Ok(val) = axum::http::HeaderValue::from_str(&count.to_string()) {
                resp.headers_mut().insert("x-synapse-legs-attempted", val);
            }
            resp.headers_mut().insert(
                "x-synapse-routing-degraded",
                axum::http::HeaderValue::from_static("all_legs_failed"),
            );
        }
        resp
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn content_blocked_is_bad_request_with_named_scanners() {
        let e = GatewayError::ContentBlocked {
            policy: "strict".into(),
            scanners: vec!["secrets".into(), "role_override".into()],
            patterns: vec!["aws_access_key".into()],
        };
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
        assert_eq!(e.code(), "content_blocked");
        let msg = e.to_string();
        assert!(msg.contains("strict"), "message names the policy: {msg}");
    }

    #[tokio::test]
    async fn content_blocked_body_names_the_patterns() {
        use http_body_util::BodyExt;
        let e = GatewayError::ContentBlocked {
            policy: "strict".into(),
            scanners: vec!["secrets".into()],
            patterns: vec!["aws_access_key".into()],
        };
        let bytes = e
            .into_response()
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["patterns"], json!(["aws_access_key"]));
    }

    #[tokio::test]
    async fn all_legs_failed_stamps_attempt_headers_and_body_count() {
        use http_body_util::BodyExt;
        let e = GatewayError::AllLegsFailed {
            route: "tiered".into(),
            failures: vec![
                LegFailure {
                    provider: "vertex".into(),
                    model: "gemini-3-flash".into(),
                    message: "503".into(),
                },
                LegFailure {
                    provider: "vertex".into(),
                    model: "gemini-3-pro".into(),
                    message: "503".into(),
                },
            ],
        };
        let resp = e.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(resp.headers()["x-synapse-legs-attempted"], "2");
        assert_eq!(
            resp.headers()["x-synapse-routing-degraded"],
            "all_legs_failed"
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "all_legs_failed");
        assert_eq!(body["error"]["legs_attempted"], 2);
    }

    #[test]
    fn uhp_error_variants_map_to_spec_status_codes_and_error_codes() {
        assert_eq!(
            GatewayError::HarnessMismatch {
                expected: "chrn_a".into(),
                requested: "chrn_b".into()
            }
            .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            GatewayError::HarnessMismatch {
                expected: "chrn_a".into(),
                requested: "chrn_b".into()
            }
            .code(),
            "harness_mismatch"
        );
        assert_eq!(
            GatewayError::SessionBusy("hsess_1".into()).status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            GatewayError::ModelUnavailable("no compatible cell".into()).status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            GatewayError::HarnessUnavailable("all bases failed".into()).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
