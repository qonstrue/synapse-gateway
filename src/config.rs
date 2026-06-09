//! Layered runtime config (env + file paths). Env takes precedence.

use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub addr: String,
    pub metrics_addr: String,
    pub routes_path: String,
    pub pricing_path: String,
    pub ledger_backend: LedgerBackend,
    pub ledger_dsn: Option<String>,
    pub default_tenant: String,
    pub request_timeout: Duration,
    /// Provider credentials/base-urls, read straight from the env map.
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerBackend {
    Sqlite,
    Postgres,
}

impl Config {
    pub fn from_env_map(env: &HashMap<String, String>) -> anyhow::Result<Self> {
        let get = |k: &str| env.get(k).cloned().filter(|s| !s.trim().is_empty());
        let get_or = |k: &str, d: &str| get(k).unwrap_or_else(|| d.to_string());
        let backend = match get_or("SYNAPSE_LEDGER_BACKEND", "sqlite").as_str() {
            "sqlite" => LedgerBackend::Sqlite,
            "postgres" => LedgerBackend::Postgres,
            other => anyhow::bail!("SYNAPSE_LEDGER_BACKEND must be sqlite|postgres, got '{other}'"),
        };
        Ok(Self {
            addr: get_or("SYNAPSE_ADDR", "0.0.0.0:8080"),
            metrics_addr: get_or("SYNAPSE_METRICS_ADDR", "0.0.0.0:9090"),
            routes_path: get_or("SYNAPSE_ROUTES_PATH", "config/routes.toml"),
            pricing_path: get_or("SYNAPSE_PRICING_PATH", "config/pricing.toml"),
            ledger_backend: backend,
            ledger_dsn: get("SYNAPSE_LEDGER_DSN"),
            default_tenant: get_or("SYNAPSE_DEFAULT_TENANT", "unattributed"),
            request_timeout: Duration::from_secs(
                get_or("SYNAPSE_REQUEST_TIMEOUT_SECS", "120")
                    .parse()
                    .map_err(|e| anyhow::anyhow!("SYNAPSE_REQUEST_TIMEOUT_SECS: {e}"))?,
            ),
            env: env.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn defaults_apply_when_env_empty() {
        let c = Config::from_env_map(&env(&[])).unwrap();
        assert_eq!(c.addr, "0.0.0.0:8080");
        assert_eq!(c.ledger_backend, LedgerBackend::Sqlite);
        assert_eq!(c.default_tenant, "unattributed");
    }

    #[test]
    fn env_overrides_and_validates_backend() {
        let c = Config::from_env_map(&env(&[("SYNAPSE_LEDGER_BACKEND", "postgres")])).unwrap();
        assert_eq!(c.ledger_backend, LedgerBackend::Postgres);
        let err = Config::from_env_map(&env(&[("SYNAPSE_LEDGER_BACKEND", "mysql")])).unwrap_err();
        assert!(err.to_string().contains("sqlite|postgres"));
    }
}
