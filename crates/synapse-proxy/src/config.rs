//! Proxy config: listeners, context sources, and routes with transform steps.

use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_addr")]
    pub addr: String,
    #[serde(default = "default_admin_addr")]
    pub admin_addr: String,
    #[serde(default = "default_metrics_addr")]
    pub metrics_addr: String,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub routes: Vec<Route>,
}

fn default_addr() -> String {
    "0.0.0.0:8787".into()
}
fn default_admin_addr() -> String {
    "127.0.0.1:8788".into()
}
fn default_metrics_addr() -> String {
    "0.0.0.0:9090".into()
}

/// Context sources. `static_values` are TOML literals; `env` maps a context key
/// to the env var it is read from at startup.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContextConfig {
    #[serde(default, rename = "static")]
    pub static_values: HashMap<String, String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Route {
    #[serde(default)]
    pub name: Option<String>,
    pub path_prefix: String,
    pub upstream: String,
    #[serde(default)]
    pub strip_prefix: bool,
    #[serde(default)]
    pub methods: Vec<String>,
    /// Cycle-1 sugar: static headers injected on every forwarded request.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub require_context: Vec<String>,
    #[serde(default)]
    pub request_steps: Vec<RequestStep>,
    #[serde(default)]
    pub response_steps: Vec<ResponseStep>,
}

impl Route {
    /// Metrics/label identifier: explicit `name`, else `path_prefix`.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.path_prefix)
    }
}

/// A request-pipeline step: a built-in (externally tagged by key) or a named
/// custom transform.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStep {
    Inject(InjectSpec),
    Wrap(WrapSpec),
    Transform(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStep {
    ErrorRemap(ErrorRemapSpec),
    Transform(String),
}

/// Inject a context value or constant into a header or a (dotted) body path.
/// Exactly one of `header`/`body` and one of `from_context`/`const` must be set;
/// validated when the transform is built (Task 4).
#[derive(Debug, Clone, Deserialize)]
pub struct InjectSpec {
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub from_context: Option<String>,
    #[serde(default, rename = "const")]
    pub constant: Option<Value>,
}

/// Nest the incoming JSON body under `under` and inject sibling body fields.
#[derive(Debug, Clone, Deserialize)]
pub struct WrapSpec {
    pub under: String,
    #[serde(default)]
    pub inject: Vec<InjectSpec>,
}

/// Map an upstream status to a normalized `{error, detail}` body (status kept).
#[derive(Debug, Clone, Deserialize)]
pub struct ErrorRemapSpec {
    pub when_status: u16,
    pub error: String,
    #[serde(default)]
    pub detail: Option<String>,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let path = std::env::var("SYNAPSE_PROXY_CONFIG_PATH")
            .unwrap_or_else(|_| "synapse-proxy.toml".to_string());
        let content =
            std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?;
        let mut config = Self::from_toml_str(&content)?;
        if let Ok(addr) = std::env::var("SYNAPSE_PROXY_ADDR") {
            if !addr.trim().is_empty() {
                config.addr = addr;
            }
        }
        Ok(config)
    }
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        toml::from_str(s).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        addr = "0.0.0.0:8787"
        admin_addr = "127.0.0.1:8788"
        metrics_addr = "0.0.0.0:9090"
        [context]
        static = { tenant = "acme" }
        env = { org = "BROKER_ORG_ID" }
        [[routes]]
        name = "cortex"
        path_prefix = "/v1/cortex"
        upstream = "http://cortex:8080"
        strip_prefix = true
        methods = ["POST"]
        require_context = ["org"]
        request_steps = [
          { inject = { header = "X-Tenant-Id", from_context = "org" } },
          { inject = { header = "X-User-Id", const = "_default" } },
        ]
        [[routes]]
        name = "call"
        path_prefix = "/v1/call"
        upstream = "http://up:8080"
        request_steps = [ { wrap = { under = "request", inject = [ { body = "org", from_context = "org" } ] } } ]
        response_steps = [ { error_remap = { when_status = 401, error = "auth_expired" } } ]
    "#;

    #[test]
    fn parses_listeners_context_and_steps() {
        let c = Config::from_toml_str(SAMPLE).unwrap();
        assert_eq!(c.admin_addr, "127.0.0.1:8788");
        assert_eq!(c.metrics_addr, "0.0.0.0:9090");
        assert_eq!(
            c.context.static_values.get("tenant").map(String::as_str),
            Some("acme")
        );
        assert_eq!(
            c.context.env.get("org").map(String::as_str),
            Some("BROKER_ORG_ID")
        );
        let cortex = &c.routes[0];
        assert_eq!(cortex.label(), "cortex");
        assert_eq!(cortex.methods, vec!["POST"]);
        assert_eq!(cortex.require_context, vec!["org"]);
        assert_eq!(cortex.request_steps.len(), 2);
        assert!(matches!(cortex.request_steps[0], RequestStep::Inject(_)));
        let call = &c.routes[1];
        assert!(matches!(call.request_steps[0], RequestStep::Wrap(_)));
        assert!(matches!(
            call.response_steps[0],
            ResponseStep::ErrorRemap(_)
        ));
    }

    #[test]
    fn label_falls_back_to_path_prefix() {
        let c = Config::from_toml_str(
            r#"[[routes]]
            path_prefix = "/x"
            upstream = "http://u""#,
        )
        .unwrap();
        assert_eq!(c.routes[0].label(), "/x");
    }
}
