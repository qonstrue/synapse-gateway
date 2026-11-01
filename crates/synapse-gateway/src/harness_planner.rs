//! UHP 2D route planner, session state pinning, and runner dispatch.

use std::collections::{BTreeMap, HashSet};
use std::sync::RwLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tap::{Pipe, Tap, TapFallible};

use crate::error::{GatewayError, LegFailure};
use crate::gateway::{Gateway, RequestCtx};
use crate::jev_native::JevNativeProvider;
use crate::ledger::UsageEntry;
use crate::routing::effort::Effort;
use crate::routing::harness_jev::{
    build_harness_questions, build_harness_state, eligible_grid, order_grid, parse_harness_answers,
    select_2d, HarnessAnswers, HarnessRoutePlan, HarnessRoutingReport, PlannedHarnessLeg,
    Selection2d, TaskRequirements, UhpStateInput,
};
use crate::routing::harness_table::{ConfiguredHarness, HarnessJevRoute, WireProtocol};
use crate::routing::jev_router::{self, DecisionOutcome, EffortPolicy, RoutingMode};
use crate::routing::table::ChainLeg;

/// Inbound UHP `POST /v1/responses` request envelope.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct UhpCreateRequest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
    #[serde(default)]
    pub routing_strategy: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<Effort>,
    #[serde(default)]
    pub max_step: Option<u32>,
    #[serde(default)]
    pub stream: Option<bool>,
}

impl UhpCreateRequest {
    pub fn harness_id(&self) -> Option<&str> {
        self.metadata.get("harness_id").and_then(Value::as_str)
    }

    pub fn env_id(&self) -> Option<&str> {
        self.metadata.get("env_id").and_then(Value::as_str)
    }

    pub fn prompt_text(&self) -> String {
        match &self.input {
            Value::String(s) => s.clone(),
            Value::Array(items) => items
                .iter()
                .filter_map(|item| {
                    item.get("content")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| {
                            item.get("content").and_then(Value::as_array).map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                        })
                })
                .collect::<Vec<_>>()
                .join("\n"),
            other => other.to_string(),
        }
    }

    pub fn has_vision(&self) -> bool {
        fn contains_image(v: &Value) -> bool {
            match v {
                Value::Object(map) => {
                    map.get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|t| t.contains("image"))
                        || map.values().any(contains_image)
                }
                Value::Array(arr) => arr.iter().any(contains_image),
                _ => false,
            }
        }
        contains_image(&self.input)
    }
}

/// Pinned state of an active UHP session.
#[derive(Debug, Clone)]
pub struct UhpSessionRecord {
    pub session_id: String,
    pub harness_id: String,
    pub pinned_profile: Option<String>,
    pub pinned_base: String,
    pub env_id: Option<String>,
    pub turn_count: u32,
    pub history: Vec<(String, String)>,
}

/// Stored UHP response envelope for `GET /v1/responses/:id` and continuation lookup.
#[derive(Debug, Clone)]
pub struct StoredUhpResponse {
    pub response_id: String,
    pub session: UhpSessionRecord,
    pub body: Value,
}

/// Thread-safe store of UHP sessions and responses, plus per-session busy locks.
#[derive(Default)]
pub struct UhpSessionStore {
    responses: RwLock<BTreeMap<String, StoredUhpResponse>>,
    busy_sessions: RwLock<HashSet<String>>,
}

impl UhpSessionStore {
    pub fn get_response(&self, response_id: &str) -> Option<StoredUhpResponse> {
        self.responses.read().unwrap().get(response_id).cloned()
    }

    pub fn record_response(&self, stored: StoredUhpResponse) {
        self.responses
            .write()
            .unwrap()
            .insert(stored.response_id.clone(), stored);
    }

    pub fn try_acquire_session(&self, session_id: &str) -> Result<SessionGuard<'_>, GatewayError> {
        let mut busy = self.busy_sessions.write().unwrap();
        if !busy.insert(session_id.to_string()) {
            return Err(GatewayError::SessionBusy(session_id.to_string()));
        }
        Ok(SessionGuard {
            store: self,
            session_id: session_id.to_string(),
        })
    }
}

pub struct SessionGuard<'a> {
    store: &'a UhpSessionStore,
    session_id: String,
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        self.store
            .busy_sessions
            .write()
            .unwrap()
            .remove(&self.session_id);
    }
}

/// Dispatches a planned `(base, leg, wire_protocol)` turn to a harness runner.
#[derive(Debug, Clone)]
pub struct UhpTurnDispatch {
    pub session_id: String,
    pub response_id: String,
    pub harness_id: String,
    pub profile: Option<String>,
    pub base: String,
    pub tier: Option<String>,
    pub wire: WireProtocol,
    pub leg: ChainLeg,
    pub max_step: Option<u32>,
    pub timeout_seconds: Option<u32>,
    pub request: UhpCreateRequest,
}

/// Outcome returned by a [`UhpRunner`] for one turn.
#[derive(Debug, Clone)]
pub struct UhpTurnOutput {
    pub output_text: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub turns: u32,
}

#[async_trait]
pub trait UhpRunner: Send + Sync {
    async fn execute_turn(&self, dispatch: &UhpTurnDispatch)
        -> Result<UhpTurnOutput, GatewayError>;
}

struct HarnessDecision {
    answers: HarnessAnswers,
    input_tokens: u64,
    output_tokens: u64,
}

impl Gateway {
    /// Resolve a `HarnessRoute` into a [`HarnessRoutePlan`], consulting Jev on
    /// `jev` routes and pinning `base` when continuing an existing session.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn plan_harness_route(
        &self,
        route_alias: &str,
        harness_cfg: &ConfiguredHarness,
        req: &UhpCreateRequest,
        prior_session: Option<&UhpSessionRecord>,
        ctx: &RequestCtx,
        session_id: &str,
        response_id: &str,
    ) -> Result<HarnessRoutePlan, GatewayError> {
        let static_legs = self
            .harnesses
            .static_legs(route_alias)
            .ok_or_else(|| GatewayError::UnknownModel(route_alias.to_string()))?;
        let jev_route = self.harnesses.jev_route(route_alias);
        let mode = jev_router::resolve_mode(jev_route.is_some(), req.routing_strategy.as_deref())
            .map_err(GatewayError::BadRequest)?;
        let reqs = self.extract_requirements(req, harness_cfg, prior_session);

        match (mode, jev_route) {
            (RoutingMode::Jev | RoutingMode::StaticOverride, Some(route)) => {
                self.plan_jev_harness(
                    route_alias,
                    harness_cfg,
                    req,
                    &reqs,
                    prior_session,
                    ctx,
                    session_id,
                    response_id,
                    route,
                    mode,
                )
                .await
            }
            _ => {
                let legs: Vec<PlannedHarnessLeg> = static_legs
                    .iter()
                    .filter(|sl| {
                        reqs.pinned_base.as_deref().is_none_or(|p| p == sl.base)
                            && reqs
                                .client_model
                                .as_deref()
                                .is_none_or(|m| m == sl.leg.model)
                    })
                    .map(|sl| PlannedHarnessLeg {
                        profile: None,
                        base: sl.base.clone(),
                        tier: 0,
                        leg: ChainLeg {
                            effort: req.reasoning_effort.or(sl.leg.effort),
                            ..sl.leg.clone()
                        },
                        wire: sl.wire,
                        max_step: req.max_step,
                        timeout_seconds: None,
                    })
                    .collect();
                if legs.is_empty() {
                    if reqs.client_model.is_some() {
                        return Err(GatewayError::ModelUnavailable(format!(
                            "no leg on harness route '{route_alias}' matches requested model"
                        )));
                    }
                    return Err(GatewayError::HarnessUnavailable(format!(
                        "no static legs on harness route '{route_alias}' satisfy request capabilities"
                    )));
                }
                Ok(HarnessRoutePlan {
                    mode,
                    legs,
                    profile_names: Vec::new(),
                    tier_names: Vec::new(),
                    decided_profile: None,
                    decided_tier: None,
                    outcome: None,
                    profile_outcome: None,
                    client_effort: req.reasoning_effort.is_some(),
                    step_bumped: false,
                    guard: None,
                })
            }
        }
    }

    fn extract_requirements(
        &self,
        req: &UhpCreateRequest,
        harness_cfg: &ConfiguredHarness,
        prior_session: Option<&UhpSessionRecord>,
    ) -> TaskRequirements {
        let pinned_base = prior_session
            .map(|s| s.pinned_base.clone())
            .or_else(|| (harness_cfg.base != "auto").then(|| harness_cfg.base.clone()));
        let client_model = req
            .model
            .as_deref()
            .filter(|m| !m.is_empty() && *m != "auto")
            .map(str::to_string);
        let mcp_transports: HashSet<String> = req
            .tools
            .iter()
            .filter(|t| t.get("type").and_then(Value::as_str) == Some("mcp"))
            .filter_map(|t| {
                t.get("transport")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| t.get("server_url").map(|_| "http".to_string()))
            })
            .collect();
        let needs_vertex_native = req.tools.iter().any(|t| {
            matches!(
                t.get("type").and_then(Value::as_str),
                Some("google_search" | "url_context" | "code_execution")
            )
        });
        TaskRequirements {
            pinned_base,
            client_model,
            needs_vertex_native,
            needs_vision: req.has_vision(),
            needs_skills: false,
            needs_environment: req.env_id().is_some(),
            mcp_transports,
            needs_hard_tool_block: !harness_cfg.disabled_tools.is_empty(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn plan_jev_harness(
        &self,
        route_alias: &str,
        harness_cfg: &ConfiguredHarness,
        req: &UhpCreateRequest,
        reqs: &TaskRequirements,
        prior_session: Option<&UhpSessionRecord>,
        ctx: &RequestCtx,
        session_id: &str,
        response_id: &str,
        route: &HarnessJevRoute,
        mode: RoutingMode,
    ) -> Result<HarnessRoutePlan, GatewayError> {
        let grid = eligible_grid(route, self.harnesses.bases(), reqs).ok_or_else(|| {
            if let Some(m) = &reqs.client_model {
                GatewayError::ModelUnavailable(format!(
                    "model '{m}' is not available on harness route '{route_alias}' for the required capabilities"
                ))
            } else {
                GatewayError::HarnessUnavailable(format!(
                    "no (base, model) cell on harness route '{route_alias}' satisfies request capabilities"
                ))
            }
        })?;

        let ask_profile = prior_session.is_none() && grid.active_profiles.len() >= 2;
        let ask_horizon = route.router.horizon_threshold.is_some();

        let (selection, _answers) = match mode {
            RoutingMode::StaticOverride => (
                Selection2d {
                    profile: (!route.profiles.is_empty()).then(|| route.default_profile_index()),
                    tier: route.default_tier_index(),
                    outcome: DecisionOutcome::StaticOverride,
                    profile_outcome: (!route.profiles.is_empty())
                        .then_some(DecisionOutcome::StaticOverride),
                    bump_effort: false,
                    bump_steps: false,
                },
                None,
            ),
            _ => {
                self.decide_harness_2d(
                    route_alias,
                    harness_cfg,
                    req,
                    prior_session,
                    ctx,
                    session_id,
                    response_id,
                    route,
                    &grid,
                    ask_profile,
                    ask_horizon,
                )
                .await
            }
        };

        let chosen_profile = prior_session
            .and_then(|s| s.pinned_profile.as_deref())
            .and_then(|name| route.profiles.iter().position(|p| p.name == name))
            .or(selection.profile);

        let effort_policy = match req.reasoning_effort.is_some() {
            true => EffortPolicy::Client,
            false => EffortPolicy::Tier {
                bump: selection.bump_effort,
            },
        };
        let legs = order_grid(
            &grid,
            route,
            chosen_profile,
            selection.tier,
            effort_policy,
            selection.bump_steps,
            req.max_step,
        );
        if legs.is_empty() {
            return Err(GatewayError::HarnessUnavailable(format!(
                "no executable legs on harness route '{route_alias}'"
            )));
        }

        let decided_profile_name = chosen_profile
            .and_then(|idx| route.profiles.get(idx))
            .map(|p| p.name.as_str())
            .unwrap_or("none");
        let decided_base = legs.first().map(|l| l.base.as_str()).unwrap_or("none");
        let decided_tier_name = route.tiers[selection.tier].name.as_str();
        self.metrics.harness_routing_decision(
            route_alias,
            decided_profile_name,
            decided_base,
            decided_tier_name,
            selection.outcome.metric_label(),
        );

        Ok(HarnessRoutePlan {
            mode,
            legs,
            profile_names: route.profiles.iter().map(|p| p.name.clone()).collect(),
            tier_names: route.tiers.iter().map(|t| t.name.clone()).collect(),
            decided_profile: chosen_profile,
            decided_tier: Some(selection.tier),
            outcome: Some(selection.outcome),
            profile_outcome: selection.profile_outcome,
            client_effort: req.reasoning_effort.is_some(),
            step_bumped: selection.bump_steps,
            guard: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn decide_harness_2d(
        &self,
        route_alias: &str,
        harness_cfg: &ConfiguredHarness,
        req: &UhpCreateRequest,
        prior_session: Option<&UhpSessionRecord>,
        ctx: &RequestCtx,
        session_id: &str,
        response_id: &str,
        route: &HarnessJevRoute,
        grid: &crate::routing::harness_jev::EligibleGrid,
        ask_profile: bool,
        ask_horizon: bool,
    ) -> (Selection2d, Option<HarnessAnswers>) {
        let state_input = UhpStateInput {
            latest_user_message: req.prompt_text(),
            recent_history: prior_session.map(|s| s.history.clone()).unwrap_or_default(),
            instructions: req.instructions.clone(),
            has_images: req.has_vision(),
            input_filenames: Vec::new(),
            has_environment: req.env_id().is_some(),
            mcp_server_count: req
                .tools
                .iter()
                .filter(|t| t.get("type").and_then(Value::as_str) == Some("mcp"))
                .count(),
            skill_count: 0,
            is_continuation: prior_session.is_some(),
        };
        let state = build_harness_state(&state_input, harness_cfg);
        let questions = build_harness_questions(
            &grid.active_profiles,
            &route.tiers,
            ask_profile,
            ask_horizon,
        );

        let result = match &self.jev_native {
            None => Err(DecisionOutcome::Unavailable),
            Some(provider) => {
                let started = Instant::now();
                let body = json!({
                    "model": route.router.model,
                    "state": state,
                    "questions": questions,
                });
                tokio::time::timeout(
                    Duration::from_millis(route.router.timeout_ms),
                    call_harness_jev(provider, body, route_alias),
                )
                .await
                .unwrap_or(Err(DecisionOutcome::Timeout))
                .tap(|_| {
                    self.metrics
                        .routing_decision_duration(route_alias, started.elapsed().as_secs_f64())
                })
            }
        };

        let answers = result.as_ref().ok().map(|d| d.answers.clone());
        let selection = select_2d(
            result.as_ref().map(|d| d.answers.clone()).map_err(|&e| e),
            route,
            ask_profile,
        );
        if let Ok(d) = &result {
            let prof_idx = prior_session
                .and_then(|s| s.pinned_profile.as_deref())
                .and_then(|name| route.profiles.iter().position(|p| p.name == name))
                .or(selection.profile);
            let prof_name = prof_idx
                .and_then(|idx| route.profiles.get(idx))
                .map(|p| p.name.as_str())
                .unwrap_or("default");
            let base_name = prior_session
                .map(|s| s.pinned_base.as_str())
                .or_else(|| {
                    prof_idx
                        .and_then(|idx| grid.profiles.get(idx))
                        .and_then(|p| p.bases.first().map(String::as_str))
                })
                .or_else(|| grid.bases.keys().next().map(String::as_str))
                .unwrap_or("default");
            let tier_name = route.tiers[selection.tier].name.as_str();
            let composite_tier = format!("{prof_name}/{base_name}/{tier_name}");

            let attr = self.attribution_of(ctx, route_alias);
            self.ledger.enqueue(UsageEntry {
                ts: Utc::now(),
                tenant: self.tenant_of(ctx).to_string(),
                workspace: attr.workspace,
                user: attr.user,
                thread: Some(session_id.to_string()),
                message: Some(response_id.to_string()),
                route: route_alias.to_string(),
                provider: "typesafe".into(),
                model: route.router.model.clone(),
                lane: "jev".into(),
                input_tokens: d.input_tokens,
                output_tokens: d.output_tokens,
                cost_usd: self.pricing.cost_usd(
                    "typesafe",
                    &route.router.model,
                    d.input_tokens,
                    d.output_tokens,
                ),
                request_id: ctx
                    .resolved_request_id()
                    .unwrap_or_else(|| response_id.to_string()),
                status: "ok".into(),
                op: "route_decision".into(),
                user_task_type: attr.user_task_type,
                ai_task_type: attr.ai_task_type,
                score: Some(d.answers.difficulty),
                confidence: Some(d.answers.difficulty_confidence),
                tier: Some(composite_tier),
                outcome: Some(selection.outcome.metric_label().to_string()),
            });
        }
        (selection, answers)
    }

    /// Execute a UHP `POST /v1/responses` turn end-to-end: resolve harness and
    /// session, run 2D Jev planner, walk failover legs via the configured
    /// [`UhpRunner`], persist the response, and emit ledger + metrics.
    pub async fn uhp_response(
        &self,
        req: UhpCreateRequest,
        ctx: &RequestCtx,
    ) -> Result<(Value, HarnessRoutingReport), GatewayError> {
        let prior = match req.previous_response_id.as_deref() {
            Some(prev_id) => Some(self.uhp_sessions.get_response(prev_id).ok_or_else(|| {
                GatewayError::BadRequest(format!("unknown previous_response_id '{prev_id}'"))
            })?),
            None => None,
        };

        let harness_id = match (req.harness_id(), prior.as_ref()) {
            (Some(requested), Some(prev)) if requested != prev.session.harness_id => {
                return Err(GatewayError::HarnessMismatch {
                    expected: prev.session.harness_id.clone(),
                    requested: requested.to_string(),
                });
            }
            (Some(requested), _) => requested.to_string(),
            (None, Some(prev)) => prev.session.harness_id.clone(),
            (None, None) => self
                .harnesses
                .harnesses()
                .keys()
                .next()
                .cloned()
                .unwrap_or_else(|| "auto".into()),
        };

        let default_harness = ConfiguredHarness {
            name: harness_id.clone(),
            base: "auto".into(),
            route: Some(harness_id.clone()),
            system_prompt: String::new(),
            disabled_tools: Vec::new(),
        };
        let harness_cfg = self
            .harnesses
            .harness(&harness_id)
            .unwrap_or(&default_harness);
        let route_alias = harness_cfg
            .route
            .as_deref()
            .unwrap_or(harness_id.as_str())
            .to_string();

        let session_id = prior
            .as_ref()
            .map(|p| p.session.session_id.clone())
            .unwrap_or_else(|| format!("sess_{}", uuid::Uuid::new_v4().simple()));
        let _busy_guard = self.uhp_sessions.try_acquire_session(&session_id)?;
        let response_id = format!("resp_{}", uuid::Uuid::new_v4().simple());

        let plan = self
            .plan_harness_route(
                &route_alias,
                harness_cfg,
                &req,
                prior.as_ref().map(|p| &p.session),
                ctx,
                &session_id,
                &response_id,
            )
            .await?;

        let runner = self.uhp_runner.as_ref().ok_or_else(|| {
            GatewayError::HarnessUnavailable("no UHP runner configured on gateway".into())
        })?;

        let started = Instant::now();
        let mut failures = Vec::new();
        for planned in &plan.legs {
            let profile_name = planned
                .profile
                .and_then(|idx| plan.profile_names.get(idx))
                .cloned();
            let tier_name = plan.tier_names.get(planned.tier).cloned();
            let dispatch = UhpTurnDispatch {
                session_id: session_id.clone(),
                response_id: response_id.clone(),
                harness_id: harness_id.clone(),
                profile: profile_name.clone(),
                base: planned.base.clone(),
                tier: tier_name.clone(),
                wire: planned.wire,
                leg: planned.leg.clone(),
                max_step: planned.max_step,
                timeout_seconds: planned.timeout_seconds,
                request: req.clone(),
            };
            match runner.execute_turn(&dispatch).await {
                Ok(output) => {
                    let elapsed_secs = started.elapsed().as_secs_f64();
                    let report = plan.report_for(Some((
                        &planned.base,
                        &planned.leg.provider,
                        &planned.leg.model,
                    )));

                    self.metrics.harness_turn(
                        &harness_id,
                        &planned.base,
                        &planned.leg.model,
                        "completed",
                        elapsed_secs,
                    );

                    let cost = self.pricing.cost_usd(
                        &planned.leg.provider,
                        &planned.leg.model,
                        output.input_tokens,
                        output.output_tokens,
                    );
                    let attr = self.attribution_of(ctx, &route_alias);
                    self.ledger.enqueue(UsageEntry {
                        ts: Utc::now(),
                        tenant: self.tenant_of(ctx).to_string(),
                        workspace: attr.workspace,
                        user: attr.user,
                        thread: Some(session_id.clone()),
                        message: Some(response_id.clone()),
                        route: route_alias.clone(),
                        provider: planned.leg.provider.clone(),
                        model: planned.leg.model.clone(),
                        lane: "uhp".into(),
                        input_tokens: output.input_tokens,
                        output_tokens: output.output_tokens,
                        cost_usd: cost,
                        request_id: ctx
                            .resolved_request_id()
                            .unwrap_or_else(|| response_id.clone()),
                        status: "ok".into(),
                        op: "harness_turn".into(),
                        user_task_type: attr.user_task_type,
                        ai_task_type: attr.ai_task_type,
                        score: None,
                        confidence: None,
                        tier: Some(format!(
                            "{}/{}/{}",
                            profile_name.as_deref().unwrap_or("static"),
                            planned.base,
                            tier_name.as_deref().unwrap_or("static")
                        )),
                        outcome: plan.outcome.map(|o| o.metric_label().to_string()),
                    });

                    let mut history = prior
                        .as_ref()
                        .map(|p| p.session.history.clone())
                        .unwrap_or_default();
                    history.push(("user".into(), req.prompt_text()));
                    history.push(("assistant".into(), output.output_text.clone()));

                    let session_record = UhpSessionRecord {
                        session_id: session_id.clone(),
                        harness_id: harness_id.clone(),
                        pinned_profile: profile_name.clone(),
                        pinned_base: planned.base.clone(),
                        env_id: req
                            .env_id()
                            .map(str::to_string)
                            .or_else(|| prior.as_ref().and_then(|p| p.session.env_id.clone())),
                        turn_count: prior.as_ref().map_or(1, |p| p.session.turn_count + 1),
                        history,
                    };
                    let body = json!({
                        "id": response_id,
                        "object": "response",
                        "status": "completed",
                        "model": format!("{}/{}", planned.leg.provider, planned.leg.model),
                        "previous_response_id": req.previous_response_id,
                        "output": [{
                            "type": "message",
                            "role": "assistant",
                            "content": [{
                                "type": "output_text",
                                "text": output.output_text,
                            }],
                        }],
                        "usage": {
                            "input_tokens": output.input_tokens,
                            "output_tokens": output.output_tokens,
                            "total_tokens": output.input_tokens + output.output_tokens,
                            "turns": output.turns,
                            "cost_usd": cost,
                        },
                        "metadata": {
                            "harness_id": harness_id,
                            "session_id": session_id,
                            "env_id": session_record.env_id,
                            "harness_profile": profile_name,
                            "harness_base": planned.base,
                            "model_tier": tier_name,
                            "wire_protocol": planned.wire.as_str(),
                        }
                    });
                    self.uhp_sessions.record_response(StoredUhpResponse {
                        response_id,
                        session: session_record,
                        body: body.clone(),
                    });
                    return Ok((body, report));
                }
                Err(err) => {
                    failures.push(LegFailure {
                        provider: format!("{}:{}", planned.base, planned.leg.provider),
                        model: planned.leg.model.clone(),
                        message: err.to_string(),
                    });
                }
            }
        }

        self.metrics.harness_turn(
            &harness_id,
            plan.legs.first().map_or("none", |l| l.base.as_str()),
            "none",
            "failed",
            started.elapsed().as_secs_f64(),
        );
        Err(GatewayError::AllLegsFailed {
            route: route_alias,
            failures,
        })
    }

    pub fn get_uhp_response(&self, response_id: &str) -> Option<Value> {
        self.uhp_sessions.get_response(response_id).map(|r| r.body)
    }
}

async fn call_harness_jev(
    provider: &JevNativeProvider,
    body: Value,
    route: &str,
) -> Result<HarnessDecision, DecisionOutcome> {
    let warn = |kind: &str, status: Option<u16>| {
        tracing::warn!(
            target: "synapse::routing",
            route,
            error.kind = kind,
            http.status = ?status,
            "jev 2D harness decision failed; routing to default_profile/default_tier"
        )
    };
    let resp = provider
        .evaluate(body)
        .await
        .tap_err(|_| warn("transport", None))
        .map_err(|_| DecisionOutcome::Error)?
        .error_for_status()
        .tap_err(|e| warn("http_status", e.status().map(|s| s.as_u16())))
        .map_err(|_| DecisionOutcome::Error)?;
    let status = resp.status().as_u16();
    let value: Value = resp
        .json()
        .await
        .tap_err(|_| warn("unreadable_body", Some(status)))
        .map_err(|_| DecisionOutcome::Error)?;
    parse_harness_answers(&value["answers"])
        .map(|answers| HarnessDecision {
            answers,
            input_tokens: value["usage"]["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: value["usage"]["output_tokens"].as_u64().unwrap_or(0),
        })
        .ok_or_else(|| {
            warn("malformed_answers", Some(status));
            DecisionOutcome::Error
        })
        .pipe(std::future::ready)
        .await
}
