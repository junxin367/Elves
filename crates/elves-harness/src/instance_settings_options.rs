//! 用户要求把实际配置中的额外选项纳入设置；只开放固定字段，不向网页返回环境、命令或凭据。
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::{Value, json};

use super::ApiError;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Definition {
    backend: String,
    section: String,
    id: String,
    path: Vec<String>,
    kind: String,
    group: String,
    label: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    options: Vec<Value>,
    min: Option<u64>,
    max: Option<u64>,
    feature: Option<String>,
    #[serde(default)]
    configured_only: bool,
    read_only_reason: Option<String>,
}

fn definitions() -> &'static [Definition] {
    // 原生设置说明按用户要求保持简短，保留生效范围和对已有会话的影响。
    static OPTIONS: OnceLock<Vec<Definition>> = OnceLock::new();
    OPTIONS.get_or_init(|| {
        serde_json::from_str(include_str!("instance-setting-options.json"))
            .expect("bundled instance setting definitions must be valid")
    })
}

fn definition(backend: &str, section: &str, key: &str) -> Option<&'static Definition> {
    definitions()
        .iter()
        .find(|item| item.backend == backend && item.section == section && item.id == key)
}

fn raw_value(document: &Value, item: &Definition) -> Value {
    // 用户要求同一功能保持同一口径；原生 fast 兼容值只展示为 priority，不改写已有文件。
    if item.id == "defaultServiceTier" && document["service_tier"] == "fast" {
        return json!("priority");
    }
    if item.id == "experimentalContextManagement"
        && document["features"]["context_management"].is_boolean()
    {
        return document["features"]["context_management"].clone();
    }
    item.path
        .iter()
        .fold(document, |value, key| &value[key])
        .clone()
}

fn valid_value(item: &Definition, value: &Value) -> bool {
    value.is_null()
        || match item.kind.as_str() {
            "boolean" => value.is_boolean(),
            "integer" => value.as_u64().is_some_and(|number| {
                number >= item.min.unwrap_or(1)
                    && number <= item.max.unwrap_or(9_007_199_254_740_991)
            }),
            "integerString" => value.as_str().is_some_and(|text| {
                !text.is_empty()
                    && text.bytes().all(|byte| byte.is_ascii_digit())
                    && text.parse::<u64>().ok().is_some_and(|number| {
                        number >= item.min.unwrap_or(1)
                            && number <= item.max.unwrap_or(9_007_199_254_740_991)
                    })
            }),
            "enum" => item.options.iter().any(|option| option["value"] == *value),
            _ => false,
        }
}

fn unavailable(item: &Definition, features: &Value) -> Option<String> {
    if let Some(reason) = &item.read_only_reason {
        return Some(
            if item
                .feature
                .as_ref()
                .is_some_and(|feature| features[feature]["stage"] == "removed")
            {
                format!("所选原生引擎已移除此特性。{reason}")
            } else {
                reason.clone()
            },
        );
    }
    if item.backend == "claude" && features[&item.id] != true {
        return Some(
            "所选实例的 Claude CLI 尚未确认支持此原生字段，暂不可修改；请重新读取核对。".into(),
        );
    }
    let feature = item.feature.as_ref()?;
    match features[feature]["stage"].as_str() {
        Some("stable" | "experimental" | "underDevelopment") => None,
        Some("removed") => Some("所选实例的原生引擎已移除此功能，未修改原配置。".into()),
        Some("deprecated") => Some("所选实例的原生引擎已弃用此功能，未修改原配置。".into()),
        _ => Some("无法确认所选实例是否支持此原生特性，请重新读取；未借用其他实例的能力。".into()),
    }
}

pub(super) fn contains(backend: &str, section: &str, key: &str) -> bool {
    definition(backend, section, key).is_some()
}

pub(super) fn validate_changes(
    backend: &str,
    section: &str,
    changes: &Value,
    features: &Value,
) -> Result<(), ApiError> {
    for (key, value) in changes.as_object().into_iter().flatten() {
        if let Some(item) = definition(backend, section, key) {
            if let Some(reason) = unavailable(item, features) {
                return Err(ApiError::new(409, reason));
            }
            if !valid_value(item, value) {
                return Err(ApiError::new(400, format!("{}的值无效", item.label)));
            }
        }
    }
    Ok(())
}

pub(super) fn apply_codex(
    document: &mut toml_edit::DocumentMut,
    section: &str,
    key: &str,
    value: &Value,
) -> Result<bool, ApiError> {
    let Some(item) = definition("codex", section, key) else {
        return Ok(false);
    };
    if item.read_only_reason.is_some() || !valid_value(item, value) {
        return Err(ApiError::new(400, "此额外配置仅供查看或值无效"));
    }
    let replacement = if value.is_null() {
        None
    } else if let Some(value) = value.as_bool() {
        Some(toml_edit::Value::from(value))
    } else if let Some(value) = value.as_i64() {
        Some(toml_edit::Value::from(value))
    } else {
        Some(toml_edit::Value::from(value.as_str().unwrap()))
    };
    let context = document
        .get("features")
        .and_then(toml_edit::Item::as_table_like)
        .and_then(|table| table.get("context_management"));
    let context_is_bool = context.and_then(toml_edit::Item::as_bool).is_some();
    if key == "experimentalContextManagement"
        && value.is_null()
        && (context_is_bool
            || context
                .and_then(toml_edit::Item::as_table_like)
                .is_some_and(|table| {
                    table.is_empty() || table.len() == 1 && table.contains_key("experimental_mode")
                }))
    {
        super::set_toml(
            document.as_item_mut(),
            &["features", "context_management"],
            None,
        )?;
        return Ok(true);
    }
    if key == "experimentalContextManagement" && context_is_bool {
        // 原生配置同时接受布尔值与带 experimental_mode 的表；改成表时只替换此特性，不动其他配置。
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        document["features"]["context_management"] = toml_edit::Item::Table(table);
    }
    let path = item.path.iter().map(String::as_str).collect::<Vec<_>>();
    super::set_toml(document.as_item_mut(), &path, replacement)?;
    Ok(true)
}

pub(super) fn claude_boolean(section: &str, key: &str, value: &Value) -> bool {
    definition("claude", section, key).is_some_and(|item| {
        item.read_only_reason.is_none() && item.kind == "boolean" && valid_value(item, value)
    })
}

pub(super) fn append_view(
    backend: &str,
    section: &str,
    document: &Value,
    values: &mut Value,
    features: &Value,
) -> Vec<Value> {
    let mut rows = Vec::new();
    for item in definitions()
        .iter()
        .filter(|item| item.backend == backend && item.section == section)
    {
        let raw = raw_value(document, item);
        if item.configured_only && raw.is_null() {
            continue;
        }
        let valid = valid_value(item, &raw);
        let reason = if valid {
            unavailable(item, features)
        } else {
            Some("原配置的类型或取值无效，请在原生配置中核对；未将配置原文发送到网页。".into())
        };
        let value = if valid { raw } else { Value::Null };
        values[&item.id] = value.clone();
        rows.push(json!({
            "id":item.id,"kind":item.kind,"label":item.label,"description":item.description,
            "group":item.group,"options":item.options,"min":item.min,"max":item.max,
            "readOnly":reason.is_some(),"readOnlyReason":reason,"value":value,"valueInvalid":!valid
        }));
    }
    if backend == "claude" && section == "models" {
        // 当前 CLI 未确认 modelSettings.<model>.effortLevel 的持久化合同；只展示安全的模型 ID 和已知深度。
        for (model, settings) in document["modelSettings"]
            .as_object()
            .into_iter()
            .flatten()
            .take(64)
        {
            if model.is_empty()
                || model.len() > 200
                || !model
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_./:".contains(&byte))
            {
                continue;
            }
            let effort = settings["effortLevel"]
                .as_str()
                .filter(|value| ["low", "medium", "high", "xhigh", "max"].contains(value));
            let id = format!("nativeModelEffort:{model}");
            let value = json!(effort);
            values[&id] = value.clone();
            rows.push(json!({
                "id":id,"kind":"text","label":format!("{model} 的原生思考深度"),
                "description":"","group":"原生配置诊断","value":value,"readOnly":true,
                "readOnlyReason":"当前 CLI 未确认逐模型 effortLevel 的持久化合同；仅展示已有配置，不认定其实际生效，也不改写其他模型。",
                "options":[],"valueInvalid":effort.is_none() && !settings["effortLevel"].is_null()
            }));
        }
    }
    rows
}
