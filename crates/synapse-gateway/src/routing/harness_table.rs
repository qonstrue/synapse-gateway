//! `config/harnesses.toml` loader: `[bases]` runtime capabilities, `[harnesses]`
//! UHP catalog entries, and `[harness_routes]` 2D `(profile × tier)` routing tables.
//! Spec: `docs/superpowers/specs/2026-10-31-jev-2d-harness-router-design.md`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::{anyhow, bail};
use serde::{Deserialize, Serialize};
use tap::{Pipe, Tap};

use crate::routing::effort::Effort;
use crate::routing::table::{escalation_order, ChainLeg, RouteTable, MAX_TIERS};

fn default_true() -> bool {
    true
}
fn default_jev_model() -> String {
    "jev-latest".into()
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

/// Wire protocols a harness base can speak when calling back into `synapse-gateway`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WireProtocol {
    AnthropicMessages,
    VertexAnthropic,
    OpenaiResponses,
    GeminiNative,
    OpenaiChat,
    Bedrock,
    Typesafe,
}

impl WireProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic_messages",
            Self::VertexAnthropic => "vertex_anthropic",
            Self::OpenaiResponses => "openai_responses",
            Self::GeminiNative => "gemini_native",
            Self::OpenaiChat => "openai_chat",
            Self::Bedrock => "bedrock",
            Self::Typesafe => "typesafe",
        }
    }
}

/// How a harness base enforces `disabled_tools` restrictions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEnforcement {
    Hard,
    #[default]
    Instruction,
}

/// Runtime capabilities of one harness binary in `[bases.<id>]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BaseSpec {
    pub label: String,
    pub wire: HashSet<WireProtocol>,
    #[serde(default = "default_true")]
    pub skills: bool,
    #[serde(default = "default_true")]
    pub environments: bool,
    #[serde(default)]
    pub mcp_transports: HashSet<String>,
    #[serde(default)]
    pub tool_enforcement: ToolEnforcement,
}

/// Static seed entry in `[harnesses.<id>]` served by `GET /v1/harnesses`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ConfiguredHarness {
    pub name: String,
    pub base: String,
    #[serde(default)]
    pub route: Option<String>,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub disabled_tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct HarnessJevRouterConfig {
    #[serde(default = "default_jev_model")]
    pub model: String,
    #[serde(default)]
    pub default_profile: Option<String>,
    pub default_tier: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_min_confidence")]
    pub min_confidence: f64,
    #[serde(default = "default_min_confidence")]
    pub min_profile_confidence: f64,
    #[serde(default = "default_reasoning_threshold")]
    pub reasoning_threshold: f64,
    #[serde(default)]
    pub horizon_threshold: Option<f64>,
}

/// Axis 1 (work archetype) entry in `[harness_routes.<alias>.profiles.<name>]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HarnessProfile {
    pub name: String,
    pub description: String,
    pub bases: Vec<String>,
}

/// A model leg inside a harness tier: either a reference to `routes.toml` or a direct leg.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum ModelTarget {
    RouteAlias { route: String },
    Direct(ChainLeg),
}

/// Axis 2 (difficulty tier) entry in `[[harness_routes.<alias>.tiers]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessTier {
    pub name: String,
    pub description: String,
    pub effort: Effort,
    pub max_step: Option<u32>,
    pub timeout_seconds: Option<u32>,
    pub bases: Option<Vec<String>>,
    pub legs: Vec<ChainLeg>,
}

/// Static fallback leg on a `strategy = "static"` harness route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticHarnessLeg {
    pub base: String,
    pub leg: ChainLeg,
    pub wire: WireProtocol,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HarnessJevRoute {
    pub policy: Option<String>,
    pub router: HarnessJevRouterConfig,
    pub profiles: Vec<HarnessProfile>,
    pub tiers: Vec<HarnessTier>,
}

impl HarnessJevRoute {
    pub fn default_tier_index(&self) -> usize {
        self.tiers
            .iter()
            .position(|t| t.name == self.router.default_tier)
            .unwrap_or(0)
    }

    pub fn default_profile_index(&self) -> usize {
        self.router
            .default_profile
            .as_deref()
            .and_then(|def| self.profiles.iter().position(|p| p.name == def))
            .unwrap_or(0)
    }
}

/// Highest-fidelity wire protocol shared by `base_wires` and `(provider, model)`.
pub fn preferred_wire(
    base_wires: &HashSet<WireProtocol>,
    provider: &str,
    model: &str,
) -> Option<WireProtocol> {
    provider_wires(provider, model)
        .iter()
        .copied()
        .find(|w| base_wires.contains(w))
}

/// Ordered candidate wire protocols that `(provider, model)` can serve.
pub fn provider_wires(provider: &str, model: &str) -> &'static [WireProtocol] {
    match provider {
        "anthropic" => &[WireProtocol::AnthropicMessages],
        "vertex" if model.starts_with("claude-") => &[WireProtocol::VertexAnthropic],
        "vertex" => &[WireProtocol::GeminiNative, WireProtocol::OpenaiChat],
        "openai" => &[WireProtocol::OpenaiResponses, WireProtocol::OpenaiChat],
        "bedrock" => &[WireProtocol::Bedrock],
        "typesafe" => &[WireProtocol::Typesafe],
        _ => &[WireProtocol::OpenaiChat],
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct HarnessesFile {
    #[serde(default)]
    bases: BTreeMap<String, BaseSpec>,
    #[serde(default)]
    harnesses: BTreeMap<String, ConfiguredHarness>,
    #[serde(default)]
    harness_routes: HashMap<String, HarnessRouteEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProfileEntry {
    description: String,
    bases: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TierEntry {
    name: String,
    description: String,
    effort: Effort,
    #[serde(default)]
    max_step: Option<u32>,
    #[serde(default)]
    timeout_seconds: Option<u32>,
    #[serde(default)]
    bases: Option<Vec<String>>,
    #[serde(default)]
    legs: Vec<ModelTarget>,
}

#[derive(Debug, Clone, Deserialize)]
struct StaticLegEntry {
    base: String,
    #[serde(flatten)]
    target: ModelTarget,
}

#[derive(Debug, Clone, Deserialize)]
struct HarnessRouteEntry {
    #[serde(default)]
    policy: Option<String>,
    #[serde(default)]
    strategy: Option<String>,
    #[serde(default)]
    jev_router: Option<HarnessJevRouterConfig>,
    #[serde(default)]
    profiles: BTreeMap<String, ProfileEntry>,
    #[serde(default)]
    tiers: Option<Vec<TierEntry>>,
    #[serde(default)]
    legs: Option<Vec<StaticLegEntry>>,
}

#[derive(Debug, Clone, Default)]
pub struct HarnessRouteTable {
    bases: BTreeMap<String, BaseSpec>,
    harnesses: BTreeMap<String, ConfiguredHarness>,
    static_routes: HashMap<String, Vec<StaticHarnessLeg>>,
    jev_routes: HashMap<String, HarnessJevRoute>,
    policies: HashMap<String, String>,
}

impl HarnessRouteTable {
    pub fn from_path(path: impl AsRef<Path>, routes: &RouteTable) -> anyhow::Result<Self> {
        let p = path.as_ref();
        match p.exists() {
            false => Ok(Self::default()),
            true => std::fs::read_to_string(p)?.pipe(|s| Self::from_toml_str(&s, routes)),
        }
    }

    pub fn from_toml_str(s: &str, routes: &RouteTable) -> anyhow::Result<Self> {
        let parsed: HarnessesFile = toml::from_str(s)?;
        parsed.bases.iter().try_for_each(|(id, spec)| {
            if id.trim().is_empty() || spec.label.trim().is_empty() {
                bail!("base '{id}' requires a non-empty id and label");
            }
            if spec.wire.is_empty() {
                bail!("base '{id}' must declare at least one wire protocol");
            }
            Ok(())
        })?;

        let policies: HashMap<String, String> = parsed
            .harness_routes
            .iter()
            .filter_map(|(alias, entry)| {
                entry
                    .policy
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|p| (alias.clone(), p.to_string()))
            })
            .collect();

        let built_routes: Vec<(String, Vec<StaticHarnessLeg>, Option<HarnessJevRoute>)> = parsed
            .harness_routes
            .into_iter()
            .map(
                |(alias, entry)| match entry.strategy.as_deref().unwrap_or("static") {
                    "static" => {
                        if entry.tiers.is_some() || !entry.profiles.is_empty() {
                            bail!(
                            "harness route '{alias}': profiles and tiers require strategy = \"jev\""
                        );
                        }
                        if entry.jev_router.is_some() {
                            bail!(
                                "harness route '{alias}': jev_router requires strategy = \"jev\""
                            );
                        }
                        let legs = expand_static_legs(
                            &alias,
                            entry.legs.as_deref().unwrap_or(&[]),
                            &parsed.bases,
                            routes,
                        )?;
                        Ok((alias, legs, None))
                    }
                    "jev" => {
                        let built = build_jev_route(&alias, entry, &parsed.bases, routes)?;
                        let flat = flatten_jev_to_static(&built, &parsed.bases);
                        Ok((alias, flat, Some(built)))
                    }
                    other => bail!("harness route '{alias}': unknown strategy '{other}'"),
                },
            )
            .collect::<anyhow::Result<Vec<_>>>()?;

        let static_routes: HashMap<String, Vec<StaticHarnessLeg>> = built_routes
            .iter()
            .map(|(alias, legs, _)| (alias.clone(), legs.clone()))
            .collect();
        let jev_routes: HashMap<String, HarnessJevRoute> = built_routes
            .into_iter()
            .filter_map(|(alias, _, jev)| jev.map(|j| (alias, j)))
            .collect();

        parsed
            .harnesses
            .iter()
            .try_for_each(|(id, h)| match h.base.as_str() {
                "auto" => {
                    let route_alias = h.route.as_deref().ok_or_else(|| {
                        anyhow!("harness '{id}' with base = \"auto\" requires a route")
                    })?;
                    match static_routes.contains_key(route_alias) {
                        true => Ok(()),
                        false => {
                            bail!("harness '{id}' references unknown harness_route '{route_alias}'")
                        }
                    }
                }
                concrete => match parsed.bases.contains_key(concrete) {
                    true => Ok(()),
                    false => bail!("harness '{id}' references unknown base '{concrete}'"),
                },
            })?;

        Ok(Self {
            bases: parsed.bases,
            harnesses: parsed.harnesses,
            static_routes,
            jev_routes,
            policies,
        })
    }

    pub fn bases(&self) -> &BTreeMap<String, BaseSpec> {
        &self.bases
    }

    pub fn base(&self, id: &str) -> Option<&BaseSpec> {
        self.bases.get(id)
    }

    pub fn harnesses(&self) -> &BTreeMap<String, ConfiguredHarness> {
        &self.harnesses
    }

    pub fn harness(&self, id: &str) -> Option<&ConfiguredHarness> {
        self.harnesses.get(id)
    }

    pub fn jev_route(&self, alias: &str) -> Option<&HarnessJevRoute> {
        self.jev_routes.get(alias)
    }

    pub fn static_legs(&self, alias: &str) -> Option<&[StaticHarnessLeg]> {
        self.static_routes.get(alias).map(Vec::as_slice)
    }

    pub fn policy(&self, alias: &str) -> Option<&str> {
        self.policies.get(alias).map(String::as_str)
    }

    pub fn referenced_providers(&self) -> HashSet<String> {
        self.static_routes
            .values()
            .flatten()
            .map(|l| l.leg.provider.clone())
            .chain((!self.jev_routes.is_empty()).then(|| "typesafe".to_string()))
            .collect()
    }

    /// Remove legs whose provider is in `drop`, re-picking `default_tier` or
    /// downgrading `jev` routes to static when `typesafe` is dropped or `< 2`
    /// tiers survive.
    pub fn without_providers(&self, drop: &HashSet<String>) -> Self {
        let bases = self.bases.clone();
        let jev_routes: HashMap<String, HarnessJevRoute> = match drop.contains("typesafe") {
            true => {
                self.jev_routes.keys().for_each(|alias| {
                    tracing::warn!(
                        route = %alias,
                        "provider 'typesafe' is not configured; downgrading 2D jev harness route to static"
                    );
                });
                HashMap::new()
            }
            false => self
                .jev_routes
                .iter()
                .filter_map(|(alias, route)| {
                    prune_jev_route(alias, route, &bases, drop).map(|r| (alias.clone(), r))
                })
                .collect(),
        };
        let static_routes = self
            .static_routes
            .iter()
            .map(|(alias, legs)| {
                let kept = match jev_routes.get(alias) {
                    Some(jev) => flatten_jev_to_static(jev, &bases),
                    None => legs
                        .iter()
                        .filter(|l| !drop.contains(&l.leg.provider))
                        .cloned()
                        .collect(),
                };
                (alias.clone(), kept)
            })
            .collect();
        Self {
            bases,
            harnesses: self.harnesses.clone(),
            static_routes,
            jev_routes,
            policies: self.policies.clone(),
        }
    }

    /// Remove bases in `drop_bases` from the base catalog, profiles, tier base
    /// overrides, and static legs; collapse `profiles` to 1D when `< 2` survive.
    pub fn without_bases(&self, drop_bases: &HashSet<String>) -> Self {
        let bases: BTreeMap<String, BaseSpec> = self
            .bases
            .iter()
            .filter(|(k, _)| !drop_bases.contains(*k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let empty_providers = HashSet::new();
        let jev_routes: HashMap<String, HarnessJevRoute> = self
            .jev_routes
            .iter()
            .filter_map(|(alias, route)| {
                prune_jev_route(alias, route, &bases, &empty_providers).map(|r| (alias.clone(), r))
            })
            .collect();
        let static_routes = self
            .static_routes
            .iter()
            .map(|(alias, legs)| {
                let kept = match jev_routes.get(alias) {
                    Some(jev) => flatten_jev_to_static(jev, &bases),
                    None => legs
                        .iter()
                        .filter(|l| bases.contains_key(&l.base))
                        .cloned()
                        .collect(),
                };
                (alias.clone(), kept)
            })
            .collect();
        Self {
            bases,
            harnesses: self.harnesses.clone(),
            static_routes,
            jev_routes,
            policies: self.policies.clone(),
        }
    }
}

fn expand_targets(
    alias: &str,
    targets: &[ModelTarget],
    routes: &RouteTable,
    allow_typesafe: bool,
) -> anyhow::Result<Vec<ChainLeg>> {
    targets
        .iter()
        .map(|target| match target {
            ModelTarget::Direct(leg) => match !allow_typesafe && leg.provider == "typesafe" {
                true => bail!(
                    "harness route '{alias}': leg cannot use provider 'typesafe' unless base wire is 'typesafe'"
                ),
                false => Ok(vec![leg.clone()]),
            },
            ModelTarget::RouteAlias { route } => {
                let resolved = routes.legs(route).ok_or_else(|| {
                    anyhow!(
                        "harness route '{alias}' references unknown route '{route}' in routes.toml"
                    )
                })?;
                let filtered: Vec<ChainLeg> = resolved
                    .iter()
                    .filter(|l| allow_typesafe || l.provider != "typesafe")
                    .cloned()
                    .collect();
                match filtered.is_empty() {
                    true => bail!(
                        "harness route '{alias}': route alias '{route}' resolves to no usable legs"
                    ),
                    false => Ok(filtered),
                }
            }
        })
        .collect::<anyhow::Result<Vec<Vec<ChainLeg>>>>()
        .map(|nested| nested.into_iter().flatten().collect())
}

fn expand_static_legs(
    alias: &str,
    entries: &[StaticLegEntry],
    bases: &BTreeMap<String, BaseSpec>,
    routes: &RouteTable,
) -> anyhow::Result<Vec<StaticHarnessLeg>> {
    entries
        .iter()
        .map(|entry| {
            let spec = bases.get(&entry.base).ok_or_else(|| {
                anyhow!("harness route '{alias}': unknown base '{}'", entry.base)
            })?;
            let allow_typesafe = spec.wire.contains(&WireProtocol::Typesafe);
            let legs = expand_targets(
                alias,
                std::slice::from_ref(&entry.target),
                routes,
                allow_typesafe,
            )?;
            legs.into_iter()
                .map(|leg| {
                    let wire = preferred_wire(&spec.wire, &leg.provider, &leg.model).ok_or_else(
                        || {
                            anyhow!(
                                "harness route '{alias}': base '{}' wire is incompatible with ({}, {})",
                                entry.base,
                                leg.provider,
                                leg.model
                            )
                        },
                    )?;
                    Ok(StaticHarnessLeg {
                        base: entry.base.clone(),
                        leg,
                        wire,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()
        })
        .collect::<anyhow::Result<Vec<Vec<_>>>>()
        .map(|nested| nested.into_iter().flatten().collect())
}

fn validate_printable_name(alias: &str, kind: &str, name: &str) -> anyhow::Result<()> {
    if name.trim().is_empty() {
        bail!("harness route '{alias}': a {kind} has an empty name");
    }
    if !name.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        bail!(
            "harness route '{alias}': {kind} name '{name}' must be printable ASCII without spaces"
        );
    }
    Ok(())
}

fn build_jev_route(
    alias: &str,
    entry: HarnessRouteEntry,
    bases: &BTreeMap<String, BaseSpec>,
    routes: &RouteTable,
) -> anyhow::Result<HarnessJevRoute> {
    if entry.legs.is_some() {
        bail!("harness route '{alias}': strategy = \"jev\" declares tiers, not legs");
    }
    let router = entry.jev_router.ok_or_else(|| {
        anyhow!("harness route '{alias}': strategy = \"jev\" requires a jev_router table")
    })?;
    if !(0.0..=1.0).contains(&router.min_confidence) {
        bail!("harness route '{alias}': min_confidence must be within [0, 1]");
    }
    if !(0.0..=1.0).contains(&router.min_profile_confidence) {
        bail!("harness route '{alias}': min_profile_confidence must be within [0, 1]");
    }
    if !(0.0..=1.0).contains(&router.reasoning_threshold) {
        bail!("harness route '{alias}': reasoning_threshold must be within [0, 1]");
    }
    if router
        .horizon_threshold
        .is_some_and(|h| !(0.0..=1.0).contains(&h))
    {
        bail!("harness route '{alias}': horizon_threshold must be within [0, 1]");
    }
    if router.timeout_ms == 0 {
        bail!("harness route '{alias}': timeout_ms must be > 0");
    }

    let profiles: Vec<HarnessProfile> = entry
        .profiles
        .into_iter()
        .map(|(name, p)| {
            validate_printable_name(alias, "profile", &name)?;
            if p.description.trim().is_empty() {
                bail!("harness route '{alias}': profile '{name}' needs a description");
            }
            if p.bases.is_empty() {
                bail!("harness route '{alias}': profile '{name}' has no bases");
            }
            p.bases.iter().try_for_each(|b| {
                bases.contains_key(b).then_some(()).ok_or_else(|| {
                    anyhow!(
                        "harness route '{alias}': profile '{name}' references unknown base '{b}'"
                    )
                })
            })?;
            Ok(HarnessProfile {
                name,
                description: p.description,
                bases: p.bases,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    if !profiles.is_empty() {
        if !(2..=MAX_TIERS).contains(&profiles.len()) {
            bail!(
                "harness route '{alias}': strategy = \"jev\" needs 2 to {MAX_TIERS} profiles when profiles are declared, got {}",
                profiles.len()
            );
        }
        let def_prof = router.default_profile.as_deref().ok_or_else(|| {
            anyhow!(
                "harness route '{alias}': default_profile is required when profiles are declared"
            )
        })?;
        if !profiles.iter().any(|p| p.name == def_prof) {
            bail!("harness route '{alias}': default_profile '{def_prof}' is not a profile");
        }
    }

    let raw_tiers = entry
        .tiers
        .ok_or_else(|| anyhow!("harness route '{alias}': strategy = \"jev\" requires tiers"))?;
    if !(2..=MAX_TIERS).contains(&raw_tiers.len()) {
        bail!(
            "harness route '{alias}': strategy = \"jev\" needs 2 to {MAX_TIERS} tiers, got {}",
            raw_tiers.len()
        );
    }

    if let Some(dup) = raw_tiers.iter().enumerate().find_map(|(i, t)| {
        raw_tiers[..i]
            .iter()
            .any(|prev| prev.name == t.name)
            .then_some(t.name.as_str())
    }) {
        bail!("harness route '{alias}': duplicate tier name '{dup}'");
    }

    let any_typesafe_base = bases
        .values()
        .any(|b| b.wire.contains(&WireProtocol::Typesafe));
    let tiers: Vec<HarnessTier> = raw_tiers
        .into_iter()
        .map(|t| {
            validate_printable_name(alias, "tier", &t.name)?;
            if t.description.trim().is_empty() {
                bail!(
                    "harness route '{alias}': tier '{}' needs a description",
                    t.name
                );
            }
            if t.max_step == Some(0) {
                bail!(
                    "harness route '{alias}': tier '{}' max_step must be > 0",
                    t.name
                );
            }
            if t.timeout_seconds == Some(0) {
                bail!(
                    "harness route '{alias}': tier '{}' timeout_seconds must be > 0",
                    t.name
                );
            }
            if let Some(ref tb) = t.bases {
                if tb.is_empty() {
                    bail!(
                        "harness route '{alias}': tier '{}' bases override cannot be empty",
                        t.name
                    );
                }
                tb.iter().try_for_each(|b| {
                    bases.contains_key(b).then_some(()).ok_or_else(|| {
                        anyhow!(
                            "harness route '{alias}': tier '{}' references unknown base '{b}'",
                            t.name
                        )
                    })
                })?;
            }
            if t.legs.is_empty() {
                bail!("harness route '{alias}': tier '{}' has no legs", t.name);
            }
            let legs = expand_targets(alias, &t.legs, routes, any_typesafe_base)?;
            Ok(HarnessTier {
                name: t.name,
                description: t.description,
                effort: t.effort,
                max_step: t.max_step,
                timeout_seconds: t.timeout_seconds,
                bases: t.bases,
                legs,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    if !tiers.iter().any(|t| t.name == router.default_tier) {
        bail!(
            "harness route '{alias}': default_tier '{}' is not a tier",
            router.default_tier
        );
    }

    profiles.iter().try_for_each(|p| {
        let has_cell = p.bases.iter().any(|b| {
            let spec = &bases[b];
            tiers.iter().any(|t| {
                t.bases
                    .as_ref()
                    .is_none_or(|allowed| allowed.contains(b))
                    && t.legs
                        .iter()
                        .any(|l| preferred_wire(&spec.wire, &l.provider, &l.model).is_some())
            })
        });
        match has_cell {
            true => Ok(()),
            false => bail!(
                "harness route '{alias}': profile '{}' has no wire-compatible (base, leg) cells across any tier",
                p.name
            ),
        }
    })?;

    Ok(HarnessJevRoute {
        policy: entry.policy,
        router,
        profiles,
        tiers,
    })
}

fn flatten_jev_to_static(
    route: &HarnessJevRoute,
    bases: &BTreeMap<String, BaseSpec>,
) -> Vec<StaticHarnessLeg> {
    let candidate_bases: Vec<String> = match route.profiles.is_empty() {
        true => bases.keys().cloned().collect(),
        false => {
            let def = route.default_profile_index();
            std::iter::once(def)
                .chain((0..route.profiles.len()).filter(move |&i| i != def))
                .flat_map(|i| route.profiles[i].bases.iter().cloned())
                .collect()
        }
    };
    let tier_order = escalation_order(route.tiers.len(), route.default_tier_index());
    let raw: Vec<StaticHarnessLeg> = candidate_bases
        .into_iter()
        .filter_map(|base| bases.get(&base).map(|spec| (base, spec)))
        .flat_map(|(base, spec)| {
            tier_order
                .iter()
                .copied()
                .filter(|&ti| {
                    route.tiers[ti]
                        .bases
                        .as_ref()
                        .is_none_or(|allowed| allowed.contains(&base))
                })
                .flat_map(|ti| {
                    let effort = Some(route.tiers[ti].effort);
                    let base_name = base.clone();
                    route.tiers[ti].legs.iter().filter_map(move |l| {
                        preferred_wire(&spec.wire, &l.provider, &l.model).map(|wire| {
                            StaticHarnessLeg {
                                base: base_name.clone(),
                                leg: ChainLeg {
                                    effort,
                                    ..l.clone()
                                },
                                wire,
                            }
                        })
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect();

    raw.iter()
        .enumerate()
        .filter(|(idx, item)| {
            !raw[..*idx].iter().any(|prev| {
                prev.base == item.base
                    && prev.leg.provider == item.leg.provider
                    && prev.leg.model == item.leg.model
            })
        })
        .map(|(_, item)| item.clone())
        .collect()
}

fn prune_jev_route(
    alias: &str,
    route: &HarnessJevRoute,
    bases: &BTreeMap<String, BaseSpec>,
    drop_providers: &HashSet<String>,
) -> Option<HarnessJevRoute> {
    if bases.is_empty() {
        tracing::warn!(route = %alias, "no harness bases survive; downgrading 2D jev route to static");
        return None;
    }
    let surviving_profiles: Vec<HarnessProfile> = route
        .profiles
        .iter()
        .filter_map(|p| {
            let kept_bases: Vec<String> = p
                .bases
                .iter()
                .filter(|b| bases.contains_key(*b))
                .cloned()
                .collect();
            (!kept_bases.is_empty()).then(|| HarnessProfile {
                bases: kept_bases,
                ..p.clone()
            })
        })
        .collect();

    let (profiles, default_profile) = match surviving_profiles.len() {
        0 if !route.profiles.is_empty() => {
            tracing::warn!(
                route = %alias,
                "no profiles have surviving bases; downgrading 2D jev route to static"
            );
            return None;
        }
        1 => {
            tracing::warn!(
                route = %alias,
                surviving = %surviving_profiles[0].name,
                "only 1 profile survived lenient pruning; collapsing profile axis to 1D"
            );
            (Vec::new(), None)
        }
        _ => {
            let def = route
                .router
                .default_profile
                .as_deref()
                .filter(|d| surviving_profiles.iter().any(|p| p.name == *d))
                .map(str::to_string)
                .or_else(|| {
                    surviving_profiles.first().map(|p| p.name.clone()).tap(|f| {
                        if let Some(ref name) = f {
                            tracing::warn!(
                                route = %alias,
                                new_default = %name,
                                "default_profile had no surviving bases; re-picked first surviving profile"
                            );
                        }
                    })
                });
            (surviving_profiles, def)
        }
    };

    let active_bases: HashSet<&String> = match profiles.is_empty() {
        true => bases.keys().collect(),
        false => profiles.iter().flat_map(|p| p.bases.iter()).collect(),
    };

    let old_default_tier = route.default_tier_index();
    let kept_tiers: Vec<(usize, HarnessTier)> = route
        .tiers
        .iter()
        .enumerate()
        .filter_map(|(idx, t)| {
            let tier_bases = t.bases.as_ref().map(|tb| {
                tb.iter()
                    .filter(|b| bases.contains_key(*b))
                    .cloned()
                    .collect::<Vec<_>>()
            });
            if tier_bases.as_ref().is_some_and(Vec::is_empty) {
                return None;
            }
            let legs: Vec<ChainLeg> = t
                .legs
                .iter()
                .filter(|l| !drop_providers.contains(&l.provider))
                .filter(|l| {
                    active_bases.iter().any(|b| {
                        tier_bases.as_ref().is_none_or(|tb| tb.contains(b))
                            && preferred_wire(&bases[*b].wire, &l.provider, &l.model).is_some()
                    })
                })
                .cloned()
                .collect();
            (!legs.is_empty()).then(|| {
                (
                    idx,
                    HarnessTier {
                        bases: tier_bases,
                        legs,
                        ..t.clone()
                    },
                )
            })
        })
        .collect();

    if kept_tiers.len() < 2 {
        tracing::warn!(
            route = %alias,
            surviving = kept_tiers.len(),
            "fewer than 2 tiers survive lenient pruning; downgrading 2D jev route to static"
        );
        return None;
    }

    let default_tier = kept_tiers
        .iter()
        .find(|(i, _)| *i >= old_default_tier)
        .or_else(|| kept_tiers.last())
        .map(|(_, t)| t.name.clone())
        .unwrap_or_else(|| route.router.default_tier.clone());

    Some(HarnessJevRoute {
        policy: route.policy.clone(),
        router: HarnessJevRouterConfig {
            default_profile,
            default_tier,
            ..route.router.clone()
        },
        profiles,
        tiers: kept_tiers.into_iter().map(|(_, t)| t).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::effort::Effort;
    use crate::routing::table::RouteTable;

    const ROUTES_TOML: &str = r#"
[routes."lite"]
legs = [{ provider = "vertex", model = "gemini-3.5-flash-lite" }]

[routes."conversation"]
legs = [
    { provider = "anthropic", model = "claude-sonnet-4.6" },
    { provider = "openai", model = "gpt-5.4" },
]

[routes."deep"]
legs = [{ provider = "openai", model = "gpt-5.3-codex" }]
"#;

    const BASES_TOML: &str = r#"
[bases."pi"]
label = "Pi"
wire = ["openai_chat", "openai_responses", "anthropic_messages"]
skills = true
environments = true
mcp_transports = ["http", "stdio"]
tool_enforcement = "hard"

[bases."claude-code"]
label = "Claude Code"
wire = ["anthropic_messages", "vertex_anthropic", "bedrock"]
skills = true
environments = true
mcp_transports = ["http", "sse", "stdio"]
tool_enforcement = "hard"

[bases."codex"]
label = "Codex"
wire = ["openai_responses"]
skills = true
environments = true
mcp_transports = ["http", "stdio"]
tool_enforcement = "instruction"

[bases."systemone"]
label = "System One"
wire = ["typesafe"]
skills = false
environments = false
mcp_transports = []
tool_enforcement = "instruction"
"#;

    fn sample_harnesses_toml() -> String {
        format!(
            r#"{BASES_TOML}
[harnesses."chrn_default"]
name = "Workspace Auto Agent"
base = "auto"
route = "auto"
system_prompt = "Be concise."

[harness_routes."auto"]
policy = "default"
strategy = "jev"

[harness_routes."auto".jev_router]
default_profile = "repo_engineering"
default_tier = "moderate"
horizon_threshold = 0.75

[harness_routes."auto".profiles.quick_script]
description = "Focused file edits and narrow CLI commands"
bases = ["pi", "claude-code"]

[harness_routes."auto".profiles.repo_engineering]
description = "Multi-file software engineering and debugging"
bases = ["claude-code", "codex", "pi"]

[[harness_routes."auto".tiers]]
name = "trivial"
description = "Single-step lookup or small config tweak"
effort = "low"
max_step = 10
timeout_seconds = 120
legs = [
    {{ route = "lite" }},
    {{ provider = "anthropic", model = "claude-haiku-5.5" }},
]

[[harness_routes."auto".tiers]]
name = "moderate"
description = "Standard feature implementation or unit tests"
effort = "medium"
max_step = 35
timeout_seconds = 600
legs = [
    {{ route = "conversation" }},
]

[[harness_routes."auto".tiers]]
name = "hard"
description = "Multi-component refactoring or complex bugs"
effort = "high"
max_step = 80
timeout_seconds = 1800
bases = ["codex", "claude-code"]
legs = [
    {{ route = "deep" }},
    {{ provider = "anthropic", model = "claude-opus-5" }},
]
"#
        )
    }

    fn routes() -> RouteTable {
        RouteTable::from_toml_str(ROUTES_TOML).unwrap()
    }

    #[test]
    fn parses_2d_harness_route_and_expands_route_aliases() {
        let table = HarnessRouteTable::from_toml_str(&sample_harnesses_toml(), &routes()).unwrap();
        assert_eq!(table.bases().len(), 4);
        assert_eq!(
            table.base("claude-code").unwrap().tool_enforcement,
            ToolEnforcement::Hard
        );
        let h = table.harness("chrn_default").unwrap();
        assert_eq!(h.base, "auto");
        assert_eq!(h.route.as_deref(), Some("auto"));

        let jev = table.jev_route("auto").unwrap();
        assert_eq!(jev.profiles.len(), 2);
        assert_eq!(jev.tiers.len(), 3);
        assert_eq!(jev.tiers[0].effort, Effort::Low);
        assert_eq!(jev.tiers[0].max_step, Some(10));
        assert_eq!(jev.tiers[0].legs.len(), 2);
        assert_eq!(jev.tiers[0].legs[0].model, "gemini-3.5-flash-lite");
        assert_eq!(jev.tiers[1].legs.len(), 2);
        assert_eq!(jev.router.model, "jev-latest");
        assert_eq!(jev.router.timeout_ms, 400);
        assert_eq!(jev.router.min_confidence, 0.5);
        assert_eq!(jev.router.min_profile_confidence, 0.5);
        assert_eq!(jev.router.reasoning_threshold, 0.7);
        assert_eq!(jev.router.horizon_threshold, Some(0.75));
        assert!(table.referenced_providers().contains("typesafe"));
        assert!(table.referenced_providers().contains("anthropic"));
    }

    #[test]
    fn preferred_wire_picks_compatible_protocol_for_base_and_leg() {
        let table = HarnessRouteTable::from_toml_str(&sample_harnesses_toml(), &routes()).unwrap();
        let claude_code = table.base("claude-code").unwrap();
        let codex = table.base("codex").unwrap();
        let pi = table.base("pi").unwrap();

        assert_eq!(
            preferred_wire(&claude_code.wire, "anthropic", "claude-sonnet-4.6"),
            Some(WireProtocol::AnthropicMessages)
        );
        assert_eq!(
            preferred_wire(&claude_code.wire, "vertex", "claude-sonnet-4-6"),
            Some(WireProtocol::VertexAnthropic)
        );
        assert_eq!(preferred_wire(&claude_code.wire, "openai", "gpt-5.4"), None);
        assert_eq!(
            preferred_wire(&codex.wire, "openai", "gpt-5.4"),
            Some(WireProtocol::OpenaiResponses)
        );
        assert_eq!(
            preferred_wire(&pi.wire, "openai", "gpt-5.4"),
            Some(WireProtocol::OpenaiResponses)
        );
        assert_eq!(
            preferred_wire(&pi.wire, "qwen", "qwen3-max"),
            Some(WireProtocol::OpenaiChat)
        );
    }

    #[test]
    fn rejects_profile_with_no_wire_compatible_legs_across_tiers() {
        let bad = format!(
            r#"{BASES_TOML}
[harness_routes."auto"]
strategy = "jev"
[harness_routes."auto".jev_router]
default_profile = "codex_only"
default_tier = "easy"

[harness_routes."auto".profiles.codex_only]
description = "OpenAI codex only"
bases = ["codex"]

[harness_routes."auto".profiles.claude_only]
description = "Anthropic claude only"
bases = ["claude-code"]

[[harness_routes."auto".tiers]]
name = "easy"
description = "Easy tier with only anthropic legs"
effort = "low"
legs = [{{ provider = "anthropic", model = "claude-haiku-5.5" }}]

[[harness_routes."auto".tiers]]
name = "hard"
description = "Hard tier with only anthropic legs"
effort = "high"
legs = [{{ provider = "anthropic", model = "claude-opus-5" }}]
"#
        );
        let err = HarnessRouteTable::from_toml_str(&bad, &routes())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("profile 'codex_only' has no wire-compatible (base, leg) cells"),
            "unexpected err: {err}"
        );
    }

    #[test]
    fn rejects_invalid_harness_route_configs() {
        let cases: Vec<(String, &str)> = vec![
            (
                format!(
                    r#"{BASES_TOML}
[harness_routes."auto"]
strategy = "jev"
[harness_routes."auto".jev_router]
default_tier = "easy"
[harness_routes."auto".profiles.only_one]
description = "Single profile"
bases = ["pi"]
[[harness_routes."auto".tiers]]
name = "easy"
description = "Easy"
effort = "low"
legs = [{{ provider = "openai", model = "gpt-5.4-mini" }}]
[[harness_routes."auto".tiers]]
name = "hard"
description = "Hard"
effort = "high"
legs = [{{ provider = "openai", model = "gpt-5.4" }}]
"#
                ),
                "needs 2 to 10 profiles",
            ),
            (
                format!(
                    r#"{BASES_TOML}
[harness_routes."auto"]
strategy = "jev"
[harness_routes."auto".jev_router]
default_profile = "missing"
default_tier = "easy"
[harness_routes."auto".profiles.a]
description = "Profile A"
bases = ["pi"]
[harness_routes."auto".profiles.b]
description = "Profile B"
bases = ["pi"]
[[harness_routes."auto".tiers]]
name = "easy"
description = "Easy"
effort = "low"
legs = [{{ provider = "openai", model = "gpt-5.4-mini" }}]
[[harness_routes."auto".tiers]]
name = "hard"
description = "Hard"
effort = "high"
legs = [{{ provider = "openai", model = "gpt-5.4" }}]
"#
                ),
                "default_profile 'missing' is not a profile",
            ),
            (
                format!(
                    r#"{BASES_TOML}
[harness_routes."auto"]
strategy = "jev"
[harness_routes."auto".jev_router]
default_tier = "easy"
[[harness_routes."auto".tiers]]
name = "easy"
description = "Easy"
effort = "low"
bases = ["unknown-base"]
legs = [{{ provider = "openai", model = "gpt-5.4-mini" }}]
[[harness_routes."auto".tiers]]
name = "hard"
description = "Hard"
effort = "high"
legs = [{{ provider = "openai", model = "gpt-5.4" }}]
"#
                ),
                "unknown base 'unknown-base'",
            ),
        ];
        cases.iter().for_each(|(toml, needle)| {
            let err = HarnessRouteTable::from_toml_str(toml, &routes())
                .unwrap_err()
                .to_string();
            assert!(err.contains(needle), "expected '{needle}' in: {err}");
        });
    }

    #[test]
    fn lenient_pruning_drops_unavailable_bases_and_collapses_single_profile_to_1d() {
        let table = HarnessRouteTable::from_toml_str(&sample_harnesses_toml(), &routes())
            .unwrap()
            .without_bases(&HashSet::from(["pi".to_string(), "codex".to_string()]));
        let jev = table.jev_route("auto").unwrap();
        assert_eq!(jev.profiles.len(), 2);
        assert!(jev.profiles.iter().all(|p| p.bases == vec!["claude-code"]));

        let single_surviving = table.without_bases(&HashSet::from(["claude-code".to_string()]));
        assert!(single_surviving.jev_route("auto").is_none());
    }

    #[test]
    fn lenient_pruning_collapses_profiles_to_1d_when_one_profile_survives() {
        let toml = format!(
            r#"{BASES_TOML}
[harness_routes."auto"]
strategy = "jev"
[harness_routes."auto".jev_router]
default_profile = "quick_script"
default_tier = "easy"

[harness_routes."auto".profiles.quick_script]
description = "Quick script on pi"
bases = ["pi"]

[harness_routes."auto".profiles.repo_eng]
description = "Repo engineering on claude-code"
bases = ["claude-code"]

[[harness_routes."auto".tiers]]
name = "easy"
description = "Easy"
effort = "low"
legs = [{{ provider = "anthropic", model = "claude-haiku-5.5" }}]

[[harness_routes."auto".tiers]]
name = "hard"
description = "Hard"
effort = "high"
legs = [{{ provider = "anthropic", model = "claude-opus-5" }}]
"#
        );
        let table = HarnessRouteTable::from_toml_str(&toml, &routes())
            .unwrap()
            .without_bases(&HashSet::from(["pi".to_string()]));
        let jev = table.jev_route("auto").unwrap();
        assert!(jev.profiles.is_empty());
        assert_eq!(jev.tiers.len(), 2);
    }

    #[test]
    fn lenient_pruning_without_typesafe_downgrades_jev_route_to_static() {
        let table = HarnessRouteTable::from_toml_str(&sample_harnesses_toml(), &routes())
            .unwrap()
            .without_providers(&HashSet::from(["typesafe".to_string()]));
        assert!(table.jev_route("auto").is_none());
        let static_legs = table.static_legs("auto").unwrap();
        assert!(!static_legs.is_empty());
        assert_eq!(static_legs[0].leg.effort, Some(Effort::Medium));
    }

    #[test]
    fn shipped_harnesses_toml_and_commented_example_parse() {
        let shipped_routes =
            RouteTable::from_toml_str(include_str!("../../config/routes.toml")).unwrap();
        let shipped = include_str!("../../config/harnesses.toml");
        let base_only = HarnessRouteTable::from_toml_str(shipped, &shipped_routes).unwrap();
        assert_eq!(base_only.bases().len(), 5);

        let (head, tail) = shipped
            .split_once("# [harnesses.\"chrn_default\"]")
            .unwrap();
        let uncommented_tail = format!("# [harnesses.\"chrn_default\"]{tail}")
            .lines()
            .map(|l| {
                l.strip_prefix("# ")
                    .or_else(|| l.strip_prefix('#'))
                    .unwrap_or(l)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let full_toml = format!("{head}\n{uncommented_tail}");
        let full = HarnessRouteTable::from_toml_str(&full_toml, &shipped_routes).unwrap();
        let jev = full.jev_route("auto").unwrap();
        assert_eq!(jev.profiles.len(), 3);
        assert_eq!(jev.tiers.len(), 4);
    }
}
