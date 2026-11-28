//! 设置按实际实例隔离；仅修改明确支持的原生键，远程专属偏好不写入原生配置。
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use std::{collections::{BTreeMap, BTreeSet}, sync::OnceLock};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::permissions::{self, ConfigFile};
use super::{ApiError, HarnessAuthorization, WebState};

#[path = "instance_settings_options.rs"]
mod options;

const PREFERENCES_FILE: &str = "remote-instance-preferences.json";
// 用户要求三端统一用百分比设置自动压缩阈值，范围统一为 50～98%（留空恢复原生默认）。
const AUTO_COMPACT_MIN_PERCENT: u64 = 50;
const AUTO_COMPACT_MAX_PERCENT: u64 = 98;
const CLAUDE_AUTOCOMPACT_BUFFER_TOKENS: u64 = 33_000;
// 用户要求在「缓存与压缩」加入上下文大小与缓存时间：Codex 写 model_context_window，Claude 写 env.CLAUDE_CODE_AUTO_COMPACT_WINDOW（≥100k）与 env.CLAUDE_CODE_PROMPT_CACHE_TTL。
const CODEX_CONTEXT_WINDOW_MIN: u64 = 32_000;
const CLAUDE_CONTEXT_WINDOW_MIN: u64 = 100_000;
const CONTEXT_WINDOW_MAX: u64 = 10_000_000;
const ANTIGRAVITY_MIN_AUTO_COMPACT_VERSION: (u64, u64, u64) = (1, 5, 15);
const MAX_CODEX_CATALOG_BYTES: usize = 4 * 1024 * 1024;
type HelpCache = BTreeMap<String, (Instant, String)>;
static CLAUDE_HELP: OnceLock<Mutex<HelpCache>> = OnceLock::new();
type CapabilityCache = BTreeMap<String, (Instant, Value)>;
static CLAUDE_OPTIONS: OnceLock<Mutex<CapabilityCache>> = OnceLock::new();
type AgyVersionCache = BTreeMap<String, (Instant, Result<(u64, u64, u64), String>)>;
static ANTIGRAVITY_VERSION: OnceLock<Mutex<AgyVersionCache>> = OnceLock::new();

// 用户要求核对真实 Claude 实例；与 WSL 配置写入程序的布尔键白名单保持一致，不探测任意用户键。
const CLAUDE_BOOLEAN_OPTIONS: &[&str] = &[
    "syncClaudeAiSkills",
    "syncClaudeAiPlugins",
    "disableBundledSkills",
    "skipDangerousModePermissionPrompt",
    "switchModelsOnFlag",
];

fn preferences(directory: &Path) -> Result<(Value, String), ApiError> {
    let (text, revision) = permissions::read_file(&directory.join(PREFERENCES_FILE))?;
    let value = if text.trim().is_empty() {
        json!({"instances":{}})
    } else {
        serde_json::from_str::<Value>(&text)
            .map_err(|_| ApiError::new(422, "实例偏好文件格式无效，未覆盖原文件"))?
    };
    if !value.is_object() || !value["instances"].is_object() {
        return Err(ApiError::new(422, "实例偏好文件格式无效，未覆盖原文件"));
    }
    Ok((value, revision))
}

fn hidden_model_ids(value: &Value) -> Result<Vec<String>, ApiError> {
    if value.is_null() {
        return Ok(vec![]);
    }
    // 单实例最多 512 个隐藏 ID；每项沿用原生模型字段的 200 字节上限，避免把配置全文误存为名单。
    let invalid = || ApiError::new(400, "模型隐藏名单需为不重复的模型 ID 列表，最多 512 项");
    let rows = value.as_array().filter(|rows| rows.len() <= 512).ok_or_else(invalid)?;
    let mut seen = BTreeSet::new();
    rows.iter().map(|row| {
        let id = row.as_str().filter(|id| !id.is_empty() && id.len() <= 200
            && !id.chars().any(char::is_control)).ok_or_else(invalid)?;
        if !seen.insert(id) {
            return Err(invalid());
        }
        Ok(id.to_owned())
    }).collect()
}

fn apply_hidden_model_visibility(catalog: &mut Value, hidden: &[String]) {
    // 用户要求所有会话实例（Codex、Claude、Antigravity）统一默认显示全部模型并允许按实例隐藏；
    // 仅在返回视图上叠加 hiddenByInstance，保留原生完整目录及能力，隐藏不能改变已有会话的执行模型。
    if let Some(rows) = catalog["data"].as_array_mut() {
        for row in rows {
            row["hiddenByInstance"] = json!(row["model"]
                .as_str()
                .is_some_and(|id| hidden.iter().any(|hidden| hidden == id)));
        }
    }
}

pub(super) fn decorate_codex_model_visibility(
    directory: &Path,
    instance: &str,
    mut catalog: Value,
) -> Result<Value, ApiError> {
    let (preference, _) = preferences(directory)?;
    let hidden = hidden_model_ids(&preference["instances"][instance]["hiddenModelIds"])?;
    apply_hidden_model_visibility(&mut catalog, &hidden);
    Ok(catalog)
}

pub(super) fn decorate_claude_model_visibility(
    directory: &Path,
    instance: &str,
    mut catalog: Value,
) -> Result<Value, ApiError> {
    let (preference, _) = preferences(directory)?;
    let hidden = hidden_model_ids(&preference["instances"][instance]["hiddenModelIds"])?;
    // 用户要求移除设置页「实例默认值」；仅叠加显示模型偏好，不再覆写原生默认深度。
    apply_hidden_model_visibility(&mut catalog, &hidden);
    Ok(catalog)
}

pub(super) fn antigravity_permission_view(
    directory: &Path,
    instance: &str,
) -> Result<(String, String), ApiError> {
    let (preference, revision) = preferences(directory)?;
    let value = &preference["instances"][instance]["defaultPermissionMode"];
    let mode = match value {
        Value::Null => String::new(),
        Value::String(s) if s.is_empty() || permissions::ANTIGRAVITY_MODES.contains(&s.as_str()) => {
            s.clone()
        }
        _ => {
            return Err(ApiError::new(
                422,
                "Antigravity 默认权限配置无效，请重新读取实例权限",
            ));
        }
    };
    Ok((mode, revision))
}

pub(super) fn antigravity_default_permission_mode(
    directory: &Path,
    instance: &str,
) -> Result<String, ApiError> {
    let (mode, _) = antigravity_permission_view(directory, instance)?;
    Ok(if mode.is_empty() {
        "default".into()
    } else {
        mode
    })
}

pub(super) async fn save_antigravity_permission_mode(
    directory: &Path,
    instance: &str,
    mode: &str,
    expected: &str,
    authorization: &HarnessAuthorization,
) -> Result<(), ApiError> {
    let value = if mode.is_empty() {
        Value::Null
    } else {
        json!(mode)
    };
    save_preference(
        directory,
        instance,
        "defaultPermissionMode",
        &value,
        expected,
        authorization,
    )
    .await
}

/// 为 Antigravity 官方 ACP 模型目录叠加当前实例的显示名单，不修改 ACP 原始缓存或原生默认值。
pub(super) fn decorate_antigravity_model_visibility(
    directory: &Path,
    instance: &str,
    mut catalog: Value,
) -> Result<Value, ApiError> {
    let (preference, _) = preferences(directory)?;
    let hidden = hidden_model_ids(&preference["instances"][instance]["hiddenModelIds"])?;
    apply_hidden_model_visibility(&mut catalog, &hidden);
    Ok(catalog)
}

fn validate_hidden_model_selection(
    ids: &[String],
    previous: &[String],
    catalog: Option<&Value>,
) -> Result<(), ApiError> {
    if ids.iter().any(|id| !previous.contains(id) && !catalog.is_some_and(|catalog| {
        catalog["data"].as_array().is_some_and(|rows| rows.iter().any(|row| row["model"] == id.as_str()))
    })) {
        return Err(ApiError::new(409, "待隐藏模型已不在当前实例目录中，请重新读取；恢复显示不受影响"));
    }
    Ok(())
}

pub(super) fn ultra_enabled(directory: &Path, instance: &str) -> Result<bool, ApiError> {
    let (value, _) = preferences(directory)?;
    // 用户要求 Codex 默认不开放 Ultra；仅当前实例的显式开启值允许显示，不能按模型支持情况自动开启。
    match &value["instances"][instance]["ultraReasoningEnabled"] {
        Value::Null => Ok(false),
        Value::Bool(enabled) => Ok(*enabled),
        _ => Err(ApiError::new(
            422,
            "Ultra 思考深度配置无效，请重新读取实例设置",
        )),
    }
}

pub(super) fn persistent_sides_enabled(directory: &Path, instance: &str) -> Result<bool, ApiError> {
    let (value, _) = preferences(directory)?;
    // 用户要求持久侧边默认关闭；这是按实例共享的 Harness 偏好，只影响之后新建的侧边。
    match &value["instances"][instance]["persistentSideThreads"] {
        Value::Null => Ok(false),
        Value::Bool(enabled) => Ok(*enabled),
        _ => Err(ApiError::new(422, "持久侧边配置无效，请重新读取实例设置")),
    }
}

pub(super) fn filter_model_rows(models: &mut [Value], enabled: bool) {
    if enabled {
        return;
    }
    for model in models {
        if let Some(efforts) = model["supportedReasoningEfforts"].as_array_mut() {
            efforts.retain(|effort| effort["reasoningEffort"] != "ultra");
        }
        if model["defaultReasoningEffort"] == "ultra" {
            model["defaultReasoningEffort"] = model["supportedReasoningEfforts"]
                .as_array()
                .and_then(|efforts| efforts.last())
                .map(|effort| effort["reasoningEffort"].clone())
                .unwrap_or(Value::Null);
        }
    }
}

pub(super) fn filter_models(value: &mut Value, enabled: bool) {
    if let Some(models) = value["data"].as_array_mut() {
        filter_model_rows(models, enabled);
    }
    value["ultraReasoningEnabled"] = json!(
        enabled
            && value["data"]
                .as_array()
                .is_some_and(|models| models.iter().any(|model| {
                    model["supportedReasoningEfforts"]
                        .as_array()
                        .is_some_and(|efforts| {
                            efforts
                                .iter()
                                .any(|effort| effort["reasoningEffort"] == "ultra")
                        })
                }))
    );
    if !enabled && value["defaultReasoningEffort"] == "ultra" {
        value["defaultReasoningEffort"] = Value::Null;
    }
}

fn validate_auto_compact_percent(backend: &str, value: &Value) -> Result<Option<u64>, ApiError> {
    if value.is_null() {
        return Ok(None);
    }
    if let Some(percent) = value.as_u64()
        && (AUTO_COMPACT_MIN_PERCENT..=AUTO_COMPACT_MAX_PERCENT).contains(&percent)
    {
        return Ok(Some(percent));
    }
    let label = match backend {
        "codex" => "Codex",
        "antigravity" => "Antigravity",
        _ => "Claude",
    };
    Err(ApiError::new(
        400,
        format!(
            "{label} 自动压缩阈值必须为 {AUTO_COMPACT_MIN_PERCENT}～{AUTO_COMPACT_MAX_PERCENT} 的整数，或留空使用原生默认"
        ),
    ))
}

fn format_tokens_with_commas(tokens: u64) -> String {
    let digits = tokens.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn read_codex_catalog_entries(codex_home: &Path, config: &Value) -> Vec<(String, u64)> {
    let effective = config.get("config").unwrap_or(config);
    let override_window = effective["model_context_window"]
        .as_u64()
        .filter(|&value| value > 0);
    let mut candidates = Vec::new();
    if let Some(catalog_path) = effective["model_catalog_json"]
        .as_str()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        let path = Path::new(catalog_path);
        candidates.push(if path.is_absolute() {
            path.to_path_buf()
        } else {
            codex_home.join(path)
        });
    }
    candidates.push(codex_home.join("models_cache.json"));
    for candidate in candidates {
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !metadata.is_file() || metadata.len() as usize > MAX_CODEX_CATALOG_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&candidate) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let Some(models) = parsed["models"].as_array() else {
            continue;
        };
        let mut rows = Vec::new();
        for entry in models {
            let Some(slug) = entry["slug"].as_str().filter(|slug| !slug.is_empty()) else {
                continue;
            };
            let base = entry["context_window"]
                .as_u64()
                .filter(|&value| value > 0)
                .or_else(|| entry["max_context_window"].as_u64().filter(|&value| value > 0));
            let max_window = entry["max_context_window"].as_u64().filter(|&value| value > 0);
            // 与 openai/codex models-manager `with_config_overrides` 保持一致：model_context_window 不超过模型 max_context_window。
            let resolved = match override_window {
                Some(cfg) => Some(max_window.map_or(cfg, |max| cfg.min(max))),
                None => base,
            };
            if let Some(window) = resolved {
                rows.push((slug.to_owned(), window));
            }
        }
        if !rows.is_empty() {
            return rows;
        }
    }
    Vec::new()
}

fn codex_window_summary(codex_home: &Path, config: &Value) -> (Option<u64>, usize) {
    let entries = read_codex_catalog_entries(codex_home, config);
    if !entries.is_empty() {
        let mut counts: BTreeMap<u64, usize> = BTreeMap::new();
        for (_, window) in entries {
            *counts.entry(window).or_insert(0) += 1;
        }
        let primary = counts
            .iter()
            .max_by_key(|&(window, count)| (*count, *window))
            .map(|(window, _)| *window);
        return (primary, counts.len());
    }
    let effective = config.get("config").unwrap_or(config);
    match effective["model_context_window"]
        .as_u64()
        .filter(|&value| value > 0)
    {
        Some(window) => (Some(window), 1),
        None => (None, 0),
    }
}

pub(super) fn codex_model_context_window(
    codex_home: &Path,
    config: &Value,
    model: Option<&str>,
) -> Option<u64> {
    let effective = config.get("config").unwrap_or(config);
    let target = model
        .filter(|value| !value.is_empty())
        .or_else(|| effective["model"].as_str().filter(|value| !value.is_empty()));
    let entries = read_codex_catalog_entries(codex_home, effective);
    if let Some(target) = target {
        if let Some((_, window)) = entries
            .iter()
            .find(|(slug, _)| slug == target || slug.eq_ignore_ascii_case(target))
        {
            return Some(*window);
        }
    }
    if let Some(window) = effective["model_context_window"]
        .as_u64()
        .filter(|&value| value > 0)
    {
        return Some(window);
    }
    codex_window_summary(codex_home, effective).0
}

pub(super) fn apply_codex_auto_compact(
    directory: &Path,
    instance: &str,
    codex_home: &Path,
    config: &Value,
    model: Option<&str>,
    params: &mut Value,
) {
    // 用户要求 Codex 使用原生 model_auto_compact_token_limit（自动触发而非轮末），
    // 按 Elves 实例偏好保存的百分比（50%～98%）结合当前会话模型上下文窗口动态换算下发。
    let Ok((preference, _)) = preferences(directory) else {
        return;
    };
    let Ok(Some(percent)) = validate_auto_compact_percent(
        "codex",
        &preference["instances"][instance]["autoCompactPercent"],
    ) else {
        return;
    };
    let Some(window) = codex_model_context_window(codex_home, config, model) else {
        return;
    };
    let limit = ((window as u128 * percent as u128) / 100).max(1) as u64;
    if !params["config"].is_object() {
        params["config"] = json!({});
    }
    params["config"]["model_auto_compact_token_limit"] = json!(limit);
}

pub(super) fn antigravity_auto_compact_override(
    directory: &Path,
    instance: &str,
) -> Result<Option<String>, ApiError> {
    let (preference, _) = preferences(directory)?;
    let percent = validate_auto_compact_percent(
        "antigravity",
        &preference["instances"][instance]["autoCompactPercent"],
    )?;
    Ok(percent.map(|value| format!("{value}%")))
}

fn parse_semver(text: &str) -> Option<(u64, u64, u64)> {
    for token in text.split(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | '(' | ')')) {
        let candidate = token.trim().trim_start_matches(['v', 'V']);
        let core = candidate.split(['-', '+']).next().unwrap_or("");
        let mut parts = core.split('.');
        let (Some(a), Some(b), Some(c), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if let (Ok(major), Ok(minor), Ok(patch)) =
            (a.parse::<u64>(), b.parse::<u64>(), c.parse::<u64>())
        {
            return Some((major, minor, patch));
        }
    }
    None
}

async fn antigravity_version(
    config: &super::antigravity::acp::EnvConfig,
) -> Result<(u64, u64, u64), String> {
    let key = format!("{}:{}", config.instance_id(), config.executable);
    let cache = ANTIGRAVITY_VERSION.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Some((at, result)) = cache.lock().await.get(&key)
        && at.elapsed() < Duration::from_secs(60)
    {
        return result.clone();
    }
    if let Err(err) = config.validate() {
        return Err(err.to_string());
    }
    #[cfg(test)]
    if config.executable == "/fixture/agy" {
        return Ok(ANTIGRAVITY_MIN_AUTO_COMPACT_VERSION);
    }
    let command = if config.is_local() {
        let mut cmd = tokio::process::Command::new(&config.executable);
        cmd.arg("--version");
        cmd
    } else {
        let mut cmd = tokio::process::Command::new("wsl.exe");
        cmd.args([
            "--distribution",
            &config.distribution,
            "--user",
            &config.user,
            "--cd",
            "~",
            "--exec",
            "/bin/bash",
            "-lc",
            "exec \"$@\"",
            "--",
            &config.executable,
            "--version",
        ]);
        cmd
    };
    let result = match super::preferences::output(command).await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            parse_semver(&text).ok_or_else(|| "Antigravity CLI 版本输出无法识别".to_owned())
        }
        Err(err) => Err(err.message),
    };
    cache
        .lock()
        .await
        .insert(key, (Instant::now(), result.clone()));
    result
}

fn parse_env_u64(env: &Value, key: &str) -> Option<u64> {
    env[key]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|text| text.parse::<u64>().ok())
        .filter(|&value| value > 0)
}

fn parse_claude_token_count(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_u64().filter(|&n| n > 0) {
        return Some(number);
    }
    let raw = value.as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return raw.parse::<u64>().ok().filter(|&n| n > 0);
    }
    let lower = raw.to_ascii_lowercase();
    let (prefix, multiplier) = if let Some(p) = lower.strip_suffix('k') {
        (p, 1_000.0_f64)
    } else if let Some(p) = lower.strip_suffix('m') {
        (p, 1_000_000.0_f64)
    } else {
        return None;
    };
    let num: f64 = prefix.trim().parse().ok()?;
    if !num.is_finite() || num <= 0.0 {
        return None;
    }
    let scaled = (num * multiplier).round();
    if !scaled.is_finite() || scaled < 1.0 || scaled > u64::MAX as f64 {
        return None;
    }
    Some(scaled as u64)
}

fn validate_context_window(backend: &str, value: &Value) -> Result<Option<u64>, ApiError> {
    if value.is_null() {
        return Ok(None);
    }
    let min = if backend == "claude" {
        CLAUDE_CONTEXT_WINDOW_MIN
    } else {
        CODEX_CONTEXT_WINDOW_MIN
    };
    if let Some(tokens) = value.as_u64()
        && (min..=CONTEXT_WINDOW_MAX).contains(&tokens)
    {
        return Ok(Some(tokens));
    }
    let label = if backend == "claude" { "Claude" } else { "Codex" };
    Err(ApiError::new(
        400,
        format!("{label} 上下文大小必须为 {min}～{CONTEXT_WINDOW_MAX} 的整数，或留空使用原生默认"),
    ))
}

fn validate_prompt_cache_ttl(value: &Value) -> Result<Option<&'static str>, ApiError> {
    if value.is_null() {
        return Ok(None);
    }
    match value.as_str().map(str::trim) {
        Some("") => Ok(None),
        Some("5m") => Ok(Some("5m")),
        Some("1h") => Ok(Some("1h")),
        _ => Err(ApiError::new(
            400,
            "Claude 缓存时间仅支持 5 分钟（5m）、1 小时（1h）或留空使用原生默认",
        )),
    }
}

fn codex_context_window_row(
    codex_home: &Path,
    document: &Value,
    adapter_read_only: bool,
) -> (Value, Value) {
    // 用户要求 Codex 上下文大小未覆写时，输入框左侧直接显示从模型目录读取到的上下文数值（与下方自动压缩阈值保持一致），不展示描述性介绍。
    let raw = &document["model_context_window"];
    let mut catalog_doc = document.clone();
    if let Some(obj) = catalog_doc.as_object_mut() {
        obj.remove("model_context_window");
    }
    let default_window = codex_model_context_window(codex_home, &catalog_doc, None);
    let default_preview = default_window
        .map(format_tokens_with_commas)
        .unwrap_or_default();
    // 用户要求输入框左侧只显示数值或默认状态，不附带原生配置 key。
    let (value, cli_preview, value_invalid) = if raw.is_null() {
        (Value::Null, default_preview, false)
    } else if let Some(tokens) = raw.as_u64()
        && (CODEX_CONTEXT_WINDOW_MIN..=CONTEXT_WINDOW_MAX).contains(&tokens)
    {
        (json!(tokens), format_tokens_with_commas(tokens), false)
    } else if let Some(tokens) = raw.as_u64()
        && tokens > 0
        && tokens < CODEX_CONTEXT_WINDOW_MIN
    {
        (
            Value::Null,
            format!(
                "原生配置：{}（低于 {}）",
                format_tokens_with_commas(tokens),
                format_tokens_with_commas(CODEX_CONTEXT_WINDOW_MIN)
            ),
            false,
        )
    } else {
        (Value::Null, "原生配置值无效".into(), true)
    };
    // 用户要求精简设置项描述：功能本身可自解释时不赘述，仅保留影响范围与约束，单位在输入框右侧显示为 TK。
    let row = json!({
        "id": "contextWindow",
        "kind": "integer",
        "label": "上下文大小",
        "group": "缓存与压缩",
        "description": "覆盖所有模型。",
        "value": value,
        "min": CODEX_CONTEXT_WINDOW_MIN,
        "max": CONTEXT_WINDOW_MAX,
        "unit": "TK",
        "cliPreview": cli_preview,
        "defaultLabel": "原生默认",
        "readOnly": adapter_read_only,
        "readOnlyReason": if adapter_read_only {
            json!("该 Codex 实例的独立执行适配器尚未接入，上下文大小仅供查看。")
        } else {
            Value::Null
        },
        "valueInvalid": value_invalid,
        "options": []
    });
    (value, row)
}

fn claude_context_window_row(document: &Value) -> (Value, Value) {
    let raw = &document["env"]["CLAUDE_CODE_AUTO_COMPACT_WINDOW"];
    // 用户要求输入框左侧只显示数值或默认状态，不附带原生配置 key。
    let (value, cli_preview, value_invalid) = if raw.is_null() {
        (Value::Null, "原生默认（随模型）".into(), false)
    } else if let Some(tokens) = parse_claude_token_count(raw)
        && (CLAUDE_CONTEXT_WINDOW_MIN..=CONTEXT_WINDOW_MAX).contains(&tokens)
    {
        (json!(tokens), format_tokens_with_commas(tokens), false)
    } else if let Some(tokens) = parse_claude_token_count(raw)
        && tokens < CLAUDE_CONTEXT_WINDOW_MIN
    {
        (
            Value::Null,
            format!(
                "原生配置：{}（低于 100,000）",
                format_tokens_with_commas(tokens)
            ),
            false,
        )
    } else {
        (Value::Null, "原生配置值无效".into(), true)
    };
    // 用户要求精简设置项描述：功能本身可自解释时不赘述，仅保留下限约束，单位在输入框右侧显示为 TK。
    let row = json!({
        "id": "contextWindow",
        "kind": "integer",
        "label": "上下文大小",
        "group": "缓存与压缩",
        "description": "最低 100,000。",
        "value": value,
        "min": CLAUDE_CONTEXT_WINDOW_MIN,
        "max": CONTEXT_WINDOW_MAX,
        "unit": "TK",
        "cliPreview": cli_preview,
        "defaultLabel": "原生默认",
        "readOnly": false,
        "readOnlyReason": Value::Null,
        "valueInvalid": value_invalid,
        "options": []
    });
    (value, row)
}

fn unsupported_context_window_row(reason: &str) -> Value {
    // 用户要求所有实例均展示「上下文大小」，原生不支持的实例右侧改为纯文字「不支持」。
    json!({
        "id": "contextWindow",
        "kind": "integer",
        "label": "上下文大小",
        "group": "缓存与压缩",
        "description": "",
        "value": Value::Null,
        "min": Value::Null,
        "max": Value::Null,
        "cliPreview": "不支持",
        "readOnly": true,
        "readOnlyReason": reason,
        "valueInvalid": false,
        "options": []
    })
}

fn claude_prompt_cache_ttl_row(document: &Value) -> (Value, Value) {
    // env.CLAUDE_CODE_PROMPT_CACHE_TTL 优先于顶层 promptCacheTtl；两者均未配置时使用原生默认。
    let raw = if !document["env"]["CLAUDE_CODE_PROMPT_CACHE_TTL"].is_null() {
        &document["env"]["CLAUDE_CODE_PROMPT_CACHE_TTL"]
    } else {
        &document["promptCacheTtl"]
    };
    let (value, read_only, read_only_reason, value_invalid) = if raw.is_null() {
        (Value::Null, false, Value::Null, false)
    } else if let Some(ttl) = raw.as_str().map(str::trim)
        && matches!(ttl, "5m" | "1h")
    {
        (json!(ttl), false, Value::Null, false)
    } else {
        (
            Value::Null,
            true,
            json!("原配置中的 CLAUDE_CODE_PROMPT_CACHE_TTL 取值无效，请在原生配置中核对；未覆盖原文件。"),
            true,
        )
    };
    // 用户要求精简设置项描述：缓存时间下拉选项已自解释，不额外增加冗余介绍。
    let row = json!({
        "id": "promptCacheTtl",
        "kind": "enum",
        "label": "缓存时间",
        "group": "缓存与压缩",
        "description": "",
        "value": value,
        "readOnly": read_only,
        "readOnlyReason": read_only_reason,
        "valueInvalid": value_invalid,
        "options": [
            {"value": "5m", "label": "5 分钟"},
            {"value": "1h", "label": "1 小时"}
        ]
    });
    (value, row)
}

fn unsupported_prompt_cache_ttl_row(reason: &str) -> Value {
    // 用户要求所有实例均展示「缓存时间」，原生不支持的实例右侧改为纯文字「不支持」。
    json!({
        "id": "promptCacheTtl",
        "kind": "enum",
        "label": "缓存时间",
        "group": "缓存与压缩",
        "description": "",
        "value": Value::Null,
        "cliPreview": "不支持",
        "readOnly": true,
        "readOnlyReason": reason,
        "valueInvalid": false,
        "options": []
    })
}

fn codex_auto_compact_row(
    codex_home: &Path,
    document: &Value,
    preference_value: &Value,
    adapter_read_only: bool,
) -> Result<(Value, Value), ApiError> {
    let percent = validate_auto_compact_percent("codex", preference_value)?;
    let effective_window = codex_model_context_window(codex_home, document, None);
    let native_limit = document["model_auto_compact_token_limit"]
        .as_u64()
        .filter(|&value| value > 0);
    // 用户要求 Codex 自动压缩阈值输入框左侧只显示对应数值，不附带配置 key 或描述性介绍。
    let cli_preview = if let Some(pct) = percent {
        match effective_window {
            Some(window) => format_tokens_with_commas(
                ((window as u128 * pct as u128) / 100).max(1) as u64,
            ),
            None => format!("{pct}%"),
        }
    } else if let Some(limit) = native_limit {
        format_tokens_with_commas(limit)
    } else if let Some(window) = effective_window {
        format_tokens_with_commas(((window as u128 * 90) / 100).max(1) as u64)
    } else {
        "90%".into()
    };
    let value = percent.map(Value::from).unwrap_or(Value::Null);
    let row = json!({
        "id": "autoCompactPercent",
        "kind": "integer",
        "label": "自动压缩阈值",
        "group": "缓存与压缩",
        "description": "50～98%",
        "value": value,
        "min": AUTO_COMPACT_MIN_PERCENT,
        "max": AUTO_COMPACT_MAX_PERCENT,
        "unit": "%",
        "cliPreview": cli_preview,
        "defaultLabel": "90%",
        "readOnly": adapter_read_only,
        "readOnlyReason": if adapter_read_only {
            json!("该 Codex 实例的独立执行适配器尚未接入，自动压缩阈值仅供查看。")
        } else {
            Value::Null
        },
        "valueInvalid": false,
        "options": []
    });
    Ok((value, row))
}

fn claude_auto_compact_row(document: &Value) -> (Value, Value) {
    let env = &document["env"];
    let window = parse_claude_token_count(&env["CLAUDE_CODE_AUTO_COMPACT_WINDOW"])
        .or_else(|| parse_claude_token_count(&env["CLAUDE_CODE_MAX_CONTEXT_TOKENS"]))
        .filter(|&value| value > CLAUDE_AUTOCOMPACT_BUFFER_TOKENS);
    let (default_preview, default_label) = match window {
        Some(tokens) => {
            let approx = (tokens - CLAUDE_AUTOCOMPACT_BUFFER_TOKENS) as f64 * 100.0 / tokens as f64;
            (
                format!("原生默认：预留 33k（约 {approx:.1}%）"),
                format!("{approx:.0}%"),
            )
        }
        None => ("原生默认：预留 33k".into(), "默认".into()),
    };
    let raw = &env["CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"];
    let (value, cli_preview, placeholder, value_invalid) = if raw.is_null() {
        (Value::Null, default_preview, default_label, false)
    } else if let Some(parsed) = parse_env_u64(env, "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE")
        && (1..=100).contains(&parsed)
    {
        // 用户要求输入框左侧只显示数值，不附带配置 key；原生配置超出 50～98% 时按原值提示而不标成格式无效。
        if (AUTO_COMPACT_MIN_PERCENT..=AUTO_COMPACT_MAX_PERCENT).contains(&parsed) {
            (json!(parsed), format!("{parsed}%"), default_label, false)
        } else if parsed < AUTO_COMPACT_MIN_PERCENT {
            (
                Value::Null,
                format!("原生配置：{parsed}%（低于 {AUTO_COMPACT_MIN_PERCENT}%）"),
                format!("{parsed}%"),
                false,
            )
        } else {
            (
                Value::Null,
                format!("原生配置：{parsed}%（高于 {AUTO_COMPACT_MAX_PERCENT}%）"),
                format!("{parsed}%"),
                false,
            )
        }
    } else {
        (Value::Null, "原生配置值无效".into(), default_label, true)
    };
    let row = json!({
        "id": "autoCompactPercent",
        "kind": "integer",
        "label": "自动压缩阈值",
        "group": "缓存与压缩",
        "description": "50～98%",
        "value": value,
        "min": AUTO_COMPACT_MIN_PERCENT,
        "max": AUTO_COMPACT_MAX_PERCENT,
        "unit": "%",
        "cliPreview": cli_preview,
        "defaultLabel": placeholder,
        "readOnly": false,
        "readOnlyReason": Value::Null,
        "valueInvalid": value_invalid,
        "options": []
    });
    (value, row)
}

fn antigravity_auto_compact_row(
    preference_value: &Value,
    version_result: &Result<(u64, u64, u64), String>,
) -> Result<(Value, Value), ApiError> {
    let percent = validate_auto_compact_percent("antigravity", preference_value)?;
    let (read_only, read_only_reason) = match version_result {
        Ok(version) if *version >= ANTIGRAVITY_MIN_AUTO_COMPACT_VERSION => (false, None),
        Ok((major, minor, patch)) => (
            true,
            Some(format!(
                "当前 Antigravity CLI 版本为 {major}.{minor}.{patch}，需升级到 1.5.15 及以上才支持自动压缩阈值。"
            )),
        ),
        Err(reason) => (
            true,
            Some(format!(
                "未能确认当前 Antigravity CLI 版本（需 1.5.15 及以上）：{reason}"
            )),
        ),
    };
    // 用户要求输入框左侧只显示数值或默认状态，不附带配置 key。
    let cli_preview = match percent {
        Some(pct) => format!("{pct}%"),
        None => "原生默认（随模型）".into(),
    };
    let value = percent.map(Value::from).unwrap_or(Value::Null);
    let row = json!({
        "id": "autoCompactPercent",
        "kind": "integer",
        "label": "自动压缩阈值",
        "group": "缓存与压缩",
        "description": "50～98%",
        "value": value,
        "min": AUTO_COMPACT_MIN_PERCENT,
        "max": AUTO_COMPACT_MAX_PERCENT,
        "unit": "%",
        "cliPreview": cli_preview,
        "defaultLabel": "默认",
        "readOnly": read_only,
        "readOnlyReason": read_only_reason,
        "valueInvalid": false,
        "options": []
    });
    Ok((value, row))
}

// 用户要求「手动压缩模型」提供独立开关（默认关闭，避免默认就生效）与策略菜单（未设置时默认「默认使用」，可选「缓存失效使用」）；
// 开关、模型和策略三者独立保存，选择「跟随会话模型」或关闭开关不得改写已设置的策略。
pub(crate) const COMPACTION_MODEL_MODE_CACHE_EXPIRED: &str = "cache_expired";
pub(crate) const COMPACTION_MODEL_MODE_ALWAYS: &str = "always";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompactionModelMode {
    CacheExpired,
    Always,
}

impl CompactionModelMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::CacheExpired => COMPACTION_MODEL_MODE_CACHE_EXPIRED,
            Self::Always => COMPACTION_MODEL_MODE_ALWAYS,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompactionModelPreference {
    pub enabled: bool,
    pub model: Option<String>,
    pub mode: CompactionModelMode,
}

impl CompactionModelPreference {
    /// 用户要求尽快显示压缩开始；仅当缓存结果确实可能改变本次型号时才扫描会话统计。
    pub(crate) fn needs_cache_check(&self, current: &str) -> bool {
        self.mode == CompactionModelMode::CacheExpired
            && resolve_manual_compaction_model(self, current, false).is_some()
    }
}

fn parse_compaction_model_enabled(value: &Value) -> Result<bool, ApiError> {
    if value.is_null() {
        return Ok(false);
    }
    value
        .as_bool()
        .ok_or_else(|| ApiError::new(400, "手动压缩模型开关必须为布尔值"))
}

fn parse_compaction_model(value: &Value) -> Result<Option<String>, ApiError> {
    if value.is_null() {
        return Ok(None);
    }
    let text = value
        .as_str()
        .map(str::trim)
        .ok_or_else(|| ApiError::new(400, "手动压缩模型需为模型 ID 或留空跟随会话模型"))?;
    if text.is_empty() || text == "session" {
        return Ok(None);
    }
    if text.len() > 200 || text.chars().any(char::is_control) {
        return Err(ApiError::new(400, "手动压缩模型标识无效"));
    }
    Ok(Some(text.to_owned()))
}

fn parse_compaction_model_mode(value: &Value) -> Result<CompactionModelMode, ApiError> {
    if value.is_null() {
        return Ok(CompactionModelMode::Always);
    }
    match value.as_str().map(str::trim) {
        Some(COMPACTION_MODEL_MODE_ALWAYS) | Some("") => Ok(CompactionModelMode::Always),
        Some(COMPACTION_MODEL_MODE_CACHE_EXPIRED) => Ok(CompactionModelMode::CacheExpired),
        _ => Err(ApiError::new(
            400,
            "手动压缩模型策略仅支持“默认使用”或“缓存失效使用”",
        )),
    }
}

fn validate_compaction_model_choice(
    model: Option<&str>,
    catalog: Option<&Value>,
) -> Result<(), ApiError> {
    let Some(model) = model else {
        return Ok(());
    };
    let rows = catalog
        .and_then(|value| value["data"].as_array())
        .ok_or_else(|| {
            ApiError::new(
                409,
                "尚未取得当前实例的模型目录，请先重新读取后再选择手动压缩模型",
            )
        })?;
    if !rows.iter().any(|row| row["model"].as_str() == Some(model)) {
        return Err(ApiError::new(
            400,
            format!("模型 {model} 不在当前实例的模型目录中"),
        ));
    }
    Ok(())
}

pub(crate) fn compaction_model_preference(
    directory: &Path,
    instance: &str,
) -> Result<CompactionModelPreference, ApiError> {
    let (document, _) = preferences(directory)?;
    let entry = &document["instances"][instance];
    Ok(CompactionModelPreference {
        enabled: parse_compaction_model_enabled(&entry["compactionModelEnabled"])?,
        model: parse_compaction_model(&entry["compactionModel"])?,
        mode: parse_compaction_model_mode(&entry["compactionModelMode"])?,
    })
}

/// 根据实例手动压缩模型开关、策略与当前模型缓存状态，决定本次手动压缩是否需要临时切换到手动压缩模型。
/// 开关关闭时始终不切换；仅用于手动压缩（执行中由原生处理且通常存在热缓存，不触发切换）；
/// 选“默认使用”时始终切换，选“缓存失效使用”时仅在当前模型缓存已过期或无有效缓存时切换。
pub(crate) fn resolve_manual_compaction_model(
    preference: &CompactionModelPreference,
    current_model: &str,
    cache_valid: bool,
) -> Option<String> {
    if !preference.enabled {
        return None;
    }
    let target = preference
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())?;
    let current = current_model.trim();
    if !current.is_empty() && target.eq_ignore_ascii_case(current) {
        return None;
    }
    match preference.mode {
        CompactionModelMode::Always => Some(target.to_owned()),
        CompactionModelMode::CacheExpired => (!cache_valid).then(|| target.to_owned()),
    }
}

fn compaction_model_row(
    catalog: Option<&Value>,
    enabled: bool,
    current_model: Option<&str>,
    mode: CompactionModelMode,
    read_only: bool,
    read_only_reason: Option<String>,
) -> Value {
    // 用户要求所有会话实例的「缓存与压缩」统一提供「手动压缩模型」：
    // 独立开关默认关闭（避免默认就生效），关闭时禁用模型与策略选择并保留原值；
    // 未设置过时模型默认「跟随会话模型」、策略默认「默认使用」，已设置过的策略不因改选模型而被重置。
    let mut options = vec![json!({"value": "session", "label": "跟随会话模型"})];
    let mut seen = BTreeSet::from(["session".to_owned()]);
    if let Some(rows) = catalog.and_then(|value| value["data"].as_array()) {
        for row in rows {
            let Some(id) = row["model"]
                .as_str()
                .map(str::trim)
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            if !seen.insert(id.to_owned()) {
                continue;
            }
            let label = row["displayName"]
                .as_str()
                .map(str::trim)
                .filter(|label| !label.is_empty())
                .unwrap_or(id);
            options.push(json!({"value": id, "label": label}));
        }
    }
    let selected = current_model.unwrap_or("session");
    if selected != "session" && !seen.contains(selected) {
        options.push(json!({
            "value": selected,
            "label": format!("{selected}（当前配置）"),
            "disabled": true
        }));
    }
    json!({
        "id": "compactionModel",
        "kind": "enum",
        "label": "手动压缩模型",
        "group": "缓存与压缩",
        "description": "仅用于手动压缩，完成后自动切回原模型。",
        "value": selected,
        "enabledKey": "compactionModelEnabled",
        "enabledValue": enabled,
        "modeKey": "compactionModelMode",
        "modeValue": mode.as_str(),
        // 用户要求策略下拉菜单描述保持简短，在窄弹层中单行显示，不重复外层说明。
        "modeOptions": [
            {
                "value": COMPACTION_MODEL_MODE_ALWAYS,
                "label": "默认使用",
                "description": "手动压缩时始终切换"
            },
            {
                "value": COMPACTION_MODEL_MODE_CACHE_EXPIRED,
                "label": "缓存失效使用",
                "description": "无有效缓存时切换"
            }
        ],
        "readOnly": read_only,
        "readOnlyReason": read_only_reason,
        "options": options
    })
}

fn unsupported_compaction_prompt_row(reason: &str) -> Value {
    // 用户要求所有实例均展示「压缩提示词」，不支持的实例不渲染“设置”按钮，改为右侧文字说明“不支持”。
    json!({
        "id": "compactionPrompt",
        "kind": "text",
        "label": "压缩提示词",
        "group": "缓存与压缩",
        "description": "",
        "value": Value::Null,
        "readOnly": true,
        "readOnlyReason": reason,
        "cliPreview": "不支持",
        "options": [],
        "min": Value::Null,
        "max": 20000,
        "defaultValue": Value::Null
    })
}

fn clear_codex_static_auto_compact(text: &str) -> Result<String, ApiError> {
    let mut document = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| ApiError::new(422, "Codex 配置格式无效，未覆盖原文件"))?;
    set_toml(
        document.as_item_mut(),
        &["model_auto_compact_token_limit"],
        None,
    )?;
    Ok(toml_output(text, &document))
}

async fn save_preference(
    directory: &Path,
    instance: &str,
    key: &str,
    value: &Value,
    expected: &str,
    authorization: &HarnessAuthorization,
) -> Result<(), ApiError> {
    let (mut document, revision) = preferences(directory)?;
    if revision != expected {
        return Err(ApiError::new(409, "实例偏好已变化，请重新读取后再保存"));
    }
    let instances = document["instances"].as_object_mut().unwrap();
    let entry = instances.entry(instance).or_insert_with(|| json!({}));
    let entry = entry
        .as_object_mut()
        .ok_or_else(|| ApiError::new(422, "实例偏好格式无效，未覆盖原文件"))?;
    if value.is_null() {
        entry.remove(key);
    } else {
        entry.insert(key.to_owned(), value.clone());
    }
    let output = serde_json::to_string_pretty(&document).unwrap() + "\n";
    let permit = authorization
        .prepare_dispatch()
        .await
        .map_err(super::authorization_error)?;
    permit
        .commit(|| permissions::save_file(&directory.join(PREFERENCES_FILE), &output, expected))
        .map_err(super::authorization_error)
}

async fn claude_help(config: &super::claude::WslConfig) -> Result<String, ApiError> {
    let key = format!("{}:{}", config.instance_id(), config.executable);
    let cache = CLAUDE_HELP.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Some((at, text)) = cache.lock().await.get(&key)
        && at.elapsed() < Duration::from_secs(60)
    {
        return Ok(text.clone());
    }
    config
        .validate()
        .map_err(|_| ApiError::new(400, "Claude 实例身份或 CLI 路径无效"))?;
    // 用户要求能力查询只走所属 WSL CLI，并固定在该用户 Home 的登录环境，不加载项目上下文。
    let mut command = tokio::process::Command::new("wsl.exe");
    command.args([
        "--distribution",
        &config.distribution,
        "--user",
        &config.user,
        "--cd",
        "~",
    ]);
    super::claude::Claude::wsl_login_exec(&mut command, &config.executable);
    command.arg("--help");
    let text = String::from_utf8(super::preferences::output(command).await?)
        .map_err(|_| ApiError::new(503, "Claude CLI 帮助返回格式无效"))?;
    cache
        .lock()
        .await
        .insert(key, (Instant::now(), text.clone()));
    Ok(text)
}

async fn claude_option_capabilities(config: &super::claude::WslConfig) -> Result<Value, ApiError> {
    let key = format!("{}:{}", config.instance_id(), config.executable);
    let cache = CLAUDE_OPTIONS.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Some((at, value)) = cache.lock().await.get(&key)
        && at.elapsed() < Duration::from_secs(60)
    {
        return Ok(value.clone());
    }
    config
        .validate()
        .map_err(|_| ApiError::new(400, "Claude 实例身份或 CLI 路径无效"))?;
    let probe = CLAUDE_BOOLEAN_OPTIONS
        .iter()
        .map(|key| ((*key).to_owned(), json!("__codex_elves_type_probe__")))
        .collect::<serde_json::Map<_, _>>();
    let mut command = tokio::process::Command::new("wsl.exe");
    command.args([
        "--distribution",
        &config.distribution,
        "--user",
        &config.user,
        "--cd",
        "~",
    ]);
    super::claude::Claude::wsl_login_exec(&mut command, &config.executable);
    // doctor 只检查配置；临时命令行覆盖用于确认字段类型，不写用户配置、不创建模型请求或会话。
    command.args([
        "--settings",
        &serde_json::to_string(&probe).unwrap(),
        "doctor",
    ]);
    let output = String::from_utf8(super::preferences::output(command).await?)
        .map_err(|_| ApiError::new(503, "Claude 配置能力检查返回格式无效"))?;
    let mut value = json!({});
    for field in CLAUDE_BOOLEAN_OPTIONS {
        value[*field] = json!(output.lines().any(|line| {
            line.contains(&format!("› {field}: Expected boolean"))
                && line.contains("received string")
        }));
    }
    // 不返回 doctor 原文；其中可能包含账号、路径、命令和其他原生配置。
    cache
        .lock()
        .await
        .insert(key, (Instant::now(), value.clone()));
    Ok(value)
}

fn native_document(backend: &str, text: &str) -> Result<Value, ApiError> {
    if backend == "codex" {
        let value = toml::from_str::<toml::Value>(text)
            .map_err(|_| ApiError::new(422, "Codex 配置格式无效，未覆盖原文件"))?;
        serde_json::to_value(value).map_err(|_| ApiError::new(422, "Codex 配置格式无效"))
    } else {
        let value = if text.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(text.trim_start_matches('\u{feff}'))
                .map_err(|_| ApiError::new(422, "Claude 配置格式无效，未覆盖原文件"))?
        };
        if !value.is_object() {
            return Err(ApiError::new(422, "Claude 配置必须为 JSON 对象"));
        }
        Ok(value)
    }
}

const CLAUDE_SKILL_OVERRIDE_STATES: &[&str] =
    &["on", "name-only", "user-invocable-only", "off"];

fn claude_skill_name(id: &str) -> Option<&str> {
    let name = id.strip_prefix("claude-skill:")?;
    (!name.is_empty()
        && name.len() <= 256
        && name
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_' | ':' | '.')))
    .then_some(name)
}

fn claude_skill_override_state<'a>(
    document: &'a Value,
    name: &str,
) -> Result<&'a str, ApiError> {
    let overrides = &document["skillOverrides"];
    if overrides.is_null() {
        return Ok("on");
    }
    let overrides = overrides.as_object().ok_or_else(|| {
        ApiError::new(422, "Claude skillOverrides 配置格式无效，未覆盖原文件")
    })?;
    let Some(value) = overrides.get(name) else {
        return Ok("on");
    };
    let state = value.as_str().filter(|state| {
        CLAUDE_SKILL_OVERRIDE_STATES.contains(state)
    });
    state.ok_or_else(|| ApiError::new(422, "Claude Skill 覆盖状态无效，未覆盖原文件"))
}

fn decorate_claude_skill_inventory(
    inventory: &mut Value,
    document: &Value,
) -> Result<(), ApiError> {
    let Some(rows) = inventory["data"].as_array_mut() else {
        return Err(ApiError::new(503, "Claude Skill 列表格式无效"));
    };
    for row in rows {
        let Some(name) = row["name"].as_str() else {
            continue;
        };
        let state = claude_skill_override_state(document, name)?;
        let plugin = row["scope"] == "plugin";
        row["override"] = json!(state);
        row["enabled"] = json!(state != "off");
        row["canToggle"] = json!(!plugin);
        if plugin {
            row["readOnlyReason"] = json!("插件 Skill 由插件管理");
        }
    }
    Ok(())
}

fn json_output(original: &str, document: &Value) -> String {
    let ending = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut output = serde_json::to_string_pretty(document)
        .expect("JSON 配置文档序列化不应失败")
        .replace('\n', ending);
    if original.starts_with('\u{feff}') {
        output.insert(0, '\u{feff}');
    }
    output.push_str(ending);
    output
}

fn updated_claude_skill_override(text: &str, change: &Value) -> Result<String, ApiError> {
    let change = change
        .as_object()
        .filter(|change| change.len() == 2)
        .ok_or_else(|| ApiError::new(400, "Claude Skill 开关参数无效"))?;
    let id = change
        .get("id")
        .and_then(Value::as_str)
        .and_then(claude_skill_name)
        .ok_or_else(|| ApiError::new(400, "请选择当前实例的 Claude Skill"))?;
    let enabled = change
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| ApiError::new(400, "Claude Skill 开关必须为布尔值"))?;
    let mut document = native_document("claude", text)?;
    let root = document
        .as_object_mut()
        .ok_or_else(|| ApiError::new(422, "Claude 配置必须为 JSON 对象"))?;
    if enabled {
        let remove_overrides = if let Some(overrides) = root.get_mut("skillOverrides") {
            let overrides = overrides.as_object_mut().ok_or_else(|| {
                ApiError::new(422, "Claude skillOverrides 配置格式无效，未覆盖原文件")
            })?;
            overrides.remove(id);
            overrides.is_empty()
        } else {
            false
        };
        if remove_overrides {
            root.remove("skillOverrides");
        }
    } else {
        let overrides = root
            .entry("skillOverrides")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| {
                ApiError::new(422, "Claude skillOverrides 配置格式无效，未覆盖原文件")
            })?;
        overrides.insert(id.to_owned(), json!("off"));
    }
    Ok(json_output(text, &document))
}

fn optional_bool(value: &Value) -> Result<Value, ApiError> {
    if value.is_null() || value.is_boolean() {
        Ok(value.clone())
    } else {
        Err(ApiError::new(422, "实例配置中的开关类型无效，未覆盖原配置"))
    }
}

pub(super) fn apply_codex_compaction_default(config: &Value, params: &mut Value) {
    // 用户指定 Codex 默认压缩提示词沿用旧管理器；显式原生提示词或文件配置继续优先，不能改写代理全局设置。
    // 文件路径可能只出现在原生配置层，不能因顶层序列化没有该字段就覆盖用户的提示词文件。
    let native_override = |value: &Value| !value["compact_prompt"].is_null()
        || value["experimental_compact_prompt_file"].as_str().is_some_and(|path| !path.is_empty());
    let effective = config.get("config").unwrap_or(config);
    if native_override(effective)
        || config["layers"].as_array().into_iter().flatten()
            .any(|layer| native_override(&layer["config"]))
    {
        return;
    }
    if !params["config"].is_object() {
        params["config"] = json!({});
    }
    if params["config"]["compact_prompt"].is_null() {
        params["config"]["compact_prompt"] =
            json!(elves_core::layered_compaction::DEFAULT_COMPACTION_PROMPT);
    }
}

fn general_view(backend: &str, document: &Value) -> Result<Value, ApiError> {
    if backend == "codex" {
        let legacy_agents = optional_bool(&document["features"]["multi_agent"])?;
        let v2_agents = optional_bool(&document["features"]["multi_agent_v2"]["enabled"])?;
        let agents = if legacy_agents == true || v2_agents == true {
            json!(true)
        } else if legacy_agents == false && v2_agents == false {
            json!(false)
        } else {
            Value::Null
        };
        Ok(json!({
            "todo":optional_bool(&document["tools"]["update_plan"]["enabled"])?,
            "memories":optional_bool(&document["features"]["memories"])?,
            "multiAgent":agents,
            "goals":optional_bool(&document["features"]["goals"])?
        }))
    } else {
        Ok(json!({
            "awaySummaryEnabled":optional_bool(&document["awaySummaryEnabled"])?,
            "promptSuggestionEnabled":optional_bool(&document["promptSuggestionEnabled"])?
        }))
    }
}

fn set_toml(
    item: &mut toml_edit::Item,
    path: &[&str],
    value: Option<toml_edit::Value>,
) -> Result<(), ApiError> {
    let table = item
        .as_table_like_mut()
        .ok_or_else(|| ApiError::new(422, "原生配置结构不兼容，未覆盖原文件"))?;
    let key = path[0];
    if path.len() == 1 {
        if let Some(mut value) = value {
            if let Some(original) = table.get(key).and_then(toml_edit::Item::as_value) {
                *value.decor_mut() = original.decor().clone();
            }
            // 已有键直接替换值，保留键本身的前缀注释；insert 会重建键并丢失模型前的注释。
            if let Some(original) = table.get_mut(key) {
                *original = toml_edit::Item::Value(value);
            } else {
                table.insert(key, toml_edit::Item::Value(value));
            }
        } else {
            table.remove(key);
        }
    } else {
        if !table.contains_key(key) {
            if value.is_none() {
                return Ok(());
            }
            let mut child = toml_edit::Table::new();
            child.set_implicit(true);
            table.insert(key, toml_edit::Item::Table(child));
        }
        set_toml(table.get_mut(key).unwrap(), &path[1..], value)?;
    }
    Ok(())
}

fn toml_output(original: &str, document: &toml_edit::DocumentMut) -> String {
    // 原生配置编辑必须保留 BOM 和换行格式；常规设置与 MCP 共用，避免 toml_edit 渲染时丢失这些信息。
    let mut output = document.to_string();
    if original.contains("\r\n") {
        output = output.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    if original.starts_with('\u{feff}') && !output.starts_with('\u{feff}') {
        output.insert(0, '\u{feff}');
    }
    output
}

fn updated_native(
    backend: &str,
    section: &str,
    text: &str,
    changes: &Value,
) -> Result<String, ApiError> {
    let changes = changes
        .as_object()
        .filter(|changes| !changes.is_empty() && changes.len() <= 4)
        .ok_or_else(|| ApiError::new(400, "请选择需要保存的设置"))?;
    if backend == "codex" {
        let mut document = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|_| ApiError::new(422, "Codex 配置格式无效，未覆盖原文件"))?;
        for (key, value) in changes {
            if options::apply_codex(&mut document, section, key, value)? {
                continue;
            }
            if section == "general" && key == "contextWindow" {
                let window = validate_context_window("codex", value)?;
                let replacement = window.map(|tokens| toml_edit::Value::from(tokens as i64));
                set_toml(document.as_item_mut(), &["model_context_window"], replacement)?;
                continue;
            }
            // 用户要求移除设置页「实例默认值」，不再通过实例设置改写原生 model / model_reasoning_effort。
            let path: &[&str] = match (section, key.as_str()) {
                ("general", "todo") => &["tools", "update_plan", "enabled"],
                ("general", "memories") => &["features", "memories"],
                ("general", "multiAgent") => &["features", "multi_agent_v2", "enabled"],
                ("general", "goals") => &["features", "goals"],
                ("hooks", "enabled") => &["features", "hooks"],
                _ => return Err(ApiError::new(400, "不支持的 Codex 实例设置")),
            };
            let replacement = if value.is_null() {
                None
            } else {
                Some(toml_edit::Value::from(
                    value
                        .as_bool()
                        .ok_or_else(|| ApiError::new(400, "开关必须为布尔值"))?,
                ))
            };
            // 用户要求 Todo、记忆、子智能体和目标模式按实例控制；保留原文件注释及所有其他配置。
            if section == "general" && key == "multiAgent" {
                // 当前 CLI 同时提供两代子智能体；总开关关闭两者，不能只关闭 V2 后回退旧版。
                set_toml(
                    document.as_item_mut(),
                    &["features", "multi_agent"],
                    replacement.clone(),
                )?;
            }
            set_toml(document.as_item_mut(), path, replacement)?;
        }
        Ok(toml_output(text, &document))
    } else {
        let mut document = native_document(backend, text)?;
        let root = document.as_object_mut().unwrap();
        if (changes.contains_key("autoCompactPercent")
            || changes.contains_key("contextWindow")
            || changes.contains_key("promptCacheTtl"))
            && changes.len() != 1
        {
            return Err(ApiError::new(400, "该缓存与压缩设置需要单独保存"));
        }
        for (key, value) in changes {
            if !options::claude_boolean(section, key, value) {
                match (section, key.as_str()) {
                    ("general", "awaySummaryEnabled") if value.is_boolean() => {}
                    ("general", "promptSuggestionEnabled")
                        if value.is_null() || value.is_boolean() => {}
                    ("general", "autoCompactPercent") => {
                        let percent = validate_auto_compact_percent("claude", value)?;
                        match percent {
                            None => {
                                if let Some(env_value) = root.get_mut("env") {
                                    let env = env_value.as_object_mut().ok_or_else(|| {
                                        ApiError::new(422, "Claude env 配置格式无效，未覆盖原文件")
                                    })?;
                                    env.remove("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE");
                                    if env.is_empty() {
                                        root.remove("env");
                                    }
                                }
                            }
                            Some(pct) => {
                                let env = root
                                    .entry("env")
                                    .or_insert_with(|| json!({}))
                                    .as_object_mut()
                                    .ok_or_else(|| {
                                        ApiError::new(422, "Claude env 配置格式无效，未覆盖原文件")
                                    })?;
                                env.insert(
                                    "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE".into(),
                                    json!(pct.to_string()),
                                );
                            }
                        }
                        continue;
                    }
                    ("general", "contextWindow") => {
                        let window = validate_context_window("claude", value)?;
                        match window {
                            None => {
                                if let Some(env_value) = root.get_mut("env") {
                                    let env = env_value.as_object_mut().ok_or_else(|| {
                                        ApiError::new(422, "Claude env 配置格式无效，未覆盖原文件")
                                    })?;
                                    env.remove("CLAUDE_CODE_AUTO_COMPACT_WINDOW");
                                    if env.is_empty() {
                                        root.remove("env");
                                    }
                                }
                            }
                            Some(tokens) => {
                                let env = root
                                    .entry("env")
                                    .or_insert_with(|| json!({}))
                                    .as_object_mut()
                                    .ok_or_else(|| {
                                        ApiError::new(422, "Claude env 配置格式无效，未覆盖原文件")
                                    })?;
                                env.insert(
                                    "CLAUDE_CODE_AUTO_COMPACT_WINDOW".into(),
                                    json!(tokens.to_string()),
                                );
                            }
                        }
                        continue;
                    }
                    ("general", "promptCacheTtl") => {
                        let ttl = validate_prompt_cache_ttl(value)?;
                        // 清理顶层同名 promptCacheTtl，防止清空 env 后旧顶层键继续生效。
                        root.remove("promptCacheTtl");
                        match ttl {
                            None => {
                                if let Some(env_value) = root.get_mut("env") {
                                    let env = env_value.as_object_mut().ok_or_else(|| {
                                        ApiError::new(422, "Claude env 配置格式无效，未覆盖原文件")
                                    })?;
                                    env.remove("CLAUDE_CODE_PROMPT_CACHE_TTL");
                                    if env.is_empty() {
                                        root.remove("env");
                                    }
                                }
                            }
                            Some(ttl) => {
                                let env = root
                                    .entry("env")
                                    .or_insert_with(|| json!({}))
                                    .as_object_mut()
                                    .ok_or_else(|| {
                                        ApiError::new(422, "Claude env 配置格式无效，未覆盖原文件")
                                    })?;
                                env.insert("CLAUDE_CODE_PROMPT_CACHE_TTL".into(), json!(ttl));
                            }
                        }
                        continue;
                    }
                    ("hooks", "enabled") if value.is_boolean() => {
                        root.insert("disableAllHooks".into(), json!(!value.as_bool().unwrap()));
                        continue;
                    }
                    _ => return Err(ApiError::new(400, "不支持的 Claude 实例设置")),
                }
            }
            if value.is_null() {
                root.remove(key);
            } else {
                root.insert(key.clone(), value.clone());
            }
        }
        Ok(json_output(text, &document))
    }
}

fn extension_view(section: &str, backend: &str, document: &Value) -> Result<Value, ApiError> {
    // 扩展页只返回名称、状态和数量，不把 MCP 环境变量、Header 或 Hook 命令及其中凭据发送给网页。
    if backend == "claude" {
        let disabled = optional_bool(&document["disableAllHooks"])?;
        let rows =
            if section == "hooks" {
                document["hooks"].as_object().into_iter().flatten().map(|(event, rules)| json!({
                "id":event,"name":event,
                "description":format!("{} 条匹配规则",rules.as_array().map_or(0,Vec::len)),
                "canToggle":false
            })).collect::<Vec<_>>()
            } else {
                vec![]
            };
        return Ok(json!({"data":rows,
            "enabled":disabled.as_bool().map(|disabled|!disabled),
            "message":"仅显示所选实例用户级配置；项目、插件和组织策略继续由原生 CLI 管理。"}));
    }
    Ok(match section {
        "mcp" => {
            if let Some(servers) = document["mcp_servers"].as_object() {
                for value in servers.values() {
                    optional_bool(&value["enabled"])?;
                }
            }
            let rows = document["mcp_servers"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(name, value)| {
                    json!({
                        "id":name,"name":name,"enabled":value["enabled"],"canToggle":true,
                        // 用户要求 MCP 读取后明确显示开关：原生 CLI 未配置 enabled 时默认开启，原值仍保留空值以说明继承来源。
                        "defaultEnabled":true,
                        "transport":if value["command"].is_string() {"stdio"} else if value["url"].is_string() {"http"} else {"未指定连接方式"}
                    })
                })
                .collect::<Vec<_>>();
            // MCP 列表不显示常规说明，仍只展示用户级配置，不接管项目或插件服务。
            json!({"data":rows})
        }
        "hooks" => {
            let rows = document["hooks"]["state"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(name, value)| {
                    json!({
                        "id":name,"name":name,"enabled":value["enabled"].as_bool()
                    })
                })
                .collect::<Vec<_>>();
            json!({"data":rows,"enabled":optional_bool(&document["features"]["hooks"])?})
        }
        _ => json!({"data":[]}),
    })
}

fn updated_mcp(text: &str, change: &Value) -> Result<String, ApiError> {
    let id = change["id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 200)
        .ok_or_else(|| ApiError::new(400, "请选择当前实例的 MCP 服务"))?;
    let enabled = change["enabled"]
        .as_bool()
        .ok_or_else(|| ApiError::new(400, "MCP 开关必须为布尔值"))?;
    let original = native_document("codex", text)?;
    if !original["mcp_servers"]
        .as_object()
        .is_some_and(|servers| servers.get(id).is_some_and(Value::is_object))
    {
        return Err(ApiError::new(409, "MCP 服务已不在当前实例中，请重新读取"));
    }
    let mut document = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| ApiError::new(422, "Codex 配置格式无效，未覆盖原文件"))?;
    set_toml(
        document.as_item_mut(),
        &["mcp_servers", id, "enabled"],
        Some(toml_edit::Value::from(enabled)),
    )?;
    Ok(toml_output(text, &document))
}

async fn claude_mcp(
    state: &WebState,
    config: &super::claude::WslConfig,
    authorization: &HarnessAuthorization,
) -> Result<Value, ApiError> {
    if state.settings.read_only {
        return Ok(json!({"data":[],"message":
            "只读访问不会启动 Claude MCP 健康检查，请在该实例的原生 CLI 中查看。"}));
    }
    let mut command = tokio::process::Command::new("wsl.exe");
    command.args([
        "--distribution",
        &config.distribution,
        "--user",
        &config.user,
        "--cd",
        "~",
    ]);
    super::claude::Claude::wsl_login_exec(&mut command, &config.executable);
    command
        .args(["mcp", "list"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(crate::windows_create_hidden_process());
    // CLI 会检测已批准的 MCP；派发前再次检查写保护，不能在授权变成只读后启动本机工具进程。
    let probe = state
        .controller
        .authorization(authorization.device.clone(), Some("probeInstanceSettings"));
    let permit = probe
        .prepare_dispatch()
        .await
        .map_err(super::authorization_error)?;
    let child = permit
        .commit(|| Ok(command.spawn()?))
        .map_err(super::authorization_error)?;
    let output = tokio::time::timeout(Duration::from_secs(25), child.wait_with_output())
        .await
        .map_err(|_| ApiError::new(504, "Claude MCP 检查超时，请在该实例原生 CLI 中核对"))?
        .map_err(|_| ApiError::new(503, "无法读取所选 Claude 实例的 MCP 列表"))?;
    if !output.status.success() || output.stdout.len() > 64 * 1024 {
        return Err(ApiError::new(
            503,
            "Claude CLI 未成功返回 MCP 列表，请在该实例原生 CLI 中核对",
        ));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| ApiError::new(503, "Claude MCP 列表编码无效"))?;
    let rows = text
        .lines()
        .filter_map(|line| {
            let (name, status) = line.trim().split_once(": ")?;
            if name.is_empty()
                || name.len() > 160
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_.@/-".contains(c))
            {
                return None;
            }
            let status = if status.contains("Connected") {
                "已连接"
            } else if status.contains("Pending approval") {
                "等待原生批准"
            } else if status.contains("Failed") {
                "连接失败"
            } else if status.to_ascii_lowercase().contains("authentication") {
                "需要登录"
            } else if status.to_ascii_lowercase().contains("disabled") {
                "已关闭"
            } else {
                return None;
            };
            // 不转发 CLI 原文：命令、URL、环境和 Header 可能包含凭据，只返回受限名称和状态。
            Some(json!({"id":name,"name":name,"description":status,"canToggle":false}))
        })
        .collect::<Vec<_>>();
    if rows.is_empty() && !text.to_ascii_lowercase().contains("no mcp servers") {
        return Err(ApiError::new(
            503,
            "Claude CLI 返回的 MCP 列表格式尚未支持，未将其显示为空列表",
        ));
    }
    Ok(json!({"data":rows,"message":
        "由所选实例的 Claude CLI 返回；服务增删和项目授权继续在同一 CLI 中管理。"}))
}

async fn model_catalog(
    state: &WebState,
    instance: &str,
    authorization: &HarnessAuthorization,
    force_reload: bool,
) -> Result<Value, ApiError> {
    let value = state
        .controller
        .call_authorized(
            "models",
            json!({"instanceId":instance,"includeAvailable":true,"forceReload":force_reload}),
            authorization,
        )
        .await
        .map_err(super::authorization_error)?;
    super::native_result(value)
}

async fn save_native(
    target: &ConfigFile,
    text: &str,
    revision: &str,
    payload: &Value,
    authorization: &HarnessAuthorization,
) -> Result<(), ApiError> {
    if payload["expectedRevision"] != revision {
        return Err(ApiError::new(409, "实例配置已变化，请重新读取后再保存"));
    }
    let output = if target.backend == "codex" && payload["section"] == "mcp" {
        let changes = payload["settings"].as_object().unwrap();
        if changes.len() != 1 || !changes.contains_key("server") {
            return Err(ApiError::new(400, "只允许切换已有 MCP 服务"));
        }
        updated_mcp(text, &changes["server"])?
    } else {
        updated_native(
            &target.backend,
            payload["section"].as_str().unwrap_or("general"),
            text,
            &payload["settings"],
        )?
    };
    let native_changes = if target.backend == "claude" && payload["section"] == "hooks" {
        json!({"disableAllHooks":!payload["settings"]["enabled"].as_bool().unwrap()})
    } else {
        payload["settings"].clone()
    };
    permissions::save_settings_file(target, &output, &native_changes, revision, authorization).await
}

async fn settings_capabilities(
    state: &WebState,
    target: &ConfigFile,
    instance: &str,
    section: &str,
) -> (Value, Value, String) {
    let mut capabilities = json!({});
    let mut native_features = json!({});
    let mut capability_error = String::new();
    if matches!(section, "general" | "models")
        && let Some(config) = &target.claude_config
    {
        // CLI 能力读取失败时仍保留已接入的回顾配置；未确认的新选项禁用，并把失败原因返回页面。
        let help = match claude_help(config).await {
            Ok(help) => help,
            Err(error) => {
                capability_error = error.message;
                String::new()
            }
        };
        capabilities = json!({
            "promptSuggestions":help.contains("--prompt-suggestions")
        });
        if section == "general" && help.contains("--settings") && help.contains("doctor") {
            match claude_option_capabilities(config).await {
                Ok(value) => capabilities["extraSettings"] = value,
                Err(error) => {
                    if !capability_error.is_empty() {
                        capability_error.push(' ');
                    }
                    capability_error.push_str(&error.message);
                }
            }
        }
    }
    if target.backend == "codex"
        && instance == "codex:local"
        && ["general", "models"].contains(&section)
    {
        match state.controller.harness.settings_features().await {
            Ok(value) => native_features = value,
            Err(_) => capability_error = "所选 Codex 原生引擎未成功返回特性目录，实验能力暂不可修改，原生诊断项保持只读，请重新读取。".into(),
        }
    }
    (capabilities, native_features, capability_error)
}

async fn antigravity_settings(
    state: &WebState,
    target: &ConfigFile,
    instance: &str,
    section: &str,
    write: bool,
    payload: &Value,
    authorization: &HarnessAuthorization,
) -> Result<Response, ApiError> {
    let gemini_root = target
        .antigravity_root
        .as_deref()
        .ok_or_else(|| ApiError::new(404, "无法确认该 Antigravity 实例的配置目录"))?;
    let directory = &state.controller.state_dir;
    let model_settings = matches!(section, "general" | "models");
    let mut capability_error = String::new();
    let (catalog_result, version_result) = tokio::join!(
        async {
            if model_settings {
                Some(
                    model_catalog(
                        state,
                        instance,
                        authorization,
                        payload["forceReload"] == true,
                    )
                    .await,
                )
            } else {
                None
            }
        },
        async {
            if section == "general" {
                match target.antigravity_config.as_ref() {
                    Some(config) => Some(antigravity_version(config).await),
                    None => Some(Err("未找到当前 Antigravity CLI 可执行路径".to_owned())),
                }
            } else {
                None
            }
        }
    );
    let catalog = match catalog_result {
        Some(Ok(value)) => Some(value),
        Some(Err(failure)) if section == "general" => {
            capability_error = format!("无法读取当前 Antigravity 实例模型目录：{}", failure.message);
            None
        }
        Some(Err(failure)) => return Err(failure),
        None => None,
    };

    let _serial = permissions::CONFIG_WRITE
        .get_or_init(|| Mutex::new(()))
        .lock()
        .await;
    let mut updated = false;

    if write {
        let changes = payload["settings"]
            .as_object()
            .filter(|changes| !changes.is_empty())
            .ok_or_else(|| ApiError::new(400, "请选择需要保存的设置"))?;
        match section {
            "general" | "models" => {
                if changes.contains_key("hiddenModelIds") {
                    if changes.len() != 1 || !changes["hiddenModelIds"].is_array() {
                        return Err(ApiError::new(
                            400,
                            "模型显示设置仅支持单独修改当前 Antigravity 实例的隐藏名单",
                        ));
                    }
                    let ids = hidden_model_ids(&changes["hiddenModelIds"])?;
                    let (preference, _) = preferences(directory)?;
                    let previous =
                        hidden_model_ids(&preference["instances"][instance]["hiddenModelIds"])?;
                    validate_hidden_model_selection(&ids, &previous, catalog.as_ref())?;
                    save_preference(
                        directory,
                        instance,
                        "hiddenModelIds",
                        &json!(ids),
                        payload["expectedPreferenceRevision"]
                            .as_str()
                            .unwrap_or(""),
                        authorization,
                    )
                    .await?;
                } else if changes.contains_key("autoCompactPercent") {
                    if section != "general" || changes.len() != 1 {
                        return Err(ApiError::new(400, "自动压缩阈值需要单独保存"));
                    }
                    match version_result.as_ref() {
                        Some(Ok(ver)) if *ver >= ANTIGRAVITY_MIN_AUTO_COMPACT_VERSION => {}
                        Some(Ok((major, minor, patch))) => {
                            return Err(ApiError::new(
                                409,
                                format!(
                                    "当前 Antigravity CLI 版本为 {major}.{minor}.{patch}，需升级到 1.5.15 及以上才支持自动压缩阈值"
                                ),
                            ));
                        }
                        Some(Err(reason)) => {
                            return Err(ApiError::new(
                                409,
                                format!("未能确认当前 Antigravity CLI 版本（需 1.5.15 及以上）：{reason}"),
                            ));
                        }
                        None => {
                            return Err(ApiError::new(409, "当前 Antigravity 实例不支持自动压缩阈值"));
                        }
                    }
                    validate_auto_compact_percent("antigravity", &changes["autoCompactPercent"])?;
                    save_preference(
                        directory,
                        instance,
                        "autoCompactPercent",
                        &changes["autoCompactPercent"],
                        payload["expectedPreferenceRevision"]
                            .as_str()
                            .unwrap_or(""),
                        authorization,
                    )
                    .await?;
                } else if changes.contains_key("compactionModelEnabled") {
                    if section != "general" || changes.len() != 1 {
                        return Err(ApiError::new(400, "手动压缩模型开关需要单独保存"));
                    }
                    let enabled = changes["compactionModelEnabled"]
                        .as_bool()
                        .ok_or_else(|| ApiError::new(400, "手动压缩模型开关必须为布尔值"))?;
                    save_preference(
                        directory,
                        instance,
                        "compactionModelEnabled",
                        &json!(enabled),
                        payload["expectedPreferenceRevision"]
                            .as_str()
                            .unwrap_or(""),
                        authorization,
                    )
                    .await?;
                } else if changes.contains_key("compactionModel") {
                    if section != "general" || changes.len() != 1 {
                        return Err(ApiError::new(400, "手动压缩模型需要单独保存"));
                    }
                    let parsed = parse_compaction_model(&changes["compactionModel"])?;
                    validate_compaction_model_choice(parsed.as_deref(), catalog.as_ref())?;
                    let stored = parsed.map(Value::from).unwrap_or(Value::Null);
                    save_preference(
                        directory,
                        instance,
                        "compactionModel",
                        &stored,
                        payload["expectedPreferenceRevision"]
                            .as_str()
                            .unwrap_or(""),
                        authorization,
                    )
                    .await?;
                } else if changes.contains_key("compactionModelMode") {
                    if section != "general" || changes.len() != 1 {
                        return Err(ApiError::new(400, "手动压缩模型策略需要单独保存"));
                    }
                    let mode = parse_compaction_model_mode(&changes["compactionModelMode"])?;
                    // 显式保存用户选定的策略，避免后续调整模型或开关时覆盖已设置的策略。
                    save_preference(
                        directory,
                        instance,
                        "compactionModelMode",
                        &json!(mode.as_str()),
                        payload["expectedPreferenceRevision"]
                            .as_str()
                            .unwrap_or(""),
                        authorization,
                    )
                    .await?;
                } else {
                    return Err(ApiError::new(400, "不支持的 Antigravity 实例设置"));
                }
            }
            "mcp" => {
                if changes.len() != 1 || !changes.contains_key("server") {
                    return Err(ApiError::new(400, "只允许切换已有 Antigravity MCP 服务"));
                }
                super::antigravity::local_store::update_mcp_server_enabled(
                    gemini_root,
                    payload["expectedRevision"].as_str().unwrap_or(""),
                    &changes["server"],
                    authorization,
                )
                .await?;
            }
            "hooks" => {
                if changes.len() != 1 || !changes.contains_key("hook") {
                    return Err(ApiError::new(400, "只允许切换已有 Antigravity 钩子"));
                }
                super::antigravity::local_store::update_hook_enabled(
                    gemini_root,
                    payload["expectedRevision"].as_str().unwrap_or(""),
                    &changes["hook"],
                    authorization,
                )
                .await?;
            }
            "skills" => {
                return Err(ApiError::new(
                    400,
                    "Antigravity Skills 按用户目录自动发现，暂不支持单独开关",
                ));
            }
            _ => return Err(ApiError::new(400, "不支持的 Antigravity 设置分类")),
        }
        state.controller.shared_settings.instance_settings_changed(
            &state.controller,
            instance,
            &payload["settings"],
        );
        updated = true;
    }

    let (preference, preference_revision) = preferences(directory)?;
    let mut extra_settings = Vec::new();
    let (values, revision, path_display, inventory) = match section {
        "general" | "models" => {
            let (_, rev) = permissions::read_file(&target.path)?;
            let hidden = hidden_model_ids(&preference["instances"][instance]["hiddenModelIds"])?;
            let mut map = json!({
                "hiddenModelIds": hidden,
            });
            if section == "general" {
                let version = version_result
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|| Err("未检测到 Antigravity CLI 版本".to_owned()));
                let (value, row) = antigravity_auto_compact_row(
                    &preference["instances"][instance]["autoCompactPercent"],
                    &version,
                )?;
                let comp_enabled = parse_compaction_model_enabled(
                    &preference["instances"][instance]["compactionModelEnabled"],
                )?;
                let comp_model =
                    parse_compaction_model(&preference["instances"][instance]["compactionModel"])?;
                let comp_mode = parse_compaction_model_mode(
                    &preference["instances"][instance]["compactionModelMode"],
                )?;
                // 缓存与压缩 5 项顺序统一：上下文大小 → 自动压缩阈值 → 缓存时间 → 手动压缩模型 → 压缩提示词。
                map["contextWindow"] = Value::Null;
                map["autoCompactPercent"] = value;
                map["promptCacheTtl"] = Value::Null;
                map["compactionModelEnabled"] = json!(comp_enabled);
                map["compactionModel"] = json!(comp_model.as_deref().unwrap_or("session"));
                map["compactionModelMode"] = json!(comp_mode.as_str());
                map["compactionPrompt"] = Value::Null;
                extra_settings.push(unsupported_context_window_row(
                    "建议调小下方“自动压缩阈值”。",
                ));
                extra_settings.push(row);
                extra_settings.push(unsupported_prompt_cache_ttl_row(""));
                extra_settings.push(compaction_model_row(
                    catalog.as_ref(),
                    comp_enabled,
                    comp_model.as_deref(),
                    comp_mode,
                    false,
                    None,
                ));
                extra_settings.push(unsupported_compaction_prompt_row(""));
            }
            (map, rev, target.display.clone(), None)
        }
        "skills" => {
            let (_, rev) = permissions::read_file(&target.path)?;
            let inv = super::antigravity::local_store::scan_local_skills(
                gemini_root,
                target.antigravity_home_root.as_deref(),
            )?;
            (
                json!({}),
                rev,
                format!(
                    "{}/antigravity/skills",
                    target
                        .display
                        .trim_end_matches("/config/config.json")
                        .trim_end_matches(['/', '\\'])
                ),
                Some(inv),
            )
        }
        "mcp" => {
            let (inv, rev) = super::antigravity::local_store::read_mcp_inventory(gemini_root)?;
            (
                json!({}),
                rev,
                format!(
                    "{}/config/mcp_config.json",
                    target
                        .display
                        .trim_end_matches("/config/config.json")
                        .trim_end_matches(['/', '\\'])
                ),
                Some(inv),
            )
        }
        "hooks" => {
            let (inv, rev) = super::antigravity::local_store::read_hooks_inventory(gemini_root)?;
            (
                json!({}),
                rev,
                format!(
                    "{}/config/hooks.json",
                    target
                        .display
                        .trim_end_matches("/config/config.json")
                        .trim_end_matches(['/', '\\'])
                ),
                Some(inv),
            )
        }
        _ => return Err(ApiError::new(400, "实例设置分类无效")),
    };

    authorization
        .check()
        .await
        .map_err(super::authorization_error)?;
    let (settings_epoch, instance_revision) = state.controller.shared_settings.instance_version();
    let mut response = json!({
        "ok": true,
        "instanceId": instance,
        "backend": "antigravity",
        "settingsEpoch": settings_epoch,
        "instanceRevision": instance_revision,
        "path": path_display,
        "revision": revision,
        "preferenceRevision": preference_revision,
        "settings": values,
        "capabilities": {},
        "capabilityError": capability_error,
        "extraSettings": extra_settings,
        "readOnly": state.settings.read_only,
        "readOnlyReason": "",
        "updated": updated,
    });
    if let Some(cat) = catalog {
        let decorated = decorate_antigravity_model_visibility(directory, instance, cat)?;
        response["models"] = decorated["data"].clone();
    } else if matches!(section, "general" | "models") {
        response["models"] = json!([]);
    }
    if let Some(inv) = inventory {
        response["inventory"] = inv;
    }
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(response)).into_response())
}

pub(super) async fn settings(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Response, ApiError> {
    let device = super::authorized(&state, &headers).await?;
    let instance = payload["instanceId"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 160)
        .ok_or_else(|| ApiError::new(400, "请选择会话实例"))?;
    let section = payload["section"].as_str().unwrap_or("general");
    if !["general", "models", "skills", "mcp", "hooks"].contains(&section) {
        return Err(ApiError::new(400, "实例设置分类无效"));
    }
    let write = match payload["operation"].as_str().unwrap_or("read") {
        "read" => false,
        "save" => true,
        _ => return Err(ApiError::new(400, "实例设置操作无效")),
    };
    if write && state.settings.read_only {
        return Err(ApiError::new(403, "当前为只读访问"));
    }
    let authorization = state
        .controller
        .authorization(device, write.then_some("setInstanceSettings"));
    let environments = super::preferences::detect(&state.controller.state_dir, false).await;
    let target = permissions::config_target(&environments, instance)?;
    if target.backend == "antigravity" {
        return antigravity_settings(
            &state,
            &target,
            instance,
            section,
            write,
            &payload,
            &authorization,
        )
        .await;
    }
    let adapter_read_only = target.backend == "codex" && instance != "codex:local";
    if write && adapter_read_only {
        return Err(ApiError::new(
            409,
            "该 Codex 实例的独立执行适配器尚未接入，原生配置仅供查看",
        ));
    }
    if target.backend == "codex"
        && instance != "codex:local"
        && ["models", "skills", "hooks"].contains(&section)
    {
        return Err(ApiError::new(
            409,
            "该 Codex 实例的独立执行适配器尚未接入，未改投本机实例",
        ));
    }
    // 模型默认值并入会话实例，读取和保存共用当前实例及同一原生配置版本；models 仅保留兼容入口。
    let model_settings = matches!(section, "general" | "models") && !adapter_read_only;
    // 用户要求设置操作减少等待：能力与目录独立并行准备，普通读取/非模型保存复用原生目录缓存。
    // 只有用户明确重新读取时强制查目录；模型默认值写入后仍在下方重新取得已生效目录。
    let ((capabilities, native_features, mut capability_error), catalog_result) = tokio::join!(
        settings_capabilities(&state, &target, instance, section),
        async {
            if model_settings {
                Some(model_catalog(&state, instance, &authorization, payload["forceReload"] == true).await)
            } else {
                None
            }
        }
    );
    let mut catalog = match catalog_result {
        Some(result) => match result {
            Ok(value) => Some(value),
            Err(failure) if section == "general" => {
                // 合并后模型目录读取失败不能阻断已支持的其他设置；写模型前仍须确认同一实例的目录。
                if !capability_error.is_empty() {
                    capability_error.push(' ');
                }
                capability_error.push_str(&format!("无法读取当前实例模型目录：{}", failure.message));
                None
            }
            Err(failure) => return Err(failure),
        },
        None => None,
    };
    let _serial = permissions::CONFIG_WRITE
        .get_or_init(|| Mutex::new(()))
        .lock()
        .await;
    let (text, revision) = permissions::read_file(&target.path)?;
    let document = native_document(&target.backend, &text)?;
    let directory = &state.controller.state_dir;
    let mut updated = false;
    if write {
        let changes = payload["settings"]
            .as_object()
            .filter(|changes| !changes.is_empty())
            .ok_or_else(|| ApiError::new(400, "请选择需要保存的设置"))?;
        options::validate_changes(
            &target.backend,
            section,
            &payload["settings"],
            if target.backend == "codex" {
                &native_features
            } else {
                &capabilities["extraSettings"]
            },
        )?;
        if changes.contains_key("hiddenModelIds") {
            // 用户要求所有会话实例统一支持显示模型；写入只保存在 Elves 实例偏好，不修改 Codex/Claude 原生配置文件。
            if !matches!(section, "general" | "models") || changes.len() != 1
                || !changes["hiddenModelIds"].is_array()
            {
                return Err(ApiError::new(400, "模型显示设置仅支持单独修改当前实例的隐藏名单"));
            }
            let ids = hidden_model_ids(&changes["hiddenModelIds"])?;
            let (preference, _) = preferences(directory)?;
            let previous = hidden_model_ids(&preference["instances"][instance]["hiddenModelIds"])?;
            validate_hidden_model_selection(&ids, &previous, catalog.as_ref())?;
            save_preference(directory, instance, "hiddenModelIds", &json!(ids),
                payload["expectedPreferenceRevision"].as_str().unwrap_or(""), &authorization).await?;
        } else if changes.contains_key("persistentSideThreads") {
            if section != "general" || target.backend != "codex" || changes.len() != 1
                || !changes["persistentSideThreads"].is_boolean()
            {
                return Err(ApiError::new(400, "持久侧边开关需要单独保存布尔值"));
            }
            if changes["persistentSideThreads"] == true {
                state.controller.harness.require_persistent_sides().await
                    .map_err(super::authorization_error)?;
            }
            save_preference(directory, instance, "persistentSideThreads", &changes["persistentSideThreads"],
                payload["expectedPreferenceRevision"].as_str().unwrap_or(""), &authorization).await?;
        } else if changes.contains_key("ultraReasoningEnabled") {
            if !matches!(section, "general" | "models") || target.backend != "codex" || changes.len() != 1 {
                return Err(ApiError::new(400, "Ultra 开关需要单独保存"));
            }
            let value = &changes["ultraReasoningEnabled"];
            if !value.is_boolean() {
                return Err(ApiError::new(400, "Ultra 开关必须为布尔值"));
            }
            save_preference(
                directory,
                instance,
                "ultraReasoningEnabled",
                value,
                payload["expectedPreferenceRevision"].as_str().unwrap_or(""),
                &authorization,
            )
            .await?;
        } else if changes.contains_key("compactionModelEnabled") {
            if section != "general" || changes.len() != 1 {
                return Err(ApiError::new(400, "手动压缩模型开关需要单独保存"));
            }
            let enabled = changes["compactionModelEnabled"]
                .as_bool()
                .ok_or_else(|| ApiError::new(400, "手动压缩模型开关必须为布尔值"))?;
            save_preference(
                directory,
                instance,
                "compactionModelEnabled",
                &json!(enabled),
                payload["expectedPreferenceRevision"]
                    .as_str()
                    .unwrap_or(""),
                &authorization,
            )
            .await?;
        } else if changes.contains_key("compactionModel") {
            if section != "general" || changes.len() != 1 {
                return Err(ApiError::new(400, "手动压缩模型需要单独保存"));
            }
            let parsed = parse_compaction_model(&changes["compactionModel"])?;
            validate_compaction_model_choice(parsed.as_deref(), catalog.as_ref())?;
            let stored = parsed.map(Value::from).unwrap_or(Value::Null);
            save_preference(
                directory,
                instance,
                "compactionModel",
                &stored,
                payload["expectedPreferenceRevision"]
                    .as_str()
                    .unwrap_or(""),
                &authorization,
            )
            .await?;
        } else if changes.contains_key("compactionModelMode") {
            if section != "general" || changes.len() != 1 {
                return Err(ApiError::new(400, "手动压缩模型策略需要单独保存"));
            }
            let mode = parse_compaction_model_mode(&changes["compactionModelMode"])?;
            // 显式保存用户选定的策略，避免后续调整模型或开关时覆盖已设置的策略。
            save_preference(
                directory,
                instance,
                "compactionModelMode",
                &json!(mode.as_str()),
                payload["expectedPreferenceRevision"]
                    .as_str()
                    .unwrap_or(""),
                &authorization,
            )
            .await?;
        } else if changes.contains_key("autoCompactPercent") && target.backend == "codex" {
            if section != "general" || changes.len() != 1 {
                return Err(ApiError::new(400, "自动压缩阈值需要单独保存"));
            }
            validate_auto_compact_percent("codex", &changes["autoCompactPercent"])?;
            let expected_pref = payload["expectedPreferenceRevision"]
                .as_str()
                .unwrap_or("");
            let (_, current_pref_revision) = preferences(directory)?;
            if current_pref_revision != expected_pref {
                return Err(ApiError::new(409, "实例偏好已变化，请重新读取后再保存"));
            }
            let has_static_limit = !document["model_auto_compact_token_limit"].is_null();
            if has_static_limit {
                if payload["expectedRevision"] != revision {
                    return Err(ApiError::new(409, "实例配置已变化，请重新读取后再保存"));
                }
                // 清理原生 config.toml 旧静态 model_auto_compact_token_limit，避免与百分比动态换算冲突。
                let cleared = clear_codex_static_auto_compact(&text)?;
                permissions::save_settings_file(
                    &target,
                    &cleared,
                    &json!({"model_auto_compact_token_limit": null}),
                    &revision,
                    &authorization,
                )
                .await?;
            }
            save_preference(
                directory,
                instance,
                "autoCompactPercent",
                &changes["autoCompactPercent"],
                expected_pref,
                &authorization,
            )
            .await?;
        } else if changes.contains_key("skill") && target.backend == "claude" {
            if section != "skills" || changes.len() != 1 {
                return Err(ApiError::new(400, "Claude Skill 开关需要单独保存"));
            }
            if payload["expectedRevision"] != revision {
                return Err(ApiError::new(409, "实例配置已变化，请重新读取后再保存"));
            }
            let native = state
                .controller
                .call_authorized(
                    "skills",
                    json!({"instanceId":instance,"forceReload":true}),
                    &authorization,
                )
                .await
                .map_err(super::authorization_error)?;
            let mut native = super::native_result(native)?;
            decorate_claude_skill_inventory(&mut native, &document)?;
            let change = &changes["skill"];
            let row = native["data"]
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["id"] == change["id"]))
                .ok_or_else(|| ApiError::new(409, "Claude Skill 已不在当前实例中，请重新读取"))?;
            if row["canToggle"] != true {
                return Err(ApiError::new(403, "该 Claude Skill 由插件或原生策略管理"));
            }
            let updated = updated_claude_skill_override(&text, change)?;
            permissions::save_settings_file(
                &target,
                &updated,
                &payload["settings"],
                &revision,
                &authorization,
            )
            .await?;
            state
                .controller
                .shared_settings
                .instance_settings_changed(&state.controller, instance, &payload["settings"]);
        } else if changes.contains_key("skill") {
            if section != "skills"
                || target.backend != "codex"
                || instance != "codex:local"
                || changes.len() != 1
            {
                return Err(ApiError::new(400, "该实例尚不支持原生技能启停"));
            }
            if payload["expectedRevision"] != revision {
                return Err(ApiError::new(409, "实例配置已变化，请重新读取后再保存"));
            }
            state
                .controller
                .harness
                .settings_skills(&target.home, Some(&changes["skill"]), &authorization)
                .await
                .map_err(super::authorization_error)?;
            state
                .controller
                .shared_settings
                .instance_settings_changed(&state.controller, instance, &payload["settings"]);
        } else {
            if changes.contains_key("promptSuggestionEnabled")
                && capabilities["promptSuggestions"] != true
            {
                return Err(ApiError::new(409, "当前 Claude CLI 尚未确认支持输入建议"));
            }
            save_native(&target, &text, &revision, &payload, &authorization).await?;
        }
        if changes.contains_key("ultraReasoningEnabled")
            || changes.contains_key("persistentSideThreads")
            || changes.contains_key("autoCompactPercent") && target.backend == "codex"
            || changes.contains_key("compactionModelEnabled")
            || changes.contains_key("compactionModel")
            || changes.contains_key("compactionModelMode")
            || changes.contains_key("hiddenModelIds")
        {
            state
                .controller
                .shared_settings
                .instance_settings_changed(&state.controller, instance, &payload["settings"]);
        }
        updated = true;
    }
    if updated && model_settings && catalog.is_some()
        && payload["settings"].get("ultraReasoningEnabled").is_some()
    {
        catalog = Some(model_catalog(&state, instance, &authorization, true).await?);
    }
    let (text, revision) = permissions::read_file(&target.path)?;
    let document = native_document(&target.backend, &text)?;
    let (preference, preference_revision) = preferences(directory)?;
    let mut values = match section {
        "general" => general_view(&target.backend, &document)?,
        "models" => json!({
            "ultraReasoningEnabled":ultra_enabled(directory, instance)?
        }),
        _ => json!({}),
    };
    if target.backend == "codex" && section == "general" {
        values["persistentSideThreads"] = json!(persistent_sides_enabled(directory, instance)?);
    }
    if section == "general" && model_settings {
        values["ultraReasoningEnabled"] = json!(ultra_enabled(directory, instance)?);
    }
    if model_settings {
        values["hiddenModelIds"] = json!(hidden_model_ids(
            &preference["instances"][instance]["hiddenModelIds"])?);
    }
    let mut extra_settings = options::append_view(
        &target.backend,
        section,
        &document,
        &mut values,
        if target.backend == "codex" {
            &native_features
        } else {
            &capabilities["extraSettings"]
        },
    );
    if section == "general" {
        let ((context_value, context_row), (compact_value, compact_row), (cache_ttl_value, cache_ttl_row)) =
            if target.backend == "codex" {
                let codex_home = target.path.parent().unwrap_or(Path::new(""));
                let ctx = codex_context_window_row(codex_home, &document, adapter_read_only);
                let cmp = codex_auto_compact_row(
                    codex_home,
                    &document,
                    &preference["instances"][instance]["autoCompactPercent"],
                    adapter_read_only,
                )?;
                let ttl = (Value::Null, unsupported_prompt_cache_ttl_row(""));
                (ctx, cmp, ttl)
            } else {
                (
                    claude_context_window_row(&document),
                    claude_auto_compact_row(&document),
                    claude_prompt_cache_ttl_row(&document),
                )
            };
        let comp_enabled = parse_compaction_model_enabled(
            &preference["instances"][instance]["compactionModelEnabled"],
        )?;
        let comp_model =
            parse_compaction_model(&preference["instances"][instance]["compactionModel"])?;
        let comp_mode = parse_compaction_model_mode(
            &preference["instances"][instance]["compactionModelMode"],
        )?;
        values["contextWindow"] = context_value;
        values["autoCompactPercent"] = compact_value;
        values["promptCacheTtl"] = cache_ttl_value;
        values["compactionModelEnabled"] = json!(comp_enabled);
        values["compactionModel"] = json!(comp_model.as_deref().unwrap_or("session"));
        values["compactionModelMode"] = json!(comp_mode.as_str());
        let compaction_model_row = compaction_model_row(
            catalog.as_ref(),
            comp_enabled,
            comp_model.as_deref(),
            comp_mode,
            adapter_read_only,
            adapter_read_only
                .then(|| "该 Codex 实例的独立执行适配器尚未接入，手动压缩模型仅供查看。".to_owned()),
        );
        let (mut cache_rows, other_rows): (Vec<Value>, Vec<Value>) = extra_settings
            .into_iter()
            .partition(|row| row["group"] == "缓存与压缩");
        // 缓存与压缩 5 项顺序统一：上下文大小 → 自动压缩阈值 → 缓存时间 → 手动压缩模型 → 压缩提示词。
        let mut ordered_cache = vec![context_row, compact_row, cache_ttl_row, compaction_model_row];
        ordered_cache.append(&mut cache_rows);
        if target.backend == "claude" {
            // 用户要求 Claude 的「压缩提示词」改为不支持，右侧展示纯文字「不支持」且不保留冗余描述。
            values["compactionPrompt"] = Value::Null;
            ordered_cache.push(unsupported_compaction_prompt_row(""));
        }
        ordered_cache.extend(other_rows);
        extra_settings = ordered_cache;
    }
    authorization
        .check()
        .await
        .map_err(super::authorization_error)?;
    let (settings_epoch, instance_revision) = state.controller.shared_settings.instance_version();
    let mut response = json!({
        "ok":true,"instanceId":instance,"backend":target.backend,
        "settingsEpoch":settings_epoch,"instanceRevision":instance_revision,
        "path":target.display,"revision":revision,"preferenceRevision":preference_revision,
        "settings":values,"capabilities":capabilities,"capabilityError":capability_error,
        "extraSettings":extra_settings,
        "readOnly":state.settings.read_only||adapter_read_only,
        "readOnlyReason":if adapter_read_only {
            "该 Codex 实例的独立执行适配器尚未接入，原生配置仅供查看。"
        } else {""},
        "updated":updated
    });
    if let Some(mut catalog) = catalog {
        if target.backend == "claude" {
            // 隐藏名单可能在本次保存中改变；返回前按已确认的新偏好标记，完整模型与能力不变。
            catalog = decorate_claude_model_visibility(directory, instance, catalog)?;
        } else if target.backend == "codex" {
            catalog = decorate_codex_model_visibility(directory, instance, catalog)?;
        }
        response["models"] = catalog["data"].clone();
    }
    drop(_serial);
    if ["skills", "mcp", "hooks"].contains(&section) {
        let mut inventory = extension_view(section, &target.backend, &document)?;
        if target.backend == "codex" && ["skills", "hooks"].contains(&section) {
            if instance != "codex:local" {
                return Err(ApiError::new(
                    409,
                    "该 Codex 实例的独立执行适配器尚未接入，未改投本机实例",
                ));
            }
            let native = if section == "skills" {
                state
                    .controller
                    .harness
                    .settings_skills(&target.home, None, &authorization)
                    .await
            } else {
                state.controller.harness.settings_hooks(&target.home).await
            }
            .map_err(super::authorization_error)?;
            inventory["data"] = native["data"].clone();
            inventory["message"] = native["message"].clone();
        } else if target.backend == "claude" && section == "skills" {
            let native = state
                .controller
                .call_authorized(
                    "skills",
                    json!({"instanceId":instance,"forceReload":true}),
                    &authorization,
                )
                .await
                .map_err(super::authorization_error)?;
            let native = super::native_result(native)?;
            let mut native = native;
            decorate_claude_skill_inventory(&mut native, &document)?;
            inventory["data"] = native["data"].clone();
            inventory["message"] = json!(if state.settings.read_only {
                "只读访问不会启动 Claude Skill 探测，请在该实例的原生 CLI 中查看。"
            } else {
                "由所选实例的 Claude CLI 返回；开关保存到该实例的 skillOverrides。"
            });
        } else if target.backend == "claude" && section == "mcp" {
            inventory = claude_mcp(
                &state,
                target.claude_config.as_ref().unwrap(),
                &authorization,
            )
            .await?;
        }
        response["inventory"] = inventory;
    }
    authorization
        .check()
        .await
        .map_err(super::authorization_error)?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(response)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_persistent_sides_default_off_and_preferences_are_instance_scoped() {
        let directory = tempfile::tempdir().unwrap();
        assert!(!persistent_sides_enabled(directory.path(), "codex:local").unwrap());
        std::fs::write(directory.path().join(PREFERENCES_FILE),
            r#"{"instances":{"codex:local":{"persistentSideThreads":true},"codex:other":{"persistentSideThreads":false}}}"#).unwrap();
        assert!(persistent_sides_enabled(directory.path(), "codex:local").unwrap());
        assert!(!persistent_sides_enabled(directory.path(), "codex:other").unwrap());
        std::fs::write(directory.path().join(PREFERENCES_FILE),
            r#"{"instances":{"codex:local":{"persistentSideThreads":"true"}}}"#).unwrap();
        assert!(persistent_sides_enabled(directory.path(), "codex:local").is_err());
    }

    #[test]
    fn codex_model_visibility_defaults_to_all_and_preserves_native_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = json!({"data":[
            {"model":"gpt-5.4","supportedReasoningEfforts":[{"reasoningEffort":"high"}]},
            {"model":"gpt-5.4-mini","supportedReasoningEfforts":[{"reasoningEffort":"medium"}]}],
            "defaultModel":"gpt-5.4","defaultReasoningEffort":"high"});
        let shown = decorate_codex_model_visibility(directory.path(), "codex:local", catalog.clone()).unwrap();
        assert_eq!(shown["data"].as_array().unwrap().len(), 2);
        assert_eq!(shown["data"][0]["hiddenByInstance"], false);
        assert_eq!(shown["data"][1]["hiddenByInstance"], false);
        std::fs::write(directory.path().join(PREFERENCES_FILE),
            serde_json::to_string(&json!({"instances":{
                "codex:local":{"hiddenModelIds":["gpt-5.4-mini"]},
                "codex:wsl:ubuntu":{"hiddenModelIds":["gpt-5.4"]}
            }})).unwrap()).unwrap();
        let local = decorate_codex_model_visibility(directory.path(), "codex:local", catalog.clone()).unwrap();
        let wsl = decorate_codex_model_visibility(directory.path(), "codex:wsl:ubuntu", catalog.clone()).unwrap();
        assert_eq!(local["data"][0]["hiddenByInstance"], false);
        assert_eq!(local["data"][1]["hiddenByInstance"], true);
        assert_eq!(wsl["data"][0]["hiddenByInstance"], true);
        assert_eq!(wsl["data"][1]["hiddenByInstance"], false);
        assert_eq!(local["defaultModel"], "gpt-5.4");
        assert_eq!(local["data"][1]["supportedReasoningEfforts"], catalog["data"][1]["supportedReasoningEfforts"]);
        assert!(catalog["data"][1]["hiddenByInstance"].is_null(), "原始 Codex 目录缓存不能被展示偏好污染");
    }

    #[test]
    fn claude_model_visibility_defaults_to_all_and_preserves_native_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = json!({"data":[
            {"model":"first","supportedReasoningEfforts":[{"reasoningEffort":"high"}]},
            {"model":"second","supportedReasoningEfforts":[]}],
            "defaultModel":"first","defaultReasoningEffort":"high"});
        let shown = decorate_claude_model_visibility(directory.path(), "claude:wsl:one", catalog.clone()).unwrap();
        assert_eq!(shown["data"].as_array().unwrap().len(), 2);
        assert_eq!(shown["data"][0]["hiddenByInstance"], false);
        assert_eq!(shown["data"][1]["hiddenByInstance"], false);
        std::fs::write(directory.path().join(PREFERENCES_FILE),
            serde_json::to_string(&json!({"instances":{
                "claude:wsl:one":{"hiddenModelIds":["first"]},
                "claude:wsl:two":{"hiddenModelIds":["second"]}
            }})).unwrap()).unwrap();
        let one = decorate_claude_model_visibility(directory.path(), "claude:wsl:one", catalog.clone()).unwrap();
        let two = decorate_claude_model_visibility(directory.path(), "claude:wsl:two", catalog.clone()).unwrap();
        assert_eq!(one["data"][0]["hiddenByInstance"], true);
        assert_eq!(one["data"][1]["hiddenByInstance"], false);
        assert_eq!(two["data"][0]["hiddenByInstance"], false);
        assert_eq!(two["data"][1]["hiddenByInstance"], true);
        assert_eq!(one["defaultModel"], "first");
        assert_eq!(one["data"][0]["supportedReasoningEfforts"], catalog["data"][0]["supportedReasoningEfforts"]);
        assert!(catalog["data"][0]["hiddenByInstance"].is_null(), "原始目录缓存不能被展示偏好污染");
    }

    #[test]
    fn claude_model_visibility_new_models_show_and_reset_reuses_native_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let preference = directory.path().join(PREFERENCES_FILE);
        std::fs::write(&preference, r#"{"instances":{"claude:wsl:one":{"hiddenModelIds":["old"]}}}"#).unwrap();
        let catalog = json!({"data":[{"model":"old"},{"model":"new"}],"catalogComplete":true});
        let shown = decorate_claude_model_visibility(directory.path(), "claude:wsl:one", catalog.clone()).unwrap();
        assert_eq!(shown["data"][0]["hiddenByInstance"], true);
        assert_eq!(shown["data"][1]["hiddenByInstance"], false);
        std::fs::write(&preference, r#"{"instances":{"claude:wsl:one":{"hiddenModelIds":[]}}}"#).unwrap();
        let restored = decorate_claude_model_visibility(directory.path(), "claude:wsl:one", catalog).unwrap();
        assert!(restored["data"].as_array().unwrap().iter().all(|row| row["hiddenByInstance"] == false));
        assert_eq!(restored["catalogComplete"], true);
    }

    #[test]
    fn claude_model_visibility_validates_ids_without_writing_native_settings() {
        assert_eq!(hidden_model_ids(&Value::Null).unwrap(), Vec::<String>::new());
        assert!(hidden_model_ids(&json!(["model", "model"])).is_err());
        assert!(hidden_model_ids(&json!([""])).is_err());
        assert!(hidden_model_ids(&json!(["bad\nid"])).is_err());
        assert!(hidden_model_ids(&json!(["x".repeat(201)])).is_err());
        assert!(hidden_model_ids(&json!((0..513).map(|id| format!("model-{id}")).collect::<Vec<_>>())).is_err());
        let catalog = json!({"data":[{"model":"present"}]});
        assert!(validate_hidden_model_selection(&["present".into()], &[], Some(&catalog)).is_ok());
        assert!(validate_hidden_model_selection(&["unknown".into()], &[], Some(&catalog)).is_err());
        assert!(validate_hidden_model_selection(&["retired".into()], &["retired".into()], None).is_ok());
        assert!(validate_hidden_model_selection(&[], &["retired".into()], None).is_ok());
        assert!(updated_native("claude", "general", "{}", &json!({"hiddenModelIds":["present"]})).is_err());
        assert!(updated_native("codex", "general", "", &json!({"hiddenModelIds":["present"]})).is_err());
    }

    #[test]
    fn claude_skill_overrides_preserve_states_and_disable_plugin_toggles() {
        let before = "\u{feff}{\r\n  \"skillOverrides\": {\r\n    \"review\": \"name-only\"\r\n  },\r\n  \"model\": \"keep\"\r\n}\r\n";
        let mut inventory = json!({"data":[
            {"id":"claude-skill:review","name":"review","scope":"native"},
            {"id":"claude-skill:plugin-check","name":"plugin-check","scope":"plugin"}
        ]});
        let document = native_document("claude", before).unwrap();
        decorate_claude_skill_inventory(&mut inventory, &document).unwrap();
        assert_eq!(inventory["data"][0]["override"], "name-only");
        assert_eq!(inventory["data"][0]["enabled"], true);
        assert_eq!(inventory["data"][0]["canToggle"], true);
        assert_eq!(inventory["data"][1]["canToggle"], false);
        assert_eq!(inventory["data"][1]["readOnlyReason"], "插件 Skill 由插件管理");

        let disabled = updated_claude_skill_override(
            before,
            &json!({"id":"claude-skill:review","enabled":false}),
        )
        .unwrap();
        let disabled_document = native_document("claude", &disabled).unwrap();
        assert_eq!(disabled_document["skillOverrides"]["review"], "off");
        assert_eq!(disabled_document["model"], "keep");
        assert!(disabled.starts_with('\u{feff}'));
        assert!(disabled.contains("\r\n"));

        let restored = updated_claude_skill_override(
            &disabled,
            &json!({"id":"claude-skill:review","enabled":true}),
        )
        .unwrap();
        let restored_document = native_document("claude", &restored).unwrap();
        assert!(restored_document["skillOverrides"].is_null());
        assert_eq!(restored_document["model"], "keep");
        assert!(updated_claude_skill_override(
            before,
            &json!({"id":"claude-skill:unknown","enabled":false})
        )
        .is_ok());
        assert!(updated_claude_skill_override(
            before,
            &json!({"id":"claude-skill:review","enabled":"off"})
        )
        .is_err());
    }

    #[test]
    fn merged_instance_settings_preserve_extra_and_compaction_native_contracts() {
        let before = "# 保留配置\nmodel = \"old\"\nmodel_reasoning_effort = \"high\"\n[features]\ngoals = true\n";
        // 用户要求移除设置页「实例默认值」，不再允许通过实例设置改写 Codex/Claude 默认模型与默认思考深度。
        assert!(updated_native("codex", "general", before, &json!({"model":"new"})).is_err());
        assert!(updated_native("codex", "general", before, &json!({"reasoningEffort":"low"})).is_err());
        assert!(updated_native("claude", "general", "{}", &json!({"model":"selected-model"})).is_err());
        assert!(updated_native("claude", "general", "{}", &json!({"reasoningEffort":"max"})).is_err());
        let updated = updated_native("codex", "general", before,
            &json!({"reasoningSummary":"detailed","defaultServiceTier":"priority"})).unwrap();
        let saved = native_document("codex", &updated).unwrap();
        assert_eq!(saved["model"], "old");
        assert_eq!(saved["model_reasoning_effort"], "high");
        assert_eq!(saved["model_reasoning_summary"], "detailed");
        assert_eq!(saved["service_tier"], "priority");
        assert_eq!(saved["features"]["goals"], true);
        assert!(updated.contains("# 保留配置"));
        assert!(updated_native("codex", "general", before, &json!({"compactionModel":"invented"})).is_err());
        assert!(updated_native("codex", "general", before, &json!({"autoCompactTokenLimit":600000})).is_err());
    }

    #[test]
    fn codex_compaction_prompt_reuses_manager_default_and_respects_native_overrides() {
        let default = elves_core::layered_compaction::DEFAULT_COMPACTION_PROMPT;
        let mut params = json!({"config":{"model_reasoning_effort":"high"}});
        apply_codex_compaction_default(&json!({}), &mut params);
        assert_eq!(params["config"]["compact_prompt"], default);
        assert_eq!(params["config"]["model_reasoning_effort"], "high");
        for config in [json!({"compact_prompt":"custom"}),
            json!({"experimental_compact_prompt_file":"/fixture/custom.md"}),
            json!({"config":{},"layers":[{"config":{"experimental_compact_prompt_file":"/fixture/project.md"}}]}),
            json!({"config":{"compact_prompt":"effective"},"layers":[]})] {
            let mut params = json!({"config":{"model_reasoning_effort":"high"}});
            apply_codex_compaction_default(&config, &mut params);
            assert!(params["config"]["compact_prompt"].is_null());
        }
        let before = "\u{feff}# 保留\r\nmodel = \"fixture\" # 模型注释\r\n";
        let saved = updated_native("codex", "general", before,
            &json!({"compactionPrompt":"第一行\n第二行"})).unwrap();
        assert_eq!(native_document("codex", &saved).unwrap()["compact_prompt"], "第一行\n第二行");
        let cleared = updated_native("codex", "general", &saved,
            &json!({"compactionPrompt":null})).unwrap();
        assert!(native_document("codex", &cleared).unwrap()["compact_prompt"].is_null());
        assert!(cleared.contains("# 模型注释"));
        let mut values = json!({});
        let rows = options::append_view("codex", "general", &json!({}), &mut values, &features());
        let prompt = rows.iter().find(|row| row["id"] == "compactionPrompt").unwrap();
        assert_eq!(prompt["defaultValue"], default);
        assert_eq!(prompt["readOnly"], false);
    }

    #[test]
    fn claude_compaction_prompt_is_unsupported_and_rejected() {
        let row = unsupported_compaction_prompt_row("");
        assert_eq!(row["id"], "compactionPrompt");
        assert_eq!(row["group"], "缓存与压缩");
        assert_eq!(row["readOnly"], true);
        assert_eq!(row["cliPreview"], "不支持");
        assert_eq!(row["readOnlyReason"], "");
        assert!(updated_native("claude", "general", "{}", &json!({"compactionPrompt":"自定义"})).is_err());
    }

    fn features() -> Value {
        json!({
            "chronicle":{"stage":"underDevelopment"},
            "context_management":{"stage":"underDevelopment"},
            "local_thread_store_compression":{"stage":"underDevelopment"}
        })
    }

    #[test]
    fn extra_codex_settings_preserve_unrelated_data_comments_and_encoding() {
        let before = "\u{feff}# 保留配置\r\nmodel = \"fixture-model\" # 模型注释\r\n\
            [mcp_servers.fixture]\r\nurl = \"https://fixture.invalid\"\r\n\
            [mcp_servers.fixture.env]\r\nAPI_KEY = \"fixture-secret\"\r\n";
        let first = updated_native(
            "codex",
            "general",
            before,
            &json!({"generateMemories":false,"useMemories":true,"maxConcurrentThreads":12}),
        )
        .unwrap();
        let second = updated_native("codex", "general", &first,
            &json!({"chronicle":false,"experimentalContextManagement":true,"threadStoreCompression":true})).unwrap();
        let after = updated_native("codex", "models", &second,
            &json!({"reasoningSummary":"detailed","defaultServiceTier":"priority"})).unwrap();
        assert!(after.starts_with('\u{feff}'));
        assert!(after.contains("# 模型注释\r\n"));
        assert!(!after.replace("\r\n", "").contains('\n'));
        let original = native_document("codex", before).unwrap();
        let saved = native_document("codex", &after).unwrap();
        assert_eq!(saved["model"], original["model"]);
        assert_eq!(saved["mcp_servers"], original["mcp_servers"]);
        assert_eq!(saved["agents"]["max_concurrent_threads_per_session"], 12);
        assert_eq!(saved["memories"]["generate_memories"], false);
        assert_eq!(
            saved["features"]["context_management"]["experimental_mode"],
            true
        );
        assert_eq!(saved["model_reasoning_summary"], "detailed");
        let reset = updated_native("codex", "general", &after,
            &json!({"generateMemories":null,"maxConcurrentThreads":null,"experimentalContextManagement":null})).unwrap();
        let reset = native_document("codex", &reset).unwrap();
        assert!(reset["memories"]["generate_memories"].is_null());
        assert!(reset["agents"]["max_concurrent_threads_per_session"].is_null());
        assert!(reset["features"]["context_management"].is_null());
        assert_eq!(reset["mcp_servers"], original["mcp_servers"]);
    }

    #[test]
    fn extra_codex_context_accepts_boolean_and_empty_native_defaults() {
        let before = "[features]\ncontext_management = true\nmemories = true\n";
        let disabled = updated_native(
            "codex",
            "general",
            before,
            &json!({"experimentalContextManagement":false}),
        )
        .unwrap();
        let document = native_document("codex", &disabled).unwrap();
        assert_eq!(
            document["features"]["context_management"]["experimental_mode"],
            false
        );
        assert_eq!(document["features"]["memories"], true);
        let reset = updated_native(
            "codex",
            "general",
            before,
            &json!({"experimentalContextManagement":null}),
        )
        .unwrap();
        assert!(
            native_document("codex", &reset).unwrap()["features"]["context_management"].is_null()
        );
        assert!(
            updated_native(
                "codex",
                "general",
                "",
                &json!({"experimentalContextManagement":true})
            )
            .is_ok()
        );
    }

    #[test]
    fn extra_settings_validate_native_capabilities_sections_and_scalar_types() {
        let legacy = json!({"service_tier":"fast"});
        let mut values = json!({});
        options::append_view("codex", "models", &legacy, &mut values, &json!({}));
        assert_eq!(values["defaultServiceTier"], "priority");
        assert_eq!(legacy["service_tier"], "fast");
        let untouched = updated_native(
            "codex",
            "models",
            "service_tier = \"fast\"\n",
            &json!({"reasoningSummary":"detailed"}),
        )
        .unwrap();
        assert_eq!(
            native_document("codex", &untouched).unwrap()["service_tier"],
            "fast"
        );
        for changes in [
            json!({"chronicle":true}),
            json!({"experimentalContextManagement":false}),
            json!({"threadStoreCompression":null}),
        ] {
            assert!(options::validate_changes("codex", "general", &changes, &features()).is_ok());
            assert!(options::validate_changes("codex", "general", &changes, &json!({})).is_err());
        }
        let removed = json!({"chronicle":{"stage":"removed"}});
        assert!(
            options::validate_changes("codex", "general", &json!({"chronicle":true}), &removed)
                .is_err()
        );
        for changes in [
            json!({"maxConcurrentThreads":0}),
            json!({"maxConcurrentThreads":1.5}),
            json!({"generateMemories":"true"}),
            json!({"unknownSetting":true}),
        ] {
            assert!(updated_native("codex", "general", "", &changes).is_err());
        }
        assert!(
            updated_native(
                "codex",
                "general",
                "",
                &json!({"reasoningSummary":"detailed"})
            )
            .is_ok()
        );
        assert!(
            updated_native(
                "codex",
                "models",
                "",
                &json!({"reasoningSummary":"invalid"})
            )
            .is_err()
        );
        assert!(
            updated_native(
                "codex",
                "models",
                "",
                &json!({"autoCompactTokenLimit":900000})
            )
            .is_err()
        );
    }

    #[test]
    fn extra_views_expose_only_safe_whitelisted_values_and_invalid_state() {
        let document = json!({
            "features":{"chronicle":"fixture-secret","js_repl":true},
            "env":{"API_KEY":"fixture-secret"},
            "mcp_servers":{"fixture":{"http_headers":{"Authorization":"fixture-secret"}}}
        });
        let mut values = general_view("codex", &document).unwrap();
        let mut native = features();
        native["js_repl"] = json!({"stage":"removed"});
        let rows = options::append_view("codex", "general", &document, &mut values, &native);
        let invalid = rows.iter().find(|row| row["id"] == "chronicle").unwrap();
        assert_eq!(invalid["valueInvalid"], true);
        assert_eq!(invalid["readOnly"], true);
        let removed = rows.iter().find(|row| row["id"] == "legacyJsRepl").unwrap();
        assert!(
            removed["readOnlyReason"]
                .as_str()
                .unwrap()
                .contains("已移除")
        );
        let serialized =
            serde_json::to_string(&json!({"settings":values,"extraSettings":rows})).unwrap();
        assert!(!serialized.contains("fixture-secret"));
        assert!(!serialized.contains("Authorization"));
        assert!(!serialized.contains("API_KEY"));
    }

    #[test]
    fn claude_extra_booleans_roundtrip_without_rewriting_other_native_settings() {
        let original = json!({"model":"opus","env":{"API_KEY":"fixture-secret"},
            "permissions":{"deny":["fixture-command"]},"modelSettings":{"claude-fixture":{"effortLevel":"high"}}});
        let mut text = format!(
            "\u{feff}{}\r\n",
            serde_json::to_string_pretty(&original)
                .unwrap()
                .replace('\n', "\r\n")
        );
        let capabilities = Value::Object(
            CLAUDE_BOOLEAN_OPTIONS
                .iter()
                .map(|key| ((*key).into(), json!(true)))
                .collect(),
        );
        for key in CLAUDE_BOOLEAN_OPTIONS {
            let change = json!({*key:true});
            assert!(options::validate_changes("claude", "general", &change, &capabilities).is_ok());
            assert!(options::validate_changes("claude", "general", &change, &json!({})).is_err());
            text = updated_native("claude", "general", &text, &change).unwrap();
            assert_eq!(native_document("claude", &text).unwrap()[*key], true);
            text = updated_native("claude", "general", &text, &json!({*key:null})).unwrap();
            assert!(native_document("claude", &text).unwrap()[*key].is_null());
            assert!(updated_native("claude", "general", &text, &json!({*key:1})).is_err());
        }
        assert!(text.starts_with('\u{feff}'));
        assert!(!text.replace("\r\n", "").contains('\n'));
        assert_eq!(native_document("claude", &text).unwrap(), original);
    }

    #[test]
    fn diagnostic_environment_and_model_overrides_remain_readonly_and_redacted() {
        let document = json!({"env":{"CLAUDE_CODE_PROMPT_CACHE_TTL":"1h",
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW":"600000","CLAUDE_AUTOCOMPACT_PCT_OVERRIDE":"95",
            "ANTHROPIC_AUTH_TOKEN":"fixture-secret"},
            "modelSettings":{"claude-fixture":{"effortLevel":"xhigh"},"invalid model":{"effortLevel":"fixture-secret"}}});
        let mut values = general_view("claude", &document).unwrap();
        let rows = options::append_view("claude", "general", &document, &mut values, &json!({}));
        for key in [
            "nativePromptCacheTtl",
            "nativeAutoCompactWindow",
            "nativeAutoCompactPercent",
        ] {
            assert!(
                !rows.iter().any(|row| row["id"] == key),
                "{key} 已提升至「缓存与压缩」正式配置项，不再重复作为只读诊断项展示"
            );
        }
        let models = options::append_view("claude", "models", &document, &mut values, &json!({}));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["value"], "xhigh");
        assert_eq!(models[0]["readOnly"], true);
        assert!(
            !serde_json::to_string(&json!({"values":values,"rows":rows,"models":models}))
                .unwrap()
                .contains("fixture-secret")
        );
        assert!(
            updated_native(
                "claude",
                "models",
                "{}",
                &json!({"nativeModelEffort:claude-fixture":"max"})
            )
            .is_err()
        );
    }

    #[test]
    fn antigravity_model_visibility_defaults_and_permission_preferences_are_instance_scoped() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(PREFERENCES_FILE);
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "instances": {
                    "antigravity:local": {
                        "defaultModel": "claude-opus-5-5",
                        "defaultReasoningEffort": "high",
                        "hiddenModelIds": ["gpt-oss-120b"],
                        "defaultPermissionMode": "accept-edits"
                    },
                    "antigravity:wsl:other": {
                        "defaultPermissionMode": "bypassPermissions"
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let catalog = json!({
            "data": [
                {
                    "model": "gemini-4-neon",
                    "supportedReasoningEfforts": [{"reasoningEffort": "high"}, {"reasoningEffort": "xhigh"}]
                },
                {
                    "model": "claude-opus-5-5",
                    "supportedReasoningEfforts": [{"reasoningEffort": "medium"}, {"reasoningEffort": "high"}]
                },
                {
                    "model": "gpt-oss-120b",
                    "supportedReasoningEfforts": [{"reasoningEffort": "medium"}]
                }
            ],
            "defaultModel": "gemini-4-neon",
            "defaultReasoningEffort": "xhigh"
        });
        let local = decorate_antigravity_model_visibility(
            directory.path(),
            "antigravity:local",
            catalog.clone(),
        )
        .unwrap();
        // 移除「实例默认值」后，仅叠加显示模型标记，不再用旧偏好覆盖 ACP 原生 defaultModel / defaultReasoningEffort。
        assert_eq!(local["defaultModel"], "gemini-4-neon");
        assert_eq!(local["defaultReasoningEffort"], "xhigh");
        assert_eq!(local["data"][0]["hiddenByInstance"], false);
        assert_eq!(local["data"][1]["hiddenByInstance"], false);
        assert_eq!(local["data"][2]["hiddenByInstance"], true);

        let other = decorate_antigravity_model_visibility(
            directory.path(),
            "antigravity:wsl:other",
            catalog,
        )
        .unwrap();
        assert_eq!(other["defaultModel"], "gemini-4-neon");
        assert_eq!(other["data"][2]["hiddenByInstance"], false);

        assert_eq!(
            antigravity_default_permission_mode(directory.path(), "antigravity:local").unwrap(),
            "accept-edits"
        );
        assert_eq!(
            antigravity_default_permission_mode(directory.path(), "antigravity:wsl:other").unwrap(),
            "bypassPermissions"
        );
        assert_eq!(
            antigravity_default_permission_mode(directory.path(), "antigravity:wsl:unset").unwrap(),
            "default"
        );
    }

    #[test]
    fn unified_auto_compact_percent_rules_cover_codex_claude_and_antigravity() {
        // 1. 通用校验：三端统一范围 50～98%。
        assert!(validate_auto_compact_percent("codex", &json!(49)).is_err());
        assert_eq!(validate_auto_compact_percent("codex", &json!(50)).unwrap(), Some(50));
        assert_eq!(validate_auto_compact_percent("codex", &json!(98)).unwrap(), Some(98));
        assert!(validate_auto_compact_percent("codex", &json!(99)).is_err());
        assert_eq!(validate_auto_compact_percent("codex", &Value::Null).unwrap(), None);

        assert!(validate_auto_compact_percent("claude", &json!(49)).is_err());
        assert_eq!(validate_auto_compact_percent("claude", &json!(50)).unwrap(), Some(50));
        assert_eq!(validate_auto_compact_percent("claude", &json!(98)).unwrap(), Some(98));
        assert!(validate_auto_compact_percent("claude", &json!(99)).is_err());

        assert!(validate_auto_compact_percent("antigravity", &json!(49)).is_err());
        assert_eq!(validate_auto_compact_percent("antigravity", &json!(50)).unwrap(), Some(50));
        assert_eq!(validate_auto_compact_percent("antigravity", &json!(98)).unwrap(), Some(98));
        assert!(validate_auto_compact_percent("antigravity", &json!(99)).is_err());

        // 2. Codex：按实例模型目录动态换算 model_auto_compact_token_limit，不写死 1M。
        let dir = tempfile::tempdir().unwrap();
        let codex_home = dir.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).unwrap();
        std::fs::write(
            codex_home.join("custom-catalog.json"),
            serde_json::to_string(&json!({
                "models": [
                    {"slug": "gpt-6.1-sol", "context_window": 600000, "max_context_window": 600000},
                    {"slug": "gpt-6-luna", "context_window": 372000, "max_context_window": 372000}
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join(PREFERENCES_FILE),
            serde_json::to_string(&json!({
                "instances": {
                    "codex:local": { "autoCompactPercent": 85 },
                    "antigravity:local": { "autoCompactPercent": 88 }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let codex_doc = json!({"model_catalog_json": "custom-catalog.json", "model": "gpt-6.1-sol"});
        let mut params_sol = json!({"model": "gpt-6.1-sol"});
        apply_codex_auto_compact(
            dir.path(),
            "codex:local",
            &codex_home,
            &codex_doc,
            Some("gpt-6.1-sol"),
            &mut params_sol,
        );
        assert_eq!(params_sol["config"]["model_auto_compact_token_limit"], 510_000);
        let mut params_luna = json!({"model": "gpt-6-luna"});
        apply_codex_auto_compact(
            dir.path(),
            "codex:local",
            &codex_home,
            &codex_doc,
            Some("gpt-6-luna"),
            &mut params_luna,
        );
        assert_eq!(params_luna["config"]["model_auto_compact_token_limit"], 316_200);

        // Codex 读取视图：输入框左侧只显示数值，不附带配置 key 或描述介绍。
        let (v_set, row_set) =
            codex_auto_compact_row(&codex_home, &codex_doc, &json!(85), false).unwrap();
        assert_eq!(v_set, 85);
        assert_eq!(row_set["cliPreview"], "510,000");
        assert_eq!(row_set["unit"], "%");
        let (v_native, row_native) = codex_auto_compact_row(
            &codex_home,
            &json!({"model_catalog_json":"custom-catalog.json","model_auto_compact_token_limit":900000}),
            &Value::Null,
            false,
        )
        .unwrap();
        assert!(v_native.is_null());
        assert_eq!(row_native["cliPreview"], "900,000");
        let (_, row_default) =
            codex_auto_compact_row(&codex_home, &codex_doc, &Value::Null, false).unwrap();
        assert_eq!(row_default["cliPreview"], "540,000");

        // 清理 Codex 静态 model_auto_compact_token_limit 时保留注释和无关配置。
        let cleaned = clear_codex_static_auto_compact(
            "# 注释\nmodel = \"gpt-6.1-sol\"\nmodel_auto_compact_token_limit = 900000\n",
        )
        .unwrap();
        assert!(cleaned.contains("# 注释"));
        assert!(native_document("codex", &cleaned).unwrap()["model_auto_compact_token_limit"].is_null());

        // 3. Claude：直接读写 settings.json 的 env.CLAUDE_AUTOCOMPACT_PCT_OVERRIDE，保留其他 env 键。
        let claude_before = "{\n  \"env\": {\n    \"ANTHROPIC_BASE_URL\": \"https://fixture.invalid\",\n    \"CLAUDE_CODE_MAX_CONTEXT_TOKENS\": \"600000\"\n  }\n}\n";
        let claude_saved = updated_native(
            "claude",
            "general",
            claude_before,
            &json!({"autoCompactPercent": 85}),
        )
        .unwrap();
        let claude_doc = native_document("claude", &claude_saved).unwrap();
        assert_eq!(claude_doc["env"]["CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"], "85");
        assert_eq!(claude_doc["env"]["ANTHROPIC_BASE_URL"], "https://fixture.invalid");
        let (c_val, c_row) = claude_auto_compact_row(&claude_doc);
        assert_eq!(c_val, 85);
        assert_eq!(c_row["cliPreview"], "85%");
        let (_, c_low_row) = claude_auto_compact_row(&json!({"env":{"CLAUDE_AUTOCOMPACT_PCT_OVERRIDE":"40"}}));
        assert_eq!(c_low_row["cliPreview"], "原生配置：40%（低于 50%）");
        let (_, c_high_row) = claude_auto_compact_row(&json!({"env":{"CLAUDE_AUTOCOMPACT_PCT_OVERRIDE":"99"}}));
        assert_eq!(c_high_row["cliPreview"], "原生配置：99%（高于 98%）");
        let claude_cleared = updated_native(
            "claude",
            "general",
            &claude_saved,
            &json!({"autoCompactPercent": null}),
        )
        .unwrap();
        let claude_cleared_doc = native_document("claude", &claude_cleared).unwrap();
        assert!(claude_cleared_doc["env"]["CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"].is_null());
        assert_eq!(claude_cleared_doc["env"]["ANTHROPIC_BASE_URL"], "https://fixture.invalid");
        let (c_def_val, c_def_row) = claude_auto_compact_row(&claude_cleared_doc);
        assert!(c_def_val.is_null());
        assert_eq!(c_def_row["cliPreview"], "原生默认：预留 33k（约 94.5%）");
        assert!(
            updated_native("claude", "general", claude_before, &json!({"autoCompactPercent": 49}))
                .is_err()
        );
        assert!(
            updated_native("claude", "general", claude_before, &json!({"autoCompactPercent": 99}))
                .is_err()
        );
        assert!(
            updated_native("claude", "general", claude_before, &json!({"autoCompactWindow": 600000}))
                .is_err()
        );

        // 4. Antigravity：版本 >= 1.5.15 可写，低于 1.5.15 只读；环境变量返回 "88%"。
        assert_eq!(parse_semver("1.5.18"), Some((1, 5, 18)));
        assert_eq!(parse_semver("agy v1.5.15-beta.1"), Some((1, 5, 15)));
        assert_eq!(
            antigravity_auto_compact_override(dir.path(), "antigravity:local").unwrap(),
            Some("88%".into())
        );
        let (a_val, a_row) =
            antigravity_auto_compact_row(&json!(88), &Ok((1, 5, 18))).unwrap();
        assert_eq!(a_val, 88);
        assert_eq!(a_row["readOnly"], false);
        assert_eq!(a_row["cliPreview"], "88%");
        let (_, a_old) = antigravity_auto_compact_row(&Value::Null, &Ok((1, 5, 14))).unwrap();
        assert_eq!(a_old["readOnly"], true);
        assert!(a_old["readOnlyReason"].as_str().unwrap().contains("1.5.15"));
        assert_eq!(a_old["cliPreview"], "原生默认（随模型）");

        let model_catalog = json!({"data":[
            {"model":"gpt-6.1-sol","displayName":"GPT-6.1 Sol"},
            {"model":"gpt-6.1-terra","displayName":"GPT-6.1 Terra"}
        ]});
        // 未设置过偏好时：开关默认关闭、模型默认跟随会话模型、策略默认「默认使用」。
        let unset_pref = compaction_model_preference(dir.path(), "codex:unset").unwrap();
        assert_eq!(
            unset_pref,
            CompactionModelPreference {
                enabled: false,
                model: None,
                mode: CompactionModelMode::Always,
            }
        );
        let model_row = compaction_model_row(
            Some(&model_catalog),
            false,
            Some("gpt-6.1-sol"),
            CompactionModelMode::Always,
            false,
            None,
        );
        assert_eq!(model_row["id"], "compactionModel");
        assert_eq!(model_row["label"], "手动压缩模型");
        assert_eq!(model_row["group"], "缓存与压缩");
        assert_eq!(model_row["readOnly"], false);
        assert_eq!(model_row["enabledKey"], "compactionModelEnabled");
        assert_eq!(model_row["enabledValue"], false);
        assert_eq!(model_row["value"], "gpt-6.1-sol");
        assert_eq!(model_row["modeKey"], "compactionModelMode");
        assert_eq!(model_row["modeValue"], "always");
        assert_eq!(model_row["modeOptions"].as_array().unwrap().len(), 2);
        assert_eq!(model_row["modeOptions"][0]["value"], "always");
        assert_eq!(model_row["modeOptions"][0]["description"], "手动压缩时始终切换");
        assert_eq!(model_row["modeOptions"][1]["value"], "cache_expired");
        assert_eq!(model_row["modeOptions"][1]["description"], "无有效缓存时切换");
        assert_eq!(model_row["options"].as_array().unwrap().len(), 3);

        // 压缩模型开关与策略解析：关闭时绝不切换；开启后缓存失效使用仅在 cache_valid == false 时切换；默认使用始终切换；同模型不切换。
        let pref_disabled = CompactionModelPreference {
            enabled: false,
            model: Some("gpt-6.1-sol".into()),
            mode: CompactionModelMode::Always,
        };
        assert_eq!(resolve_manual_compaction_model(&pref_disabled, "gpt-6.1-terra", false), None);
        let pref_expired = CompactionModelPreference {
            enabled: true,
            model: Some("gpt-6.1-sol".into()),
            mode: CompactionModelMode::CacheExpired,
        };
        assert_eq!(resolve_manual_compaction_model(&pref_expired, "gpt-6.1-terra", true), None);
        assert_eq!(
            resolve_manual_compaction_model(&pref_expired, "gpt-6.1-terra", false).as_deref(),
            Some("gpt-6.1-sol")
        );
        assert_eq!(resolve_manual_compaction_model(&pref_expired, "gpt-6.1-sol", false), None);
        let pref_always = CompactionModelPreference {
            enabled: true,
            model: Some("gpt-6.1-sol".into()),
            mode: CompactionModelMode::Always,
        };
        assert!(!pref_disabled.needs_cache_check("gpt-6.1-terra"));
        assert!(!pref_always.needs_cache_check("gpt-6.1-terra"));
        assert!(!pref_expired.needs_cache_check("GPT-6.1-sol"));
        assert!(pref_expired.needs_cache_check("gpt-6.1-terra"));
        assert_eq!(
            resolve_manual_compaction_model(&pref_always, "gpt-6.1-terra", true).as_deref(),
            Some("gpt-6.1-sol")
        );
        // 已有策略在模型切回「跟随会话模型」或关闭开关后仍独立保留，不被重置。
        std::fs::write(
            dir.path().join(PREFERENCES_FILE),
            serde_json::to_string(&json!({
                "instances": {
                    "codex:saved": {
                        "compactionModelEnabled": false,
                        "compactionModelMode": "cache_expired"
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let preserved = compaction_model_preference(dir.path(), "codex:saved").unwrap();
        assert!(!preserved.enabled);
        assert_eq!(preserved.model, None);
        assert_eq!(preserved.mode, CompactionModelMode::CacheExpired);
        assert!(validate_compaction_model_choice(Some("gpt-6.1-sol"), Some(&model_catalog)).is_ok());
        assert!(validate_compaction_model_choice(Some("unknown-model"), Some(&model_catalog)).is_err());

        let prompt_row = unsupported_compaction_prompt_row("");
        assert_eq!(prompt_row["id"], "compactionPrompt");
        assert_eq!(prompt_row["group"], "缓存与压缩");
        assert_eq!(prompt_row["readOnly"], true);
        assert_eq!(prompt_row["cliPreview"], "不支持");
        assert_eq!(prompt_row["readOnlyReason"], "");
    }

    #[test]
    fn context_window_and_prompt_cache_ttl_cover_codex_claude_and_antigravity() {
        // 1. Codex 上下文大小读写 config.toml 的 model_context_window，缓存时间标为不支持。
        let before_codex = "\u{feff}# 保留注释\r\nmodel = \"gpt-6.1-sol\"\r\n";
        assert!(updated_native("codex", "general", before_codex, &json!({"contextWindow": 10_000})).is_err());
        let codex_saved = updated_native("codex", "general", before_codex, &json!({"contextWindow": 600_000})).unwrap();
        assert!(codex_saved.starts_with('\u{feff}'));
        assert!(codex_saved.contains("# 保留注释\r\n"));
        let codex_doc = native_document("codex", &codex_saved).unwrap();
        assert_eq!(codex_doc["model_context_window"], 600_000);
        let dir = tempfile::tempdir().unwrap();
        let (cv, crow) = codex_context_window_row(dir.path(), &codex_doc, false);
        assert_eq!(cv, 600_000);
        assert_eq!(crow["cliPreview"], "600,000");
        assert_eq!(crow["unit"], "TK");
        assert_eq!(crow["readOnly"], false);
        let codex_cleared = updated_native("codex", "general", &codex_saved, &json!({"contextWindow": null})).unwrap();
        let codex_cleared_doc = native_document("codex", &codex_cleared).unwrap();
        assert!(codex_cleared_doc["model_context_window"].is_null());
        let (cv_def, crow_def) = codex_context_window_row(dir.path(), &codex_cleared_doc, false);
        assert!(cv_def.is_null());
        assert_eq!(crow_def["cliPreview"], "");
        std::fs::write(
            dir.path().join("models_cache.json"),
            serde_json::to_string(&json!({
                "models": [{"slug": "gpt-6.1-sol", "context_window": 600000, "max_context_window": 600000}]
            }))
            .unwrap(),
        )
        .unwrap();
        let (cv_cat, crow_cat) = codex_context_window_row(dir.path(), &codex_cleared_doc, false);
        assert!(cv_cat.is_null());
        assert_eq!(crow_cat["cliPreview"], "600,000");
        assert!(updated_native("codex", "general", before_codex, &json!({"promptCacheTtl": "1h"})).is_err());

        // 2. Claude 上下文大小读写 env.CLAUDE_CODE_AUTO_COMPACT_WINDOW，缓存时间读写 env.CLAUDE_CODE_PROMPT_CACHE_TTL。
        let before_claude = "{\n  \"promptCacheTtl\": \"5m\",\n  \"env\": {\n    \"ANTHROPIC_BASE_URL\": \"https://fixture.invalid\"\n  }\n}\n";
        assert!(updated_native("claude", "general", before_claude, &json!({"contextWindow": 99_999})).is_err());
        let claude_ctx_saved = updated_native("claude", "general", before_claude, &json!({"contextWindow": 500_000})).unwrap();
        let claude_ctx_doc = native_document("claude", &claude_ctx_saved).unwrap();
        assert_eq!(claude_ctx_doc["env"]["CLAUDE_CODE_AUTO_COMPACT_WINDOW"], "500000");
        assert_eq!(claude_ctx_doc["env"]["ANTHROPIC_BASE_URL"], "https://fixture.invalid");
        let (cl_v, cl_row) = claude_context_window_row(&claude_ctx_doc);
        assert_eq!(cl_v, 500_000);
        assert_eq!(cl_row["cliPreview"], "500,000");
        assert_eq!(cl_row["unit"], "TK");
        let (_, cl_low_row) = claude_context_window_row(&json!({"env":{"CLAUDE_CODE_AUTO_COMPACT_WINDOW":"50000"}}));
        assert_eq!(cl_low_row["cliPreview"], "原生配置：50,000（低于 100,000）");
        let (_, suffix_row) = claude_context_window_row(&json!({"env":{"CLAUDE_CODE_AUTO_COMPACT_WINDOW":"600k"}}));
        assert_eq!(suffix_row["value"], 600_000);
        assert_eq!(parse_claude_token_count(&json!("1m")), Some(1_000_000));
        let claude_ctx_cleared = updated_native("claude", "general", &claude_ctx_saved, &json!({"contextWindow": null})).unwrap();
        assert!(native_document("claude", &claude_ctx_cleared).unwrap()["env"]["CLAUDE_CODE_AUTO_COMPACT_WINDOW"].is_null());

        let claude_ttl_saved = updated_native("claude", "general", before_claude, &json!({"promptCacheTtl": "1h"})).unwrap();
        let claude_ttl_doc = native_document("claude", &claude_ttl_saved).unwrap();
        assert_eq!(claude_ttl_doc["env"]["CLAUDE_CODE_PROMPT_CACHE_TTL"], "1h");
        assert!(claude_ttl_doc["promptCacheTtl"].is_null(), "写入 env 时须清理顶层旧 promptCacheTtl");
        let (ttl_v, ttl_row) = claude_prompt_cache_ttl_row(&claude_ttl_doc);
        assert_eq!(ttl_v, "1h");
        assert_eq!(ttl_row["readOnly"], false);
        let claude_ttl_cleared = updated_native("claude", "general", &claude_ttl_saved, &json!({"promptCacheTtl": null})).unwrap();
        let claude_ttl_cleared_doc = native_document("claude", &claude_ttl_cleared).unwrap();
        assert!(claude_ttl_cleared_doc["env"]["CLAUDE_CODE_PROMPT_CACHE_TTL"].is_null());
        assert!(claude_ttl_cleared_doc["promptCacheTtl"].is_null());
        assert!(updated_native("claude", "general", before_claude, &json!({"promptCacheTtl": "2h"})).is_err());
        let (bad_ttl_v, bad_ttl_row) = claude_prompt_cache_ttl_row(&json!({"env":{"CLAUDE_CODE_PROMPT_CACHE_TTL":"2h"}}));
        assert!(bad_ttl_v.is_null());
        assert_eq!(bad_ttl_row["readOnly"], true);
        assert_eq!(bad_ttl_row["valueInvalid"], true);

        // 3. Antigravity 上下文大小与缓存时间均展示只读「不支持」。
        let agy_ctx = unsupported_context_window_row("建议调小下方“自动压缩阈值”。");
        assert_eq!(agy_ctx["id"], "contextWindow");
        assert_eq!(agy_ctx["readOnly"], true);
        assert_eq!(agy_ctx["cliPreview"], "不支持");
        assert_eq!(agy_ctx["readOnlyReason"], "建议调小下方“自动压缩阈值”。");
        let agy_ttl = unsupported_prompt_cache_ttl_row("");
        assert_eq!(agy_ttl["id"], "promptCacheTtl");
        assert_eq!(agy_ttl["readOnly"], true);
        assert_eq!(agy_ttl["cliPreview"], "不支持");
        assert_eq!(agy_ttl["readOnlyReason"], "");
    }
}
