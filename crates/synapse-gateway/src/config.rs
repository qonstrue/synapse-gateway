//! Layered runtime config (env + file paths). Env takes precedence.

use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub addr: String,
    pub metrics_addr: String,
    pub routes_path: String,
    pub pricing_path: String,
    pub guardrails_path: String,
    pub a2a_path: String,
    pub ai_task_types_path: String,
    pub ledger_backends: Vec<LedgerBackend>,
    pub default_tenant: String,
    pub request_timeout: Duration,
    pub stream_idle_timeout: Duration,
    pub embed_default_input_per_mtok: f64,
    /// Provider credentials/base-urls, read straight from the env map.
    pub env: HashMap<String, String>,
    /// How to treat a route whose provider this process cannot satisfy.
    pub provider_validation: ProviderValidation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerBackend {
    Sqlite,
    Postgres,
    Pubsub,
    Sns,
}

impl LedgerBackend {
    pub fn label(self) -> &'static str {
        match self {
            LedgerBackend::Sqlite => "sqlite",
            LedgerBackend::Postgres => "postgres",
            LedgerBackend::Pubsub => "pubsub",
            LedgerBackend::Sns => "sns",
        }
    }
}

fn parse_ledger_backends(list: &str) -> anyhow::Result<Vec<LedgerBackend>> {
    let mut out: Vec<LedgerBackend> = Vec::new();
    for raw in list.split(',') {
        let name = raw.trim().to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        let b = match name.as_str() {
            "sqlite" => LedgerBackend::Sqlite,
            "postgres" => LedgerBackend::Postgres,
            "pubsub" => LedgerBackend::Pubsub,
            "sns" => LedgerBackend::Sns,
            other => anyhow::bail!("unknown ledger backend '{other}' (sqlite|postgres|pubsub|sns)"),
        };
        if out.contains(&b) {
            anyhow::bail!("duplicate ledger backend '{name}' in SYNAPSE_LEDGER_BACKENDS");
        }
        out.push(b);
    }
    if out.is_empty() {
        anyhow::bail!("SYNAPSE_LEDGER_BACKENDS resolved to an empty backend list");
    }
    Ok(out)
}

/// What to do when a route table references a provider this process cannot build —
/// a credential absent from the env, or an id this binary does not recognise.
///
/// `Strict` (the default) refuses to start. That is the right default for a
/// dedicated gateway: a route table it cannot fully serve is a deploy mistake,
/// and failing at boot surfaces it immediately.
///
/// `Lenient` drops the legs it cannot serve, keeps the rest of each route, and
/// starts. It exists because one route table is commonly shared by several
/// processes — a gateway plus in-process consumers — and a leg added for one of
/// them should not take the others down. A route left with no legs disappears
/// from the table, so calling it is a 404 rather than a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProviderValidation {
    #[default]
    Strict,
    Lenient,
}

impl ProviderValidation {
    /// `SYNAPSE_PROVIDER_VALIDATION=lenient` opts in; anything else is strict.
    pub fn from_env(env: &HashMap<String, String>) -> Self {
        match env
            .get("SYNAPSE_PROVIDER_VALIDATION")
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("lenient") => Self::Lenient,
            _ => Self::Strict,
        }
    }
}

/// Resolve the GCP project id for Vertex from an env map.
/// `VERTEX_PROJECT_ID` is preferred; `VERTEX_PROJECT` is accepted for compatibility.
pub fn vertex_project_from_env(env: &HashMap<String, String>) -> Option<String> {
    ["VERTEX_PROJECT_ID", "VERTEX_PROJECT"]
        .into_iter()
        .find_map(|key| {
            env.get(key)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
}

impl Config {
    pub fn from_env_map(env: &HashMap<String, String>) -> anyhow::Result<Self> {
        let get = |k: &str| env.get(k).cloned().filter(|s| !s.trim().is_empty());
        let get_or = |k: &str, d: &str| get(k).unwrap_or_else(|| d.to_string());
        let backends_raw = get("SYNAPSE_LEDGER_BACKENDS")
            .or_else(|| get("SYNAPSE_LEDGER_BACKEND"))
            .unwrap_or_else(|| "sqlite".to_string());
        let ledger_backends = parse_ledger_backends(&backends_raw)?;
        Ok(Self {
            addr: get_or("SYNAPSE_ADDR", "0.0.0.0:8080"),
            metrics_addr: get_or("SYNAPSE_METRICS_ADDR", "0.0.0.0:9090"),
            routes_path: get_or("SYNAPSE_ROUTES_PATH", "config/routes.toml"),
            pricing_path: get_or("SYNAPSE_PRICING_PATH", "config/pricing.toml"),
            guardrails_path: get_or("SYNAPSE_GUARDRAILS_PATH", "config/guardrails.toml"),
            a2a_path: get_or("SYNAPSE_A2A_PATH", "config/a2a.toml"),
            ai_task_types_path: get_or("SYNAPSE_AI_TASK_TYPES_PATH", "config/ai_task_types.toml"),
            ledger_backends,
            default_tenant: get_or("SYNAPSE_DEFAULT_TENANT", "unattributed"),
            request_timeout: Duration::from_secs(
                get_or("SYNAPSE_REQUEST_TIMEOUT_SECS", "120")
                    .parse()
                    .map_err(|e| anyhow::anyhow!("SYNAPSE_REQUEST_TIMEOUT_SECS: {e}"))?,
            ),
            stream_idle_timeout: Duration::from_secs(
                get_or("SYNAPSE_STREAM_IDLE_TIMEOUT_SECS", "60")
                    .parse()
                    .map_err(|e| anyhow::anyhow!("SYNAPSE_STREAM_IDLE_TIMEOUT_SECS: {e}"))?,
            ),
            embed_default_input_per_mtok: get_or(
                "SYNAPSE_EMBED_DEFAULT_INPUT_PRICE_PER_MTOK",
                "0.10",
            )
            .parse()
            .unwrap_or(0.10),
            env: env.clone(),
            provider_validation: ProviderValidation::from_env(env),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_validation_defaults_to_strict_and_opts_in_on_lenient() {
        let cases = [
            (vec![], ProviderValidation::Strict),
            (
                vec![("SYNAPSE_PROVIDER_VALIDATION", "lenient")],
                ProviderValidation::Lenient,
            ),
            (
                vec![("SYNAPSE_PROVIDER_VALIDATION", "  LENIENT  ")],
                ProviderValidation::Lenient,
            ),
            (
                vec![("SYNAPSE_PROVIDER_VALIDATION", "strict")],
                ProviderValidation::Strict,
            ),
            // An unrecognised value must not silently loosen validation.
            (
                vec![("SYNAPSE_PROVIDER_VALIDATION", "yes")],
                ProviderValidation::Strict,
            ),
        ];
        for (pairs, want) in cases {
            let e = env(&pairs);
            assert_eq!(ProviderValidation::from_env(&e), want, "for {pairs:?}");
            assert_eq!(Config::from_env_map(&e).unwrap().provider_validation, want);
        }
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn vertex_project_from_env_prefers_project_id() {
        let env = env(&[
            ("VERTEX_PROJECT_ID", "from-id"),
            ("VERTEX_PROJECT", "from-legacy"),
        ]);
        assert_eq!(vertex_project_from_env(&env).as_deref(), Some("from-id"));
    }

    #[test]
    fn vertex_project_from_env_falls_back_to_vertex_project() {
        let env = env(&[("VERTEX_PROJECT", "legacy-only")]);
        assert_eq!(
            vertex_project_from_env(&env).as_deref(),
            Some("legacy-only")
        );
    }

    #[test]
    fn vertex_project_from_env_ignores_blank_values() {
        let env = env(&[("VERTEX_PROJECT_ID", "  "), ("VERTEX_PROJECT", "ok")]);
        assert_eq!(vertex_project_from_env(&env).as_deref(), Some("ok"));
    }

    #[test]
    fn defaults_apply_when_env_empty() {
        let c = Config::from_env_map(&env(&[])).unwrap();
        assert_eq!(c.addr, "0.0.0.0:8080");
        assert_eq!(c.ledger_backends, vec![LedgerBackend::Sqlite]);
        assert_eq!(c.default_tenant, "unattributed");
    }

    #[test]
    fn env_overrides_and_validates_backend() {
        let c = Config::from_env_map(&env(&[("SYNAPSE_LEDGER_BACKEND", "postgres")])).unwrap();
        assert_eq!(c.ledger_backends, vec![LedgerBackend::Postgres]);
        let err = Config::from_env_map(&env(&[("SYNAPSE_LEDGER_BACKEND", "mysql")])).unwrap_err();
        assert!(err.to_string().contains("sqlite|postgres"));
    }

    #[test]
    fn parses_stream_timeouts() {
        let c = Config::from_env_map(&env(&[
            ("SYNAPSE_REQUEST_TIMEOUT_SECS", "30"),
            ("SYNAPSE_STREAM_IDLE_TIMEOUT_SECS", "45"),
        ]))
        .unwrap();
        assert_eq!(c.request_timeout, std::time::Duration::from_secs(30));
        assert_eq!(c.stream_idle_timeout, std::time::Duration::from_secs(45));
    }

    #[test]
    fn parses_backend_list() {
        let c =
            Config::from_env_map(&env(&[("SYNAPSE_LEDGER_BACKENDS", "postgres, pubsub")])).unwrap();
        assert_eq!(
            c.ledger_backends,
            vec![LedgerBackend::Postgres, LedgerBackend::Pubsub]
        );
    }

    #[test]
    fn singular_backend_is_back_compat() {
        let c = Config::from_env_map(&env(&[("SYNAPSE_LEDGER_BACKEND", "sns")])).unwrap();
        assert_eq!(c.ledger_backends, vec![LedgerBackend::Sns]);
    }

    #[test]
    fn defaults_to_sqlite_list() {
        let c = Config::from_env_map(&env(&[])).unwrap();
        assert_eq!(c.ledger_backends, vec![LedgerBackend::Sqlite]);
    }

    #[test]
    fn rejects_unknown_and_duplicate_backends() {
        assert!(Config::from_env_map(&env(&[("SYNAPSE_LEDGER_BACKENDS", "mysql")])).is_err());
        assert!(
            Config::from_env_map(&env(&[("SYNAPSE_LEDGER_BACKENDS", "pubsub,pubsub")])).is_err()
        );
    }

    #[test]
    fn guardrails_path_defaults_and_overrides() {
        let c = Config::from_env_map(&env(&[])).unwrap();
        assert_eq!(c.guardrails_path, "config/guardrails.toml");
        let c = Config::from_env_map(&env(&[("SYNAPSE_GUARDRAILS_PATH", "/etc/g.toml")])).unwrap();
        assert_eq!(c.guardrails_path, "/etc/g.toml");
    }

    #[test]
    fn a2a_path_defaults_and_overrides() {
        let c = Config::from_env_map(&env(&[])).unwrap();
        assert_eq!(c.a2a_path, "config/a2a.toml");
        let c = Config::from_env_map(&env(&[("SYNAPSE_A2A_PATH", "/etc/a2a.toml")])).unwrap();
        assert_eq!(c.a2a_path, "/etc/a2a.toml");
    }

    #[test]
    fn ai_task_types_path_defaults_and_overrides() {
        let c = Config::from_env_map(&env(&[])).unwrap();
        assert_eq!(c.ai_task_types_path, "config/ai_task_types.toml");
        let c =
            Config::from_env_map(&env(&[("SYNAPSE_AI_TASK_TYPES_PATH", "/etc/ai.toml")])).unwrap();
        assert_eq!(c.ai_task_types_path, "/etc/ai.toml");
    }

    #[test]
    fn shipped_ai_task_types_sample_parses_and_covers_known_aliases() {
        let content = std::fs::read_to_string("config/ai_task_types.toml")
            .expect("config/ai_task_types.toml should exist");
        let table = crate::ai_task_type::AiTaskTypeTable::from_toml_str(&content).unwrap();
        assert_eq!(table.resolve("conversation"), "conversation");
        // An alias absent from the shipped taxonomy still resolves, to the default.
        assert_eq!(table.resolve("no-such-alias"), "simple");
    }

    #[test]
    fn shipped_guardrails_sample_builds_an_engine() {
        let content = std::fs::read_to_string("config/guardrails.toml")
            .expect("config/guardrails.toml should exist");
        let cfg = crate::guard::GuardrailsConfig::from_toml_str(&content).unwrap();
        crate::guard::GuardEngine::from_config(&cfg).expect("sample policies must compile");
    }
}
