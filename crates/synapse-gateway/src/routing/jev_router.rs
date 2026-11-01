//! Jev router: pure decision logic for `strategy = "jev"` routes. Jev rates
//! each request's difficulty against the route's tiers; this module turns the
//! answers into an ordered, effort-stamped leg plan and a client-facing report.
//! Spec: `docs/superpowers/specs/2026-09-28-jev-router-design.md`.

use serde_json::{json, Map, Value};

use crate::routing::effort::Effort;
use crate::routing::request::{ChatRequest, Message};
use crate::routing::table::{escalation_order, ChainLeg, JevRoute, Tier};

/// Character budget for the whole Jev `state` (≈ 6k tokens).
pub const STATE_BUDGET: usize = 24_000;
/// Cap on `latest_user_message`; longer messages keep their head and tail.
pub const LATEST_CAP: usize = 16_000;
/// Cap on `system_prompt`.
pub const SYSTEM_CAP: usize = 2_000;
/// Cap on each `recent_history` entry.
pub const HISTORY_ENTRY_CAP: usize = 2_000;

/// How a request's legs were planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RoutingMode {
    /// Jev chose the tier.
    Jev,
    /// A plain `legs` route.
    #[default]
    Static,
    /// A `jev` route the client asked to serve without Jev.
    StaticOverride,
}

impl RoutingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jev => "jev",
            Self::Static => "static",
            Self::StaticOverride => "static-override",
        }
    }
}

/// The mode for one request: the route's strategy unless `routing_strategy`
/// overrides it. `Err` carries the 400 message.
pub fn resolve_mode(is_jev_route: bool, requested: Option<&str>) -> Result<RoutingMode, String> {
    match (is_jev_route, requested) {
        (true, None | Some("jev")) => Ok(RoutingMode::Jev),
        (true, Some("static")) => Ok(RoutingMode::StaticOverride),
        (false, None | Some("static")) => Ok(RoutingMode::Static),
        (false, Some("jev")) => Err("routing_strategy \"jev\" requires a route with tiers".into()),
        (_, Some(other)) => Err(format!(
            "unknown routing_strategy '{other}' (expected \"static\" or \"jev\")"
        )),
    }
}

/// The bounded `state` Jev rates: the latest user message, recent history,
/// system prompt, and tool/image flags. Never logged (customer data).
pub fn build_state(req: &ChatRequest) -> Value {
    let latest = req.messages.iter().rposition(|m| m.role == "user");
    let latest_text = latest
        .map(|i| head_tail(&message_text(&req.messages[i].content), LATEST_CAP))
        .unwrap_or_default();
    let system = req
        .messages
        .iter()
        .find(|m| m.role == "system")
        .map(|m| truncate(&message_text(&m.content), SYSTEM_CAP))
        .unwrap_or_default();
    let remaining =
        STATE_BUDGET.saturating_sub(latest_text.chars().count() + system.chars().count());
    json!({
        "latest_user_message": latest_text,
        "recent_history": recent_history(&req.messages, latest, remaining),
        "system_prompt": system,
        "has_tools": req.tools.as_ref().is_some_and(|t| !t.is_empty()),
        "has_images": has_images(req),
    })
}

/// Non-system messages with text, other than the latest user message, filled
/// newest-first until `budget` characters, returned in chronological order.
fn recent_history(messages: &[Message], latest: Option<usize>, budget: usize) -> Vec<Value> {
    messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(i, m)| m.role != "system" && Some(*i) != latest)
        .map(|(_, m)| {
            (
                m.role.as_str(),
                truncate(&message_text(&m.content), HISTORY_ENTRY_CAP),
            )
        })
        .filter(|(_, text)| !text.is_empty())
        .scan(0usize, |used, (role, text)| {
            *used += text.chars().count();
            (*used <= budget).then_some((role, text))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|(role, content)| json!({ "role": role, "content": content }))
        .collect()
}

fn message_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(part_text)
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn part_text(part: &Value) -> String {
    match part.get("type").and_then(Value::as_str) {
        Some("text") => part
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Some("image_url" | "image") => "[image]".into(),
        Some("input_audio" | "audio") => "[audio]".into(),
        Some("file") => "[file]".into(),
        _ => String::new(),
    }
}

fn is_image_part(part: &Value) -> bool {
    matches!(
        part.get("type").and_then(Value::as_str),
        Some("image_url" | "image")
    )
}

fn has_images(req: &ChatRequest) -> bool {
    req.messages.iter().any(|m| {
        m.content
            .as_array()
            .is_some_and(|parts| parts.iter().any(is_image_part))
    }) || req
        .vertex
        .as_ref()
        .and_then(|v| v.media_uris.as_ref())
        .is_some_and(|uris| !uris.is_empty())
}

pub(crate) fn truncate(s: &str, cap: usize) -> String {
    s.chars().take(cap).collect()
}

/// `s` unchanged when within `cap` characters; otherwise its first and last
/// `cap / 2` characters joined by an ellipsis line.
pub(crate) fn head_tail(s: &str, cap: usize) -> String {
    let n = s.chars().count();
    match n <= cap {
        true => s.to_string(),
        false => format!(
            "{}\n…\n{}",
            s.chars().take(cap / 2).collect::<String>(),
            s.chars().skip(n - cap / 2).collect::<String>()
        ),
    }
}

const DIFFICULTY_INSTRUCTIONS: &str = "How demanding is it to produce a high-quality reply to \
`latest_user_message`, given `recent_history` and `system_prompt`?";
const REASONING_INSTRUCTIONS: &str = "Does replying well to `latest_user_message` require careful \
step-by-step reasoning such as maths, logic, planning, or debugging?";

/// The two questions every decision asks. `difficulty`'s levels are the tier
/// descriptions, so its score indexes `tiers`.
pub fn build_questions(tiers: &[Tier]) -> Map<String, Value> {
    Map::from_iter([
        (
            "difficulty".to_string(),
            json!({
                "type": "score",
                "instructions": DIFFICULTY_INSTRUCTIONS,
                "criteria": tiers.iter().map(|t| t.description.as_str()).collect::<Vec<_>>(),
            }),
        ),
        (
            "needs_reasoning".to_string(),
            json!({
                "type": "noul",
                "instructions": REASONING_INSTRUCTIONS,
            }),
        ),
    ])
}

/// The parts of a Jev response the router acts on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Answers {
    /// Probability-weighted tier index.
    pub difficulty: f64,
    pub confidence: f64,
    pub needs_reasoning: Option<f64>,
}

/// `None` when `difficulty` is missing, not a score answer, or lacks its
/// score or confidence.
pub fn parse_answers(answers: &Value) -> Option<Answers> {
    answers
        .get("difficulty")
        .filter(|d| d.get("type").and_then(Value::as_str) == Some("score"))
        .and_then(|d| {
            Some(Answers {
                difficulty: d.get("score")?.as_f64()?,
                confidence: d.get("confidence")?.as_f64()?,
                needs_reasoning: answers
                    .get("needs_reasoning")
                    .and_then(|n| n.get("noul"))
                    .and_then(Value::as_f64),
            })
        })
}

/// How a decision was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionOutcome {
    Decided,
    LowConfidence,
    Timeout,
    /// Transport error, non-2xx, or an unparseable answer.
    Error,
    /// No Jev provider configured on this gateway.
    Unavailable,
    StaticOverride,
}

impl DecisionOutcome {
    /// `outcome` label of `synapse_routing_decisions_total`.
    pub fn metric_label(self) -> &'static str {
        match self {
            Self::Decided => "decided",
            Self::LowConfidence => "low_confidence",
            Self::Timeout => "timeout",
            Self::Error | Self::Unavailable => "error",
            Self::StaticOverride => "static_override",
        }
    }

    /// `x-synapse-routing-degraded` value when the decision fell back.
    pub fn degraded_reason(self) -> Option<&'static str> {
        match self {
            Self::Decided | Self::StaticOverride => None,
            Self::LowConfidence => Some("low_confidence"),
            Self::Timeout => Some("timeout"),
            Self::Error => Some("error"),
            Self::Unavailable => Some("jev_unavailable"),
        }
    }
}

/// The tier a decision picked (before eligibility), and whether effort bumps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub tier: usize,
    pub outcome: DecisionOutcome,
    pub bump: bool,
}

/// Nearest tier to a probability-weighted score (half rounds up), clamped.
/// `NaN` maps to tier 0.
pub fn score_to_tier(score: f64, tiers: usize) -> usize {
    (score + 0.5)
        .floor()
        .clamp(0.0, tiers.saturating_sub(1) as f64) as usize
}

/// Turn a decision result into a tier and effort bump.
pub fn select(result: Result<Answers, DecisionOutcome>, route: &JevRoute) -> Selection {
    let bump = |a: &Answers| {
        a.needs_reasoning
            .is_some_and(|p| p >= route.router.reasoning_threshold)
    };
    match result {
        Ok(a) if a.confidence < route.router.min_confidence => Selection {
            tier: route.default_index(),
            outcome: DecisionOutcome::LowConfidence,
            bump: bump(&a),
        },
        Ok(a) => Selection {
            tier: score_to_tier(a.difficulty, route.tiers.len()),
            outcome: DecisionOutcome::Decided,
            bump: bump(&a),
        },
        Err(outcome) => Selection {
            tier: route.default_index(),
            outcome,
            bump: false,
        },
    }
}

/// How planned legs get their reasoning effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortPolicy {
    /// Each tier's configured effort, one step harder when `bump`.
    Tier { bump: bool },
    /// The client set its own effort; the planner sets none.
    Client,
}

/// One leg of a plan and the index of the tier it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedLeg {
    pub leg: ChainLeg,
    pub tier: usize,
}

/// Legs in fallback order from tier `start` (see [`escalation_order`]), each
/// stamped with an effort per `policy`. Tiers with no legs contribute nothing.
pub fn order_legs(tiers: &[Tier], start: usize, policy: EffortPolicy) -> Vec<PlannedLeg> {
    escalation_order(tiers.len(), start)
        .into_iter()
        .flat_map(|i| {
            let effort = match policy {
                EffortPolicy::Client => None,
                EffortPolicy::Tier { bump: true } => Some(tiers[i].effort.bump()),
                EffortPolicy::Tier { bump: false } => Some(tiers[i].effort),
            };
            tiers[i].legs.iter().map(move |l| PlannedLeg {
                leg: ChainLeg {
                    effort,
                    ..l.clone()
                },
                tier: i,
            })
        })
        .collect()
}

/// `tiers` with legs the request cannot use removed (native-Vertex features
/// only run on `vertex` legs). Empty tiers are kept so indices still match
/// Jev's score levels.
pub fn eligible_tiers(tiers: &[Tier], vertex_only: bool) -> Vec<Tier> {
    tiers
        .iter()
        .map(|t| Tier {
            legs: t
                .legs
                .iter()
                .filter(|l| !vertex_only || l.provider == "vertex")
                .cloned()
                .collect(),
            ..t.clone()
        })
        .collect()
}

/// `wanted` if it has legs, else the nearest harder tier with legs, else the
/// nearest easier one; `None` when no tier has legs.
pub fn nearest_serving(tiers: &[Tier], wanted: usize) -> Option<usize> {
    let serving = |i: &usize| !tiers[*i].legs.is_empty();
    (wanted..tiers.len())
        .find(serving)
        .or_else(|| (0..wanted.min(tiers.len())).rev().find(serving))
}

/// A request's execution order plus what is needed to report on it.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutePlan {
    pub mode: RoutingMode,
    pub legs: Vec<PlannedLeg>,
    /// Tier names by index; empty for static routes.
    pub tier_names: Vec<String>,
    /// Tier Jev picked (or `default_tier`), before eligibility.
    pub decided: Option<usize>,
    pub outcome: Option<DecisionOutcome>,
    /// The client's own effort replaces the effort this plan would apply.
    pub client_effort: bool,
    /// `masked` when the guardrails rewrote the request or the reply;
    /// `blocked` when output scanners blocked the reply.
    pub guard: Option<&'static str>,
}

impl RoutePlan {
    /// A static plan, legs untouched. `client_effort` sticks only when a leg
    /// carries an effort for it to override (a `jev` route downgraded to
    /// static); plain routes plan no effort and report none.
    pub fn static_legs(legs: &[ChainLeg], client_effort: bool) -> Self {
        Self {
            mode: RoutingMode::Static,
            legs: legs
                .iter()
                .map(|l| PlannedLeg {
                    leg: l.clone(),
                    tier: 0,
                })
                .collect(),
            tier_names: Vec::new(),
            decided: None,
            outcome: None,
            client_effort: client_effort && legs.iter().any(|l| l.effort.is_some()),
            guard: None,
        }
    }

    /// The legs in execution order, for the lane executors.
    pub fn chain(&self) -> Vec<ChainLeg> {
        self.legs.iter().map(|p| p.leg.clone()).collect()
    }

    /// Effort the first leg will run with, or `client`; `None` when the plan
    /// sets no effort (static routes).
    pub fn planned_effort(&self) -> Option<&'static str> {
        match self.client_effort {
            true => Some("client"),
            false => self
                .legs
                .first()
                .and_then(|p| p.leg.effort)
                .map(Effort::as_str),
        }
    }

    /// What to tell the client once `served` (`provider`, `model`) answered;
    /// `None` when no single leg served (e.g. hybrid extraction). The first
    /// matching leg in plan order wins.
    pub fn report_for(&self, served: Option<(&str, &str)>) -> RoutingReport {
        let served = served.and_then(|(provider, model)| {
            self.legs
                .iter()
                .find(|p| p.leg.provider == provider && p.leg.model == model)
        });
        let tier = served.and_then(|p| self.tier_names.get(p.tier)).cloned();
        let decided = self.decided.and_then(|d| self.tier_names.get(d)).cloned();
        RoutingReport {
            mode: self.mode,
            tier_decided: decided.filter(|d| tier.as_ref().is_some_and(|t| t != d)),
            effort: match self.client_effort {
                true => Some("client".to_string()),
                false => served
                    .and_then(|p| p.leg.effort)
                    .map(|e| e.as_str().to_string()),
            },
            degraded: self.outcome.and_then(DecisionOutcome::degraded_reason),
            guard: self.guard,
            tier,
        }
    }
}

/// Client-facing summary of how a request was routed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutingReport {
    pub mode: RoutingMode,
    /// Tier that served.
    pub tier: Option<String>,
    /// Tier Jev picked, only when a different tier served.
    pub tier_decided: Option<String>,
    /// Effort applied by the serving leg, or `client`.
    pub effort: Option<String>,
    pub degraded: Option<&'static str>,
    /// `masked` when the guardrails rewrote the request or the reply;
    /// `blocked` when output scanners blocked the reply.
    pub guard: Option<&'static str>,
}

impl RoutingReport {
    /// `x-synapse-*` response headers, in a stable order.
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        std::iter::once(("x-synapse-routing", self.mode.as_str().to_string()))
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
    use crate::routing::request::ChatRequest;
    use serde_json::json;

    fn req(body: serde_json::Value) -> ChatRequest {
        serde_json::from_value(body).unwrap()
    }

    #[test]
    fn resolve_mode_follows_route_unless_overridden() {
        assert_eq!(resolve_mode(true, None), Ok(RoutingMode::Jev));
        assert_eq!(resolve_mode(true, Some("jev")), Ok(RoutingMode::Jev));
        assert_eq!(
            resolve_mode(true, Some("static")),
            Ok(RoutingMode::StaticOverride)
        );
        assert_eq!(resolve_mode(false, None), Ok(RoutingMode::Static));
        assert_eq!(resolve_mode(false, Some("static")), Ok(RoutingMode::Static));
        assert!(resolve_mode(false, Some("jev"))
            .unwrap_err()
            .contains("requires a route with tiers"));
        assert!(resolve_mode(true, Some("fastest"))
            .unwrap_err()
            .contains("unknown routing_strategy 'fastest'"));
    }

    #[test]
    fn routing_mode_labels() {
        assert_eq!(RoutingMode::Jev.as_str(), "jev");
        assert_eq!(RoutingMode::Static.as_str(), "static");
        assert_eq!(RoutingMode::StaticOverride.as_str(), "static-override");
        assert_eq!(RoutingMode::default(), RoutingMode::Static);
    }

    #[test]
    fn state_carries_latest_user_message_system_prompt_and_flags() {
        let s = build_state(&req(json!({
            "model": "auto",
            "messages": [
                {"role": "system", "content": "You are terse."},
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": "answer"},
                {"role": "user", "content": [
                    {"type": "text", "text": "what is in"},
                    {"type": "image_url", "image_url": {"url": "https://x/y.png"}}
                ]}
            ],
            "tools": [{"type": "function", "function": {"name": "f"}}]
        })));
        assert_eq!(s["latest_user_message"], "what is in\n[image]");
        assert_eq!(s["system_prompt"], "You are terse.");
        assert_eq!(s["has_tools"], true);
        assert_eq!(s["has_images"], true);
        assert_eq!(
            s["recent_history"],
            json!([
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": "answer"}
            ])
        );
    }

    #[test]
    fn non_text_parts_become_placeholders_and_system_prompt_is_capped() {
        let s = build_state(&req(json!({
            "model": "auto",
            "messages": [
                {"role": "system", "content": "s".repeat(SYSTEM_CAP + 500)},
                {"role": "user", "content": [
                    {"type": "input_audio", "input_audio": {"data": "..."}},
                    {"type": "file", "file": {"file_id": "f"}},
                    {"type": "unknown"}
                ]}
            ],
            "tools": []
        })));
        assert_eq!(s["latest_user_message"], "[audio]\n[file]");
        assert_eq!(
            s["system_prompt"].as_str().unwrap().chars().count(),
            SYSTEM_CAP
        );
        assert_eq!(s["has_tools"], false);
        assert_eq!(s["has_images"], false);
    }

    #[test]
    fn long_latest_message_keeps_head_and_tail() {
        let long = format!("{}{}", "a".repeat(10_000), "z".repeat(10_000));
        let s = build_state(&req(json!({
            "model": "auto",
            "messages": [{"role": "user", "content": long}]
        })));
        let latest = s["latest_user_message"].as_str().unwrap();
        assert!(latest.starts_with(&"a".repeat(8_000)));
        assert!(latest.ends_with(&"z".repeat(8_000)));
        assert!(latest.contains("\n…\n"));
        assert_eq!(latest.chars().count(), LATEST_CAP + "\n…\n".chars().count());
    }

    #[test]
    fn history_fills_the_budget_newest_first_in_chronological_order() {
        let messages: Vec<serde_json::Value> = (0..30)
            .map(|i| {
                json!({
                    "role": if i % 2 == 0 { "user" } else { "assistant" },
                    "content": format!("{i}:{}", "x".repeat(1_990))
                })
            })
            .chain(std::iter::once(
                json!({"role": "user", "content": "latest"}),
            ))
            .collect();
        let s = build_state(&req(json!({"model": "auto", "messages": messages})));
        let history = s["recent_history"].as_array().unwrap();
        let total: usize = history
            .iter()
            .map(|h| h["content"].as_str().unwrap().chars().count())
            .sum();
        assert!(total <= STATE_BUDGET, "history {total} exceeds budget");
        assert!(history.len() < 30, "older messages were dropped");
        assert!(history.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .starts_with("29:"));
        let first_idx: usize = history[0]["content"]
            .as_str()
            .unwrap()
            .split(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(first_idx, 30 - history.len());
    }

    #[test]
    fn vertex_media_uris_count_as_images_and_no_user_message_is_empty_string() {
        let s = build_state(&req(json!({
            "model": "auto",
            "messages": [{"role": "system", "content": "sys"}],
            "vertex": {"media_uris": ["gs://b/v.mp4"]}
        })));
        assert_eq!(s["has_images"], true);
        assert_eq!(s["latest_user_message"], "");
        assert_eq!(s["has_tools"], false);
    }

    use crate::routing::table::RouteTable;

    fn route() -> JevRoute {
        RouteTable::from_toml_str(
            r#"
            [routes."auto"]
            strategy = "jev"
            [routes."auto".jev_router]
            default_tier = "moderate"
            [[routes."auto".tiers]]
            name = "trivial"
            description = "Greetings"
            effort = "none"
            legs = [{ provider = "qwen", model = "qwen-flash" }]
            [[routes."auto".tiers]]
            name = "moderate"
            description = "Everyday questions"
            effort = "low"
            legs = [{ provider = "vertex", model = "gemini-2.5-flash" }]
            [[routes."auto".tiers]]
            name = "hard"
            description = "Multi-step analysis"
            effort = "medium"
            legs = [{ provider = "vertex", model = "gemini-2.5-pro" }]
            [[routes."auto".tiers]]
            name = "expert"
            description = "Proofs and deep debugging"
            effort = "max"
            legs = [{ provider = "vertex", model = "gemini-3.1-pro-preview" }]
            "#,
        )
        .unwrap()
        .jev_route("auto")
        .unwrap()
        .clone()
    }

    fn answers(difficulty: f64, confidence: f64, needs_reasoning: Option<f64>) -> Answers {
        Answers {
            difficulty,
            confidence,
            needs_reasoning,
        }
    }

    #[test]
    fn questions_use_tier_descriptions_in_order() {
        let q = build_questions(&route().tiers);
        assert_eq!(q["difficulty"]["type"], "score");
        assert_eq!(
            q["difficulty"]["criteria"],
            json!([
                "Greetings",
                "Everyday questions",
                "Multi-step analysis",
                "Proofs and deep debugging"
            ])
        );
        assert_eq!(q["needs_reasoning"]["type"], "noul");
        assert!(q["difficulty"]["instructions"]
            .as_str()
            .unwrap()
            .contains("`latest_user_message`"));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn questions_match_the_spec_contract_exactly() {
        assert_eq!(
            Value::Object(build_questions(&route().tiers)),
            json!({
                "difficulty": {
                    "type": "score",
                    "instructions": "How demanding is it to produce a high-quality reply to \
                        `latest_user_message`, given `recent_history` and `system_prompt`?",
                    "criteria": [
                        "Greetings",
                        "Everyday questions",
                        "Multi-step analysis",
                        "Proofs and deep debugging"
                    ]
                },
                "needs_reasoning": {
                    "type": "noul",
                    "instructions": "Does replying well to `latest_user_message` require \
                        careful step-by-step reasoning such as maths, logic, planning, or debugging?"
                }
            })
        );
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        let crabs = "🦀".repeat(LATEST_CAP + 101);
        let separator = "\n…\n";
        let kept = head_tail(&crabs, LATEST_CAP);
        assert_eq!(kept.chars().filter(|c| *c == '🦀').count(), LATEST_CAP);
        assert_eq!(kept.chars().count(), LATEST_CAP + separator.chars().count());
        assert_eq!(kept.replacen(separator, "", 1), "🦀".repeat(LATEST_CAP));
        let cut = truncate(&crabs, SYSTEM_CAP);
        assert_eq!(cut, "🦀".repeat(SYSTEM_CAP));
    }

    #[test]
    fn history_entries_are_capped_individually() {
        let s = build_state(&req(json!({
            "model": "auto",
            "messages": [
                {"role": "user", "content": "q".repeat(HISTORY_ENTRY_CAP + 700)},
                {"role": "assistant", "content": "short"},
                {"role": "user", "content": "latest"}
            ]
        })));
        assert_eq!(
            s["recent_history"][0]["content"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            HISTORY_ENTRY_CAP
        );
        assert_eq!(s["recent_history"][1]["content"], "short");
    }

    #[test]
    fn history_skips_entries_without_text() {
        let s = build_state(&req(json!({
            "model": "auto",
            "messages": [
                {"role": "user", "content": "weather in Lisbon?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "c1", "type": "function",
                     "function": {"name": "weather", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": "sunny"},
                {"role": "assistant", "content": ""},
                {"role": "assistant", "content": [{"type": "unknown"}]},
                {"role": "user", "content": "and tomorrow?"}
            ]
        })));
        assert_eq!(
            s["recent_history"],
            json!([
                {"role": "user", "content": "weather in Lisbon?"},
                {"role": "tool", "content": "sunny"}
            ])
        );
    }

    #[test]
    fn parses_score_and_noul_answers() {
        let a = parse_answers(&json!({
            "difficulty": {"type": "score", "score": 1.15, "confidence": 0.77, "probabilities": {}},
            "needs_reasoning": {"type": "noul", "noul": 0.82}
        }))
        .unwrap();
        assert_eq!(a, answers(1.15, 0.77, Some(0.82)));
        assert_eq!(
            parse_answers(
                &json!({"difficulty": {"type": "score", "score": 2.0, "confidence": 1.0}})
            ),
            Some(answers(2.0, 1.0, None))
        );
        assert_eq!(
            parse_answers(&json!({"difficulty": {"type": "noul", "noul": 0.5}})),
            None
        );
        assert_eq!(
            parse_answers(&json!({"difficulty": {"type": "score", "score": 2.0}})),
            None
        );
        assert_eq!(parse_answers(&json!({})), None);
    }

    #[test]
    fn score_rounds_half_up_and_clamps() {
        assert_eq!(score_to_tier(0.0, 4), 0);
        assert_eq!(score_to_tier(0.49, 4), 0);
        assert_eq!(score_to_tier(0.5, 4), 1);
        assert_eq!(score_to_tier(2.3, 4), 2);
        assert_eq!(score_to_tier(9.0, 4), 3);
        assert_eq!(score_to_tier(-1.0, 4), 0);
        assert_eq!(score_to_tier(f64::NAN, 4), 0);
    }

    #[test]
    fn score_maps_onto_one_and_ten_tiers() {
        [0.0, 0.5, 0.99, 7.0, -3.0]
            .into_iter()
            .for_each(|s| assert_eq!(score_to_tier(s, 1), 0, "score {s}"));
        assert_eq!(score_to_tier(0.49, 10), 0);
        assert_eq!(score_to_tier(4.5, 10), 5);
        assert_eq!(score_to_tier(8.49, 10), 8);
        assert_eq!(score_to_tier(9.0, 10), 9);
        assert_eq!(score_to_tier(12.0, 10), 9);
    }

    #[test]
    fn confident_answer_picks_scored_tier_and_bumps_on_reasoning() {
        let r = route();
        assert_eq!(
            select(Ok(answers(2.3, 0.9, Some(0.8))), &r),
            Selection {
                tier: 2,
                outcome: DecisionOutcome::Decided,
                bump: true
            }
        );
        assert_eq!(
            select(Ok(answers(2.3, 0.9, Some(0.69))), &r),
            Selection {
                tier: 2,
                outcome: DecisionOutcome::Decided,
                bump: false
            }
        );
        assert_eq!(
            select(Ok(answers(2.3, 0.9, None)), &r),
            Selection {
                tier: 2,
                outcome: DecisionOutcome::Decided,
                bump: false
            }
        );
    }

    #[test]
    fn thresholds_are_inclusive() {
        assert_eq!(
            select(Ok(answers(0.2, 0.5, Some(0.7))), &route()),
            Selection {
                tier: 0,
                outcome: DecisionOutcome::Decided,
                bump: true
            }
        );
    }

    #[test]
    fn low_confidence_uses_default_tier() {
        assert_eq!(
            select(Ok(answers(3.0, 0.49, Some(0.9))), &route()),
            Selection {
                tier: 1,
                outcome: DecisionOutcome::LowConfidence,
                bump: true
            }
        );
    }

    #[test]
    fn failures_use_default_tier_without_bump() {
        [
            DecisionOutcome::Timeout,
            DecisionOutcome::Error,
            DecisionOutcome::Unavailable,
        ]
        .into_iter()
        .for_each(|o| {
            assert_eq!(
                select(Err(o), &route()),
                Selection {
                    tier: 1,
                    outcome: o,
                    bump: false
                }
            )
        });
    }

    #[test]
    fn outcome_labels_and_degraded_reasons() {
        use DecisionOutcome::*;
        assert_eq!(Decided.metric_label(), "decided");
        assert_eq!(LowConfidence.metric_label(), "low_confidence");
        assert_eq!(Timeout.metric_label(), "timeout");
        assert_eq!(Error.metric_label(), "error");
        assert_eq!(Unavailable.metric_label(), "error");
        assert_eq!(StaticOverride.metric_label(), "static_override");
        assert_eq!(Decided.degraded_reason(), None);
        assert_eq!(StaticOverride.degraded_reason(), None);
        assert_eq!(Timeout.degraded_reason(), Some("timeout"));
        assert_eq!(LowConfidence.degraded_reason(), Some("low_confidence"));
        assert_eq!(Unavailable.degraded_reason(), Some("jev_unavailable"));
        assert_eq!(Error.degraded_reason(), Some("error"));
    }

    use crate::routing::effort::Effort;

    fn models(plan: &[PlannedLeg]) -> Vec<(&str, usize, Option<Effort>)> {
        plan.iter()
            .map(|p| (p.leg.model.as_str(), p.tier, p.leg.effort))
            .collect()
    }

    #[test]
    fn order_escalates_then_descends_with_tier_effort() {
        let r = route();
        assert_eq!(
            models(&order_legs(&r.tiers, 1, EffortPolicy::Tier { bump: false })),
            vec![
                ("gemini-2.5-flash", 1, Some(Effort::Low)),
                ("gemini-2.5-pro", 2, Some(Effort::Medium)),
                ("gemini-3.1-pro-preview", 3, Some(Effort::Max)),
                ("qwen-flash", 0, Some(Effort::None)),
            ]
        );
    }

    #[test]
    fn order_from_last_tier_only_descends_and_bump_saturates() {
        let r = route();
        assert_eq!(
            models(&order_legs(&r.tiers, 3, EffortPolicy::Tier { bump: true })),
            vec![
                ("gemini-3.1-pro-preview", 3, Some(Effort::Max)),
                ("gemini-2.5-pro", 2, Some(Effort::High)),
                ("gemini-2.5-flash", 1, Some(Effort::Medium)),
                ("qwen-flash", 0, Some(Effort::Minimal)),
            ]
        );
    }

    #[test]
    fn client_policy_stamps_no_effort() {
        assert!(order_legs(&route().tiers, 0, EffortPolicy::Client)
            .iter()
            .all(|p| p.leg.effort.is_none()));
    }

    #[test]
    fn vertex_only_eligibility_empties_other_tiers_and_keeps_indices() {
        let tiers = eligible_tiers(&route().tiers, true);
        assert_eq!(tiers.len(), 4);
        assert!(tiers[0].legs.is_empty());
        assert_eq!(nearest_serving(&tiers, 0), Some(1));
        assert_eq!(nearest_serving(&tiers, 2), Some(2));
        assert_eq!(
            models(&order_legs(&tiers, 1, EffortPolicy::Tier { bump: false }))
                .iter()
                .map(|(m, _, _)| *m)
                .collect::<Vec<_>>(),
            vec![
                "gemini-2.5-flash",
                "gemini-2.5-pro",
                "gemini-3.1-pro-preview"
            ]
        );
        assert_eq!(eligible_tiers(&route().tiers, false), route().tiers);
    }

    #[test]
    fn nearest_serving_prefers_harder_then_easier_and_none_when_empty() {
        let mut tiers = route().tiers;
        tiers[2].legs.clear();
        tiers[3].legs.clear();
        assert_eq!(nearest_serving(&tiers, 2), Some(1));
        tiers.iter_mut().for_each(|t| t.legs.clear());
        assert_eq!(nearest_serving(&tiers, 1), None);
    }

    fn tiered_plan(outcome: DecisionOutcome, client_effort: bool) -> RoutePlan {
        let r = route();
        let policy = match client_effort {
            true => EffortPolicy::Client,
            false => EffortPolicy::Tier { bump: false },
        };
        RoutePlan {
            mode: RoutingMode::Jev,
            legs: order_legs(&r.tiers, 2, policy),
            tier_names: r.tiers.iter().map(|t| t.name.clone()).collect(),
            decided: Some(2),
            outcome: Some(outcome),
            client_effort,
            guard: None,
        }
    }

    #[test]
    fn report_names_served_tier_and_effort() {
        let report = tiered_plan(DecisionOutcome::Decided, false)
            .report_for(Some(("vertex", "gemini-2.5-pro")));
        assert_eq!(
            report,
            RoutingReport {
                mode: RoutingMode::Jev,
                tier: Some("hard".into()),
                tier_decided: None,
                effort: Some("medium".into()),
                degraded: None,
                guard: None,
            }
        );
        assert_eq!(
            report.headers(),
            vec![
                ("x-synapse-routing", "jev".to_string()),
                ("x-synapse-tier", "hard".to_string()),
                ("x-synapse-reasoning-effort", "medium".to_string()),
            ]
        );
    }

    #[test]
    fn report_flags_fallback_tier_client_effort_and_degradation() {
        let report = tiered_plan(DecisionOutcome::Timeout, true)
            .report_for(Some(("vertex", "gemini-3.1-pro-preview")));
        assert_eq!(report.tier.as_deref(), Some("expert"));
        assert_eq!(report.tier_decided.as_deref(), Some("hard"));
        assert_eq!(report.effort.as_deref(), Some("client"));
        assert_eq!(report.degraded, Some("timeout"));
        assert!(report
            .headers()
            .contains(&("x-synapse-routing-degraded", "timeout".to_string())));
    }

    #[test]
    fn planned_effort_is_the_first_legs_effort_or_client() {
        assert_eq!(
            tiered_plan(DecisionOutcome::Decided, false).planned_effort(),
            Some("medium")
        );
        let bumped = RoutePlan {
            legs: order_legs(&route().tiers, 2, EffortPolicy::Tier { bump: true }),
            ..tiered_plan(DecisionOutcome::Decided, false)
        };
        assert_eq!(bumped.planned_effort(), Some("high"));
        assert_eq!(
            tiered_plan(DecisionOutcome::Decided, true).planned_effort(),
            Some("client")
        );
        assert_eq!(RoutePlan::static_legs(&[], false).planned_effort(), None);
    }

    #[test]
    fn unserved_tiered_plan_reports_no_tier_or_effort() {
        let report = tiered_plan(DecisionOutcome::Decided, false).report_for(None);
        assert_eq!(
            report.headers(),
            vec![("x-synapse-routing", "jev".to_string())]
        );
    }

    #[test]
    fn static_override_reports_tier_and_effort_without_degradation() {
        let plan = RoutePlan {
            mode: RoutingMode::StaticOverride,
            outcome: Some(DecisionOutcome::StaticOverride),
            ..tiered_plan(DecisionOutcome::StaticOverride, false)
        };
        assert_eq!(
            plan.report_for(Some(("vertex", "gemini-2.5-pro")))
                .headers(),
            vec![
                ("x-synapse-routing", "static-override".to_string()),
                ("x-synapse-tier", "hard".to_string()),
                ("x-synapse-reasoning-effort", "medium".to_string()),
            ]
        );
    }

    #[test]
    fn static_plan_reports_only_the_mode_and_keeps_legs() {
        let legs = vec![ChainLeg {
            provider: "qwen".into(),
            model: "qwen-max".into(),
            ..Default::default()
        }];
        let plan = RoutePlan::static_legs(&legs, false);
        assert_eq!(plan.chain(), legs);
        assert_eq!(
            plan.report_for(Some(("qwen", "qwen-max"))).headers(),
            vec![("x-synapse-routing", "static".to_string())]
        );
        assert_eq!(plan.report_for(None).tier, None);
        let client = RoutePlan::static_legs(&legs, true);
        assert!(!client.client_effort);
        assert_eq!(client.report_for(Some(("qwen", "qwen-max"))).effort, None);
    }

    #[test]
    fn downgraded_jev_route_reports_the_served_legs_effort_or_client() {
        let legs = route().static_legs();
        let plan = RoutePlan::static_legs(&legs, false);
        assert_eq!(
            plan.report_for(Some(("vertex", "gemini-2.5-pro")))
                .headers(),
            vec![
                ("x-synapse-routing", "static".to_string()),
                ("x-synapse-reasoning-effort", "medium".to_string()),
            ]
        );
        assert_eq!(plan.planned_effort(), Some("low"));
        let client = RoutePlan::static_legs(&legs, true);
        assert_eq!(
            client
                .report_for(Some(("vertex", "gemini-2.5-pro")))
                .effort
                .as_deref(),
            Some("client")
        );
        assert_eq!(client.planned_effort(), Some("client"));
    }
}
