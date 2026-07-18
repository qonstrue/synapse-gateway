//! Compile parsed config + a transform registry into ready-to-serve routes.
//! The binary uses built-ins only; library consumers register custom transforms
//! by name before `build()`.

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::{Config, RequestStep, ResponseStep, Route};
use crate::context::ContextStore;
use crate::transform::error_remap::ErrorRemap;
use crate::transform::inject::Inject;
use crate::transform::wrap::Wrap;
use crate::transform::{RequestTransform, ResponseTransform, TransformRegistry};

pub struct CompiledRoute {
    pub name: String,
    pub path_prefix: String,
    pub upstream: String,
    pub strip_prefix: bool,
    pub methods: Vec<String>, // upper-case; empty = any
    pub require_context: Vec<String>,
    pub request: Vec<Arc<dyn RequestTransform>>,
    pub response: Vec<Arc<dyn ResponseTransform>>,
}

impl std::fmt::Debug for CompiledRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledRoute")
            .field("name", &self.name)
            .field("path_prefix", &self.path_prefix)
            .field("upstream", &self.upstream)
            .field("strip_prefix", &self.strip_prefix)
            .field("methods", &self.methods)
            .field("require_context", &self.require_context)
            .field("request_len", &self.request.len())
            .field("response_len", &self.response.len())
            .finish()
    }
}

pub struct BuiltProxy {
    pub routes: Vec<CompiledRoute>,
    pub context: Arc<ContextStore>,
    pub admin_addr: String,
    pub metrics_addr: String,
    pub addr: String,
}

impl std::fmt::Debug for BuiltProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltProxy")
            .field("routes", &self.routes)
            .field("admin_addr", &self.admin_addr)
            .field("metrics_addr", &self.metrics_addr)
            .field("addr", &self.addr)
            .finish()
    }
}

pub struct ProxyBuilder {
    config: Config,
    registry: TransformRegistry,
}

impl ProxyBuilder {
    pub fn from_config(config: Config) -> Self {
        Self {
            config,
            registry: TransformRegistry::default(),
        }
    }
    pub fn request_transform(
        mut self,
        name: impl Into<String>,
        t: Arc<dyn RequestTransform>,
    ) -> Self {
        self.registry.register_request(name, t);
        self
    }
    pub fn response_transform(
        mut self,
        name: impl Into<String>,
        t: Arc<dyn ResponseTransform>,
    ) -> Self {
        self.registry.register_response(name, t);
        self
    }

    /// Build the context store (static ⊕ env, env wins) and compile every route.
    pub fn build(self) -> anyhow::Result<BuiltProxy> {
        let mut base: HashMap<String, String> = self.config.context.static_values.clone();
        for (key, var) in &self.config.context.env {
            if let Ok(val) = std::env::var(var) {
                if !val.trim().is_empty() {
                    base.insert(key.clone(), val); // env precedence over static
                }
            }
        }
        let context = Arc::new(ContextStore::new(base));

        let routes = self
            .config
            .routes
            .iter()
            .map(|r| compile_route(r, &self.registry))
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(BuiltProxy {
            routes,
            context,
            admin_addr: self.config.admin_addr.clone(),
            metrics_addr: self.config.metrics_addr.clone(),
            addr: self.config.addr.clone(),
        })
    }
}

fn compile_route(r: &Route, reg: &TransformRegistry) -> anyhow::Result<CompiledRoute> {
    let mut request: Vec<Arc<dyn RequestTransform>> = Vec::new();
    // cycle-1 sugar: static headers become inject steps first.
    for (name, value) in &r.headers {
        request.push(Arc::new(Inject::from_spec(&crate::config::InjectSpec {
            header: Some(name.clone()),
            body: None,
            from_context: None,
            constant: Some(serde_json::Value::String(value.clone())),
        })?));
    }
    for step in &r.request_steps {
        request.push(match step {
            RequestStep::Inject(s) => Arc::new(Inject::from_spec(s)?),
            RequestStep::Wrap(s) => Arc::new(Wrap::from_spec(s)?),
            RequestStep::Transform(name) => reg.request.get(name).cloned().ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown request transform '{name}' on route '{}'",
                    r.label()
                )
            })?,
        });
    }
    let mut response: Vec<Arc<dyn ResponseTransform>> = Vec::new();
    for step in &r.response_steps {
        response.push(match step {
            ResponseStep::ErrorRemap(s) => Arc::new(ErrorRemap::from_spec(s)?),
            ResponseStep::Transform(name) => reg.response.get(name).cloned().ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown response transform '{name}' on route '{}'",
                    r.label()
                )
            })?,
        });
    }
    Ok(CompiledRoute {
        name: r.label().to_string(),
        path_prefix: r.path_prefix.clone(),
        upstream: r.upstream.clone(),
        strip_prefix: r.strip_prefix,
        methods: r.methods.iter().map(|m| m.to_ascii_uppercase()).collect(),
        require_context: r.require_context.clone(),
        request,
        response,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_builtins_and_sugar() {
        let cfg = Config::from_toml_str(
            r#"
            [[routes]]
            name = "r"
            path_prefix = "/x"
            upstream = "http://u"
            headers = { x-static = "v" }
            request_steps = [ { inject = { header = "X-Org", from_context = "org" } } ]
            response_steps = [ { error_remap = { when_status = 401, error = "auth_expired" } } ]
        "#,
        )
        .unwrap();
        let built = ProxyBuilder::from_config(cfg).build().unwrap();
        let route = &built.routes[0];
        assert_eq!(route.request.len(), 2); // static header sugar + inject step
        assert_eq!(route.response.len(), 1);
    }

    #[test]
    fn unknown_named_transform_fails_fast() {
        let cfg = Config::from_toml_str(
            r#"
            [[routes]]
            path_prefix = "/x"
            upstream = "http://u"
            request_steps = [ { transform = "nope" } ]
        "#,
        )
        .unwrap();
        let err = ProxyBuilder::from_config(cfg).build().unwrap_err();
        assert!(err.to_string().contains("unknown request transform 'nope'"));
    }
}
