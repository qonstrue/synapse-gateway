//! Pure 2D (`harness × model`) Jev routing logic: capability pre-filtering,
//! UHP state construction, 2D question generation (`profile` choice +
//! `difficulty` score + `needs_reasoning` / `needs_deep_horizon` noul),
//! answer parsing, independent confidence selection, and base-outer / tier-inner
//! fallback grid ordering.
//! Spec: `docs/superpowers/specs/2026-10-31-jev-2d-harness-router-design.md`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tap::Pipe;

use crate::routing::harness_table::{
    preferred_wire, BaseSpec, ConfiguredHarness, HarnessJevRoute, HarnessProfile, HarnessTier,
    ToolEnforcement, WireProtocol,
};
use crate::routing::jev_router::{
    head_tail, score_to_tier, truncate, DecisionOutcome, EffortPolicy, RoutingMode,
    HISTORY_ENTRY_CAP, LATEST_CAP, STATE_BUDGET, SYSTEM_CAP,
};
use crate::routing::table::{escalation_order, ChainLeg};

/// Normalized UHP request attributes fed into [`build_harness_state`].
#[derive(Debug, Clone, Default)]
pub struct UhpStateInput {
    pub latest_user_message: String,
    pub recent_history: Vec<(String, String)>,
    pub instructions: Option<String>,
    pub has_images: bool,
    pub input_filenames: Vec<String>,
    pub has_environment: bool,
    pub mcp_server_count: usize,
    pub skill_count: usize,
    pub is_continuation: bool,
}

/// Bounded JSON `state` sent to `/v1/systemone` for a 2D harness route.
pub fn build_harness_state(req: &UhpStateInput, harness: &ConfiguredHarness) -> Value {
    let latest = head_tail(&req.latest_user_message, LATEST_CAP);
    let combined_system = match (
        harness.system_prompt.trim().is_empty(),
        req.instructions
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    ) {
        (true, None) => None,
        (false, None) => Some(truncate(&harness.system_prompt, SYSTEM_CAP)),
        (true, Some(inst)) => Some(truncate(inst, SYSTEM_CAP)),
        (false, Some(inst)) => Some(truncate(
            &format!("{}\n{inst}", harness.system_prompt.trim()),
            SYSTEM_CAP,
        )),
    };
    let extensions: Vec<String> = req
        .input_filenames
        .iter()
        .filter_map(|name| {
            Path::new(name)
                .extension()
                .and_then(|e| e.to_str())
                .map(|ext| format!(".{}", ext.to_ascii_lowercase()))
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(12)
        .collect();

    let fixed_overhead = latest.len() + combined_system.as_ref().map_or(0, String::len) + 512;
    let history_budget = STATE_BUDGET.saturating_sub(fixed_overhead);
    let history: Vec<Value> = req
        .recent_history
        .iter()
        .rev()
        .scan(0usize, |used, (role, content)| {
            let trimmed = truncate(content, HISTORY_ENTRY_CAP);
            let cost = trimmed.len() + role.len() + 24;
            (*used + cost <= history_budget).then(|| {
                *used += cost;
                json!({ "role": role, "content": trimmed })
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    json!({
        "latest_user_message": latest,
        "recent_history": history,
        "has_images": req.has_images,
        "has_input_files": !req.input_filenames.is_empty(),
        "input_file_extensions": extensions,
        "has_environment": req.has_environment,
        "mcp_server_count": req.mcp_server_count,
        "skill_count": req.skill_count,
        "is_continuation": req.is_continuation,
    })
    .pipe(|mut v| {
        if let Some(sys) = combined_system {
            v["system_prompt"] = Value::String(sys);
        }
        v
    })
}

/// Build the Jev `/v1/systemone` `questions` map for a 2D harness route.
/// Omits `profile` when `!ask_profile` or `profiles.len() < 2`; omits
/// `needs_deep_horizon` when `!ask_horizon`.
pub fn build_harness_questions(
    profiles: &[HarnessProfile],
    tiers: &[HarnessTier],
    ask_profile: bool,
    ask_horizon: bool,
) -> Map<String, Value> {
    let tier_criteria: Vec<Value> = tiers
        .iter()
        .map(|t| Value::String(t.description.clone()))
        .collect();

    let profile_entry = (ask_profile && profiles.len() >= 2).then(|| {
        let profile_criteria: Map<String, Value> = profiles
            .iter()
            .map(|p| (p.name.clone(), Value::String(p.description.clone())))
            .collect();
        (
            "profile".to_string(),
            json!({
                "type": "choice",
                "instructions": "Which work archetype best matches what is needed to complete `latest_user_message`, given `system_prompt` and the attached workspace context?",
                "criteria": profile_criteria,
            }),
        )
    });

    let horizon_entry = ask_horizon.then(|| {
        (
            "needs_deep_horizon".to_string(),
            json!({
                "type": "noul",
                "instructions": "Will completing `latest_user_message` require many iterative tool-call rounds (such as exploring many files, running multiple test-fix cycles, or gathering many sources)?",
            }),
        )
    });

    profile_entry
        .into_iter()
        .chain([
            (
                "difficulty".to_string(),
                json!({
                    "type": "score",
                    "instructions": "How demanding is it to complete `latest_user_message` end-to-end, given `recent_history` and `system_prompt`?",
                    "criteria": tier_criteria,
                }),
            ),
            (
                "needs_reasoning".to_string(),
                json!({
                    "type": "noul",
                    "instructions": "Does completing `latest_user_message` require careful step-by-step reasoning such as maths, logic, architectural planning, or subtle debugging?",
                }),
            ),
        ])
        .chain(horizon_entry)
        .collect()
}

/// Capability constraints extracted from a UHP request before calling Jev.
#[derive(Debug, Clone, Default)]
pub struct TaskRequirements {
    pub pinned_base: Option<String>,
    pub client_model: Option<String>,
    pub needs_vertex_native: bool,
    pub needs_vision: bool,
    pub needs_skills: bool,
    pub needs_environment: bool,
    pub mcp_transports: HashSet<String>,
    pub needs_hard_tool_block: bool,
}

/// Surviving bases, profiles, and tiers after [`eligible_grid`] pre-filtering.
#[derive(Debug, Clone)]
pub struct EligibleGrid {
    pub bases: BTreeMap<String, BaseSpec>,
    /// Profiles with `>= 1` surviving `(base, leg)` cell, for Jev's `choice` question.
    pub active_profiles: Vec<HarnessProfile>,
    /// Full-length profile list (empty `bases` where pruned) so indices match `route.profiles`.
    pub profiles: Vec<HarnessProfile>,
    /// Full-length tier list (empty `legs` where pruned) so indices match `route.tiers`.
    pub tiers: Vec<HarnessTier>,
}

/// Known text-only models that cannot accept `input_image` parts.
pub fn is_text_only_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.starts_with("deepseek-chat")
        || m.starts_with("deepseek-reasoner")
        || m.starts_with("deepseek-r1")
        || m.starts_with("qwen3-coder")
        || m.starts_with("jev-")
}

/// Filter `route` and `bases_catalog` by `reqs`. Returns `None` if zero
/// `(base, leg)` cells survive across the entire 2D grid.
pub fn eligible_grid(
    route: &HarnessJevRoute,
    bases_catalog: &BTreeMap<String, BaseSpec>,
    reqs: &TaskRequirements,
) -> Option<EligibleGrid> {
    let bases: BTreeMap<String, BaseSpec> = bases_catalog
        .iter()
        .filter(|(id, spec)| {
            reqs.pinned_base.as_deref().is_none_or(|p| p == *id)
                && (!reqs.needs_skills || spec.skills)
                && (!reqs.needs_environment || spec.environments)
                && reqs.mcp_transports.is_subset(&spec.mcp_transports)
                && (!reqs.needs_hard_tool_block || spec.tool_enforcement == ToolEnforcement::Hard)
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    if bases.is_empty() {
        return None;
    }

    let leg_compatible_with_base = |base_id: &str, leg: &ChainLeg| -> bool {
        bases.get(base_id).is_some_and(|spec| {
            (!reqs.needs_vertex_native || leg.provider == "vertex")
                && (!reqs.needs_vision || !is_text_only_model(&leg.model))
                && reqs.client_model.as_deref().is_none_or(|m| m == leg.model)
                && preferred_wire(&spec.wire, &leg.provider, &leg.model).is_some()
        })
    };

    let profiles: Vec<HarnessProfile> = route
        .profiles
        .iter()
        .map(|p| {
            let kept_bases: Vec<String> = p
                .bases
                .iter()
                .filter(|b| {
                    bases.contains_key(*b)
                        && route.tiers.iter().any(|t| {
                            t.bases.as_ref().is_none_or(|tb| tb.contains(*b))
                                && t.legs.iter().any(|l| leg_compatible_with_base(b, l))
                        })
                })
                .cloned()
                .collect();
            HarnessProfile {
                bases: kept_bases,
                ..p.clone()
            }
        })
        .collect();

    let candidate_bases: Vec<String> = match route.profiles.is_empty() {
        true => bases.keys().cloned().collect(),
        false => profiles
            .iter()
            .flat_map(|p| p.bases.iter().cloned())
            .collect(),
    };

    if candidate_bases.is_empty() {
        return None;
    }

    let tiers: Vec<HarnessTier> = route
        .tiers
        .iter()
        .map(|t| {
            let legs: Vec<ChainLeg> = t
                .legs
                .iter()
                .filter(|l| {
                    candidate_bases.iter().any(|b| {
                        t.bases.as_ref().is_none_or(|tb| tb.contains(b))
                            && leg_compatible_with_base(b, l)
                    })
                })
                .cloned()
                .collect();
            HarnessTier { legs, ..t.clone() }
        })
        .collect();

    tiers.iter().any(|t| !t.legs.is_empty()).then(|| {
        let active_profiles = profiles
            .iter()
            .filter(|p| !p.bases.is_empty())
            .cloned()
            .collect();
        EligibleGrid {
            bases,
            active_profiles,
            profiles,
            tiers,
        }
    })
}

/// Parsed answers from `/v1/systemone` for a 2D harness route.
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessAnswers {
    pub profile: Option<String>,
    pub profile_confidence: Option<f64>,
    pub difficulty: f64,
    pub difficulty_confidence: f64,
    pub needs_reasoning: Option<f64>,
    pub needs_deep_horizon: Option<f64>,
}

pub fn parse_harness_answers(answers: &Value) -> Option<HarnessAnswers> {
    let diff = &answers["difficulty"];
    let difficulty = diff["score"]
        .as_f64()
        .filter(|x| x.is_finite() && *x >= 0.0)?;
    let difficulty_confidence = diff["confidence"]
        .as_f64()
        .filter(|x| x.is_finite() && (0.0..=1.0).contains(x))?;

    let profile_obj = &answers["profile"];
    let profile = profile_obj["choice"]
        .as_str()
        .or_else(|| profile_obj["answer"].as_str())
        .map(str::to_string);
    let profile_confidence = profile_obj["confidence"]
        .as_f64()
        .filter(|x| x.is_finite() && (0.0..=1.0).contains(x));

    let needs_reasoning = answers["needs_reasoning"]["noul"]
        .as_f64()
        .filter(|x| x.is_finite() && (0.0..=1.0).contains(x));
    let needs_deep_horizon = answers["needs_deep_horizon"]["noul"]
        .as_f64()
        .filter(|x| x.is_finite() && (0.0..=1.0).contains(x));

    Some(HarnessAnswers {
        profile,
        profile_confidence,
        difficulty,
        difficulty_confidence,
        needs_reasoning,
        needs_deep_horizon,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection2d {
    pub profile: Option<usize>,
    pub tier: usize,
    pub outcome: DecisionOutcome,
    pub profile_outcome: Option<DecisionOutcome>,
    pub bump_effort: bool,
    pub bump_steps: bool,
}

/// Turn `result` into a 2D `(profile, tier)` selection with independent
/// confidence checks on each axis.
pub fn select_2d(
    result: Result<HarnessAnswers, DecisionOutcome>,
    route: &HarnessJevRoute,
    ask_profile: bool,
) -> Selection2d {
    let has_profiles = ask_profile && !route.profiles.is_empty();
    match result {
        Err(outcome) => Selection2d {
            profile: has_profiles.then(|| route.default_profile_index()),
            tier: route.default_tier_index(),
            outcome,
            profile_outcome: has_profiles.then_some(outcome),
            bump_effort: false,
            bump_steps: false,
        },
        Ok(a) => {
            let (tier, outcome) = match a.difficulty_confidence >= route.router.min_confidence {
                true => (
                    score_to_tier(a.difficulty, route.tiers.len()),
                    DecisionOutcome::Decided,
                ),
                false => (route.default_tier_index(), DecisionOutcome::LowConfidence),
            };
            let (profile, profile_outcome) = match has_profiles {
                false => (None, None),
                true => {
                    let matched = a
                        .profile
                        .as_deref()
                        .and_then(|name| route.profiles.iter().position(|p| p.name == name));
                    let conf_ok = a
                        .profile_confidence
                        .is_some_and(|c| c >= route.router.min_profile_confidence);
                    match (matched, conf_ok) {
                        (Some(idx), true) => (Some(idx), Some(DecisionOutcome::Decided)),
                        _ => (
                            Some(route.default_profile_index()),
                            Some(DecisionOutcome::LowConfidence),
                        ),
                    }
                }
            };
            let bump_effort = a
                .needs_reasoning
                .is_some_and(|p| p >= route.router.reasoning_threshold);
            let bump_steps = route
                .router
                .horizon_threshold
                .zip(a.needs_deep_horizon)
                .is_some_and(|(th, p)| p >= th);
            Selection2d {
                profile,
                tier,
                outcome,
                profile_outcome,
                bump_effort,
                bump_steps,
            }
        }
    }
}

/// One executable `(base, model)` cell in the planned fallback sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedHarnessLeg {
    pub profile: Option<usize>,
    pub base: String,
    pub tier: usize,
    pub leg: ChainLeg,
    pub wire: WireProtocol,
    pub max_step: Option<u32>,
    pub timeout_seconds: Option<u32>,
}

/// Order compatible `(base, leg)` cells with `base` as the outer loop and
/// `escalation_order(tiers.len(), start_tier)` as the inner loop, de-duplicating
/// `(base, tier, provider, model)` across profiles.
pub fn order_grid(
    grid: &EligibleGrid,
    route: &HarnessJevRoute,
    chosen_profile: Option<usize>,
    start_tier: usize,
    effort_policy: EffortPolicy,
    bump_steps: bool,
    client_max_step: Option<u32>,
) -> Vec<PlannedHarnessLeg> {
    let tier_order = escalation_order(grid.tiers.len(), start_tier);
    let last_tier = grid.tiers.len().saturating_sub(1);

    let profile_bases: Vec<(Option<usize>, String)> = match grid.profiles.is_empty() {
        true => grid.bases.keys().map(|b| (None, b.clone())).collect(),
        false => {
            let primary = chosen_profile.unwrap_or_else(|| route.default_profile_index());
            let default_prof = route.default_profile_index();
            std::iter::once(primary)
                .chain((default_prof != primary).then_some(default_prof))
                .chain((0..grid.profiles.len()).filter(move |&i| i != primary && i != default_prof))
                .flat_map(|pi| {
                    grid.profiles[pi]
                        .bases
                        .iter()
                        .map(move |b| (Some(pi), b.clone()))
                })
                .collect()
        }
    };

    let raw: Vec<PlannedHarnessLeg> = profile_bases
        .into_iter()
        .filter_map(|(prof_idx, base)| grid.bases.get(&base).map(|spec| (prof_idx, base, spec)))
        .flat_map(|(prof_idx, base, spec)| {
            tier_order
                .iter()
                .copied()
                .filter(|&ti| {
                    grid.tiers[ti]
                        .bases
                        .as_ref()
                        .is_none_or(|allowed| allowed.contains(&base))
                })
                .flat_map(|ti| {
                    let tier = &grid.tiers[ti];
                    let effort = match effort_policy {
                        EffortPolicy::Client => None,
                        EffortPolicy::Tier { bump: true } => Some(tier.effort.bump()),
                        EffortPolicy::Tier { bump: false } => Some(tier.effort),
                    };
                    let max_step = client_max_step.or_else(|| match bump_steps {
                        true => grid.tiers[(ti + 1).min(last_tier)]
                            .max_step
                            .or(tier.max_step),
                        false => tier.max_step,
                    });
                    let timeout_seconds = tier.timeout_seconds;
                    let base_name = base.clone();
                    tier.legs.iter().filter_map(move |l| {
                        preferred_wire(&spec.wire, &l.provider, &l.model).map(|wire| {
                            PlannedHarnessLeg {
                                profile: prof_idx,
                                base: base_name.clone(),
                                tier: ti,
                                leg: ChainLeg {
                                    effort,
                                    ..l.clone()
                                },
                                wire,
                                max_step,
                                timeout_seconds,
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
                    && prev.tier == item.tier
                    && prev.leg.provider == item.leg.provider
                    && prev.leg.model == item.leg.model
            })
        })
        .map(|(_, item)| item.clone())
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct HarnessRoutePlan {
    pub mode: RoutingMode,
    pub legs: Vec<PlannedHarnessLeg>,
    pub profile_names: Vec<String>,
    pub tier_names: Vec<String>,
    pub decided_profile: Option<usize>,
    pub decided_tier: Option<usize>,
    pub outcome: Option<DecisionOutcome>,
    pub profile_outcome: Option<DecisionOutcome>,
    pub client_effort: bool,
    pub step_bumped: bool,
    pub guard: Option<&'static str>,
}

impl HarnessRoutePlan {
    /// Build a client-facing [`HarnessRoutingReport`] once `(base, provider, model)`
    /// has been selected/served.
    pub fn report_for(&self, served: Option<(&str, &str, &str)>) -> HarnessRoutingReport {
        let matched = served
            .and_then(|(base, provider, model)| {
                self.legs
                    .iter()
                    .find(|p| p.base == base && p.leg.provider == provider && p.leg.model == model)
            })
            .or_else(|| self.legs.first());

        let base = matched.map(|p| p.base.clone());
        let profile = matched
            .and_then(|p| p.profile)
            .and_then(|idx| self.profile_names.get(idx))
            .cloned();
        let decided_prof_name = self
            .decided_profile
            .and_then(|idx| self.profile_names.get(idx))
            .cloned();
        let profile_decided =
            decided_prof_name.filter(|d| profile.as_ref().is_some_and(|p| p != d));

        let tier = matched.and_then(|p| self.tier_names.get(p.tier)).cloned();
        let decided_tier_name = self
            .decided_tier
            .and_then(|idx| self.tier_names.get(idx))
            .cloned();
        let tier_decided = decided_tier_name.filter(|d| tier.as_ref().is_some_and(|t| t != d));

        let effort = match (self.tier_names.is_empty(), self.client_effort) {
            (true, _) => None,
            (false, true) => Some("client".to_string()),
            (false, false) => matched
                .and_then(|p| p.leg.effort)
                .map(|e| e.as_str().to_string()),
        };

        let max_step = matched.and_then(|p| p.max_step);

        let degraded = self
            .outcome
            .and_then(DecisionOutcome::degraded_reason)
            .or_else(|| match self.profile_outcome {
                Some(DecisionOutcome::LowConfidence) => Some("low_profile_confidence"),
                Some(other) => other.degraded_reason(),
                None => None,
            });

        HarnessRoutingReport {
            mode: self.mode,
            harness_base: base,
            profile,
            profile_decided,
            tier,
            tier_decided,
            effort,
            max_step,
            degraded,
            guard: self.guard,
        }
    }
}

/// Client-facing summary of how a UHP turn was routed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HarnessRoutingReport {
    pub mode: RoutingMode,
    pub harness_base: Option<String>,
    pub profile: Option<String>,
    pub profile_decided: Option<String>,
    pub tier: Option<String>,
    pub tier_decided: Option<String>,
    pub effort: Option<String>,
    pub max_step: Option<u32>,
    pub degraded: Option<&'static str>,
    pub guard: Option<&'static str>,
}

impl HarnessRoutingReport {
    /// `x-synapse-*` response headers in deterministic order.
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        std::iter::once(("x-synapse-routing", self.mode.as_str().to_string()))
            .chain(
                self.harness_base
                    .clone()
                    .map(|b| ("x-synapse-harness-base", b)),
            )
            .chain(self.profile.clone().map(|p| ("x-synapse-profile", p)))
            .chain(
                self.profile_decided
                    .clone()
                    .map(|p| ("x-synapse-profile-decided", p)),
            )
            .chain(self.tier.clone().map(|t| ("x-synapse-tier", t)))
            .chain(
                self.tier_decided
                    .clone()
                    .map(|t| ("x-synapse-tier-decided", t)),
            )
            .chain(
                self.effort
                    .clone()
                    .map(|e| ("x-synapse-reasoning-effort", e)),
            )
            .chain(self.max_step.map(|s| ("x-synapse-max-step", s.to_string())))
            .chain(
                self.degraded
                    .map(|d| ("x-synapse-routing-degraded", d.to_string())),
            )
            .chain(self.guard.map(|g| ("x-synapse-guard", g.to_string())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::effort::Effort;
    use crate::routing::harness_table::HarnessJevRouterConfig;
    use serde_json::json;

    fn sample_harness() -> ConfiguredHarness {
        ConfiguredHarness {
            name: "Workspace Auto Agent".into(),
            base: "auto".into(),
            route: Some("auto".into()),
            system_prompt: "Always run cargo test before finishing.".into(),
            disabled_tools: vec![],
        }
    }

    fn sample_profiles() -> Vec<HarnessProfile> {
        vec![
            HarnessProfile {
                name: "quick_script".into(),
                description: "Focused file edits and CLI one-liners".into(),
                bases: vec!["pi".into()],
            },
            HarnessProfile {
                name: "repo_engineering".into(),
                description: "Multi-file software engineering and debugging".into(),
                bases: vec!["claude-code".into(), "codex".into()],
            },
        ]
    }

    fn sample_tiers() -> Vec<HarnessTier> {
        vec![
            HarnessTier {
                name: "trivial".into(),
                description: "Single-step lookup or typo fix".into(),
                effort: Effort::Low,
                max_step: Some(10),
                timeout_seconds: Some(120),
                bases: None,
                legs: vec![ChainLeg {
                    provider: "anthropic".into(),
                    model: "claude-haiku-5.5".into(),
                    ..Default::default()
                }],
            },
            HarnessTier {
                name: "hard".into(),
                description: "Multi-component refactoring".into(),
                effort: Effort::High,
                max_step: Some(80),
                timeout_seconds: Some(1800),
                bases: None,
                legs: vec![ChainLeg {
                    provider: "anthropic".into(),
                    model: "claude-opus-5".into(),
                    ..Default::default()
                }],
            },
        ]
    }

    fn test_bases() -> BTreeMap<String, BaseSpec> {
        BTreeMap::from([
            (
                "pi".into(),
                BaseSpec {
                    label: "Pi".into(),
                    wire: HashSet::from([
                        WireProtocol::OpenaiResponses,
                        WireProtocol::AnthropicMessages,
                    ]),
                    skills: true,
                    environments: true,
                    mcp_transports: HashSet::from(["http".into(), "stdio".into()]),
                    tool_enforcement: ToolEnforcement::Hard,
                },
            ),
            (
                "claude-code".into(),
                BaseSpec {
                    label: "Claude Code".into(),
                    wire: HashSet::from([WireProtocol::AnthropicMessages]),
                    skills: true,
                    environments: true,
                    mcp_transports: HashSet::from(["http".into(), "sse".into(), "stdio".into()]),
                    tool_enforcement: ToolEnforcement::Hard,
                },
            ),
            (
                "codex".into(),
                BaseSpec {
                    label: "Codex".into(),
                    wire: HashSet::from([WireProtocol::OpenaiResponses]),
                    skills: true,
                    environments: true,
                    mcp_transports: HashSet::from(["http".into(), "stdio".into()]),
                    tool_enforcement: ToolEnforcement::Instruction,
                },
            ),
        ])
    }

    fn test_route() -> HarnessJevRoute {
        HarnessJevRoute {
            policy: Some("default".into()),
            router: HarnessJevRouterConfig {
                model: "jev-latest".into(),
                default_profile: Some("repo_engineering".into()),
                default_tier: "trivial".into(),
                timeout_ms: 400,
                min_confidence: 0.5,
                min_profile_confidence: 0.5,
                reasoning_threshold: 0.7,
                horizon_threshold: Some(0.75),
            },
            profiles: sample_profiles(),
            tiers: vec![
                HarnessTier {
                    name: "trivial".into(),
                    description: "Single-step lookup".into(),
                    effort: Effort::Low,
                    max_step: Some(10),
                    timeout_seconds: Some(120),
                    bases: None,
                    legs: vec![
                        ChainLeg {
                            provider: "anthropic".into(),
                            model: "claude-haiku-5.5".into(),
                            ..Default::default()
                        },
                        ChainLeg {
                            provider: "deepseek".into(),
                            model: "deepseek-reasoner".into(),
                            ..Default::default()
                        },
                    ],
                },
                HarnessTier {
                    name: "hard".into(),
                    description: "Multi-component refactoring".into(),
                    effort: Effort::High,
                    max_step: Some(80),
                    timeout_seconds: Some(1800),
                    bases: None,
                    legs: vec![
                        ChainLeg {
                            provider: "anthropic".into(),
                            model: "claude-opus-5".into(),
                            ..Default::default()
                        },
                        ChainLeg {
                            provider: "openai".into(),
                            model: "gpt-5.3-codex".into(),
                            ..Default::default()
                        },
                    ],
                },
            ],
        }
    }

    #[test]
    fn build_harness_state_extracts_files_images_and_metadata_within_budget() {
        let req = UhpStateInput {
            latest_user_message: "Refactor src/lib.rs and Cargo.toml".into(),
            recent_history: vec![("user".into(), "Initial setup".into())],
            instructions: Some("Keep public API stable.".into()),
            has_images: true,
            input_filenames: vec![
                "src/lib.rs".into(),
                "Cargo.toml".into(),
                "src/main.rs".into(),
            ],
            has_environment: true,
            mcp_server_count: 2,
            skill_count: 1,
            is_continuation: false,
        };
        let state = build_harness_state(&req, &sample_harness());
        assert_eq!(
            state["latest_user_message"],
            "Refactor src/lib.rs and Cargo.toml"
        );
        assert_eq!(
            state["system_prompt"],
            "Always run cargo test before finishing.\nKeep public API stable."
        );
        assert_eq!(state["has_images"], true);
        assert_eq!(state["has_input_files"], true);
        assert_eq!(state["input_file_extensions"], json!([".rs", ".toml"]));
        assert_eq!(state["has_environment"], true);
        assert_eq!(state["mcp_server_count"], 2);
        assert_eq!(state["skill_count"], 1);
        assert_eq!(state["is_continuation"], false);
        assert!(state.to_string().len() <= 24_000);
    }

    #[test]
    fn build_harness_questions_includes_profile_on_turn_1_and_omits_on_continuation() {
        let profiles = sample_profiles();
        let tiers = sample_tiers();

        let q_turn1 = build_harness_questions(&profiles, &tiers, true, true);
        assert_eq!(q_turn1["profile"]["type"], "choice");
        assert_eq!(
            q_turn1["profile"]["criteria"]["repo_engineering"],
            "Multi-file software engineering and debugging"
        );
        assert_eq!(q_turn1["difficulty"]["type"], "score");
        assert_eq!(
            q_turn1["difficulty"]["criteria"],
            json!([
                "Single-step lookup or typo fix",
                "Multi-component refactoring"
            ])
        );
        assert_eq!(q_turn1["needs_reasoning"]["type"], "noul");
        assert_eq!(q_turn1["needs_deep_horizon"]["type"], "noul");

        let q_turn2 = build_harness_questions(&profiles, &tiers, false, false);
        assert!(!q_turn2.contains_key("profile"));
        assert!(!q_turn2.contains_key("needs_deep_horizon"));
        assert!(q_turn2.contains_key("difficulty"));
        assert!(q_turn2.contains_key("needs_reasoning"));
    }

    #[test]
    fn eligible_grid_filters_by_hard_tool_block_mcp_transport_and_vision() {
        let route = test_route();
        let bases = test_bases();
        let reqs = TaskRequirements {
            pinned_base: None,
            client_model: None,
            needs_vertex_native: false,
            needs_vision: true,
            needs_skills: true,
            needs_environment: false,
            mcp_transports: HashSet::from(["sse".into()]),
            needs_hard_tool_block: true,
        };
        let grid = eligible_grid(&route, &bases, &reqs).expect("claude-code survives");
        assert_eq!(grid.active_profiles.len(), 1);
        assert_eq!(grid.active_profiles[0].name, "repo_engineering");
        assert_eq!(grid.active_profiles[0].bases, vec!["claude-code"]);
        assert_eq!(grid.tiers[0].legs.len(), 1);
        assert_eq!(grid.tiers[0].legs[0].model, "claude-haiku-5.5");
        assert_eq!(grid.tiers[1].legs.len(), 1);
        assert_eq!(grid.tiers[1].legs[0].model, "claude-opus-5");
    }

    #[test]
    fn parse_and_select_2d_gates_profile_and_difficulty_confidence_independently() {
        let route = test_route();
        let raw = json!({
            "profile": {
                "type": "choice",
                "choice": "quick_script",
                "probabilities": {"quick_script": 0.35, "repo_engineering": 0.65},
                "confidence": 0.35
            },
            "difficulty": {
                "type": "score",
                "score": 1.0,
                "confidence": 0.9
            },
            "needs_reasoning": {
                "type": "noul",
                "noul": 0.85
            },
            "needs_deep_horizon": {
                "type": "noul",
                "noul": 0.80
            }
        });
        let ans = parse_harness_answers(&raw).expect("valid answers");
        let sel = select_2d(Ok(ans), &route, true);
        assert_eq!(sel.tier, 1);
        assert_eq!(sel.outcome, DecisionOutcome::Decided);
        assert_eq!(sel.profile, Some(1));
        assert_eq!(sel.profile_outcome, Some(DecisionOutcome::LowConfidence));
        assert!(sel.bump_effort);
        assert!(sel.bump_steps);
    }

    #[test]
    fn order_grid_orders_base_outer_tier_inner_and_deduplicates_shared_bases() {
        let route = test_route();
        let bases = test_bases();
        let reqs = TaskRequirements::default();
        let grid = eligible_grid(&route, &bases, &reqs).unwrap();

        let planned = order_grid(
            &grid,
            &route,
            Some(1),
            0,
            EffortPolicy::Tier { bump: true },
            true,
            None,
        );

        let summary: Vec<_> = planned
            .iter()
            .map(|p| {
                (
                    p.base.as_str(),
                    p.tier,
                    p.leg.model.as_str(),
                    p.leg.effort,
                    p.max_step,
                )
            })
            .collect();

        assert_eq!(
            summary[0],
            (
                "claude-code",
                0,
                "claude-haiku-5.5",
                Some(Effort::Medium),
                Some(80)
            )
        );
        assert_eq!(
            summary[1],
            (
                "claude-code",
                1,
                "claude-opus-5",
                Some(Effort::Xhigh),
                Some(80)
            )
        );
        assert_eq!(
            summary[2],
            ("codex", 1, "gpt-5.3-codex", Some(Effort::Xhigh), Some(80))
        );
        assert_eq!(summary[3].0, "pi");
    }

    #[test]
    fn harness_route_plan_reports_headers_and_degraded_profile_confidence() {
        let route = test_route();
        let bases = test_bases();
        let grid = eligible_grid(&route, &bases, &TaskRequirements::default()).unwrap();
        let legs = order_grid(
            &grid,
            &route,
            Some(1),
            1,
            EffortPolicy::Tier { bump: false },
            false,
            None,
        );
        let plan = HarnessRoutePlan {
            mode: RoutingMode::Jev,
            legs,
            profile_names: route.profiles.iter().map(|p| p.name.clone()).collect(),
            tier_names: route.tiers.iter().map(|t| t.name.clone()).collect(),
            decided_profile: Some(1),
            decided_tier: Some(1),
            outcome: Some(DecisionOutcome::Decided),
            profile_outcome: Some(DecisionOutcome::LowConfidence),
            client_effort: false,
            step_bumped: false,
            guard: None,
        };

        let report = plan.report_for(Some(("claude-code", "anthropic", "claude-opus-5")));
        let headers = report.headers();
        assert!(headers.contains(&("x-synapse-routing", "jev".into())));
        assert!(headers.contains(&("x-synapse-harness-base", "claude-code".into())));
        assert!(headers.contains(&("x-synapse-profile", "repo_engineering".into())));
        assert!(headers.contains(&("x-synapse-tier", "hard".into())));
        assert!(headers.contains(&("x-synapse-reasoning-effort", "high".into())));
        assert!(headers.contains(&("x-synapse-max-step", "80".into())));
        assert!(headers.contains(&(
            "x-synapse-routing-degraded",
            "low_profile_confidence".into()
        )));
    }
}
