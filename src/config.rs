//! Layered runtime config (env + file paths). Env takes precedence.

use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub addr: String,
    pub metrics_addr: String,
    pub routes_path: String,
    pub pricing_path: String,
    pub ledger_backends: Vec<LedgerBackend>,
    pub default_tenant: String,
    pub request_timeout: Duration,
    pub stream_idle_timeout: Duration,
    /// Provider credentials/base-urls, read straight from the env map.
    pub env: HashMap<String, String>,
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
            env: env.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
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
}
