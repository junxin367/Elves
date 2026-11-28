//! 只投影有原生后台身份的任务，不以普通工具完成、列表消失或超时推断终态。
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// 使用当前连接实际声明的版本；版本不明或低于隔离双任务实测的版本，仅保留只读能力。
pub(crate) fn supported_since(version: &str, minimum: &str) -> bool {
    use crate::cli_wrapper::{compare_codex_versions, parse_codex_cli_version};
    parse_codex_cli_version(&version.replace('/', " "))
        .zip(parse_codex_cli_version(minimum))
        .is_some_and(|(actual, min)| !compare_codex_versions(&actual, &min).is_lt())
}

pub(crate) fn terminal(status: &Value) -> Option<&'static str> {
    match status.as_str()? {
        "completed" => Some("completed"),
        "failed" => Some("failed"),
        "stopped" | "killed" | "interrupted" => Some("stopped"),
        "cancelled" | "canceled" => Some("cancelled"),
        _ => None,
    }
}

pub(crate) fn bounded_tail(s: &str, limit: usize) -> (&str, bool) {
    let mut start = s.len().saturating_sub(limit);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    (&s[start..], start != 0)
}

pub(crate) fn describe(id: &str, kind: &str, name: &Value) -> Value {
    json!({"id":id,"kind":kind,"name":name,"status":"unknown","canStop":false})
}

fn copy(row: &mut Value, key: &str, value: &Value) {
    if !value.is_null() {
        row[key] = value.clone();
    }
}

/// 列表不携带输出，仅打开对应任务详情时返回该任务的原文。
pub(crate) fn historical(turns: &[Value], detail: Option<&str>) -> BTreeMap<String, Value> {
    let mut rows = BTreeMap::new();
    for item in turns
        .iter()
        .flat_map(|t| t["items"].as_array().into_iter().flatten())
    {
        let notice = item["type"] == "claudeTaskNotification";
        let id = if notice {
            if item["kind"] == "schedule" || item["ambient"] == true {
                continue;
            }
            item["taskId"].as_str()
        } else {
            item["backgroundTaskId"].as_str()
        };
        let Some(id) = id.filter(|id| !id.is_empty() && detail.is_none_or(|want| want == *id))
        else {
            continue;
        };
        let kind = if notice {
            item["kind"].as_str().unwrap_or("task")
        } else {
            "command"
        };
        let row = rows
            .entry(id.to_owned())
            .or_insert_with(|| describe(id, kind, &Value::Null));
        if kind != "task" {
            row["kind"] = json!(kind);
        }
        let state = if notice {
            &item["status"]
        } else if item["backgroundDisposition"].is_string() {
            &item["backgroundDisposition"]
        } else {
            &item["backgroundStatus"]
        };
        if let Some(status) = terminal(state) {
            if row["status"] != "failed" || status != "completed" {
                row["status"] = json!(status);
            }
        }
        copy(row, "name", &item["description"]);
        if row["name"].is_null() {
            copy(row, "name", &item["command"]);
        }
        copy(row, "exitCode", &item["exitCode"]);
        copy(row, "startedAtMs", &item["startedAtMs"]);
        copy(row, "startedAtMs", &item["backgroundStartedAtMs"]);
        copy(row, "completedAtMs", &item["backgroundEndedAtMs"]);
        if notice {
            copy(row, "completedAtMs", &item["completedAtMs"]);
        }
        if detail.is_some() {
            copy(row, "command", &item["command"]);
            // Claude 的初次 Bash 工具返回只是“已转入后台”的启动回执，不是命令 stdout。
            if item["source"] == "claude" {
                copy(row, "output", &item["backgroundOutput"]);
                copy(row, "outputTruncated", &item["backgroundOutputTruncated"]);
                if item["backgroundOutput"].is_string() {
                    row["outputKind"] = json!("final");
                }
            } else {
                if terminal(&item["backgroundStatus"]).is_some() {
                    copy(row, "output", &item["aggregatedOutput"]);
                    row["outputKind"] = json!("final");
                }
            }
            copy(
                row,
                "summary",
                if notice {
                    &item["summary"]
                } else {
                    &item["backgroundSummary"]
                },
            );
        }
    }
    rows
}

pub(crate) fn result(
    id: &str,
    mut records: Vec<Value>,
    detail: Option<&str>,
    reason: Option<&str>,
) -> Value {
    for row in &mut records {
        if let Some(o) = row.as_object_mut() {
            o.remove("nativeProcessId");
            o.remove("terminated");
        }
    }
    if let Some(task) = detail {
        let item = records.into_iter().find(|r| r["id"] == task);
        return json!({"ok":true,"threadId":id,"taskId":task,"task":item,"reason":reason});
    }
    records.retain(|row| terminal(&row["status"]).is_some());
    records.sort_by(|a, b| {
        b["completedAtMs"]
            .as_u64()
            .cmp(&a["completedAtMs"].as_u64())
    });
    json!({"ok":true,"threadId":id,"records":records,"reason":reason})
}
