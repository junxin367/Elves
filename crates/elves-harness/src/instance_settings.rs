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
type HelpCache = BTreeMap<String, (Instant, String)>;
static CLAUDE_HELP: OnceLock<Mutex<HelpCache>> = OnceLock::new();
type CapabilityCache = BTreeMap<String, (Instant, Value)>;
static CLAUDE_OPTIONS: OnceLock<Mutex<CapabilityCache>> = OnceLock::new();

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

fn claude_default_effort(value: &Value) -> Result<Option<&str>, ApiError> {
    if value.is_null() {
        return Ok(None);
    }
    value.as_str().filter(|effort| ["low", "medium", "high", "xhigh", "max"].contains(effort))
        .map(Some).ok_or_else(|| ApiError::new(400, "Claude 默认思考深度必须是当前模型支持的原生等级，或使用模型默认"))
}

pub(super) fn decorate_claude_model_visibility(
    directory: &Path,
    instance: &str,
    mut catalog: Value,
) -> Result<Value, ApiError> {
    let (preference, _) = preferences(directory)?;
    let hidden = hidden_model_ids(&preference["instances"][instance]["hiddenModelIds"])?;
    // 用户要求 Claude 默认显示全部模型、允许按实例隐藏；保留原生完整目录及能力，隐藏不能改变已有会话的执行模型。
    if let Some(rows) = catalog["data"].as_array_mut() {
        for row in rows {
            row["hiddenByInstance"] = json!(row["model"].as_str()
                .is_some_and(|id| hidden.iter().any(|hidden| hidden == id)));
        }
    }
    // 用户要求在会话实例设置 Claude 默认深度；只叠加完整目录的默认值，历史会话快照仍保留自身等级。
    if catalog["catalogComplete"] != false {
        let effort = claude_default_effort(&preference["instances"][instance]["defaultReasoningEffort"])?;
        let supported = effort.is_some_and(|effort| catalog["data"].as_array().is_some_and(|rows| rows.iter().any(|row| {
            row["supportedReasoningEfforts"].as_array().is_some_and(|efforts| {
                efforts.iter().any(|value| value["reasoningEffort"] == effort)
            })
        })));
        // 保存回执可能带有保存前的展示默认值；恢复“使用模型默认”时同样必须清除该旧值。
        catalog["defaultReasoningEffort"] = json!(if supported { effort.unwrap() } else { "" });
    }
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

pub(super) fn claude_auto_compact(
    directory: &Path,
    instance: &str,
) -> Result<Option<String>, ApiError> {
    let (value, _) = preferences(directory)?;
    let value = &value["instances"][instance]["autoCompactWindow"];
    validate_compact(value)?;
    Ok(match value {
        Value::Null => None,
        Value::String(value) => Some(value.clone()),
        value => Some(value.to_string()),
    })
}

fn validate_compact(value: &Value) -> Result<(), ApiError> {
    if value.is_null()
        || value == "auto"
        || value
            .as_u64()
            .is_some_and(|tokens| (100_000..=1_000_000).contains(&tokens))
    {
        Ok(())
    } else {
        Err(ApiError::new(
            400,
            "自动压缩窗口必须为自动或 100000～1000000 Token",
        ))
    }
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

fn optional_bool(value: &Value) -> Result<Value, ApiError> {
    if value.is_null() || value.is_boolean() {
        Ok(value.clone())
    } else {
        Err(ApiError::new(422, "实例配置中的开关类型无效，未覆盖原配置"))
    }
}

fn validate_compaction_prompt(value: &Value) -> Result<(), ApiError> {
    if value.is_null()
        || value.as_str().is_some_and(|text| {
            !text.trim().is_empty()
                && text.chars().count() <= 20_000
                && !text.chars().any(|character| {
                    character.is_control() && !matches!(character, '\n' | '\r' | '\t')
                })
        })
    {
        Ok(())
    } else {
        Err(ApiError::new(400, "压缩提示词需为 1～20000 个字符，或恢复默认"))
    }
}

pub(super) fn claude_compaction_prompt(
    directory: &Path,
    instance: &str,
) -> Result<String, ApiError> {
    let (preference, _) = preferences(directory)?;
    let value = &preference["instances"][instance]["compactionPrompt"];
    validate_compaction_prompt(value)?;
    Ok(value.as_str().unwrap_or("").to_owned())
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
        if matches!(section, "general" | "models")
            && changes.contains_key("model")
            && !changes.contains_key("reasoningEffort")
        {
            // 切换默认模型时同步撤销旧模型深度；独立 API 调用也遵守页面的同一约定。
            set_toml(document.as_item_mut(), &["model_reasoning_effort"], None)?;
        }
        for (key, value) in changes {
            if options::apply_codex(&mut document, section, key, value)? {
                continue;
            }
            let path: &[&str] = match (section, key.as_str()) {
                ("general", "todo") => &["tools", "update_plan", "enabled"],
                ("general", "memories") => &["features", "memories"],
                ("general", "multiAgent") => &["features", "multi_agent_v2", "enabled"],
                ("general", "goals") => &["features", "goals"],
                ("general" | "models", "model") => &["model"],
                ("general" | "models", "reasoningEffort") => &["model_reasoning_effort"],
                ("hooks", "enabled") => &["features", "hooks"],
                _ => return Err(ApiError::new(400, "不支持的 Codex 实例设置")),
            };
            let replacement = if value.is_null() {
                None
            } else if matches!(key.as_str(), "todo" | "memories" | "multiAgent" | "goals" | "enabled") {
                Some(toml_edit::Value::from(
                    value
                        .as_bool()
                        .ok_or_else(|| ApiError::new(400, "开关必须为布尔值"))?,
                ))
            } else {
                Some(toml_edit::Value::from(
                    value
                        .as_str()
                        .filter(|value| {
                            !value.is_empty()
                                && value.len() <= 200
                                && !value.chars().any(char::is_control)
                        })
                        .ok_or_else(|| ApiError::new(400, "模型或思考深度无效"))?,
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
        for (key, value) in changes {
            if !options::claude_boolean(section, key, value) {
                match (section, key.as_str()) {
                    ("general", "awaySummaryEnabled") if value.is_boolean() => {}
                    ("general", "promptSuggestionEnabled")
                        if value.is_null() || value.is_boolean() => {}
                    ("hooks", "enabled") if value.is_boolean() => {
                        root.insert("disableAllHooks".into(), json!(!value.as_bool().unwrap()));
                        continue;
                    }
                    // CLI 仅确认 model 是可持久化的原生设置；effortLevel 未获确认，不能写入后显示假成功。
                    ("general" | "models", "model")
                        if value.is_null()
                            || value.as_str().is_some_and(|value| {
                                !value.is_empty()
                                    && value.len() <= 200
                                    && !value.chars().any(char::is_control)
                            }) => {}
                    _ => return Err(ApiError::new(400, "不支持的 Claude 实例设置")),
                }
            }
            if value.is_null() {
                root.remove(key);
            } else {
                root.insert(key.clone(), value.clone());
            }
        }
        let ending = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let mut output = serde_json::to_string_pretty(&document)
            .unwrap()
            .replace('\n', ending);
        if text.starts_with('\u{feff}') {
            output.insert(0, '\u{feff}');
        }
        output.push_str(ending);
        Ok(output)
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

fn validate_models(
    backend: &str,
    document: &Value,
    changes: &Value,
    catalog: &Value,
    ultra: bool,
) -> Result<(), ApiError> {
    if backend == "claude" && changes.get("effortLevel").is_some() {
        return Err(ApiError::new(
            400,
            "Claude 默认思考深度请使用会话实例的 reasoningEffort 设置，不写入原生 effortLevel 字段。",
        ));
    }
    let model = changes
        .get("model")
        .unwrap_or(&document["model"])
        .as_str()
        .or_else(|| catalog["defaultModel"].as_str());
    let effort = changes.get("reasoningEffort").and_then(Value::as_str);
    if changes["model"].is_null() && changes.get("model").is_some() && effort.is_some() {
        return Err(ApiError::new(
            400,
            "默认模型选择“使用原生默认”时，默认思考深度需选择“使用模型默认”。",
        ));
    }
    let selected = model.and_then(|model| {
        catalog["data"]
            .as_array()?
            .iter()
            .find(|row| row["model"] == model)
            .or_else(|| (backend == "claude").then(|| {
                catalog["data"].as_array()?.iter().find(|row| row["resolvedModel"] == model)
            }).flatten())
    });
    // 移除失效的模型覆盖不依赖旧模型仍在目录中；只有明确选择新模型时才要求目录命中。
    if changes.get("model").is_some_and(|value| !value.is_null()) && selected.is_none() {
        return Err(ApiError::new(409, "模型已不在当前实例目录中，请重新读取"));
    }
    if let Some(effort) = effort {
        if effort == "ultra" && (backend != "codex" || !ultra) {
            return Err(ApiError::new(409, "当前实例未开放 Ultra 思考深度"));
        }
        if !selected.is_some_and(|model| {
            model["supportedReasoningEfforts"]
                .as_array()
                .is_some_and(|values| {
                    values
                        .iter()
                        .any(|value| value["reasoningEffort"] == effort)
                })
        }) {
            return Err(ApiError::new(409, "当前模型不支持该思考深度，请重新选择"));
        }
    }
    Ok(())
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
            "promptSuggestions":help.contains("--prompt-suggestions"),
            "autoCompactWindow":help.contains("--autocompact"),
            "defaultReasoningEffort":help.contains("--effort <")
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
        if changes.keys().any(|key| matches!(key.as_str(), "model" | "reasoningEffort" | "effortLevel"))
            && catalog.is_none()
        {
            return Err(ApiError::new(409, "未取得当前实例模型目录，请重新读取后再保存模型设置"));
        }
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
            if target.backend != "claude" || !matches!(section, "general" | "models") || changes.len() != 1
                || !changes["hiddenModelIds"].is_array()
            {
                return Err(ApiError::new(400, "模型显示设置仅支持单独修改当前 Claude 实例的隐藏名单"));
            }
            let ids = hidden_model_ids(&changes["hiddenModelIds"])?;
            let (preference, _) = preferences(directory)?;
            let previous = hidden_model_ids(&preference["instances"][instance]["hiddenModelIds"])?;
            validate_hidden_model_selection(&ids, &previous, catalog.as_ref())?;
            save_preference(directory, instance, "hiddenModelIds", &json!(ids),
                payload["expectedPreferenceRevision"].as_str().unwrap_or(""), &authorization).await?;
        } else if target.backend == "claude" && changes.contains_key("reasoningEffort") {
            if !matches!(section, "general" | "models") || changes.len() != 1 {
                return Err(ApiError::new(400, "Claude 默认思考深度需要单独保存"));
            }
            let value = &changes["reasoningEffort"];
            if claude_default_effort(value)?.is_some() && capabilities["defaultReasoningEffort"] != true {
                return Err(ApiError::new(409, "所选实例的 Claude CLI 未提供 --effort，无法设置默认思考深度"));
            }
            validate_models("claude", &document, &payload["settings"], catalog.as_ref().unwrap(), false)?;
            // 默认深度属于 Harness 实例偏好，后续新会话经原生控制接口交给 CLI 的 --effort；不伪造 settings.json 键。
            save_preference(directory, instance, "defaultReasoningEffort", value,
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
        } else if changes.contains_key("compactionPrompt") && target.backend == "claude" {
            if section != "general" || changes.len() != 1 {
                return Err(ApiError::new(400, "压缩提示词需要单独保存"));
            }
            validate_compaction_prompt(&changes["compactionPrompt"])?;
            // Claude 只给所选实例的原生 /compact 传附加说明，不伪造 CLI 配置键或改变正常对话的系统提示词。
            save_preference(
                directory, instance, "compactionPrompt", &changes["compactionPrompt"],
                payload["expectedPreferenceRevision"].as_str().unwrap_or(""), &authorization,
            ).await?;
        } else if changes.contains_key("autoCompactWindow") {
            if section != "general"
                || target.backend != "claude"
                || changes.len() != 1
                || capabilities["autoCompactWindow"] != true
            {
                return Err(ApiError::new(400, "当前 Claude CLI 不支持自动压缩窗口设置"));
            }
            validate_compact(&changes["autoCompactWindow"])?;
            save_preference(
                directory,
                instance,
                "autoCompactWindow",
                &changes["autoCompactWindow"],
                payload["expectedPreferenceRevision"].as_str().unwrap_or(""),
                &authorization,
            )
            .await?;
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
            if let Some(catalog) = &catalog
                && changes.keys().any(|key| matches!(key.as_str(), "model" | "reasoningEffort" | "effortLevel"))
            {
                validate_models(
                    &target.backend,
                    &document,
                    &payload["settings"],
                    catalog,
                    ultra_enabled(directory, instance)?,
                )?;
            }
            save_native(&target, &text, &revision, &payload, &authorization).await?;
        }
        if changes.contains_key("ultraReasoningEnabled")
            || changes.contains_key("persistentSideThreads")
            || changes.contains_key("autoCompactWindow")
            || changes.contains_key("compactionPrompt") && target.backend == "claude"
            || changes.contains_key("hiddenModelIds")
            || target.backend == "claude" && changes.contains_key("reasoningEffort")
        {
            state
                .controller
                .shared_settings
                .instance_settings_changed(&state.controller, instance, &payload["settings"]);
        }
        updated = true;
    }
    if updated && model_settings && catalog.is_some()
        && ["model", "reasoningEffort", "ultraReasoningEnabled"].iter().any(|key| payload["settings"].get(*key).is_some())
    {
        catalog = Some(model_catalog(&state, instance, &authorization, true).await?);
    }
    let (text, revision) = permissions::read_file(&target.path)?;
    let document = native_document(&target.backend, &text)?;
    let (preference, preference_revision) = preferences(directory)?;
    let mut values = match section {
        "general" => general_view(&target.backend, &document)?,
        "models" => json!({
            "model":document["model"],
            "reasoningEffort":if target.backend == "codex" {
                document["model_reasoning_effort"].clone()
            } else {Value::Null},
            "ultraReasoningEnabled":ultra_enabled(directory, instance)?
        }),
        _ => json!({}),
    };
    if target.backend == "codex" && section == "general" {
        values["persistentSideThreads"] = json!(persistent_sides_enabled(directory, instance)?);
    }
    if section == "general" && model_settings {
        let defaults = json!({
            "model":document["model"],
            "reasoningEffort":if target.backend == "codex" {
                document["model_reasoning_effort"].clone()
            } else {Value::Null},
            "ultraReasoningEnabled":ultra_enabled(directory, instance)?
        });
        values.as_object_mut().unwrap().extend(defaults.as_object().unwrap().clone());
    }
    if target.backend == "claude" && section == "general" {
        let window = &preference["instances"][instance]["autoCompactWindow"];
        validate_compact(window)?;
        values["autoCompactWindow"] = window.clone();
    }
    if target.backend == "claude" && matches!(section, "general" | "models") {
        values["hiddenModelIds"] = json!(hidden_model_ids(
            &preference["instances"][instance]["hiddenModelIds"])?);
        values["reasoningEffort"] = json!(claude_default_effort(
            &preference["instances"][instance]["defaultReasoningEffort"])?);
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
        // 先按实例暴露真实能力边界：当前两个原生引擎均未确认独立压缩模型接口，不能借用旧代理模型配置。
        values["compactionModel"] = json!("session");
        extra_settings.push(json!({
            "id":"compactionModel","kind":"enum","label":"压缩模型","group":"缓存与压缩",
            "description":"","value":"session","readOnly":true,
            "readOnlyReason":"当前尚未确认该实例的独立压缩模型接口，暂跟随会话模型；未借用代理的全局设置。",
            "options":[{"value":"session","label":"跟随会话模型"}]
        }));
        if target.backend == "claude" {
            let prompt = &preference["instances"][instance]["compactionPrompt"];
            validate_compaction_prompt(prompt)?;
            values["compactionPrompt"] = prompt.clone();
            extra_settings.push(json!({
                "id":"compactionPrompt","kind":"text","label":"压缩提示词","group":"缓存与压缩",
                "description":"作为所选实例原生 /compact 的附加说明，仅用于手动压缩；自动压缩继续使用原生规则。",
                "value":prompt,"readOnly":false,"readOnlyReason":null,"options":[],
                "min":null,"max":20000,"defaultValue":null
            }));
        }
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
        }
        response["models"] = catalog["data"].clone();
        response["effectiveModel"] = catalog["defaultModel"].clone();
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
            inventory["data"] = native["data"].clone();
            inventory["message"] = json!(if state.settings.read_only {
                "只读访问不会启动 Claude Skill 探测，请在该实例的原生 CLI 中查看。"
            } else {
                "由所选实例的 Claude CLI 返回；单项启停仍由原生 CLI 管理。"
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
    }

    #[test]
    fn claude_instance_default_effort_is_scoped_supported_and_resettable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(PREFERENCES_FILE);
        std::fs::write(&path, r#"{"instances":{"claude:wsl:one":{"defaultReasoningEffort":"max"}}}"#).unwrap();
        let catalog = json!({"catalogComplete":true,"defaultModel":"model-a","defaultReasoningEffort":"",
            "data":[{"model":"model-a","supportedReasoningEfforts":[{"reasoningEffort":"max"}]}]});
        let shown = decorate_claude_model_visibility(directory.path(), "claude:wsl:one", catalog.clone()).unwrap();
        assert_eq!(shown["defaultReasoningEffort"], "max");
        let other = decorate_claude_model_visibility(directory.path(), "claude:wsl:two", catalog.clone()).unwrap();
        assert_eq!(other["defaultReasoningEffort"], "");
        let mut unsupported = catalog.clone();
        unsupported["data"][0]["supportedReasoningEfforts"] = json!([]);
        assert_eq!(decorate_claude_model_visibility(directory.path(), "claude:wsl:one", unsupported).unwrap()["defaultReasoningEffort"], "");
        let mut history = catalog.clone();
        history["catalogComplete"] = json!(false);
        history["defaultReasoningEffort"] = json!("low");
        assert_eq!(decorate_claude_model_visibility(directory.path(), "claude:wsl:one", history).unwrap()["defaultReasoningEffort"], "low");
        std::fs::write(&path, r#"{"instances":{"claude:wsl:one":{}}}"#).unwrap();
        assert_eq!(decorate_claude_model_visibility(directory.path(), "claude:wsl:one", shown).unwrap()["defaultReasoningEffort"], "");
        assert!(claude_default_effort(&json!("ultra")).is_err());
        assert!(claude_default_effort(&json!(true)).is_err());
        assert!(validate_models("claude", &json!({"model":"model-a"}), &json!({"reasoningEffort":"max"}), &catalog, false).is_ok());
        assert!(validate_models("claude", &json!({"model":"model-a"}), &json!({"reasoningEffort":"high"}), &catalog, false).is_err());
        assert!(updated_native("claude", "general", "{}", &json!({"reasoningEffort":"max"})).is_err());
    }

    #[test]
    fn merged_instance_settings_preserve_model_and_compaction_native_contracts() {
        let before = "# 保留配置\nmodel = \"old\"\nmodel_reasoning_effort = \"high\"\n[features]\ngoals = true\n";
        let updated = updated_native("codex", "general", before, &json!({"model":"new"})).unwrap();
        let updated = updated_native("codex", "general", &updated,
            &json!({"autoCompactTokenLimit":600000,"reasoningSummary":"detailed","defaultServiceTier":"priority"})).unwrap();
        let saved = native_document("codex", &updated).unwrap();
        assert_eq!(saved["model"], "new");
        assert!(saved["model_reasoning_effort"].is_null());
        assert_eq!(saved["model_auto_compact_token_limit"], 600000);
        assert_eq!(saved["model_reasoning_summary"], "detailed");
        assert_eq!(saved["service_tier"], "priority");
        assert_eq!(saved["features"]["goals"], true);
        assert!(updated.contains("# 保留配置"));
        assert_eq!(native_document("claude",
            &updated_native("claude", "general", "{}", &json!({"model":"selected-model"})).unwrap()).unwrap()["model"], "selected-model");
        assert!(updated_native("codex", "general", before, &json!({"compactionModel":"invented"})).is_err());
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
    fn claude_compaction_prompt_is_scoped_to_the_exact_instance() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join(PREFERENCES_FILE),
            serde_json::to_string(&json!({"instances":{
                "claude:wsl:one":{"compactionPrompt":"实例一说明"},
                "claude:wsl:two":{"compactionPrompt":"实例二说明"}
            }})).unwrap()).unwrap();
        assert_eq!(claude_compaction_prompt(directory.path(), "claude:wsl:one").unwrap(), "实例一说明");
        assert_eq!(claude_compaction_prompt(directory.path(), "claude:wsl:two").unwrap(), "实例二说明");
        assert_eq!(claude_compaction_prompt(directory.path(), "claude:wsl:missing").unwrap(), "");
        assert!(validate_compaction_prompt(&json!(" \n\t")).is_err());
        assert!(validate_compaction_prompt(&json!("invalid\u{0000}")).is_err());
        assert!(validate_compaction_prompt(&json!("x".repeat(20001))).is_err());
        assert!(validate_compaction_prompt(&Value::Null).is_ok());
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
            &json!({"autoCompactTokenLimit":900000,"reasoningSummary":"detailed","defaultServiceTier":"priority"})).unwrap();
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
        assert_eq!(saved["model_auto_compact_token_limit"], 900000);
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
                &json!({"autoCompactTokenLimit":9007199254740992_u64})
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
        assert_eq!(values["nativePromptCacheTtl"], "1h");
        for key in [
            "nativePromptCacheTtl",
            "nativeAutoCompactWindow",
            "nativeAutoCompactPercent",
        ] {
            assert!(
                rows.iter()
                    .any(|row| row["id"] == key && row["readOnly"] == true)
            );
            assert!(updated_native("claude", "general", "{}", &json!({key:null})).is_err());
            assert!(
                options::validate_changes("claude", "general", &json!({key:null}), &json!({}))
                    .is_err()
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
}
