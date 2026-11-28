//! 同一次压缩的型号和计时证据。未知字段留空，不能借用今天的设置补旧记录。
use serde_json::{Value, json};

pub(crate) fn model(s: &str) -> Option<&str> {
    (!s.trim().is_empty()
        && s.len() <= 200
        && !s.chars().any(char::is_control)
        && !matches!(s.trim(), "<synthetic>" | "unknown"))
    .then(|| s.trim())
}

pub(crate) fn rank(source: &str) -> u8 {
    match source {
        "native" => 3,
        "specified" => 2,
        "inferred" => 1,
        _ => 0,
    }
}

pub(crate) fn set_model(item: &mut Value, name: Option<&str>, source: &str) {
    let Some(name) = name.and_then(model) else {
        return;
    };
    if rank(source) > 0
        && (item["compactionModel"].as_str().and_then(model).is_none()
            || rank(source) > rank(item["compactionModelSource"].as_str().unwrap_or("")))
    {
        item["compactionModel"] = json!(name);
        item["compactionModelSource"] = json!(source);
    }
}

pub(crate) fn merge_recorded_time(target: &mut Value, facts: &Value) {
    let priority = |item: &Value| match item["recordedAtSource"].as_str() {
        Some("nativeEnd") => 5,
        Some("record") => 4,
        Some("observedEnd") => 3,
        Some("nativeStart") => 2,
        Some("observedStart") => 1,
        _ => 0,
    };
    if facts["recordedAtMs"].as_u64().is_some_and(|v| v > 0)
        && (target["recordedAtMs"].as_u64().is_none_or(|v| v == 0)
            || priority(facts) > priority(target))
    {
        target["recordedAtMs"] = facts["recordedAtMs"].clone();
        target["recordedAtSource"] = facts["recordedAtSource"].clone();
    }
}

/// 只接受同条目的完整原生时间对，或同一连接观测到的时间对，不混用时钟也不以整轮结束代替。
pub(crate) fn timing(item: &mut Value) {
    let ms = |key: &str| {
        item[key]
            .as_u64()
            .filter(|v| *v > 0 && *v <= 8_640_000_000_000_000)
    };
    // Claude 的旧控制记录把 SDK 接收时间和转录时间写在这两个字段里；只有显式 duration 才证明其来源。
    let native = (item["backend"] != "claude")
        .then(|| ms("startedAtMs").zip(ms("completedAtMs")))
        .flatten();
    let observed = ms("hoverObservedStartedAtMs").zip(ms("hoverObservedCompletedAtMs"));
    let times = [
        (ms("completedAtMs"), "nativeEnd"),
        (ms("hoverObservedCompletedAtMs"), "observedEnd"),
        (ms("startedAtMs"), "nativeStart"),
        (ms("hoverObservedStartedAtMs"), "observedStart"),
    ];
    if ms("recordedAtMs").is_some() && item["recordedAtSource"].is_null() {
        item["recordedAtSource"] = json!("record");
    }
    if let Some((at, origin)) = times.into_iter().find_map(|(at, s)| at.map(|n| (n, s))) {
        merge_recorded_time(item, &json!({"recordedAtMs":at,"recordedAtSource":origin}));
    }
    let elapsed = |(a, b): (u64, u64)| b.checked_sub(a).filter(|n| *n > 0);
    // 原生明确给零是有效数据；两个相同时间点、开始后重连或未执行都不能算出“耗时零秒”。
    let explicit = item["durationMs"].as_u64();
    let measured = item["compactionDurationSource"] == "observed";
    let best = explicit
        .filter(|_| !measured)
        .map(|n| (n, "native"))
        .or_else(|| native.and_then(elapsed).map(|n| (n, "native")))
        .or_else(|| {
            explicit
                .filter(|n| measured && *n > 0)
                .or_else(|| observed.and_then(elapsed))
                .map(|n| (n, "observed"))
        });
    if let Some((n, source)) = best.filter(|_| item["outcome"] != "noop") {
        item["durationMs"] = json!(n);
        item["compactionDurationSource"] = json!(source);
    } else if let Some(fields) = item.as_object_mut() {
        fields.remove("durationMs");
        fields.remove("compactionDurationSource");
    }
}
