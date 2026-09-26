//! Route table: client-facing model alias → ordered fallback legs.

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct ChainLeg {
    pub provider: String,
    pub model: String,
    /// Optional per-leg region override for the native Vertex lane. When unset,
    /// the lane falls back to the provider's configured region (env
    /// `VERTEX_LOCATION`). Lets a route pin a model to the region that serves it
    /// (e.g. `global` for Gemini 3 previews) without a process-wide env change.
    #[serde(default)]
    pub region: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RouteEntry {
    legs: Vec<ChainLeg>,
    #[serde(default)]
    policy: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RoutesFile {
    routes: HashMap<String, RouteEntry>,
}

#[derive(Debug, Clone)]
pub struct RouteTable {
    routes: HashMap<String, Vec<ChainLeg>>,
    policies: HashMap<String, String>,
}

impl RouteTable {
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        let file = toml::from_str::<RoutesFile>(s)?;
        let mut routes = HashMap::new();
        let mut policies = HashMap::new();
        for (name, entry) in file.routes {
            if let Some(policy) = entry.policy {
                policies.insert(name.clone(), policy);
            }
            routes.insert(name, entry.legs);
        }
        Ok(Self { routes, policies })
    }

    /// Ordered legs for a model alias, or `None` if the alias is unknown.
    pub fn legs(&self, model: &str) -> Option<&[ChainLeg]> {
        self.routes.get(model).map(Vec::as_slice)
    }

    /// Policy name selected by a route alias, or `None` when unset.
    pub fn policy_of(&self, model: &str) -> Option<&str> {
        self.policies.get(model).map(String::as_str)
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

    /// This table with every leg belonging to `drop` removed, and any route left
    /// with no legs removed entirely.
    ///
    /// A multi-leg route survives on its remaining legs, so a route whose first
    /// choice is unavailable degrades to its fallback instead of disappearing.
    pub fn without_providers(&self, drop: &std::collections::HashSet<String>) -> Self {
        let routes: HashMap<String, Vec<ChainLeg>> = self
            .routes
            .iter()
            .map(|(name, legs)| {
                let kept: Vec<ChainLeg> = legs
                    .iter()
                    .filter(|l| !drop.contains(&l.provider))
                    .cloned()
                    .collect();
                (name.clone(), kept)
            })
            .filter(|(_, legs)| !legs.is_empty())
            .collect();
        let policies = self
            .policies
            .iter()
            .filter(|(name, _)| routes.contains_key(*name))
            .map(|(name, policy)| (name.clone(), policy.clone()))
            .collect();
        Self { routes, policies }
    }

    /// Consecutive Vertex legs for Gemini-native passthrough fallback.
    ///
    /// Prefer a route whose first leg is `vertex` + `model` (lexicographically
    /// first alias if several match). Otherwise start at the first matching
    /// vertex leg in the lex-first route that contains it. Stop before the first
    /// non-vertex leg. If nothing matches, return a single synthetic leg so
    /// callers always have ≥1 attempt (same as today's single forward).
    pub fn vertex_fallback_chain(&self, model: &str) -> VertexPassthroughChain {
        let mut aliases: Vec<&String> = self.routes.keys().collect();
        aliases.sort();

        let from_first = aliases.iter().find_map(|alias| {
            let legs = self.routes.get(*alias)?;
            let first = legs.first()?;
            if first.provider == "vertex" && first.model == model {
                Some(((*alias).clone(), 0usize))
            } else {
                None
            }
        });

        let resolved = from_first.or_else(|| {
            aliases.iter().find_map(|alias| {
                let legs = self.routes.get(*alias)?;
                legs.iter()
                    .position(|l| l.provider == "vertex" && l.model == model)
                    .map(|idx| ((*alias).clone(), idx))
            })
        });

        match resolved {
            Some((route, start)) => {
                let legs = self.routes.get(&route).expect("alias from routes keys");
                let chain: Vec<ChainLeg> = legs[start..]
                    .iter()
                    .take_while(|l| l.provider == "vertex")
                    .cloned()
                    .collect();
                VertexPassthroughChain {
                    route: Some(route),
                    legs: chain,
                }
            }
            None => VertexPassthroughChain {
                route: None,
                legs: vec![ChainLeg {
                    provider: "vertex".into(),
                    model: model.to_string(),
                    region: None,
                }],
            },
        }
    }
}

/// Vertex-only fallback chain used by Gemini passthrough.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VertexPassthroughChain {
    /// Matched route alias, if any.
    pub route: Option<String>,
    pub legs: Vec<ChainLeg>,
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

    fn drop_set(ids: &[&str]) -> std::collections::HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn without_providers_keeps_a_route_alive_on_its_remaining_legs() {
        let t = RouteTable::from_toml_str(SAMPLE)
            .unwrap()
            .without_providers(&drop_set(&["vertex"]));
        // gemini-pro loses its vertex leg but still serves via qwen.
        let legs = t.legs("gemini-pro").unwrap();
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0].provider, "qwen");
        // fast was vertex-only, so the alias is gone rather than empty.
        assert!(t.legs("fast").is_none());
        assert_eq!(t.aliases(), vec!["gemini-pro".to_string()]);
    }

    #[test]
    fn without_providers_is_a_no_op_when_nothing_matches() {
        let before = RouteTable::from_toml_str(SAMPLE).unwrap();
        let after = before.without_providers(&drop_set(&["typesafe"]));
        assert_eq!(before.aliases(), after.aliases());
        assert_eq!(after.legs("gemini-pro").unwrap().len(), 2);
    }

    #[test]
    fn without_providers_drops_the_policy_of_a_dropped_route() {
        let with_policy = r#"
            [routes."jev-first"]
            policy = "default"
            legs = [{ provider = "typesafe", model = "jev-latest" }]
            [routes."kept"]
            policy = "default"
            legs = [{ provider = "vertex", model = "gemini-3-flash" }]
        "#;
        let t = RouteTable::from_toml_str(with_policy)
            .unwrap()
            .without_providers(&drop_set(&["typesafe"]));
        assert!(t.policy_of("jev-first").is_none());
        assert_eq!(t.policy_of("kept"), Some("default"));
    }

    #[test]
    fn parses_and_resolves_legs_in_order() {
        let t = RouteTable::from_toml_str(SAMPLE).unwrap();
        let legs = t.legs("gemini-pro").unwrap();
        assert_eq!(legs.len(), 2);
        assert_eq!(
            legs[0],
            ChainLeg {
                provider: "vertex".into(),
                model: "gemini-3-pro".into(),
                ..Default::default()
            }
        );
        assert_eq!(legs[1].provider, "qwen");
        assert!(t.legs("nope").is_none());
    }

    #[test]
    fn parses_optional_per_leg_region() {
        let t = RouteTable::from_toml_str(
            r#"
            [routes."visual"]
            legs = [
              { provider = "vertex", model = "gemini-3.1-pro-preview", region = "global" },
              { provider = "qwen", model = "qwen3-vl-plus" },
            ]
        "#,
        )
        .unwrap();
        let legs = t.legs("visual").unwrap();
        assert_eq!(legs[0].region.as_deref(), Some("global"));
        assert_eq!(legs[1].region, None);
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

    #[test]
    fn parses_optional_route_policy_and_defaults_to_none() {
        let t = RouteTable::from_toml_str(
            r#"
            [routes."guarded"]
            policy = "strict"
            legs = [{ provider = "vertex", model = "gemini-3-pro" }]
            [routes."plain"]
            legs = [{ provider = "qwen", model = "qwen-max" }]
        "#,
        )
        .unwrap();
        assert_eq!(t.policy_of("guarded"), Some("strict"));
        assert_eq!(t.policy_of("plain"), None);
        assert_eq!(t.policy_of("missing"), None);
    }

    #[test]
    fn vertex_fallback_chain_deep_like_and_stops_before_qwen() {
        let t = RouteTable::from_toml_str(
            r#"
            [routes."conversation"]
            legs = [
              { provider = "vertex", model = "gemini-3.1-pro-preview", region = "global" },
              { provider = "vertex", model = "gemini-2.5-pro", region = "us-central1" },
            ]
            [routes."visual"]
            legs = [
              { provider = "vertex", model = "gemini-flash-image", region = "global" },
              { provider = "qwen", model = "qwen3-vl-plus" },
            ]
        "#,
        )
        .unwrap();
        let c = t.vertex_fallback_chain("gemini-3.1-pro-preview");
        assert_eq!(c.route.as_deref(), Some("conversation"));
        assert_eq!(c.legs.len(), 2);
        assert_eq!(c.legs[1].model, "gemini-2.5-pro");

        let image = t.vertex_fallback_chain("gemini-flash-image");
        assert_eq!(image.legs.len(), 1);
    }

    #[test]
    fn vertex_fallback_chain_picks_lex_first_alias() {
        let t = RouteTable::from_toml_str(
            r#"
            [routes."planning"]
            legs = [
              { provider = "vertex", model = "gemini-3.1-pro-preview" },
              { provider = "vertex", model = "gemini-2.5-pro" },
            ]
            [routes."conversation"]
            legs = [
              { provider = "vertex", model = "gemini-3.1-pro-preview" },
              { provider = "vertex", model = "gemini-2.5-pro" },
            ]
        "#,
        )
        .unwrap();
        assert_eq!(
            t.vertex_fallback_chain("gemini-3.1-pro-preview")
                .route
                .as_deref(),
            Some("conversation")
        );
    }

    #[test]
    fn vertex_fallback_chain_unknown_is_synthetic_single() {
        let t = RouteTable::from_toml_str(SAMPLE).unwrap();
        let c = t.vertex_fallback_chain("totally-unknown");
        assert_eq!(c.route, None);
        assert_eq!(c.legs.len(), 1);
        assert_eq!(c.legs[0].model, "totally-unknown");
    }
}
