//! Gateway error type → OpenAI-shaped JSON + HTTP status.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
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
    AllLegsFailed { route: String, failures: Vec<LegFailure> },
    #[error("all providers for route '{0}' are unavailable")]
    AllCircuitsOpen(String),
    #[error("upstream timed out")]
    UpstreamTimeout,
    #[error("upstream error {status}: {body}")]
    Upstream { status: u16, body: String },
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LegFailure {
    pub provider: String,
    pub model: String,
    pub message: String,
}

impl GatewayError {
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
        }
    }

    fn code(&self) -> &'static str {
        match self {
            GatewayError::UnknownModel(_) => "model_not_found",
            GatewayError::NativeFeatureUnsupported { .. } => "native_feature_unsupported",
            GatewayError::BadRequest(_) => "invalid_request_error",
            GatewayError::AllLegsFailed { .. } => "all_legs_failed",
            GatewayError::AllCircuitsOpen(_) => "circuit_open",
            GatewayError::UpstreamTimeout => "upstream_timeout",
            GatewayError::Upstream { .. } => "upstream_error",
        }
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let mut error = json!({
            "type": self.code(),
            "message": self.to_string(),
            "code": self.code(),
        });
        if let GatewayError::AllLegsFailed { failures, .. } = &self {
            error["failures"] = json!(failures);
        }
        (self.status(), Json(json!({ "error": error }))).into_response()
    }
}
