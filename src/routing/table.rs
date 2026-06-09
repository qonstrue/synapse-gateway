//! Route table: client-facing model alias → ordered fallback legs.

use serde::Deserialize;
use std::collections::HashMap;
use tap::Pipe;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ChainLeg {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RouteEntry {
    legs: Vec<ChainLeg>,
}

#[derive(Debug, Clone, Deserialize)]
struct RoutesFile {
    routes: HashMap<String, RouteEntry>,
}

#[derive(Debug, Clone)]
pub struct RouteTable {
    routes: HashMap<String, Vec<ChainLeg>>,
}

impl RouteTable {
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        toml::from_str::<RoutesFile>(s)?
            .routes
            .into_iter()
            .map(|(name, entry)| (name, entry.legs))
            .collect::<HashMap<_, _>>()
            .pipe(|routes| Self { routes })
            .pipe(Ok)
    }

    /// Ordered legs for a model alias, or `None` if the alias is unknown.
    pub fn legs(&self, model: &str) -> Option<&[ChainLeg]> {
        self.routes.get(model).map(Vec::as_slice)
    }

    /// All registered aliases (for `/v1/models`), sorted for stable output.
    pub fn aliases(&self) -> Vec<String> {
        let mut v: Vec<String> = self.routes.keys().cloned().collect();
        v.sort();
        v
    }

    /// Provider ids referenced by any leg (for fail-fast credential validation).
    pub fn referenced_providers(&self) -> std::collections::HashSet<String> {
        self.routes
            .values()
            .flatten()
            .map(|l| l.provider.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [routes."gemini-pro"]
        legs = [
          { provider = "vertex", model = "gemini-3-pro" },
          { provider = "qwen", model = "qwen-max" },
        ]
        [routes."fast"]
        legs = [{ provider = "vertex", model = "gemini-3-flash" }]
    "#;

    #[test]
    fn parses_and_resolves_legs_in_order() {
        let t = RouteTable::from_toml_str(SAMPLE).unwrap();
        let legs = t.legs("gemini-pro").unwrap();
        assert_eq!(legs.len(), 2);
        assert_eq!(
            legs[0],
            ChainLeg {
                provider: "vertex".into(),
                model: "gemini-3-pro".into()
            }
        );
        assert_eq!(legs[1].provider, "qwen");
        assert!(t.legs("nope").is_none());
    }

    #[test]
    fn aliases_are_sorted() {
        let t = RouteTable::from_toml_str(SAMPLE).unwrap();
        assert_eq!(
            t.aliases(),
            vec!["fast".to_string(), "gemini-pro".to_string()]
        );
    }

    #[test]
    fn referenced_providers_collected() {
        let t = RouteTable::from_toml_str(SAMPLE).unwrap();
        let p = t.referenced_providers();
        assert!(p.contains("vertex"));
        assert!(p.contains("qwen"));
    }
}
