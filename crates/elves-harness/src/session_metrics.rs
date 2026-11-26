//! 编辑框底行只读原生用量；与发送、审批及正文投影互不等待。
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use elves_core::cache_expiry::{CacheExpiry, SessionCacheExpiry, infer_cache_expiry};
use serde_json::{Value, json};
use super::session_metrics_cost as billing;

const SAFE_INTEGER: u64 = 9_007_199_254_740_991;
// 用户要求统计不能拖慢会话：只定位当前会话文件，首次最多尾读 8 MiB，后续只读追加；不扫描子代理或整个归档。
const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;
const CACHE_CAPACITY: usize = 64;

pub(super) fn with_cache_expiry(mut metrics: Value, session_identity: &str) -> Value {
    if let Some(snapshot) = crate::protocol_proxy::session_cache_expiry(session_identity)
        && proxy_cache_matches(&metrics, &snapshot)
    {
        // 代理可见实际出站请求，优先于只根据原生模型推断的结果；读取不会重新发放 TTL。
        if let Some(object) = metrics.as_object_mut() {
            for field in [
                "cacheExpiresAtMs",
                "cacheRemainingMs",
                "cacheObservedAtMs",
                "cacheTtlMs",
                "cacheExpirySource",
            ] {
                object.remove(field);
            }
        }
        add_cache_expiry(&mut metrics, snapshot.expiry);
    }
    metrics
}

fn proxy_cache_matches(metrics: &Value, snapshot: &SessionCacheExpiry) -> bool {
    // 禁用代理、更换模型或开始新轮次后，旧代理观察不能覆盖较新的原生缓存记录。
    if metrics["cacheModel"]
        .as_str()
        .is_some_and(|model| !model.is_empty() && model != snapshot.model)
        || snapshot
            .turn_id
            .as_deref()
            .is_some_and(|id| metrics["turnId"].as_str() != Some(id))
    {
        return false;
    }
    !(snapshot.turn_id.is_none() || metrics["cacheExpirySource"] == "response")
        || timestamp(&metrics["cacheObservedAtMs"]).is_none_or(|at| at <= snapshot.completed_at_ms)
}

fn add_native_cache_expiry(
    metrics: &mut Value,
    at: Option<u64>,
    model: &str,
    expiry: Option<CacheExpiry>,
) {
    if let Some(at) = at {
        metrics["cacheObservedAtMs"] = json!(at);
        metrics["cacheModel"] = json!(model);
    }
    add_cache_expiry(metrics, expiry);
}

fn add_cache_expiry(metrics: &mut Value, expiry: Option<CacheExpiry>) {
    let Some(expiry) = expiry.filter(|expiry| expiry.expires_at_ms <= SAFE_INTEGER) else {
        return;
    };
    let Some(object) = metrics.as_object_mut() else {
        return;
    };
    let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
    object.insert("cacheExpiresAtMs".to_owned(), json!(expiry.expires_at_ms));
    object.insert(
        "cacheRemainingMs".to_owned(),
        json!(expiry.expires_at_ms.saturating_sub(now)),
    );
    object.insert("cacheObservedAtMs".to_owned(), json!(expiry.observed_at_ms));
    object.insert("cacheTtlMs".to_owned(), json!(expiry.ttl_ms));
    object.insert("cacheExpirySource".to_owned(), json!(expiry.source));
}

fn count(value: &Value) -> Option<u64> {
    value.as_u64().filter(|value| *value <= SAFE_INTEGER)
}

fn timestamp(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        chrono::DateTime::parse_from_rfc3339(value.as_str()?)
            .ok()?
            .timestamp_millis()
            .try_into()
            .ok()
    })
}

fn summary(
    source: &str,
    turn_id: &str,
    total: Option<u64>,
    usage: Option<(u64, u64, u64, u64)>,
    duration: Option<u64>,
    first_token: Option<u64>,
    cached_input: Option<u64>,
) -> Value {
    let speed = usage
        .zip(duration)
        .and_then(|((_, output, _, _), ms)| (ms > 0).then_some(output as f64 * 1000.0 / ms as f64));
    // 用户要求缓存命中率只使用原生实测值：本轮缓存读取量除以本轮输入总量，分母无效或字段不完整时不输出。
    let cache_hit_rate = usage
        .zip(cached_input)
        .and_then(|((input, _, _, _), cached)| {
            (input > 0 && cached <= input).then_some(cached as f64 / input as f64)
        });
    json!({"source":source,"turnId":turn_id,"totalTokens":total,
        "inputTokens":usage.map(|usage| usage.0),"outputTokens":usage.map(|usage| usage.1),
        "turnTokens":usage.map(|usage| usage.2),"steps":usage.map(|usage| usage.3),
        "durationMs":duration,"tokensPerSecond":speed,"firstTokenMs":first_token,
        "cacheHitRate":cache_hit_rate,"cachedInputTokens":cached_input})
}

/// 同一 assistant 的多段内容共用请求身份；重复快照和流式中间值只接替，不累计成多步。
pub(super) fn observe_claude(turn: &mut Value, frame: &Value) {
    if frame["type"] == "stream_event" {
        let event = &frame["event"];
        let delta = &event["delta"];
        // 用户要求首 Token 使用真实流式输出；正文、思考和工具参数均可作为首个模型输出，完整回复不能倒推首 Token。
        if event["type"] == "content_block_delta"
            && ["text", "thinking", "partial_json"]
                .iter()
                .any(|key| delta[*key].as_str().is_some_and(|text| !text.is_empty()))
            && turn["_metricsFirstTokenObservedAtMs"].is_null()
            && let Some(at) = timestamp(&frame["timestampMs"])
        {
            turn["_metricsFirstTokenObservedAtMs"] = json!(at);
        }
        return;
    }
    if frame["type"] != "assistant"
        || frame["isApiErrorMessage"] == true
        || frame["message"]["model"] == "<synthetic>"
    {
        return;
    }
    let message = &frame["message"];
    let Some(id) = message["id"].as_str().or_else(|| frame["uuid"].as_str()) else {
        return;
    };
    let raw = &message["usage"];
    let parts: Vec<_> = raw["iterations"]
        .as_array()
        .filter(|parts| !parts.is_empty())
        .map(|parts| parts.iter().collect())
        .unwrap_or_else(|| vec![raw]);
    let mut input = 0_u64;
    let mut output = 0_u64;
    let mut steps = 0_u64;
    let mut cached_input = 0_u64;
    let mut cache_known = true;
    for part in parts {
        if part["type"].as_str().is_some_and(|kind| {
            !matches!(
                kind,
                "message" | "fallback_message" | "compaction" | "advisor_message"
            )
        }) {
            continue;
        }
        let (Some(tokens), Some(generated)) =
            (count(&part["input_tokens"]), count(&part["output_tokens"]))
        else {
            return;
        };
        let cached = part.get("cache_read_input_tokens").map_or(Some(0), count);
        let created = part
            .get("cache_creation_input_tokens")
            .map_or(Some(0), count);
        let (Some(cached), Some(created)) = (cached, created) else {
            return;
        };
        if part.get("cache_read_input_tokens").is_some() {
            cached_input = cached_input.saturating_add(cached);
        } else {
            cache_known = false;
        }
        input = input
            .saturating_add(tokens)
            .saturating_add(cached)
            .saturating_add(created);
        output = output.saturating_add(generated);
        steps += 1;
    }
    if steps == 0 || input.saturating_add(output) > SAFE_INTEGER {
        return;
    }
    let current = &turn["_metricsRequests"][id];
    input = input.max(count(&current["inputTokens"]).unwrap_or(0));
    output = output.max(count(&current["outputTokens"]).unwrap_or(0));
    steps = steps.max(count(&current["steps"]).unwrap_or(0));
    let cached_input = if cache_known {
        Some(cached_input.max(count(&current["cachedInputTokens"]).unwrap_or(0)))
    } else {
        count(&current["cachedInputTokens"])
    };
    if input.saturating_add(output) > SAFE_INTEGER {
        return;
    }
    // 同一请求的流式接替不重复计费；低于已确认用量的快照不能回退对应的模型/缓存费用明细。
    let billing_parts = billing::merge_claude_parts(&current["billingParts"], billing::claude_parts(message));
    let billing_parts = if billing::parts_total(&billing_parts)
        .is_some_and(|total| total != input.saturating_add(output)) { Value::Null } else { billing_parts };
    // 用户要求按原生记录倒计时：同一请求的流式接替及历史重读保留首次时间，不用刷新到达时间续期。
    let cache_observed = timestamp(&current["cacheObservedAtMs"])
        .or_else(|| timestamp(&current["observedAtMs"]))
        .or_else(|| timestamp(&frame["timestampMs"]));
    let cache_expiry = cache_observed.and_then(|at| {
        infer_cache_expiry(
            "anthropic",
            message["model"].as_str().unwrap_or(""),
            &Value::Null,
            raw,
            at,
            serde_json::from_value(current["cacheExpiry"].clone()).ok(),
        )
    });
    turn["_metricsRequests"][id] = json!({"inputTokens":input,"outputTokens":output,
        "cachedInputTokens":cached_input,"steps":steps,"observedAtMs":frame["timestampMs"],
        "cacheObservedAtMs":cache_observed,"cacheModel":message["model"],"cacheExpiry":cache_expiry,
        "billingParts":billing_parts});
}

pub(super) fn claude(turns: &[Value]) -> Value {
    let latest = turns.iter().rev().find(|turn| {
        turn["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["type"] == "userMessage"))
    });
    let mut total = 0_u64;
    let mut seen = HashSet::new();
    let mut any_usage = false;
    let mut latest_cache = None;
    for turn in turns {
        for (id, usage) in turn["_metricsRequests"].as_object().into_iter().flatten() {
            if let Some(at) = timestamp(&usage["cacheObservedAtMs"])
                && latest_cache.as_ref().is_none_or(|(old, _, _)| at >= *old)
            {
                latest_cache = Some((
                    at,
                    serde_json::from_value::<CacheExpiry>(usage["cacheExpiry"].clone()).ok(),
                    usage["cacheModel"].as_str().unwrap_or(""),
                ));
            }
            if seen.insert(id)
                && let (Some(input), Some(output)) =
                    (count(&usage["inputTokens"]), count(&usage["outputTokens"]))
            {
                total = total.saturating_add(input).saturating_add(output);
                any_usage = true;
            }
        }
    }
    let mut input = 0_u64;
    let mut output = 0_u64;
    let mut steps = 0_u64;
    let mut observed = None;
    let mut cached_input = 0_u64;
    let mut cache_known = true;
    let mut billing_parts = Vec::new();
    let mut billing_complete = true;
    for usage in latest
        .into_iter()
        .flat_map(|turn| turn["_metricsRequests"].as_object())
        .flat_map(|requests| requests.values())
    {
        input = input.saturating_add(count(&usage["inputTokens"]).unwrap_or(0));
        output = output.saturating_add(count(&usage["outputTokens"]).unwrap_or(0));
        steps = steps.saturating_add(count(&usage["steps"]).unwrap_or(0));
        if let Some(cached) = count(&usage["cachedInputTokens"]) {
            cached_input = cached_input.saturating_add(cached);
        } else {
            cache_known = false;
        }
        observed = observed.max(timestamp(&usage["observedAtMs"]));
        if let Some(parts) = usage["billingParts"].as_array()
            && billing_parts.len() + parts.len() <= billing::MAX_PARTS
        {
            billing_parts.extend(parts.iter().cloned());
        } else {
            billing_complete = false;
        }
    }
    let duration = latest.and_then(|turn| {
        count(&turn["durationMs"]).or_else(|| {
            let start =
                timestamp(&turn["turnStartedAtMs"]).or_else(|| timestamp(&turn["startedAt"]))?;
            let end = timestamp(&turn["turnCompletedAtMs"])
                .or_else(|| timestamp(&turn["completedAt"]))
                .or(observed)?;
            end.checked_sub(start)
        })
    });
    // 用户确认“步”为模型调用次数，“总”为当前原生会话累计；不把工具数量或子代理汇总混入。
    let mut metrics = summary(
        "claude-native",
        latest.and_then(|turn| turn["id"].as_str()).unwrap_or(""),
        (any_usage && total <= SAFE_INTEGER).then_some(total),
        (steps > 0).then_some((input, output, input.saturating_add(output), steps)),
        duration,
        latest.and_then(|turn| {
            let start =
                timestamp(&turn["turnStartedAtMs"]).or_else(|| timestamp(&turn["startedAt"]))?;
            timestamp(&turn["_metricsFirstTokenObservedAtMs"])?.checked_sub(start)
        }),
        (steps > 0 && cache_known).then_some(cached_input),
    );
    if let Some((at, expiry, model)) = latest_cache {
        add_native_cache_expiry(&mut metrics, Some(at), model, expiry);
    }
    billing::attach(&mut metrics, json!(billing_parts), billing_complete);
    metrics
}

fn codex_side_time(value: &Value) -> Option<u64> {
    timestamp(value).map(|value| {
        if value < 100_000_000_000 {
            value.saturating_mul(1000)
        } else {
            value
        }
    })
}

#[derive(Default)]
pub(super) struct CodexSideMetrics {
    native_total: Option<u64>,
    total: u64,
    total_incomplete: bool,
    round: Option<CodexSideRound>,
    cache: Option<(u64, String, Option<CacheExpiry>)>,
}

#[derive(Default)]
struct CodexSideRound {
    id: String,
    input: u64,
    output: u64,
    tokens: u64,
    steps: u64,
    cached_input: Option<u64>,
    observed_at: u64,
    incomplete: bool,
    billing_parts: Vec<Value>,
    billing_incomplete: bool,
}

impl CodexSideMetrics {
    pub(super) fn observe(&mut self, turn_id: &str, usage: &Value, model: &str, at: u64, has_model_output: bool) {
        let last = &usage["last"];
        let (Some(native_total), Some(tokens), Some(input), Some(output)) = (
            count(&usage["total"]["totalTokens"]),
            count(&last["totalTokens"]),
            count(&last["inputTokens"]),
            count(&last["outputTokens"]),
        ) else { return; };
        // 用户要求独立记账：只累加侧边当前轮次的请求用量；含父历史的原生累计值仅用于判重和检查缺口。
        // 压缩后的上下文估算只有 totalTokens，没有相加匹配的输入/输出，不算模型调用。
        if tokens == 0 || input + output != tokens { return; }
        let Some(base) = native_total.checked_sub(tokens) else { return; };
        if self.round.is_none() && !has_model_output {
            // 首次真实模型输出前的用量可能是 fork 继承快照，先作基线，不能记为侧边用量。
            self.native_total = self.native_total.max(Some(native_total));
            return;
        }
        if self.native_total.is_some_and(|previous| base < previous) { return; }
        let gap = self.native_total.is_some_and(|previous| base > previous);
        self.native_total = Some(native_total);
        self.total_incomplete |= gap;
        self.total = self.total.saturating_add(tokens);
        if self.round.as_ref().is_none_or(|round| round.id != turn_id) {
            self.round = Some(CodexSideRound {
                id: turn_id.to_owned(), cached_input: Some(0), ..Default::default()
            });
        }
        let round = self.round.as_mut().unwrap();
        round.input = round.input.saturating_add(input);
        round.output = round.output.saturating_add(output);
        round.tokens = round.tokens.saturating_add(tokens);
        round.steps += 1;
        round.cached_input = round.cached_input.zip(count(&last["cachedInputTokens"]))
            .map(|(total, cached)| total.saturating_add(cached));
        round.observed_at = at;
        round.incomplete |= gap;
        if round.billing_parts.len() < billing::MAX_PARTS {
            round.billing_parts.push(billing::responses_part(model, last, true, &usage["serviceTier"]));
        } else {
            round.billing_incomplete = true;
        }
        let previous = self.cache.as_ref().filter(|(_, name, _)| name == model)
            .and_then(|(_, _, expiry)| *expiry);
        let expiry = infer_cache_expiry("responses", model, &Value::Null, last, at, previous);
        self.cache = Some((at, model.to_owned(), expiry));
    }

    pub(super) fn value(&self, turns: &[Value], first_token: Option<(String, u64)>) -> Value {
        let Some(round) = &self.round else { return Value::Null; };
        let Some(turn) = turns.iter().rev().find(|turn| turn["id"] == round.id) else {
            return Value::Null;
        };
        let start = codex_side_time(&turn["turnStartedAtMs"]).or_else(|| codex_side_time(&turn["startedAt"]));
        let duration = count(&turn["durationMs"]).or_else(|| {
            let end = codex_side_time(&turn["turnCompletedAtMs"])
                .or_else(|| codex_side_time(&turn["completedAt"]))
                .unwrap_or(round.observed_at);
            end.checked_sub(start?)
        });
        let first_token = first_token.filter(|(id, _)| id == &round.id)
            .and_then(|(_, observed)| start.and_then(|start| observed.checked_sub(start)));
        let mut metrics = summary(
            "codex-side", &round.id,
            (!self.total_incomplete && self.total <= SAFE_INTEGER).then_some(self.total),
            (!round.incomplete && round.tokens <= SAFE_INTEGER)
                .then_some((round.input, round.output, round.tokens, round.steps)),
            duration, first_token, round.cached_input,
        );
        if self.total_incomplete {
            // 覆盖先前已显示的合计；缺口不能沿用之前的数字当作当前总量。
            metrics["totalIncomplete"] = json!(true);
        }
        if let Some((at, model, expiry)) = &self.cache {
            add_native_cache_expiry(&mut metrics, Some(*at), model, *expiry);
        }
        billing::attach(&mut metrics, json!(round.billing_parts), !round.incomplete && !round.billing_incomplete);
        metrics
    }
}

#[derive(Clone, Default)]
struct CodexMetrics {
    model: String,
    service_tier: Value,
    billing_parts: Vec<Value>,
    billing_complete: bool,
    cache_request: Value,
    cache_expiry: Option<CacheExpiry>,
    cache_observed_at: Option<u64>,
    turn_id: String,
    started: Option<u64>,
    ended: Option<u64>,
    observed: Option<u64>,
    total: Option<u64>,
    own_total: Option<u64>,
    own_complete: bool,
    previous_total: Option<u64>,
    input: u64,
    output: u64,
    cached_input: Option<u64>,
    tokens: u64,
    steps: u64,
}

impl CodexMetrics {
    fn consume(&mut self, line: &[u8]) {
        // 正文和图片不参与统计，不解析它们的大 JSON。
        if ![
            b"\"token_count\"".as_slice(),
            b"\"task_started\"",
            b"\"task_complete\"",
            b"\"task_completed\"",
            b"\"task_aborted\"",
            b"\"turn_aborted\"",
            b"\"turn_context\"",
        ]
        .iter()
        .any(|needle| line.windows(needle.len()).any(|part| part == *needle))
        {
            return;
        }
        let Ok(row) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        let payload = &row["payload"];
        if row["type"] == "turn_context" {
            if let Some(model) = payload["model"].as_str() {
                if self.model != model {
                    self.cache_expiry = None;
                    self.cache_observed_at = None;
                }
                self.model = model.to_owned();
                self.service_tier = payload.get("service_tier").or_else(|| payload.get("serviceTier"))
                    .cloned().unwrap_or(Value::Null);
                // 保留原生记录明确提供的保留策略；没有记录时按该模型默认值估算。
                self.cache_request = json!({});
                for field in ["prompt_cache_options", "prompt_cache_retention"] {
                    if let Some(value) = payload.get(field) {
                        self.cache_request[field] = value.clone();
                    }
                }
            }
            return;
        }
        if row["type"] != "event_msg" {
            return;
        }
        let at = timestamp(&row["timestamp"]);
        match payload["type"].as_str() {
            Some("task_started") => {
                let Some(id) = payload["turn_id"].as_str().filter(|id| !id.is_empty()) else {
                    return;
                };
                if self.turn_id != id {
                    self.turn_id = id.to_owned();
                    self.started = at;
                    self.ended = None;
                    self.observed = None;
                    self.input = 0;
                    self.output = 0;
                    self.cached_input = Some(0);
                    self.tokens = 0;
                    self.steps = 0;
                    self.billing_parts.clear();
                    self.billing_complete = true;
                }
            }
            Some("task_complete" | "task_completed" | "task_aborted" | "turn_aborted") => {
                if payload["turn_id"]
                    .as_str()
                    .is_none_or(|id| id == self.turn_id)
                {
                    self.ended = at;
                }
            }
            Some("token_count") => {
                let info = &payload["info"];
                let cumulative = count(&info["total_token_usage"]["total_tokens"]);
                if let Some(cumulative) = cumulative {
                    self.total = Some(cumulative);
                    if self.previous_total == Some(cumulative) {
                        return;
                    }
                    self.previous_total = Some(cumulative);
                } else {
                    return;
                }
                let last = &info["last_token_usage"];
                // 用户要求持久侧边统计不含父会话累计值；只累加此侧边原生 rollout 的实际模型调用。
                if !self.turn_id.is_empty() && let Some(tokens) = count(&last["total_tokens"]) {
                    self.own_total = Some(self.own_total.unwrap_or(0).saturating_add(tokens));
                }
                if let Some(at) = at {
                    self.cache_observed_at = Some(at);
                    self.cache_expiry = infer_cache_expiry(
                        "responses",
                        &self.model,
                        &self.cache_request,
                        last,
                        at,
                        self.cache_expiry,
                    );
                }
                if !self.turn_id.is_empty()
                    && let (Some(input), Some(output), Some(tokens)) = (
                        count(&last["input_tokens"]),
                        count(&last["output_tokens"]),
                        count(&last["total_tokens"]),
                    )
                    && tokens > 0
                {
                    self.input = self.input.saturating_add(input);
                    self.output = self.output.saturating_add(output);
                    self.cached_input = self
                        .cached_input
                        .zip(count(&last["cached_input_tokens"]))
                        .map(|(total, cached)| total.saturating_add(cached));
                    self.tokens = self.tokens.saturating_add(tokens);
                    self.steps += 1;
                    self.observed = at;
                    if self.billing_parts.len() < billing::MAX_PARTS && input.checked_add(output) == Some(tokens) {
                        self.billing_parts.push(billing::responses_part(
                            &self.model, last, false, &self.service_tier,
                        ));
                    } else {
                        self.billing_complete = false;
                    }
                }
            }
            _ => {}
        }
    }

    #[cfg(test)]
    fn value(&self) -> Value {
        self.value_with_first_token(None)
    }

    fn value_with_first_token(&self, first_token: Option<(String, u64)>) -> Value {
        let duration = self
            .started
            .zip(self.ended.or(self.observed))
            .and_then(|(start, end)| end.checked_sub(start));
        let first_token = first_token
            .filter(|(turn_id, _)| turn_id == &self.turn_id)
            .and_then(|(_, observed)| self.started.and_then(|start| observed.checked_sub(start)));
        let mut metrics = summary(
            "codex-rollout",
            &self.turn_id,
            self.total,
            (self.steps > 0 && self.tokens <= SAFE_INTEGER).then_some((
                self.input,
                self.output,
                self.tokens,
                self.steps,
            )),
            duration,
            first_token,
            self.cached_input,
        );
        add_native_cache_expiry(
            &mut metrics,
            self.cache_observed_at,
            &self.model,
            self.cache_expiry,
        );
        billing::attach(&mut metrics, json!(self.billing_parts), self.billing_complete);
        metrics
    }

    fn value_for_thread(&self, first_token: Option<(String, u64)>, attached: bool) -> Value {
        let mut value = self.value_with_first_token(first_token);
        if attached {
            value["source"] = json!("codex-side");
            value["totalTokens"] = if self.own_complete {
                json!(self.own_total.filter(|total| *total <= SAFE_INTEGER))
            } else { Value::Null };
        }
        value
    }
}

#[derive(Clone)]
struct Cached {
    len: u64,
    modified: Option<SystemTime>,
    offset: u64,
    tail: Vec<u8>,
    parser: CodexMetrics,
}

pub(super) fn codex(home: &Path, id: &str, first_token: Option<(String, u64)>) -> Value {
    codex_thread(home, id, first_token, false)
}

pub(super) fn codex_thread(home: &Path, id: &str, first_token: Option<(String, u64)>, attached: bool) -> Value {
    let Some(path) = super::context_usage::rollout_path(home, id) else {
        return Value::Null;
    };
    with_cache_expiry(
        read_codex_kind(&path, first_token, attached).unwrap_or(Value::Null),
        id,
    )
}

#[cfg(test)]
fn read_codex(path: &Path) -> std::io::Result<Value> {
    read_codex_with_first_token(path, None)
}

#[cfg(test)]
fn read_codex_with_first_token(
    path: &Path,
    first_token: Option<(String, u64)>,
) -> std::io::Result<Value> {
    read_codex_kind(path, first_token, false)
}

fn read_codex_kind(path: &Path, first_token: Option<(String, u64)>, attached: bool) -> std::io::Result<Value> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Cached>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    let (len, modified) = (metadata.len(), metadata.modified().ok());
    let cached = cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(path)
        .cloned();
    if let Some(entry) = &cached
        && entry.len == len
        && entry.modified == modified
    {
        return Ok(entry.parser.value_for_thread(first_token, attached));
    }
    let mut parser = CodexMetrics::default();
    let mut offset = len.saturating_sub(MAX_READ_BYTES);
    parser.own_complete = offset == 0;
    if let Some(entry) = cached
        && len > entry.len
        && len.saturating_sub(entry.offset) <= MAX_READ_BYTES
    {
        file.seek(SeekFrom::Start(
            entry.offset.saturating_sub(entry.tail.len() as u64),
        ))?;
        let mut tail = vec![0; entry.tail.len()];
        file.read_exact(&mut tail)?;
        if tail == entry.tail {
            offset = entry.offset;
            parser = entry.parser;
        }
    }
    let skip_first = if offset > 0 {
        file.seek(SeekFrom::Start(offset - 1))?;
        let mut previous = [0];
        file.read_exact(&mut previous)?;
        previous[0] != b'\n'
    } else {
        false
    };
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(MAX_READ_BYTES).read_to_end(&mut bytes)?;
    let start = if skip_first {
        bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |index| index + 1)
    } else {
        0
    };
    let end = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(start, |index| index + 1);
    for line in bytes[start..end].split(|byte| *byte == b'\n') {
        parser.consume(line);
    }
    let parsed = offset + end as u64;
    let tail = bytes[end.saturating_sub(128)..end].to_vec();
    let result = parser.value_for_thread(first_token, attached);
    let mut cache = cache.lock().unwrap_or_else(|error| error.into_inner());
    if cache.len() >= CACHE_CAPACITY && !cache.contains_key(path) {
        cache.clear();
    }
    cache.insert(
        path.to_owned(),
        Cached {
            len,
            modified,
            offset: parsed,
            tail,
            parser,
        },
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn request(id: &str, input: u64, output: u64) -> Value {
        json!({"type":"assistant","timestampMs":2000,
            "message":{"id":id,"model":"native-model","usage":{
                "input_tokens":input,"output_tokens":output,"cache_read_input_tokens":20,
                "cache_creation_input_tokens":30}}})
    }

    fn turn(id: &str) -> Value {
        json!({"id":id,"turnStartedAtMs":1000,"turnCompletedAtMs":3000,
            "items":[{"type":"userMessage"}]})
    }

    fn event(kind: &str, at: &str, fields: Value) -> Value {
        let mut payload = fields;
        payload["type"] = json!(kind);
        json!({"type":"event_msg","timestamp":at,"payload":payload})
    }

    fn usage(total: u64, last: u64) -> Value {
        event(
            "token_count",
            "2026-11-13T00:00:02Z",
            json!({"info":{"total_token_usage":{"total_tokens":total},
                "last_token_usage":{"total_tokens":last,"input_tokens":last-20,"output_tokens":20,
                    "cached_input_tokens":(last-20)/2}}}),
        )
    }

    #[test]
    fn session_metrics_claude_cache_ttl_comes_from_native_usage_and_replay_does_not_extend_it() {
        let mut current = turn("cache");
        let mut row = request("cache-request", 100, 40);
        row["message"]["model"] = json!("claude-haiku-5-5");
        row["message"]["usage"]["cache_creation"] = json!({"ephemeral_1h_input_tokens":30});
        observe_claude(&mut current, &row);
        let first = claude(&[current.clone()]);
        assert_eq!(first["cacheExpiresAtMs"], 2000 + 3_600_000);
        assert_eq!(first["cacheExpirySource"], "response");
        row["timestampMs"] = json!(100_000);
        observe_claude(&mut current, &row);
        let replay = claude(&[current]);
        assert_eq!(replay["cacheExpiresAtMs"], first["cacheExpiresAtMs"]);
        assert_eq!(
            replay["cacheRemainingMs"], 0,
            "旧记录已过期，不能因读取旧历史重新激活缓存"
        );
    }

    #[test]
    fn session_metrics_old_proxy_observation_cannot_override_another_turn_model_or_new_native_response()
     {
        let mut snapshot = SessionCacheExpiry {
            expiry: None,
            model: "gpt-6.1-sol".into(),
            turn_id: Some("turn".into()),
            completed_at_ms: 1000,
        };
        assert!(proxy_cache_matches(
            &json!({"turnId":"turn","cacheModel":"gpt-6.1-sol"}),
            &snapshot
        ));
        assert!(!proxy_cache_matches(
            &json!({"turnId":"next","cacheModel":"gpt-6.1-sol"}),
            &snapshot
        ));
        assert!(!proxy_cache_matches(
            &json!({"turnId":"turn","cacheModel":"claude-haiku-5-5"}),
            &snapshot
        ));
        snapshot.turn_id = None;
        assert!(!proxy_cache_matches(
            &json!({"cacheModel":"gpt-6.1-sol","cacheObservedAtMs":2000}),
            &snapshot
        ));
        snapshot.turn_id = Some("turn".into());
        assert!(!proxy_cache_matches(
            &json!({"turnId":"turn","cacheModel":"gpt-6.1-sol",
            "cacheObservedAtMs":2000,"cacheExpirySource":"response"}),
            &snapshot
        ));
    }

    #[test]
    fn session_metrics_claude_new_unknown_cache_result_does_not_inherit_an_old_models_ttl() {
        let mut previous = turn("previous");
        let mut row = request("previous-request", 100, 40);
        row["message"]["usage"]["cache_creation"] = json!({"ephemeral_1h_input_tokens":30});
        observe_claude(&mut previous, &row);
        let mut current = turn("new-model");
        row["message"]["id"] = json!("new-request");
        row["message"]["usage"]
            .as_object_mut()
            .unwrap()
            .remove("cache_creation");
        row["timestampMs"] = json!(3000);
        observe_claude(&mut current, &row);
        assert!(claude(&[previous, current])["cacheExpiresAtMs"].is_null());
    }

    #[test]
    fn session_metrics_codex_native_model_default_and_request_hint_keep_original_timestamp() {
        let mut parser = CodexMetrics::default();
        parser.consume(
            json!({"type":"turn_context","payload":{"model":"gpt-6.1-sol"}})
                .to_string()
                .as_bytes(),
        );
        let row = usage(2000, 1400);
        parser.consume(row.to_string().as_bytes());
        let at = timestamp(&row["timestamp"]).unwrap();
        assert_eq!(parser.value()["cacheExpiresAtMs"], at + 1_800_000);
        parser.consume(row.to_string().as_bytes());
        assert_eq!(parser.value()["cacheExpiresAtMs"], at + 1_800_000);
        parser.consume(json!({"type":"turn_context","payload":{"model":"gpt-5.5","prompt_cache_retention":"in_memory"}}).to_string().as_bytes());
        parser.consume(usage(4000, 1400).to_string().as_bytes());
        assert_eq!(parser.value()["cacheExpiresAtMs"], at + 300_000);
        assert_eq!(parser.value()["cacheExpirySource"], "request");
    }

    #[test]
    fn session_metrics_codex_side_cache_uses_own_model_and_does_not_renew_replayed_usage() {
        let current = json!({"id":"side-turn"});
        let usage = json!({"total":{"totalTokens":12053},"last":{
            "inputTokens":2048,"cachedInputTokens":1024,"outputTokens":5,"totalTokens":2053}});
        let mut side = CodexSideMetrics::default();
        side.observe("side-turn", &usage, "gpt-6.1-sol", 1000, true);
        side.observe("side-turn", &usage, "gpt-6.1-sol", 100_000, true);
        let metrics = side.value(&[current], None);
        assert_eq!(metrics["cacheExpiresAtMs"], 1000 + 1_800_000);
        assert_eq!(metrics["totalTokens"], 2053);
        assert_eq!(metrics["steps"], 1);
    }

    #[test]
    fn session_metrics_codex_side_sums_only_last_usage_and_deduplicates_native_totals() {
        let current = json!({"id":"side-turn","startedAt":1,"completedAt":5,
            "tokenUsage":{"total":{"totalTokens":999999}},"items":[{"type":"userMessage"}]});
        let mut side = CodexSideMetrics::default();
        for (cumulative, input, cached, output) in [
            (100140, 100, 20, 40),
            (100140, 100, 20, 40),
            (100400, 200, 100, 60),
        ] {
            side.observe("side-turn",
                &json!({"total":{"totalTokens":cumulative},"last":{
                    "inputTokens":input,"cachedInputTokens":cached,"outputTokens":output,"totalTokens":input+output}}),
                "", 3000, true);
        }
        let value = side.value(&[current], Some(("side-turn".into(), 2000)));
        assert_eq!(value["source"], "codex-side");
        assert_eq!(value["totalTokens"], 400);
        assert_eq!(value["turnTokens"], 400);
        assert_eq!(value["steps"], 2);
        assert_eq!(value["tokensPerSecond"], 25.0);
        assert_eq!(value["firstTokenMs"], 1000);
        assert_eq!(value["cacheHitRate"], 0.4);
    }

    #[test]
    fn session_metrics_codex_side_missing_cache_does_not_hide_other_usage() {
        let current = json!({"id":"side-turn","startedAt":1,"items":[{"type":"userMessage"}]});
        let mut side = CodexSideMetrics::default();
        side.observe("side-turn", &json!({"total":{"totalTokens":1140},"last":{
            "inputTokens":100,"outputTokens":40,"totalTokens":140}}), "", 3000, true);
        let value = side.value(&[current], None);
        assert_eq!(value["turnTokens"], 140);
        assert_eq!(value["durationMs"], 2000);
        assert!(value["cacheHitRate"].is_null());
    }

    #[test]
    fn session_metrics_codex_side_skips_context_estimates_and_flags_usage_gaps() {
        let turns = [json!({"id":"first"}), json!({"id":"second"}), json!({"id":"third"})];
        let mut side = CodexSideMetrics::default();
        let usage = |total| json!({"total":{"totalTokens":total},
            "last":{"inputTokens":100,"outputTokens":40,"totalTokens":140}});
        side.observe("first", &usage(1140), "", 1000, true);
        side.observe("second", &json!({"total":{"totalTokens":1140},
            "last":{"inputTokens":0,"outputTokens":0,"totalTokens":887}}), "", 1200, true);
        assert_eq!(side.value(&turns, None)["turnId"], "first");
        side.observe("second", &usage(1420), "", 2000, true);
        assert!(side.value(&turns, None)["totalTokens"].is_null());
        assert!(side.value(&turns, None)["turnTokens"].is_null());
        side.observe("third", &usage(1560), "", 3000, true);
        assert!(side.value(&turns, None)["totalTokens"].is_null());
        assert_eq!(side.value(&turns, None)["turnTokens"], 140);
    }

    #[test]
    fn session_metrics_claude_counts_model_calls_once_per_native_request() {
        let mut current = turn("first");
        let row = request("same-request", 100, 40);
        observe_claude(&mut current, &row);
        observe_claude(&mut current, &row);
        observe_claude(&mut current, &request("same-request", 100, 10));
        let value = claude(&[current]);
        assert_eq!(value["steps"], 1);
        assert_eq!(value["turnTokens"], 190);
        assert_eq!(value["tokensPerSecond"], 20.0);
        assert_eq!(value["cacheHitRate"], 20.0 / 150.0);
    }

    #[test]
    fn session_metrics_claude_first_token_uses_only_real_stream_delta() {
        let mut current = turn("stream");
        for (at, text) in [(1240, "首"), (1300, "个")] {
            observe_claude(
                &mut current,
                &json!({"type":"stream_event","timestampMs":at,
                "event":{"type":"content_block_delta","delta":{"type":"text_delta","text":text}}}),
            );
        }
        observe_claude(&mut current, &request("request", 100, 40));
        let value = claude(&[current.clone()]);
        assert_eq!(value["firstTokenMs"], 240);
        assert_eq!(value["cacheHitRate"], 20.0 / 150.0);

        let mut historical = turn("historical");
        observe_claude(&mut historical, &request("complete-only", 100, 40));
        assert!(claude(&[historical])["firstTokenMs"].is_null());
    }

    #[test]
    fn session_metrics_claude_missing_cache_field_hides_only_the_rate() {
        let mut current = turn("missing-cache");
        let mut row = request("request", 100, 40);
        row["message"]["usage"]
            .as_object_mut()
            .unwrap()
            .remove("cache_read_input_tokens");
        observe_claude(&mut current, &row);
        let value = claude(&[current]);
        assert_eq!(value["turnTokens"], 170);
        assert!(value["cacheHitRate"].is_null());
    }

    #[test]
    fn session_metrics_claude_iterations_replace_top_level_and_preserve_cache_tokens() {
        let mut current = turn("iterations");
        let mut row = request("request", 9000, 9000);
        row["message"]["usage"]["iterations"] = json!([
            {"type":"message","input_tokens":100,"output_tokens":20,"cache_read_input_tokens":50},
            {"type":"compaction","input_tokens":10,"output_tokens":5},
            {"type":"advisor_message","input_tokens":9999,"output_tokens":9999}
        ]);
        observe_claude(&mut current, &row);
        let value = claude(&[current]);
        assert_eq!(value["steps"], 3);
        assert_eq!(value["turnTokens"], 20183);
        assert_eq!(value["totalTokens"], 20183);
    }

    #[test]
    fn session_metrics_claude_new_turn_does_not_relabel_previous_turn_usage() {
        let mut previous = turn("previous");
        observe_claude(&mut previous, &request("one", 100, 40));
        let next = turn("next");
        let value = claude(&[previous, next]);
        assert_eq!(value["totalTokens"], 190);
        assert_eq!(value["turnId"], "next");
        assert!(value["turnTokens"].is_null());
        assert!(value["steps"].is_null());
        assert!(value["tokensPerSecond"].is_null());
    }

    #[test]
    fn session_metrics_claude_split_message_keeps_input_and_final_output_without_counting_twice() {
        let mut current = turn("split");
        observe_claude(&mut current, &request("same", 100, 0));
        observe_claude(&mut current, &request("same", 0, 40));
        let value = claude(&[current]);
        assert_eq!(value["steps"], 1);
        assert_eq!(value["inputTokens"], 150);
        assert_eq!(value["outputTokens"], 40);
        assert_eq!(value["turnTokens"], 190);
        let detail = billing::with_prices(value, &Value::Null);
        assert_eq!(detail["billing"]["rows"][0]["tokens"], 100);
        assert_eq!(detail["billing"]["rows"][1]["tokens"], 40);
        assert_eq!(detail["billing"]["rows"][2]["tokens"], 20);
        assert_eq!(detail["billing"]["rows"][3]["tokens"], 30);
        assert!(detail["billing"]["totalCostUsd"].is_null());
    }

    #[test]
    fn session_metrics_codex_keeps_model_and_tier_for_each_native_call() {
        let mut parser = CodexMetrics::default();
        for row in [
            event("task_started", "2026-11-27T00:00:00Z", json!({"turn_id":"turn"})),
            json!({"type":"turn_context","payload":{"model":"first-model","service_tier":"priority"}}),
            usage(200, 200),
            json!({"type":"turn_context","payload":{"model":"second-model","service_tier":"flex"}}),
            usage(500, 300),
            usage(500, 300),
        ] {
            parser.consume(&serde_json::to_vec(&row).unwrap());
        }
        let value = parser.value();
        assert_eq!(value["_billingParts"].as_array().unwrap().len(), 2);
        assert_eq!(value["_billingParts"][0]["model"], "first-model");
        assert_eq!(value["_billingParts"][0]["serviceTier"], "priority");
        assert_eq!(value["_billingParts"][1]["model"], "second-model");
        assert_eq!(value["_billingParts"][1]["serviceTier"], "flex");
        assert_eq!(billing::parts_total(&value["_billingParts"]), Some(500));
    }

    #[test]
    fn session_metrics_codex_attached_total_excludes_parent_and_hides_partial_history() {
        let mut parser = CodexMetrics { own_complete:true, ..Default::default() };
        for row in [
            event("task_started", "2026-11-18T00:00:00Z", json!({"turn_id":"side"})),
            usage(10200, 200), usage(10200, 200), usage(10500, 300),
        ] {
            parser.consume(&serde_json::to_vec(&row).unwrap());
        }
        assert_eq!(parser.value()["totalTokens"], 10500);
        let side = parser.value_for_thread(None, true);
        assert_eq!(side["source"], "codex-side");
        assert_eq!(side["totalTokens"], 500);
        assert_eq!(side["turnTokens"], 500);
        parser.own_complete = false;
        assert!(parser.value_for_thread(None, true)["totalTokens"].is_null());
        assert_eq!(parser.value_for_thread(None, true)["turnTokens"], 500);
    }

    #[test]
    fn session_metrics_codex_repeated_usage_and_rate_updates_do_not_increase_steps() {
        let mut parser = CodexMetrics::default();
        for row in [
            event(
                "task_started",
                "2026-11-13T00:00:00Z",
                json!({"turn_id":"turn"}),
            ),
            usage(1200, 200),
            usage(1200, 200),
            event(
                "token_count",
                "2026-11-13T00:00:03Z",
                json!({"rate_limits":{}}),
            ),
            usage(1500, 300),
            event(
                "task_complete",
                "2026-11-13T00:00:04Z",
                json!({"turn_id":"turn"}),
            ),
        ] {
            parser.consume(&serde_json::to_vec(&row).unwrap());
        }
        let value = parser.value();
        assert_eq!(value["turnTokens"], 500);
        assert_eq!(value["totalTokens"], 1500);
        assert_eq!(value["steps"], 2);
        assert_eq!(value["tokensPerSecond"], 10.0);
        assert_eq!(value["cacheHitRate"], 0.5);
        let first_token_at = timestamp(&json!("2026-11-13T00:00:01.240Z")).unwrap();
        let live = parser.value_with_first_token(Some(("turn".into(), first_token_at)));
        assert_eq!(live["firstTokenMs"], 1240);
        assert!(parser
            .value_with_first_token(Some(("other-turn".into(), first_token_at)))["firstTokenMs"]
            .is_null());
    }

    #[test]
    fn session_metrics_codex_missing_cache_field_does_not_invent_zero_rate() {
        let mut parser = CodexMetrics::default();
        let started = event(
            "task_started",
            "2026-11-13T00:00:00Z",
            json!({"turn_id":"turn"}),
        );
        let mut missing = usage(1200, 200);
        missing["payload"]["info"]["last_token_usage"]
            .as_object_mut()
            .unwrap()
            .remove("cached_input_tokens");
        for row in [started, missing] {
            parser.consume(&serde_json::to_vec(&row).unwrap());
        }
        let value = parser.value();
        assert_eq!(value["turnTokens"], 200);
        assert!(value["cacheHitRate"].is_null());
    }

    #[test]
    fn session_metrics_codex_new_turn_skips_previous_request_snapshot() {
        let mut parser = CodexMetrics::default();
        for row in [
            event(
                "task_started",
                "2026-11-13T00:00:00Z",
                json!({"turn_id":"previous"}),
            ),
            usage(1200, 200),
            event(
                "task_started",
                "2026-11-13T00:00:03Z",
                json!({"turn_id":"next"}),
            ),
            usage(1200, 200),
        ] {
            parser.consume(&serde_json::to_vec(&row).unwrap());
        }
        assert_eq!(parser.value()["totalTokens"], 1200);
        assert!(parser.value()["turnTokens"].is_null());
        assert!(parser.value()["steps"].is_null());
    }

    #[test]
    fn session_metrics_codex_cache_only_accepts_complete_appended_lines_and_detects_rewrite() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        let started = event(
            "task_started",
            "2026-11-13T00:00:00Z",
            json!({"turn_id":"turn"}),
        );
        std::fs::write(&path, format!("{started}\n{}\n", usage(1200, 200))).unwrap();
        assert_eq!(read_codex(&path).unwrap()["steps"], 1);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let line = serde_json::to_string(&usage(1500, 300)).unwrap();
        file.write_all(line.as_bytes()).unwrap();
        assert_eq!(read_codex(&path).unwrap()["steps"], 1);
        file.write_all(b"\n").unwrap();
        assert_eq!(read_codex(&path).unwrap()["steps"], 2);
        drop(file);
        std::fs::write(&path, format!("{started}\n{}\n", usage(900, 100))).unwrap();
        let rewritten = read_codex(&path).unwrap();
        assert_eq!(rewritten["totalTokens"], 900);
        assert_eq!(rewritten["steps"], 1);
    }
}
