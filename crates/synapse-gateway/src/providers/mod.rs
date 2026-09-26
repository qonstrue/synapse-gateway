//! Provider catalog: genai clients + circuit breakers, keyed by provider id.
pub mod genai_provider;
pub mod vertex_auth;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::config::vertex_project_from_env;
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

/// Why a provider referenced by a route table cannot be built here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unsatisfiable {
    /// A recognised provider whose credential is absent from the env.
    MissingCredential {
        provider: String,
        required: &'static str,
    },
    /// An id this binary does not recognise — an older build meeting a newer
    /// route table, which is how a shared table breaks a lagging consumer.
    UnknownProvider { provider: String },
}

impl Unsatisfiable {
    pub fn provider(&self) -> &str {
        match self {
            Self::MissingCredential { provider, .. } | Self::UnknownProvider { provider } => {
                provider
            }
        }
    }
}

impl std::fmt::Display for Unsatisfiable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingCredential { provider, required } => {
                write!(f, "provider '{provider}' needs {required}, which is unset")
            }
            Self::UnknownProvider { provider } => {
                write!(f, "provider '{provider}' is not recognised by this build")
            }
        }
    }
}

/// Which of `referenced` this env cannot satisfy. Pure: builds no clients and
/// makes no network calls, so it is safe to call before deciding what to drop.
pub fn unsatisfiable_providers(
    env: &HashMap<String, String>,
    referenced: &std::collections::HashSet<String>,
) -> Vec<Unsatisfiable> {
    let present = |k: &str| env.get(k).is_some_and(|v| !v.trim().is_empty());
    let mut out: Vec<Unsatisfiable> = referenced
        .iter()
        .filter_map(|id| match id.as_str() {
            // Accepts either spelling, so report the preferred one.
            "vertex" => {
                vertex_project_from_env(env)
                    .is_none()
                    .then(|| Unsatisfiable::MissingCredential {
                        provider: id.clone(),
                        required: "VERTEX_PROJECT_ID",
                    })
            }
            "qwen" => (!present("DASHSCOPE_API_KEY")).then(|| Unsatisfiable::MissingCredential {
                provider: id.clone(),
                required: "DASHSCOPE_API_KEY",
            }),
            "openai" => (!present("OPENAI_API_KEY")).then(|| Unsatisfiable::MissingCredential {
                provider: id.clone(),
                required: "OPENAI_API_KEY",
            }),
            "typesafe" => {
                (!present("TYPESAFE_API_KEY")).then(|| Unsatisfiable::MissingCredential {
                    provider: id.clone(),
                    required: "TYPESAFE_API_KEY",
                })
            }
            "oai_compat" => {
                (!present("OAI_COMPAT_BASE_URL")).then(|| Unsatisfiable::MissingCredential {
                    provider: id.clone(),
                    required: "OAI_COMPAT_BASE_URL",
                })
            }
            _ => Some(Unsatisfiable::UnknownProvider {
                provider: id.clone(),
            }),
        })
        .collect();
    out.sort_by(|a, b| a.provider().cmp(b.provider()));
    out
}

impl Catalog {
    pub fn get(&self, id: &str) -> Option<&Arc<Provider>> {
        self.providers.get(id)
    }

    /// Build every provider referenced by `referenced`, validating credentials
    /// fail-fast. Recognised ids: `vertex`, `qwen`, `openai`, `oai_compat`;
    /// `typesafe` is validated but not built (it runs on the native Jev lane).
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
                    let project = vertex_project_from_env(env).ok_or_else(|| {
                        anyhow::anyhow!(
                            "route references provider 'vertex' but VERTEX_PROJECT_ID and VERTEX_PROJECT are unset"
                        )
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
                // TypeSafe System One (Jev) has no genai client: the leg runs
                // through `JevNativeProvider` on the Gateway, like the native
                // Vertex lane. Validate the credential here for fail-fast
                // parity; the leg itself is consumed by the Jev lane before
                // the standard executor ever sees it.
                "typesafe" => {
                    if get("TYPESAFE_API_KEY").is_none() {
                        anyhow::bail!(
                            "route references provider 'typesafe' but TYPESAFE_API_KEY is unset"
                        );
                    }
                    continue;
                }
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

    /// Construct a catalog from pre-built providers (tests and embedders).
    pub fn from_map(providers: HashMap<String, Arc<Provider>>) -> Self {
        Self { providers }
    }

    /// Test-only catalog of OpenAI-compatible providers from (id, base_url) pairs.
    #[cfg(test)]
    pub fn for_test(pairs: Vec<(&'static str, String)>) -> Self {
        let mut providers: HashMap<String, Arc<Provider>> = HashMap::new();
        for (id, base_url) in pairs {
            let p = build_openai_compat_provider(
                id,
                OpenAiCompatConfig {
                    base_url,
                    api_key: "k".into(),
                    request_timeout: Duration::from_secs(5),
                    endpoint_override: None,
                },
            )
            .unwrap();
            providers.insert(id.to_string(), Arc::new(p));
        }
        Self { providers }
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
    fn refs(ids: &[&str]) -> std::collections::HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn unsatisfiable_reports_the_env_var_each_provider_needs() {
        let missing = unsatisfiable_providers(
            &env(&[]),
            &refs(&["vertex", "qwen", "openai", "typesafe", "oai_compat"]),
        );
        let pairs: Vec<(&str, &str)> = missing
            .iter()
            .map(|u| match u {
                Unsatisfiable::MissingCredential { provider, required } => {
                    (provider.as_str(), *required)
                }
                Unsatisfiable::UnknownProvider { provider } => (provider.as_str(), "unknown"),
            })
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("oai_compat", "OAI_COMPAT_BASE_URL"),
                ("openai", "OPENAI_API_KEY"),
                ("qwen", "DASHSCOPE_API_KEY"),
                ("typesafe", "TYPESAFE_API_KEY"),
                ("vertex", "VERTEX_PROJECT_ID"),
            ]
        );
    }

    #[test]
    fn unsatisfiable_is_empty_when_every_credential_is_present() {
        let satisfied = env(&[
            ("VERTEX_PROJECT", "legacy-spelling-counts"),
            ("DASHSCOPE_API_KEY", "k"),
            ("TYPESAFE_API_KEY", "k"),
        ]);
        assert!(
            unsatisfiable_providers(&satisfied, &refs(&["vertex", "qwen", "typesafe"])).is_empty()
        );
    }

    #[test]
    fn unsatisfiable_flags_an_id_this_build_does_not_know() {
        // The failure mode of a shared route table: a newer leg reaching an older binary.
        assert_eq!(
            unsatisfiable_providers(&env(&[]), &refs(&["from-the-future"])),
            vec![Unsatisfiable::UnknownProvider {
                provider: "from-the-future".into()
            }]
        );
    }

    #[test]
    fn whitespace_only_credential_counts_as_unset() {
        let blank = env(&[("TYPESAFE_API_KEY", "   ")]);
        assert_eq!(
            unsatisfiable_providers(&blank, &refs(&["typesafe"])),
            vec![Unsatisfiable::MissingCredential {
                provider: "typesafe".into(),
                required: "TYPESAFE_API_KEY"
            }]
        );
    }

    #[test]
    fn builds_vertex_when_project_id_present() {
        let cat = Catalog::build(
            &env(&[("VERTEX_PROJECT_ID", "my-gcp-project")]),
            &refs(&["vertex"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(cat.get("vertex").is_some());
    }

    #[test]
    fn builds_vertex_when_legacy_project_present() {
        let cat = Catalog::build(
            &env(&[("VERTEX_PROJECT", "my-gcp-project")]),
            &refs(&["vertex"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(cat.get("vertex").is_some());
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
    fn typesafe_without_key_fails_fast_with_named_error() {
        let err =
            Catalog::build(&env(&[]), &refs(&["typesafe"]), Duration::from_secs(5)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("typesafe"), "{msg}");
        assert!(msg.contains("TYPESAFE_API_KEY"), "{msg}");
    }

    #[test]
    fn typesafe_with_key_is_validated_but_not_built() {
        let cat = Catalog::build(
            &env(&[("TYPESAFE_API_KEY", "sk-test")]),
            &refs(&["typesafe"]),
            Duration::from_secs(5),
        )
        .unwrap();
        // No genai client: the leg is served by JevNativeProvider on the Gateway.
        assert!(cat.get("typesafe").is_none());
    }

    #[test]
    fn unknown_provider_id_errors() {
        let err = Catalog::build(&env(&[]), &refs(&["bogus"]), Duration::from_secs(5)).unwrap_err();
        assert!(err.to_string().contains("unknown provider id"));
    }
}
