//! Antigravity ACP 协议载荷与 Elves 会话模型的纯映射函数。

use std::collections::BTreeMap;

use base64::Engine;
use serde_json::{Value, json};

use super::super::ApiError;

const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_REPLAY_CHARS: usize = 36_000;

/// 归一化 `session/list` 返回的 cwd：
/// Windows 上的 `agy acp` 可能返回 `/e%3A/code/...` 或 `/C:/Users/...` 形式的 URI 路径，
/// 需转为标准本机路径才能与 Elves 的项目目录正确归组。
pub(crate) fn normalize_acp_cwd(raw: &str, is_wsl: bool) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if is_wsl {
        return trimmed.to_owned();
    }
    let decoded = percent_decode(trimmed);
    let stripped = decoded
        .strip_prefix("file:///")
        .or_else(|| decoded.strip_prefix("file://"))
        .unwrap_or(&decoded);
    let win_candidate = if stripped.starts_with('/')
        && stripped.len() >= 3
        && stripped.as_bytes()[1].is_ascii_alphabetic()
        && stripped.as_bytes()[2] == b':'
    {
        &stripped[1..]
    } else {
        stripped
    };
    if win_candidate.len() >= 2
        && win_candidate.as_bytes()[0].is_ascii_alphabetic()
        && win_candidate.as_bytes()[1] == b':'
    {
        let drive = (win_candidate.as_bytes()[0] as char).to_ascii_uppercase();
        let rest = win_candidate[1..].replace('/', "\\");
        return format!("{drive}{rest}");
    }
    decoded
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_owned())
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 提取 ACP `_meta.antigravity.tier` 中的官方订阅等级信息。
pub(crate) fn extract_tier(value: &Value) -> Option<Value> {
    let tier = &value["_meta"]["antigravity"]["tier"];
    if !tier.is_object() {
        return None;
    }
    let name = tier["name"].as_str().map(str::trim).unwrap_or("");
    let id = tier["id"].as_str().map(str::trim).unwrap_or("");
    if name.is_empty() && id.is_empty() {
        return None;
    }
    Some(json!({
        "id": id,
        "name": if name.is_empty() { id } else { name },
        "upgradeUri": tier["upgradeUri"].as_str().unwrap_or("")
    }))
}

/// 判断 `agy acp` 版本是否支持运行中直接以 `session/prompt` 引导（1.5.0+）。
pub(crate) fn supports_prompt_steering(init: &Value) -> bool {
    let Some(version) = init["agentInfo"]["version"].as_str() else {
        return false;
    };
    let mut parts = version
        .trim()
        .split('.')
        .filter_map(|p| p.parse::<u64>().ok());
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    (major, minor) >= (1, 5)
}

/// 从 ACP `session/new` 或 `session/load` 的 `configOptions` 与 `models` 构造 Elves 模型目录。
pub(crate) fn normalize_models_from_setup(setup: &Value) -> Value {
    let config_options = setup["configOptions"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let model_opt = config_options.iter().find(|opt| {
        opt["id"] == "model" || opt["category"] == "model"
    });
    let thought_opt = config_options.iter().find(|opt| {
        opt["category"] == "thought_level" || opt["id"] == "thought_level"
    });
    let mode_opt = config_options.iter().find(|opt| {
        opt["category"] == "mode" || opt["id"] == "mode"
    });

    let global_efforts: Vec<String> = thought_opt
        .map(extract_select_option_values)
        .unwrap_or_default();
    let available_model_ids: Vec<String> = setup["models"]["availableModels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["modelId"].as_str().map(str::to_owned))
        .collect();

    let mut rows = Vec::new();
    let mut seen = std::collections::HashSet::new();

    if let Some(opt) = model_opt {
        for (value, name, description) in extract_select_options(opt) {
            if value.is_empty() || !seen.insert(value.clone()) {
                continue;
            }
            let efforts = infer_model_efforts(&value, &available_model_ids, &global_efforts);
            rows.push(json!({
                "id": value,
                "model": value,
                "displayName": if name.is_empty() { value.clone() } else { name },
                "description": description,
                "supportedReasoningEfforts": efforts.into_iter().map(|e| json!({"reasoningEffort": e})).collect::<Vec<_>>(),
            }));
        }
    } else if let Some(available) = setup["models"]["availableModels"].as_array() {
        for entry in available {
            let Some(value) = entry["modelId"].as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            if !seen.insert(value.to_owned()) {
                continue;
            }
            let name = entry["name"].as_str().unwrap_or(value);
            rows.push(json!({
                "id": value,
                "model": value,
                "displayName": name,
                "description": entry["description"].clone(),
                "supportedReasoningEfforts": [],
            }));
        }
    }

    let default_model = model_opt
        .and_then(|opt| opt["currentValue"].as_str())
        .or_else(|| setup["models"]["currentModelId"].as_str())
        .or_else(|| rows.first().and_then(|r| r["model"].as_str()))
        .unwrap_or("")
        .to_owned();
    let default_effort = thought_opt
        .and_then(|opt| opt["currentValue"].as_str())
        .unwrap_or("")
        .to_owned();

    let mut permission_modes = Vec::new();
    if let Some(modes) = setup["modes"]["availableModes"].as_array() {
        for mode in modes {
            if let Some(id) = mode["id"]
                .as_str()
                .filter(|s| matches!(*s, "default" | "accept-edits" | "auto"))
            {
                permission_modes.push(id.to_owned());
            }
        }
    } else if let Some(opt) = mode_opt {
        permission_modes = extract_select_option_values(opt)
            .into_iter()
            .filter(|s| matches!(s.as_str(), "default" | "accept-edits" | "auto"))
            .collect();
    }
    if permission_modes.is_empty() {
        permission_modes = vec!["default".into(), "accept-edits".into(), "auto".into()];
    }
    // 用户要求去除计划模式、增加无限制模式（经官方 `agy acp --dangerously-skip-permissions` 实现）。
    permission_modes.push("bypassPermissions".into());

    json!({
        "ok": true,
        "backend": "antigravity",
        "data": rows,
        "catalogComplete": true,
        "defaultModel": default_model,
        "defaultReasoningEffort": default_effort,
        "permissionModes": permission_modes,
        "nativeCapabilities": {
            "goal": true,
            "compact": true
        }
    })
}

fn extract_select_options(opt: &Value) -> Vec<(String, String, Value)> {
    let mut out = Vec::new();
    let Some(options) = opt["options"].as_array() else {
        return out;
    };
    for entry in options {
        if let Some(nested) = entry["options"].as_array() {
            for item in nested {
                if let Some(val) = item["value"].as_str() {
                    out.push((
                        val.trim().to_owned(),
                        item["name"].as_str().unwrap_or(val).trim().to_owned(),
                        item["description"].clone(),
                    ));
                }
            }
        } else if let Some(val) = entry["value"].as_str() {
            out.push((
                val.trim().to_owned(),
                entry["name"].as_str().unwrap_or(val).trim().to_owned(),
                entry["description"].clone(),
            ));
        }
    }
    out
}

fn extract_select_option_values(opt: &Value) -> Vec<String> {
    extract_select_options(opt)
        .into_iter()
        .map(|(v, _, _)| v)
        .filter(|v| !v.is_empty())
        .collect()
}

/// 根据 `models.availableModels` 中该模型家族实际存在的思考深度变体推导每个模型的支持等级；
/// 若 ACP 未提供平铺变体，则沿用当前会话返回的 `thought_level` 选项。
fn infer_model_efforts(
    model_id: &str,
    available_model_ids: &[String],
    global_efforts: &[String],
) -> Vec<String> {
    if available_model_ids.is_empty() {
        return global_efforts.to_vec();
    }
    let known_order = ["low", "medium", "high", "xhigh", "max"];
    let prefix = format!("{model_id}-");
    let mut matched = Vec::new();
    for level in known_order {
        let candidate = format!("{prefix}{level}");
        if available_model_ids.iter().any(|id| id == &candidate) {
            matched.push(level.to_owned());
        }
    }
    if !matched.is_empty() {
        return matched;
    }
    // 若模型 ID 本身已在 availableModels 中作为独立条目（无思考变体，如 orchestrator / gpt-oss），不虚构等级。
    if available_model_ids.iter().any(|id| id == model_id) {
        return Vec::new();
    }
    global_efforts.to_vec()
}

/// 将 Elves 发送载荷转换为 ACP `session/prompt` 的 `ContentBlock[]`。
pub(crate) fn build_prompt_blocks(
    payload: &Value,
    supports_image: bool,
) -> anyhow::Result<Vec<Value>> {
    let mut blocks = Vec::new();
    if let Some(prompt) = payload["prompt"].as_str().filter(|s| !s.trim().is_empty()) {
        blocks.push(json!({ "type": "text", "text": prompt }));
    }
    for image in payload["images"].as_array().into_iter().flatten() {
        if !supports_image {
            return Err(ApiError::new(400, "当前 Antigravity ACP 实例未宣告图片输入能力").into());
        }
        let url = image["dataUrl"]
            .as_str()
            .ok_or_else(|| ApiError::new(400, "Antigravity 图片格式无效"))?;
        let (mime, data) = url
            .strip_prefix("data:")
            .and_then(|s| s.split_once(";base64,"))
            .filter(|(mime, _)| {
                matches!(*mime, "image/png" | "image/jpeg" | "image/webp" | "image/bmp")
            })
            .ok_or_else(|| {
                ApiError::new(400, "Antigravity ACP 图片仅支持 PNG、JPEG、WebP 和 BMP")
            })?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| ApiError::new(400, "图片 Base64 数据无效"))?;
        if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
            return Err(ApiError::new(400, "Antigravity 单张图片大小需小于 10 MiB").into());
        }
        blocks.push(json!({
            "type": "image",
            "data": data,
            "mimeType": mime,
        }));
    }
    if blocks.is_empty() {
        return Err(ApiError::new(400, "请输入消息或添加图片").into());
    }
    Ok(blocks)
}

/// `apply_session_update` 产生的副作用，用于推送 Elves SSE 事件与更新会话元数据。
#[derive(Default)]
pub(crate) struct UpdateEffect {
    pub changed_items: Vec<Value>,
    pub plan_changed: Option<Vec<Value>>,
    pub token_usage: Option<Value>,
    pub title_updated: Option<String>,
    pub config_options_updated: Option<Value>,
    pub mode_updated: Option<String>,
    pub available_commands: Option<Vec<Value>>,
    pub first_token_at_ms: Option<u64>,
}

/// 将单条 ACP `session/update` 通知投影到当前 Elves `turn` 中。
pub(crate) fn apply_session_update(
    turn: &mut Value,
    update: &Value,
    now_ms: u64,
) -> UpdateEffect {
    let mut effect = UpdateEffect::default();
    let kind = update["sessionUpdate"].as_str().unwrap_or("");
    match kind {
        "agent_message_chunk" | "agent_message" => {
            let text = extract_content_text(&update["content"]);
            if text.is_empty() {
                return effect;
            }
            if turn["firstTokenAtMs"].is_null() {
                turn["firstTokenAtMs"] = json!(now_ms);
                effect.first_token_at_ms = Some(now_ms);
            }
            let items = ensure_items_array(turn);
            let append_existing = items.last().is_some_and(|item| {
                item["type"] == "agentMessage" && item["status"] == "inProgress"
            });
            if append_existing {
                let last = items.last_mut().unwrap();
                let mut current = last["text"].as_str().unwrap_or("").to_owned();
                current.push_str(&text);
                last["text"] = json!(current);
                effect.changed_items.push(last.clone());
            } else {
                close_in_progress_text_items(items);
                let item_id = format!("agy-msg-{}", items.len() + 1);
                let item = json!({
                    "id": item_id,
                    "type": "agentMessage",
                    "text": text,
                    "status": "inProgress",
                    "startedAtMs": now_ms,
                });
                items.push(item.clone());
                effect.changed_items.push(item);
            }
        }
        "agent_thought_chunk" | "agent_thought" => {
            let text = extract_content_text(&update["content"]);
            if text.is_empty() {
                return effect;
            }
            if turn["firstTokenAtMs"].is_null() {
                turn["firstTokenAtMs"] = json!(now_ms);
                effect.first_token_at_ms = Some(now_ms);
            }
            let items = ensure_items_array(turn);
            let append_existing = items.last().is_some_and(|item| {
                item["type"] == "reasoning" && item["status"] == "inProgress"
            });
            if append_existing {
                let last = items.last_mut().unwrap();
                let mut current = last["content"]
                    .as_array()
                    .and_then(|arr| arr.first())
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                current.push_str(&text);
                last["content"] = json!([current]);
                effect.changed_items.push(last.clone());
            } else {
                close_in_progress_text_items(items);
                let item_id = format!("agy-thought-{}", items.len() + 1);
                let item = json!({
                    "id": item_id,
                    "type": "reasoning",
                    "summary": [],
                    "content": [text],
                    "status": "inProgress",
                    "startedAtMs": now_ms,
                });
                items.push(item.clone());
                effect.changed_items.push(item);
            }
        }
        "tool_call" | "tool_call_update" => {
            let Some(tool_call_id) = update["toolCallId"].as_str().filter(|s| !s.is_empty()) else {
                return effect;
            };
            let items = ensure_items_array(turn);
            close_in_progress_text_items(items);
            let item_id = format!("agy-tool-{tool_call_id}");
            let existing_idx = items.iter().position(|item| item["id"] == item_id);
            let existing = existing_idx.and_then(|idx| items.get(idx)).cloned();
            let projected = project_tool_call(&item_id, existing.as_ref(), update, now_ms);
            if let Some(idx) = existing_idx {
                items[idx] = projected.clone();
            } else {
                items.push(projected.clone());
            }
            effect.changed_items.push(projected);
        }
        "plan" | "plan_update" => {
            if let Some(entries) = update["entries"].as_array() {
                let plan: Vec<Value> = entries
                    .iter()
                    .filter_map(|entry| {
                        let step = entry["content"]
                            .as_str()
                            .or_else(|| entry["step"].as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())?;
                        let status = match entry["status"].as_str().unwrap_or("pending") {
                            "completed" => "completed",
                            "in_progress" | "inProgress" => "inProgress",
                            "cancelled" | "canceled" => "cancelled",
                            _ => "pending",
                        };
                        Some(json!({ "step": step, "status": status }))
                    })
                    .collect();
                turn["plan"] = json!(plan);
                effect.plan_changed = Some(plan);
            }
        }
        "usage_update" => {
            if let Some(usage) = normalize_usage_update(update) {
                effect.token_usage = Some(usage);
            }
        }
        "session_info_update" => {
            if let Some(title) = update["title"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                effect.title_updated = Some(title.to_owned());
            }
        }
        "config_option_update" => {
            if update["configOptions"].is_array() {
                effect.config_options_updated = Some(update["configOptions"].clone());
            }
        }
        "current_mode_update" => {
            if let Some(mode_id) = update["currentModeId"]
                .as_str()
                .or_else(|| update["modeId"].as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                effect.mode_updated = Some(mode_id.to_owned());
            }
        }
        "available_commands_update" => {
            if let Some(commands) = update["availableCommands"].as_array() {
                let normalized = normalize_available_commands(commands);
                effect.available_commands = Some(normalized);
            }
        }
        _ => {}
    }
    effect
}

fn ensure_items_array(turn: &mut Value) -> &mut Vec<Value> {
    if !turn["items"].is_array() {
        turn["items"] = json!([]);
    }
    turn["items"].as_array_mut().unwrap()
}

pub(crate) fn close_in_progress_text_items(items: &mut [Value]) {
    for item in items.iter_mut() {
        if matches!(item["type"].as_str(), Some("agentMessage" | "reasoning"))
            && item["status"] == "inProgress"
        {
            item["status"] = json!("completed");
        }
    }
}

pub(crate) fn finalize_turn_items(turn: &mut Value, failed: bool, interrupted: bool, now_ms: u64) {
    let terminal = if interrupted {
        "interrupted"
    } else if failed {
        "failed"
    } else {
        "completed"
    };
    if let Some(items) = turn["items"].as_array_mut() {
        for item in items.iter_mut() {
            if item["status"] == "inProgress" || item["status"] == "pending" {
                item["status"] = json!(terminal);
                if item["completedAtMs"].is_null() {
                    item["completedAtMs"] = json!(now_ms);
                }
            }
        }
    }
}

fn extract_content_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    if let Some(text) = content["text"].as_str() {
        return text.to_owned();
    }
    if let Some(arr) = content.as_array() {
        let mut combined = String::new();
        for entry in arr {
            if let Some(text) = entry["text"].as_str() {
                combined.push_str(text);
            } else if let Some(text) = entry["content"]["text"].as_str() {
                combined.push_str(text);
            }
        }
        return combined;
    }
    String::new()
}

fn normalize_tool_status(raw: Option<&str>, fallback: &str) -> &'static str {
    match raw {
        Some("pending") => "inProgress",
        Some("in_progress" | "inProgress") => "inProgress",
        Some("completed") => "completed",
        Some("failed") => "failed",
        Some("cancelled" | "canceled") => "interrupted",
        _ => match fallback {
            "completed" => "completed",
            "failed" => "failed",
            "interrupted" => "interrupted",
            _ => "inProgress",
        },
    }
}

fn project_tool_call(
    item_id: &str,
    existing: Option<&Value>,
    update: &Value,
    now_ms: u64,
) -> Value {
    let kind = update["kind"]
        .as_str()
        .or_else(|| existing.and_then(|e| e["_acpKind"].as_str()))
        .unwrap_or("other");
    let title = update["title"]
        .as_str()
        .or_else(|| existing.and_then(|e| e["_acpTitle"].as_str()))
        .unwrap_or("工具调用");
    let status = normalize_tool_status(
        update["status"].as_str(),
        existing
            .and_then(|e| e["status"].as_str())
            .unwrap_or("inProgress"),
    );
    let raw_input = if !update["rawInput"].is_null() {
        update["rawInput"].clone()
    } else {
        existing
            .and_then(|e| e.get("_acpRawInput").cloned())
            .unwrap_or(Value::Null)
    };
    let raw_output = if !update["rawOutput"].is_null() {
        update["rawOutput"].clone()
    } else {
        existing
            .and_then(|e| e.get("_acpRawOutput").cloned())
            .unwrap_or(Value::Null)
    };
    let started_at = existing
        .and_then(|e| e["startedAtMs"].as_u64())
        .unwrap_or(now_ms);
    let completed_at = if matches!(status, "completed" | "failed" | "interrupted") {
        Some(
            existing
                .and_then(|e| e["completedAtMs"].as_u64())
                .unwrap_or(now_ms),
        )
    } else {
        None
    };

    let content_text = extract_tool_content_text(&update["content"]);
    let diff_changes = extract_tool_diffs(&update["content"], &update["locations"]);

    if kind == "execute" {
        let command = raw_input["CommandLine"]
            .as_str()
            .or_else(|| raw_input["command_line"].as_str())
            .or_else(|| raw_input["commandLine"].as_str())
            .or_else(|| raw_input["command"].as_str())
            .or_else(|| existing.and_then(|e| e["command"].as_str()))
            .unwrap_or(title);
        let cwd = raw_input["Cwd"]
            .as_str()
            .or_else(|| raw_input["WorkingDirectory"].as_str())
            .or_else(|| raw_input["cwd"].as_str())
            .or_else(|| existing.and_then(|e| e["cwd"].as_str()))
            .unwrap_or("");
        let output = raw_output["combinedOutput"]
            .as_str()
            .or_else(|| raw_output["combined_output"].as_str())
            .or_else(|| raw_output["output"].as_str())
            .map(str::to_owned)
            .filter(|s| !s.is_empty())
            .or_else(|| (!content_text.is_empty()).then_some(content_text.clone()))
            .or_else(|| {
                existing
                    .and_then(|e| e["aggregatedOutput"].as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_default();
        let exit_code = raw_output["exitCode"]
            .as_i64()
            .or_else(|| raw_output["exit_code"].as_i64())
            .or_else(|| existing.and_then(|e| e["exitCode"].as_i64()));
        let mut item = json!({
            "id": item_id,
            "type": "commandExecution",
            "command": command,
            "cwd": cwd,
            "aggregatedOutput": output,
            "status": status,
            "startedAtMs": started_at,
            "_acpKind": kind,
            "_acpTitle": title,
            "_acpRawInput": raw_input,
            "_acpRawOutput": raw_output,
        });
        if let Some(code) = exit_code {
            item["exitCode"] = json!(code);
        }
        if let Some(at) = completed_at {
            item["completedAtMs"] = json!(at);
        }
        return item;
    }

    if kind == "edit" || !diff_changes.is_empty() {
        let changes = if !diff_changes.is_empty() {
            diff_changes
        } else {
            existing
                .and_then(|e| e["changes"].as_array().cloned())
                .unwrap_or_default()
        };
        let mut item = json!({
            "id": item_id,
            "type": "fileChange",
            "changes": changes,
            "status": status,
            "startedAtMs": started_at,
            "_acpKind": kind,
            "_acpTitle": title,
            "_acpRawInput": raw_input,
            "_acpRawOutput": raw_output,
        });
        if let Some(at) = completed_at {
            item["completedAtMs"] = json!(at);
        }
        return item;
    }

    let mut arguments = if let Some(obj) = raw_input.as_object() {
        Value::Object(obj.clone())
    } else {
        json!({})
    };
    if arguments["title"].is_null() {
        arguments["title"] = json!(title);
    }
    let result_text = if !content_text.is_empty() {
        content_text
    } else if let Some(text) = raw_output.as_str() {
        text.to_owned()
    } else if !raw_output.is_null() {
        serde_json::to_string_pretty(&raw_output).unwrap_or_default()
    } else {
        existing
            .and_then(|e| e["result"]["text"].as_str())
            .unwrap_or("")
            .to_owned()
    };
    let mut item = json!({
        "id": item_id,
        "type": "dynamicToolCall",
        "source": "antigravity",
        "tool": title,
        "arguments": arguments,
        "status": status,
        "success": status != "failed",
        "startedAtMs": started_at,
        "_acpKind": kind,
        "_acpTitle": title,
        "_acpRawInput": raw_input,
        "_acpRawOutput": raw_output,
    });
    if !result_text.is_empty() {
        item["result"] = json!({ "text": result_text });
    }
    if let Some(at) = completed_at {
        item["completedAtMs"] = json!(at);
    }
    item
}

fn extract_tool_content_text(content: &Value) -> String {
    let mut parts = Vec::new();
    for entry in content.as_array().into_iter().flatten() {
        if entry["type"] == "content" {
            let text = extract_content_text(&entry["content"]);
            if !text.is_empty() {
                parts.push(text);
            }
        } else if let Some(text) = entry["text"].as_str().filter(|s| !s.is_empty()) {
            parts.push(text.to_owned());
        }
    }
    parts.join("\n")
}

fn extract_tool_diffs(content: &Value, locations: &Value) -> Vec<Value> {
    let mut changes = Vec::new();
    for entry in content.as_array().into_iter().flatten() {
        if entry["type"] == "diff"
            && let Some(path) = entry["path"].as_str().filter(|s| !s.is_empty())
        {
            let old_text = entry["oldText"].as_str().unwrap_or("");
            let new_text = entry["newText"].as_str().unwrap_or("");
            let diff = format!(
                "--- a/{path}\n+++ b/{path}\n{}",
                simple_diff_preview(old_text, new_text)
            );
            changes.push(json!({
                "path": path,
                "kind": if old_text.is_empty() { "add" } else { "update" },
                "diff": diff,
            }));
        }
    }
    if changes.is_empty() {
        for loc in locations.as_array().into_iter().flatten() {
            if let Some(path) = loc["path"].as_str().filter(|s| !s.is_empty()) {
                changes.push(json!({ "path": path, "kind": "update", "diff": "" }));
            }
        }
    }
    changes
}

fn simple_diff_preview(old_text: &str, new_text: &str) -> String {
    let mut out = String::new();
    for line in old_text.lines().take(60) {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in new_text.lines().take(60) {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// 归一化 ACP `available_commands_update` 命令列表为 Elves 斜杠技能结构（自动过滤与内置菜单重复的 `goal`/`compact`）。
pub(crate) fn normalize_available_commands(commands: &[Value]) -> Vec<Value> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for cmd in commands {
        let Some(name) = cmd["name"].as_str().map(str::trim).filter(|s| {
            super::local_store::valid_slash_command_name(s)
        }) else {
            continue;
        };
        let key = name.to_ascii_lowercase();
        if matches!(key.as_str(), "goal" | "compact") || !seen.insert(key) {
            continue;
        }
        out.push(json!({
            "name": name,
            "command": name,
            "path": format!("antigravity-skill:{name}"),
            "description": cmd["description"].as_str().unwrap_or("").trim(),
            "argumentHint": cmd["input"]["hint"].as_str().unwrap_or("").trim(),
            "backend": "antigravity",
            "source": "antigravity",
        }));
    }
    out
}

/// 将 ACP `usage_update` 通知或 `PromptResponse.usage` 转换为 Elves `tokenUsage` 结构。
pub(crate) fn normalize_usage_update(value: &Value) -> Option<Value> {
    let size = value["size"].as_u64();
    let used = value["used"].as_u64();
    let input = value["inputTokens"].as_u64();
    let output = value["outputTokens"].as_u64();
    let cached = value["cachedReadTokens"]
        .as_u64()
        .or_else(|| value["cachedInputTokens"].as_u64());
    let thought = value["thoughtTokens"]
        .as_u64()
        .or_else(|| value["reasoningOutputTokens"].as_u64());
    let total = value["totalTokens"]
        .as_u64()
        .or(used)
        .or_else(|| match (input, cached, output) {
            (Some(i), Some(c), Some(o)) => i.checked_add(c)?.checked_add(o),
            _ => None,
        });
    if total.is_none() && size.is_none() && input.is_none() && output.is_none() {
        return None;
    }
    let mut usage = json!({
        "last": {
            "totalTokens": total,
            "inputTokens": input,
            "outputTokens": output,
            "cachedInputTokens": cached,
            "reasoningOutputTokens": thought,
        },
        "total": {
            "totalTokens": total,
            "inputTokens": input,
            "outputTokens": output,
            "cachedInputTokens": cached,
            "reasoningOutputTokens": thought,
        }
    });
    if let Some(window) = size.filter(|&w| w > 0) {
        usage["modelContextWindow"] = json!(window);
        usage["modelContextWindowSource"] = json!({"kind": "native", "key": "acp.usage_update.size"});
    }
    Some(usage)
}

/// 合并已有 `tokenUsage` 与新到达的 `usage_update` / `PromptResponse.usage` / 本地转录用量，
/// 保证 `PromptResponse.usage`（不含 `size`）不会冲掉 `usage_update` 已提供的原生 `modelContextWindow`。
pub(crate) fn merge_token_usage(existing: &Value, incoming: &Value, fallback_model: &str) -> Value {
    let mut merged = incoming.clone();
    if !merged["modelContextWindow"].is_u64() {
        if existing["modelContextWindow"].as_u64().is_some_and(|w| w > 0) {
            merged["modelContextWindow"] = existing["modelContextWindow"].clone();
            if !existing["modelContextWindowSource"].is_null() {
                merged["modelContextWindowSource"] = existing["modelContextWindowSource"].clone();
            }
        } else if let Some(window) = super::local_store::known_antigravity_context_window(fallback_model) {
            merged["modelContextWindow"] = json!(window);
            merged["modelContextWindowSource"] = json!({"kind": "model-default", "key": fallback_model});
        }
    }
    if merged["last"]["totalTokens"].as_u64().is_none()
        && existing["last"]["totalTokens"].as_u64().unwrap_or(0) > 0
    {
        merged["last"] = existing["last"].clone();
    }
    merged
}

/// 将 ACP `session/request_permission` 映射为 Elves 审批或用户提问请求。
#[derive(Clone)]
pub(crate) struct MappedPermission {
    pub elves_request: Value,
    pub is_user_input: bool,
    pub tool_call_id: String,
    pub options: Vec<(String, String, String)>, // (optionId, kind, name)
}

pub(crate) fn map_permission_request(
    thread_id: &str,
    request_id: &str,
    params: &Value,
) -> MappedPermission {
    let tool_call = &params["toolCall"];
    let tool_call_id = tool_call["toolCallId"]
        .as_str()
        .unwrap_or(request_id)
        .to_owned();
    let title = tool_call["title"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Antigravity 请求确认");
    let options: Vec<(String, String, String)> = params["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|opt| {
            let id = opt["optionId"].as_str()?.trim();
            if id.is_empty() {
                return None;
            }
            let kind = opt["kind"].as_str().unwrap_or("").trim().to_owned();
            let name = opt["name"].as_str().unwrap_or(id).trim().to_owned();
            Some((id.to_owned(), kind, name))
        })
        .collect();

    // 参考 T3 AntigravityProtocol.ts：toolCallId 以 `interaction_` 开头表示原生交互式提问。
    if tool_call_id.starts_with("interaction_") && !options.is_empty() {
        let question_options: Vec<Value> = options
            .iter()
            .map(|(id, _, name)| {
                json!({
                    "value": id,
                    "label": name,
                    "description": name,
                })
            })
            .collect();
        let elves_request = json!({
            "id": request_id,
            "method": "item/tool/requestUserInput",
            "params": {
                "threadId": thread_id,
                "questions": [{
                    "id": tool_call_id,
                    "header": "Antigravity 提问",
                    "question": title,
                    "options": question_options,
                    "optionsOnly": true,
                }]
            }
        });
        return MappedPermission {
            elves_request,
            is_user_input: true,
            tool_call_id,
            options,
        };
    }

    let raw_input = &tool_call["rawInput"];
    let command = raw_input["CommandLine"]
        .as_str()
        .or_else(|| raw_input["commandLine"].as_str())
        .or_else(|| raw_input["command"].as_str())
        .unwrap_or(title);
    let mut decisions = Vec::new();
    if options.iter().any(|(_, kind, _)| kind == "allow_once") {
        decisions.push("accept");
    }
    if options.iter().any(|(_, kind, _)| kind == "allow_always") {
        decisions.push("acceptForSession");
    }
    if options.iter().any(|(_, kind, _)| kind == "reject_once" || kind == "reject_always") {
        decisions.push("decline");
    }
    decisions.push("cancel");
    if decisions.len() == 1 && !options.is_empty() {
        decisions.insert(0, "accept");
    }

    let elves_request = json!({
        "id": request_id,
        "method": "item/commandExecution/requestApproval",
        "params": {
            "threadId": thread_id,
            "command": command,
            "reason": title,
            "availableDecisions": decisions,
        }
    });
    MappedPermission {
        elves_request,
        is_user_input: false,
        tool_call_id,
        options,
    }
}

/// 将 Elves 用户决策映射为 ACP `RequestPermissionResponse`。
pub(crate) fn build_permission_response(
    mapped: &MappedPermission,
    action: &str,
    payload: &Value,
) -> Value {
    if mapped.is_user_input && action == "answer" {
        // 共享前端与 Claude 一致以 response.answers 提交；ACP 选项提问不支持任意自由文本。
        let answers = payload.get("response").unwrap_or(payload).get("answers").unwrap_or(&Value::Null);
        let selected_val = answers
            .get(&mapped.tool_call_id)
            .and_then(|entry| {
                entry
                    .as_str()
                    .or_else(|| entry["answers"].as_array()?.first()?.as_str())
                    .or_else(|| entry.as_array()?.first()?.as_str())
            })
            .map(str::trim);
        if let Some(val) = selected_val {
            if let Some((opt_id, _, _)) = mapped
                .options
                .iter()
                .find(|(id, _, name)| id == val || name == val)
            {
                return json!({ "outcome": { "outcome": "selected", "optionId": opt_id } });
            }
        }
        return json!({ "outcome": { "outcome": "cancelled" } });
    }

    let decision = payload["decision"].as_str().unwrap_or("cancel");
    let target_kind = match decision {
        "accept" => Some("allow_once"),
        "acceptForSession" => Some("allow_always"),
        "decline" => Some("reject_once"),
        _ => None,
    };
    if let Some(kind) = target_kind {
        if let Some((opt_id, _, _)) = mapped.options.iter().find(|(_, k, _)| k == kind) {
            return json!({ "outcome": { "outcome": "selected", "optionId": opt_id } });
        }
        if decision == "accept"
            && let Some((opt_id, _, _)) = mapped
                .options
                .iter()
                .find(|(_, k, _)| k.starts_with("allow"))
                .or_else(|| mapped.options.first())
        {
            return json!({ "outcome": { "outcome": "selected", "optionId": opt_id } });
        }
        if decision == "decline"
            && let Some((opt_id, _, _)) = mapped
                .options
                .iter()
                .find(|(_, k, _)| k.starts_with("reject"))
        {
            return json!({ "outcome": { "outcome": "selected", "optionId": opt_id } });
        }
    }
    json!({ "outcome": { "outcome": "cancelled" } })
}

/// 历史消息重新编辑时，因 ACP 暂不提供原生历史截断 / `session/fork`，
/// 将保留的前序轮次整理为紧凑对话上下文，通过 ACP 官方宣告的 `_meta.antigravity.systemPrompt.append` 传入新会话。
pub(crate) fn build_edit_replay_system_prompt(kept_turns: &[Value]) -> Option<String> {
    if kept_turns.is_empty() {
        return None;
    }
    let mut lines = Vec::new();
    lines.push(
        "The user edited an earlier message in this conversation. Below is the preserved prior conversation history before the edited message; continue seamlessly from it:"
            .to_owned(),
    );
    let mut total_chars = lines[0].len();
    for (idx, turn) in kept_turns.iter().enumerate() {
        let Some(items) = turn["items"].as_array() else {
            continue;
        };
        let mut user_parts = Vec::new();
        let mut assistant_parts = Vec::new();
        for item in items {
            match item["type"].as_str() {
                Some("userMessage") => {
                    for part in item["content"].as_array().into_iter().flatten() {
                        if let Some(text) = part["text"].as_str().filter(|s| !s.trim().is_empty()) {
                            user_parts.push(text.trim().to_owned());
                        }
                    }
                }
                Some("agentMessage") => {
                    if let Some(text) = item["text"].as_str().filter(|s| !s.trim().is_empty()) {
                        assistant_parts.push(text.trim().to_owned());
                    }
                }
                _ => {}
            }
        }
        if user_parts.is_empty() && assistant_parts.is_empty() {
            continue;
        }
        let block = format!(
            "\n[Turn {}]\nUser: {}\nAssistant: {}",
            idx + 1,
            clip_text(&user_parts.join("\n"), 4000),
            clip_text(&assistant_parts.join("\n"), 6000)
        );
        if total_chars + block.len() > MAX_REPLAY_CHARS {
            break;
        }
        total_chars += block.len();
        lines.push(block);
    }
    (lines.len() > 1).then(|| lines.join("\n"))
}

fn clip_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let clipped: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{clipped}…")
    } else {
        clipped
    }
}

/// 将 `session/load` 回放通知流组装为历史轮次列表（若 ACP 返回了回放帧）。
pub(crate) fn collect_replay_turns(updates: &[Value], now_ms: u64) -> Vec<Value> {
    let mut turns: Vec<Value> = Vec::new();
    for update in updates {
        let kind = update["sessionUpdate"].as_str().unwrap_or("");
        if matches!(kind, "user_message_chunk" | "user_message") {
            let text = extract_content_text(&update["content"]);
            if text.is_empty() {
                continue;
            }
            let append_user = turns.last().is_some_and(|t| {
                t["items"]
                    .as_array()
                    .is_some_and(|items| items.len() == 1 && items[0]["type"] == "userMessage")
            });
            if append_user {
                let last = turns.last_mut().unwrap();
                let mut current = last["items"][0]["content"][0]["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                current.push_str(&text);
                last["items"][0]["content"][0]["text"] = json!(current);
            } else {
                let turn_id = format!("agy-replay-{}", turns.len() + 1);
                let msg_id = format!("{turn_id}-user");
                turns.push(json!({
                    "id": turn_id,
                    "status": "completed",
                    "items": [{
                        "id": msg_id,
                        "type": "userMessage",
                        "content": [{ "type": "text", "text": text }],
                        "sentAtMs": now_ms,
                    }]
                }));
            }
        } else if !turns.is_empty() {
            let last = turns.last_mut().unwrap();
            let _ = apply_session_update(last, update, now_ms);
        }
    }
    for turn in &mut turns {
        finalize_turn_items(turn, false, false, now_ms);
    }
    turns
}

fn estimate_turn_steps(turn: &Value) -> Option<u64> {
    // 与 Codex/Claude 一致，“步”只取真实模型调用次数，不从工具或正文片段推算。
    turn["_acpUsage"]["steps"].as_u64()
}

/// 计算单会话累计统计指标，完整对齐 Elves 输入区左下 `#session-metrics` 字段契约。
pub(crate) fn compute_session_metrics(
    turns: &[Value],
    token_usage: &Value,
    fallback_model: &str,
    now_ms: u64,
) -> Value {
    let mut total_input = Some(0u64);
    let mut total_output = Some(0u64);
    let mut total_cached = Some(0u64);
    let mut total_thought = Some(0u64);
    let mut cumulative_total = Some(0u64);
    let mut has_turn_usage = false;
    for turn in turns {
        if let Some(u) = turn.get("_acpUsage").filter(|v| v.is_object()) {
            let step_in = super::transcript_metrics::input(u);
            let cached = u["cachedReadTokens"]
                .as_u64()
                .or_else(|| u["cachedInputTokens"].as_u64());
            has_turn_usage = true;
            let add = |sum: Option<u64>, value: Option<u64>| sum.zip(value).and_then(|(s,v)|s.checked_add(v));
            cumulative_total = add(cumulative_total, super::transcript_metrics::total(u));
            total_input = add(total_input, step_in);
            total_output = add(total_output, u["outputTokens"].as_u64());
            total_cached = add(total_cached, cached);
            total_thought = add(total_thought, u["thoughtTokens"].as_u64());
        }
    }
    // 新轮次只返回已确认的数据；共享前端在尚无本轮实测值时保留上一轮显示及其原 turnId。
    let Some(turn) = turns.last() else {
        return Value::Null;
    };
    let Some(turn_id) = turn["id"].as_str().filter(|s| !s.is_empty()) else {
        return Value::Null;
    };

    let mut metrics = json!({
        "source": "antigravity-acp",
        "turnId": turn_id,
    });
    if let Some(total) = cumulative_total.filter(|_| has_turn_usage) {
        metrics["totalTokens"] = json!(total);
        // 账号页使用会话累计，输入区使用本轮；分开保存，避免修正本轮缓存时改变账号页统计口径。
        if has_turn_usage {
            metrics["sessionTokens"] = json!({"inputTokens":total_input,"outputTokens":total_output,
                "cachedInputTokens":total_cached,"reasoningTokens":total_thought});
        }
    }
    if has_turn_usage && cumulative_total.is_none() {
        metrics["totalIncomplete"] = json!(true);
    }
    if let Some(steps) = estimate_turn_steps(turn) {
        metrics["steps"] = json!(steps);
    }

    let last_usage = turn.get("_acpUsage").filter(|v| v.is_object());
    let mut turn_out_for_speed = 0u64;
    if let Some(u) = last_usage {
        let turn_cached = u["cachedReadTokens"]
            .as_u64()
            .or_else(|| u["cachedInputTokens"].as_u64());
        let turn_in = super::transcript_metrics::input(u);
        let turn_out = u["outputTokens"].as_u64();
        turn_out_for_speed = turn_out.unwrap_or(0);
        metrics["turnTokens"] = json!(super::transcript_metrics::total(u));
        metrics["inputTokens"] = json!(turn_in);
        metrics["outputTokens"] = json!(turn_out);
        metrics["cachedInputTokens"] = json!(turn_cached);
        if let Some((input,cached)) = turn_in.zip(turn_cached).filter(|(i,c)| *i > 0 && c <= i) {
            metrics["cacheHitRate"] = json!(cached as f64 / input as f64);
        }
    }

    let start_ms = turn["turnStartedAtMs"]
        .as_u64()
        .or_else(|| turn["createdAt"].as_u64())
        .filter(|&t| t > 0);
    let historical = turn["_nativeRequestTimestamps"] == true
        || turn_id.starts_with("agy-local-turn-");
    let first_ms = turn["firstTokenAtMs"].as_u64().filter(|&t| t > 0 && !historical);
    let observed = last_usage.and_then(|u| u["lastResponseObservedAtMs"].as_u64()
        .or_else(|| (u["source"] != "transcript").then(|| u["lastObservedAtMs"].as_u64()).flatten()));
    let end_ms = turn["turnCompletedAtMs"]
        .as_u64()
        .or_else(|| turn["completedAt"].as_u64())
        .filter(|&t| t > 0 && !historical)
        .or(observed);

    if let (Some(start), Some(first)) = (start_ms, first_ms)
        && first >= start
    {
        metrics["firstTokenMs"] = json!(first - start);
    }
    if let (Some(start), Some(end)) = (start_ms, end_ms)
        && end >= start
    {
        let duration = end - start;
        if duration > 0 {
            metrics["durationMs"] = json!(duration);
        }
        if turn_out_for_speed > 0 {
            // 与共享统计同口径：输出量除以轮次开始至用量实际观测/结束的耗时，轮询不推进分母。
            let gen_ms = duration;
            if gen_ms > 0 {
                let speed = (turn_out_for_speed as f64) * 1000.0 / (gen_ms as f64);
                if speed.is_finite() && speed > 0.0 {
                    metrics["tokensPerSecond"] = json!(speed);
                }
            }
        }
    }

    // 推断缓存过期倒计时（仅基于最后一次真实模型响应时间计算，轮询不会续期）。
    if let Some(u) = last_usage {
        let model = u["model"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(fallback_model);
        // 与速度使用同一次真实用量观测；原生请求开始时间不能充当缓存完成时间。
        let observed_at = observed.or(end_ms).filter(|&t| t > 0 && !historical);
        if let Some(at) = observed_at
            && !model.is_empty()
        {
            let cache_evidence = json!({
                "inputTokens": u["inputTokens"].as_u64().unwrap_or(0),
                "cachedInputTokens": u["cachedReadTokens"]
                    .as_u64()
                    .or_else(|| u["cachedInputTokens"].as_u64())
                    .unwrap_or(0),
            });
            if let Some(expiry) = elves_core::cache_expiry::infer_cache_expiry(
                "antigravity",
                model,
                &json!({}),
                &cache_evidence,
                at,
                None,
            ) {
                metrics["cacheModel"] = json!(model);
                metrics["cacheObservedAtMs"] = json!(expiry.observed_at_ms);
                metrics["cacheTtlMs"] = json!(expiry.ttl_ms);
                metrics["cacheExpiresAtMs"] = json!(expiry.expires_at_ms);
                metrics["cacheRemainingMs"] = json!(expiry.expires_at_ms.saturating_sub(now_ms));
                metrics["cacheExpirySource"] = json!(expiry.source);
            }
        }
    }

    if token_usage["modelContextWindow"].is_u64() {
        metrics["modelContextWindow"] = token_usage["modelContextWindow"].clone();
    } else if let Some(window) = super::local_store::known_antigravity_context_window(fallback_model) {
        metrics["modelContextWindow"] = json!(window);
    }
    if let Some(usage) = last_usage {
        // ACP 汇总不能代替逐次调用的模型和上下文；无转录明细时仅展示 Token，不猜测费用。
        crate::session_metrics_cost::attach(
            &mut metrics, usage["billingParts"].clone(), usage["billingComplete"] != false,
        );
    }
    metrics
}

/// 聚合所有 Antigravity 会话中来自 ACP 的累计 Token 用量（按实例维度）。
pub(crate) fn aggregate_instance_acp_tokens(
    conversations: &BTreeMap<String, Value>,
) -> Value {
    let mut sessions = 0u64;
    let mut input = 0u64;
    let mut output = 0u64;
    let mut cached = 0u64;
    let mut thought = 0u64;
    for entry in conversations.values() {
        sessions += 1;
        let totals = entry.get("sessionTokens").filter(|value| value.is_object()).unwrap_or(entry);
        input = input.saturating_add(totals["inputTokens"].as_u64().unwrap_or(0));
        output = output.saturating_add(totals["outputTokens"].as_u64().unwrap_or(0));
        cached = cached.saturating_add(totals["cachedInputTokens"].as_u64().unwrap_or(0));
        thought = thought.saturating_add(totals["reasoningTokens"].as_u64().unwrap_or(0));
    }
    json!({
        "sessions": sessions,
        "totalTokens": input.saturating_add(output),
        "inputTokens": input,
        "outputTokens": output,
        "cachedInputTokens": cached,
        "reasoningTokens": thought,
    })
}

#[cfg(test)]
mod metrics_tests {
    use super::*;

    #[test]
    fn agy_metrics_context_updates_are_not_billable_tokens_or_model_steps() {
        let usage = normalize_usage_update(&json!({"used": 12000, "size": 100000})).unwrap();
        let turns = vec![json!({"id":"current","status":"inProgress","turnStartedAtMs":1000,
            "items":[{"type":"agentMessage","text":"native output"},{"type":"commandExecution","id":"tool"}]})];
        let metrics = compute_session_metrics(&turns, &usage, "gemini-4-neon", 3000);
        assert!(metrics["totalTokens"].is_null(), "上下文占用不能充当累计 Token");
        assert!(metrics["steps"].is_null(), "正文和工具数量不能推算模型调用次数");
    }

    #[test]
    fn agy_metrics_live_speed_uses_observed_usage_time_instead_of_poll_time() {
        let turn = json!({"id":"current","status":"inProgress","turnStartedAtMs":1000,
            "firstTokenAtMs":1200,"_acpUsage":{"inputTokens":100,"outputTokens":60,
                "cachedReadTokens":20,"lastObservedAtMs":3000,"model":"gemini-4-neon","steps":1}});
        let first = compute_session_metrics(&[turn.clone()], &Value::Null, "", 4000);
        let later = compute_session_metrics(&[turn], &Value::Null, "", 9000);
        assert_eq!(first["tokensPerSecond"], 30.0);
        assert_eq!(later["tokensPerSecond"], first["tokensPerSecond"], "等待工具或轮询不能改变已观测速度");
        assert_eq!(first["cacheObservedAtMs"], 3000);
        assert_eq!(later["cacheExpiresAtMs"], first["cacheExpiresAtMs"], "轮询不能延长缓存倒计时");
    }

    #[test]
    fn agy_metrics_partial_usage_does_not_invent_cache_hits() {
        let normalized = normalize_usage_update(&json!({"inputTokens":100,"outputTokens":10})).unwrap();
        assert!(normalized["last"]["cachedInputTokens"].is_null());
        assert!(normalized["last"]["totalTokens"].is_null(), "缺失缓存量时不能把输入输出之和当完整总量");
    }

    #[test]
    fn agy_metrics_explicit_zero_context_replaces_the_previous_usage() {
        let old = normalize_usage_update(&json!({"used":1200,"size":10000})).unwrap();
        let zero = normalize_usage_update(&json!({"used":0,"size":10000})).unwrap();
        assert_eq!(merge_token_usage(&old, &zero, "")["last"]["totalTokens"], 0);
        let window_only = normalize_usage_update(&json!({"size":20000})).unwrap();
        assert_eq!(merge_token_usage(&old, &window_only, "")["last"]["totalTokens"], 1200);
    }

    #[test]
    fn agy_metrics_history_timestamps_do_not_claim_stream_latency() {
        let native = super::super::local_store::parse_transcript_jsonl(concat!(
            "{\"type\":\"USER_INPUT\",\"step_index\":0,\"created_at\":\"1970-01-01T00:00:01Z\",\"content\":\"history\"}\n",
            "{\"type\":\"PLANNER_RESPONSE\",\"step_index\":1,\"created_at\":\"1970-01-01T00:00:01Z\",\"model\":\"gemini-4-neon-high\",\"input_tokens\":100,\"cache_read_tokens\":20,\"output_tokens\":10}\n"
        ));
        let metrics = compute_session_metrics(&native.turns, &native.token_usage, &native.model, 10000);
        assert_eq!(metrics["turnTokens"], 130);
        assert!(metrics["firstTokenMs"].is_null());
        assert!(metrics["durationMs"].is_null());
        assert!(metrics["tokensPerSecond"].is_null());
        assert!(metrics["cacheObservedAtMs"].is_null(), "请求开始时间不能当作缓存写入或读取完成时间");
    }

    #[test]
    fn session_metrics_antigravity_separates_current_turn_and_session_totals() {
        let turns = vec![
            json!({"id":"first","_acpUsage":{"inputTokens":100,"uncachedInputTokens":60,
                "cachedReadTokens":40,"outputTokens":10,"totalTokens":110}}),
            json!({"id":"last","_acpUsage":{"inputTokens":200,"uncachedInputTokens":50,
                "cachedReadTokens":150,"outputTokens":20,"totalTokens":220}}),
        ];
        let metrics = compute_session_metrics(&turns, &Value::Null, "model", 1000);
        assert_eq!(metrics["inputTokens"], 200);
        assert_eq!(metrics["cachedInputTokens"], 150);
        assert_eq!(metrics["turnTokens"], 220);
        assert_eq!(metrics["totalTokens"], 330);
        let aggregate = aggregate_instance_acp_tokens(&BTreeMap::from([("session".into(), metrics)]));
        assert_eq!(aggregate["inputTokens"], 300);
        assert_eq!(aggregate["cachedInputTokens"], 190);
        assert_eq!(aggregate["totalTokens"], 330);
    }
}
