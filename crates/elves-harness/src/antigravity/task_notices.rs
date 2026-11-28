//! 只读原生转录中的后台命令回执。ACP 普通工具完成和模型输出本身都不是任务唤醒证据。

use std::collections::HashMap;

use serde_json::{Value, json};

const LAUNCH: &str = "Tool is running as a background task with task id: ";
const ENVELOPE: &str = "The following is a <SYSTEM_MESSAGE> not actually sent by the user. It is provided by the system as important information to pay attention to.";

#[derive(Default)]
enum Previous {
    #[default]
    Other,
    Command(u64),
    Answered(u64),
    SuccessfulWake {
        step: u64,
        notice: Value,
    },
}

#[derive(Default)]
pub(super) struct TaskNotices {
    previous: Previous,
    open: HashMap<u64, (usize, usize)>,
}

fn launch_id(content: &str) -> Option<&str> {
    let mut lines = content.lines();
    let first = lines.next()?;
    if let Some(id) = first.strip_prefix(LAUNCH) {
        return Some(id);
    }
    let _ = chrono::DateTime::parse_from_rfc3339(first.strip_prefix("Created At: ")?).ok()?;
    lines.next()?.strip_prefix(LAUNCH)
}

pub(super) fn has_unsettled_command(turn: &Value, session: &str) -> bool {
    turn["items"].as_array().into_iter().flatten().any(|item| {
        item["type"] == "commandExecution"
            && !matches!(item["backgroundStatus"].as_str(), Some("completed" | "failed" | "stopped"))
            && item["backgroundTaskId"].as_str().or_else(|| launch_id(item["aggregatedOutput"].as_str()?))
                .and_then(|id| id.strip_prefix(session)?.strip_prefix("/task-"))
                .is_some_and(|step| step.parse::<u64>().is_ok())
    })
}

// 来源、信封和发送者都要匹配。命令 stdout、用户引用、仍在执行的提醒，不能冒充原生结束。
fn result<'a>(
    content: &'a str,
    session: &str,
) -> Option<(u64, &'static str, Option<i64>, &'a str)> {
    let mut lines = content.lines();
    if lines.next()? != ENVELOPE || lines.next()? != "" || lines.next()? != "<SYSTEM_MESSAGE>" {
        return None;
    }
    let (time, rest) = lines
        .next()?
        .strip_prefix("[Message] timestamp=")?
        .split_once(" sender=")?;
    chrono::DateTime::parse_from_rfc3339(time).ok()?;
    let (sender, message) =
        rest.split_once(" priority=MESSAGE_PRIORITY_HIGH content=Task id \"")?;
    let step = sender
        .strip_prefix(session)?
        .strip_prefix("/task-")?
        .parse()
        .ok()?;
    let disposition = message.strip_prefix(sender)?;
    let mut first = lines.next()?;
    if lines.next_back()? != "</SYSTEM_MESSAGE>" {
        return None;
    }
    match disposition {
        "\" was canceled with result:" => Some((step, "stopped", None, first)),
        "\" finished with result:" => {
            // 只认原生结果开头（含官方截断预览头），不扫描 stdout 或打开日志 URL 来猜退出码。
            if first.starts_with("The tool's output was truncated due to being too long. You can find the full output in a file at file:///") {
                if lines.next()? != "" || lines.next()? != "Here is a preview with the middle of the output truncated:" {
                    return None;
                }
                first = lines.next()?;
            }
            let code: i64 = first
                .strip_prefix("Command exited with code ")?
                .strip_suffix('.')?
                .parse()
                .ok()?;
            Some((
                step,
                if code == 0 { "completed" } else { "failed" },
                Some(code),
                first,
            ))
        }
        _ => None,
    }
}

impl TaskNotices {
    pub(super) fn observe(&mut self, session: &str, row: &Value, turns: &mut [Value]) {
        let previous = std::mem::take(&mut self.previous);
        let Some(step) = row["step_index"].as_u64() else {
            return;
        };
        let content = row["content"].as_str().unwrap_or("");
        let kind = row["type"].as_str();
        if kind == Some("PLANNER_RESPONSE")
            && row["source"] == "MODEL"
            && row["status"] == "DONE"
            && row["model"].as_str().is_some_and(|model| !model.is_empty())
        {
            let calls = row["tool_calls"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            if let Previous::SuccessfulWake { step: wake, notice } = previous
                && wake.checked_add(1) == Some(step)
                && (!content.trim().is_empty()
                    || !calls.is_empty()
                    || row["thinking"]
                        .as_str()
                        .is_some_and(|s| !s.trim().is_empty()))
                && let Some(items) = turns
                    .last_mut()
                    .and_then(|turn| turn["items"].as_array_mut())
            {
                items.push(notice);
            }
            self.previous = match calls {
                [call] if call["name"] == "run_command" => Previous::Command(step),
                [] if !content.trim().is_empty() => Previous::Answered(step),
                _ => Previous::Other,
            };
        } else if kind == Some("GENERIC") && row["source"] == "MODEL" && row["status"] == "RUNNING"
        {
            let Previous::Command(call) = previous else {
                return;
            };
            let id = format!("{session}/task-{step}");
            if call.checked_add(1) != Some(step) || launch_id(content) != Some(id.as_str()) {
                return;
            }
            let index = turns.len().saturating_sub(1);
            let Some(items) = turns
                .last_mut()
                .and_then(|turn| turn["items"].as_array_mut())
            else {
                return;
            };
            let call_id = format!("agy-local-tool-{call}-0");
            if let Some((i, command)) = items
                .iter_mut()
                .enumerate()
                .find(|(_, item)| item["type"] == "commandExecution" && item["id"] == call_id)
            {
                command["backgroundTaskId"] = json!(id);
                if let Some(time)=row["created_at"].as_str().and_then(|s|chrono::DateTime::parse_from_rfc3339(s).ok()) {
                    command["backgroundStartedAtMs"]=json!(time.timestamp_millis());
                }
                // 历史只证明曾转入后台；未收到终态时不把旧记录作为实时运行列表。
                command["backgroundStatus"] = json!("unknown");
                self.open.insert(step, (index, i));
            }
        } else if kind == Some("SYSTEM_MESSAGE")
            && row["source"] == "SYSTEM"
            && row["status"] == "DONE"
        {
            let Some((task, status, code, summary)) = result(content, session) else {
                return;
            };
            let Some((t, i)) = self.open.remove(&task) else {
                return;
            };
            let command = &mut turns[t]["items"][i];
            command["backgroundStatus"] = json!(status);
            if let Some(time) = content.lines().find_map(|line| line.strip_prefix("[Message] timestamp="))
                .and_then(|s| s.split_once(" sender=")).and_then(|(s,_)| chrono::DateTime::parse_from_rfc3339(s).ok()) {
                command["backgroundEndedAtMs"] = json!(time.timestamp_millis());
            }
            if status == "stopped" { command["backgroundDisposition"] = json!("cancelled"); }
            command["backgroundSummary"] = json!(summary);
            // 原生输出留在原工具，并保留内部空白与换行。只去掉信封的最后一个换行，不展开截断日志的路径。
            if let Some(body) = content.strip_suffix("</SYSTEM_MESSAGE>").or_else(|| content.strip_suffix("</SYSTEM_MESSAGE>\n"))
                && let Some(body) = body.strip_suffix("\r\n").or_else(|| body.strip_suffix('\n'))
                && let Some((_, output)) = body.split_once("\nCommand output:\r\n").or_else(|| body.split_once("\nCommand output:\n"))
            {
                command["aggregatedOutput"] = json!(output);
            }
            command["exitCode"] = json!(code);
            command["status"] = json!(if status == "stopped" {
                "interrupted"
            } else {
                status
            });
            // 已答完且无待执行工具 → 本任务原生成功唤醒消息 → 新模型输出，三步连续才解释这次续跑。
            // 中间夹入用户输入、其他任务或重试时不做归因；同一任务的重复结束回执已从 open 移除。
            if status == "completed"
                && matches!(previous, Previous::Answered(n) if n.checked_add(1) == Some(step))
            {
                self.previous = Previous::SuccessfulWake {
                    step,
                    notice: json!({
                        "id":format!("agy-task-wake-{task}-{step}"),"type":"backgroundTaskContinuation",
                        "source":"antigravity","kind":"command","taskId":command["backgroundTaskId"],
                        "status":"completed","summary":summary
                    }),
                };
            }
        }
    }
}

/// 已由 Elves 接续执行的历史不能被本地转录覆盖。只按同一任务的启动回执补回工具终态，
/// 同一轮内有唯一匹配的后续回复时，再将已证实的通知插在回复之前；不复制模型正文或错误。
pub(super) fn merge_managed(saved: &mut [Value], parsed: &[Value]) {
    let mut owners = HashMap::new();
    for (t, turn) in saved.iter().enumerate() {
        for (i, item) in turn["items"].as_array().into_iter().flatten().enumerate() {
            if item["type"] != "commandExecution" {
                continue;
            }
            if let Some(id) = item["backgroundTaskId"]
                .as_str()
                .or_else(|| launch_id(item["aggregatedOutput"].as_str()?))
            {
                owners
                    .entry(id.to_owned())
                    .and_modify(|owner| *owner = None)
                    .or_insert(Some((t, i)));
            }
        }
    }
    if owners.is_empty() {
        return;
    }
    // 先写回工具状态，后插入通知，避免索引随插入移动。
    for item in parsed
        .iter()
        .flat_map(|turn| turn["items"].as_array().into_iter().flatten())
    {
        if item["type"] != "commandExecution" {
            continue;
        }
        let Some((t, i)) = item["backgroundTaskId"]
            .as_str()
            .and_then(|id| owners.get(id))
            .copied()
            .flatten()
        else {
            continue;
        };
        let target = &mut saved[t]["items"][i];
        if target["command"] != item["command"] || !item["backgroundSummary"].is_string() {
            continue;
        }
        if matches!(target["status"].as_str(), Some("failed" | "interrupted")) && target["status"] != item["status"] {
            continue;
        }
        if let Some(output) = item["aggregatedOutput"].as_str()
            && target["aggregatedOutput"].as_str().is_none_or(|text| text.is_empty()
                || launch_id(text) == item["backgroundTaskId"].as_str())
        {
            target["aggregatedOutput"] = json!(output);
        }
        for field in [
            "backgroundTaskId",
            "backgroundStatus",
            "backgroundSummary",
            "backgroundEndedAtMs",
            "backgroundStartedAtMs",
            "backgroundDisposition",
            "status",
            "exitCode",
        ] {
            target[field] = item[field].clone();
        }
    }
    for turn in parsed {
        let Some(items) = turn["items"].as_array() else {
            continue;
        };
        for (index, event) in items
            .iter()
            .enumerate()
            .filter(|(_, item)| item["type"] == "backgroundTaskContinuation")
        {
            let Some((owner, _)) = event["taskId"]
                .as_str()
                .and_then(|id| owners.get(id))
                .copied()
                .flatten()
            else {
                continue;
            };
            let Some(target) = saved[owner]["items"].as_array_mut() else {
                continue;
            };
            if target.iter().any(|item| item["id"] == event["id"])
                || !items[..index]
                    .iter()
                    .any(|item| item["backgroundTaskId"] == event["taskId"])
            {
                continue;
            }
            let Some(reply) = items[index + 1..]
                .iter()
                .find(|item| item["type"] != "reasoning")
            else {
                continue;
            };
            if reply["type"] != "agentMessage" {
                continue;
            }
            let mut matches = target
                .iter()
                .enumerate()
                .filter(|(_, item)| item["type"] == "agentMessage" && item["text"] == reply["text"])
                .map(|(i, _)| i);
            if let Some(at) = matches.next()
                && matches.next().is_none()
                && target[..at]
                    .iter()
                    .any(|item| item["backgroundTaskId"] == event["taskId"] && item["backgroundStatus"] == "completed")
            {
                target.insert(at, event.clone());
            }
        }
    }
}
