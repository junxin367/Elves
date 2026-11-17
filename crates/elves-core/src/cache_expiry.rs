//! 输入区缓存倒计时的证据与默认规则；不改变代理的压缩路由。
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheExpirySource {
    Response,
    Request,
    ModelDefault,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheExpiry {
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
    pub ttl_ms: u64,
    pub source: CacheExpirySource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCacheExpiry {
    pub expiry: Option<CacheExpiry>,
    pub model: String,
    pub turn_id: Option<String>,
    pub completed_at_ms: u64,
}

fn ttl(value: &str) -> Option<u64> {
    match value.trim().to_ascii_lowercase().as_str() {
        "5m" | "mixed" => Some(5 * MINUTE),
        "30m" => Some(30 * MINUTE),
        "1h" => Some(HOUR),
        "12h" => Some(12 * HOUR),
        "24h" => Some(24 * HOUR),
        _ => None,
    }
}

fn number(value: &Value, pointers: &[&str]) -> u64 {
    pointers
        .iter()
        .filter_map(|path| value.pointer(path).and_then(Value::as_u64))
        .max()
        .unwrap_or(0)
}

fn response_ttl(usage: &Value) -> Option<u64> {
    // 用户要求响应证据优先；混合缓存取最先过期的一段，不能把部分 1h 写入当作全部缓存均可保留 1h。
    [
        (
            5 * MINUTE,
            &[
                "/cache_creation/ephemeral_5m_input_tokens",
                "/cache_creation_5m_input_tokens",
            ][..],
        ),
        (
            30 * MINUTE,
            &[
                "/input_tokens_details/cache_write_tokens_details/ttl_30m_tokens",
                "/prompt_tokens_details/cache_write_tokens_details/ttl_30m_tokens",
            ][..],
        ),
        (
            HOUR,
            &[
                "/cache_creation/ephemeral_1h_input_tokens",
                "/cache_creation_1h_input_tokens",
            ][..],
        ),
        (
            12 * HOUR,
            &[
                "/input_tokens_details/cache_write_tokens_details/ttl_12h_tokens",
                "/prompt_tokens_details/cache_write_tokens_details/ttl_12h_tokens",
            ][..],
        ),
    ]
    .into_iter()
    .filter_map(|(duration, paths)| (number(usage, paths) > 0).then_some(duration))
    .min()
    .or_else(|| usage["cache_ttl"].as_str().and_then(ttl))
}

pub(crate) fn request_ttl(request: &Value) -> Option<Option<u64>> {
    if let Some(value) = request.pointer("/prompt_cache_options/ttl") {
        return Some(value.as_str().and_then(ttl));
    }
    if let Some(value) = request.get("prompt_cache_retention") {
        // OpenAI 的 in_memory 是不活动窗口，24h 是保留上限，均不是承诺存活时长。
        // 分别采用 5 分钟、30 分钟估算；不能显示确定的 24 小时剩余寿命。
        return Some(match value.as_str() {
            Some("in_memory") => Some(5 * MINUTE),
            Some("24h") => Some(30 * MINUTE),
            _ => None,
        });
    }
    let mut controls = vec![request.get("cache_control")];
    for field in ["tools", "system", "messages"] {
        for item in request[field].as_array().into_iter().flatten() {
            controls.push(item.get("cache_control"));
            for part in item["content"].as_array().into_iter().flatten() {
                controls.push(part.get("cache_control"));
            }
        }
    }
    let controls: Vec<_> = controls.into_iter().flatten().collect();
    if controls.is_empty() {
        return None;
    }
    let mut shortest = None;
    for control in controls {
        if control["type"] != "ephemeral" {
            return Some(None);
        }
        let duration = match control.get("ttl") {
            None => 5 * MINUTE,
            Some(value) => {
                let Some(duration) = value.as_str().and_then(ttl) else {
                    return Some(None);
                };
                duration
            }
        };
        shortest = Some(shortest.map_or(duration, |old: u64| old.min(duration)));
    }
    Some(shortest)
}

fn model_default(model: &str) -> Option<u64> {
    let model = model.trim().to_ascii_lowercase().replace([' ', '_'], "-");
    if model.starts_with("claude-") {
        // CLI 的订阅 1h 分支受配置、模型和超额用量影响；无原生明细时只采用 Claude 的默认 5m，不能凭登录方式推断 1h。
        return Some(5 * MINUTE);
    }
    if let Some(version) = model.strip_prefix("gpt-") {
        let mut pieces = version.split(['.', '-']);
        let major = pieces
            .next()?
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u32>()
            .ok()?;
        let minor = pieces
            .next()
            .and_then(|part| part.parse::<u32>().ok())
            .unwrap_or(0);
        // GPT-5.6 起默认 30m；旧模型受组织保留策略影响，缺少请求配置时采用保守的 5m 不活动窗口。
        return Some(if major > 5 || major == 5 && minor >= 6 {
            30 * MINUTE
        } else {
            5 * MINUTE
        });
    }
    ["o1", "o3", "o4"]
        .iter()
        .any(|prefix| model == *prefix || model.starts_with(&format!("{prefix}-")))
        .then_some(5 * MINUTE)
}

/// 合并同一响应的流式用量片段，只保留缓存推断需要的字段，不保存正文、工具参数或凭据。
pub(crate) fn merge_cache_usage(target: &mut Value, usage: &Value) {
    if !target.is_object() {
        *target = serde_json::json!({});
    }
    for key in [
        "input_tokens",
        "prompt_tokens",
        "inputTokens",
        "cache_read_input_tokens",
        "cached_input_tokens",
        "cachedInputTokens",
        "cached_tokens",
        "prompt_cache_hit_tokens",
        "cache_creation_input_tokens",
        "cache_creation_5m_input_tokens",
        "cache_creation_1h_input_tokens",
        "ephemeral_5m_input_tokens",
        "ephemeral_1h_input_tokens",
        "cache_write_tokens",
        "ttl_30m_tokens",
        "ttl_12h_tokens",
    ] {
        if let Some(value) = usage[key].as_u64() {
            target[key] = Value::from(value.max(target[key].as_u64().unwrap_or(0)));
        }
    }
    for key in [
        "cache_creation",
        "input_tokens_details",
        "prompt_tokens_details",
        "cache_write_tokens_details",
    ] {
        if usage[key].is_object() {
            merge_cache_usage(&mut target[key], &usage[key]);
        }
    }
    if let Some(value) = usage["cache_ttl"].as_str()
        && ttl(value).is_some()
    {
        let duration = ttl(value).unwrap();
        if target["cache_ttl"]
            .as_str()
            .and_then(ttl)
            .is_none_or(|old| duration < old)
        {
            target["cache_ttl"] = Value::from(value);
        }
    }
    for part in usage["iterations"].as_array().into_iter().flatten() {
        merge_cache_usage(target, part);
    }
}

/// 同一缓存身份的成功请求才可更新；轮询、快照读取和页面重绘不能调用本函数续期。
pub fn infer_cache_expiry(
    protocol: &str,
    model: &str,
    request: &Value,
    usage: &Value,
    observed_at_ms: u64,
    previous: Option<CacheExpiry>,
) -> Option<CacheExpiry> {
    let mut evidence = Value::Null;
    merge_cache_usage(&mut evidence, usage);
    infer_cache_expiry_with_hint(
        protocol,
        model,
        request_ttl(request),
        &evidence,
        observed_at_ms,
        previous,
    )
}

pub(crate) fn infer_cache_expiry_with_hint(
    protocol: &str,
    model: &str,
    request_hint: Option<Option<u64>>,
    usage: &Value,
    observed_at_ms: u64,
    previous: Option<CacheExpiry>,
) -> Option<CacheExpiry> {
    let response_ttl = response_ttl(usage);
    let read = number(
        usage,
        &[
            "/cache_read_input_tokens",
            "/cached_input_tokens",
            "/cachedInputTokens",
            "/input_tokens_details/cached_tokens",
            "/prompt_tokens_details/cached_tokens",
            "/prompt_cache_hit_tokens",
        ],
    );
    let written = number(
        usage,
        &[
            "/cache_creation_input_tokens",
            "/input_tokens_details/cache_write_tokens",
            "/prompt_tokens_details/cache_write_tokens",
            "/input_tokens_details/cache_write_tokens_details/ttl_30m_tokens",
            "/input_tokens_details/cache_write_tokens_details/ttl_12h_tokens",
            "/prompt_tokens_details/cache_write_tokens_details/ttl_30m_tokens",
            "/prompt_tokens_details/cache_write_tokens_details/ttl_12h_tokens",
        ],
    );
    let automatic_write = protocol != "anthropic"
        && model_default(model).is_some()
        && number(usage, &["/input_tokens", "/prompt_tokens", "/inputTokens"]) >= 1024;
    if response_ttl.is_none() && read == 0 && written == 0 && !automatic_write {
        return None;
    }
    let (duration, source) = if let Some(duration) = response_ttl {
        (duration, CacheExpirySource::Response)
    } else if let Some(duration) = request_hint {
        (duration?, CacheExpirySource::Request)
    } else if let Some(previous) = previous.filter(|_| read > 0 && written == 0) {
        (previous.ttl_ms, previous.source)
    } else {
        (model_default(model)?, CacheExpirySource::ModelDefault)
    };
    let expires_at_ms = if duration == 12 * HOUR && read > 0 && written == 0 {
        // OpenAI 12h 命中只延长到“原截止时间”和“本次读取后 30m”较晚者，不重新发放 12h。
        previous
            .map(|previous| previous.expires_at_ms)
            .unwrap_or(0)
            .max(observed_at_ms.checked_add(30 * MINUTE)?)
    } else {
        observed_at_ms.checked_add(duration)?
    };
    Some(CacheExpiry {
        observed_at_ms,
        expires_at_ms,
        ttl_ms: duration,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cache_expiry_prefers_response_then_request_then_known_model_default() {
        let usage = json!({"cache_read_input_tokens":100,"cache_creation_input_tokens":1,
            "cache_creation":{"ephemeral_1h_input_tokens":1}});
        let request = json!({"cache_control":{"type":"ephemeral","ttl":"5m"}});
        let expiry = infer_cache_expiry(
            "anthropic",
            "claude-haiku-5-5",
            &request,
            &usage,
            1000,
            None,
        )
        .unwrap();
        assert_eq!(expiry.ttl_ms, HOUR);
        assert_eq!(expiry.source, CacheExpirySource::Response);
        let usage = json!({"cache_read_input_tokens":100});
        let expiry = infer_cache_expiry("anthropic", "claude-haiku-5-5",
            &json!({"messages":[{"role":"user","content":[{"type":"text","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]}),
            &usage, 1000, None).unwrap();
        assert_eq!(expiry.expires_at_ms, 1000 + HOUR);
        assert_eq!(expiry.source, CacheExpirySource::Request);
        let expiry = infer_cache_expiry(
            "anthropic",
            "claude-haiku-5-5",
            &Value::Null,
            &usage,
            1000,
            None,
        )
        .unwrap();
        assert_eq!(expiry.ttl_ms, 5 * MINUTE);
        assert_eq!(expiry.source, CacheExpirySource::ModelDefault);
    }

    #[test]
    fn cache_expiry_uses_shortest_mixed_lifetime_and_handles_iterations() {
        let expiry = infer_cache_expiry(
            "anthropic",
            "claude-opus-5-5",
            &Value::Null,
            &json!({"iterations":[
                {"cache_creation":{"ephemeral_1h_input_tokens":50}},
                {"cache_creation":{"ephemeral_5m_input_tokens":100}}
            ]}),
            1000,
            None,
        )
        .unwrap();
        assert_eq!(expiry.ttl_ms, 5 * MINUTE);
        let expiry = infer_cache_expiry("responses", "gpt-6.1-sol", &Value::Null,
            &json!({"input_tokens_details":{"cache_write_tokens_details":{"ttl_30m_tokens":5,"ttl_12h_tokens":10}}}),
            1000, None).unwrap();
        assert_eq!(expiry.ttl_ms, 30 * MINUTE);
    }

    #[test]
    fn cache_expiry_new_openai_defaults_and_retention_have_distinct_semantics() {
        let usage = json!({"input_tokens":2048,"input_tokens_details":{"cached_tokens":1024}});
        for (model, duration) in [
            ("gpt-5.6", 30 * MINUTE),
            ("gpt-6.1-sol", 30 * MINUTE),
            ("gpt 6.1 sol", 30 * MINUTE),
            ("gpt-5.5", 5 * MINUTE),
            ("gpt-4o", 5 * MINUTE),
        ] {
            assert_eq!(
                infer_cache_expiry("responses", model, &Value::Null, &usage, 0, None)
                    .unwrap()
                    .ttl_ms,
                duration
            );
        }
        for (retention, duration) in [("in_memory", 5 * MINUTE), ("24h", 30 * MINUTE)] {
            let expiry = infer_cache_expiry(
                "responses",
                "gpt-5.5",
                &json!({"prompt_cache_retention":retention}),
                &usage,
                0,
                None,
            )
            .unwrap();
            assert_eq!(expiry.ttl_ms, duration);
            assert_eq!(expiry.source, CacheExpirySource::Request);
        }
    }

    #[test]
    fn cache_expiry_twelve_hour_reads_do_not_restart_a_twelve_hour_window() {
        let request = json!({"prompt_cache_options":{"ttl":"12h"}});
        let write =
            json!({"input_tokens_details":{"cache_write_tokens_details":{"ttl_12h_tokens":100}}});
        let written =
            infer_cache_expiry("responses", "gpt-6.1-sol", &request, &write, 1000, None).unwrap();
        let read = json!({"input_tokens_details":{"cached_tokens":100}});
        let hit = infer_cache_expiry(
            "responses",
            "gpt-6.1-sol",
            &request,
            &read,
            HOUR,
            Some(written),
        )
        .unwrap();
        assert_eq!(hit.expires_at_ms, written.expires_at_ms);
        let later = infer_cache_expiry(
            "responses",
            "gpt-6.1-sol",
            &request,
            &read,
            12 * HOUR,
            Some(hit),
        )
        .unwrap();
        assert_eq!(later.expires_at_ms, 12 * HOUR + 30 * MINUTE);
        let first_hit =
            infer_cache_expiry("responses", "gpt-6.1-sol", &request, &read, 1000, None).unwrap();
        assert_eq!(first_hit.expires_at_ms, 1000 + 30 * MINUTE);
    }

    #[test]
    fn cache_expiry_unknown_models_disabled_policies_and_missing_evidence_stay_unknown() {
        let read = json!({"cache_read_input_tokens":100});
        assert!(
            infer_cache_expiry("anthropic", "unknown-model", &Value::Null, &read, 0, None)
                .is_none()
        );
        assert!(
            infer_cache_expiry(
                "anthropic",
                "claude-opus-5-5",
                &Value::Null,
                &json!({"input_tokens":2048}),
                0,
                None
            )
            .is_none()
        );
        for request in [
            json!({"prompt_cache_options":{"ttl":"disabled"}}),
            json!({"cache_control":{"type":"ephemeral","ttl":"unknown"}}),
            json!({"cache_control":{"type":"disabled"}}),
        ] {
            assert!(
                infer_cache_expiry("anthropic", "claude-opus-5-5", &request, &read, 0, None)
                    .is_none()
            );
        }
        // 工具输入 schema 内的同名字段不是请求缓存策略。
        let request =
            json!({"tools":[{"input_schema":{"cache_control":{"type":"ephemeral","ttl":"1h"}}}]});
        assert_eq!(
            infer_cache_expiry("anthropic", "claude-opus-5-5", &request, &read, 0, None)
                .unwrap()
                .ttl_ms,
            5 * MINUTE
        );
        assert!(
            infer_cache_expiry(
                "responses",
                "gpt-6.1-sol",
                &Value::Null,
                &json!({}),
                0,
                None
            )
            .is_none()
        );
    }
}
