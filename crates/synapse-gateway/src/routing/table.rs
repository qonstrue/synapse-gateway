//! Route table: client-facing model alias → ordered fallback legs.

use anyhow::{anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tap::Tap;

use crate::routing::effort::Effort;
use crate::routing::jev_router::{order_legs, EffortPolicy};

/// Jev Score questions accept at most ten levels.
pub const MAX_TIERS: usize = 10;

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChainLeg {
    pub provider: String,
    pub model: String,
    /// Optional per-leg region override for the native Vertex lane. When unset,
    /// the lane falls back to the provider's configured region (env
    /// `VERTEX_LOCATION`). Lets a route pin a model to the region that serves it
    /// (e.g. `global` for Gemini 3 previews) without a process-wide env change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Reasoning effort the route planner chose for this leg. Never read from
    /// config: tiers carry effort and the planner stamps it onto their legs.
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
}

#[derive(Debug, Clone, Deserialize)]
struct RouteEntry {
    /// Optional only so `jev` routes can omit it; static routes still require it.
    #[serde(default)]
    legs: Option<Vec<ChainLeg>>,
    #[serde(default)]
    policy: Option<String>,
    #[serde(default)]
    strategy: Option<String>,
    #[serde(default)]
    jev_router: Option<JevRouterConfig>,
    #[serde(default)]
    tiers: Vec<Tier>,
}

#[derive(Debug, Clone, Deserialize)]
struct RoutesFile {
    routes: HashMap<String, RouteEntry>,
}

/// Decision settings of a `strategy = "jev"` route.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct JevRouterConfig {
    #[serde(default = "default_jev_model")]
    pub model: String,
    pub default_tier: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_min_confidence")]
    pub min_confidence: f64,
    #[serde(default = "default_reasoning_threshold")]
    pub reasoning_threshold: f64,
}

fn default_jev_model() -> String {
    crate::jev_native::DEFAULT_MODEL.to_string()
}
fn default_timeout_ms() -> u64 {
    400
}
fn default_min_confidence() -> f64 {
    0.5
}
fn default_reasoning_threshold() -> f64 {
    0.7
}

/// One difficulty level of a `jev` route; tiers are ordered easiest → hardest.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Tier {
    pub name: String,
    /// Task difficulty in words; sent to Jev as a Score level, so it must
    /// describe the work, never the model.
    pub description: String,
    pub effort: Effort,
    pub legs: Vec<ChainLeg>,
}

/// A validated `strategy = "jev"` route.
#[derive(Debug, Clone, PartialEq)]
pub struct JevRoute {
    pub router: JevRouterConfig,
    pub tiers: Vec<Tier>,
}

impl JevRoute {
    /// Index of `default_tier` (validated to exist at load time).
    pub fn default_index(&self) -> usize {
        self.tiers
            .iter()
            .position(|t| t.name == self.router.default_tier)
            .unwrap_or(0)
    }

    /// Legs in static order: `default_tier`, each harder tier, then each easier
    /// tier, stamped with their tier's configured effort.
    pub fn static_legs(&self) -> Vec<ChainLeg> {
        order_legs(
            &self.tiers,
            self.default_index(),
            EffortPolicy::Tier { bump: false },
        )
        .into_iter()
        .map(|p| p.leg)
        .collect()
    }

    /// This route without legs of `drop`: empty tiers removed and `default_tier`
    /// re-picked (the first surviving tier at or above the old default, else the
    /// hardest surviving tier). `None` when
    /// fewer than two tiers survive — nothing is left to choose between.
    fn pruned(&self, drop: &HashSet<String>) -> Option<JevRoute> {
        let default = self.default_index();
        let survivors: Vec<(usize, Tier)> = self
            .tiers
            .iter()
            .enumerate()
            .map(|(i, t)| {
                (
                    i,
                    Tier {
                        legs: t
                            .legs
                            .iter()
                            .filter(|l| !drop.contains(&l.provider))
                            .cloned()
                            .collect(),
                        ..t.clone()
                    },
                )
            })
            .filter(|(_, t)| !t.legs.is_empty())
            .collect();
        let default_tier = survivors
            .iter()
            .find(|(i, _)| *i >= default)
            .or_else(|| survivors.last())
            .map(|(_, t)| t.name.clone())?;
        (survivors.len() >= 2).then(|| JevRoute {
            router: JevRouterConfig {
                default_tier,
                ..self.router.clone()
            },
            tiers: survivors.into_iter().map(|(_, t)| t).collect(),
        })
    }
}

/// Tier indices in fallback order: `start`, each harder tier, then each easier
/// tier. `start` is clamped into range.
pub fn escalation_order(len: usize, start: usize) -> Vec<usize> {
    let start = start.min(len.saturating_sub(1));
    (start..len).chain((0..start).rev()).collect()
}

#[derive(Debug, Clone)]
pub struct RouteTable {
    /// Every route's legs; a `jev` route's in static order.
    routes: HashMap<String, Vec<ChainLeg>>,
    policies: HashMap<String, String>,
    jev: HashMap<String, JevRoute>,
    /// Aliases declared `strategy = "jev"`, including those since downgraded
    /// to static.
    jev_declared: HashSet<String>,
}

impl RouteTable {
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        let file = toml::from_str::<RoutesFile>(s)?;
        let policies = file
            .routes
            .iter()
            .filter_map(|(name, e)| e.policy.clone().map(|p| (name.clone(), p)))
            .collect();
        let kinds = file
            .routes
            .into_iter()
            .map(|(name, entry)| route_kind(&name, entry).map(|kind| (name, kind)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let jev: HashMap<String, JevRoute> = kinds
            .iter()
            .filter_map(|(name, (_, j))| j.clone().map(|j| (name.clone(), j)))
            .collect();
        let jev_declared = jev.keys().cloned().collect();
        let routes = kinds
            .into_iter()
            .map(|(name, (legs, _))| (name, legs))
            .collect();
        Ok(Self {
            routes,
            policies,
            jev,
            jev_declared,
        })
    }

    /// The tiers and decision settings of a `strategy = "jev"` route.
    pub fn jev_route(&self, model: &str) -> Option<&JevRoute> {
        self.jev.get(model)
    }

    /// Ordered legs for a model alias, or `None` if the alias is unknown.
    pub fn legs(&self, model: &str) -> Option<&[ChainLeg]> {
        self.routes.get(model).map(Vec::as_slice)
    }

    /// Policy name selected by a route alias, or `None` when unset.
    pub fn policy_of(&self, model: &str) -> Option<&str> {
        self.policies.get(model).map(String::as_str)
    }

    /// Every `(alias, policy)` pair a route names, sorted by alias.
    pub fn route_policies(&self) -> Vec<(&str, &str)> {
        let mut v: Vec<(&str, &str)> = self
            .policies
            .iter()
            .map(|(a, p)| (a.as_str(), p.as_str()))
            .collect();
        v.sort();
        v
    }

    /// All registered aliases (for `/v1/models`), sorted for stable output.
    pub fn aliases(&self) -> Vec<String> {
        let mut v: Vec<String> = self.routes.keys().cloned().collect();
        v.sort();
        v
    }

    /// Provider ids referenced by any leg, plus `typesafe` when a `jev` route
    /// needs Jev to decide (for fail-fast credential validation).
    pub fn referenced_providers(&self) -> HashSet<String> {
        self.routes
            .values()
            .flatten()
            .map(|l| l.provider.clone())
            .chain((!self.jev.is_empty()).then(|| "typesafe".to_string()))
            .collect()
    }

    /// This table with every leg belonging to `drop` removed, and any route left
    /// with no legs removed entirely.
    ///
    /// A multi-leg route survives on its remaining legs, so a route whose first
    /// choice is unavailable degrades to its fallback instead of disappearing.
    /// A `jev` route prunes inside its tiers; it becomes a static route when
    /// `typesafe` is dropped or fewer than two tiers survive.
    pub fn without_providers(&self, drop: &HashSet<String>) -> Self {
        let jev: HashMap<String, JevRoute> = self
            .jev
            .iter()
            .filter_map(|(name, route)| {
                match drop.contains("typesafe") {
                    true => None,
                    false => route.pruned(drop),
                }
                .tap(|kept| match kept {
                    None => tracing::warn!(
                        route = %name,
                        "jev route downgraded to static (removed if no legs remain): typesafe unavailable or fewer than two tiers left"
                    ),
                    Some(k) if k.router.default_tier != route.router.default_tier => {
                        tracing::warn!(
                            route = %name,
                            from = %route.router.default_tier,
                            to = %k.router.default_tier,
                            "jev route default_tier pruned; re-picked"
                        )
                    }
                    Some(_) => {}
                })
                .map(|k| (name.clone(), k))
            })
            .collect();
        let routes: HashMap<String, Vec<ChainLeg>> = self
            .routes
            .iter()
            .map(|(name, legs)| {
                let kept = jev.get(name).map_or_else(
                    || {
                        legs.iter()
                            .filter(|l| !drop.contains(&l.provider))
                            .cloned()
                            .collect()
                    },
                    JevRoute::static_legs,
                );
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
        Self {
            routes,
            policies,
            jev,
            jev_declared: self.jev_declared.clone(),
        }
    }

    /// The guard policies a Gemini passthrough request for `model` is screened
    /// under, sorted and distinct. A non-`jev` route alias gives its own policy;
    /// a model id gives the policy of every non-`jev` route with a `vertex` leg
    /// for it. A route without a policy, or a model no route lists, gives
    /// `default`.
    pub fn passthrough_policies(&self, model: &str) -> Vec<&str> {
        let policy = |alias: &str| self.policy_of(alias).unwrap_or("default");
        if !self.jev_declared.contains(model) && self.routes.contains_key(model) {
            return vec![policy(model)];
        }
        let mut v: Vec<&str> = self
            .routes
            .iter()
            .filter(|(alias, legs)| {
                !self.jev_declared.contains(*alias)
                    && legs
                        .iter()
                        .any(|l| l.provider == "vertex" && l.model == model)
            })
            .map(|(alias, _)| policy(alias))
            .collect();
        if v.is_empty() {
            v.push("default");
        }
        v.sort();
        v.dedup();
        v
    }

    /// Consecutive Vertex legs for Gemini-native passthrough fallback.
    ///
    /// Prefer a route whose first leg is `vertex` + `model` (lexicographically
    /// first alias if several match). Otherwise start at the first matching
    /// vertex leg in the lex-first route that contains it. Stop before the first
    /// non-vertex leg. If nothing matches, return a single synthetic leg so
    /// callers always have ≥1 attempt (same as today's single forward).
    /// `jev` routes never take part: their legs span difficulty tiers, not
    /// fallbacks for one model. A `model` that is the alias of a non-`jev` route
    /// resolves to that route's leading Vertex legs, which may be none.
    pub fn vertex_fallback_chain(&self, model: &str) -> VertexPassthroughChain {
        if !self.jev_declared.contains(model) {
            if let Some(legs) = self.routes.get(model) {
                return VertexPassthroughChain {
                    route: Some(model.to_string()),
                    legs: legs
                        .iter()
                        .take_while(|l| l.provider == "vertex")
                        .cloned()
                        .collect(),
                };
            }
        }
        let mut aliases: Vec<&String> = self
            .routes
            .keys()
            .filter(|alias| !self.jev_declared.contains(*alias))
            .collect();
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
                    ..Default::default()
                }],
            },
        }
    }
}

/// Split one `routes.toml` entry into its flat legs and, for `jev` routes, the
/// validated tiers.
fn route_kind(name: &str, entry: RouteEntry) -> anyhow::Result<(Vec<ChainLeg>, Option<JevRoute>)> {
    match (
        entry.strategy.as_deref().unwrap_or("static"),
        entry.tiers.is_empty(),
        entry.legs,
    ) {
        ("static", false, _) => bail!("route '{name}': tiers require strategy = \"jev\""),
        ("static", true, Some(legs)) => Ok((legs, None)),
        ("static", true, None) => bail!("route '{name}': missing field `legs`"),
        ("jev", _, Some(legs)) if !legs.is_empty() => {
            bail!("route '{name}': a jev route declares tiers, not legs")
        }
        ("jev", true, _) => bail!("route '{name}': strategy = \"jev\" requires tiers"),
        ("jev", false, _) => validate_jev_route(name, entry.jev_router, entry.tiers)
            .map(|r| (r.static_legs(), Some(r))),
        (other, _, _) => {
            bail!("route '{name}': unknown strategy '{other}' (expected \"static\" or \"jev\")")
        }
    }
}

fn validate_jev_route(
    name: &str,
    router: Option<JevRouterConfig>,
    tiers: Vec<Tier>,
) -> anyhow::Result<JevRoute> {
    let router = router
        .ok_or_else(|| anyhow!("route '{name}': strategy = \"jev\" requires a jev_router table"))?;
    let problems: Vec<String> = [
        (!(2..=MAX_TIERS).contains(&tiers.len()))
            .then(|| format!("needs 2 to {MAX_TIERS} tiers, found {}", tiers.len())),
        tiers
            .iter()
            .find(|t| t.name.trim().is_empty())
            .map(|_| "tier names must be non-empty".to_string()),
        tiers
            .iter()
            .find(|t| !t.name.chars().all(|c| c.is_ascii_graphic() || c == ' '))
            .map(|t| format!("tier names must be printable ASCII: '{}'", t.name)),
        duplicate_name(&tiers).map(|n| format!("duplicate tier name '{n}'")),
        tiers
            .iter()
            .find(|t| t.description.trim().is_empty())
            .map(|t| format!("tier '{}' needs a description", t.name)),
        tiers
            .iter()
            .find(|t| t.legs.is_empty())
            .map(|t| format!("tier '{}' has no legs", t.name)),
        tiers
            .iter()
            .find(|t| t.legs.iter().any(|l| l.provider == "typesafe"))
            .map(|t| format!("tier '{}' cannot use provider 'typesafe'", t.name)),
        (!tiers.iter().any(|t| t.name == router.default_tier))
            .then(|| format!("default_tier '{}' is not a tier", router.default_tier)),
        (!(0.0..=1.0).contains(&router.min_confidence))
            .then(|| "min_confidence must be within [0, 1]".to_string()),
        (!(0.0..=1.0).contains(&router.reasoning_threshold))
            .then(|| "reasoning_threshold must be within [0, 1]".to_string()),
        (router.timeout_ms == 0).then(|| "timeout_ms must be > 0".to_string()),
    ]
    .into_iter()
    .flatten()
    .collect();
    match problems.as_slice() {
        [] => Ok(JevRoute { router, tiers }),
        _ => bail!("route '{name}': {}", problems.join("; ")),
    }
}

fn duplicate_name(tiers: &[Tier]) -> Option<&str> {
    tiers
        .iter()
        .enumerate()
        .find(|(i, t)| tiers[..*i].iter().any(|p| p.name == t.name))
        .map(|(_, t)| t.name.as_str())
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

    #[test]
    fn vertex_fallback_chain_prefers_an_exact_alias() {
        let t = RouteTable::from_toml_str(
            r#"
            [routes."aaa"]
            legs = [{ provider = "vertex", model = "gemini-x" }]
            [routes."chat"]
            policy = "strict"
            legs = [
              { provider = "vertex", model = "gemini-x", region = "us" },
              { provider = "vertex", model = "gemini-y" },
              { provider = "qwen", model = "qwen-max" },
            ]
        "#,
        )
        .unwrap();
        let by_alias = t.vertex_fallback_chain("chat");
        assert_eq!(by_alias.route.as_deref(), Some("chat"));
        assert_eq!(
            by_alias
                .legs
                .iter()
                .map(|l| l.model.as_str())
                .collect::<Vec<_>>(),
            ["gemini-x", "gemini-y"]
        );
        assert_eq!(
            t.vertex_fallback_chain("gemini-x").route.as_deref(),
            Some("aaa")
        );
    }

    #[test]
    fn vertex_fallback_chain_alias_without_leading_vertex_legs_is_empty() {
        let t = RouteTable::from_toml_str(
            r#"[routes."typed"]
               legs = [{ provider = "typesafe", model = "jev-latest" }, { provider = "vertex", model = "gemini-x" }]"#,
        )
        .unwrap();
        let c = t.vertex_fallback_chain("typed");
        assert_eq!(c.route.as_deref(), Some("typed"));
        assert!(c.legs.is_empty());
    }

    #[test]
    fn vertex_fallback_chain_never_resolves_a_jev_alias() {
        let t = RouteTable::from_toml_str(
            r#"
            [routes."tiered"]
            strategy = "jev"
            [routes."tiered".jev_router]
            default_tier = "standard"
            [[routes."tiered".tiers]]
            name = "standard"
            description = "simple"
            effort = "low"
            legs = [{ provider = "vertex", model = "gemini-x" }]
            [[routes."tiered".tiers]]
            name = "hard"
            description = "complex"
            effort = "medium"
            legs = [{ provider = "vertex", model = "gemini-y" }]
        "#,
        )
        .unwrap();
        assert_eq!(t.vertex_fallback_chain("tiered").route, None);
    }

    #[test]
    fn passthrough_policies_cover_every_route_listing_a_model() {
        let t = RouteTable::from_toml_str(
            r#"
            [routes."aaa"]
            legs = [{ provider = "vertex", model = "gemini-x" }]
            [routes."chat"]
            policy = "strict"
            legs = [{ provider = "qwen", model = "qwen-max" }, { provider = "vertex", model = "gemini-x" }]
            [routes."other"]
            policy = "strict"
            legs = [{ provider = "vertex", model = "gemini-x" }]
        "#,
        )
        .unwrap();
        assert_eq!(t.passthrough_policies("gemini-x"), ["default", "strict"]);
        assert_eq!(t.passthrough_policies("chat"), ["strict"]);
        assert_eq!(t.passthrough_policies("aaa"), ["default"]);
        assert_eq!(t.passthrough_policies("unknown"), ["default"]);
    }

    #[test]
    fn route_policies_lists_alias_policy_pairs() {
        let t = RouteTable::from_toml_str(
            r#"
            [routes."a"]
            policy = "strict"
            legs = [{ provider = "vertex", model = "m" }]
            [routes."b"]
            legs = [{ provider = "vertex", model = "m" }]
        "#,
        )
        .unwrap();
        assert_eq!(t.route_policies(), vec![("a", "strict")]);
    }

    fn tier(name: &str, effort: &str, provider: &str, model: &str) -> String {
        format!(
            "[[routes.\"auto\".tiers]]\nname = \"{name}\"\ndescription = \"{name} work\"\n\
             effort = \"{effort}\"\nlegs = [{{ provider = \"{provider}\", model = \"{model}\" }}]\n"
        )
    }

    fn jev_toml(router_extra: &str, tiers: &[String]) -> String {
        format!(
            "[routes.\"auto\"]\nstrategy = \"jev\"\n[routes.\"auto\".jev_router]\n\
             default_tier = \"moderate\"\n{router_extra}\n{}",
            tiers.concat()
        )
    }

    fn three_tiers() -> Vec<String> {
        vec![
            tier("trivial", "none", "qwen", "qwen-flash"),
            tier("moderate", "low", "vertex", "gemini-2.5-flash"),
            tier("hard", "medium", "vertex", "gemini-2.5-pro"),
        ]
    }

    fn load_err(toml: &str) -> String {
        RouteTable::from_toml_str(toml).unwrap_err().to_string()
    }

    #[test]
    fn parses_a_tiered_route_with_router_defaults() {
        let t = RouteTable::from_toml_str(&jev_toml("", &three_tiers())).unwrap();
        let r = t.jev_route("auto").unwrap();
        assert_eq!(r.tiers.len(), 3);
        assert_eq!(r.tiers[2].effort, crate::routing::effort::Effort::Medium);
        assert_eq!(r.router.model, "jev-latest");
        assert_eq!(r.router.timeout_ms, 400);
        assert_eq!(r.router.min_confidence, 0.5);
        assert_eq!(r.router.reasoning_threshold, 0.7);
        assert_eq!(r.default_index(), 1);
        assert!(t.jev_route("missing").is_none());
    }

    #[test]
    fn jev_route_legs_are_in_static_order_with_tier_effort() {
        let t = RouteTable::from_toml_str(&jev_toml("", &three_tiers())).unwrap();
        let legs = t.legs("auto").unwrap();
        let got: Vec<(&str, Option<crate::routing::effort::Effort>)> =
            legs.iter().map(|l| (l.model.as_str(), l.effort)).collect();
        use crate::routing::effort::Effort;
        assert_eq!(
            got,
            vec![
                ("gemini-2.5-flash", Some(Effort::Low)),
                ("gemini-2.5-pro", Some(Effort::Medium)),
                ("qwen-flash", Some(Effort::None)),
            ]
        );
    }

    #[test]
    fn referenced_providers_include_typesafe_for_jev_routes() {
        let t = RouteTable::from_toml_str(&jev_toml("", &three_tiers())).unwrap();
        let p = t.referenced_providers();
        assert!(p.contains("typesafe"));
        assert!(p.contains("qwen"));
        assert!(!RouteTable::from_toml_str(SAMPLE)
            .unwrap()
            .referenced_providers()
            .contains("typesafe"));
    }

    #[test]
    fn empty_static_legs_stay_legal() {
        assert!(RouteTable::from_toml_str("[routes.\"dummy\"]\nlegs = []").is_ok());
    }

    #[test]
    fn jev_route_with_empty_legs_and_valid_tiers_is_accepted() {
        let toml = jev_toml("", &three_tiers()).replacen(
            "strategy = \"jev\"\n",
            "strategy = \"jev\"\nlegs = []\n",
            1,
        );
        let t = RouteTable::from_toml_str(&toml).unwrap();
        assert_eq!(t.jev_route("auto").unwrap().tiers.len(), 3);
        assert_eq!(t.legs("auto").unwrap().len(), 3);
    }

    #[test]
    fn static_route_without_legs_is_still_rejected() {
        assert!(
            load_err("[routes.\"dummy\"]\npolicy = \"default\"\n").contains("missing field `legs`")
        );
    }

    #[test]
    fn rejects_invalid_tier_config() {
        let cases: Vec<(String, &str)> = vec![
            (
                format!("[routes.\"auto\"]\n{}", tier("a", "low", "qwen", "m")),
                "tiers require strategy = \"jev\"",
            ),
            (
                "[routes.\"auto\"]\nstrategy = \"jev\"\nlegs = [{ provider = \"qwen\", model = \"m\" }]\n\
                 [routes.\"auto\".jev_router]\ndefault_tier = \"a\"\n"
                    .to_string(),
                "declares tiers, not legs",
            ),
            (
                "[routes.\"auto\"]\nstrategy = \"jev\"\n[routes.\"auto\".jev_router]\ndefault_tier = \"a\"\n"
                    .to_string(),
                "requires tiers",
            ),
            (
                "[routes.\"auto\"]\nstrategy = \"fastest\"\nlegs = []\n".to_string(),
                "unknown strategy 'fastest'",
            ),
            (
                format!("[routes.\"auto\"]\nstrategy = \"jev\"\n{}", three_tiers().concat()),
                "requires a jev_router table",
            ),
            (jev_toml("", &[tier("moderate", "low", "qwen", "m")]), "needs 2 to 10 tiers"),
            (
                jev_toml(
                    "",
                    &[tier("moderate", "low", "qwen", "a"), tier("moderate", "high", "qwen", "b")],
                ),
                "duplicate tier name 'moderate'",
            ),
            (
                jev_toml(
                    "",
                    &[tier("easy", "low", "qwen", "a"), tier("hard", "high", "qwen", "b")],
                ),
                "default_tier 'moderate' is not a tier",
            ),
            (
                jev_toml(
                    "",
                    &[tier("moderate", "low", "qwen", "a"), tier("jev", "high", "typesafe", "jev-latest")],
                ),
                "tier 'jev' cannot use provider 'typesafe'",
            ),
            (
                jev_toml(
                    "",
                    &[
                        tier("moderate", "low", "qwen", "a"),
                        "[[routes.\"auto\".tiers]]\nname = \"hard\"\ndescription = \"hard work\"\neffort = \"high\"\nlegs = []\n"
                            .to_string(),
                    ],
                ),
                "tier 'hard' has no legs",
            ),
            (
                jev_toml(
                    "",
                    &[
                        tier("moderate", "low", "qwen", "a"),
                        "[[routes.\"auto\".tiers]]\nname = \"hard\"\ndescription = \" \"\neffort = \"high\"\nlegs = [{ provider = \"qwen\", model = \"b\" }]\n"
                            .to_string(),
                    ],
                ),
                "tier 'hard' needs a description",
            ),
            (jev_toml("min_confidence = 1.5", &three_tiers()), "min_confidence must be within [0, 1]"),
            (
                jev_toml("reasoning_threshold = -0.1", &three_tiers()),
                "reasoning_threshold must be within [0, 1]",
            ),
            (jev_toml("timeout_ms = 0", &three_tiers()), "timeout_ms must be > 0"),
            (
                jev_toml("", &[tier("moderate", "extreme", "qwen", "a"), tier("hard", "high", "qwen", "b")]),
                "unknown variant",
            ),
            (
                jev_toml("", &[tier("moderate", "low", "qwen", "a"), tier("", "high", "qwen", "b")]),
                "tier names must be non-empty",
            ),
            (
                jev_toml("", &[tier("moderate", "low", "qwen", "a"), tier("  ", "high", "qwen", "b")]),
                "tier names must be non-empty",
            ),
            (
                jev_toml("", &[tier("moderate", "low", "qwen", "a"), tier("difícil", "high", "qwen", "b")]),
                "tier names must be printable ASCII: 'difícil'",
            ),
            (
                jev_toml(
                    "",
                    &std::iter::once(tier("moderate", "low", "qwen", "m"))
                        .chain((1..=MAX_TIERS).map(|i| tier(&format!("t{i}"), "low", "qwen", "m")))
                        .collect::<Vec<_>>(),
                ),
                "needs 2 to 10 tiers, found 11",
            ),
        ];
        cases.iter().for_each(|(toml, needle)| {
            let err = load_err(toml);
            assert!(err.contains(needle), "expected '{needle}' in: {err}");
        });
    }

    #[test]
    fn pruning_drops_empty_tiers_and_keeps_the_route_jev() {
        let toml = jev_toml(
            "",
            &[
                tier("trivial", "none", "qwen", "qwen-flash"),
                tier("moderate", "low", "vertex", "gemini-2.5-flash"),
                tier("hard", "medium", "vertex", "gemini-2.5-pro"),
                tier("expert", "high", "openai", "gpt-x"),
            ],
        );
        let t = RouteTable::from_toml_str(&toml)
            .unwrap()
            .without_providers(&drop_set(&["qwen"]));
        let r = t.jev_route("auto").unwrap();
        assert_eq!(
            r.tiers.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["moderate", "hard", "expert"]
        );
        assert_eq!(r.router.default_tier, "moderate");
        assert!(t.legs("auto").unwrap().iter().all(|l| l.provider != "qwen"));
    }

    #[test]
    fn pruning_the_default_tier_repicks_the_nearest_harder_tier() {
        let t = RouteTable::from_toml_str(&jev_toml("", &three_tiers()))
            .unwrap()
            .without_providers(&drop_set(&["vertex"]));
        // moderate and hard were vertex-only: one tier left ⇒ downgraded to static.
        assert!(t.jev_route("auto").is_none());
        assert_eq!(t.legs("auto").unwrap().len(), 1);
        assert_eq!(t.legs("auto").unwrap()[0].model, "qwen-flash");

        let four = jev_toml(
            "",
            &[
                tier("trivial", "none", "qwen", "qwen-flash"),
                tier("moderate", "low", "openai", "gpt-mini"),
                tier("hard", "medium", "vertex", "gemini-2.5-pro"),
                tier("expert", "high", "vertex", "gemini-3.1-pro-preview"),
            ],
        );
        let t = RouteTable::from_toml_str(&four)
            .unwrap()
            .without_providers(&drop_set(&["openai"]));
        assert_eq!(t.jev_route("auto").unwrap().router.default_tier, "hard");
    }

    #[test]
    fn pruning_the_hardest_default_falls_back_to_the_nearest_easier_tier() {
        use crate::routing::effort::Effort;
        let toml = jev_toml(
            "",
            &[
                tier("trivial", "none", "qwen", "qwen-flash"),
                tier("easy", "low", "openai", "gpt-mini"),
                tier("moderate", "high", "vertex", "gemini-2.5-pro"),
            ],
        );
        let t = RouteTable::from_toml_str(&toml)
            .unwrap()
            .without_providers(&drop_set(&["vertex"]));
        assert_eq!(t.jev_route("auto").unwrap().router.default_tier, "easy");
        assert_eq!(
            t.legs("auto")
                .unwrap()
                .iter()
                .map(|l| (l.model.as_str(), l.effort))
                .collect::<Vec<_>>(),
            vec![
                ("gpt-mini", Some(Effort::Low)),
                ("qwen-flash", Some(Effort::None)),
            ]
        );
    }

    #[test]
    fn pruning_every_tier_removes_the_route() {
        let t = RouteTable::from_toml_str(&jev_toml("", &three_tiers()))
            .unwrap()
            .without_providers(&drop_set(&["qwen", "vertex"]));
        assert!(t.jev_route("auto").is_none());
        assert!(t.legs("auto").is_none());
    }

    #[test]
    fn pruning_typesafe_downgrades_jev_routes_to_static() {
        use crate::routing::effort::Effort;
        let t = RouteTable::from_toml_str(&jev_toml("", &three_tiers()))
            .unwrap()
            .without_providers(&drop_set(&["typesafe"]));
        assert!(t.jev_route("auto").is_none());
        assert_eq!(
            t.legs("auto")
                .unwrap()
                .iter()
                .map(|l| (l.model.as_str(), l.effort))
                .collect::<Vec<_>>(),
            vec![
                ("gemini-2.5-flash", Some(Effort::Low)),
                ("gemini-2.5-pro", Some(Effort::Medium)),
                ("qwen-flash", Some(Effort::None)),
            ]
        );
        assert!(!t.referenced_providers().contains("typesafe"));
    }

    #[test]
    fn jev_routes_never_join_the_vertex_fallback_chain() {
        let toml = format!(
            "{}\n[routes.\"conversation\"]\nlegs = [{{ provider = \"vertex\", model = \"gemini-2.5-pro\" }}]\n",
            jev_toml(
                "",
                &[
                    tier("trivial", "none", "qwen", "qwen-flash"),
                    tier("moderate", "low", "vertex", "gemini-2.5-pro"),
                    tier("hard", "medium", "vertex", "gemini-3.1-pro-preview"),
                ],
            )
        );
        let tiered = RouteTable::from_toml_str(&toml).unwrap();
        let downgraded = tiered.without_providers(&drop_set(&["typesafe"]));
        assert!(downgraded.jev_route("auto").is_none());
        [tiered, downgraded].iter().for_each(|t| {
            let pro = t.vertex_fallback_chain("gemini-2.5-pro");
            assert_eq!(pro.route.as_deref(), Some("conversation"));
            assert_eq!(pro.legs.len(), 1);
            let preview = t.vertex_fallback_chain("gemini-3.1-pro-preview");
            assert_eq!(preview.route, None);
            assert_eq!(preview.legs.len(), 1);
        });
    }

    #[test]
    fn shipped_routes_and_commented_jev_example_parse() {
        let shipped = include_str!("../../config/routes.toml");
        assert!(RouteTable::from_toml_str(shipped).is_ok());
        let example = shipped
            .lines()
            .skip_while(|l| !l.starts_with("# [routes.\"auto\"]"))
            .map(|l| {
                l.strip_prefix("# ")
                    .or_else(|| l.strip_prefix('#'))
                    .unwrap_or(l)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let t = RouteTable::from_toml_str(&example).unwrap();
        assert_eq!(t.jev_route("auto").unwrap().tiers.len(), 4);
    }
}
