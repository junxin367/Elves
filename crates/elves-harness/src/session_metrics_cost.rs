//! 本轮 Token 明细与公开价格估算；不访问账号、凭据或官方账单。
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const PRICE_SOURCE: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const MAX_PRICE_BYTES: usize = 16 * 1024 * 1024;
const PRICE_TTL_MS: u64 = 24 * 60 * 60 * 1000;
pub(super) const MAX_PARTS: usize = 2048;
const FIELDS: [(&str, &str); 6] = [
    ("inputTokens", "input"),
    ("outputTokens", "output"),
    ("cachedInputTokens", "cacheRead"),
    ("cacheWrite5mTokens", "cacheWrite5m"),
    ("cacheWrite1hTokens", "cacheWrite1h"),
    ("cacheWriteTokens", "cacheWrite"),
];

fn count(value: &Value) -> Option<u64> {
    value.as_u64().filter(|value| *value <= SAFE_INTEGER)
}

pub(super) fn responses_part(model: &str, usage: &Value, camel: bool, tier: &Value) -> Value {
    let (input_key, output_key, cache_key) = if camel {
        ("inputTokens", "outputTokens", "cachedInputTokens")
    } else {
        ("input_tokens", "output_tokens", "cached_input_tokens")
    };
    let input = count(&usage[input_key]);
    let cached = count(&usage[cache_key]);
    let valid_cached = cached.filter(|cached| input.is_some_and(|input| *cached <= input));
    // 用户要求输入和缓存分行且不重复记账；缺缓存明细时不能把总输入冒充未缓存输入。
    json!({"model":model,"inputTokens":input.zip(valid_cached).and_then(|(input,cached)|input.checked_sub(cached)),
        "outputTokens":count(&usage[output_key]),"cachedInputTokens":valid_cached,
        "cacheWrite5mTokens":0,"cacheWrite1hTokens":0,"cacheWriteTokens":0,
        "contextTokens":input,"serviceTier":tier,
        "reasoningTokens":count(&usage[if camel {"reasoningOutputTokens"} else {"reasoning_output_tokens"}])})
}

pub(super) fn claude_parts(message: &Value) -> Value {
    let usage = &message["usage"];
    let raw: Vec<_> = usage["iterations"]
        .as_array()
        .filter(|items| !items.is_empty())
        .map(|items| items.iter().collect())
        .unwrap_or_else(|| vec![usage]);
    if raw.len() > MAX_PARTS {
        return Value::Null;
    }
    let mut parts = Vec::new();
    for part in raw {
        if part["type"].as_str().is_some_and(|kind| {
            !matches!(
                kind,
                "message" | "fallback_message" | "compaction" | "advisor_message"
            )
        }) {
            continue;
        }
        let input = count(&part["input_tokens"]);
        let output = count(&part["output_tokens"]);
        let read = count(&part["cache_read_input_tokens"]);
        let created = count(&part["cache_creation_input_tokens"]);
        let creation = &part["cache_creation"];
        let five = count(&creation["ephemeral_5m_input_tokens"]);
        let hour = count(&creation["ephemeral_1h_input_tokens"]);
        let (five, hour, unknown) = match (created, five, hour) {
            (Some(0), None | Some(0), None | Some(0)) => (Some(0), Some(0), Some(0)),
            (Some(total), Some(five), Some(hour)) if five.checked_add(hour) == Some(total) => {
                (Some(five), Some(hour), Some(0))
            }
            (Some(total), Some(five), None) if five == total => (Some(five), Some(0), Some(0)),
            (Some(total), None, Some(hour)) if hour == total => (Some(0), Some(hour), Some(0)),
            // 详情只显示一次写入总量。原生给了总量但 TTL 缺失、不齐或矛盾时仍可展示数量，未知档不计价。
            (Some(total), _, _) if total > 0 => (Some(0), Some(0), Some(total)),
            _ => (None, None, None),
        };
        let context = input
            .zip(read)
            .zip(created)
            .and_then(|((input, read), created)| input.checked_add(read)?.checked_add(created))
            .filter(|total| *total <= SAFE_INTEGER);
        parts.push(json!({"model":part["model"].as_str().or_else(||message["model"].as_str()).unwrap_or(""),
            "inputTokens":input,"outputTokens":output,"cachedInputTokens":read,
            "cacheWrite5mTokens":five,"cacheWrite1hTokens":hour,"cacheWriteTokens":unknown,
            "contextTokens":context,
            "serviceTier":part.get("service_tier").or_else(||usage.get("service_tier")),
            "speed":part.get("speed").or_else(||usage.get("speed")),
            "inferenceGeo":part.get("inference_geo").or_else(||usage.get("inference_geo"))}));
    }
    json!(parts)
}

pub(super) fn merge_claude_parts(previous: &Value, mut next: Value) -> Value {
    let Some(old) = previous.as_array() else {
        return next;
    };
    let Some(parts) = next.as_array_mut() else {
        return previous.clone();
    };
    if old.len() != parts.len() {
        return next;
    }
    for (old, part) in old.iter().zip(parts) {
        if old["model"] != part["model"] {
            return Value::Null;
        }
        // 写入三档是同一次调用的互斥分解；逐档取最大值会把先报的未知时长和后来补齐的 5m/1h 算两遍。
        let write_keys = ["cacheWrite5mTokens", "cacheWrite1hTokens", "cacheWriteTokens"];
        let write_total = |value: &Value| write_keys.iter().try_fold(0_u64, |sum, k| sum.checked_add(count(&value[*k])?));
        if let Some(old_total) = write_total(old)
            && write_total(part).is_none_or(|new| new < old_total
                || new == old_total && count(&old["cacheWriteTokens"]) < count(&part["cacheWriteTokens"]))
        {
            for key in write_keys {
                part[key] = old[key].clone();
            }
        }
        // Claude 同一请求可先报告输入、后报告输出；分段快照按分项接替，不丢失已确认的输入或重复算一步。
        for (field, _) in FIELDS {
            if write_keys.contains(&field) {
                continue;
            }
            if let Some(value) = count(&old[field]) {
                part[field] = json!(count(&part[field]).map_or(value, |next| next.max(value)));
            }
        }
        let context = [
            "inputTokens",
            "cachedInputTokens",
            "cacheWrite5mTokens",
            "cacheWrite1hTokens",
            "cacheWriteTokens",
        ]
        .iter()
        .try_fold(0_u64, |sum, key| sum.checked_add(count(&part[*key])?))
        .filter(|sum| *sum <= SAFE_INTEGER);
        part["contextTokens"] = json!(context);
        for field in ["serviceTier", "speed", "inferenceGeo"] {
            if part[field].is_null() {
                part[field] = old[field].clone();
            }
        }
    }
    next
}

pub(super) fn attach(metrics: &mut Value, parts: Value, complete: bool) {
    if metrics.is_object() {
        metrics["_billingParts"] = parts;
        metrics["_billingComplete"] = json!(complete);
    }
}

pub(super) fn parts_total(parts: &Value) -> Option<u64> {
    parts.as_array()?.iter().try_fold(0_u64, |total, part| {
        FIELDS.iter().try_fold(total, |total, (field, _)| {
            total
                .checked_add(count(&part[*field])?)
                .filter(|total| *total <= SAFE_INTEGER)
        })
    })
}

fn model_price<'a>(models: &'a Value, model: &str) -> Option<&'a Value> {
    if model.is_empty() {
        return None;
    }
    let unprefixed = ["openai/", "anthropic/", "gemini/", "google/"]
        .iter()
        .find_map(|prefix| model.strip_prefix(prefix))
        .unwrap_or(model);
    for key in [model, unprefixed] {
        if let Some(row) = models.get(key).filter(|row| row.is_object()) {
            return Some(row);
        }
        // 仅移除明确的快照日期，不按近似名称猜测新型号价格。
        if let Some((base, date)) = key.rsplit_once('-')
            && date.len() == 8
            && date.bytes().all(|ch| ch.is_ascii_digit())
            && let Some(row) = models.get(base).filter(|row| row.is_object())
        {
            return Some(row);
        }
    }
    // 公开表对 Gemini 使用协议前缀；仅匹配同一家族的精确型号，不尝试其他供应商的价格。
    let prefix = if unprefixed.starts_with("gemini-") {
        Some("gemini")
    } else if unprefixed.starts_with("claude-") {
        Some("anthropic")
    } else if unprefixed.starts_with("gpt-")
        || unprefixed.starts_with("o3")
        || unprefixed.starts_with("o4")
    {
        Some("openai")
    } else {
        None
    };
    if let Some(prefix) = prefix {
        return models
            .get(format!("{prefix}/{unprefixed}"))
            .filter(|row| row.is_object());
    }
    None
}

fn finite(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn part_rate(row: &Value, part: &Value, key: &str) -> Option<f64> {
    let tier = match part["serviceTier"].as_str() {
        None if part["serviceTier"].is_null() => "",
        Some("standard" | "default" | "auto") => "",
        Some("priority") => "_priority",
        Some("flex") => "_flex",
        Some("batch") => "_batches",
        Some("ultrafast") => "_ultrafast",
        _ => return None,
    };
    let mut threshold = (0_f64, String::new());
    for field in row.as_object()?.keys() {
        let Some(suffix) = field.strip_prefix("input_cost_per_token_above_") else {
            continue;
        };
        let Some((size, rest)) = suffix.split_once("_tokens") else {
            continue;
        };
        if !rest.is_empty() && rest != tier {
            continue;
        }
        let (size, multiplier) = if let Some(n) = size.strip_suffix('k') {
            (n, 1000.0)
        } else if let Some(n) = size.strip_suffix('m') {
            (n, 1_000_000.0)
        } else {
            (size, 1.0)
        };
        let Ok(limit) = size.parse::<f64>() else {
            continue;
        };
        let limit = limit * multiplier;
        // 已声明长上下文档位但请求上下文未知时，不假定走普通价。
        let context = count(&part["contextTokens"])?;
        if limit.is_finite() && limit > threshold.0 && context as f64 > limit {
            threshold = (
                limit,
                format!("_above_{}_tokens", suffix.split("_tokens").next()?),
            );
        }
    }
    let mut price = finite(&row[format!("{key}{}{tier}", threshold.1)])
        .or_else(|| finite(&row[format!("{key}{tier}{}", threshold.1)]))
        .or_else(|| finite(&row[format!("{key}{tier}")]))?;
    // Fast、优先档位与地域价格只使用价格表中明确的规则，不套固定倍数或回退到普通档位。
    if part["speed"] == "fast" && tier.is_empty() {
        price *= finite(&row["provider_specific_entry"]["fast"]).filter(|n| *n > 0.0)?;
    } else if !part["speed"].is_null() && part["speed"] != "standard" && part["speed"] != "fast" {
        return None;
    }
    if !part["inferenceGeo"].is_null() && !part["inferenceGeo"].is_string() {
        return None;
    }
    if let Some(geo) = part["inferenceGeo"]
        .as_str()
        .filter(|geo| !matches!(*geo, "" | "global"))
    {
        price *= finite(&row[format!("regional_processing_uplift_multiplier_{geo}")])
            .or_else(|| finite(&row["provider_specific_entry"][geo]))
            .filter(|n| *n > 0.0)?;
    }
    price.is_finite().then_some(price)
}

fn component_cost(models: &Value, part: &Value, field: &str, kind: &str) -> Option<f64> {
    let tokens = count(&part[field])?;
    if tokens == 0 {
        return Some(0.0);
    }
    let row = model_price(models, part["model"].as_str().unwrap_or(""))?;
    let key = match kind {
        "input" => "input_cost_per_token",
        "output" => "output_cost_per_token",
        "cacheRead" => "cache_read_input_token_cost",
        "cacheWrite5m" => "cache_creation_input_token_cost",
        "cacheWrite1h" => "cache_creation_input_token_cost_above_1hr",
        // 原生未报告写入 TTL 时只展示 Token，不能猜成 5m 或 1h 费用。
        _ => return None,
    };
    let cost = if kind == "output" && row.get("output_cost_per_reasoning_token").is_some() {
        let reasoning = count(&part["reasoningTokens"]).filter(|count| *count <= tokens)?;
        (tokens - reasoning) as f64 * part_rate(row, part, key)?
            + reasoning as f64 * part_rate(row, part, "output_cost_per_reasoning_token")?
    } else {
        tokens as f64 * part_rate(row, part, key)?
    };
    (cost.is_finite() && cost >= 0.0).then_some(cost)
}

pub(super) fn with_prices(mut metrics: Value, prices: &Value) -> Value {
    let Some(object) = metrics.as_object_mut() else {
        return metrics;
    };
    let raw = object.remove("_billingParts").unwrap_or(Value::Null);
    let complete = object.remove("_billingComplete") != Some(json!(false));
    let parts = raw
        .as_array()
        .filter(|parts| !parts.is_empty() && parts.len() <= MAX_PARTS);
    let mut models = Vec::new();
    let mut rows = Vec::new();
    let mut total_cost = Some(0.0);
    if let Some(parts) = parts.filter(|_| complete) {
        for part in parts {
            if let Some(model) = part["model"]
                .as_str()
                .filter(|model| !model.is_empty() && model.len() <= 256)
                && !models.contains(&model)
            {
                models.push(model);
            }
        }
        for (field, kind) in FIELDS {
            let tokens = parts.iter().try_fold(0_u64, |sum, part| {
                sum.checked_add(count(&part[field])?)
                    .filter(|sum| *sum <= SAFE_INTEGER)
            });
            let cost = parts.iter().try_fold(0.0, |sum, part| {
                let value = sum + component_cost(prices, part, field, kind)?;
                value.is_finite().then_some(value)
            });
            total_cost = total_cost.zip(cost).map(|(total, cost)| total + cost);
            // 保留输入/输出的真实零；未发生缓存时省略缓存行，未知量不伪造为零。
            if let Some(tokens) =
                tokens.filter(|tokens| *tokens > 0 || matches!(kind, "input" | "output"))
            {
                rows.push(json!({"kind":kind,"tokens":tokens,"costUsd":cost}));
            }
        }
        if !parts_total(&raw)
            .zip(count(&metrics["turnTokens"]))
            .is_some_and(|(parts, total)| parts == total)
        {
            total_cost = None;
        }
    } else {
        // 旧快照只有汇总数据时仍显示已知明细，但不借当前选择模型为历史费用补价。
        let input = count(&metrics["inputTokens"]);
        let cached = count(&metrics["cachedInputTokens"]);
        // Claude 总输入还包含缓存写入；旧汇总没有写入明细时，不能只减缓存读取后称为普通输入。
        let uncached = (metrics["source"] != "claude-native")
            .then(|| {
                input
                    .zip(cached)
                    .and_then(|(input, cached)| input.checked_sub(cached))
            })
            .flatten();
        for (kind, tokens) in [
            ("input", uncached),
            ("output", count(&metrics["outputTokens"])),
            ("cacheRead", cached),
        ] {
            if let Some(tokens) = tokens.filter(|tokens| *tokens > 0 || kind != "cacheRead") {
                rows.push(json!({"kind":kind,"tokens":tokens,"costUsd":null}));
            }
        }
        total_cost = None;
    }
    metrics["billing"] = json!({"models":models,"rows":rows,
        "totalTokens":count(&metrics["turnTokens"]),
        "totalCostUsd":total_cost.filter(|cost| cost.is_finite() && *cost >= 0.0)});
    metrics
}

#[derive(Default)]
struct PriceState {
    models: Arc<Value>,
    checked: Option<Instant>,
    refreshing: bool,
}

pub(super) fn enrich(metrics: Value, directory: &Path) -> Value {
    static PRICES: OnceLock<Mutex<HashMap<PathBuf, PriceState>>> = OnceLock::new();
    let cache = PRICES.get_or_init(|| Mutex::new(HashMap::new()));
    let (models, refresh) = {
        let mut cache = cache.lock().unwrap_or_else(|error| error.into_inner());
        let state = cache.entry(directory.to_owned()).or_default();
        let refresh = !cfg!(test)
            && !state.refreshing
            && state
                .checked
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(300));
        if refresh {
            state.refreshing = true;
        }
        (state.models.clone(), refresh)
    };
    if refresh {
        let directory = directory.to_owned();
        // 用户要求统计不能影响发送；价格表在后台复用/刷新，统计响应不等待文件读取或网络。
        tokio::spawn(async move {
            let result = load_prices(&directory).await;
            let mut cache = cache.lock().unwrap_or_else(|error| error.into_inner());
            let state = cache.entry(directory).or_default();
            if let Ok(models) = result {
                state.models = Arc::new(models);
            }
            state.checked = Some(Instant::now());
            state.refreshing = false;
        });
    }
    with_prices(metrics, &models)
}

async fn load_prices(directory: &Path) -> anyhow::Result<Value> {
    // 沿用项目已有的公开价格缓存，仅保存价格规则，不上传用量、消息、实例或账号信息。
    let path = directory.join("claude-public-model-prices.json");
    let cached = if tokio::fs::metadata(&path)
        .await
        .is_ok_and(|meta| meta.len() <= MAX_PRICE_BYTES as u64)
    {
        tokio::fs::read(&path)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .filter(|value| value["source"] == PRICE_SOURCE && value["models"].is_object())
    } else {
        None
    };
    let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
    if let Some(value) = &cached
        && value["fetchedAtMs"]
            .as_u64()
            .is_some_and(|at| at <= now + 30_000 && now.saturating_sub(at) < PRICE_TTL_MS)
    {
        return Ok(value["models"].clone());
    }
    let downloaded = async {
        let client = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::limited(2))
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(6))
            .build()?;
        let mut response = client.get(PRICE_SOURCE).send().await?.error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            anyhow::ensure!(
                bytes.len() + chunk.len() <= MAX_PRICE_BYTES,
                "公开价格表过大"
            );
            bytes.extend_from_slice(&chunk);
        }
        let models: Value = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(models.is_object(), "公开价格表格式无效");
        let stored = json!({"source":PRICE_SOURCE,"fetchedAtMs":now,"models":models});
        let _ = crate::settings::atomic_write(&path, &serde_json::to_vec(&stored)?);
        anyhow::Ok(models)
    }
    .await;
    downloaded.or_else(|error| cached.map(|value| value["models"].clone()).ok_or(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prices() -> Value {
        json!({"model-a":{"input_cost_per_token":0.000002,"output_cost_per_token":0.00001,
            "cache_read_input_token_cost":0.0000002},
            "model-b":{"input_cost_per_token":0.000003,"output_cost_per_token":0.000015,
            "cache_read_input_token_cost":0.0000003,"cache_creation_input_token_cost":0.00000375,
            "cache_creation_input_token_cost_above_1hr":0.000006}})
    }

    fn metrics(parts: Value, total: u64) -> Value {
        let mut metrics = json!({"turnId":"turn","turnTokens":total});
        attach(&mut metrics, parts, true);
        with_prices(metrics, &prices())
    }

    fn close(actual: &Value, expected: f64) {
        assert!(
            (actual.as_f64().unwrap() - expected).abs() < 1e-10,
            "{actual} != {expected}"
        );
    }

    #[test]
    fn session_metrics_cost_splits_cached_input_and_does_not_double_count_reasoning() {
        let part = responses_part(
            "model-a",
            &json!({"input_tokens":81600,"output_tokens":4800,
            "cached_input_tokens":63200,"reasoning_output_tokens":3200}),
            false,
            &Value::Null,
        );
        let value = metrics(json!([part]), 86400);
        let bill = &value["billing"];
        assert_eq!(bill["models"], json!(["model-a"]));
        assert_eq!(bill["rows"][0]["tokens"], 18400);
        assert_eq!(bill["rows"][1]["tokens"], 4800);
        assert_eq!(bill["rows"][2]["tokens"], 63200);
        close(&bill["rows"][0]["costUsd"], 0.0368);
        close(&bill["rows"][1]["costUsd"], 0.048);
        close(&bill["rows"][2]["costUsd"], 0.01264);
        close(&bill["totalCostUsd"], 0.09744);
        assert!(value.get("_billingParts").is_none() && value.get("_billingComplete").is_none());
    }

    #[test]
    fn session_metrics_cost_uses_each_request_model_and_native_cache_write_ttl() {
        let parts = claude_parts(&json!({"model":"model-b","usage":{
            "input_tokens":18400,"output_tokens":4800,"cache_read_input_tokens":63200,
            "cache_creation_input_tokens":10000,
            "cache_creation":{"ephemeral_5m_input_tokens":8000,"ephemeral_1h_input_tokens":2000}}}));
        let value = metrics(parts, 96400);
        assert_eq!(value["billing"]["rows"].as_array().unwrap().len(), 5);
        close(&value["billing"]["totalCostUsd"], 0.18816);

        let mut parts = vec![responses_part(
            "model-a",
            &json!({"input_tokens":100,"output_tokens":20,
            "cached_input_tokens":0}),
            false,
            &Value::Null,
        )];
        parts.push(responses_part(
            "model-b",
            &json!({"input_tokens":100,"output_tokens":20,
            "cached_input_tokens":0}),
            false,
            &Value::Null,
        ));
        let value = metrics(json!(parts), 240);
        assert_eq!(value["billing"]["models"], json!(["model-a", "model-b"]));
        close(&value["billing"]["totalCostUsd"], 0.001);
    }

    #[test]
    fn session_metrics_cost_keeps_missing_counts_prices_and_cache_ttl_unknown() {
        let part = responses_part(
            "missing-model",
            &json!({"inputTokens":100,"outputTokens":20,
            "cachedInputTokens":40}),
            true,
            &Value::Null,
        );
        let value = metrics(json!([part]), 120);
        assert!(value["billing"]["totalCostUsd"].is_null());
        assert_eq!(value["billing"]["rows"][0]["tokens"], 60);
        assert!(value["billing"]["rows"][0]["costUsd"].is_null());

        let part = responses_part(
            "model-a",
            &json!({"inputTokens":100,"outputTokens":20}),
            true,
            &Value::Null,
        );
        let value = metrics(json!([part]), 120);
        assert_eq!(value["billing"]["rows"].as_array().unwrap().len(), 1);
        assert_eq!(value["billing"]["rows"][0]["kind"], "output");
        assert!(value["billing"]["totalCostUsd"].is_null());

        let parts = claude_parts(&json!({"model":"model-b","usage":{
            "input_tokens":10,"output_tokens":10,"cache_read_input_tokens":0,
            "cache_creation_input_tokens":20}}));
        let value = metrics(parts, 40);
        assert_eq!(value["billing"]["rows"][2]["kind"], "cacheWrite");
        assert_eq!(value["billing"]["rows"][2]["tokens"], 20);
        assert!(value["billing"]["totalCostUsd"].is_null());
    }

    #[test]
    fn session_metrics_cost_preserves_native_write_total_until_both_ttl_buckets_are_known() {
        let mut message = json!({"model":"model-b","usage":{"input_tokens":10,"output_tokens":10,
            "cache_read_input_tokens":0,"cache_creation_input_tokens":20,"cache_creation":{"ephemeral_5m_input_tokens":8}}});
        let partial = claude_parts(&message);
        let unknown = metrics(partial.clone(), 40);
        assert_eq!(unknown["billing"]["rows"][2], json!({"kind":"cacheWrite","tokens":20,"costUsd":null}));
        assert!(unknown["billing"]["totalCostUsd"].is_null());
        message["usage"]["cache_creation"]["ephemeral_1h_input_tokens"] = json!(12);
        let resolved = merge_claude_parts(&partial, claude_parts(&message));
        for snapshot in [resolved.clone(), merge_claude_parts(&resolved, partial)] {
            let value = metrics(snapshot, 40);
            let rows = value["billing"]["rows"].as_array().unwrap();
            assert_eq!((rows.len(), &rows[2]["tokens"], &rows[3]["tokens"]), (4, &json!(8), &json!(12)));
            close(&value["billing"]["totalCostUsd"], 0.000282);
        }
    }

    #[test]
    fn session_metrics_cost_respects_context_threshold_and_service_tier() {
        let prices = json!({"model-a":{
            "input_cost_per_token":0.000002,"input_cost_per_token_priority":0.000004,
            "input_cost_per_token_above_200k_tokens_priority":0.000008,
            "output_cost_per_token_priority":0.00002,"cache_read_input_token_cost_priority":0.0000004}});
        let part = responses_part(
            "model-a",
            &json!({"input_tokens":220000,"output_tokens":100,
            "cached_input_tokens":200000}),
            false,
            &json!("priority"),
        );
        let mut value = json!({"turnTokens":220100});
        attach(&mut value, json!([part]), true);
        let value = with_prices(value, &prices);
        close(&value["billing"]["totalCostUsd"], 0.242);
        let mut prices_without_priority = prices.clone();
        prices_without_priority["model-a"]
            .as_object_mut()
            .unwrap()
            .remove("cache_read_input_token_cost_priority");
        let part = responses_part(
            "model-a",
            &json!({"input_tokens":220000,"output_tokens":100,
            "cached_input_tokens":200000}),
            false,
            &json!("priority"),
        );
        let mut value = json!({"turnTokens":220100});
        attach(&mut value, json!([part]), true);
        assert!(with_prices(value, &prices_without_priority)["billing"]["totalCostUsd"].is_null());
    }

    #[test]
    fn session_metrics_cost_does_not_price_incomplete_or_mismatched_snapshots() {
        let part = responses_part(
            "model-a",
            &json!({"input_tokens":100,"output_tokens":20,
            "cached_input_tokens":40}),
            false,
            &Value::Null,
        );
        assert!(metrics(json!([part.clone()]), 121)["billing"]["totalCostUsd"].is_null());
        let mut value =
            json!({"turnTokens":120,"inputTokens":100,"outputTokens":20,"cachedInputTokens":40});
        attach(&mut value, json!([part]), false);
        let value = with_prices(value, &prices());
        assert!(value["billing"]["totalCostUsd"].is_null());
        assert!(value["billing"]["models"].as_array().unwrap().is_empty());
        assert!(
            value["billing"]["rows"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["costUsd"].is_null())
        );
        let old_claude = with_prices(
            json!({"source":"claude-native","turnTokens":170,
            "inputTokens":150,"outputTokens":20,"cachedInputTokens":40}),
            &prices(),
        );
        assert!(
            old_claude["billing"]["rows"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["kind"] != "input")
        );
    }

    #[test]
    fn session_metrics_cost_zero_usage_is_zero_but_invalid_cache_is_not_input() {
        let part = responses_part(
            "missing",
            &json!({"input_tokens":0,"output_tokens":0,
            "cached_input_tokens":0}),
            false,
            &Value::Null,
        );
        close(&metrics(json!([part]), 0)["billing"]["totalCostUsd"], 0.0);
        let part = responses_part(
            "model-a",
            &json!({"input_tokens":10,"output_tokens":20,
            "cached_input_tokens":11}),
            false,
            &Value::Null,
        );
        let value = metrics(json!([part]), 30);
        assert_eq!(value["billing"]["rows"][0]["kind"], "output");
        assert!(value["billing"]["totalCostUsd"].is_null());
    }

    #[test]
    fn session_metrics_cost_matches_exact_protocol_keys_without_guessing_other_models() {
        let prices = json!({"gemini/gemini-example":{"input_cost_per_token":0.000001,
            "output_cost_per_token":0.000005,"cache_read_input_token_cost":0.0000001}});
        let part = responses_part(
            "gemini-example",
            &json!({"inputTokens":100,
            "outputTokens":10,"cachedInputTokens":50}),
            true,
            &Value::Null,
        );
        let mut value = json!({"turnTokens":110});
        attach(&mut value, json!([part]), true);
        close(
            &with_prices(value, &prices)["billing"]["totalCostUsd"],
            0.000105,
        );
        assert!(model_price(&prices, "gemini-example-new").is_none());
        assert!(model_price(&json!({"vertex_ai/gemini-example":{}}), "gemini-example").is_none());
    }

    #[test]
    fn session_metrics_cost_merges_claude_split_usage_without_losing_final_output() {
        let first = claude_parts(&json!({"model":"model-b","usage":{"input_tokens":100,
            "output_tokens":0,"cache_read_input_tokens":20,"cache_creation_input_tokens":0}}));
        let last = claude_parts(&json!({"model":"model-b","usage":{"input_tokens":0,
            "output_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}));
        let merged = merge_claude_parts(&first, last);
        let value = metrics(merged, 130);
        assert_eq!(value["billing"]["rows"][0]["tokens"], 100);
        assert_eq!(value["billing"]["rows"][1]["tokens"], 10);
        assert_eq!(value["billing"]["rows"][2]["tokens"], 20);
        close(&value["billing"]["totalCostUsd"], 0.000456);
    }
}
