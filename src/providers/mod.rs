//! Provider catalog: genai clients + circuit breakers, keyed by provider id.
pub mod genai_provider;
pub mod vertex_auth;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::providers::genai_provider::{
    build_openai_compat_provider, build_vertex_provider, OpenAiCompatConfig, Provider,
    VertexProviderConfig,
};
use crate::providers::vertex_auth::VertexAuth;

/// Built provider clients keyed by provider id.
#[derive(Debug)]
pub struct Catalog {
    providers: HashMap<String, Arc<Provider>>,
}

impl Catalog {
    pub fn get(&self, id: &str) -> Option<&Arc<Provider>> {
        self.providers.get(id)
    }

    /// Build every provider referenced by `referenced`, validating credentials
    /// fail-fast. Recognised ids: `vertex`, `qwen`, `openai`, `oai_compat`.
    pub fn build(
        env: &HashMap<String, String>,
        referenced: &std::collections::HashSet<String>,
        request_timeout: Duration,
    ) -> anyhow::Result<Self> {
        let get = |k: &str| env.get(k).cloned().filter(|s| !s.trim().is_empty());
        let mut providers: HashMap<String, Arc<Provider>> = HashMap::new();

        for id in referenced {
            let provider = match id.as_str() {
                "vertex" => {
                    let project = get("VERTEX_PROJECT").ok_or_else(|| {
                        anyhow::anyhow!("route references provider 'vertex' but VERTEX_PROJECT is unset")
                    })?;
                    build_vertex_provider(
                        "vertex",
                        VertexProviderConfig {
                            project,
                            region: "global".into(),
                            request_timeout,
                            endpoint_override: None,
                        },
                        Arc::new(VertexAuth::from_adc()),
                    )?
                }
                "qwen" => build_openai_compat_provider(
                    "qwen",
                    OpenAiCompatConfig {
                        base_url: get("DASHSCOPE_BASE_URL").unwrap_or_else(|| {
                            "https://dashscope-intl.aliyuncs.com/compatible-mode/v1".into()
                        }),
                        api_key: get("DASHSCOPE_API_KEY").ok_or_else(|| {
                            anyhow::anyhow!("route references provider 'qwen' but DASHSCOPE_API_KEY is unset")
                        })?,
                        request_timeout,
                        endpoint_override: None,
                    },
                )?,
                "openai" => build_openai_compat_provider(
                    "openai",
                    OpenAiCompatConfig {
                        base_url: get("OPENAI_BASE_URL").unwrap_or_else(|| "https://api.openai.com/v1".into()),
                        api_key: get("OPENAI_API_KEY").ok_or_else(|| {
                            anyhow::anyhow!("route references provider 'openai' but OPENAI_API_KEY is unset")
                        })?,
                        request_timeout,
                        endpoint_override: None,
                    },
                )?,
                "oai_compat" => build_openai_compat_provider(
                    "oai_compat",
                    OpenAiCompatConfig {
                        base_url: get("OAI_COMPAT_BASE_URL").ok_or_else(|| {
                            anyhow::anyhow!("route references provider 'oai_compat' but OAI_COMPAT_BASE_URL is unset")
                        })?,
                        api_key: get("OAI_COMPAT_API_KEY").unwrap_or_else(|| "not-needed".into()),
                        request_timeout,
                        endpoint_override: None,
                    },
                )?,
                other => anyhow::bail!("unknown provider id in route table: '{other}'"),
            };
            providers.insert(id.clone(), Arc::new(provider));
        }
        Ok(Self { providers })
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }
    fn refs(ids: &[&str]) -> std::collections::HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn missing_dashscope_key_fails_fast_with_named_error() {
        let err = Catalog::build(&env(&[]), &refs(&["qwen"]), Duration::from_secs(5)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("qwen"), "{msg}");
        assert!(msg.contains("DASHSCOPE_API_KEY"), "{msg}");
    }

    #[test]
    fn builds_qwen_when_key_present() {
        let cat = Catalog::build(
            &env(&[("DASHSCOPE_API_KEY", "sk-test")]),
            &refs(&["qwen"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(cat.get("qwen").is_some());
        assert!(cat.get("vertex").is_none());
    }

    #[test]
    fn unknown_provider_id_errors() {
        let err = Catalog::build(&env(&[]), &refs(&["bogus"]), Duration::from_secs(5)).unwrap_err();
        assert!(err.to_string().contains("unknown provider id"));
    }
}
